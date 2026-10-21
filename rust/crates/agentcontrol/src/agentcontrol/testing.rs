//! The fixtures of the controller's tests: a helper process that speaks the
//! runner's protocol without Unity, and controllers wired to it. The tests of
//! other crates that drive a controller use them through the `testing` feature.
use super::*;
use crate::sysutil::testing::{
    helper_arguments, notify_signals, shell_quote, signalled, temp_dir, write_executable,
};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    os::fd::FromRawFd,
    process,
    sync::atomic::AtomicU64,
    time::Instant,
};
use tempfile::TempDir;
pub const HELPER: &str = "agentcontrol::tests::controller_helper_process";

pub fn helper_write(path: &str, value: &Value) -> io::Result<()> {
    fs::create_dir_all(sysutil::dir(path))?;
    let temporary = format!("{path}.temporary");
    fs::write(&temporary, jsondoc::encode(value))?;
    fs::rename(temporary, path)
}
// Bounds for outcomes that are expected to arrive: the nominal seconds, stretched so a
// loaded machine does not turn a slow step into a failure.
pub const PATIENCE: u64 = 10;
pub fn seconds(count: u64) -> Context {
    Context::background().with_timeout(Duration::from_secs(count * PATIENCE))
}
pub fn millis(count: u64) -> Context {
    Context::background().with_timeout(Duration::from_millis(count))
}

// This subprocess implements the runner/file protocol without Unity. Each
// fixture has its own project, output, process group, and request artifacts.
// The wrapper script passes the controller's stdout pipe as descriptor 3 and
// discards the test harness's own stdout.
/// The body of the helper process the fixtures start: see `HELPER`.
pub fn helper_process() {
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
    // complete line is recorded as it was written. Three modes are a runner
    // whose command reader is gone: it closed the input (and, in one of them,
    // outlives the signals of a stop as well), or it leaves it unread.
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
        "commands-closed" | "commands-closed-ignore-signals" => {
            drop(unsafe { File::from_raw_fd(0) })
        }
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
        if signalled() && !mode.ends_with("ignore-signals") {
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
pub mod watched_session {
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

pub struct Fixture {
    pub controller: Controller,
    _project: TempDir,
}
impl Fixture {
    /// The fixture's project directory.
    pub fn project(&self) -> String {
        self.controller.0.options.project.clone()
    }
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
pub fn shorten_grace(controller: &Controller) {
    let mut inner = controller.lock();
    inner.stop_grace = Duration::from_millis(40);
    inner.terminate_grace = Duration::from_millis(40);
}
pub fn controller_fixture(mode: &str) -> Fixture {
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
pub fn start_ready(c: &Controller) -> State {
    c.start().unwrap();
    match c.wait(&seconds(3), 0, true) {
        Ok(state) => state,
        Err(failure) => panic!("ready: {:?} {}", failure.state, failure.error),
    }
}
// A request whose short wait is meant to end first. On a loaded machine the wait
// can end before the command is even delivered; repeating the same immutable
// request then delivers it.
pub fn request_briefly(c: &Controller, wait: u64, args: &BTreeMap<String, Vec<u8>>) -> Value {
    for _ in 0..500 {
        match c.request(&millis(wait), "custom", args) {
            Ok(result) => return result,
            Err(error) if error.is_deadline_exceeded() => {}
            Err(error) => panic!("{error}"),
        }
    }
    panic!("request was never delivered");
}
pub fn request_args(value: Value) -> BTreeMap<String, Vec<u8>> {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, item)| (key.clone(), jsondoc::encode(item)))
        .collect()
}
pub fn emit_fixture_event(c: &Controller, event: Value) {
    helper_write(
        &sysutil::join(&[&c.0.options.output, "helper-event.json"]),
        &event,
    )
    .unwrap();
}

pub fn refusal(report: &str) -> Value {
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

// The command lines the fixture runner has read from its standard input.
pub fn commands(c: &Controller) -> Vec<String> {
    fs::read_to_string(sysutil::join(&[
        &c.0.options.output,
        "helper-commands.jsonl",
    ]))
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect()
}

pub fn wait_for(what: &str, reached: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(PATIENCE);
    while !reached() {
        assert!(Instant::now() < deadline, "{what} did not happen");
        thread::sleep(Duration::from_millis(5));
    }
}

// The `code-updated` of a restart, naming the fixture's own readiness report.
pub fn restarted(c: &Controller, pid: i64, mut event: Value) -> Value {
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

// Prints one event and returns the state once the controller has taken it.
// The fixture runner prints an event file only when it changed, so a serial
// number keeps two equal events apart.
pub fn accept(c: &Controller, mut event: Value) -> State {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    event["serial"] = json!(SERIAL.fetch_add(1, Ordering::SeqCst));
    let before = c.status().last_sequence;
    emit_fixture_event(c, event);
    wait_for("the event", || c.status().last_sequence > before);
    c.status()
}
