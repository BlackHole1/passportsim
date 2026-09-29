//! `passportsim serial`: cursor-based reads of a console, and writes into it.
//!
//! Reading is a delta from an absolute cursor that survives resets. With no cursor a call continues
//! where this session left off; the result carries the next cursor. Only whole lines are consumed,
//! so an unfinished line is never shown twice ([`crate::shape::shape_serial`]).
//!
//! Writing does not advance virtual time: the bytes are journaled now and consumed in the next
//! `run`. A `usj` write needs an open host client, because the peripheral drops what nobody is
//! sending; with none it is `E_STATE` naming the input that opens one. A write is whole or refused:
//! a ring without room would keep only a prefix, so a write larger than [`Session::usj_room`] is a
//! retryable `E_STATE`. `uart0` has no host-to-guest channel, so writing it is `E_USAGE`.
//!
//! Waiting for output is `run --until serial:/.../`, not an operation here.

use std::fmt::Write as _;

use pemu_core::hostio::SerialStream;
use pemu_core::input::{InputEvent, SerialChan};

use crate::error::{ApiError, E_LEASE, E_STATE, E_USAGE};
use crate::output::Output;
use crate::registry::command;
use crate::shape::{Cursor, ShapeLimits, shape_serial};
use crate::spec::{HandlerCx, Schema};

use super::run::{parse_stream, stream_name};
use crate::args::{enum_of, instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::session::NOW;
use crate::session::Session;

pub const MAX_BYTES: u64 = 1_048_576;

pub const DEFAULT_MAX_BYTES: u64 = 8_192;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    /// Into the console's host-to-guest ring.
    Write,
}

impl Op {
    pub const fn as_str(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
        }
    }

    pub fn parse(text: &str) -> Option<Op> {
        match text {
            "read" => Some(Op::Read),
            "write" => Some(Op::Write),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub op: Op,
    pub stream: SerialStream,
    /// The session's own when absent.
    pub cursor: Option<u64>,
    pub max_bytes: u64,
    pub text: Option<String>,
    /// As hexadecimal pairs.
    pub hex: Option<Vec<u8>>,
    pub newline: bool,
}

impl SerialArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<SerialArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "op",
                "stream",
                "cursor",
                "max_bytes",
                "text",
                "hex",
                "newline",
            ],
        )?;
        let op = enum_of(args, "op", Op::parse, "read, write")?
            .ok_or_else(|| usage("op", "is required: `read` or `write`"))?;
        let hex = match opt_str(args, "hex")? {
            None => None,
            Some(text) => Some(parse_hex(text)?),
        };
        let text = opt_str(args, "text")?.map(str::to_owned);
        if op == Op::Write && text.is_none() && hex.is_none() {
            return Err(usage("text", "a write needs `text` or `hex`"));
        }
        if text.is_some() && hex.is_some() {
            return Err(usage("hex", "a write takes `text` or `hex`, not both"));
        }
        let max_bytes = opt_u64(args, "max_bytes")?.unwrap_or(DEFAULT_MAX_BYTES);
        if max_bytes == 0 || max_bytes > MAX_BYTES {
            return Err(usage(
                "max_bytes",
                &format!("expected 1 to {MAX_BYTES} bytes"),
            ));
        }
        Ok(SerialArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            op,
            stream: enum_of(args, "stream", parse_stream, "usj, uart0")?
                .unwrap_or(SerialStream::UsjTx),
            cursor: opt_u64(args, "cursor")?,
            max_bytes,
            text,
            hex,
            newline: opt_bool(args, "newline")?.unwrap_or(false),
        })
    }

    pub fn payload(&self) -> Vec<u8> {
        let mut bytes = match (&self.text, &self.hex) {
            (Some(text), _) => text.as_bytes().to_vec(),
            (None, Some(hex)) => hex.clone(),
            (None, None) => Vec::new(),
        };
        if self.newline {
            bytes.push(b'\n');
        }
        bytes
    }
}

/// An even-length run of hexadecimal digits.
fn parse_hex(text: &str) -> Result<Vec<u8>, ApiError> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(usage(
            "hex",
            "expected an even number of hexadecimal digits",
        ));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&text[i..i + 2], 16)
                .map_err(|_| usage("hex", "expected hexadecimal digits"))
        })
        .collect()
}

fn rx_chan(stream: SerialStream) -> SerialChan {
    match stream {
        SerialStream::UsjTx => SerialChan::USJ,
        SerialStream::Uart0Tx => SerialChan::UART0,
    }
}

