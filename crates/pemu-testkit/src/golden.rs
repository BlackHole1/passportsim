//! The golden prefix runner: compares a run's console against the prefix a milestone claims, and
//! reports the first differing line with context. The comparison is `pemu-verify`'s; this adds
//! finding the golden and a readable failure.
//!
//! A committed golden lives at `tests/golden/<image>/<name>` and is read as bytes. A derived
//! golden carries a device fingerprint, so it lives under the data root and a test skips when it
//! is absent.

use std::fmt;
use std::path::{Path, PathBuf};

use pemu_verify::goldens::check_prefix;
use pemu_verify::normalize::normalize;

use crate::corpus::{CorpusError, RootError, data_root_from_env};

/// Re-exported so a milestone test, whose only dependency is this crate, can name them.
pub use pemu_verify::goldens::{Golden, Header, Kind, TextMismatch};
pub use pemu_verify::normalize::{BootSelect, Console};

/// Committed goldens below the repository root.
pub const COMMITTED_DIR: &str = "tests/golden";
/// Derived goldens below the data root; not committed, because their headers carry device
/// fingerprints.
pub const DERIVED_DIR: &str = "goldens";
/// Lines of context printed either side of the first difference.
pub const CONTEXT_LINES: usize = 3;

/// The repository root, from this crate's manifest directory at compile time: a build-time
/// constant of the workspace layout, not a host directory role.
pub fn repo_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir.pop();
    dir
}

/// Why a golden could not be loaded. No variant prints a host path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoldenError {
    NotFound {
        name: String,
        /// Whether it was looked for in the tree or below the data root.
        committed: bool,
    },
    NoDataRoot(RootError),
    Unreadable {
        name: String,
        kind: String,
    },
    /// Bad header or non-UTF-8 body.
    Invalid {
        name: String,
        detail: String,
    },
}

impl fmt::Display for GoldenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GoldenError::NotFound {
                name,
                committed: true,
            } => {
                write!(f, "golden `{name}` is not committed under {COMMITTED_DIR}/")
            }
            GoldenError::NotFound {
                name,
                committed: false,
            } => write!(
                f,
                "golden `{name}` is not below the data root in {DERIVED_DIR}/"
            ),
            GoldenError::NoDataRoot(err) => write!(f, "{err}"),
            GoldenError::Unreadable { name, kind } => {
                write!(f, "golden `{name}` cannot be read ({kind})")
            }
            GoldenError::Invalid { name, detail } => {
                write!(f, "golden `{name}` is invalid: {detail}")
            }
        }
    }
}

impl From<CorpusError> for GoldenError {
    fn from(err: CorpusError) -> GoldenError {
        match err {
            CorpusError::Root(root) => GoldenError::NoDataRoot(root),
            other => GoldenError::Invalid {
                name: String::new(),
                detail: other.to_string(),
            },
        }
    }
}

/// Reads a committed golden, `name` relative to `tests/golden/`.
pub fn committed(name: &str) -> Result<Golden, GoldenError> {
    read_golden(&repo_root().join(COMMITTED_DIR).join(name), name, true)
}

/// Reads a golden derived on this host, `name` relative to `<data root>/goldens/`.
pub fn derived(name: &str) -> Result<Golden, GoldenError> {
    let root = data_root_from_env().map_err(GoldenError::NoDataRoot)?;
    read_golden(&root.join(DERIVED_DIR).join(name), name, false)
}

pub fn at_path(path: &Path, name: &str) -> Result<Golden, GoldenError> {
    read_golden(path, name, true)
}

fn read_golden(path: &Path, name: &str, committed: bool) -> Result<Golden, GoldenError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(GoldenError::NotFound {
                name: name.to_string(),
                committed,
            });
        }
        Err(err) => {
            return Err(GoldenError::Unreadable {
                name: name.to_string(),
                kind: format!("{:?}", err.kind()),
            });
        }
    };
    Golden::parse(&bytes).map_err(|err| GoldenError::Invalid {
        name: name.to_string(),
        detail: err.to_string(),
    })
}

/// A prefix comparison that did not hold, with a rendered report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixFailure {
    pub golden: String,
    pub mismatch: TextMismatch,
    /// The first differing line with [`CONTEXT_LINES`] around it, ready to print.
    pub report: String,
}

impl fmt::Display for PrefixFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "golden `{}`: {}", self.golden, self.report)
    }
}

/// Compares a produced console against the prefix a milestone claims. `claimed` is a line count,
/// `None` for the whole golden; success returns the number of lines compared.
pub fn compare_prefix(
    name: &str,
    golden: &Golden,
    produced: &Console,
    claimed: Option<usize>,
) -> Result<usize, PrefixFailure> {
    check_prefix(golden, produced, claimed).map_err(|mismatch| PrefixFailure {
        golden: name.to_string(),
        report: render(golden, produced, &mismatch),
        mismatch,
    })
}

/// [`compare_prefix`] from raw console bytes, normalized first.
pub fn compare_prefix_bytes(
    name: &str,
    golden: &Golden,
    produced: &[u8],
    select: BootSelect,
    claimed: Option<usize>,
) -> Result<usize, PrefixFailure> {
    compare_prefix(name, golden, &normalize(produced, select), claimed)
}

#[track_caller]
pub fn assert_prefix_bytes(
    name: &str,
    golden: &Golden,
    produced: &[u8],
    select: BootSelect,
    claimed: Option<usize>,
) -> usize {
    match compare_prefix_bytes(name, golden, produced, select, claimed) {
        Ok(lines) => lines,
        Err(failure) => panic!("{failure}"),
    }
}

/// Renders the first differing line with its neighbours, 1-based; `-` is the golden and `+` the
/// run.
fn render(golden: &Golden, produced: &Console, mismatch: &TextMismatch) -> String {
    let expected = golden.lines();
    let actual: Vec<&str> = produced
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    match mismatch {
        TextMismatch::OverClaimed { claimed, golden } => format!(
            "the test claims {claimed} lines and the golden holds {golden}; \
             the claim is wrong, not the run"
        ),
        TextMismatch::TooShort { claimed, produced } => {
            let mut out = format!(
                "the run produced {produced} lines and the milestone claims {claimed}; \
                 the first missing line is {}:",
                produced + 1
            );
            out.push_str(&context(&expected, &actual, *produced));
            out
        }
        TextMismatch::Line { at, .. } => {
            let mut out = format!("line {} differs:", at + 1);
            out.push_str(&context(&expected, &actual, *at));
            out
        }
    }
}

fn context(expected: &[&str], actual: &[&str], at: usize) -> String {
    let first = at.saturating_sub(CONTEXT_LINES);
    let last = (at + CONTEXT_LINES + 1).min(expected.len().max(actual.len()));
    let mut out = String::new();
    for n in first..last {
        let golden_line = expected.get(n).copied();
        let run_line = actual.get(n).copied();
        if n == at || golden_line != run_line {
            match golden_line {
                Some(line) => out.push_str(&format!("\n  {:>4} - {line}", n + 1)),
                None => out.push_str(&format!("\n  {:>4} - <end of golden>", n + 1)),
            }
            match run_line {
                Some(line) => out.push_str(&format!("\n  {:>4} + {line}", n + 1)),
                None => out.push_str(&format!("\n  {:>4} + <end of run>", n + 1)),
            }
        } else if let Some(line) = golden_line {
            out.push_str(&format!("\n  {:>4}   {line}", n + 1));
        }
    }
    out
}
