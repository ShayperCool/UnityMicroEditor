use super::*;
use crate::sysutil::testing::{
    helper_arguments, notify_signals, shell_quote, signalled, temp_dir, write_executable,
};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    os::fd::FromRawFd,
    path::Path,
    process,
    time::Instant,
};
use tempfile::TempDir;

const HELPER: &str = "agentcontrol::tests::controller_helper_process";

fn helper_write(path: &str, value: &Value) -> io::Result<()> {
    fs::create_dir_all(sysutil::dir(path))?;
    let temporary = format!("{path}.temporary");
    fs::write(&temporary, jsondoc::encode(value))?;
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

// This subprocess implements the runner/file protocol without Unity. Each
// fixture has its own project, output, process group, and request artifacts.
// The wrapper script passes the controller's stdout pipe as descriptor 3 and
// discards the test harness's own stdout.
#[test]
fn controller_helper_process() {
    if env::var("UNITY_AGENT_CONTROL_HELPER").as_deref() != Ok("1") {
        return;
    }
    let args: Vec<String> = env::args().collect();
    let option = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1))
            .cloned()
            .unwrap_or_default()
    };
    let (project, output) = (option("--project"), option("--output"));
    let mode = fs::read_to_string(sysutil::join(&[&project, "helper-mode"])).unwrap_or_default();
    notify_signals();
    let idle = |exit_on_signal: bool| -> ! {
        loop {
            if signalled() && exit_on_signal {
                process::exit(0);
            }
            thread::sleep(Duration::from_millis(2));
        }
    };
    if mode == "child" {
        idle(true);
    }
    // SAFETY: descriptor 3 is the inherited event pipe and is used only here.
    let mut events = unsafe {
        libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
        File::from_raw_fd(3)
    };
    match mode.as_str() {
        "overflow" => {
            let _ = events.write_all(&vec![b'x'; EVENT_LINE_MAXIMUM + 1]);
            idle(false);
        }
        "truncated" => {
            let _ = events.write_all(br#"{"event":"ready""#);
            process::exit(0);
        }
        "empty-exit" => process::exit(0),
        "stderr-exit" => {
            let mut stderr = io::stderr();
            let _ = stderr
                .write_all(format!("discarded-prefix-marker{}\n", "x".repeat(40000)).as_bytes());
            let _ = stderr.write_all(b"native loader actual failure reason\x00 invalidUTF8:\xff\n");
            process::exit(1);
        }
        _ => {}
    }
    let manifest = sysutil::join(&[
        &project,
        match mode.as_str() {
            "library-manifest" => "Library/UnityMicroEditor/manifests/rev-1/scene.json",
            "foreign-library-manifest" => "Library/ScriptAssemblies/rev-1/scene.json",
            _ => "Temp/manifests/rev-1/scene.json",
        },
    ]);
    let report = sysutil::join(&[&output, "runs/first/report.json"]);
    let control = sysutil::join(&[&output, "runs/first/control"]);
    if let Err(error) = fs::create_dir_all(&control) {
        eprintln!("{error}");
        process::exit(4);
    }
    if helper_write(&manifest, &json!({"revision": "rev-1"})).is_err() {
        process::exit(4);
    }
    let mut pid = i64::from(process::id());
    if mode == "spawn-child" {
        let child_project = sysutil::join(&[&project, "child"]);
        fs::create_dir_all(&child_project).unwrap();
        fs::write(sysutil::join(&[&child_project, "helper-mode"]), "child").unwrap();
        let child = Command::new(env::current_exe().unwrap())
            .args(helper_arguments(HELPER))
            .args(["--project", &child_project])
            .process_group(0)
            .spawn();
        match child {
            Ok(child) => pid = i64::from(child.id()),
            Err(_) => process::exit(4),
        }
    }
    let mut ready = json!({
        "schemaVersion": 1, "pid": pid, "controlDirectory": control,
        "scene": {"isLoaded": true}, "errors": 0, "exceptions": 0,
    });
    if mode == "bad-pid" {
        ready["pid"] = json!(pid + 100000);
    }
    if mode == "symlink-control" {
        let escaped = sysutil::join(&[&project, "outside"]);
        fs::create_dir_all(&escaped).unwrap();
        fs::remove_dir(&control).unwrap();
        std::os::unix::fs::symlink(&escaped, &control).unwrap();
    }
    if helper_write(&format!("{report}.ready.json"), &ready).is_err() {
        process::exit(4);
    }
    let mut emit = |event: &Value| {
        let mut line = jsondoc::encode(event);
        line.push(b'\n');
        let _ = events.write_all(&line);
    };
    emit(&json!({"event": "ready", "pid": pid, "manifest": manifest, "report": report}));
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: HashMap<String, Instant> = HashMap::new();
    let mut executions = 0;
    let mut last_event = Vec::new();
    loop {
        if signalled() && mode != "ignore-signals" {
            process::exit(0);
        }
        if let Ok(data) = fs::read(sysutil::join(&[&output, "helper-event.json"]))
            && data != last_event
        {
            last_event = data;
            if let Ok(event) = serde_json::from_slice::<Value>(&last_event) {
                if event["event"] == json!("crash") {
                    process::exit(7);
                }
                emit(&event);
            }
        }
        let mut requests: Vec<String> = fs::read_dir(&control)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".request.json"))
            .collect();
        requests.sort();
        for name in requests {
            let Ok(payload) = fs::read(sysutil::join(&[&control, &name])) else {
                continue;
            };
            let Ok(request) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            let id = request["requestId"].as_str().unwrap_or("").to_string();
            let arguments = &request["arguments"];
            let response = sysutil::join(&[&control, &format!("{id}.response.json")]);
            if fs::metadata(&response).is_ok() {
                continue;
            }
            if seen.insert(id.clone()) {
                executions += 1;
                let _ = helper_write(
                    &sysutil::join(&[&output, "executions.json"]),
                    &json!(executions),
                );
                let delay = arguments["delayMs"].as_f64().unwrap_or(0.0);
                if delay > 0.0 {
                    pending.insert(
                        id.clone(),
                        Instant::now() + Duration::from_millis(delay as u64),
                    );
                }
            }
            if pending.get(&id).is_some_and(|due| Instant::now() < *due) {
                continue;
            }
            if arguments["dropResponse"] == json!(true) {
                continue;
            }
            let mut hash = crate::sha256(&payload);
            if arguments["badHash"] == json!(true) {
                hash = "0".repeat(64);
            }
            let (mut success, mut failure) = (true, Value::Null);
            if request["expectedPid"] != json!(pid) {
                (success, failure) = (false, json!({"code": "stale_process"}));
            }
            if request["operation"] != json!("status")
                && request["expectedRevision"] != json!("rev-1")
            {
                (success, failure) = (false, json!({"code": "stale_revision"}));
            }
            let revision = arguments["responseRevision"].as_str().unwrap_or("rev-1");
            let _ = helper_write(
                &response,
                &json!({
                    "schemaVersion": 1, "requestId": id, "requestSha256": hash, "pid": pid, "revision": revision,
                    "success": success, "result": {"executions": executions}, "error": failure,
                }),
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct Fixture {
    controller: Controller,
    _project: TempDir,
}
impl std::ops::Deref for Fixture {
    type Target = Controller;
    fn deref(&self) -> &Controller {
        &self.controller
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let closed = self.controller.close();
        if !thread::panicking() {
            closed.unwrap();
        }
    }
}
fn shorten_grace(controller: &Controller) {
    let mut inner = controller.lock();
    inner.stop_grace = Duration::from_millis(40);
    inner.terminate_grace = Duration::from_millis(40);
}
fn controller_fixture(mode: &str) -> Fixture {
    let (directory, project) = temp_dir();
    fs::write(sysutil::join(&[&project, "helper-mode"]), mode).unwrap();
    let binary = env::current_exe()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    let runner = sysutil::join(&[&project, "runner"]);
    write_executable(
        &runner,
        &format!(
            "#!/bin/sh\nexec env UNITY_AGENT_CONTROL_HELPER=1 {} {} \"$@\" 3>&1 >/dev/null\n",
            shell_quote(&binary),
            helper_arguments(HELPER).join(" "),
        ),
    );
    let controller = Controller::new(
        &Context::background(),
        Options {
            project,
            runner,
            ..Options::default()
        },
    )
    .unwrap();
    shorten_grace(&controller);
    Fixture {
        controller,
        _project: directory,
    }
}
fn start_ready(c: &Controller) -> State {
    c.start().unwrap();
    match c.wait(&seconds(3), 0, true) {
        Ok(state) => state,
        Err(failure) => panic!("ready: {:?} {}", failure.state, failure.error),
    }
}
// A request whose short wait is meant to end first. On a loaded machine the wait
// can end before the command is even delivered; repeating the same immutable
// request then delivers it.
fn request_briefly(c: &Controller, wait: u64, args: &BTreeMap<String, Vec<u8>>) -> Value {
    for _ in 0..500 {
        match c.request(&millis(wait), "custom", args) {
            Ok(result) => return result,
            Err(error) if error.is_deadline_exceeded() => {}
            Err(error) => panic!("{error}"),
        }
    }
    panic!("request was never delivered");
}
fn request_args(value: Value) -> BTreeMap<String, Vec<u8>> {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, item)| (key.clone(), jsondoc::encode(item)))
        .collect()
}
fn emit_fixture_event(c: &Controller, event: Value) {
    helper_write(
        &sysutil::join(&[&c.0.options.output, "helper-event.json"]),
        &event,
    )
    .unwrap();
}
fn executions(c: &Controller) -> String {
    fs::read_to_string(sysutil::join(&[&c.0.options.output, "executions.json"])).unwrap_or_default()
}
fn alive(pid: i64) -> bool {
    sysutil::kill(pid, 0).is_ok()
}
fn updated(pid: i64) -> Value {
    json!({"event": "updated", "ack": {"success": true, "restartRequired": false, "loadedRevision": "rev-2", "pid": pid}})
}