pub fn serial_on(session: &mut Session, args: &SerialArgs) -> Result<Output, ApiError> {
    match args.op {
        Op::Read => read(session, args),
        Op::Write => write_bytes(session, args),
    }
}

fn read(session: &mut Session, args: &SerialArgs) -> Result<Output, ApiError> {
    // An explicit cursor is a one-off view and leaves the session's own alone.
    let from = Cursor(args.cursor.unwrap_or_else(|| session.cursor(args.stream).0));
    let chunk: Vec<u8> = {
        let ring = session.machine().io().serial_ring(args.stream);
        ring.slices(from.0)
            .iter()
            .copied()
            .take(usize::try_from(args.max_bytes).unwrap_or(usize::MAX))
            .collect()
    };
    let excerpt = shape_serial(&chunk, from, &[], &ShapeLimits::DEFAULT);
    if args.cursor.is_none() {
        session.set_cursor(args.stream, excerpt.next_cursor);
    }
    let receipt = session.receipt();
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "op": Op::Read.as_str(),
        "stream": stream_name(args.stream),
        "vt_us": receipt.vt_us,
        "cursor": excerpt.cursor.0,
        "next_cursor": excerpt.next_cursor.0,
        "bytes": excerpt.bytes,
        "held_bytes": excerpt.held_bytes,
        "serial": excerpt.to_json(),
    });
    let text = excerpt.to_text();
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

/// Journaled now, consumed by the next `run`.
fn write_bytes(session: &mut Session, args: &SerialArgs) -> Result<Output, ApiError> {
    if args.stream == SerialStream::Uart0Tx {
        return Err(usage(
            "stream",
            "`uart0` has no host-to-guest channel, so it can be read but not written",
        )
        .with_hint("write to the console with `--stream usj`"));
    }
    // The drop rule is the USB Serial/JTAG peripheral's; UART0 has no host client to have open.
    if args.stream == SerialStream::UsjTx && !session.machine().io().usj_ctrl.client_open() {
        return Err(ApiError::new(
            E_STATE,
            "no host client has the console open, so a write would be dropped",
        )
        .with_hint("`input usb open` opens the client side of the USB Serial/JTAG console"));
    }
    let payload = args.payload();
    let written = payload.len();
    let room = session.usj_room();
    if written > room {
        return Err(ApiError::new(
            E_STATE,
            format!(
                "the console input ring has room for {room} of the {written} byte(s) at this instant, and a write is never truncated"
            ),
        )
        .retryable()
        .with_hint("`run --for 10ms` lets the guest drain the ring; or split the write"));
    }
    session
        .machine()
        .input(
            NOW,
            InputEvent::SerialIn {
                chan: rx_chan(args.stream),
                data: payload,
            },
        )
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the console write at the current instant",
            )
        })?;
    session.note_usj_journaled(written);
    let receipt = session.receipt();
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "op": Op::Write.as_str(),
        "stream": stream_name(args.stream),
        "vt_us": receipt.vt_us,
        "written_bytes": written,
        "host_port": "open",
    });
    let mut text = String::new();
    let _ = write!(
        text,
        "{} wrote {written} byte(s) to {} at vt={}us",
        session.id,
        stream_name(args.stream),
        receipt.vt_us
    );
    Ok(Output::new(json, text, receipt))
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["op"],
        "description": "`serial` arguments.",
        "properties": {
            "instance": instance_schema(),
            "op": { "type": "string", "enum": ["read", "write"], "description": "What to do." },
            "stream": { "type": "string", "enum": ["usj", "uart0"], "description": "Console (usj)." },
            "cursor": { "type": "integer", "minimum": 0, "description": "Byte offset." },
            "max_bytes": { "type": "integer", "minimum": 1, "maximum": 1048576, "description": "Largest read (8192)." },
            "text": { "type": "string", "description": "Text to write." },
            "hex": { "type": "string", "pattern": "^([0-9a-fA-F]{2})*$", "description": "Bytes as hex pairs." },
            "newline": { "type": "boolean", "description": "Append a newline (false)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "op": { "type": "string" },
            "stream": { "type": "string" },
            "vt_us": { "type": "integer" },
            "cursor": { "type": "integer" },
            "next_cursor": { "type": "integer" },
            "bytes": { "type": "integer" },
            "held_bytes": { "type": "integer" },
            "serial": { "type": "object" },
            "written_bytes": { "type": "integer" },
            "host_port": { "type": "string" }
        }
    })
}

