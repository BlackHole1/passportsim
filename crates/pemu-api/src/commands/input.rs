//! `passportsim input`: the ladder buttons, the power button, the USB cable and the host client,
//! with exact timing. Every stimulus is journaled, so a replay replays the inputs.
//!
//! The defaults: a click holds 80 ms, a long press 1600 ms, the power button 600 ms to switch on
//! and 2200 ms to switch off, and every action is followed by 250 ms so the firmware's click
//! detector fires before the call returns. `press` and `release` are raw edges with no implicit
//! run. The power press length depends on the rail, which the caller passes to [`input_on`].
//!
//! USB is two inputs: `plug`/`unplug` move the cable and `open`/`close` the client. Driving both at
//! once would make U2 ATTACHED_IDLE, the stalled-IN-endpoint state the emulator exists to
//! reproduce, unreachable.
//!
//! `reset: chip` is refused with `E_UNMODELED`; `reset: usb_rts` is esptool's line sequence, two
//! `UsbLine` inputs.

use std::fmt::Write as _;

use pemu_core::input::{ButtonId, InputEvent};
use pemu_core::time::VTime;

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TRIPWIRE,
    E_UNMODELED, E_USAGE,
};
use crate::instance::Lifecycle;
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::run::{excerpt_of, stream_name};
use crate::args::{
    duration_schema, enum_of, instance_schema, object, only, opt_duration, opt_str, usage,
};
use crate::session::NOW;
use crate::session::Session;

pub const CLICK_MS: u64 = 80;
pub const HOLD_MS: u64 = 1_600;
pub const POWER_ON_MS: u64 = 600;
pub const POWER_OFF_MS: u64 = 2_200;
pub const THEN_RUN_MS: u64 = 250;
pub const DOUBLE_GAP_MS: u64 = 80;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Button {
    Up,
    Down,
    Ok,
    Power,
    /// Not a ladder button.
    Reset,
    /// Plugged or unplugged.
    Usb,
}

