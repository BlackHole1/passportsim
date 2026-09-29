//! The versioned snapshot container: the section codec, the [`SnapSection`] trait and the
//! canonical encoding every identity hash is taken over. A snapshot is a [`SnapHeader`] and a
//! table of [`Section`]s, each with its own version and codec, so a reader refuses an unknown
//! required section and skips an unknown optional one. Hashes are taken over the parsed structure
//! re-encoded canonically, never over the bytes a file arrived in. Derived state (caches, page
//! table, poll tracker, `HookSet`) is rebuilt after a restore.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::fidelity::FidelityLedger;
use crate::hostio::{HostIo, LineMark, PcmRecord, RingRestoreError, SerialStream, UsjCtrl};
use crate::journal::{Determinism, JournalEntry, JournalState, LiveNote, LiveStream};
use crate::rng::DetRng;
use crate::sched::Scheduler;

pub const MAGIC: [u8; 8] = *b"PEMUSNAP";

/// Version of the container framing. Also bumped when a machine revision makes the previous
/// revision's snapshots unrestorable as a whole (`pemu-machine` writes all its sections at one
/// `SECTION_VERSION`), so the refusal is `UnsupportedFormat`, not a missing or short section.
pub const FORMAT_VERSION: u16 = 28;

/// blake3 of a stored snapshot stream, so a host finds a damaged file before parsing it. Not a
/// run identity: see [`Snapshot::canonical_hash`].
pub fn stored_digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// Largest section payload the reader accepts, so a corrupt length allocates nothing.
const MAX_SECTION_LEN: u32 = 256 << 20;

/// Section name such as `hart` or `soc.<periph>`; a reader that does not know a section decides
/// from the name alone whether it may go on.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SectionId(pub String);

impl SectionId {
    pub const HART: &'static str = "hart";
    pub const RAM: &'static str = "ram";
    pub const RTC_RAM: &'static str = "rtc_ram";
    pub const FLASH_DELTA: &'static str = "flash_delta";
    pub const SCHED: &'static str = "sched";
    pub const RNG: &'static str = "rng";
    pub const CLOCK: &'static str = "clock";
    pub const JOURNAL_CURSOR: &'static str = "journal_cursor";
    pub const JOURNAL_PENDING: &'static str = "journal_pending";
    pub const HOSTIO: &'static str = "hostio";
    pub const HLE: &'static str = "hle";
    pub const BLE: &'static str = "ble";
    pub const WIFI: &'static str = "wifi";
    pub const LAN: &'static str = "lan";
    pub const LEDGER: &'static str = "ledger";
    /// Poll tracker and hang-detector clocks; outside `state_hash`, like `host`.
    pub const HANG: &'static str = "hang";
    /// `FramePort` generation and dirty span: host-visible frame numbering, outside `state_hash`.
    pub const FRAME: &'static str = "frame";

    pub const SOC_PREFIX: &'static str = "soc.";
    pub const BOARD_PREFIX: &'static str = "board.";

    pub fn new(name: impl Into<String>) -> SectionId {
        SectionId(name.into())
    }

    pub fn soc(periph: &str) -> SectionId {
        SectionId(format!("{}{periph}", SectionId::SOC_PREFIX))
    }

    pub fn board(chip: &str) -> SectionId {
        SectionId(format!("{}{chip}", SectionId::BOARD_PREFIX))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a reader that does not know this section may go on without it: `ledger` is
    /// bookkeeping, and a build without a radio has nothing to restore `ble`, `wifi` or `lan` into.
    /// UNVERIFIED: no source fixes the optional set; it is this crate's choice.
    pub fn is_optional(&self) -> bool {
        matches!(
            self.0.as_str(),
            SectionId::LEDGER | SectionId::BLE | SectionId::WIFI | SectionId::LAN
        )
    }

    /// What a restore does with this section; the default classifier of [`Snapshot::plan_restore`].
    pub fn fate(&self, known: bool) -> SectionFate {
        match (known, self.is_optional()) {
            (true, _) => SectionFate::Apply,
            (false, true) => SectionFate::Skip,
            (false, false) => SectionFate::Unknown,
        }
    }
}

impl fmt::Display for SectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a restore does with one section.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SectionFate {
    Apply,
    Skip,
    /// Refused with [`SnapError::UnknownSection`].
    Unknown,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Codec {
    /// The canonical codec.
    #[default]
    Postcard,
    /// Postcard in LZ4. No build writes it yet; reading it is [`SnapError::UnsupportedCodec`].
    PostcardLz4,
}

impl Codec {
    fn tag(self) -> u8 {
        match self {
            Codec::Postcard => 0,
            Codec::PostcardLz4 => 1,
        }
    }

    fn from_tag(tag: u8) -> Option<Codec> {
        match tag {
            0 => Some(Codec::Postcard),
            1 => Some(Codec::PostcardLz4),
            _ => None,
        }
    }
}

/// One versioned snapshot section. `bytes` is always the decoded postcard payload; `codec` only
/// records the framing, so every hash sees one representation of a section's state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Section {
    pub version: u16,
    pub codec: Codec,
    pub bytes: Vec<u8>,
}

/// Snapshot header. eFuse contents never appear here, exported or not: only the kind and a hash.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SnapHeader {
    pub format: u16,
    pub core_version: String,
    pub rom_sha256: [u8; 32],
    /// In load order.
    pub image_sha256: Vec<[u8; 32]>,
    /// Over the parsed `MachineConfig`, never over the config file's bytes.
    pub config_hash: [u8; 32],
    pub efuse_kind: EfuseKind,
    /// All zero in a redacted export: an unsalted hash of a device's eFuse would identify that
    /// device to anyone holding its dump.
    pub efuse_hash: [u8; 32],
    /// Written by `snapshot export` ([`SnapOpts::export`]).
    pub exported: bool,
    /// Secret-bearing state was removed ([`SnapOpts::export`] without `include_secrets`).
    pub redacted: bool,
}

/// Where a run's eFuse image came from; `pemu-machine`'s `EfuseSource` maps onto this.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum EfuseKind {
    /// From the machine seed: no device data, untainted.
    #[default]
    Synthesized,
    /// From a device dump, which taints the run.
    Imported,
}

impl SnapHeader {
    /// Every hash zero, eFuse synthesized; `pemu-machine` fills the fields from the loaded assets.
    pub fn new() -> SnapHeader {
        SnapHeader {
            format: FORMAT_VERSION,
            core_version: env!("CARGO_PKG_VERSION").to_string(),
            rom_sha256: [0; 32],
            image_sha256: Vec::new(),
            config_hash: [0; 32],
            efuse_kind: EfuseKind::default(),
            efuse_hash: [0; 32],
            exported: false,
            redacted: false,
        }
    }

    /// The asset half of the run identity. Version and export flags are absent: two builds that
    /// differ only in their version string run the same program on the same assets.
    pub fn identity_hash(&self) -> [u8; 32] {
        let mut out = Vec::new();
        self.rom_sha256.snap_write(&mut out);
        self.image_sha256.snap_write(&mut out);
        self.efuse_hash.snap_write(&mut out);
        self.config_hash.snap_write(&mut out);
        (self.efuse_kind == EfuseKind::Imported).snap_write(&mut out);
        *blake3::hash(&out).as_bytes()
    }
}

impl Default for SnapHeader {
    fn default() -> Self {
        SnapHeader::new()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct SnapOpts {
    /// The snapshot leaves this machine: redact secret-bearing state unless `include_secrets`.
    pub export: bool,
    pub include_secrets: bool,
}

/// Live-bridge policy of `Machine::fork`.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub enum LivePolicy {
    #[default]
    Refuse,
    /// Give the copy a journaled link-down and detached bridges; the original keeps them.
    LinkDown,
}

/// Error of the snapshot codec, of `Machine::restore` and of `Machine::fork`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SnapError {
    BadMagic,
    /// A newer writer, or a corrupt file.
    UnsupportedFormat {
        found: u16,
        supported: u16,
    },
    Truncated {
        /// A container part such as `"section table"`, or a section name.
        at: &'static str,
    },
    /// The bytes parsed but do not describe a snapshot (bad length, non-UTF-8 name, unknown codec).
    Malformed {
        at: &'static str,
        reason: &'static str,
    },
    DuplicateSection(SectionId),
    /// A required section this build does not know.
    UnknownSection(SectionId),
    MissingSection(SectionId),
    Incompatible {
        section: SectionId,
        found: u16,
        expected: u16,
    },
    UnsupportedCodec {
        section: SectionId,
        codec: Codec,
    },
    /// A section decoded but left bytes over.
    TrailingBytes {
        section: SectionId,
    },
    /// The header's ROM, flash image, eFuse or config hash differs from the machine it is
    /// restored into (`E_SNAPSHOT`).
    IdentityMismatch {
        field: IdentityField,
    },
    /// A live host peer is attached, so the state is not plain data (`E_STATE`).
    LiveBridge {
        bridge: String,
    },
}

/// The run-identity field [`SnapError::IdentityMismatch`] names, in restore comparison order.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IdentityField {
    Rom,
    Image,
    /// [`SnapHeader::efuse_kind`] or [`SnapHeader::efuse_hash`].
    Efuse,
    Config,
}

impl IdentityField {
    pub fn name(self) -> &'static str {
        match self {
            IdentityField::Rom => "ROM image",
            IdentityField::Image => "flash image",
            IdentityField::Efuse => "eFuse image",
            IdentityField::Config => "machine configuration",
        }
    }
}

impl SnapHeader {
    /// The first run-identity field on which `self` and `other` differ. `check_efuse` false skips
    /// the eFuse fields: a redacted export carries no eFuse hash.
    pub fn identity_mismatch(
        &self,
        other: &SnapHeader,
        check_efuse: bool,
    ) -> Option<IdentityField> {
        if self.rom_sha256 != other.rom_sha256 {
            Some(IdentityField::Rom)
        } else if self.image_sha256 != other.image_sha256 {
            Some(IdentityField::Image)
        } else if check_efuse
            && (self.efuse_kind != other.efuse_kind || self.efuse_hash != other.efuse_hash)
        {
            Some(IdentityField::Efuse)
        } else if self.config_hash != other.config_hash {
            Some(IdentityField::Config)
        } else {
            None
        }
    }
}

