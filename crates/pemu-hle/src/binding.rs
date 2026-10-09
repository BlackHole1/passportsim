//! Fail-closed binding profiles per IDF version, the binding record, and the `RadioModule`
//! extension point with its no-op default `NoRadio`.
//!
//! For each profile symbol, binding checks presence, size, section and a hash of the
//! relocation-masked skeleton of the first 32 code bytes ([`skeleton_hash`]). Three outcomes:
//!
//! - **absent**: the linker garbage-collected it, so the row is skipped by name and binds no hook.
//!   A [`BoundSymbol::required`] row is the exception: its absence is a mismatch;
//! - **present and matching**: it binds;
//! - **present and different**: a [`BindingMismatch`], which disables the whole feature: the
//!   module contributes no hooks, the receipt marks it `unsupported image`, and the rest of the
//!   machine keeps running.
//!
//! A check that cannot be made fails too: the code hash needs the image bytes, which `ElfInfo`
//! does not keep, so [`bind_profile`] refuses a profile that pins one and [`bind_profile_image`]
//! carries the bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::snap::SectionId;
use pemu_loader::elf::ElfInfo;
use pemu_loader::hex;
use pemu_loader::symbols::{SymKind, Symbol, SymbolTable};

use crate::hooks::{HookKind, HookRef, HookSet, ModuleIndex};
use crate::log_synth::LogLineTemplate;
use crate::magic::{MagicKind, MagicPcs};
use crate::observe::ObserveKind;
use crate::tripwire::{TripwireSet, TripwireSpec, blob_defined_set};

pub const CODE_HASH_BYTES: usize = 32;

/// Fail-closed binding for one IDF version.
pub struct BindingProfile {
    /// For example `5.5.3`.
    pub idf: &'static str,
    pub symbols: Vec<BoundSymbol>,
    pub log_lines: Vec<LogLineTemplate>,
}

/// What binding a symbol does. Not every profile symbol is hooked: data symbols
/// (`pxCurrentTCBs`, `xIsrStackBottom`) and nested-call targets (`xQueueGiveFromISR`) are only
/// resolved.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolRole {
    /// A `K_HOOK` terminator at the function entry.
    Hook(HookKind),
    /// Address resolved, nothing hooked.
    Data,
    /// Address resolved, nothing hooked.
    Call,
}

/// One symbol of a `BindingProfile` and the checks pinned for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundSymbol {
    /// Looked up in the image's full symbol table, local symbols included.
    pub name: &'static str,
    pub role: SymbolRole,
    /// When true its absence is a mismatch rather than a skip.
    pub required: bool,
    /// `None` accepts any size.
    pub size: Option<u32>,
    /// `None` accepts any section.
    pub section: Option<&'static str>,
    /// `None` accepts any kind.
    pub kind: Option<SymKind>,
    /// [`skeleton_hash`] of the first [`CODE_HASH_BYTES`] bytes at the symbol address; `None`
    /// does not pin the code. A pin that cannot be checked without the image bytes is a mismatch.
    pub code_hash: Option<[u8; 32]>,
}

impl BoundSymbol {
    pub fn hook(name: &'static str, kind: HookKind) -> BoundSymbol {
        BoundSymbol {
            name,
            role: SymbolRole::Hook(kind),
            required: false,
            size: None,
            section: None,
            kind: Some(SymKind::Func),
            code_hash: None,
        }
    }

    pub fn data(name: &'static str) -> BoundSymbol {
        BoundSymbol {
            name,
            role: SymbolRole::Data,
            required: false,
            size: None,
            section: None,
            kind: None,
            code_hash: None,
        }
    }

    pub fn call(name: &'static str) -> BoundSymbol {
        BoundSymbol {
            name,
            role: SymbolRole::Call,
            required: false,
            size: None,
            section: None,
            kind: Some(SymKind::Func),
            code_hash: None,
        }
    }

    pub fn required(mut self) -> BoundSymbol {
        self.required = true;
        self
    }

    pub fn with_size(mut self, size: u32) -> BoundSymbol {
        self.size = Some(size);
        self
    }

    pub fn with_section(mut self, section: &'static str) -> BoundSymbol {
        self.section = Some(section);
        self
    }

    pub fn with_code_hash(mut self, hash: [u8; 32]) -> BoundSymbol {
        self.code_hash = Some(hash);
        self
    }
}

/// Which check of [`BoundSymbol`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MismatchField {
    Missing,
    Size,
    Section,
    Kind,
    /// The code skeleton differs, or the bytes could not be read.
    CodeHash,
    /// Two profile rows want to hook the same address with different hooks.
    Collision,
    /// The app descriptor's `idf_ver` is not the IDF version the profile binds.
    IdfVersion,
    /// The module's worker could not be registered with the HLE core.
    Worker,
    /// An image without an ELF holds more than one place the symbol could be, or its witnesses
    /// disagree ([`crate::image_symbols`]).
    Ambiguous,
}

impl MismatchField {
    /// The receipt wording.
    pub fn receipt_word(self) -> &'static str {
        match self {
            MismatchField::Missing => "missing",
            MismatchField::Size => "size",
            MismatchField::Section => "section",
            MismatchField::Kind => "kind",
            MismatchField::CodeHash => "code_hash",
            MismatchField::Collision => "collision",
            MismatchField::IdfVersion => "idf_version",
            MismatchField::Worker => "worker",
            MismatchField::Ambiguous => "ambiguous",
        }
    }
}

/// One failed check: what a receipt needs to say why a feature became `unsupported image`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingMismatch {
    pub symbol: String,
    pub field: MismatchField,
    pub expected: String,
    pub found: String,
}

impl fmt::Display for BindingMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {:?} expected {}, image has {}",
            self.symbol, self.field, self.expected, self.found
        )
    }
}

/// Everything binding needs about an image. `ElfInfo` keeps no file bytes, so the code hash needs
/// them alongside; a view without them fails that check closed.
#[derive(Copy, Clone, Debug)]
pub struct ImageView<'a> {
    pub elf: &'a ElfInfo,
    pub file: Option<&'a [u8]>,
    /// Its `r_rwip_*` and `r_btdm_*` entry points are tripwires for every image.
    pub rom: Option<&'a SymbolTable>,
    /// The loaded segments, used for the code bytes when there are no ELF file bytes: a machine
    /// keeps the flash image, not the ELF file, and these are the bytes the guest executes.
    pub segments: &'a [LoadedSegment<'a>],
}

