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
    sync::atomic::AtomicU64,
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
    // The controller's commands arrive on the inherited standard input. Each
    // complete line is recorded as it was written. Two modes are a runner whose
    // command reader is gone: it closed the input, or it leaves it unread.
    let commands = sysutil::join(&[&output, "helper-commands.jsonl"]);
    match mode.as_str() {
        // SAFETY: descriptor 0 belongs to this process and nothing reads it.
        "commands-closed" => drop(unsafe { File::from_raw_fd(0) }),
        "commands-unread" => {}
        _ => {
            thread::spawn(move || {
                for line in io::stdin().lines().map_while(|line| line.ok()) {
                    let log = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&commands);
                    let _ = log.and_then(|mut log| writeln!(log, "{line}"));
                }
            });
        }
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
    if helper_write(&manifest, &json!({"revision": "rev-1"})).is_err()
        || helper_write(
            &sysutil::join(&[&output, "helper-arguments.json"]),
            &json!(args),
        )
        .is_err()
    {
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
    let mut readiness =
        json!({"event": "ready", "pid": pid, "manifest": manifest, "report": report});
    if mode == "hot-reload-off" {
        readiness["hotReload"] = json!(false);
    }
    emit(&readiness);
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

#[test]
fn start_arguments_reach_the_runner_and_reset_replaces_them() {
    let c = controller_fixture("");
    let ctx = seconds(3);
    let launched = |c: &Controller| -> Vec<String> {
        let path = sysutil::join(&[&c.0.options.output, "helper-arguments.json"]);
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    };
    let scene = "0123456789abcdef0123456789abcdef=Assets/Extra.unity";
    let start = json!({"action": "start", "arguments": {
        "gameArguments": ["-content-url", "http://127.0.0.1:9/content"],
        "contentScenes": [scene], "contentPrefabs": ["Assets/Extra.prefab"],
        "addressables": false}});
    c.handle(&ctx, "session_control", &jsondoc::encode(&start))
        .unwrap();
    let ready = c.wait(&ctx, 0, true).unwrap();
    assert_eq!(
        ready.launch.game_arguments,
        ["-content-url", "http://127.0.0.1:9/content"]
    );
    let args = launched(&c);
    let separator = args.iter().rposition(|arg| arg == "--").unwrap();
    assert_eq!(
        args[separator + 1..],
        ["-content-url", "http://127.0.0.1:9/content"],
        "game arguments did not follow the runner's own flags"
    );
    let flag = |args: &[String], name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .map(|index| args[index + 1].clone())
    };
    assert_eq!(
        flag(&args[..separator], "--content-scene").as_deref(),
        Some(scene)
    );
    assert_eq!(
        flag(&args[..separator], "--content-prefab").as_deref(),
        Some("Assets/Extra.prefab")
    );
    assert!(
        args[..separator]
            .iter()
            .any(|arg| arg == "--no-addressables"),
        "addressables: false did not reach the runner"
    );

    // A running Player keeps its command line: other arguments need a reset.
    let other = json!({"action": "start", "arguments": {"gameArguments": ["-other"]}});
    assert!(
        c.handle(&ctx, "session_control", &jsondoc::encode(&other))
            .is_err(),
        "start silently ignored different launch arguments"
    );
    c.handle(&ctx, "session_control", &jsondoc::encode(&start))
        .unwrap();
    let reset = json!({"action": "reset", "arguments": {"gameArguments": ["-other"]}});
    c.handle(&ctx, "session_control", &jsondoc::encode(&reset))
        .unwrap();
    let again = c.wait(&ctx, ready.last_sequence, true).unwrap();
    assert_ne!(again.pid, ready.pid);
    let args = launched(&c);
    let separator = args.iter().rposition(|arg| arg == "--").unwrap();
    assert_eq!(args[separator + 1..], ["-other"]);
    assert_eq!(
        flag(&args[..separator], "--content-scene").as_deref(),
        Some(scene),
        "a reset that names only game arguments dropped the content scenes"
    );
    assert!(
        args[..separator]
            .iter()
            .any(|arg| arg == "--no-addressables"),
        "a reset that names only game arguments dropped the Addressables choice"
    );
    c.stop(&ctx).unwrap();
    for invalid in [
        json!({"gameArguments": ["--unity-cli-frames", "1"]}),
        json!({"gameArguments": "-flag"}),
        json!({"contentScenes": ["--watch"]}),
        json!({"addressables": "off"}),
    ] {
        let start = json!({"action": "start", "arguments": invalid});
        assert!(
            c.handle(&ctx, "session_control", &jsondoc::encode(&start))
                .is_err(),
            "invalid launch arguments accepted: {start}"
        );
    }
}

#[test]
fn rejection_that_needs_preparation_is_reported_until_an_update_applies() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "new source needs a slot; regenerate source graph", "preparationRequired": true}),
    );
    let state = c
        .wait(&seconds(1), ready.last_sequence, false)
        .expect_err("rejection was not reported")
        .state;
    assert!(
        state.preparation_required && c.status().preparation_required,
        "status hides that the Player stays on its old code: {state:?}"
    );
    emit_fixture_event(&c, updated(ready.pid));
    let state = c.wait(&seconds(1), state.last_sequence, false).unwrap();
    assert!(!state.preparation_required);
}

fn runner_arguments(c: &Controller) -> Vec<String> {
    let path = sysutil::join(&[&c.0.options.output, "helper-arguments.json"]);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
// The command lines the fixture runner has read from its standard input.
fn commands(c: &Controller) -> Vec<String> {
    fs::read_to_string(sysutil::join(&[
        &c.0.options.output,
        "helper-commands.jsonl",
    ]))
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect()
}
fn wait_for(what: &str, reached: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(PATIENCE);
    while !reached() {
        assert!(Instant::now() < deadline, "{what} did not happen");
        thread::sleep(Duration::from_millis(5));
    }
}
// The `code-updated` of a restart, naming the fixture's own readiness report.
fn restarted(c: &Controller, pid: i64, mut event: Value) -> Value {
    let options = &c.0.options;
    event["event"] = json!("code-updated");
    event["pid"] = json!(pid);
    event["manifest"] = json!(sysutil::join(&[
        &options.project,
        "Temp/manifests/rev-1/scene.json"
    ]));
    event["report"] = json!(sysutil::join(&[&options.output, "runs/first/report.json"]));
    event
}
fn refusal(report: &str) -> Value {
    json!({
        "event": "update-rejected",
        "error": "the C# edit cannot be applied to the running Player: Game.Boss was added (3 reasons)",
        "paths": ["Assets/Scripts/Enemy.cs"],
        "fullRecompileRequired": true,
        "unsupported": [
            {"code": "type_added", "member": "Game.Boss", "message": "a type was added."},
            {"code": "generic_type", "member": "Game.Pool`1::Take()", "message": "a method of a generic type is not patched"},
        ],
        "unsupportedCount": 3,
        "report": report,
    })
}
// Prints one event and returns the state once the controller has taken it.
// The fixture runner prints an event file only when it changed, so a serial
// number keeps two equal events apart.
fn accept(c: &Controller, mut event: Value) -> State {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    event["serial"] = json!(SERIAL.fetch_add(1, Ordering::SeqCst));
    let before = c.status().last_sequence;
    emit_fixture_event(c, event);
    wait_for("the event", || c.status().last_sequence > before);
    c.status()
}
// What the runner prints for the outcome of an edit, by a short name. A
// `:true` or `:false` suffix is its statement about the code on a scene update.
fn outcome(c: &Controller, name: &str) -> Value {
    let state = c.status();
    let (name, statement) = match name.split_once(':') {
        Some((name, statement)) => (name, Some(statement == "true")),
        None => (name, None),
    };
    let mut event = match name {
        "refusal" => refusal("/owned/report.json"),
        "compile-error" => json!({"event": "update-rejected", "error": "CS1002: ; expected"}),
        // The scene part of a change, rejected while a refused edit waits: its
        // own error with the runner's statement and the reasons that stand.
        "scene-rejected" => {
            let mut event = refusal("/owned/report.json");
            event["error"] = json!(SCENE_REJECTED);
            event
        }
        "updated" => updated(state.pid),
        "unchanged" => json!({"event": name, "pid": state.pid, "revision": state.revision}),
        // A generation is never used twice by one Player process.
        "code-patched" => json!({"event": name, "pid": state.pid, "revision": state.revision,
            "codeGeneration": state.last_sequence + 1}),
        "restart" => restarted(c, state.pid, json!({"reason": "hot_reload_unsupported"})),
        _ => panic!("unknown outcome {name}"),
    };
    if let Some(statement) = statement {
        event["fullRecompileRequired"] = json!(statement);
    }
    event
}
const SCENE_REJECTED: &str = "unresolved managed component type 0123 in Assets/Scenes/A.unity";
fn orderings<'a>(items: &[&'a str]) -> Vec<Vec<&'a str>> {
    if items.len() < 2 {
        return vec![items.to_vec()];
    }
    let mut all = Vec::new();
    for index in 0..items.len() {
        let mut rest = items.to_vec();
        let first = rest.remove(index);
        for mut ordering in orderings(&rest) {
            ordering.insert(0, first);
            all.push(ordering);
        }
    }
    all
}
// The typed failure of a wait that found a refused edit standing.
fn expect_refused(outcome: std::result::Result<State, StateError>, context: &str) -> StateError {
    let failure = match outcome {
        Ok(state) => {
            panic!("{context}: an edit that is not in the Player was confirmed: {state:?}")
        }
        Err(failure) => failure,
    };
    let typed = failure.typed.clone();
    assert_eq!(
        typed.as_ref().map(|typed| typed.code),
        Some("full_recompile_required"),
        "{context}: {:?} {}",
        failure.state,
        failure.error
    );
    assert_eq!(
        typed.unwrap().details["next"],
        json!({"tool": "hot_reload", "arguments": {"action": "recompile"}}),
        "{context}"
    );
    assert!(
        failure.state.hot_reload.full_recompile_required,
        "{context}: {:?}",
        failure.state
    );
    failure
}
fn recompile_in_background(
    c: &Controller,
    id: &str,
) -> thread::JoinHandle<std::result::Result<State, StateError>> {
    let (controller, id, sent) = (c.clone(), id.to_string(), commands(c).len());
    let worker = thread::spawn(move || controller.recompile(&seconds(3), &id));
    wait_for("the recompile command", || commands(c).len() > sent);
    worker
}
// A stop that stays under way: the `ignore-signals` runner outlives the
// signals of a stop, so the session is `stopping` until `end_runner`.
fn stop_slowly(c: &Controller) -> thread::JoinHandle<std::result::Result<State, StateError>> {
    {
        let mut inner = c.lock();
        inner.stop_grace = Duration::from_secs(60 * PATIENCE);
        inner.terminate_grace = Duration::from_secs(60 * PATIENCE);
    }
    let stopping = c.clone();
    let stopper = thread::spawn(move || stopping.stop(&seconds(3)));
    wait_for("the stop", || c.status().phase == "stopping");
    stopper
}
fn end_runner(c: &Controller) {
    let runner = c.lock().command.expect("the runner is already reaped");
    sysutil::kill(-runner.pid, libc::SIGKILL).unwrap();
}

