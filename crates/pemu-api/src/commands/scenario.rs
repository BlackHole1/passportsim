//! `passportsim scenario`: the runner. The document, its reader and the JUnit rendering are
//! [`crate::scenario`].
//!
//! A step key is valid when some `CommandSpec::scenario_step` claims it or it is one of
//! [`crate::scenario::BUILT_IN_STEPS`]; anything else is `E_USAGE` before the first step runs. A
//! mapping value is the argument object; a scalar fills the command's first positional argument.
//!
//! Globs are expanded here ([`expand`]), because `cmd.exe` and PowerShell do not expand them; only
//! the directory listing comes from the host ([`ScenarioIo`]). `--jobs` needs threads and the boot
//! cache, so the parallel half is a host seam, [`BatchRunner`]: the daemon boots one instance per
//! distinct boot, forks one copy per scenario and runs up to `jobs` at once. Without a runner, with
//! `--jobs 1` or with an `instance`, scenarios run in order on the bound instance.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_STATE, E_TIMEOUT, E_USAGE, E_WALL_BUDGET};
use crate::output::Output;
use crate::receipt::Strictness;
use crate::registry::command;
use crate::scenario::{
    Defaults, Report, RunStatus, Scenario, ScenarioError, Step, StepReport, StepStatus, Yaml, junit,
};
use crate::shape::ShapeLimits;
use crate::spec::{CommandSpec, HandlerCx, Schema};

use crate::args::{instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::pool::{Pool, with_pool};

/// 64 is far past any host's useful parallelism and keeps a typo from claiming a huge pool.
pub const JOBS_MAX: u64 = 64;
pub const WALL_BUDGET_MS: u64 = 600_000;
pub const JUNIT_DEFAULT: &str = "junit.xml";

/// How this command reaches scenario files and writes its report; `pemu-api` opens no file. Glob
/// matching stays in [`expand`].
#[derive(Copy, Clone)]
pub struct ScenarioIo {
    pub read_text: fn(&str) -> Result<String, String>,
    /// Relative and forward-slashed, in any order.
    pub list_files: fn(&str) -> Result<Vec<String>, String>,
    /// Returns the relative path it landed at.
    pub write_text: fn(&str, &str) -> Result<String, String>,
    /// Relative to the scenario root and forward-slashed, never the absolute path a forwarding CLI
    /// sent.
    pub display_path: fn(&str) -> String,
}

/// Falls back to the file name when the host's answer is still absolute.
fn shown_path(io: &ScenarioIo, path: &str) -> String {
    let shown = (io.display_path)(path).replace('\\', "/");
    let bytes = shown.as_bytes();
    let absolute = shown.starts_with('/')
        || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic());
    if absolute {
        shown.rsplit('/').next().unwrap_or_default().to_owned()
    } else {
        shown
    }
}

fn io_slot() -> &'static Mutex<Option<ScenarioIo>> {
    static IO: OnceLock<Mutex<Option<ScenarioIo>>> = OnceLock::new();
    IO.get_or_init(|| Mutex::new(None))
}

pub fn set_io(io: ScenarioIo) {
    let mut guard: MutexGuard<'_, Option<ScenarioIo>> = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(io);
}

/// The runner fills [`RunOptions::instance`] with the fork it made for it.
#[derive(Clone, Debug)]
pub struct BatchItem {
    /// Already parsed and checked against the registry.
    pub scenario: Scenario,
    pub options: RunOptions,
}

#[derive(Clone, Debug, Default)]
pub struct BatchOutcome {
    /// In item order.
    pub reports: Vec<Report>,
    /// Carried under the result's `batch` key.
    pub detail: serde_json::Value,
}

/// Runs every item on its own instance, at most `jobs` at once. An error refuses the whole batch
/// instead of one report per scenario.
pub type BatchRunner = fn(items: Vec<BatchItem>, jobs: usize) -> Result<BatchOutcome, ApiError>;

#[derive(Clone, Debug, PartialEq)]
pub struct BatchStart {
    /// In a fixed order, so two scenarios asking for the same boot share one template.
    pub args: serde_json::Value,
    /// [`run_scenario`] does not apply these again ([`RunOptions::started_with`]).
    pub keys: Vec<String>,
}

/// Its `image` as `fw`, plus the `setup` keys that change the boot and that `start` takes (`seed`,
/// `mode`, `usb`); other world keys go through `env` in [`run_scenario`]. No `image`, `setup.power:
/// off` (the boot cache holds a booted machine), `flash_seed`, `snapshot` and `board` are
/// `E_USAGE`.
pub fn batch_start(scenario: &Scenario) -> Result<BatchStart, ApiError> {
    if scenario.image.is_empty() {
        return Err(ApiError::new(
            E_USAGE,
            "a parallel batch forks each scenario from the boot cache of its `image`, and this \
             scenario names none",
        )
        .with_hint("add `image:` to the file, or run it with `--jobs 1`"));
    }
    let mut args = serde_json::Map::new();
    args.insert("fw".into(), scenario.image.clone().into());
    let mut keys = Vec::new();
    let Yaml::Map(entries) = &scenario.setup else {
        return Ok(BatchStart {
            args: serde_json::Value::Object(args),
            keys,
        });
    };
    let mut found: BTreeMap<&str, &Yaml> = BTreeMap::new();
    for (key, value) in entries {
        found.insert(key.as_str(), value);
    }
    for key in ["seed", "mode", "usb"] {
        if let Some(value) = found.get(key) {
            args.insert(key.into(), value.to_json());
            keys.push(key.to_owned());
        }
    }
    if let Some(power) = found.get("power")
        && match power {
            Yaml::Bool(on) => !on,
            Yaml::Str(text) => text == "off",
            _ => false,
        }
    {
        return Err(ApiError::new(
            E_USAGE,
            "`setup.power: off` cannot fork from the boot cache, which holds a booted machine",
        )
        .with_hint(
            "run this file with `--jobs 1` against an instance started with `--power off`",
        ));
    }
    for key in ["flash_seed", "snapshot", "board"] {
        if found.contains_key(key) {
            return Err(ApiError::new(
                E_USAGE,
                format!(
                    "`setup.{key}` has no `start` argument on this build, so no batch can boot it"
                ),
            ));
        }
    }
    Ok(BatchStart {
        args: serde_json::Value::Object(args),
        keys,
    })
}

fn batch_slot() -> &'static Mutex<Option<BatchRunner>> {
    static RUNNER: OnceLock<Mutex<Option<BatchRunner>>> = OnceLock::new();
    RUNNER.get_or_init(|| Mutex::new(None))
}

pub fn set_batch_runner(runner: Option<BatchRunner>) {
    *batch_slot().lock().unwrap_or_else(|e| e.into_inner()) = runner;
}

fn batch_runner() -> Option<BatchRunner> {
    *batch_slot().lock().unwrap_or_else(|e| e.into_inner())
}

fn io() -> Result<ScenarioIo, ApiError> {
    let guard = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    (*guard).ok_or_else(|| {
        ApiError::new(
            E_STATE,
            "this build cannot read a scenario file, so only `inline` scenarios run",
        )
        .with_hint("a host installs file access with `commands::scenario::set_io`")
    })
}

/// `*` and `?` stay inside one path segment and `**` crosses separators, the shell's reading: a `*`
/// that crossed `/` would make `tests/scenarios/*.yaml` pick up files in subdirectories.
#[must_use]
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // `**` takes zero or more whole segments.
            (0..=path.len()).any(|take| segments_match(rest, &path[take..]))
        }
        Some((head, rest)) => match path.split_first() {
            None => false,
            Some((segment, tail)) => segment_matches(head, segment) && segments_match(rest, tail),
        },
    }
}

/// Iterative backtracking, so a hostile pattern costs time but no stack.
#[must_use]
pub fn segment_matches(pattern: &str, segment: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let segment: Vec<char> = segment.chars().collect();
    let (mut p, mut s) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while s < segment.len() {
        match pattern.get(p) {
            Some('?') => {
                p += 1;
                s += 1;
            }
            Some('*') => {
                star = Some(p);
                mark = s;
                p += 1;
            }
            Some(c) if *c == segment[s] => {
                p += 1;
                s += 1;
            }
            _ => match star {
                Some(at) => {
                    p = at + 1;
                    mark += 1;
                    s = mark;
                }
                None => return false,
            },
        }
    }
    while pattern.get(p) == Some(&'*') {
        p += 1;
    }
    p == pattern.len()
}

/// Sorted, so a batch runs the same way twice. A pattern with no wildcard selects itself.
#[must_use]
pub fn expand(pattern: &str, candidates: &[String]) -> Vec<String> {
    if !pattern.contains(['*', '?']) {
        return vec![pattern.to_owned()];
    }
    let mut hits: Vec<String> = candidates
        .iter()
        .filter(|path| glob_matches(pattern, path))
        .cloned()
        .collect();
    hits.sort_unstable();
    hits.dedup();
    hits
}

/// Everything up to the first wildcard segment.
#[must_use]
pub fn glob_root(pattern: &str) -> String {
    let mut root = Vec::new();
    for segment in pattern.split('/') {
        if segment.contains(['*', '?']) {
            break;
        }
        root.push(segment);
    }
    if root.len() == pattern.split('/').count() {
        root.pop();
    }
    root.join("/")
}

/// Translates a YAML matcher mapping into the matcher text `run` takes.
pub fn matcher_text(value: &Yaml) -> Result<String, ScenarioError> {
    if let Some(text) = plain_matcher(value) {
        return Ok(text);
    }
    let entries = value.as_map().ok_or_else(|| {
        ScenarioError::whole(format!(
            "a matcher is a mapping with one key, saw {}",
            value.kind()
        ))
    })?;
    let [(key, body)] = entries else {
        return Err(ScenarioError::whole(format!(
            "a matcher has exactly one key, saw {}",
            entries.len()
        )));
    };
    match key.as_str() {
        "serial" => serial_matcher(body),
        "log" => log_matcher(body),
        "ui" => ui_matcher(body),
        "event" => Ok(format!(
            "event:{}",
            body.as_str()
                .ok_or_else(|| ScenarioError::whole("`event` names an event kind"))?
        )),
        "symbol" => symbol_matcher(body),
        "any" | "all" | "seq" => composite(key, body),
        other => Err(ScenarioError::whole(format!(
            "`{other}` is not a matcher \
             (serial, log, ui, event, symbol, any, all, seq)"
        ))),
    }
}

/// A matcher already written in the text grammar.
fn plain_matcher(value: &Yaml) -> Option<String> {
    match value {
        Yaml::Str(text) if text.contains(':') || text.contains('(') => Some(text.clone()),
        _ => None,
    }
}

