//! Command outputs and artifact references.
//!
//! The redaction pass runs over the raw text before it is shaped, because shaping cuts and a cut
//! secret cannot be matched again.
//!
//! Paths in an output are relative to the instance artifact root and forward-slashed on every host.
//! Only `status` reports the absolute root, home-redacted: an absolute path elsewhere would expose
//! the user's name and break the example goldens compared across hosts.

use crate::receipt::Receipt;
use crate::shape::{ShapeLimits, TEXT_BUDGET_CHARS};

#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub json: serde_json::Value,
    pub text: String,
    pub artifacts: Vec<ArtifactRef>,
    pub receipt: Receipt,
    pub vt_us: u64,
}

impl Output {
    #[must_use]
    pub fn new(json: serde_json::Value, text: impl Into<String>, receipt: Receipt) -> Output {
        Output {
            json,
            text: text.into(),
            vt_us: receipt.vt_us,
            artifacts: Vec::new(),
            receipt,
        }
    }

    /// Adds an artifact after checking its path with [`check_artifact_path`].
    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Result<Output, PathError> {
        check_artifact_path(&artifact.path)?;
        self.artifacts.push(artifact);
        Ok(self)
    }

    /// The receipt is a one-line suffix; an empty text gives the receipt line alone.
    #[must_use]
    pub fn to_text(&self) -> String {
        let receipt = self.receipt.one_line();
        if self.text.is_empty() {
            receipt
        } else {
            format!("{}\n{receipt}", self.text)
        }
    }

    /// The command's own object with `receipt`, `artifacts` and `vt_us` merged in. A non-object
    /// `json` is wrapped under `result`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = match &self.json {
            serde_json::Value::Object(map) => map.clone(),
            other => {
                let mut map = serde_json::Map::new();
                map.insert("result".into(), other.clone());
                map
            }
        };
        map.insert("vt_us".into(), self.vt_us.into());
        map.insert(
            "artifacts".into(),
            serde_json::Value::Array(self.artifacts.iter().map(ArtifactRef::to_json).collect()),
        );
        map.insert("receipt".into(), self.receipt.to_json());
        serde_json::Value::Object(map)
    }

    #[must_use]
    pub fn text_chars(&self) -> usize {
        self.to_text().chars().count()
    }

    #[must_use]
    pub fn fits_text_budget(&self) -> bool {
        self.text_chars() <= TEXT_BUDGET_CHARS
    }

    /// Re-shapes the text to fit `limits`. The text must already be redacted, because a cut secret
    /// can no longer be matched.
    #[must_use]
    pub fn shaped(mut self, limits: &ShapeLimits) -> Output {
        // The receipt line comes out of the budget the body is shaped to.
        let receipt_line = self.receipt.one_line().chars().count() + 1;
        let mut limits = *limits;
        limits.budget_chars = limits
            .budget_chars
            .map(|budget| budget.saturating_sub(receipt_line));
        self.text = crate::shape::shape_text_within(&self.text, &limits).text();
        self
    }

    /// Must always be empty outside `status`.
    #[must_use]
    pub fn absolute_paths(&self) -> Vec<String> {
        let mut found = Vec::new();
        collect_absolute_paths(&self.to_json(), &mut found);
        for token in self.text.split_whitespace() {
            if contains_absolute_path(token) {
                found.push(token.to_string());
            }
        }
        found
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRef {
    /// Relative to the instance artifact root, forward-slashed on every host.
    pub path: String,
    pub sha256: String,
    pub media_type: String,
    pub bytes: u64,
}

impl ArtifactRef {
    pub fn new(
        path: impl Into<String>,
        sha256: impl Into<String>,
        media_type: impl Into<String>,
        bytes: u64,
    ) -> Result<ArtifactRef, PathError> {
        let path = path.into();
        check_artifact_path(&path)?;
        Ok(ArtifactRef {
            path,
            sha256: sha256.into(),
            media_type: media_type.into(),
            bytes,
        })
    }

    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path,
            "sha256": self.sha256,
            "media_type": self.media_type,
            "bytes": self.bytes,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Absolute,
    /// A backslash is a file-name character on one host and a separator on the other.
    Backslash,
    Traversal,
    EmptySegment,
    BadSegment(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => f.write_str("the path is empty"),
            PathError::Absolute => f.write_str(
                "the path is absolute; artifact paths are relative to the artifact root",
            ),
            PathError::Backslash => {
                f.write_str("the path uses a backslash; artifact paths are forward-slashed")
            }
            PathError::Traversal => f.write_str("the path has a `.` or `..` segment"),
            PathError::EmptySegment => f.write_str("the path has an empty segment"),
            PathError::BadSegment(segment) => {
                write!(f, "the segment `{segment}` is outside [a-z0-9][a-z0-9._-]*")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// Relative, forward-slashed, no traversal, and every segment inside `[a-z0-9][a-z0-9._-]*`.
pub fn check_artifact_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.contains('\\') {
        return Err(PathError::Backslash);
    }
    if is_absolute_path(path) {
        return Err(PathError::Absolute);
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(PathError::EmptySegment);
        }
        if segment == "." || segment == ".." {
            return Err(PathError::Traversal);
        }
        let mut chars = segment.chars();
        let first = chars.next().unwrap_or('/');
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(PathError::BadSegment(segment.to_string()));
        }
        if !chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(PathError::BadSegment(segment.to_string()));
        }
    }
    Ok(())
}