#[test]
fn launch_selects_hot_reload_flags_and_keeps_the_command_channel() {
    let c = controller_fixture("");
    let ctx = seconds(3);
    let status = |c: &Controller| match c.handle(&ctx, "hot_reload", br#"{"action":"status"}"#) {
        Ok(Reply::Value(status)) => status,
        other => panic!("hot reload status: {other:?}"),
    };
    assert_eq!(
        status(&c),
        json!({"enabled": true, "onUnsupportedEdit": "report", "codeGeneration": 0,
            "fullRecompileRequired": false, "unsupported": [], "unsupportedCount": 0, "phase": "stopped"}),
        "a stopped controller did not report what its next launch uses"
    );
    let flags = |c: &Controller| -> Vec<String> {
        runner_arguments(c)
            .into_iter()
            .skip_while(|argument| argument != "run")
            .filter(|argument| {
                argument.contains("hot-reload")
                    || argument == "--commands-stdin"
                    || argument == "report"
                    || argument == "restart"
            })
            .collect()
    };
    let ready = start_ready(&c);
    assert_eq!(
        flags(&c),
        ["--hot-reload-fallback", "report", "--commands-stdin"]
    );
    let encoded = String::from_utf8(jsondoc::marshal(&ready).unwrap()).unwrap();
    assert!(
        !encoded.contains("hotReload") && !encoded.contains("launch"),
        "defaults changed the session state: {encoded}"
    );

    let reset = |arguments: Value| -> State {
        let before = c.status().last_sequence;
        let raw = jsondoc::encode(&json!({"action": "reset", "arguments": arguments}));
        c.handle(&ctx, "session_control", &raw).unwrap();
        c.wait(&ctx, before, true).unwrap()
    };
    let restart = reset(json!({"onUnsupportedEdit": "restart"}));
    assert_eq!(
        flags(&c),
        ["--hot-reload-fallback", "restart", "--commands-stdin"]
    );
    assert_eq!(restart.launch.on_unsupported_edit, "restart");
    assert_eq!(status(&c)["onUnsupportedEdit"], "restart");

    let disabled = reset(json!({"hotReload": false}));
    assert_eq!(flags(&c), ["--no-hot-reload", "--commands-stdin"]);
    assert_eq!(disabled.launch.hot_reload, Some(false));
    let encoded = String::from_utf8(jsondoc::marshal(&disabled).unwrap()).unwrap();
    assert!(
        encoded.contains(r#""hotReload":{"enabled":false,"codeGeneration":0,"fullRecompileRequired":false,"unsupported":[],"unsupportedCount":0}"#)
            && encoded.contains(r#""launch":{"hotReload":false,"onUnsupportedEdit":"restart"}"#),
        "{encoded}"
    );
    assert_eq!(
        status(&c),
        json!({"enabled": false, "codeGeneration": 0, "fullRecompileRequired": false,
            "unsupported": [], "unsupportedCount": 0, "phase": "ready"})
    );
    // The command channel does not depend on hot reload: a recompile is the
    // way to restart this Player on the newest code.
    let worker = recompile_in_background(&c, "without-hot-reload");
    emit_fixture_event(
        &c,
        restarted(
            &c,
            disabled.pid,
            json!({"reason": "requested", "requestId": "without-hot-reload"}),
        ),
    );
    let state = worker.join().unwrap().unwrap();
    assert!(state.phase == "ready" && !state.hot_reload.enabled);

    // The next fixture runner must not replay the event of this one.
    fs::remove_file(sysutil::join(&[&c.0.options.output, "helper-event.json"])).unwrap();
    let enabled = reset(json!({"hotReload": null, "onUnsupportedEdit": null}));
    assert_eq!(
        flags(&c),
        ["--hot-reload-fallback", "report", "--commands-stdin"]
    );
    assert!(enabled.launch.is_empty() && enabled.hot_reload.is_default());
    c.stop(&ctx).unwrap();
    for invalid in [
        json!({"hotReload": "yes"}),
        json!({"onUnsupportedEdit": "ignore"}),
        json!({"onUnsupportedEdit": true}),
    ] {
        let start = json!({"action": "start", "arguments": invalid});
        assert!(
            c.handle(&ctx, "session_control", &jsondoc::encode(&start))
                .is_err(),
            "invalid hot reload launch arguments accepted: {start}"
        );
    }
}

#[test]
fn readiness_reports_whether_the_runner_patches_code() {
    let c = controller_fixture("hot-reload-off");
    let ready = start_ready(&c);
    assert!(
        !ready.hot_reload.enabled && ready.launch.is_empty(),
        "the runner's own statement was ignored: {ready:?}"
    );
}

#[test]
fn code_patch_confirms_the_same_player_and_its_generation() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let report = sysutil::join(&[&c.0.options.output, "runs/first/hot-reload/1/report.json"]);
    let members: Vec<String> = (0..20)
        .map(|index| format!("Game.Enemy::M{index}()"))
        .collect();
    emit_fixture_event(
        &c,
        json!({
            "event": "code-patched", "paths": ["Assets/Scripts/Enemy.cs"], "pid": ready.pid,
            "revision": ready.revision, "codeGeneration": 1, "assemblies": ["Assembly-CSharp"],
            "patchedMethods": 20, "addedMethods": 1, "addedFields": 1, "restoredMethods": 0,
            "members": members,
            "notes": [{"code": "existing_instances_keep_default", "member": "Game.Enemy::armor", "message": "existing objects see the default value"}],
            "notesCount": 1, "compileSeconds": 0.7, "patchSeconds": 0.02, "sourceToAckSeconds": 0.9,
            "report": report,
        }),
    );
    let patched = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
    assert!(
        patched.phase == "ready"
            && patched.pid == ready.pid
            && patched.revision == ready.revision
            && patched.control_directory == ready.control_directory
            && patched.last_update_sequence == patched.last_sequence
            && patched.last_ready_sequence == ready.last_ready_sequence,
        "a code patch changed the Player identity: {patched:?}"
    );
    // No Player becomes ready for a patch, and a readiness wait of before the
    // edit still ends with it.
    assert_eq!(
        c.wait(&seconds(1), ready.last_sequence, true).unwrap(),
        patched
    );
    let patch = patched.hot_reload.last_patch.clone().unwrap();
    assert_eq!(
        (patched.hot_reload.code_generation, patch.code_generation),
        (1, 1)
    );
    assert_eq!(
        (
            patch.sequence,
            patch.patched_methods,
            patch.added_methods,
            patch.added_fields
        ),
        (patched.last_sequence, 20, 1, 1)
    );
    assert_eq!(patch.members.len(), 16, "the member list was not bounded");
    assert_eq!(patch.notes[0].code, "existing_instances_keep_default");
    let encoded = String::from_utf8(jsondoc::marshal(&patched).unwrap()).unwrap();
    assert!(
        encoded.contains(&format!(
            r#""hotReload":{{"enabled":true,"codeGeneration":1,"fullRecompileRequired":false,"unsupported":[],"unsupportedCount":0,"lastPatch":{{"sequence":{},"codeGeneration":1,"patchedMethods":20,"addedMethods":1,"addedFields":1,"restoredMethods":0,"assemblies":["Assembly-CSharp"],"members":["#,
            patched.last_sequence
        )),
        "{encoded}"
    );
    // Runtime commands stay bound to the same Player and scene revision.
    let response = c
        .request(
            &seconds(1),
            "status",
            &request_args(json!({"requestId": "after-code-patch"})),
        )
        .unwrap();
    assert_eq!(response["success"], true, "{response}");

    emit_fixture_event(
        &c,
        json!({"event": "code-patched", "pid": ready.pid, "revision": ready.revision, "codeGeneration": 2, "restoredMethods": 20}),
    );
    let reverted = c.wait(&seconds(1), patched.last_sequence, false).unwrap();
    let patch = reverted.hot_reload.last_patch.unwrap();
    assert_eq!(
        (
            reverted.hot_reload.code_generation,
            patch.restored_methods,
            patch.members.len()
        ),
        (2, 20, 0)
    );
    // The code state belongs to the Player process and ends with it.
    let stopped = c.stop(&seconds(3)).unwrap();
    assert!(stopped.hot_reload.is_default(), "{stopped:?}");
}

#[test]
fn malformed_code_patch_fails_the_session() {
    for field in ["pid", "revision", "codeGeneration", "no-generation"] {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        let mut event = json!({"event": "code-patched", "pid": ready.pid, "revision": ready.revision, "codeGeneration": 1});
        match field {
            "pid" => event["pid"] = json!(ready.pid + 1),
            "revision" => event["revision"] = json!("rev-unaccepted"),
            "codeGeneration" => event["codeGeneration"] = json!("1"),
            _ => drop(event.as_object_mut().unwrap().remove("codeGeneration")),
        }
        emit_fixture_event(&c, event);
        let failure = c
            .wait(&seconds(1), ready.last_sequence, false)
            .expect_err("a code patch without the accepted identity was acknowledged");
        assert!(
            failure.state.phase == "failed"
                && failure.state.last_update_sequence <= ready.last_sequence
                && failure.error.message().starts_with("runner code patch"),
            "{field}: {:?} {}",
            failure.state,
            failure.error
        );
    }
}

#[test]
fn refused_code_edit_asks_for_the_recompile_that_restarts_the_player() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let report = sysutil::join(&[&c.0.options.output, "runs/first/hot-reload/1/report.json"]);
    emit_fixture_event(&c, refusal(&report));
    let refused = c
        .wait(&seconds(1), ready.last_sequence, false)
        .expect_err("an edit that is not in the Player was acknowledged");
    let typed = refused.typed.clone().expect("the refusal has no code");
    assert_eq!(typed.code, "full_recompile_required");
    assert_eq!(
        typed.details,
        json!({
            "unsupported": [
                {"code": "type_added", "member": "Game.Boss", "message": "a type was added."},
                {"code": "generic_type", "member": "Game.Pool`1::Take()", "message": "a method of a generic type is not patched"},
            ],
            "unsupportedCount": 3,
            "report": report,
            "next": {"tool": "hot_reload", "arguments": {"action": "recompile"}},
        })
    );
    assert_eq!(
        refused.error.message(),
        "The C# edit compiled but cannot be applied to the running Player: Game.Boss: a type was added [type_added] (and 2 more). The Player still runs the previous code. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset."
    );
    let state = refused.state;
    assert!(
        state.phase == "ready"
            && state.pid == ready.pid
            && state.code_refused()
            && state.hot_reload.full_recompile_required
            && state.hot_reload.unsupported_count == 3
            && state.hot_reload.report == report
            && state
                .update_error
                .starts_with("the C# edit cannot be applied"),
        "{state:?}"
    );
    let encoded = String::from_utf8(jsondoc::marshal(&c.status()).unwrap()).unwrap();
    assert!(
        encoded.contains(r#""hotReload":{"enabled":true,"codeGeneration":0,"fullRecompileRequired":true,"unsupported":[{"code":"type_added","member":"Game.Boss","message":"a type was added."},"#),
        "{encoded}"
    );

    // A compile error after the refusal keeps today's untyped reply, and the
    // newest compiled code still waits for the recompile.
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS1002: ; expected"}),
    );
    let compile = c
        .wait(&seconds(1), state.last_sequence, false)
        .expect_err("compile error was not reported");
    assert!(
        compile.typed.is_none()
            && compile.error.message() == "CS1002: ; expected"
            && !compile.state.code_refused()
            && compile.state.hot_reload.full_recompile_required,
        "{:?} {}",
        compile.state,
        compile.error
    );
    // Neither does a scene update say anything about the code: the caller
    // that has read the refusal is still not told that its edit is applied.
    emit_fixture_event(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision}),
    );
    let scene = expect_refused(
        c.wait(&seconds(1), compile.state.last_sequence, false),
        "scene update",
    );
    assert_eq!(
        scene.error.message(),
        format!(
            "The C# edit compiled but cannot be applied to the running Player: Game.Boss: a type was added [type_added] (and 2 more). The Player still runs the previous code; the other source changes confirmed after sequence {} are applied. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.",
            compile.state.last_sequence
        )
    );
    let scene = *scene.state;
    assert!(scene.update_error.is_empty() && scene.last_update_sequence == scene.last_sequence);

    let worker = recompile_in_background(&c, "rc-1");
    assert_eq!(
        commands(&c),
        [r#"{"command":"recompile","requestId":"rc-1"}"#],
        "the runner did not read exactly one command line"
    );
    assert_eq!(c.status().hot_reload.pending_recompile, "rc-1");
    let second = c
        .recompile(&seconds(1), "rc-2")
        .expect_err("a second recompile was sent while one is pending");
    assert_eq!(
        second.typed,
        Some(Typed {
            code: "recompile_pending",
            details: json!({"requestId": "rc-1"}),
        })
    );
    // The reply names the ways on: wait, repeat the id, or replace a runner
    // that does not answer.
    for way in [
        "session_control(wait_update)",
        "with that requestId",
        "session_control(reset)",
    ] {
        assert!(second.error.message().contains(way), "{}", second.error);
    }
    // An update of a source edit that the runner handles first is not the answer.
    emit_fixture_event(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "paths": ["Assets/Main.unity"]}),
    );
    wait_for("the scene update", || {
        c.status().last_update_sequence > scene.last_sequence
    });
    assert!(
        !worker.is_finished(),
        "an unrelated update ended the recompile"
    );
    emit_fixture_event(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "requested", "requestId": "rc-1"}),
        ),
    );
    let state = worker.join().unwrap().unwrap();
    let restart = state.hot_reload.last_restart.clone().unwrap();
    assert!(
        state.phase == "ready"
            && state.last_update_sequence == state.last_sequence
            && state.last_ready_sequence == state.last_sequence
            && state.update_error.is_empty()
            && !state.hot_reload.full_recompile_required
            && state.hot_reload.unsupported.is_empty()
            && state.hot_reload.report.is_empty()
            && state.hot_reload.pending_recompile.is_empty()
            && restart.reason == "requested"
            && restart.request_id == "rc-1",
        "{state:?}"
    );
    assert_eq!(commands(&c).len(), 1, "the command was written twice");
}

