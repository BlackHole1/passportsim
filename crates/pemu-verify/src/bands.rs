//! Timestamp bands. A golden directory carries a `bands.toml` naming the anchors (ESP_LOG lines
//! whose first occurrence is compared), and it may narrow the two tolerances:
//!
//! - **deltas** between consecutive anchors present in both logs:
//!   `|dt_emu - dt_dev| <= max(5 ms, 0.20 x dt_dev)`;
//! - **absolute** timestamps: `|t_emu - t_dev| <= max(10 ms, 0.20 x t_dev)`.
//!
//! Deltas are checked first: when one fails the absolute check is skipped, because one late phase
//! shifts every later timestamp and would bury the real cause. Tolerances are integer arithmetic,
//! so a band decides the same way on every host.

use crate::normalize::Console;
use crate::spec_toml::{self, Error as SpecError};

/// One tolerance: a floor in milliseconds and a percentage of the reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tolerance {
    pub floor_ms: u32,
    pub percent: u32,
}

impl Tolerance {
    /// The allowance for a reference value, `max(floor_ms, percent% of reference)`.
    pub fn allowance_ms(self, reference_ms: u32) -> u32 {
        let scaled = (u64::from(reference_ms) * u64::from(self.percent) / 100) as u32;
        scaled.max(self.floor_ms)
    }

    pub fn allows(self, reference_ms: u32, emulated_ms: u32) -> bool {
        reference_ms.abs_diff(emulated_ms) <= self.allowance_ms(reference_ms)
    }
}

/// The delta band: `max(5 ms, 0.20 x dt_dev)`.
pub const DELTA: Tolerance = Tolerance {
    floor_ms: 5,
    percent: 20,
};

/// The absolute band: `max(10 ms, 0.20 x t_dev)`.
pub const ABSOLUTE: Tolerance = Tolerance {
    floor_ms: 10,
    percent: 20,
};

/// One anchor: a name for reports and the substring that finds its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub name: String,
    /// Substring matched against a normalized line; the first match wins.
    pub needle: String,
}

/// A golden's `bands.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bands {
    pub anchors: Vec<Anchor>,
    /// Delta tolerance; [`DELTA`] unless the file narrows it.
    pub delta: Tolerance,
    /// Absolute tolerance; [`ABSOLUTE`] unless the file narrows it.
    pub absolute: Tolerance,
}

impl Default for Bands {
    fn default() -> Self {
        Bands {
            anchors: Vec::new(),
            delta: DELTA,
            absolute: ABSOLUTE,
        }
    }
}

impl Bands {
    /// Parses a `bands.toml`.
    ///
    /// ```text
    /// schema = 1
    /// delta_floor_ms = 5      # optional, defaults to DELTA
    /// delta_percent = 20      # optional
    /// absolute_floor_ms = 10  # optional
    /// absolute_percent = 20   # optional
    ///
    /// [[anchor]]
    /// name = "app loaded"
    /// line = "boot: Loaded app from partition"
    /// ```
    pub fn parse(text: &str) -> Result<Bands, SpecError> {
        let doc = spec_toml::parse(text)?;
        let field = |key: &str, fallback: u32| -> Result<u32, SpecError> {
            match doc.root.pairs.get(key) {
                None => Ok(fallback),
                Some(value) => value
                    .as_int()
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(|| SpecError::new(1, format!("`{key}` is not a small integer"))),
            }
        };
        let mut anchors = Vec::new();
        for table in doc.array("anchor") {
            anchors.push(Anchor {
                name: table.str_field("name")?.to_string(),
                needle: table.str_field("line")?.to_string(),
            });
        }
        if anchors.is_empty() {
            return Err(SpecError::new(1, "a bands file needs at least one anchor"));
        }
        Ok(Bands {
            anchors,
            delta: Tolerance {
                floor_ms: field("delta_floor_ms", DELTA.floor_ms)?,
                percent: field("delta_percent", DELTA.percent)?,
            },
            absolute: Tolerance {
                floor_ms: field("absolute_floor_ms", ABSOLUTE.floor_ms)?,
                percent: field("absolute_percent", ABSOLUTE.percent)?,
            },
        })
    }

