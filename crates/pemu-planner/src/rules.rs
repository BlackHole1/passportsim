//! The refusal rules of the flash-to-device planner, the device facts they judge, and the flash
//! layout constants. Pure: no port, no file, no process. A fact that is absent or does not parse
//! refuses; no rule guesses in the permissive direction.

use pemu_loader::bundle::TomlLite;
use pemu_loader::partitions::{Partition, ptype, subtype};

/// Flash sector size. The flasher stub erases every sector a write covers, so every write range
/// is rounded out to it.
pub const SECTOR: u32 = 0x1000;
pub const FLASH_SIZE: u32 = 0x80_0000;
pub const BOOTLOADER_OFFSET: u32 = 0x0;
pub const BOOTLOADER_END: u32 = 0x8000;
pub const TABLE_OFFSET: u32 = 0x8000;
/// End of the partition table range; `nvs` starts here.
pub const TABLE_END: u32 = 0x9000;
/// The largest partition table `verify_firmware.py` reads.
pub const TABLE_LEN: u32 = 0xC00;
pub const APP_OFFSET: u32 = 0x1_0000;
/// The factory partition size and the largest app the planner writes.
pub const APP_MAX: u32 = 0x30_0000;
/// The protected identity partition.
pub const CARDID_NAME: &str = "cardid";
pub const CARDID_OFFSET: u32 = 0x35_6000;
pub const CARDID_SIZE: u32 = 0x4000;
/// One past the end of the protected window, [0x356000, 0x35A000).
pub const CARDID_END: u32 = CARDID_OFFSET + CARDID_SIZE;
/// Start of the flash left over between the official `factory` end and cardid; nothing in the
/// official layout lives in [0x310000, 0x356000), so a write there is called out in the prompt.
pub const LEFTOVER_START: u32 = 0x31_0000;
pub const LEFTOVER_END: u32 = CARDID_OFFSET;
/// The chip revision the device must report.
pub const CHIP_REVISION: ChipRevision = ChipRevision { major: 1, minor: 1 };
pub const FLASH_MANUFACTURER: u8 = 0x20;
pub const FLASH_DEVICE: u16 = 0x4017;
/// Discovery matches on Espressif's USB Serial/JTAG VID:PID.
pub const USB_VID: u16 = 0x303A;
pub const USB_PID: u16 = 0x1001;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChipRevision {
    pub major: u8,
    pub minor: u8,
}

