//! `parity-mcp-call` runs one MCP request against a loopback Streamable HTTP server.
use crate::flags::FlagSet;
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};
use std::{
    io::Read,
    time::{Duration, Instant},
};

const PROTOCOL_VERSION: &str = "2025-03-26";
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
struct Request<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: &'a RawValue,
    /// Absent for a notification.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u32>,
}

struct Client {
    endpoint: String,
    session: Option<String>,
    agent: ureq::Agent,
    /// The whole exchange shares one time budget.
    deadline: Instant,
}

/// Extracts the JSON-RPC reply from a plain JSON body or from an SSE stream, where
/// notifications may precede it.
fn decode_response(body: &[u8], content_type: &str) -> Result<String, String> {
    let text = std::str::from_utf8(body).map_err(|_| "server returned invalid JSON")?;
    if content_type.contains("text/event-stream") {
        return text
            .lines()
            .filter_map(|line| line.trim().strip_prefix("data:"))
            .map(str::trim)
            .find(|data| {
                serde_json::from_str::<Value>(data).is_ok_and(|reply| reply.get("id").is_some())
            })
            .map(str::to_owned)
            .ok_or_else(|| "SSE response has no JSON-RPC reply".into());
    }
    serde_json::from_str::<serde::de::IgnoredAny>(text)
        .map_err(|_| "server returned invalid JSON")?;
    Ok(text.to_owned())
}

/// A JSON-RPC error, a tool `isError` or a nested `success` that is not true, at any
/// level of the reply's `result`/`structuredContent` chain.
fn operation_error(reply: &Value) -> Result<(), String> {
    let envelope = match reply {
        Value::Object(envelope) => envelope,
        Value::Null => return Ok(()),
        _ => return Err("MCP reply is not a JSON object".into()),
    };
    if let Some(error) = envelope.get("error").filter(|error| !error.is_null()) {
        return Err(format!("operation error: {error}"));
    }
    if envelope.get("isError") == Some(&Value::Bool(true)) {
        return Err("MCP tool returned isError=true; see the saved reply".into());
    }
    if matches!(
        envelope.get("success"),
        Some(Value::Bool(false) | Value::Null)
    ) {
        return Err("operation returned success=false; see the saved reply".into());
    }
    for key in ["result", "structuredContent"] {
        if let Some(nested) = envelope.get(key) {
            operation_error(nested)?;
        }
    }
    Ok(())
}

/// Re-indents JSON text without reordering members or rewriting numbers and strings, so a
/// saved reply is what the server sent.
fn indent(json: &str) -> String {
    fn line(out: &mut String, depth: usize) {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    }
    let mut out = String::with_capacity(json.len() * 2);
    let (mut depth, mut in_string, mut escaped, mut opened) = (0usize, false, false, false);
    for character in json.chars() {
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if matches!(character, ' ' | '\t' | '\n' | '\r') {
            continue;
        }
        let closing = matches!(character, '}' | ']');
        // An empty object or array stays on one line.
        if std::mem::take(&mut opened) {
            if closing {
                depth -= 1;
                out.push(character);
                continue;
            }
            line(&mut out, depth);
        }
        match character {
            '{' | '[' => {
                out.push(character);
                depth += 1;
                opened = true;
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                line(&mut out, depth);
                out.push(character);
            }
            ',' => {
                out.push(character);
                line(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '"' => {
                out.push(character);
                in_string = true;
            }
            _ => out.push(character),
        }
    }
    out
}

impl Client {
    fn new(endpoint: &str, timeout: Duration) -> Self {
        Self {
            endpoint: endpoint.into(),
            session: None,
            // The endpoint is loopback: no proxy applies, and statuses are reported with
            // the server's own explanation.
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .proxy(None)
                .build()
                .into(),
            deadline: Instant::now() + timeout,
        }
    }

    /// Sends a request and returns its reply, or a notification (`id` absent) and returns
    /// nothing.
    fn request(
        &mut self,
        method: &str,
        id: Option<u32>,
        params: &RawValue,
    ) -> Result<Option<String>, String> {
        let payload = serde_json::to_vec(&Request {
            jsonrpc: "2.0",
            method,
            params,
            id,
        })
        .map_err(|error| error.to_string())?;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("{method}: MCP request timed out"));
        }
        let mut request = self
            .agent
            .post(&self.endpoint)
            .config()
            .timeout_global(Some(remaining))
            .build()
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(session) = &self.session {
            request = request
                .header("Mcp-Session-Id", session)
                .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
        let mut response = request
            .send(&payload[..])
            .map_err(|error| format!("{method}: {error}"))?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let (session, content_type) = (header("Mcp-Session-Id"), header("Content-Type"));
        if !session.is_empty() {
            self.session = Some(session);
        }
        let status = response.status();
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|error| format!("{method}: {error}"))?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err("MCP response exceeds 16 MiB".into());
        }
        if status.is_client_error() || status.is_server_error() {
            return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&body)));
        }
        if id.is_none() {
            return Ok(None);
        }
        decode_response(&body, &content_type).map(Some)
    }

    /// Ends the server session, if one was opened; a failure here changes nothing.
    fn close(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let _ = self
            .agent
            .delete(&self.endpoint)
            .config()
            .timeout_global(Some(Duration::from_secs(1)))
            .build()
            .header("Mcp-Session-Id", session)
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .call();
    }

    /// The handshake followed by the one requested call.
    fn exchange(&mut self, method: &str, params: &RawValue) -> Result<String, String> {
        let raw = |value: Value| RawValue::from_string(value.to_string()).expect("valid JSON");
        let initialize = raw(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "UnityMicroEditorParity", "version": "0.1.0"},
        }));
        let initialized = self
            .request("initialize", Some(1), &initialize)?
            .unwrap_or_default();
        operation_error(&parse(&initialized)?)?;
        self.request("notifications/initialized", None, &raw(json!({})))?;
        Ok(self.request(method, Some(2), params)?.unwrap_or_default())
    }
}