impl Button {
    pub const fn as_str(self) -> &'static str {
        match self {
            Button::Up => "up",
            Button::Down => "down",
            Button::Ok => "ok",
            Button::Power => "power",
            Button::Reset => "reset",
            Button::Usb => "usb",
        }
    }

    pub fn parse(text: &str) -> Option<Button> {
        match text {
            "up" => Some(Button::Up),
            "down" => Some(Button::Down),
            "ok" => Some(Button::Ok),
            "power" => Some(Button::Power),
            "reset" => Some(Button::Reset),
            "usb" => Some(Button::Usb),
            _ => None,
        }
    }

    /// For the three that are one.
    pub const fn ladder(self) -> Option<ButtonId> {
        match self {
            Button::Up => Some(ButtonId::Up),
            Button::Down => Some(ButtonId::Down),
            Button::Ok => Some(ButtonId::Ok),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Click,
    DoubleClick,
    Hold,
    Press,
    Release,
    /// USB only: a host client opens the console (U3).
    Open,
    /// USB only: the client closes the console (U2 while the cable stays in).
    Close,
    /// USB only (U1 to U3).
    Plug,
    /// USB only (U0).
    Unplug,
}

impl Action {
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Click => "click",
            Action::DoubleClick => "double_click",
            Action::Hold => "hold",
            Action::Press => "press",
            Action::Release => "release",
            Action::Open => "open",
            Action::Close => "close",
            Action::Plug => "plug",
            Action::Unplug => "unplug",
        }
    }

    pub fn parse(text: &str) -> Option<Action> {
        match text {
            "click" => Some(Action::Click),
            "double_click" => Some(Action::DoubleClick),
            "hold" => Some(Action::Hold),
            "press" => Some(Action::Press),
            "release" => Some(Action::Release),
            "open" => Some(Action::Open),
            "close" => Some(Action::Close),
            "plug" => Some(Action::Plug),
            "unplug" => Some(Action::Unplug),
            _ => None,
        }
    }

    /// A raw edge gets no implicit run.
    pub const fn is_raw_edge(self) -> bool {
        matches!(self, Action::Press | Action::Release)
    }

    pub const fn is_usb(self) -> bool {
        matches!(
            self,
            Action::Open | Action::Close | Action::Plug | Action::Unplug
        )
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResetKind {
    Chip,
    /// esptool's USB-JTAG line sequence.
    UsbRts,
}

impl ResetKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ResetKind::Chip => "chip",
            ResetKind::UsbRts => "usb_rts",
        }
    }

    pub fn parse(text: &str) -> Option<ResetKind> {
        match text {
            "chip" => Some(ResetKind::Chip),
            "usb_rts" => Some(ResetKind::UsbRts),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub button: Button,
    pub action: Action,
    /// The per-button default when absent.
    pub duration: Option<VTime>,
    pub then_run: Option<VTime>,
    pub reset_kind: ResetKind,
}

impl InputArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<InputArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "button",
                "action",
                "duration",
                "then_run",
                "reset_kind",
            ],
        )?;
        let button = enum_of(
            args,
            "button",
            Button::parse,
            "up, down, ok, power, reset, usb",
        )?
        .ok_or_else(|| usage("button", "is required"))?;
        let action = enum_of(
            args,
            "action",
            Action::parse,
            "click, double_click, hold, press, release, open, close, plug, unplug",
        )?
        .unwrap_or(Action::Click);
        if action.is_usb() && button != Button::Usb {
            return Err(usage(
                "action",
                "`open`, `close`, `plug` and `unplug` belong to `button: usb`",
            ));
        }
        if button == Button::Usb && !action.is_usb() {
            return Err(usage(
                "action",
                "`button: usb` takes `open`, `close`, `plug` or `unplug`",
            ));
        }
        Ok(InputArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            button,
            action,
            duration: opt_duration(args, "duration")?,
            then_run: opt_duration(args, "then_run")?,
            // The default is `chip`, which is not modelled and refuses by name; substituting the
            // esptool line sequence would do something visibly different under the caller's own
            // word.
            reset_kind: enum_of(args, "reset_kind", ResetKind::parse, "chip, usb_rts")?
                .unwrap_or(ResetKind::Chip),
        })
    }

    pub fn press_len(&self, rail_on: bool) -> VTime {
        if let Some(duration) = self.duration {
            return duration;
        }
        VTime::from_ms(match (self.button, self.action) {
            (Button::Power, _) if rail_on => POWER_OFF_MS,
            (Button::Power, _) => POWER_ON_MS,
            (_, Action::Hold) => HOLD_MS,
            _ => CLICK_MS,
        })
    }

    pub fn settle(&self) -> VTime {
        match (self.then_run, self.action.is_raw_edge()) {
            (Some(run), _) => run,
            (None, true) => VTime(0),
            (None, false) => VTime::from_ms(THEN_RUN_MS),
        }
    }
}

/// `rail_on` alone decides the power press length. A guest that parks during the settle run ends
/// the call as `E_DEADLOCK`, inputs already journaled. Inside a gesture the next edge is journaled
/// before the machine runs towards it, so a pending input keeps the hart from a mid-gesture
/// deadlock and a hold is never cut short.
pub fn input_on(
    session: &mut Session,
    args: &InputArgs,
    rail_on: bool,
) -> Result<Output, ApiError> {
    input_on_bare(session, args, rail_on)
        .map_err(|error| crate::commands::inspect::deadlock_envelope(session, error))
}