#[test]
fn lifecycle_and_lifetime_cancellation() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let started = c.start().unwrap();
    assert!(
        started.session_id == ready.session_id && started.pid == ready.pid,
        "start duplicated an owned runner"
    );
    c.0.lifetime.cancel();
    let deadline = Instant::now() + Duration::from_secs(3 * PATIENCE);
    while c.status().phase != "stopped" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let state = c.status();
    assert!(
        state.phase == "stopped" && state.pid == 0,
        "cancelled controller retained process: {state:?}"
    );
    assert!(!alive(ready.pid), "owned runner was not reaped");
    let restarted = c.start();
    assert!(
        restarted.as_ref().is_err_and(Error::is_canceled),
        "closed lifetime allowed another process: {restarted:?}"
    );
}

#[test]
fn prepared_replacement_readiness_acknowledges_only_configuration_update() {
    let c = controller_fixture("");
    let prepare = |name: &str| {
        let baseline = sysutil::join(&[
            &c.0.options.project,
            "Library/UnityMicroEditor/preparations",
            name,
            "current.json",
        ]);
        fs::create_dir_all(sysutil::dir(&baseline)).unwrap();
        fs::write(&baseline, "{}").unwrap();
        c.configure_prepared(&baseline, "Assets/Main.unity")
            .unwrap();
    };
    prepare("first");
    let initial = start_ready(&c);
    assert_eq!(
        initial.last_update_sequence, 0,
        "initial readiness was reported as a source update"
    );
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "message": "regenerate source graph"}),
    );
    let ctx = seconds(3);
    let rejected = c
        .wait(&ctx, initial.last_sequence, false)
        .expect_err("expected graph rejection")
        .state;
    c.stop(&ctx).unwrap();
    let _ = fs::remove_file(sysutil::join(&[&c.0.options.output, "helper-event.json"]));
    prepare("second");
    c.start().unwrap();
    let confirmed = c.wait(&ctx, rejected.last_sequence, false).unwrap();
    assert!(
        confirmed.phase == "ready"
            && confirmed.pid != initial.pid
            && confirmed.last_update_sequence > rejected.last_sequence
            && confirmed.update_error.is_empty(),
        "prepared replacement did not confirm an update: {confirmed:?}"
    );
    assert_eq!(
        confirmed
            .events
            .last()
            .unwrap()
            .data
            .get("preparationActivated"),
        Some(&json!(true)),
        "configuration activation was not identified in the readiness event"
    );
    c.stop(&ctx).unwrap();
    c.start().unwrap();
    let restarted = c.wait(&ctx, confirmed.last_sequence, true).unwrap();
    assert_eq!(
        restarted.last_update_sequence, 0,
        "ordinary restart manufactured a source update"
    );
}

