//! Tests of the generated surfaces. Every test runs against the real registry unless it names a
//! synthetic one.

use std::fs;
use std::path::{Path, PathBuf};

use pemu_api::error::{ApiError, E_USAGE};
use pemu_api::output::Output as CommandOutput;
use pemu_api::spec::{
    Annotations, CapsGroup, CliShape, CommandSpec, Example, HandlerCx, any_schema,
};
use serde_json::{Value, json};

use super::validate::{self, Problem};
use super::{Kind, generate, model, plan, render, replace_file, tsgen};

/// A directory of this run's own, under the system temporary directory.
fn temp_root(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut dir = std::env::temp_dir();
    dir.push(format!("pemu-docs-{tag}-{}-{unique}", std::process::id()));
    fs::create_dir_all(&dir).expect("temporary directory");
    dir
}

fn remove(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

/// The relative path of the first committed Markdown page, or `None` on an empty registry.
fn first_committed_page() -> Option<PathBuf> {
    let commands = model::commands().expect("registry");
    plan(&commands)
        .expect("plan")
        .into_iter()
        .find(|output| output.kind == Kind::Committed && output.path.starts_with("docs/commands"))
        .map(|output| output.path)
}

// --- byte stability and the write/check cycle -----------------------------------------------

#[test]
fn generated_text_is_byte_stable_across_two_runs() {
    let commands = model::commands().expect("registry");
    let first = plan(&commands).expect("first run");
    let second = plan(&commands).expect("second run");
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.path, b.path);
        assert_eq!(
            a.contents,
            b.contents,
            "{} is not byte-stable",
            a.path.display()
        );
    }
}

#[test]
fn every_generated_file_is_lf_only_and_ends_with_a_newline() {
    let commands = model::commands().expect("registry");
    for output in plan(&commands).expect("plan") {
        assert!(
            !output.contents.contains('\r'),
            "{} holds a CR byte",
            output.path.display()
        );
        assert!(
            output.contents.ends_with('\n'),
            "{} has no final newline",
            output.path.display()
        );
    }
}

#[test]
fn check_accepts_the_tree_the_generator_just_wrote() {
    let root = temp_root("fresh");
    generate(&root, false).expect("write");
    generate(&root, true).expect("check after write");
    remove(&root);
}

/// A reader of a generated file never sees it missing or truncated while the generator replaces it.
///
/// `xtask package` copies `docs/errors.md` into a package while another xtask command, or another
/// thread of the test binary, may be regenerating it; a truncating write would expose an empty or
/// half-written file.
#[test]
fn a_reader_never_sees_a_generated_file_missing_or_truncated_while_it_is_rewritten() {
    let root = temp_root("atomic");
    generate(&root, false).expect("write");
    let path = root.join("docs/errors.md");
    let fresh = fs::read(&path).expect("read the aggregate");
    let stale = b"stale\n".to_vec();
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut reads = 0usize;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let bytes = fs::read(&path).expect("the file is never missing");
                assert!(
                    bytes == fresh || bytes == stale,
                    "read {} bytes that are neither version",
                    bytes.len()
                );
                reads += 1;
            }
            reads
        });
        for _ in 0..100 {
            replace_file(&path, &stale).expect("make it stale");
            generate(&root, false).expect("regenerate");
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().expect("reader") > 0);
    });
    let leftovers: Vec<_> = fs::read_dir(root.join("docs"))
        .expect("docs dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    generate(&root, true).expect("check after the rewrites");
    remove(&root);
}

#[test]
fn check_fails_on_a_hand_edited_file() {
    let root = temp_root("edited");
    generate(&root, false).expect("write");
    let Some(page) = first_committed_page() else {
        remove(&root);
        return;
    };
    let path = root.join(&page);
    let mut text = fs::read_to_string(&path).expect("read the page");
    text.push_str("\nhand-written paragraph\n");
    fs::write(&path, text).expect("edit the page");

    let error = generate(&root, true).expect_err("a hand edit must fail --check");
    assert!(error.contains("out of date"), "{error}");
    assert!(error.contains(&page.display().to_string()), "{error}");
    remove(&root);
}

