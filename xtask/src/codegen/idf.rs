//! Parsers for ESP-IDF v5.5.3 headers of the ESP32-C3 (Apache-2.0): the register headers under
//! `components/soc/esp32c3/register/soc/` in both comment formats
//! (`/* NAME : R/W ;bitpos:[4:0] ;default: 5'd0 ; */` and `/** NAME : R/W; bitpos: [0];
//! default: 0;`), and C enums such as `periph_interrupt_t` in `soc/interrupts.h`.

/// One register macro `#define NAME_REG[(i)] (BASE[(i)] + 0xOFF)` with its field comments.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RegDef {
    /// Macro name including the `_REG` suffix.
    pub name: String,
    pub offset: u32,
    /// 1-based line of the `#define`.
    pub line: usize,
    pub fields: Vec<FieldDef>,
}

/// One field comment of a register.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldDef {
    pub name: String,
    pub shift: u8,
    pub width: u8,
    /// Access string as written in the header.
    pub access: String,
    /// Header default, not shifted.
    pub default: u32,
    /// 1-based line of the comment.
    pub line: usize,
    /// Set when the header layout needed a correction, such as `bitpos: [32:0]`.
    pub quirk: Option<&'static str>,
}

/// Note for a `bitpos: [32:0]` field (`USB_SERIAL_JTAG_DATE`), read as `[31:0]`.
pub const QUIRK_BITPOS_32: &str = "IDF bitpos [32:0] read as [31:0]";
/// Note for a field whose header default is empty (`GPIO_STRAPPING`, `GPIO_IN_DATA`: input
/// latches), read as 0.
pub const QUIRK_EMPTY_DEFAULT: &str = "IDF default empty (input latch); read as 0 UNVERIFIED";

/// Where field comments of a register header go.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Scope {
    /// No register yet, or after a memory window: a field comment is an error.
    None,
    /// After a register `#define`: field comments belong to the last register.
    Open,
    /// After an alias `#define SYSTEM_WIFI_CLK_EN_REG SYSCON_WIFI_CLK_EN_REG`: the comments
    /// repeat the aliased register's fields and are skipped.
    Alias,
}

/// Parses every register and its fields. A `#define` of an address that does not end in
/// `_REG` (a memory window such as `XTS_AES_PLAIN_MEM`) ends the current register; so does an
/// alias `#define A_REG B_REG` (`syscon_reg.h`), whose field comments are skipped.
pub fn parse_register_header(text: &str) -> Result<Vec<RegDef>, String> {
    let mut regs: Vec<RegDef> = Vec::new();
    let mut scope = Scope::None;
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let trimmed = raw.trim();
        if let Some((name, offset)) = address_define(trimmed) {
            scope = if name.ends_with("_REG") {
                regs.push(RegDef {
                    name,
                    offset,
                    line,
                    fields: Vec::new(),
                });
                Scope::Open
            } else {
                Scope::None
            };
        } else if alias_define(trimmed) {
            scope = Scope::Alias;
        } else if let Some(field) = field_comment(trimmed, line)? {
            match (scope, regs.last_mut()) {
                (Scope::Open, Some(reg)) => reg.fields.push(field),
                (Scope::Alias, _) => {}
                _ => {
                    return Err(format!(
                        "line {line}: field `{}` outside a register",
                        field.name
                    ));
                }
            }
        }
    }
    apply_shift_macros(text, &mut regs)?;
    Ok(regs)
}

/// Note for a field whose bitpos comment disagrees with its `_S` and `_V` macros; the macros
/// win (`usb_serial_jtag_reg.h`: `IN_FIFO_CNT` `[2:0]` but `_V` 0x3; `usb_serial_jtag_struct.h`
/// agrees with the macros).
pub const QUIRK_MACROS: &str =
    "IDF bitpos comment disagrees with its _S and _V macros; macros used";

/// Replaces `(shift, width)` by the `NAME_S` and `NAME_V` macros where both exist and disagree
/// with the comment. The header default must still fit the corrected width.
fn apply_shift_macros(text: &str, regs: &mut [RegDef]) -> Result<(), String> {
    let mut shifts = std::collections::BTreeMap::new();
    let mut masks = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("#define ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let (Some(name), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let value = value.trim_start_matches('(').trim_end_matches(')');
        if let Some(field) = name.strip_suffix("_S") {
            if let Ok(v) = value.parse::<u8>() {
                shifts.insert(field.to_string(), v);
            }
        } else if let Some(field) = name.strip_suffix("_V")
            && let Some(hex) = value.strip_prefix("0x")
            && let Ok(v) = u64::from_str_radix(hex, 16)
        {
            masks.insert(field.to_string(), v);
        }
    }
    for reg in regs {
        for f in &mut reg.fields {
            let (Some(&shift), Some(&mask)) = (shifts.get(&f.name), masks.get(&f.name)) else {
                continue;
            };
            let width = 64 - mask.leading_zeros();
            if mask == 0 || mask != (1u64 << width) - 1 || u32::from(shift) + width > 32 {
                continue;
            }
            let width = width as u8;
            if (shift, width) == (f.shift, f.width) {
                continue;
            }
            if u64::from(f.default) > mask {
                return Err(format!(
                    "line {}: default of {} wider than _V",
                    f.line, f.name
                ));
            }
            (f.shift, f.width, f.quirk) = (shift, width, Some(QUIRK_MACROS));
        }
    }
    Ok(())
}