impl ChipRevision {
    pub fn parse(text: &str) -> Option<ChipRevision> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (major, minor) = text.split_once('.')?;
        Some(ChipRevision {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

impl core::fmt::Display for ChipRevision {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
}

/// One refusal rule. Its stable id is used by receipts, command output and the one-test-per-rule
/// suite.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    ChipRevision,
    FlashId,
    CardidMissing,
    /// The device partition table has `cardid` somewhere other than 0x356000 size 0x4000.
    CardidMoved,
    /// The image's partition table moves, shrinks or drops `cardid`.
    ImageMovesCardid,
    /// A write range, rounded out to 4 KB sectors, touches [0x356000, 0x35A000).
    CardidOverlap,
    BeyondFlash,
    AppTooLarge,
    /// Non-0xFF image bytes anywhere the plan would not write, which it would silently drop.
    DataDropped,
    /// A segment at an offset other than the bootloader, the partition table or an app partition.
    SegmentNotAllowed,
    /// Two writes overlap once rounded out to sectors, or the segment list has no single
    /// partition table at 0x8000 or more than one bootloader (a second table could replace the
    /// checked one on the device).
    SegmentOverlap,
    /// A write, rounded out to sectors, touches a data partition of the device's own table
    /// (`nvs`, `phy_init`, `cardid` or any other data partition).
    WritesDeviceData,
    /// The image table's data partitions differ from the device's (offset, size, type or subtype);
    /// only a person's `--erase-nvs` may change `nvs`.
    DataLayoutMismatch,
    EraseRequested,
    EraseNvsNotHuman,
    VerifyFirmware,
    NoVerifiedBackup,
    BackupNotOwnerOnly,
    BackupInsideRepository,
    NoDevice,
    AmbiguousPort,
    /// `--port` names no discovered device, or a path the host does not accept as a flashing port
    /// (on macOS only a call-out device, on Windows only a `COM<n>` name).
    PortNotAllowed,
    EsptoolScript,
    EsptoolNotAllowed,
    /// An emulator port outside `socket://127.0.0.1:*` and `rfc2217://127.0.0.1:*`, or a
    /// `socket://` port used for flashing.
    EmulatorPort,
    ConfirmationMismatch,
}

impl Rule {
    pub const ALL: [Rule; 26] = [
        Rule::ChipRevision,
        Rule::FlashId,
        Rule::CardidMissing,
        Rule::CardidMoved,
        Rule::ImageMovesCardid,
        Rule::CardidOverlap,
        Rule::BeyondFlash,
        Rule::AppTooLarge,
        Rule::DataDropped,
        Rule::SegmentNotAllowed,
        Rule::SegmentOverlap,
        Rule::WritesDeviceData,
        Rule::DataLayoutMismatch,
        Rule::EraseRequested,
        Rule::EraseNvsNotHuman,
        Rule::VerifyFirmware,
        Rule::NoVerifiedBackup,
        Rule::BackupNotOwnerOnly,
        Rule::BackupInsideRepository,
        Rule::NoDevice,
        Rule::AmbiguousPort,
        Rule::PortNotAllowed,
        Rule::EsptoolScript,
        Rule::EsptoolNotAllowed,
        Rule::EmulatorPort,
        Rule::ConfirmationMismatch,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Rule::ChipRevision => "chip_revision",
            Rule::FlashId => "flash_id",
            Rule::CardidMissing => "cardid_missing",
            Rule::CardidMoved => "cardid_moved",
            Rule::ImageMovesCardid => "image_moves_cardid",
            Rule::CardidOverlap => "cardid_overlap",
            Rule::BeyondFlash => "beyond_flash",
            Rule::AppTooLarge => "app_too_large",
            Rule::DataDropped => "data_dropped",
            Rule::SegmentNotAllowed => "segment_not_allowed",
            Rule::SegmentOverlap => "segment_overlap",
            Rule::WritesDeviceData => "writes_device_data",
            Rule::DataLayoutMismatch => "data_layout_mismatch",
            Rule::EraseRequested => "erase_requested",
            Rule::EraseNvsNotHuman => "erase_nvs_not_human",
            Rule::VerifyFirmware => "verify_firmware",
            Rule::NoVerifiedBackup => "no_verified_backup",
            Rule::BackupNotOwnerOnly => "backup_not_owner_only",
            Rule::BackupInsideRepository => "backup_inside_repository",
            Rule::NoDevice => "no_device",
            Rule::AmbiguousPort => "ambiguous_port",
            Rule::PortNotAllowed => "port_not_allowed",
            Rule::EsptoolScript => "esptool_script",
            Rule::EsptoolNotAllowed => "esptool_not_allowed",
            Rule::EmulatorPort => "emulator_port",
            Rule::ConfirmationMismatch => "confirmation_mismatch",
        }
    }
}

/// One fired rule. The detail names offsets, sizes and rule facts only, never a device identity
/// value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub rule: Rule,
    pub detail: String,
}

impl Refusal {
    pub fn new(rule: Rule, detail: impl Into<String>) -> Refusal {
        Refusal {
            rule,
            detail: detail.into(),
        }
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.rule.id(), self.detail)
    }
}

/// `[start, end)` rounded out to whole sectors. `u64`, so a hostile `offset + len` cannot wrap.
pub fn round_to_sectors(start: u64, end: u64) -> (u64, u64) {
    let sector = u64::from(SECTOR);
    let s = start / sector * sector;
    let e = end.div_ceil(sector) * sector;
    (s, e)
}