#[test]
fn stop_continues_cleanup_after_caller_timeout() {
    let c = controller_fixture("ignore-signals");
    let ready = start_ready(&c);
    let interrupted = c.stop(&millis(5));
    assert!(
        interrupted
            .as_ref()
            .is_err_and(|failure| failure.error.is_deadline_exceeded()),
        "stop should return caller deadline while background cleanup continues: {interrupted:?}"
    );
    let state = c.stop(&seconds(3)).unwrap();
    assert_eq!(
        state.phase, "stopped",
        "bounded force stop not reaped: {state:?}"
    );
    assert!(!alive(ready.pid), "force-stopped runner remains");
}

#[test]
fn wait_distinguishes_progress_completion_and_rejection() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    emit_fixture_event(&c, json!({"event": "compiling", "stage": "Roslyn"}));
    let progress = c.wait(&millis(80), ready.last_sequence, false);
    assert!(
        progress
            .as_ref()
            .is_err_and(|failure| failure.error.is_deadline_exceeded()),
        "progress falsely acknowledged update: {progress:?}"
    );
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS1002: expected semicolon"}),
    );
    let rejection = c
        .wait(&seconds(1), ready.last_sequence, false)
        .expect_err("rejection was not reported");
    let state = rejection.state;
    assert!(
        rejection.error.message().contains("CS1002")
            && state.phase == "ready"
            && state.pid == ready.pid,
        "rejection lost accepted Player identity: {state:?} {}",
        rejection.error
    );
    let after = state.last_sequence;
    emit_fixture_event(&c, updated(ready.pid));
    let state = c.wait(&seconds(1), after, false).unwrap();
    assert!(
        state.revision == "rev-2" && state.last_update_sequence > after,
        "confirmed update was not published: {state:?}"
    );
}

#[test]
fn source_undo_acknowledges_accepted_revision() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS1002: expected semicolon"}),
    );
    let rejected = c
        .wait(&seconds(1), ready.last_sequence, false)
        .expect_err("compile rejection was not observable")
        .state;
    assert!(!rejected.update_error.is_empty());
    // Restoring the accepted source bytes needs no reload or new readiness
    // artifact. The runner must still confirm which live revision it retained.
    emit_fixture_event(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision}),
    );
    let recovered = c.wait(&seconds(1), rejected.last_sequence, false).unwrap();
    assert!(
        recovered.phase == "ready"
            && recovered.pid == ready.pid
            && recovered.revision == ready.revision
            && recovered.last_update_sequence > rejected.last_sequence
            && recovered.update_error.is_empty()
            && recovered.last_error.is_empty(),
        "source undo did not acknowledge retained Player: {recovered:?}"
    );
}