// A wait succeeds only when the Player runs the newest compiled code,
// whatever the runner reported in between and whatever the caller has read:
// every order of the four outcomes, after each event, for every mark a caller
// can hold. The state and `hot_reload(status)` say the same.
#[test]
fn no_wait_succeeds_while_a_refused_edit_is_not_in_the_player() {
    let c = controller_fixture("");
    start_ready(&c);
    // The model: whether a refusal stands, and the last confirmed update.
    let (mut stale, mut confirmed) = (false, 0);
    for ordering in orderings(&["refusal", "updated", "unchanged", "code-patched"]) {
        let began = c.status().last_sequence;
        for (index, name) in ordering.iter().enumerate() {
            let state = accept(&c, outcome(&c, name));
            match *name {
                "refusal" => stale = true,
                "code-patched" => (stale, confirmed) = (false, state.last_sequence),
                _ => confirmed = state.last_sequence,
            }
            let context = format!("{ordering:?}, event {index}");
            assert_eq!(
                state.hot_reload.full_recompile_required, stale,
                "{context}: {state:?}"
            );
            assert_eq!(c.hot_reload()["fullRecompileRequired"], stale, "{context}");
            // No mark, the mark of before these edits, and one after each
            // event before the newest.
            for after in std::iter::once(0).chain(began..state.last_sequence) {
                for readiness in [false, true] {
                    let context = format!("{context}, after {after}, readiness {readiness}");
                    let waited = c.wait(&seconds(1), after, readiness);
                    if !stale {
                        match waited {
                            Ok(applied) => assert_eq!(applied, state, "{context}"),
                            Err(failure) => panic!("{context}: {}", failure.error),
                        }
                        continue;
                    }
                    let failure = expect_refused(waited, &context);
                    let message = failure.error.message();
                    let since = format!(
                        "the other source changes confirmed after sequence {after} are applied"
                    );
                    assert!(
                        message.contains("Game.Boss: a type was added [type_added] (and 2 more)")
                            && message.contains("The Player still runs the previous code")
                            && message.contains(&since) == (confirmed > after),
                        "{context}: {message}"
                    );
                }
            }
        }
    }
}