#[test]
fn check_reports_an_injected_cr_byte_as_crlf_found_with_the_fix_hint() {
    let root = temp_root("crlf");
    generate(&root, false).expect("write");
    let Some(page) = first_committed_page() else {
        remove(&root);
        return;
    };
    let path = root.join(&page);
    let text = fs::read_to_string(&path).expect("read the page");
    // Exactly what a Windows checkout with `core.autocrlf=true` produces.
    fs::write(&path, text.replace('\n', "\r\n")).expect("inject CR bytes");

    let error = generate(&root, true).expect_err("a CR byte must fail --check");
    assert!(
        error.starts_with("CRLF found"),
        "CRLF is its own failure class: {error}"
    );
    assert!(error.contains(&page.display().to_string()), "{error}");
    assert!(error.contains("eol=lf"), "the fix hint is missing: {error}");
    assert!(
        !error.contains("out of date"),
        "a CR byte must not be reported as a content diff: {error}"
    );
    remove(&root);
}

#[test]
fn check_reports_a_missing_committed_file_but_not_a_missing_aggregate() {
    let root = temp_root("missing");
    generate(&root, false).expect("write");
    let commands = model::commands().expect("registry");
    for output in plan(&commands).expect("plan") {
        if output.kind == Kind::Aggregate {
            fs::remove_file(root.join(&output.path)).expect("remove the aggregate");
        }
    }
    generate(&root, true).expect("a fresh checkout has no aggregate build output");

    let Some(page) = first_committed_page() else {
        remove(&root);
        return;
    };
    fs::remove_file(root.join(&page)).expect("remove the page");
    let error = generate(&root, true).expect_err("a missing committed file must fail --check");
    assert!(error.contains("(missing)"), "{error}");
    remove(&root);
}

/// The three aggregates are gitignored build outputs, so a merge cannot update them and failing
/// `--check` on one would ask a commit to fix a file no commit tracks. A committed file in the
/// same state must still fail: that is the half of the check that has teeth.
#[test]
fn check_reports_a_stale_aggregate_but_still_fails_a_stale_committed_file() {
    let root = temp_root("stale-aggregate");
    generate(&root, false).expect("write");
    let commands = model::commands().expect("registry");
    let outputs = plan(&commands).expect("plan");

    let aggregates: Vec<_> = outputs
        .iter()
        .filter(|output| output.kind == Kind::Aggregate)
        .collect();
    assert!(!aggregates.is_empty(), "the registry produces aggregates");
    for output in &aggregates {
        fs::write(root.join(&output.path), "stale build output\n").expect("age the aggregate");
    }
    generate(&root, true).expect("a stale aggregate is reported, not failed");

    let Some(page) = first_committed_page() else {
        remove(&root);
        return;
    };
    fs::write(root.join(&page), "stale committed page\n").expect("age the page");
    let error = generate(&root, true).expect_err("a stale committed file must still fail");
    assert!(error.contains("out of date"), "{error}");
    assert!(error.contains(&page.display().to_string()), "{error}");
    for output in &aggregates {
        assert!(
            !error.contains(&output.path.display().to_string()),
            "an aggregate must not be named in the failure: {error}"
        );
    }
    remove(&root);
}

#[test]
fn a_generated_file_no_command_produces_is_reported() {
    let root = temp_root("stale");
    generate(&root, false).expect("write");
    let stale = root.join("docs/commands/zz_removed_command.md");
    fs::write(&stale, "<!-- left over -->\n").expect("write the leftover");
    let error = generate(&root, true).expect_err("a leftover page must fail");
    assert!(error.contains("zz_removed_command.md"), "{error}");
    remove(&root);
}

// --- the example validator ------------------------------------------------------------------

#[test]
fn every_registered_example_validates_against_its_own_input_schema() {
    let commands = model::commands().expect("registry");
    super::check_examples(&commands).expect("every example validates");
}

