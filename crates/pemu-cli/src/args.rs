//! From argv to one JSON argument object. Three sources, each overriding the one before:
//!
//! 1. the `--json` document, the only way to pass a nested object or array, because `cmd.exe` has
//!    no single quotes and Windows PowerShell 5.1 strips embedded double quotes. It is `--json
//!    @<file>` or `--json -` (stdin); an inline document is refused rather than half-working per
//!    shell;
//! 2. the positional arguments of `CommandSpec::cli.positional`, in their declared order;
//! 3. the flags, one per top-level scalar property, plus the `--no-<flag>` spelling where one
//!    exists (see [`crate::tree`]).
//!
//! A value is typed from the schema: `integer` and `number` become JSON numbers, a `boolean` is
//! `true` when its switch is given, a `string` stays a string (`--label 7` is `"7"`), and a union
//! (a `Duration` is "integer milliseconds or `1.5s`") is the narrowest scalar the text parses as.

use clap::ArgMatches;
use clap::parser::ValueSource;
use pemu_api::error::{ApiError, E_USAGE};
use pemu_api::spec::CommandSpec;

use crate::glob;
use crate::json::{Map, Value};
use crate::tree::{self, Form, JSON_ARG};

fn usage(detail: impl Into<String>) -> ApiError {
    ApiError::new(E_USAGE, detail.into())
}

pub trait JsonSource {
    /// For `--json -`. Errors with the reason standard input could not be read.
    fn stdin(&mut self) -> Result<String, String>;

    /// For `--json @<path>`. Errors with the reason the file could not be read.
    fn file(&mut self, path: &std::path::Path) -> Result<String, String>;
}

pub struct HostJson;

impl JsonSource for HostJson {
    fn stdin(&mut self) -> Result<String, String> {
        use std::io::Read as _;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        Ok(text)
    }

    fn file(&mut self, path: &std::path::Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

/// `None` when there is none. A `@<path>` may be a glob, because no Windows shell expands one for a
/// program; it must name exactly one file. Anything else, an unreadable file or a document that is
/// not a JSON object is `E_USAGE`.
pub fn read_document(
    spelling: Option<&str>,
    source: &mut dyn JsonSource,
) -> Result<Option<Map<String, Value>>, ApiError> {
    let Some(spelling) = spelling else {
        return Ok(None);
    };
    let text = match spelling {
        "-" => source
            .stdin()
            .map_err(|e| usage(format!("`--json -`: standard input could not be read: {e}")))?,
        other => {
            let Some(pattern) = other.strip_prefix('@') else {
                return Err(usage(format!(
                    "`--json {other}`: the documented forms are `--json @<file>` and `--json -`"
                ))
                .with_hint(
                    "an inline document does not survive `cmd.exe` or Windows PowerShell 5.1; write it to a file, or pipe it in with `-`",
                ));
            };
            let matched = glob::expand(pattern);
            let path = match matched.len() {
                1 => matched.into_iter().next().expect("exactly one"),
                n => {
                    return Err(usage(format!(
                        "`--json @{pattern}` names {n} files; one call takes one argument document"
                    )));
                }
            };
            source
                .file(&path)
                .map_err(|e| usage(format!("`--json @{pattern}`: {e}")))?
        }
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| usage(format!("`--json`: the document is not JSON: {e}")))?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(usage("`--json`: the document must be a JSON object")),
    }
}

/// `E_USAGE` for an unreadable `--json` document or a flag whose text does not fit its schema type.
pub fn assemble(
    spec: &CommandSpec,
    matches: &ArgMatches,
    source: &mut dyn JsonSource,
) -> Result<Value, ApiError> {
    let mut args = read_document(
        matches.get_one::<String>(JSON_ARG).map(String::as_str),
        source,
    )?
    .unwrap_or_default();
    for property in tree::properties(spec) {
        match property.form {
            // A nested value has no argv spelling; it came in through the document above.
            Form::Json => {}
            Form::Switch => {
                if matches.value_source(&property.name) == Some(ValueSource::CommandLine) {
                    args.insert(property.name.clone(), Value::Bool(true));
                } else if property.negated
                    && matches.value_source(&property.negated_name())
                        == Some(ValueSource::CommandLine)
                {
                    args.insert(property.name.clone(), Value::Bool(false));
                }
            }
            Form::Positional | Form::Value => {
                if let Some(text) = matches.get_one::<String>(&property.name) {
                    args.insert(
                        property.name.clone(),
                        scalar(text, property.kind.as_deref(), &property.name)?,
                    );
                }
            }
        }
    }
    Ok(Value::Object(args))
}

fn scalar(text: &str, kind: Option<&str>, name: &str) -> Result<Value, ApiError> {
    match kind {
        Some("integer") => text.parse::<i64>().map(Value::from).map_err(|_| {
            usage(format!(
                "`--{}`: `{text}` is not a whole number",
                dashed(name)
            ))
        }),
        Some("number") => serde_json::Number::from_f64(
            text.parse::<f64>()
                .map_err(|_| usage(format!("`--{}`: `{text}` is not a number", dashed(name))))?,
        )
        .map(Value::Number)
        .ok_or_else(|| {
            usage(format!(
                "`--{}`: `{text}` is not a finite number",
                dashed(name)
            ))
        }),
        Some("boolean") => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(usage(format!(
                "`--{}`: `{text}` is not `true` or `false`",
                dashed(name)
            ))),
        },
        // Whatever it looks like.
        Some("string") => Ok(Value::String(text.to_owned())),
        // So `--timeout 250` is 250 milliseconds and `--timeout 1.5s` is the string the schema's
        // other branch accepts.
        _ => Ok(match (text.parse::<i64>(), text.parse::<f64>()) {
            (Ok(whole), _) => Value::from(whole),
            (_, Ok(real)) => serde_json::Number::from_f64(real)
                .map_or_else(|| Value::String(text.to_owned()), Value::Number),
            _ => Value::String(text.to_owned()),
        }),
    }
}