// What a wait for an update and a wait for readiness return, both with the
// mark of before the listed events.
#[test]
fn waits_report_the_code_the_player_runs_after_every_outcome() {
    enum Expect {
        Applied,
        Refused(&'static str),
        Rejected,
    }
    use Expect::{Applied, Refused, Rejected};
    const BOSS: &str = "Game.Boss: a type was added [type_added] (and 2 more)";
    const UNLISTED: &str = "the reasons are not reported yet";
    let cases: [(&[&str], Expect, Expect); 21] = [
        (&["unchanged"], Applied, Applied),
        (&["compile-error"], Rejected, Rejected),
        // The runner states that the Player runs the newest code again: the
        // refused edit was reverted.
        (&["refusal", "unchanged:false"], Applied, Applied),
        (&["refusal", "updated:false"], Applied, Applied),
        // It states the opposite on the scene part of a change whose refusal
        // it prints afterwards, and on every scene update while one stands.
        (&["updated:true"], Refused(UNLISTED), Refused(UNLISTED)),
        (&["unchanged:true"], Refused(UNLISTED), Refused(UNLISTED)),
        (&["updated:true", "refusal"], Refused(BOSS), Refused(BOSS)),
        (&["refusal", "unchanged:true"], Refused(BOSS), Refused(BOSS)),
        // A new Player process loads the newest code.
        (&["refusal", "restart"], Applied, Applied),
        (&["refusal", "updated", "restart"], Applied, Applied),
        (&["updated:true", "restart"], Applied, Applied),
        // A compile error is the outcome while nothing follows it. The
        // refusal is back with the next update that does not lift it.
        (&["refusal", "compile-error"], Rejected, Rejected),
        (&["compile-error", "refusal"], Refused(BOSS), Refused(BOSS)),
        (
            &["refusal", "compile-error", "updated"],
            Refused(BOSS),
            Refused(BOSS),
        ),
        (
            &["refusal", "compile-error", "unchanged:false"],
            Applied,
            Applied,
        ),
        (&["code-patched", "compile-error"], Rejected, Rejected),
        // A scene reload leaves a ready Player, which answers a readiness
        // wait also when a rejection follows.
        (&["updated", "compile-error"], Rejected, Applied),
        (&["updated", "refusal"], Refused(BOSS), Refused(BOSS)),
        // A scene that needs the refused code is rejected with the runner's
        // statement and the reasons. It is no compile error: the recompile
        // is what applies it, also for a caller that wakes on this event
        // before the refusal of the same change is printed.
        (&["refusal", "scene-rejected"], Refused(BOSS), Refused(BOSS)),
        (&["scene-rejected"], Refused(BOSS), Refused(BOSS)),
        (&["scene-rejected", "refusal"], Refused(BOSS), Refused(BOSS)),
    ];
    for (events, update, readiness) in cases {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        // Progress is no outcome.
        accept(&c, json!({"event": "compiling", "stage": "Roslyn"}));
        for is_ready in [false, true] {
            let waiting = c.wait(&millis(40), ready.last_sequence, is_ready);
            assert!(
                waiting
                    .as_ref()
                    .is_err_and(|failure| failure.error.is_deadline_exceeded()),
                "{events:?}: progress answered a wait: {waiting:?}"
            );
        }
        for name in events {
            accept(&c, outcome(&c, name));
        }
        let state = c.status();
        for (is_ready, expected) in [(false, &update), (true, &readiness)] {
            let context = format!("{events:?}, readiness {is_ready}");
            let waited = c.wait(&seconds(1), ready.last_sequence, is_ready);
            match expected {
                Applied => match waited {
                    Ok(applied) => assert!(
                        applied == state && !state.hot_reload.full_recompile_required,
                        "{context}: {applied:?}"
                    ),
                    Err(failure) => panic!("{context}: {}", failure.error),
                },
                Refused(reason) => {
                    let failure = expect_refused(waited, &context);
                    assert!(
                        failure.error.message().contains(reason),
                        "{context}: {}",
                        failure.error
                    );
                }
                Rejected => {
                    let failure = waited.expect_err(&context);
                    assert!(
                        failure.typed.is_none()
                            && failure.error.message() == "CS1002: ; expected"
                            && failure.state.phase == "ready",
                        "{context}: {:?} {}",
                        failure.state,
                        failure.error
                    );
                }
            }
        }
    }

    // The rejected scene part keeps its own error in the session state,
    // beside the reasons of the refusal.
    let c = controller_fixture("");
    start_ready(&c);
    accept(&c, outcome(&c, "refusal"));
    let state = accept(&c, outcome(&c, "scene-rejected"));
    assert_eq!(state.update_error, SCENE_REJECTED);
    assert_eq!(state.hot_reload.unsupported[0].code, "type_added");
    assert_eq!(state.hot_reload.unsupported_count, 3);
    assert!(state.code_refused() && !state.preparation_required);

    // A refusal that lists no reason is reported with the runner's own
    // sentence, also after later events replaced the session's update error.
    let c = controller_fixture("");
    let ready = start_ready(&c);
    accept(
        &c,
        json!({"event": "update-rejected", "error": "the C# edit needs a restart; see the report.", "fullRecompileRequired": true}),
    );
    accept(&c, outcome(&c, "compile-error"));
    accept(&c, outcome(&c, "updated"));
    let failure = expect_refused(
        c.wait(&seconds(1), ready.last_sequence, false),
        "no listed reason",
    );
    assert_eq!(
        failure.error.message(),
        format!(
            "The C# edit compiled but cannot be applied to the running Player: the C# edit needs a restart; see the report. The Player still runs the previous code; the other source changes confirmed after sequence {} are applied. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.",
            ready.last_sequence
        )
    );
}

#[test]
fn restart_records_why_hot_reload_caused_it() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    accept(&c, refusal("/owned/hot-reload/1/report.json"));
    // The runner restarted by itself. `report` of a readiness event is the
    // Player's report, so the result of the patch builder has its own name.
    let fallback = accept(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "hot_reload_unsupported", "unsupportedCount": 4,
                "unsupported": [{"code": "type_added", "member": "Game.Boss", "message": "a type was added"}],
                "hotReloadReport": "/owned/hot-reload/2/report.json"}),
        ),
    );
    let restart = fallback.hot_reload.last_restart.clone().unwrap();
    assert!(
        !fallback.hot_reload.full_recompile_required
            && fallback.hot_reload.unsupported.is_empty()
            && fallback.hot_reload.code_generation == 0
            && restart.sequence == fallback.last_sequence
            && restart.reason == "hot_reload_unsupported"
            && restart.unsupported_count == 4
            && restart.unsupported[0].member == "Game.Boss"
            && restart.report == "/owned/hot-reload/2/report.json",
        "{fallback:?}"
    );
    // A patch that left the Player unusable.
    let failed = accept(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "hot_reload_failed", "error": "the Player exited during a code patch"}),
        ),
    );
    let encoded = String::from_utf8(jsondoc::marshal(&failed).unwrap()).unwrap();
    assert!(
        encoded.contains(&format!(
            r#""lastRestart":{{"sequence":{},"reason":"hot_reload_failed","error":"the Player exited during a code patch","unsupportedCount":0}}"#,
            failed.last_sequence
        )),
        "{encoded}"
    );
    // A restart that names no reason is not one of hot reload.
    let plain = accept(&c, restarted(&c, ready.pid, json!({})));
    assert!(plain.hot_reload.is_default(), "{plain:?}");
}