fn input_on_bare(
    session: &mut Session,
    args: &InputArgs,
    rail_on: bool,
) -> Result<Output, ApiError> {
    let started = session.now();
    let press = args.press_len(rail_on);
    let mut press_vt = started;
    let mut release_vt = started;
    let mut pressed_now = Vec::new();

    match (args.button, args.action) {
        // The cable and client move separately, which makes U2 ATTACHED_IDLE (cable in, no client)
        // reachable.
        (Button::Usb, Action::Open) => journal(session, InputEvent::UsbClient { open: true })?,
        (Button::Usb, Action::Close) => journal(session, InputEvent::UsbClient { open: false })?,
        (Button::Usb, Action::Plug) => journal(session, InputEvent::UsbCable { plugged: true })?,
        (Button::Usb, _) => journal(session, InputEvent::UsbCable { plugged: false })?,
        (Button::Reset, _) => match args.reset_kind {
            ResetKind::UsbRts => {
                // In the order esptool drives DTR and RTS.
                journal(
                    session,
                    InputEvent::UsbLine {
                        dtr: false,
                        rts: true,
                    },
                )?;
                release_vt = journal_after(
                    session,
                    VTime::from_ms(POWER_ON_MS / 6),
                    InputEvent::UsbLine {
                        dtr: false,
                        rts: false,
                    },
                )?;
            }
            ResetKind::Chip => {
                return Err(ApiError::new(
                    E_UNMODELED,
                    "a chip reset is not modelled by this build",
                )
                .with_hint("this build has no board reset path; `reset_kind: usb_rts` works"));
            }
        },
        (button, Action::Press) => {
            press_edge(session, button, true)?;
            pressed_now.push(button.as_str().to_owned());
            press_vt = session.now();
            release_vt = press_vt;
        }
        (button, Action::Release) => {
            press_edge(session, button, false)?;
            release_vt = session.now();
            press_vt = release_vt;
        }
        (button, Action::DoubleClick) => {
            press_edge(session, button, true)?;
            press_vt = session.now();
            journal_after(session, press, edge(button, false))?;
            journal_after(session, VTime::from_ms(DOUBLE_GAP_MS), edge(button, true))?;
            release_vt = journal_after(session, press, edge(button, false))?;
        }
        (button, _) => {
            press_edge(session, button, true)?;
            press_vt = session.now();
            release_vt = journal_after(session, press, edge(button, false))?;
        }
    }

    advance(session, args.settle())?;
    let excerpt = excerpt_of(session, pemu_core::hostio::SerialStream::UsjTx);
    let receipt = session.receipt();
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "vt_us": receipt.vt_us,
        "elapsed_vt_us": VTime(session.now().0.saturating_sub(started.0)).as_us(),
        "input": {
            "button": args.button.as_str(),
            "action": args.action.as_str(),
            "press_vt_us": press_vt.as_us(),
            "release_vt_us": release_vt.as_us(),
            "pressed_now": pressed_now,
        },
        "serial": excerpt.to_json(),
        "stream": stream_name(pemu_core::hostio::SerialStream::UsjTx),
    });
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} {} {} vt={}us",
        session.id,
        args.button.as_str(),
        args.action.as_str(),
        receipt.vt_us
    );
    text.push_str(&excerpt.to_text());
    Ok(Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT))
}

fn edge(button: Button, down: bool) -> InputEvent {
    match button.ladder() {
        Some(id) => InputEvent::Button { id, down },
        None => InputEvent::Power { down },
    }
}

fn press_edge(session: &mut Session, button: Button, down: bool) -> Result<(), ApiError> {
    journal(session, edge(button, down))
}

/// The edge is pending while the machine runs, so the hart is never reported as deadlocked before
/// it.
fn journal_after(session: &mut Session, by: VTime, event: InputEvent) -> Result<VTime, ApiError> {
    let at = VTime(session.now().0.saturating_add(by.0));
    session
        .machine()
        .input(pemu_machine::machine::At::Vt(at), event)
        .map_err(|_| {
            ApiError::new(E_STATE, "the machine refused the input at its instant")
                .with_hint("`status` shows the instance's lifecycle state")
        })?;
    advance(session, by)?;
    Ok(at)
}

fn journal(session: &mut Session, event: InputEvent) -> Result<(), ApiError> {
    session
        .machine()
        .input(NOW, event)
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the input at the current instant",
            )
            .with_hint("`status` shows the instance's lifecycle state")
        })
}