fn parse(reply: &str) -> Result<Value, String> {
    serde_json::from_str(reply).map_err(|error| error.to_string())
}

fn is_loopback_http(endpoint: &str) -> bool {
    endpoint.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        let host = uri.host().unwrap_or_default();
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        uri.scheme_str() == Some("http") && matches!(host, "127.0.0.1" | "localhost" | "::1")
    })
}

fn run(flags: &FlagSet) -> Result<(), String> {
    let mut params = flags.get_string("params");
    let params_file = flags.get_string("params-file");
    if !params_file.is_empty() {
        params = std::fs::read_to_string(&params_file)
            .map_err(|error| format!("{params_file}: {error}"))?;
    }
    let endpoint = flags.get_string("url");
    if !is_loopback_http(&endpoint) {
        return Err("only loopback HTTP MCP URLs are supported".into());
    }
    let params: Box<RawValue> =
        serde_json::from_str(&params).map_err(|_| "params must be valid JSON")?;
    let timeout = Duration::from_nanos(flags.get_duration("timeout").max(0) as u64);
    let mut client = Client::new(&endpoint, timeout);
    let result = client.exchange(&flags.get_string("method"), &params);
    client.close();
    let result = result?;
    // The reply is saved even when it reports a failure: it is the evidence.
    super::emit(&flags.get_string("output"), &(indent(&result) + "\n"))
        .map_err(|error| error.to_string())?;
    operation_error(&parse(&result)?)
}

