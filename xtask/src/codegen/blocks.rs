//! `specs/blocks/<block>.toml`: the four per-block logical tables (`reset_domains`, `wait`,
//! `overrides`, `stable_read`) plus the block's fidelity class, parsed, validated and
//! merged into the generated register tables of `regs.rs`. Schema: `specs/README.md` section 3.
//!
//! Checks:
//!
//! 1. every file parses with the required header keys and no unknown key, and every row and header
//!    carries a non-empty `provenance`;
//! 2. `block` names a `c3_devices!` row whose base and size the header repeats, or `chip` a board
//!    chip; the file name is `<name>.toml`;
//! 3. for a block with rows in `specs/c3-registers.csv`, each `register` and `field` belongs to it;
//!    blocks without CSV rows name registers from IDF headers, checked for shape only;
//! 4. no duplicate rows, and `wait` ids are unique across files;
//! 5. exactly [`SEED_WAITS`] rows carry `seed = true`.
//!
//! Merged into the generated tables (replacing the UNVERIFIED import defaults):
//!
//! - `RegSpec::domain`: the register's `reset_domains` mask, else the `"*"` row, which must agree
//!   with the CSV's per-scope reset columns.
//! - `RegSpec::stable_read`: a `stable_read` row.
//! - `RegSpec::class`: [`Spec::class_of`] (UNVERIFIED inheritance rule). A `registers` glob must
//!   match a register; an `offsets` row is only for a block without CSV rows and becomes
//!   `gen/classes.rs` (`super::classes`).
//! - `FieldSpec::reset`: an integer `value` replaces the CSV reset.
//! - `RegSpec::cite`: the CSV citation, then this file and the row kinds that touched the register.

use std::fs;
use std::path::Path;

use super::regs::{Block, SCOPE_CHIP, SCOPE_CORE, SCOPE_SYSTEM};

/// Directory of the block files, relative to the workspace root.
pub const SPEC_DIR: &str = "specs/blocks";

/// File holding the `c3_devices!` table, read to check `block`, `base` and `size`.
pub const DEVICE_TABLE: &str = "crates/pemu-soc-c3/src/periph/mod.rs";

/// Number of `seed = true` busy-wait rows: 28 waits and one tripwire.
pub const SEED_WAITS: usize = 29;

/// Seed rows with `kind = "tripwire"`: the BLE baseband row.
pub const SEED_TRIPWIRES: usize = 1;

/// Scope names of a `reset_domains` row and their `RegSpec::domain` bits (`ResetScope`).
const SCOPES: [(&str, u8); 3] = [
    ("chip", SCOPE_CHIP),
    ("system", SCOPE_SYSTEM),
    ("core", SCOPE_CORE),
];

/// Fidelity classes, as the `pemu_core::fidelity::Fidelity` variant names.
const CLASSES: [&str; 4] = ["A", "B", "C", "U"];

/// One row of the `c3_devices!` table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceRow {
    pub name: String,
    pub base: u32,
    pub size: u32,
}

/// A `[[reset_domains]]` row: which scopes restore which registers.
#[derive(Debug)]
pub struct ResetRow {
    /// `"*"` for every register of the block, else one register name.
    pub registers: String,
    /// `RegSpec::domain` mask of the `scopes` array.
    pub mask: u8,
}

/// Bound of a `[[wait]]` row: how long the guest may poll before the model has to answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Within {
    /// `within = "<bound>"`: the same bound under every timing profile.
    All(String),
    /// `within = { fast = ..., device = ... }`: one bound per timing profile.
    PerProfile { fast: String, device: String },
}

/// A `[[wait]]` row, kept whole: codegen checks the shape, merges the rows into the generated
/// `busy-waits` table of `gen/waits.rs` and leaves the model tests of each row to the block's own
/// package.
#[derive(Debug)]
pub struct WaitRow {
    pub id: String,
    pub kind: String,
    pub seed: bool,
    pub register: String,
    pub field: String,
    pub trigger: String,
    pub expect: String,
    pub within: Within,
    pub polled_at: Vec<String>,
    pub images: Vec<String>,
    pub milestone: String,
}

