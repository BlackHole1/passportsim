//! `passportsim net_capture`: the pcap capture of the Wi-Fi data plane (`pemu_radio::lan::pcap`),
//! read as counters or written out as an artifact.
//!
//! Every Ethernet frame the guest sent and received, with its virtual instant, bounded to the
//! newest 64 KiB. The capture is module state and always on, so a snapshot carries it and a replay
//! records the same frames; `action: start|stop` is refused by name.
//!
//! `save` writes `net/<label>.pcap` or `net/<vt_us>.pcap` through
//! [`crate::artifact_io::ArtifactIo`] and returns its path and SHA-256. The bytes go through the
//! value redaction pass first, so a MAC in the `SecretSet` is masked in the file as in the text.
//! Bridged payloads are kept as the guest saw them: they are the instance's own traffic.

use pemu_core::time::VTime;

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_STATE, E_USAGE};
use crate::output::{ArtifactRef, Output};
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::net_http::wifi_state;
use super::nfc_tap::bind_checked;
use crate::args::{instance_schema, object, only, opt_str, usage};
use crate::session::Session;

pub const NET_DIR: &str = "net";
/// IANA `application/vnd.tcpdump.pcap`.
pub const PCAP_MEDIA_TYPE: &str = "application/vnd.tcpdump.pcap";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Status,
    Save,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetCaptureArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub op: Op,
    /// `save`: the file stem.
    pub label: Option<String>,
}

/// One artifact-path segment, `^[a-z0-9][a-z0-9._-]*$`.
fn label_is_valid(label: &str) -> bool {
    label
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && label.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_'
        })
        && label.len() <= 64
}

impl NetCaptureArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<NetCaptureArgs, ApiError> {
        let args = object(value)?;
        if args.contains_key("action") {
            return Err(usage(
                "action",
                "the capture is always on (module state, bounded to its newest 64 KiB); `op: \
                 save` writes it and `op: status` reads its counters",
            ));
        }
        only(args, &["instance", "op", "label"])?;
        let op = match opt_str(args, "op")? {
            None | Some("status") => Op::Status,
            Some("save") => Op::Save,
            Some(other) => {
                return Err(usage(
                    "op",
                    &format!("`{other}` is not one of status, save"),
                ));
            }
        };
        let label = opt_str(args, "label")?.map(str::to_owned);
        if let Some(label) = &label {
            if op != Op::Save {
                return Err(usage("label", "only `save` writes a file"));
            }
            if !label_is_valid(label) {
                return Err(usage(
                    "label",
                    "a label is one path segment of [a-z0-9][a-z0-9._-]*",
                ));
            }
        }
        Ok(NetCaptureArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            op,
            label,
        })
    }

    pub fn artifact_path(&self, vt: VTime) -> String {
        match &self.label {
            Some(label) => format!("{NET_DIR}/{label}.pcap"),
            None => format!("{NET_DIR}/{}.pcap", vt.as_us()),
        }
    }
}

