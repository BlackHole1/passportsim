//! `cargo xtask probes compare <device-capture> <emulator-record>`: a device capture of a
//! campaign probe against the emulator's run of the same image, row by row (the silicon evidence
//! campaign, `specs/notes/silicon-campaign.md`).
//!
//! Every fact line of a campaign probe is `TAG|<fact>|row=<row id>[,<row id>...]|key=value...`:
//! the first positional segment names the fact and is unique within a run, and `row` names the
//! inventory rows the fact bears on. A fact is compared as the whole list of its fields other
//! than `row`, in order; the header, footer and `NOTE` lines are not facts. Per fact the verdict
//! is one of three:
//!
//! - **equal**: both runs printed the fact with the same fields;
//! - **different**: both printed it and at least one field differs; each differing field is
//!   listed with both values, and with the device-to-emulator ratio when both are numbers
//!   (timings differ by nature, so the ratio is what a reader of a `TIME` line looks at);
//! - **not printed**: one run printed it and the other did not, naming which.
//!
//! A row takes the weakest verdict of its facts (different, then not printed, then equal). The
//! report is data, not a gate: the command fails only when a file cannot be read or parsed, or
//! the two runs are of different probes. `FAIL` lines of either run are listed in the report's header.
//!
//! Some fields differ by how the device run was taken rather than by what the chip did
//! ([`METHOD_FIELDS`]). They are left out of the comparison, so they move no verdict, and the
//! report's header lists each one that differs as a `METHOD` line with the reason.

use std::collections::BTreeMap;

/// Fields of one fact that differ between a device capture and the emulator record by capture
/// method, not by chip behaviour: compared by neither verdict, listed as `METHOD` lines.
struct MethodFields {
    /// The probe the fact belongs to.
    probe: &'static str,
    /// The fact, `TAG|label`.
    fact: &'static str,
    /// The fields left out of the comparison.
    fields: &'static [&'static str],
    /// Why they differ.
    why: &'static str,
}

/// Every capture-method field of the campaign probes.
///
/// `probe_campaign_reset` boot1: the device capture starts with the capture tool's RTS hard
/// reset, a `USB_UART_CHIP` reset (raw 0x15, IDF reason 11) of a running chip, so the RTC counter
/// holds the time since power-up; the emulator starts at a power-on (raw 0x01, reason 1). The
/// probe causes every later boot itself, so from boot2 on the fields compare.
const METHOD_FIELDS: &[MethodFields] = &[
    MethodFields {
        probe: "probe_campaign_reset",
        fact: "BOOT|boot1",
        fields: &["reason", "raw", "rtc_time_ms"],
        why: "capture method: the device boot is the capture's RTS reset (USB_UART_CHIP, 0x15) of \
              a running chip, the emulator's a power-on",
    },
    // The RTC times of the same boot, from the same counter.
    MethodFields {
        probe: "probe_campaign_reset",
        fact: "TIMEBASE|boot1",
        fields: &["rtc_counter_us", "rtc_time_us"],
        why: "capture method: the device boot is the capture's RTS reset (USB_UART_CHIP, 0x15) of \
              a running chip, the emulator's a power-on",
    },
];

/// The capture-method entry of `field` of fact `key` of `probe`, if there is one.
fn method_field(probe: &str, key: &str, field: &str) -> Option<&'static MethodFields> {
    METHOD_FIELDS
        .iter()
        .find(|m| m.probe == probe && m.fact == key && m.fields.contains(&field))
}

use super::line::{ProbeLine, parse_console};

/// Tags of lines that are not facts.
const NOT_FACTS: [&str; 3] = ["PROBE", "DONE", "NOTE"];

/// One fact of a run: its rows and its compared fields.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fact {
    rows: Vec<String>,
    fields: Vec<(String, String)>,
}

/// The facts of one run, keyed by `TAG|fact`, and its probe name and failures.
#[derive(Debug, Default)]
struct Facts {
    name: Option<String>,
    facts: BTreeMap<String, Fact>,
    order: Vec<String>,
    failures: Vec<String>,
}

