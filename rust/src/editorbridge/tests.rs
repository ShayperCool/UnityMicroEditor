use super::*;
use crate::sysutil::testing::{
    helper_arguments, notify_signals, shell_quote, signalled, temp_dir, write_executable,
};
use serde_json::{Value, json};
use std::{collections::HashMap, env, fs, path::Path, process};
use tempfile::TempDir;

const HELPER: &str = "editorbridge::tests::editor_bridge_helper";
const BRIDGE: &str = "Library/UnityMicroEditor/editor-bridge";

fn fixture_json(path: &str, data: &[u8]) -> io::Result<()> {
    fs::create_dir_all(sysutil::dir(path))?;
    let temporary = format!("{path}.temp");
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)
}
// Bounds for outcomes that are expected to arrive: the nominal seconds, stretched so a
// loaded machine does not turn a slow step into a failure.
const PATIENCE: u64 = 10;
fn seconds(count: u64) -> Context {
    Context::background().with_timeout(Duration::from_secs(count * PATIENCE))
}
fn millis(count: u64) -> Context {
    Context::background().with_timeout(Duration::from_millis(count))
}

// A real subprocess implements the mailbox without Unity. Actual Editor tests
// are opt-in and never share fixture output with these lifecycle/receipt tests.
#[test]
fn editor_bridge_helper() {
    if env::var("UNITY_EDITOR_BRIDGE_TEST_HELPER").as_deref() != Ok("1") {
        return;
    }
    let args: Vec<Vec<u8>> = env::args().map(String::into_bytes).collect();
    let project = String::from_utf8(argument(&args, "-projectPath").to_vec()).unwrap();
    let editor = env::var("UNITY_EDITOR_BRIDGE_TEST_EDITOR").unwrap_or_default();
    let mailbox = sysutil::join(&[&project, BRIDGE]);
    notify_signals();
    let pid = i64::from(process::id());
    let mode = fs::read_to_string(sysutil::join(&[&project, "helper-mode"])).unwrap_or_default();
    if mode == "early-exit" {
        process::exit(7);
    }
    let owner = env::var("UNITY_MICRO_EDITOR_BRIDGE_OWNER").unwrap_or_default();
    let mut heartbeat_project = project.clone();
    if mode == "wrong-project" {
        heartbeat_project.push_str("-wrong");
    }
    let heartbeat = json!({
        "schemaVersion": 1, "backend": "unity_editor", "project": heartbeat_project, "unityVersion": PROFILE,
        "editor": editor, "pid": pid, "ownerToken": owner, "batchMode": true, "isCompiling": false, "isUpdating": false,
    });
    if fixture_json(
        &sysutil::join(&[&mailbox, "ready.json"]),
        &jsondoc::encode(&heartbeat),
    )
    .is_err()
    {
        process::exit(8);
    }
    struct Job {
        request: Envelope,
        data: Vec<u8>,
        due: Instant,
        drop: bool,
    }
    let completed = |id: &str, data: &[u8]| Response {
        schema_version: 1,
        backend: "unity_editor".into(),
        request_id: id.into(),
        request_sha256: crate::sha256(data),
        project: project.clone(),
        unity_version: PROFILE.into(),
        editor: editor.clone(),
        pid,
        status: "completed".into(),
        ..Response::default()
    };
    let mut pending: HashMap<String, Job> = HashMap::new();
    let mut executed = 0;
    loop {
        let stop = sysutil::join(&[&mailbox, &format!("owner-{owner}.stop.json")]);
        if fs::metadata(&stop).is_ok() {
            let _ = fs::remove_file(&stop);
            process::exit(0);
        }
        if signalled() {
            process::exit(0);
        }
        let due: Vec<String> = pending
            .iter()
            .filter(|(_, job)| !job.drop && Instant::now() >= job.due)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            let job = pending.remove(&id).unwrap();
            let arguments: Value =
                serde_json::from_slice(&job.request.arguments).unwrap_or_default();
            let mut response = completed(&id, &job.data);
            response.success = true;
            response.result = match job.request.action.as_str() {
                "export_graph" => jsondoc::encode(
                    &json!({"schemaVersion": 1, "project": project, "unity": editor, "assemblies": []}),
                ),
                "scene_proof" => jsondoc::encode(
                    &json!({"schemaVersion": 1, "scenePath": "Assets/Test.unity", "sceneDirty": false}),
                ),
                _ => br#"{"fixture":true}"#.to_vec(),
            };
            if arguments["badHash"] == json!(true) {
                response.request_sha256 = "0".repeat(64);
            }
            let path = sysutil::join(&[&mailbox, &format!("{id}.response.json")]);
            if fixture_json(&path, &jsondoc::marshal(&response).unwrap()).is_err() {
                process::exit(8);
            }
        }
        let mut requests: Vec<String> = fs::read_dir(&mailbox)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".request.json"))
            .collect();
        requests.sort();
        for name in requests {
            let path = sysutil::join(&[&mailbox, &name]);
            let Ok(data) = fs::read(&path) else {
                continue;
            };
            let Ok(request) = decode_envelope(&data) else {
                continue;
            };
            let id = request.id.clone();
            let response_path = sysutil::join(&[&mailbox, &format!("{id}.response.json")]);
            if fs::metadata(&response_path).is_ok() {
                let _ = fs::remove_file(&path);
                continue;
            }
            if pending.contains_key(&id) {
                continue;
            }
            let claim = sysutil::join(&[&mailbox, &format!("{id}.accepted.json")]);
            if fs::metadata(&claim).is_ok() {
                let mut response = completed(&id, &data);
                response.error = Some(Failure {
                    code: "outcome_unknown".into(),
                    message: "Accepted receipt is unavailable.".into(),
                });
                let _ = fixture_json(&response_path, &jsondoc::marshal(&response).unwrap());
                let _ = fs::remove_file(&path);
                continue;
            }
            if fs::rename(&path, &claim).is_err() {
                continue;
            }
            executed += 1;
            let _ = fixture_json(
                &sysutil::join(&[&mailbox, "executions.json"]),
                executed.to_string().as_bytes(),
            );
            let arguments: Value = serde_json::from_slice(&request.arguments).unwrap_or_default();
            let delay = arguments["delayMs"].as_f64().unwrap_or(0.0);
            pending.insert(
                id,
                Job {
                    request,
                    data,
                    due: Instant::now() + Duration::from_millis(delay as u64),
                    drop: arguments["dropResponse"] == json!(true),
                },
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct Fixture {
    client: Client,
    _project: TempDir,
}
impl std::ops::Deref for Fixture {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let closed = self.client.close();
        if !thread::panicking() {
            closed.unwrap();
        }
    }
}
fn version_line(ending: &str) -> String {
    format!("m_EditorVersion: {PROFILE}{ending}")
}
fn write_version(project: &str, text: &str) {
    fs::create_dir_all(sysutil::join(&[project, "ProjectSettings"])).unwrap();
    fs::write(
        sysutil::join(&[project, "ProjectSettings/ProjectVersion.txt"]),
        text,
    )
    .unwrap();
}
fn client_fixture(mode: &str) -> Fixture {
    let (directory, project) = temp_dir();
    write_version(&project, &version_line("\n"));
    fs::write(sysutil::join(&[&project, "helper-mode"]), mode).unwrap();
    let binary = env::current_exe()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    let editor = sysutil::join(&[&project, "Unity"]);
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = '-version' ]; then printf '{PROFILE}\\n'; exit 0; fi\nexec env UNITY_EDITOR_BRIDGE_TEST_HELPER=1 UNITY_EDITOR_BRIDGE_TEST_EDITOR={} {} {} \"$@\"\n",
        shell_quote(&editor),
        shell_quote(&binary),
        helper_arguments(HELPER).join(" "),
    );
    write_executable(&editor, &script);
    let client = Client::new(
        &Context::background(),
        Options {
            project,
            editor,
            startup_timeout: Duration::from_secs(5 * PATIENCE),
            poll_interval: Duration::from_millis(5),
        },
    )
    .unwrap();
    client.0.state.lock().unwrap().stop_grace = Duration::from_millis(200);
    Fixture {
        client,
        _project: directory,
    }
}
fn request(id: &str, action: &str, arguments: Value) -> Request {
    Request {
        id: id.into(),
        action: action.into(),
        arguments: if arguments.is_null() {
            Vec::new()
        } else {
            jsondoc::encode(&arguments)
        },
        expected_pid: 0,
    }
}
// A delivered request whose short wait ended first. On a loaded machine the wait
// can end before the helper claims the request; repeating the same immutable
// request reads the same pending state again.
fn pending_accepted(c: &Client, request: impl Fn() -> Request) -> Response {
    for _ in 0..500 {
        match c.do_request(&millis(30), request()) {
            Ok(response) if response.accepted => return response,
            Ok(_) => {}
            Err(error) if error.is_deadline_exceeded() => {}
            Err(error) => panic!("{error}"),
        }
    }
    panic!("request was never accepted");
}
fn owned_pid(client: &Client) -> Option<i64> {
    client
        .0
        .state
        .lock()
        .unwrap()
        .command
        .map(|command| command.pid)
}
fn executions(client: &Client) -> String {
    fs::read_to_string(sysutil::join(&[client.mailbox(), "executions.json"])).unwrap_or_default()
}

