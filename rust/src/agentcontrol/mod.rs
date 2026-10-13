//! Owns a runner process and validates immutable Player commands.
//!
//! Every context parameter is a [`Context`]; the controller lifetime is derived from the
//! context given to [`Controller::new`], and cancelling it stops the owned runner.
use crate::{
    cancel::{Context, Flag, Source, select},
    jsondoc::{self, Json, Member, Target},
    sysutil::{self, Root, new_id, valid_id},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::File,
    io::{self, PipeReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileExt, PermissionsExt},
            process::CommandExt,
        },
    },
    process::{ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

mod hotreload;
mod journal;

pub use crate::sysutil::{Error, ErrorKind, Result};
use hotreload::Refused;
pub use hotreload::{HotReload, LastPatch, LastRestart, Reason};

const STDERR_LOG: &str = "agent-runner-stderr.log";
const RUNNER_STDERR_MAXIMUM: usize = 8 * 1024;
const EVENT_LINE_MAXIMUM: usize = 4 * 1024 * 1024;
const CONTROL_ARTIFACT_MAXIMUM: i64 = 4 * 1024 * 1024;

/// An empty `output` selects `<project>/Library/UnityMicroEditor/agent` and
/// `fps` 0 selects 30.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub project: String,
    pub baseline: String,
    pub output: String,
    pub editor: String,
    pub runner: String,
    pub scene: String,
    pub fps: i64,
}

fn zero(value: &i64) -> bool {
    *value == 0
}
fn no_ready(value: &Option<Map<String, Value>>) -> bool {
    value.as_ref().is_none_or(Map::is_empty)
}
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub phase: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(skip_serializing_if = "zero")]
    pub pid: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub revision: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub control_directory: String,
    pub last_sequence: u64,
    pub last_ready_sequence: u64,
    pub last_update_sequence: u64,
    pub last_update_rejected_sequence: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub update_error: String,
    /// The runner's last answer was not its last word: it is still working
    /// on source changes that were saved before it. No wait is answered
    /// until it has said how they ended.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub update_pending: bool,
    /// The rejected update cannot be applied to this Player at all: the
    /// project must be prepared again before its sources take effect.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub preparation_required: bool,
    /// What hot reload did to the code of this Player; absent at its default.
    #[serde(skip_serializing_if = "HotReload::is_default")]
    pub hot_reload: HotReload,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub runner_stderr_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub runner_stderr_tail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Event>,
    /// The Player readiness report; numbers carry the shortest round-trip f64 formatting.
    #[serde(skip_serializing_if = "no_ready")]
    pub ready: Option<Map<String, Value>>,
    /// What `session_control(start)` asked this launch to pass on.
    #[serde(skip_serializing_if = "Launch::is_empty")]
    pub launch: Launch,
}
/// Per-launch additions to the runner command: the game's own arguments
/// (the CLI's `-- args`), the additive runtime content it may address and how
/// C# edits reach the running Player.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub game_arguments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content_scenes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content_prefabs: Vec<String>,
    /// `false` leaves the project's Addressables keys to the host's built content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addressables: Option<bool>,
    /// `false` restarts the Player on every C# edit instead of patching it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hot_reload: Option<bool>,
    /// `restart` lets the runner restart the Player by itself for an edit that
    /// cannot be applied live; the default `report` waits for a recompile.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub on_unsupported_edit: String,
}
impl Launch {
    fn is_empty(&self) -> bool {
        self.game_arguments.is_empty()
            && self.content_scenes.is_empty()
            && self.content_prefabs.is_empty()
            && self.addressables.is_none()
            && self.hot_reload.is_none()
            && self.on_unsupported_edit.is_empty()
    }
    /// The runner's `--hot-reload-fallback` value.
    fn fallback(&self) -> &'static str {
        match self.on_unsupported_edit.as_str() {
            "restart" => "restart",
            _ => "report",
        }
    }
    /// Reads `gameArguments`, `contentScenes`, `contentPrefabs`, `addressables`,
    /// `hotReload` and `onUnsupportedEdit` from the `arguments` of a
    /// start/reset. A value that is given replaces the previous one; `None`
    /// when the request names none of them.
    fn from_arguments(previous: &Launch, raw: Option<&Vec<u8>>) -> Result<Option<Launch>> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        let members = match jsondoc::parse(raw).map_err(Error::new)? {
            Json::Null => return Ok(None),
            Json::Object(members) => members,
            other => return Err(Error::new(Target::VALUE.mismatch(other.kind(), "object"))),
        };
        let mut launch = previous.clone();
        let mut given = false;
        for member in &members {
            let switch = match member.key.as_str() {
                "addressables" => Some(&mut launch.addressables),
                "hotReload" => Some(&mut launch.hot_reload),
                _ => None,
            };
            if let Some(switch) = switch {
                *switch = match &member.value {
                    Json::Null => None,
                    Json::Bool(enabled) => Some(*enabled),
                    _ => return Err(Error::new(format!("{} must be a boolean", member.key))),
                };
                given = true;
                continue;
            }
            if member.key == "onUnsupportedEdit" {
                launch.on_unsupported_edit = match &member.value {
                    Json::Null => String::new(),
                    Json::String(policy) if policy == "report" || policy == "restart" => {
                        policy.clone()
                    }
                    _ => {
                        return Err(Error::new(
                            "onUnsupportedEdit must be \"report\" or \"restart\"",
                        ));
                    }
                };
                given = true;
                continue;
            }
            let target = match member.key.as_str() {
                "gameArguments" => &mut launch.game_arguments,
                "contentScenes" => &mut launch.content_scenes,
                "contentPrefabs" => &mut launch.content_prefabs,
                _ => continue,
            };
            let invalid = || Error::new(format!("{} must be an array of strings", member.key));
            let items = match &member.value {
                Json::Null => &[][..],
                Json::Array(items) => &items[..],
                _ => return Err(invalid()),
            };
            *target = items
                .iter()
                .map(|item| item.as_str().map(str::to_string).ok_or_else(invalid))
                .collect::<Result<_>>()?;
            given = true;
        }
        if !given {
            return Ok(None);
        }
        let total: usize = [
            &launch.game_arguments,
            &launch.content_scenes,
            &launch.content_prefabs,
        ]
        .iter()
        .flat_map(|list| list.iter())
        .map(|value| value.len() + 1)
        .sum();
        if total > 32 * 1024 {
            return Err(Error::new("launch arguments exceed 32 KiB"));
        }
        for value in &launch.game_arguments {
            if value.starts_with("--unity-cli-") || value.contains('\0') {
                return Err(Error::new(
                    "gameArguments must not override the runner's --unity-cli-* protocol or contain NUL",
                ));
            }
        }
        for value in launch.content_scenes.iter().chain(&launch.content_prefabs) {
            if value.trim().is_empty() || value.starts_with('-') || value.contains('\0') {
                return Err(Error::new(
                    "contentScenes are GUID=Assets/...unity and contentPrefabs are Assets/...prefab",
                ));
            }
        }
        Ok(Some(launch))
    }
}
/// Runner stdout event; numbers keep their original literal text.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Event {
    pub sequence: u64,
    pub data: Map<String, Value>,
}

/// The code and details of a failure that callers act on by code. The MCP
/// layer reports it as a typed error instead of `operation_failed`.
#[derive(Debug, Clone, PartialEq)]
pub struct Typed {
    pub code: &'static str,
    pub details: Value,
}

/// A session state returned together with the error that ended the call (`stop`
/// after a caller timeout, `wait` on failure/rejection).
#[derive(Debug, Clone)]
pub struct StateError {
    pub state: Box<State>,
    pub error: Error,
    pub typed: Option<Typed>,
}
impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for StateError {}

