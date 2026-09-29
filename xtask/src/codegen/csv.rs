//! Rows of `specs/c3-registers.csv`; `specs/README.md` section 2 describes the columns.

use super::access::Access;

/// Header line of `specs/c3-registers.csv`.
pub const HEADER: &str = "block,register,offset,field,shift,width,access,access_basis,\
reset_chip,reset_system,reset_core,idf_access,cite,note";

/// Path of the table, relative to the workspace root.
pub const SPEC_PATH: &str = "specs/c3-registers.csv";

/// Field value after a reset of one scope.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ScopeReset {
    Value(u32),
    /// The field keeps its value across this reset scope.
    Keep,
}

/// One field row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub block: String,
    pub register: String,
    pub offset: u32,
    pub field: String,
    pub shift: u8,
    pub width: u8,
    pub access: Access,
    /// False when the access type is inferred (`UNVERIFIED`).
    pub exact: bool,
    pub reset_chip: u32,
    pub reset_system: ScopeReset,
    pub reset_core: ScopeReset,
    pub idf_access: String,
    pub cite: String,
    pub note: String,
}

fn scope_str(s: ScopeReset) -> String {
    match s {
        ScopeReset::Value(v) => hex(v),
        ScopeReset::Keep => "keep".to_string(),
    }
}

fn hex(v: u32) -> String {
    format!("0x{v:X}")
}

/// Renders the table with its header line. Fails when a text value holds a comma, a quote or a
/// line break.
pub fn render(rows: &[Row]) -> Result<String, String> {
    let mut out = String::from(HEADER);
    out.push('\n');
    for r in rows {
        let cols = [
            r.block.clone(),
            r.register.clone(),
            format!("0x{:03X}", r.offset),
            r.field.clone(),
            r.shift.to_string(),
            r.width.to_string(),
            r.access.as_str().to_string(),
            if r.exact { "idf" } else { "UNVERIFIED" }.to_string(),
            hex(r.reset_chip),
            scope_str(r.reset_system),
            scope_str(r.reset_core),
            r.idf_access.clone(),
            r.cite.clone(),
            r.note.clone(),
        ];
        if let Some(bad) = cols.iter().find(|c| c.contains([',', '"', '\n', '\r'])) {
            return Err(format!(
                "{}.{}: value `{bad}` is not CSV-safe",
                r.register, r.field
            ));
        }
        out.push_str(&cols.join(","));
        out.push('\n');
    }
    Ok(out)
}

/// Parses the table and checks each row: a known block name is not checked here, but widths,
/// shifts, reset values and access types are.
pub fn parse(text: &str) -> Result<Vec<Row>, String> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, h)) if h == HEADER => {}
        _ => return Err(format!("{SPEC_PATH}: unexpected header line")),
    }
    let mut rows = Vec::new();
    for (idx, raw) in lines {
        let line = idx + 1;
        let err = |what: &str| format!("{SPEC_PATH}:{line}: {what}");
        let c: Vec<&str> = raw.split(',').collect();
        if c.len() != 14 {
            return Err(err("expected 14 columns"));
        }
        let num = |s: &str| -> Result<u32, String> {
            s.strip_prefix("0x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .ok_or_else(|| err(&format!("bad hex `{s}`")))
        };
        let scope = |s: &str| -> Result<ScopeReset, String> {
            if s == "keep" {
                Ok(ScopeReset::Keep)
            } else {
                num(s).map(ScopeReset::Value)
            }
        };
        let shift: u8 = c[4].parse().map_err(|_| err("bad shift"))?;
        let width: u8 = c[5].parse().map_err(|_| err("bad width"))?;
        if width == 0 || u32::from(shift) + u32::from(width) > 32 {
            return Err(err("field outside 32 bits"));
        }
        let exact = match c[7] {
            "idf" => true,
            "UNVERIFIED" => false,
            other => return Err(err(&format!("bad access_basis `{other}`"))),
        };
        let row = Row {
            block: c[0].to_string(),
            register: c[1].to_string(),
            offset: num(c[2])?,
            field: c[3].to_string(),
            shift,
            width,
            access: Access::parse(c[6]).ok_or_else(|| err("bad access"))?,
            exact,
            reset_chip: num(c[8])?,
            reset_system: scope(c[9])?,
            reset_core: scope(c[10])?,
            idf_access: c[11].to_string(),
            cite: c[12].to_string(),
            note: c[13].to_string(),
        };
        let max = if width == 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        let fits = |v: ScopeReset| !matches!(v, ScopeReset::Value(x) if x > max);
        if row.reset_chip > max || !fits(row.reset_system) || !fits(row.reset_core) {
            return Err(err("reset value wider than the field"));
        }
        rows.push(row);
    }
    Ok(rows)
}