#[test]
fn the_validator_rejects_a_mutated_example() {
    let commands = model::commands().expect("registry");
    let mut checked = 0;
    let mut mutable = 0;
    for command in &commands {
        let schema = (command.spec.input_schema)().to_value();
        // The schema's own first required property, never the example's alphabetically first key:
        // that one is required only by accident, and dropping an optional property is a mutation
        // the validator is right to accept.
        let Some(required) = schema
            .get("required")
            .and_then(Value::as_array)
            .and_then(|list| list.first())
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        mutable += command.spec.examples.len();
        for example in command.spec.examples {
            let mut args: Value = serde_json::from_str(example.args).expect("example JSON");
            let fields = args.as_object_mut().expect("examples are JSON objects");
            assert!(
                fields.remove(&required).is_some(),
                "{}: example {:?} does not carry its own required property {required:?}",
                command.spec.name,
                example.title
            );
            let problems = validate::validate(&schema, &args)
                .expect_err("dropping a required property must fail validation");
            assert!(
                problems.iter().any(|p| p.why.contains("missing required")),
                "{problems:?}"
            );
            checked += 1;
        }
    }
    // A command with a required property must have had the mutation above run.
    assert_eq!(
        checked, mutable,
        "every example of a command with a required property must have been mutated"
    );

    let schema = json!({
        "type": "object",
        "properties": { "timeout": { "type": "string" }, "count": { "type": "integer" } },
        "required": ["timeout"],
        "additionalProperties": false
    });
    validate::validate(&schema, &json!({"timeout": "5s", "count": 3})).expect("the sound example");
    for bad in [
        json!({"count": 3}),
        json!({"timeout": 5}),
        json!({"timeout": "5s", "extra": true}),
        json!([]),
    ] {
        validate::validate(&schema, &bad).expect_err("a mutated example must be rejected");
    }
}

#[test]
fn the_validator_covers_the_schemars_subset() {
    let schema = json!({
        "$defs": {
            "Mode": { "enum": ["deterministic", "realtime"] },
            "Port": { "type": "integer", "minimum": 1, "maximum": 65535 }
        },
        "type": "object",
        "properties": {
            "mode": { "$ref": "#/$defs/Mode" },
            "port": { "$ref": "#/$defs/Port" },
            "kind": { "const": "start" },
            "tags": { "type": "array", "items": { "type": "string", "maxLength": 4 } },
            "either": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            "exactly": { "oneOf": [{ "type": "integer" }, { "type": "boolean" }] }
        },
        "required": ["mode"]
    });
    validate::validate(
        &schema,
        &json!({"mode": "realtime", "port": 8765, "kind": "start", "tags": ["ui"],
                "either": null, "exactly": 7}),
    )
    .expect("a conforming instance");

    let cases: [(Value, &str); 7] = [
        (json!({"mode": "fast"}), "`enum`"),
        (json!({"mode": "realtime", "port": 0}), "`minimum`"),
        (json!({"mode": "realtime", "port": 70000}), "`maximum`"),
        (json!({"mode": "realtime", "kind": "stop"}), "`const`"),
        (
            json!({"mode": "realtime", "tags": ["toolong"]}),
            "`maxLength`",
        ),
        (json!({"mode": "realtime", "either": 3}), "`anyOf`"),
        (json!({"port": 1}), "missing required"),
    ];
    for (instance, expected) in cases {
        let problems = validate::validate(&schema, &instance)
            .expect_err(&format!("{instance} must be rejected"));
        assert!(
            problems.iter().any(|p| p.why.contains(expected)),
            "{instance} should have failed on {expected}, got {problems:?}"
        );
    }
}

#[test]
fn the_validator_points_at_the_failing_value() {
    let schema = json!({
        "type": "object",
        "properties": {
            "report": {
                "type": "object",
                "properties": { "corpus": { "type": "array", "items": { "type": "string" } } }
            }
        }
    });
    let problems = validate::validate(&schema, &json!({"report": {"corpus": ["ok", 3]}}))
        .expect_err("the second item is not a string");
    assert_eq!(
        problems,
        vec![Problem {
            at: "/report/corpus/1".to_string(),
            why: "expected type \"string\", found integer".to_string(),
        }]
    );
}

#[test]
fn unchecked_keywords_are_reported_rather_than_silently_skipped() {
    let schema = json!({
        "type": "object",
        "properties": { "id": { "type": "string", "pattern": "^[a-z]+$" } }
    });
    // `pattern` is the UNVERIFIED row of the validator: it is not enforced, but it is visible.
    validate::validate(&schema, &json!({"id": "NOT LOWERCASE"})).expect("pattern is not checked");
    let found = validate::unchecked_keywords(&schema);
    assert!(found.contains("pattern"), "{found:?}");
    assert!(validate::unchecked_keywords(&json!({"type": "string"})).is_empty());
}