fn serial_matcher(body: &Yaml) -> Result<String, ScenarioError> {
    let pattern = match (body.get("re"), body.get("literal"), body.as_str()) {
        (Some(re), _, _) => format!("/{}/", text_of(re, "serial.re")?),
        (None, Some(literal), _) => format!("\"{}\"", text_of(literal, "serial.literal")?),
        (None, None, Some(text)) => format!("/{text}/"),
        _ => {
            return Err(ScenarioError::whole(
                "a serial matcher needs `re` or `literal`",
            ));
        }
    };
    let mut text = format!("serial:{pattern}");
    if let Some(stream) = body.get("stream").and_then(Yaml::as_str) {
        let _ = write!(text, ",{stream}");
    }
    if let Some(mode) = body.get("mode").and_then(Yaml::as_str) {
        let _ = write!(text, ",{mode}");
    }
    if let Some(from) = body.get("from").and_then(Yaml::as_str) {
        let _ = write!(text, ",from={from}");
    }
    Ok(text)
}

/// An absent field is the grammar's `*`.
fn log_matcher(body: &Yaml) -> Result<String, ScenarioError> {
    let tag = body
        .get("tag")
        .and_then(Yaml::as_str)
        .unwrap_or_else(|| "*".to_owned());
    let level = body
        .get("level")
        .and_then(Yaml::as_str)
        .unwrap_or_else(|| "*".to_owned());
    let re = body
        .get("re")
        .ok_or_else(|| ScenarioError::whole("a log matcher needs `re`"))?;
    Ok(format!("log:{tag}:{level}:/{}/", text_of(re, "log.re")?))
}

/// A nested `selected: {text: T}` is written `ui: {selected: T}`: the grammar has one attribute and
/// one pattern per matcher, and a nested query belongs to `ui.expect`.
fn ui_matcher(body: &Yaml) -> Result<String, ScenarioError> {
    if body.as_str().as_deref() == Some("changed") {
        return Ok("ui:changed".to_owned());
    }
    let entries = body
        .as_map()
        .ok_or_else(|| ScenarioError::whole("a ui matcher is `changed` or `{<attr>: text}`"))?;
    let [(attr, pattern)] = entries else {
        return Err(ScenarioError::whole(
            "a ui matcher names exactly one attribute",
        ));
    };
    Ok(format!(
        "ui:{attr}=\"{}\"",
        text_of(pattern, "the ui pattern")?
    ))
}

fn symbol_matcher(body: &Yaml) -> Result<String, ScenarioError> {
    if let Some(name) = body.as_str() {
        return Ok(format!("symbol:{name}"));
    }
    let target = body
        .get("name")
        .or_else(|| body.get("addr"))
        .ok_or_else(|| ScenarioError::whole("a symbol matcher needs `name` or `addr`"))?;
    let mut text = format!("symbol:{}", text_of(target, "symbol.name")?);
    if let Some(Yaml::Int(hits)) = body.get("hits") {
        let _ = write!(text, ":hits={hits}");
    }
    Ok(text)
}

fn composite(key: &str, body: &Yaml) -> Result<String, ScenarioError> {
    let items = body
        .as_seq()
        .ok_or_else(|| ScenarioError::whole(format!("`{key}` takes a sequence of matchers")))?;
    let children: Result<Vec<String>, ScenarioError> = items.iter().map(matcher_text).collect();
    Ok(format!("{key}({})", children?.join(",")))
}

fn text_of(value: &Yaml, what: &str) -> Result<String, ScenarioError> {
    value
        .as_str()
        .ok_or_else(|| ScenarioError::whole(format!("{what} is a scalar, saw {}", value.kind())))
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Empty lets each command bind the one live instance.
    pub instance: String,
    /// `set` steps add to them.
    pub vars: BTreeMap<String, String>,
    pub stop_on_failure: bool,
    pub source: String,
    /// Zero means no bound.
    pub wall_budget_ms: u64,
    /// Neither applied again nor refused.
    pub started_with: Vec<String>,
}

/// Not [`crate::commands::run::WallBudget`], which binds to a `Session` a scenario may not have
/// yet; it reads the same host clock and gives the same refusal. With no clock nothing is enforced.
#[derive(Copy, Clone, Debug)]
struct ScenarioBudget {
    clock: Option<crate::pool::HostClock>,
    started_ms: u64,
    budget_ms: u64,
}

impl ScenarioBudget {
    /// `0` never runs out.
    fn start(budget_ms: u64) -> ScenarioBudget {
        let clock = with_pool(|pool: &mut Pool| pool.host_clock());
        ScenarioBudget {
            clock,
            started_ms: clock.map_or(0, |now| now()),
            budget_ms,
        }
    }

    fn spent(&self) -> bool {
        match self.clock {
            Some(now) if self.budget_ms > 0 => {
                now().saturating_sub(self.started_ms) >= self.budget_ms
            }
            _ => false,
        }
    }
}

/// Sorted so a refusal message reads the same twice.
#[must_use]
pub fn registry_aliases() -> Vec<&'static str> {
    let mut aliases: Vec<&'static str> = crate::registry::commands()
        .iter()
        .filter_map(|spec| spec.scenario_step)
        .collect();
    aliases.sort_unstable();
    aliases.dedup();
    aliases
}

fn command_of(alias: &str) -> Option<&'static CommandSpec> {
    crate::registry::commands()
        .iter()
        .find(|spec| spec.scenario_step == Some(alias))
}

/// World state (`usb`, `battery`) becomes one journaled `env` call. Start-time state cannot be done
/// to an instance somebody else booted, so `power` is checked against the lifecycle and the rest is
/// refused by name. Keys the batch booted with (`started_with`) are skipped.
fn apply_setup(
    setup: &crate::scenario::Yaml,
    instance: &str,
    started_with: &[String],
) -> Result<(), ApiError> {
    let entries = match setup {
        Yaml::Null => return Ok(()),
        Yaml::Map(entries) => entries,
        other => {
            return Err(usage(
                "setup",
                &format!("is a mapping, saw {}", other.kind()),
            ));
        }
    };
    let mut env_args = serde_json::Map::new();
    for (key, value) in entries {
        if started_with.contains(key) {
            continue;
        }
        match key.as_str() {
            "usb" | "battery" | "nfc" | "mic" => {
                env_args.insert(key.clone(), value.to_json());
            }
            "power" => check_power(value, instance)?,
            "seed" | "mode" => {
                return Err(ApiError::new(
                    E_USAGE,
                    format!(
                        "`setup.{key}` is a property of the instance, and this runner binds to an \
                         instance it did not create"
                    ),
                )
                .with_hint(
                    "pass it to `start` when you create the instance, or run the files as a \
                     parallel batch (`--jobs 2` or more, no `instance`), which boots each \
                     scenario with its `seed`, `mode` and `usb`",
                ));
            }
            "flash_seed" | "snapshot" | "board" => {
                return Err(ApiError::new(
                    E_USAGE,
                    format!(
                        "`setup.{key}` is a property of the instance, and `start` takes no \
                         `{key}` on this build, so no runner can establish it"
                    ),
                ));
            }
            "hints" => {
                return Err(ApiError::new(
                    E_USAGE,
                    "`setup.hints` is not supported: this build has no ui-hint vocabulary",
                ));
            }
            "wifi" => {
                return Err(ApiError::new(
                    E_USAGE,
                    "`setup.wifi` is not supported; a `wifi.ap` step sets the access points",
                ));
            }
            "strict" => {
                return Err(ApiError::new(
                    E_USAGE,
                    "strictness is the caller's, not the file's: pass `--strict`",
                ));
            }
            other => {
                return Err(usage(
                    "setup",
                    &format!(
                        "`{other}` is not a scenario@1 setup key (known: power, usb, battery, \
                         nfc, mic, seed, mode, flash_seed, snapshot, board, hints, wifi, strict)"
                    ),
                ));
            }
        }
    }
    if env_args.is_empty() {
        return Ok(());
    }
    if !instance.is_empty() {
        env_args.insert("instance".into(), instance.into());
    }
    let spec = crate::registry::find("env")
        .ok_or_else(|| ApiError::new(E_INTERNAL, "`env` is not registered"))?;
    let mut cx = HandlerCx {};
    (spec.handler)(&mut cx, serde_json::Value::Object(env_args)).map(|_| ())
}