/// An `[[overrides]]` row: a deliberate difference from the IDF header default.
#[derive(Debug)]
pub struct OverrideRow {
    /// The `register` of the row, the glob of its `registers` key, or the `command` of a chip file.
    pub register: String,
    /// True when the name above came from `registers` and is a glob over the block's register
    /// names (see [`Spec::class_of`]).
    pub glob: bool,
    /// Block offsets this row classes, from its `offsets` key, ascending and without duplicates.
    /// Empty for every row that names registers.
    pub offsets: Vec<u32>,
    pub field: String,
    /// The value when it is an integer; `None` when the row states behavior in prose.
    pub value: Option<u32>,
    /// Fidelity class of this value, not of the block.
    pub class: String,
    /// The row's sources, kept whole so `super::fidelity` can print them as the citation column
    /// of `docs/fidelity.md`.
    pub provenance: String,
}

/// Whether `name` matches `pattern`, in which `*` stands for any run of characters including none.
/// Every other character matches itself; there is no other metacharacter.
pub(super) fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            let Some(rest) = name.strip_prefix(head) else {
                return false;
            };
            (0..=rest.len())
                .filter(|i| rest.is_char_boundary(*i))
                .any(|i| glob_matches(tail, &rest[i..]))
        }
    }
}

/// A `[[stable_read]]` row: a register an oracle read diff may compare.
#[derive(Debug)]
pub struct StableRow {
    pub register: String,
}

/// One parsed block file.
#[derive(Debug)]
pub struct Spec {
    /// File name inside [`SPEC_DIR`].
    pub file: String,
    /// `block` for an MMIO block, `chip` for a board chip.
    pub name: String,
    pub is_chip: bool,
    /// Window of the `c3_devices!` row; both 0 for a chip file.
    pub base: u32,
    pub size: u32,
    /// Fidelity class of the block.
    pub class: String,
    /// The milestone that first needs the block.
    pub milestone: String,
    /// The header's sources, the citation of a register that takes the block's class.
    pub provenance: String,
    pub resets: Vec<ResetRow>,
    pub waits: Vec<WaitRow>,
    pub overrides: Vec<OverrideRow>,
    pub stable: Vec<StableRow>,
}

impl Spec {
    /// Mask of the `registers = "*"` row, when the file has one.
    fn default_mask(&self) -> Option<u8> {
        self.resets
            .iter()
            .find(|r| r.registers == "*")
            .map(|r| r.mask)
    }

    /// Mask of the row naming `register`, when the file has one.
    fn named_mask(&self, register: &str) -> Option<u8> {
        self.resets
            .iter()
            .find(|r| r.registers == register)
            .map(|r| r.mask)
    }

    /// Row kinds of this file that name `register`, in table order, for the generated citation.
    fn tags(&self, register: &str) -> Vec<&'static str> {
        let mut tags = Vec::new();
        if self.named_mask(register).is_some() {
            tags.push("reset_domains");
        }
        if self.waits.iter().any(|w| w.register == register) {
            tags.push("wait");
        }
        if self.overrides.iter().any(|o| o.names(register)) {
            tags.push("overrides");
        }
        if self.stable.iter().any(|s| s.register == register) {
            tags.push("stable_read");
        }
        tags
    }

    /// Class of `register`, strongest claim first (UNVERIFIED inheritance rule; a register no row
    /// names counts as untouched, U):
    ///
    /// 1. its own `overrides` row;
    /// 2. the block class, when a `reset_domains`, `wait` or `stable_read` row names it;
    /// 3. the `registers` glob row that matches it, which covers an array and never demotes a
    ///    register a row names on its own;
    /// 4. `U`. A `register = "*"` overrides row is prose for the whole block and classes nothing.
    fn class_of(&self, register: &str) -> &str {
        if let Some(o) = self
            .overrides
            .iter()
            .find(|o| !o.glob && o.register == register)
        {
            return &o.class;
        }
        if self.inherits(register) {
            return &self.class;
        }
        match self.overrides.iter().find(|o| o.names(register)) {
            Some(o) => &o.class,
            None => "U",
        }
    }

    /// Whether `register` takes the block's class from a `reset_domains`, `wait` or `stable_read`
    /// row of its own (step 2 of [`Spec::class_of`]).
    pub fn inherits(&self, register: &str) -> bool {
        self.named_mask(register).is_some()
            || self.waits.iter().any(|w| w.register == register)
            || self.stable.iter().any(|s| s.register == register)
    }

    /// Whether `row` is the row that gives `register` its class, which is what
    /// `docs/fidelity.md` counts per claim (`super::fidelity`).
    pub fn classes(&self, row: &OverrideRow, register: &str) -> bool {
        if !row.names(register) {
            return false;
        }
        if !row.glob {
            return true;
        }
        !self.inherits(register)
            && !self
                .overrides
                .iter()
                .any(|o| !o.glob && o.register == register)
    }

    /// The `(offset, class)` pairs of this file's `offsets` rows, ascending, or an error naming an
    /// offset two rows class differently. Empty for a file with no such row.
    pub fn offset_classes(&self) -> Result<Vec<(u32, &str)>, String> {
        let mut out: Vec<(u32, &str)> = Vec::new();
        for o in &self.overrides {
            for off in &o.offsets {
                match out.iter().find(|(seen, _)| seen == off) {
                    Some((_, class)) if *class == o.class => {}
                    Some(_) => {
                        return Err(format!(
                            "{SPEC_DIR}/{}: two overrides rows class offset {off:#05X} differently",
                            self.file
                        ));
                    }
                    None => out.push((*off, o.class.as_str())),
                }
            }
        }
        out.sort_unstable_by_key(|(off, _)| *off);
        Ok(out)
    }
}