pub fn touches_cardid(start: u64, end: u64) -> bool {
    start < u64::from(CARDID_END) && end > u64::from(CARDID_OFFSET)
}

/// The synthetic cardid the rehearsal target carries: 0x4000 bytes of a fixed non-0xFF sequence
/// that belongs to no device. An erased window cannot show a stray stub erase; a seeded one shows
/// it byte for byte.
pub fn synthetic_cardid() -> Vec<u8> {
    (0..CARDID_SIZE)
        .map(|i| (i as u8).wrapping_mul(37) ^ 0x5C)
        .collect()
}

/// A copy of a merged `image` whose cardid window carries [`synthetic_cardid`], for the rehearsal
/// target only; the plan is built from the original. A short (unpadded) image is first grown with
/// 0xFF past the window, so it too gets a seeded window.
pub fn with_synthetic_cardid(image: &[u8]) -> Vec<u8> {
    let mut seeded = image.to_vec();
    let (start, end) = (CARDID_OFFSET as usize, CARDID_END as usize);
    if seeded.len() < end {
        seeded.resize(end, 0xFF);
    }
    seeded[start..end].copy_from_slice(&synthetic_cardid());
    seeded
}

/// Refusals for one write range `[offset, offset + len)`: rounded out to sectors, it must not
/// touch cardid and must not pass 8 MB.
pub fn check_write_range(name: &str, offset: u32, len: u64) -> Vec<Refusal> {
    let mut out = Vec::new();
    let raw_end = u64::from(offset) + len;
    if raw_end > u64::from(FLASH_SIZE) {
        out.push(Refusal::new(
            Rule::BeyondFlash,
            format!("`{name}` at {offset:#x} ends at {raw_end:#x}, beyond {FLASH_SIZE:#x}"),
        ));
        return out;
    }
    let (s, e) = round_to_sectors(u64::from(offset), raw_end);
    if touches_cardid(s, e) {
        out.push(Refusal::new(
            Rule::CardidOverlap,
            format!(
                "`{name}` rounds out to [{s:#x}, {e:#x}), which touches cardid [{CARDID_OFFSET:#x}, {CARDID_END:#x})"
            ),
        ));
    }
    if e > u64::from(FLASH_SIZE) {
        out.push(Refusal::new(
            Rule::BeyondFlash,
            format!("`{name}` rounds out past {FLASH_SIZE:#x}"),
        ));
    }
    out
}

fn cardid_in_place(partitions: &[Partition]) -> Result<(), (Rule, String)> {
    let Some(cardid) = partitions.iter().find(|p| p.name == CARDID_NAME) else {
        return Err((Rule::CardidMissing, "no `cardid` entry".to_owned()));
    };
    if cardid.offset != CARDID_OFFSET || cardid.size != CARDID_SIZE {
        return Err((
            Rule::CardidMoved,
            format!(
                "`cardid` is at {:#x} size {:#x}, not {CARDID_OFFSET:#x} size {CARDID_SIZE:#x}",
                cardid.offset, cardid.size
            ),
        ));
    }
    Ok(())
}

/// The facts the planner judges a device by. Non-identity only: no MAC, unique id, calibration
/// value or cardid byte ever appears here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFacts {
    pub chip: String,
    pub revision: ChipRevision,
    pub flash_manufacturer: u8,
    pub flash_device: u16,
    /// The device partition table (read at 0x8000, 0xC00 bytes).
    pub partitions: Vec<Partition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactsError(pub String);

impl core::fmt::Display for FactsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "device facts: {}", self.0)
    }
}

pub fn parse_number(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16).ok(),
        None => text.replace('_', "").parse().ok(),
    }
}

fn type_code(text: &str) -> Option<u8> {
    match text {
        "app" => Some(ptype::APP),
        "data" => Some(ptype::DATA),
        other => parse_number(other).and_then(|n| u8::try_from(n).ok()),
    }
}

