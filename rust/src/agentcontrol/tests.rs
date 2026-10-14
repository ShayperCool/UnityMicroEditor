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
    // SAFETY: descriptor 3 is the inherited event pipe and is used only here.
    let events = std::sync::Arc::new(std::sync::Mutex::new(unsafe {
        libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
        File::from_raw_fd(3)
    }));
    // The `sync` commands that wait for their answer, by request id.
    let syncs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    match mode.as_str() {
        // SAFETY: descriptor 0 belongs to this process and nothing reads it.
        "commands-closed" => drop(unsafe { File::from_raw_fd(0) }),
        // The real loop reads its commands itself.
        "commands-unread" | "watch-loop" => {}
        _ => {
            // A `sync` command is handed to the loop below, which answers it
            // as the runner does: after the event of everything that was
            // "saved" (written as `helper-event.json`) before the command.
            // The other commands are recorded.
            let syncs = syncs.clone();
            thread::spawn(move || {
                for line in io::stdin().lines().map_while(|line| line.ok()) {
                    let command = serde_json::from_str::<Value>(&line).unwrap_or_default();
                    if command["command"] == json!("sync") {
                        syncs.lock().unwrap().push(command["requestId"].clone());
                        continue;
                    }
                    let log = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&commands);
                    let _ = log.and_then(|mut log| writeln!(log, "{line}"));
                }
            });
        }
    }
    match mode.as_str() {
        "overflow" => {
            let _ = events
                .lock()
                .unwrap()
                .write_all(&vec![b'x'; EVENT_LINE_MAXIMUM + 1]);
            idle(false);
        }
        "truncated" => {
            let _ = events.lock().unwrap().write_all(br#"{"event":"ready""#);
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
    let emit = |event: &Value| {
        let mut line = jsondoc::encode(event);
        line.push(b'\n');
        let _ = events.lock().unwrap().write_all(&line);
    };
    let mut readiness =
        json!({"event": "ready", "pid": pid, "manifest": manifest, "report": report});
    if mode == "hot-reload-off" {
        readiness["hotReload"] = json!(false);
    }
    if mode == "watch-loop" {
        // The watcher exists before the session is ready, as in the runner.
        watched_session::run(&project, events.clone(), pid, &manifest, &report, &|| {
            emit(&readiness)
        });
    }
    emit(&readiness);
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: HashMap<String, Instant> = HashMap::new();
    let mut executions = 0;
    let mut last_event = Vec::new();
    // The mode `sync-held` answers a sync only once the file
    // `helper-sync-release` exists, like a runner that is still compiling
    // a save.
    let release = sysutil::join(&[&output, "helper-sync-release"]);
    loop {
        if signalled() && mode != "ignore-signals" {
            process::exit(0);
        }
        // Taken before the event file is read: an event that was written
        // before one of these commands is printed before its answer.
        let answered = match mode == "sync-held" && fs::metadata(&release).is_err() {
            true => Vec::new(),
            false => std::mem::take(&mut *syncs.lock().unwrap()),
        };
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
        for id in answered {
            emit(&json!({"event": "synced", "requestId": id}));
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

/// The runner's own watch loop in the fixture process: the real watcher over
/// the fixture project, the real command reader on the inherited standard
/// input and the real loop, with steps that are scripted by the name of the
/// saved file. The controller of a test then talks to the loop as it talks
/// to a runner, and a test saves files as an agent does.
///   Patch*.cs   compiles and is patched in
///   Refuse*.cs  compiles and cannot be patched
///   Broken*.cs  does not compile
/// `helper-compile-ms` in the project is how long a compile takes.
mod watched_session {
    use super::*;
    use crate::{
        hotreload::{Fallback, Outcome, Patched, Reason as Why, Refusal},
        runner::{
            ProjectWatcher, Wake,
            testing::{
                Commands, Compiled, Event as Line, Loop, Restarted, Scene, Steps, Window,
                ready_event, unchanged_event, watch,
            },
        },
    };
    use std::sync::{Arc, Mutex};

    struct Scripted {
        events: Arc<Mutex<File>>,
        project: String,
        pid: i64,
        manifest: String,
        report: String,
        generation: u64,
        /// What the compile of this window made of its paths.
        outcome: Option<Outcome>,
        compiled: bool,
    }
    impl Steps for Scripted {
        fn emit(&mut self, event: Line) {
            let mut line = event.line().into_bytes();
            line.push(b'\n');
            let _ = self.events.lock().unwrap().write_all(&line);
        }
        fn invalidate(&mut self, _: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        fn compile(&mut self, _: &Context, paths: Option<&[String]>) -> anyhow::Result<Compiled> {
            let slow = fs::read_to_string(sysutil::join(&[&self.project, "helper-compile-ms"]))
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0);
            thread::sleep(Duration::from_millis(slow));
            let names: Vec<String> = paths
                .unwrap_or_default()
                .iter()
                .map(|path| sysutil::base(path))
                .collect();
            let named = |prefix: &str| names.iter().any(|name| name.starts_with(prefix));
            if named("Broken") {
                anyhow::bail!("Assets/Broken.cs(1,1): error CS1002: ; expected");
            }
            self.outcome = if named("Refuse") {
                Some(Outcome::Unsupported(Refusal {
                    reasons: vec![Why::new("type_added", "Game.Boss", "A type was added.")],
                    report: "/out/refused/report.json".into(),
                }))
            } else if named("Patch") {
                self.generation += 1;
                Some(Outcome::Patched(Patched {
                    generation: self.generation,
                    assemblies: vec!["Assembly-CSharp".into()],
                    patched_methods: 1,
                    members: vec!["Game.A::M()".into()],
                    report: format!("/out/{}/report.json", self.generation),
                    ..Patched::default()
                }))
            } else {
                None
            };
            self.compiled |= self.outcome.is_some();
            Ok(match self.outcome {
                Some(_) => Compiled::Updated,
                None => Compiled::Unaffected,
            })
        }
        fn compile_pending(&self) -> bool {
            self.compiled
        }
        fn patch_code(&mut self, _: &Context, _: &Window) -> anyhow::Result<Outcome> {
            Ok(self.outcome.take().expect("a compile before the patch"))
        }
        fn pid(&self) -> i32 {
            self.pid as i32
        }
        fn revision(&self) -> String {
            "rev-1".into()
        }
        fn bind_newest(&mut self, _: &Context) -> anyhow::Result<()> {
            Ok(())
        }
        fn prepare(&mut self, _: &Context, _: &Window, _: bool, _: bool) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn restart(&mut self, _: &Context, window: &Window) -> anyhow::Result<Restarted> {
            self.generation = 0;
            Ok(Restarted {
                event: ready_event(window, &self.manifest, "", self.pid as i32, &self.report),
                compile: std::mem::take(&mut self.compiled)
                    .then(crate::compiler::CompileUpdate::default),
            })
        }
        fn merged_recompile(&mut self) -> Option<String> {
            None
        }
        fn scene(&mut self, _: &Context, window: &Window, _: Instant) -> anyhow::Result<Scene> {
            Ok(Scene::Unchanged(unchanged_event(
                window,
                0.01,
                self.pid as i32,
                "rev-1",
            )))
        }
    }

    pub fn run(
        project: &str,
        events: Arc<Mutex<File>>,
        pid: i64,
        manifest: &str,
        report: &str,
        ready: &dyn Fn(),
    ) -> ! {
        let ctx = Context::background();
        let wake = Wake::new();
        fs::create_dir_all(sysutil::join(&[project, "Assets"])).unwrap();
        let watcher = ProjectWatcher::new(&ctx, &wake, project).unwrap();
        let commands = Commands::read(io::stdin(), &wake);
        ready();
        let mut steps = Scripted {
            events,
            project: project.into(),
            pid,
            manifest: manifest.into(),
            report: report.into(),
            generation: 0,
            outcome: None,
            compiled: false,
        };
        let mut state = Loop::new(true, Fallback::Report);
        let ended = watch(
            &ctx,
            &wake,
            &watcher,
            &commands,
            &mut state,
            &mut steps,
            |_| signalled().then_some(Ok(())),
        );
        process::exit(if ended.is_ok() { 0 } else { 5 });
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
/// The words of a wait that has nothing to report after its mark.
const NOTHING_FOLLOWED: &str = "Nothing was applied or rejected after ";
/// How a scene or asset rejection reads in the session state.
fn scene_part(error: &str) -> String {
    format!("The scene or asset part of the save was rejected: {error}.")
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
    // Progress is no outcome: the runner has answered, nothing follows the
    // mark, and the wait says so instead of waiting for a save nobody made.
    accept(&c, json!({"event": "compiling", "stage": "Roslyn"}));
    let progress = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
    assert!(
        progress.attention.starts_with(NOTHING_FOLLOWED)
            && progress
                .attention
                .contains(&format!("afterSequence {}", ready.last_sequence))
            && progress.last_update_sequence == 0,
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
    // Nothing was saved: the runner answers, and the wait says that nothing
    // followed its mark instead of waiting for a save nobody made.
    let idle = c.handle(
        &seconds(1),
        "session_control",
        br#"{"action":"wait_update","afterSequence":1}"#,
    );
    assert!(
        matches!(&idle, Ok(Reply::State(state)) if state.phase == "ready"
            && state.attention.starts_with(NOTHING_FOLLOWED) && state.events.is_empty()),
        "{idle:?}"
    );
    // A mark that no event of this server has reached is of an earlier
    // server process or another project: the reply covers every event of
    // this server and says so; a wait of no time is no wait.
    let ahead = c.handle(
        &seconds(1),
        "session_control",
        br#"{"action":"wait_update","afterSequence":7}"#,
    );
    assert!(
        matches!(&ahead, Ok(Reply::State(state)) if state.attention.starts_with(
            "Nothing was applied or rejected after afterSequence 7, which is above lastSequence 1 of this server (it was read from an earlier server process or another project"
        )),
        "{ahead:?}"
    );
    let refused = c
        .handle(
            &seconds(1),
            "session_control",
            br#"{"action":"wait","timeoutSeconds":0}"#,
        )
        .expect_err("a wait of no time was taken");
    assert!(
        refused
            .error
            .message()
            .contains("timeoutSeconds must be at least 1"),
        "{}",
        refused.error
    );
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
    // The policy of the next launch is named also while hot reload is off.
    assert_eq!(
        status(&c),
        json!({"enabled": false, "codeGeneration": 0, "fullRecompileRequired": false,
            "unsupported": [], "unsupportedCount": 0, "phase": "ready",
            "onUnsupportedEdit": "restart"})
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
            // The mark of the caller's next wait: a failed call has no state.
            "lastSequence": refused.state.last_sequence,
        })
    );
    assert_eq!(
        refused.error.message(),
        "The C# edit compiled but cannot be applied to the running Player: Game.Boss: a type was added [type_added] (and 2 more). The Player keeps the code it ran before this edit. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset."
    );
    let state = refused.state.clone();
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

    // A caller that has read the refusal and asks again is told again, at
    // once: the Player still does not run the newest compiled code.
    for readiness in [false, true] {
        let again = expect_refused(
            c.wait(&millis(2000), state.last_sequence, readiness),
            "the refusal with its own sequence as the mark",
        );
        assert_eq!(again.error.message(), refused.error.message());
    }

    // A compile error after the refusal keeps today's untyped reply, and the
    // newest compiled code still waits for the recompile.
    accept(
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
    accept(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision}),
    );
    let scene = expect_refused(
        c.wait(&seconds(1), compile.state.last_sequence, false),
        "scene update",
    );
    assert_eq!(
        scene.error.message(),
        // The confirmation was an `unchanged`: nothing was applied, and the
        // reply does not say that something was.
        "The C# edit compiled but cannot be applied to the running Player: Game.Boss: a type was added [type_added] (and 2 more). The Player keeps the code it ran before this edit. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset."
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
    // The model: whether a refusal stands, and the last confirmed update
    // with the event that confirmed it.
    let (mut stale, mut confirmed, mut by) = (false, 0, "");
    for ordering in orderings(&["refusal", "updated", "unchanged", "code-patched"]) {
        let began = c.status().last_sequence;
        for (index, name) in ordering.iter().enumerate() {
            let state = accept(&c, outcome(&c, name));
            match *name {
                "refusal" => stale = true,
                "code-patched" => (stale, confirmed, by) = (false, state.last_sequence, *name),
                _ => (confirmed, by) = (state.last_sequence, *name),
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
                    // What the wait did bring is named by what it was: a
                    // scene update, or an earlier edit that was patched in.
                    // An `unchanged` applied nothing.
                    let brought = match (confirmed > after, by) {
                        (true, "updated") => format!(
                            "; a scene or asset change confirmed at sequence {confirmed} is applied"
                        ),
                        (true, "code-patched") => format!(
                            "; an earlier C# edit was applied at sequence {confirmed} and is in the Player (hotReload.lastPatch)"
                        ),
                        _ => String::new(),
                    };
                    assert!(
                        message.contains("Game.Boss: a type was added [type_added] (and 2 more)")
                            && message.contains(&format!(
                                "The Player keeps the code it ran before this edit{brought}. Call hot_reload"
                            )),
                        "{context}: {message}"
                    );
                    assert_eq!(
                        failure.typed.unwrap().details["lastSequence"],
                        state.last_sequence
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
        // A scene reload leaves a ready Player, and the compile error that
        // follows is still the outcome for both waits: the Player does not
        // run what was saved.
        (&["updated", "compile-error"], Rejected, Rejected),
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
        // Progress is no outcome: both waits say that nothing followed.
        accept(&c, json!({"event": "compiling", "stage": "Roslyn"}));
        for is_ready in [false, true] {
            let waiting = c.wait(&seconds(1), ready.last_sequence, is_ready).unwrap();
            assert!(
                waiting.attention.starts_with(NOTHING_FOLLOWED)
                    && waiting.last_update_sequence == 0,
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
                    Ok(mut applied) => {
                        // A restart the runner made by itself is said first.
                        let attention = std::mem::take(&mut applied.attention);
                        assert_eq!(
                            attention
                                .starts_with("The edit could not be applied to the running Player"),
                            events.contains(&"restart"),
                            "{context}: {attention}"
                        );
                        assert!(
                            applied == state && !state.hot_reload.full_recompile_required,
                            "{context}: {applied:?}"
                        )
                    }
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
            "The C# edit compiled but cannot be applied to the running Player: the C# edit needs a restart; see the report. The Player keeps the code it ran before this edit; a scene or asset change confirmed at sequence {} is applied. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.",
            failure.state.last_update_sequence
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
    // Such a runner cannot say either whether it has seen the last saved
    // change, so no wait vouches for it: the session is seen ready in its
    // state, and both waits name the reset as well.
    let started = |c: &Controller| -> State {
        c.start().unwrap();
        wait_for("a ready Player", || c.status().phase == "ready");
        c.status()
    };
    let c = controller_fixture("commands-closed");
    let ready = started(&c);
    reported(&c, &ready, "broken pipe");
    for readiness in [false, true] {
        let failure = c
            .wait(&seconds(1), 0, readiness)
            .expect_err("a wait answered for a runner that takes no commands");
        let message = failure.error.message();
        assert!(
            failure.typed.is_none()
                && message.contains("takes no commands")
                && message.contains("session_control(reset)"),
            "{message}"
        );
    }

    // A runner that stopped reading leaves a full pipe. The write must fail
    // there: the controller holds the session lock while it writes.
    let c = controller_fixture("commands-unread");
    let ready = started(&c);
    // The command is written and never answered: the wait says what it
    // waited for instead of answering from what it knows.
    let unanswered = c
        .wait(&millis(100), 0, true)
        .expect_err("a wait answered without the runner's word");
    assert!(
        unanswered.error.is_deadline_exceeded()
            && unanswered.typed.as_ref().is_some_and(|typed| {
                typed.code == "wait_timeout" && typed.details["runnerAnswered"] == false
            }),
        "{} {:?}",
        unanswered.error,
        unanswered.typed
    );
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
        accept(&c, refusal(&format!("/owned/report-{after}.json")));
        let refused = c
            .wait(&seconds(1), after, false)
            .expect_err("refusal was not reported");
        assert!(refused.state.hot_reload.full_recompile_required);
        *refused.state
    };
    // The edit was reverted: the compiled code equals the loaded code again.
    let refused = refuse(ready.last_sequence);
    accept(
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
    accept(
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
                "a scene or asset change confirmed at sequence {} is applied",
                scene.state.last_update_sequence
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
    pub(crate) use crate::hotreload::Reason as Why;
    pub(crate) use crate::runner::testing::Compiled;
    use crate::{
        hotreload::{Failure, Fallback, Outcome, Patched, Refusal, Unchanged},
        runner::testing::{
            Event as Line, Loop, Restarted, Scene, Steps, Window, ready_event,
            scene_rejected_event, unchanged_event, updated_event,
        },
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
        /// The content routes of the next window differ from the Player's,
        /// so the window restarts it.
        pub routes_differ: bool,
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
                routes_differ: false,
                compiled: false,
                pid: ready.pid,
                revision: ready.revision.clone(),
                manifest: sysutil::join(&[&options.project, "Temp/manifests/rev-1/scene.json"]),
                report: sysutil::join(&[&options.output, "runs/first/report.json"]),
            }
        }
        pub fn edit(&mut self, outcome: Outcome) {
            self.compiles.push_back(Compiled::Updated);
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
            self.compiled |= matches!(compiled, Compiled::Updated);
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
                None => Ok(!std::mem::take(&mut self.routes_differ)),
            }
        }
        // The fixture has one Player process, so a restart names it again.
        fn restart(&mut self, _: &Context, window: &Window) -> anyhow::Result<Restarted> {
            self.revision = "rev-1".into();
            Ok(Restarted {
                event: ready_event(window, &self.manifest, "", self.pid as i32, &self.report),
                compile: std::mem::take(&mut self.compiled)
                    .then(crate::compiler::CompileUpdate::default),
            })
        }
        fn merged_recompile(&mut self) -> Option<String> {
            None
        }
        // The events are made by the functions the runner's own steps use;
        // only the Player's acknowledgement is written here.
        fn scene(
            &mut self,
            _: &Context,
            window: &Window,
            started: Instant,
        ) -> anyhow::Result<Scene> {
            let pid = self.pid as i32;
            Ok(
                match self.reloads.pop_front().unwrap_or(Reload::Unchanged) {
                    Reload::Unchanged => {
                        Scene::Unchanged(unchanged_event(window, 0.25, pid, &self.revision))
                    }
                    Reload::Updated => {
                        self.revision = "rev-2".into();
                        Scene::Updated(updated_event(
                            window,
                            0.25,
                            started,
                            format!(
                                r#"{{"success":true,"restartRequired":false,"loadedRevision":"rev-2","pid":{pid}}}"#
                            ),
                            &json!({"hits": 1, "misses": 0}),
                        ))
                    }
                    Reload::Rejected => Scene::Rejected {
                        event: scene_rejected_event(SCENE_REJECTED, "null".into()),
                        fatal: None,
                    },
                },
            )
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
        let queued = script.queued;
        state
            .window(&Context::background(), script, window, &|| queued)
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
        let expected = scene_part(prepare_error.unwrap_or(SCENE_REJECTED));
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
    // The error is the scene's, and the C# part of the same save is live.
    assert_eq!(
        failure.error.message(),
        format!(
            "{} The C# edit of the same save is applied (code generation 4).",
            scene_part(SCENE_REJECTED)
        )
    );
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

/// A wait is the caller's question "did what I saved arrive". The runner
/// may have answered an earlier save and not yet looked at a later one;
/// what the controller knows then says "applied". No wait answers from it
/// before the runner has said that it answered for everything saved before
/// the call.
#[test]
fn a_wait_is_held_until_the_runner_has_answered_for_what_was_saved_before_it() {
    let c = controller_fixture("sync-held");
    c.start().unwrap();
    wait_for("a ready Player", || c.status().phase == "ready");
    let ready = c.status();
    // The answer to a first save. Nothing in it is pending.
    let first = accept(&c, outcome(&c, "code-patched"));
    assert!(first.last_update_sequence > ready.last_sequence && !first.update_pending);
    // A second save is being compiled; the runner has printed nothing about
    // it. A wait with the mark from before both saves, with a later mark and
    // without one finds an applied update and must not take it for the answer.
    for (after, readiness) in [
        (ready.last_sequence, false),
        (ready.last_sequence, true),
        (0, false),
        (0, true),
    ] {
        let early = c
            .wait(&millis(150), after, readiness)
            .expect_err("a wait answered before the runner did");
        let typed = early.typed.expect("a wait that ran out of time says why");
        assert!(early.error.is_deadline_exceeded(), "{}", early.error);
        assert_eq!(typed.code, "wait_timeout");
        assert_eq!(
            (
                &typed.details["runnerAnswered"],
                &typed.details["afterSequence"]
            ),
            (&json!(false), &json!(after))
        );
        assert!(
            early.error.message().contains("may still be compiling")
                && early.error.message().contains("Repeat the call"),
            "{}",
            early.error
        );
    }
    let waiter = {
        let (controller, after) = (c.clone(), ready.last_sequence);
        thread::spawn(move || controller.wait(&seconds(3), after, false))
    };
    thread::sleep(Duration::from_millis(100));
    assert!(
        !waiter.is_finished(),
        "the wait answered for the first save"
    );
    // The second save cannot be applied; then the runner answers the command.
    accept(&c, refusal("/owned/report.json"));
    fs::write(
        sysutil::join(&[&c.0.options.output, "helper-sync-release"]),
        "",
    )
    .unwrap();
    expect_refused(waiter.join().unwrap(), "the wait after two saves");
}

/// The same with the runner's own loop in the fixture process: its watcher
/// over the project, its command reader, its windows. The saves are files,
/// as an agent writes them, and each compile takes a while.
#[test]
fn one_wait_after_two_saves_reports_the_last_of_them() {
    let c = controller_fixture("watch-loop");
    let project = c.0.options.project.clone();
    fs::write(sysutil::join(&[&project, "helper-compile-ms"]), "400").unwrap();
    let ready = start_ready(&c);
    let save = |name: &str| fs::write(sysutil::join(&[&project, "Assets", name]), "x").unwrap();
    let mark = ready.last_sequence;

    // The first save is patched in, and the runner's answer is its last
    // word: nothing is queued when it prints it.
    save("PatchA.cs");
    wait_for("the first answer", || {
        c.status().last_update_sequence > mark
    });
    let first = c.status();
    assert!(
        first.hot_reload.code_generation == 1 && !first.update_pending,
        "{first:?}"
    );
    // The second save comes after that answer and cannot be patched. The
    // wait is called at once, with the mark from before the first save.
    save("RefuseB.cs");
    let started = Instant::now();
    let waited = c.wait(&seconds(3), mark, false);
    let refused = expect_refused(waited, "one wait after two saves");
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "the wait did not span the compile of the second save: {:?}",
        started.elapsed()
    );
    assert_eq!(refused.state.hot_reload.unsupported[0].code, "type_added");
    // The same call without a mark and as a readiness wait.
    expect_refused(c.wait(&seconds(3), 0, false), "no mark");
    expect_refused(c.wait(&seconds(3), mark, true), "readiness");

    // The refused file is saved again as code that can be patched: the wait
    // that follows the save reports the patch, not the refusal it finds.
    save("PatchB.cs");
    let patched = c
        .wait(&seconds(3), refused.state.last_sequence, false)
        .expect("the patch after the refusal");
    assert!(
        patched.hot_reload.code_generation == 2 && !patched.hot_reload.full_recompile_required,
        "{patched:?}"
    );
    // Two saves in a row while the first is being compiled: the answer to
    // the first is marked, and one wait reports the second.
    let mark = patched.last_sequence;
    save("PatchC.cs");
    thread::sleep(Duration::from_millis(150));
    save("RefuseD.cs");
    expect_refused(c.wait(&seconds(3), mark, false), "a save during a compile");
    let events: Vec<(String, bool)> = c
        .status()
        .events
        .iter()
        .filter(|event| event.sequence > mark)
        .map(|event| {
            (
                event.data["event"].as_str().unwrap().to_string(),
                event.data.get("pending") == Some(&json!(true)),
            )
        })
        .collect();
    assert_eq!(
        events,
        [
            ("code-patched".to_string(), true),
            ("update-rejected".to_string(), false)
        ]
    );
    // The answer to the command is no event of the session.
    assert!(
        c.status()
            .events
            .iter()
            .all(|event| event.data["event"] != "synced")
    );
}

/// Each of the six answers of a window sets `updatePending` when the runner
/// marks it and clears it when it does not, and a wait follows that. A
/// progress line between two answers changes nothing.
#[test]
fn every_answer_of_a_window_sets_and_clears_update_pending() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    let step = |script: &mut Script,
                state: &mut crate::runner::testing::Loop,
                name: &str,
                pending: bool,
                paths: &[&str]| {
        let mark = c.status().last_sequence;
        script.queued = pending;
        let after = run(&c, state, script, &window(paths, None));
        let last = after.events.last().unwrap().data["event"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(last, name);
        assert_eq!(after.update_pending, pending, "{name}, marked {pending}");
        if pending {
            // Progress is no answer, and takes none back.
            let progress = accept(&c, json!({"event": "native-discovery", "count": 3}));
            assert!(
                progress.update_pending,
                "{name}: a progress line ended the mark"
            );
        }
        let waited = c.wait(&millis(if pending { 150 } else { 2000 }), mark, false);
        match pending {
            true => {
                let failure = waited.expect_err("a pending answer answered a wait");
                assert!(
                    failure.error.is_deadline_exceeded(),
                    "{name}: {}",
                    failure.error
                );
                let typed = failure.typed.expect("the wait says what it waited for");
                assert_eq!(
                    (
                        typed.code,
                        &typed.details["updatePending"],
                        &typed.details["runnerAnswered"]
                    ),
                    ("wait_timeout", &json!(true), &json!(true)),
                    "{name}"
                );
            }
            false => assert!(
                waited.is_ok(),
                "{name}: the last word did not answer the wait"
            ),
        }
    };
    let (scene, source, routes) = (
        &["/p/Assets/S.unity"][..],
        &["/p/Assets/A.cs"][..],
        &["/p/Assets/B.unity"][..],
    );
    step(&mut script, &mut state, "unchanged", true, scene);
    step(&mut script, &mut state, "unchanged", false, scene);
    script.reloads.push_back(Reload::Updated);
    step(&mut script, &mut state, "updated", true, scene);
    script.reloads.push_back(Reload::Updated);
    step(&mut script, &mut state, "updated", false, scene);
    script.edit(patch(1, Vec::new()));
    step(&mut script, &mut state, "code-patched", true, source);
    // Changed routes restart the Player: on the compile the patch brought,
    // and then with nothing newer to load.
    script.routes_differ = true;
    step(&mut script, &mut state, "code-updated", false, routes);
    script.edit(patch(1, Vec::new()));
    step(&mut script, &mut state, "code-patched", true, source);
    script.routes_differ = true;
    step(&mut script, &mut state, "code-updated", true, routes);
    script.routes_differ = true;
    step(&mut script, &mut state, "content-updated", false, routes);
    script.routes_differ = true;
    step(&mut script, &mut state, "content-updated", true, routes);
    script.prepare_error = Some("import failed");
    step(&mut script, &mut state, "update-rejected", true, scene);
    step(&mut script, &mut state, "unchanged", false, scene);
}

/// What ends the notes in the state besides a patch: a compile that needs
/// no patch and has none, and a refusal.
#[test]
fn notes_in_the_state_end_with_a_plain_confirmation_and_with_a_refusal() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    let source = window(&["/p/Assets/A.cs"], None);
    script.edit(noted());
    let stated = run(&c, &mut state, &mut script, &source);
    assert_eq!(
        (
            stated.hot_reload.notes_count,
            stated.hot_reload.new_notes_count
        ),
        (1, 1)
    );
    // The change is taken back: the compile needs no patch and has no notes.
    script.edit(unchanged());
    let plain = run(&c, &mut state, &mut script, &source);
    assert!(plain.hot_reload.is_default(), "{plain:?}");
    script.edit(noted());
    assert_eq!(
        run(&c, &mut state, &mut script, &source)
            .hot_reload
            .notes_count,
        1
    );
    // An edit that is refused: nothing of the newest compile is in the Player.
    script.edit(refused());
    let refusal = run(&c, &mut state, &mut script, &source).hot_reload;
    assert!(refusal.full_recompile_required);
    assert!(
        refusal.notes.is_empty()
            && refusal.notes_count == 0
            && refusal.notes_report.is_empty()
            && refusal.new_notes.is_empty()
            && refusal.new_notes_count == 0
            && refusal.new_notes_sequence == 0,
        "{refusal:?}"
    );
}

/// A wait that succeeds while the patch has a limit says so before anything
/// else, for the notes the answers after its mark brought and for no
/// others, in words that claim no more than is known.
#[test]
fn a_wait_whose_answer_brought_notes_says_so_first() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    let source = window(&["/p/Assets/A.cs"], None);
    let keeps = Why {
        revision: "aa".into(),
        ..Why::new(
            "running_coroutines_keep_old_code",
            "Game.A::Loop()",
            "A coroutine that is already running finishes on the old code.",
        )
    };
    const HELD: &str = "The edit is applied, with a limit that lasts until the Player restarts: ";
    const ADVICE: &str = "If what you are checking depends on it, hot_reload with action \"recompile\" restarts the Player on the saved code and resets game state; otherwise go on. A process of the same game that starts on the saved code meanwhile (another Player of a network game, for instance) runs all of it, so the two may not agree until this Player restarts too.";

    script.edit(patch(1, vec![keeps.clone()]));
    run(&c, &mut state, &mut script, &source);
    let waited = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
    assert_eq!(
        waited.attention,
        format!(
            "{HELD}A coroutine that is already running finishes on the old code [running_coroutines_keep_old_code]. {ADVICE}"
        )
    );
    assert_eq!(
        (
            waited.hot_reload.new_notes_count,
            waited.hot_reload.new_notes_sequence,
            waited.hot_reload.new_notes[0].member.as_str()
        ),
        (1, waited.last_sequence, "Game.A::Loop()")
    );
    // It is the first member of the reply, and no part of the state a
    // status call reads.
    let encoded = String::from_utf8(jsondoc::marshal(&waited).unwrap()).unwrap();
    assert!(
        encoded.starts_with(r#"{"attention":"The edit is applied"#),
        "{encoded}"
    );
    assert!(c.status().attention.is_empty());
    // A caller whose mark is before this answer is told again; one that
    // gives no mark has read it with the wait above and is told that
    // nothing followed.
    assert!(
        c.wait(&seconds(1), waited.last_sequence - 1, false)
            .unwrap()
            .attention
            .starts_with(HELD)
    );
    let again = c.wait_within(&seconds(1), None, false, None).unwrap();
    assert!(
        again.attention.starts_with(NOTHING_FOLLOWED) && again.hot_reload.new_notes.is_empty(),
        "{again:?}"
    );

    // An edit elsewhere: the note stands in the patch and is no news.
    let mark = c.status().last_sequence;
    script.edit(patch(2, vec![keeps.clone()]));
    run(&c, &mut state, &mut script, &source);
    let elsewhere = c.wait(&seconds(1), mark, false).unwrap();
    assert!(elsewhere.attention.is_empty(), "{}", elsewhere.attention);
    assert_eq!(
        (
            elsewhere.hot_reload.new_notes_count,
            elsewhere
                .hot_reload
                .last_patch
                .as_ref()
                .unwrap()
                .notes_count
        ),
        (0, 1)
    );
    // Two saves in two windows and ONE wait with the mark from before the
    // first: what the first window brought is told, although the answer of
    // the second, an edit elsewhere, brought nothing. This is the group of
    // edits a caller is told to make before one wait.
    let mark = elsewhere.last_sequence;
    let changed = Why {
        revision: "bb".into(),
        ..keeps.clone()
    };
    script.edit(patch(3, vec![changed.clone()]));
    run(&c, &mut state, &mut script, &source);
    script.edit(patch(4, vec![changed.clone()]));
    let second = run(&c, &mut state, &mut script, &source);
    assert_eq!(second.hot_reload.new_notes_count, 0, "{second:?}");
    let grouped = c.wait(&seconds(1), mark, false).unwrap();
    assert!(grouped.attention.starts_with(HELD), "{}", grouped.attention);
    assert_eq!(
        (
            grouped.hot_reload.new_notes_count,
            grouped.hot_reload.new_notes_sequence,
            grouped.hot_reload.new_notes[0].code.as_str()
        ),
        (
            1,
            grouped.last_sequence - 1,
            "running_coroutines_keep_old_code"
        )
    );
    // A status reads the state as it is: the last answer brought nothing.
    assert_eq!(c.status().hot_reload.new_notes_count, 0);
    // A caller that has read the first of the two answers is told nothing.
    let later = c
        .wait(&seconds(1), grouped.last_sequence - 1, false)
        .unwrap();
    assert!(later.attention.is_empty(), "{}", later.attention);

    // The same over three windows, and the edit of the first one is taken
    // back in the third: the patch has the note no more, and nothing is
    // said about a limit that has ended.
    let mark = later.last_sequence;
    let field = note("existing_instances_keep_default", "Game.A::f");
    script.edit(patch(5, vec![changed.clone(), field.clone()]));
    run(&c, &mut state, &mut script, &source);
    script.edit(patch(6, vec![changed.clone(), field.clone()]));
    run(&c, &mut state, &mut script, &source);
    let both = c.wait(&seconds(1), mark, false).unwrap();
    assert!(
        both.attention.contains("[existing_instances_keep_default]"),
        "{}",
        both.attention
    );
    script.edit(patch(7, vec![changed.clone()]));
    run(&c, &mut state, &mut script, &source);
    let taken_back = c.wait(&seconds(1), mark, false).unwrap();
    assert!(
        taken_back.attention.is_empty() && taken_back.hot_reload.new_notes.is_empty(),
        "{taken_back:?}"
    );

    // Several new notes: the sentence names the one that matters most, not
    // the first of the builder's order, and counts the others. A note about
    // lookups by name alone is listed and not worth the sentence.
    let mark = taken_back.last_sequence;
    let mut notes = vec![
        changed.clone(),
        note("added_method_invisible_by_name", "Game.A::Helper()"),
        note("existing_instances_keep_default", "Game.A::f"),
        Why {
            revision: "cc".into(),
            ..Why::new(
                "added_method_named_by_string",
                "Game.A::Blast()",
                "The string \"Blast\" in code of Game.A names the added method Game.A::Blast().",
            )
        },
    ];
    notes.sort_by(|a, b| a.member.cmp(&b.member));
    script.edit(patch(8, notes.clone()));
    run(&c, &mut state, &mut script, &source);
    let several = c.wait(&seconds(1), mark, true).unwrap();
    assert!(
        several.attention.starts_with(&format!(
            "{HELD}The string \"Blast\" in code of Game.A names the added method Game.A::Blast() [added_method_named_by_string] (and 2 more in hotReload.newNotes). "
        )),
        "{}",
        several.attention
    );
    assert_eq!(
        several
            .hot_reload
            .new_notes
            .iter()
            .map(|note| note.code.as_str())
            .collect::<Vec<_>>(),
        [
            "added_method_named_by_string",
            "existing_instances_keep_default",
            "added_method_invisible_by_name"
        ]
    );
    // Alone, the note about lookups by name is in the list and no reason to
    // read anything first.
    let mark = several.last_sequence;
    notes.push(note("added_method_invisible_by_name", "Game.A::Other()"));
    script.edit(patch(9, notes.clone()));
    run(&c, &mut state, &mut script, &source);
    let minor = c.wait(&seconds(1), mark, false).unwrap();
    assert!(
        minor.attention.is_empty() && minor.hot_reload.new_notes_count == 1,
        "{minor:?}"
    );
    // A value that was lost is told without the offer of a restart, which
    // would not bring it back.
    let mark = minor.last_sequence;
    notes.push(Why::new(
        "field_value_reset",
        "Game.A::speed",
        "Field Game.A::speed changed its type; its previous value is lost.",
    ));
    script.edit(patch(10, notes.clone()));
    run(&c, &mut state, &mut script, &source);
    let reset = c.wait(&seconds(1), mark, false).unwrap();
    assert_eq!(
        reset.attention,
        "The edit is applied and its code runs. Note: Field Game.A::speed changed its type; its previous value is lost [field_value_reset]."
    );

    // More new notes than a list holds: the sentence says where all are.
    let mark = reset.last_sequence;
    for index in 0..20 {
        notes.push(note(
            "existing_instances_keep_default",
            &format!("Game.B::f{index:02}"),
        ));
    }
    script.edit(patch(11, notes.clone()));
    let many = run(&c, &mut state, &mut script, &source);
    let report = many.hot_reload.last_patch.as_ref().unwrap().report.clone();
    let many = c.wait(&seconds(1), mark, false).unwrap();
    assert!(
        many.attention.contains(&format!(
            "(and 19 more notes: hotReload.newNotes lists 16 of them, {report} all)"
        )),
        "{}",
        many.attention
    );
    assert_eq!(
        (
            many.hot_reload.new_notes.len(),
            many.hot_reload.new_notes_count
        ),
        (16, 20)
    );

    // A change that needs no patch and is not all in the Player.
    let mark = many.last_sequence;
    script.edit(unchanged());
    run(&c, &mut state, &mut script, &source);
    script.edit(noted());
    run(&c, &mut state, &mut script, &source);
    let noted = c.wait(&seconds(1), mark, false).unwrap();
    assert!(
        noted.hot_reload.new_notes[0].code == "accessibility_changed"
            && noted.hot_reload.notes_count == 1,
        "{noted:?}"
    );
    // A scene update while the notes stand repeats them, and brings none.
    let mark = noted.last_sequence;
    script.reloads.push_back(Reload::Updated);
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/S.unity"], None),
    );
    let scene = c.wait(&seconds(1), mark, false).unwrap();
    assert!(
        scene.attention.is_empty()
            && scene.hot_reload.new_notes.is_empty()
            && scene.hot_reload.notes_count == 1,
        "{scene:?}"
    );
}

/// A scene that the running Player rejects while a refusal stands is named
/// in the typed reply with its own error: a scene that only lacks the new
/// code and one that is wrong by itself are otherwise the same reply.
#[test]
fn the_refusal_names_the_scene_change_the_player_rejected() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    script.edit(refused());
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/A.cs"], None),
    );
    let plain = expect_refused(c.wait(&seconds(1), ready.last_sequence, false), "the edit");
    assert!(
        plain
            .typed
            .as_ref()
            .unwrap()
            .details
            .get("rejected")
            .is_none()
            && !plain.error.message().contains("was rejected"),
        "{}",
        plain.error
    );
    // The scene edit that uses the new type.
    let mark = plain.state.last_sequence;
    script.reloads.push_back(Reload::Rejected);
    let rejected = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/S.unity"], None),
    );
    assert_eq!(rejected.events.last().unwrap().data["part"], "scene");
    for (after, readiness) in [(mark, false), (mark, true), (ready.last_sequence, false)] {
        let failure = expect_refused(c.wait(&seconds(1), after, readiness), "the scene edit");
        assert_eq!(
            failure.typed.as_ref().unwrap().details["rejected"],
            json!({"error": SCENE_REJECTED, "sequence": rejected.last_sequence})
        );
        assert!(
            failure.error.message().contains(&format!(
                "The Player keeps the code it ran before this edit. A scene or asset change saved since was rejected by the running Player: {SCENE_REJECTED}. If it only needs the new code, the recompile applies it. Call hot_reload"
            )),
            "{}",
            failure.error
        );
    }
    // A caller that has read this rejection is told the refusal alone.
    let read = expect_refused(c.wait(&seconds(1), rejected.last_sequence, false), "read");
    assert!(read.typed.unwrap().details.get("rejected").is_none());
    // The scene is saved in a form the Player loads: nothing is rejected
    // any more, and the reply says what was applied.
    script.reloads.push_back(Reload::Updated);
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/S.unity"], None),
    );
    let applied = expect_refused(c.wait(&seconds(1), mark, false), "the repaired scene");
    assert!(
        applied
            .typed
            .as_ref()
            .unwrap()
            .details
            .get("rejected")
            .is_none()
            && applied.error.message().contains(&format!(
                "a scene or asset change confirmed at sequence {} is applied. Call hot_reload",
                applied.state.last_update_sequence
            )),
        "{}",
        applied.error
    );
}

/// A reply lists the events of its call, not the last 64 of the session.
#[test]
fn replies_list_the_newest_events_and_a_wait_those_after_its_mark() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let background = Context::background();
    let call = |name: &str, arguments: Value| -> State {
        match c.handle(&background, name, &jsondoc::encode(&arguments)) {
            Ok(Reply::State(state)) => *state,
            other => panic!("{other:?}"),
        }
    };
    for index in 0..20 {
        accept(&c, json!({"event": "native-discovery", "index": index}));
    }
    let mark = c.status().last_sequence;
    accept(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"sourceHashes": {"Assets/A.cs": "0123"}, "compiledAssemblies": ["Assembly-CSharp"],
                "notes": [{"code": "accessibility_changed", "member": "A::B()", "message": "stands"}], "notesCount": 1,
                "newNotes": [{"code": "accessibility_changed", "member": "A::B()", "message": "stands"}], "newNotesCount": 1}),
        ),
    );
    let sequences =
        |state: &State| -> Vec<u64> { state.events.iter().map(|event| event.sequence).collect() };
    // The session keeps all of them.
    assert_eq!(c.status().events.len(), 22);
    let status = call("session_control", json!({"action": "status"}));
    assert_eq!(
        sequences(&status),
        (mark - 6..=mark + 1).collect::<Vec<_>>()
    );
    assert_eq!(
        (status.events_omitted, status.last_sequence),
        (14, mark + 1)
    );
    // The same reply where it is made without the handler: a status while
    // a preparation runs.
    let made = c.status_reply();
    assert_eq!(
        (sequences(&made), made.events_omitted),
        (sequences(&status), 14)
    );
    // The hashes of every source are the runner's own record, and the notes
    // that stand and those that are new are in the state: an event keeps
    // their counts.
    let last = &status.events.last().unwrap().data;
    assert!(last.get("sourceHashes").is_none() && last["compiledAssemblies"].is_array());
    assert!(
        last.get("notes").is_none()
            && last["notesCount"] == 1
            && last.get("newNotes").is_none()
            && last["newNotesCount"] == 1,
        "{last:?}"
    );
    // The session keeps the event as the runner printed it.
    assert!(c.status().events.last().unwrap().data["notes"].is_array());
    assert!(
        c.status()
            .events
            .last()
            .unwrap()
            .data
            .get("sourceHashes")
            .is_some()
    );
    let none = call("session_control", json!({"action": "status", "events": 0}));
    assert!(none.events.is_empty() && none.events_omitted == 22);
    let encoded = String::from_utf8(jsondoc::marshal(&none).unwrap()).unwrap();
    assert!(!encoded.contains("\"events\":") && encoded.contains("\"eventsOmitted\":22"));
    let all = call("session_control", json!({"action": "status", "events": 64}));
    assert_eq!((all.events.len(), all.events_omitted), (22, 0));
    // A wait: what came after its mark.
    let waited = call(
        "session_control",
        json!({"action": "wait_update", "afterSequence": mark}),
    );
    assert_eq!(
        (sequences(&waited), waited.events_omitted),
        (vec![mark + 1], 21)
    );
    // A wait without a mark: what came after the last wait that answered,
    // which is nothing here, and the whole session for the mark 0.
    let unmarked = call("session_control", json!({"action": "wait"}));
    assert!(
        unmarked.events.is_empty() && unmarked.attention.starts_with(NOTHING_FOLLOWED),
        "{unmarked:?}"
    );
    let whole = call(
        "session_control",
        json!({"action": "wait", "afterSequence": 0}),
    );
    assert_eq!(whole.events.len(), 16);
    // A wait that is given a time says what it waited for when it is over,
    // long before the caller's own patience ends. The runner marks its
    // answer as not its last: it is still working on what was saved.
    accept(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": c.status().revision, "pending": true}),
    );
    let asked = Instant::now();
    let failure = c
        .handle(
            &seconds(3),
            "session_control",
            &jsondoc::encode(
                &json!({"action": "wait_update", "afterSequence": mark + 1, "timeoutSeconds": 1}),
            ),
        )
        .expect_err("a wait answered on an answer the runner marked as pending");
    assert!(
        asked.elapsed() < Duration::from_secs(15),
        "timeoutSeconds did not end the wait: {:?}",
        asked.elapsed()
    );
    let typed = failure.typed.expect("a typed timeout");
    assert_eq!(typed.code, "wait_timeout");
    assert_eq!(
        typed.details,
        json!({"afterSequence": mark + 1, "phase": "ready", "lastSequence": mark + 2,
            "lastUpdateSequence": mark + 2, "lastUpdateRejectedSequence": 0,
            "updatePending": true, "runnerAnswered": true})
    );
    assert!(
        failure
            .error
            .message()
            .contains("the session is still working on saved source changes. Nothing was decided. Repeat the call"),
        "{}",
        failure.error
    );
}

/// A recompile cannot apply a change that has left the prepared graph. Its
/// answer is the runner's rejection, and it names what does apply it.
#[test]
fn a_recompile_that_needs_a_new_graph_names_what_applies_the_change() {
    let c = controller_fixture("");
    start_ready(&c);
    const REJECTION: &str =
        "managed assembly configuration/GUID changed; regenerate source graph: Assets/Old.cs.meta";
    let worker = recompile_in_background(&c, "rc-graph");
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": REJECTION, "preparationRequired": true, "requestId": "rc-graph"}),
    );
    let failure = worker
        .join()
        .unwrap()
        .expect_err("a recompile applied a change that needs a preparation");
    let message = failure.error.message();
    assert!(
        failure.typed.is_none()
            && failure.state.preparation_required
            && message.starts_with(&format!(
                "{REJECTION}. The change needs a new preparation of the project"
            ))
            && message.contains("session_control(wait_update)"),
        "{message}"
    );
    // While the rejection stands another recompile is refused before the
    // runner is asked (session6-4); an update ends it.
    let again = c
        .recompile(&seconds(1), "rc-again")
        .expect_err("a recompile while the rejection stands");
    assert!(
        again.error.message().starts_with(&format!(
            "{REJECTION}. The change needs a new preparation of the project"
        )),
        "{}",
        again.error
    );
    emit_fixture_event(&c, updated(c.status().pid));
    wait_for("the update", || !c.status().preparation_required);
    // A compile error is the compiler's text and nothing else.
    let worker = recompile_in_background(&c, "rc-error");
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS1002: ; expected", "requestId": "rc-error"}),
    );
    let failure = worker
        .join()
        .unwrap()
        .expect_err("a compile error restarted");
    assert_eq!(failure.error.message(), "CS1002: ; expected");
}