impl OverrideRow {
    /// Whether this row speaks about the register named `register`: it names it, or its glob
    /// matches it.
    pub fn names(&self, register: &str) -> bool {
        if self.glob {
            glob_matches(&self.register, register)
        } else {
            self.register == register
        }
    }
}

/// Parses the `c3_devices!` table out of `DEVICE_TABLE`: one row per
/// `name: Marker, KONST @ base, size => model;` line of the macro invocation.
pub fn device_rows(root: &Path) -> Result<Vec<DeviceRow>, String> {
    let path = root.join(DEVICE_TABLE);
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let body = text
        .split_once("c3_devices! {")
        .ok_or_else(|| format!("{DEVICE_TABLE}: no c3_devices! invocation"))?
        .1
        .split_once("\n}")
        .ok_or_else(|| format!("{DEVICE_TABLE}: unterminated c3_devices! invocation"))?
        .0;
    let mut rows = Vec::new();
    for line in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let err = || format!("{DEVICE_TABLE}: cannot read the c3_devices! row `{line}`");
        let (name, rest) = line.split_once(':').ok_or_else(err)?;
        let (_, rest) = rest.split_once('@').ok_or_else(err)?;
        let (base, rest) = rest.split_once(',').ok_or_else(err)?;
        let (size, _) = rest.split_once("=>").ok_or_else(err)?;
        rows.push(DeviceRow {
            name: name.trim().to_string(),
            base: hex_literal(base).ok_or_else(err)?,
            size: hex_literal(size).ok_or_else(err)?,
        });
    }
    if rows.is_empty() {
        return Err(format!("{DEVICE_TABLE}: empty c3_devices! table"));
    }
    Ok(rows)
}

/// Reads a Rust integer literal of the table, with `_` separators and an optional `0x` prefix.
fn hex_literal(s: &str) -> Option<u32> {
    let t: String = s.trim().chars().filter(|c| *c != '_').collect();
    match t.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

/// Header keys of a block file (`block`) and of a chip file (`chip`), plus the row arrays.
const HEADER_KEYS: [&str; 14] = [
    "schema",
    "block",
    "chip",
    "base",
    "size",
    "bus",
    "address",
    "class",
    "milestone",
    "provenance",
    "reset_domains",
    "wait",
    "overrides",
    "stable_read",
];

/// Reads and parses every `specs/blocks/*.toml` file, in file-name order.
pub fn load(root: &Path) -> Result<Vec<Spec>, String> {
    let dir = root.join(SPEC_DIR);
    let mut files: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))? {
        let name = entry
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.ends_with(".toml") {
            files.push(name);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!("{SPEC_DIR}: no block files"));
    }
    let mut specs = Vec::new();
    for file in files {
        let text =
            fs::read_to_string(dir.join(&file)).map_err(|e| format!("{SPEC_DIR}/{file}: {e}"))?;
        specs.push(parse(&file, &text)?);
    }
    Ok(specs)
}

