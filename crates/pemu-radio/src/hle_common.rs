//! What the BLE and Wi-Fi radio modules share: profile rows and their strict parser, binding
//! against an image, module host setup, and handler helpers.
//!
//! `TomlLite` skips what it does not recognize, so the parser here refuses unknown tables,
//! missing keys and malformed hashes: a row that parsed empty would make the binding guess.

use std::collections::BTreeMap;

use pemu_hle::binding::{
    BindingMismatch, BindingProfile, BoundSymbol, CODE_HASH_BYTES, ImageView, MismatchField,
    ModuleSymbols, bind_profile_image, skeleton_hash,
};
use pemu_hle::continuation::{HandlerState, Resume};
use pemu_hle::guest_call::{HleAction, HleError, HleErrorKind};
use pemu_hle::hooks::{HandlerKind, HookKind, HookSet, ModuleIndex};
use pemu_hle::log_synth::LogSynth;
use pemu_hle::worker::WorkerCalls;
use pemu_loader::bundle::TomlTable;
use pemu_loader::hex;
use pemu_loader::symbols::SymbolTable;

/// One accepted build of a hooked function: its `st_size` and the SHA-256 of the skeleton of its
/// first 32 code bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    /// Informative only.
    pub builds: String,
    pub size: u32,
    pub code_sha256: [u8; 32],
    /// The sdkconfig-dependent lines of `log-lines.toml` were checked for an image of this shape.
    pub log_lines_verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRow {
    pub name: String,
    pub handler: u16,
    pub section: String,
    /// At least one.
    pub variants: Vec<Variant>,
}

#[derive(Default)]
pub(crate) struct ProfileRows {
    idf: Option<String>,
    module: Option<String>,
    hooks: Vec<HookRow>,
    calls: Vec<String>,
    data: Vec<(String, u32)>,
}

#[derive(Debug)]
pub(crate) struct Rows {
    pub idf: String,
    pub module: String,
    pub hooks: Vec<HookRow>,
    pub calls: Vec<String>,
    pub data: Vec<(String, u32)>,
}

impl ProfileRows {
    /// Takes `table` when it is one of the shared tables, and says whether it was.
    pub(crate) fn take(&mut self, table: &TomlTable) -> Result<bool, String> {
        match (table.name.as_str(), table.array) {
            ("profile", false) => {
                self.idf = Some(need(table, "idf")?.to_string());
                self.module = Some(need(table, "module")?.to_string());
            }
            ("hook", true) => self.hooks.push(HookRow {
                name: need(table, "name")?.to_string(),
                handler: u16::try_from(need_u32(table, "handler")?)
                    .map_err(|_| "a handler number above 65535".to_string())?,
                section: need(table, "section")?.to_string(),
                variants: Vec::new(),
            }),
            ("variant", true) => {
                let hook = self
                    .hooks
                    .last_mut()
                    .ok_or("a [[variant]] before any [[hook]]")?;
                hook.variants.push(Variant {
                    builds: need(table, "builds")?.to_string(),
                    size: need_u32(table, "size")?,
                    code_sha256: digest(need(table, "code_sha256")?)?,
                    log_lines_verified: match table.string("log_lines") {
                        None => false,
                        Some("verified") => true,
                        Some(other) => {
                            return Err(format!("log_lines `{other}` is not verified"));
                        }
                    },
                });
            }
            ("call", true) => self.calls.push(need(table, "name")?.to_string()),
            ("data", true) => self
                .data
                .push((need(table, "name")?.to_string(), need_u32(table, "size")?)),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Refuses a hook with no variant, two hooks with one name or handler, and a missing
    /// `[profile]`.
    pub(crate) fn finish(self) -> Result<Rows, String> {
        let hooks = self.hooks;
        if let Some(hook) = hooks.iter().find(|h| h.variants.is_empty()) {
            return Err(format!("hook `{}` has no [[variant]]", hook.name));
        }
        for (i, hook) in hooks.iter().enumerate() {
            if hooks[..i]
                .iter()
                .any(|h| h.name == hook.name || h.handler == hook.handler)
            {
                return Err(format!("hook `{}` repeats a name or handler", hook.name));
            }
        }
        Ok(Rows {
            idf: self.idf.ok_or("no [profile]")?,
            module: self.module.ok_or("no [profile]")?,
            hooks,
            calls: self.calls,
            data: self.data,
        })
    }
}

pub(crate) fn unknown_table(name: &str, array: bool) -> String {
    format!(
        "unknown table `{}{name}{}`",
        if array { "[[" } else { "[" },
        if array { "]]" } else { "]" }
    )
}

pub(crate) fn need<'a>(table: &'a TomlTable, key: &str) -> Result<&'a str, String> {
    table
        .string(key)
        .ok_or_else(|| format!("[{}] has no string `{key}`", table.name))
}

pub(crate) fn need_u32(table: &TomlTable, key: &str) -> Result<u32, String> {
    table
        .integer(key)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("[{}] has no 32-bit number `{key}`", table.name))
}