pub fn net_capture_on(session: &mut Session, args: &NetCaptureArgs) -> Result<Output, ApiError> {
    let state = wifi_state(session)?.ok_or_else(|| {
        ApiError::new(
            E_STATE,
            "this instance has no bound Wi-Fi module, or the guest has not reached it: there is \
             no data plane to capture",
        )
        .with_hint("`inspect fidelity` reports the binding")
    })?;
    let capture = &state.capture;
    let tx = capture
        .records
        .iter()
        .filter(|r| r.dir == pemu_radio::lan::pcap::dir::TX)
        .count();
    let mut artifact = None;
    if args.op == Op::Save {
        let io = crate::artifact_io::installed().ok_or_else(|| {
            ApiError::new(
                E_STATE,
                "this build cannot write an artifact, so the capture has nowhere to go",
            )
            .with_hint("`op: status` reads the counters")
        })?;
        let set = super::snapshot::secrets_of(session);
        let bytes = crate::redact::Redactor::new(&set).redact_bytes(&capture.to_pcap());
        let path = args.artifact_path(session.now());
        let written = (io.write)(&path, &bytes).map_err(|err| {
            ApiError::new(E_STATE, format!("the capture could not be written: {err}"))
        })?;
        let sha256 = super::snapshot::sha256_hex(&bytes);
        artifact = Some(
            ArtifactRef::new(written, sha256, PCAP_MEDIA_TYPE, bytes.len() as u64).map_err(
                |err| {
                    ApiError::new(
                        E_INTERNAL,
                        format!("the artifact path is not usable: {err}"),
                    )
                },
            )?,
        );
    }
    // A tainted instance's capture says so in its receipt.
    let _ = super::snapshot::secrets_of(session);
    let receipt = session.receipt();
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "vt_us": receipt.vt_us,
        "op": match args.op { Op::Status => "status", Op::Save => "save" },
        "frames": capture.records.len(),
        "tx": tx,
        "rx": capture.records.len() - tx,
        "bytes": capture.bytes,
        "dropped": capture.dropped,
        "linktype": pemu_radio::lan::pcap::LINKTYPE_ETHERNET,
        "capture": artifact.as_ref().map_or(serde_json::Value::Null, ArtifactRef::to_json),
    });
    let mut text = format!(
        "{} net_capture: {} frame(s) ({} tx, {} rx), {} byte(s), {} dropped",
        session.id,
        capture.records.len(),
        tx,
        capture.records.len() - tx,
        capture.bytes,
        capture.dropped
    );
    if let Some(a) = &artifact {
        text.push_str(&format!(
            "\n  {}",
            a.to_json()["path"].as_str().unwrap_or("")
        ));
    }
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

pub fn input_schema() -> Schema {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "`net_capture` arguments.",
        "properties": {
            "instance": instance_schema(),
            "op": { "enum": ["status", "save"], "description": "Default status." },
            "label": { "type": "string", "pattern": "^[a-z0-9][a-z0-9._-]*$", "maxLength": 64 }
        }
    });
    Schema::try_from(schema).unwrap_or_default()
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "op": { "type": "string" },
            "frames": { "type": "integer" },
            "tx": { "type": "integer" },
            "rx": { "type": "integer" },
            "bytes": { "type": "integer" },
            "dropped": { "type": "integer" },
            "linktype": { "type": "integer" },
            "capture": { "type": ["object", "null"] }
        }
    })
}

/// Read the Wi-Fi data plane's pcap capture, or write it as an artifact.
#[command(
    api_crate = crate,
    name = "net_capture",
    group = radio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    scenario_step = "net.capture",
    errors(E_USAGE, E_STATE, E_LEASE, E_INTERNAL),
    example(
        title = "Count the captured frames",
        args = r#"{}"#,
    ),
    example(
        title = "Write the capture as net/dhcp.pcap",
        args = r#"{"op":"save","label":"dhcp"}"#,
    ),
)]
pub fn net_capture(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = NetCaptureArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_NET_CAPTURE.annotations, args.instance.as_deref()),
        |session| net_capture_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_parse_and_refuse_by_name() {
        let a = NetCaptureArgs::from_json(&serde_json::json!({})).unwrap();
        assert_eq!(a.op, Op::Status);
        let a =
            NetCaptureArgs::from_json(&serde_json::json!({"op":"save","label":"dhcp"})).unwrap();
        assert_eq!(a.artifact_path(VTime::from_us(5)), "net/dhcp.pcap");
        let a = NetCaptureArgs::from_json(&serde_json::json!({"op":"save"})).unwrap();
        assert_eq!(a.artifact_path(VTime::from_us(5)), "net/5.pcap");
        for bad in [
            serde_json::json!({"action":"start"}),
            serde_json::json!({"op":"start"}),
            serde_json::json!({"label":"x"}),
            serde_json::json!({"op":"save","label":"../x"}),
            serde_json::json!({"op":"save","label":"X"}),
        ] {
            let err = NetCaptureArgs::from_json(&bad).expect_err("refused");
            assert_eq!(err.code, E_USAGE, "{bad}");
        }
    }

    #[test]
    fn every_example_parses_and_the_command_is_in_the_radio_group() {
        let spec = crate::registry::find("net_capture").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Radio);
        for example in spec.examples {
            let value = example.args_json().expect("the example is JSON");
            NetCaptureArgs::from_json(&value).expect("every example parses");
        }
    }
}