#[test]
fn a_property_named_like_an_unchecked_keyword_is_not_reported_as_one() {
    // `screenshot` takes a `format` argument (a PNG of `raw`, `glass` or `perceived`): the key is
    // a property name, nothing in the schema is unvalidated, and a warning here would be a false
    // positive that trains readers to ignore the real ones.
    for schema in [
        json!({"type": "object", "properties": {"format": {"type": "string"}}}),
        json!({"type": "object", "properties": {"pattern": {"type": "string"}}}),
        json!({"$defs": {"propertyNames": {"type": "string"}},
               "type": "object", "properties": {"x": {"$ref": "#/$defs/propertyNames"}}}),
        // Not a schema position either: a `default` or an `enum` value that happens to be an
        // object with such a key.
        json!({"type": "object", "default": {"format": "png"}}),
        json!({"enum": [{"pattern": "x"}]}),
    ] {
        assert!(
            validate::unchecked_keywords(&schema).is_empty(),
            "a property name is not a keyword: {schema}"
        );
    }
    // The same keywords in schema position are still reported, however deep they sit.
    for (schema, expected) in [
        (
            json!({"type": "object", "properties": {"id": {"type": "string", "format": "uuid"}}}),
            "format",
        ),
        (
            json!({"type": "array", "items": {"type": "string", "pattern": "^a$"}}),
            "pattern",
        ),
        (
            json!({"anyOf": [{"type": "string"}, {"type": "object", "propertyNames": true}]}),
            "propertyNames",
        ),
        (
            json!({"$defs": {"Id": {"type": "string", "pattern": "^a$"}},
                   "$ref": "#/$defs/Id"}),
            "pattern",
        ),
    ] {
        let found = validate::unchecked_keywords(&schema);
        assert!(found.contains(expected), "{schema}: {found:?}");
    }
}

// --- the MCP tool list and its budget ---------------------------------------------------------

/// A handler that is never called by a test.
fn never(_cx: &mut HandlerCx, _args: Value) -> Result<CommandOutput, ApiError> {
    unreachable!("a synthetic command is never run")
}

/// A synthetic registered command, leaked so it has the `'static` lifetime a spec needs.
fn synthetic(name: &'static str, summary: &'static str) -> model::Command {
    let spec: &'static CommandSpec = Box::leak(Box::new(CommandSpec {
        name,
        group: CapsGroup::Core,
        summary,
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
    }));
    model::Command {
        spec,
        hosts: model::hosts_of(spec),
    }
}

#[test]
fn an_mcp_tool_carries_the_four_hints_and_the_full_host_matrix() {
    let commands = model::commands().expect("registry");
    for command in &commands {
        let tool = super::mcp::tool(command);
        assert_eq!(tool["name"], json!(command.mcp_tool()));
        let annotations = &tool["annotations"];
        for hint in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(annotations[hint].is_boolean(), "{hint} is missing");
        }
        assert_eq!(
            annotations["openWorldHint"],
            json!(command.spec.group == CapsGroup::Device),
            "openWorldHint is the Device group"
        );
        let hosts = annotations["x-passport-hosts"]
            .as_array()
            .expect("the host matrix column");
        let expected = model::HOSTS
            .iter()
            .filter(|host| command.runs_on(**host))
            .count();
        assert_eq!(hosts.len(), expected);
    }
}