#[test]
fn project_version_parsing() {
    for (name, text, valid) in [
        ("lf", version_line("\n"), true),
        (
            "crlf",
            format!(
                "{}m_EditorVersionWithRevision: {PROFILE} (hash)\r\n",
                version_line("\r\n")
            ),
            true,
        ),
        ("no-final-newline", version_line(""), true),
        (
            "line-whitespace",
            format!("  {}", version_line(" \r\n")),
            true,
        ),
        (
            "wrong-version",
            "m_EditorVersion: 6000.6.4f1\n".into(),
            false,
        ),
        ("suffix", version_line("-extra\n"), false),
        ("duplicate", version_line("\n").repeat(2), false),
        (
            "conflicting",
            format!("{}m_EditorVersion: 6000.6.4f1\n", version_line("\n")),
            false,
        ),
        (
            "embedded",
            format!("unrelated: {}", version_line("\n")),
            false,
        ),
        (
            "missing",
            format!("m_EditorVersionWithRevision: {PROFILE} (hash)\n"),
            false,
        ),
    ] {
        let outcome = validate_project_version(text.as_bytes());
        assert_eq!(outcome.is_ok(), valid, "{name}: {outcome:?}");
    }
    let c = client_fixture("");
    write_version(&c.options().project, &version_line("\r\n"));
    let second = Client::new(&Context::background(), c.options().clone())
        .expect("CRLF project rejected before bridge creation");
    second.close().unwrap();
}