fn dashed(name: &str) -> String {
    name.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};

    use pemu_api::registry;

    /// So no test writes a file.
    struct Memory {
        stdin: String,
        files: Vec<(PathBuf, String)>,
    }

    impl JsonSource for Memory {
        fn stdin(&mut self) -> Result<String, String> {
            Ok(self.stdin.clone())
        }

        fn file(&mut self, path: &Path) -> Result<String, String> {
            self.files
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, text)| text.clone())
                .ok_or_else(|| format!("{}: no such file", path.display()))
        }
    }

    fn memory() -> Memory {
        Memory {
            stdin: String::new(),
            files: Vec::new(),
        }
    }

    fn args_of(argv: &[&str], source: &mut dyn JsonSource) -> Result<Value, ApiError> {
        let matches = crate::tree::build()
            .try_get_matches_from(argv)
            .expect("the tree accepts the argv of this test");
        let (name, sub) = matches.subcommand().expect("a subcommand");
        let spec = registry::find_with_alias(name).expect("the subcommand is a command");
        assemble(spec, sub, source)
    }

    #[test]
    fn positionals_and_flags_become_the_argument_object() {
        let args = args_of(
            &[
                "passportsim",
                "start",
                "official",
                "--label",
                "smoke",
                "--seed",
                "7",
            ],
            &mut memory(),
        )
        .expect("every value fits its type");
        assert_eq!(args["fw"], "official");
        assert_eq!(args["label"], "smoke");
        assert_eq!(args["seed"], 7);
        assert!(args.get("power").is_none(), "an absent flag is absent");
    }

    #[test]
    fn a_switch_is_only_present_when_it_is_given() {
        let bare = args_of(&["passportsim", "stop"], &mut memory()).expect("no argument");
        assert!(bare.get("keep_artifacts").is_none());
        let given = args_of(&["passportsim", "stop", "--keep-artifacts"], &mut memory())
            .expect("the switch is given");
        assert_eq!(given["keep_artifacts"], true);
    }

    #[test]
    fn a_switch_that_defaults_to_true_is_reachable_from_argv_through_its_negated_spelling() {
        let dropped = args_of(
            &["passportsim", "stop", "--no-keep-artifacts"],
            &mut memory(),
        )
        .expect("the negated switch is given");
        assert_eq!(dropped["keep_artifacts"], false);

        let both = crate::tree::build().try_get_matches_from([
            "passportsim",
            "stop",
            "--keep-artifacts",
            "--no-keep-artifacts",
        ]);
        assert!(both.is_err(), "the two spellings contradict each other");
    }

    #[test]
    fn a_union_typed_value_keeps_the_narrowest_scalar_the_schema_accepts() {
        let args = args_of(
            &["passportsim", "run", "--for", "250", "--timeout", "1.5s"],
            &mut memory(),
        )
        .expect("both branches of Duration");
        assert_eq!(args["for"], 250);
        assert_eq!(args["timeout"], "1.5s");
    }

    #[test]
    fn a_string_property_never_becomes_a_number() {
        let args = args_of(
            &["passportsim", "start", "official", "--label", "7"],
            &mut memory(),
        )
        .expect("a label is a string");
        assert_eq!(args["label"], "7");
    }

    /// Reading it as untyped would turn `--label 7` into the number 7.
    #[test]
    fn a_nullable_string_property_is_still_a_string() {
        assert_eq!(
            scalar("7", Some("string"), "label").expect("a string stays a string"),
            Value::String("7".to_owned())
        );
        assert_eq!(
            scalar("7", None, "label").expect("an untyped property is read as a scalar"),
            Value::from(7i64),
            "which is why the nullable form must not reach this branch"
        );
    }

    #[test]
    fn a_value_outside_its_type_is_usage() {
        let error = args_of(
            &["passportsim", "start", "official", "--seed", "many"],
            &mut memory(),
        )
        .expect_err("`many` is no integer");
        assert_eq!(error.code, E_USAGE);
        assert!(error.message.contains("--seed"), "{}", error.message);
    }

    #[test]
    fn the_json_document_is_read_from_standard_input() {
        let mut source = Memory {
            stdin: r#"{"fw":"official","boot":"none"}"#.to_owned(),
            files: Vec::new(),
        };
        let args = args_of(&["passportsim", "start", "--json", "-"], &mut source)
            .expect("stdin carries the document");
        assert_eq!(args["fw"], "official");
        assert_eq!(args["boot"], "none");
    }

    #[test]
    fn a_flag_overrides_the_json_document() {
        let mut source = Memory {
            stdin: r#"{"fw":"official","label":"from-json"}"#.to_owned(),
            files: Vec::new(),
        };
        let args = args_of(
            &[
                "passportsim",
                "start",
                "--json",
                "-",
                "--label",
                "from-argv",
            ],
            &mut source,
        )
        .expect("both sources are read");
        assert_eq!(args["label"], "from-argv");
    }

    #[test]
    fn the_json_document_is_read_from_a_file() {
        let mut source = Memory {
            stdin: String::new(),
            files: vec![(
                PathBuf::from("args.json"),
                r#"{"fw":"official"}"#.to_owned(),
            )],
        };
        let args = args_of(
            &["passportsim", "start", "--json", "@args.json"],
            &mut source,
        )
        .expect("the file carries the document");
        assert_eq!(args["fw"], "official");
    }

    #[test]
    fn an_inline_document_is_refused_with_the_two_documented_forms() {
        let error = read_document(Some(r#"{"fw":"official"}"#), &mut memory())
            .expect_err("an inline document is not a documented form");
        assert_eq!(error.code, E_USAGE);
        assert!(
            error.message.contains("`--json @<file>`"),
            "{}",
            error.message
        );
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("PowerShell"),
            "{:?}",
            error.hint
        );
    }

    #[test]
    fn a_document_that_is_not_an_object_is_refused() {
        let mut source = Memory {
            stdin: "[1, 2]".to_owned(),
            files: Vec::new(),
        };
        let error = read_document(Some("-"), &mut source).expect_err("an array is no argument set");
        assert_eq!(error.code, E_USAGE);
        assert!(error.message.contains("JSON object"), "{}", error.message);
    }

    #[test]
    fn a_missing_json_file_is_usage_and_names_the_pattern() {
        let error = read_document(Some("@no-such-file.json"), &mut memory())
            .expect_err("the file is not there");
        assert_eq!(error.code, E_USAGE);
        assert!(
            error.message.contains("no-such-file.json"),
            "{}",
            error.message
        );
    }
}