#[test]
fn recompile_needs_a_ready_session() {
    let not_ready = |c: &Controller, phase: &str| {
        for raw in [
            &br#"{"action":"recompile"}"#[..],
            br#"{"action":"recompile","requestId":"rc-early"}"#,
        ] {
            let failure = c
                .handle(&seconds(3), "hot_reload", raw)
                .expect_err("recompile without a ready Player");
            assert_eq!(
                failure.typed,
                Some(Typed {
                    code: "session_not_ready",
                    details: json!({"phase": phase}),
                }),
                "{}",
                failure.error
            );
            assert!(matches!(failure.reply, Some(Reply::State(state)) if state.phase == phase));
        }
        assert!(
            c.status().hot_reload.pending_recompile.is_empty(),
            "a command was sent to a {phase} session"
        );
    };
    let c = controller_fixture("");
    not_ready(&c, "stopped");
    let message = |raw: &[u8]| {
        c.handle(&seconds(3), "hot_reload", raw)
            .expect_err("invalid hot reload call accepted")
            .error
            .to_string()
    };
    assert_eq!(
        message(br#"{"action":"apply"}"#),
        "unsupported runtime operation \"hot_reload_apply\""
    );
    assert_eq!(
        message(br#"{"action":"recompile","requestId":"bad id"}"#),
        "requestId must be ASCII letters, digits or hyphens, at most 64 characters"
    );
    // The command pipe exists from the launch to the end of the runner, but
    // only a ready Player can be restarted on new code. The `child` runner
    // never reports readiness.
    let starting = controller_fixture("child");
    starting.start().unwrap();
    not_ready(&starting, "starting");
    let stopping = controller_fixture("ignore-signals");
    start_ready(&stopping);
    let stopper = stop_slowly(&stopping);
    not_ready(&stopping, "stopping");
    end_runner(&stopping);
    assert_eq!(stopper.join().unwrap().unwrap().phase, "stopped");
}

#[test]
fn recompile_is_answered_only_by_the_event_that_names_it() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let worker = recompile_in_background(&c, "rc-1");
    // The runner first finishes what it was doing. The compile error of a
    // source edit, a failed scene reload and the answer to another command
    // are rejections too, and none of them is the answer to this command.
    for (error, id) in [
        ("CS1002: ; expected", None),
        ("scene reload failed", Some("rc-other")),
    ] {
        let before = c.status().last_update_rejected_sequence;
        let mut event =
            json!({"event": "update-rejected", "error": error, "paths": ["Assets/A.cs"]});
        if let Some(id) = id {
            event["requestId"] = json!(id);
        }
        emit_fixture_event(&c, event);
        wait_for("the rejection", || {
            c.status().last_update_rejected_sequence > before
        });
    }
    // Neither is a refusal, whatever it names: a restart needs no patch, so
    // a refusal is about a source edit.
    let mut refused = refusal("/owned/report.json");
    refused["requestId"] = json!("rc-1");
    emit_fixture_event(&c, refused);
    wait_for("the refusal", || {
        c.status().hot_reload.full_recompile_required
    });
    // A restart that names another cause belongs to an edit handled first.
    emit_fixture_event(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "hot_reload_unsupported", "unsupportedCount": 1,
                "unsupported": [{"code": "type_added", "member": "Game.Boss", "message": "a type was added"}]}),
        ),
    );
    wait_for("the fallback restart", || {
        c.status().hot_reload.last_restart.is_some()
    });
    let fallback = c.status();
    let restart = fallback.hot_reload.last_restart.clone().unwrap();
    assert!(
        !worker.is_finished()
            && fallback.last_update_sequence == fallback.last_sequence
            && restart.reason == "hot_reload_unsupported"
            && restart.unsupported_count == 1
            && restart.unsupported[0].member == "Game.Boss"
            && fallback.hot_reload.pending_recompile == "rc-1",
        "{fallback:?}"
    );
    // The compile check of the command itself failed: the Player is kept.
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS0103: The name 'boss' does not exist", "requestId": "rc-1"}),
    );
    let rejected = worker
        .join()
        .unwrap()
        .expect_err("a compile error restarted the Player");
    assert!(
        rejected.typed.is_none()
            && rejected.error.message() == "CS0103: The name 'boss' does not exist"
            && rejected.state.phase == "ready"
            && rejected.state.pid == ready.pid
            && rejected.state.hot_reload.pending_recompile.is_empty(),
        "{:?} {}",
        rejected.state,
        rejected.error
    );

    // A restart answers under either readiness name: the runner reports one
    // without a new compilation as `content-updated`. It answers by its
    // reason when the id is missing, and by its id when it was merged with a
    // restart that has another cause.
    for (name, id, answer) in [
        (
            "content-updated",
            "rc-2",
            json!({"reason": "requested", "requestId": "rc-2"}),
        ),
        ("code-updated", "rc-3", json!({"reason": "requested"})),
        (
            "code-updated",
            "rc-4",
            json!({"reason": "hot_reload_unsupported", "requestId": "rc-4"}),
        ),
    ] {
        let worker = recompile_in_background(&c, id);
        let mut event = restarted(&c, ready.pid, answer);
        event["event"] = json!(name);
        emit_fixture_event(&c, event);
        let state = match worker.join().unwrap() {
            Ok(state) => state,
            Err(failure) => panic!("{name} {id}: {:?} {}", failure.state, failure.error),
        };
        assert!(
            state.phase == "ready"
                && state.last_update_sequence == state.last_sequence
                && state.hot_reload.pending_recompile.is_empty(),
            "{name} {id}: {state:?}"
        );
    }
    assert_eq!(commands(&c).len(), 4, "a command was written twice");

    // The runner is stopped while a command is pending.
    let worker = recompile_in_background(&c, "rc-5");
    let stopping = c.clone();
    let stopper = thread::spawn(move || stopping.stop(&seconds(3)).map(drop));
    let stopped = worker
        .join()
        .unwrap()
        .expect_err("a stopped session confirmed a recompile");
    assert!(
        stopped
            .error
            .message()
            .contains("before the requested recompile"),
        "{}",
        stopped.error
    );
    stopper.join().unwrap().unwrap();
    assert!(c.status().hot_reload.is_default());
}

// A request id names one command. Asking again returns what the runner
// answered instead of restarting the Player once more.
#[test]
fn answered_recompile_is_replayed_for_its_request_id() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    // The client gives up on the call; the command stays with the runner.
    let gave_up = c
        .recompile(&millis(50), "rc-1")
        .expect_err("a recompile ended without an answer");
    assert!(gave_up.error.is_deadline_exceeded(), "{}", gave_up.error);
    assert_eq!(c.status().hot_reload.pending_recompile, "rc-1");
    emit_fixture_event(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "requested", "requestId": "rc-1"}),
        ),
    );
    wait_for("the restart", || {
        let state = c.status();
        state.hot_reload.pending_recompile.is_empty() && state.hot_reload.last_restart.is_some()
    });
    let replayed = c.recompile(&seconds(1), "rc-1").unwrap();
    assert!(
        replayed.phase == "ready"
            && replayed.hot_reload.pending_recompile.is_empty()
            && replayed
                .hot_reload
                .last_restart
                .as_ref()
                .unwrap()
                .request_id
                == "rc-1",
        "{replayed:?}"
    );
    // The outcome belongs to its id, also while the next command is pending.
    let next = recompile_in_background(&c, "rc-2");
    let replayed = c.recompile(&seconds(1), "rc-1").unwrap();
    assert_eq!(replayed.hot_reload.pending_recompile, "rc-2");
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS1002: ; expected", "requestId": "rc-2"}),
    );
    let rejected = next.join().unwrap().expect_err("compile error");
    assert_eq!(rejected.error.message(), "CS1002: ; expected");
    // A repeated id says that it repeats, so that a fixed source is not
    // mistaken for one that still fails.
    let again = c
        .recompile(&seconds(1), "rc-2")
        .expect_err("a rejected recompile was replayed as a success");
    assert!(again.typed.is_none() && again.state.pid == ready.pid);
    assert_eq!(
        again.error.message(),
        format!(
            "recompile rc-2 was already rejected at sequence {} (send a new requestId to recompile again): CS1002: ; expected",
            rejected.state.last_sequence
        )
    );

    // An edit refused after the restart is not hidden behind its success.
    emit_fixture_event(&c, refusal("/owned/report.json"));
    wait_for("the refusal", || {
        c.status().hot_reload.full_recompile_required
    });
    let stale = c
        .recompile(&seconds(1), "rc-1")
        .expect_err("a recorded restart hid a refused edit");
    assert_eq!(
        stale.typed.as_ref().map(|typed| typed.code),
        Some("full_recompile_required")
    );
    assert_eq!(
        stale.error.message(),
        "Recompile rc-1 restarted the Player, and a C# edit compiled after it cannot be applied to the running Player: Game.Boss: a type was added [type_added] (and 2 more). Call hot_reload with action \"recompile\" and a new requestId to restart the Player on the new code; game state is reset."
    );

    // Only the newest outcomes are kept; a forgotten id is a new command.
    let mut expected = vec!["rc-1".to_string(), "rc-2".to_string()];
    for index in 0..hotreload::RECOMPILE_HISTORY {
        let id = format!("fill-{index}");
        let worker = recompile_in_background(&c, &id);
        emit_fixture_event(
            &c,
            json!({"event": "update-rejected", "error": format!("answer {index}"), "requestId": id}),
        );
        worker.join().unwrap().expect_err("compile error");
        expected.push(id);
    }
    let worker = recompile_in_background(&c, "rc-1");
    expected.push("rc-1".into());
    assert_eq!(c.status().hot_reload.pending_recompile, "rc-1");
    let sent: Vec<String> = commands(&c)
        .iter()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["requestId"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(sent, expected, "a replayed id reached the runner again");
    c.stop(&seconds(3)).unwrap();
    worker.join().unwrap().expect_err("stopped");
}

