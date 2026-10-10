//! Attaches to a public Editor mailbox or owns a batch Editor.
//! Receipts and accepted IDs survive Editor and MCP process restarts.
//!
//! Every context parameter is a [`Context`]; the client lifetime is a context derived from
//! the one given to [`Client::new`], and cancelling it closes the client and its owned Editor.
use crate::{
    cancel::{Context, Flag, Source, select},
    jsondoc::{self, Json, Member, Target},
    sysutil::{self, Root},
};
use serde::{Serialize, Serializer, ser::Error as _};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub use crate::sysutil::{Error, ErrorKind, Result};

pub const PROFILE: &str = crate::ENGINE_VERSION;
const REQUEST_LIMIT: usize = 256 * 1024;
const RESPONSE_LIMIT: usize = 32 * 1024 * 1024;

/// Zero durations select the defaults (180 s startup, 50 ms poll).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub project: String,
    pub editor: String,
    pub startup_timeout: Duration,
    pub poll_interval: Duration,
}
/// `arguments` is raw JSON (empty means `{}`); an empty `id` mints a new one and
/// `expected_pid` 0 binds the request to the current Editor.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub id: String,
    pub action: String,
    pub arguments: Vec<u8>,
    pub expected_pid: i64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}
// Raw JSON output: compacted and HTML-escaped, `null` when absent.
fn raw_json<S: Serializer>(raw: &[u8], serializer: S) -> std::result::Result<S::Ok, S::Error> {
    if raw.is_empty() {
        return serializer.serialize_unit();
    }
    let text = String::from_utf8_lossy(&jsondoc::compact(raw)).into_owned();
    serde_json::value::RawValue::from_string(text)
        .map_err(S::Error::custom)?
        .serialize(serializer)
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub schema_version: i64,
    pub backend: String,
    pub request_id: String,
    pub request_sha256: String,
    pub project: String,
    pub unity_version: String,
    pub editor: String,
    pub pid: i64,
    pub status: String,
    pub success: bool,
    pub accepted: bool,
    pub receipt: String,
    /// Raw JSON exactly as the Editor wrote it; empty when absent.
    #[serde(skip_serializing_if = "Vec::is_empty", serialize_with = "raw_json")]
    pub result: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub backend: String,
    pub pid: i64,
    pub owned: bool,
    pub mailbox: String,
    pub project: String,
    pub editor: String,
    pub unity_version: String,
    pub batch_mode: bool,
    pub is_compiling: bool,
    pub is_updating: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preparation {
    pub backend: String,
    pub pid: i64,
    #[serde(serialize_with = "raw_json")]
    pub graph: Vec<u8>,
    #[serde(serialize_with = "raw_json")]
    pub scene_proof: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct Envelope {
    schema_version: i64,
    id: String,
    project: String,
    unity_version: String,
    editor: String,
    expected_pid: i64,
    action: String,
    arguments: Vec<u8>,
}
impl Envelope {
    fn encode(&self) -> Vec<u8> {
        let arguments = if self.arguments.is_empty() {
            b"null".to_vec()
        } else {
            jsondoc::compact(&self.arguments)
        };
        let mut data = format!(
            "{{\"schemaVersion\":{},\"requestId\":{},\"project\":{},\"unityVersion\":{},\"editor\":{},\"expectedPid\":{},\"action\":{},\"arguments\":",
            self.schema_version,
            jsondoc::quote(&self.id),
            jsondoc::quote(&self.project),
            jsondoc::quote(&self.unity_version),
            jsondoc::quote(&self.editor),
            self.expected_pid,
            jsondoc::quote(&self.action),
        )
        .into_bytes();
        data.extend_from_slice(&arguments);
        data.push(b'}');
        data
    }
}
#[derive(Debug, Clone, Default)]
struct Heartbeat {
    schema_version: i64,
    backend: String,
    project: String,
    unity_version: String,
    editor: String,
    pid: i64,
    owner_token: String,
    batch_mode: bool,
    is_compiling: bool,
    is_updating: bool,
}

// One value, struct rules, then nothing but whitespace.
fn decode<T: Default>(
    data: &[u8],
    target_type: &str,
    fields: &[&str],
    mut set: impl FnMut(&mut T, usize, &Member, Target<'_>) -> std::result::Result<(), String>,
) -> Result<T> {
    let (json, end) = jsondoc::parse_first(data).map_err(Error::new)?;
    let members: &[Member] = match &json {
        Json::Null => &[],
        Json::Object(members) => members,
        other => {
            return Err(Error::new(
                Target::VALUE.mismatch(other.kind(), target_type),
            ));
        }
    };
    let mut value = T::default();
    jsondoc::decode_struct(members, fields, |field, member| {
        let at = Target {
            strukt: target_type,
            field: fields[field],
        };
        set(&mut value, field, member, at)
    })
    .map_err(Error::new)?;
    if !jsondoc::rest_is_space(data, end) {
        return Err(Error::new("trailing Editor artifact JSON"));
    }
    Ok(value)
}
fn decode_envelope(data: &[u8]) -> Result<Envelope> {
    const FIELDS: [&str; 8] = [
        "schemaVersion",
        "requestId",
        "project",
        "unityVersion",
        "editor",
        "expectedPid",
        "action",
        "arguments",
    ];
    decode(
        data,
        "envelope",
        &FIELDS,
        |e: &mut Envelope, field, member, at| {
            let value = &member.value;
            match field {
                0 => jsondoc::set_int(&mut e.schema_version, value, at),
                1 => jsondoc::set_string(&mut e.id, value, at),
                2 => jsondoc::set_string(&mut e.project, value, at),
                3 => jsondoc::set_string(&mut e.unity_version, value, at),
                4 => jsondoc::set_string(&mut e.editor, value, at),
                5 => jsondoc::set_int(&mut e.expected_pid, value, at),
                6 => jsondoc::set_string(&mut e.action, value, at),
                _ => {
                    e.arguments = member.raw(data).to_vec();
                    Ok(())
                }
            }
        },
    )
}
fn decode_heartbeat(data: &[u8]) -> Result<Heartbeat> {
    const FIELDS: [&str; 10] = [
        "schemaVersion",
        "backend",
        "project",
        "unityVersion",
        "editor",
        "pid",
        "ownerToken",
        "batchMode",
        "isCompiling",
        "isUpdating",
    ];
    decode(
        data,
        "heartbeat",
        &FIELDS,
        |h: &mut Heartbeat, field, member, at| {
            let value = &member.value;
            match field {
                0 => jsondoc::set_int(&mut h.schema_version, value, at),
                1 => jsondoc::set_string(&mut h.backend, value, at),
                2 => jsondoc::set_string(&mut h.project, value, at),
                3 => jsondoc::set_string(&mut h.unity_version, value, at),
                4 => jsondoc::set_string(&mut h.editor, value, at),
                5 => jsondoc::set_int(&mut h.pid, value, at),
                6 => jsondoc::set_string(&mut h.owner_token, value, at),
                7 => jsondoc::set_bool(&mut h.batch_mode, value, at),
                8 => jsondoc::set_bool(&mut h.is_compiling, value, at),
                _ => jsondoc::set_bool(&mut h.is_updating, value, at),
            }
        },
    )
}
// The receipt plus the raw text of its exact-case `success` member.
fn decode_response(data: &[u8]) -> Result<(Response, Vec<u8>)> {
    const FIELDS: [&str; 14] = [
        "schemaVersion",
        "backend",
        "requestId",
        "requestSha256",
        "project",
        "unityVersion",
        "editor",
        "pid",
        "status",
        "success",
        "accepted",
        "receipt",
        "result",
        "error",
    ];
    let mut success = Vec::new();
    let response = decode(
        data,
        "Response",
        &FIELDS,
        |r: &mut Response, field, member, at| {
            let value = &member.value;
            if member.key == "success" {
                success = member.raw(data).to_vec();
            }
            match field {
                0 => jsondoc::set_int(&mut r.schema_version, value, at),
                1 => jsondoc::set_string(&mut r.backend, value, at),
                2 => jsondoc::set_string(&mut r.request_id, value, at),
                3 => jsondoc::set_string(&mut r.request_sha256, value, at),
                4 => jsondoc::set_string(&mut r.project, value, at),
                5 => jsondoc::set_string(&mut r.unity_version, value, at),
                6 => jsondoc::set_string(&mut r.editor, value, at),
                7 => jsondoc::set_int(&mut r.pid, value, at),
                8 => jsondoc::set_string(&mut r.status, value, at),
                9 => jsondoc::set_bool(&mut r.success, value, at),
                10 => jsondoc::set_bool(&mut r.accepted, value, at),
                11 => jsondoc::set_string(&mut r.receipt, value, at),
                12 => {
                    r.result = member.raw(data).to_vec();
                    Ok(())
                }
                _ => match value {
                    Json::Null => {
                        r.error = None;
                        Ok(())
                    }
                    Json::Object(members) => {
                        let failure = r.error.get_or_insert_with(Failure::default);
                        jsondoc::decode_struct(members, &["code", "message"], |field, member| {
                            let (target, name) = match field {
                                0 => (&mut failure.code, "error.code"),
                                _ => (&mut failure.message, "error.message"),
                            };
                            let at = Target {
                                strukt: "Failure",
                                field: name,
                            };
                            jsondoc::set_string(target, &member.value, at)
                        })
                    }
                    other => Err(at.mismatch(other.kind(), "Failure")),
                },
            }
        },
    )?;
    Ok((response, success))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Owned {
    id: u64,
    pid: i64,
}
#[derive(Default)]
struct State {
    command: Option<Owned>,
    done: Option<Flag>,
    owner_token: String,
    started: String,
    exit_error: Option<String>,
    closed: bool,
    stop_grace: Duration,
    launched: u64,
}
struct Inner {
    options: Options,
    mailbox: String,
    lifetime: Context,
    start: Mutex<()>,
    state: Mutex<State>,
}
/// Cheap to clone; all clones share one mailbox, lifetime and owned Editor.
#[derive(Clone)]
pub struct Client(Arc<Inner>);

struct StartupLock(File);
impl Drop for StartupLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is open for the lifetime of `self`.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Accepts one exact pinned version line with LF, CRLF, or no final newline.
/// Conflicting, suffixed and duplicate declarations fail.
pub fn validate_project_version(data: &[u8]) -> Result<()> {
    if data.len() > 65536 {
        return Err(Error::new("project version file exceeds its byte bound"));
    }
    let mut count = 0;
    for line in String::from_utf8_lossy(data).split('\n') {
        let line = line.trim();
        if !line.starts_with("m_EditorVersion:") {
            continue;
        }
        count += 1;
        if line != format!("m_EditorVersion: {PROFILE}") {
            return Err(Error::new(format!(
                "project requires the verified Unity {PROFILE} version line"
            )));
        }
    }
    if count != 1 {
        return Err(Error::new(
            "project version file requires one unambiguous Editor version declaration",
        ));
    }
    Ok(())
}

impl Client {
    pub fn new(ctx: &Context, mut options: Options) -> Result<Client> {
        if !sysutil::is_abs(&options.project) || !sysutil::is_abs(&options.editor) {
            return Err(Error::new(
                "Editor bridge requires an explicit absolute project and Editor executable",
            ));
        }
        options.project = sysutil::eval_symlinks(&options.project)?;
        options.editor = sysutil::eval_symlinks(&options.editor)?;
        if !sysutil::stat(&options.project).is_ok_and(|info| info.is_dir()) {
            return Err(Error::new("Editor project does not exist"));
        }
        if !sysutil::stat(&options.editor)
            .is_ok_and(|info| info.is_regular() && info.mode & 0o111 != 0)
        {
            return Err(Error::new("Editor must be an executable regular file"));
        }
        let version = read_path(
            &sysutil::join(&[&options.project, "ProjectSettings/ProjectVersion.txt"]),
            65536,
        );
        if !version.is_ok_and(|version| validate_project_version(&version).is_ok()) {
            return Err(Error::new(format!(
                "Editor bridge requires project profile Unity {PROFILE}"
            )));
        }
        if options.startup_timeout.is_zero() {
            options.startup_timeout = Duration::from_secs(180);
        }
        if options.startup_timeout > Duration::from_secs(15 * 60) {
            return Err(Error::new(
                "Editor startup timeout must be positive and at most 15 minutes",
            ));
        }
        if options.poll_interval.is_zero() {
            options.poll_interval = Duration::from_millis(50);
        }
        if options.poll_interval < Duration::from_millis(1)
            || options.poll_interval > Duration::from_secs(1)
        {
            return Err(Error::new(
                "Editor poll interval must be between 1 ms and 1 second",
            ));
        }
        validate_editor_executable(ctx, &options.editor)?;
        let mailbox = sysutil::join(&[&options.project, "Library/UnityMicroEditor/editor-bridge"]);
        canonical(&mailbox)?;
        sysutil::mkdir_all(&mailbox)?;
        let lifetime = ctx.with_cancel();
        let client = Client(Arc::new(Inner {
            options,
            mailbox,
            lifetime: lifetime.clone(),
            start: Mutex::new(()),
            state: Mutex::new(State {
                stop_grace: Duration::from_secs(10),
                ..State::default()
            }),
        }));
        let watcher = client.clone();
        thread::spawn(move || {
            lifetime.wait();
            let _ = watcher.close();
        });
        Ok(client)
    }

    pub fn mailbox(&self) -> &str {
        &self.0.mailbox
    }
    /// Options after validation: resolved paths and effective durations.
    pub fn options(&self) -> &Options {
        &self.0.options
    }

    /// Never starts a second Editor for an already open project.
    /// It never owns or terminates a discovered interactive Editor.
    pub fn own_process_or_attach(&self, ctx: &Context) -> Result<Connection> {
        let _starting = self.0.start.lock().unwrap();
        if let Some(error) = self.0.lifetime.error() {
            return Err(error.into());
        }
        if let Some(error) = ctx.error() {
            return Err(error.into());
        }
        let _lock = self.startup_lock(ctx)?;
        let deadline = Instant::now() + self.0.options.startup_timeout;
        let log = sysutil::join(&[&self.0.mailbox, "editor.log"]);
        let mut started_here = false;
        loop {
            if let Ok(connected) = self.read_connection() {
                return Ok(connected);
            }
            let pid = self.find_editor()?;
            let (command, done, exit_error) = {
                let state = self.0.state.lock().unwrap();
                (
                    state.command,
                    state.done.clone(),
                    state.exit_error.clone().unwrap_or("<nil>".into()),
                )
            };
            if pid == 0 && command.is_none() {
                if started_here {
                    return Err(Error::new(format!(
                        "owned batch Editor exited before bridge readiness: {exit_error}; inspect {log}"
                    )));
                }
                self.start_batch()?;
                started_here = true;
            } else if command.is_some() && done.is_some_and(|done| done.is_set()) && started_here {
                return Err(Error::new(format!(
                    "owned batch Editor exited before readiness: {exit_error}"
                )));
            }
            if Instant::now() > deadline {
                return Err(Error::new(format!(
                    "Editor bridge unavailable for project (Editor PID {pid}); install the runtime package and inspect compilation errors; log {log}"
                )));
            }
            self.wait(ctx)?;
        }
    }

    // Serializes discovery/startup across separate MCP or CLI clients. Unity's own
    // project lock remains authoritative; this prevents racing two owned starts.
    fn startup_lock(&self, ctx: &Context) -> Result<StartupLock> {
        let root = self.open_root()?;
        let file = root.open_file(
            "startup.lock",
            libc::O_CREAT | libc::O_RDWR | libc::O_NONBLOCK,
            0o600,
        )?;
        if !sysutil::fstat(&file).is_ok_and(|info| info.is_regular()) {
            return Err(Error::new("Editor startup lock is not a regular file"));
        }
        loop {
            // SAFETY: the descriptor is open for the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(StartupLock(file));
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EWOULDBLOCK) => self.wait(ctx)?,
                Some(libc::EINTR) => {}
                _ => return Err(Error::new(sysutil::errno_text(&error))),
            }
        }
    }

    fn read_connection(&self) -> Result<Connection> {
        let root = self.open_root()?;
        let data = read(&root, "ready.json", 65536)?;
        let options = &self.0.options;
        let heartbeat = decode_heartbeat(&data).ok().filter(|h| {
            h.schema_version == 1
                && h.backend == "unity_editor"
                && h.project == options.project
                && h.editor == options.editor
                && h.unity_version == PROFILE
                && h.pid >= 1
        });
        let Some(h) = heartbeat else {
            return Err(Error::new("Editor heartbeat has incompatible identity"));
        };
        if !self.belongs(h.pid) {
            return Err(Error::new(
                "Editor heartbeat PID does not belong to this project and executable",
            ));
        }
        let owned = {
            let state = self.0.state.lock().unwrap();
            state.command.is_some_and(|command| command.pid == h.pid)
                && state.owner_token == h.owner_token
        };
        Ok(Connection {
            backend: "unity_editor".into(),
            pid: h.pid,
            owned,
            mailbox: self.0.mailbox.clone(),
            project: h.project,
            editor: h.editor,
            unity_version: h.unity_version,
            batch_mode: h.batch_mode,
            is_compiling: h.is_compiling,
            is_updating: h.is_updating,
        })
    }

    fn start_batch(&self) -> Result<()> {
        let inner = &self.0;
        if let Some(error) = inner.lifetime.error() {
            return Err(error.into());
        }
        validate_editor_executable(&inner.lifetime, &inner.options.editor)?;
        let log = sysutil::join(&[&inner.mailbox, "editor.log"]);
        canonical(&log)?;
        let owner = sysutil::new_id();
        let mut command = Command::new(&inner.options.editor);
        command
            .args(["-batchmode", "-nographics", "-projectPath"])
            .arg(&inner.options.project)
            .args(["-buildTarget", "Linux64", "-executeMethod"])
            .arg("UnityMicroEditor.Editor.AgentEditorBridge.RunBatch")
            .arg("-logFile")
            .arg(&log)
            .env("UNITY_MICRO_EDITOR_BRIDGE_OWNER", &owner)
            .process_group(0)
            // Unity logs to its owned file. Avoid an unread stdout pipe blocking startup.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut state = inner.state.lock().unwrap();
        if state.closed || inner.lifetime.error().is_some() {
            return Err(Error::new("Editor client is closed"));
        }
        if state.command.is_some() {
            return Ok(());
        }
        let mut child = command
            .spawn()
            .map_err(|error| Error::os("fork/exec", &inner.options.editor, &error))?;
        let pid = i64::from(child.id());
        let started = match process(pid) {
            Ok(process) => process.started,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        state.launched += 1;
        let owned = Owned {
            id: state.launched,
            pid,
        };
        let done = Flag::new();
        state.command = Some(owned);
        state.owner_token = owner;
        state.started = started;
        state.exit_error = None;
        state.done = Some(done.clone());
        let client = self.clone();
        thread::spawn(move || {
            let exit = match child.wait() {
                Ok(status) => sysutil::exit_error(status),
                Err(error) => Some(format!("wait: {}", sysutil::errno_text(&error))),
            };
            let mut state = client.0.state.lock().unwrap();
            if state.command == Some(owned) {
                state.command = None;
                state.exit_error = exit;
            }
            done.set();
        });
        Ok(())
    }

    /// Delivers one immutable request and waits for its receipt. A request that
    /// was delivered but is not confirmed returns `Ok` with status
    /// `result_pending` (waiting ended) or `result_unknown` (Editor gone).
    pub fn do_request(&self, ctx: &Context, mut request: Request) -> Result<Response> {
        if let Some(error) = ctx.error() {
            return Err(error.into());
        }
        if request.id.is_empty() {
            request.id = sysutil::new_id();
        }
        if !sysutil::valid_id(&request.id) || !allowed(&request.action) || request.expected_pid < 0
        {
            return Err(Error::new(
                "Editor request requires a bounded immutable ID, allowlisted action and nonnegative expected PID",
            ));
        }
        request.arguments = canonical_arguments(&request.arguments)?;
        if request.action == "prefab_prepare" || request.action == "native_prepare" {
            self.validate_staging()?;
        }
        let options = &self.0.options;
        let root = self.open_root()?;
        let request_name = format!("{}.request.json", request.id);
        let response_name = format!("{}.response.json", request.id);
        let accepted_name = format!("{}.accepted.json", request.id);
        let (existing, mut accepted) = existing_envelope(&root, &request.id)?;
        let (data, original) = match existing {
            Some(data) => {
                let original = decode_envelope(&data)?;
                self.same_request(&original, &request)?;
                (data, original)
            }
            None => {
                let connection = self.own_process_or_attach(ctx)?;
                let pid = match request.expected_pid {
                    0 => connection.pid,
                    pid => pid,
                };
                if pid != connection.pid {
                    return Err(Error::new(format!(
                        "Editor request targets stale PID {pid}; current PID {}",
                        connection.pid
                    )));
                }
                let original = Envelope {
                    schema_version: 1,
                    id: request.id.clone(),
                    project: options.project.clone(),
                    unity_version: PROFILE.into(),
                    editor: options.editor.clone(),
                    expected_pid: pid,
                    action: request.action.clone(),
                    arguments: request.arguments.clone(),
                };
                let mut data = original.encode();
                data.push(b'\n');
                if data.len() > REQUEST_LIMIT {
                    return Err(Error::new("Editor request exceeds byte bound"));
                }
                match publish(&root, &request_name, &data) {
                    Ok(()) => (data, original),
                    Err(error) if !error.is_exist() => return Err(error),
                    Err(_) => {
                        let (existing, claimed) = existing_envelope(&root, &request.id)?;
                        accepted = claimed;
                        let Some(data) = existing else {
                            return Err(Error::new(
                                "concurrent Editor request publication lost immutable envelope",
                            ));
                        };
                        let original = decode_envelope(&data)?;
                        self.same_request(&original, &request)?;
                        (data, original)
                    }
                }
            }
        };
        let hash = crate::sha256(&data);
        if accepted && self.belongs(original.expected_pid) {
            // Ask the current Editor to replay an unavailable receipt. Its accepted
            // marker prevents executing the action again, even across domain reload.
            if read(&root, &response_name, RESPONSE_LIMIT).is_err_and(|e| e.is_not_exist())
                && let Err(error) = publish(&root, &request_name, &data)
                && !error.is_exist()
            {
                return Err(error);
            }
        }
        loop {
            match self.read_response(&root, &original, &hash) {
                Ok(response) => return Ok(response),
                Err(error) if !error.is_not_exist() => return Err(error),
                Err(_) => {}
            }
            match read(&root, &accepted_name, REQUEST_LIMIT) {
                Ok(claim) => {
                    if claim != data {
                        return Err(Error::new(
                            "Editor accepted marker differs from immutable request",
                        ));
                    }
                    accepted = true;
                }
                Err(error) if !error.is_not_exist() => return Err(error),
                Err(_) => {}
            }
            if !self.belongs(original.expected_pid) {
                return Ok(self.uncertain(
                    &original,
                    &hash,
                    accepted,
                    "result_unknown",
                    "The original Editor process is unavailable; the immutable request will not be reexecuted.",
                ));
            }
            if self.wait(ctx).is_err() {
                return Ok(self.uncertain(
                    &original,
                    &hash,
                    accepted,
                    "result_pending",
                    "Waiting ended; the delivered request may still complete. Retry the same request ID to read its receipt.",
                ));
            }
        }
    }

    fn receipt(&self, id: &str) -> String {
        sysutil::join(&[&self.0.mailbox, &format!("{id}.response.json")])
    }
    fn read_response(&self, root: &Root, request: &Envelope, hash: &str) -> Result<Response> {
        let data = read(
            root,
            &format!("{}.response.json", request.id),
            RESPONSE_LIMIT,
        )?;
        let (mut response, success) = decode_response(&data)?;
        let options = &self.0.options;
        if response.schema_version != 1
            || response.backend != "unity_editor"
            || response.request_id != request.id
            || response.request_sha256 != hash
            || response.project != options.project
            || response.editor != options.editor
            || response.unity_version != PROFILE
            || response.status != "completed"
            || response.pid < 1
            || success != b"true" && success != b"false"
            || response.success && response.pid != request.expected_pid
            || !response.success && response.error.is_none()
        {
            return Err(Error::new(
                "Editor receipt identity, bounds or result is incompatible",
            ));
        }
        response.accepted = true;
        response.receipt = self.receipt(&request.id);
        Ok(response)
    }
    fn same_request(&self, original: &Envelope, request: &Request) -> Result<()> {
        let arguments = canonical_arguments(&original.arguments)?;
        let options = &self.0.options;
        if original.schema_version != 1
            || original.id != request.id
            || original.project != options.project
            || original.editor != options.editor
            || original.unity_version != PROFILE
            || original.action != request.action
            || original.expected_pid < 1
            || request.expected_pid != 0 && request.expected_pid != original.expected_pid
            || arguments != request.arguments
        {
            return Err(Error::new(
                "Editor request ID is already bound to different arguments or process guards",
            ));
        }
        Ok(())
    }
    fn uncertain(
        &self,
        request: &Envelope,
        hash: &str,
        accepted: bool,
        status: &str,
        message: &str,
    ) -> Response {
        Response {
            schema_version: 1,
            backend: "unity_editor".into(),
            request_id: request.id.clone(),
            request_sha256: hash.into(),
            project: self.0.options.project.clone(),
            unity_version: PROFILE.into(),
            editor: self.0.options.editor.clone(),
            pid: request.expected_pid,
            status: status.into(),
            success: false,
            accepted,
            receipt: self.receipt(&request.id),
            result: Vec::new(),
            error: Some(Failure {
                code: status.into(),
                message: message.into(),
            }),
        }
    }

    /// Exports refreshed graph and saved-scene proof through the same real Editor.
    pub fn prepare(&self, ctx: &Context, scene: &str) -> Result<Preparation> {
        let connection = self.own_process_or_attach(ctx)?;
        let mut preparation = Preparation {
            backend: "unity_editor".into(),
            pid: connection.pid,
            ..Preparation::default()
        };
        for action in ["refresh", "export_graph", "scene_proof"] {
            let arguments = match action {
                "scene_proof" => format!("{{\"scene\":{}}}", jsondoc::quote(scene)).into_bytes(),
                _ => b"{}".to_vec(),
            };
            let response = self.do_request(
                ctx,
                Request {
                    id: String::new(),
                    action: action.into(),
                    arguments,
                    expected_pid: connection.pid,
                },
            )?;
            if !response.success {
                let message = match &response.error {
                    Some(failure) => &failure.message,
                    None => &response.status,
                };
                return Err(Error::new(format!(
                    "Editor {action}: {message}; request {} ({})",
                    response.request_id, response.status
                )));
            }
            match action {
                "export_graph" => preparation.graph = response.result,
                "scene_proof" => preparation.scene_proof = response.result,
                _ => {}
            }
        }
        Ok(preparation)
    }

    /// Publishes JSON only into owned project caches, without overwriting.
    pub fn save_result(&self, response: &Response, path: &str) -> Result<()> {
        if !response.success
            || response.status != "completed"
            || !jsondoc::valid(&response.result)
            || !sysutil::is_owned_cache_path(&self.0.options.project, path)
        {
            return Err(Error::new(
                "only successful Editor results may be saved to a new owned cache file",
            ));
        }
        canonical(path)?;
        let directory = sysutil::dir(path);
        sysutil::mkdir_all(&directory)?;
        let root = Root::open(&directory)?;
        let mut data = response.result.clone();
        data.push(b'\n');
        publish(&root, &sysutil::base(path), &data)
    }

    /// Stops the owned batch Editor, if any, and ends the client lifetime. An
    /// attached Editor is never stopped. Safe to call repeatedly.
    pub fn close(&self) -> Result<()> {
        let inner = &self.0;
        let mut state = inner.state.lock().unwrap();
        if state.closed {
            let done = state.done.clone();
            drop(state);
            if let Some(done) = done
                && !done.wait(Some(Duration::from_secs(20)))
            {
                return Err(Error::new("owned Editor close timed out"));
            }
            return Ok(());
        }
        state.closed = true;
        inner.lifetime.cancel();
        let (command, done) = (state.command, state.done.clone());
        let (started, grace, owner) = (
            state.started.clone(),
            state.stop_grace,
            state.owner_token.clone(),
        );
        drop(state);
        let (Some(command), Some(done)) = (command, done) else {
            return Ok(());
        };
        let signal = |signal: libc::c_int| {
            if process(command.pid).is_ok_and(|now| now.started == started) {
                let _ = sysutil::kill(-command.pid, signal);
            }
        };
        // Ask the owned batch Editor to exit through its public update loop.
        // Unix SIGINT can crash Unity's native shutdown path on Linux; retain
        // bounded signals only if the Editor cannot process its mailbox.
        if let Ok(root) = self.open_root() {
            let body = format!(
                "{{\"ownerToken\":{},\"pid\":{},\"project\":{}}}\n",
                jsondoc::quote(&owner),
                command.pid,
                jsondoc::quote(&inner.options.project)
            );
            let _ = publish(&root, &format!("owner-{owner}.stop.json"), body.as_bytes());
        }
        if done.wait(Some(grace)) {
            return Ok(());
        }
        signal(libc::SIGTERM);
        if done.wait(Some(Duration::from_secs(3))) {
            return Ok(());
        }
        signal(libc::SIGKILL);
        if done.wait(Some(Duration::from_secs(3))) {
            return Ok(());
        }
        Err(Error::new(
            "owned Editor did not exit after bounded cleanup",
        ))
    }

    fn wait(&self, ctx: &Context) -> Result<()> {
        let lifetime = &self.0.lifetime;
        let sources = [Source::Context(ctx), Source::Context(lifetime)];
        match select(&sources, Some(self.0.options.poll_interval)) {
            None => Ok(()),
            Some(0) => Err(ctx.error().expect("selected context is done").into()),
            Some(_) => Err(lifetime.error().expect("selected context is done").into()),
        }
    }
    fn open_root(&self) -> Result<Root> {
        canonical(&self.0.mailbox)?;
        Root::open(&self.0.mailbox)
    }
    fn validate_staging(&self) -> Result<()> {
        let project = &self.0.options.project;
        let Ok(data) = read_path(
            &sysutil::join(&[project, "ProjectSettings/UnityMicroEditorStaging.json"]),
            65536,
        ) else {
            return Err(Error::new(
                "Editor preparation requires an isolated staging project marker",
            ));
        };
        let marker = decode(
            &data,
            "",
            &["schemaVersion", "sourceProject"],
            |marker: &mut (i64, String), field, member, at| match field {
                0 => jsondoc::set_int(&mut marker.0, &member.value, at),
                _ => jsondoc::set_string(&mut marker.1, &member.value, at),
            },
        );
        let source = match marker {
            Ok((1, source))
                if sysutil::is_abs(&source)
                    && sysutil::clean(&source) == source
                    && source != *project =>
            {
                source
            }
            _ => {
                return Err(Error::new("Editor preparation staging marker is invalid"));
            }
        };
        canonical(&source)?;
        if !sysutil::stat(&source).is_ok_and(|info| info.is_dir()) {
            return Err(Error::new("Editor preparation source project is absent"));
        }
        Ok(())
    }
    fn belongs(&self, pid: i64) -> bool {
        let Ok(process) = process(pid) else {
            return false;
        };
        if import_worker(&process.args) {
            return false;
        }
        let owned = {
            let state = self.0.state.lock().unwrap();
            state.command.is_some_and(|command| command.pid == pid)
                && state.started == process.started
        };
        if !owned {
            let executable = std::fs::read_link(format!("/proc/{pid}/exe"));
            if !executable
                .is_ok_and(|path| path.as_os_str().as_bytes() == self.0.options.editor.as_bytes())
            {
                return false;
            }
        }
        self.same_project(argument(&process.args, "-projectPath"))
    }
    // Canonicalization resolves the argument; a relative result never equals the
    // absolute project.
    fn same_project(&self, project: &[u8]) -> bool {
        project.first() == Some(&b'/')
            && std::fs::canonicalize(sysutil::os_str(project))
                .is_ok_and(|path| path.as_os_str().as_bytes() == self.0.options.project.as_bytes())
    }
    fn find_editor(&self) -> Result<i64> {
        let entries =
            std::fs::read_dir("/proc").map_err(|error| Error::os("open", "/proc", &error))?;
        let mut names: Vec<Vec<u8>> = entries
            .filter_map(|entry| Some(entry.ok()?.file_name().as_bytes().to_vec()))
            .collect();
        names.sort();
        for name in names {
            let Some(pid) = std::str::from_utf8(&name)
                .ok()
                .and_then(|name| name.parse::<i64>().ok())
            else {
                continue;
            };
            let Ok(process) = process(pid) else {
                continue;
            };
            if import_worker(&process.args)
                || !self.same_project(argument(&process.args, "-projectPath"))
            {
                continue;
            }
            if self.belongs(pid) {
                return Ok(pid);
            }
            return Err(Error::new(format!(
                "project already has another Editor/runtime executable (PID {pid}); refusing a second Editor"
            )));
        }
        Ok(0)
    }
}

fn validate_editor_executable(ctx: &Context, editor: &str) -> Result<()> {
    // A version-only invocation opens no project and acquires no project lock.
    // Validate before package installation can change authored inputs. Discard
    // excess output while retaining a strict bound even for a bad executable.
    let mismatch = format!("Editor executable does not report pinned Unity {PROFILE}");
    let version = ctx.with_timeout(Duration::from_secs(15));
    let outcome = run_version(&version, editor);
    version.cancel();
    let (output, overflow) = outcome.map_err(|error| Error::wrap(&mismatch, &error))?;
    if overflow || String::from_utf8_lossy(&output).trim() != PROFILE {
        return Err(Error::new(mismatch));
    }
    Ok(())
}
// Runs `editor -version` under `ctx`, waiting up to one second for output after the
// process ends or is killed, and keeps at most 4096 bytes of combined output.
fn run_version(ctx: &Context, editor: &str) -> Result<(Vec<u8>, bool)> {
    const WAIT_DELAY: Duration = Duration::from_secs(1);
    if let Some(error) = ctx.error() {
        return Err(error.into());
    }
    let failed = |error: io::Error| Error::os("fork/exec", editor, &error);
    let (mut reader, writer) = io::pipe().map_err(failed)?;
    let mut command = Command::new(editor);
    command
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(writer.try_clone().map_err(failed)?)
        .stderr(writer);
    let spawned = command.spawn();
    // The parent copies of the write end must close for the pipe to report EOF.
    drop(command);
    let mut child = spawned.map_err(failed)?;
    let (mut output, mut overflow, mut eof) = (Vec::new(), false, false);
    let (mut status, mut interrupted, mut io_deadline) = (None, None, None::<Instant>);
    let mut killed = false;
    let mut chunk = [0u8; 4096];
    loop {
        let now = Instant::now();
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| Error::new(format!("wait: {}", sysutil::errno_text(&error))))?;
            if status.is_some() {
                io_deadline.get_or_insert(now + WAIT_DELAY);
            } else if !killed && let Some(error) = ctx.error() {
                killed = true;
                if child.kill().is_ok() {
                    interrupted = Some(error);
                }
                io_deadline.get_or_insert(now + WAIT_DELAY);
            }
        }
        let expired = io_deadline.is_some_and(|deadline| now >= deadline);
        if status.is_some() && (eof || expired) {
            break;
        }
        if eof || expired {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        let mut poll = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` points to one initialized descriptor record.
        if unsafe { libc::poll(&mut poll, 1, 5) } <= 0 {
            continue;
        }
        match reader.read(&mut chunk) {
            Ok(0) => eof = true,
            Ok(count) => {
                let remaining = 4096 - output.len();
                overflow |= count > remaining;
                output.extend_from_slice(&chunk[..count.min(remaining)]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => eof = true,
        }
    }
    if let Some(message) = status.and_then(sysutil::exit_error) {
        return Err(Error::new(message));
    }
    if let Some(error) = interrupted {
        return Err(error.into());
    }
    if !eof {
        return Err(Error::new("output did not close after the process ended"));
    }
    Ok((output, overflow))
}

struct Process {
    started: String,
    args: Vec<Vec<u8>>,
}
fn process(pid: i64) -> Result<Process> {
    if pid < 1 {
        return Err(Error::new("invalid process ID"));
    }
    let path = format!("/proc/{pid}/stat");
    let data = std::fs::read(&path).map_err(|error| Error::os("open", &path, &error))?;
    let (started, _parent) = sysutil::parse_proc_stat(&data).map_err(Error::new)?;
    let mut data = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .filter(|data| data.len() <= 65536)
        .ok_or_else(|| Error::new("process arguments unavailable"))?;
    while data.last() == Some(&0) {
        data.pop();
    }
    Ok(Process {
        started,
        args: data.split(|&byte| byte == 0).map(<[u8]>::to_vec).collect(),
    })
}
fn import_worker(args: &[Vec<u8>]) -> bool {
    argument(args, "-name").starts_with(b"AssetImportWorker")
        && !argument(args, "-parentPid").is_empty()
}
fn argument<'a>(args: &'a [Vec<u8>], name: &str) -> &'a [u8] {
    let name = name.as_bytes();
    for (index, arg) in args.iter().enumerate() {
        if arg.eq_ignore_ascii_case(name) && index + 1 < args.len() {
            return &args[index + 1];
        }
        if arg.len() > name.len() + 1
            && arg[..name.len()].eq_ignore_ascii_case(name)
            && arg[name.len()] == b'='
        {
            return &arg[name.len() + 1..];
        }
    }
    b""
}
fn existing_envelope(root: &Root, id: &str) -> Result<(Option<Vec<u8>>, bool)> {
    for suffix in [".accepted.json", ".request.json"] {
        match read(root, &format!("{id}{suffix}"), REQUEST_LIMIT) {
            Ok(data) => return Ok((Some(data), suffix == ".accepted.json")),
            Err(error) if !error.is_not_exist() => return Err(error),
            Err(_) => {}
        }
    }
    Ok((None, false))
}
fn read_path(path: &str, limit: usize) -> Result<Vec<u8>> {
    canonical(path)?;
    let root = Root::open(&sysutil::dir(path))?;
    read(&root, &sysutil::base(path), limit)
}
fn read(root: &Root, name: &str, limit: usize) -> Result<Vec<u8>> {
    let file = root.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    if !sysutil::fstat(&file).is_ok_and(|info| info.is_regular() && info.size <= limit as i64) {
        return Err(Error::new("Editor artifact is not a bounded regular file"));
    }
    let mut data = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|error| Error::os("read", name, &error))?;
    if data.len() > limit {
        return Err(Error::new("Editor artifact exceeds byte bound"));
    }
    Ok(data)
}
fn publish(root: &Root, name: &str, data: &[u8]) -> Result<()> {
    let temporary = format!(".publish-{}", sysutil::new_id());
    let mut file = root.open_file(
        &temporary,
        libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
        0o600,
    )?;
    let written = file
        .write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|error| Error::os("write", &temporary, &error));
    drop(file);
    let linked = written.and_then(|()| root.link(&temporary, name));
    let _ = root.remove(&temporary);
    linked
}
fn canonical(path: &str) -> Result<()> {
    if !sysutil::is_abs(path) || sysutil::clean(path) != path {
        return Err(Error::new("Editor path must be absolute and canonical"));
    }
    let mut current = path.to_string();
    loop {
        match sysutil::lstat(&current) {
            Ok(info) if info.is_symlink() => {
                return Err(Error::new("Editor paths cannot contain symlinks"));
            }
            Err(error) if error.raw_os_error() != Some(libc::ENOENT) => {
                return Err(Error::os("lstat", &current, &error));
            }
            _ => {}
        }
        let parent = sysutil::dir(&current);
        if parent == current {
            return Ok(());
        }
        current = parent;
    }
}
// Canonical arguments are a JSON object without duplicate keys, re-encoded the
// canonical way: sorted keys, verbatim numbers, HTML escapes, and an empty array
// as `null`.
fn canonical_arguments(data: &[u8]) -> Result<Vec<u8>> {
    let data: &[u8] = if data.is_empty() { b"{}" } else { data };
    if data.len() > REQUEST_LIMIT {
        return Err(Error::new("Editor arguments exceed byte bound"));
    }
    let rules = jsondoc::TokenRules {
        max_depth: 128,
        nesting_error: "JSON nesting exceeds bounds",
        duplicate_error: "duplicate JSON argument key",
    };
    let (value, end) = jsondoc::parse_tokens(data, rules).map_err(Error::new)?;
    let mut result = Vec::new();
    encode_canonical(&value, &mut result);
    if !matches!(value, Json::Object(_)) {
        return Err(Error::new("Editor arguments must be a JSON object"));
    }
    if !jsondoc::rest_is_space(data, end) {
        return Err(Error::new("trailing Editor argument JSON"));
    }
    Ok(result)
}
fn encode_canonical(value: &Json, result: &mut Vec<u8>) {
    match value {
        Json::Null => result.extend_from_slice(b"null"),
        Json::Bool(flag) => result.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Json::Number(text) => result.extend_from_slice(text.as_bytes()),
        Json::String(text) => result.extend_from_slice(jsondoc::quote(text).as_bytes()),
        Json::Array(items) if items.is_empty() => result.extend_from_slice(b"null"),
        Json::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                result.push(if index == 0 { b'[' } else { b',' });
                encode_canonical(item, result);
            }
            result.push(b']');
        }
        Json::Object(members) => {
            let mut sorted: Vec<&Member> = members.iter().collect();
            sorted.sort_by(|a, b| a.key.cmp(&b.key));
            result.push(b'{');
            for (index, member) in sorted.iter().enumerate() {
                if index != 0 {
                    result.push(b',');
                }
                result.extend_from_slice(jsondoc::quote(&member.key).as_bytes());
                result.push(b':');
                encode_canonical(&member.value, result);
            }
            result.push(b'}');
        }
    }
}
fn allowed(action: &str) -> bool {
    matches!(
        action,
        "status"
            | "export_graph"
            | "scene_proof"
            | "refresh"
            | "build_project"
            | "build_host"
            | "prefab_prepare"
            | "component_schema"
            | "native_prepare"
    )
}

#[cfg(test)]
mod tests;