impl fmt::Display for SnapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapError::BadMagic => write!(f, "not a snapshot: wrong magic"),
            SnapError::UnsupportedFormat { found, supported } => write!(
                f,
                "snapshot format version {found} is not readable by this build (format {supported})"
            ),
            SnapError::Truncated { at } => write!(f, "snapshot ends inside {at}"),
            SnapError::Malformed { at, reason } => {
                write!(f, "malformed snapshot at {at}: {reason}")
            }
            SnapError::DuplicateSection(id) => write!(f, "section {id} appears twice"),
            SnapError::UnknownSection(id) => {
                write!(f, "required section {id} is unknown to this build")
            }
            SnapError::MissingSection(id) => write!(f, "section {id} is missing"),
            SnapError::Incompatible {
                section,
                found,
                expected,
            } => write!(
                f,
                "section {section} is version {found}, this build reads version {expected}"
            ),
            SnapError::UnsupportedCodec { section, codec } => {
                write!(
                    f,
                    "section {section} uses codec {codec:?}, which this build cannot decode"
                )
            }
            SnapError::TrailingBytes { section } => {
                write!(f, "section {section} has bytes past its value")
            }
            SnapError::IdentityMismatch { field } => write!(
                f,
                "the snapshot was taken on another {}; restore it into a machine built from the \
                 same assets and configuration",
                field.name()
            ),
            SnapError::LiveBridge { bridge } => {
                write!(
                    f,
                    "a live bridge is attached ({bridge}); the state is not plain data"
                )
            }
        }
    }
}

impl std::error::Error for SnapError {}

/// Reads one canonical section encoding. It never trusts a length it has not compared against the
/// bytes it holds, so a hostile section ends in an error, never in a panic or a huge allocation.
pub struct SnapReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    at: &'static str,
}

impl<'a> SnapReader<'a> {
    /// `at` names what is being read in any error (a section name).
    pub fn new(bytes: &'a [u8], at: &'static str) -> SnapReader<'a> {
        SnapReader { bytes, pos: 0, at }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn truncated(&self) -> SnapError {
        SnapError::Truncated { at: self.at }
    }

    fn malformed(&self, reason: &'static str) -> SnapError {
        SnapError::Malformed {
            at: self.at,
            reason,
        }
    }

    pub fn byte(&mut self) -> Result<u8, SnapError> {
        let b = *self.bytes.get(self.pos).ok_or_else(|| self.truncated())?;
        self.pos += 1;
        Ok(b)
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], SnapError> {
        let end = self.pos.checked_add(n).ok_or_else(|| self.truncated())?;
        let out = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| self.truncated())?;
        self.pos = end;
        Ok(out)
    }

    /// An unsigned LEB128 varint of at most `max_bytes` bytes (3 for `u16`, 5 for `u32`, 10 for
    /// `u64`). An overlong encoding, or a value past the width, is [`SnapError::Malformed`].
    pub fn varint(&mut self, max_bytes: usize) -> Result<u64, SnapError> {
        let mut value: u64 = 0;
        for i in 0..max_bytes {
            let b = self.byte()?;
            let shift = 7 * i;
            let payload = u64::from(b & 0x7f);
            if shift >= 64 || (payload << shift) >> shift != payload {
                return Err(self.malformed("varint does not fit its type"));
            }
            value |= payload << shift;
            if b & 0x80 == 0 {
                // A final byte of zero past the first spells a shorter value the long way: one
                // value has one encoding, or the same state would have two `canonical_hash`es.
                if i > 0 && b == 0 {
                    return Err(self.malformed("varint is not minimally encoded"));
                }
                return Ok(value);
            }
        }
        Err(self.malformed("varint longer than its type allows"))
    }

    /// A varint length prefix, refused when larger than the bytes left.
    pub fn len_prefix(&mut self) -> Result<usize, SnapError> {
        let len = self.varint(10)?;
        let len = usize::try_from(len).map_err(|_| self.malformed("length past the end"))?;
        if len > self.remaining() {
            return Err(self.truncated());
        }
        Ok(len)
    }

    fn finish(self, section: &SectionId) -> Result<(), SnapError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(SnapError::TrailingBytes {
                section: section.clone(),
            })
        }
    }
}

/// Unsigned LEB128, the postcard encoding of an unsigned integer wider than a byte.
fn write_varint(value: u64, out: &mut Vec<u8>) {
    let mut value = value;
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A value a snapshot section is built from, in postcard's encoding, so [`snap_struct!`] and
/// `postcard` agree. Canonical: one value has one encoding, minimal varints, no padding.
pub trait SnapValue: Sized {
    fn snap_write(&self, out: &mut Vec<u8>);

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError>;
}

/// Implements [`SnapValue`] for a struct as its listed fields, in the order listed. A struct that
/// encodes a tag, validates on read or has conditional fields writes its impl by hand.
///
/// ```
/// use pemu_core::snap::{SnapReader, SnapValue, snap_struct};
///
/// #[derive(Debug, PartialEq)]
/// struct Battery {
///     millivolts: u32,
///     charging: bool,
/// }
/// snap_struct!(Battery { millivolts, charging });
///
/// let mut bytes = Vec::new();
/// Battery { millivolts: 300, charging: true }.snap_write(&mut bytes);
/// // 300 as a varint, then the flag.
/// assert_eq!(bytes, vec![0xac, 0x02, 0x01]);
/// let back = Battery::snap_read(&mut SnapReader::new(&bytes, "board.battery")).unwrap();
/// assert_eq!(back, Battery { millivolts: 300, charging: true });
/// ```
///
/// # What the macro refuses
///
/// Every field must be listed, because `snap_read` builds the struct with a literal:
///
/// ```compile_fail
/// use pemu_core::snap::snap_struct;
///
/// struct Battery {
///     millivolts: u32,
///     charging: bool,
/// }
/// snap_struct!(Battery { millivolts });
/// ```
///
/// And every field type must have a [`SnapValue`] encoding, so a float is a compile error:
///
/// ```compile_fail
/// use pemu_core::snap::snap_struct;
///
/// struct Battery {
///     volts: f64,
/// }
/// snap_struct!(Battery { volts });
/// ```
///
/// So is a `usize`, whose width depends on the host:
///
/// ```compile_fail
/// use pemu_core::snap::snap_struct;
///
/// struct Battery {
///     samples: usize,
/// }
/// snap_struct!(Battery { samples });
/// ```
///
/// And a type with no encoding of its own:
///
/// ```compile_fail
/// use pemu_core::snap::snap_struct;
///
/// struct Gauge;
///
/// struct Battery {
///     gauge: Gauge,
/// }
/// snap_struct!(Battery { gauge });
/// ```
#[macro_export]
macro_rules! snap_struct {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl $crate::snap::SnapValue for $ty {
            fn snap_write(&self, out: &mut ::std::vec::Vec<u8>) {
                $($crate::snap::SnapValue::snap_write(&self.$field, out);)*
            }

            fn snap_read(
                r: &mut $crate::snap::SnapReader<'_>,
            ) -> ::core::result::Result<$ty, $crate::snap::SnapError> {
                ::core::result::Result::Ok($ty {
                    $($field: $crate::snap::SnapValue::snap_read(r)?,)*
                })
            }
        }
    };
}

#[doc(inline)]
pub use crate::snap_struct;

impl SnapValue for u8 {
    fn snap_write(&self, out: &mut Vec<u8>) {
        out.push(*self);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        r.byte()
    }
}

impl SnapValue for i8 {
    fn snap_write(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        Ok(r.byte()? as i8)
    }
}

impl SnapValue for bool {
    fn snap_write(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        match r.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(r.malformed("bool is neither 0 nor 1")),
        }
    }
}

/// Unsigned integers wider than a byte: LEB128, bounded by the postcard byte count of the width.
macro_rules! unsigned_value {
    ($ty:ty, $max_bytes:expr) => {
        impl SnapValue for $ty {
            fn snap_write(&self, out: &mut Vec<u8>) {
                write_varint(u64::from(*self), out);
            }

            fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
                let v = r.varint($max_bytes)?;
                <$ty>::try_from(v).map_err(|_| r.malformed("integer does not fit its type"))
            }
        }
    };
}

unsigned_value!(u16, 3);
unsigned_value!(u32, 5);
unsigned_value!(u64, 10);

/// Signed integers wider than a byte: zigzag, then LEB128, as postcard encodes them.
macro_rules! signed_value {
    ($ty:ty, $unsigned:ty, $bits:expr, $max_bytes:expr) => {
        impl SnapValue for $ty {
            fn snap_write(&self, out: &mut Vec<u8>) {
                let zigzag = ((*self << 1) ^ (*self >> ($bits - 1))) as $unsigned;
                write_varint(u64::from(zigzag), out);
            }

            fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
                let v = r.varint($max_bytes)?;
                let zigzag = <$unsigned>::try_from(v)
                    .map_err(|_| r.malformed("integer does not fit its type"))?;
                Ok(((zigzag >> 1) as $ty) ^ -((zigzag & 1) as $ty))
            }
        }
    };
}

signed_value!(i16, u16, 16, 3);
signed_value!(i32, u32, 32, 5);
signed_value!(i64, u64, 64, 10);

impl SnapValue for String {
    fn snap_write(&self, out: &mut Vec<u8>) {
        write_varint(self.len() as u64, out);
        out.extend_from_slice(self.as_bytes());
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        let len = r.len_prefix()?;
        let bytes = r.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| r.malformed("string is not UTF-8"))
    }
}

impl<T: SnapValue> SnapValue for Vec<T> {
    fn snap_write(&self, out: &mut Vec<u8>) {
        write_varint(self.len() as u64, out);
        for item in self {
            item.snap_write(out);
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        let len = r.len_prefix()?;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(T::snap_read(r)?);
        }
        Ok(out)
    }
}

impl<T: SnapValue> SnapValue for Option<T> {
    fn snap_write(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(v) => {
                out.push(1);
                v.snap_write(out);
            }
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        match r.byte()? {
            0 => Ok(None),
            1 => Ok(Some(T::snap_read(r)?)),
            _ => Err(r.malformed("Option discriminant is neither 0 nor 1")),
        }
    }
}