fn subtype_code(text: &str) -> Option<u8> {
    match text {
        "factory" => Some(subtype::FACTORY),
        "nvs" => Some(subtype::NVS),
        "phy" => Some(subtype::PHY),
        "ota" => Some(subtype::OTA_DATA),
        other => match other.strip_prefix("ota_") {
            Some(n) => n
                .parse::<u8>()
                .ok()
                .and_then(|n| subtype::OTA_0.checked_add(n)),
            None => parse_number(other).and_then(|n| u8::try_from(n).ok()),
        },
    }
}

impl DeviceFacts {
    pub fn passport(partitions: Vec<Partition>) -> DeviceFacts {
        DeviceFacts {
            chip: "ESP32-C3".to_owned(),
            revision: CHIP_REVISION,
            flash_manufacturer: FLASH_MANUFACTURER,
            flash_device: FLASH_DEVICE,
            partitions,
        }
    }

    /// Reads the `tests/fixtures/device-facts.toml` shape:
    ///
    /// ```toml
    /// [chip]
    /// name = "ESP32-C3"
    /// revision = "v1.1"
    /// [flash]
    /// manufacturer = "0x20"
    /// device = "0x4017"
    /// [[partition]]
    /// name = "cardid"
    /// type = "data"
    /// subtype = "nvs"
    /// offset = "0x356000"
    /// size = "0x4000"
    /// ```
    ///
    /// Every key is required; a missing or unparsable one is an error, never a default.
    pub fn from_toml(text: &str) -> Result<DeviceFacts, FactsError> {
        let doc = TomlLite::parse(text);
        let need = |table: &str, key: &str| {
            doc.string(table, key)
                .ok_or_else(|| FactsError(format!("missing `{table}.{key}`")))
        };
        let chip = need("chip", "name")?.to_owned();
        let revision = ChipRevision::parse(need("chip", "revision")?)
            .ok_or_else(|| FactsError("`chip.revision` is not `v<major>.<minor>`".to_owned()))?;
        let number = |table: &str, key: &str, max: u64| -> Result<u64, FactsError> {
            parse_number(need(table, key)?)
                .filter(|n| *n <= max)
                .ok_or_else(|| FactsError(format!("`{table}.{key}` is not a number in range")))
        };
        let flash_manufacturer = number("flash", "manufacturer", 0xFF)? as u8;
        let flash_device = number("flash", "device", 0xFFFF)? as u16;
        let mut partitions = Vec::new();
        for (index, table) in doc
            .tables()
            .iter()
            .filter(|t| t.array && t.name == "partition")
            .enumerate()
        {
            let field = |key: &str| {
                table
                    .string(key)
                    .ok_or_else(|| FactsError(format!("partition {index}: missing `{key}`")))
            };
            let bad = |key: &str| FactsError(format!("partition {index}: bad `{key}`"));
            let u32_of = |key: &str| -> Result<u32, FactsError> {
                parse_number(field(key)?)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| bad(key))
            };
            partitions.push(Partition {
                name: field("name")?.to_owned(),
                ptype: type_code(field("type")?).ok_or_else(|| bad("type"))?,
                subtype: subtype_code(field("subtype")?).ok_or_else(|| bad("subtype"))?,
                offset: u32_of("offset")?,
                size: u32_of("size")?,
                flags: 0,
            });
        }
        if partitions.is_empty() {
            return Err(FactsError("no `[[partition]]` entries".to_owned()));
        }
        Ok(DeviceFacts {
            chip,
            revision,
            flash_manufacturer,
            flash_device,
            partitions,
        })
    }
}