// The session stays ready when the command channel is gone, so the error has
// to name the call that replaces the runner.
#[test]
fn recompile_names_the_reset_when_the_runner_takes_no_commands() {
    let reported = |c: &Controller, ready: &State, cause: &str| {
        // Repeating the call fails the same way and sends nothing.
        for id in ["rc-1", "rc-2"] {
            let failure = c
                .recompile(&seconds(1), id)
                .expect_err("a command was written to a runner that reads none");
            let message = failure.error.message();
            assert!(
                failure.typed.is_none()
                    && message.contains(cause)
                    && message.contains("session_control(reset)")
                    && failure.state.phase == "ready"
                    && failure.state.pid == ready.pid
                    && failure.state.hot_reload.pending_recompile.is_empty(),
                "{cause}: {:?} {message}",
                failure.state
            );
        }
    };
    let c = controller_fixture("commands-closed");
    let ready = start_ready(&c);
    reported(&c, &ready, "broken pipe");

    // A runner that stopped reading leaves a full pipe. The write must fail
    // there: the controller holds the session lock while it writes.
    let c = controller_fixture("commands-unread");
    let ready = start_ready(&c);
    let flags = {
        let mut inner = c.lock();
        let commands = inner.commands.as_mut().unwrap();
        // SAFETY: both calls only read or set properties of an open pipe.
        let flags = unsafe {
            libc::fcntl(commands.as_raw_fd(), libc::F_SETPIPE_SZ, 4096);
            libc::fcntl(commands.as_raw_fd(), libc::F_GETFL)
        };
        // Blank lines fill the pipe; a blocking one would hang right here.
        while flags & libc::O_NONBLOCK != 0 && commands.write(b"\n").is_ok() {}
        flags
    };
    assert!(
        flags >= 0 && flags & libc::O_NONBLOCK != 0,
        "the command pipe would block the controller: flags {flags:#o}"
    );
    reported(&c, &ready, "resource temporarily unavailable");
}

// A stop does not turn a confirmation of the Player it is stopping into a
// protocol error of the session.
#[test]
fn update_confirmed_while_stopping_leaves_a_stopped_session() {
    for name in ["code-patched", "unchanged", "updated"] {
        let c = controller_fixture("ignore-signals");
        let ready = start_ready(&c);
        let stopper = stop_slowly(&c);
        emit_fixture_event(
            &c,
            match name {
                "updated" => updated(ready.pid),
                _ => {
                    json!({"event": name, "pid": ready.pid, "revision": ready.revision, "codeGeneration": 1})
                }
            },
        );
        wait_for("the confirmation", || {
            c.status().last_sequence > ready.last_sequence
        });
        // The confirmation is not the readiness of a Player that is going away.
        let waiting = c.wait(&millis(40), ready.last_sequence, true);
        assert!(
            waiting
                .as_ref()
                .is_err_and(|failure| failure.error.is_deadline_exceeded()),
            "{name}: a stopping Player answered a readiness wait: {waiting:?}"
        );
        end_runner(&c);
        let stopped = stopper.join().unwrap().unwrap();
        assert!(
            stopped.phase == "stopped" && stopped.last_error.is_empty(),
            "{name}: {stopped:?}"
        );
    }
}

// The reply an agent gets through the server the `mcp` command builds: the
// service in front of the controller, and the MCP tools in front of the service.
#[test]
fn mcp_reply_names_the_full_recompile_and_the_call_that_applies_it() {
    use crate::{agent, mcp};
    let c = controller_fixture("");
    let project = c.0.options.project.clone();
    for directory in ["Assets", "ProjectSettings"] {
        fs::create_dir_all(sysutil::join(&[&project, directory])).unwrap();
    }
    for (path, content) in [
        (
            "ProjectSettings/ProjectVersion.txt",
            "m_EditorVersion: 6000.6.3f1\n",
        ),
        ("Assets/Main.unity", "selected scene"),
    ] {
        fs::write(sysutil::join(&[&project, path]), content).unwrap();
    }
    let service = agent::Service::new(agent::Options {
        project: project.clone(),
        scene: "Assets/Main.unity".into(),
        asynchronous: true,
        controller: Some(c.controller.clone()),
        lifetime: Some(Context::background()),
        ..Default::default()
    });
    let runtime: Arc<dyn mcp::RuntimeHandler> = {
        let service = service.clone();
        Arc::new(move |ctx: &Context, name: &str, raw: &[u8]| {
            service
                .runtime(ctx, name, raw)
                .map_err(agent::handler_error)
        })
    };
    let server = mcp::Server::new(&mcp::Options {
        project,
        runtime: Some(runtime),
        ..Default::default()
    })
    .unwrap();
    let call = |name: &str, arguments: Value| -> (bool, Value) {
        let result = server
            .call_tool(&seconds(3), name, Some(&jsondoc::encode(&arguments)))
            .unwrap();
        (result.is_error, serde_json::from_str(&result.text).unwrap())
    };
    let ready = start_ready(&c);
    // A refusal is answered by a recompile even when its text reads like a
    // graph rejection, which would queue a preparation and replace the Player.
    let mut event = refusal("/owned/report.json");
    event["error"] = json!("Game.Boss is outside the prepared managed graph of the running Player");
    emit_fixture_event(&c, event);
    let (is_error, reply) = call(
        "session_control",
        json!({"action": "wait_update", "afterSequence": ready.last_sequence}),
    );
    let error = &reply["error"];
    assert!(is_error && reply["success"] == false, "{reply}");
    assert_eq!(error["code"], "full_recompile_required", "{reply}");
    assert_eq!(
        error["details"]["next"],
        json!({"tool": "hot_reload", "arguments": {"action": "recompile"}})
    );
    assert_eq!(error["details"]["unsupported"][0]["code"], "type_added");
    assert_eq!(error["details"]["unsupportedCount"], 3);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("Game.Boss: a type was added [type_added] (and 2 more)"),
        "{reply}"
    );
    // A status poll shows the same state and starts no preparation for it.
    let (is_error, status) = call("session_control", json!({"action": "status"}));
    assert!(
        !is_error
            && status["data"]["hotReload"]["fullRecompileRequired"] == true
            && status["data"].get("preparation").is_none()
            && status["data"].get("preparationRequired").is_none(),
        "{status}"
    );
    let refused = status["data"]["lastSequence"].as_u64().unwrap();
    let (_, status) = call("hot_reload", json!({"action": "status"}));
    assert!(
        status["success"] == true
            && status["data"]["fullRecompileRequired"] == true
            && status["data"]["onUnsupportedEdit"] == "report"
            && status["data"]["phase"] == "ready",
        "{status}"
    );
    // A scene edit confirmed after the refusal does not make the answer a
    // success: not with the mark of before the C# edit, not without a mark,
    // not with the mark of the status that showed the refusal, and not for
    // an agent that waits for readiness.
    accept(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "paths": ["Assets/Main.unity"]}),
    );
    for arguments in [
        json!({"action": "wait_update", "afterSequence": ready.last_sequence}),
        json!({"action": "wait_update"}),
        json!({"action": "wait_update", "afterSequence": refused}),
        json!({"action": "wait", "afterSequence": refused}),
        json!({"action": "wait"}),
    ] {
        let (is_error, reply) = call("session_control", arguments.clone());
        assert!(
            is_error
                && reply["success"] == false
                && reply["error"]["code"] == "full_recompile_required"
                && reply["error"]["details"]["unsupportedCount"] == 3
                && reply["error"]["details"]["next"]["tool"] == "hot_reload",
            "{arguments}: {reply}"
        );
    }
    let worker = {
        let server = server.clone();
        thread::spawn(move || {
            server.call_tool(
                &seconds(3),
                "hot_reload",
                Some(br#"{"action":"recompile","requestId":"rc-mcp"}"#),
            )
        })
    };
    wait_for("the recompile command", || !commands(&c).is_empty());
    assert_eq!(
        commands(&c),
        [r#"{"command":"recompile","requestId":"rc-mcp"}"#]
    );
    emit_fixture_event(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "requested", "requestId": "rc-mcp"}),
        ),
    );
    let result = worker.join().unwrap().unwrap();
    let reply: Value = serde_json::from_str(&result.text).unwrap();
    let data = &reply["data"];
    assert!(
        !result.is_error
            && reply["success"] == true
            && data["phase"] == "ready"
            && data["hotReload"]["fullRecompileRequired"] == false
            && data["hotReload"]["lastRestart"]["requestId"] == "rc-mcp"
            // The fixture Player publishes no tool catalog; the refresh was attempted.
            && data["projectToolsRefreshError"].is_string(),
        "{reply}"
    );
    service.close();
}