#[test]
fn rejects_unchanged_with_wrong_player_identity() {
    for field in ["pid", "revision"] {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        let mut event = json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision});
        event[field] = match field {
            "pid" => json!(ready.pid + 1),
            _ => json!("rev-unaccepted"),
        };
        emit_fixture_event(&c, event);
        let failure = c
            .wait(&seconds(1), ready.last_sequence, false)
            .expect_err("incorrect unchanged identity acknowledged");
        assert!(
            failure.state.phase == "failed"
                && failure.state.last_update_sequence <= ready.last_sequence,
            "{field}: {:?}",
            failure.state
        );
    }
}

#[test]
fn retries_immutable_command_and_exposes_pending_results() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let mut args = request_args(json!({"requestId": "once", "arguments": {"delayMs": 300}}));
    let result = request_briefly(&c, 20, &args);
    assert_eq!(
        result["status"], "result_pending",
        "accepted canceled request was hidden: {result}"
    );
    let result = c.request(&seconds(1), "custom", &args).unwrap();
    assert_eq!(
        result["success"], true,
        "same request retry did not retrieve original result: {result}"
    );
    emit_fixture_event(&c, updated(ready.pid));
    c.wait(&seconds(1), ready.last_sequence, false).unwrap();
    let replay = c.request(&Context::background(), "custom", &args).unwrap();
    assert_eq!(
        replay["revision"], "rev-1",
        "retry after scene update rebuilt envelope or lost historic result: {replay}"
    );
    args.insert("arguments".into(), br#"{"delayMs":301}"#.to_vec());
    assert!(
        c.request(&Context::background(), "custom", &args).is_err(),
        "request ID reused for another operation"
    );
    assert_eq!(executions(&c), "1", "retry executed twice");
}

#[test]
fn pending_request_cannot_replay_on_replacement_player() {
    let c = controller_fixture("");
    start_ready(&c);
    let args = request_args(json!({"requestId": "pending", "arguments": {"dropResponse": true}}));
    request_briefly(&c, 20, &args);
    c.stop(&seconds(1)).unwrap();
    start_ready(&c);
    let result = c.request(&Context::background(), "custom", &args).unwrap();
    assert_eq!(
        result["status"], "result_unknown",
        "old request ID reexecuted on new Player: {result}"
    );
}

#[test]
fn guards_stale_revisions_response_identity_and_bounds() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let background = Context::background();
    let stale = request_args(json!({"expectedRevision": "old-revision", "requestId": "stale"}));
    let result = c.request(&background, "pause", &stale).unwrap();
    assert_eq!(
        result["success"], false,
        "stale revision accepted: {result}"
    );
    assert_eq!(result["error"]["code"], "stale_revision");
    assert!(
        !Path::new(&sysutil::join(&[
            &ready.control_directory,
            "stale.request.json"
        ]))
        .exists(),
        "stale command reached Player"
    );
    for arguments in [
        json!({"badHash": true}),
        json!({"responseRevision": "unrelated"}),
    ] {
        let error = c
            .request(
                &seconds(1),
                "custom",
                &request_args(json!({"arguments": arguments})),
            )
            .expect_err("invalid response proof accepted");
        assert!(error.message().contains("identity"), "{error}");
    }
    assert!(
        c.handle(&background, "runtime_query", b"null").is_err(),
        "nonobject command accepted"
    );
    let oversized = request_args(json!({"arguments": {"text": "x".repeat(65536)}}));
    assert!(
        c.request(&background, "custom", &oversized).is_err(),
        "oversized request accepted"
    );
}

