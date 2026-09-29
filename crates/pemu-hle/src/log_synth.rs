//! Log synthesis: console lines the replaced functions would have printed, from
//! `specs/hle/idf-5.5.3/log-lines.toml`.
//!
//! Replacing `esp_bt_controller_init` removes the device's `BLE_INIT` and `phy_init` lines. The
//! handler restores them by nested calls to the image's own log functions, so the guest applies
//! its own level filter, timestamp and formatting. Under IDF 5.5 log v1 (`log/include/esp_log.h`,
//! `log/src/log.c`) a line is two calls:
//!
//! 1. `esp_log_timestamp()` ([`LogSynth::timestamp_call`]);
//! 2. `esp_log(level, tag, format, timestamp, tag, text)` ([`LogSynth::call`]), with the text as
//!    the last `%s`, so a `%` in it is printed rather than interpreted.
//!
//! The simpler `esp_log(config, tag, "%s", text)` of `specs/notes/g3-behavior.md` (`g3-hook-api`)
//! prints no `I (<ms>) <tag>: ` prefix and no newline, so it cannot reproduce a device line.

use crate::guest_call::{Arg, CallRequest, SCRATCH_WORD};

/// IDF log levels (`esp_log_level.h`), the first argument of the log v1 `esp_log`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    None = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Verbose = 5,
}

/// One log line of a binding profile: text plus a level and a tag; rendering is the guest's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogLineTemplate {
    pub level: Option<LogLevel>,
    /// `BLE_INIT` or `phy_init`.
    pub tag: String,
    /// Already carrying any value the handler substituted.
    pub text: String,
}

impl LogLineTemplate {
    pub fn new(level: LogLevel, tag: &str, text: &str) -> LogLineTemplate {
        LogLineTemplate {
            level: Some(level),
            tag: tag.to_string(),
            text: text.to_string(),
        }
    }
}

/// Checks that `format` holds exactly the conversions `%lu`, `%s`, `%s`, in that order, which is
/// what [`LogSynth::call`] passes.
pub fn check_format(format: &str) -> Result<(), String> {
    let conversions: Vec<&str> = format
        .match_indices('%')
        .map(|(at, _)| {
            let rest = &format[at..];
            if rest.starts_with("%lu") {
                "%lu"
            } else if rest.starts_with("%s") {
                "%s"
            } else {
                "?"
            }
        })
        .collect();
    if conversions == ["%lu", "%s", "%s"] {
        Ok(())
    } else {
        Err(format!("format `{format}` is not `%lu`, `%s`, `%s`"))
    }
}

/// Builds the nested calls that make the guest print a synthesized line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSynth {
    esp_log: Option<u32>,
    esp_log_timestamp: Option<u32>,
    format: String,
}

impl LogSynth {
    /// `None` unless the image has both `esp_log` and `esp_log_timestamp`: a missing symbol is a
    /// skip, never a guessed address.
    pub fn new(
        esp_log: Option<u32>,
        esp_log_timestamp: Option<u32>,
        format: &str,
    ) -> Result<LogSynth, String> {
        check_format(format)?;
        Ok(LogSynth {
            esp_log,
            esp_log_timestamp,
            format: format.to_string(),
        })
    }

    pub fn is_bound(&self) -> bool {
        self.esp_log.is_some() && self.esp_log_timestamp.is_some()
    }

    /// The first call of a line; its `a0` is the timestamp [`LogSynth::call`] takes.
    pub fn timestamp_call(&self) -> Option<CallRequest> {
        self.esp_log?;
        Some(CallRequest::new(
            "esp_log_timestamp",
            self.esp_log_timestamp?,
            &[],
        ))
    }