fn read_facts(text: &str, side: &str) -> Result<Facts, String> {
    let console = parse_console(text).map_err(|e| format!("{side}: {e}"))?;
    let mut out = Facts::default();
    for line in &console.lines {
        match line.tag {
            "PROBE" => {
                if out.name.is_none() {
                    out.name = line.field("name").map(str::to_string);
                }
                continue;
            }
            "FAIL" => {
                out.failures.push(format!(
                    "{}: {}",
                    line.field("what").unwrap_or_default(),
                    line.field("detail").unwrap_or_default()
                ));
                continue;
            }
            tag if NOT_FACTS.contains(&tag) => continue,
            _ => {}
        }
        let key = fact_key(line)?;
        let fact = Fact {
            rows: line
                .field("row")
                .map(|r| r.split(',').map(str::to_string).collect())
                .unwrap_or_else(|| vec!["(no row)".to_string()]),
            fields: line
                .fields
                .iter()
                .filter(|(k, _)| *k != "row")
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        if out.facts.insert(key.clone(), fact).is_some() {
            return Err(format!("{side}: the fact `{key}` is printed twice"));
        }
        out.order.push(key);
    }
    Ok(out)
}

/// `TAG|label`, and `@<data>` for a line with a `data` field: the placement kernels of
/// `probe_campaign_timing` (`cpi_at_*`, `cpi_gap_*`, `cpi_flash_*`) print one fact
/// once per operand placement, which `data` names, so the placement is part of the fact.
fn fact_key(line: &ProbeLine<'_>) -> Result<String, String> {
    let label = line
        .label()
        .ok_or_else(|| format!("a `{}` line has no fact label", line.tag))?;
    Ok(match line.field("data") {
        Some(data) => format!("{}|{label}@{data}", line.tag),
        None => format!("{}|{label}", line.tag),
    })
}

/// The verdict of one fact or one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Both sides printed the same fields.
    Equal,
    /// One side did not print the fact.
    NotPrinted,
    /// Both printed it and a field differs.
    Different,
}

impl Verdict {
    fn word(self) -> &'static str {
        match self {
            Verdict::Equal => "equal",
            Verdict::NotPrinted => "not printed",
            Verdict::Different => "different",
        }
    }
}

/// The comparison of two runs.
#[derive(Debug)]
pub struct Report {
    /// The probe both runs are of.
    pub name: String,
    /// Per row, its verdict and the lines that explain it (empty for an equal row).
    pub rows: BTreeMap<String, (Verdict, Vec<String>)>,
    /// Facts compared, and how many of each verdict.
    pub facts: usize,
    /// `FAIL` lines of the device run and of the emulator run.
    pub failures: (Vec<String>, Vec<String>),
    /// The capture-method fields that differ ([`METHOD_FIELDS`]), one explanation each.
    pub method: Vec<String>,
}

impl Report {
    /// How many rows have `verdict`.
    pub fn count(&self, verdict: Verdict) -> usize {
        self.rows.values().filter(|(v, _)| *v == verdict).count()
    }

    /// The report as text: a header line, then one `ROW` line per row with its explanation.
    pub fn render(&self) -> String {
        let mut out = format!(
            "probes compare: {}, {} fact(s) over {} row(s): {} equal, {} different, {} not printed",
            self.name,
            self.facts,
            self.rows.len(),
            self.count(Verdict::Equal),
            self.count(Verdict::Different),
            self.count(Verdict::NotPrinted),
        );
        for (side, list) in [("device", &self.failures.0), ("emulator", &self.failures.1)] {
            for f in list {
                out.push_str(&format!("\nFAIL on the {side}: {f}"));
            }
        }
        for m in &self.method {
            out.push_str(&format!("\nMETHOD {m}"));
        }
        for (row, (verdict, why)) in &self.rows {
            out.push_str(&format!("\nROW {row}: {}", verdict.word()));
            for line in why {
                out.push_str(&format!("\n  {line}"));
            }
        }
        out
    }
}

/// The device-to-emulator ratio of two decimal numbers, for a measured field. A hexadecimal
/// value is a register word or a digest, where a ratio means nothing.
fn ratio(device: &str, emulator: &str) -> Option<String> {
    let parse = |s: &str| -> Option<f64> {
        let s = s.trim();
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            return None;
        }
        s.parse::<f64>().ok()
    };
    let (d, e) = (parse(device)?, parse(emulator)?);
    if e == 0.0 {
        return None;
    }
    Some(format!("{:.3}", d / e))
}