/// A save that left the prepared graph stands as a rejection; a recompile
/// issued then would restart the Player on the old graph without what was
/// added, and is refused before anything is sent to the runner
/// (session6-4).
#[test]
fn a_recompile_while_the_sources_have_left_the_graph_is_refused() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    const REJECTION: &str = "managed configuration/metadata is outside the prepared managed graph: Packages/com.s6.core/Runtime/Gameplay/S6Gameplay.asmdef";
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": REJECTION, "preparationRequired": true}),
    );
    wait_for("the rejection", || c.status().preparation_required);
    let failure = c
        .recompile(&seconds(1), "rc-left")
        .expect_err("a recompile restarted on a graph the sources have left");
    let message = failure.error.message();
    assert!(
        message.starts_with(&format!(
            "{REJECTION}. The change needs a new preparation of the project"
        )),
        "{message}"
    );
    assert!(
        !commands(&c).iter().any(|line| line.contains("rc-left")),
        "the command was sent"
    );
    // An update that follows ends it: the sources are back in the graph.
    emit_fixture_event(&c, updated(ready.pid));
    wait_for("the update", || !c.status().preparation_required);
    let worker = recompile_in_background(&c, "rc-back");
    emit_fixture_event(
        &c,
        json!({"event": "update-rejected", "error": "CS1002: ; expected", "requestId": "rc-back"}),
    );
    let failure = worker.join().unwrap().expect_err("a compile error");
    assert_eq!(failure.error.message(), "CS1002: ; expected");
}

