//! MCP over streamable HTTP, mounted at `/mcp`, answered by the same [`crate::mcp_stdio::Mcp`] as
//! stdio.
//!
//! `/mcp` passes the same token, `Host` and `Origin` checks as every route: a page that could POST
//! JSON-RPC to the daemon would be a full agent. The `Mcp-Method`/`Mcp-Name` routing headers are
//! echoed but never trusted over the body. `GET /mcp` is 405 rather than an event stream, because
//! the server has no server-initiated message to send.

use std::sync::Arc;

use crate::http::{JSON, Request, Response, Server};
use crate::mcp_stdio::{Mcp, RpcError, RpcRequest};

/// MCP elicitation over streamable HTTP: always `Unavailable`, since this mount has no event
/// stream to send a request on. A confirmation falls through to the dialog or terminal path.
#[derive(Clone, Copy, Debug, Default)]
pub struct HttpElicitation;

impl pemu_planner::flow::ElicitationPort for HttpElicitation {
    fn elicit(&mut self, _message: &str) -> pemu_planner::flow::Elicited {
        pemu_planner::flow::Elicited::Unavailable(
            "MCP over streamable HTTP here answers each request once and carries no request to the \
             client, so it cannot ask; use `passportsim mcp` (stdio) with an elicitation-capable \
             client, or the dialog or terminal path"
                .to_owned(),
        )
    }
}

/// Header a `2026-07-28` client names the method in.
pub const METHOD_HEADER: &str = "mcp-method";

/// Header a `2026-07-28` client names the tool in.
pub const NAME_HEADER: &str = "mcp-name";

pub const VERSION_HEADER: &str = "mcp-protocol-version";

/// Header naming the tool groups of one `/mcp` request, as a `--caps` list (`audio,nfc`).
/// Streamable HTTP is stateless per request, so the groups travel with each request; without the
/// header the session has the daemon's `serve --caps`.
pub const CAPS_HEADER: &str = "passportsim-caps";

pub fn with_caps_header(mcp: Mcp, request: &Request) -> Result<Mcp, Response> {
    let Some(list) = request.headers.get(CAPS_HEADER) else {
        return Ok(mcp);
    };
    match crate::mcp_stdio::parse_caps(list) {
        Ok(caps) => Ok(mcp.with_caps(&caps)),
        Err(why) => Err(Response::json(
            400,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": serde_json::Value::Null,
                "error": RpcError::new(-32600, format!("header `{CAPS_HEADER}`: {why}")).to_json(),
            }),
        )),
    }
}

/// Serves one `POST /mcp` request. The caller has already run [`Server::authorize`].
pub fn handle(mcp: &mut Mcp, request: &Request) -> Response {
    let value: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(e) => {
            return Response::json(
                400,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": RpcError::new(-32700, e.to_string()).to_json(),
                }),
            );
        }
    };
    // A batch answers with the array of non-notification answers; an all-notification batch is
    // 202 with no body.
    let (responses, batch) = match &value {
        serde_json::Value::Array(items) => (
            items
                .iter()
                .filter_map(|item| answer(mcp, item))
                .collect::<Vec<_>>(),
            true,
        ),
        single => (answer(mcp, single).into_iter().collect::<Vec<_>>(), false),
    };
    let version = mcp.negotiated().to_string();
    let response = if responses.is_empty() {
        Response::new(202, JSON, Vec::new())
    } else if batch {
        Response::json(200, &serde_json::Value::Array(responses))
    } else {
        Response::json(200, &responses[0])
    };
    let response = response.header(VERSION_HEADER, version);
    match request.headers.get(METHOD_HEADER) {
        Some(method) => response.header(METHOD_HEADER, method),
        None => response,
    }
}

fn answer(mcp: &mut Mcp, value: &serde_json::Value) -> Option<serde_json::Value> {
    match RpcRequest::parse(value) {
        Ok(request) => mcp.handle(&request),
        Err(why) => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": value.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "error": RpcError::new(-32600, why).to_json(),
        })),
    }
}