#[test]
fn an_mcp_tool_inlines_its_defs_so_the_measured_bytes_are_the_served_bytes() {
    // The `$defs` are written once in the registry and inlined per tool at `tools/list` time (only
    // the definitions a tool uses), and recursive definitions are inlined to a fixed depth of 3.
    // Measuring the `$ref` form would under-count what a client receives.
    fn recursive_schema() -> pemu_api::spec::Schema {
        pemu_api::spec::schemars::json_schema!({
            "$defs": {
                "Matcher": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "within": { "$ref": "#/$defs/Matcher" }
                    }
                }
            },
            "type": "object",
            "properties": { "match": { "$ref": "#/$defs/Matcher" } },
            "required": ["match"]
        })
    }
    let command = with_input("defs_probe", recursive_schema, &[]);
    let tool = super::mcp::tool(&command);
    let text = serde_json::to_string(&tool).expect("serialize");
    assert!(
        !text.contains("$ref"),
        "a served schema carries no `$ref`: {text}"
    );
    assert!(
        !text.contains("$defs"),
        "the registry map is not served: {text}"
    );

    // The definition is substituted three times along the recursive path, and the fourth is the
    // schema that accepts anything, so the inlining terminates.
    let mut node = &tool["inputSchema"]["properties"]["match"];
    for _ in 0..3 {
        assert!(node["properties"]["text"].is_object(), "{tool}");
        node = &node["properties"]["within"];
    }
    assert_eq!(node, &json!(true));

    // What `mcp-size` measures is that larger payload, not the schemars shape.
    let raw = serde_json::to_string(&(command.spec.input_schema)().to_value()).expect("serialize");
    assert!(
        super::tool_bytes(&command) > raw.len(),
        "the served tool is larger than the `$ref` form it is built from"
    );
}

#[test]
fn mcp_size_reports_a_number_for_the_registered_core_list() {
    let commands = model::commands().expect("registry");
    let core: Vec<model::Command> = commands
        .iter()
        .copied()
        .filter(|command| command.spec.group == CapsGroup::Core)
        .collect();
    let report = crate::mcp_size::measure(&core, super::CORE_BUDGET_BYTES);
    assert_eq!(report.tools, core.len());
    assert_eq!(report.budget, super::CORE_BUDGET_BYTES);
    assert_eq!(report.bytes, super::list_bytes(&core));
    assert!(
        report.bytes > 0,
        "even an empty list serializes to `{{...}}`"
    );
    assert!(
        report.within_budget(),
        "the registered core list is over budget: {}",
        report.line()
    );
    assert!(report.line().contains(&report.bytes.to_string()));
    report.into_result().expect("within budget");
}

/// `power` and `usb` are the opt-in `power` group, so `mcp-size --caps power` measures them on top
/// of the core list while the default (core) list leaves them out.
#[test]
fn mcp_size_caps_power_adds_the_power_group_to_the_core_list() {
    let groups = crate::mcp_size::parse_caps("power").expect("a caps group");
    assert_eq!(
        groups,
        std::collections::BTreeSet::from([CapsGroup::Core, CapsGroup::Power])
    );
    assert!(crate::mcp_size::parse_caps("power,battery").is_err());
    let commands = model::commands().expect("registry");
    let enabled: Vec<model::Command> = commands
        .iter()
        .copied()
        .filter(|command| groups.contains(&command.spec.group))
        .collect();
    let power: Vec<&str> = enabled
        .iter()
        .filter(|command| command.spec.group == CapsGroup::Power)
        .map(|command| command.spec.name)
        .collect();
    assert_eq!(power, ["power", "usb"]);
    let core: Vec<model::Command> = enabled
        .iter()
        .copied()
        .filter(|command| command.spec.group == CapsGroup::Core)
        .collect();
    let core_bytes = super::list_bytes(&core);
    let line = crate::mcp_size::caps_line(&groups, &enabled, core_bytes);
    assert!(line.contains("--caps core,power"), "{line}");
    assert!(
        line.contains(&format!("{} tool(s)", core.len() + 2)),
        "{line}"
    );
    assert!(super::list_bytes(&enabled) > core_bytes);
}

#[test]
fn mcp_size_fails_a_synthetic_over_budget_list_and_names_the_offender() {
    let small = synthetic("small_tool", "A short summary.");
    // One line, no CR: a summary that alone blows a small budget.
    let long: &'static str = Box::leak("Very long summary. ".repeat(200).into_boxed_str());
    let big = synthetic("big_tool", long);

    let list = [small, big];
    let report = crate::mcp_size::measure(&list, 1024);
    assert!(!report.within_budget());
    let line = report.line();
    assert!(line.contains("over the 1024-byte budget"), "{line}");
    assert!(
        line.contains("`passport_big_tool`"),
        "the offending command must be named: {line}"
    );
    let error = report.into_result().expect_err("over budget must fail");
    assert_eq!(error, line);

    // The same list inside a generous budget passes and still reports a number.
    let report = crate::mcp_size::measure(&list, 1 << 20);
    assert!(report.within_budget());
    assert!(report.line().contains("within the"));
    report.into_result().expect("inside the budget");
}