/// Read new console output from a cursor, or write bytes.
#[command(
    api_crate = crate,
    name = "serial",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    cli(positional = ["op"]),
    scenario_step = "serial.write",
    errors(E_USAGE, E_STATE, E_LEASE),
    example(
        title = "Read what the guest printed since the last call",
        args = r#"{"op":"read"}"#,
    ),
    example(
        title = "Send a line to the guest",
        args = r#"{"op":"write","text":"ping","newline":true}"#,
    ),
)]
pub fn serial(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = SerialArgs::from_json(&args)?;
    // On the checked-out session, outside the pool lock.
    crate::pool::with_session(
        |pool| {
            let id = pool.bind(SPEC_SERIAL.annotations, args.instance.as_deref())?;
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            // Under `endpoint --clock agent` a host tool is the console's USB host; an agent write
            // beside it would interleave two streams.
            if args.op == Op::Write && super::endpoint::is_open(pool, id) {
                return Err(ApiError::new(
                    E_STATE,
                    "an endpoint client is the console's host, so an agent write would interleave with it",
                )
                .with_hint("send the bytes through the endpoint, or `endpoint --close` first"));
            }
            if args.op == Op::Write
                && let Some(state) = pool.table().get(id)
            {
                state.lease.check_call(
                    crate::lease::LeaseHolder::Agent,
                    SPEC_SERIAL.annotations,
                    now,
                )?;
            }
            Ok(id)
        },
        |session| serial_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::start::tests::{TestMachine, started};

    fn read_args() -> SerialArgs {
        SerialArgs {
            instance: None,
            op: Op::Read,
            stream: SerialStream::UsjTx,
            cursor: None,
            max_bytes: DEFAULT_MAX_BYTES,
            text: None,
            hex: None,
            newline: false,
        }
    }

    /// So the scripted lines are released.
    fn advance(session: &mut Session, ms: u64) {
        let until = pemu_core::time::VTime(session.now().0 + pemu_core::time::VTime::from_ms(ms).0);
        session.run_until(until);
    }

    #[test]
    fn a_read_consumes_whole_lines_and_advances_the_session_cursor() {
        let (mut pool, id) = started(TestMachine::new().line(1, "alpha").line(2, "beta"));
        let session = pool.session_mut(id).expect("the scripted instance");
        advance(session, 10);
        let out = serial_on(session, &read_args()).expect("a read always succeeds");
        assert_eq!(out.json["op"], "read");
        assert_eq!(out.json["cursor"], 0);
        assert_eq!(out.json["next_cursor"], 11);
        assert_eq!(out.json["bytes"], 11);
        assert_eq!(out.json["serial"]["lines_total"], 2);
        assert!(
            out.text.contains("alpha") && out.text.contains("beta"),
            "{}",
            out.text
        );
        // The cursor moved, so a second read returns nothing.
        let again = serial_on(session, &read_args()).expect("a read always succeeds");
        assert_eq!(again.json["cursor"], 11);
        assert_eq!(again.json["bytes"], 0);
    }

    #[test]
    fn an_unfinished_line_is_held_back_rather_than_shown_twice() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        session.machine().io().serial_write(
            SerialStream::UsjTx,
            b"half",
            pemu_core::time::VTime(0),
        );
        let out = serial_on(session, &read_args()).expect("a read always succeeds");
        assert_eq!(out.json["bytes"], 0);
        assert_eq!(out.json["held_bytes"], 4);
        assert_eq!(out.json["next_cursor"], 0);
    }

    #[test]
    fn an_explicit_cursor_is_a_one_off_view_and_leaves_the_session_cursor_alone() {
        let (mut pool, id) = started(TestMachine::new().line(1, "alpha"));
        let session = pool.session_mut(id).expect("the scripted instance");
        advance(session, 10);
        let mut args = read_args();
        args.cursor = Some(0);
        let out = serial_on(session, &args).expect("a read always succeeds");
        assert_eq!(out.json["bytes"], 6);
        assert_eq!(session.cursor(SerialStream::UsjTx).0, 0);
    }

    #[test]
    fn a_write_without_an_open_host_client_is_state_and_names_the_input_that_opens_one() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        // A client is attached by default, so the test closes it first.
        session.machine().io().usj_ctrl.set_client_open(false);
        let args = SerialArgs {
            op: Op::Write,
            text: Some("ping".to_owned()),
            ..read_args()
        };
        let error = serial_on(session, &args).expect_err("nobody is listening");
        assert_eq!(error.code, E_STATE);
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("usb open"),
            "{:?}",
            error.hint
        );
    }

    #[test]
    fn a_uart0_write_is_refused_because_uart0_has_no_host_to_guest_channel() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let uart0 = SerialArgs {
            op: Op::Write,
            stream: SerialStream::Uart0Tx,
            text: Some("ping".to_owned()),
            ..read_args()
        };
        let error = serial_on(session, &uart0).expect_err("uart0 is read-only");
        assert_eq!(error.code, E_USAGE);
        assert!(
            error.message.contains("no host-to-guest channel"),
            "{}",
            error.message
        );
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("--stream usj")
        );
        assert_eq!(
            session.machine().io().usj_rx.len(),
            0,
            "nothing was journaled"
        );

        let read = SerialArgs {
            stream: SerialStream::Uart0Tx,
            ..read_args()
        };
        serial_on(session, &read).expect("uart0 is still readable");
    }

    /// A write that fits after a first one at the same instant counts the first one's bytes.
    #[test]
    fn a_write_larger_than_the_ring_room_is_refused_not_truncated() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let capacity = session.machine().io().usj_rx.capacity();
        let too_big = SerialArgs {
            op: Op::Write,
            hex: Some(vec![0x41; capacity + 1]),
            ..read_args()
        };
        let error = serial_on(session, &too_big).expect_err("larger than the ring");
        assert_eq!(error.code, E_STATE);
        assert!(error.retryable);
        assert!(
            error.message.contains(&format!("room for {capacity}")),
            "{}",
            error.message
        );
        assert_eq!(
            session.machine().io().usj_rx.len(),
            0,
            "nothing was journaled"
        );

        let fits = SerialArgs {
            op: Op::Write,
            hex: Some(vec![0x41; capacity]),
            ..read_args()
        };
        let out = serial_on(session, &fits).expect("exactly the room");
        assert_eq!(out.json["written_bytes"], capacity);
        let one_more = SerialArgs {
            op: Op::Write,
            hex: Some(vec![0x42]),
            ..read_args()
        };
        let error = serial_on(session, &one_more).expect_err("the ring is full at this instant");
        assert_eq!(error.code, E_STATE);
    }

    #[test]
    fn a_write_reaches_the_host_to_guest_ring_and_does_not_advance_time() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        assert!(
            session.machine().io().usj_ctrl.client_open(),
            "a client is attached by default"
        );
        let args = SerialArgs {
            op: Op::Write,
            text: Some("ping".to_owned()),
            newline: true,
            ..read_args()
        };
        let out = serial_on(session, &args).expect("the client is open");
        assert_eq!(out.json["written_bytes"], 5);
        assert_eq!(out.json["host_port"], "open");
        assert_eq!(out.json["vt_us"], 0);
        assert_eq!(session.now(), pemu_core::time::VTime(0));
        assert_eq!(session.machine().io().usj_rx.len(), 5);
    }

    #[test]
    fn a_hex_write_sends_the_bytes_it_names() {
        let args = SerialArgs::from_json(&serde_json::json!({
            "op": "write",
            "hex": "0a1b2c",
        }))
        .expect("an even run of hex digits");
        assert_eq!(args.payload(), vec![0x0a, 0x1b, 0x2c]);
    }

    #[test]
    fn malformed_arguments_are_usage() {
        let cases = [
            serde_json::json!({}),
            serde_json::json!({ "op": "peek" }),
            serde_json::json!({ "op": "write" }),
            serde_json::json!({ "op": "write", "text": "a", "hex": "0a" }),
            serde_json::json!({ "op": "write", "hex": "0a1" }),
            serde_json::json!({ "op": "read", "max_bytes": 0 }),
            serde_json::json!({ "op": "read", "max_bytes": 2_000_000 }),
            serde_json::json!({ "op": "read", "stream": "uart9" }),
            serde_json::json!({ "op": "read", "nonsense": true }),
        ];
        for case in cases {
            let error = SerialArgs::from_json(&case).expect_err("outside the schema");
            assert_eq!(error.code, E_USAGE, "{case}");
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("serial").expect("#[command] registered serial");
        for example in spec.examples {
            let args = example.args_json().expect("an example is JSON");
            SerialArgs::from_json(&args).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.needs_instance && !spec.annotations.advances_time);
        assert_eq!(spec.scenario_step, Some("serial.write"));
    }
}