#[test]
fn status_copies_nested_maps_and_concurrent_retry() {
    let c = controller_fixture("");
    start_ready(&c);
    let mut state = c.status();
    state.ready.as_mut().unwrap()["scene"]["isLoaded"] = json!(false);
    state.events[0].data["event"] = json!("changed-by-client");
    let current = c.status();
    assert!(
        current.ready.unwrap()["scene"]["isLoaded"] == json!(true)
            && current.events[0].data["event"] == json!("ready"),
        "client mutated controller state"
    );
    let workers: Vec<_> = (0..12)
        .map(|_| {
            let c = c.controller.clone();
            thread::spawn(move || {
                c.request(
                    &seconds(1),
                    "pause",
                    &request_args(json!({"requestId": "parallel"})),
                )
                .map(drop)
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    assert_eq!(
        executions(&c),
        "1",
        "concurrent identical command executed more than once"
    );
}

#[test]
fn protocol_failures_are_bounded() {
    for (mode, reason) in [
        ("overflow", "line too long"),
        ("truncated", "runner stdout contained malformed JSON event"),
        (
            "empty-exit",
            "runner exited before confirmed Player readiness",
        ),
        (
            "bad-pid",
            "runner returned an incompatible Player readiness identity",
        ),
        (
            "symlink-control",
            "runner returned an invalid control endpoint",
        ),
        (
            "foreign-library-manifest",
            "runner readiness names an invalid owned report/manifest",
        ),
    ] {
        let c = controller_fixture(mode);
        c.start().unwrap();
        let failure = c
            .wait(&seconds(1), 0, true)
            .expect_err("bad runner was accepted or hung");
        assert_eq!(failure.state.phase, "failed", "{mode}: {:?}", failure.state);
        assert!(
            failure.error.message().starts_with(reason),
            "{mode}: {}",
            failure.error
        );
        c.stop(&seconds(1)).unwrap();
    }
}

#[test]
fn failed_runner_returns_bounded_stderr_reason() {
    let c = controller_fixture("stderr-exit");
    c.start().unwrap();
    let failure = c
        .wait(&seconds(2), 0, true)
        .expect_err("runner failure reason missing from wait");
    let state = failure.state;
    assert!(
        state.phase == "failed"
            && failure
                .error
                .message()
                .contains("native loader actual failure reason"),
        "{state:?} {}",
        failure.error
    );
    assert!(
        failure
            .error
            .message()
            .starts_with("exit status 1\nrunner stderr:\n")
    );
    let status = c.status();
    assert!(
        status.runner_stderr_path
            == sysutil::join(&[&c.0.options.output, "agent-runner-stderr.log"])
            && status.runner_stderr_tail == state.runner_stderr_tail
            && status
                .last_error
                .contains("native loader actual failure reason"),
        "status lost owned stderr diagnostics {status:?}"
    );
    assert_eq!(
        state.runner_stderr_tail,
        "native loader actual failure reason invalidUTF8:?"
    );
    assert!(
        !state.runner_stderr_tail.contains("discarded-prefix-marker")
            && !state.runner_stderr_tail.contains(&"x".repeat(100)),
        "oversized first-line fragment leaked into useful tail"
    );
    assert!(
        state.runner_stderr_tail.len() <= RUNNER_STDERR_MAXIMUM
            && state.last_error.len() <= RUNNER_STDERR_MAXIMUM
            && !state.runner_stderr_tail.contains('\0'),
        "stderr failure diagnostics are not bounded sanitized UTF8"
    );
    c.stop(&seconds(1)).unwrap();
}

#[test]
fn runner_stderr_tail_rejects_replacement_symlink_and_special_files() {
    for mode in ["replacement", "symlink", "fifo"] {
        let (_directory, output) = temp_dir();
        let path = sysutil::join(&[&output, "agent-runner-stderr.log"]);
        let mut writer = File::create(&path).unwrap();
        writer.write_all(b"owned original reason\n").unwrap();
        assert_eq!(
            read_runner_stderr_tail(&output, &writer).unwrap(),
            "owned original reason"
        );
        fs::rename(&path, format!("{path}.original")).unwrap();
        let (_foreign, foreign) = temp_dir();
        match mode {
            "replacement" => fs::write(&path, "foreign replacement content").unwrap(),
            "symlink" => {
                let target = sysutil::join(&[&foreign, "foreign.log"]);
                fs::write(&target, "foreign replacement content").unwrap();
                std::os::unix::fs::symlink(&target, &path).unwrap();
            }
            _ => {
                let name = std::ffi::CString::new(path.clone()).unwrap();
                // SAFETY: `name` is a NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
        }
        let tail = read_runner_stderr_tail(&output, &writer);
        assert!(
            tail.is_err(),
            "{mode}: unsafe log replacement was read: {tail:?}"
        );
    }
}

#[test]
fn runner_failure_message_retains_end_within_bound() {
    let tail = format!("{}actual final reason", "ж".repeat(8192));
    let message = runner_failure_message("exit status 1", &tail);
    assert!(
        message.len() <= RUNNER_STDERR_MAXIMUM && message.ends_with("actual final reason"),
        "bounded failure message lost useful suffix or UTF8"
    );
    assert_eq!(runner_failure_message("exit status 1", ""), "exit status 1");
    assert_eq!(to_valid_utf8(b"a\xff\xfeb\xffc"), "a?b?c");
}

#[test]
fn rejects_symlink_output_and_artifacts() {
    let runner = env::current_exe()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    for root in ["Temp", "Library"] {
        let (_directory, project) = temp_dir();
        let (_elsewhere, elsewhere) = temp_dir();
        std::os::unix::fs::symlink(&elsewhere, sysutil::join(&[&project, root])).unwrap();
        let output = sysutil::join(&[
            &project,
            match root {
                "Temp" => "Temp/agent",
                _ => "Library/UnityMicroEditor/agent",
            },
        ]);
        let options = Options {
            project,
            runner: runner.clone(),
            output,
            ..Options::default()
        };
        assert!(
            Controller::new(&Context::background(), options).is_err(),
            "symlink output ancestor accepted"
        );
    }
    let (_directory, directory) = temp_dir();
    let (_secret, secret) = temp_dir();
    let target = sysutil::join(&[&secret, "secret"]);
    fs::write(&target, "private").unwrap();
    let link = sysutil::join(&[&directory, "response.json"]);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(read_bounded(&link, 65536).is_err(), "response symlink read");
    assert!(
        publish_immutable(&link, b"command").is_err(),
        "request symlink overwritten"
    );
    assert_eq!(fs::read(&target).unwrap(), b"private");
}

#[test]
fn owns_only_strict_cache_children() {
    let (_directory, project) = temp_dir();
    let runner = env::current_exe()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    for (path, valid) in [
        ("Temp/agent", true),
        ("Library/UnityMicroEditor/agent", true),
        ("Temp", false),
        ("Library/UnityMicroEditor", false),
        ("Library", false),
        ("Library/ScriptAssemblies/agent", false),
        ("Library/UnityMicroEditor-other/agent", false),
    ] {
        let options = Options {
            project: project.clone(),
            runner: runner.clone(),
            output: sysutil::join(&[&project, path]),
            ..Options::default()
        };
        let controller = Controller::new(&Context::background(), options);
        assert_eq!(
            controller.is_ok(),
            valid,
            "unexpected cache ownership: {path}"
        );
        if let Ok(controller) = controller {
            controller.close().unwrap();
        }
    }
}

#[test]
fn library_session_survives_editor_temp_cleanup() {
    let c = controller_fixture("library-manifest");
    let project = &c.0.options.project;
    assert_eq!(
        c.0.options.output,
        sysutil::join(&[project, "Library/UnityMicroEditor/agent"]),
        "default controller output is volatile"
    );
    let ready = start_ready(&c);
    fs::create_dir_all(sysutil::join(&[project, "Temp"])).unwrap();
    fs::remove_dir_all(sysutil::join(&[project, "Temp"])).unwrap();
    let response = c
        .request(
            &seconds(1),
            "status",
            &request_args(json!({"requestId": "after-editor-restart"})),
        )
        .unwrap();
    assert!(
        response["success"] == json!(true) && c.status().pid == ready.pid,
        "Editor Temp cleanup broke Library endpoint: {response}"
    );
}

#[test]
fn cleans_separate_owned_player_after_runner_crash() {
    let c = controller_fixture("spawn-child");
    let ready = start_ready(&c);
    emit_fixture_event(&c, json!({"event": "crash"}));
    let deadline = Instant::now() + Duration::from_secs(PATIENCE);
    let crashed = |state: &State| state.phase == "failed" && state.pid == 0;
    while Instant::now() < deadline && !crashed(&c.status()) {
        thread::sleep(Duration::from_millis(5));
    }
    let state = c.status();
    assert!(crashed(&state), "crashed runner not reaped: {state:?}");
    assert!(
        state.last_error.starts_with("exit status 7"),
        "{}",
        state.last_error
    );
    // SIGKILL is delivered asynchronously; the stopped runner can be reaped
    // before the kernel schedules its separate Player process to exit.
    let deadline = Instant::now() + Duration::from_secs(PATIENCE);
    while let Ok((identity, _)) = read_process_identity(ready.pid) {
        assert!(
            Instant::now() <= deadline,
            "separate owned Player remains alive after runner crash: {identity:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn number_pid_keeps_exact_integer_identity() {
    let number = |text: &str| Value::Number(serde_json::from_str(text).unwrap());
    for value in [
        number("1.5"),
        number("-1"),
        number("999999999999999999999999"),
        number("1e3"),
        number("1.0"),
        json!("123"),
        Value::Null,
    ] {
        assert_eq!(
            number_pid(Some(&value)),
            0,
            "invalid PID was coerced: {value}"
        );
    }
    assert_eq!(number_pid(None), 0);
    let pid = i64::from(process::id());
    assert_eq!(number_pid(Some(&number(&pid.to_string()))), pid);
}

#[test]
fn persistent_receipt_survives_server_and_control_removal() {
    let c = controller_fixture("");
    let state = start_ready(&c);
    let background = Context::background();
    let mut args = request_args(json!({"requestId": "durable-once", "arguments": {"value": 23}}));
    let first = c.request(&background, "custom", &args).unwrap();
    c.close().unwrap();
    fs::remove_dir_all(&state.control_directory).unwrap();
    let restarted = Controller::new(&background, c.options()).unwrap();
    let replay = restarted.request(&background, "custom", &args).unwrap();
    assert_eq!(
        jsondoc::encode(&first),
        jsondoc::encode(&replay),
        "persistent receipt changed"
    );
    args.insert("arguments".into(), br#"{"value":24}"#.to_vec());
    assert!(
        restarted.request(&background, "custom", &args).is_err(),
        "restart accepted conflicting request ID"
    );
    assert_eq!(executions(&c), "1", "replay executed command again");
    restarted.close().unwrap();
}

#[test]
fn persistent_pending_cannot_execute_on_replacement_player() {
    let c = controller_fixture("");
    start_ready(&c);
    let args =
        request_args(json!({"requestId": "durable-pending", "arguments": {"dropResponse": true}}));
    request_briefly(&c, 25, &args);
    c.close().unwrap();
    let restarted = Controller::new(&Context::background(), c.options()).unwrap();
    shorten_grace(&restarted);
    start_ready(&restarted);
    let result = restarted
        .request(&Context::background(), "custom", &args)
        .unwrap();
    assert_eq!(
        result["status"], "result_unknown",
        "pending command replayed: {result}"
    );
    assert_eq!(executions(&c), "1", "restarted Player accepted old command");
    restarted.close().unwrap();
}

#[test]
fn rejects_corrupt_persistent_request() {
    let c = controller_fixture("");
    start_ready(&c);
    c.request(
        &Context::background(),
        "status",
        &request_args(json!({"requestId": "corrupt"})),
    )
    .unwrap();
    c.close().unwrap();
    let path = sysutil::join(&[
        &c.0.options.output,
        ".agent-requests",
        "corrupt.request-record.json",
    ]);
    let data = fs::read(&path).unwrap();
    let mut record = RequestRecord::decode(&data).unwrap();
    assert_eq!(
        record.encode(),
        data,
        "record did not survive a decode/encode round trip"
    );
    record.directory = sysutil::join(&[&c.0.options.project, "Assets"]);
    fs::write(&path, record.encode()).unwrap();
    let reopened = Controller::new(&Context::background(), c.options());
    assert_eq!(
        reopened.err().map(|error| error.to_string()).as_deref(),
        Some("persistent runtime request record has an invalid identity or ownership"),
        "foreign control path accepted from persistent journal"
    );
}

#[test]
fn handle_routes_session_and_runtime_operations() {
    let c = controller_fixture("");
    let background = Context::background();
    let message = |raw: &[u8], operation: &str| {
        c.handle(&background, operation, raw)
            .expect_err("invalid arguments accepted")
            .error
            .to_string()
    };
    assert_eq!(
        message(&vec![b' '; 65537], "session_control"),
        "runtime arguments exceed 64 KiB"
    );
    assert_eq!(
        message(b"null", "session_control"),
        "runtime arguments must be an object"
    );
    assert_eq!(
        message(b"[]", "session_control"),
        "json: cannot unmarshal array into value of type object"
    );
    assert_eq!(
        message(b"{", "session_control"),
        "unexpected end of JSON input"
    );
    assert_eq!(
        message(b"{}", "session_control"),
        "unexpected end of JSON input"
    );
    assert_eq!(
        message(br#"{"action":7}"#, "session_control"),
        "json: cannot unmarshal number into value of type string"
    );
    assert_eq!(
        message(br#"{"action":"launch"}"#, "session_control"),
        "unsupported runtime operation \"session_launch\""
    );
    assert_eq!(
        message(b"", "runtime_query"),
        "runtime operation is required"
    );
    assert_eq!(
        message(br#"{"operation":null}"#, "runtime_command"),
        "unsupported runtime operation \"runtime_command\""
    );
    assert_eq!(
        message(
            br#"{"action":"wait","afterSequence":-1}"#,
            "session_control"
        ),
        "json: cannot unmarshal number -1 into value of type uint64"
    );
    assert_eq!(
        message(br#"{"operation":"status"}"#, "runtime_query"),
        "Player is not ready; use session_start and session_wait"
    );
    let Ok(Reply::State(stopped)) = c.handle(&background, "session_status", b"") else {
        panic!("status did not return a state");
    };
    assert_eq!(
        String::from_utf8(jsondoc::marshal(&Reply::State(stopped)).unwrap()).unwrap(),
        r#"{"phase":"stopped","lastSequence":0,"lastReadySequence":0,"lastUpdateSequence":0,"lastUpdateRejectedSequence":0}"#
    );
    let Ok(Reply::State(starting)) =
        c.handle(&background, "session_control", br#"{"action":"start"}"#)
    else {
        panic!("start did not return a state");
    };
    assert!(!starting.session_id.is_empty());
    let waited = c.handle(
        &seconds(3),
        "session_control",
        br#"{"action":"wait","afterSequence":0}"#,
    );
    let Ok(Reply::State(ready)) = waited else {
        panic!("wait did not return a state: {waited:?}");
    };
    assert_eq!(
        (ready.phase.as_str(), ready.last_ready_sequence),
        ("ready", 1)
    );
    let encoded = String::from_utf8(jsondoc::marshal(&ready).unwrap()).unwrap();
    assert!(
        encoded.starts_with(&format!(
            "{{\"phase\":\"ready\",\"sessionId\":\"{}\",\"pid\":{},\"revision\":\"rev-1\",\"controlDirectory\":",
            ready.session_id, ready.pid
        )) && encoded.contains("\"events\":[{\"sequence\":1,\"data\":{\"event\":\"ready\",")
            && encoded.contains("\"ready\":{\"controlDirectory\":"),
        "{encoded}"
    );
    let pending = c.handle(
        &millis(60),
        "session_control",
        br#"{"action":"wait_update","afterSequence":1}"#,
    );
    let failure = pending.expect_err("update wait ended without an update");
    assert!(failure.error.is_deadline_exceeded());
    assert!(matches!(failure.reply, Some(Reply::State(state)) if state.phase == "ready"));
    let query = format!(
        r#"{{"operation":"status","requestId":"routed","expectedPid":{}}}"#,
        ready.pid
    );
    let Ok(Reply::Value(result)) = c.handle(&seconds(1), "runtime_query", query.as_bytes()) else {
        panic!("runtime query did not return a result");
    };
    assert_eq!(
        jsondoc::encode(&result),
        format!(
            r#"{{"error":null,"pid":{},"requestId":"routed","result":{{"executions":1}},"revision":"rev-1","success":true}}"#,
            ready.pid
        )
        .into_bytes()
    );
    let envelope = fs::read(sysutil::join(&[
        &ready.control_directory,
        "routed.request.json",
    ]))
    .unwrap();
    assert_eq!(
        String::from_utf8(envelope).unwrap(),
        format!(
            r#"{{"schemaVersion":1,"requestId":"routed","expectedPid":{},"expectedRevision":"rev-1","operation":"status","arguments":{{}}}}"#,
            ready.pid
        )
    );
    let forwarded =
        br#"{"requestId":"forwarded","path":"/Root","depth":2,"expectedRevision":"rev-1"}"#;
    assert!(c.handle(&seconds(1), "hierarchy", forwarded).is_ok());
    let envelope = fs::read(sysutil::join(&[
        &ready.control_directory,
        "forwarded.request.json",
    ]))
    .unwrap();
    assert!(
        String::from_utf8(envelope)
            .unwrap()
            .ends_with(r#""operation":"hierarchy","arguments":{"depth":2,"path":"/Root"}}"#)
    );
    let Ok(Reply::State(reset)) =
        c.handle(&seconds(3), "session_control", br#"{"action":"reset"}"#)
    else {
        panic!("reset did not return a state");
    };
    assert!(reset.phase == "starting" && reset.session_id != ready.session_id && !alive(ready.pid));
    // A runner interrupted while it reports readiness fails its session instead.
    c.wait(&seconds(3), reset.last_sequence, true).unwrap();
    let Ok(Reply::State(stopped)) =
        c.handle(&seconds(3), "session_control", br#"{"action":"stop"}"#)
    else {
        panic!("stop did not return a state");
    };
    assert_eq!(stopped.phase, "stopped");
}

// Byte-for-byte agreement with the recorded reference cases (see sysutil::oracle).
#[test]
fn recorded_cases_agree_on_journal_receipts_state_and_routing() {
    use crate::sysutil::oracle::compare;
    compare("decodeReceipt", |case| {
        let record = RequestRecord {
            id: "q".into(),
            hash: case.text("hash").into(),
            pid: 9,
            revision: "rev-1".into(),
            ..RequestRecord::default()
        };
        journal::decode_receipt(&case.bytes("data"), &record, case.text("operation"))
            .map(|value| jsondoc::encode(&value))
            .map_err(|error| error.to_string())
    });
    compare("record", |case| {
        let envelope = case.bytes("envelope");
        let record = RequestRecord {
            id: case.text("id").into(),
            operation: case.text("operation").into(),
            session_id: case.text("session").into(),
            directory: case.text("directory").into(),
            revision: case.text("revision").into(),
            hash: crate::sha256(&envelope),
            pid: case.number("pid"),
            arguments: case.bytes("arguments"),
            envelope,
        };
        Ok(record.encode())
    });
    compare("validateRecord", |case| {
        let invalid = "persistent runtime request journal has an invalid record";
        let record = RequestRecord::decode(&case.bytes("data"))
            .filter(|record| case.text("name") == format!("{}.request-record.json", record.id));
        Ok(match record {
            None => invalid.to_string(),
            Some(record) => journal::validate(case.text("output"), &record)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default(),
        }
        .into_bytes())
    });
    compare("runnerFailureMessage", |case| {
        Ok(runner_failure_message(case.text("reason"), case.text("tail")).into_bytes())
    });
    compare("numberPID", |case| {
        // A decoded event never carries a literal that is not a JSON number.
        let Ok(number) = serde_json::from_str(case.text("literal")) else {
            return Ok(case.bytes_output());
        };
        Ok(number_pid(Some(&Value::Number(number)))
            .to_string()
            .into_bytes())
    });
    compare("state", |case| {
        let data = case.bytes("data");
        if data.is_empty() {
            return jsondoc::marshal(&State {
                phase: "failed".into(),
                last_error: "exit status 1\nrunner stderr:\n<x>".into(),
                update_error: "u".into(),
                runner_stderr_tail: "t".into(),
                ready: Some(Map::new()),
                ..State::default()
            });
        }
        let json = jsondoc::parse(&data)?;
        let members = json.members().unwrap();
        jsondoc::marshal(&State {
            phase: "ready".into(),
            session_id: "s".into(),
            pid: 12,
            revision: "rev-1".into(),
            control_directory: "/c/<x>".into(),
            last_sequence: u64::MAX,
            last_ready_sequence: 2,
            runner_stderr_path: "/o/agent-runner-stderr.log".into(),
            events: vec![
                Event {
                    sequence: 1,
                    data: jsondoc::to_map(members, false)?,
                },
                Event::default(),
            ],
            ready: Some(jsondoc::to_map(members, true)?),
            ..State::default()
        })
        .map(|encoded| {
            String::from_utf8(encoded)
                .unwrap()
                .replace("\"sequence\":0", "\"sequence\":2")
                .into_bytes()
        })
    });
    let (_directory, project) = temp_dir();
    let options = Options {
        project,
        runner: env::current_exe()
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap(),
        ..Options::default()
    };
    let controller = Controller::new(&Context::background(), options).unwrap();
    compare("handle", |case| {
        match controller.handle(
            &Context::background(),
            case.text("operation"),
            &case.bytes("raw"),
        ) {
            Ok(reply) => jsondoc::marshal(&reply),
            Err(failure) => Err(failure.error.to_string()),
        }
    });
    controller.close().unwrap();
}