impl<T: SnapValue, const N: usize> SnapValue for [T; N] {
    fn snap_write(&self, out: &mut Vec<u8>) {
        for item in self {
            item.snap_write(out);
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Self, SnapError> {
        let mut items = Vec::with_capacity(N);
        for _ in 0..N {
            items.push(T::snap_read(r)?);
        }
        // The loop pushed exactly N items, so the conversion cannot fail.
        items.try_into().map_err(|_| SnapError::Malformed {
            at: "array",
            reason: "wrong element count",
        })
    }
}

/// One versioned snapshot section, implemented through [`value_section`] and
/// [`value_from_section`] for a [`SnapValue`] type, or with postcard for a serde layout.
pub trait SnapSection: Sized {
    const NAME: &'static str;
    /// A bump needs a migration or an explicit [`SnapError::Incompatible`].
    const VERSION: u16;

    fn section_id() -> SectionId {
        SectionId::new(Self::NAME)
    }

    fn encode(&self) -> Result<Section, SnapError>;

    /// Refuses another version, bytes left over, or bytes that are not this value.
    fn decode(section: &Section) -> Result<Self, SnapError>;
}

/// [`SnapSection::encode`] for a type whose bytes are its [`SnapValue`] encoding.
pub fn value_section<T: SnapValue + SnapSection>(value: &T) -> Result<Section, SnapError> {
    let mut bytes = Vec::new();
    value.snap_write(&mut bytes);
    Ok(Section {
        version: T::VERSION,
        codec: Codec::Postcard,
        bytes,
    })
}

/// [`SnapSection::decode`] for a type whose bytes are its [`SnapValue`] encoding.
pub fn value_from_section<T: SnapValue + SnapSection>(section: &Section) -> Result<T, SnapError> {
    let id = check_section::<T>(section)?;
    let mut reader = SnapReader::new(&section.bytes, T::NAME);
    let value = T::snap_read(&mut reader)?;
    reader.finish(&id)?;
    Ok(value)
}

/// A section holding `value` in its serde form, postcard at `version`, for a section whose name is
/// not fixed at compile time (`soc.<periph>`, `board.<chip>`).
pub fn serde_section<T: Serialize>(
    value: &T,
    version: u16,
    at: &'static str,
) -> Result<Section, SnapError> {
    let bytes = postcard::to_stdvec(value).map_err(|_| SnapError::Malformed {
        at,
        reason: "the value cannot be encoded",
    })?;
    Ok(Section {
        version,
        codec: Codec::Postcard,
        bytes,
    })
}

/// A `#[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, N>")]` field decoder: a
/// `Vec<T>` of exactly `N` elements. A derived `Vec` takes any length, so a model indexing a fixed
/// window would panic on a corrupt section later instead of refusing the restore.
pub fn exact_vec<'de, D, T, const N: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let v = Vec::<T>::deserialize(d)?;
    if v.len() != N {
        return Err(<D::Error as serde::de::Error>::invalid_length(
            v.len(),
            &LenExpected(N),
        ));
    }
    Ok(v)
}

/// [`exact_vec`] for a boxed slice.
pub fn exact_boxed<'de, D, T, const N: usize>(d: D) -> Result<Box<[T]>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    exact_vec::<D, T, N>(d).map(Vec::into_boxed_slice)
}

struct LenExpected(usize);

impl serde::de::Expected for LenExpected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} elements", self.0)
    }
}

/// The value [`serde_section`] wrote, with the same refusals as the [`SnapSection`] decoders.
pub fn serde_from_section<T: serde::de::DeserializeOwned>(
    section: &Section,
    id: SectionId,
    version: u16,
    at: &'static str,
) -> Result<T, SnapError> {
    if section.version != version {
        return Err(SnapError::Incompatible {
            section: id,
            found: section.version,
            expected: version,
        });
    }
    if section.codec != Codec::Postcard {
        return Err(SnapError::UnsupportedCodec {
            section: id,
            codec: section.codec,
        });
    }
    let (value, rest) = postcard::take_from_bytes::<T>(&section.bytes).map_err(|e| match e {
        postcard::Error::DeserializeUnexpectedEnd => SnapError::Truncated { at },
        _ => SnapError::Malformed {
            at,
            reason: "the bytes are not this section's value",
        },
    })?;
    if !rest.is_empty() {
        return Err(SnapError::TrailingBytes { section: id });
    }
    Ok(value)
}

fn check_section<T: SnapSection>(section: &Section) -> Result<SectionId, SnapError> {
    let id = T::section_id();
    if section.version != T::VERSION {
        return Err(SnapError::Incompatible {
            section: id,
            found: section.version,
            expected: T::VERSION,
        });
    }
    if section.codec != Codec::Postcard {
        return Err(SnapError::UnsupportedCodec {
            section: id,
            codec: section.codec,
        });
    }
    Ok(id)
}

/// Implements [`SnapSection`] as postcard for a type whose serde layout is its section content.
macro_rules! postcard_section {
    ($ty:ty, $name:expr, $version:expr) => {
        impl SnapSection for $ty {
            const NAME: &'static str = $name;
            const VERSION: u16 = $version;

            fn encode(&self) -> Result<Section, SnapError> {
                let bytes = postcard::to_stdvec(self).map_err(|_| SnapError::Malformed {
                    at: $name,
                    reason: "the value cannot be encoded",
                })?;
                Ok(Section {
                    version: $version,
                    codec: Codec::Postcard,
                    bytes,
                })
            }

            fn decode(section: &Section) -> Result<Self, SnapError> {
                let id = check_section::<Self>(section)?;
                let (value, rest) =
                    postcard::take_from_bytes::<Self>(&section.bytes).map_err(|e| match e {
                        postcard::Error::DeserializeUnexpectedEnd => {
                            SnapError::Truncated { at: $name }
                        }
                        _ => SnapError::Malformed {
                            at: $name,
                            reason: "the bytes are not this section's value",
                        },
                    })?;
                if !rest.is_empty() {
                    return Err(SnapError::TrailingBytes { section: id });
                }
                Ok(value)
            }
        }
    };
}

// The heap is rebuilt, not saved (`sched::SchedState`).
postcard_section!(Scheduler, SectionId::SCHED, 1);
// The ChaCha20 key and the keystream cache are rebuilt, not saved (`rng::RngState`).
postcard_section!(DetRng, SectionId::RNG, 1);
postcard_section!(Clock, SectionId::CLOCK, 1);
// Optional: a build without a ledger restores the same guest state.
postcard_section!(FidelityLedger, SectionId::LEDGER, 1);

/// The `journal_cursor` section: everything of [`JournalState`] except the pending entries.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct JournalCursor {
    pub cursor: u64,
    pub next_seq: u64,
    pub class: Determinism,
    pub live_notes: Vec<LiveNote>,
    pub live_note_count: u64,
    pub live_next: [u64; LiveStream::ALL.len()],
}

/// The `journal_pending` section: the entries with `at` past the cursor, in `(at, seq)` order.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct JournalPending(pub Vec<JournalEntry>);

postcard_section!(JournalCursor, SectionId::JOURNAL_CURSOR, 1);
postcard_section!(JournalPending, SectionId::JOURNAL_PENDING, 1);

pub fn split_journal(state: JournalState) -> (JournalCursor, JournalPending) {
    (
        JournalCursor {
            cursor: state.cursor,
            next_seq: state.next_seq,
            class: state.class,
            live_notes: state.live_notes,
            live_note_count: state.live_note_count,
            live_next: state.live_next,
        },
        JournalPending(state.pending),
    )
}

/// Puts the two journal sections back into the state `Journal::restore` takes.
pub fn join_journal(cursor: JournalCursor, pending: JournalPending) -> JournalState {
    JournalState {
        pending: pending.0,
        cursor: cursor.cursor,
        next_seq: cursor.next_seq,
        class: cursor.class,
        live_notes: cursor.live_notes,
        live_note_count: cursor.live_note_count,
        live_next: cursor.live_next,
    }
}

/// The `hostio` section: undrained host-to-guest content, `usj_ctrl`, the line index and read
/// cursors, never the rings, so [`HostIoSection::restore_into`] refills existing rings without
/// allocating. Guest-to-host rings are host capture; inbound `net` and `hci` live in `hle.machine`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct HostIoSection {
    /// Absolute cursor of the oldest undrained `usj_rx` byte; the other tails work the same way.
    pub usj_rx_tail: u64,
    pub usj_rx: Vec<u8>,
    pub audio_in_tail: u64,
    pub audio_in: Vec<i16>,
    pub audio_in_record_tail: u64,
    pub audio_in_records: Vec<PcmRecord>,
    /// Zero samples `audio_in` handed out because it ran dry.
    pub audio_in_underflows: u64,
    /// Host microphone samples dropped: the oldest frames an overflow evicted and audio that
    /// arrived while the capture path was not running.
    pub audio_in_dropped: u64,
    pub usj_ctrl: UsjCtrl,
    /// Per stream, in [`SerialStream::index`] order, like the two fields after it.
    pub line_tails: [u64; SerialStream::ALL.len()],
    pub line_marks: [Vec<LineMark>; SerialStream::ALL.len()],
    pub line_read_cursors: [u64; SerialStream::ALL.len()],
}

postcard_section!(HostIoSection, SectionId::HOSTIO, 1);

impl HostIoSection {
    pub fn capture(io: &HostIo) -> HostIoSection {
        HostIoSection {
            usj_rx_tail: io.usj_rx.tail(),
            usj_rx: io.usj_rx.slices(io.usj_rx.tail()).iter().copied().collect(),
            audio_in_tail: io.audio_in.tail(),
            audio_in: io
                .audio_in
                .slices(io.audio_in.tail())
                .iter()
                .copied()
                .collect(),
            audio_in_record_tail: io.audio_in.record_tail(),
            audio_in_records: io
                .audio_in
                .record_slices(io.audio_in.record_tail())
                .iter()
                .copied()
                .collect(),
            audio_in_underflows: io.audio_in.underflows(),
            audio_in_dropped: io.audio_in.dropped(),
            usj_ctrl: io.usj_ctrl,
            line_tails: SerialStream::ALL.map(|s| io.lines.tail(s)),
            line_marks: SerialStream::ALL.map(|s| {
                io.lines
                    .slices(s, io.lines.tail(s))
                    .iter()
                    .copied()
                    .collect()
            }),
            line_read_cursors: io.lines.read_cursors(),
        }
    }

    /// Puts the content back into `io` in place. Every window is checked before any is written, so
    /// a refusal leaves `io` exactly as it was.
    pub fn restore_into(&self, io: &mut HostIo) -> Result<(), RingRestoreError> {
        check_window(io.usj_rx.capacity(), self.usj_rx_tail, self.usj_rx.len())?;
        check_window(
            io.audio_in.capacity(),
            self.audio_in_tail,
            self.audio_in.len(),
        )?;
        check_window(
            io.audio_in.record_capacity(),
            self.audio_in_record_tail,
            self.audio_in_records.len(),
        )?;
        for stream in SerialStream::ALL {
            check_window(
                io.lines.capacity(stream),
                self.line_tails[stream.index()],
                self.line_marks[stream.index()].len(),
            )?;
        }
        io.usj_rx.restore(self.usj_rx_tail, &self.usj_rx)?;
        io.audio_in.restore(
            self.audio_in_tail,
            &self.audio_in,
            self.audio_in_record_tail,
            &self.audio_in_records,
            self.audio_in_underflows,
        )?;
        io.audio_in.restore_dropped(self.audio_in_dropped);
        io.lines.restore(
            self.line_tails,
            SerialStream::ALL.map(|s| self.line_marks[s.index()].as_slice()),
            self.line_read_cursors,
        )?;
        io.usj_ctrl = self.usj_ctrl;
        Ok(())
    }
}