// --- an empty or tiny registry ----------------------------------------------------------------

#[test]
fn an_empty_registry_still_renders_every_aggregate_surface() {
    let empty: &'static [CommandSpec] = &[];
    // The real host-support table names real commands, so the synthetic empty registry is
    // checked against no rows; `model::from_specs` itself always checks the real table.
    let commands = model::from_specs_with_rows(empty, &[]).expect("an empty registry is valid");
    assert!(
        model::from_specs(empty).is_err_and(|e| e.contains("host support table")),
        "the real table is checked against every registry, an empty one included"
    );
    assert!(commands.is_empty());

    let outputs = plan(&commands).expect("plan over an empty registry");
    let paths: Vec<String> = outputs
        .iter()
        .map(|output| output.path.display().to_string())
        .collect();
    assert_eq!(
        paths,
        vec![
            "docs/schema/error@1.json",
            "docs/commands/index.md",
            "docs/errors.md",
            "web/src/gen/index.d.ts",
            "skills/passportsim/references/commands.md",
            "skills/passportsim/references/errors.md",
        ]
    );
    let index = render::index_page(&commands);
    assert!(index.contains("no command is registered yet"), "{index}");
    let errors = render::errors_page(&commands);
    assert!(
        errors.contains("`E_USAGE`"),
        "the pre-registered codes are listed without any command"
    );
    assert!(errors.contains("pre-registered"));
    let index_ts = tsgen::index_module(&commands);
    assert!(
        index_ts.contains("no command is registered yet"),
        "{index_ts}"
    );
    assert!(index_ts.contains("export declare function call"));
}

/// `docs/errors.md` has a "Meaning" column, so every code it renders needs a sentence.
///
/// The sentence lives in `pemu_api::error`'s `error_codes!` table, beside the number, and the
/// macro forces one on every pre-registered code at compile time. A code a command registers on
/// its own is only caught here. This runs against the registry a shipped build links, which is the
/// set the document is generated from.
#[test]
fn every_code_the_errors_page_renders_has_a_meaning() {
    let codes = pemu_api::error::registered_error_codes();
    assert!(!codes.is_empty(), "the registry has error codes");
    let missing: Vec<&str> = codes
        .iter()
        .filter(|code| code.meaning().is_none())
        .map(|code| code.name)
        .collect();
    assert!(
        missing.is_empty(),
        "no sentence in `error_codes!` for {missing:?}; `docs/errors.md` renders that column"
    );

    let commands = model::commands().expect("the registry");
    let page = render::errors_page(&commands);
    assert!(
        page.contains("| Code | Number | Meaning | Registered by |"),
        "{page}"
    );
    for code in codes {
        let meaning = code.meaning().expect("a sentence");
        assert!(
            page.contains(meaning),
            "`{}` renders without its meaning",
            code.name
        );
    }
}

#[test]
fn a_tiny_synthetic_registry_renders_every_per_item_surface() {
    let command = synthetic("tiny", "A synthetic command.");
    let outputs = plan(&[command]).expect("plan over one command");
    let paths: Vec<String> = outputs
        .iter()
        .map(|output| output.path.display().to_string())
        .collect();
    for expected in [
        "docs/commands/tiny.md",
        "docs/schema/commands/tiny.input.json",
        "docs/schema/commands/tiny.output.json",
        "web/src/gen/commands/tiny.d.ts",
        "skills/passportsim/references/commands/tiny.md",
    ] {
        assert!(paths.contains(&expected.to_string()), "{paths:?}");
    }
    let page = render::command_page(&command);
    assert!(page.contains("# `passportsim tiny`"));
    assert!(page.contains("| yes | yes | yes |"), "the full host matrix");
    let skill = render::skill_command_page(&command);
    assert!(skill.contains("# `passportsim tiny`"));
    assert!(
        skill.contains("| `passport_tiny` | none | yes | yes | yes |"),
        "{skill}"
    );
    // An installed skill is only its own directory: every link stays inside `references/`.
    for part in skill.split("](").skip(1) {
        let target = part.split(')').next().unwrap_or_default();
        assert_eq!(target, "../errors.md", "{skill}");
    }
}