/// A leading `/` or `\` (including `\\?\` and UNC) or a drive letter and separator. A home-redacted
/// `~/...` path is not absolute.
#[must_use]
pub fn is_absolute_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    match bytes {
        [b'/', ..] | [b'\\', ..] => true,
        [drive, b':', sep, ..] if drive.is_ascii_alphabetic() && matches!(sep, b'/' | b'\\') => {
            true
        }
        _ => false,
    }
}

/// Scans by offset, not by whitespace token, because a path is usually quoted, parenthesised or
/// after `key=`. A candidate counts only where the preceding character cannot continue a path, so
/// `p1/screen.png`, `~/...` and `https://host/path` do not match.
#[must_use]
pub fn contains_absolute_path(text: &str) -> bool {
    find_absolute_path(text).is_some()
}

#[must_use]
pub fn find_absolute_path(text: &str) -> Option<usize> {
    text.char_indices()
        .filter(|&(at, _)| {
            at == 0
                || text[..at]
                    .chars()
                    .next_back()
                    .is_some_and(ends_a_path_candidate)
        })
        .find(|&(at, _)| is_absolute_path(&text[at..]))
        .map(|(at, _)| at)
}

/// Path characters and alphanumerics continue a relative path, a home-redacted path or a URL.
fn ends_a_path_candidate(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '~' | '.' | '-' | '_' | ':' | '/' | '\\'))
}