fn check_window(capacity: usize, tail: u64, len: usize) -> Result<(), RingRestoreError> {
    if len > capacity {
        return Err(RingRestoreError::TooLong { len, capacity });
    }
    if tail.checked_add(len as u64).is_none() {
        return Err(RingRestoreError::CursorOverflow);
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub header: SnapHeader,
    /// A `BTreeMap`, so sections are written in name order, not in model visit order.
    pub sections: BTreeMap<SectionId, Section>,
}

/// What a restore applies and what it skips, from [`Snapshot::plan_restore`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RestorePlan {
    pub apply: Vec<SectionId>,
    /// Unknown optional sections; the receipt reports them.
    pub skipped: Vec<SectionId>,
}

impl Snapshot {
    pub fn new(header: SnapHeader) -> Snapshot {
        Snapshot {
            header,
            sections: BTreeMap::new(),
        }
    }

    /// Encodes `value` under its [`SnapSection::NAME`], replacing any section already there.
    pub fn put<T: SnapSection>(&mut self, value: &T) -> Result<(), SnapError> {
        let section = value.encode()?;
        self.sections.insert(T::section_id(), section);
        Ok(())
    }

    /// Puts a section built elsewhere (`soc.<periph>`, `board.<chip>`, `hart`) under `id`.
    pub fn put_raw(&mut self, id: SectionId, section: Section) {
        self.sections.insert(id, section);
    }

    pub fn section(&self, id: &SectionId) -> Result<&Section, SnapError> {
        self.sections
            .get(id)
            .ok_or_else(|| SnapError::MissingSection(id.clone()))
    }

    pub fn get<T: SnapSection>(&self) -> Result<T, SnapError> {
        T::decode(self.section(&T::section_id())?)
    }

    /// Splits the sections into the ones a restore applies and the ones it skips, refusing an
    /// unknown required section. The usual `classify` is `|id| id.fate(self.knows(id))`.
    pub fn plan_restore(
        &self,
        classify: impl Fn(&SectionId) -> SectionFate,
    ) -> Result<RestorePlan, SnapError> {
        let mut plan = RestorePlan {
            apply: Vec::new(),
            skipped: Vec::new(),
        };
        for id in self.sections.keys() {
            match classify(id) {
                SectionFate::Apply => plan.apply.push(id.clone()),
                SectionFate::Skip => plan.skipped.push(id.clone()),
                SectionFate::Unknown => return Err(SnapError::UnknownSection(id.clone())),
            }
        }
        Ok(plan)
    }

