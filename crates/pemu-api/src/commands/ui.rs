//! `passportsim ui`: the pruned LVGL tree with element refs, or a diff since a revision.
//!
//! The walk is [`pemu_introspect::lvgl`]'s, reached through the
//! [`crate::commands::inspect::Introspectors`] seam. What is here is the revision an `eN` ref
//! belongs to, the diff, and the budget.
//!
//! A revision is minted per successful walk, and the newest tree is kept per instance so
//! [`pemu_introspect::lvgl::UiTree::resolve_stale`] can resolve an older ref while its address
//! holds an object of the same class under the same parent; otherwise the call is `E_STALE_REF`.
//! The diff is over the rendered lines, because that is what the agent read last time.
//!
//! The official menu is 63 objects, 17 lines and 976 characters when pruned. `max_nodes` (default
//! 300) bounds any other tree: beyond it the rendering is cut and `truncated` says so, rather than
//! blowing the 4,000-character output budget.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

use pemu_introspect::lvgl::{Prune, UiTree};

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_STALE_REF, E_STATE, E_USAGE};
use crate::instance::InstanceId;
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::inspect::{introspectors, walker_error, with_walk_firmware};
use crate::args::{instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::pool::Pool;
use crate::session::Session;

pub const SETTLE_TIMEOUT_DEFAULT: pemu_core::time::VTime = pemu_core::time::VTime::from_ms(1_000);

/// Runs the session in small slices until the LVGL safe point holds, within the virtual `timeout`,
/// and not at all when it already holds. `Ok(None)` when the host has no app ELF and cannot tell.
/// The walk reads DWARF, which `pemu-api` does not hold, so the host installs it with
/// [`set_safe_point`].
pub type SafePointHook = fn(&mut Session, pemu_core::time::VTime) -> Result<Option<bool>, ApiError>;

fn safe_point_slot() -> &'static Mutex<Option<SafePointHook>> {
    static SLOT: OnceLock<Mutex<Option<SafePointHook>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn set_safe_point(hook: Option<SafePointHook>) {
    *safe_point_slot().lock().unwrap_or_else(|e| e.into_inner()) = hook;
}

/// `settle: ui`, which `run` calls after a `ui:` match and `ui` before a walk. `Ok(None)` when no
/// host installed a [`SafePointHook`].
pub fn settle_to_safe_point(
    session: &mut Session,
    timeout: pemu_core::time::VTime,
) -> Result<Option<bool>, ApiError> {
    let hook = *safe_point_slot().lock().unwrap_or_else(|e| e.into_inner());
    match hook {
        Some(hook) => hook(session, timeout),
        None => Ok(None),
    }
}

pub const MAX_NODES_DEFAULT: u64 = 300;
pub const MAX_NODES_LIMIT: u64 = 2000;

/// The pool's table, not the process's: every pool mints `p1` first, so one process table would let
/// a second pool's `p1` continue the first one's revisions.
#[derive(Default)]
pub struct Trees {
    latest: BTreeMap<InstanceId, UiTree>,
}

impl Trees {
    pub fn get(&self, id: InstanceId) -> Option<&UiTree> {
        self.latest.get(&id)
    }

    /// One past the newest, or 1.
    pub fn next_rev(&self, id: InstanceId) -> u64 {
        self.latest.get(&id).map_or(1, |tree| tree.rev + 1)
    }

    pub fn put(&mut self, id: InstanceId, tree: UiTree) {
        self.latest.insert(id, tree);
    }

    /// Nothing has to call it when an instance stops: the pool never mints a stopped id again, and
    /// the table goes with the pool.
    pub fn forget(&mut self, id: InstanceId) {
        self.latest.remove(&id);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    /// Prune semantically, or show every object.
    pub prune: bool,
    pub include_style: bool,
    /// Return only the lines that changed since this revision.
    pub diff: Option<u64>,
    /// What makes `E_STALE_REF` reachable.
    pub resolve: Option<String>,
    pub max_nodes: u64,
    /// To reach an LVGL safe point before the walk.
    pub timeout: pemu_core::time::VTime,
}

impl Default for UiArgs {
    fn default() -> UiArgs {
        UiArgs {
            instance: None,
            prune: true,
            include_style: false,
            diff: None,
            resolve: None,
            max_nodes: MAX_NODES_DEFAULT,
            timeout: SETTLE_TIMEOUT_DEFAULT,
        }
    }
}

impl UiArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<UiArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "prune",
                "include_style",
                "diff",
                "resolve",
                "max_nodes",
                "timeout",
            ],
        )?;
        let prune = match opt_str(args, "prune")? {
            None => true,
            Some("semantic") => true,
            Some("none") => false,
            Some(other) => {
                return Err(usage(
                    "prune",
                    &format!("`{other}` is not one of semantic, none"),
                ));
            }
        };
        let max_nodes = opt_u64(args, "max_nodes")?.unwrap_or(MAX_NODES_DEFAULT);
        if max_nodes == 0 || max_nodes > MAX_NODES_LIMIT {
            return Err(usage(
                "max_nodes",
                &format!("expected 1..={MAX_NODES_LIMIT}"),
            ));
        }
        Ok(UiArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            prune,
            include_style: opt_bool(args, "include_style")?.unwrap_or(false),
            diff: opt_u64(args, "diff")?,
            resolve: opt_str(args, "resolve")?.map(str::to_owned),
            max_nodes,
            timeout: crate::args::opt_duration(args, "timeout")?.unwrap_or(SETTLE_TIMEOUT_DEFAULT),
        })
    }
}