/// Compares a device capture with the emulator's record of the same probe.
pub fn compare(device: &str, emulator: &str) -> Result<Report, String> {
    let dev = read_facts(device, "device capture")?;
    let emu = read_facts(emulator, "emulator record")?;
    let name = match (&dev.name, &emu.name) {
        (Some(d), Some(e)) if d == e => d.clone(),
        (Some(d), Some(e)) => {
            return Err(format!(
                "the device capture is of `{d}` and the emulator record of `{e}`"
            ));
        }
        _ => return Err("a run has no PROBE header line".to_string()),
    };
    let mut keys: Vec<&String> = emu.order.iter().collect();
    for k in &dev.order {
        if !emu.facts.contains_key(k) {
            keys.push(k);
        }
    }
    let mut rows: BTreeMap<String, (Verdict, Vec<String>)> = BTreeMap::new();
    let mut method = Vec::new();
    for key in &keys {
        let (verdict, why, fact_rows) = match (dev.facts.get(*key), emu.facts.get(*key)) {
            (Some(d), Some(e)) => {
                let mut why = Vec::new();
                let names: Vec<&String> = {
                    let mut n: Vec<&String> = e.fields.iter().map(|(k, _)| k).collect();
                    for (k, _) in &d.fields {
                        if !n.contains(&k) {
                            n.push(k);
                        }
                    }
                    n
                };
                for field in names {
                    let dv = d.fields.iter().find(|(k, _)| k == field).map(|(_, v)| v);
                    let ev = e.fields.iter().find(|(k, _)| k == field).map(|(_, v)| v);
                    if dv != ev {
                        let dv = dv.map_or("(absent)", String::as_str);
                        let ev = ev.map_or("(absent)", String::as_str);
                        if let Some(m) = method_field(&name, key, field) {
                            method.push(format!(
                                "{key} {field}: device={dv} emulator={ev} ({})",
                                m.why
                            ));
                            continue;
                        }
                        let r = ratio(dv, ev)
                            .map(|r| format!(" (device/emulator {r})"))
                            .unwrap_or_default();
                        why.push(format!("{key} {field}: device={dv} emulator={ev}{r}"));
                    }
                }
                let verdict = if why.is_empty() {
                    Verdict::Equal
                } else {
                    Verdict::Different
                };
                (verdict, why, e.rows.clone())
            }
            (Some(d), None) => (
                Verdict::NotPrinted,
                vec![format!("{key}: printed by the device only")],
                d.rows.clone(),
            ),
            (None, Some(e)) => (
                Verdict::NotPrinted,
                vec![format!("{key}: printed by the emulator only")],
                e.rows.clone(),
            ),
            (None, None) => unreachable!("every key comes from one of the two runs"),
        };
        for row in fact_rows {
            let entry = rows.entry(row).or_insert((Verdict::Equal, Vec::new()));
            entry.0 = entry.0.max(verdict);
            entry.1.extend(why.iter().cloned());
        }
    }
    Ok(Report {
        name,
        rows,
        facts: keys.len(),
        failures: (dev.failures, emu.failures),
        method,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMU: &str = "\
ESP-ROM:esp32c3-api1-20210207\n\
PROBE|name=p|schema=passport-emu/probe-line/1\n\
REG|a.X|row=a.X|addr=0x1|val=0x00000001\n\
REG|a.Y|row=a.Y|addr=0x2|val=0x00000002\n\
TIME|t|row=a.T,a.X|us=100|sum=7\n\
GATE|g|row=a.G|read=0x5\n\
DONE|name=p|status=ok\n";

    #[test]
    fn equal_different_and_not_printed_are_told_apart_per_row() {
        let device = "\
PROBE|name=p|schema=passport-emu/probe-line/1\r\n\
REG|a.X|row=a.X|addr=0x1|val=0x00000001\r\n\
REG|a.Y|row=a.Y|addr=0x2|val=0x00000003\r\n\
TIME|t|row=a.T,a.X|us=250|sum=7\r\n\
SWD|reset|row=a.S|raw=0x12\r\n\
DONE|name=p|status=ok\r\n";
        let r = compare(device, EMU).expect("both parse");
        assert_eq!(r.name, "p");
        assert_eq!(r.facts, 5);
        assert_eq!(r.rows["a.Y"].0, Verdict::Different);
        assert!(r.rows["a.Y"].1[0].contains("device=0x00000003 emulator=0x00000002"));
        // A timing that differs makes both of its rows different, with the ratio.
        assert_eq!(r.rows["a.T"].0, Verdict::Different);
        assert_eq!(r.rows["a.X"].0, Verdict::Different);
        assert!(r.rows["a.T"].1[0].contains("(device/emulator 2.500)"));
        assert_eq!(r.rows["a.G"].0, Verdict::NotPrinted);
        assert!(r.rows["a.G"].1[0].contains("emulator only"));
        assert_eq!(r.rows["a.S"].0, Verdict::NotPrinted);
        assert!(r.rows["a.S"].1[0].contains("device only"));
        let text = r.render();
        assert!(text.starts_with("probes compare: p, 5 fact(s) over 5 row(s): 0 equal"));
    }

    #[test]
    fn an_identical_run_is_equal_on_every_row() {
        let r = compare(EMU, EMU).expect("both parse");
        assert_eq!(r.count(Verdict::Equal), r.rows.len());
        assert_eq!(r.count(Verdict::Different), 0);
    }

    #[test]
    fn runs_of_two_probes_are_refused() {
        let other = EMU.replace("name=p", "name=q");
        assert!(compare(&other, EMU).unwrap_err().contains("`q`"));
    }

    #[test]
    fn a_fact_printed_twice_is_refused() {
        let twice = EMU.replace("DONE|", "REG|a.X|row=a.X|addr=0x1|val=0x00000001\nDONE|");
        assert!(compare(&twice, EMU).unwrap_err().contains("printed twice"));
    }

    /// A fact printed once per operand placement (`data=`, the `cpi_at_*` kernels of
    /// `probe_campaign_timing`) is one fact per placement, not a fact printed twice.
    #[test]
    fn a_placed_fact_is_one_fact_per_placement() {
        let placed = |static_cycles: u32| {
            EMU.replace(
                "DONE|",
                &format!(
                    "TIME|k|row=a.K|data=static|cycles={static_cycles}\n\
                     TIME|k|row=a.K|data=heap_mid|cycles=3071\nDONE|"
                ),
            )
        };
        let r = compare(&placed(3583), &placed(3582)).expect("both parse");
        assert_eq!(r.rows["a.K"].0, Verdict::Different);
        assert_eq!(
            r.rows["a.K"].1,
            vec!["TIME|k@static cycles: device=3583 emulator=3582 (device/emulator 1.000)"]
        );
        let twice = placed(3583).replace("data=heap_mid", "data=static");
        assert!(
            compare(&twice, EMU)
                .unwrap_err()
                .contains("`TIME|k@static` is printed twice")
        );
    }

    /// The boot1 reset cause and RTC time of `probe_campaign_reset` differ by
    /// capture method, so they move no verdict and are listed as `METHOD` lines; the same field
    /// of a later boot still compares.
    #[test]
    fn capture_method_fields_are_listed_not_compared() {
        let emu = "\
PROBE|name=probe_campaign_reset|schema=passport-emu/probe-line/1\n\
BOOT|boot1|row=r.B|step=0|reason=1|raw=0x01|rtc_magic=0|rtc_time_ms=6\n\
BOOT|boot2|row=r.C|step=1|reason=7|raw=0x12|rtc_magic=1|rtc_time_ms=14\n\
DONE|name=probe_campaign_reset|status=ok\n";
        let device = "\
PROBE|name=probe_campaign_reset|schema=passport-emu/probe-line/1\n\
BOOT|boot1|row=r.B|step=0|reason=11|raw=0x15|rtc_magic=0|rtc_time_ms=2852258\n\
BOOT|boot2|row=r.C|step=1|reason=7|raw=0x12|rtc_magic=1|rtc_time_ms=51\n\
DONE|name=probe_campaign_reset|status=ok\n";
        let r = compare(device, emu).expect("both parse");
        assert_eq!(r.rows["r.B"].0, Verdict::Equal, "{:?}", r.rows["r.B"]);
        assert_eq!(r.rows["r.C"].0, Verdict::Different);
        assert_eq!(r.method.len(), 3);
        assert!(r.method[0].starts_with("BOOT|boot1 reason: device=11 emulator=1 (capture"));
        assert!(
            r.render()
                .contains("\nMETHOD BOOT|boot1 rtc_time_ms: device=2852258")
        );
        // Another probe's fact of the same name compares as usual.
        let other = |t: &str| t.replace("probe_campaign_reset", "p");
        let r = compare(&other(device), &other(emu)).expect("both parse");
        assert_eq!(r.rows["r.B"].0, Verdict::Different);
        assert!(r.method.is_empty());
    }

    #[test]
    fn fail_lines_reach_the_header() {
        let failed = EMU.replace("DONE|", "FAIL|what=x|detail=broke\nDONE|");
        let r = compare(&failed, EMU).expect("both parse");
        assert_eq!(r.failures.0, vec!["x: broke".to_string()]);
        assert!(r.render().contains("FAIL on the device: x: broke"));
    }
}