#[test]
fn refusal_ends_when_the_player_runs_the_current_code_again() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let refuse = |after: u64| -> State {
        // A different report path makes each refusal a new fixture event.
        emit_fixture_event(&c, refusal(&format!("/owned/report-{after}.json")));
        let refused = c
            .wait(&seconds(1), after, false)
            .expect_err("refusal was not reported");
        assert!(refused.state.hot_reload.full_recompile_required);
        *refused.state
    };
    // The edit was reverted: the compiled code equals the loaded code again.
    let refused = refuse(ready.last_sequence);
    emit_fixture_event(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "fullRecompileRequired": false}),
    );
    let healed = c.wait(&seconds(1), refused.last_sequence, false).unwrap();
    assert!(
        healed.hot_reload.is_default() && !healed.code_refused(),
        "{healed:?}"
    );
    // A later edit made the whole difference patchable.
    let refused = refuse(healed.last_sequence);
    emit_fixture_event(
        &c,
        json!({"event": "code-patched", "pid": ready.pid, "revision": ready.revision, "codeGeneration": 1, "patchedMethods": 1}),
    );
    let patched = c.wait(&seconds(1), refused.last_sequence, false).unwrap();
    assert!(
        !patched.hot_reload.full_recompile_required
            && patched.hot_reload.unsupported.is_empty()
            && patched.hot_reload.unsupported_count == 0
            && patched.hot_reload.report.is_empty()
            && patched.hot_reload.code_generation == 1,
        "{patched:?}"
    );
    // A scene reload without that statement leaves the refusal in place. The
    // reload is applied, and the reply says both.
    let stale = refuse(patched.last_sequence);
    accept(&c, updated(ready.pid));
    let scene = expect_refused(
        c.wait(&seconds(1), stale.last_sequence, false),
        "scene reload",
    );
    assert!(
        scene.state.revision == "rev-2"
            && scene.state.hot_reload.code_generation == 1
            && scene.error.message().contains(&format!(
                "the other source changes confirmed after sequence {} are applied",
                stale.last_sequence
            )),
        "{:?} {}",
        scene.state,
        scene.error
    );
}

// The runner's watch loop with scripted steps: what it prints is what the
// controller reads, so neither side is tested against lines written by hand.
mod loop_lines {
    use super::*;
    pub(crate) use crate::runner::testing::Compiled;
    use crate::{
        hotreload::{Failure, Fallback, Outcome, Patched, Reason as Why, Refusal, Unchanged},
        runner::testing::{Event as Line, Loop, Restarted, Scene, Steps, Window},
    };
    use std::{collections::VecDeque, time::SystemTime};

    /// What the scripted scene step does.
    pub enum Reload {
        Unchanged,
        Updated,
        Rejected,
    }

    pub struct Script {
        pub lines: Vec<Value>,
        pub compiles: VecDeque<Compiled>,
        pub patches: VecDeque<Outcome>,
        pub reloads: VecDeque<Reload>,
        pub prepare_error: Option<&'static str>,
        pub queued: bool,
        compiled: bool,
        pid: i64,
        revision: String,
        manifest: String,
        report: String,
    }
    impl Script {
        pub fn new(c: &Controller, ready: &State) -> Self {
            let options = &c.0.options;
            Self {
                lines: Vec::new(),
                compiles: VecDeque::new(),
                patches: VecDeque::new(),
                reloads: VecDeque::new(),
                prepare_error: None,
                queued: false,
                compiled: false,
                pid: ready.pid,
                revision: ready.revision.clone(),
                manifest: sysutil::join(&[&options.project, "Temp/manifests/rev-1/scene.json"]),
                report: sysutil::join(&[&options.output, "runs/first/report.json"]),
            }
        }
        pub fn edit(&mut self, outcome: Outcome) {
            self.compiles.push_back(Compiled::Updated { seconds: 0.5 });
            self.patches.push_back(outcome);
        }
    }
    impl Steps for Script {
        fn emit(&mut self, event: Line) {
            self.lines
                .push(serde_json::from_str(&event.line()).unwrap());
        }
        fn invalidate(&mut self, _: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        fn compile(&mut self, _: &Context, _: Option<&[String]>) -> anyhow::Result<Compiled> {
            let compiled = self.compiles.pop_front().unwrap_or(Compiled::Unaffected);
            self.compiled |= matches!(compiled, Compiled::Updated { .. });
            Ok(compiled)
        }
        fn compile_pending(&self) -> bool {
            self.compiled
        }
        fn patch_code(&mut self, _: &Context, _: &Window) -> anyhow::Result<Outcome> {
            Ok(self.patches.pop_front().expect("an unexpected code patch"))
        }
        fn pid(&self) -> i32 {
            self.pid as i32
        }
        fn revision(&self) -> String {
            self.revision.clone()
        }
        fn bind_newest(&mut self, _: &Context) -> anyhow::Result<()> {
            Ok(())
        }
        fn prepare(&mut self, _: &Context, _: &Window, _: bool, _: bool) -> anyhow::Result<bool> {
            match self.prepare_error.take() {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => Ok(true),
            }
        }
        // The fixture has one Player process, so a restart names it again.
        fn restart(&mut self, _: &Context, window: &Window) -> anyhow::Result<Restarted> {
            self.revision = "rev-1".into();
            Ok(Restarted {
                event: Line::new("content-updated")
                    .with("paths", &window.paths)
                    .with("manifest", &self.manifest)
                    .with("contentRoutes", "")
                    .seconds("sourceToReadySeconds", 1.5)
                    .with("pid", &(self.pid as i32))
                    .with("report", &self.report),
                compile: std::mem::take(&mut self.compiled)
                    .then(crate::compiler::CompileUpdate::default),
            })
        }
        fn merged_recompile(&mut self) -> Option<String> {
            None
        }
        fn scene(&mut self, _: &Context, window: &Window, _: Instant) -> anyhow::Result<Scene> {
            let pid = self.pid as i32;
            Ok(
                match self.reloads.pop_front().unwrap_or(Reload::Unchanged) {
                    Reload::Unchanged => Scene::Unchanged(
                        Line::new("unchanged")
                            .with("paths", &window.paths)
                            .seconds("prepareSeconds", 0.25)
                            .with("pid", &pid)
                            .with("revision", &self.revision),
                    ),
                    Reload::Updated => {
                        self.revision = "rev-2".into();
                        Scene::Updated(
                        Line::new("updated")
                            .with("paths", &window.paths)
                            .seconds("prepareSeconds", 0.25)
                            .raw(
                                "ack",
                                format!(
                                    r#"{{"success":true,"restartRequired":false,"loadedRevision":"rev-2","pid":{pid}}}"#
                                ),
                            ),
                    )
                    }
                    Reload::Rejected => Scene::Rejected {
                        event: Line::new("update-rejected")
                            .with("error", SCENE_REJECTED)
                            .raw("ack", "null".into()),
                        fatal: None,
                    },
                },
            )
        }
        fn changes_pending(&self) -> bool {
            self.queued
        }
    }