#[test]
fn rejects_executable_profile_before_cache_mutation() {
    for (name, script) in [
        (
            "wrong-profile",
            "#!/bin/sh\nprintf '6000.6.4f1\\n'\n".to_string(),
        ),
        (
            "excess-output",
            format!(
                "#!/bin/sh\nprintf '{PROFILE}\\n'\ni=0\nwhile [ $i -lt 1000 ]; do printf 'unexpected-output'; i=$((i+1)); done\n"
            ),
        ),
    ] {
        let (_directory, project) = temp_dir();
        write_version(&project, &version_line("\r\n"));
        let editor = sysutil::join(&[&project, "Unity"]);
        write_executable(&editor, &script);
        let options = Options {
            project: project.clone(),
            editor,
            ..Options::default()
        };
        match Client::new(&Context::background(), options) {
            Ok(client) => {
                let _ = client.close();
                panic!("{name}: wrong executable was accepted before authored installation");
            }
            Err(error) => assert!(
                error
                    .message()
                    .starts_with("Editor executable does not report pinned Unity"),
                "{name}: {error}"
            ),
        }
        assert!(
            !Path::new(&sysutil::join(&[&project, "Library"])).exists(),
            "{name}: failed executable preflight created a cache directory"
        );
    }
}

#[test]
fn owned_process_and_persistent_receipt() {
    let c = client_fixture("");
    let ctx = seconds(5);
    let connection = c.own_process_or_attach(&ctx).unwrap();
    assert!(
        connection.owned && connection.pid >= 1,
        "owned startup: {connection:?}"
    );
    let response = c
        .do_request(&ctx, request("immutable-once", "status", Value::Null))
        .unwrap();
    assert!(response.success, "first command: {response:?}");
    c.close().unwrap();
    let fresh = Client::new(&Context::background(), c.options().clone()).unwrap();
    let replay = fresh
        .do_request(&ctx, request("immutable-once", "status", Value::Null))
        .unwrap();
    assert!(
        replay.success && replay.pid == connection.pid,
        "restart lost persisted receipt: {replay:?}"
    );
    assert_eq!(replay, response);
    assert!(
        owned_pid(&fresh).is_none(),
        "reading historic receipt started another Editor"
    );
    assert!(
        fresh
            .do_request(&ctx, request("immutable-once", "refresh", Value::Null))
            .is_err(),
        "request ID reused for another action"
    );
    assert_eq!(executions(&c), "1", "receipt retry executed twice");
    fresh.close().unwrap();
}

#[test]
fn pending_retry_and_response_loss_never_reexecutes() {
    let c = client_fixture("");
    let connection = c.own_process_or_attach(&seconds(5)).unwrap();
    let slow = || request("slow-once", "build_project", json!({"delayMs": 300}));
    let pending = pending_accepted(&c, slow);
    assert!(
        pending.status == "result_pending" && pending.accepted,
        "accepted cancellation hidden: {pending:?}"
    );
    let reply = c.do_request(&seconds(5), slow()).unwrap();
    assert!(
        reply.success,
        "retry did not obtain original completion: {reply:?}"
    );
    fs::remove_file(&reply.receipt).unwrap();
    let unknown = c.do_request(&seconds(5), slow()).unwrap();
    assert!(
        !unknown.success
            && unknown
                .error
                .as_ref()
                .is_some_and(|e| e.code == "outcome_unknown"),
        "response loss reexecuted or claimed success: {unknown:?}"
    );
    assert_eq!(executions(&c), "1", "receipt loss executed twice");
    assert_eq!(
        owned_pid(&c),
        Some(connection.pid),
        "command retry replaced owned Editor"
    );
}