/// The identity refusals: chip ESP32-C3 revision v1.1, flash 0x20/0x4017 and `cardid` at 0x356000
/// size 0x4000 in the device's own table.
pub fn check_identity(facts: &DeviceFacts) -> Vec<Refusal> {
    let mut out = Vec::new();
    if facts.chip != "ESP32-C3" || facts.revision != CHIP_REVISION {
        out.push(Refusal::new(
            Rule::ChipRevision,
            format!(
                "the device reports {} revision {}, not ESP32-C3 revision {CHIP_REVISION}",
                facts.chip, facts.revision
            ),
        ));
    }
    if facts.flash_manufacturer != FLASH_MANUFACTURER || facts.flash_device != FLASH_DEVICE {
        out.push(Refusal::new(
            Rule::FlashId,
            format!(
                "the flash reports {:#04x}/{:#06x}, not {FLASH_MANUFACTURER:#04x}/{FLASH_DEVICE:#06x}",
                facts.flash_manufacturer, facts.flash_device
            ),
        ));
    }
    if let Err((rule, detail)) = cardid_in_place(&facts.partitions) {
        out.push(Refusal::new(
            rule,
            format!("device partition table: {detail}"),
        ));
    }
    out
}

pub fn check_image_cardid(partitions: &[Partition]) -> Vec<Refusal> {
    match cardid_in_place(partitions) {
        Ok(()) => Vec::new(),
        Err((_, detail)) => vec![Refusal::new(
            Rule::ImageMovesCardid,
            format!("image partition table: {detail}"),
        )],
    }
}

/// What the host found out about a backup; the planner reads no files.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BackupEvidence {
    /// The backup was read back and its SHA-256 equals the bytes read from the device.
    pub verified: bool,
    pub owner_only: bool,
    pub inside_repository: bool,
}

pub fn check_backup(backup: Option<&BackupEvidence>) -> Vec<Refusal> {
    let Some(backup) = backup else {
        return vec![Refusal::new(Rule::NoVerifiedBackup, "no backup was taken")];
    };
    let mut out = Vec::new();
    if !backup.verified {
        out.push(Refusal::new(
            Rule::NoVerifiedBackup,
            "the backup was not verified by reading it back",
        ));
    }
    if !backup.owner_only {
        out.push(Refusal::new(
            Rule::BackupNotOwnerOnly,
            "the backup file is readable by someone other than its owner",
        ));
    }
    if backup.inside_repository {
        out.push(Refusal::new(
            Rule::BackupInsideRepository,
            "the backup lies inside the repository",
        ));
    }
    out
}

/// Whether `path` names a script the planner must not spawn: `.bat`, `.cmd` or `.ps1`, in any
/// case (CVE-2024-24576).
pub fn is_refused_script(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".bat", ".cmd", ".ps1"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PortUse {
    /// Writing flash: needs the modem lines, so only `rfc2217://`.
    Flash,
    /// Monitor and log only: `socket://` is accepted too.
    Monitor,
}

/// The emulator port allow list: `socket://127.0.0.1:<port>` and `rfc2217://127.0.0.1:<port>`.
/// Flashing needs `rfc2217://`, because esptool forces `no_reset` on `socket://`.
pub fn check_emulator_port(url: &str, usage: PortUse) -> Result<(), Refusal> {
    let refuse = |why: &str| Err(Refusal::new(Rule::EmulatorPort, why.to_owned()));
    let (scheme, rest) = match url.split_once("://") {
        Some(parts) => parts,
        None => return refuse("an emulator port is a `rfc2217://` or `socket://` URL"),
    };
    let Some(port) = rest.strip_prefix("127.0.0.1:") else {
        return refuse("an emulator port binds 127.0.0.1 only");
    };
    if port.is_empty()
        || !port.bytes().all(|b| b.is_ascii_digit())
        || port.parse::<u16>().map_or(true, |p| p == 0)
    {
        return refuse("the port number is not 1 to 65535");
    }
    match (scheme, usage) {
        ("rfc2217", _) | ("socket", PortUse::Monitor) => Ok(()),
        ("socket", PortUse::Flash) => {
            refuse("`socket://` is monitor-only; flashing needs `rfc2217://`")
        }
        _ => refuse("only `rfc2217://` and `socket://` are accepted"),
    }
}