/// A rejection that no update followed is the outcome for every mark: the
/// caller that has read it and passes its sequence is not held, and is not
/// sent to a recompile that cannot succeed. A compile error on top of a
/// refused edit is reported as the compile error it is.
#[test]
fn a_rejection_that_stands_answers_every_mark() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    accept(&c, outcome(&c, "refusal"));
    let error = accept(&c, outcome(&c, "compile-error"));
    for after in [ready.last_sequence, error.last_sequence, 0] {
        for readiness in [false, true] {
            let failure = c
                .wait(&seconds(1), after, readiness)
                .expect_err("a compile error that stands was passed over");
            assert!(
                failure.typed.is_none() && failure.error.message() == "CS1002: ; expected",
                "after {after}: {:?} {}",
                failure.typed,
                failure.error
            );
        }
    }
    // The sources compile again and are still what was refused: the refusal
    // is the outcome, for a mark at the newest event too.
    let stated = accept(
        &c,
        json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "fullRecompileRequired": true}),
    );
    let refused = expect_refused(
        c.wait(&seconds(1), stated.last_sequence, false),
        "the refusal that stands",
    );
    assert!(
        refused.error.message().contains("Call hot_reload"),
        "{}",
        refused.error
    );
    // A mark ahead of the session (a client that counted its own edit in)
    // is given the refusal too, by both waits, through the tool call.
    for action in ["wait_update", "wait"] {
        let ahead = c
            .handle(
                &seconds(1),
                "session_control",
                format!(
                    r#"{{"action":"{action}","afterSequence":{}}}"#,
                    stated.last_sequence + 1
                )
                .as_bytes(),
            )
            .expect_err("a mark ahead of the session passed over the refusal that stands");
        assert!(
            ahead.typed.as_ref().map(|typed| typed.code) == Some("full_recompile_required")
                && ahead.error.message().contains("Call hot_reload"),
            "{action}: {:?} {}",
            ahead.typed,
            ahead.error
        );
    }
    // A scene the Player would not load, with nothing refused: its error for
    // every mark, and it says whose error it is.
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let rejected = accept(
        &c,
        json!({"event": "update-rejected", "error": SCENE_REJECTED, "part": "scene", "fullRecompileRequired": false}),
    );
    for after in [ready.last_sequence, rejected.last_sequence] {
        let failure = c
            .wait(&seconds(1), after, false)
            .expect_err("a rejected scene was passed over");
        assert_eq!(failure.error.message(), scene_part(SCENE_REJECTED));
    }
}