// --- the TypeScript surface --------------------------------------------------------------------

#[test]
fn typescript_types_follow_required_optional_and_unions() {
    let command = synthetic("ts_probe", "A synthetic command.");
    fn probe_schema() -> pemu_api::spec::Schema {
        pemu_api::spec::schemars::json_schema!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer" },
                "mode": { "enum": ["a", "b"] },
                "tags": { "type": "array", "items": { "type": "string" } },
                "maybe": { "type": ["string", "null"] },
                "nested": { "type": "object", "properties": { "deep": { "type": "boolean" } },
                            "required": ["deep"] }
            },
            "required": ["name", "mode"]
        })
    }
    let mut spec = *command.spec;
    spec.input_schema = probe_schema;
    let spec: &'static CommandSpec = Box::leak(Box::new(spec));
    let command = model::Command {
        spec,
        hosts: model::hosts_of(spec),
    };

    let module = tsgen::command_module(&command).expect("render");
    assert!(module.contains("export type TsProbeArgs = {"), "{module}");
    assert!(module.contains("name: string;"), "{module}");
    assert!(module.contains("count?: number;"), "{module}");
    assert!(module.contains("mode: \"a\" | \"b\";"), "{module}");
    assert!(module.contains("tags?: string[];"), "{module}");
    assert!(module.contains("maybe?: string | null;"), "{module}");
    assert!(module.contains("deep: boolean;"), "{module}");
    // `any_schema` is `true`, which accepts anything.
    assert!(
        module.contains("export type TsProbeResult = unknown;"),
        "{module}"
    );
}

#[test]
fn an_unconstrained_object_is_record_string_unknown_not_the_empty_object() {
    // `Record<string, never>` accepts only `{}`, so a real result would fail the `bun test` type
    // check that guards the browser JS surface. Only an explicit `additionalProperties: false`
    // with no `properties` is the empty object.
    fn probe_schema() -> pemu_api::spec::Schema {
        pemu_api::spec::schemars::json_schema!({
            "type": "object",
            "properties": {
                "summary": { "type": "object" },
                "bundled_roms": { "type": "array", "items": { "type": "object" } },
                "closed": { "type": "object", "additionalProperties": false },
                "counts": { "type": "object", "additionalProperties": { "type": "integer" } }
            }
        })
    }
    let command = with_input("obj_ts_probe", probe_schema, &[]);
    let module = tsgen::command_module(&command).expect("render");
    assert!(
        module.contains("summary?: Record<string, unknown>;"),
        "{module}"
    );
    assert!(
        module.contains("bundled_roms?: Array<Record<string, unknown>>;"),
        "{module}"
    );
    assert!(
        module.contains("closed?: Record<string, never>;"),
        "{module}"
    );
    assert!(
        module.contains("counts?: Record<string, number>;"),
        "{module}"
    );
}

#[test]
fn the_pascal_case_of_a_registry_name_is_the_typescript_name() {
    assert_eq!(model::pascal_case("doctor"), "Doctor");
    assert_eq!(model::pascal_case("net_http"), "NetHttp");
    assert_eq!(model::pascal_case("ble_gatt"), "BleGatt");
}

#[test]
fn every_command_gets_its_one_http_route() {
    // The HTTP surface: `POST /v1/instances/{id}/commands/{name}` plus the three GET aliases,
    // with no instance-less form. `needs_instance` does not change the route, and no generated
    // table may invent one.
    let command = synthetic("no_instance", "A synthetic command.");
    assert!(!command.spec.annotations.needs_instance);
    assert_eq!(
        command.http_route(),
        "POST /v1/instances/{id}/commands/no_instance"
    );

    let mut spec = *command.spec;
    spec.annotations.needs_instance = true;
    let spec: &'static CommandSpec = Box::leak(Box::new(spec));
    let with_instance = model::Command {
        spec,
        hosts: model::hosts_of(spec),
    };
    assert_eq!(with_instance.http_route(), command.http_route());

    for command in model::commands().expect("registry") {
        assert!(
            command
                .http_route()
                .starts_with("POST /v1/instances/{id}/commands/"),
            "{} is routed outside the command route: {}",
            command.spec.name,
            command.http_route()
        );
    }
}