    /// The whole snapshot as a byte stream: magic, format, header, section table, payloads. Never
    /// writes a stream this build cannot read back: the format is [`FORMAT_VERSION`], and an
    /// unwritable codec or oversize payload is refused.
    pub fn to_bytes(&self) -> Result<Vec<u8>, SnapError> {
        let header = SnapHeader {
            format: FORMAT_VERSION,
            ..self.header.clone()
        };
        let header = postcard::to_stdvec(&header).map_err(|_| SnapError::Malformed {
            at: "header",
            reason: "the header cannot be encoded",
        })?;
        for (id, section) in &self.sections {
            if section.codec != Codec::Postcard {
                return Err(SnapError::UnsupportedCodec {
                    section: id.clone(),
                    codec: section.codec,
                });
            }
            if section.bytes.len() > MAX_SECTION_LEN as usize {
                return Err(SnapError::Malformed {
                    at: "section table",
                    reason: "a section length is past what a snapshot can hold",
                });
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        write_section_table(&self.sections, None, &mut out);
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Snapshot, SnapError> {
        let mut r = SnapReader::new(bytes, "container");
        if r.take(MAGIC.len()).map_err(|_| SnapError::BadMagic)? != MAGIC {
            return Err(SnapError::BadMagic);
        }
        let format = u16::from_le_bytes(read_fixed::<2>(&mut r)?);
        if format != FORMAT_VERSION {
            return Err(SnapError::UnsupportedFormat {
                found: format,
                supported: FORMAT_VERSION,
            });
        }
        let _flags = u16::from_le_bytes(read_fixed::<2>(&mut r)?);
        let header_len = u32::from_le_bytes(read_fixed::<4>(&mut r)?) as usize;
        let header_bytes = r.take(header_len)?;
        let header: SnapHeader = postcard::from_bytes(header_bytes).map_err(|e| match e {
            postcard::Error::DeserializeUnexpectedEnd => SnapError::Truncated { at: "header" },
            _ => SnapError::Malformed {
                at: "header",
                reason: "the bytes are not a snapshot header",
            },
        })?;
        let sections = read_section_table(&mut r)?;
        Ok(Snapshot { header, sections })
    }

    /// The canonical encoding of the sections, in name order with every codec normalized to
    /// [`Codec::Postcard`]; the header is not part of it.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_section_table(&self.sections, Some(Codec::Postcard), &mut out);
        out
    }

    /// blake3 over [`Snapshot::canonical_bytes`]: the value `Machine::state_hash` returns.
    pub fn canonical_hash(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }

    /// The run identity: the header's asset hashes plus the persisted flash deltas and the input
    /// journal, in the canonical encoding. An absent section folds in as a zero length.
    pub fn run_identity(&self) -> [u8; 32] {
        let mut out = Vec::new();
        out.extend_from_slice(&self.header.identity_hash());
        for name in [
            SectionId::FLASH_DELTA,
            SectionId::JOURNAL_CURSOR,
            SectionId::JOURNAL_PENDING,
        ] {
            let id = SectionId::new(name);
            match self.sections.get(&id) {
                Some(section) => write_section(&id, section, Codec::Postcard, &mut out),
                None => write_varint(0, &mut out),
            }
        }
        *blake3::hash(&out).as_bytes()
    }
}

fn read_fixed<const N: usize>(r: &mut SnapReader<'_>) -> Result<[u8; N], SnapError> {
    let bytes = r.take(N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    Ok(out)
}

/// Writes the section count, the table and the payloads. `codec` overrides every entry's codec,
/// which is how [`Snapshot::canonical_bytes`] normalizes; `None` keeps each section's own.
fn write_section_table(
    sections: &BTreeMap<SectionId, Section>,
    codec: Option<Codec>,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    for (id, section) in sections {
        write_section(id, section, codec.unwrap_or(section.codec), out);
    }
}

/// Name, version, codec, length, bytes.
fn write_section(id: &SectionId, section: &Section, codec: Codec, out: &mut Vec<u8>) {
    let name = id.as_str().as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&section.version.to_le_bytes());
    out.push(codec.tag());
    out.extend_from_slice(&(section.bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&section.bytes);
}

fn read_section_table(r: &mut SnapReader<'_>) -> Result<BTreeMap<SectionId, Section>, SnapError> {
    let count = u32::from_le_bytes(read_fixed::<4>(r)?);
    let mut sections = BTreeMap::new();
    for _ in 0..count {
        let name_len = u16::from_le_bytes(read_fixed::<2>(r)?) as usize;
        let name = r.take(name_len)?;
        let name = core::str::from_utf8(name).map_err(|_| SnapError::Malformed {
            at: "section table",
            reason: "a section name is not UTF-8",
        })?;
        let id = SectionId::new(name);
        let version = u16::from_le_bytes(read_fixed::<2>(r)?);
        let codec = Codec::from_tag(r.byte()?).ok_or(SnapError::Malformed {
            at: "section table",
            reason: "unknown codec tag",
        })?;
        let len = u32::from_le_bytes(read_fixed::<4>(r)?);
        if len > MAX_SECTION_LEN {
            return Err(SnapError::Malformed {
                at: "section table",
                reason: "a section length is past what a snapshot can hold",
            });
        }
        let bytes = r.take(len as usize)?.to_vec();
        if codec != Codec::Postcard {
            return Err(SnapError::UnsupportedCodec { section: id, codec });
        }
        if sections
            .insert(
                id.clone(),
                Section {
                    version,
                    codec,
                    bytes,
                },
            )
            .is_some()
        {
            return Err(SnapError::DuplicateSection(id));
        }
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_mismatch_names_the_first_differing_field() {
        let base = SnapHeader::new();
        assert_eq!(base.identity_mismatch(&base.clone(), true), None);
        let mut other = base.clone();
        other.config_hash[0] = 1;
        other.efuse_hash[0] = 1;
        assert_eq!(
            base.identity_mismatch(&other, true),
            Some(IdentityField::Efuse)
        );
        assert_eq!(
            base.identity_mismatch(&other, false),
            Some(IdentityField::Config)
        );
        other.image_sha256.push([0; 32]);
        assert_eq!(
            base.identity_mismatch(&other, true),
            Some(IdentityField::Image)
        );
        other.rom_sha256[31] = 9;
        assert_eq!(
            base.identity_mismatch(&other, true),
            Some(IdentityField::Rom)
        );
        let text = SnapError::IdentityMismatch {
            field: IdentityField::Image,
        }
        .to_string();
        assert!(text.contains("flash image"), "{text}");
    }

    use serde::{Deserialize, Serialize};

    use crate::fidelity::{Fidelity, FirstTouch, LedgerSubject, TouchAccess};
    use crate::hostio::UsjEnumeration;
    use crate::input::{ButtonId, InputEvent};
    use crate::journal::{Journal, Origin};
    use crate::rng::RngStream;
    use crate::sched::{EventKey, MachineTimer, Owner, PeriphId};
    use crate::time::VTime;

    /// A test section whose bytes are its [`SnapValue`] encoding, as the `hle` section's are.
    macro_rules! value_test_section {
        ($ty:ident, $name:expr, $version:expr) => {
            impl SnapSection for $ty {
                const NAME: &'static str = $name;
                const VERSION: u16 = $version;

                fn encode(&self) -> Result<Section, SnapError> {
                    value_section(self)
                }

                fn decode(section: &Section) -> Result<$ty, SnapError> {
                    value_from_section(section)
                }
            }
        };
    }

    /// Mirror of `pemu_rv32::exec::Hart`, field for field: `pemu-rv32` depends on this crate, so
    /// the golden bytes below pin this layout instead.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
    struct TestHart {
        x: [u32; 32],
        pc: u32,
        csr: TestCsr,
        wfi: bool,
        insns: u64,
        stores: u64,
        spmon: TestSpMonitor,
    }
    snap_struct!(TestHart {
        x,
        pc,
        csr,
        wfi,
        insns,
        stores,
        spmon
    });
    value_test_section!(TestHart, SectionId::HART, 1);

    /// Mirror of `Csr`, like [`TestHart`].
    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
    struct TestCsr {
        mstatus: u32,
        mtvec: u32,
        mepc: u32,
        mcause: u32,
        mtval: u32,
        mscratch: u32,
        pmpcfg: [u8; 16],
        pmpaddr: [u32; 16],
        tselect: u32,
        tdata1: [u32; 8],
        tdata2: [u32; 8],
        tcontrol: u32,
        mpcer: u32,
        mpcmr: u32,
        csr000: u32,
    }
    snap_struct!(TestCsr {
        mstatus,
        mtvec,
        mepc,
        mcause,
        mtval,
        mscratch,
        pmpcfg,
        pmpaddr,
        tselect,
        tdata1,
        tdata2,
        tcontrol,
        mpcer,
        mpcmr,
        csr000
    });

    /// Mirror of `SpMonitor`, like [`TestHart`].
    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
    struct TestSpMonitor {
        on_min: bool,
        on_max: bool,
        min: u32,
        max: u32,
    }
    snap_struct!(TestSpMonitor {
        on_min,
        on_max,
        min,
        max
    });

    fn sample_hart() -> TestHart {
        let mut x = [0u32; 32];
        x[1] = 0x4000_0400; // ra
        x[2] = 0x3FCD_FFF0; // sp
        x[10] = 0x0000_002A; // a0
        let mut pmpaddr = [0u32; 16];
        pmpaddr[0] = 0x0FFF_FFFF;
        let mut pmpcfg = [0u8; 16];
        pmpcfg[0] = 0x1F;
        TestHart {
            x,
            pc: 0x4038_0120,
            csr: TestCsr {
                mstatus: 0x0000_1880,
                mtvec: 0x4038_0001,
                mepc: 0x4038_00F4,
                mcause: 0x0000_0007,
                mtval: 0x0000_0000,
                mscratch: 0x3FC8_0000,
                pmpcfg,
                pmpaddr,
                tselect: 0,
                tdata1: [0; 8],
                tdata2: [0; 8],
                tcontrol: 0,
                mpcer: 0x0000_0001,
                mpcmr: 0x0000_0003,
                csr000: 0,
            },
            wfi: false,
            insns: 12_345,
            stores: 678,
            spmon: TestSpMonitor {
                on_min: true,
                on_max: false,
                min: 0x3FC8_0000,
                max: 0x3FCE_0000,
            },
        }
    }

    /// Two live events and a cancelled slot: a free list, a stale generation, a later `next_seq`.
    fn sample_sched() -> Scheduler {
        let mut sched = Scheduler::new();
        let first = sched.schedule(
            VTime(0),
            VTime::from_us(10),
            EventKey {
                owner: Owner::Periph(PeriphId(3)),
                tag: 7,
            },
        );
        sched.schedule(
            VTime(0),
            VTime::from_us(5),
            EventKey {
                owner: Owner::Machine(MachineTimer(1)),
                tag: 0,
            },
        );
        sched.schedule(
            VTime(0),
            VTime::from_us(20),
            EventKey {
                owner: Owner::Journal,
                tag: 2,
            },
        );
        sched.cancel(first);
        sched
    }

    /// Two streams drawn from by different amounts, so the section carries more than the seed.
    fn sample_rng() -> DetRng {
        let mut rng = DetRng::new(0x0123_4567_89AB_CDEF);
        let mut guest = rng.stream(RngStream::GUEST_ENTROPY);
        guest.next_u32();
        guest.next_u64();
        let mut identity = rng.stream(RngStream::IDENTITY);
        let mut bytes = [0u8; 6];
        identity.fill_bytes(&mut bytes);
        rng
    }

    fn sample_clock() -> Clock {
        let mut clock = Clock::new(160_000_000, 1_000);
        clock.set_counting(0, true);
        clock.rebase(4_000, 80_000_000);
        clock.stall(4_000, 12_500, true);
        clock
    }

    fn sample_journal() -> JournalState {
        let mut journal = Journal::new();
        journal.append(
            VTime(0),
            VTime::from_us(1),
            Origin::Agent,
            InputEvent::Button {
                id: ButtonId::Ok,
                down: true,
            },
        );
        journal.append(
            VTime(0),
            VTime::from_ms(2),
            Origin::Scenario,
            InputEvent::Power { down: false },
        );
        journal.save()
    }

    /// Every window has a non-zero tail, so the section carries cursors and not only bytes.
    fn sample_hostio() -> HostIo {
        let mut io = HostIo::new(512);

        io.usj_rx.push(b"AT+RST\r\n");
        let mut drained = [0u8; 2];
        io.usj_rx.pop(&mut drained);

        io.audio_in
            .write(VTime::from_us(100), 16_000, 1, &[1, -1, 2, -2, 3, -3]);
        let mut out = [0i16; 8];
        io.audio_in.pop_or_silence(&mut out);
        io.audio_in.push(&[4, -4]);
        io.audio_in.count_dropped(3);

        io.usj_ctrl.set_client_open(false);
        io.usj_ctrl.set_line_state(true, false);
        io.usj_ctrl.set_enumeration(UsjEnumeration::Enumerated);

        let line = b"boot\nok\n";
        io.usj_tx.write(line);
        io.lines
            .index(SerialStream::UsjTx, 0, line, VTime::from_us(7));
        io.uart0_tx.write(b"x\n");
        io.lines
            .index(SerialStream::Uart0Tx, 0, b"x\n", VTime::from_us(9));
        io.lines.set_read_cursor(SerialStream::UsjTx, 5);

        io
    }

    fn sample_hostio_section() -> HostIoSection {
        HostIoSection::capture(&sample_hostio())
    }

    fn sample_ledger() -> FidelityLedger {
        let mut ledger = FidelityLedger::default();
        ledger.first_touch(FirstTouch {
            periph: PeriphId(2),
            off: 0x10,
            access: TouchAccess::Read,
            size: 4,
            now: VTime::from_us(3),
            allowlisted: false,
        });
        ledger.note(LedgerSubject::Block(PeriphId(2)), Fidelity::B);
        ledger
    }

    /// Encode, decode, encode again; works for section types without `PartialEq`.
    fn round_trip<T: SnapSection>(value: &T) -> Section {
        let section = value.encode().expect("encodes");
        assert_eq!(section.version, T::VERSION, "{}", T::NAME);
        assert_eq!(section.codec, Codec::Postcard, "{}", T::NAME);
        let again = T::decode(&section)
            .unwrap_or_else(|e| panic!("{} does not decode: {e}", T::NAME))
            .encode()
            .expect("re-encodes");
        assert_eq!(again, section, "{} did not round trip", T::NAME);
        section
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `hart` v1: 32 registers, pc, the CSR block, `wfi`, `insns`, `stores`, the stack monitor.
    const HART_GOLDEN_V1: &str = "\
008088808004f0ffb7fe03000000000000002a0000000000000000000000000000000000000000\
00a082e0810480318180e08104f481e0810407008080a0fe031f0000000000000000000000000000\
00ffffff7f0000000000000000000000000000000000000000000000000000000000000000000103\
0000b960a60501008080a0fe038080b8fe03";

    const SCHED_GOLDEN_V1: &str = "030301000001c096b10201030100000180dac4090204020100";

    const RNG_GOLDEN_V1: &str = "ef9bafcdf8acd191010200030202";

    /// A round trip cannot see a renamed or reordered field; golden bytes can.
    const CLOCK_GOLDEN_V1: &str = "94d2f60ba01fa11fd461e807d4610080e892260100";

    const LEDGER_GOLDEN_V1: &str = "0102100004c08db7010001000201";

    const JOURNAL_CURSOR_GOLDEN_V1: &str = "0002000000000000";

    const JOURNAL_PENDING_GOLDEN_V1: &str = "02c0843d000000020180a8d6b90701010100";

    /// Includes the dropped count (`03` after the underflow count `02`).
    const HOSTIO_GOLDEN_V1_FORMAT5: &str = "\
02062b5253540d0a06020807000180c2d72f807d01000203010000010200000200\
04c09fab030007c09fab03010101c0a8a5040500";

    #[test]
    fn hart_section_golden_bytes() {
        let section = sample_hart().encode().expect("encodes");
        assert_eq!(section.version, 1);
        assert_eq!(section.codec, Codec::Postcard);
        assert_eq!(hex(&section.bytes), HART_GOLDEN_V1);
        assert_eq!(TestHart::section_id(), SectionId::new(SectionId::HART));
    }

    #[test]
    fn every_model_section_of_this_crate_has_golden_bytes() {
        let (cursor, pending) = split_journal(sample_journal());
        let cases: [(&str, Section, &str); 5] = [
            (
                SectionId::CLOCK,
                sample_clock().encode().expect("encodes"),
                CLOCK_GOLDEN_V1,
            ),
            (
                SectionId::LEDGER,
                sample_ledger().encode().expect("encodes"),
                LEDGER_GOLDEN_V1,
            ),
            (
                SectionId::JOURNAL_CURSOR,
                cursor.encode().expect("encodes"),
                JOURNAL_CURSOR_GOLDEN_V1,
            ),
            (
                SectionId::JOURNAL_PENDING,
                pending.encode().expect("encodes"),
                JOURNAL_PENDING_GOLDEN_V1,
            ),
            (
                SectionId::HOSTIO,
                sample_hostio_section().encode().expect("encodes"),
                HOSTIO_GOLDEN_V1_FORMAT5,
            ),
        ];
        for (name, section, golden) in cases {
            assert_eq!(section.version, 1, "{name}");
            assert_eq!(section.codec, Codec::Postcard, "{name}");
            assert_eq!(hex(&section.bytes), golden, "{name}");
        }
        assert_eq!(Clock::section_id(), SectionId::new(SectionId::CLOCK));
        assert_eq!(
            FidelityLedger::section_id(),
            SectionId::new(SectionId::LEDGER)
        );
        assert_eq!(
            JournalCursor::section_id(),
            SectionId::new(SectionId::JOURNAL_CURSOR)
        );
        assert_eq!(
            JournalPending::section_id(),
            SectionId::new(SectionId::JOURNAL_PENDING)
        );
        assert_eq!(
            HostIoSection::section_id(),
            SectionId::new(SectionId::HOSTIO)
        );
    }

    #[test]
    fn sched_section_golden_bytes() {
        let section = sample_sched().encode().expect("encodes");
        assert_eq!(section.version, 1);
        assert_eq!(hex(&section.bytes), SCHED_GOLDEN_V1);
        assert_eq!(Scheduler::section_id(), SectionId::new(SectionId::SCHED));
    }

    #[test]
    fn rng_section_golden_bytes() {
        let section = sample_rng().encode().expect("encodes");
        assert_eq!(section.version, 1);
        assert_eq!(hex(&section.bytes), RNG_GOLDEN_V1);
        assert_eq!(DetRng::section_id(), SectionId::new(SectionId::RNG));
        // GUEST_ENTROPY (0) drew one u32 and one u64, so 3 words; IDENTITY (2) drew 6 bytes, so
        // 2 words. BOARD_NOISE (1) was never drawn from and is absent.
        assert!(RNG_GOLDEN_V1.ends_with("0200030202"));
    }

    #[test]
    fn every_core_section_round_trips() {
        let (cursor, pending) = split_journal(sample_journal());
        round_trip(&sample_sched());
        round_trip(&sample_rng());
        round_trip(&sample_clock());
        round_trip(&cursor);
        round_trip(&pending);
        round_trip(&sample_ledger());
        round_trip(&sample_hostio_section());

        // `DetRng` has no `PartialEq`, so compare what its bytes mean.
        let rng = DetRng::decode(&sample_rng().encode().expect("encodes")).expect("decodes");
        assert_eq!(rng.seed(), sample_rng().seed());
        assert_eq!(
            rng.positions().collect::<Vec<_>>(),
            sample_rng().positions().collect::<Vec<_>>()
        );
        assert_eq!(
            Scheduler::decode(&sample_sched().encode().unwrap()).unwrap(),
            sample_sched()
        );
        assert_eq!(
            Clock::decode(&sample_clock().encode().unwrap()).unwrap(),
            sample_clock()
        );
        assert_eq!(
            FidelityLedger::decode(&sample_ledger().encode().unwrap()).unwrap(),
            sample_ledger()
        );
        assert_eq!(
            JournalCursor::decode(&cursor.encode().unwrap()).unwrap(),
            cursor
        );
        assert_eq!(
            JournalPending::decode(&pending.encode().unwrap()).unwrap(),
            pending
        );
        assert_eq!(
            HostIoSection::decode(&sample_hostio_section().encode().unwrap()).unwrap(),
            sample_hostio_section()
        );

        let hart = sample_hart();
        assert_eq!(
            TestHart::decode(&hart.encode().expect("encodes")).expect("decodes"),
            hart
        );
    }

    #[test]
    fn the_hostio_section_restores_the_channels_it_carries() {
        let section = sample_hostio_section();
        let mut io = HostIo::new(512);
        section.restore_into(&mut io).expect("restores");
        assert_eq!(HostIoSection::capture(&io), section);

        assert_eq!(io.usj_rx.tail(), 2);
        let mut bytes = [0u8; 8];
        let read = io.usj_rx.read(io.usj_rx.tail(), &mut bytes);
        assert_eq!(&bytes[..read.n], b"+RST\r\n");
        assert_eq!(io.audio_in.tail(), 6);
        let mut samples = [0i16; 4];
        assert_eq!(io.audio_in.pop(&mut samples), 2);
        assert_eq!(&samples[..2], &[4, -4]);
        assert_eq!(io.audio_in.underflows(), 2);
        assert_eq!(io.audio_in.dropped(), 3);
        assert!(!io.usj_ctrl.client_open());
        assert!(io.usj_ctrl.rts() && !io.usj_ctrl.dtr());
        assert_eq!(io.lines.lines(SerialStream::UsjTx), 2);
        assert_eq!(io.lines.lines(SerialStream::Uart0Tx), 1);
        assert_eq!(io.lines.read_cursor(SerialStream::UsjTx), 5);

        let mut small = HostIo::new(4);
        let before = HostIoSection::capture(&small);
        assert_eq!(
            section.restore_into(&mut small).unwrap_err(),
            RingRestoreError::TooLong {
                len: 6,
                capacity: 4
            }
        );
        assert_eq!(HostIoSection::capture(&small), before);
    }

    #[test]
    fn the_two_journal_sections_rebuild_the_journal_state() {
        let state = sample_journal();
        let (cursor, pending) = split_journal(state.clone());
        assert_eq!(pending.0.len(), 2);
        assert_eq!(join_journal(cursor, pending), state);
    }

    /// Every section name, the ones other crates own as opaque bytes.
    fn sample_snapshot() -> Snapshot {
        let mut header = SnapHeader::new();
        header.rom_sha256 = [0x11; 32];
        header.image_sha256 = vec![[0x22; 32], [0x33; 32]];
        header.config_hash = [0x44; 32];
        header.efuse_hash = [0x55; 32];
        header.efuse_kind = EfuseKind::Synthesized;

        let mut snap = Snapshot::new(header);
        snap.put(&sample_hostio_section()).expect("hostio");
        snap.put(&sample_sched()).expect("sched");
        snap.put(&sample_rng()).expect("rng");
        snap.put(&sample_clock()).expect("clock");
        let (cursor, pending) = split_journal(sample_journal());
        snap.put(&cursor).expect("journal_cursor");
        snap.put(&pending).expect("journal_pending");
        snap.put(&sample_ledger()).expect("ledger");
        snap.put(&sample_hart()).expect("hart");
        for (name, byte) in [
            (SectionId::RAM, 0xA0u8),
            (SectionId::RTC_RAM, 0xA1),
            (SectionId::FLASH_DELTA, 0xA2),
            (SectionId::HLE, 0xA4),
            (SectionId::BLE, 0xA5),
            (SectionId::WIFI, 0xA6),
            (SectionId::LAN, 0xA7),
        ] {
            snap.put_raw(
                SectionId::new(name),
                Section {
                    version: 1,
                    codec: Codec::Postcard,
                    bytes: vec![byte; 4],
                },
            );
        }
        snap.put_raw(
            SectionId::soc("systimer"),
            Section {
                version: 2,
                codec: Codec::Postcard,
                bytes: vec![0xB0, 0xB1],
            },
        );
        snap.put_raw(
            SectionId::board("st7789"),
            Section {
                version: 3,
                codec: Codec::Postcard,
                bytes: vec![0xC0],
            },
        );
        snap
    }

    #[test]
    fn the_container_round_trips_every_section() {
        let snap = sample_snapshot();
        assert_eq!(snap.sections.len(), 17);
        let bytes = snap.to_bytes().expect("writes");
        assert_eq!(&bytes[..MAGIC.len()], &MAGIC);
        let back = Snapshot::from_bytes(&bytes).expect("reads");
        assert_eq!(back, snap);
        assert_eq!(back.get::<Scheduler>().expect("sched"), sample_sched());
        assert_eq!(
            back.get::<DetRng>()
                .expect("rng")
                .positions()
                .collect::<Vec<_>>(),
            sample_rng().positions().collect::<Vec<_>>()
        );
        assert_eq!(back.get::<Clock>().expect("clock"), sample_clock());
        assert_eq!(
            back.get::<FidelityLedger>().expect("ledger"),
            sample_ledger()
        );
        assert_eq!(
            back.section(&SectionId::soc("systimer"))
                .expect("soc")
                .bytes,
            vec![0xB0, 0xB1]
        );
    }

    #[test]
    fn the_writer_never_emits_a_stream_this_build_refuses() {
        let mut snap = sample_snapshot();
        snap.header.format = 9_999;
        let bytes = snap.to_bytes().expect("writes");
        let back = Snapshot::from_bytes(&bytes).expect("reads");
        assert_eq!(back.header.format, FORMAT_VERSION);
        assert_eq!(back.sections, snap.sections);
        assert_eq!(back.canonical_hash(), snap.canonical_hash());

        let mut huge = Snapshot::new(SnapHeader::new());
        huge.put_raw(
            SectionId::new(SectionId::RAM),
            Section {
                version: 1,
                codec: Codec::Postcard,
                bytes: vec![0; MAX_SECTION_LEN as usize + 1],
            },
        );
        assert_eq!(
            huge.to_bytes().unwrap_err(),
            SnapError::Malformed {
                at: "section table",
                reason: "a section length is past what a snapshot can hold",
            }
        );
    }

    #[test]
    fn a_missing_section_is_named() {
        let snap = Snapshot::new(SnapHeader::new());
        assert_eq!(
            snap.get::<Scheduler>().unwrap_err(),
            SnapError::MissingSection(SectionId::new(SectionId::SCHED))
        );
    }

    #[test]
    fn a_stream_that_is_not_a_snapshot_is_refused_by_its_magic() {
        assert_eq!(Snapshot::from_bytes(&[]).unwrap_err(), SnapError::BadMagic);
        assert_eq!(
            Snapshot::from_bytes(b"not a snapshot at all").unwrap_err(),
            SnapError::BadMagic
        );
        let mut bytes = sample_snapshot().to_bytes().expect("writes");
        bytes[7] ^= 0xFF;
        assert_eq!(
            Snapshot::from_bytes(&bytes).unwrap_err(),
            SnapError::BadMagic
        );
    }

    /// Each older format changed a section layout without a section version bump, so the whole
    /// container is refused rather than misdecoded.
    #[test]
    fn a_format_5_through_27_container_is_refused() {
        assert_eq!(FORMAT_VERSION, 28);
        for older in [
            5u16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
            27,
        ] {
            let mut bytes = sample_snapshot().to_bytes().expect("writes");
            bytes[MAGIC.len()..MAGIC.len() + 2].copy_from_slice(&older.to_le_bytes());
            assert_eq!(
                Snapshot::from_bytes(&bytes).unwrap_err(),
                SnapError::UnsupportedFormat {
                    found: older,
                    supported: FORMAT_VERSION,
                }
            );
        }
    }

    #[test]
    fn an_unknown_container_format_is_refused() {
        let mut bytes = sample_snapshot().to_bytes().expect("writes");
        bytes[MAGIC.len()..MAGIC.len() + 2].copy_from_slice(&9_999u16.to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&bytes).unwrap_err(),
            SnapError::UnsupportedFormat {
                found: 9_999,
                supported: FORMAT_VERSION,
            }
        );
    }

    #[test]
    fn every_truncation_of_a_snapshot_is_refused_without_a_panic() {
        let bytes = sample_snapshot().to_bytes().expect("writes");
        for len in 0..bytes.len() {
            let error = Snapshot::from_bytes(&bytes[..len])
                .expect_err("a prefix of a snapshot is not a snapshot");
            assert!(
                matches!(
                    error,
                    SnapError::BadMagic | SnapError::Truncated { .. } | SnapError::Malformed { .. }
                ),
                "prefix of {len} B gave {error}"
            );
        }
        assert!(Snapshot::from_bytes(&bytes).is_ok());
    }

    /// Both codec paths: postcard and [`SnapValue`].
    #[test]
    fn a_truncated_section_payload_is_refused() {
        let mut section = sample_sched().encode().expect("encodes");
        section.bytes.truncate(section.bytes.len() - 1);
        assert_eq!(
            Scheduler::decode(&section).unwrap_err(),
            SnapError::Truncated {
                at: SectionId::SCHED
            }
        );

        let mut section = sample_hart().encode().expect("encodes");
        section.bytes.truncate(section.bytes.len() - 1);
        assert_eq!(
            TestHart::decode(&section).unwrap_err(),
            SnapError::Truncated {
                at: SectionId::HART
            }
        );
        let full = sample_hart().encode().expect("encodes");
        for len in 0..full.bytes.len() {
            let cut = Section {
                bytes: full.bytes[..len].to_vec(),
                ..full.clone()
            };
            assert!(TestHart::decode(&cut).is_err(), "prefix of {len} B decoded");
        }
    }

    #[test]
    fn an_unknown_section_version_is_refused() {
        let mut section = sample_sched().encode().expect("encodes");
        section.version = 7;
        assert_eq!(
            Scheduler::decode(&section).unwrap_err(),
            SnapError::Incompatible {
                section: SectionId::new(SectionId::SCHED),
                found: 7,
                expected: 1,
            }
        );

        let mut section = sample_hart().encode().expect("encodes");
        section.version = 0;
        assert_eq!(
            TestHart::decode(&section).unwrap_err(),
            SnapError::Incompatible {
                section: SectionId::new(SectionId::HART),
                found: 0,
                expected: 1,
            }
        );
    }

    #[test]
    fn bytes_past_a_section_value_are_refused() {
        let mut section = sample_sched().encode().expect("encodes");
        section.bytes.push(0);
        assert_eq!(
            Scheduler::decode(&section).unwrap_err(),
            SnapError::TrailingBytes {
                section: SectionId::new(SectionId::SCHED),
            }
        );

        let mut section = sample_hart().encode().expect("encodes");
        section.bytes.push(0);
        assert_eq!(
            TestHart::decode(&section).unwrap_err(),
            SnapError::TrailingBytes {
                section: SectionId::new(SectionId::HART),
            }
        );
    }

    #[test]
    fn a_codec_this_build_cannot_decode_is_refused() {
        let mut snap = Snapshot::new(SnapHeader::new());
        snap.put_raw(
            SectionId::new(SectionId::RAM),
            Section {
                version: 1,
                codec: Codec::PostcardLz4,
                bytes: vec![0; 4],
            },
        );
        assert_eq!(
            snap.to_bytes().unwrap_err(),
            SnapError::UnsupportedCodec {
                section: SectionId::new(SectionId::RAM),
                codec: Codec::PostcardLz4,
            }
        );

        // The same refusal on the way in, from a stream a future build wrote.
        let mut bytes = Snapshot::new(SnapHeader::new()).to_bytes().expect("writes");
        bytes.truncate(bytes.len() - 4);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        write_section(
            &SectionId::new(SectionId::RAM),
            &Section {
                version: 1,
                codec: Codec::Postcard,
                bytes: vec![0; 4],
            },
            Codec::PostcardLz4,
            &mut bytes,
        );
        assert_eq!(
            Snapshot::from_bytes(&bytes).unwrap_err(),
            SnapError::UnsupportedCodec {
                section: SectionId::new(SectionId::RAM),
                codec: Codec::PostcardLz4,
            }
        );
    }

    #[test]
    fn a_section_named_twice_is_refused() {
        let section = Section {
            version: 1,
            codec: Codec::Postcard,
            bytes: vec![1, 2, 3],
        };
        let id = SectionId::new(SectionId::RAM);
        let mut bytes = Snapshot::new(SnapHeader::new()).to_bytes().expect("writes");
        bytes.truncate(bytes.len() - 4);
        bytes.extend_from_slice(&2u32.to_le_bytes());
        write_section(&id, &section, Codec::Postcard, &mut bytes);
        write_section(&id, &section, Codec::Postcard, &mut bytes);
        assert_eq!(
            Snapshot::from_bytes(&bytes).unwrap_err(),
            SnapError::DuplicateSection(id)
        );
    }

    #[test]
    fn a_section_length_past_the_end_is_refused() {
        let mut bytes = Snapshot::new(SnapHeader::new()).to_bytes().expect("writes");
        bytes.truncate(bytes.len() - 4);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(b"ram");
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(Codec::Postcard.tag());
        let mut huge = bytes.clone();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&huge).unwrap_err(),
            SnapError::Malformed {
                at: "section table",
                reason: "a section length is past what a snapshot can hold",
            }
        );
        // Below the cap but past the bytes that are there: truncation, not a huge allocation.
        let mut long = bytes;
        long.extend_from_slice(&(MAX_SECTION_LEN - 1).to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&long).unwrap_err(),
            SnapError::Truncated { at: "container" }
        );
    }

    #[test]
    fn restore_refuses_an_unknown_required_section_and_skips_an_optional_one() {
        let mut snap = sample_snapshot();
        // A build with no BLE world does not know `ble`, which is optional, and does not know
        // `soc.newthing`, which a newer build wrote and which is required.
        let known = |id: &SectionId| id.as_str() != SectionId::BLE;
        let plan = snap
            .plan_restore(|id| id.fate(known(id)))
            .expect("an unknown optional section is skipped");
        assert_eq!(plan.skipped, vec![SectionId::new(SectionId::BLE)]);
        assert_eq!(plan.apply.len(), snap.sections.len() - 1);
        assert!(!plan.apply.contains(&SectionId::new(SectionId::BLE)));

        snap.put_raw(
            SectionId::soc("newthing"),
            Section {
                version: 1,
                codec: Codec::Postcard,
                bytes: vec![0],
            },
        );
        assert_eq!(
            snap.plan_restore(|id| id.fate(known(id) && id.as_str() != "soc.newthing"))
                .unwrap_err(),
            SnapError::UnknownSection(SectionId::soc("newthing"))
        );
    }

    #[test]
    fn only_the_documented_sections_are_optional() {
        for name in [
            SectionId::LEDGER,
            SectionId::BLE,
            SectionId::WIFI,
            SectionId::LAN,
        ] {
            assert!(SectionId::new(name).is_optional(), "{name}");
            assert_eq!(SectionId::new(name).fate(false), SectionFate::Skip);
            assert_eq!(SectionId::new(name).fate(true), SectionFate::Apply);
        }
        for name in [
            SectionId::HART,
            SectionId::RAM,
            SectionId::RTC_RAM,
            SectionId::FLASH_DELTA,
            SectionId::SCHED,
            SectionId::RNG,
            SectionId::CLOCK,
            SectionId::JOURNAL_CURSOR,
            SectionId::JOURNAL_PENDING,
            SectionId::HOSTIO,
            SectionId::HLE,
        ] {
            assert!(!SectionId::new(name).is_optional(), "{name}");
            assert_eq!(SectionId::new(name).fate(false), SectionFate::Unknown);
        }
        assert_eq!(SectionId::soc("gpio").fate(false), SectionFate::Unknown);
        assert_eq!(SectionId::board("cw2017").fate(false), SectionFate::Unknown);
        assert_eq!(SectionId::soc("gpio").as_str(), "soc.gpio");
        assert_eq!(SectionId::board("cw2017").as_str(), "board.cw2017");
    }

    #[test]
    fn the_state_hash_is_over_the_parsed_structure_not_the_file_bytes() {
        let snap = sample_snapshot();
        let ordered = snap.to_bytes().expect("writes");

        // A stream with the same header and sections, but the table written back to front.
        let header = postcard::to_stdvec(&snap.header).expect("header");
        let mut shuffled = Vec::new();
        shuffled.extend_from_slice(&MAGIC);
        shuffled.extend_from_slice(&snap.header.format.to_le_bytes());
        shuffled.extend_from_slice(&0u16.to_le_bytes());
        shuffled.extend_from_slice(&(header.len() as u32).to_le_bytes());
        shuffled.extend_from_slice(&header);
        shuffled.extend_from_slice(&(snap.sections.len() as u32).to_le_bytes());
        for (id, section) in snap.sections.iter().rev() {
            write_section(id, section, section.codec, &mut shuffled);
        }
        assert_ne!(shuffled, ordered, "the two files differ byte for byte");

        let a = Snapshot::from_bytes(&ordered).expect("reads");
        let b = Snapshot::from_bytes(&shuffled).expect("reads");
        assert_eq!(a, b);
        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
        assert_eq!(a.canonical_hash(), b.canonical_hash());
        assert_eq!(a.run_identity(), b.run_identity());

        // A section marked compressed is the same state: `Section::bytes` is always decoded.
        let mut compressed = a.clone();
        for name in [SectionId::RAM, SectionId::FLASH_DELTA, SectionId::HOSTIO] {
            compressed
                .sections
                .get_mut(&SectionId::new(name))
                .expect("section")
                .codec = Codec::PostcardLz4;
        }
        assert_ne!(
            compressed.sections, a.sections,
            "the two differ in the codec"
        );
        assert_eq!(compressed.canonical_bytes(), a.canonical_bytes());
        assert_eq!(compressed.canonical_hash(), a.canonical_hash());
        assert_eq!(compressed.run_identity(), a.run_identity());
    }

    #[test]
    fn the_state_hash_covers_the_sections_and_the_identity_covers_the_assets() {
        let snap = sample_snapshot();
        let mut other_build = snap.clone();
        other_build.header.core_version = "99.0.0".to_string();
        other_build.header.rom_sha256 = [0xEE; 32];
        assert_eq!(other_build.canonical_hash(), snap.canonical_hash());
        assert_ne!(other_build.run_identity(), snap.run_identity());

        let mut moved = snap.clone();
        moved.put(&Clock::new(80_000_000, 1_000)).expect("clock");
        assert_ne!(moved.canonical_hash(), snap.canonical_hash());
        assert_eq!(moved.run_identity(), snap.run_identity());
    }

    #[test]
    fn the_run_identity_follows_the_journal() {
        let snap = sample_snapshot();
        let mut longer = snap.clone();
        let mut journal = Journal::new();
        journal.append(
            VTime(0),
            VTime::from_us(1),
            Origin::Agent,
            InputEvent::Button {
                id: ButtonId::Up,
                down: true,
            },
        );
        let (cursor, pending) = split_journal(journal.save());
        longer.put(&cursor).expect("journal_cursor");
        longer.put(&pending).expect("journal_pending");
        assert_ne!(longer.run_identity(), snap.run_identity());

        let mut assets_only = Snapshot::new(snap.header.clone());
        assert_ne!(assets_only.run_identity(), [0; 32]);
        assets_only.header.config_hash = [0x77; 32];
        assert_ne!(
            assets_only.run_identity(),
            Snapshot::new(snap.header).run_identity()
        );
    }

    #[test]
    fn the_run_identity_follows_the_persisted_flash_deltas() {
        fn with_delta(bytes: Vec<u8>) -> Snapshot {
            let mut snap = Snapshot::new(SnapHeader::new());
            snap.put_raw(
                SectionId::new(SectionId::FLASH_DELTA),
                Section {
                    version: 1,
                    codec: Codec::Postcard,
                    bytes,
                },
            );
            snap
        }
        assert_ne!(
            with_delta(vec![1]).run_identity(),
            with_delta(vec![2]).run_identity()
        );
        assert_ne!(
            with_delta(vec![1]).run_identity(),
            with_delta(Vec::new()).run_identity()
        );
        assert_ne!(
            with_delta(Vec::new()).run_identity(),
            Snapshot::new(SnapHeader::new()).run_identity()
        );

        // Guest RAM is state, not identity.
        let snap = sample_snapshot();
        let mut ram = snap.clone();
        ram.put_raw(
            SectionId::new(SectionId::RAM),
            Section {
                version: 1,
                codec: Codec::Postcard,
                bytes: vec![0x0F; 4],
            },
        );
        assert_eq!(ram.run_identity(), snap.run_identity());
        assert_ne!(ram.canonical_hash(), snap.canonical_hash());
    }

    #[test]
    fn the_header_identity_is_the_assets_and_not_the_build() {
        let base = SnapHeader::new();
        let mut same = base.clone();
        same.core_version = "0.0.0-other".to_string();
        same.format = base.format;
        same.exported = true;
        same.redacted = true;
        assert_eq!(same.identity_hash(), base.identity_hash());

        for change in [
            |h: &mut SnapHeader| h.rom_sha256 = [1; 32],
            |h: &mut SnapHeader| h.image_sha256 = vec![[2; 32]],
            |h: &mut SnapHeader| h.config_hash = [3; 32],
            |h: &mut SnapHeader| h.efuse_hash = [4; 32],
            |h: &mut SnapHeader| h.efuse_kind = EfuseKind::Imported,
        ] {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(changed.identity_hash(), base.identity_hash());
        }
    }

    /// Every leaf type [`SnapValue`] encodes, once.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
    struct TestLeaves {
        a: u8,
        b: u16,
        c: u32,
        d: u64,
        e: i8,
        f: i16,
        g: i32,
        h: i64,
        flag: bool,
        text: String,
        list: Vec<u32>,
        maybe: Option<u64>,
        fixed: [u8; 3],
        nothing: Option<String>,
        nested_list: Vec<Vec<i16>>,
    }
    snap_struct!(TestLeaves {
        a,
        b,
        c,
        d,
        e,
        f,
        g,
        h,
        flag,
        text,
        list,
        maybe,
        fixed,
        nothing,
        nested_list
    });
    value_test_section!(TestLeaves, "test.leaves", 1);

    /// A section at a version other than 1, with a field that is itself a struct.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
    struct TestNested {
        count: u32,
        flag: bool,
        spmon: TestSpMonitor,
    }
    snap_struct!(TestNested { count, flag, spmon });
    value_test_section!(TestNested, "test.nested", 4);

    fn sample_leaves() -> TestLeaves {
        TestLeaves {
            a: 0xFE,
            b: 0xBEEF,
            c: 0xDEAD_BEEF,
            d: u64::MAX - 3,
            e: -128,
            f: i16::MIN,
            g: -1,
            h: i64::MIN,
            flag: true,
            text: "soc.systimer".to_string(),
            list: vec![0, 1, 300, u32::MAX],
            maybe: Some(1 << 40),
            fixed: [7, 8, 9],
            nothing: None,
            nested_list: vec![vec![-1, 0, 1], Vec::new()],
        }
    }

    fn sample_nested() -> TestNested {
        TestNested {
            count: 1_000_000,
            flag: false,
            spmon: TestSpMonitor {
                on_min: true,
                on_max: false,
                min: 0x3FC8_0000,
                max: 0x3FCE_0000,
            },
        }
    }

    /// Otherwise one state would have two spellings.
    #[test]
    fn snap_struct_bytes_match_postcard() {
        for (name, derived, serialized) in [
            (
                "hart",
                sample_hart().encode().expect("encodes").bytes,
                postcard::to_stdvec(&sample_hart()).expect("postcard"),
            ),
            (
                "test.leaves",
                sample_leaves().encode().expect("encodes").bytes,
                postcard::to_stdvec(&sample_leaves()).expect("postcard"),
            ),
            (
                "test.nested",
                sample_nested().encode().expect("encodes").bytes,
                postcard::to_stdvec(&sample_nested()).expect("postcard"),
            ),
        ] {
            assert_eq!(hex(&derived), hex(&serialized), "{name}");
        }
    }

    #[test]
    fn snap_struct_round_trips_every_leaf_type() {
        let value = sample_leaves();
        let section = round_trip(&value);
        assert_eq!(section.version, 1);
        assert_eq!(TestLeaves::decode(&section).expect("decodes"), value);

        let nested = sample_nested();
        assert_eq!(TestNested::VERSION, 4);
        assert_eq!(
            TestNested::decode(&round_trip(&nested)).expect("decodes"),
            nested
        );
    }

    #[test]
    fn the_leaf_encodings_are_canonical() {
        fn written<T: SnapValue>(v: T) -> String {
            let mut out = Vec::new();
            v.snap_write(&mut out);
            hex(&out)
        }
        assert_eq!(written(0u8), "00");
        assert_eq!(written(0xFFu8), "ff");
        assert_eq!(written(0u32), "00");
        assert_eq!(written(127u32), "7f");
        assert_eq!(written(128u32), "8001");
        assert_eq!(written(u32::MAX), "ffffffff0f");
        assert_eq!(written(u64::MAX), "ffffffffffffffffff01");
        assert_eq!(written(0i32), "00");
        assert_eq!(written(-1i32), "01");
        assert_eq!(written(1i32), "02");
        assert_eq!(written(-2i32), "03");
        assert_eq!(written(true), "01");
        assert_eq!(written(false), "00");
        assert_eq!(written([1u8, 2, 3]), "010203");
        assert_eq!(written(vec![1u8, 2, 3]), "03010203");
        assert_eq!(written(Option::<u32>::None), "00");
        assert_eq!(written(Some(1u32)), "0101");
        assert_eq!(written("ok".to_string()), "026f6b");
    }

    #[test]
    fn a_value_that_is_not_this_sections_is_refused() {
        fn read_u32(bytes: &[u8]) -> Result<u32, SnapError> {
            u32::snap_read(&mut SnapReader::new(bytes, "test"))
        }
        // 0x8000_0000_0 would need a sixth byte: past what a u32 varint may be.
        assert_eq!(
            read_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]).unwrap_err(),
            SnapError::Malformed {
                at: "test",
                reason: "varint longer than its type allows",
            }
        );
        // Five bytes, but the value is 2^35: it does not fit a u32.
        assert_eq!(
            read_u32(&[0x80, 0x80, 0x80, 0x80, 0x80]).unwrap_err(),
            SnapError::Malformed {
                at: "test",
                reason: "varint longer than its type allows",
            }
        );
        assert_eq!(
            read_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]).unwrap_err(),
            SnapError::Malformed {
                at: "test",
                reason: "integer does not fit its type",
            }
        );
        assert_eq!(read_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]), Ok(u32::MAX));

        // Overlong: `[0x80, 0x00]` is a two-byte spelling of 0, and
        // `[0xFF, 0xFF, 0xFF, 0xFF, 0x00]` a five-byte spelling of a four-byte value.
        for overlong in [
            &[0x80u8, 0x00][..],
            &[0x81, 0x00][..],
            &[0xFF, 0xFF, 0xFF, 0xFF, 0x00][..],
        ] {
            assert_eq!(
                read_u32(overlong).unwrap_err(),
                SnapError::Malformed {
                    at: "test",
                    reason: "varint is not minimally encoded",
                },
                "{overlong:02x?}"
            );
        }
        assert_eq!(read_u32(&[0x00]), Ok(0));
        assert_eq!(read_u32(&[0x01]), Ok(1));
        assert_eq!(read_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]), Ok(u32::MAX));
        assert_eq!(
            u64::snap_read(&mut SnapReader::new(
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
                "test"
            )),
            Ok(u64::MAX)
        );

        assert_eq!(
            bool::snap_read(&mut SnapReader::new(&[2], "test")).unwrap_err(),
            SnapError::Malformed {
                at: "test",
                reason: "bool is neither 0 nor 1",
            }
        );
        assert_eq!(
            Option::<u8>::snap_read(&mut SnapReader::new(&[2], "test")).unwrap_err(),
            SnapError::Malformed {
                at: "test",
                reason: "Option discriminant is neither 0 nor 1",
            }
        );
        assert_eq!(
            String::snap_read(&mut SnapReader::new(&[2, 0xFF, 0xFE], "test")).unwrap_err(),
            SnapError::Malformed {
                at: "test",
                reason: "string is not UTF-8",
            }
        );
        assert_eq!(
            Vec::<u64>::snap_read(&mut SnapReader::new(&[0xFF, 0xFF, 0x7F], "test")).unwrap_err(),
            SnapError::Truncated { at: "test" }
        );
    }

    #[test]
    fn every_error_names_what_is_wrong() {
        assert_eq!(
            SnapError::BadMagic.to_string(),
            "not a snapshot: wrong magic"
        );
        assert!(
            SnapError::UnknownSection(SectionId::soc("gpio"))
                .to_string()
                .contains("soc.gpio")
        );
        assert!(
            SnapError::LiveBridge {
                bridge: "virtual-LAN bridge".to_string(),
            }
            .to_string()
            .contains("virtual-LAN bridge")
        );
        assert!(
            SnapError::Incompatible {
                section: SectionId::new(SectionId::HART),
                found: 2,
                expected: 1,
            }
            .to_string()
            .contains("version 2")
        );
    }

    #[test]
    fn the_defaults_are_the_safe_ones() {
        assert_eq!(LivePolicy::default(), LivePolicy::Refuse);
        assert_eq!(
            SnapOpts::default(),
            SnapOpts {
                export: false,
                include_secrets: false,
            }
        );
        assert_eq!(SnapHeader::new().efuse_kind, EfuseKind::Synthesized);
        assert_eq!(SnapHeader::new().format, FORMAT_VERSION);
        assert!(!SnapHeader::new().exported);
    }

    #[derive(Serialize, Deserialize)]
    struct Window {
        #[serde(deserialize_with = "exact_vec::<_, _, 4>")]
        words: Vec<u32>,
        #[serde(deserialize_with = "exact_boxed::<_, _, 2>")]
        pixels: Box<[u16]>,
    }

    #[test]
    fn an_exact_length_field_refuses_another_length() {
        let good = Window {
            words: vec![1, 2, 3, 4],
            pixels: vec![5, 6].into_boxed_slice(),
        };
        let section = serde_section(&good, 1, "window").unwrap();
        let id = SectionId::new("window");
        let back: Window = serde_from_section(&section, id.clone(), 1, "window").unwrap();
        assert_eq!(
            (back.words, &*back.pixels),
            (vec![1, 2, 3, 4], &[5u16, 6][..])
        );
        for bad in [
            Window {
                words: vec![1, 2, 3],
                pixels: vec![5, 6].into_boxed_slice(),
            },
            Window {
                words: vec![1, 2, 3, 4],
                pixels: vec![5, 6, 7].into_boxed_slice(),
            },
        ] {
            let section = serde_section(&bad, 1, "window").unwrap();
            assert!(matches!(
                serde_from_section::<Window>(&section, id.clone(), 1, "window"),
                Err(SnapError::Malformed { .. })
            ));
        }
    }
}