fn check_power(value: &crate::scenario::Yaml, instance: &str) -> Result<(), ApiError> {
    let want_on = match value {
        // The reader already folds YAML 1.1 `on` and `off`.
        Yaml::Bool(on) => *on,
        Yaml::Str(text) if text == "on" => true,
        Yaml::Str(text) if text == "off" => false,
        other => {
            return Err(usage(
                "setup.power",
                &format!("is `on` or `off`, saw {}", other.kind()),
            ));
        }
    };
    let id = crate::instance::InstanceId::parse(instance).map_err(|_| {
        ApiError::new(
            E_STATE,
            "`setup.power` is checked against a running instance, and none is running",
        )
        .with_hint("`start --fw <corpus id or path>` creates one")
    })?;
    let lifecycle = with_pool(|pool: &mut Pool| pool.table().get(id).map(|state| state.lifecycle))
        .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{instance}`")))?;
    let is_on = lifecycle != crate::instance::Lifecycle::PoweredOff;
    if is_on == want_on {
        return Ok(());
    }
    Err(ApiError::new(
        E_STATE,
        format!(
            "`setup.power: {}` but instance `{instance}` is {}",
            if want_on { "on" } else { "off" },
            lifecycle.as_str()
        ),
    )
    .with_hint("`start --power on|off` sets it; the runner cannot power an instance it bound"))
}

/// The runner cannot load an image, so it refuses a mismatch: otherwise a scenario would report a
/// green result about firmware it never touched.
fn check_image(image: &str, instance: &str) -> Result<(), ApiError> {
    if image.is_empty() {
        return Ok(());
    }
    let id = crate::instance::InstanceId::parse(instance).map_err(|_| {
        ApiError::new(
            E_STATE,
            format!(
                "the scenario names `image: {image}`, and no instance is running to check it \
                     against"
            ),
        )
        .with_hint("`start --fw <corpus id or path>` creates one")
    })?;
    let fw = with_pool(|pool: &mut Pool| pool.session(id).map(|session| session.fw.clone()))
        .ok_or_else(|| ApiError::new(E_STATE, format!("no instance `{instance}`")))?;
    if fw == image {
        return Ok(());
    }
    Err(ApiError::new(
        E_STATE,
        format!("the scenario names `image: {image}`, and `{instance}` was started from `{fw}`"),
    )
    .with_hint("start the instance with `--fw` matching the scenario, or drop the `image` key"))
}

/// With `stop_on_failure`, every step after the first failure is reported `skipped` rather than
/// dropped, so the report shows how far the scenario got.
pub fn run_scenario(scenario: &Scenario, options: &RunOptions) -> Report {
    let mut state = State {
        vars: options.vars.clone(),
        defaults: scenario.defaults.clone(),
        fail_on: Vec::new(),
        instance: options.instance.clone(),
        steps: Vec::new(),
        stopped: false,
        stop_on_failure: options.stop_on_failure,
        budget: ScenarioBudget::start(options.wall_budget_ms),
        out_of_budget: false,
    };
    // `image` and `setup` are preconditions checked before step 1; if they fail nothing runs.
    state.bind_instance();
    for check in [
        check_image(&scenario.image, &state.instance),
        apply_setup(&scenario.setup, &state.instance, &options.started_with),
    ] {
        if let Err(error) = check {
            state.push_error(0, "setup", "", &error);
            state.stopped = true;
        }
    }
    for matcher in &scenario.fail_on {
        match matcher_text(matcher) {
            Ok(text) => state.fail_on.push(text),
            Err(err) => {
                state.push_error(0, "fail_on", "", &ApiError::from(err));
                state.stopped = true;
            }
        }
    }
    // Always walked, so a stopped scenario still lists its steps as `skipped`.
    state.walk(&scenario.steps);
    // The caveats come from the instance's receipt, which accumulates over the run, so a caveat
    // from any step reaches the verdict.
    let caveats = receipt_caveats(&state.instance);
    let status = if state.out_of_budget {
        // Ahead of `Error`: they exit 6 and 8, and only one is worth retrying.
        RunStatus::WallBudget
    } else if state.steps.iter().any(|s| s.status == StepStatus::Error) {
        RunStatus::Error
    } else if state.steps.iter().any(|s| s.status == StepStatus::Fail) {
        RunStatus::Fail
    } else if caveats.is_empty() {
        RunStatus::Pass
    } else {
        RunStatus::PassWithCaveats
    };
    Report {
        name: scenario.name.clone(),
        source: options.source.clone(),
        instance: state.instance.clone(),
        status,
        caveats,
        vt_us: state.steps.last().map_or(0, |step| step.vt_us),
        steps: state.steps,
    }
}

/// None when it bound no instance.
fn receipt_caveats(instance: &str) -> Vec<crate::receipt::Caveat> {
    let Ok(id) = crate::instance::InstanceId::parse(instance) else {
        return Vec::new();
    };
    with_pool(|pool: &mut Pool| {
        pool.session_mut(id)
            .map(|session| session.receipt().caveats())
    })
    .unwrap_or_default()
}

struct State {
    vars: BTreeMap<String, String>,
    defaults: Defaults,
    fail_on: Vec<String>,
    instance: String,
    steps: Vec<StepReport>,
    stopped: bool,
    stop_on_failure: bool,
    budget: ScenarioBudget,
    /// So the report can say so rather than just "fail".
    out_of_budget: bool,
}

impl State {
    fn walk(&mut self, steps: &[Step]) {
        for step in steps {
            // Running out of host budget is infrastructure, not a result; remaining steps are
            // `skipped`.
            if !self.stopped && self.budget.spent() {
                self.out_of_budget = true;
                self.stopped = true;
                self.push_error(
                    self.steps.len(),
                    &step.key,
                    &step.name,
                    &crate::commands::run::wall_budget_exceeded(self.budget.budget_ms),
                );
                continue;
            }
            if self.stopped {
                self.push(StepReport {
                    index: self.steps.len(),
                    name: step.name.clone(),
                    key: step.key.clone(),
                    status: StepStatus::Skipped,
                    vt_us: self.steps.last().map_or(0, |s| s.vt_us),
                    elapsed_vt_us: 0,
                    error: None,
                });
                continue;
            }
            self.one(step);
        }
    }

    fn one(&mut self, step: &Step) {
        match step.key.as_str() {
            "repeat" => self.repeat(step),
            "set" => self.set(step),
            "ui.expect" => self.ui_expect(step),
            _ => self.call(step),
        }
    }

    fn repeat(&mut self, step: &Step) {
        let times = match crate::scenario::repeat_times(&step.value) {
            Ok(times) => times,
            Err(err) => return self.fail_step(step, &ApiError::from(err), StepStatus::Error),
        };
        let body = match crate::scenario::repeat_steps(&step.value) {
            Ok(body) => body,
            Err(err) => return self.fail_step(step, &ApiError::from(err), StepStatus::Error),
        };
        for _ in 0..times {
            if self.stopped {
                break;
            }
            self.walk(&body);
        }
    }

    /// A `ui` call, whose walk becomes the newest revision, then the [`crate::scenario::UiExpect`]
    /// assertion over that tree. A failed walk is an `error`; a broken assertion is a `fail` with
    /// one line per assertion.
    fn ui_expect(&mut self, step: &Step) {
        let value = self.substitute(&step.value);
        let expect = match crate::scenario::UiExpect::parse(&value) {
            Ok(expect) => expect,
            Err(err) => return self.fail_step(step, &ApiError::from(err), StepStatus::Error),
        };
        let Some(spec) = crate::registry::find("ui") else {
            let error = ApiError::new(E_INTERNAL, "this build registers no `ui` command");
            return self.fail_step(step, &error, StepStatus::Error);
        };
        let mut args = serde_json::Map::new();
        if self.instance_is_set() {
            args.insert("instance".into(), self.instance.clone().into());
        }
        // The walk waits for a safe point within the step's timeout.
        if let Some(timeout) = step
            .timeout
            .clone()
            .or_else(|| self.defaults.timeout.clone())
        {
            args.insert("timeout".into(), timeout.into());
        }
        let before = self.steps.last().map_or(0, |s| s.vt_us);
        let output = match (spec.handler)(&mut HandlerCx {}, serde_json::Value::Object(args)) {
            Ok(output) => output,
            Err(error) => return self.fail_step(step, &self.envelope(error), StepStatus::Error),
        };
        let Some(id) = output
            .json
            .get("instance")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| crate::instance::InstanceId::parse(text).ok())
        else {
            let error = ApiError::new(E_INTERNAL, "`ui` answered without an instance");
            return self.fail_step(step, &error, StepStatus::Error);
        };
        if !self.instance_is_set() {
            self.instance = id.to_string();
        }
        if output.json.get("settled") == Some(&serde_json::Value::Bool(false)) {
            let error = ApiError::new(
                E_TIMEOUT,
                "ui.expect: the guest reached no LVGL safe point within the step timeout, so no \
                 settled tree was read",
            );
            return self.fail_step(step, &error, StepStatus::Fail);
        }
        // The `ui` handler above ran on the process pool, and `ui::Trees` is kept per pool.
        let tree = with_pool(|pool: &mut Pool| {
            pool.with_table(|trees: &mut super::ui::Trees| trees.get(id).cloned())
        });
        let Some(tree) = tree else {
            let error = ApiError::new(E_INTERNAL, "`ui` kept no tree for the instance");
            return self.fail_step(step, &error, StepStatus::Error);
        };
        let failures = expect.failures(&tree);
        let vt = output.receipt.vt_us;
        let error = (!failures.is_empty()).then(|| {
            serde_json::json!({
                "code": "E_ASSERT",
                "message": format!("ui.expect at ui_rev {}: {}", tree.rev, failures.join("; ")),
            })
        });
        let failed = error.is_some();
        self.push(StepReport {
            index: self.steps.len(),
            name: step.name.clone(),
            key: step.key.clone(),
            status: if failed {
                StepStatus::Fail
            } else {
                StepStatus::Pass
            },
            vt_us: vt,
            elapsed_vt_us: vt.saturating_sub(before),
            error,
        });
        if failed && !step.continue_on_error {
            self.stopped = self.stop_on_failure;
        }
    }

    /// A `${var}` substitution for later steps.
    fn set(&mut self, step: &Step) {
        let Some(entries) = step.value.as_map() else {
            let error = ApiError::new(E_USAGE, "`set` takes a mapping of names to values");
            return self.fail_step(step, &error, StepStatus::Error);
        };
        for (name, value) in entries {
            self.vars
                .insert(name.clone(), value.as_str().unwrap_or_default());
        }
        let vt = self.steps.last().map_or(0, |s| s.vt_us);
        self.push(StepReport {
            index: self.steps.len(),
            name: step.name.clone(),
            key: step.key.clone(),
            status: StepStatus::Pass,
            vt_us: vt,
            elapsed_vt_us: 0,
            error: None,
        });
    }

    fn call(&mut self, step: &Step) {
        let (name, args) = match self.arguments(step) {
            Ok(pair) => pair,
            Err(error) => return self.fail_step(step, &error, StepStatus::Error),
        };
        let Some(spec) = crate::registry::find(&name) else {
            let error = ApiError::new(E_USAGE, format!("`{name}` is not a registered command"));
            return self.fail_step(step, &error, StepStatus::Error);
        };
        let before = self.steps.last().map_or(0, |s| s.vt_us);
        let mut cx = HandlerCx {};
        match (spec.handler)(&mut cx, args) {
            Ok(output) => {
                // A `run` whose `fail_if` fired is a failed step although the call succeeded. A
                // caveat is not: it reaches the verdict through the receipt.
                let failed = output
                    .json
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    == Some("fail");
                let vt = output.receipt.vt_us;
                if !self.instance_is_set()
                    && let Some(id) = output
                        .json
                        .get("instance")
                        .and_then(serde_json::Value::as_str)
                {
                    self.instance = id.to_owned();
                }
                // A field the answer does not hold fails the step, like a `fail_on` matcher firing.
                let unmet = step
                    .expect
                    .as_ref()
                    .map(|expect| {
                        crate::scenario::answer_failures(&self.substitute(expect), &output.json)
                    })
                    .unwrap_or_default();
                let error = if failed {
                    Some(serde_json::json!({
                        "code": "E_ASSERT",
                        "message": format!(
                            "a `fail_on` matcher fired: {}",
                            output.json.get("match").cloned().unwrap_or(serde_json::Value::Null)
                        ),
                    }))
                } else if unmet.is_empty() {
                    None
                } else {
                    Some(serde_json::json!({
                        "code": "E_ASSERT",
                        "message": format!("expect: {}", unmet.join("; ")),
                    }))
                };
                let failed = error.is_some();
                let status = if failed {
                    StepStatus::Fail
                } else {
                    StepStatus::Pass
                };
                self.push(StepReport {
                    index: self.steps.len(),
                    name: step.name.clone(),
                    key: step.key.clone(),
                    status,
                    vt_us: vt,
                    elapsed_vt_us: vt.saturating_sub(before),
                    error,
                });
                if failed && !step.continue_on_error {
                    self.stopped = self.stop_on_failure;
                }
            }
            Err(error) => {
                // A timeout is the run's own assertion failing, so it is a `fail`; anything else is
                // an `error`.
                let status = if error.code == crate::error::E_TIMEOUT {
                    StepStatus::Fail
                } else {
                    StepStatus::Error
                };
                // A guest fault carries its envelope (task table, console tail, next call) into the
                // report.
                self.fail_step(step, &self.envelope(error), status);
            }
        }
    }

    /// While the instance that produced it is still live. An error that reports no fault is
    /// returned untouched.
    fn envelope(&self, error: ApiError) -> ApiError {
        let Ok(id) = crate::instance::InstanceId::parse(&self.instance) else {
            return error;
        };
        with_pool(|pool| crate::commands::inspect::fault_envelope_in_pool(pool, id, error))
    }

    fn arguments(&self, step: &Step) -> Result<(String, serde_json::Value), ApiError> {
        let value = self.substitute(&step.value);
        let (name, mut args) = match step.key.as_str() {
            // `delay: DUR` is `run {for: DUR}`.
            "delay" => (
                "run".to_owned(),
                serde_json::json!({ "for": scalar(&value)? }),
            ),
            // `expect_not: {serial: {re}, for: DUR}` is `run {for, fail_if}`.
            "expect_not" => {
                let mut object = serde_json::Map::new();
                let window = value.get("for").and_then(Yaml::as_str);
                let matcher = value
                    .as_map()
                    .map(|entries| {
                        Yaml::Map(
                            entries
                                .iter()
                                .filter(|(key, _)| key != "for")
                                .cloned()
                                .collect(),
                        )
                    })
                    .ok_or_else(|| {
                        ApiError::new(E_USAGE, "`expect_not` takes a matcher and a `for` window")
                    })?;
                object.insert(
                    "for".into(),
                    window
                        .or_else(|| self.defaults.timeout.clone())
                        .unwrap_or_else(|| "1s".to_owned())
                        .into(),
                );
                object.insert(
                    "fail_if".into(),
                    serde_json::json!([matcher_text(&matcher).map_err(ApiError::from)?]),
                );
                ("run".to_owned(), serde_json::Value::Object(object))
            }
            alias => {
                let spec = command_of(alias)
                    .ok_or_else(|| ApiError::new(E_USAGE, format!("`{alias}` is not a step")))?;
                let args = match &value {
                    Yaml::Map(_) => value.to_json(),
                    Yaml::Null => serde_json::json!({}),
                    scalar => {
                        let key = spec.cli.positional.first().ok_or_else(|| {
                            ApiError::new(
                                E_USAGE,
                                format!(
                                    "`{alias}` takes no positional argument, so its step value is \
                                     a mapping"
                                ),
                            )
                        })?;
                        // A bare scalar under `wait` is a matcher too.
                        serde_json::json!({ *key: scalar.to_json() })
                    }
                };
                (spec.name.to_owned(), args)
            }
        };
        // `wait: {serial: {..}}` is a matcher mapping, not a `run` argument object.
        if step.key == "wait" {
            let until = matcher_text(&value).map_err(ApiError::from)?;
            args = serde_json::json!({ "until": until });
        }
        let map = args
            .as_object_mut()
            .ok_or_else(|| ApiError::new(E_USAGE, "a step's arguments are an object"))?;
        if let Some(timeout) = step
            .timeout
            .clone()
            .or_else(|| self.defaults.timeout.clone())
            && waits(&name)
            && !map.contains_key("timeout")
        {
            map.insert("timeout".into(), timeout.into());
        }
        if !self.fail_on.is_empty() && waits(&name) && !map.contains_key("fail_if") {
            map.insert("fail_if".into(), self.fail_on.clone().into());
        }
        if self.instance_is_set() && !map.contains_key("instance") && needs_instance(&name) {
            map.insert("instance".into(), self.instance.clone().into());
        }
        Ok((name, args))
    }

    fn substitute(&self, value: &Yaml) -> Yaml {
        match value {
            Yaml::Str(text) if text.contains("${") => Yaml::Str(substitute(text, &self.vars)),
            Yaml::Seq(items) => Yaml::Seq(items.iter().map(|item| self.substitute(item)).collect()),
            Yaml::Map(entries) => Yaml::Map(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), self.substitute(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn instance_is_set(&self) -> bool {
        !self.instance.is_empty()
    }

    /// Resolves the single live instance up front so the preconditions can check it; every command
    /// would bind the same one anyway.
    fn bind_instance(&mut self) {
        if self.instance_is_set() {
            return;
        }
        if let Some(id) = with_pool(|pool: &mut Pool| pool.live_ids().first().copied()) {
            self.instance = id.to_string();
        }
    }

    fn push(&mut self, report: StepReport) {
        self.steps.push(report);
    }

    fn push_error(&mut self, index: usize, key: &str, name: &str, error: &ApiError) {
        self.steps.push(StepReport {
            index,
            name: name.to_owned(),
            key: key.to_owned(),
            status: StepStatus::Error,
            vt_us: 0,
            elapsed_vt_us: 0,
            error: Some(error.to_json()),
        });
    }

    fn fail_step(&mut self, step: &Step, error: &ApiError, status: StepStatus) {
        let vt = self.steps.last().map_or(0, |s| s.vt_us);
        self.push(StepReport {
            index: self.steps.len(),
            name: step.name.clone(),
            key: step.key.clone(),
            status,
            vt_us: vt,
            elapsed_vt_us: 0,
            error: Some(error.to_json()),
        });
        if !step.continue_on_error {
            self.stopped = self.stop_on_failure;
        }
    }
}

fn waits(name: &str) -> bool {
    name == "run"
}

/// Read from the registry so the runner never guesses.
fn needs_instance(name: &str) -> bool {
    crate::registry::find(name).is_some_and(|spec| spec.annotations.needs_instance)
}

fn scalar(value: &Yaml) -> Result<serde_json::Value, ApiError> {
    match value {
        Yaml::Map(_) | Yaml::Seq(_) | Yaml::Null => Err(ApiError::new(
            E_USAGE,
            format!("this step takes a scalar, saw {}", value.kind()),
        )),
        other => Ok(other.to_json()),
    }
}

/// An unset name is left as written, so a missing `set` shows up in the command's own refusal.
#[must_use]
pub fn substitute(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
            Some(end) => {
                let name = &after[..end];
                match vars.get(name) {
                    Some(value) => out.push_str(value),
                    None => {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

crate::matchers::str_enum! {
    pub enum ScenarioOp {
        Run = "run",
        /// Checks every step key without running anything.
        Validate = "validate",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenarioArgs {
    pub op: ScenarioOp,
    /// `*`, `?` and `**` are expanded by this command.
    pub file: Option<String>,
    /// For a caller that has no file.
    pub inline: Option<String>,
    pub instance: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub stop_on_failure: bool,
    pub wall_budget_ms: u64,
    pub jobs: u64,
    /// `None` writes none.
    pub junit: Option<String>,
    /// Changes no verdict, only the exit code a `pass_with_caveats` carries: 7 under `--strict`, 10
    /// without.
    pub strict: bool,
}

impl Default for ScenarioArgs {
    fn default() -> ScenarioArgs {
        ScenarioArgs {
            op: ScenarioOp::Run,
            file: None,
            inline: None,
            instance: None,
            vars: BTreeMap::new(),
            stop_on_failure: true,
            wall_budget_ms: WALL_BUDGET_MS,
            jobs: 1,
            junit: None,
            strict: false,
        }
    }
}

impl ScenarioArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<ScenarioArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "op",
                "file",
                "inline",
                "instance",
                "vars",
                "stop_on_failure",
                "wall_budget_ms",
                "jobs",
                "junit",
                "strict",
            ],
        )?;
        let op = match opt_str(args, "op")? {
            None => ScenarioOp::Run,
            Some(text) => ScenarioOp::parse(text)
                .ok_or_else(|| usage("op", &format!("`{text}` is not one of run, validate")))?,
        };
        let file = opt_str(args, "file")?.map(str::to_owned);
        let inline = opt_str(args, "inline")?.map(str::to_owned);
        if file.is_some() == inline.is_some() {
            return Err(usage("file", "exactly one of `file` and `inline` is given"));
        }
        let mut vars = BTreeMap::new();
        match args.get("vars") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    let text = match value {
                        serde_json::Value::String(text) => text.clone(),
                        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                            value.to_string()
                        }
                        _ => return Err(usage("vars", "values are strings, numbers or booleans")),
                    };
                    vars.insert(key.clone(), text);
                }
            }
            Some(_) => return Err(usage("vars", "expected an object")),
        }
        let jobs = opt_u64(args, "jobs")?.unwrap_or(1);
        if jobs == 0 || jobs > JOBS_MAX {
            return Err(usage("jobs", &format!("expected 1..={JOBS_MAX}")));
        }
        let junit = opt_str(args, "junit")?.map(str::to_owned);
        if let Some(path) = &junit {
            crate::output::check_artifact_path(path)
                .map_err(|err| usage("junit", &format!("{err}")))?;
        }
        Ok(ScenarioArgs {
            op,
            file,
            inline,
            instance: opt_str(args, "instance")?.map(str::to_owned),
            vars,
            stop_on_failure: opt_bool(args, "stop_on_failure")?.unwrap_or(true),
            wall_budget_ms: opt_u64(args, "wall_budget_ms")?.unwrap_or(WALL_BUDGET_MS),
            jobs,
            junit,
            strict: opt_bool(args, "strict")?.unwrap_or(false),
        })
    }
}

struct Source {
    shown: String,
    text: String,
}

fn sources(args: &ScenarioArgs) -> Result<Vec<Source>, ApiError> {
    if let Some(text) = &args.inline {
        return Ok(vec![Source {
            shown: String::new(),
            text: text.clone(),
        }]);
    }
    let pattern = args
        .file
        .clone()
        .ok_or_else(|| usage("file", "is required when `inline` is absent"))?;
    let io = io()?;
    let paths = if pattern.contains(['*', '?']) {
        let root = glob_root(&pattern);
        let listed = (io.list_files)(&root).map_err(|err| {
            let root = shown_path(&io, &root);
            ApiError::new(E_STATE, format!("`{root}` cannot be listed: {err}"))
        })?;
        let hits = expand(&pattern, &listed);
        if hits.is_empty() {
            let pattern = shown_path(&io, &pattern);
            return Err(
                ApiError::new(E_USAGE, format!("`{pattern}` matched no scenario file")).with_hint(
                    "quote the pattern so this command expands it rather than the shell",
                ),
            );
        }
        hits
    } else {
        vec![pattern]
    };
    paths
        .into_iter()
        .map(|path| {
            let shown = shown_path(&io, &path);
            let text = (io.read_text)(&path).map_err(|err| {
                ApiError::new(E_STATE, format!("`{shown}` cannot be read: {err}"))
            })?;
            Ok(Source { shown, text })
        })
        .collect()
}

pub fn scenario_on(args: &ScenarioArgs) -> Result<Output, ApiError> {
    let sources = sources(args)?;
    let aliases = registry_aliases();
    let mut reports: Vec<Option<Report>> = Vec::with_capacity(sources.len());
    let mut batch: Vec<(usize, BatchItem)> = Vec::new();
    let runner = batch_runner()
        .filter(|_| args.op == ScenarioOp::Run && args.instance.is_none() && args.jobs > 1);
    // A batch runs on one host budget, so what the first scenario did not spend is left for the
    // next.
    let mut left_ms = args.wall_budget_ms;
    for source in &sources {
        let parsed = Scenario::parse(&source.text).and_then(|scenario| {
            scenario.check_steps(&aliases)?;
            Ok(scenario)
        });
        let scenario = match parsed {
            Ok(scenario) => scenario,
            Err(err) => {
                reports.push(Some(Report::unreadable(&source.shown, &err)));
                continue;
            }
        };
        if args.op == ScenarioOp::Validate {
            reports.push(Some(Report {
                name: scenario.name.clone(),
                source: source.shown.clone(),
                instance: String::new(),
                status: RunStatus::Pass,
                caveats: Vec::new(),
                vt_us: 0,
                steps: Vec::new(),
            }));
            continue;
        }
        // The file's own `defaults.wall_budget` can only lower the caller's budget.
        let budget_ms = match &scenario.defaults.wall_budget {
            None => left_ms,
            Some(text) => {
                let parsed = crate::matchers::parse_duration(text)
                    .map_err(|err| usage("defaults.wall_budget", &err.message))?;
                left_ms.min(parsed.as_us() / 1_000)
            }
        };
        let options = RunOptions {
            instance: args.instance.clone().unwrap_or_default(),
            vars: args.vars.clone(),
            stop_on_failure: args.stop_on_failure,
            source: source.shown.clone(),
            wall_budget_ms: budget_ms,
            started_with: Vec::new(),
        };
        if runner.is_some() {
            // In parallel every scenario starts on the whole batch budget.
            batch.push((reports.len(), BatchItem { scenario, options }));
            reports.push(None);
            continue;
        }
        let started = with_pool(|pool: &mut Pool| pool.host_clock().map(|now| now()));
        let report = run_scenario(&scenario, &options);
        if let (Some(started), Some(now)) = (
            started,
            with_pool(|pool: &mut Pool| pool.host_clock().map(|now| now())),
        ) {
            left_ms = left_ms.saturating_sub(now.saturating_sub(started));
        }
        reports.push(Some(report));
    }
    let mut detail = None;
    if let Some(runner) = runner
        && !batch.is_empty()
    {
        let (slots, items): (Vec<usize>, Vec<BatchItem>) = batch.into_iter().unzip();
        let jobs = usize::try_from(args.jobs).unwrap_or(usize::MAX);
        let outcome = runner(items.clone(), jobs)?;
        let mut ran = outcome.reports.into_iter();
        for (slot, item) in slots.into_iter().zip(items) {
            reports[slot] = Some(ran.next().unwrap_or_else(|| {
                Report::not_run(
                    &item.scenario.name,
                    &item.options.source,
                    &ApiError::new(E_INTERNAL, "the batch runner returned no report for it"),
                )
            }));
        }
        detail = Some(outcome.detail);
    }
    let reports: Vec<Report> = reports.into_iter().flatten().collect();
    let mut out = output(args, &reports)?;
    if let Some(detail) = detail {
        out.json["batch"] = detail;
    }
    Ok(out)
}

/// Writes one JUnit file for the whole batch when the caller asked for one.
fn output(args: &ScenarioArgs, reports: &[Report]) -> Result<Output, ApiError> {
    let junit_path = match (&args.junit, args.op) {
        (Some(path), ScenarioOp::Run) => {
            let io = io()?;
            let written = (io.write_text)(path, &junit(reports)).map_err(|err| {
                ApiError::new(
                    E_STATE,
                    format!("the JUnit report could not be written: {err}"),
                )
            })?;
            Some(written)
        }
        _ => None,
    };
    let status = reports
        .iter()
        .map(|report| report.status)
        .max_by_key(|status| status.severity())
        .unwrap_or(RunStatus::Pass);
    let strictness = if args.strict {
        Strictness::Strict
    } else {
        Strictness::Lenient
    };
    let caveats: Vec<crate::receipt::Caveat> = reports
        .iter()
        .flat_map(|report| report.caveats.iter().cloned())
        .collect();
    let receipt = with_pool(|pool: &mut Pool| {
        pool.live_ids()
            .first()
            .and_then(|id| pool.session_mut(*id))
            .map_or_else(crate::receipt::Receipt::default, |session| {
                session.receipt()
            })
    });
    let json = serde_json::json!({
        "op": args.op.as_str(),
        // The word `pemu-cli` reads to pick an exit code.
        "result": if status.assertions_hold() { "pass" } else { "fail" },
        "status": status.as_str(),
        "exit_code": crate::scenario::exit_code(status, &caveats, strictness),
        "caveats": caveats.len(),
        "jobs": args.jobs,
        "wall_budget_ms": args.wall_budget_ms,
        "junit_path": junit_path,
        "scenarios": reports.iter().map(|report| report.to_json(strictness)).collect::<Vec<_>>(),
    });
    let mut text = String::new();
    for report in reports {
        let _ = writeln!(text, "{}", report.to_text());
    }
    if reports.len() > 1 {
        let passed = reports
            .iter()
            .filter(|report| report.status.assertions_hold())
            .count();
        let _ = writeln!(
            text,
            "{} of {} scenario(s) passed, jobs={}",
            passed,
            reports.len(),
            args.jobs
        );
    }
    if let Some(path) = &junit_path {
        let _ = write!(text, "junit: {path}");
    }
    Ok(Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT))
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`scenario` arguments; exactly one of `file` and `inline`.",
        "properties": {
            "op": { "type": "string", "enum": ["run", "validate"], "description": "What to do (run)." },
            "file": { "type": "string", "description": "Scenario file or glob; quote it." },
            "inline": { "type": "string", "description": "Scenario YAML text." },
            "instance": instance_schema(),
            "vars": { "type": "object", "additionalProperties": { "type": ["string", "number", "boolean"] }, "description": "`${var}` values." },
            "stop_on_failure": { "type": "boolean", "description": "Stop at the first failing step (true)." },
            "wall_budget_ms": { "type": "integer", "minimum": 1000, "description": "Host budget in ms (600000)." },
            "jobs": { "type": "integer", "minimum": 1, "maximum": JOBS_MAX, "description": "Instances the host may run in parallel (1)." },
            "junit": { "type": "string", "description": "JUnit path, relative and forward-slashed." },
            "strict": { "type": "boolean", "description": "Strict exit codes: a caveat exits 7 rather than 10 (false)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "op": { "type": "string" },
            "result": { "type": "string", "enum": ["pass", "fail"] },
            "status": { "type": "string", "enum": ["pass", "pass_with_caveats", "fail", "error", "wall_budget"] },
            "exit_code": { "type": "integer" },
            "caveats": { "type": "array", "items": { "type": "object" } },
            "jobs": { "type": "integer" },
            "wall_budget_ms": { "type": "integer" },
            "junit_path": { "type": ["string", "null"] },
            "batch": { "type": "object", "description": "A parallel batch: `forked_from` lists one template per distinct boot, `parallel` is how many ran at once (capped by the daemon's room, so it varies by host), and `wall_budget` is `per_scenario`." },
            "scenarios": { "type": "array", "items": { "type": "object" } }
        }
    })
}

/// Run or validate a scenario file, and write the JUnit report.
#[command(
    api_crate = crate,
    name = "scenario",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time),
    cli(positional = ["file"]),
    errors(E_USAGE, E_STATE, E_LEASE, E_TIMEOUT, E_WALL_BUDGET, E_INTERNAL),
    example(
        title = "Run one scenario file",
        args = r#"{"file":"tests/scenarios/official-menu-smoke.yaml"}"#,
    ),
    example(
        title = "Run a whole suite and write one JUnit file",
        args = r#"{"file":"tests/scenarios/*.yaml","jobs":8,"junit":"junit.xml"}"#,
    ),
    example(
        title = "Check that a scenario reads without running it",
        args = r#"{"op":"validate","file":"tests/scenarios/*.yaml"}"#,
    ),
    example(
        title = "Run a scenario written inline",
        args = "{\"inline\":\"schema: passportsim/scenario@1\\nname: smoke\\nsteps:\\n  - delay: 1ms\\n\"}",
    ),
)]
pub fn scenario(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = ScenarioArgs::from_json(&args)?;
    scenario_on(&args)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use pemu_core::input::InputEvent;

    use crate::commands::start::{Boot, StartArgs};
    use crate::instance::Lifecycle;
    use pemu_core::time::VTime;

    /// A test that installs into the shared process state holds this lock for its whole body.
    static WORLD: StdMutex<()> = StdMutex::new(());

    fn world() -> std::sync::MutexGuard<'static, ()> {
        match WORLD.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    static FILES: StdMutex<Option<BTreeMap<String, String>>> = StdMutex::new(None);

    fn files<R>(f: impl FnOnce(&mut BTreeMap<String, String>) -> R) -> R {
        let mut guard = FILES.lock().expect("the file map is never poisoned");
        f(guard.get_or_insert_with(BTreeMap::new))
    }

    fn test_read(path: &str) -> Result<String, String> {
        files(|map| map.get(path).cloned()).ok_or_else(|| format!("no file `{path}`"))
    }

    fn test_list(root: &str) -> Result<Vec<String>, String> {
        Ok(files(|map| {
            map.keys()
                .filter(|path| root.is_empty() || path.starts_with(&format!("{root}/")))
                .cloned()
                .collect()
        }))
    }

    /// A path under `/abs/root/` loses that prefix.
    fn test_display(path: &str) -> String {
        path.strip_prefix(TEST_ROOT).unwrap_or(path).to_owned()
    }

    const TEST_ROOT: &str = "/abs/root/";

    fn test_write(path: &str, text: &str) -> Result<String, String> {
        files(|map| map.insert(path.to_owned(), text.to_owned()));
        Ok(path.to_owned())
    }

    /// The journal of the instance [`world_with_instance`] installed. `JournalMachine` leaves
    /// `journal_len` at its default, so it says nothing.
    static JOURNAL: StdMutex<Option<Arc<StdMutex<Vec<InputEvent>>>>> = StdMutex::new(None);

    fn world_with_instance() -> std::sync::MutexGuard<'static, ()> {
        let guard = world();
        files(BTreeMap::clear);
        set_io(ScenarioIo {
            read_text: test_read,
            list_files: test_list,
            write_text: test_write,
            display_path: test_display,
        });
        with_pool(|pool: &mut Pool| {
            for id in pool.live_ids() {
                let _ = pool.destroy(id);
            }
            let (machine, journal) = crate::commands::env::tests::JournalMachine::new();
            *JOURNAL
                .lock()
                .expect("the journal handle is never poisoned") = Some(journal);
            let args = StartArgs {
                fw: "official".to_owned(),
                boot: Boot::None,
                ..StartArgs::default()
            };
            let id = pool.attach(&args, Box::new(machine));
            pool.table_mut()
                .get_mut(id)
                .expect("just created")
                .transition(Lifecycle::Paused, VTime(0))
                .expect("starting -> paused");
        });
        guard
    }

    fn run_inline(text: &str) -> Report {
        let scenario = Scenario::parse(text).expect("the scenario reads");
        run_scenario(&scenario, &RunOptions::default())
    }

    #[test]
    fn an_image_that_is_not_the_instances_firmware_stops_the_scenario() {
        let _world = world_with_instance();
        let report = run_inline(
            "schema: passportsim/scenario@1\n\
             name: wrong-image\n\
             image: some-other-build\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Error, "{report:?}");
        let error = report.steps[0].error.as_ref().expect("a refusal");
        assert_eq!(error["code"], "E_STATE");
        assert!(
            error["message"]
                .as_str()
                .unwrap_or_default()
                .contains("started from `official`"),
            "{error}"
        );
        assert_eq!(
            report.steps[1].status,
            StepStatus::Skipped,
            "no step runs under conditions the scenario did not get: {report:?}"
        );
    }

    #[test]
    fn the_instances_own_firmware_satisfies_the_image_key() {
        let _world = world_with_instance();
        let report = run_inline(
            "schema: passportsim/scenario@1\n\
             name: right-image\n\
             image: official\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{report:?}");
    }

    #[test]
    fn setup_world_state_reaches_the_instance_as_an_env_call() {
        let _world = world_with_instance();
        let report = run_inline(
            "schema: passportsim/scenario@1\n\
             name: setup-applied\n\
             setup: {usb: unplugged, battery: {mv: 3300, soc: 20}}\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{report:?}");
        let journal = JOURNAL
            .lock()
            .expect("the journal handle is never poisoned")
            .as_ref()
            .expect("world_with_instance kept one")
            .lock()
            .expect("the journal is never poisoned")
            .clone();
        assert!(
            journal
                .iter()
                .any(|event| matches!(event, InputEvent::UsbCable { plugged: false })),
            "`setup.usb: unplugged` did not reach the journal: {journal:?}"
        );
        assert!(
            journal
                .iter()
                .any(|event| matches!(event, InputEvent::Battery(set) if set.mv == Some(3300))),
            "`setup.battery` did not reach the journal: {journal:?}"
        );
    }

    #[test]
    fn a_batch_boot_carries_the_setup_keys_that_change_a_boot() {
        let parse = |setup: &str| {
            Scenario::parse(&format!(
                "schema: passportsim/scenario@1\nname: s\nimage: official\n{setup}steps:\n  - delay: 1ms\n"
            ))
            .expect("the scenario reads")
        };
        let plain = batch_start(&parse("")).expect("a plain file forks");
        assert_eq!(plain.args, serde_json::json!({"fw": "official"}));
        assert!(plain.keys.is_empty());
        let unplugged = batch_start(&parse(
            "setup: {usb: unplugged, battery: {mv: 3300}, seed: 7}\n",
        ))
        .expect("forks");
        assert_eq!(
            unplugged.args,
            serde_json::json!({"fw": "official", "seed": 7, "usb": "unplugged"})
        );
        assert_eq!(
            unplugged.keys,
            ["seed", "usb"],
            "`battery` stays an `env` call"
        );
        let same = batch_start(&parse("setup: {seed: 7, usb: unplugged}\n")).expect("forks");
        assert_eq!(
            same.args.to_string(),
            unplugged.args.to_string(),
            "the same boot written in another order is the same template"
        );
        assert_ne!(
            plain.args, unplugged.args,
            "another U-state is another boot"
        );
        for (setup, expect) in [
            ("setup: {power: off}\n", "power: off"),
            ("setup: {flash_seed: nvs.bin}\n", "flash_seed"),
            ("setup: {board: rev2}\n", "board"),
        ] {
            let refused = batch_start(&parse(setup)).expect_err(setup);
            assert_eq!(refused.code, E_USAGE);
            assert!(refused.message.contains(expect), "{refused}");
        }
        let unnamed =
            Scenario::parse("schema: passportsim/scenario@1\nname: s\nsteps:\n  - delay: 1ms\n")
                .expect("reads");
        assert!(batch_start(&unnamed).is_err(), "no image, no template");
    }

    #[test]
    fn a_setup_key_the_instance_was_started_with_is_not_applied_again() {
        let _world = world_with_instance();
        let scenario = Scenario::parse(
            "schema: passportsim/scenario@1\n\
             name: started\n\
             setup: {usb: unplugged, seed: 7}\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        )
        .expect("reads");
        let report = run_scenario(
            &scenario,
            &RunOptions {
                started_with: vec!["usb".to_owned(), "seed".to_owned()],
                ..RunOptions::default()
            },
        );
        assert_eq!(report.status, RunStatus::Pass, "{report:?}");
        let journal = JOURNAL
            .lock()
            .expect("the journal handle is never poisoned")
            .as_ref()
            .expect("world_with_instance kept one")
            .lock()
            .expect("the journal is never poisoned")
            .clone();
        assert!(
            !journal
                .iter()
                .any(|event| matches!(event, InputEvent::UsbCable { .. })),
            "`usb` was applied after the boot: {journal:?}"
        );
    }

    #[test]
    fn a_batch_runner_refusal_refuses_the_command() {
        let _world = world_with_instance();
        files(|map| {
            map.insert("tests/scenarios/one.yaml".to_owned(), ONE.to_owned());
            map.insert("tests/scenarios/two.yaml".to_owned(), TWO.to_owned());
        });
        fn full(_: Vec<BatchItem>, _: usize) -> Result<BatchOutcome, ApiError> {
            Err(ApiError::new(E_STATE, "no room for a single fork"))
        }
        set_batch_runner(Some(full));
        let refused = scenario_on(&args(serde_json::json!({
            "file": "tests/scenarios/*.yaml",
            "jobs": 4
        })));
        set_batch_runner(None);
        let refused = refused.expect_err("the batch is refused as a whole");
        assert_eq!(refused.message, "no room for a single fork");
    }

    #[test]
    fn a_start_time_setup_key_is_refused_by_name() {
        let _world = world_with_instance();
        for (key, expect) in [
            ("seed: 7", "instance"),
            ("mode: deterministic", "instance"),
            ("hints: ui-hints.yaml", "ui-hint vocabulary"),
            ("wifi: {aps: []}", "wifi.ap"),
            ("strict: false", "--strict"),
        ] {
            let report = run_inline(&format!(
                "schema: passportsim/scenario@1\n\
                 name: refused\n\
                 setup: {{{key}}}\n\
                 steps:\n\
                 \x20 - delay: 1ms\n"
            ));
            assert_eq!(report.status, RunStatus::Error, "`{key}`: {report:?}");
            let error = report.steps[0].error.as_ref().expect("a refusal");
            let message = error["message"].as_str().unwrap_or_default().to_owned()
                + error["hint"].as_str().unwrap_or_default();
            assert!(message.contains(expect), "`{key}` said: {error}");
        }
    }

    #[test]
    fn setup_power_is_checked_against_the_instances_lifecycle() {
        let _world = world_with_instance();
        let on = run_inline(
            "schema: passportsim/scenario@1\n\
             name: power-on\n\
             setup: {power: on}\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        );
        assert_eq!(on.status, RunStatus::Pass, "{on:?}");

        with_pool(|pool: &mut Pool| {
            let id = pool.live_ids()[0];
            pool.table_mut()
                .get_mut(id)
                .expect("live")
                .transition(Lifecycle::PoweredOff, VTime(0))
                .expect("paused -> powered_off");
        });
        let off = run_inline(
            "schema: passportsim/scenario@1\n\
             name: power-on\n\
             setup: {power: on}\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        );
        assert_eq!(off.status, RunStatus::Error, "{off:?}");
        assert!(
            off.steps[0].error.as_ref().expect("a refusal")["message"]
                .as_str()
                .unwrap_or_default()
                .contains("powered_off"),
            "{:?}",
            off.steps[0].error
        );
    }

    /// The runner asks [`crate::receipt::Receipt::caveats`] through [`receipt_caveats`]; this pins
    /// both ends of that wiring.
    #[test]
    fn the_verdict_is_the_receipts_verdict_not_a_step_count() {
        let _world = world_with_instance();
        let report = run_inline(
            "schema: passportsim/scenario@1\n\
             name: verdict\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        );
        let live = with_pool(|pool: &mut Pool| {
            let id = pool.live_ids()[0];
            pool.session_mut(id)
                .expect("the instance")
                .receipt()
                .caveats()
        });
        assert_eq!(
            report.caveats, live,
            "the report carries the instance's own caveats"
        );
        assert_eq!(
            report.status,
            if live.is_empty() {
                RunStatus::Pass
            } else {
                RunStatus::PassWithCaveats
            },
            "the status follows the receipt: {report:?}"
        );
        assert!(
            live.is_empty(),
            "`Session::receipt` gained caveats: the assertion above now exercises the \
             pass_with_caveats path end to end, and the note on it can go"
        );
    }

    #[test]
    fn the_output_carries_the_result_word() {
        let _world = world_with_instance();
        files(|map| {
            map.insert(
                "s/broken.yaml".to_owned(),
                "schema: passportsim/scenario@1\nname: broken\nsteps:\n  - delay: nope\n"
                    .to_owned(),
            );
        });
        let args = ScenarioArgs {
            file: Some("s/broken.yaml".to_owned()),
            ..ScenarioArgs::default()
        };
        let out = scenario_on(&args).expect("the batch reports rather than refusing");
        assert_eq!(out.json["result"], "fail", "{}", out.text);
        assert_eq!(out.json["status"], "error", "{}", out.text);
        assert_eq!(
            out.json["exit_code"], 8,
            "a step that could not run is INFRA"
        );
    }

    #[test]
    fn a_spent_host_budget_stops_the_scenario_and_reports_wall_budget() {
        /// Jumps a minute per read, so any budget is spent at the first check.
        fn jumpy() -> u64 {
            static NOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            NOW.fetch_add(60_000, std::sync::atomic::Ordering::Relaxed)
        }

        let _world = world_with_instance();
        with_pool(|pool: &mut Pool| pool.set_host_clock(Some(jumpy)));
        let scenario = Scenario::parse(
            "schema: passportsim/scenario@1\n\
             name: out-of-time\n\
             steps:\n\
             \x20 - delay: 1ms\n\
             \x20 - delay: 1ms\n",
        )
        .expect("the scenario reads");
        let report = run_scenario(
            &scenario,
            &RunOptions {
                wall_budget_ms: 1,
                ..RunOptions::default()
            },
        );
        with_pool(|pool: &mut Pool| pool.set_host_clock(None));
        assert_eq!(report.status, RunStatus::WallBudget, "{report:?}");
        assert_eq!(
            crate::scenario::exit_code(report.status, &report.caveats, Strictness::Lenient),
            6,
            "the host budget exits 6 WALL_TIMEOUT"
        );
        assert_eq!(
            report.steps[0].error.as_ref().expect("a refusal")["code"],
            "E_WALL_BUDGET"
        );
    }

    #[test]
    fn with_no_host_clock_the_scenario_budget_is_not_enforced() {
        let _world = world_with_instance();
        with_pool(|pool: &mut Pool| pool.set_host_clock(None));
        let scenario = Scenario::parse(
            "schema: passportsim/scenario@1\n\
             name: no-clock\n\
             steps:\n\
             \x20 - delay: 1ms\n",
        )
        .expect("the scenario reads");
        let report = run_scenario(
            &scenario,
            &RunOptions {
                wall_budget_ms: 1,
                ..RunOptions::default()
            },
        );
        assert_eq!(report.status, RunStatus::Pass, "{report:?}");
    }

    #[test]
    fn a_star_stays_inside_one_path_segment_and_a_double_star_crosses_them() {
        assert!(glob_matches(
            "tests/scenarios/*.yaml",
            "tests/scenarios/a.yaml"
        ));
        assert!(!glob_matches(
            "tests/scenarios/*.yaml",
            "tests/scenarios/deep/a.yaml"
        ));
        assert!(glob_matches(
            "tests/scenarios/**/*.yaml",
            "tests/scenarios/deep/a.yaml"
        ));
        assert!(glob_matches("tests/**/a.yaml", "tests/a.yaml"));
        assert!(glob_matches(
            "tests/scenarios/?.yaml",
            "tests/scenarios/a.yaml"
        ));
        assert!(!glob_matches(
            "tests/scenarios/?.yaml",
            "tests/scenarios/ab.yaml"
        ));
        assert!(!glob_matches(
            "tests/scenarios/*.yaml",
            "tests/scenarios/a.yml"
        ));
    }

    #[test]
    fn an_expansion_is_sorted_and_a_plain_path_selects_itself() {
        let listed = vec![
            "tests/scenarios/b.yaml".to_owned(),
            "tests/scenarios/a.yaml".to_owned(),
            "tests/scenarios/notes.md".to_owned(),
        ];
        assert_eq!(
            expand("tests/scenarios/*.yaml", &listed),
            vec![
                "tests/scenarios/a.yaml".to_owned(),
                "tests/scenarios/b.yaml".to_owned()
            ]
        );
        assert_eq!(
            expand("tests/scenarios/only.yaml", &[]),
            vec!["tests/scenarios/only.yaml".to_owned()]
        );
        assert_eq!(glob_root("tests/scenarios/*.yaml"), "tests/scenarios");
        assert_eq!(glob_root("**/a.yaml"), "");
        assert_eq!(glob_root("tests/scenarios/a.yaml"), "tests/scenarios");
    }

    #[test]
    fn a_matcher_mapping_becomes_the_matcher_text_run_takes() {
        for (yaml, text) in [
            (
                "{serial: {re: \"pk_app: ready\"}}",
                "serial:/pk_app: ready/",
            ),
            (
                "{serial: {re: ready, stream: uart0, from: start}}",
                "serial:/ready/,uart0,from=start",
            ),
            ("{serial: {literal: ready}}", "serial:\"ready\""),
            (
                "{log: {tag: pk_app, level: I, re: ready}}",
                "log:pk_app:I:/ready/",
            ),
            ("{log: {re: ready}}", "log:*:*:/ready/"),
            ("{ui: changed}", "ui:changed"),
            ("{ui: {label: Button}}", "ui:label=\"Button\""),
            ("{event: panic}", "event:panic"),
            (
                "{symbol: {name: enter_menu, hits: 3}}",
                "symbol:enter_menu:hits=3",
            ),
            (
                "{any: [{event: panic}, {serial: {re: ready}}]}",
                "any(event:panic,serial:/ready/)",
            ),
        ] {
            let value = Yaml::parse(yaml).expect("the fragment reads");
            assert_eq!(matcher_text(&value).expect(yaml), text, "{yaml}");
        }
    }

    /// Otherwise the translation is just a different way of being wrong.
    #[test]
    fn every_translated_matcher_compiles_in_the_arch_grammar() {
        for yaml in [
            "{serial: {re: ready}}",
            "{serial: {re: ready, from: now}}",
            "{log: {tag: pk_app, level: E, re: failed}}",
            "{ui: changed}",
            "{ui: {label: Button}}",
            "{event: reset}",
            "{symbol: {name: enter_menu}}",
            "{all: [{event: reset}, {event: panic}]}",
        ] {
            let value = Yaml::parse(yaml).expect("reads");
            let text = matcher_text(&value).expect(yaml);
            crate::matchers::Matcher::parse(&text)
                .unwrap_or_else(|e| panic!("{yaml} -> {text}: {e:?}"));
        }
    }

    #[test]
    fn a_matcher_key_with_no_arch_row_is_refused_by_name() {
        let value = Yaml::parse("{pixel: {x: 1}}").expect("reads");
        let error = matcher_text(&value).expect_err("there is no `pixel` row");
        assert!(
            error.message.contains("`pixel` is not a matcher"),
            "{error}"
        );
    }

    fn run(text: &str) -> Report {
        let scenario = Scenario::parse(text).expect("the scenario reads");
        scenario
            .check_steps(&registry_aliases())
            .expect("every step key is known");
        run_scenario(
            &scenario,
            &RunOptions {
                stop_on_failure: true,
                ..RunOptions::default()
            },
        )
    }

    #[test]
    fn a_scalar_step_value_fills_the_first_positional_argument() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: press\nsteps:\n  - press: down\n  - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{}", report.to_text());
        assert_eq!(report.steps.len(), 2);
        let live = with_pool(|pool: &mut Pool| {
            pool.live_ids()
                .first()
                .map(ToString::to_string)
                .unwrap_or_default()
        });
        assert_eq!(
            report.instance, live,
            "the runner reports the instance the steps bound"
        );
        assert!(report.steps[0].vt_us > 0, "a click advances virtual time");
    }

    #[test]
    fn a_mapping_step_value_is_the_argument_object() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: verbatim\nsteps:\n  - press: {button: ok, action: press}\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{}", report.to_text());
    }

    #[test]
    fn a_repeat_runs_its_body_once_per_pass() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: loop\nsteps:\n  - repeat:\n      times: 3\n      steps:\n        - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{}", report.to_text());
        assert_eq!(report.steps.len(), 3);
        assert!(
            report.steps.iter().all(|step| step.key == "delay"),
            "{:?}",
            report.steps
        );
    }

    #[test]
    fn a_set_step_supplies_a_variable_to_a_later_step() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: vars\nsteps:\n  - set: {hold: 2ms}\n  - delay: ${hold}\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{}", report.to_text());
        assert_eq!(report.steps[1].elapsed_vt_us, 2000);
        let mut vars = BTreeMap::new();
        vars.insert("a".to_owned(), "1".to_owned());
        assert_eq!(substitute("x${a}y", &vars), "x1y");
        assert_eq!(
            substitute("x${b}y", &vars),
            "x${b}y",
            "an unset name stays written, so the command refuses it rather than seeing nothing"
        );
    }

    #[test]
    fn a_failing_step_stops_the_scenario_and_the_rest_is_skipped() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: stop\ndefaults: {timeout: 1ms}\nsteps:\n  - wait: {serial: {re: never}}\n  - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Fail);
        assert_eq!(report.steps[0].status, StepStatus::Fail);
        assert_eq!(
            report.steps[0]
                .error
                .as_ref()
                .and_then(|e| e["code"].as_str()),
            Some("E_TIMEOUT"),
            "a timeout is the wait's own assertion failing"
        );
        assert_eq!(report.steps[1].status, StepStatus::Skipped);
        assert_eq!(report.failed_step(), Some(0));
    }

    /// `false` is the default.
    #[test]
    fn continue_on_error_lets_the_scenario_go_on() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: go-on\ndefaults: {timeout: 1ms}\nsteps:\n  - wait: {serial: {re: never}}\n    continue_on_error: true\n  - delay: 1ms\n",
        );
        assert_eq!(report.status, RunStatus::Fail);
        assert_eq!(report.steps[1].status, StepStatus::Pass);
    }

    #[test]
    fn expect_not_passes_while_the_pattern_does_not_appear() {
        let _world = world_with_instance();
        let report = run(
            "schema: passportsim/scenario@1\nname: negative\nsteps:\n  - expect_not: {serial: {re: Guru}, for: 2ms}\n",
        );
        assert_eq!(report.status, RunStatus::Pass, "{}", report.to_text());
        assert_eq!(report.steps[0].elapsed_vt_us, 2000);
    }

    #[test]
    fn ui_expect_asserts_on_the_walked_tree() {
        let _world = world_with_instance();
        crate::commands::inspect::set_introspectors(crate::commands::inspect::tests::SCRIPTED);
        let report = run(
            "schema: passportsim/scenario@1\nname: ui\nsteps:\n  - ui.expect: {contains: [{text: Display, class: label}]}\n  - ui.expect: {contains: [{text: Button}]}\n",
        );
        assert_eq!(
            report.steps[0].status,
            StepStatus::Pass,
            "{}",
            report.to_text()
        );
        assert_eq!(
            report.steps[1].status,
            StepStatus::Fail,
            "{}",
            report.to_text()
        );
        let error = report.steps[1]
            .error
            .as_ref()
            .expect("the failure is described");
        assert_eq!(error["code"], "E_ASSERT");
        let message = error["message"].as_str().expect("text");
        assert!(message.contains("text \"Button\""), "{message}");
        assert!(
            message.contains("ui_rev 2"),
            "each step took a revision: {message}"
        );
    }

    #[test]
    fn ui_expect_fails_when_no_safe_point_comes_within_the_step_timeout() {
        let _world = world_with_instance();
        crate::commands::inspect::set_introspectors(crate::commands::inspect::tests::SCRIPTED);
        super::super::ui::set_safe_point(Some(super::super::ui::tests::scripted_safe_point));
        with_pool(|pool: &mut Pool| {
            for id in pool.live_ids() {
                pool.session_mut(id).expect("live").fw = "settle-never".to_owned();
            }
        });
        let report = run(
            "schema: passportsim/scenario@1\nname: ui\nsteps:\n  - ui.expect: {contains: [{text: Display}]}\n    timeout: 30ms\n",
        );
        assert_eq!(
            report.steps[0].status,
            StepStatus::Fail,
            "{}",
            report.to_text()
        );
        let error = report.steps[0].error.as_ref().expect("described");
        assert_eq!(error["code"], "E_TIMEOUT");
    }

    fn args(json: serde_json::Value) -> ScenarioArgs {
        ScenarioArgs::from_json(&json).expect("inside the schema")
    }

    const ONE: &str = "schema: passportsim/scenario@1\nname: one\nsteps:\n  - delay: 1ms\n";
    const TWO: &str = "schema: passportsim/scenario@1\nname: two\nsteps:\n  - delay: 2ms\n";

    #[test]
    fn a_quoted_glob_runs_the_whole_suite_into_one_junit_file() {
        let _world = world_with_instance();
        files(|map| {
            map.insert("tests/scenarios/one.yaml".to_owned(), ONE.to_owned());
            map.insert("tests/scenarios/two.yaml".to_owned(), TWO.to_owned());
            map.insert(
                "tests/scenarios/notes.md".to_owned(),
                "not a scenario".to_owned(),
            );
        });
        let out = scenario_on(&args(serde_json::json!({
            "file": "tests/scenarios/*.yaml",
            "jobs": 8,
            "junit": "junit.xml"
        })))
        .expect("the batch runs");
        assert_eq!(out.json["status"], "pass");
        assert_eq!(out.json["exit_code"], 0);
        assert_eq!(out.json["jobs"], 8);
        assert_eq!(out.json["junit_path"], "junit.xml");
        let names: Vec<&str> = out.json["scenarios"]
            .as_array()
            .expect("one entry per file")
            .iter()
            .map(|s| s["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["one", "two"], "`notes.md` is not a match");
        let xml = files(|map| map.get("junit.xml").cloned()).expect("the report was written");
        assert!(xml.contains("<testsuite name=\"one\""), "{xml}");
        assert!(xml.contains("<testsuite name=\"two\""), "{xml}");
        assert_eq!(xml.matches("<testsuites").count(), 1, "one file, one root");
    }

    #[test]
    fn a_forwarded_absolute_pattern_shows_relative_sources() {
        let _world = world_with_instance();
        files(|map| {
            map.insert(
                format!("{TEST_ROOT}tests/scenarios/one.yaml"),
                ONE.to_owned(),
            );
            map.insert(
                format!("{TEST_ROOT}tests/scenarios/broken.yaml"),
                "schema: [\n".to_owned(),
            );
        });
        let out = scenario_on(&args(serde_json::json!({
            "file": format!("{TEST_ROOT}tests/scenarios/*.yaml"),
            "junit": "junit.xml"
        })))
        .expect("the batch answers");
        let text = out.json.to_string();
        assert!(!text.contains(TEST_ROOT), "{text}");
        let sources: Vec<&str> = out.json["scenarios"]
            .as_array()
            .expect("one entry per file")
            .iter()
            .map(|s| s["source"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            sources,
            ["tests/scenarios/broken.yaml", "tests/scenarios/one.yaml"]
        );
        let xml = files(|map| map.get("junit.xml").cloned()).expect("the report was written");
        assert!(!xml.contains(TEST_ROOT), "{xml}");
        assert!(
            xml.contains("<property name=\"source\" value=\"tests/scenarios/one.yaml\"/>"),
            "{xml}"
        );
        let missing = scenario_on(&args(serde_json::json!({
            "file": format!("{TEST_ROOT}tests/scenarios/missing.yaml")
        })))
        .expect_err("an unreadable file is refused");
        assert!(
            missing
                .message
                .starts_with("`tests/scenarios/missing.yaml` cannot be read"),
            "{missing}"
        );
        // A host that answers an absolute form still shows no directory.
        let io = ScenarioIo {
            display_path: str::to_owned,
            ..io().expect("installed")
        };
        assert_eq!(shown_path(&io, "/Users/someone/w/a.yaml"), "a.yaml");
        assert_eq!(shown_path(&io, "C:\\w\\a.yaml"), "a.yaml");
        assert_eq!(shown_path(&io, "tests\\a.yaml"), "tests/a.yaml");
    }

    #[test]
    fn a_glob_that_matches_nothing_says_so_rather_than_passing_silently() {
        let _world = world_with_instance();
        let error = scenario_on(&args(serde_json::json!({"file":"tests/scenarios/*.yaml"})))
            .expect_err("no file matches");
        assert_eq!(error.code, E_USAGE);
        assert!(
            error.message.contains("matched no scenario file"),
            "{error}"
        );
    }

    #[test]
    fn validate_reads_and_checks_without_running_a_step() {
        let _world = world_with_instance();
        files(|map| {
            map.insert("tests/scenarios/one.yaml".to_owned(), ONE.to_owned());
            map.insert(
                "tests/scenarios/bad.yaml".to_owned(),
                "schema: passportsim/scenario@1\nname: bad\nsteps:\n  - pres: ok\n".to_owned(),
            );
        });
        let out = scenario_on(&args(
            serde_json::json!({"op":"validate","file":"tests/scenarios/*.yaml"}),
        ))
        .expect("validate always answers");
        assert_eq!(out.json["status"], "error");
        assert_eq!(out.json["exit_code"], 8);
        assert_eq!(out.json["scenarios"][0]["status"], "error");
        assert!(
            out.json["scenarios"][0]["steps"][0]["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("did you mean `press`"),
            "{}",
            out.json["scenarios"][0]["steps"][0]["error"]["message"]
        );
        assert_eq!(out.json["scenarios"][1]["status"], "pass");
        assert_eq!(out.json["junit_path"], serde_json::Value::Null);
    }

    #[test]
    fn an_inline_scenario_needs_no_file_access() {
        let _world = world_with_instance();
        let out = scenario_on(&args(serde_json::json!({ "inline": ONE }))).expect("runs");
        assert_eq!(out.json["status"], "pass");
        assert_eq!(out.json["scenarios"][0]["source"], "");
    }

    #[test]
    fn every_argument_outside_the_schema_is_usage() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "file": "a.yaml", "inline": "x" }),
            serde_json::json!({ "file": "a.yaml", "op": "rehearse" }),
            serde_json::json!({ "file": "a.yaml", "jobs": 0 }),
            serde_json::json!({ "file": "a.yaml", "jobs": 65 }),
            serde_json::json!({ "file": "a.yaml", "junit": "/tmp/j.xml" }),
            serde_json::json!({ "file": "a.yaml", "vars": { "a": [] } }),
            serde_json::json!({ "file": "a.yaml", "nonsense": 1 }),
        ] {
            assert_eq!(
                ScenarioArgs::from_json(&bad)
                    .expect_err("outside the schema")
                    .code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("scenario").expect("#[command] registered scenario");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            ScenarioArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.advances_time);
        assert!(
            !spec.annotations.needs_instance,
            "a batch creates its own instances"
        );
    }

    /// Embedded rather than read, so a fixture that stops parsing fails the build here rather than
    /// at the first run of the suite.
    const FIXTURES: &[(&str, &str)] = &[
        (
            "official-menu-smoke.yaml",
            include_str!("../../../../tests/scenarios/official-menu-smoke.yaml"),
        ),
        (
            "env-journal.yaml",
            include_str!("../../../../tests/scenarios/env-journal.yaml"),
        ),
        (
            "snapshot-redacted-export.yaml",
            include_str!("../../../../tests/scenarios/snapshot-redacted-export.yaml"),
        ),
    ];

    #[test]
    fn every_committed_fixture_reads_as_a_scenario_this_build_can_run() {
        let aliases = registry_aliases();
        for (file, text) in FIXTURES {
            let scenario = crate::scenario::Scenario::parse(text)
                .unwrap_or_else(|err| panic!("tests/scenarios/{file}: {err}"));
            assert!(!scenario.name.is_empty(), "{file} names itself");
            assert!(!scenario.steps.is_empty(), "{file} has steps");
            scenario
                .check_steps(&aliases)
                .unwrap_or_else(|err| panic!("tests/scenarios/{file}: {err}"));
        }
    }

    #[test]
    fn every_matcher_a_fixture_waits_on_compiles() {
        for (file, text) in FIXTURES {
            let scenario = crate::scenario::Scenario::parse(text).expect("parses");
            for matcher in &scenario.fail_on {
                matcher_text(matcher)
                    .unwrap_or_else(|err| panic!("tests/scenarios/{file} fail_on: {err}"));
            }
            for step in &scenario.steps {
                if step.key == "wait" {
                    let text = matcher_text(&step.value).unwrap_or_else(|err| {
                        panic!("tests/scenarios/{file} line {}: {err}", step.line)
                    });
                    crate::matchers::Matcher::parse(&text).unwrap_or_else(|err| {
                        panic!("tests/scenarios/{file} line {}: {err:?}", step.line)
                    });
                }
            }
        }
    }

    /// The CI suite runs `tests/scenarios/*.yaml` and names one file in it.
    #[test]
    fn the_committed_fixtures_are_what_the_exits_name() {
        let names: Vec<&str> = FIXTURES.iter().map(|(file, _)| *file).collect();
        assert!(
            names.contains(&"official-menu-smoke.yaml"),
            "the menu smoke fixture is committed"
        );
        for name in names {
            let path = format!("tests/scenarios/{name}");
            assert!(
                glob_matches("tests/scenarios/*.yaml", &path),
                "`{path}` is not matched by the fixture glob"
            );
        }
    }

    #[test]
    fn the_inline_example_is_a_scenario_that_runs() {
        let _world = world_with_instance();
        let spec = crate::registry::find("scenario").expect("registered");
        let example = spec
            .examples
            .iter()
            .find(|example| example.args.contains("inline"))
            .expect("one example is inline");
        let json = example.args_json().expect("JSON");
        let out = scenario_on(&ScenarioArgs::from_json(&json).expect("parses")).expect("runs");
        assert_eq!(out.json["status"], "pass", "{}", out.text);
    }

    #[test]
    fn an_expect_the_answer_does_not_hold_fails_the_step_naming_the_field() {
        let _world = world_with_instance();
        let held = run(
            "schema: passportsim/scenario@1\nname: held\nsteps:\n  - env: {usb: open}\n    expect: {effects: []}\n",
        );
        assert_eq!(held.status, RunStatus::Pass, "{held:?}");
        let broken = run(
            "schema: passportsim/scenario@1\nname: broken\nsteps:\n  - env: {usb: open}\n    expect: {applied: {usb: charger}, rssi: 1}\n  - delay: 1ms\n",
        );
        assert_eq!(broken.status, RunStatus::Fail, "{broken:?}");
        assert_eq!(broken.steps[0].status, StepStatus::Fail, "{broken:?}");
        let error = broken.steps[0].error.as_ref().expect("an assertion error");
        assert_eq!(error["code"], "E_ASSERT", "{error}");
        let message = error["message"].as_str().unwrap_or_default();
        assert!(message.starts_with("expect: "), "{message}");
        assert!(
            message.contains("`rssi`: the answer has no such field"),
            "{message}"
        );
        assert_eq!(broken.steps[1].status, StepStatus::Skipped, "{broken:?}");
    }
}