/// A synthetic command whose input schema is `schema`, with `positional` as its CLI positionals.
fn with_input(
    name: &'static str,
    schema: fn() -> pemu_api::spec::Schema,
    positional: &'static [&'static str],
) -> model::Command {
    let base = synthetic(name, "A synthetic command.");
    let mut spec = *base.spec;
    spec.input_schema = schema;
    spec.cli = CliShape {
        positional,
        aliases: &[],
        cli_only: &[],
    };
    let spec: &'static CommandSpec = Box::leak(Box::new(spec));
    model::Command {
        spec,
        hosts: model::hosts_of(spec),
    }
}

#[test]
fn the_synopsis_puts_positionals_first_and_brackets_optional_flags() {
    fn args_schema() -> pemu_api::spec::Schema {
        pemu_api::spec::schemars::json_schema!({
            "type": "object",
            "properties": {
                "what": { "type": "string" },
                "timeout": { "type": "string" },
                "wall_budget_ms": { "type": "integer" }
            },
            "required": ["what", "timeout"]
        })
    }
    let command = with_input("syn_probe", args_schema, &["what"]);

    let page = render::command_page(&command);
    assert!(
        page.contains(
            "passportsim syn_probe <what> --timeout <TIMEOUT> \
             [--wall-budget-ms <WALL_BUDGET_MS>] [--json <@file|->]"
        ),
        "{page}"
    );
    // The positional is not repeated as a flag.
    assert!(!page.contains("--what"), "{page}");
}

#[test]
fn an_object_property_is_carried_by_json_and_a_boolean_is_a_bare_flag() {
    // Top-level schema properties become flags; nested objects use `--json @file` or `--json -`
    // (stdin), the documented forms. A `--report <REPORT>` for an object property is
    // a command nobody can run, and it would bind the `pemu-cli` clap tree to that shape.
    fn args_schema() -> pemu_api::spec::Schema {
        pemu_api::spec::schemars::json_schema!({
            "$defs": { "Mode": { "enum": ["fast", "slow"] } },
            "type": "object",
            "properties": {
                "report": { "type": "object", "properties": { "id": { "type": "string" } } },
                "tags": { "type": "array", "items": { "type": "string" } },
                "force": { "type": "boolean" },
                "mode": { "$ref": "#/$defs/Mode" },
                "label": { "type": ["string", "null"] }
            },
            "required": ["report", "force"]
        })
    }
    let command = with_input("obj_probe", args_schema, &[]);
    let page = render::command_page(&command);

    assert!(
        page.contains(
            "passportsim obj_probe --force [--label <LABEL>] [--mode <MODE>] --json <@file|->"
        ),
        "{page}"
    );
    // Neither the object nor the array is published as a flag.
    assert!(!page.contains("--report"), "{page}");
    assert!(!page.contains("--tags"), "{page}");
    // A boolean takes no value, a scalar does, and the `$ref` to an enum stays a flag.
    assert!(!page.contains("--force <FORCE>"), "{page}");
    assert!(
        page.contains("| `report` | carried by `--json` |"),
        "{page}"
    );
    assert!(page.contains("| `tags` | carried by `--json` |"), "{page}");
    assert!(page.contains("| `force` | `--force` |"), "{page}");
    assert!(page.contains("| `mode` | `--mode <MODE>` |"), "{page}");
}

#[test]
fn json_stays_optional_when_no_required_property_needs_it() {
    fn args_schema() -> pemu_api::spec::Schema {
        pemu_api::spec::schemars::json_schema!({
            "type": "object",
            "properties": {
                "report": { "type": "object" },
                "name": { "type": "string" }
            },
            "required": ["name"]
        })
    }
    let command = with_input("opt_probe", args_schema, &[]);
    let page = render::command_page(&command);
    // The object is optional, so the command runs without `--json`; forcing it would publish a
    // requirement the command does not have.
    assert!(
        page.contains("passportsim opt_probe --name <NAME> [--json <@file|->]"),
        "{page}"
    );
}