/// The `/mcp` mount: the auth checks, then [`handle`].
pub fn serve(server: &Arc<Server>, mcp: &mut Mcp, request: &Request) -> Response {
    if let Err(e) = server.authorize(request) {
        return Response::unauthorized(&e);
    }
    if request.method != crate::http::Method::Post {
        return Response::json(
            405,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": serde_json::Value::Null,
                "error": RpcError::new(-32600, "the MCP mount takes POST; it opens no event stream").to_json(),
            }),
        )
        .header("allow", "POST");
    }
    handle(mcp, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Auth, Token};
    use crate::daemon::Shutdown;
    use crate::http::Method;
    use crate::mcp_stdio::{FALLBACK_VERSION, PROTOCOL_VERSIONS, TOOL_PREFIX, parse_caps};
    use crate::pool::Pool;
    use pemu_api::error::{ApiError, E_TIMEOUT};
    use pemu_api::output::Output;
    use pemu_api::receipt::Receipt;
    use pemu_api::spec::{
        Annotations, CapsGroup, CliShape, CommandSpec, Example, HandlerCx, any_schema,
    };
    use std::collections::BTreeSet;

    fn echo(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
        Ok(Output::new(
            serde_json::json!({ "echo": args }),
            "echoed",
            Receipt::default(),
        ))
    }

    fn times_out(_cx: &mut HandlerCx, _args: serde_json::Value) -> Result<Output, ApiError> {
        Err(
            ApiError::new(E_TIMEOUT, "until {ui:{text:\"Wi-Fi\"}} not met")
                .with_hint("call passport_inspect"),
        )
    }

    const fn spec(
        name: &'static str,
        group: CapsGroup,
        annotations: Annotations,
        handler: pemu_api::spec::Handler,
    ) -> CommandSpec {
        CommandSpec {
            name,
            group,
            summary: "A command the MCP tests dispatch to.",
            input_schema: any_schema,
            output_schema: any_schema,
            annotations,
            cli: CliShape::EMPTY,
            scenario_step: None,
            examples: &[Example {
                title: "example",
                args: "{}",
            }],
            errors: &[],
            handler,
        }
    }

    static SPECS: &[CommandSpec] = &[
        spec(
            "input",
            CapsGroup::Core,
            Annotations {
                needs_instance: true,
                idempotent: true,
                ..Annotations::EMPTY
            },
            echo,
        ),
        spec("run", CapsGroup::Core, Annotations::EMPTY, times_out),
        spec(
            "flash_device",
            CapsGroup::Device,
            Annotations {
                destructive: true,
                human_confirm: true,
                native_only: true,
                ..Annotations::EMPTY
            },
            echo,
        ),
    ];

    fn token() -> Token {
        Token::from_bytes([0x21; 32])
    }

    fn server_with(caps: BTreeSet<CapsGroup>) -> Arc<Server> {
        Arc::new(
            Server::new(
                Auth::new(token(), 8765),
                Arc::new(Pool::new(4)),
                Shutdown::new(),
                caps,
            )
            .with_commands(SPECS),
        )
    }

    fn post(body: serde_json::Value) -> Request {
        Request::new(Method::Post, "/mcp")
            .header("host", "127.0.0.1:8765")
            .header("authorization", format!("Bearer {}", token().to_hex()))
            .body(body.to_string())
    }

    fn rpc(id: u32, method: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn body(response: &Response) -> serde_json::Value {
        serde_json::from_slice(&response.body).expect("a JSON body")
    }

    #[test]
    fn initialize_negotiates_a_supported_version_and_falls_back_otherwise() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        for version in PROTOCOL_VERSIONS {
            let response = serve(
                &server,
                &mut mcp,
                &post(rpc(
                    1,
                    "initialize",
                    serde_json::json!({ "protocolVersion": version }),
                )),
            );
            assert_eq!(response.status, 200);
            assert_eq!(body(&response)["result"]["protocolVersion"], version);
            assert_eq!(response.get(VERSION_HEADER), Some(version));
        }
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(
                1,
                "initialize",
                serde_json::json!({ "protocolVersion": "1999-01-01" }),
            )),
        );
        assert_eq!(
            body(&response)["result"]["protocolVersion"],
            FALLBACK_VERSION
        );
        let caps = &body(&response)["result"]["capabilities"];
        assert!(caps.get("tools").is_some());
        assert!(caps.get("sampling").is_none());
        assert!(caps.get("roots").is_none());
        assert!(caps.get("logging").is_none());
    }

    #[test]
    fn tools_list_is_filtered_by_caps_and_maps_the_annotations_to_hints() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(2, "tools/list", serde_json::json!({}))),
        );
        let tools = body(&response)["result"]["tools"].clone();
        let names: Vec<String> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["name"].as_str().expect("name").to_string())
            .collect();
        assert_eq!(
            names,
            [format!("{TOOL_PREFIX}input"), format!("{TOOL_PREFIX}run")],
            "the Device tool is absent without `--caps device`"
        );
        assert_eq!(tools[0]["annotations"]["idempotentHint"], true);
        assert_eq!(tools[0]["annotations"]["destructiveHint"], false);

        let server = server_with(parse_caps("device").expect("caps"));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(2, "tools/list", serde_json::json!({}))),
        );
        let tools = body(&response)["result"]["tools"].clone();
        let device = tools
            .as_array()
            .expect("array")
            .iter()
            .find(|t| t["name"] == format!("{TOOL_PREFIX}flash_device"))
            .expect("the device tool");
        assert_eq!(device["annotations"]["destructiveHint"], true);
        assert_eq!(device["annotations"]["openWorldHint"], true);
        assert_eq!(device["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn a_session_narrows_the_tool_list_and_refuses_calls_outside_its_groups() {
        let server = Arc::new(
            Server::new(
                Auth::new(token(), 8765),
                Arc::new(Pool::new(4)),
                Shutdown::new(),
                parse_caps("device").expect("caps"),
            )
            .with_commands(SPECS)
            .with_mcp_caps(&BTreeSet::from([CapsGroup::Core])),
        );
        let names = |response: &Response| -> Vec<String> {
            body(response)["result"]["tools"]
                .as_array()
                .expect("array")
                .iter()
                .map(|t| t["name"].as_str().expect("name").to_string())
                .collect()
        };
        let mut mcp = Mcp::new(Arc::clone(&server));
        let list = serve(
            &server,
            &mut mcp,
            &post(rpc(1, "tools/list", serde_json::json!({}))),
        );
        assert!(!names(&list).contains(&format!("{TOOL_PREFIX}flash_device")));
        let call = serve(
            &server,
            &mut mcp,
            &post(rpc(
                2,
                "tools/call",
                serde_json::json!({ "name": format!("{TOOL_PREFIX}flash_device"), "arguments": {} }),
            )),
        );
        let result = &body(&call)["result"];
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(result["structuredContent"]["error"]["code"], "E_STATE");
        assert!(
            result["structuredContent"]["error"]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("--caps device")),
            "{result}"
        );

        let request =
            post(rpc(3, "tools/list", serde_json::json!({}))).header(CAPS_HEADER, "device");
        let mut mcp =
            with_caps_header(Mcp::new(Arc::clone(&server)), &request).expect("a valid list");
        let list = serve(&server, &mut mcp, &request);
        assert!(names(&list).contains(&format!("{TOOL_PREFIX}flash_device")));

        let request = post(rpc(4, "tools/list", serde_json::json!({}))).header(CAPS_HEADER, "wifi");
        let refused = with_caps_header(Mcp::new(Arc::clone(&server)), &request).expect_err("400");
        assert_eq!(refused.status, 400);

        // Only the MCP surface is narrowed: the REST route still runs the Device command.
        assert!(server.caps().contains(&CapsGroup::Device));
    }

    #[test]
    fn a_tool_call_addresses_its_instance_by_an_ordinary_argument() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let id = server.pool().host_for_test("p901");
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(
                3,
                "tools/call",
                serde_json::json!({
                    "name": format!("{TOOL_PREFIX}input"),
                    "arguments": { "instance": id.to_string(), "button": "down" }
                }),
            )),
        );
        let result = body(&response)["result"].clone();
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["echo"]["button"], "down");
        assert!(
            result["structuredContent"]["echo"]
                .get("instance")
                .is_none()
        );
        // The text is the shaped text with a one-line receipt, not the JSON again: a doubled body
        // would exceed the 4,000-character default text budget.
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert!(
            text.starts_with("echoed\n"),
            "the shaped text of the command, got {text:?}"
        );
        assert!(
            !text.contains("\"echo\""),
            "the structured JSON must not travel a second time as text: {text:?}"
        );
        server
            .pool()
            .stop(id, pemu_core::time::VTime::default())
            .expect("stop");
    }

    #[test]
    fn a_failed_command_is_an_is_error_result_not_a_json_rpc_error() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(
                4,
                "tools/call",
                serde_json::json!({ "name": format!("{TOOL_PREFIX}run"), "arguments": {} }),
            )),
        );
        let answer = body(&response);
        assert!(
            answer.get("error").is_none(),
            "a guest failure must not become a transport error"
        );
        let result = &answer["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["error"]["code"], "E_TIMEOUT");
        assert_eq!(
            result["structuredContent"]["error"]["hint"],
            "call passport_inspect"
        );
    }

    #[test]
    fn an_unknown_method_and_an_unknown_tool_are_told_apart() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(5, "resources/list", serde_json::json!({}))),
        );
        assert_eq!(body(&response)["error"]["code"], -32601);
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(
                6,
                "tools/call",
                serde_json::json!({ "name": "not_ours", "arguments": {} }),
            )),
        );
        assert_eq!(body(&response)["error"]["code"], -32602);
    }

    #[test]
    fn server_discover_needs_the_2026_protocol() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        serve(
            &server,
            &mut mcp,
            &post(rpc(
                1,
                "initialize",
                serde_json::json!({ "protocolVersion": "2025-06-18" }),
            )),
        );
        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(7, "server/discover", serde_json::json!({}))),
        );
        assert_eq!(body(&response)["error"]["code"], -32601);

        let response = serve(
            &server,
            &mut mcp,
            &post(rpc(
                8,
                "server/discover",
                serde_json::json!({ "_meta": { "protocolVersion": "2026-07-28" } }),
            )),
        );
        let result = &body(&response)["result"];
        assert_eq!(result["serverInfo"]["name"], "passportsim");
        assert_eq!(result["protocolVersions"][0], "2026-07-28");
        assert!(result["tools"].as_array().expect("tools").len() >= 2);
    }

    #[test]
    fn a_notification_gets_no_response_body() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
        );
        assert_eq!(response.status, 202);
        assert!(response.body.is_empty());
    }

    #[test]
    fn a_batch_answers_only_the_requests_that_have_an_id() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let response = serve(
            &server,
            &mut mcp,
            &post(serde_json::json!([
                rpc(10, "ping", serde_json::json!({})),
                { "jsonrpc": "2.0", "method": "notifications/initialized" },
                rpc(11, "ping", serde_json::json!({})),
            ])),
        );
        let answers = body(&response);
        let answers = answers.as_array().expect("an array of answers");
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["id"], 10);
        assert_eq!(answers[1]["id"], 11);
    }

    #[test]
    fn the_mcp_mount_is_guarded_like_every_other_route() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let request = Request::new(Method::Post, "/mcp")
            .header("host", "127.0.0.1:8765")
            .body(rpc(1, "tools/list", serde_json::json!({})).to_string());
        assert_eq!(serve(&server, &mut mcp, &request).status, 401);
        let request = post(rpc(1, "tools/list", serde_json::json!({})))
            .header("origin", "http://evil.example");
        assert_eq!(serve(&server, &mut mcp, &request).status, 403);
        // A foreign `Host` with the right token: the DNS-rebinding shape.
        let request = Request::new(Method::Post, "/mcp")
            .header("host", "evil.example:8765")
            .header("authorization", format!("Bearer {}", token().to_hex()))
            .body(rpc(1, "tools/list", serde_json::json!({})).to_string());
        assert_eq!(serve(&server, &mut mcp, &request).status, 403);
        let request = Request::new(Method::Get, "/mcp")
            .header("host", "127.0.0.1:8765")
            .header("authorization", format!("Bearer {}", token().to_hex()));
        let response = serve(&server, &mut mcp, &request);
        assert_eq!(response.status, 405);
        assert_eq!(response.get("allow"), Some("POST"));
    }

    #[test]
    fn the_routing_headers_are_echoed_and_never_replace_the_body() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let request = post(rpc(12, "ping", serde_json::json!({})))
            .header(METHOD_HEADER, "tools/call")
            .header(NAME_HEADER, "passport_flash_device");
        let response = serve(&server, &mut mcp, &request);
        assert_eq!(body(&response)["result"], serde_json::json!({}));
        assert_eq!(response.get(METHOD_HEADER), Some("tools/call"));
    }

    #[test]
    fn a_body_that_is_not_json_is_a_parse_error() {
        let server = server_with(BTreeSet::from([CapsGroup::Core]));
        let mut mcp = Mcp::new(Arc::clone(&server));
        let request = Request::new(Method::Post, "/mcp")
            .header("host", "127.0.0.1:8765")
            .header("authorization", format!("Bearer {}", token().to_hex()))
            .body("{oops");
        let response = serve(&server, &mut mcp, &request);
        assert_eq!(response.status, 400);
        assert_eq!(body(&response)["error"]["code"], -32700);
    }

    #[test]
    fn elicitation_over_streamable_http_is_unavailable() {
        use pemu_planner::flow::{ElicitationPort, Elicited};
        assert!(matches!(
            HttpElicitation.elicit("Flash plan ab..?"),
            Elicited::Unavailable(hint) if hint.contains("dialog or terminal path")
        ));
    }
}