/// Parses one block file. `file` is its name inside [`SPEC_DIR`], used in every message.
pub fn parse(file: &str, text: &str) -> Result<Spec, String> {
    let at = |what: &str| format!("{SPEC_DIR}/{file}: {what}");
    let t: toml::Table = text.parse().map_err(|e| at(&format!("{e}")))?;
    if let Some(extra) = t.keys().find(|k| !HEADER_KEYS.contains(&k.as_str())) {
        return Err(at(&format!("unknown key `{extra}`")));
    }
    if int(&t, "schema", &at)? != 1 {
        return Err(at("schema must be 1"));
    }
    let header_provenance = text_of(&t, "provenance", &at)?;
    let class = class_of(&t, &at)?;
    let milestone = text_of(&t, "milestone", &at)?;
    if milestone != "LATER" && !milestone.starts_with('M') {
        return Err(at(&format!("milestone `{milestone}` is not M<n> or LATER")));
    }
    let is_chip = t.contains_key("chip");
    let name = text_of(&t, if is_chip { "chip" } else { "block" }, &at)?;
    if file != format!("{name}.toml") {
        return Err(at(&format!("file name does not match `{name}`")));
    }
    let (base, size) = if is_chip {
        text_of(&t, "bus", &at)?;
        if !t.contains_key("address") {
            return Err(at("chip file without `address`"));
        }
        (0, 0)
    } else {
        let base = u32::try_from(int(&t, "base", &at)?).map_err(|_| at("base out of range"))?;
        let size = u32::try_from(int(&t, "size", &at)?).map_err(|_| at("size out of range"))?;
        (base, size)
    };
    Ok(Spec {
        file: file.to_string(),
        name,
        is_chip,
        base,
        size,
        class,
        milestone,
        provenance: header_provenance,
        resets: reset_rows(&t, &at)?,
        waits: wait_rows(&t, &at)?,
        overrides: override_rows(&t, &at)?,
        stable: stable_rows(&t, &at)?,
    })
}

/// Rows of one table of the file, or an empty slice when the table is absent.
fn rows<'a>(
    t: &'a toml::Table,
    key: &str,
    at: &dyn Fn(&str) -> String,
) -> Result<Vec<&'a toml::Table>, String> {
    let Some(value) = t.get(key) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| at(&format!("`{key}` is not an array of tables")))?;
    array
        .iter()
        .map(|v| {
            v.as_table()
                .ok_or_else(|| at(&format!("a `{key}` row is not a table")))
        })
        .collect()
}

/// Checks that a row or header carries only `allowed` keys and holds every key of `required`.
fn keys(
    t: &toml::Table,
    allowed: &[&str],
    required: &[&str],
    at: &dyn Fn(&str) -> String,
) -> Result<(), String> {
    if let Some(extra) = t.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(at(&format!("unknown key `{extra}`")));
    }
    if let Some(missing) = required.iter().find(|k| !t.contains_key(**k)) {
        return Err(at(&format!("missing key `{missing}`")));
    }
    Ok(())
}

/// A non-empty string value.
fn text_of(t: &toml::Table, key: &str, at: &dyn Fn(&str) -> String) -> Result<String, String> {
    match t.get(key).and_then(toml::Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
        Some(_) => Err(at(&format!("`{key}` is empty"))),
        None => Err(at(&format!("missing string `{key}`"))),
    }
}

/// An integer value.
fn int(t: &toml::Table, key: &str, at: &dyn Fn(&str) -> String) -> Result<i64, String> {
    t.get(key)
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| at(&format!("missing integer `{key}`")))
}