/// Successful result of [`Controller::handle`]: a session state or the JSON
/// object of a runtime request. Serializes as the value itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Reply {
    State(Box<State>),
    Value(Value),
}
/// Error of [`Controller::handle`]. `reply` is the result returned
/// next to the error (the session state for stop/wait, a zero state for a
/// failed start); callers inspect it for `wait_update` rejections.
#[derive(Debug, Clone)]
pub struct Failure {
    pub error: Error,
    pub reply: Option<Reply>,
    pub typed: Option<Typed>,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for Failure {}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            error,
            reply: None,
            typed: None,
        }
    }
}
impl From<StateError> for Failure {
    fn from(failure: StateError) -> Self {
        Self {
            error: failure.error,
            reply: Some(Reply::State(failure.state)),
            typed: failure.typed,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RequestRecord {
    id: String,
    operation: String,
    session_id: String,
    directory: String,
    revision: String,
    hash: String,
    pid: i64,
    arguments: Vec<u8>,
    envelope: Vec<u8>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProcessIdentity {
    pid: i64,
    started: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Runner {
    id: u64,
    pid: i64,
}
struct Inner {
    baseline: String,
    scene: String,
    state: State,
    command: Option<Runner>,
    done: Option<Flag>,
    changed: Flag,
    requests: HashMap<String, Arc<RequestRecord>>,
    stop_requested: bool,
    player: ProcessIdentity,
    event_sizes: VecDeque<usize>,
    event_bytes: usize,
    stop_grace: Duration,
    terminate_grace: Duration,
    last_ready_baseline: String,
    last_ready_scene: String,
    preparation_update_pending: bool,
    launched: u64,
    launch: Launch,
    /// The runner's standard input, which carries its commands.
    commands: Option<ChildStdin>,
    /// The `recompile` commands written to this runner, oldest first.
    recompiles: VecDeque<hotreload::Recompile>,
}
impl Inner {
    fn notify(&mut self) {
        std::mem::take(&mut self.changed).set();
    }
}
struct Shared {
    options: Options,
    lifetime: Context,
    inner: Mutex<Inner>,
}
/// Cheap to clone; all clones share one runner session and request journal.
#[derive(Clone)]
pub struct Controller(Arc<Shared>);

// The identity a pending request is bound to.
#[derive(PartialEq, Eq)]
struct Binding {
    session_id: String,
    pid: i64,
    directory: String,
    ready: bool,
}

impl Controller {
    pub fn new(ctx: &Context, mut options: Options) -> Result<Controller> {
        if !sysutil::is_abs(&options.project) || !sysutil::is_abs(&options.runner) {
            return Err(Error::new(
                "runtime controller requires explicit absolute project and runner paths",
            ));
        }
        options.project = sysutil::eval_symlinks(&options.project)?;
        if !sysutil::stat(&options.project).is_ok_and(|info| info.is_dir()) {
            return Err(Error::new("runtime project must be an existing directory"));
        }
        options.runner = sysutil::eval_symlinks(&options.runner)?;
        if !sysutil::stat(&options.runner)
            .is_ok_and(|info| info.is_regular() && info.mode & 0o111 != 0)
        {
            return Err(Error::new(
                "runtime runner must be an executable regular file",
            ));
        }
        if options.output.is_empty() {
            options.output =
                sysutil::join(&[&options.project, "Library", "UnityMicroEditor", "agent"]);
        }
        if !sysutil::is_owned_cache_path(&options.project, &options.output) {
            return Err(Error::new(
                "runtime output must stay inside the explicit project's Temp or Library/UnityMicroEditor cache",
            ));
        }
        canonical_destination(&options.output)?;
        if options.fps == 0 {
            options.fps = 30;
        }
        if !(1..=240).contains(&options.fps) {
            return Err(Error::new("runtime FPS must be between 1 and 240"));
        }
        let lifetime = ctx.with_cancel();
        let requests = match journal::load(&options.output) {
            Ok(requests) => requests,
            Err(error) => {
                lifetime.cancel();
                return Err(error);
            }
        };
        let inner = Inner {
            baseline: options.baseline.clone(),
            scene: options.scene.clone(),
            state: State {
                phase: "stopped".into(),
                ..State::default()
            },
            command: None,
            done: None,
            changed: Flag::new(),
            requests,
            stop_requested: false,
            player: ProcessIdentity::default(),
            event_sizes: VecDeque::new(),
            event_bytes: 0,
            stop_grace: Duration::from_secs(12),
            terminate_grace: Duration::from_secs(5),
            last_ready_baseline: String::new(),
            last_ready_scene: String::new(),
            preparation_update_pending: false,
            launched: 0,
            launch: Launch::default(),
            commands: None,
            recompiles: VecDeque::new(),
        };
        let controller = Controller(Arc::new(Shared {
            options,
            lifetime: lifetime.clone(),
            inner: Mutex::new(inner),
        }));
        let watcher = controller.clone();
        thread::spawn(move || {
            lifetime.wait();
            let stop = Context::background().with_timeout(Duration::from_secs(20));
            let _ = watcher.stop(&stop);
        });
        Ok(controller)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.inner.lock().unwrap()
    }
    /// Options after validation, with the currently configured baseline/scene.
    pub fn options(&self) -> Options {
        let inner = self.lock();
        Options {
            baseline: inner.baseline.clone(),
            scene: inner.scene.clone(),
            ..self.0.options.clone()
        }
    }

    pub fn status(&self) -> State {
        self.lock().state.clone()
    }
    fn binding(&self) -> Binding {
        let inner = self.lock();
        Binding {
            session_id: inner.state.session_id.clone(),
            pid: inner.state.pid,
            directory: inner.state.control_directory.clone(),
            ready: inner.state.phase == "ready",
        }
    }

    /// Changes the next launch only while no runner is active.
    /// A preparation failure leaves the current running Player and baseline intact.
    pub fn configure_prepared(&self, baseline: &str, scene: &str) -> Result<()> {
        let mut inner = self.lock();
        if inner.command.is_some() {
            return Err(Error::new(
                "stop the current runtime before activating a new preparation",
            ));
        }
        if !sysutil::is_abs(baseline)
            || !sysutil::is_owned_cache_path(&self.0.options.project, baseline)
        {
            return Err(Error::new(
                "prepared baseline must be in the explicit project cache",
            ));
        }
        canonical_destination(baseline)?;
        if !sysutil::stat(baseline).is_ok_and(|info| info.is_regular()) {
            return Err(Error::new("prepared baseline does not exist"));
        }
        if !scene.starts_with("Assets/")
            || sysutil::clean(scene) != scene
            || sysutil::ext(scene) != ".unity"
        {
            return Err(Error::new(
                "prepared scene must be an authored Assets/...unity path",
            ));
        }
        inner.preparation_update_pending = !inner.last_ready_baseline.is_empty()
            && (baseline != inner.last_ready_baseline || scene != inner.last_ready_scene);
        inner.baseline = baseline.into();
        inner.scene = scene.into();
        Ok(())
    }

    /// Launches the runner unless one is already owned. The process is spawned
    /// and reaped by one dedicated thread, so its parent-death signal is tied
    /// to that thread rather than to the caller's.
    pub fn start(&self) -> Result<State> {
        let options = &self.0.options;
        let mut inner = self.lock();
        if inner.command.is_some() {
            drop(inner);
            return Ok(self.status());
        }
        if let Some(error) = self.0.lifetime.error() {
            return Err(error.into());
        }
        canonical_destination(&options.output)?;
        sysutil::mkdir_all(&options.output)?;
        let mut command = Command::new(&options.runner);
        command
            .args(["run", "--project", &options.project])
            .args(["--output", &options.output, "--watch", "--control"])
            .args(["--fps", &options.fps.to_string()]);
        for (flag, value) in [
            ("--baseline", &inner.baseline),
            ("--editor", &options.editor),
            ("--scene", &inner.scene),
        ] {
            if !value.is_empty() {
                command.args([flag, value]);
            }
        }
        for (flag, values) in [
            ("--content-scene", &inner.launch.content_scenes),
            ("--content-prefab", &inner.launch.content_prefabs),
        ] {
            for value in values {
                command.args([flag, value]);
            }
        }
        if inner.launch.addressables == Some(false) {
            command.arg("--no-addressables");
        }
        // The runner would restart the Player by itself for an edit it cannot
        // apply live. Here the caller decides when the game loses its state.
        if inner.launch.hot_reload == Some(false) {
            command.arg("--no-hot-reload");
        } else {
            command.args(["--hot-reload-fallback", inner.launch.fallback()]);
        }
        command.arg("--commands-stdin");
        if !inner.launch.game_arguments.is_empty() {
            command.arg("--").args(&inner.launch.game_arguments);
        }
        let (stdout, writer) = io::pipe()
            .map_err(|error| Error::new(format!("pipe: {}", sysutil::errno_text(&error))))?;
        let stderr = Root::open(&options.output)?.open_file(
            STDERR_LOG,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
        )?;
        let child_stderr = stderr
            .try_clone()
            .map_err(|error| Error::os("dup", STDERR_LOG, &error))?;
        // SAFETY: `getpid` has no preconditions.
        let parent = unsafe { libc::getpid() };
        command
            .current_dir(&options.project)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(writer)
            .stderr(child_stderr);
        // SAFETY: the hook runs between fork and exec and only makes
        // async-signal-safe system calls without allocating.
        unsafe {
            command.pre_exec(move || {
                // A shutdown interrupts the runner first.
                sysutil::default_stop_signals();
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // Signal self if the parent is already dead.
                if libc::getppid() != parent {
                    libc::kill(libc::getpid(), libc::SIGTERM);
                }
                Ok(())
            });
        }
        inner.launched += 1;
        let id = inner.launched;
        let done = Flag::new();
        let (spawned, started) = mpsc::channel();
        let observer = self.clone();
        let observed = done.clone();
        thread::spawn(move || observer.observe(command, id, stdout, stderr, observed, spawned));
        let (pid, commands) = match started.recv() {
            Ok(Ok(started)) => started,
            Ok(Err(error)) => {
                let error = Error::os("fork/exec", &options.runner, &error);
                inner.state.phase = "failed".into();
                inner.state.last_error = error.to_string();
                inner.notify();
                return Err(error);
            }
            Err(_) => return Err(Error::new("runner launch thread ended unexpectedly")),
        };
        if let Some(commands) = &commands {
            hotreload::nonblocking(commands);
        }
        inner.command = Some(Runner { id, pid });
        inner.commands = commands;
        inner.recompiles.clear();
        inner.done = Some(done);
        inner.stop_requested = false;
        inner.player = ProcessIdentity::default();
        inner.event_sizes.clear();
        inner.event_bytes = 0;
        inner.state = State {
            phase: "starting".into(),
            session_id: new_id(),
            last_sequence: inner.state.last_sequence,
            runner_stderr_path: sysutil::join(&[&options.output, STDERR_LOG]),
            hot_reload: HotReload::launched(inner.launch.hot_reload != Some(false)),
            launch: inner.launch.clone(),
            ..State::default()
        };
        inner.notify();
        drop(inner);
        Ok(self.status())
    }

    fn observe(
        &self,
        mut command: Command,
        id: u64,
        stdout: PipeReader,
        stderr: File,
        done: Flag,
        spawned: mpsc::Sender<io::Result<(i64, Option<ChildStdin>)>>,
    ) {
        let child = command.spawn();
        // The parent copies of the child's descriptors must close before reading.
        drop(command);
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                let _ = spawned.send(Err(error));
                return;
            }
        };
        let runner = Runner {
            id,
            pid: i64::from(child.id()),
        };
        // Taken before the wait below, which would close it.
        let _ = spawned.send(Ok((runner.pid, child.stdin.take())));
        let abandon = Arc::new(AtomicBool::new(false));
        let (read_done, read_result) = mpsc::channel();
        {
            let (reader, abandon, done) = (self.clone(), abandon.clone(), done.clone());
            thread::spawn(move || {
                let _ = read_done.send(reader.read_events(runner, stdout, &abandon, done));
            });
        }
        // A separate pipe keeps Wait from discarding buffered events. Waiting for
        // process exit independently also handles descendants that retain stdout.
        let exit_error = match child.wait() {
            Ok(status) => sysutil::exit_error(status),
            Err(error) => Some(format!("wait: {}", sysutil::errno_text(&error))),
        };
        let read_error = match read_result.recv_timeout(Duration::from_millis(250)) {
            Ok(result) => result,
            Err(_) => {
                abandon.store(true, Ordering::SeqCst);
                read_result.recv().unwrap_or(None)
            }
        };
        let output = &self.0.options.output;
        let failure = |inner: &Inner| {
            read_error.is_some()
                || exit_error.is_some() && !inner.stop_requested
                || !inner.stop_requested && inner.state.phase == "starting"
        };
        let failed = {
            let inner = self.lock();
            inner.command == Some(runner) && failure(&inner)
        };
        // Read while the original writer descriptor still proves this log's
        // identity. Filesystem work stays outside the controller mutex.
        let stderr_tail = match failed {
            true => read_runner_stderr_tail(output, &stderr).unwrap_or_default(),
            false => String::new(),
        };
        drop(stderr);
        let mut inner = self.lock();
        if inner.command == Some(runner) {
            signal_owned_player(&inner.player, libc::SIGKILL);
            inner.command = None;
            inner.forget_code();
            if failure(&inner) {
                inner.state.phase = "failed".into();
                let reason = read_error
                    .clone()
                    .or(exit_error.clone())
                    .unwrap_or("runner exited before confirmed Player readiness".into());
                inner.state.runner_stderr_path = sysutil::join(&[output, STDERR_LOG]);
                inner.state.last_error = runner_failure_message(&reason, &stderr_tail);
                inner.state.runner_stderr_tail = stderr_tail;
            } else {
                inner.state.phase = "stopped".into();
            }
            inner.state.pid = 0;
            inner.state.control_directory.clear();
            inner.notify();
        }
        done.set();
    }

    // The stdout scanner: lines of at most 4 MiB, each one JSON object. Returns
    // the protocol or read error that ended the stream, if any.
    fn read_events(
        &self,
        runner: Runner,
        mut stdout: PipeReader,
        abandon: &AtomicBool,
        done: Flag,
    ) -> Option<String> {
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let (mut ended, mut scanned) = (false, 0);
        let mut stream_error = None;
        let mut read_error = loop {
            let limit = pending.len().min(EVENT_LINE_MAXIMUM);
            let newline = pending[scanned.min(limit)..limit]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|index| index + scanned.min(limit));
            let mut line: Vec<u8> = match newline {
                Some(index) => {
                    let rest = pending.split_off(index + 1);
                    let mut line = std::mem::replace(&mut pending, rest);
                    line.pop();
                    scanned = 0;
                    line
                }
                None if pending.len() >= EVENT_LINE_MAXIMUM => {
                    break Some("line too long".to_string());
                }
                None if ended && pending.is_empty() => break None,
                None if ended => std::mem::take(&mut pending),
                None => {
                    scanned = limit;
                    match read_available(&mut stdout, &mut chunk, abandon) {
                        Ok(0) => ended = true,
                        Ok(count) => pending.extend_from_slice(&chunk[..count]),
                        Err(error) => {
                            ended = true;
                            stream_error = error;
                        }
                    }
                    continue;
                }
            };
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if String::from_utf8_lossy(&line).trim().is_empty() {
                continue;
            }
            let event = match jsondoc::parse_first(&line) {
                Ok((Json::Object(members), end)) => {
                    jsondoc::to_map(&members, false).map(|event| (event, end))
                }
                _ => Err(String::new()),
            };
            let Ok((event, end)) = event else {
                break Some("runner stdout contained malformed JSON event".to_string());
            };
            if !jsondoc::rest_is_space(&line, end) {
                break Some("runner stdout event contains trailing data".to_string());
            }
            let mut inner = self.lock();
            if inner.command != Some(runner) {
                break None;
            }
            let outcome = self.accept_event(&mut inner, runner, event, line.len());
            inner.notify();
            drop(inner);
            if let Err(error) = outcome {
                break Some(error.to_string());
            }
        };
        if read_error.is_none() {
            read_error = stream_error;
        }
        if let Some(message) = &read_error {
            let grace = {
                let mut inner = self.lock();
                inner.state.phase = "failed".into();
                inner.state.last_error = message.clone();
                inner.notify();
                inner.terminate_grace
            };
            let _ = sysutil::kill(-runner.pid, libc::SIGTERM);
            thread::spawn(move || {
                if !done.wait(Some(grace)) {
                    let _ = sysutil::kill(-runner.pid, libc::SIGKILL);
                }
            });
        }
        read_error
    }

    fn accept_event(
        &self,
        inner: &mut Inner,
        runner: Runner,
        event: Map<String, Value>,
        size: usize,
    ) -> Result<()> {
        let options = &self.0.options;
        let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or("").to_string();
        let name = text(event.get("event"));
        if name.is_empty() || name.len() > 128 {
            return Err(Error::new("runner stdout has no bounded event name"));
        }
        inner.state.last_sequence += 1;
        let sequence = inner.state.last_sequence;
        inner.state.events.push(Event {
            sequence,
            data: event.clone(),
        });
        inner.event_sizes.push_back(size);
        inner.event_bytes += size;
        while inner.state.events.len() > 64 || inner.event_bytes > 8 * 1024 * 1024 {
            inner.event_bytes -= inner.event_sizes.pop_front().unwrap_or(0);
            inner.state.events.remove(0);
        }
        // The answers of a watch window. One that the runner marks `pending`
        // is followed by another: of the same window, or of the next one,
        // which handles source changes that were saved meanwhile.
        if matches!(
            name.as_str(),
            "code-updated"
                | "content-updated"
                | "updated"
                | "unchanged"
                | "code-patched"
                | "update-rejected"
        ) {
            inner.state.update_pending = event.get("pending") == Some(&Value::Bool(true));
        }
        match name.as_str() {
            "ready" | "code-updated" | "content-updated" => {
                let (report, manifest) = (text(event.get("report")), text(event.get("manifest")));
                if !owned_path(&options.output, &report)
                    || !sysutil::is_owned_cache_path(&options.project, &manifest)
                    || canonical_destination(&manifest).is_err()
                {
                    return Err(Error::new(
                        "runner readiness names an invalid owned report/manifest",
                    ));
                }
                if !sysutil::lstat(&manifest).is_ok_and(|info| info.is_regular()) {
                    return Err(Error::new("runner ready manifest is not a regular file"));
                }
                let data = read_bounded(&format!("{report}.ready.json"), CONTROL_ARTIFACT_MAXIMUM);
                let ready = data
                    .as_ref()
                    .ok()
                    .and_then(|data| Some((decode_ready(data)?, data)))
                    .filter(|(ready, _)| {
                        ready.schema == 1
                            && ready.pid >= 1
                            && number_pid(event.get("pid")) == ready.pid
                            && ready.scene_loaded
                            && ready.errors == 0
                            && ready.exceptions == 0
                    });
                let Some((ready, data)) = ready else {
                    return Err(Error::new(
                        "runner returned an incompatible Player readiness identity",
                    ));
                };
                if let Err(error) = open_owned_directory(&options.output, &ready.control_directory)
                {
                    return Err(Error::wrap(
                        "runner returned an invalid control endpoint",
                        &error,
                    ));
                }
                let revision = sysutil::base(&sysutil::dir(&manifest));
                if !valid_id(&revision) {
                    return Err(Error::new("runner readiness has no valid revision"));
                }
                let identity = read_process_identity(ready.pid)
                    .ok()
                    .filter(|(_, parent)| ready.pid == runner.pid || *parent == runner.pid);
                let Some((identity, _)) = identity else {
                    return Err(Error::new(
                        "runner readiness PID does not belong to its session",
                    ));
                };
                if ready.pid != runner.pid {
                    inner.player = identity;
                }
                inner.state.pid = ready.pid;
                inner.state.control_directory = ready.control_directory;
                inner.state.revision = revision;
                // Decoding into the existing map keeps earlier keys.
                let report = jsondoc::parse(data)
                    .and_then(|json| jsondoc::to_map(json.members().unwrap_or(&[]), true))
                    .map_err(Error::new)?;
                inner
                    .state
                    .ready
                    .get_or_insert_with(Map::new)
                    .extend(report);
                if !inner.stop_requested {
                    inner.state.phase = "ready".into();
                }
                inner.state.last_error.clear();
                inner.state.last_ready_sequence = sequence;
                if name != "ready" || inner.preparation_update_pending {
                    inner.state.last_update_sequence = sequence;
                    inner.state.update_error.clear();
                    inner.state.preparation_required = false;
                }
                if inner.preparation_update_pending {
                    if let Some(stored) = inner.state.events.last_mut() {
                        stored
                            .data
                            .insert("preparationActivated".into(), Value::Bool(true));
                    }
                    inner.preparation_update_pending = false;
                }
                inner.last_ready_baseline = inner.baseline.clone();
                inner.last_ready_scene = inner.scene.clone();
                inner.state.hot_reload.restarted(sequence, &event);
                // The runner names a restart without a new compilation
                // `content-updated`; a requested one is that whenever the
                // sources did not change since the last compilation.
                inner.answer_recompile(sequence, &event, None);
            }
            "updated" => {
                let ack = event.get("ack").and_then(Value::as_object);
                let field = |name: &str| ack.and_then(|ack| ack.get(name));
                let revision = text(field("loadedRevision"));
                if ack.is_none()
                    || field("success") != Some(&Value::Bool(true))
                    || field("restartRequired") != Some(&Value::Bool(false))
                    || number_pid(field("pid")) != inner.state.pid
                    || !valid_id(&revision)
                    || inner.state.pid < 1
                {
                    return Err(Error::new(
                        "runner update lacks a confirmed Player revision/PID acknowledgement",
                    ));
                }
                inner.state.revision = revision;
                inner.state.last_update_sequence = sequence;
                inner.state.last_ready_sequence = sequence;
                inner.state.last_error.clear();
                inner.state.update_error.clear();
                inner.state.preparation_required = false;
                inner.state.hot_reload.confirmed(&event);
            }
            // A confirmation that was under way when a stop began is still one
            // of the Player being stopped, not a protocol error of the session.
            "unchanged" => {
                let revision = text(event.get("revision"));
                if !matches!(inner.state.phase.as_str(), "ready" | "stopping")
                    || inner.state.pid < 1
                    || number_pid(event.get("pid")) != inner.state.pid
                    || !valid_id(&revision)
                    || revision != inner.state.revision
                {
                    return Err(Error::new(
                        "runner unchanged update does not confirm the accepted Player revision/PID",
                    ));
                }
                inner.state.last_update_sequence = sequence;
                inner.state.last_error.clear();
                inner.state.update_error.clear();
                inner.state.preparation_required = false;
                inner.state.hot_reload.confirmed(&event);
            }
            // The same Player with new method bodies: neither its PID nor
            // its scene revision changes, only the code generation. It is
            // accepted in the phases of `unchanged`.
            "code-patched" => {
                let revision = text(event.get("revision"));
                let generation = hotreload::count(event.get("codeGeneration")).unwrap_or(0);
                if !matches!(inner.state.phase.as_str(), "ready" | "stopping")
                    || inner.state.pid < 1
                    || number_pid(event.get("pid")) != inner.state.pid
                    || !valid_id(&revision)
                    || revision != inner.state.revision
                    || generation < 1
                {
                    return Err(Error::new(
                        "runner code patch does not confirm the accepted Player revision/PID and a code generation",
                    ));
                }
                inner.state.hot_reload.patched(sequence, generation, &event);
                inner.state.last_update_sequence = sequence;
                inner.state.last_error.clear();
                inner.state.update_error.clear();
                inner.state.preparation_required = false;
            }
            "update-rejected" => {
                let mut message = text(event.get("error"));
                if message.is_empty() {
                    message = "runner rejected the source update".into();
                }
                inner.state.last_error = message.clone();
                inner.state.update_error = message.clone();
                inner.state.preparation_required =
                    event.get("preparationRequired") == Some(&Value::Bool(true));
                inner.state.last_update_rejected_sequence = sequence;
                // A restart needs no patch, so a refusal is about a source
                // edit and never the answer to a `recompile` command.
                if event.get("fullRecompileRequired") == Some(&Value::Bool(true)) {
                    inner.state.hot_reload.refused(sequence, &event, &message);
                } else {
                    // The rejection of a scene part says when no refusal
                    // stands, as a confirmation does.
                    inner.state.hot_reload.confirmed(&event);
                    inner.answer_recompile(sequence, &event, Some(&message));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Requests shutdown (SIGINT, then SIGTERM, then SIGKILL to the runner's
    /// process group) and waits for the runner to be reaped. If `ctx` ends
    /// first the current state and its error are returned while cleanup
    /// continues in the background.
    pub fn stop(&self, ctx: &Context) -> std::result::Result<State, StateError> {
        let mut inner = self.lock();
        let (Some(command), Some(done)) = (inner.command, inner.done.clone()) else {
            drop(inner);
            return Ok(self.status());
        };
        inner.state.phase = "stopping".into();
        let start_shutdown = !inner.stop_requested;
        inner.stop_requested = true;
        let (grace, terminate_grace) = (inner.stop_grace, inner.terminate_grace);
        inner.notify();
        drop(inner);
        if start_shutdown {
            let done = done.clone();
            thread::spawn(move || stop_process(command.pid, &done, grace, terminate_grace));
        }
        match select(&[Source::Flag(&done), Source::Context(ctx)], None) {
            Some(1) => Err(StateError {
                state: Box::new(self.status()),
                error: ctx.error().expect("selected context is done").into(),
                typed: None,
            }),
            _ => Ok(self.status()),
        }
    }

    /// Ends the controller lifetime and stops the runner within 20 seconds.
    pub fn close(&self) -> Result<()> {
        self.0.lifetime.cancel();
        let ctx = Context::background().with_timeout(Duration::from_secs(20));
        let outcome = self.stop(&ctx);
        ctx.cancel();
        outcome.map(drop).map_err(|failure| failure.error)
    }

    /// Blocks until the session answered for what followed sequence `after`,
    /// or it failed, stopped, or `ctx` ended. A confirmed or rejected source
    /// update answers an update wait (`!ready`). A readiness wait needs a
    /// ready Player, and that Player becoming ready answers it too. Neither
    /// succeeds while hot reload refused an edit whose code is not in the
    /// Player: both fail with the typed `full_recompile_required`.
    pub fn wait(
        &self,
        ctx: &Context,
        after: u64,
        ready: bool,
    ) -> std::result::Result<State, StateError> {
        loop {
            let (state, changed) = {
                let inner = self.lock();
                (inner.state.clone(), inner.changed.clone())
            };
            let ended = state.phase == "failed" || state.phase == "stopped";
            let confirmed =
                state.last_update_sequence > after || ready && state.last_ready_sequence > after;
            let rejected = state.last_update_rejected_sequence > after;
            // An edit that was patched in, refused or rejected starts no
            // Player, and a readiness wait must not outlast it. An answer the
            // runner marked as not its last is none: the changes it is still
            // working on may be the caller's.
            let answered = (confirmed || rejected)
                && (!ready || state.phase == "ready")
                && !state.update_pending;
            if ended || answered {
                // A rejection is the outcome while no update followed it. The
                // Player that became ready before one still answers a
                // readiness wait.
                let rejection = rejected
                    && state.last_update_rejected_sequence > state.last_update_sequence
                    && !(ready && state.last_ready_sequence > after);
                let message = if state.phase == "failed" {
                    state.last_error.clone()
                } else if state.phase == "stopped" {
                    "runtime session stopped before the requested readiness/update".into()
                } else if rejection && !state.code_refused() {
                    state.update_error.clone()
                } else if state.hot_reload.full_recompile_required {
                    // Success would say that the Player runs the caller's C#
                    // edit. A scene update confirmed since proves nothing
                    // about the code, whatever the caller has read before.
                    let told = match state.last_update_sequence > after {
                        true => Refused::UpdatedAfter(after),
                        false => Refused::Edit,
                    };
                    let (error, typed) = state.hot_reload.refusal(told);
                    return Err(StateError {
                        state: Box::new(state),
                        error,
                        typed: Some(typed),
                    });
                } else {
                    return Ok(state);
                };
                return Err(StateError {
                    state: Box::new(state),
                    error: Error::new(message),
                    typed: None,
                });
            }
            if select(&[Source::Context(ctx), Source::Flag(&changed)], None) == Some(0) {
                return Err(StateError {
                    state: Box::new(self.status()),
                    error: ctx.error().expect("selected context is done").into(),
                    typed: None,
                });
            }
        }
    }

    /// The MCP boundary. `raw` is the JSON arguments object. Runtime commands
    /// accept expectedRevision and expectedPid so clients can bind observations
    /// to the Player they actually inspected.
    pub fn handle(
        &self,
        ctx: &Context,
        operation: &str,
        raw: &[u8],
    ) -> std::result::Result<Reply, Failure> {
        if raw.len() > 65536 {
            return Err(Error::new("runtime arguments exceed 64 KiB").into());
        }
        let raw: &[u8] = if raw.is_empty() { b"{}" } else { raw };
        let mut args: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        match jsondoc::parse(raw).map_err(Error::new)? {
            Json::Null => return Err(Error::new("runtime arguments must be an object").into()),
            Json::Object(members) => {
                for member in &members {
                    args.insert(member.key.clone(), member.raw(raw).to_vec());
                }
            }
            other => {
                return Err(Error::new(Target::VALUE.mismatch(other.kind(), "object")).into());
            }
        }
        let mut operation = operation.to_string();
        let prefix = match operation.as_str() {
            "session_control" => Some("session"),
            "hot_reload" => Some("hot_reload"),
            _ => None,
        };
        if let Some(prefix) = prefix {
            let mut action = String::new();
            unmarshal(args.get("action"), |value| {
                jsondoc::set_string(&mut action, value, Target::VALUE)
            })?;
            operation = format!("{prefix}_{action}");
        }
        match operation.as_str() {
            "session_status" => return Ok(Reply::State(Box::new(self.status()))),
            "session_start" => {
                let previous = self.lock().launch.clone();
                if let Some(launch) = Launch::from_arguments(&previous, args.get("arguments"))? {
                    let mut inner = self.lock();
                    if inner.command.is_some() && inner.launch != launch {
                        return Err(Error::new(
                            "the Player already runs with other launch arguments; use session_control(reset) to relaunch with these",
                        )
                        .into());
                    }
                    inner.launch = launch;
                }
                return match self.start() {
                    Ok(state) => Ok(Reply::State(Box::new(state))),
                    Err(error) => Err(Failure {
                        error,
                        reply: Some(Reply::State(Box::default())),
                        typed: None,
                    }),
                };
            }
            "session_stop" => return Ok(Reply::State(Box::new(self.stop(ctx)?))),
            "session_reset" => {
                let previous = self.lock().launch.clone();
                let launch = Launch::from_arguments(&previous, args.get("arguments"))?;
                if let Err(failure) = self.stop(ctx) {
                    return Err(failure.error.into());
                }
                if let Some(launch) = launch {
                    self.lock().launch = launch;
                }
                return match self.start() {
                    Ok(state) => Ok(Reply::State(Box::new(state))),
                    Err(error) => Err(Failure {
                        error,
                        reply: Some(Reply::State(Box::default())),
                        typed: None,
                    }),
                };
            }
            "session_wait" | "session_wait_update" => {
                let mut after = 0u64;
                if let Some(value) = args.get("afterSequence") {
                    unmarshal(Some(value), |value| {
                        jsondoc::set_uint64(&mut after, value, Target::VALUE)
                    })?;
                }
                return Ok(Reply::State(Box::new(self.wait(
                    ctx,
                    after,
                    operation == "session_wait",
                )?)));
            }
            "hot_reload_status" => return Ok(Reply::Value(self.hot_reload())),
            "hot_reload_recompile" => {
                let mut id = new_id();
                if let Some(value) = args.get("requestId") {
                    unmarshal(Some(value), |value| {
                        jsondoc::set_string(&mut id, value, Target::VALUE)
                    })?;
                }
                return Ok(Reply::State(Box::new(self.recompile(ctx, &id)?)));
            }
            "runtime_query" | "runtime_command" => {
                let selected = unmarshal(args.get("operation"), |value| {
                    jsondoc::set_string(&mut operation, value, Target::VALUE)
                });
                if selected.is_err() {
                    return Err(Error::new("runtime operation is required").into());
                }
            }
            _ => {}
        }
        // `stream` is sent by the remote session alone, through `request`.
        if operation == "stream" || !allowed_operation(&operation) {
            return Err(unsupported(&operation).into());
        }
        Ok(Reply::Value(self.request(ctx, &operation, &args)?))
    }

    /// Delivers one immutable runtime command, or replays its recorded outcome.
    /// `args` maps the top-level argument names to their raw JSON. A delivered
    /// but unconfirmed command yields `{"status":"result_pending"}` (waiting
    /// ended) or `{"status":"result_unknown"}` (its Player is gone).
    pub fn request(
        &self,
        ctx: &Context,
        operation: &str,
        args: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Value> {
        if !allowed_operation(operation) {
            return Err(unsupported(operation));
        }
        if let Some(error) = ctx.error() {
            return Err(error.into());
        }
        let mut id = new_id();
        if let Some(value) = args.get("requestId") {
            unmarshal(Some(value), |value| {
                jsondoc::set_string(&mut id, value, Target::VALUE)
            })?;
        }
        if !valid_id(&id) {
            return Err(Error::new(
                "requestId must be ASCII letters, digits or hyphens, at most 64 characters",
            ));
        }
        let canonical_arguments = canonical_request_arguments(args)?;
        let options = &self.0.options;
        let record = {
            let mut inner = self.lock();
            let existing = inner.requests.get(&id).cloned();
            let (mut pid, mut revision) = match &existing {
                Some(record) => (record.pid, record.revision.clone()),
                None => (inner.state.pid, inner.state.revision.clone()),
            };
            if let Some(value) = args.get("expectedPid") {
                let decoded = unmarshal(Some(value), |value| {
                    jsondoc::set_int(&mut pid, value, Target::VALUE)
                });
                if decoded.is_err() || pid < 1 {
                    return Err(Error::new("expectedPid must be a positive integer"));
                }
            }
            if let Some(value) = args.get("expectedRevision") {
                let decoded = unmarshal(Some(value), |value| {
                    jsondoc::set_string(&mut revision, value, Target::VALUE)
                });
                if decoded.is_err() || !valid_id(&revision) {
                    return Err(Error::new(
                        "expectedRevision must be a bounded revision identifier",
                    ));
                }
            }
            match existing {
                Some(record) => {
                    if record.operation != operation
                        || record.pid != pid
                        || record.revision != revision
                        || record.arguments != canonical_arguments
                    {
                        return Err(Error::new(
                            "requestId was already used for another command or revision",
                        ));
                    }
                    record
                }
                None => {
                    let state = &inner.state;
                    if state.phase != "ready" || state.pid < 1 || state.control_directory.is_empty()
                    {
                        return Err(Error::new(
                            "Player is not ready; use session_start and session_wait",
                        ));
                    }
                    if inner.requests.len() >= 4096 {
                        return Err(Error::new(
                            "session request journal is full; inspect outstanding results and create a new controller",
                        ));
                    }
                    if pid != state.pid || operation != "status" && revision != state.revision {
                        let (code, message) = if pid != state.pid {
                            ("stale_process", "Command targets another Player process")
                        } else {
                            ("stale_revision", "Command targets another scene revision")
                        };
                        return Ok(json!({
                            "requestId": id, "pid": state.pid, "revision": state.revision, "success": false,
                            "error": {"code": code, "message": message},
                        }));
                    }
                    let payload =
                        request_envelope(&id, pid, &revision, operation, &canonical_arguments);
                    if payload.len() > 65536 {
                        return Err(Error::new("runtime request exceeds 64 KiB"));
                    }
                    let record = Arc::new(RequestRecord {
                        id: id.clone(),
                        operation: operation.into(),
                        session_id: state.session_id.clone(),
                        directory: state.control_directory.clone(),
                        pid,
                        revision,
                        hash: crate::sha256(&payload),
                        arguments: canonical_arguments,
                        envelope: payload,
                    });
                    journal::persist_request(&options.output, &record)?;
                    inner.requests.insert(id.clone(), record.clone());
                    record
                }
            }
        };
        match journal::read_receipt(&options.output, &record) {
            Ok(receipt) => return journal::decode_receipt(&receipt, &record, operation),
            Err(error) if !error.is_not_exist() => return Err(error),
            Err(_) => {}
        }
        let response = sysutil::join(&[&record.directory, &format!("{id}.response.json")]);
        let outcome =
            |status: &str| json!({"requestId": id, "status": status, "response": response});
        let root = match open_owned_directory(&options.output, &record.directory) {
            Ok(root) => root,
            Err(error) if error.is_not_exist() => return Ok(outcome("result_unknown")),
            Err(error) => return Err(error),
        };
        let bound = Binding {
            session_id: record.session_id.clone(),
            pid: record.pid,
            directory: record.directory.clone(),
            ready: true,
        };
        if self.binding() == bound {
            if let Some(error) = ctx.error() {
                return Err(error.into());
            }
            publish_root_immutable(&root, &format!("{id}.request.json"), &record.envelope)?;
        }
        loop {
            match read_root_bounded(
                &root,
                &format!("{id}.response.json"),
                CONTROL_ARTIFACT_MAXIMUM,
            ) {
                Ok(contents) => {
                    let result = journal::decode_receipt(&contents, &record, operation)?;
                    journal::persist_receipt(&options.output, &record, &contents)?;
                    return Ok(result);
                }
                Err(error) if !error.is_not_exist() => return Err(error),
                Err(_) => {}
            }
            if self.binding() != bound {
                return Ok(outcome("result_unknown"));
            }
            if select(&[Source::Context(ctx)], Some(Duration::from_millis(20))).is_some() {
                return Ok(outcome("result_pending"));
            }
        }
    }
}

fn unsupported(operation: &str) -> Error {
    Error::new(format!(
        "unsupported runtime operation {}",
        jsondoc::quoted_text(operation)
    ))
}
// Decodes one scalar argument into its target; an absent value is empty input.
fn unmarshal(
    raw: Option<&Vec<u8>>,
    set: impl FnOnce(&Json) -> std::result::Result<(), String>,
) -> Result<()> {
    let value = jsondoc::parse(raw.map_or(&[][..], Vec::as_slice)).map_err(Error::new)?;
    set(&value).map_err(Error::new)
}
// The command arguments: the explicit `arguments` object or every other
// top-level argument, re-encoded as a decoded map (sorted keys, verbatim numbers,
// last duplicate key wins).
fn canonical_request_arguments(args: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let arguments = match args.get("arguments") {
        Some(arguments) if !arguments.is_empty() => arguments.clone(),
        _ => forwarded_arguments(args),
    };
    if arguments.len() > 65536 {
        return Err(Error::new("runtime request exceeds 64 KiB"));
    }
    let not_object = || Error::new("runtime arguments must be an object");
    let (object, end) = match jsondoc::parse_first(&arguments) {
        Ok((Json::Object(members), end)) => (
            jsondoc::to_map(&members, false).map_err(|_| not_object())?,
            end,
        ),
        _ => return Err(not_object()),
    };
    let canonical = jsondoc::encode(&Value::Object(object));
    if !jsondoc::rest_is_space(&arguments, end) {
        return Err(Error::new("runtime arguments contain trailing data"));
    }
    Ok(canonical)
}
// The immutable command file the Player reads; its SHA-256 binds the receipt.
fn request_envelope(
    id: &str,
    pid: i64,
    revision: &str,
    operation: &str,
    arguments: &[u8],
) -> Vec<u8> {
    let mut payload = format!(
        "{{\"schemaVersion\":1,\"requestId\":{},\"expectedPid\":{pid},\"expectedRevision\":{},\"operation\":{},\"arguments\":",
        jsondoc::quote(id),
        jsondoc::quote(revision),
        jsondoc::quote(operation),
    )
    .into_bytes();
    payload.extend_from_slice(&jsondoc::compact(arguments));
    payload.push(b'}');
    payload
}
// JSON of the remaining top-level arguments (a map of raw values);
// the error of an invalid raw value is discarded and encoding continues with no bytes.
fn forwarded_arguments(args: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut result = vec![b'{'];
    for (key, value) in args {
        if matches!(
            key.as_str(),
            "operation" | "requestId" | "expectedPid" | "expectedRevision"
        ) {
            continue;
        }
        if !jsondoc::valid(value) {
            return Vec::new();
        }
        if result.len() > 1 {
            result.push(b',');
        }
        result.extend_from_slice(jsondoc::quote(key).as_bytes());
        result.push(b':');
        result.extend_from_slice(&jsondoc::compact(value));
    }
    result.push(b'}');
    result
}

#[derive(Default)]
struct Ready {
    schema: i64,
    pid: i64,
    control_directory: String,
    errors: i64,
    exceptions: i64,
    scene_loaded: bool,
}
fn decode_ready(data: &[u8]) -> Option<Ready> {
    const FIELDS: [&str; 6] = [
        "schemaVersion",
        "pid",
        "controlDirectory",
        "Errors",
        "Exceptions",
        "Scene",
    ];
    let json = jsondoc::parse(data).ok()?;
    let members: &[Member] = match &json {
        Json::Null => &[],
        Json::Object(members) => members,
        _ => return None,
    };
    let mut ready = Ready::default();
    let at = Target::VALUE;
    jsondoc::decode_struct(members, &FIELDS, |field, member| {
        let value = &member.value;
        match field {
            0 => jsondoc::set_int(&mut ready.schema, value, at),
            1 => jsondoc::set_int(&mut ready.pid, value, at),
            2 => jsondoc::set_string(&mut ready.control_directory, value, at),
            3 => jsondoc::set_int(&mut ready.errors, value, at),
            4 => jsondoc::set_int(&mut ready.exceptions, value, at),
            _ => match value {
                Json::Null => Ok(()),
                Json::Object(scene) => jsondoc::decode_struct(scene, &["isLoaded"], |_, member| {
                    jsondoc::set_bool(&mut ready.scene_loaded, &member.value, at)
                }),
                other => Err(at.mismatch(other.kind(), "struct")),
            },
        }
    })
    .ok()?;
    Some(ready)
}

// Waits for readable stdout in short slices so an abandoned stream (a
// descendant still holding the pipe after the runner exited) ends like EOF.
fn read_available(
    stdout: &mut PipeReader,
    chunk: &mut [u8],
    abandon: &AtomicBool,
) -> std::result::Result<usize, Option<String>> {
    loop {
        if abandon.load(Ordering::SeqCst) {
            return Err(None);
        }
        let mut poll = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` points to one initialized descriptor record.
        if unsafe { libc::poll(&mut poll, 1, 20) } <= 0 {
            continue;
        }
        match stdout.read(chunk) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Some(format!("read |0: {}", sysutil::errno_text(&error)))),
        }
    }
}

fn stop_process(pid: i64, done: &Flag, grace: Duration, terminate_grace: Duration) {
    for (signal, duration) in [
        (libc::SIGINT, grace),
        (libc::SIGTERM, terminate_grace),
        (libc::SIGKILL, Duration::ZERO),
    ] {
        if done.is_set() {
            return;
        }
        let _ = sysutil::kill(-pid, signal);
        if duration.is_zero() || done.wait(Some(duration)) {
            return;
        }
    }
}

fn read_runner_stderr_tail(output: &str, writer: &File) -> Result<String> {
    canonical_destination(output)?;
    let Some(expected) = sysutil::fstat(writer).ok().filter(|info| info.is_regular()) else {
        return Err(Error::new("runner stderr writer is not a regular file"));
    };
    let root = Root::open(output)?;
    let file = root.open_file(STDERR_LOG, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    let Some(before) = sysutil::fstat(&file)
        .ok()
        .filter(|info| info.is_regular() && expected.same_file(info))
    else {
        return Err(Error::new(
            "runner stderr log identity differs from its writer",
        ));
    };
    let maximum = RUNNER_STDERR_MAXIMUM as i64;
    let start = (before.size - maximum).max(0);
    let mut buffer = vec![0u8; (before.size - start) as usize];
    let mut count = 0;
    while count < buffer.len() {
        match file.read_at(&mut buffer[count..], start as u64 + count as u64) {
            Ok(0) => break,
            Ok(read) => count += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::os("read", STDERR_LOG, &error)),
        }
    }
    let unchanged = sysutil::fstat(&file)
        .ok()
        .zip(root.lstat(STDERR_LOG).ok())
        .is_some_and(|(after, current)| {
            current.is_regular()
                && expected.same_file(&after)
                && expected.same_file(&current)
                && count == buffer.len()
                && before.size == after.size
                && before.size == current.size
                && before.modified == after.modified
                && before.modified == current.modified
        });
    if !unchanged {
        return Err(Error::new(
            "runner stderr log changed while reading diagnostics",
        ));
    }
    let mut tail = &buffer[..];
    if start != 0 {
        // Avoid displaying a fragment of an oversized first line when subsequent
        // complete lines include the actual failure reason.
        if let Some(newline) = tail.iter().position(|&byte| byte == b'\n')
            && newline + 1 < tail.len()
        {
            tail = &tail[newline + 1..];
        }
    }
    let text: Vec<u8> = tail.iter().copied().filter(|&byte| byte != 0).collect();
    Ok(to_valid_utf8(&text).trim().to_string())
}
// Text with each run of invalid UTF-8 bytes replaced by one `?`.
fn to_valid_utf8(mut data: &[u8]) -> String {
    let mut text = String::new();
    let mut replaced = false;
    loop {
        match std::str::from_utf8(data) {
            Ok(valid) => {
                text.push_str(valid);
                return text;
            }
            Err(error) => {
                let (valid, rest) = data.split_at(error.valid_up_to());
                if !valid.is_empty() {
                    replaced = false;
                    text.push_str(std::str::from_utf8(valid).unwrap());
                }
                if !replaced {
                    text.push('?');
                    replaced = true;
                }
                data = &rest[1..];
            }
        }
    }
}
fn runner_failure_message(reason: &str, tail: &str) -> String {
    if tail.is_empty() {
        return reason.into();
    }
    let mut prefix = format!("{reason}\nrunner stderr:\n").into_bytes();
    prefix.truncate(RUNNER_STDERR_MAXIMUM);
    let available = RUNNER_STDERR_MAXIMUM - prefix.len();
    let mut tail = tail.as_bytes().to_vec();
    if tail.len() > available {
        tail = to_valid_utf8(&tail[tail.len() - available..]).into_bytes();
    }
    prefix.extend_from_slice(&tail);
    // A reason cut inside a character is the only way to get invalid bytes here.
    String::from_utf8(prefix)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

fn allowed_operation(operation: &str) -> bool {
    matches!(
        operation,
        "status"
            | "hierarchy"
            | "inspect"
            | "logs"
            | "pause"
            | "resume"
            | "wait_frames"
            | "screenshot"
            | "raycast"
            | "custom"
            | "tools"
            | "invoke"
            | "simulation_pause"
            | "simulation_resume"
            | "simulation_step"
            | "input"
            | "stream"
    )
}

#[cfg(test)]
fn publish_immutable(path: &str, bytes: &[u8]) -> Result<()> {
    let directory = sysutil::dir(path);
    let root = open_owned_directory(&directory, &directory)?;
    publish_root_immutable(&root, &sysutil::base(path), bytes)
}
fn publish_root_immutable(root: &Root, name: &str, bytes: &[u8]) -> Result<()> {
    publish_root_bounded_immutable(root, name, bytes, 65536)
}
fn publish_root_bounded_immutable(
    root: &Root,
    name: &str,
    bytes: &[u8],
    maximum: i64,
) -> Result<()> {
    if bytes.len() as i64 > maximum {
        return Err(Error::new("immutable artifact exceeds its size limit"));
    }
    match read_root_bounded(root, name, maximum) {
        Ok(old) if old != bytes => {
            return Err(Error::new("requestId was already used for another command"));
        }
        Ok(_) => return Ok(()),
        Err(error) if !error.is_not_exist() => return Err(error),
        Err(_) => {}
    }
    let temporary = format!(".command-{}", new_id());
    let mut file = root.open_file(
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let written = file
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all())
        .map_err(|error| Error::os("write", &temporary, &error));
    drop(file);
    let linked = written.and_then(|()| match root.link(&temporary, name) {
        Err(error) if error.is_exist() => match read_root_bounded(root, name, maximum) {
            Ok(old) if old == bytes => Ok(()),
            _ => Err(Error::new("requestId conflicts with a concurrent command")),
        },
        other => other,
    });
    let _ = root.remove(&temporary);
    linked
}

fn read_bounded(path: &str, maximum: i64) -> Result<Vec<u8>> {
    let directory = sysutil::dir(path);
    let root = open_owned_directory(&directory, &directory)?;
    read_root_bounded(&root, &sysutil::base(path), maximum)
}
fn read_root_bounded(root: &Root, name: &str, maximum: i64) -> Result<Vec<u8>> {
    if maximum < 0 {
        return Err(Error::new("invalid control artifact size limit"));
    }
    let info = root.lstat(name)?;
    if !info.is_regular() || info.size > maximum {
        return Err(Error::new("control artifact is not a bounded regular file"));
    }
    let file = root.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    if !sysutil::fstat(&file).is_ok_and(|opened| {
        opened.is_regular() && info.same_file(&opened) && opened.size <= maximum
    }) {
        return Err(Error::new(
            "control artifact changed or is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    let read = file.take(maximum as u64 + 1).read_to_end(&mut bytes);
    if bytes.len() as i64 > maximum {
        return Err(Error::new("control artifact exceeded its size limit"));
    }
    read.map_err(|error| Error::os("read", name, &error))?;
    Ok(bytes)
}

fn owned_path(root: &str, path: &str) -> bool {
    sysutil::is_abs(path)
        && sysutil::clean(path) == path
        && path != root
        && sysutil::within(root, path)
}

fn canonical_destination(path: &str) -> Result<()> {
    if !sysutil::is_abs(path) || sysutil::clean(path) != path {
        return Err(Error::new("control paths must be clean absolute paths"));
    }
    let mut current = path.to_string();
    loop {
        match sysutil::lstat(&current) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                let parent = sysutil::dir(&current);
                if parent == current {
                    return Err(Error::os("lstat", &current, &error));
                }
                current = parent;
            }
            Err(error) => return Err(Error::os("lstat", &current, &error)),
            Ok(_) => {
                if !sysutil::eval_symlinks(&current).is_ok_and(|canonical| canonical == current) {
                    return Err(Error::new(format!(
                        "control path contains symlink ancestors: {path}"
                    )));
                }
                return Ok(());
            }
        }
    }
}

fn open_owned_directory(root: &str, path: &str) -> Result<Root> {
    if !sysutil::is_abs(root)
        || !sysutil::is_abs(path)
        || sysutil::clean(path) != path
        || !sysutil::within(root, path)
    {
        return Err(Error::new("control directory is outside its owned output"));
    }
    canonical_destination(root)?;
    canonical_destination(path)?;
    let parent = Root::open(root)?;
    if root == path {
        return Ok(parent);
    }
    let relative = sysutil::rel(root, path)
        .ok_or_else(|| Error::new(format!("Rel: can't make {path} relative to {root}")))?;
    parent.open_root(&relative)
}

// An exact positive integer from a decoded event number, otherwise 0.
fn number_pid(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number
            .to_string()
            .parse::<i64>()
            .ok()
            .filter(|pid| *pid >= 1)
            .unwrap_or(0),
        _ => 0,
    }
}

fn read_process_identity(pid: i64) -> Result<(ProcessIdentity, i64)> {
    let path = format!("/proc/{pid}/stat");
    let file = File::open(&path).map_err(|error| Error::os("open", &path, &error))?;
    let mut data = Vec::new();
    if file.take(4097).read_to_end(&mut data).is_err() || data.len() > 4096 {
        return Err(Error::new("process identity exceeds bounds"));
    }
    let (started, parent) = sysutil::parse_proc_stat(&data).map_err(|reason| {
        Error::new(match reason {
            "process is exited" => "process is missing or exited",
            other => other,
        })
    })?;
    Ok((ProcessIdentity { pid, started }, parent))
}

fn signal_owned_player(identity: &ProcessIdentity, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(identity.pid) else {
        return;
    };
    if pid < 1 {
        return;
    }
    let current = read_process_identity(identity.pid);
    // SAFETY: `getpgid` has no memory-safety preconditions.
    let group = unsafe { libc::getpgid(pid) };
    if current.is_ok_and(|(current, _)| current.started == identity.started) && group == pid {
        let _ = sysutil::kill(-identity.pid, signal);
    }
}

#[cfg(test)]
mod tests;