#[test]
fn accepted_request_unknown_after_process_exit() {
    let c = client_fixture("");
    c.own_process_or_attach(&seconds(5)).unwrap();
    let dropped = || request("unknown-once", "build_host", json!({"dropResponse": true}));
    let pending = pending_accepted(&c, dropped);
    assert!(pending.accepted, "{pending:?}");
    c.close().unwrap();
    let fresh = Client::new(&Context::background(), c.options().clone()).unwrap();
    let response = fresh.do_request(&Context::background(), dropped()).unwrap();
    assert!(
        response.status == "result_unknown" && owned_pid(&fresh).is_none(),
        "old accepted ID replayed in new Editor: {response:?}"
    );
    fresh.close().unwrap();
}

#[test]
fn rejects_protocol_drift_and_unsafe_paths() {
    let c = client_fixture("");
    let ctx = seconds(5);
    let drift = c.do_request(
        &ctx,
        request("bad-hash", "status", json!({"badHash": true})),
    );
    assert_eq!(
        drift.unwrap_err().message(),
        "Editor receipt identity, bounds or result is incompatible"
    );
    for arguments in [
        br#"{"x":1,"x":2}"#.to_vec(),
        b"[]".to_vec(),
        b"{} {}".to_vec(),
        vec![b' '; REQUEST_LIMIT + 1],
    ] {
        let malformed = Request {
            action: "status".into(),
            arguments,
            ..Request::default()
        };
        assert!(
            c.do_request(&ctx, malformed).is_err(),
            "malformed/oversized arguments accepted"
        );
    }
    assert!(
        c.do_request(&ctx, request("", "execute_code", Value::Null))
            .is_err(),
        "generic code evaluation allowed"
    );
    assert!(
        c.do_request(&ctx, request("", "prefab_prepare", Value::Null))
            .is_err(),
        "prefab preparation allowed on authored target"
    );
    let (_private, private) = temp_dir();
    let target = sysutil::join(&[&private, "private"]);
    fs::write(&target, "private").unwrap();
    let link = sysutil::join(&[c.mailbox(), "link.response.json"]);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let root = c.open_root().unwrap();
    assert!(
        read(&root, "link.response.json", RESPONSE_LIMIT).is_err(),
        "response symlink followed"
    );
    assert!(
        publish(&root, "link.response.json", b"replace").is_err(),
        "response symlink overwritten"
    );
    assert_eq!(fs::read(&target).unwrap(), b"private");
    for root_name in ["Library", "ProjectSettings"] {
        let (_directory, project) = temp_dir();
        write_version(&project, &version_line("\n"));
        if root_name == "ProjectSettings" {
            fs::remove_dir_all(sysutil::join(&[&project, root_name])).unwrap();
        }
        let (_elsewhere, elsewhere) = temp_dir();
        std::os::unix::fs::symlink(&elsewhere, sysutil::join(&[&project, root_name])).unwrap();
        let options = Options {
            project,
            editor: c.options().editor.clone(),
            ..Options::default()
        };
        assert!(
            Client::new(&Context::background(), options).is_err(),
            "symlink project path accepted"
        );
    }
}

#[test]
fn startup_failure_and_client_lifetime_are_bounded() {
    let c = client_fixture("early-exit");
    let ctx = seconds(5);
    let failed = c
        .own_process_or_attach(&ctx)
        .expect_err("failed batch startup marked ready");
    assert!(
        failed.message().contains("exit status 7"),
        "startup failure lost the exit reason: {failed}"
    );
    let live = client_fixture("");
    let connection = live.own_process_or_attach(&ctx).unwrap();
    live.0.lifetime.cancel();
    let deadline = Instant::now() + Duration::from_secs(5 * PATIENCE);
    while Instant::now() < deadline && process(connection.pid).is_ok() {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        process(connection.pid).is_err(),
        "cancelled client retained its owned Editor"
    );
    let restarted = live.own_process_or_attach(&ctx);
    assert!(
        restarted.as_ref().is_err_and(Error::is_canceled),
        "closed lifetime restarted Editor: {restarted:?}"
    );
}