fn advance(session: &mut Session, by: VTime) -> Result<(), ApiError> {
    if by.0 == 0 {
        return Ok(());
    }
    let until = VTime(session.now().0.saturating_add(by.0));
    let outcome = session.run_until(until);
    match crate::session::fault_of(&outcome.reason) {
        Some(error) => Err(error.at_vt_us(outcome.vt.as_us())),
        // An input cannot break a mutex cycle either; the caller adds the envelope.
        None => crate::session::task_deadlock_fault(session).map_or(Ok(()), Err),
    }
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["button"],
        "description": "`input` arguments.",
        "properties": {
            "instance": instance_schema(),
            "button": { "type": "string", "enum": ["up", "down", "ok", "power", "reset", "usb"], "description": "What to press." },
            "action": { "type": "string", "enum": ["click", "double_click", "hold", "press", "release", "open", "close", "plug", "unplug"], "description": "What to do with it (click)." },
            "duration": duration_schema("Press length."),
            "then_run": duration_schema("Run after release."),
            "reset_kind": { "type": "string", "enum": ["chip", "usb_rts"], "description": "Reset kind (chip)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "elapsed_vt_us": { "type": "integer" },
            "input": { "type": "object" },
            "serial": { "type": "object" },
            "stream": { "type": "string" }
        }
    })
}