/// One segment of a loaded image.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LoadedSegment<'a> {
    pub addr: u32,
    pub data: &'a [u8],
}

impl<'a> ImageView<'a> {
    pub fn new(elf: &'a ElfInfo, file: &'a [u8]) -> ImageView<'a> {
        ImageView {
            elf,
            file: Some(file),
            rom: None,
            segments: &[],
        }
    }

    pub fn with_rom(mut self, rom: &'a SymbolTable) -> ImageView<'a> {
        self.rom = Some(rom);
        self
    }

    /// A profile that pins a code hash cannot bind against this view.
    pub fn symbols_only(elf: &'a ElfInfo) -> ImageView<'a> {
        ImageView {
            elf,
            file: None,
            rom: None,
            segments: &[],
        }
    }

    pub fn with_segments(mut self, segments: &'a [LoadedSegment<'a>]) -> ImageView<'a> {
        self.segments = segments;
        self
    }

    /// The first `len` bytes at `addr`: from the defining section when the view has the ELF file
    /// bytes, otherwise from the loaded segment that covers them.
    pub fn code_at(&self, addr: u32, len: usize) -> Option<&'a [u8]> {
        let Some(file) = self.file else {
            return self.segments.iter().find_map(|seg| {
                let start = addr.checked_sub(seg.addr)? as usize;
                seg.data.get(start..start.checked_add(len)?)
            });
        };
        let section = self.elf.sections.iter().find(|s| {
            s.is_alloc() && s.has_bits() && addr >= s.addr && u64::from(addr) < s.end()
        })?;
        let data = section.data(file)?;
        let start = (addr - section.addr) as usize;
        data.get(start..start.checked_add(len)?)
    }
}

/// Binds `profile` against `elf`: every symbol must match, or no hooks are returned. A profile
/// that pins a code hash gets a [`MismatchField::CodeHash`] mismatch here; use
/// [`bind_profile_image`].
pub fn bind_profile(
    profile: &BindingProfile,
    elf: &ElfInfo,
) -> Result<HookSet, Vec<BindingMismatch>> {
    bind_profile_image(profile, &ImageView::symbols_only(elf), ModuleIndex::CORE)
        .map(|bound| bound.set)
}

/// [`bind_profile`] with the image bytes and the module index, as a `RadioModule` calls it. Also
/// returns the resolved addresses, which handlers need for data and nested-call symbols.
pub fn bind_profile_image(
    profile: &BindingProfile,
    image: &ImageView<'_>,
    module: ModuleIndex,
) -> Result<ProfileBinding, Vec<BindingMismatch>> {
    let mut mismatches = Vec::new();
    let mut binding = ProfileBinding::default();
    for row in &profile.symbols {
        let Some(sym) = image.elf.symbols.lookup(row.name) else {
            if row.required {
                mismatches.push(BindingMismatch {
                    symbol: row.name.to_string(),
                    field: MismatchField::Missing,
                    expected: "linked".to_string(),
                    found: "not linked".to_string(),
                });
            } else {
                binding.skipped.push(row.name.to_string());
            }
            continue;
        };
        check_symbol(row, sym, image, &mut mismatches);
        binding.addresses.insert(row.name.to_string(), sym.addr);
        if let SymbolRole::Hook(kind) = row.role {
            let id = HookRef { kind, module }.to_id();
            if let Some(old) = binding.set.insert(sym.addr, id)
                && old != id
            {
                mismatches.push(BindingMismatch {
                    symbol: row.name.to_string(),
                    field: MismatchField::Collision,
                    expected: format!("one hook at {:#010x}", sym.addr),
                    found: format!("hook {:#010x} and hook {:#010x}", old.0, id.0),
                });
            }
            binding.hooked.insert(row.name.to_string());
        }
    }
    if mismatches.is_empty() {
        Ok(binding)
    } else {
        Err(mismatches)
    }
}

/// Runs the checks of one row against the symbol the image holds.
fn check_symbol(
    row: &BoundSymbol,
    sym: &Symbol,
    image: &ImageView<'_>,
    out: &mut Vec<BindingMismatch>,
) {
    let mut fail = |field, expected: String, found: String| {
        out.push(BindingMismatch {
            symbol: row.name.to_string(),
            field,
            expected,
            found,
        });
    };
    if let Some(size) = row.size
        && size != sym.size
    {
        fail(
            MismatchField::Size,
            format!("{size:#x}"),
            format!("{:#x}", sym.size),
        );
    }
    if let Some(kind) = row.kind
        && kind != sym.kind
    {
        fail(
            MismatchField::Kind,
            format!("{kind:?}"),
            format!("{:?}", sym.kind),
        );
    }
    if let Some(want) = row.section {
        let found = section_name(image.elf, sym);
        if found != Some(want) {
            fail(
                MismatchField::Section,
                want.to_string(),
                found.unwrap_or("(none)").to_string(),
            );
        }
    }
    if let Some(want) = row.code_hash {
        match image.code_at(sym.addr, CODE_HASH_BYTES) {
            Some(bytes) => {
                let found = skeleton_hash(bytes);
                if found != want {
                    fail(MismatchField::CodeHash, hex(&want), hex(&found));
                }
            }
            None => fail(
                MismatchField::CodeHash,
                hex(&want),
                "image bytes unavailable".to_string(),
            ),
        }
    }
}

fn section_name<'a>(elf: &'a ElfInfo, sym: &Symbol) -> Option<&'a str> {
    match sym.section {
        pemu_loader::symbols::SymSection::Index(index) => elf
            .sections
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.name.as_str()),
        _ => None,
    }
}

/// SHA-256 of the first code bytes of a hooked function, the digest the loader already uses.
pub fn code_hash(bytes: &[u8]) -> [u8; 32] {
    pemu_loader::sha256(bytes)
}