#[test]
fn startup_lock_and_import_worker_discovery() {
    let c = client_fixture("");
    let lock = c.startup_lock(&Context::background()).unwrap();
    let other = Client::new(&Context::background(), c.options().clone()).unwrap();
    let blocked = other.own_process_or_attach(&millis(30));
    assert!(
        blocked.as_ref().is_err_and(Error::is_deadline_exceeded),
        "another client ignored startup ownership: {blocked:?}"
    );
    drop(lock);
    let mut worker = Command::new(env::current_exe().unwrap())
        .args(helper_arguments(HELPER))
        .args(["-projectPath", &c.options().project])
        .args(["-name", "AssetImportWorkerHW0", "-parentPid", "123"])
        .env("UNITY_EDITOR_BRIDGE_TEST_HELPER", "1")
        .env("UNITY_EDITOR_BRIDGE_TEST_EDITOR", &c.options().editor)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let worker_pid = i64::from(worker.id());
    let deadline = Instant::now() + Duration::from_secs(5 * PATIENCE);
    let outcome = loop {
        let ready = read_path(&sysutil::join(&[c.mailbox(), "ready.json"]), 65536)
            .ok()
            .and_then(|data| decode_heartbeat(&data).ok());
        if ready.is_some_and(|ready| ready.pid == worker_pid) {
            break Ok(());
        }
        if Instant::now() > deadline {
            break Err("import worker helper did not become live");
        }
        thread::sleep(Duration::from_millis(5));
    };
    let found = c.find_editor();
    let connection = c.read_connection();
    let _ = sysutil::kill(worker_pid, libc::SIGTERM);
    worker.wait().unwrap();
    other.close().unwrap();
    outcome.unwrap();
    assert_eq!(
        found.unwrap(),
        0,
        "Unity import worker mistaken for an Editor"
    );
    assert!(
        connection.is_err(),
        "import worker heartbeat accepted as Editor"
    );
}

#[test]
fn staging_marker_cannot_alias_source_project() {
    let c = client_fixture("");
    let project = c.options().project.clone();
    let path = sysutil::join(&[&project, "ProjectSettings/UnityMicroEditorStaging.json"]);
    let marker = |source: &str| {
        fixture_json(
            &path,
            &jsondoc::encode(&json!({"schemaVersion": 1, "sourceProject": source})),
        )
        .unwrap()
    };
    for source in [
        project.clone(),
        format!("{project}/../{}", sysutil::base(&project)),
        "/absent-unity-editor-source".to_string(),
    ] {
        marker(&source);
        assert!(
            c.validate_staging().is_err(),
            "invalid staging source accepted: {source}"
        );
    }
    let (_source, source) = temp_dir();
    marker(&source);
    c.validate_staging().unwrap();
}

#[test]
fn preparation_and_no_overwrite_result() {
    let c = client_fixture("");
    let ctx = seconds(5);
    let prepared = c.prepare(&ctx, "Assets/Test.unity").unwrap();
    assert!(
        prepared.backend == "unity_editor"
            && jsondoc::valid(&prepared.graph)
            && jsondoc::valid(&prepared.scene_proof),
        "preparation export: {prepared:?}"
    );
    let response = c
        .do_request(&ctx, request("", "status", Value::Null))
        .unwrap();
    let project = &c.options().project;
    let path = sysutil::join(&[project, "Library/UnityMicroEditor/exports/status.json"]);
    c.save_result(&response, &path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"{\"fixture\":true}\n");
    assert!(
        c.save_result(&response, &path).is_err(),
        "Editor result replaced accepted output"
    );
    assert!(
        c.save_result(&response, &sysutil::join(&[project, "Assets/status.json"]))
            .is_err(),
        "Editor result published into authored assets"
    );
}