    /// Checks one emulated console against a reference console.
    pub fn check(&self, reference: &Console, emulated: &Console) -> Report {
        let mut report = Report::default();
        let mut both: Vec<(&str, u32, u32)> = Vec::new();
        for anchor in &self.anchors {
            match (
                reference.first_ts(&anchor.needle),
                emulated.first_ts(&anchor.needle),
            ) {
                (Some(dev), Some(emu)) => both.push((anchor.name.as_str(), dev, emu)),
                (Some(_), None) => report.missing_emulated.push(anchor.name.clone()),
                (None, _) => report.missing_reference.push(anchor.name.clone()),
            }
        }
        for pair in both.windows(2) {
            let (from, dev_from, emu_from) = pair[0];
            let (to, dev_to, emu_to) = pair[1];
            let dev_dt = dev_to.saturating_sub(dev_from);
            let emu_dt = emu_to.saturating_sub(emu_from);
            if !self.delta.allows(dev_dt, emu_dt) {
                report.failures.push(Failure::Delta {
                    from: from.to_string(),
                    to: to.to_string(),
                    reference_ms: dev_dt,
                    emulated_ms: emu_dt,
                    allowed_ms: self.delta.allowance_ms(dev_dt),
                });
            }
        }
        if !report.failures.is_empty() {
            report.absolute_skipped = true;
            return report;
        }
        for (name, dev, emu) in both {
            if !self.absolute.allows(dev, emu) {
                report.failures.push(Failure::Absolute {
                    anchor: name.to_string(),
                    reference_ms: dev,
                    emulated_ms: emu,
                    allowed_ms: self.absolute.allowance_ms(dev),
                });
            }
        }
        report
    }
}

/// One band failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A delta between two consecutive anchors left its band.
    Delta {
        from: String,
        to: String,
        reference_ms: u32,
        emulated_ms: u32,
        allowed_ms: u32,
    },
    /// An absolute timestamp left its band.
    Absolute {
        anchor: String,
        reference_ms: u32,
        emulated_ms: u32,
        allowed_ms: u32,
    },
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::Delta {
                from,
                to,
                reference_ms,
                emulated_ms,
                allowed_ms,
            } => write!(
                f,
                "delta {from} -> {to}: reference {reference_ms} ms, emulated {emulated_ms} ms, \
                 allowed +/-{allowed_ms} ms"
            ),
            Failure::Absolute {
                anchor,
                reference_ms,
                emulated_ms,
                allowed_ms,
            } => write!(
                f,
                "absolute {anchor}: reference {reference_ms} ms, emulated {emulated_ms} ms, \
                 allowed +/-{allowed_ms} ms"
            ),
        }
    }
}

/// The outcome of a band check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub failures: Vec<Failure>,
    /// Anchors the reference has and the emulator does not.
    pub missing_emulated: Vec<String>,
    /// Anchors the reference itself does not have; the bands file names a line that never ran.
    pub missing_reference: Vec<String>,
    /// True when a delta failed, so the absolute check was not run.
    pub absolute_skipped: bool,
}

