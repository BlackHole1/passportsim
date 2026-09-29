//! The command registry. Natively, `#[command]` items register into the link-time list `COMMANDS`
//! through `linkme`. On wasm32, `pemu-wasm`'s build script generates the list and `set_commands`
//! installs it.
//!
//! # `#[command]`
//!
//! The attribute goes on a command's handler and builds its `CommandSpec`, validating its
//! arguments at expansion:
//!
//! ```text
//! #[command(
//!     name = "run",                   // required, `[a-z][a-z0-9_]*`
//!     group = core,                   // required: core | audio | radio | nfc | debug | device | power
//!     input = RunArgs,                // optional, any `schemars::JsonSchema` type
//!     output = RunResult,             // optional
//!     annotations(read_only, idempotent, advances_time, needs_instance, native_only,
//!                 destructive, human_confirm),      // optional, any subset
//!     cli(positional = ["until"], aliases = ["r"]), // optional
//!     scenario_step = "wait",         // optional
//!     errors(E_TIMEOUT, E_WALL_BUDGET),             // optional, Core or own-group codes only
//!     example(title = "Run until the menu is ready", args = r#"{"timeout":"5s"}"#),
//!     api_crate = crate,              // only inside `pemu-api` itself; defaults to `::pemu_api`
//! )]
//! /// One line, the summary of the command.
//! pub fn run(cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> { .. }
//! ```
//!
//! The expansion keeps the function, adds `pub const SPEC_RUN: CommandSpec` next to it, and
//! natively adds a `linkme` element pointing at it. At least one `example` is required. The first
//! doc-comment line becomes the summary.
//!
//! ```
//! use pemu_api::error::{ApiError, E_TIMEOUT};
//! use pemu_api::output::Output;
//! use pemu_api::registry::{command, find};
//! use pemu_api::spec::HandlerCx;
//!
//! /// Advance virtual time until a matcher fires.
//! #[command(
//!     name = "doc_run",
//!     group = core,
//!     annotations(advances_time, needs_instance),
//!     errors(E_TIMEOUT),
//!     example(title = "Wait for the menu", args = r#"{"timeout":"5s"}"#),
//! )]
//! fn doc_run(cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
//!     Err(ApiError::new(E_TIMEOUT, "no match within the timeout").retryable())
//! }
//!
//! fn main() {
//!     let spec = find("doc_run").expect("the attribute registered the command");
//!     assert_eq!(spec.summary, "Advance virtual time until a matcher fires.");
//!     assert!(spec.annotations.advances_time && !spec.annotations.read_only);
//!     assert_eq!(SPEC_DOC_RUN.errors, &[E_TIMEOUT]);
//! }
//! ```
//!
//! An error code from another group's range fails the build (a `const` assertion of
//! `ErrorCode::allowed_in`). Here an Nfc command lists a Debug-range code:
//!
//! ```compile_fail
//! use pemu_api::error::{ApiError, ErrorCode};
//! use pemu_api::output::Output;
//! use pemu_api::registry::command;
//! use pemu_api::spec::HandlerCx;
//!
//! const E_DEBUG_ONLY: ErrorCode = ErrorCode { name: "E_DEBUG_ONLY", number: 4500 };
//!
//! /// Read a tag.
//! #[command(
//!     name = "doc_nfc",
//!     group = nfc,
//!     errors(E_DEBUG_ONLY),
//!     example(title = "Read", args = "{}"),
//! )]
//! fn doc_nfc(cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
//!     unimplemented!()
//! }
//!
//! fn main() {}
//! ```

use crate::spec::{CommandSpec, name_is_valid, scenario_step_is_valid};

/// `#[command]`; its grammar is in the module docs.
pub use pemu_macros::command;

/// Re-exported so `#[command]` expansions can name it without their own dependency.
#[cfg(not(target_arch = "wasm32"))]
pub use linkme;

#[cfg(not(target_arch = "wasm32"))]
#[linkme::distributed_slice]
pub static COMMANDS: [CommandSpec];

#[cfg(not(target_arch = "wasm32"))]
pub fn commands() -> &'static [CommandSpec] {
    &COMMANDS
}

#[cfg(target_arch = "wasm32")]
static WASM_COMMANDS: std::sync::OnceLock<&'static [CommandSpec]> = std::sync::OnceLock::new();

/// Installs the list `pemu-wasm`'s build script generates, since `linkme` does not work on wasm32.
/// Called once before the first `pemu_call`; a second call returns its argument in `Err`.
#[cfg(target_arch = "wasm32")]
pub fn set_commands(list: &'static [CommandSpec]) -> Result<(), &'static [CommandSpec]> {
    WASM_COMMANDS.set(list)
}

/// Empty until `set_commands` installs the generated list.
#[cfg(target_arch = "wasm32")]
pub fn commands() -> &'static [CommandSpec] {
    WASM_COMMANDS.get().copied().unwrap_or(&[])
}