/// The compiler prints every error of the sources. The state keeps the
/// beginning of a rejection and a file all of it, and an event in a reply
/// keeps less: a status after several failed compiles stays small.
#[test]
fn a_long_compile_error_is_bounded_in_every_reply() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let lines: Vec<String> = (0..600)
        .map(|index| {
            format!(
                "/p/Assets/Scripts/Enemy.cs({index},13): error CS0103: The name 'missing{index}' does not exist in the current context"
            )
        })
        .collect();
    let output = format!(
        "verify lifecycle Assembly-CSharp: dotnet: exit status 1\n{}",
        lines.join("\n")
    );
    assert!(output.len() > 60_000);
    let mut last = ready.clone();
    for attempt in 0..8 {
        last = accept(
            &c,
            json!({"event": "update-rejected", "error": output, "paths": [format!("Assets/Scripts/E{attempt}.cs")]}),
        );
    }
    let failure = c
        .wait(&seconds(1), ready.last_sequence, false)
        .expect_err("a compile error was passed over");
    let message = failure.error.message();
    // The first errors, whole lines, and where the rest is.
    let file = sysutil::join(&[
        &c.0.options.output,
        "rejections",
        &format!("{}-{}.txt", c.0.server, last.last_sequence),
    ]);
    assert!(message.len() < 4400, "{}", message.len());
    assert!(
        message.starts_with("verify lifecycle Assembly-CSharp: dotnet: exit status 1\n/p/Assets/Scripts/Enemy.cs(0,13)")
            && message.ends_with(&format!(" more bytes: the whole text is in {file}]")),
        "{message}"
    );
    let kept = message.rsplit_once("\n[").unwrap().0;
    assert!(output.starts_with(kept) && kept.ends_with("in the current context"));
    assert_eq!(fs::read_to_string(&file).unwrap(), output);
    // A status: eight events of the error, the state twice.
    let status = match c.handle(&seconds(1), "session_control", br#"{"action":"status"}"#) {
        Ok(Reply::State(state)) => *state,
        other => panic!("{other:?}"),
    };
    assert_eq!(status.events.len(), 8);
    let encoded = jsondoc::marshal(&status).unwrap();
    assert!(encoded.len() < 20_000, "{}", encoded.len());
    let listed = status.events.last().unwrap().data["error"]
        .as_str()
        .unwrap();
    assert!(
        listed.len() < 560
            && listed.ends_with(" more bytes]")
            && output.starts_with(&listed[..500]),
        "{listed}"
    );
    // The session keeps the event as the runner printed it.
    assert_eq!(c.status().events.last().unwrap().data["error"], output);
    // A refusal lists its reasons in the state, not in each event again.
    let refused = accept(&c, outcome(&c, "refusal"));
    let status = match c.handle(&seconds(1), "session_control", br#"{"action":"status"}"#) {
        Ok(Reply::State(state)) => *state,
        other => panic!("{other:?}"),
    };
    let event = &status.events.last().unwrap().data;
    assert!(
        event.get("unsupported").is_none()
            && event["unsupportedCount"] == 3
            && status.hot_reload.unsupported.len() == 2,
        "{event:?}"
    );
    assert_eq!(refused.hot_reload.unsupported_count, 3);
}