/// Exit status 0 for a successful operation and 1 for a transport, protocol or operation
/// failure.
pub fn main(args: Vec<String>) -> i32 {
    let mut flags = FlagSet::new(&format!("unity-micro-editor {}", super::MCP_CALL_COMMAND));
    flags.string("url", "http://127.0.0.1:8087/mcp", "loopback MCP endpoint");
    flags.string("method", "tools/list", "JSON-RPC method");
    flags.string("params", "{}", "JSON-RPC params JSON");
    flags.string("params-file", "", "read JSON-RPC params from this file");
    flags.string("output", "", "optional result file");
    flags.duration("timeout", Duration::from_secs(45), "total request timeout");
    if let Err(status) = super::parse(&mut flags, args) {
        return status;
    }
    match run(&flags) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        io::Write,
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
    };

    /// One request received by the stub server.
    #[derive(Debug, Clone)]
    pub struct Received {
        pub method: String,
        pub headers: HashMap<String, String>,
        pub body: Value,
    }

    fn receive(stream: &mut TcpStream) -> Option<Received> {
        let mut data = Vec::new();
        let mut chunk = [0; 4096];
        let end = loop {
            if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                break end;
            }
            let count = stream.read(&mut chunk).ok().filter(|count| *count > 0)?;
            data.extend_from_slice(&chunk[..count]);
        };
        let head = String::from_utf8_lossy(&data[..end]).into_owned();
        let mut lines = head.lines();
        let method = lines.next()?.split(' ').next()?.to_owned();
        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let length = headers
            .get("content-length")
            .and_then(|length| length.parse().ok())
            .unwrap_or(0);
        let mut body = data[end + 4..].to_vec();
        while body.len() < length {
            let count = stream.read(&mut chunk).ok().filter(|count| *count > 0)?;
            body.extend_from_slice(&chunk[..count]);
        }
        Some(Received {
            method,
            headers,
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        })
    }

    /// A Streamable HTTP MCP stub: it opens a session, answers requests over SSE with
    /// `result(request)`, accepts notifications and closes the session on DELETE. Returns
    /// its endpoint and the requests it has received.
    pub fn serve(
        result: impl Fn(&Value) -> Value + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<Received>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let Some(request) = receive(&mut stream) else {
                    continue;
                };
                log.lock().unwrap().push(request.clone());
                let response = if request.method == "DELETE" {
                    "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_owned()
                } else if request.body.get("id").is_none() {
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    let reply = json!({"jsonrpc": "2.0", "id": request.body["id"], "result": result(&request.body)});
                    let body = format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}}\n\nevent: message\ndata: {reply}\n\n"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nMcp-Session-Id: reference-session\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (endpoint, received)
    }

    fn empty() -> Box<RawValue> {
        RawValue::from_string("{}".into()).unwrap()
    }

    #[test]
    fn decode_sse_skips_notification() {
        let input = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\r\n\r\n";
        let result = decode_response(input.as_bytes(), "text/event-stream; charset=utf-8").unwrap();
        assert_eq!(
            parse(&result).unwrap(),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"ok": true}})
        );
    }

    #[test]
    fn session_headers_and_notification_identity() {
        let (endpoint, received) = serve(|_| json!({}));
        let mut client = Client::new(&endpoint, Duration::from_secs(10));
        let initialized = client.request("initialize", Some(1), &empty()).unwrap();
        assert_eq!(
            parse(&initialized.unwrap()).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "result": {}})
        );
        let notified = client.request("notifications/initialized", None, &empty());
        assert_eq!(notified, Ok(None));
        client.request("tools/list", Some(2), &empty()).unwrap();
        client.close();
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 4);
        assert!(!received[0].headers.contains_key("mcp-session-id"));
        for request in &received[1..] {
            assert_eq!(request.headers["mcp-session-id"], "reference-session");
            assert_eq!(request.headers["mcp-protocol-version"], PROTOCOL_VERSION);
        }
        assert_eq!(received[1].body["method"], "notifications/initialized");
        assert!(
            received[1].body.get("id").is_none(),
            "notification has request id"
        );
        assert_eq!(received[2].body["id"], 2);
        assert_eq!(received[3].method, "DELETE");
    }

    #[test]
    fn operation_errors_are_not_reported_as_success() {
        for input in [
            json!({"error": {"code": -32602}}),
            json!({"result": {"isError": true}}),
            json!({"result": {"structuredContent": {"success": false}}}),
        ] {
            assert!(operation_error(&input).is_err(), "accepted failure {input}");
        }
        let success =
            json!({"result": {"structuredContent": {"success": true, "data": {"error": null}}}});
        assert_eq!(operation_error(&success), Ok(()));
    }

    #[test]
    fn decode_rejects_incomplete_response() {
        for (body, kind) in [
            (r#"{"invalid""#, "application/json"),
            (
                "event: message\ndata: {\"method\":\"notification\"}\n",
                "text/event-stream",
            ),
        ] {
            assert!(
                decode_response(body.as_bytes(), kind).is_err(),
                "invalid or incomplete response accepted: {body}"
            );
        }
    }

    #[test]
    fn indent_keeps_members_and_literals_as_sent() {
        assert_eq!(
            indent(r#"{"b":1.50,"a":[],"c":{"d":"x, {y}: \"z\""},"e":[1,{}] }"#),
            "{\n  \"b\": 1.50,\n  \"a\": [],\n  \"c\": {\n    \"d\": \"x, {y}: \\\"z\\\"\"\n  },\n  \"e\": [\n    1,\n    {}\n  ]\n}"
        );
    }

    #[test]
    fn only_loopback_http_endpoints_are_accepted() {
        for endpoint in [
            "http://127.0.0.1:8087/mcp",
            "http://localhost:8087/mcp",
            "http://[::1]:8087/mcp",
        ] {
            assert!(is_loopback_http(endpoint), "{endpoint}");
        }
        for endpoint in [
            "https://127.0.0.1:8087/mcp",
            "http://192.168.1.2:8087/mcp",
            "http://example.com/mcp",
            "127.0.0.1:8087",
            "",
        ] {
            assert!(!is_loopback_http(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn call_saves_the_reply_and_reports_tool_failure_in_the_status() {
        let (endpoint, received) = serve(
            |request| json!({"isError": request["params"]["name"] == "failing", "echo": request["params"]}),
        );
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("reply.json");
        let call = |name: &str| {
            let params = json!({"name": name, "arguments": {"big": 9007199254740993u64}});
            main(
                [
                    "-url",
                    &endpoint,
                    "-method",
                    "tools/call",
                    "-params",
                    &params.to_string(),
                    "-output",
                    output.to_str().unwrap(),
                ]
                .map(String::from)
                .to_vec(),
            )
        };
        assert_eq!(call("working"), 0);
        let saved = std::fs::read_to_string(&output).unwrap();
        assert!(saved.contains("9007199254740993"), "{saved}");
        assert_eq!(parse(&saved).unwrap()["result"]["isError"], false);
        assert_eq!(call("failing"), 1);
        let saved = parse(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(saved["result"]["echo"]["name"], "failing");
        let methods: Vec<_> = received
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                request.body["method"]
                    .as_str()
                    .unwrap_or("DELETE")
                    .to_owned()
            })
            .collect();
        let once = [
            "initialize",
            "notifications/initialized",
            "tools/call",
            "DELETE",
        ];
        assert_eq!(methods, [once, once].concat());
    }
}