/// Every row and every header carries sources.
fn provenance(t: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<(), String> {
    text_of(t, "provenance", at).map(|_| ())
}

/// A `class` key holding one of the fidelity classes.
fn class_of(t: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<String, String> {
    let class = text_of(t, "class", at)?;
    if !CLASSES.contains(&class.as_str()) {
        return Err(at(&format!("class `{class}` is not A, B, C or U")));
    }
    Ok(class)
}

/// The `[[reset_domains]]` rows: one row per register key, `"*"` or a register name.
fn reset_rows(t: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<Vec<ResetRow>, String> {
    let mut out: Vec<ResetRow> = Vec::new();
    for row in rows(t, "reset_domains", at)? {
        keys(
            row,
            &["registers", "scopes", "provenance"],
            &["registers", "scopes", "provenance"],
            at,
        )?;
        provenance(row, at)?;
        let registers = text_of(row, "registers", at)?;
        let list = row
            .get("scopes")
            .and_then(toml::Value::as_array)
            .ok_or_else(|| at("`scopes` is not an array"))?;
        let mut mask = 0u8;
        for scope in list {
            let name = scope
                .as_str()
                .ok_or_else(|| at("a `scopes` entry is not a string"))?;
            let bit = SCOPES
                .iter()
                .find(|(s, _)| *s == name)
                .ok_or_else(|| at(&format!("scope `{name}` is not chip, system or core")))?
                .1;
            if mask & bit != 0 {
                return Err(at(&format!("scope `{name}` twice in one row")));
            }
            mask |= bit;
        }
        if out.iter().any(|r| r.registers == registers) {
            return Err(at(&format!("two reset_domains rows for `{registers}`")));
        }
        out.push(ResetRow { registers, mask });
    }
    Ok(out)
}

/// The `[[wait]]` rows. `within` is a string or a `{ fast, device }` table.
fn wait_rows(t: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<Vec<WaitRow>, String> {
    const ALL: [&str; 11] = [
        "id",
        "kind",
        "seed",
        "register",
        "field",
        "trigger",
        "expect",
        "within",
        "polled_at",
        "images",
        "milestone",
    ];
    let mut out: Vec<WaitRow> = Vec::new();
    for row in rows(t, "wait", at)? {
        let mut allowed = ALL.to_vec();
        allowed.push("provenance");
        keys(row, &allowed, &allowed, at)?;
        provenance(row, at)?;
        for key in ["id", "register", "field", "trigger", "expect", "milestone"] {
            text_of(row, key, at)?;
        }
        let mut lists = Vec::new();
        for key in ["polled_at", "images"] {
            let list = row
                .get(key)
                .and_then(toml::Value::as_array)
                .ok_or_else(|| at(&format!("`{key}` is not an array")))?;
            let mut items = Vec::new();
            for v in list {
                let item = v
                    .as_str()
                    .ok_or_else(|| at(&format!("a `{key}` entry is not a string")))?;
                items.push(item.to_string());
            }
            lists.push(items);
        }
        let images = lists.pop().unwrap_or_default();
        let polled_at = lists.pop().unwrap_or_default();
        let within = match row.get("within") {
            Some(toml::Value::String(_)) => Within::All(text_of(row, "within", at)?),
            Some(toml::Value::Table(w)) => {
                keys(w, &["fast", "device"], &["fast", "device"], at)?;
                Within::PerProfile {
                    fast: text_of(w, "fast", at)?,
                    device: text_of(w, "device", at)?,
                }
            }
            _ => return Err(at("`within` is not a string or a { fast, device } table")),
        };
        let kind = text_of(row, "kind", at)?;
        if kind != "wait" && kind != "tripwire" {
            return Err(at(&format!("kind `{kind}` is not wait or tripwire")));
        }
        let id = text_of(row, "id", at)?;
        let seed = row
            .get("seed")
            .and_then(toml::Value::as_bool)
            .ok_or_else(|| at("missing boolean `seed`"))?;
        if out.iter().any(|w| w.id == id) {
            return Err(at(&format!("two wait rows with id `{id}`")));
        }
        out.push(WaitRow {
            id,
            kind,
            seed,
            register: text_of(row, "register", at)?,
            field: text_of(row, "field", at)?,
            trigger: text_of(row, "trigger", at)?,
            expect: text_of(row, "expect", at)?,
            within,
            polled_at,
            images,
            milestone: text_of(row, "milestone", at)?,
        });
    }
    Ok(out)
}

/// The `[[overrides]]` rows: exactly one of `register`, `registers` and `command`, an optional
/// `offsets` list, and a `value` that is an integer, a prose string or, for a chip command, an
/// array of bytes.
fn override_rows(t: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<Vec<OverrideRow>, String> {
    let mut out: Vec<OverrideRow> = Vec::new();
    for row in rows(t, "overrides", at)? {
        keys(
            row,
            &[
                "register",
                "registers",
                "offsets",
                "command",
                "reg",
                "field",
                "value",
                "class",
                "provenance",
            ],
            &["value", "class", "provenance"],
            at,
        )?;
        provenance(row, at)?;
        let named = (
            row.contains_key("register"),
            row.contains_key("registers"),
            row.contains_key("command"),
        );
        let (register, glob) = match named {
            (true, false, false) => (text_of(row, "register", at)?, false),
            (false, true, false) => {
                let pattern = text_of(row, "registers", at)?;
                if !pattern.contains('*') {
                    return Err(at(&format!(
                        "`registers = \"{pattern}\"` is a glob and needs a `*`; name one register \
                         with `register`"
                    )));
                }
                if pattern.trim() == "*" {
                    return Err(at(
                        "`registers = \"*\"` would class every register of the block; state a \
                         block-wide rule in prose with `register = \"*\"`, or name the group",
                    ));
                }
                (pattern, true)
            }
            (false, false, true) => (text_of(row, "command", at)?, false),
            _ => {
                return Err(at(
                    "an overrides row names exactly one of register, registers, command",
                ));
            }
        };
        let offsets = offsets_of(row, at)?;
        let field = match row.get("field") {
            Some(_) => text_of(row, "field", at)?,
            None => String::new(),
        };
        if glob && !field.is_empty() && field != "*" {
            return Err(at(&format!(
                "overrides row: the glob `{register}` spans registers, so it cannot name the \
                 field `{field}`"
            )));
        }
        let value = match row.get("value") {
            Some(toml::Value::Integer(v)) => Some(
                u32::try_from(*v).map_err(|_| at(&format!("value {v} does not fit in 32 bits")))?,
            ),
            Some(toml::Value::String(s)) if !s.trim().is_empty() => None,
            Some(toml::Value::Array(a)) if !a.is_empty() => None,
            _ => {
                return Err(at(
                    "`value` is not an integer, a string or an array of bytes",
                ));
            }
        };
        if register == "*" && value.is_some() {
            return Err(at(
                "an overrides row with `register = \"*\"` states a block-wide rule in prose, so it \
                 cannot carry an integer `value`; name the register the value belongs to",
            ));
        }
        if glob && value.is_some() {
            return Err(at(&format!(
                "the overrides row `registers = \"{register}\"` states a rule for a register group \
                 in prose, so it cannot carry an integer `value`; name the register the value \
                 belongs to"
            )));
        }
        if !offsets.is_empty() && value.is_some() {
            return Err(at(
                "an overrides row with `offsets` classes raw offsets of a block with no register \
                 table, so it cannot carry an integer `value` for a field",
            ));
        }
        let class = class_of(row, at)?;
        if out
            .iter()
            .any(|o| o.register == register && o.field == field && o.value == value)
        {
            return Err(at(&format!("two overrides rows for `{register}`")));
        }
        out.push(OverrideRow {
            register,
            glob,
            offsets,
            field,
            value,
            class,
            provenance: text_of(row, "provenance", at)?,
        });
    }
    Ok(out)
}

/// The `offsets` key of an `[[overrides]]` row: an array of block offsets, or a
/// `{ first, last, stride }` table naming an array of registers, ascending and without duplicates.
fn offsets_of(row: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<Vec<u32>, String> {
    let word = |v: i64| -> Result<u32, String> {
        let off = u32::try_from(v).map_err(|_| at(&format!("offset {v} is not a block offset")))?;
        if off.is_multiple_of(4) {
            Ok(off)
        } else {
            Err(at(&format!("offset {off:#X} is not 4-aligned")))
        }
    };
    let mut out = match row.get("offsets") {
        None => return Ok(Vec::new()),
        Some(toml::Value::Array(a)) if !a.is_empty() => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                let v = v
                    .as_integer()
                    .ok_or_else(|| at("an `offsets` entry is not an integer"))?;
                out.push(word(v)?);
            }
            out
        }
        Some(toml::Value::Table(span)) => {
            keys(span, &["first", "last", "stride"], &["first", "last"], at)?;
            let first = word(int(span, "first", at)?)?;
            let last = word(int(span, "last", at)?)?;
            let stride = match span.get("stride") {
                Some(_) => word(int(span, "stride", at)?)?,
                None => 4,
            };
            if stride == 0 || last < first {
                return Err(at(
                    "an `offsets` span runs from `first` to `last` with a positive `stride`",
                ));
            }
            (first..=last).step_by(stride as usize).collect()
        }
        _ => {
            return Err(at(
                "`offsets` is not a non-empty array or a { first, last, stride } table",
            ));
        }
    };
    out.sort_unstable();
    let before = out.len();
    out.dedup();
    if out.len() != before {
        return Err(at("an `offsets` row lists one offset twice"));
    }
    Ok(out)
}

/// The `[[stable_read]]` rows: one per register.
fn stable_rows(t: &toml::Table, at: &dyn Fn(&str) -> String) -> Result<Vec<StableRow>, String> {
    let mut out: Vec<StableRow> = Vec::new();
    for row in rows(t, "stable_read", at)? {
        keys(
            row,
            &["register", "provenance"],
            &["register", "provenance"],
            at,
        )?;
        provenance(row, at)?;
        let register = text_of(row, "register", at)?;
        if out.iter().any(|s| s.register == register) {
            return Err(at(&format!("two stable_read rows for `{register}`")));
        }
        out.push(StableRow { register });
    }
    Ok(out)
}

/// Checks the parsed files against each other, against the `c3_devices!` table and against the
/// register tables of `specs/c3-registers.csv` (items 2 to 5 of the module documentation).
pub fn check(specs: &[Spec], blocks: &[Block], devices: &[DeviceRow]) -> Result<(), String> {
    for (i, s) in specs.iter().enumerate() {
        let at = |what: &str| format!("{SPEC_DIR}/{}: {what}", s.file);
        if specs[..i].iter().any(|p| p.name == s.name) {
            return Err(at(&format!("`{}` is named by two files", s.name)));
        }
        if s.is_chip {
            continue;
        }
        let row = devices
            .iter()
            .find(|d| d.name == s.name)
            .ok_or_else(|| at(&format!("`{}` is not a c3_devices! row", s.name)))?;
        if (row.base, row.size) != (s.base, s.size) {
            return Err(at(&format!(
                "base {:#X} size {:#X} do not match the c3_devices! row {:#X} {:#X}",
                s.base, s.size, row.base, row.size
            )));
        }
        if s.default_mask().is_none() {
            return Err(at("no `registers = \"*\"` reset_domains row"));
        }
    }
    // `offsets` rows exist for the blocks a `RegSpec` table cannot class, so a block that has one
    // must not have CSV rows, and every offset must land inside its window.
    for s in specs {
        let at = |what: &str| format!("{SPEC_DIR}/{}: {what}", s.file);
        for o in s.overrides.iter().filter(|o| !o.offsets.is_empty()) {
            if s.is_chip {
                return Err(at(
                    "a chip file has no block window, so an overrides row cannot use `offsets`",
                ));
            }
            if blocks.iter().any(|b| b.name == s.name) {
                return Err(at(&format!(
                    "`{}` has rows in specs/c3-registers.csv, so its rows name registers rather \
                     than raw offsets; drop `offsets` and name the register",
                    s.name
                )));
            }
            for off in &o.offsets {
                if off + 4 > s.size {
                    return Err(at(&format!(
                        "offset {off:#X} is outside the {:#X}-byte block window",
                        s.size
                    )));
                }
            }
        }
        s.offset_classes()?;
    }
    for d in devices {
        if !specs.iter().any(|s| !s.is_chip && s.name == d.name) {
            return Err(format!(
                "{SPEC_DIR}/{}.toml: missing, every c3_devices! row needs a block file",
                d.name
            ));
        }
    }
    let seeds: Vec<&WaitRow> = specs
        .iter()
        .flat_map(|s| &s.waits)
        .filter(|w| w.seed)
        .collect();
    if seeds.len() != SEED_WAITS {
        return Err(format!(
            "{SPEC_DIR}: {} wait rows carry `seed = true`, the seed set has {SEED_WAITS}",
            seeds.len()
        ));
    }
    let tripwires = seeds.iter().filter(|w| w.kind == "tripwire").count();
    if tripwires != SEED_TRIPWIRES {
        return Err(format!(
            "{SPEC_DIR}: {tripwires} seed rows are tripwires, the seed set has {SEED_TRIPWIRES}"
        ));
    }
    let mut ids: Vec<&str> = specs
        .iter()
        .flat_map(|s| s.waits.iter().map(|w| w.id.as_str()))
        .collect();
    ids.sort_unstable();
    if let Some(pair) = ids.windows(2).find(|p| p[0] == p[1]) {
        return Err(format!("{SPEC_DIR}: two wait rows with id `{}`", pair[0]));
    }
    for b in blocks {
        let spec = specs
            .iter()
            .find(|s| !s.is_chip && s.name == b.name)
            .ok_or_else(|| format!("{SPEC_DIR}/{}.toml: missing block file", b.name))?;
        check_names(spec, b)?;
    }
    Ok(())
}

/// Rejects a row whose register does not belong to this block, and a field that is not a field
/// of that register. `"*"` stands for every register or every field, and a `field` may
/// name several fields of one register, separated by commas.
pub(super) fn check_names(spec: &Spec, b: &Block) -> Result<(), String> {
    let at = |what: &str| format!("{SPEC_DIR}/{}: {what}", spec.file);
    let rows: Vec<(&str, &str, &str)> = spec
        .resets
        .iter()
        .map(|r| ("reset_domains", r.registers.as_str(), "*"))
        .chain(
            spec.waits
                .iter()
                .map(|w| ("wait", w.register.as_str(), w.field.as_str())),
        )
        .chain(
            spec.overrides
                .iter()
                .filter(|o| !o.glob)
                .map(|o| ("overrides", o.register.as_str(), o.field.as_str())),
        )
        .chain(
            spec.stable
                .iter()
                .map(|s| ("stable_read", s.register.as_str(), "*")),
        )
        .collect();
    for o in spec.overrides.iter().filter(|o| o.glob) {
        if !b.regs.iter().any(|r| glob_matches(&o.register, r.name)) {
            return Err(at(&format!(
                "overrides row: the glob `{}` matches no register of `{}`",
                o.register, b.name
            )));
        }
    }
    for (table, register, fields) in rows {
        if register == "*" {
            continue;
        }
        let reg = b.regs.iter().find(|r| r.name == register).ok_or_else(|| {
            at(&format!(
                "{table} row: `{register}` is not a register of `{}`",
                b.name
            ))
        })?;
        for field in fields.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if field == "*" {
                continue;
            }
            if !reg.fields.iter().any(|f| f.field == field) {
                return Err(at(&format!(
                    "{table} row: `{field}` is not a field of `{register}`"
                )));
            }
        }
    }
    Ok(())
}

/// Merges the block files into the register tables: `domain`, `stable_read`, `class`, `cite` and
/// the `overrides` reset values (the last section of the module documentation).
pub fn apply(blocks: &mut [Block], specs: &[Spec]) -> Result<(), String> {
    for b in blocks.iter_mut() {
        let name = b.name;
        let spec = specs
            .iter()
            .find(|s| !s.is_chip && s.name == name)
            .ok_or_else(|| format!("{SPEC_DIR}/{name}.toml: missing block file"))?;
        let default = spec
            .default_mask()
            .ok_or_else(|| format!("{SPEC_DIR}/{}: no `registers = \"*\"` row", spec.file))?;
        for r in &mut b.regs {
            match spec.named_mask(r.name) {
                Some(mask) => r.domain = mask,
                None if default == r.domain => {}
                None => {
                    return Err(format!(
                        "{SPEC_DIR}/{}: the `*` reset_domains row is {default:#X} but the per-scope \
                         reset columns of {}.{} give {:#X}",
                        spec.file, name, r.name, r.domain
                    ));
                }
            }
            r.stable_read = spec.stable.iter().any(|s| s.register == r.name);
            let class = spec.class_of(r.name);
            r.class = CLASSES
                .iter()
                .find(|c| **c == class)
                .copied()
                .ok_or_else(|| format!("{SPEC_DIR}/{}: class `{class}`", spec.file))?;
            let tags = spec.tags(r.name);
            if !tags.is_empty() {
                r.cite = format!("{}; {SPEC_DIR}/{} {}", r.cite, spec.file, tags.join(", "));
            }
            for o in spec.overrides.iter().filter(|o| o.register == r.name) {
                let Some(value) = o.value else { continue };
                let at = |what: &str| format!("{SPEC_DIR}/{}: {what}", spec.file);
                let mut named = r.fields.iter().filter(|f| f.field == o.field);
                let field = named.next().ok_or_else(|| {
                    at(&format!(
                        "overrides row: `{}` has no field `{}`",
                        r.name, o.field
                    ))
                })?;
                if named.next().is_some() {
                    return Err(at(&format!("overrides row: `{}` twice", o.field)));
                }
                let max = if field.width == 32 {
                    u32::MAX
                } else {
                    (1u32 << field.width) - 1
                };
                if value > max {
                    return Err(at(&format!(
                        "overrides row: {value:#X} is wider than `{}`",
                        o.field
                    )));
                }
                r.field_resets.push((o.field.clone(), value));
            }
            r.recompute_reset();
        }
    }
    Ok(())
}