/// A patch that leaves the Player unusable makes the runner restart it. The
/// caller's wait succeeds, on the saved code, and the game state it was
/// promised to keep is gone: the reply says so first.
#[test]
fn a_restart_the_runner_made_by_itself_is_said_first() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    accept(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "hot_reload_failed", "error": "the Player did not acknowledge the code patch within 20s."}),
        ),
    );
    let waited = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
    assert_eq!(
        waited.attention,
        "The runner restarted the Player by itself and game state is reset: a code patch left the Player unusable (the Player did not acknowledge the code patch within 20s). The Player runs the saved code."
    );
    // A caller that has read the restart is told nothing by it, and a
    // restart it asked for is no news either.
    assert!(
        c.wait(&seconds(1), waited.last_sequence - 1, true)
            .unwrap()
            .attention
            .starts_with("The runner restarted")
    );
    let read = c.wait_within(&seconds(1), None, true, None).unwrap();
    assert!(
        read.attention.starts_with(NOTHING_FOLLOWED),
        "{}",
        read.attention
    );
    let worker = recompile_in_background(&c, "rc-own");
    emit_fixture_event(
        &c,
        restarted(
            &c,
            ready.pid,
            json!({"reason": "requested", "requestId": "rc-own"}),
        ),
    );
    assert!(worker.join().unwrap().unwrap().attention.is_empty());
}