/// The lines of `new` not in `old`, with their positions. By content and position: a node inserted
/// reports every line after it, the honest answer when indentation carries the structure.
#[must_use]
pub fn diff_lines(old: &str, new: &str) -> Vec<(usize, String)> {
    let old: Vec<&str> = old.lines().collect();
    new.lines()
        .enumerate()
        .filter(|(index, line)| old.get(*index) != Some(line))
        .map(|(index, line)| (index, line.to_owned()))
        .collect()
}

/// Runs the guest to a safe point first.
pub fn ui_on(session: &mut Session, args: &UiArgs) -> Result<Output, ApiError> {
    ui_read(session, args, true)
}

/// `ui` is `read_only`, which the lease never refuses, and running the guest is not: so it advances
/// only when no other holder has the lease, and otherwise reads the tree where the guest stands.
pub fn may_settle(lease: &crate::lease::Lease, now: pemu_core::time::VTime) -> bool {
    let mut advancing = SPEC_UI.annotations;
    advancing.read_only = false;
    lease
        .check_call(crate::lease::LeaseHolder::Agent, advancing, now)
        .is_ok()
}

pub fn ui_read(session: &mut Session, args: &UiArgs, settle: bool) -> Result<Output, ApiError> {
    let id = session.id;
    // A tree is read settled, so the guest runs to the next safe point first (never past one that
    // holds).
    let hook = *safe_point_slot().lock().unwrap_or_else(|e| e.into_inner());
    let settled = match hook {
        Some(hook) if settle => hook(session, args.timeout)?,
        _ => None,
    };
    let (previous, rev) =
        session.with_table(|trees: &mut Trees| (trees.get(id).cloned(), trees.next_rev(id)));
    let fw = session.fw.clone();
    let tree = with_walk_firmware(&fw, || (introspectors().ui)(session.machine(), rev))
        .map_err(|err| walker_error("ui", &err))?;

    // A ref carried over from an older revision.
    let resolved = match (&args.resolve, &previous) {
        (None, _) => None,
        (Some(reference), None) => {
            return Err(stale_ref(
                reference,
                "this instance has no earlier revision",
            ));
        }
        (Some(reference), Some(previous)) => Some(
            tree.resolve_stale(reference, previous)
                .map_err(|err| stale_ref(reference, &err.to_string()))?
                .clone(),
        ),
    };

    let prune = if args.prune {
        Prune::Semantic
    } else {
        Prune::None
    };
    let rendered = tree.render(prune, args.include_style);
    let lines: Vec<&str> = rendered.lines().collect();
    let limit = usize::try_from(args.max_nodes).unwrap_or(usize::MAX);
    let truncated = lines.len() > limit;
    let shown: Vec<&str> = lines.into_iter().take(limit).collect();
    let body = shown.join("\n");

    let changed = match (args.diff, &previous) {
        (Some(want), Some(previous)) if previous.rev == want => Some(diff_lines(
            &previous.render(prune, args.include_style),
            &body,
        )),
        (Some(want), Some(previous)) => {
            return Err(ApiError::new(
                E_USAGE,
                format!(
                    "`diff` names revision {want}, and the newest kept revision is {}",
                    previous.rev
                ),
            )
            .with_hint("call `ui` without `diff` to take a fresh revision"));
        }
        (Some(want), None) => {
            return Err(ApiError::new(
                E_USAGE,
                format!("`diff` names revision {want}, and this instance has none kept"),
            ));
        }
        (None, _) => None,
    };

    let json = serde_json::json!({
        "instance": id.to_string(),
        "ui_rev": tree.rev,
        "screen": {
            "w": tree.hor_res,
            "h": tree.ver_res,
        },
        "counts": {
            "objects": tree.nodes.len(),
            "shown": shown.len(),
            "labels": tree.nodes.iter().filter(|node| node.text.is_some()).count(),
        },
        "truncated": truncated,
        "settled": settled,
        "text": body,
        "diff": changed.as_ref().map(|changed| {
            changed
                .iter()
                .map(|(index, line)| serde_json::json!({ "line": index, "text": line }))
                .collect::<Vec<_>>()
        }),
        "resolved": resolved.as_ref().map(|node| serde_json::json!({
            "ref": node.reference,
            "class": node.class,
            "text": node.text,
        })),
        "warnings": tree.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
    });

    let mut text = format!("ui_rev {} {} object(s)", tree.rev, tree.nodes.len());
    if !settle && hook.is_some() {
        text.push_str(" (read unsettled: another holder has the lease, so the guest did not run)");
    }
    if settled == Some(false) {
        let _ = write!(
            text,
            " (no LVGL safe point within {} ms; the tree may be mid-update)",
            args.timeout.as_us() / 1_000
        );
    }
    match &changed {
        Some(changed) if changed.is_empty() => text.push_str("\nno change"),
        Some(changed) => {
            for (index, line) in changed {
                let _ = write!(text, "\n{index}: {line}");
            }
        }
        None => {
            if !body.is_empty() {
                text.push('\n');
                text.push_str(&body);
            }
        }
    }
    if truncated {
        let _ = write!(text, "\n... (cut at max_nodes={})", args.max_nodes);
    }

    session.with_table(|trees: &mut Trees| trees.put(id, tree));
    let receipt = session.receipt();
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

fn stale_ref(reference: &str, detail: &str) -> ApiError {
    ApiError::new(E_STALE_REF, format!("`{reference}`: {detail}")).with_hint(
        "an `eN` ref is valid for one `ui_rev`; call `ui` again and use a ref of the new revision",
    )
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`ui` arguments.",
        "properties": {
            "instance": instance_schema(),
            "prune": { "type": "string", "enum": ["semantic", "none"], "description": "Pruning (semantic)." },
            "include_style": { "type": "boolean", "description": "Show local style colours (false)." },
            "diff": { "type": "integer", "minimum": 1, "description": "Only lines changed since this ui_rev." },
            "resolve": { "type": "string", "pattern": "^e[0-9]+$", "description": "Resolve a ref from an earlier revision." },
            "max_nodes": { "type": "integer", "minimum": 1, "maximum": MAX_NODES_LIMIT, "description": "Lines to show (300)." },
            "timeout": crate::args::duration_schema("Run to a safe point first, 1s.")
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "ui_rev": { "type": "integer" },
            "screen": { "type": "object" },
            "counts": { "type": "object" },
            "truncated": { "type": "boolean" },
            "text": { "type": "string" },
            "diff": { "type": ["array", "null"], "items": { "type": "object" } },
            "resolved": { "type": ["object", "null"] },
            "warnings": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// Read the pruned LVGL tree, or only the lines that changed since a revision.
#[command(
    api_crate = crate,
    name = "ui",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(read_only, needs_instance),
    scenario_step = "ui.snapshot",
    errors(E_USAGE, E_STATE, E_STALE_REF, E_LEASE, E_INTERNAL),
    example(
        title = "Read the pruned tree",
        args = r#"{}"#,
    ),
    example(
        title = "Read only what changed since revision 3",
        args = r#"{"diff":3}"#,
    ),
    example(
        title = "Show every object with its local style colours",
        args = r#"{"prune":"none","include_style":true}"#,
    ),
)]
pub fn ui(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = UiArgs::from_json(&args)?;
    // Decided under the pool lock, read on the checked-out session.
    let may_run = std::cell::Cell::new(false);
    // On the checked-out session, outside the pool lock.
    crate::pool::with_session(
        |pool: &mut Pool| {
            let id = pool.bind(SPEC_UI.annotations, args.instance.as_deref())?;
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            let settle = pool
                .table()
                .get(id)
                .is_none_or(|state| may_settle(&state.lease, now));
            may_run.set(settle);
            Ok(id)
        },
        |session| ui_read(session, &args, may_run.get()),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use pemu_introspect::IntrospectError;
    use pemu_machine::MachineApi;

    use crate::commands::inspect::tests::{instance, scripted_ui, world};
    use crate::commands::inspect::{Introspectors, set_introspectors};

    fn args(json: serde_json::Value) -> UiArgs {
        UiArgs::from_json(&json).expect("inside the schema")
    }

    /// So a diff has exactly one changed line.
    fn renamed_ui(machine: &mut dyn MachineApi, rev: u64) -> Result<UiTree, IntrospectError> {
        let mut tree = scripted_ui(machine, rev)?;
        tree.nodes[1].text = Some("Button".to_owned());
        Ok(tree)
    }

    /// Same address, new class: an older ref goes stale.
    fn reclassed_ui(machine: &mut dyn MachineApi, rev: u64) -> Result<UiTree, IntrospectError> {
        let mut tree = scripted_ui(machine, rev)?;
        tree.nodes[1].class = "bar".to_owned();
        tree.nodes[1].text = None;
        Ok(tree)
    }

    fn install(ui: fn(&mut dyn MachineApi, u64) -> Result<UiTree, IntrospectError>) {
        set_introspectors(Introspectors {
            ui,
            ..crate::commands::inspect::tests::SCRIPTED
        });
    }

    /// Acts on two marked firmwares only, so another module's walk is not affected.
    pub(crate) fn scripted_safe_point(
        session: &mut Session,
        timeout: pemu_core::time::VTime,
    ) -> Result<Option<bool>, ApiError> {
        match session.fw.as_str() {
            "settle-probe" => {
                let until =
                    pemu_core::time::VTime(session.now().0 + pemu_core::time::VTime::from_ms(5).0);
                session.run_until(until);
                Ok(Some(true))
            }
            "settle-never" => {
                let until = pemu_core::time::VTime(session.now().0 + timeout.0);
                session.run_until(until);
                Ok(Some(false))
            }
            _ => Ok(None),
        }
    }

    #[test]
    fn ui_advances_to_a_safe_point_before_the_walk_and_reports_it() {
        let _world = world();
        set_safe_point(Some(scripted_safe_point));
        let (mut pool, id) = instance();
        let session = pool.session_mut(id).expect("the instance");
        session.fw = "settle-probe".to_owned();
        let out = ui_on(session, &args(serde_json::json!({}))).expect("walks");
        assert_eq!(out.json["settled"], true);
        assert_eq!(
            session.now().as_us(),
            5_000,
            "the walk came after the advance"
        );

        session.fw = "settle-never".to_owned();
        let out = ui_on(session, &args(serde_json::json!({"timeout": "20ms"}))).expect("walks");
        assert_eq!(out.json["settled"], false);
        assert!(
            out.text.contains("no LVGL safe point within 20 ms"),
            "{}",
            out.text
        );
        assert_eq!(session.now().as_us(), 25_000);

        session.fw = "official".to_owned();
        let out = ui_on(session, &args(serde_json::json!({}))).expect("walks");
        assert!(
            out.json["settled"].is_null(),
            "a host that cannot tell says nothing"
        );
        assert!(UiArgs::from_json(&serde_json::json!({"timeout": "soon"})).is_err());
    }

    #[test]
    fn ui_does_not_run_the_guest_while_another_holder_has_the_lease() {
        let _world = world();
        set_safe_point(Some(scripted_safe_point));
        let mut lease = crate::lease::Lease::free();
        assert!(may_settle(&lease, pemu_core::time::VTime(0)));
        lease
            .acquire(
                crate::lease::LeaseHolder::Ui,
                pemu_core::time::VTime(0),
                None,
            )
            .expect("free");
        assert!(!may_settle(&lease, pemu_core::time::VTime(0)));
        let mut own = crate::lease::Lease::free();
        own.acquire(
            crate::lease::LeaseHolder::Agent,
            pemu_core::time::VTime(0),
            None,
        )
        .expect("free");
        assert!(may_settle(&own, pemu_core::time::VTime(0)), "its own lease");

        let (mut pool, id) = instance();
        let session = pool.session_mut(id).expect("the instance");
        session.fw = "settle-probe".to_owned();
        let out = ui_read(session, &args(serde_json::json!({})), false).expect("walks");
        assert_eq!(session.now().as_us(), 0, "no guest time ran");
        assert!(out.json["settled"].is_null());
        assert!(out.text.contains("read unsettled"), "{}", out.text);
    }

    #[test]
    fn a_tree_carries_its_revision_and_the_pruned_rendering() {
        let _world = world();
        let (mut pool, id) = instance();
        let session = pool.session_mut(id).expect("the instance");
        let out =
            ui_on(session, &args(serde_json::json!({}))).expect("the scripted walker answers");
        assert_eq!(out.json["ui_rev"], 1, "the first revision of an instance");
        assert_eq!(out.json["counts"]["objects"], 2);
        assert_eq!(out.json["counts"]["labels"], 1);
        assert_eq!(out.json["screen"]["w"], 240);
        assert!(
            out.json["text"]
                .as_str()
                .expect("a rendering")
                .contains("\"Display\""),
            "{}",
            out.json["text"]
        );
        assert_eq!(out.json["truncated"], false);
    }

    #[test]
    fn every_call_mints_the_next_revision() {
        let _world = world();
        let (mut pool, id) = instance();
        let session = pool.session_mut(id).expect("the instance");
        let first = ui_on(session, &args(serde_json::json!({}))).expect("answers");
        let second = ui_on(session, &args(serde_json::json!({}))).expect("answers");
        assert_eq!(first.json["ui_rev"], 1);
        assert_eq!(second.json["ui_rev"], 2);
    }

    /// With one table for the process the second pool's first walk was revision 3.
    #[test]
    fn two_pools_that_mint_the_same_id_keep_their_own_trees() {
        let _world = world();
        install(scripted_ui);
        let (mut first, a) = instance();
        let (mut second, b) = instance();
        assert_eq!(a, b, "every new pool mints `p1` first");
        for expected in [1, 2] {
            let session = first.session_mut(a).expect("the instance");
            let out = ui_on(session, &args(serde_json::json!({}))).expect("walks");
            assert_eq!(out.json["ui_rev"], expected);
        }
        assert!(
            second.with_table(|trees: &mut Trees| trees.get(b).is_none()),
            "the second pool sees none of the first one's trees"
        );
        let session = second.session_mut(b).expect("the instance");
        let out = ui_on(session, &args(serde_json::json!({}))).expect("walks");
        assert_eq!(out.json["ui_rev"], 1, "a pool's first walk of its `p1`");

        drop(first);
        assert_eq!(
            second.with_table(|trees: &mut Trees| trees.get(b).map(|tree| tree.rev)),
            Some(1),
            "dropping the other pool leaves this one's tree"
        );
        let session = second.session_mut(b).expect("the instance");
        let out = ui_on(session, &args(serde_json::json!({}))).expect("walks");
        assert_eq!(out.json["ui_rev"], 2);
    }

    #[test]
    fn a_diff_returns_only_the_changed_lines() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        {
            let session = pool.session_mut(id).expect("the instance");
            ui_on(session, &args(serde_json::json!({}))).expect("revision 1");
        }
        install(renamed_ui);
        let session = pool.session_mut(id).expect("the instance");
        let out = ui_on(session, &args(serde_json::json!({ "diff": 1 }))).expect("revision 2");
        let changed = out.json["diff"].as_array().expect("a diff").clone();
        assert_eq!(changed.len(), 1, "only the label line changed: {changed:?}");
        assert!(
            changed[0]["text"]
                .as_str()
                .expect("a line")
                .contains("Button"),
            "{changed:?}"
        );
        assert!(out.text.contains("Button"), "{}", out.text);
        assert!(
            out.text.len() < out.json["text"].as_str().expect("the tree").len() + 40,
            "a diff costs less than the whole tree"
        );
    }

    #[test]
    fn an_unchanged_tree_diffs_to_no_change() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        {
            let session = pool.session_mut(id).expect("the instance");
            ui_on(session, &args(serde_json::json!({}))).expect("revision 1");
        }
        let session = pool.session_mut(id).expect("the instance");
        let out = ui_on(session, &args(serde_json::json!({ "diff": 1 }))).expect("revision 2");
        assert_eq!(out.json["diff"], serde_json::json!([]));
        assert!(out.text.contains("no change"), "{}", out.text);
    }

    #[test]
    fn a_ref_from_an_older_revision_still_resolves_while_the_object_is_the_same() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        {
            let session = pool.session_mut(id).expect("the instance");
            ui_on(session, &args(serde_json::json!({}))).expect("revision 1");
        }
        let session = pool.session_mut(id).expect("the instance");
        let out = ui_on(session, &args(serde_json::json!({ "resolve": "e2" }))).expect("resolves");
        assert_eq!(out.json["resolved"]["ref"], "e2");
        assert_eq!(out.json["resolved"]["class"], "label");
    }

    #[test]
    fn a_ref_whose_object_changed_class_is_stale() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        {
            let session = pool.session_mut(id).expect("the instance");
            ui_on(session, &args(serde_json::json!({}))).expect("revision 1");
        }
        install(reclassed_ui);
        let session = pool.session_mut(id).expect("the instance");
        let error = ui_on(session, &args(serde_json::json!({ "resolve": "e2" })))
            .expect_err("the address now holds a bar");
        assert_eq!(error.code, E_STALE_REF);
        assert!(error.message.contains("e2"), "{}", error.message);
    }

    #[test]
    fn a_ref_on_an_instance_with_no_earlier_revision_is_stale_too() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        let session = pool.session_mut(id).expect("the instance");
        let error = ui_on(session, &args(serde_json::json!({ "resolve": "e1" })))
            .expect_err("nothing was walked before");
        assert_eq!(error.code, E_STALE_REF);
    }

    /// Otherwise an agent that asked for a diff would pay for the whole tree without being told.
    #[test]
    fn a_diff_against_an_unknown_revision_is_usage() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        {
            let session = pool.session_mut(id).expect("the instance");
            ui_on(session, &args(serde_json::json!({}))).expect("revision 1");
        }
        let session = pool.session_mut(id).expect("the instance");
        let error = ui_on(session, &args(serde_json::json!({ "diff": 7 })))
            .expect_err("revision 7 was never taken");
        assert_eq!(error.code, E_USAGE);
    }

    #[test]
    fn max_nodes_cuts_the_rendering_and_says_so() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        let session = pool.session_mut(id).expect("the instance");
        let out = ui_on(session, &args(serde_json::json!({ "max_nodes": 1 }))).expect("answers");
        assert_eq!(out.json["truncated"], true);
        assert_eq!(out.json["counts"]["shown"], 1);
        assert!(out.text.contains("cut at max_nodes=1"), "{}", out.text);
    }

    /// The escape hatch when the semantic prune hides what an author is looking for.
    #[test]
    fn prune_none_shows_more_lines_than_the_semantic_prune() {
        let _world = world();
        let (mut pool, id) = instance();
        install(scripted_ui);
        let pruned = {
            let session = pool.session_mut(id).expect("the instance");
            ui_on(session, &args(serde_json::json!({})))
                .expect("answers")
                .json["counts"]["shown"]
                .as_u64()
                .unwrap_or(0)
        };
        let session = pool.session_mut(id).expect("the instance");
        let full = ui_on(session, &args(serde_json::json!({ "prune": "none" })))
            .expect("answers")
            .json["counts"]["shown"]
            .as_u64()
            .unwrap_or(0);
        assert!(full >= pruned, "{full} >= {pruned}");
    }

    #[test]
    fn the_line_diff_reports_position_and_content() {
        assert_eq!(diff_lines("a\nb\nc", "a\nB\nc"), vec![(1, "B".to_owned())]);
        assert_eq!(diff_lines("a\nb", "a\nb"), Vec::new());
        assert_eq!(
            diff_lines("a\nb", "a\nb\nc"),
            vec![(2, "c".to_owned())],
            "an appended line is one changed line"
        );
    }

    #[test]
    fn every_argument_outside_the_schema_is_usage() {
        for bad in [
            serde_json::json!({ "prune": "aggressive" }),
            serde_json::json!({ "max_nodes": 0 }),
            serde_json::json!({ "max_nodes": 2001 }),
            serde_json::json!({ "nonsense": 1 }),
        ] {
            assert_eq!(
                UiArgs::from_json(&bad)
                    .expect_err("outside the schema")
                    .code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("ui").expect("#[command] registered ui");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            UiArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.read_only && spec.annotations.needs_instance);
        assert_eq!(spec.scenario_step, Some("ui.snapshot"));
    }
}