/// The relocation-masked skeleton of code: every whole RV32IMC instruction in `bytes`, with the
/// immediates a link relocates or relaxes zeroed (`mask_32`, `mask_16`) and everything else kept;
/// an instruction the window cuts keeps only its opcode. A register written by `lui` or `auipc` is
/// high until next written, and a 32-bit `addi` whose `rs1` is high or `gp` has its immediate
/// zeroed too: that is the low half of a relocated address, which moves with the link like the
/// high half does. Two builds of the same `bt.c` whose symbols moved give the same skeleton; a
/// different instruction does not.
pub fn code_skeleton(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    skeleton_into(bytes, &mut out, None);
    out
}

/// [`code_skeleton`] into a reused buffer. `ends` receives the offset after each whole
/// instruction: the skeleton of `bytes[..n]` is the first `n` bytes of this one exactly when `n`
/// is such an offset, so one pass serves every prefix that ends on an instruction.
pub(crate) fn skeleton_into(bytes: &[u8], out: &mut Vec<u8>, mut ends: Option<&mut Vec<usize>>) {
    out.clear();
    if let Some(ends) = ends.as_deref_mut() {
        ends.clear();
    }
    let mut high = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        let low = bytes[at];
        if low & 0b11 == 0b11 {
            let Some(word) = bytes
                .get(at..at + 4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            else {
                out.push(low & 0x7F);
                out.resize(bytes.len(), 0);
                return;
            };
            let mut masked = mask_32(word);
            let rs1 = (word >> 15) & 31;
            if word & 0x7F == 0x13 && (word >> 12) & 7 == 0 && (rs1 == 3 || high & (1 << rs1) != 0)
            {
                masked = word & 0x000F_FFFF;
            }
            out.extend_from_slice(&masked.to_le_bytes());
            track_high(&bytes[at..], &mut high);
            at += 4;
        } else if at + 2 <= bytes.len() {
            let half = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            out.extend_from_slice(&mask_16(half).to_le_bytes());
            track_high(&bytes[at..], &mut high);
            at += 2;
        } else {
            out.push(low & 0b11);
            at += 1;
            continue;
        }
        if let Some(ends) = ends.as_deref_mut() {
            ends.push(at);
        }
    }
}

/// Updates the set of registers that hold the high half of an address after the instruction at
/// `bytes[0]`.
fn track_high(bytes: &[u8], high: &mut u32) {
    use pemu_rv32::op::{K_AUIPC, K_LUI};
    let Some(op) = pemu_rv32::decode::decode_at(bytes, 0) else {
        return;
    };
    if op.rd == 0 {
        return;
    }
    if op.kind == K_LUI || op.kind == K_AUIPC {
        *high |= 1 << op.rd;
    } else {
        *high &= !(1 << op.rd);
    }
}

pub fn skeleton_hash(bytes: &[u8]) -> [u8; 32] {
    code_hash(&code_skeleton(bytes))
}

pub(crate) fn mask_32(word: u32) -> u32 {
    let keep = |bits: &[(u32, u32)]| {
        bits.iter().fold(0u32, |m, (lo, hi)| {
            m | (((1u64 << (hi - lo + 1)) - 1) as u32) << lo
        })
    };
    let mask = match word & 0x7F {
        // lui, auipc, jal: opcode and rd.
        0x37 | 0x17 | 0x6F => keep(&[(0, 11)]),
        // jalr and loads (I type): opcode, rd, funct3, rs1.
        0x67 | 0x03 => keep(&[(0, 19)]),
        // branches and stores (B and S type): opcode, funct3, rs1, rs2.
        0x63 | 0x23 => keep(&[(0, 6), (12, 24)]),
        _ => u32::MAX,
    };
    word & mask
}

pub(crate) fn mask_16(half: u16) -> u16 {
    let funct3 = half >> 13;
    let mask: u16 = match (half & 0b11, funct3) {
        // c.jal, c.j: funct3 and the opcode only.
        (0b01, 0b001) | (0b01, 0b101) => 0b1110_0000_0000_0011,
        // c.beqz, c.bnez: funct3, rs1', opcode.
        (0b01, 0b110) | (0b01, 0b111) => 0b1110_0011_1000_0011,
        // c.lui and c.addi16sp: funct3, rd, opcode.
        (0b01, 0b011) => 0b1110_1111_1000_0011,
        // c.lwsp: funct3, rd, opcode.
        (0b10, 0b010) => 0b1110_1111_1000_0011,
        // c.swsp: funct3, rs2, opcode.
        (0b10, 0b110) => 0b1110_0000_0111_1111,
        // c.lw, c.sw: funct3, rs1', rs2' or rd', opcode.
        (0b00, 0b010) | (0b00, 0b110) => 0b1110_0011_1001_1111,
        _ => u16::MAX,
    };
    half & mask
}

/// What one profile bound: the hooks, every resolved address, the hooked names the tripwire rule
/// subtracts, and the rows skipped because the image does not link them.
#[derive(Clone, Debug, Default)]
pub struct ProfileBinding {
    pub set: HookSet,
    pub addresses: BTreeMap<String, u32>,
    pub hooked: BTreeSet<String>,
    pub skipped: Vec<String>,
}