/// A scene update that brings a value for a field the applied patch added
/// is confirmed by the Player and is not all in it. The runner says so on
/// the event, and the wait puts it first.
#[test]
fn a_scene_value_for_an_added_field_is_said_first() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    // The edit that adds the field: the builder's note, new with its patch.
    let added = json!([{"code": "serialized_field_not_restored", "member": "Game.Lamp::glowCut",
        "message": "The added field Game.Lamp::glowCut is not restored from scenes, prefabs or assets until a restart."}]);
    let patched = accept(
        &c,
        json!({"event": "code-patched", "pid": ready.pid, "revision": ready.revision, "codeGeneration": 1,
            "patchedMethods": 1, "addedFields": 1, "notes": added, "notesCount": 1, "newNotes": added, "newNotesCount": 1}),
    );
    assert!(
        c.wait(&seconds(1), ready.last_sequence, false)
            .unwrap()
            .attention
            .contains("The added field Game.Lamp::glowCut is not restored")
    );
    // The scene edit that sets it, waited for on its own.
    let mut event = updated(ready.pid);
    event["fullRecompileRequired"] = json!(false);
    event["newNotes"] = json!([{"code": "serialized_field_not_restored", "member": "Game.Lamp::glowCut",
        "message": "/p/Assets/Lamp.unity holds a value for the added field Game.Lamp::glowCut, which the Player does not read from scenes, prefabs or assets until a restart; its objects keep what the field's initializer gave them."}]);
    event["newNotesCount"] = json!(1);
    accept(&c, event);
    let waited = c.wait(&seconds(1), patched.last_sequence, false).unwrap();
    assert!(
        waited.attention.starts_with("The edit is applied, with a limit that lasts until the Player restarts: /p/Assets/Lamp.unity holds a value for the added field Game.Lamp::glowCut")
            && waited.attention.contains("[serialized_field_not_restored]")
            && waited.revision == "rev-2",
        "{}",
        waited.attention
    );
}

/// A start that spells out the values the Player already runs with is no
/// other launch: it returns the running session like a start without
/// arguments. Another value is refused, with the call that applies it.
#[test]
fn a_start_with_the_values_in_use_returns_the_running_session() {
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let ctx = seconds(1);
    for arguments in [
        json!({"hotReload": true}),
        json!({"onUnsupportedEdit": "report"}),
        json!({"hotReload": true, "onUnsupportedEdit": "report", "addressables": true}),
        json!({"hotReload": null}),
        json!({}),
    ] {
        let start = json!({"action": "start", "arguments": arguments});
        match c.handle(&ctx, "session_control", &jsondoc::encode(&start)) {
            Ok(Reply::State(state)) => assert_eq!(
                (state.pid, state.session_id.as_str()),
                (ready.pid, ready.session_id.as_str()),
                "{start}"
            ),
            other => panic!("{start}: {other:?}"),
        }
    }
    for arguments in [
        json!({"hotReload": false}),
        json!({"onUnsupportedEdit": "restart"}),
        json!({"addressables": false}),
        json!({"gameArguments": ["--level", "2"]}),
    ] {
        let start = json!({"action": "start", "arguments": arguments});
        let refused = c
            .handle(&ctx, "session_control", &jsondoc::encode(&start))
            .expect_err("another launch was taken for the running one");
        assert!(
            refused.error.message().contains("other launch arguments"),
            "{start}: {}",
            refused.error
        );
    }
    assert_eq!(c.status().pid, ready.pid);
}