impl Report {
    /// True when nothing failed and no anchor is missing from the emulated console.
    pub fn is_pass(&self) -> bool {
        self.failures.is_empty() && self.missing_emulated.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::{BootSelect, normalize};

    const BANDS: &str = concat!(
        "schema = 1\n",
        "\n",
        "[[anchor]]\n",
        "name = \"bootloader banner\"\n",
        "line = \"boot: ESP-IDF\"\n",
        "\n",
        "[[anchor]]\n",
        "name = \"app loaded\"\n",
        "line = \"boot: Loaded app from partition\"\n",
        "\n",
        "[[anchor]]\n",
        "name = \"app_main called\"\n",
        "line = \"main_task: Calling app_main()\"\n",
    );

    fn console(entries: &[(u32, &str)]) -> Console {
        let mut text = String::new();
        for (ts, msg) in entries {
            text.push_str(&format!("I ({ts}) {msg}\n"));
        }
        normalize(text.as_bytes(), BootSelect::AllBoots)
    }

    fn reference() -> Console {
        console(&[
            (24, "boot: ESP-IDF v5.5.3 2nd stage bootloader"),
            (196, "boot: Loaded app from partition at offset 0x10000"),
            (210, "main_task: Calling app_main()"),
        ])
    }

    #[test]
    fn tolerances_are_the_arch_values() {
        assert_eq!(DELTA.allowance_ms(0), 5, "the floor applies at zero");
        assert_eq!(DELTA.allowance_ms(100), 20);
        assert_eq!(ABSOLUTE.allowance_ms(0), 10);
        assert_eq!(ABSOLUTE.allowance_ms(1000), 200);
        // 20 % of 24 ms is 4.8 ms, under the 5 ms floor, so the floor decides.
        assert_eq!(DELTA.allowance_ms(24), 5);
    }

    #[test]
    fn a_run_inside_both_bands_passes() {
        let bands = Bands::parse(BANDS).expect("parses");
        let emulated = console(&[
            (26, "boot: ESP-IDF v5.5.3 2nd stage bootloader"),
            (200, "boot: Loaded app from partition at offset 0x10000"),
            (213, "main_task: Calling app_main()"),
        ]);
        let report = bands.check(&reference(), &emulated);
        assert!(report.is_pass(), "{report:?}");
    }

    #[test]
    fn a_delta_outside_its_band_fails_and_stops_the_absolute_check() {
        let bands = Bands::parse(BANDS).expect("parses");
        // The app-load phase takes 172 ms on the reference and 20 ms here: 152 ms out, against
        // an allowance of 34 ms.
        let emulated = console(&[
            (24, "boot: ESP-IDF v5.5.3 2nd stage bootloader"),
            (44, "boot: Loaded app from partition at offset 0x10000"),
            (58, "main_task: Calling app_main()"),
        ]);
        let report = bands.check(&reference(), &emulated);
        assert!(report.absolute_skipped);
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert_eq!(
            report.failures[0],
            Failure::Delta {
                from: "bootloader banner".into(),
                to: "app loaded".into(),
                reference_ms: 172,
                emulated_ms: 20,
                allowed_ms: 34,
            }
        );
        assert!(!report.is_pass());
    }

    #[test]
    fn a_uniform_offset_keeps_the_deltas_and_fails_the_absolute_band() {
        let bands = Bands::parse(BANDS).expect("parses");
        // Every delta is exact; every timestamp is 300 ms late.
        let emulated = console(&[
            (324, "boot: ESP-IDF v5.5.3 2nd stage bootloader"),
            (496, "boot: Loaded app from partition at offset 0x10000"),
            (510, "main_task: Calling app_main()"),
        ]);
        let report = bands.check(&reference(), &emulated);
        assert!(!report.absolute_skipped);
        assert_eq!(report.failures.len(), 3);
        assert!(matches!(report.failures[0], Failure::Absolute { .. }));
    }

    #[test]
    fn a_missing_anchor_is_reported_and_does_not_pair_across_the_gap() {
        let bands = Bands::parse(BANDS).expect("parses");
        let emulated = console(&[
            (24, "boot: ESP-IDF v5.5.3 2nd stage bootloader"),
            (210, "main_task: Calling app_main()"),
        ]);
        let report = bands.check(&reference(), &emulated);
        assert_eq!(report.missing_emulated, vec!["app loaded".to_string()]);
        assert!(!report.is_pass());
        // The remaining pair is banner -> app_main: 186 ms against 186 ms, inside the band.
        assert!(report.failures.is_empty(), "{report:?}");
    }

    #[test]
    fn a_bands_file_may_narrow_a_tolerance() {
        let text = format!("delta_floor_ms = 1\ndelta_percent = 2\n{BANDS}");
        let bands = Bands::parse(&text).expect("parses");
        assert_eq!(
            bands.delta,
            Tolerance {
                floor_ms: 1,
                percent: 2
            }
        );
        assert_eq!(bands.absolute, ABSOLUTE);
    }

    #[test]
    fn a_bands_file_without_anchors_is_refused() {
        assert!(Bands::parse("schema = 1\n").is_err());
    }
}