/// Extension point, one per radio (BLE, Wi-Fi). No modules are registered by default.
pub trait RadioModule {
    /// The key of its mismatches and of its `BindingRecord` feature.
    fn name(&self) -> &'static str;
    /// Typically bind_profile(own profile, elf).
    fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>>;
    fn snapshot_sections(&self) -> &'static [SectionId];
    fn config_fragment(&self) -> MachineConfigFragment;
    /// [`RadioModule::bind`] with the whole image view, so a profile that pins code bytes can
    /// check them. The default binds from the symbols alone.
    fn bind_image(&self, image: &ImageView<'_>) -> Result<HookSet, Vec<BindingMismatch>> {
        self.bind(image.elf)
    }
    /// The object that runs this module's handlers in one machine, or `None` for a module with no
    /// handlers.
    fn host(&self, _image: &ImageView<'_>) -> Option<Box<dyn crate::core::ModuleHost>> {
        None
    }
    /// The profile id the receipt prefixes bound features with (`idf-5.5.3/ble`).
    fn profile_id(&self) -> Option<&'static str> {
        None
    }
    /// The secret inputs this module's state holds (for example a scripted access point's
    /// pre-shared key), so a `SecretSet` masks them in every output. The module decodes its own
    /// state so no caller special-cases one radio; one that cannot decode it returns nothing.
    fn secret_values(&self, _state: &[u8]) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// Whether this module's state makes the instance secret-bearing without adding to a
    /// `SecretSet`, such as the BLE btsnoop capture: its pairing keys are the run's own, so there
    /// is nothing to mask, but the operator must be told. One that cannot decode `state` answers
    /// `false`.
    fn state_taints(&self, _state: &[u8]) -> bool {
        false
    }
    /// The names this module's binding reads, so an image without an ELF can be checked for them
    /// before binding against the recovered symbols. The default names none, which never binds.
    fn image_symbols(&self) -> ModuleSymbols {
        ModuleSymbols::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleSymbols {
    pub hooks: Vec<&'static str>,
    /// Everything else the module reads once any hook binds: nested-call targets, data symbols and
    /// functions whose presence alone decides a handler's behaviour.
    pub required: Vec<&'static str>,
    /// `(hook, guard)`: `guard` is a blob-defined function the unhooked body of `hook` calls
    /// before it calls anything else or stores anything outside its frame, and which no hooked
    /// path reaches. The tripwire rule arms it wherever the image links it, so a run that enters
    /// `hook` in a shape no rule pins stops there. An image without an ELF cannot show that a
    /// function is absent; for a guarded hook it does not have to
    /// ([`crate::image_symbols::Recovered::check`]).
    pub guards: Vec<(&'static str, &'static str)>,
}

/// Merges the core hooks and the registered modules' sets into the one `HookSet` for
/// `Engine::run`. A module whose bind fails contributes no hooks and is recorded as
/// `unsupported image`.
pub fn bind(
    modules: &[&dyn RadioModule],
    elf: &ElfInfo,
) -> (HookSet, Vec<(&'static str, Vec<BindingMismatch>)>) {
    let bound = bind_all(modules, &ImageView::symbols_only(elf));
    (bound.set, bound.mismatches)
}

/// [`bind`] plus the binding record for the receipt, the resolved addresses, and the hooked names
/// the tripwire rule subtracts.
pub fn bind_all(modules: &[&dyn RadioModule], image: &ImageView<'_>) -> BoundHooks {
    let mut bound = BoundHooks {
        record: BindingRecord {
            app_elf_sha256: image.elf.sha256,
            ..BindingRecord::default()
        },
        ..BoundHooks::default()
    };
    bound.add_core_hooks(image);
    for module in modules {
        // A module whose hook lands on a pc that already holds a different hook is refused whole:
        // a silent overwrite would leave one feature believing it is bound while its hook never
        // fires.
        let result = module.bind_image(image).and_then(|set| {
            let collisions = collisions(&bound.set, &set, image.elf);
            if collisions.is_empty() {
                Ok(set)
            } else {
                Err(collisions)
            }
        });
        match result {
            Ok(set) if set.is_empty() => {
                bound
                    .record
                    .features
                    .insert(module.name().to_string(), FeatureStatus::NotLinked);
            }
            Ok(set) => {
                for (pc, id) in set.iter() {
                    bound.set.insert(pc, id);
                }
                bound
                    .record
                    .features
                    .insert(module.name().to_string(), FeatureStatus::Bound);
                if let Some(id) = module.profile_id()
                    && bound.record.profile_id.is_empty()
                {
                    bound.record.profile_id = id.to_string();
                }
            }
            Err(mismatches) => {
                bound.mismatches.push((module.name(), mismatches));
                bound
                    .record
                    .features
                    .insert(module.name().to_string(), FeatureStatus::UnsupportedImage);
            }
        }
    }
    bound.arm_tripwires(image);
    bound
}

/// Every pc of `incoming` that `merged` already hooks with a different id, as a
/// [`MismatchField::Collision`] naming the function at that pc.
fn collisions(merged: &HookSet, incoming: &HookSet, elf: &ElfInfo) -> Vec<BindingMismatch> {
    incoming
        .iter()
        .filter_map(|(pc, id)| {
            let old = merged.get(pc)?;
            (old != id).then(|| BindingMismatch {
                symbol: elf
                    .symbols
                    .func_at(pc)
                    .map(|sym| sym.name.clone())
                    .unwrap_or_else(|| format!("{pc:#010x}")),
                field: MismatchField::Collision,
                expected: format!("no other hook at {pc:#010x}"),
                found: format!(
                    "hook {:#010x} already bound there, {:#010x} refused",
                    old.0, id.0
                ),
            })
        })
        .collect()
}

/// The whole binding of one image: what [`bind`] returns plus what the receipt and the tripwire
/// rule need.
#[derive(Clone, Debug, Default)]
pub struct BoundHooks {
    pub set: HookSet,
    pub addresses: BTreeMap<String, u32>,
    /// The tripwire rule subtracts these.
    pub hooked: BTreeSet<String>,
    pub record: BindingRecord,
    pub mismatches: Vec<(&'static str, Vec<BindingMismatch>)>,
    /// The tripwires armed in [`BoundHooks::set`]; its length is the receipt's tripwire count.
    pub tripwires: TripwireSet,
}

impl BoundHooks {
    /// Arms the tripwires as `HookKind::Tripwire` hooks of the merged set (see `crate::tripwire`).
    /// Hooked symbols are subtracted by pc, after every module has bound: a module set carries no
    /// names, and a tripwire on a hooked pc would never fire and would displace the hook.
    fn arm_tripwires(&mut self, image: &ImageView<'_>) {
        let spec = TripwireSpec::load();
        let mut armed = TripwireSet::default();
        armed.arm_image(
            &blob_defined_set(),
            &image.elf.symbols,
            &self.hooked,
            &spec.coexistence_allow,
        );
        if let Some(rom) = image.rom {
            armed.arm_rom(rom);
        }
        for (pc, kind, symbol) in armed.iter() {
            if self.set.get(pc).is_some() {
                continue;
            }
            self.set
                .insert(pc, HookRef::core(HookKind::Tripwire(kind)).to_id());
            self.tripwires.arm(pc, kind, symbol);
        }
    }

    /// The hooks `bind` contributes itself: the four observe hooks and the five magic PCs. An
    /// observe symbol the image does not link is skipped by name.
    fn add_core_hooks(&mut self, image: &ImageView<'_>) {
        for kind in ObserveKind::ALL {
            let Some(addr) = image.elf.symbols.addr_of(kind.symbol()) else {
                continue;
            };
            let id = HookRef::core(HookKind::Observe(kind)).to_id();
            self.set.insert(addr, id);
            self.addresses.insert(kind.symbol().to_string(), addr);
            self.hooked.insert(kind.symbol().to_string());
        }
        if let Some(pcs) = MagicPcs::from_spec() {
            for kind in MagicKind::ALL {
                let id = HookRef::core(HookKind::Magic(kind)).to_id();
                self.set.insert(pcs.pc_of(kind), id);
            }
        }
    }
}

/// No-op `RadioModule`: no hooks, no snapshot sections and an empty config fragment, so
/// `pemu-machine` can iterate registered modules before any radio exists.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoRadio;

impl RadioModule for NoRadio {
    fn name(&self) -> &'static str {
        "none"
    }

    fn bind(&self, _elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
        Ok(HookSet::default())
    }

    fn snapshot_sections(&self) -> &'static [SectionId] {
        &[]
    }

    fn config_fragment(&self) -> MachineConfigFragment {
        MachineConfigFragment::default()
    }
}

/// Stored in `HleSection::binding`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct BindingRecord {
    pub profile_id: String,
    /// Restore returns E_SNAPSHOT when the loaded app ELF differs.
    pub app_elf_sha256: [u8; 32],
    /// Keyed by `RadioModule::name`.
    pub features: BTreeMap<String, FeatureStatus>,
    /// Log-line fidelity per bound module with handlers, keyed like `features`. Not snapshot
    /// state: `Machine::receipt` fills it when it reports.
    pub log_lines: BTreeMap<String, RadioLogLines>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct RadioLogLines {
    /// Lines whose nested `esp_log` call returned: the receipt's `synthesized_log_lines`.
    pub synthesized: u64,
    /// False when the image's lines were not checked against a device boot log of its shape, so
    /// the lines one sdkconfig decides were left out: the receipt's `log_lines: unverified`.
    pub verified: bool,
}

/// One guest-heap block a radio module holds for the blob it replaced, as `inspect heap` labels
/// it. The ledger is `pemu_radio::heap_ledger`; this is the shape `pemu-hle` can name without
/// depending on it. Not snapshot state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HeapBlock {
    pub module: String,
    /// Empty for a block this build's plan has no row for.
    pub label: String,
    pub addr: u32,
    pub bytes: u32,
    pub caps: u32,
    /// The ledger's fidelity, `blob_allocations_estimated`.
    pub fidelity: String,
    /// Fidelity class of this block's size: `A` for a count the device reported, `C` for one an
    /// sdkconfig fixes, `U` for a block with no row in this build's plan.
    pub class: String,
}

