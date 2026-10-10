//! Transport tests: the built binary is driven over stdio and HTTP with raw JSON-RPC.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const BINARY: &str = env!("CARGO_BIN_EXE_unity-micro-editor");
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;

fn project_fixture() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    for part in ["Assets/Scripts", "ProjectSettings"] {
        fs::create_dir_all(directory.path().join(part)).unwrap();
    }
    fs::write(
        directory.path().join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 6000.6.3f1\n",
    )
    .unwrap();
    directory
}
struct Stopped(Child);
impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// CommandTransport proves protocol framing is clean and no informational
// output reaches stdout.
#[test]
fn stdio_transport_starts_and_serves_tools() {
    let project = project_fixture();
    let mut child = Command::new(BINARY)
        .args(["mcp-standalone", "--project"])
        .arg(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut request = |stdin: &mut std::process::ChildStdin, line: &str| -> Value {
        writeln!(stdin, "{line}").unwrap();
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
    };
    let initialized = request(&mut stdin, INITIALIZE);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "unity-micro-editor"
    );
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    let tools = request(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 11);
    let called = request(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_capabilities","arguments":{}}}"#,
    );
    let result = &called["result"];
    assert!(result.get("isError").is_none(), "{called}");
    let reply: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(reply["success"] == true, "{reply}");
    assert_eq!(result["structuredContent"], reply);
    // Closing stdin ends the server cleanly.
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

struct Response {
    status: u16,
    body: String,
}
fn http(address: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request.push_str(&format!("Host: {address}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(request.as_bytes()).unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Response {
        status: head.split(' ').nth(1).unwrap().parse().unwrap(),
        body: body.to_string(),
    }
}

#[test]
fn streamable_http_protocol_authentication_and_origin_protection() {
    let project = project_fixture();
    let token = "a".repeat(40);
    let token_file = project.path().join("token.txt");
    fs::write(&token_file, format!("{token}\n")).unwrap();
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let child = Command::new(BINARY)
        .args([
            "mcp-standalone",
            "--transport",
            "http",
            "--listen",
            &address,
            "--project",
        ])
        .arg(project.path())
        .arg("--token-file")
        .arg(&token_file)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _stopped = Stopped(child);
    let bearer = format!("Bearer {token}");
    let started = Instant::now();
    while TcpStream::connect(&address).is_err() {
        assert!(started.elapsed() < Duration::from_secs(15), "no listener");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(http(&address, "GET", "/health", &[], "").status, 401);
    let health = http(
        &address,
        "GET",
        "/health",
        &[("Authorization", &bearer)],
        "",
    );
    assert_eq!(health.status, 200);
    assert_eq!(
        health.body,
        r#"{"status":"ready","service":"unity-micro-editor-mcp"}"#
    );
    let post = [
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/json"),
        ("Accept", "application/json, text/event-stream"),
    ];
    let initialized = http(&address, "POST", "/mcp", &post, INITIALIZE);
    assert_eq!(initialized.status, 200, "{}", initialized.body);
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "project_inspect", "arguments": {}}});
    let called = http(&address, "POST", "/mcp", &post, &call.to_string());
    assert_eq!(called.status, 200, "{}", called.body);
    let response: Value = serde_json::from_str(&called.body).unwrap();
    let reply: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(reply["success"] == true, "HTTP tool call {reply}");
    let mut cross_origin = post.to_vec();
    cross_origin.push(("Origin", "https://evil.example"));
    let refused = http(&address, "POST", "/mcp", &cross_origin, INITIALIZE);
    assert_eq!(refused.status, 403, "cross-origin body={}", refused.body);
    let mut rebinding = post.to_vec();
    rebinding.push(("Host", "evil.example"));
    let refused = http(&address, "POST", "/mcp", &rebinding, INITIALIZE);
    assert_eq!(refused.status, 403, "DNS rebinding");
}