    /// The second call of a line, with the three strings in the scratch block on the borrowed
    /// stack, so the headroom guard counts them.
    ///
    /// `esp_log` takes the console lock, so the call is blocking and never legal from a magic
    /// ISR. The line counts as synthesized once this call has returned.
    pub fn call(&self, line: &LogLineTemplate, timestamp: u32) -> Option<CallRequest> {
        let func = self.esp_log?;
        self.esp_log_timestamp?;
        let level = line.level.unwrap_or(LogLevel::Info);
        let mut scratch =
            Vec::with_capacity(line.tag.len() + self.format.len() + line.text.len() + 3);
        scratch.extend_from_slice(line.tag.as_bytes());
        scratch.push(0);
        let format_at = scratch.len() as u16;
        scratch.extend_from_slice(self.format.as_bytes());
        scratch.push(0);
        let text_at = scratch.len() as u16;
        scratch.extend_from_slice(line.text.as_bytes());
        scratch.push(0);
        // Every scratch pointer needs a whole word inside the block (`guest_call::SCRATCH_WORD`);
        // a short text at the end is padded with NULs.
        scratch.resize(scratch.len().max(usize::from(text_at) + SCRATCH_WORD), 0);
        Some(
            CallRequest::new(
                "esp_log",
                func,
                &[
                    Arg::Val(level as u32),
                    Arg::Scratch(0),
                    Arg::Scratch(format_at),
                    Arg::Val(timestamp),
                    Arg::Scratch(0),
                    Arg::Scratch(text_at),
                ],
            )
            .with_scratch(scratch),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuation::Continuations;
    use crate::guest_call::{A0, CallEngine, GuardProfile, GuestView, HleErrorKind, RA, SP};
    use crate::magic::MagicPcs;
    use crate::test_guest::{DATA, SynthGuest};

    /// Where Passport Keys has `esp_log` (`specs/notes/g3-behavior.md`, `g3-hook-api`); the
    /// synthesis resolves it per image.
    const PK_ESP_LOG: u32 = 0x4039_5BBA;
    const PK_TIMESTAMP: u32 = 0x4039_5B00;
    const INFO: &str = "I (%lu) %s: %s\n";
    const TCB: u32 = DATA + 0x1000;
    const STACK: u32 = DATA + 0x2000;

    fn engine(g: &SynthGuest) -> CallEngine {
        CallEngine {
            pcs: MagicPcs::from_spec().expect("specs/magic-pcs.toml"),
            guards: GuardProfile::default(),
            stack: g.stack_symbols(),
        }
    }

    fn synth() -> LogSynth {
        LogSynth::new(Some(PK_ESP_LOG), Some(PK_TIMESTAMP), INFO).expect("a v1 format")
    }

    fn c_string(g: &SynthGuest, addr: u32) -> String {
        let bytes = g.bytes(addr, 256);
        let end = bytes.iter().position(|b| *b == 0).expect("terminated");
        String::from_utf8(bytes[..end].to_vec()).expect("utf-8")
    }

    #[test]
    fn only_the_log_v1_conversions_are_accepted_as_the_format() {
        assert!(check_format(INFO).is_ok());
        for bad in [
            "%s",
            "I (%s) %lu: %s\n",
            "I (%lu) %s: %s %d\n",
            "I (%lu) %s\n",
        ] {
            assert!(check_format(bad).is_err(), "{bad}");
        }
        assert!(LogSynth::new(Some(1), Some(2), "%s").is_err());
    }

    #[test]
    fn a_line_is_a_timestamp_call_then_esp_log_with_the_v1_layout_on_the_borrowed_stack() {
        let mut g = SynthGuest::new();
        g.with_task(&GuardProfile::default(), TCB, STACK, STACK + 0x1000);
        g.set_reg(RA, 0x4200_1234);
        let engine = engine(&g);
        let synth = synth();
        let ts = synth.timestamp_call().expect("bound");
        assert_eq!((ts.func, ts.nargs), (PK_TIMESTAMP, 0));

        // A `%` in the text must reach the console as a `%`, not as a conversion.
        let line = LogLineTemplate::new(LogLevel::Info, "phy_init", "phy_version 100%, 0");
        let call = synth.call(&line, 407).expect("bound");
        let frame = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect("the guards pass");
        engine.start(&mut g, &frame, &call).expect("started");
        assert_eq!(g.reg(RA), engine.pcs.ret, "returns through the magic PC");
        assert_eq!(g.reg(SP), frame.sp1);
        assert_eq!(g.reg(A0), LogLevel::Info as u32);
        assert_eq!(c_string(&g, g.reg(A0 + 1)), "phy_init");
        assert_eq!(c_string(&g, g.reg(A0 + 2)), INFO, "the profile's format");
        assert_eq!(g.reg(A0 + 3), 407, "the timestamp");
        assert_eq!(c_string(&g, g.reg(A0 + 4)), "phy_init", "the tag again");
        assert_eq!(c_string(&g, g.reg(A0 + 5)), "phy_version 100%, 0");
        for r in [1, 2, 4, 5] {
            let at = g.reg(A0 + r);
            assert!(at >= frame.sp1 && at < frame.sp0, "a{r} {at:#x}");
        }
    }

    #[test]
    fn a_line_the_guards_refuse_is_not_started() {
        // The stack is one frame short of the headroom: the guard refuses and nothing is printed.
        let guards = GuardProfile::default();
        let mut g = SynthGuest::new();
        g.with_task(&guards, TCB, STACK, STACK + guards.headroom + 16);
        let engine = engine(&g);
        let call = synth()
            .call(&LogLineTemplate::new(LogLevel::Info, "BLE_INIT", "x"), 1)
            .expect("bound");
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect_err("no headroom for the strings");
        assert_eq!(err.kind, HleErrorKind::StackHeadroom);
        // The same line from ISR context is refused as blocking.
        g.set_reg(SP, STACK + 0x1000);
        let bottom = DATA + 0x8000;
        g.with_isr_stack(bottom, bottom + 1024);
        let engine = super::tests::engine(&g);
        let err = engine
            .prepare(&mut g, &Continuations::default(), 0, &call)
            .expect_err("esp_log takes the console lock");
        assert_eq!(err.kind, HleErrorKind::BlockingInIsr);
    }

    #[test]
    fn a_short_text_still_leaves_a_word_behind_every_pointer() {
        let call = synth()
            .call(&LogLineTemplate::new(LogLevel::Warn, "t", ""), 0)
            .expect("bound");
        for arg in call.args.iter().take(usize::from(call.nargs)) {
            if let Arg::Scratch(off) = arg {
                assert!(usize::from(*off) + SCRATCH_WORD <= call.scratch.len());
            }
        }
    }

    #[test]
    fn an_image_without_either_log_function_synthesizes_nothing() {
        let line = LogLineTemplate::new(LogLevel::Info, "BLE_INIT", "x");
        for (log, ts) in [(None, Some(PK_TIMESTAMP)), (Some(PK_ESP_LOG), None)] {
            let synth = LogSynth::new(log, ts, INFO).expect("format");
            assert!(!synth.is_bound());
            assert_eq!(synth.timestamp_call(), None);
            assert_eq!(synth.call(&line, 0), None);
        }
    }

    #[test]
    fn the_scratch_block_stays_inside_the_guard_cap_for_a_realistic_line() {
        let guards = GuardProfile::default();
        let line = LogLineTemplate::new(
            LogLevel::Info,
            "BLE_INIT",
            "Feature Config, ADV:1, BLE_50:1, DTM:1, SCAN:1, CCA:0, SMP:1, CONNECT:1",
        );
        let call = synth().call(&line, 0).expect("bound");
        assert!(
            call.scratch.len() <= usize::from(guards.max_scratch),
            "{} bytes",
            call.scratch.len()
        );
    }
}