#[test]
fn canonical_arguments_and_wire_documents() {
    for (input, canonical) in [
        ("", "{}"),
        (
            " { \"b\" : [ ] , \"a\" : [1, 2.50, -0, 1E3] } ",
            r#"{"a":[1,2.50,-0,1E3],"b":null}"#,
        ),
        (
            r#"{"z":{"<k>":"a&b"},"a":"é "}"#,
            "{\"a\":\"é\\u2028\",\"z\":{\"\\u003ck\\u003e\":\"a\\u0026b\"}}",
        ),
        (
            r#"{"n":null,"t":true,"f":false,"o":{}}"#,
            r#"{"f":false,"n":null,"o":{},"t":true}"#,
        ),
    ] {
        assert_eq!(
            String::from_utf8(canonical_arguments(input.as_bytes()).unwrap()).unwrap(),
            canonical
        );
    }
    for (input, message) in [
        (r#"{"x":1,"x":2}"#, "duplicate JSON argument key"),
        (r#"{"a":{"x":1,"x":2}}"#, "duplicate JSON argument key"),
        ("[]", "Editor arguments must be a JSON object"),
        ("null", "Editor arguments must be a JSON object"),
        ("{} {}", "trailing Editor argument JSON"),
    ] {
        assert_eq!(
            canonical_arguments(input.as_bytes()).unwrap_err().message(),
            message
        );
    }
    let nested = |depth: usize| format!("{{\"a\":{}{}}}", "[".repeat(depth), "]".repeat(depth));
    assert!(canonical_arguments(nested(128).as_bytes()).is_ok());
    assert_eq!(
        canonical_arguments(nested(129).as_bytes())
            .unwrap_err()
            .message(),
        "JSON nesting exceeds bounds"
    );
    let envelope = Envelope {
        schema_version: 1,
        id: "id-1".into(),
        project: "/p/a&b".into(),
        unity_version: PROFILE.into(),
        editor: "/e/<Unity>".into(),
        expected_pid: 42,
        action: "status".into(),
        arguments: b"{}".to_vec(),
    };
    let encoded = envelope.encode();
    assert_eq!(
        String::from_utf8(encoded.clone()).unwrap(),
        format!(
            "{{\"schemaVersion\":1,\"requestId\":\"id-1\",\"project\":\"/p/a\\u0026b\",\"unityVersion\":\"{PROFILE}\",\"editor\":\"/e/\\u003cUnity\\u003e\",\"expectedPid\":42,\"action\":\"status\",\"arguments\":{{}}}}"
        )
    );
    let decoded = decode_envelope(&encoded).unwrap();
    assert_eq!(
        (decoded.project.as_str(), decoded.expected_pid),
        ("/p/a&b", 42)
    );
    assert_eq!(
        decode_envelope(br#"{"schemaVersion":"1"}"#)
            .unwrap_err()
            .message(),
        "json: cannot unmarshal string into field envelope.schemaVersion of type int"
    );
    assert_eq!(
        decode_envelope(b"[]").unwrap_err().message(),
        "json: cannot unmarshal array into value of type envelope"
    );
    assert_eq!(
        decode_envelope(b"{} x").unwrap_err().message(),
        "trailing Editor artifact JSON"
    );
    let (response, success) = decode_response(
        br#"{"SchemaVersion":1,"success":true,"Success":false,"result": {"b":1, "a":[ ]},"error":{"Code":"c","message":"m"},"error":{"code":"d"}}"#,
    )
    .unwrap();
    assert_eq!(
        (
            response.schema_version,
            response.success,
            success.as_slice()
        ),
        (1, false, &b"true"[..])
    );
    assert_eq!(response.result, br#"{"b":1, "a":[ ]}"#);
    assert_eq!(
        response.error,
        Some(Failure {
            code: "d".into(),
            message: "m".into()
        })
    );
    assert_eq!(
        String::from_utf8(jsondoc::marshal(&response).unwrap()).unwrap(),
        r#"{"schemaVersion":1,"backend":"","requestId":"","requestSha256":"","project":"","unityVersion":"","editor":"","pid":0,"status":"","success":false,"accepted":false,"receipt":"","result":{"b":1,"a":[]},"error":{"code":"d","message":"m"}}"#
    );
    assert_eq!(
        String::from_utf8(jsondoc::marshal(&Preparation::default()).unwrap()).unwrap(),
        r#"{"backend":"","pid":0,"graph":null,"sceneProof":null}"#
    );
}

fn preloaded_settings(project: &str) -> String {
    let data = read_path(
        &sysutil::join(&[project, "ProjectSettings/ProjectSettings.asset"]),
        RESPONSE_LIMIT,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&data).into_owned();
    let mut entries: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if entries.is_empty() {
            if line.starts_with("  preloadedAssets:") {
                entries.push(line);
            }
            continue;
        }
        if line.starts_with("  - ") || line.starts_with("    ") {
            entries.push(line);
            continue;
        }
        break;
    }
    assert!(
        !entries.is_empty(),
        "actual project settings omitted preloadedAssets"
    );
    entries.join("\n")
}

// Opt-in integration against a real Editor; skipped unless an explicit
// disposable project and matching Unity executable are supplied.
#[test]
fn actual_editor() {
    let variable = |name: &str| env::var(name).unwrap_or_default();
    let (project, editor) = (
        variable("UNITY_EDITOR_BRIDGE_PROJECT"),
        variable("UNITY_EDITOR_BRIDGE_EDITOR"),
    );
    if project.is_empty() || editor.is_empty() {
        return;
    }
    let c = Client::new(
        &Context::background(),
        Options {
            project: project.clone(),
            editor,
            startup_timeout: Duration::from_secs(180),
            ..Options::default()
        },
    )
    .unwrap();
    let ctx = seconds(240);
    let connection = c.own_process_or_attach(&ctx).unwrap();
    let graph_id = format!("actual-{}", sysutil::new_id());
    let response = c
        .do_request(&ctx, request(&graph_id, "export_graph", Value::Null))
        .unwrap();
    assert!(response.success, "actual graph: {response:?}");
    let graph: Value = serde_json::from_slice(&response.result).unwrap();
    let count = |name: &str| graph[name].as_array().map_or(0, Vec::len);
    let native_only = variable("UNITY_EDITOR_BRIDGE_NATIVE_ONLY") == "1";
    assert!(
        count("assemblies") + count("hostAssemblies") != 0
            && (count("assemblies") == 0) == native_only,
        "actual Editor did not export Player assembly graph"
    );
    for script in graph["scripts"].as_array().into_iter().flatten() {
        assert!(
            !(script["hostProvided"] == json!(true)
                && script["typeName"]
                    == json!("UnityEngine.InputSystem.Editor.RemoteInputPlayerConnectionEditor")),
            "Editor-only InputSystem ScriptableSingleton was exported as a Player MonoScript"
        );
    }
    let scene = variable("UNITY_EDITOR_BRIDGE_SCENE");
    if !scene.is_empty() {
        let proof = c
            .do_request(&ctx, request("", "scene_proof", json!({"scene": scene})))
            .unwrap();
        assert!(proof.success, "actual scene proof: {proof:?}");
    }
    if variable("UNITY_EDITOR_BRIDGE_COMPONENTS") == "1" {
        for component in [
            "UnityEngine.BoxCollider",
            "UnityEngine.Camera",
            "UnityEngine.ParticleSystem",
        ] {
            let schema = c
                .do_request(
                    &ctx,
                    request("", "component_schema", json!({"componentType": component})),
                )
                .unwrap();
            assert!(
                schema.success,
                "actual component schema {component}: {schema:?}"
            );
            let result: Value = serde_json::from_slice(&schema.result).unwrap();
            let template = read_path(
                result["templatePath"].as_str().unwrap_or(""),
                RESPONSE_LIMIT,
            )
            .unwrap();
            assert!(
                template.windows(5).any(|window| window == b"%YAML")
                    && jsondoc::valid(result["serializedJson"].as_str().unwrap_or("").as_bytes()),
                "actual native component template invalid {component}"
            );
        }
    }
    let action = variable("UNITY_EDITOR_BRIDGE_BUILD_ACTION");
    if !action.is_empty() {
        assert!(
            action == "build_project" || action == "build_host",
            "invalid explicit integration build action"
        );
        let preloads = preloaded_settings(&project);
        let settings_path = sysutil::join(&[&project, "ProjectSettings/ProjectSettings.asset"]);
        let settings_before = read_path(&settings_path, RESPONSE_LIMIT).unwrap();
        let arguments = json!({"scenes": [scene], "development": action != "build_host"});
        let build = c.do_request(&ctx, request("", &action, arguments)).unwrap();
        assert!(build.success, "actual build: {build:?}");
        assert_eq!(
            preloaded_settings(&project),
            preloads,
            "actual build changed authored preload identities"
        );
        if action == "build_host" {
            let settings_after = read_path(&settings_path, RESPONSE_LIMIT).unwrap();
            assert!(
                settings_before == settings_after,
                "host did not restore the exact authored settings bytes"
            );
            let profile: Value = serde_json::from_slice(&build.result).unwrap();
            assert!(
                profile["development"] == json!(true) && profile["hostDevelopment"] == json!(true),
                "native host must use the development agent profile even when release was requested"
            );
            let post_build = c
                .do_request(&ctx, request("", "export_graph", Value::Null))
                .unwrap();
            assert!(post_build.success, "post-host graph failed: {post_build:?}");
            let closure: Value = serde_json::from_slice(&post_build.result).unwrap();
            assert!(
                closure["compilerInputs"][&settings_path] == json!(crate::sha256(&settings_before)),
                "post-host graph hashed temporary settings instead of restored authored configuration"
            );
        }
    }
    if variable("UNITY_EDITOR_BRIDGE_NATIVE") == "1" {
        assert!(
            connection.batch_mode,
            "actual native export must use isolated batch fixture"
        );
        let mut arguments = json!({"scenes": [scene]});
        let assets = variable("UNITY_EDITOR_BRIDGE_NATIVE_ASSETS");
        if !assets.is_empty() {
            arguments["assetPaths"] = json!(assets.split(',').collect::<Vec<_>>());
        }
        let native = c
            .do_request(&ctx, request("", "native_prepare", arguments))
            .unwrap();
        assert!(native.success, "actual native export: {native:?}");
        let catalog: Value = serde_json::from_slice(&native.result).unwrap();
        let flag = |name: &str| catalog[name] == json!(true);
        let packs = catalog["packs"].as_array().cloned().unwrap_or_default();
        let roots = catalog["sourceRoots"].as_array().map_or(0, Vec::len);
        let empty = variable("UNITY_EDITOR_BRIDGE_NATIVE_EMPTY") == "1";
        let incomplete = if empty {
            flag("nativeAssetsPresent") || !packs.is_empty() || roots == 0
        } else {
            !flag("nativeAssetsPresent") || packs.is_empty()
        };
        assert!(
            !flag("containsScenes") && flag("cookedRegistryVerified") && !incomplete,
            "native export omitted cooked verification or embedded scenes"
        );
        for pack in &packs {
            let size =
                fs::metadata(pack["bundlePath"].as_str().unwrap_or("")).map(|info| info.len());
            assert!(
                size.is_ok_and(|size| json!(size) == pack["bytes"]),
                "native bundle mismatch"
            );
        }
        let data = read_path(
            catalog["catalogPath"].as_str().unwrap_or(""),
            RESPONSE_LIMIT,
        )
        .unwrap();
        assert!(jsondoc::valid(&data), "native catalog invalid");
        assert!(
            !Path::new(&sysutil::join(&[&project, "Assets/__UnityPlayImports"])).exists(),
            "native exporter did not remove owned scratch"
        );
    }
    c.close().unwrap();
    if !connection.owned {
        assert!(
            process(connection.pid).is_ok(),
            "closing attached client killed another Editor"
        );
    }
}

// Byte-for-byte agreement with the recorded reference cases (see sysutil::oracle).
#[test]
fn recorded_cases_agree_on_documents_and_receipts() {
    use crate::sysutil::oracle::compare;
    let message = |error: Error| error.to_string();
    compare("canonicalArguments", |case| {
        canonical_arguments(&case.bytes("data")).map_err(message)
    });
    compare("validateProjectVersion", |case| {
        validate_project_version(&case.bytes("data"))
            .map(|()| Vec::new())
            .map_err(message)
    });
    compare("isOwnedCachePath", |case| {
        let owned = sysutil::is_owned_cache_path(case.text("project"), case.text("path"));
        Ok(owned.to_string().into_bytes())
    });
    compare("envelope", |case| {
        // An empty raw JSON field cannot be encoded; `do_request` never builds one.
        if !case.error.is_empty() {
            return Err(case.error.clone());
        }
        let envelope = Envelope {
            schema_version: 1,
            id: case.text("id").into(),
            project: case.text("project").into(),
            unity_version: PROFILE.into(),
            editor: case.text("editor").into(),
            expected_pid: case.number("pid"),
            action: case.text("action").into(),
            arguments: case.bytes("arguments"),
        };
        Ok(envelope.encode())
    });
    compare("decodeEnvelope", |case| {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let e = decode_envelope(&case.bytes("data")).map_err(message)?;
        Ok(jsondoc::encode(&json!({
            "schemaVersion": e.schema_version, "requestId": e.id, "project": e.project, "unityVersion": e.unity_version,
            "editor": e.editor, "expectedPid": e.expected_pid, "action": e.action, "arguments": STANDARD.encode(&e.arguments),
        })))
    });
    compare("decodeHeartbeat", |case| {
        let h = decode_heartbeat(&case.bytes("data")).map_err(message)?;
        Ok(format!(
            "{{\"schemaVersion\":{},\"backend\":{},\"project\":{},\"unityVersion\":{},\"editor\":{},\"pid\":{},\"ownerToken\":{},\"batchMode\":{},\"isCompiling\":{},\"isUpdating\":{}}}",
            h.schema_version,
            jsondoc::quote(&h.backend),
            jsondoc::quote(&h.project),
            jsondoc::quote(&h.unity_version),
            jsondoc::quote(&h.editor),
            h.pid,
            jsondoc::quote(&h.owner_token),
            h.batch_mode,
            h.is_compiling,
            h.is_updating
        )
        .into_bytes())
    });
    let (_directory, directory) = temp_dir();
    let client = Client(Arc::new(Inner {
        options: Options {
            project: "/p".into(),
            editor: "/e".into(),
            ..Options::default()
        },
        mailbox: "/mailbox".into(),
        lifetime: Context::background(),
        start: Mutex::new(()),
        state: Mutex::new(State::default()),
    }));
    let root = Root::open(&directory).unwrap();
    compare("readResponse", |case| {
        fs::write(
            sysutil::join(&[&directory, "r.response.json"]),
            case.bytes("data"),
        )
        .unwrap();
        let request = Envelope {
            id: "r".into(),
            expected_pid: 7,
            ..Envelope::default()
        };
        let response = client
            .read_response(&root, &request, case.text("hash"))
            .map_err(message)?;
        jsondoc::marshal(&response)
    });
}

/// The import workers an owned Editor leaves behind are ended with it; those
/// of another Editor or project are not (stage 5 mcp5-4).
#[test]
fn import_workers_of_the_editor_are_ended() {
    let (_guard, project) = crate::sysutil::testing::temp_dir();
    let project = crate::sysutil::eval_symlinks(&project).unwrap();
    let (_other_guard, other) = crate::sysutil::testing::temp_dir();
    let spawn = |parent: &str, path: &str| {
        std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "while :; do sleep 1; done",
                "sh",
                "-name",
                "AssetImportWorker0",
                "-parentPid",
                parent,
                "-projectPath",
                path,
            ])
            .spawn()
            .unwrap()
    };
    let mut ours = spawn("4194301", &project);
    let mut theirs = spawn("4194302", &project);
    let mut elsewhere = spawn("4194301", &other);
    std::thread::sleep(Duration::from_millis(100));
    end_import_workers(4194301, &project);
    let ended = (0..60).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        ours.try_wait().unwrap().is_some()
    });
    assert!(ended, "the worker of the Editor still runs");
    assert!(theirs.try_wait().unwrap().is_none() && elsewhere.try_wait().unwrap().is_none());
    for child in [&mut theirs, &mut elsewhere] {
        let _ = child.kill();
        let _ = child.wait();
    }
}