/// `#define A_REG B_REG`: a register name defined as another register name.
fn alias_define(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#define ") else {
        return false;
    };
    let mut words = rest.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some(a), Some(b), None) => {
            a.ends_with("_REG") && is_ident(a) && b.ends_with("_REG") && is_ident(b)
        }
        _ => false,
    }
}

/// `#define NAME[(i)] (BASE[(i)] + 0xOFF)` gives `(NAME, OFF)`.
fn address_define(line: &str) -> Option<(String, u32)> {
    let rest = line.strip_prefix("#define ")?.trim_start();
    let name_end = rest.find(|c: char| c.is_whitespace() || c == '(')?;
    let name = &rest[..name_end];
    let mut tail = rest[name_end..].trim_start();
    if let Some(after) = tail
        .strip_prefix("(i)")
        .or_else(|| tail.strip_prefix("(n)"))
    {
        tail = after.trim_start();
    }
    let inner = tail.strip_prefix('(')?.strip_suffix(')')?;
    let (base, off) = inner.split_once('+')?;
    let base = base.trim();
    let base = base.strip_suffix("(i)").unwrap_or(base);
    if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let hex = off.trim().strip_prefix("0x")?;
    let offset = u32::from_str_radix(hex, 16).ok()?;
    is_ident(name).then(|| (name.to_string(), offset))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A field comment: `NAME : ACCESS ; bitpos: [hi:lo] ; default: VALUE ;`.
fn field_comment(line: &str, line_no: usize) -> Result<Option<FieldDef>, String> {
    let Some(body) = line.strip_prefix("/**").or_else(|| line.strip_prefix("/*")) else {
        return Ok(None);
    };
    if !body.contains("bitpos") {
        return Ok(None);
    }
    let err = |what: &str| format!("line {line_no}: {what} in `{line}`");
    let (name, rest) = body.split_once(':').ok_or_else(|| err("no field name"))?;
    let name = name.trim();
    if !is_ident(name) {
        return Err(err("bad field name"));
    }
    let parts: Vec<&str> = rest.split(';').map(str::trim).collect();
    let access = parts.first().copied().unwrap_or_default();
    let bitpos = parts
        .iter()
        .find_map(|p| p.strip_prefix("bitpos"))
        .ok_or_else(|| err("no bitpos"))?;
    let default = parts
        .iter()
        .find_map(|p| p.strip_prefix("default"))
        .ok_or_else(|| err("no default"))?;
    let (shift, width, mut quirk) = parse_bitpos(bitpos).ok_or_else(|| err("bad bitpos"))?;
    let default = default.trim_start_matches([':', ' ']);
    let default = if default.is_empty() && quirk.is_none() {
        quirk = Some(QUIRK_EMPTY_DEFAULT);
        0
    } else {
        parse_default(default, width).map_err(|e| err(&e))?
    };
    Ok(Some(FieldDef {
        name: name.to_string(),
        shift,
        width,
        access: access.to_string(),
        default,
        line: line_no,
        quirk,
    }))
}

/// `: [hi:lo]` or `:[n]` gives `(shift, width, quirk)`; `[32:0]` is read as `[31:0]`.
fn parse_bitpos(s: &str) -> Option<(u8, u8, Option<&'static str>)> {
    let s = s.trim_start_matches([':', ' ']);
    let inner = s.strip_prefix('[')?.split_once(']')?.0;
    let (hi, lo) = match inner.split_once(':') {
        Some((hi, lo)) => (hi.trim().parse::<u8>().ok()?, lo.trim().parse::<u8>().ok()?),
        None => {
            let n = inner.trim().parse::<u8>().ok()?;
            (n, n)
        }
    };
    if (hi, lo) == (32, 0) {
        return Some((0, 32, Some(QUIRK_BITPOS_32)));
    }
    (hi >= lo && hi < 32).then_some((lo, hi - lo + 1, None))
}

/// Header default: `5'd0`, `8'h80`, `1'b1`, `~2'b0` (all ones of width 2), `0`, `0x10`.
pub fn parse_default(tok: &str, width: u8) -> Result<u32, String> {
    let tok = tok.trim();
    let (invert, body) = match tok.strip_prefix('~') {
        Some(b) => (true, b.trim()),
        None => (false, tok),
    };
    let (value, bits) = if let Some((w, lit)) = body.split_once('\'') {
        let bits: u32 = w
            .parse()
            .map_err(|_| format!("bad default width `{tok}`"))?;
        let mut chars = lit.chars();
        let radix = match chars.next() {
            Some('b') => 2,
            Some('h') => 16,
            Some('d') => 10,
            _ => return Err(format!("bad default radix `{tok}`")),
        };
        let v = u64::from_str_radix(chars.as_str(), radix)
            .map_err(|_| format!("bad default digits `{tok}`"))?;
        (v, bits)
    } else if let Some(hex) = body.strip_prefix("0x") {
        let hex = hex.trim_end_matches('U');
        let v = u64::from_str_radix(hex, 16).map_err(|_| format!("bad default `{tok}`"))?;
        (v, u32::from(width))
    } else {
        let dec = body.trim_end_matches('U');
        let v: u64 = dec.parse().map_err(|_| format!("bad default `{tok}`"))?;
        (v, u32::from(width))
    };
    let mask = |b: u32| if b >= 64 { u64::MAX } else { (1u64 << b) - 1 };
    let value = if invert { !value & mask(bits) } else { value };
    if value > mask(u32::from(width)) {
        return Err(format!("default `{tok}` does not fit {width} bits"));
    }
    Ok(value as u32)
}

/// One enumerator of a C enum.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EnumItem {
    pub name: String,
    pub value: i64,
    /// 1-based line of the enumerator.
    pub line: usize,
    /// True when the enumerator is assigned another enumerator's name (`A = B`).
    pub alias: bool,
}

