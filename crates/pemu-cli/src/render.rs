//! What the binary prints, and in which encoding.
//!
//! - text (the default): the command's text, then the receipt as a one-line suffix
//!   ([`pemu_api::output::Output::to_text`]).
//! - json (`--output json`): one object on stdout, the command's JSON with `receipt`, `artifacts`
//!   and `vt_us` merged in, the MCP `structuredContent` of the same call (`Output::to_json`).
//!
//! An error takes the same fork: one line on stderr in text mode, the typed envelope on stdout in
//! JSON mode, so a program never parses a human sentence.
//!
//! Everything is written as explicit UTF-8 bytes through [`std::io::Write::write_all`], never
//! through a conversion that consults a code page: a `.exe` started from a `cmd.exe` on code page
//! 936 must print the same bytes as a Mac, or goldens compared by equality would differ per host.
//! The packaged manifest declaring the UTF-8 `activeCodePage` is the other half. ANSI is never
//! emitted: enabling virtual terminal processing is a Windows console call outside this crate's
//! allowed dependencies.

use std::io::Write;

use pemu_api::error::ApiError;
use pemu_api::output::Output;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The default.
    #[default]
    Text,
    /// The MCP `structuredContent` of the same call.
    Json,
}

impl Mode {
    #[must_use]
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "text" => Some(Mode::Text),
            "json" => Some(Mode::Json),
            _ => None,
        }
    }
}

/// With the trailing newline.
#[must_use]
pub fn success(output: &Output, mode: Mode) -> String {
    let mut text = match mode {
        Mode::Text => output.to_text(),
        Mode::Json => output.to_json().to_string(),
    };
    text.push('\n');
    text
}

/// Stdout in JSON mode, where a program reads the envelope; stderr in text mode, where a human
/// reads the message and hint.
#[must_use]
pub fn failure(error: &ApiError, mode: Mode) -> (String, Stream) {
    match mode {
        Mode::Json => (format!("{}\n", error.to_json_text()), Stream::Stdout),
        Mode::Text => {
            let mut text = format!("error: {}: {}\n", error.code.name, error.message);
            if let Some(hint) = &error.hint {
                text.push_str(&format!("hint: {hint}\n"));
            }
            for line in error.serial_tail.iter() {
                text.push_str(&format!("serial: {line}\n"));
            }
            (text, Stream::Stderr)
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Results, which a program reads.
    Stdout,
    /// Diagnostics, which a human reads.
    Stderr,
}

/// A broken pipe is not an error: `passportsim status | head -1` closes it, and the exit code must
/// still be the command's own.
pub fn write(text: &str, stream: Stream) {
    let bytes = text.as_bytes();
    let _ = match stream {
        Stream::Stdout => {
            let mut out = std::io::stdout().lock();
            out.write_all(bytes).and_then(|()| out.flush())
        }
        Stream::Stderr => {
            let mut err = std::io::stderr().lock();
            err.write_all(bytes).and_then(|()| err.flush())
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_api::receipt::Receipt;

    fn output() -> Output {
        Output::new(
            serde_json::json!({ "instance": "p1", "state": "paused" }),
            "p1 paused vt=0us",
            Receipt::default(),
        )
    }

    #[test]
    fn text_mode_puts_the_receipt_on_one_line_after_the_body() {
        let text = success(&output(), Mode::Text);
        let lines: Vec<&str> = text.trim_end().split('\n').collect();
        assert_eq!(lines[0], "p1 paused vt=0us");
        assert_eq!(lines[1], "profile fast | deterministic");
        assert_eq!(lines.len(), 2, "the receipt is a one-line suffix");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn json_mode_is_one_object_carrying_the_whole_receipt() {
        let text = success(&output(), Mode::Json);
        assert_eq!(
            text.matches('\n').count(),
            1,
            "exactly one object, one line"
        );
        let value: crate::json::Value =
            serde_json::from_str(text.trim_end()).expect("one JSON object");
        assert_eq!(value["instance"], "p1");
        assert_eq!(value["receipt"]["profile"], "fast");
        assert_eq!(value["receipt"]["determinism"], "deterministic");
        assert!(value["artifacts"].is_array());
    }

    #[test]
    fn an_error_is_a_sentence_on_stderr_and_an_envelope_on_stdout() {
        let error = ApiError::new(pemu_api::error::E_TIMEOUT, "no match within 5s")
            .with_hint("raise `--timeout`")
            .with_serial_tail(vec!["I (12) pk_app: idle".to_owned()]);
        let (text, stream) = failure(&error, Mode::Text);
        assert_eq!(stream, Stream::Stderr);
        assert!(text.starts_with("error: E_TIMEOUT: no match within 5s\n"));
        assert!(text.contains("hint: raise `--timeout`"));
        assert!(text.contains("serial: I (12) pk_app: idle"));

        let (text, stream) = failure(&error, Mode::Json);
        assert_eq!(stream, Stream::Stdout);
        let value: crate::json::Value =
            serde_json::from_str(text.trim_end()).expect("the error envelope");
        assert_eq!(
            value["code"], "E_TIMEOUT",
            "the error envelope is flat, the same object the wasm and MCP surfaces return"
        );
    }

    #[test]
    fn every_rendering_is_valid_utf8_bytes_and_never_ansi() {
        let text = success(&output(), Mode::Text);
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
        assert!(
            !text.contains('\u{1b}'),
            "no escape code reaches a plain cmd.exe"
        );
        assert!(!text.contains('\r'), "LF only");
    }

    #[test]
    fn the_output_mode_is_read_from_its_two_words() {
        assert_eq!(Mode::parse("text"), Some(Mode::Text));
        assert_eq!(Mode::parse("json"), Some(Mode::Json));
        assert_eq!(Mode::parse("yaml"), None);
        assert_eq!(Mode::default(), Mode::Text);
    }
}