pub fn find(name: &str) -> Option<&'static CommandSpec> {
    commands().iter().find(|c| c.name == name)
}

/// Also resolves `cli.aliases`.
pub fn find_with_alias(name: &str) -> Option<&'static CommandSpec> {
    find(name).or_else(|| commands().iter().find(|c| c.cli.aliases.contains(&name)))
}

pub fn find_scenario_step(key: &str) -> Option<&'static CommandSpec> {
    commands().iter().find(|c| c.scenario_step == Some(key))
}

/// The MCP tool list is the union of the enabled groups.
pub fn commands_in(group: crate::spec::CapsGroup) -> impl Iterator<Item = &'static CommandSpec> {
    commands().iter().filter(move |c| c.group == group)
}

/// Why a list of command specs is not a valid registry. `#[command]` checks each command at compile
/// time; `check` covers hand-written lists (wasm32, tests) and cross-command rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryProblem {
    BadName(&'static str),
    DuplicateName(&'static str),
    BadAlias(&'static str, &'static str),
    /// The summary is empty or spans more than one line.
    BadSummary(&'static str),
    BadScenarioStep(&'static str, &'static str),
    NoExample(&'static str),
    BadExample(&'static str, &'static str),
    UnknownPositional(&'static str, &'static str),
    /// A `cli_only` argument is not a top-level property of the input schema, so removing it from
    /// the agent surfaces would silently do nothing.
    UnknownCliOnly(&'static str, &'static str),
    BadAnnotations(&'static str, &'static str),
    BadErrorCode(&'static str, crate::error::RegistryError),
    ErrorRegistry(crate::error::RegistryError),
}

impl std::fmt::Display for RegistryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(n) => write!(f, "command name {n:?} is not `[a-z][a-z0-9_]*`"),
            Self::DuplicateName(n) => write!(f, "command name {n:?} is used twice"),
            Self::BadAlias(c, a) => write!(f, "command {c}: alias {a:?} is invalid or taken"),
            Self::BadSummary(c) => write!(f, "command {c}: summary is empty or multi-line"),
            Self::BadScenarioStep(c, s) => {
                write!(f, "command {c}: scenario step {s:?} is invalid or taken")
            }
            Self::NoExample(c) => write!(f, "command {c}: no example"),
            Self::BadExample(c, t) => {
                write!(f, "command {c}: example {t:?} args are not a JSON object")
            }
            Self::UnknownPositional(c, p) => {
                write!(f, "command {c}: positional {p:?} is no input property")
            }
            Self::UnknownCliOnly(c, p) => {
                write!(f, "command {c}: cli_only {p:?} is no input property")
            }
            Self::BadAnnotations(c, why) => write!(f, "command {c}: {why}"),
            Self::BadErrorCode(c, e) => write!(f, "command {c}: {e}"),
            Self::ErrorRegistry(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RegistryProblem {}

/// Checks names, aliases, summaries, examples, positionals, annotations and error-code ranges, then
/// runs `error::check_registry` over the list's codes plus the pre-registered ones. The docs
/// generators run it before they emit.
pub fn check(specs: &[CommandSpec]) -> Result<(), RegistryProblem> {
    let mut taken: Vec<&'static str> = Vec::new();
    let mut steps: Vec<&'static str> = Vec::new();
    for spec in specs {
        if !name_is_valid(spec.name) {
            return Err(RegistryProblem::BadName(spec.name));
        }
        if taken.contains(&spec.name) {
            return Err(RegistryProblem::DuplicateName(spec.name));
        }
        taken.push(spec.name);
        for alias in spec.cli.aliases {
            if !name_is_valid(alias) || taken.contains(alias) {
                return Err(RegistryProblem::BadAlias(spec.name, alias));
            }
            taken.push(alias);
        }
        if spec.summary.trim().is_empty() || spec.summary.contains('\n') {
            return Err(RegistryProblem::BadSummary(spec.name));
        }
        if let Some(step) = spec.scenario_step {
            if !scenario_step_is_valid(step) || steps.contains(&step) {
                return Err(RegistryProblem::BadScenarioStep(spec.name, step));
            }
            steps.push(step);
        }
        if spec.examples.is_empty() {
            return Err(RegistryProblem::NoExample(spec.name));
        }
        for example in spec.examples {
            match example.args_json() {
                Ok(serde_json::Value::Object(_)) => {}
                _ => return Err(RegistryProblem::BadExample(spec.name, example.title)),
            }
        }
        check_positionals(spec)?;
        if let Err(why) = spec.annotations.check(spec.group) {
            return Err(RegistryProblem::BadAnnotations(spec.name, why));
        }
        if let Err(e) = crate::error::check_group_codes(spec.group, spec.errors) {
            return Err(RegistryProblem::BadErrorCode(spec.name, e));
        }
    }
    let codes: Vec<_> = crate::error::PRE_REGISTERED
        .iter()
        .copied()
        .chain(specs.iter().flat_map(|s| s.errors.iter().copied()))
        .collect();
    crate::error::check_registry(&codes).map_err(RegistryProblem::ErrorRegistry)
}

/// A schema without a `properties` object (the `any_schema` default) constrains nothing and is
/// accepted.
fn check_positionals(spec: &CommandSpec) -> Result<(), RegistryProblem> {
    let schema = (spec.input_schema)();
    let Some(properties) = schema.as_object().and_then(|o| o.get("properties")) else {
        return Ok(());
    };
    let Some(properties) = properties.as_object() else {
        return Ok(());
    };
    for name in spec.cli.positional {
        if !properties.contains_key(*name) {
            return Err(RegistryProblem::UnknownPositional(spec.name, name));
        }
    }
    for name in spec.cli.cli_only {
        if !properties.contains_key(*name) {
            return Err(RegistryProblem::UnknownCliOnly(spec.name, name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ApiError, E_USAGE, ErrorCode};
    use crate::output::Output;
    use crate::spec::{Annotations, CapsGroup, CliShape, Example, HandlerCx, Schema, any_schema};

    pub const E_SAMPLE_ONLY: ErrorCode = ErrorCode {
        name: "E_SAMPLE_ONLY",
        number: 4999,
    };

    fn sample_input_schema() -> Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": { "what": { "type": "string" } }
        })
    }

    /// Registered by the registry unit test.
    #[command(
        api_crate = crate,
        name = "sample_only",
        group = debug,
        input_schema = sample_input_schema,
        annotations(read_only, idempotent, needs_instance),
        cli(positional = ["what"], aliases = ["smpl"]),
        scenario_step = "sample.only",
        errors(E_USAGE, E_SAMPLE_ONLY),
        example(title = "Read the sample", args = r#"{"what":"anything"}"#),
    )]
    fn sample_only(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        unreachable!("the registry test never runs the handler")
    }

    fn never(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        unreachable!("a synthetic spec is never run")
    }

    const fn synthetic(name: &'static str, group: CapsGroup) -> CommandSpec {
        CommandSpec {
            name,
            group,
            summary: "Synthetic.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations: Annotations {
                read_only: true,
                ..Annotations::EMPTY
            },
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "Synthetic",
                args: "{}",
            }],
            errors: &[E_USAGE],
            handler: never,
        }
    }

    // Registration only happens where `linkme` works; on wasm32 `commands()` stays empty until
    // `set_commands`.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_sample_command_registers_and_is_found_by_name() {
        let spec = find("sample_only").expect("#[command] registered the sample command");
        assert_eq!(spec.name, "sample_only");
        assert_eq!(spec.group, CapsGroup::Debug);
        assert_eq!(spec.summary, "Registered by the registry unit test.");
        assert!(spec.annotations.read_only && spec.annotations.idempotent);
        assert_eq!(spec.cli.positional, ["what"]);
        assert_eq!(spec.scenario_step, Some("sample.only"));
        assert_eq!(spec.examples.len(), 1);
        assert!(find("no_such_command").is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_sample_command_is_reachable_by_alias_step_and_group() {
        assert_eq!(find_with_alias("smpl").map(|c| c.name), Some("sample_only"));
        assert_eq!(
            find_scenario_step("sample.only").map(|c| c.name),
            Some("sample_only")
        );
        assert!(commands_in(CapsGroup::Debug).any(|c| c.name == "sample_only"));
        assert!(!commands_in(CapsGroup::Core).any(|c| c.name == "sample_only"));
    }

    #[test]
    fn the_sample_commands_spec_const_is_public() {
        // The name the generated wasm32 list uses.
        assert_eq!(SPEC_SAMPLE_ONLY.name, "sample_only");
    }

    #[test]
    fn the_linked_registry_passes_its_own_check() {
        assert_eq!(check(commands()), Ok(()));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn command_error_codes_join_the_registered_set() {
        let codes = crate::error::registered_error_codes();
        assert!(codes.contains(&E_SAMPLE_ONLY));
        assert_eq!(crate::error::check_registry(&codes), Ok(()));
        assert_eq!(ErrorCode::lookup("E_SAMPLE_ONLY"), Some(E_SAMPLE_ONLY));
    }
    #[test]
    fn check_accepts_a_synthetic_list() {
        let list = [
            synthetic("alpha", CapsGroup::Core),
            synthetic("beta", CapsGroup::Nfc),
        ];
        assert_eq!(check(&list), Ok(()));
    }

    #[test]
    fn check_rejects_a_duplicate_command_name() {
        let list = [
            synthetic("alpha", CapsGroup::Core),
            synthetic("alpha", CapsGroup::Nfc),
        ];
        assert_eq!(check(&list), Err(RegistryProblem::DuplicateName("alpha")));
    }

    #[test]
    fn check_rejects_a_bad_name_and_a_taken_alias() {
        let mut bad = synthetic("Alpha", CapsGroup::Core);
        assert_eq!(check(&[bad]), Err(RegistryProblem::BadName("Alpha")));
        bad = synthetic("gamma", CapsGroup::Core);
        bad.cli = CliShape {
            positional: &[],
            aliases: &["alpha"],
            cli_only: &[],
        };
        let list = [synthetic("alpha", CapsGroup::Core), bad];
        assert_eq!(
            check(&list),
            Err(RegistryProblem::BadAlias("gamma", "alpha"))
        );
    }

    #[test]
    fn check_rejects_an_error_code_outside_the_group_range() {
        let mut spec = synthetic("alpha", CapsGroup::Nfc);
        spec.errors = &[E_SAMPLE_ONLY];
        assert_eq!(
            check(&[spec]),
            Err(RegistryProblem::BadErrorCode(
                "alpha",
                crate::error::RegistryError::OutsideGroup(E_SAMPLE_ONLY, CapsGroup::Nfc)
            ))
        );
    }

    #[test]
    fn check_rejects_a_number_outside_every_range() {
        const E_NOWHERE: ErrorCode = ErrorCode {
            name: "E_NOWHERE",
            number: 7000,
        };
        let mut spec = synthetic("alpha", CapsGroup::Core);
        spec.errors = &[E_NOWHERE];
        assert_eq!(
            check(&[spec]),
            Err(RegistryProblem::BadErrorCode(
                "alpha",
                crate::error::RegistryError::OutOfRange(E_NOWHERE)
            ))
        );
    }

    #[test]
    fn check_rejects_a_code_number_registered_under_two_names() {
        const E_CLASH: ErrorCode = ErrorCode {
            name: "E_CLASH",
            number: E_USAGE.number,
        };
        let mut spec = synthetic("alpha", CapsGroup::Core);
        spec.errors = &[E_CLASH];
        assert_eq!(
            check(&[spec]),
            Err(RegistryProblem::ErrorRegistry(
                crate::error::RegistryError::DuplicateNumber(E_USAGE, E_CLASH)
            ))
        );
    }

    #[test]
    fn check_rejects_a_missing_or_non_object_example() {
        let mut spec = synthetic("alpha", CapsGroup::Core);
        spec.examples = &[];
        assert_eq!(check(&[spec]), Err(RegistryProblem::NoExample("alpha")));
        spec.examples = &[Example {
            title: "Not an object",
            args: "[1]",
        }];
        assert_eq!(
            check(&[spec]),
            Err(RegistryProblem::BadExample("alpha", "Not an object"))
        );
    }

    #[test]
    fn check_rejects_a_positional_that_is_no_input_property() {
        let mut spec = synthetic("alpha", CapsGroup::Core);
        spec.input_schema = sample_input_schema;
        spec.cli = CliShape {
            positional: &["nope"],
            aliases: &[],
            cli_only: &[],
        };
        assert_eq!(
            check(&[spec]),
            Err(RegistryProblem::UnknownPositional("alpha", "nope"))
        );
    }

    #[test]
    fn check_rejects_contradicting_annotations_and_a_duplicate_step() {
        let mut spec = synthetic("alpha", CapsGroup::Core);
        spec.annotations = Annotations {
            read_only: true,
            destructive: true,
            ..Annotations::EMPTY
        };
        assert!(matches!(
            check(&[spec]),
            Err(RegistryProblem::BadAnnotations("alpha", _))
        ));
        let mut one = synthetic("alpha", CapsGroup::Core);
        let mut two = synthetic("beta", CapsGroup::Core);
        one.scenario_step = Some("wait");
        two.scenario_step = Some("wait");
        assert_eq!(
            check(&[one, two]),
            Err(RegistryProblem::BadScenarioStep("beta", "wait"))
        );
    }

    #[test]
    fn check_rejects_an_empty_or_multiline_summary() {
        let mut spec = synthetic("alpha", CapsGroup::Core);
        spec.summary = "two\nlines";
        assert_eq!(check(&[spec]), Err(RegistryProblem::BadSummary("alpha")));
        spec.summary = "  ";
        assert_eq!(check(&[spec]), Err(RegistryProblem::BadSummary("alpha")));
    }
}