/// Evaluates `typedef enum { ... } type_name;` with C rules: an enumerator without an
/// initializer takes the previous enumerator's value plus one, whether or not that one was an
/// alias; an initializer is a decimal or hex literal or an earlier enumerator.
pub fn parse_enum(text: &str, type_name: &str) -> Result<Vec<EnumItem>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .iter()
        .position(|l| {
            l.trim()
                .strip_prefix('}')
                .is_some_and(|r| r.trim() == format!("{type_name};"))
        })
        .ok_or_else(|| format!("enum `{type_name}` not found"))?;
    let start = lines[..end]
        .iter()
        .rposition(|l| l.trim().starts_with("typedef enum"))
        .ok_or_else(|| format!("start of enum `{type_name}` not found"))?;
    let mut items: Vec<EnumItem> = Vec::new();
    let mut next = 0i64;
    for (idx, raw) in lines.iter().enumerate().take(end).skip(start + 1) {
        let line = idx + 1;
        let code = strip_comments(raw).map_err(|e| format!("line {line}: {e}"))?;
        for entry in code.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (name, init) = match entry.split_once('=') {
                Some((n, i)) => (n.trim(), Some(i.trim())),
                None => (entry, None),
            };
            if !is_ident(name) {
                return Err(format!("line {line}: bad enumerator `{entry}`"));
            }
            let (value, alias) = match init {
                None => (next, false),
                Some(i) => match parse_int(i) {
                    Some(v) => (v, false),
                    None => {
                        let prior = items
                            .iter()
                            .find(|p| p.name == i)
                            .ok_or_else(|| format!("line {line}: unknown initializer `{i}`"))?;
                        (prior.value, true)
                    }
                },
            };
            if items.iter().any(|p| p.name == name) {
                return Err(format!("line {line}: duplicate enumerator `{name}`"));
            }
            items.push(EnumItem {
                name: name.to_string(),
                value,
                line,
                alias,
            });
            next = value + 1;
        }
    }
    Ok(items)
}

fn parse_int(s: &str) -> Option<i64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Removes `//` and single-line `/* */` comments.
fn strip_comments(line: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = line;
    loop {
        let block = rest.find("/*");
        let slash = rest.find("//");
        match (block, slash) {
            (Some(b), s) if s.is_none_or(|s| b < s) => {
                out.push_str(&rest[..b]);
                let close = rest[b + 2..]
                    .find("*/")
                    .ok_or("comment continues past the line")?;
                rest = &rest[b + 2 + close + 2..];
            }
            (_, Some(s)) => {
                out.push_str(&rest[..s]);
                return Ok(out);
            }
            _ => {
                out.push_str(rest);
                return Ok(out);
            }
        }
    }
}