// ---------------------------------------------------------------------
// Stage 5: what a wait tells a caller whose mark another caller moved or
// that comes from another server, and the shape of a reply.
// ---------------------------------------------------------------------
mod stage5_waits {
    use super::*;

    fn reply(reply: &std::result::Result<Reply, Failure>) -> Value {
        match reply {
            Ok(reply) => serde_json::to_value(reply).unwrap(),
            Err(failure) => {
                json!({"error": failure.error.message(), "typed": failure.typed.as_ref().map(|t| t.code)})
            }
        }
    }
    fn coroutine_note() -> Value {
        json!([{"code": "running_coroutines_keep_old_code", "member": "Game.A::Loop()",
            "message": "Coroutines of Game.A::Loop() that are already running finish on the old code."}])
    }
    fn patched(c: &Controller, generation: u64) -> State {
        let state = c.status();
        let notes = coroutine_note();
        accept(
            c,
            json!({"event": "code-patched", "pid": state.pid, "revision": state.revision, "codeGeneration": generation,
                "patchedMethods": 1, "paths": ["/p/Assets/A.cs"], "notes": notes, "notesCount": 1, "newNotes": notes, "newNotesCount": 1}),
        )
    }
    fn wait_update(c: &Controller, arguments: &str) -> Value {
        reply(&c.handle(&seconds(1), "session_control", arguments.as_bytes()))
    }