pub(crate) fn collect_absolute_paths(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            if contains_absolute_path(s) {
                found.push(s.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_absolute_paths(item, found);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if contains_absolute_path(key) {
                    found.push(key.clone());
                }
                collect_absolute_paths(item, found);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{Determinism, FidelityClass, FidelityEntry};

    fn receipt() -> Receipt {
        Receipt {
            vt_us: 845_213,
            journal_len: 12,
            determinism: Determinism::Deterministic,
            fidelity: vec![FidelityEntry {
                subsystem: "cpu".to_string(),
                class: FidelityClass::B,
            }],
            ..Receipt::default()
        }
    }

    fn screenshot() -> ArtifactRef {
        ArtifactRef::new(
            "20260911-0612-a1/p1/screen.png",
            "a".repeat(64),
            "image/png",
            4_096,
        )
        .expect("a relative forward-slashed path")
    }

    #[test]
    fn the_receipt_is_the_last_line_of_the_text_rendering() {
        let output = Output::new(
            serde_json::json!({"status": "matched"}),
            "menu ready",
            receipt(),
        );
        assert_eq!(
            output.to_text(),
            "menu ready\nfidelity: cpu B | profile fast | deterministic"
        );
    }

    #[test]
    fn an_empty_text_renders_the_receipt_alone() {
        let output = Output::new(serde_json::json!({}), "", receipt());
        assert_eq!(
            output.to_text(),
            "fidelity: cpu B | profile fast | deterministic"
        );
    }

    #[test]
    fn the_json_response_carries_the_receipt_the_artifacts_and_the_virtual_time() {
        let output = Output::new(serde_json::json!({"status": "matched"}), "", receipt())
            .with_artifact(screenshot())
            .expect("a valid artifact path");
        let json = output.to_json();
        assert_eq!(json["status"], "matched");
        assert_eq!(json["vt_us"], 845_213);
        assert_eq!(json["receipt"]["journal_len"], 12);
        assert_eq!(
            json["artifacts"][0]["path"],
            "20260911-0612-a1/p1/screen.png"
        );
        assert_eq!(json["artifacts"][0]["bytes"], 4_096);
        assert_eq!(json["artifacts"][0]["media_type"], "image/png");
    }

    #[test]
    fn a_non_object_json_is_wrapped_rather_than_lost() {
        let output = Output::new(serde_json::json!([1, 2, 3]), "", receipt());
        assert_eq!(output.to_json()["result"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn an_artifact_path_is_relative_and_forward_slashed() {
        assert!(check_artifact_path("20260911-0612-a1/p1/screen.png").is_ok());
        assert!(check_artifact_path("serial.log").is_ok());
        assert_eq!(check_artifact_path(""), Err(PathError::Empty));
        assert_eq!(
            check_artifact_path("/var/lib/passportsim/p1/screen.png"),
            Err(PathError::Absolute)
        );
        assert_eq!(
            check_artifact_path("p1\\screen.png"),
            Err(PathError::Backslash)
        );
        assert_eq!(
            check_artifact_path("../../etc/passwd"),
            Err(PathError::Traversal)
        );
        assert_eq!(
            check_artifact_path("p1//screen.png"),
            Err(PathError::EmptySegment)
        );
        assert_eq!(
            check_artifact_path("p1/Screen.png"),
            Err(PathError::BadSegment("Screen.png".to_string()))
        );
        assert_eq!(
            check_artifact_path("p1/.hidden"),
            Err(PathError::BadSegment(".hidden".to_string()))
        );
    }

    #[test]
    fn a_native_root_is_refused_as_an_artifact_path_on_either_host() {
        assert_eq!(
            check_artifact_path("/var/lib/passportsim/artifacts/p1/screen.png"),
            Err(PathError::Absolute)
        );
        assert_eq!(
            check_artifact_path("d:/passportsim/artifacts/p1/screen.png"),
            Err(PathError::Absolute)
        );
        assert!(ArtifactRef::new("d:\\artifacts\\screen.png", "", "image/png", 1).is_err());
    }

    #[test]
    fn absolute_paths_are_recognized_in_a_sentence_and_a_home_form_is_not() {
        assert!(contains_absolute_path("artifacts at /var/lib/passportsim"));
        assert!(contains_absolute_path("artifacts at c:\\passportsim"));
        assert!(!contains_absolute_path("artifacts at ~/passportsim"));
        assert!(!contains_absolute_path("20260911-0612-a1/p1/screen.png"));
        assert!(!is_absolute_path("c:relative.txt"));
    }

    #[test]
    fn a_quoted_or_bracketed_path_is_caught_as_well_as_a_bare_one() {
        for text in [
            "wrote '/var/lib/passportsim/p1/screen.png'",
            "wrote \"/var/lib/passportsim/p1/screen.png\"",
            "wrote (/var/lib/passportsim/p1/screen.png)",
            "path=/var/lib/passportsim/p1/screen.png",
            "[/var/lib/passportsim]",
            "root=c:\\passportsim\\artifacts",
        ] {
            assert!(contains_absolute_path(text), "{text}");
        }
        for text in [
            "wrote '20260911-0612-a1/p1/screen.png'",
            "root='~/.local/share/passportsim'",
            "docs at https://example.invalid/passport/emu",
            "see ../notes",
        ] {
            assert!(!contains_absolute_path(text), "{text}");
        }
    }

    #[test]
    fn the_guard_sees_a_quoted_path_in_the_json_and_the_text() {
        let output = Output::new(
            serde_json::json!({"note": "wrote '/var/lib/passportsim/p1/screen.png'"}),
            "wrote '/var/lib/passportsim/p1/screen.png'",
            receipt(),
        );
        assert_eq!(
            output.absolute_paths().len(),
            2,
            "{:?}",
            output.absolute_paths()
        );
    }

    #[test]
    fn a_normal_output_carries_no_absolute_path() {
        let output = Output::new(
            serde_json::json!({"artifacts_dir": "20260911-0612-a1/p1"}),
            "wrote 20260911-0612-a1/p1/screen.png",
            receipt(),
        )
        .with_artifact(screenshot())
        .expect("a valid artifact path");
        assert!(output.absolute_paths().is_empty());
    }

    #[test]
    fn the_guard_sees_an_absolute_path_in_the_text_as_well_as_the_json() {
        let output = Output::new(
            serde_json::json!({"root": "/var/lib/passportsim"}),
            "wrote /var/lib/passportsim/p1/screen.png",
            receipt(),
        );
        assert_eq!(output.absolute_paths().len(), 2);
    }

    #[test]
    fn shaping_keeps_the_whole_rendering_inside_the_budget() {
        let body: Vec<String> = (0..400)
            .map(|i| format!("i ({i}) pk_app: line {i}"))
            .collect();
        let output = Output::new(serde_json::json!({}), body.join("\n"), receipt())
            .shaped(&ShapeLimits::DEFAULT);
        assert!(output.fits_text_budget());
        assert!(output.text_chars() <= TEXT_BUDGET_CHARS);
        assert!(output.to_text().contains("lines elided"));
        assert!(output.to_text().ends_with("| deterministic"));
    }
}