/// Binding status of one feature.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub enum FeatureStatus {
    Bound,
    UnsupportedImage,
    Disabled,
    /// The image links none of the functions the module hooks, so nothing was bound. Never
    /// `Bound`: a receipt must not claim a radio model the image does not use.
    NotLinked,
}

impl FeatureStatus {
    /// The receipt wording.
    pub fn receipt_word(self) -> &'static str {
        match self {
            FeatureStatus::Bound => "bound",
            FeatureStatus::UnsupportedImage => "unsupported image",
            FeatureStatus::Disabled => "disabled",
            FeatureStatus::NotLinked => "not linked",
        }
    }
}

/// A radio module's part of `HleConfig`. Here because `pemu-machine`, which holds `HleConfig`,
/// depends on `pemu-hle`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MachineConfigFragment {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HandlerKind;
    use crate::tripwire::TripKind;
    use pemu_loader::elf::ElfSection;
    use pemu_loader::symbols::{SymBind, SymSection, SymbolTable};

    /// `.text` of the synthetic image below, with room for one 32-byte function per symbol.
    const TEXT_ADDR: u32 = 0x4200_0000;
    const TEXT_SIZE: u32 = 0x400;

    fn symbol(name: &str, addr: u32, size: u32, kind: SymKind) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size,
            kind,
            bind: SymBind::Global,
            section: SymSection::Index(1),
        }
    }

    /// A synthetic app ELF: one `.text` section whose bytes are `file`, and the given symbols.
    fn image(syms: Vec<Symbol>) -> ElfInfo {
        ElfInfo {
            sha256: [7u8; 32],
            entry: TEXT_ADDR,
            sections: vec![ElfSection {
                index: 1,
                name: ".text".to_string(),
                sh_type: pemu_loader::elf::SHT_PROGBITS,
                flags: pemu_loader::elf::SHF_ALLOC | pemu_loader::elf::SHF_EXECINSTR,
                addr: TEXT_ADDR,
                offset: 0,
                size: TEXT_SIZE,
                align: 4,
            }],
            segments: Vec::new(),
            symbols: SymbolTable::new(syms),
            app_desc: None,
        }
    }

    /// File bytes for [`image`]: `.text` at file offset 0, each byte its own index.
    fn file_bytes() -> Vec<u8> {
        (0..TEXT_SIZE).map(|i| (i & 0xFF) as u8).collect()
    }

    fn profile(symbols: Vec<BoundSymbol>) -> BindingProfile {
        BindingProfile {
            idf: "5.5.3",
            symbols,
            log_lines: Vec::new(),
        }
    }

    const BLE_INIT: &str = "esp_bt_controller_init";

    #[test]
    fn a_matching_profile_binds_every_hook_and_resolves_every_symbol() {
        let elf = image(vec![
            symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func),
            symbol("xQueueGiveFromISR", TEXT_ADDR + 0x40, 0x20, SymKind::Func),
            symbol("xIsrStackBottom", TEXT_ADDR + 0x80, 4, SymKind::Object),
        ]);
        let profile = profile(vec![
            BoundSymbol::hook(BLE_INIT, HookKind::Hle(HandlerKind(1)))
                .with_size(0x20)
                .with_section(".text"),
            BoundSymbol::call("xQueueGiveFromISR").required(),
            BoundSymbol::data("xIsrStackBottom").required(),
        ]);
        let bound = bind_profile_image(
            &profile,
            &ImageView::symbols_only(&elf),
            ModuleIndex::FIRST_MODULE,
        )
        .expect("matching profile binds");
        assert_eq!(bound.set.len(), 1, "only the Hook row takes a hook");
        assert_eq!(
            bound.set.get(TEXT_ADDR).and_then(HookRef::from_id),
            Some(HookRef {
                kind: HookKind::Hle(HandlerKind(1)),
                module: ModuleIndex::FIRST_MODULE,
            })
        );
        assert_eq!(bound.addresses.len(), 3);
        assert_eq!(bound.addresses["xIsrStackBottom"], TEXT_ADDR + 0x80);
        assert_eq!(bound.hooked, BTreeSet::from([BLE_INIT.to_string()]));
        assert!(bound.skipped.is_empty());
    }

    #[test]
    fn a_symbol_the_image_does_not_link_is_skipped_by_name() {
        // A garbage-collected function is skipped by name, not a mismatch.
        let elf = image(vec![symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func)]);
        let profile = profile(vec![
            BoundSymbol::hook(BLE_INIT, HookKind::Hle(HandlerKind(1))),
            BoundSymbol::hook("esp_wifi_set_config", HookKind::Hle(HandlerKind(2))),
        ]);
        let bound = bind_profile_image(&profile, &ImageView::symbols_only(&elf), ModuleIndex(1))
            .expect("an absent symbol is a skip, not a mismatch");
        assert_eq!(bound.skipped, ["esp_wifi_set_config"]);
        assert_eq!(bound.set.len(), 1);
    }

    #[test]
    fn a_required_symbol_the_image_does_not_link_is_a_mismatch() {
        // The profile checks that xQueueGiveFromISR and the yield-from-ISR function are linked.
        let elf = image(vec![symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func)]);
        let profile = profile(vec![BoundSymbol::call("xQueueGiveFromISR").required()]);
        let err = bind_profile_image(&profile, &ImageView::symbols_only(&elf), ModuleIndex(1))
            .expect_err("a required symbol must fail closed");
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].field, MismatchField::Missing);
        assert_eq!(err[0].symbol, "xQueueGiveFromISR");
    }

    #[test]
    fn each_pinned_field_reports_its_own_mismatch() {
        // A symbol that exists but whose size or section differs is an unsupported version.
        let elf = image(vec![symbol(BLE_INIT, TEXT_ADDR, 0x32, SymKind::Object)]);
        let profile = profile(vec![
            BoundSymbol::hook(BLE_INIT, HookKind::Hle(HandlerKind(1)))
                .with_size(0x20)
                .with_section(".iram0.text"),
        ]);
        let err = bind_profile_image(&profile, &ImageView::symbols_only(&elf), ModuleIndex(1))
            .expect_err("three checks fail");
        let fields: BTreeSet<MismatchField> = err.iter().map(|m| m.field).collect();
        assert_eq!(
            fields,
            BTreeSet::from([
                MismatchField::Size,
                MismatchField::Section,
                MismatchField::Kind
            ])
        );
        assert!(err[0].to_string().contains(BLE_INIT));
    }

    #[test]
    fn a_pinned_code_hash_is_checked_against_the_image_bytes() {
        let elf = image(vec![symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func)]);
        let file = file_bytes();
        let good = skeleton_hash(&file[..CODE_HASH_BYTES]);
        let ok = profile(vec![
            BoundSymbol::hook(BLE_INIT, HookKind::Hle(HandlerKind(1))).with_code_hash(good),
        ]);
        assert!(
            bind_profile_image(&ok, &ImageView::new(&elf, &file), ModuleIndex(1)).is_ok(),
            "the pinned hash is the skeleton of the image's own first 32 code bytes"
        );

        let bad = profile(vec![
            BoundSymbol::hook(BLE_INIT, HookKind::Hle(HandlerKind(1))).with_code_hash([0u8; 32]),
        ]);
        let err = bind_profile_image(&bad, &ImageView::new(&elf, &file), ModuleIndex(1))
            .expect_err("a different build fails closed");
        assert_eq!(err[0].field, MismatchField::CodeHash);
        assert_eq!(err[0].found, hex(&good));
    }

    #[test]
    fn a_code_hash_that_cannot_be_checked_fails_closed() {
        // An unmakeable check is a failure, never a skip: `ElfInfo` keeps no file bytes.
        let elf = image(vec![symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func)]);
        let profile = profile(vec![
            BoundSymbol::hook(BLE_INIT, HookKind::Hle(HandlerKind(1))).with_code_hash([1u8; 32]),
        ]);
        let err = bind_profile(&profile, &elf).expect_err("no bytes, no binding");
        assert_eq!(err[0].field, MismatchField::CodeHash);
        assert_eq!(err[0].found, "image bytes unavailable");
    }

    /// Little-endian bytes of RV32 instructions, 32-bit words and 16-bit halves mixed.
    fn code(parts: &[Insn]) -> Vec<u8> {
        parts
            .iter()
            .flat_map(|p| match *p {
                Insn::W(w) => w.to_le_bytes().to_vec(),
                Insn::C(h) => h.to_le_bytes().to_vec(),
            })
            .collect()
    }

    #[derive(Copy, Clone)]
    enum Insn {
        W(u32),
        C(u16),
    }

    #[test]
    fn a_relinked_function_keeps_its_skeleton_and_a_different_instruction_does_not() {
        use Insn::{C, W};
        // lui a5, 0x3fc99; lw a0, -4(a5); c.lwsp ra, 12(sp); jal ra, +0x100; beq a0, a1, +8;
        // sw a0, 8(gp); c.j +4; c.beqz a0, +6; c.lw a4, 0(a0)
        let build_a = code(&[
            W(0x3fc9_97b7),
            W(0xffc7_a503),
            C(0x40b2),
            W(0x1000_00ef),
            W(0x00b5_0463),
            W(0x00a1_a423),
            C(0xa011),
            C(0xc119),
            C(0x4118),
        ]);
        // The same instructions after a relink: every relocated immediate moved.
        let build_b = code(&[
            W(0x3fc9_a7b7),
            W(0x0047_a503),
            C(0x4092),
            W(0x2340_00ef),
            W(0x02b5_0063),
            W(0x10a1_a023),
            C(0xa809),
            C(0xc511),
            C(0x4158),
        ]);
        assert_ne!(code_hash(&build_a), code_hash(&build_b), "the bytes differ");
        assert_eq!(skeleton_hash(&build_a), skeleton_hash(&build_b));

        // A different opcode, register or funct is a different function.
        let mut other_rd = build_a.clone();
        other_rd[0] = 0xb7 ^ 0x80; // lui a5 -> lui a4 (rd bits 7..11)
        assert_ne!(skeleton_hash(&build_a), skeleton_hash(&other_rd));
        let mut add_not_lw = build_a.clone();
        add_not_lw[4] = 0x33; // lw -> an OP instruction
        assert_ne!(skeleton_hash(&build_a), skeleton_hash(&add_not_lw));
        let mut corrupt = build_a.clone();
        for byte in &mut corrupt {
            *byte ^= 0xFF;
        }
        assert_ne!(skeleton_hash(&build_a), skeleton_hash(&corrupt));
    }

    #[test]
    fn the_low_half_of_a_relocated_address_is_not_part_of_the_skeleton() {
        use Insn::{C, W};
        // lui a5, hi; addi a5, a5, lo; lbu a3, 489(a5): `esp_wifi_internal_set_sta_ip` reaching
        // `g_ic`, as two links of the same archive place it.
        let link_a = code(&[W(0x3fc9_f7b7), W(0x5447_8793), W(0x1e97_c683)]);
        let link_b = code(&[W(0x3fca_07b7), W(0x3247_8793), W(0x1e97_c683)]);
        assert_eq!(skeleton_hash(&link_a), skeleton_hash(&link_b));
        // gp-relative after relaxation: addi a5, gp, lo.
        let gp_a = code(&[W(0x5441_8793)]);
        let gp_b = code(&[W(0x3241_8793)]);
        assert_eq!(skeleton_hash(&gp_a), skeleton_hash(&gp_b));
        // An `addi` on a register no `lui` or `auipc` wrote adds a constant, which is the code.
        let const_a = code(&[C(0x4785), W(0x5447_8793)]);
        let const_b = code(&[C(0x4785), W(0x3247_8793)]);
        assert_ne!(skeleton_hash(&const_a), skeleton_hash(&const_b));
        // The register stops being high once something else writes it.
        let rewritten_a = code(&[W(0x3fc9_f7b7), C(0x4785), W(0x5447_8793)]);
        let rewritten_b = code(&[W(0x3fc9_f7b7), C(0x4785), W(0x3247_8793)]);
        assert_ne!(skeleton_hash(&rewritten_a), skeleton_hash(&rewritten_b));
    }

    #[test]
    fn a_window_that_cuts_an_instruction_keeps_its_opcode_only() {
        // addi a0, a0, 1 whose last two bytes fall outside a 2-byte window.
        let whole = 0x0015_0513u32.to_le_bytes();
        assert_eq!(code_skeleton(&whole[..2]), vec![0x13, 0x00]);
        assert_eq!(code_skeleton(&[0x13]), vec![0x13]);
        assert_eq!(
            code_skeleton(&[0x41]),
            vec![0x01],
            "half a 16-bit instruction"
        );
    }

    /// A module that binds one hook, or reports the mismatches it was built with.
    struct TestModule {
        name: &'static str,
        module: ModuleIndex,
        handler: HandlerKind,
        fail: bool,
    }

    impl RadioModule for TestModule {
        fn name(&self) -> &'static str {
            self.name
        }

        fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
            let profile = profile(vec![
                BoundSymbol::hook(BLE_INIT, HookKind::Hle(self.handler)).required(),
            ]);
            if self.fail {
                return Err(vec![BindingMismatch {
                    symbol: BLE_INIT.to_string(),
                    field: MismatchField::Size,
                    expected: "0x20".to_string(),
                    found: "0x32".to_string(),
                }]);
            }
            bind_profile_image(&profile, &ImageView::symbols_only(elf), self.module)
                .map(|bound| bound.set)
        }

        fn snapshot_sections(&self) -> &'static [SectionId] {
            &[]
        }

        fn config_fragment(&self) -> MachineConfigFragment {
            MachineConfigFragment::default()
        }
    }

    #[test]
    fn a_failed_module_contributes_no_hooks_and_the_others_still_bind() {
        let elf = image(vec![
            symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func),
            symbol("vTaskDelete", TEXT_ADDR + 0x40, 0x20, SymKind::Func),
        ]);
        let ble = TestModule {
            name: "ble",
            module: ModuleIndex(1),
            handler: HandlerKind(1),
            fail: true,
        };
        let wifi = TestModule {
            name: "wifi",
            module: ModuleIndex(2),
            handler: HandlerKind(1),
            fail: false,
        };
        let bound = bind_all(&[&ble, &wifi], &ImageView::symbols_only(&elf));
        assert_eq!(
            bound.record.features["ble"].receipt_word(),
            "unsupported image"
        );
        assert_eq!(bound.record.features["wifi"].receipt_word(), "bound");
        assert_eq!(bound.mismatches.len(), 1);
        assert_eq!(bound.mismatches[0].0, "ble");
        // The wifi module's hook is bound, and it is the wifi module's id, not the ble one's.
        assert_eq!(
            bound.set.get(TEXT_ADDR).and_then(HookRef::from_id),
            Some(HookRef {
                kind: HookKind::Hle(HandlerKind(1)),
                module: ModuleIndex(2),
            })
        );
        assert_eq!(bound.record.app_elf_sha256, elf.sha256);
    }

    #[test]
    fn a_module_hook_that_collides_with_a_bound_hook_is_refused_whole() {
        // Two modules want the same entry with different handlers, and one module wants the
        // `vTaskDelete` observe hook's pc. Neither may overwrite what is already bound.
        let elf = image(vec![
            symbol(BLE_INIT, TEXT_ADDR, 0x20, SymKind::Func),
            symbol("vTaskDelete", TEXT_ADDR + 0x40, 0x20, SymKind::Func),
        ]);
        let first = TestModule {
            name: "ble",
            module: ModuleIndex(1),
            handler: HandlerKind(1),
            fail: false,
        };
        let second = TestModule {
            name: "wifi",
            module: ModuleIndex(2),
            handler: HandlerKind(1),
            fail: false,
        };
        let bound = bind_all(&[&first, &second], &ImageView::symbols_only(&elf));
        assert_eq!(bound.record.features["ble"], FeatureStatus::Bound);
        assert_eq!(
            bound.record.features["wifi"],
            FeatureStatus::UnsupportedImage
        );
        let (name, mismatches) = &bound.mismatches[0];
        assert_eq!(*name, "wifi");
        assert_eq!(mismatches[0].field, MismatchField::Collision);
        assert_eq!(mismatches[0].symbol, BLE_INIT);
        // The first binding survives untouched.
        assert_eq!(
            bound.set.get(TEXT_ADDR).and_then(HookRef::from_id),
            Some(HookRef {
                kind: HookKind::Hle(HandlerKind(1)),
                module: ModuleIndex(1),
            })
        );

        /// Hooks the pc of `vTaskDelete`, where the core observe hook already is.
        struct OnObserve;
        impl RadioModule for OnObserve {
            fn name(&self) -> &'static str {
                "observe-thief"
            }
            fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
                let mut set = HookSet::default();
                let pc = elf.symbols.addr_of("vTaskDelete").expect("linked");
                set.insert(
                    pc,
                    HookRef {
                        kind: HookKind::Hle(HandlerKind(9)),
                        module: ModuleIndex(3),
                    }
                    .to_id(),
                );
                Ok(set)
            }
            fn snapshot_sections(&self) -> &'static [SectionId] {
                &[]
            }
            fn config_fragment(&self) -> MachineConfigFragment {
                MachineConfigFragment::default()
            }
        }
        let bound = bind_all(&[&OnObserve], &ImageView::symbols_only(&elf));
        assert_eq!(
            bound.record.features["observe-thief"],
            FeatureStatus::UnsupportedImage
        );
        assert_eq!(bound.mismatches[0].1[0].symbol, "vTaskDelete");
        assert_eq!(
            bound.set.get(TEXT_ADDR + 0x40).and_then(HookRef::from_id),
            Some(HookRef::core(HookKind::Observe(ObserveKind::TaskDelete)))
        );
    }

    #[test]
    fn bind_adds_the_core_observe_hooks_and_the_five_magic_pcs() {
        let elf = image(vec![
            symbol("vTaskDelete", TEXT_ADDR, 0x20, SymKind::Func),
            symbol("abort", TEXT_ADDR + 0x40, 0x20, SymKind::Func),
        ]);
        let (set, mismatches) = bind(&[], &elf);
        assert!(mismatches.is_empty());
        let pcs = MagicPcs::from_spec().expect("spec");
        for kind in MagicKind::ALL {
            assert_eq!(
                set.get(pcs.pc_of(kind)).and_then(HookRef::from_id),
                Some(HookRef::core(HookKind::Magic(kind))),
                "{kind:?}"
            );
        }
        assert_eq!(
            set.get(TEXT_ADDR).and_then(HookRef::from_id),
            Some(HookRef::core(HookKind::Observe(ObserveKind::TaskDelete)))
        );
        // `esp_panic_handler` and `__assert_func` are not in this image, so they are skipped.
        assert_eq!(set.len(), 2 + MagicKind::ALL.len());
    }

    #[test]
    fn bind_arms_the_tripwires_as_hooks_and_subtracts_hooked_pcs() {
        // `ppTxPkt` and `esp_wifi_set_ps` are blob-defined; the module hooks the second one, so
        // only the first becomes a tripwire. `coex_pre_init` runs for real.
        let elf = image(vec![
            symbol("ppTxPkt", TEXT_ADDR, 0x20, SymKind::Func),
            symbol("esp_wifi_set_ps", TEXT_ADDR + 0x40, 0x20, SymKind::Func),
            symbol("coex_pre_init", TEXT_ADDR + 0x80, 0x20, SymKind::Func),
        ]);
        let rom = SymbolTable::new(vec![
            symbol("r_rwip_time_get", 0x4000_1000, 0x20, SymKind::Func),
            symbol("ets_printf", 0x4000_2000, 0x20, SymKind::Func),
        ]);
        /// Hooks `esp_wifi_set_ps`.
        struct SetPs;
        impl RadioModule for SetPs {
            fn name(&self) -> &'static str {
                "wifi"
            }
            fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
                let profile = profile(vec![BoundSymbol::hook(
                    "esp_wifi_set_ps",
                    HookKind::Hle(HandlerKind(4)),
                )]);
                bind_profile_image(&profile, &ImageView::symbols_only(elf), ModuleIndex(1))
                    .map(|bound| bound.set)
            }
            fn snapshot_sections(&self) -> &'static [SectionId] {
                &[]
            }
            fn config_fragment(&self) -> MachineConfigFragment {
                MachineConfigFragment::default()
            }
        }
        let bound = bind_all(&[&SetPs], &ImageView::symbols_only(&elf).with_rom(&rom));
        let tripwire = |pc| match bound.set.get(pc).and_then(HookRef::from_id) {
            Some(HookRef {
                kind: HookKind::Tripwire(kind),
                ..
            }) => Some(kind),
            _ => None,
        };
        assert_eq!(tripwire(TEXT_ADDR), Some(TripKind::BlobInternal));
        assert_eq!(
            bound.set.get(TEXT_ADDR + 0x40).and_then(HookRef::from_id),
            Some(HookRef {
                kind: HookKind::Hle(HandlerKind(4)),
                module: ModuleIndex(1),
            }),
            "the hook, not a tripwire, holds the hooked pc"
        );
        assert_eq!(tripwire(TEXT_ADDR + 0x80), None, "allowlisted");
        assert_eq!(tripwire(0x4000_1000), Some(TripKind::Rwip));
        assert_eq!(tripwire(0x4000_2000), None);
        assert_eq!(bound.tripwires.len(), 2);
        assert_eq!(
            bound.tripwires.at(TEXT_ADDR),
            Some((TripKind::BlobInternal, "ppTxPkt"))
        );
    }

    #[test]
    fn no_radio_contributes_nothing() {
        let m = NoRadio;
        assert_eq!(m.name(), "none");
        assert!(m.snapshot_sections().is_empty());
        assert_eq!(m.config_fragment(), MachineConfigFragment::default());
    }

    #[test]
    fn no_radio_iterates_as_a_registered_module() {
        let modules: &[&dyn RadioModule] = &[&NoRadio];
        let names: Vec<&'static str> = modules.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["none"]);
        assert!(modules.iter().all(|m| m.snapshot_sections().is_empty()));
    }
}