    /// Two callers of one server wait without afterSequence: the one whose
    /// edit it was may be the second, and its reply then names what stands,
    /// the limit of its edit included.
    #[test]
    fn a_caller_whose_mark_another_moved_is_told_what_stands() {
        let c = controller_fixture("");
        start_ready(&c);
        c.wait_within(&seconds(1), None, true, None).unwrap();
        let patch = patched(&c, 1);
        let other = wait_update(&c, r#"{"action":"wait_update"}"#);
        assert!(
            other["attention"]
                .as_str()
                .unwrap()
                .contains("[running_coroutines_keep_old_code]")
        );
        let own = wait_update(&c, r#"{"action":"wait_update"}"#);
        let attention = own["attention"].as_str().unwrap();
        assert!(attention.starts_with(NOTHING_FOLLOWED), "{attention}");
        assert!(
            attention.contains(&format!(
                "What stands: the Player runs code generation 1, patched in at sequence {}; this limit of it lasts until the Player restarts: Coroutines of Game.A::Loop() that are already running finish on the old code [running_coroutines_keep_old_code].",
                patch.last_sequence
            )) && attention.contains("callers that share this server should pass afterSequence"),
            "{attention}"
        );
    }

    /// The same for a restart the runner made by itself.
    #[test]
    fn a_caller_whose_mark_another_moved_is_told_of_the_restart() {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        c.wait_within(&seconds(1), None, true, None).unwrap();
        let restart = accept(
            &c,
            restarted(
                &c,
                ready.pid,
                json!({"reason": "hot_reload_unsupported", "unsupported": [{"code": "type_added", "member": "Game.Boss", "message": "A type was added."}], "unsupportedCount": 1}),
            ),
        );
        let other = wait_update(&c, r#"{"action":"wait_update"}"#);
        assert!(
            other["attention"]
                .as_str()
                .unwrap()
                .contains("game state is reset")
        );
        let own = wait_update(&c, r#"{"action":"wait_update"}"#);
        let attention = own["attention"].as_str().unwrap();
        assert!(
            attention.contains(&format!(
                "What stands: the runner restarted the Player by itself at sequence {} (hot_reload_unsupported), and game state was reset.",
                restart.last_sequence
            )),
            "{attention}"
        );
    }

    /// Two waits in flight at once read the mark once each: a reply never
    /// lists events after its mark and says that nothing followed it.
    #[test]
    fn a_reply_never_lists_events_and_says_nothing_followed() {
        for _ in 0..12 {
            let c = controller_fixture("");
            let ready = start_ready(&c);
            c.wait_within(&seconds(1), None, true, None).unwrap();
            let notes = coroutine_note();
            accept(
                &c,
                json!({"event": "code-patched", "pid": ready.pid, "revision": ready.revision, "codeGeneration": 1,
                    "patchedMethods": 1, "notes": notes, "notesCount": 1, "newNotes": notes, "newNotesCount": 1, "pending": true}),
            );
            let waiters: Vec<_> = (0..2)
                .map(|_| {
                    let c = c.clone();
                    thread::spawn(move || wait_update(&c, r#"{"action":"wait_update"}"#))
                })
                .collect();
            thread::sleep(Duration::from_millis(100));
            accept(
                &c,
                json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "fullRecompileRequired": false}),
            );
            for answered in waiters.into_iter().map(|w| w.join().unwrap()) {
                let nothing = answered["attention"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with(NOTHING_FOLLOWED);
                let listed = answered["events"].as_array().map_or(0, Vec::len);
                assert!(!(nothing && listed > 0), "{answered}");
            }
        }
    }

    /// A mark above lastSequence comes from an earlier server or another
    /// project: the reply covers every event of this server, notes and
    /// restarts included, and says why.
    #[test]
    fn a_mark_of_another_server_reports_all_of_this_one() {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        let patch = patched(&c, 1);
        let mark = 40;
        assert!(mark > patch.last_sequence);
        let answered = wait_update(
            &c,
            &format!(r#"{{"action":"wait_update","afterSequence":{mark}}}"#),
        );
        let attention = answered["attention"].as_str().unwrap();
        assert!(
            attention.starts_with(&format!(
                "afterSequence {mark} is above lastSequence {} of this server",
                patch.last_sequence
            )) && attention.contains("[running_coroutines_keep_old_code]")
                && !attention.contains("counts its events anew"),
            "{attention}"
        );
        assert_eq!(
            answered["hotReload"]["newNotes"][0]["code"],
            "running_coroutines_keep_old_code"
        );
        assert!(!answered["events"].as_array().unwrap().is_empty());
        accept(
            &c,
            restarted(
                &c,
                ready.pid,
                json!({"reason": "hot_reload_failed", "error": "the Player did not acknowledge the code patch within 20s."}),
            ),
        );
        let answered = wait_update(
            &c,
            &format!(r#"{{"action":"wait_update","afterSequence":{mark}}}"#),
        );
        assert!(
            answered["attention"]
                .as_str()
                .unwrap()
                .contains("game state is reset"),
            "{answered}"
        );
    }

    /// A held limit leads a value that was reset: the sentence offers the
    /// recompile and does not say that all of the code runs.
    #[test]
    fn a_held_note_leads_a_told_one() {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        let notes = json!([
            {"code": "field_value_reset", "member": "Game.Hud::score", "message": "Field Game.Hud::score changed its type; its previous value is lost."},
            {"code": "added_method_delegate_identity", "member": "Game.Hud::OnHit(int)", "message": "Patched code makes a delegate of the added method Game.Hud::OnHit(int), which patch 1 also had; `-=` does not remove a handler subscribed while patch 1 was applied."}
        ]);
        accept(
            &c,
            json!({"event": "code-patched", "pid": ready.pid, "revision": ready.revision, "codeGeneration": 2,
            "patchedMethods": 1, "notes": notes, "notesCount": 2, "newNotes": notes, "newNotesCount": 2}),
        );
        let waited = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
        assert!(
            waited.attention.starts_with("The edit is applied, with a limit that lasts until the Player restarts: Patched code makes a delegate"),
            "{}",
            waited.attention
        );
    }

    /// A restart for a Netcode reason, and a refusal for one, say that the
    /// other Players of the network game must take the new code as well.
    #[test]
    fn a_netcode_restart_and_refusal_name_the_peers() {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        accept(
            &c,
            restarted(
                &c,
                ready.pid,
                json!({"reason": "hot_reload_unsupported", "unsupported": [{"code": "netcode_variable_changed", "member": "NetBall::Level", "message": "The network variable NetBall::Level was added."}], "unsupportedCount": 1}),
            ),
        );
        let waited = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
        assert!(
            waited
                .attention
                .contains("must be restarted on the new code too"),
            "{}",
            waited.attention
        );
        let mark = c.status().last_sequence;
        let mut event = refusal("/owned/report.json");
        event["unsupported"] = json!([{"code": "netcode_rpc_changed", "member": "NetBall::PingRpc(System.Int32)", "message": "The RPC was added."}]);
        event["unsupportedCount"] = json!(1);
        accept(&c, event);
        let failure = c
            .wait(&seconds(1), mark, false)
            .expect_err("the refusal stands");
        assert!(
            failure
                .error
                .message()
                .contains("must be restarted on the new code too"),
            "{}",
            failure.error.message()
        );
        // Not for other reasons.
        accept(&c, refusal("/owned/report.json"));
        let failure = c
            .wait(&seconds(1), mark, false)
            .expect_err("the refusal stands");
        assert!(
            !failure.error.message().contains("network"),
            "{}",
            failure.error.message()
        );
    }

    /// A window with thousands of paths: a listed event keeps the first few
    /// and counts the rest.
    #[test]
    fn listed_events_keep_a_few_paths() {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        let paths: Vec<String> = (0..2000)
            .map(|index| format!("/home/user/projects/MyGame/Assets/Art/Environment/Props/Rocks/rock_{index:04}.png"))
            .collect();
        accept(
            &c,
            json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "paths": paths, "fullRecompileRequired": false, "pending": true}),
        );
        accept(
            &c,
            json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "paths": paths, "fullRecompileRequired": false}),
        );
        let status = jsondoc::marshal(
            &c.handle(&seconds(1), "session_control", br#"{"action":"status"}"#)
                .unwrap(),
        )
        .unwrap();
        let waited = c
            .handle(
                &seconds(1),
                "session_control",
                format!(
                    r#"{{"action":"wait_update","afterSequence":{}}}"#,
                    ready.last_sequence
                )
                .as_bytes(),
            )
            .unwrap();
        let waited = serde_json::to_value(&waited).unwrap();
        assert!(status.len() < 30_000, "{}", status.len());
        let event = waited["events"].as_array().unwrap().last().unwrap().clone();
        let data = if event.get("data").is_some() {
            event["data"].clone()
        } else {
            event.clone()
        };
        assert_eq!(data["pathsCount"], 2000, "{event}");
        assert_eq!(data["paths"].as_array().unwrap().len(), 8, "{event}");
    }

    /// The file a reply named for a long rejection keeps its text when the
    /// next server process on the same output numbers its events anew.
    #[test]
    fn a_rejection_file_outlives_the_server_that_named_it() {
        let c = controller_fixture("");
        start_ready(&c);
        let long = |tag: &str| format!("{tag}\n{}", "x".repeat(6000));
        let first = accept(
            &c,
            json!({"event": "update-rejected", "error": long("FIRST SERVER ERROR")}),
        );
        let named = first
            .update_error
            .rsplit_once("whole text is in ")
            .unwrap()
            .1
            .trim_end_matches(']')
            .to_string();
        let (output, project, runner) = (
            c.0.options.output.clone(),
            c.0.options.project.clone(),
            c.0.options.runner.clone(),
        );
        c.close().unwrap();
        let _ = fs::remove_file(sysutil::join(&[&output, "helper-event.json"]));
        let second = Controller::new(
            &Context::background(),
            Options {
                project,
                runner,
                output: output.clone(),
                ..Options::default()
            },
        )
        .unwrap();
        shorten_grace(&second);
        start_ready(&second);
        let state = accept(
            &second,
            json!({"event": "update-rejected", "error": long("SECOND SERVER ERROR")}),
        );
        assert_eq!(state.last_sequence, first.last_sequence);
        assert!(
            fs::read_to_string(&named)
                .unwrap()
                .starts_with("FIRST SERVER ERROR\n")
        );
        let files: Vec<_> = fs::read_dir(sysutil::join(&[&output, "rejections"]))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 2, "{files:?}");
        second.close().unwrap();
    }

    /// Only the newest files are kept.
    #[test]
    fn old_rejection_files_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_str().unwrap();
        for index in 0..5 {
            fs::write(sysutil::join(&[path, &format!("s-{index}.txt")]), "x").unwrap();
            thread::sleep(Duration::from_millis(15));
        }
        prune_rejections(path, 3);
        let mut left: Vec<String> = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["s-2.txt", "s-3.txt", "s-4.txt"]);
    }

    /// A misspelled launch argument is refused; one a preparation reads is
    /// left to the project service.
    #[test]
    fn an_unknown_launch_argument_is_refused() {
        let c = controller_fixture("");
        let ready = start_ready(&c);
        for arguments in [
            r#"{"hotreload":false}"#,
            r#"{"onUnsupportedEdits":"restart"}"#,
        ] {
            let answered = wait_update(
                &c,
                &format!(r#"{{"action":"start","arguments":{arguments}}}"#),
            );
            assert!(
                answered["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("unknown start/reset argument"),
                "{answered}"
            );
        }
        let answered = wait_update(
            &c,
            r#"{"action":"start","arguments":{"rebuildHost":true,"scene":"Assets/Main.unity"}}"#,
        );
        assert_eq!(answered["pid"].as_i64(), Some(ready.pid), "{answered}");
    }
}

/// A UnityEvent saved after a patch that names a method the patch added
/// calls nothing; the runner says so on the scene update, and the wait
/// after the save leads with it although the patch listed the method under
/// another code (stage 6, session6-1 and mcp6-1). The note stands as long
/// as the patch does, also for a caller whose mark nothing followed.
#[test]
fn a_scene_naming_an_added_method_is_said_by_the_wait_after_the_save() {
    use loop_lines::*;
    let (_guard, root) = temp_dir();
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    // The patch adds Game.Lamp::OnPinged() to a MonoBehaviour; no string of
    // the code names it (the builder's Minor note).
    script.edit(patch(
        1,
        vec![Why::new(
            "added_method_invisible_by_name",
            "Game.Lamp::OnPinged()",
            "The added method Game.Lamp::OnPinged() belongs to an engine object.",
        )],
    ));
    let patched = run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/Lamp.cs"], None),
    );
    let first = c.wait(&seconds(1), ready.last_sequence, false).unwrap();
    assert_eq!(first.attention, "", "{first:?}");
    // A scene save whose button calls it by name.
    let scene = format!("{root}/Menu.unity");
    fs::write(
        &scene,
        "%YAML 1.1\n--- !u!114 &9\nMonoBehaviour:\n  m_OnClick:\n    m_PersistentCalls:\n      m_Calls:\n      - m_Target: {fileID: 7}\n        m_MethodName: OnPinged\n",
    )
    .unwrap();
    script.reloads.push_back(Reload::Updated);
    script.compiles.push_back(Compiled::Unaffected);
    let saved = run(&c, &mut state, &mut script, &window(&[&scene], None));
    let waited = c.wait(&seconds(1), patched.last_sequence, false).unwrap();
    assert!(
        waited
            .attention
            .contains("[added_method_named_by_string]"),
        "{:?} {:?}",
        waited.attention,
        waited.hot_reload.new_notes
    );
    assert_eq!(
        waited
            .hot_reload
            .new_notes
            .iter()
            .map(|note| (note.code.as_str(), note.member.as_str()))
            .collect::<Vec<_>>(),
        [("added_method_named_by_string", "Game.Lamp::OnPinged()")]
    );
    // A caller whose mark nothing followed is told what stands.
    let standing = c.wait(&seconds(1), saved.last_sequence, false).unwrap();
    assert!(
        standing
            .attention
            .contains("[added_method_named_by_string]"),
        "{}",
        standing.attention
    );
    // Another body edit keeps the patch and the method: the note stands.
    script.edit(patch(
        2,
        vec![Why::new(
            "added_method_invisible_by_name",
            "Game.Lamp::OnPinged()",
            "The added method Game.Lamp::OnPinged() belongs to an engine object.",
        )],
    ));
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/Lamp.cs"], None),
    );
    let again = c.wait(&seconds(1), patched.last_sequence, false).unwrap();
    assert!(
        again
            .hot_reload
            .new_notes
            .iter()
            .any(|note| note.code == "added_method_named_by_string"),
        "{:?}",
        again.hot_reload.new_notes
    );
    // A patch without the method ends it.
    script.edit(patch(3, Vec::new()));
    run(
        &c,
        &mut state,
        &mut script,
        &window(&["/p/Assets/Lamp.cs"], None),
    );
    let ended = c.wait(&seconds(1), patched.last_sequence, false).unwrap();
    assert!(
        ended.hot_reload.new_notes.is_empty(),
        "{:?}",
        ended.hot_reload.new_notes
    );
}

/// A recompile that puts into effect what the Player did not run (a refused
/// edit, or a limit that lasts until a restart) says so, and says in
/// general words that other processes of the same game that took the same
/// edits live need the same restart (netcode6-4). A recompile of a Player
/// that ran all of the saved code says nothing of the kind.
#[test]
fn a_recompile_says_what_it_put_into_effect_for_other_processes_of_the_game() {
    use loop_lines::*;
    let c = controller_fixture("");
    let ready = start_ready(&c);
    let (mut state, mut script) = (report_loop(), Script::new(&c, &ready));
    const OTHERS: &str = "Other processes of the same game that took the same edits without a restart (the other Players of a network game, for instance) still run without it until they are restarted on the saved code too.";
    let recompile = |state: &mut crate::runner::testing::Loop, script: &mut Script, id: &str| {
        let worker = recompile_in_background(&c, id);
        script.compiles.push_back(Compiled::Unchanged);
        run(&c, state, script, &window(&[], Some(id)));
        worker.join().unwrap().expect("the recompile")
    };
    // A refused edit.
    script.edit(refused());
    run(&c, &mut state, &mut script, &window(&["/p/Assets/A.cs"], None));
    let answered = recompile(&mut state, &mut script, "rc-refused");
    assert_eq!(
        answered.attention,
        format!(
            "The Player was restarted on the saved code, which puts into effect what it did not run before: Game.Boss: A type was added [type_added]. {OTHERS}"
        )
    );
    // A once-run note of a patch.
    script.edit(patch(
        1,
        vec![Why::new(
            "runtime_initializer_changed",
            "Game.Codec::Register()",
            "Game.Codec::Register() ran when the Player started and does not run again.",
        )],
    ));
    run(&c, &mut state, &mut script, &window(&["/p/Assets/A.cs"], None));
    let answered = recompile(&mut state, &mut script, "rc-noted");
    assert!(
        answered.attention.starts_with("The Player was restarted on the saved code, which puts into effect what it did not run before: Game.Codec::Register(): Game.Codec::Register() ran when the Player started and does not run again [runtime_initializer_changed].")
            && answered.attention.ends_with(OTHERS),
        "{}",
        answered.attention
    );
    // A Player that ran all of it: a plain restart.
    let answered = recompile(&mut state, &mut script, "rc-plain");
    assert_eq!(answered.attention, "");
    // A lookup by name alone is no limit worth a sentence.
    script.edit(patch(
        2,
        vec![note("added_method_invisible_by_name", "Game.A::OnJump()")],
    ));
    run(&c, &mut state, &mut script, &window(&["/p/Assets/A.cs"], None));
    let answered = recompile(&mut state, &mut script, "rc-minor");
    assert_eq!(answered.attention, "");
}