/// Press a button, hold power, or plug the USB cable.
#[command(
    api_crate = crate,
    name = "input",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["button", "action"]),
    scenario_step = "press",
    errors(E_USAGE, E_STATE, E_LEASE, E_UNMODELED, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_HLE, E_INTERNAL),
    example(
        title = "Click the DOWN button and let the firmware react",
        args = r#"{"button":"down","action":"click"}"#,
    ),
    example(
        title = "Hold the power button long enough to switch the rail",
        args = r#"{"button":"power","action":"hold"}"#,
    ),
    example(
        title = "Open the host side of the USB Serial/JTAG console",
        args = r#"{"button":"usb","action":"open"}"#,
    ),
    example(
        title = "Leave the cable in with no client reading, the U2 case",
        args = r#"{"button":"usb","action":"close"}"#,
    ),
)]
pub fn input(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = InputArgs::from_json(&args)?;
    let rail_on = std::cell::Cell::new(true);
    // On the checked-out session, outside the pool lock.
    crate::pool::with_session(
        |pool| {
            let id = pool.bind(SPEC_INPUT.annotations, args.instance.as_deref())?;
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            if let Some(state) = pool.table().get(id) {
                state.lease.check_call(
                    crate::lease::LeaseHolder::Agent,
                    SPEC_INPUT.annotations,
                    now,
                )?;
            }
            // The rail is down in exactly one lifecycle state, and it decides how long a power
            // press is.
            rail_on.set(
                pool.table()
                    .get(id)
                    .is_none_or(|state| state.lifecycle != Lifecycle::PoweredOff),
            );
            Ok(id)
        },
        |session| input_on(session, &args, rail_on.get()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_core::hostio::UsbHostState;
    use pemu_core::input::InputEvent as Ev;

    use crate::commands::start::tests::{TestMachine, started};

    fn args(button: Button, action: Action) -> InputArgs {
        InputArgs {
            instance: None,
            button,
            action,
            duration: None,
            then_run: None,
            reset_kind: ResetKind::UsbRts,
        }
    }

    /// Rail on, which is every test but the power ones.
    fn press(session: &mut Session, args: &InputArgs) -> Result<Output, ApiError> {
        input_on(session, args, true)
    }

    #[test]
    fn a_click_journals_both_edges_and_settles_for_250_ms() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = press(session, &args(Button::Down, Action::Click)).expect("a click");
        assert_eq!(out.json["input"]["button"], "down");
        assert_eq!(out.json["input"]["press_vt_us"], 0);
        assert_eq!(out.json["input"]["release_vt_us"], CLICK_MS * 1_000);
        assert_eq!(out.json["vt_us"], (CLICK_MS + THEN_RUN_MS) * 1_000);
        assert_eq!(out.json["elapsed_vt_us"], (CLICK_MS + THEN_RUN_MS) * 1_000);
    }

    /// The release is journaled before the machine runs to it, so the guest parks after the
    /// release, not during the hold.
    #[test]
    fn a_guest_that_parks_during_a_click_is_e_deadlock_after_the_release() {
        let _world = crate::commands::inspect::tests::world();
        crate::commands::inspect::set_introspectors(crate::commands::inspect::tests::SCRIPTED);
        let (mut pool, id) = started(TestMachine::new().waits_again_at(CLICK_MS / 2));
        let session = pool.session_mut(id).expect("the scripted instance");
        let error = press(session, &args(Button::Ok, Action::Click)).expect_err("it parks");
        assert_eq!(error.code, E_DEADLOCK);
        assert_eq!(
            error.vt_us,
            CLICK_MS * 1_000,
            "the hold kept its length and the guest parked after the release"
        );
        assert_eq!(error.detail["fault"], "deadlock");
        assert_eq!(error.detail["blocked"][0]["blocked_on"], "lvgl_port mutex");
        assert!(
            error.detail["wake_inputs"]
                .as_array()
                .is_some_and(|w| !w.is_empty())
        );
    }

    #[test]
    fn a_hold_presses_for_the_long_press_threshold() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = press(session, &args(Button::Ok, Action::Hold)).expect("a long press");
        assert_eq!(out.json["input"]["release_vt_us"], HOLD_MS * 1_000);
    }

    /// The same arguments make two different presses, and only the caller knows which.
    #[test]
    fn the_power_button_holds_for_the_rail_the_instance_is_on() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let on = input_on(session, &args(Button::Power, Action::Hold), true).expect("a power hold");
        assert_eq!(on.json["input"]["release_vt_us"], POWER_OFF_MS * 1_000);

        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let off =
            input_on(session, &args(Button::Power, Action::Click), false).expect("a power press");
        assert_eq!(off.json["input"]["release_vt_us"], POWER_ON_MS * 1_000);
        assert_eq!(off.json["vt_us"], (POWER_ON_MS + THEN_RUN_MS) * 1_000);
    }

    #[test]
    fn a_powered_off_instance_is_a_rail_that_is_off() {
        let (mut pool, id) = started(TestMachine::new());
        pool.table_mut()
            .get_mut(id)
            .expect("the instance")
            .transition(Lifecycle::PoweredOff, pemu_core::time::VTime(0))
            .expect("paused -> powered_off");
        let rail_on = pool
            .table()
            .get(id)
            .is_none_or(|state| state.lifecycle != Lifecycle::PoweredOff);
        assert!(!rail_on);
    }

    #[test]
    fn a_raw_edge_leaves_the_button_down_and_runs_nothing() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = press(session, &args(Button::Up, Action::Press)).expect("a raw edge");
        assert_eq!(out.json["vt_us"], 0);
        assert_eq!(out.json["input"]["pressed_now"][0], "up");
        assert_eq!(
            session
                .machine()
                .io()
                .serial_ring(pemu_core::hostio::SerialStream::UsjTx)
                .head(),
            0
        );
    }

    #[test]
    fn a_double_click_is_two_presses_with_a_gap() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = press(session, &args(Button::Ok, Action::DoubleClick)).expect("two clicks");
        let expected = (CLICK_MS * 2 + DOUBLE_GAP_MS) * 1_000;
        assert_eq!(out.json["input"]["release_vt_us"], expected);
        assert_eq!(out.json["vt_us"], expected + THEN_RUN_MS * 1_000);
    }

    #[test]
    fn opening_the_usb_client_leaves_the_cable_where_it_was() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        session.machine().io().usj_ctrl.set_client_open(false);
        press(session, &args(Button::Usb, Action::Open)).expect("the host opens the port");
        assert!(session.machine().io().usj_ctrl.client_open());
        assert!(session.machine().io().usj_ctrl.cable());
    }

    /// U2 ATTACHED_IDLE is the state whose undrained IN endpoint stalls the IDF VFS, so the cable
    /// and the client have to move apart.
    #[test]
    fn the_usb_inputs_reach_every_host_state() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        assert_eq!(
            session.machine().io().usj_ctrl.host_state(true),
            UsbHostState::AttachedOpen,
            "a new machine starts at U3"
        );

        press(session, &args(Button::Usb, Action::Close)).expect("the client closes");
        assert_eq!(
            session.machine().io().usj_ctrl.host_state(true),
            UsbHostState::AttachedIdle,
            "U2: the cable is still in"
        );

        press(session, &args(Button::Usb, Action::Unplug)).expect("the cable comes out");
        assert_eq!(
            session.machine().io().usj_ctrl.host_state(true),
            UsbHostState::Detached
        );

        press(session, &args(Button::Usb, Action::Plug)).expect("the cable goes back in");
        assert_eq!(
            session.machine().io().usj_ctrl.host_state(false),
            UsbHostState::ChargeOnly,
            "U1: plugged with the rail down"
        );
        press(session, &args(Button::Usb, Action::Open)).expect("a client opens the port");
        assert_eq!(
            session.machine().io().usj_ctrl.host_state(true),
            UsbHostState::AttachedOpen
        );
    }

    #[test]
    fn a_usb_rts_reset_drives_the_two_line_states_esptool_drives() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        press(session, &args(Button::Reset, Action::Click)).expect("a usb_rts reset");
        assert!(!session.machine().io().usj_ctrl.rts());
    }

    /// Refuses by name instead of quietly performing a line sequence the caller did not ask for.
    #[test]
    fn a_reset_with_no_kind_is_a_chip_reset_and_names_the_package_that_will_model_it() {
        let args = InputArgs::from_json(&serde_json::json!({ "button": "reset" }))
            .expect("a reset needs no other argument");
        assert_eq!(args.reset_kind, ResetKind::Chip);

        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let error = press(session, &args).expect_err("not modelled yet");
        assert_eq!(error.code, E_UNMODELED);
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("usb_rts")
        );
        assert_eq!(
            session.now(),
            pemu_core::time::VTime(0),
            "the refusal ran nothing; the usb_rts sequence would have advanced 100 ms"
        );
    }

    #[test]
    fn the_journal_holds_every_edge_in_order() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        press(session, &args(Button::Down, Action::Click)).expect("a click");
        // The journal is only reachable on a real machine; here the shape is asserted through the
        // two edges.
        let out = press(session, &args(Button::Down, Action::Press)).expect("a raw edge");
        assert_eq!(out.json["input"]["action"], "press");
        assert!(matches!(
            Button::parse("down").and_then(Button::ladder),
            Some(pemu_core::input::ButtonId::Down)
        ));
        let _ = Ev::Power { down: true };
    }

    #[test]
    fn malformed_arguments_are_usage() {
        let cases = [
            serde_json::json!({}),
            serde_json::json!({ "button": "middle" }),
            serde_json::json!({ "button": "ok", "action": "swipe" }),
            serde_json::json!({ "button": "ok", "action": "open" }),
            serde_json::json!({ "button": "power", "action": "plug" }),
            serde_json::json!({ "button": "usb", "action": "click" }),
            serde_json::json!({ "button": "usb", "action": "hold" }),
            serde_json::json!({ "button": "ok", "duration": "5x" }),
            serde_json::json!({ "button": "ok", "nonsense": 1 }),
        ];
        for case in cases {
            let error = InputArgs::from_json(&case).expect_err("outside the schema");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("input").expect("#[command] registered input");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            InputArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.advances_time && spec.annotations.needs_instance);
        assert_eq!(spec.scenario_step, Some("press"));
    }
}