    pub fn window(paths: &[&str], recompile: Option<&str>) -> Window {
        Window {
            paths: paths.iter().map(|path| path.to_string()).collect(),
            changed_at: Instant::now(),
            changed_wall: SystemTime::now(),
            recompile: recompile.map(str::to_string),
        }
    }
    /// Runs one window and lets the controller read every line of it.
    pub fn run(c: &Controller, state: &mut Loop, script: &mut Script, window: &Window) -> State {
        state
            .window(&Context::background(), script, window)
            .unwrap();
        let mut last = c.status();
        for line in std::mem::take(&mut script.lines) {
            last = accept(c, line);
            assert_ne!(last.phase, "failed", "{}", last.last_error);
        }
        last
    }
    pub fn note(code: &str, member: &str) -> Why {
        Why::new(code, member, "A note of the patch builder.")
    }
    pub fn patch(generation: u64, notes: Vec<Why>) -> Outcome {
        Outcome::Patched(Patched {
            generation,
            assemblies: vec!["Assembly-CSharp".into()],
            patched_methods: 1,
            members: vec!["Game.A::M()".into()],
            notes,
            report: format!("/out/{generation}/report.json"),
            ..Patched::default()
        })
    }
    pub fn refused() -> Outcome {
        Outcome::Unsupported(Refusal {
            reasons: vec![Why::new("type_added", "Game.Boss", "A type was added.")],
            report: "/out/refused/report.json".into(),
        })
    }
    pub fn noted() -> Outcome {
        Outcome::Unchanged(Unchanged {
            notes: vec![note("accessibility_changed", "Game.A::speed")],
            report: "/out/noted/report.json".into(),
        })
    }
    pub fn unchanged() -> Outcome {
        Outcome::Unchanged(Unchanged::default())
    }
    pub fn timed_out() -> Outcome {
        Outcome::TimedOut(Failure {
            error: "the Player did not acknowledge the code patch within 20s".into(),
            report: "/out/failed/report.json".into(),
        })
    }
    pub fn report_loop() -> Loop {
        Loop::new(true, Fallback::Report)
    }
}

#[test]
fn the_lines_of_the_watch_loop_are_what_the_controller_reads() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));

    // A patched edit with a note.
    script.edit(patch(
        1,
        vec![note("existing_instances_keep_default", "Game.A::f")],
    ));
    let patched = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    assert_eq!(patched.hot_reload.code_generation, 1);
    let last = patched.hot_reload.last_patch.as_ref().unwrap();
    assert_eq!(
        (last.notes_count, last.report.as_str()),
        (1, "/out/1/report.json")
    );
    assert!(!patched.update_pending);
    assert!(c.wait(&seconds(1), ready.last_sequence, false).is_ok());

    // A save with a C# part that is refused and a scene part that is applied:
    // the scene line is not the answer, the refusal after it is.
    let mark = c.status().last_sequence;
    script.edit(refused());
    script.reloads.push_back(Reload::Updated);
    let refusal = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"], None),
    );
    assert!(refusal.hot_reload.full_recompile_required && !refusal.update_pending);
    assert_eq!(refusal.hot_reload.unsupported[0].code, "type_added");
    assert_eq!(refusal.revision, "rev-2");
    assert_eq!(refusal.last_sequence, mark + 2);
    expect_refused(c.wait(&seconds(1), mark, false), "mixed window");

    // The recompile the controller sends is answered by the loop's restart.
    let worker = recompile_in_background(&c, "rc-1");
    script.compiles.push_back(Compiled::Unchanged);
    let restarted = run(&c, &mut state, &mut script, &window(&[], Some("rc-1")));
    let answer = worker.join().unwrap();
    assert!(
        answer.is_ok(),
        "{:?}",
        answer.err().map(|failure| failure.error.to_string())
    );
    assert!(!restarted.hot_reload.full_recompile_required);
    let restart = restarted.hot_reload.last_restart.as_ref().unwrap();
    assert_eq!(
        (restart.reason.as_str(), restart.request_id.as_str()),
        ("requested", "rc-1")
    );

    // A patch the Player did not acknowledge restarts with its error.
    script.edit(timed_out());
    let failed = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    let restart = failed.hot_reload.last_restart.clone().unwrap();
    assert_eq!(
        (restart.reason.as_str(), restart.report.as_str()),
        ("hot_reload_failed", "/out/failed/report.json")
    );
    assert!(restart.error.contains("within 20s"), "{restart:?}");

    // A compile that needs no patch and still is not all in the Player: the
    // builder's notes are in the state a successful wait returns, for as
    // long as they stand.
    let mark = c.status().last_sequence;
    script.edit(noted());
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    let noted_state = c.wait(&seconds(1), mark, false).unwrap();
    let hot = &noted_state.hot_reload;
    assert!(!hot.full_recompile_required && hot.code_generation == 0);
    assert_eq!(
        (hot.notes.len(), hot.notes_count, hot.notes_report.as_str()),
        (1, 1, "/out/noted/report.json")
    );
    assert_eq!(hot.notes[0].code, "accessibility_changed");
    let encoded = serde_json::to_value(&noted_state).unwrap();
    assert_eq!(encoded["hotReload"]["notes"][0]["member"], "Game.A::speed");
    assert_eq!(
        encoded["hotReload"]["notesReport"],
        "/out/noted/report.json"
    );
    // A scene edit keeps them; the next patch replaces them with its own.
    script.reloads.push_back(Reload::Updated);
    let scene = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/S.unity"], None),
    );
    assert_eq!(scene.hot_reload.notes_count, 1);
    script.edit(patch(1, Vec::new()));
    let patched = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    assert!(patched.hot_reload.notes.is_empty() && patched.hot_reload.notes_report.is_empty());
    assert_eq!(patched.hot_reload.notes_count, 0);
}

#[test]
fn a_refusal_taken_back_in_a_window_whose_scene_part_fails_is_cleared() {
    use loop_lines::*;
    for (reload, prepare_error) in [
        (Reload::Rejected, None),
        (
            Reload::Unchanged,
            Some("prepare prefab: unsupported component"),
        ),
    ] {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
        script.edit(refused());
        let refusal = run(
            &c,
            &mut state,
            &mut script,
            &window(&["/p/Assets/A.cs"], None),
        );
        assert!(refusal.hot_reload.full_recompile_required);
        // The edit is reverted, and a scene part of the same save fails.
        script.edit(unchanged());
        script.reloads.push_back(reload);
        script.prepare_error = prepare_error;
        let rejected = run(
            &c,
            &mut state,
            &mut script,
            &window(&["/p/Assets/A.cs", "/p/Assets/P.prefab"], None),
        );
        // The Player runs the newest code: nothing asks for a recompile, and
        // the wait gives the scene's own error.
        assert!(
            rejected.hot_reload.is_default() && !rejected.code_refused(),
            "{rejected:?}"
        );
        let failure = c
            .wait(&seconds(1), refusal.last_sequence, false)
            .expect_err("the scene part was rejected");
        assert!(failure.typed.is_none(), "{}", failure.error);
        let expected = prepare_error.unwrap_or(SCENE_REJECTED);
        assert_eq!(failure.error.message(), expected);
    }
}

#[test]
fn an_answer_marked_pending_answers_no_wait() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    // Two files saved one after the other: the first one is patched while
    // the second is already queued. A wait from before both must not take
    // the first patch for the answer.
    script.queued = true;
    script.edit(patch(1, Vec::new()));
    let first = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    assert!(first.update_pending && first.hot_reload.code_generation == 1);
    assert_eq!(serde_json::to_value(&first).unwrap()["updatePending"], true);
    for readiness in [false, true] {
        let early = c
            .wait(&millis(150), ready.last_sequence, readiness)
            .expect_err("a pending answer answered a wait");
        assert!(early.error.is_deadline_exceeded(), "{}", early.error);
    }
    // The second window is the last word, and the wait returns its state.
    script.queued = false;
    script.edit(patch(2, Vec::new()));
    let waiter = {
        let (controller, after) = (c.clone(), ready.last_sequence);
        thread::spawn(move || controller.wait(&seconds(3), after, false))
    };
    let second = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/B.cs"], None),
    );
    let answered = waiter.join().unwrap().unwrap();
    assert_eq!(answered.hot_reload.code_generation, 2);
    assert!(!second.update_pending);
    assert!(
        serde_json::to_value(&second)
            .unwrap()
            .get("updatePending")
            .is_none()
    );

    // A compile that raced a save is rejected, and the save's own window
    // compiles both: the rejection reaches nobody who waits.
    let mark = c.status().last_sequence;
    script.queued = true;
    script.prepare_error = Some("runtime source changed while snapshotting: /p/Assets/B.cs");
    let raced = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    assert!(raced.update_pending && raced.last_update_rejected_sequence > mark);
    script.queued = false;
    script.edit(patch(3, Vec::new()));
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/B.cs"], None),
    );
    let healed = c.wait(&seconds(1), mark, false).unwrap();
    assert_eq!(healed.hot_reload.code_generation, 3);
    assert!(healed.update_error.is_empty());

    // A window with two answers: the code part is applied, the scene part
    // rejected. The rejection is the outcome, whenever the waiter wakes.
    let mark = healed.last_sequence;
    script.edit(patch(4, Vec::new()));
    script.reloads.push_back(Reload::Rejected);
    let mixed = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"], None),
    );
    assert_eq!(mixed.last_sequence, mark + 2);
    let failure = c
        .wait(&seconds(1), mark, false)
        .expect_err("the scene part was rejected");
    assert_eq!(failure.error.message(), SCENE_REJECTED);
    assert_eq!(failure.state.hot_reload.code_generation, 4);

    // A session that ends while an answer is pending ends the wait.
    script.queued = true;
    script.reloads.push_back(Reload::Updated);
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/S.unity"], None),
    );
    let mark = c.status().last_sequence - 1;
    c.stop(&seconds(3)).unwrap();
    let stopped = c
        .wait(&seconds(1), mark, false)
        .expect_err("the session stopped");
    assert_eq!(stopped.state.phase, "stopped");
    assert!(!stopped.state.update_pending);
}