pub(crate) fn digest(text: &str) -> Result<[u8; 32], String> {
    let refuse = || format!("`{text}` is not 64 hex digits");
    // Checked on bytes, so a multi-byte character is refused rather than sliced through.
    let bytes = text.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(refuse());
    }
    let nibble = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = (nibble(bytes[2 * i]).ok_or_else(refuse)? << 4)
            | nibble(bytes[2 * i + 1]).ok_or_else(refuse)?;
    }
    Ok(out)
}

/// Binds fail-closed: `idf_ver` must be `v` plus `idf`, each hooked function the image links must
/// match a variant by size and code hash, and every call and data symbol must be linked. An image
/// that links none of the hooked functions binds nothing.
pub(crate) fn bind_view(
    image: &ImageView<'_>,
    idf: &str,
    hooks: &'static [HookRow],
    calls: &'static [String],
    data: &'static [(String, u32)],
    module: ModuleIndex,
) -> Result<HookSet, Vec<BindingMismatch>> {
    let symbols = &image.elf.symbols;
    if hooks
        .iter()
        .all(|hook| symbols.lookup(&hook.name).is_none())
    {
        return Ok(HookSet::default());
    }
    let mut mismatches = Vec::new();
    let want_idf = format!("v{idf}");
    let idf_ver = image.elf.app_desc.as_ref().map(|d| d.idf_ver.as_str());
    if idf_ver != Some(want_idf.as_str()) {
        mismatches.push(BindingMismatch {
            symbol: "esp_app_desc".to_string(),
            field: MismatchField::IdfVersion,
            expected: want_idf,
            found: idf_ver.unwrap_or("no app descriptor").to_string(),
        });
    }
    let mut rows = Vec::new();
    for hook in hooks {
        let mut row =
            BoundSymbol::hook(hook.name.as_str(), HookKind::Hle(HandlerKind(hook.handler)))
                .with_section(hook.section.as_str());
        if let Some(sym) = symbols.lookup(&hook.name) {
            let found = image.code_at(sym.addr, CODE_HASH_BYTES);
            let hash = found.map(skeleton_hash);
            let accepted = hook
                .variants
                .iter()
                .find(|v| v.size == sym.size && Some(v.code_sha256) == hash);
            match accepted {
                Some(v) => row = row.with_size(v.size).with_code_hash(v.code_sha256),
                None => mismatches.push(BindingMismatch {
                    symbol: hook.name.clone(),
                    field: if hook.variants.iter().any(|v| v.size == sym.size) {
                        MismatchField::CodeHash
                    } else {
                        MismatchField::Size
                    },
                    expected: hook
                        .variants
                        .iter()
                        .map(|v| format!("{:#x}/{}", v.size, hex(&v.code_sha256)))
                        .collect::<Vec<_>>()
                        .join(" or "),
                    found: match hash {
                        Some(hash) => format!("{:#x}/{}", sym.size, hex(&hash)),
                        None => format!("{:#x}/image bytes unavailable", sym.size),
                    },
                }),
            }
        }
        rows.push(row);
    }
    rows.extend(
        calls
            .iter()
            .map(|name| BoundSymbol::call(name.as_str()).required()),
    );
    rows.extend(
        data.iter()
            .map(|(name, size)| BoundSymbol::data(name.as_str()).with_size(*size).required()),
    );
    let bound = bind_profile_image(
        &BindingProfile {
            idf: "5.5.3",
            symbols: rows,
            log_lines: Vec::new(),
        },
        image,
        module,
    );
    match bound {
        Ok(bound) if mismatches.is_empty() => Ok(bound.set),
        Ok(_) => Err(mismatches),
        Err(more) => {
            // A row pins a hash only after the variant check accepted it, so the generic check
            // never repeats a hash mismatch.
            mismatches.extend(more);
            Err(mismatches)
        }
    }
}

/// The names an image without an ELF must provide: hooks, calls, data and `presence`.
pub(crate) fn module_symbols(
    hooks: &'static [HookRow],
    calls: &'static [String],
    data: &'static [(String, u32)],
    presence: &[&'static str],
) -> ModuleSymbols {
    ModuleSymbols {
        hooks: hooks.iter().map(|h| h.name.as_str()).collect(),
        required: calls
            .iter()
            .map(String::as_str)
            .chain(data.iter().map(|(name, _)| name.as_str()))
            .chain(presence.iter().copied())
            .collect(),
    }
}

/// `None` when `symbols` lacks one (binding already refused such an image).
pub(crate) fn symbol_addrs(
    symbols: &SymbolTable,
    calls: &[String],
    data: &[(String, u32)],
) -> Option<BTreeMap<String, u32>> {
    let mut addrs = BTreeMap::new();
    for name in calls.iter().chain(data.iter().map(|(name, _)| name)) {
        addrs.insert(name.clone(), symbols.addr_of(name)?);
    }
    Some(addrs)
}

/// Whether the image's `init` hook matched a variant whose log lines are verified.
pub(crate) fn init_lines_verified(image: &ImageView<'_>, init: &HookRow) -> bool {
    image.elf.symbols.lookup(&init.name).is_some_and(|sym| {
        let hash = image.code_at(sym.addr, CODE_HASH_BYTES).map(skeleton_hash);
        init.variants
            .iter()
            .any(|v| v.log_lines_verified && v.size == sym.size && Some(v.code_sha256) == hash)
    })
}

pub(crate) fn worker_calls(addrs: &BTreeMap<String, u32>) -> WorkerCalls {
    let addr = |name: &str| addrs.get(name).copied().unwrap_or(0);
    WorkerCalls {
        task_create: addr("xTaskCreatePinnedToCore"),
        queue_create: addr("xQueueGenericCreate"),
        semaphore_take: addr("xQueueSemaphoreTake"),
        give_from_isr: addr("xQueueGiveFromISR"),
        yield_from_isr: addr("vPortYieldFromISR"),
    }
}

pub(crate) fn mac_text(mac: &[u8]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A handler's step and locals, as little-endian words.
pub(crate) fn words_of(state: &HandlerState) -> Vec<u32> {
    state
        .bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect()
}

pub(crate) fn store_words(state: &mut HandlerState, words: &[u32]) {
    state.bytes = words.iter().flat_map(|w| w.to_le_bytes()).collect();
}

pub(crate) fn ret(a0: u32) -> HleAction {
    HleAction::Return { a0, a1: 0 }
}

pub(crate) fn fault(module: &str, what: &str, addr: u32) -> HleAction {
    HleAction::Fail(HleError::new(
        HleErrorKind::GuestFault,
        format!("{module}: {what} at {addr:#010x} is not readable guest memory"),
    ))
}

pub(crate) fn bad_step(handler: &str, step: u32) -> HleAction {
    HleAction::Fail(HleError::new(
        HleErrorKind::Handler,
        format!("{handler} was resumed at unknown step {step}"),
    ))
}

/// The first nested call of a synthesized line. Binding requires both log functions, so their
/// absence is a broken invariant.
pub(crate) fn timestamp_call(synth: &LogSynth, module: &str) -> HleAction {
    match synth.timestamp_call() {
        Some(call) => HleAction::from(call),
        None => missing_log_functions(module),
    }
}

pub(crate) fn missing_log_functions(module: &str) -> HleAction {
    HleAction::Fail(HleError::new(
        HleErrorKind::Handler,
        format!(
            "the {module} module has no esp_log or esp_log_timestamp to synthesize a line with"
        ),
    ))
}

/// Zeros for a resume that is not a return.
pub(crate) fn returned(resume: &Resume) -> (u32, &[u8]) {
    match resume {
        Resume::Returned { a0, scratch, .. } => (*a0, scratch.as_slice()),
        _ => (0, &[]),
    }
}

pub(crate) fn word_at(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use pemu_loader::bundle::TomlLite;

    use super::*;

    fn rows(text: &str) -> Result<Rows, String> {
        let mut rows = ProfileRows::default();
        for table in TomlLite::parse(text).tables() {
            if !rows.take(table)? {
                return Err(unknown_table(&table.name, table.array));
            }
        }
        rows.finish()
    }

    const HOOK: &str = "[profile]\nidf = \"5.5.3\"\nmodule = \"x\"\n\
        [[hook]]\nname = \"f\"\nhandler = 1\nsection = \".flash.text\"\n\
        [[variant]]\nbuilds = \"a\"\nsize = 4\n\
        code_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";

    #[test]
    fn a_variant_is_verified_only_by_the_one_word() {
        let parsed = rows(HOOK).expect("parses");
        assert!(!parsed.hooks[0].variants[0].log_lines_verified);
        let verified = rows(&format!("{HOOK}log_lines = \"verified\"\n")).expect("parses");
        assert!(verified.hooks[0].variants[0].log_lines_verified);
        for word in ["unverified", "yes"] {
            let err = rows(&format!("{HOOK}log_lines = \"{word}\"\n")).unwrap_err();
            assert!(err.contains(word), "{err}");
        }
    }

    #[test]
    fn a_hook_without_a_variant_or_a_profile_is_refused() {
        let no_variant = "[profile]\nidf = \"5.5.3\"\nmodule = \"x\"\n\
            [[hook]]\nname = \"f\"\nhandler = 1\nsection = \".flash.text\"\n";
        assert!(rows(no_variant).unwrap_err().contains("no [[variant]]"));
        let no_profile = HOOK.replace("[profile]\nidf = \"5.5.3\"\nmodule = \"x\"\n", "");
        assert_eq!(rows(&no_profile).unwrap_err(), "no [profile]");
        assert_eq!(rows("[[nope]]\n").unwrap_err(), "unknown table `[[nope]]`");
    }

    #[test]
    fn a_mac_prints_as_colon_separated_lowercase_hex() {
        // Three octets, so the literal is not an address of any device.
        assert_eq!(mac_text(&[0x0A, 0xBC, 0x01]), "0a:bc:01");
    }
}
