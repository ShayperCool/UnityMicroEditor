//! One native storage service in an isolated scratch directory.
use super::support::{
    abs, context_error, join, mkdir_all, os_error, read_limited, stat, wrap, write_file,
};
use crate::{
    cancel::{self, Context, Flag, Source},
    engine::DataStoreProtocol,
    sysutil,
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::{
        fs::OpenOptionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// What waiting for a child leaves behind: the exit status and the peak RSS of the child.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Exit {
    pub status: ExitStatus,
    pub max_rss_kib: i64,
}
impl Exit {
    /// The exit-status error text; `None` for a clean exit.
    pub fn error(&self) -> Option<String> {
        sysutil::exit_error(self.status)
    }
}
/// The thread that waits for the child and raises the `done` flag.
pub(crate) struct Waiter {
    pub pid: i32,
    pub done: Flag,
    result: Arc<Mutex<Option<io::Result<Exit>>>>,
}
impl Waiter {
    pub fn new(child: Child) -> Self {
        let pid = child.id() as i32;
        let (done, result) = (Flag::new(), Arc::new(Mutex::new(None)));
        let (flag, slot) = (done.clone(), result.clone());
        std::thread::spawn(move || {
            // The child is reaped here, with its resource usage; the handle only
            // keeps the standard streams alive until then.
            let _child = child;
            let outcome = loop {
                let mut status = 0;
                // SAFETY: an all-zero `rusage` is valid and the kernel fills it.
                let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
                // SAFETY: both out-pointers are valid for the duration of the call.
                let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
                if reaped == pid {
                    break Ok(Exit {
                        status: ExitStatus::from_raw(status),
                        max_rss_kib: usage.ru_maxrss,
                    });
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    break Err(error);
                }
            };
            *slot.lock().unwrap() = Some(outcome);
            flag.set();
        });
        Self { pid, done, result }
    }
    pub fn exit(&self) -> Option<Exit> {
        match &*self.result.lock().unwrap() {
            Some(Ok(exit)) => Some(*exit),
            _ => None,
        }
    }
    /// The text of the exit error once the process is gone; `None` for
    /// a clean exit or while it still runs.
    pub fn exit_error(&self) -> Option<String> {
        match &*self.result.lock().unwrap() {
            Some(Ok(exit)) => exit.error(),
            Some(Err(error)) => Some(format!("wait: {}", sysutil::errno_text(error))),
            None => None,
        }
    }
    pub fn finished(&self) -> bool {
        self.result.lock().unwrap().is_some()
    }
    /// Sends a signal to the child's whole process group.
    pub fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        sysutil::kill(-i64::from(self.pid), signal)
    }
}

/// Owns one native storage service. Stop attached import workers before
/// stopping the service.
pub struct DataStoreService {
    protocol: &'static dyn DataStoreProtocol,
    pub editor: String,
    pub context: String,
    pub logs: String,
    pub namespace: String,
    pub pid: i32,

    process: Option<Waiter>,
    stdout: Option<File>,
    started: bool,
    forced: bool,
    ready_time: Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DataStoreLogEvidence {
    pub initialized: bool,
    pub service: bool,
    pub application_started: bool,
    pub shutdown_logged: bool,
    pub startup_error_logged: bool,
    pub ready_logged: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataStoreStatus {
    pub pid: i32,
    pub running: bool,
    pub ready: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub exit_error: String,
    pub ready_seconds: super::JsonF64,
    pub forced_stop: bool,
    pub service_log: String,
    pub stdout_log: String,
    pub log_evidence: DataStoreLogEvidence,
}

pub fn read_data_store_log(
    protocol: &dyn DataStoreProtocol,
    path: &str,
) -> Result<DataStoreLogEvidence> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DataStoreLogEvidence::default());
        }
        Err(error) => return Err(os_error("open", path, &error)),
    };
    let text = read_limited(file, 128 * 1024, path)?;
    Ok(protocol.log_evidence(&text))
}

impl DataStoreService {
    /// The service of a release that keeps its artifacts in one
    /// (`Engine::data_store`).
    pub fn new(
        protocol: &'static dyn DataStoreProtocol,
        editor: &str,
        context: &str,
        logs: &str,
    ) -> Self {
        Self {
            protocol,
            editor: editor.into(),
            context: context.into(),
            logs: logs.into(),
            namespace: String::new(),
            pid: 0,
            process: None,
            stdout: None,
            started: false,
            forced: false,
            ready_time: Duration::ZERO,
        }
    }
    pub(crate) fn service_log(&self) -> String {
        sysutil::join(&[&self.logs, "datastore-service.log"])
    }
    fn stdout_log(&self) -> String {
        sysutil::join(&[&self.logs, "datastore-service-stdout.log"])
    }
    pub fn worker_arguments(&self) -> Vec<String> {
        self.protocol.worker_arguments(&self.namespace)
    }
    pub(crate) fn arguments(&self) -> Vec<String> {
        self.protocol
            .arguments(&self.namespace, &self.service_log())
    }

    pub fn status(&self) -> Result<DataStoreStatus> {
        let evidence = read_data_store_log(self.protocol, &self.service_log())?;
        let running = self.process.as_ref().is_some_and(|p| !p.finished());
        Ok(DataStoreStatus {
            pid: self.pid,
            running,
            ready: running && evidence.ready_logged,
            exit_error: self
                .process
                .as_ref()
                .and_then(Waiter::exit_error)
                .unwrap_or_default(),
            ready_seconds: super::JsonF64(self.ready_time.as_secs_f64()),
            forced_stop: self.forced,
            service_log: self.service_log(),
            stdout_log: self.stdout_log(),
            log_evidence: evidence,
        })
    }

    pub fn start(&mut self, ctx: &Context) -> Result<()> {
        if self.started {
            bail!("DataStoreService instances can only be started once");
        }
        if let Some(error) = context_error(ctx) {
            return Err(error);
        }
        self.started = true;
        self.context = abs(&self.context)?;
        self.logs = abs(&self.logs)?;
        let editor = abs(&self.editor)?;
        let info = stat(&editor).map_err(|e| wrap("Unity installation", e))?;
        let installation = if info.is_dir() {
            editor
        } else {
            sysutil::dir(&editor)
        };
        let executable = self.protocol.executable(&installation);
        let info = stat(&executable).map_err(|e| wrap("Unity.DataStore executable", e))?;
        if !info.is_regular() || info.mode & 0o111 == 0 {
            bail!("Unity.DataStore must be a regular executable");
        }
        self.namespace = sysutil::new_id();
        mkdir_all(
            &sysutil::join(&[&self.context, "Library", "DataStore"]),
            0o755,
        )?;
        mkdir_all(&self.logs, 0o755)?;
        // Stale startup markers cannot satisfy readiness for this process.
        write_file(&self.service_log(), &[], 0o600)?;
        let stdout_log = self.stdout_log();
        let stdout = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&stdout_log)
            .map_err(|e| os_error("open", &stdout_log, &e))?;
        let mut command = Command::new(&executable);
        command
            .args(self.arguments())
            .current_dir(&self.context)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stdout.try_clone()?);
        // SAFETY: `setsid` is async-signal-safe and touches no shared state.
        unsafe {
            command.pre_exec(|| match libc::setsid() {
                -1 => Err(io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let started = Instant::now();
        let child = crate::sysutil::spawn_tied(command).map_err(|error| {
            anyhow!(
                "launch Unity.DataStore: fork/exec {executable}: {}",
                sysutil::errno_text(&error)
            )
        })?;
        self.stdout = Some(stdout);
        let process = Waiter::new(child);
        self.pid = process.pid;
        let done = process.done.clone();
        self.process = Some(process);
        let deadline = started + Duration::from_secs(10);
        loop {
            let status = match self.status() {
                Ok(status) => status,
                Err(error) => {
                    return Err(join([
                        Some(wrap("DataStore readiness", error)),
                        self.stop().err(),
                    ])
                    .unwrap());
                }
            };
            if status.ready {
                self.ready_time = started.elapsed();
                return Ok(());
            }
            let log = self.service_log();
            let failure = if status.log_evidence.startup_error_logged {
                anyhow!("Unity.DataStore reported startup errors; see {log}")
            } else {
                let tick = Duration::from_millis(50)
                    .min(deadline.saturating_duration_since(Instant::now()));
                match cancel::select(&[Source::Flag(&done), Source::Context(ctx)], Some(tick)) {
                    Some(0) => {
                        let exit = self.status().map(|s| s.exit_error).unwrap_or_default();
                        anyhow!("Unity.DataStore exited before ready: {exit}; see {log}")
                    }
                    Some(_) => context_error(ctx).unwrap(),
                    None if Instant::now() >= deadline => {
                        anyhow!("Unity.DataStore readiness deadline expired; see {log}")
                    }
                    None => continue,
                }
            };
            return Err(join([Some(failure), self.stop().err()]).unwrap());
        }
    }

    fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        let Some(process) = &self.process else {
            return Ok(());
        };
        if process.done.is_set() {
            return Ok(());
        }
        match process.signal_group(signal) {
            Err(error) if error.raw_os_error() != Some(libc::ESRCH) => Err(error),
            _ => Ok(()),
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        let Some(done) = self.process.as_ref().map(|p| p.done.clone()) else {
            return Ok(());
        };
        let result = self.stop_process(&done);
        self.stdout = None;
        if result.is_ok() {
            self.release_shared_memory();
        }
        result
    }

    /// Removes the shared memory of the stopped service. The service is
    /// ended by a signal and leaves its segments in `/dev/shm`, some 180 MB
    /// a run, until later workers die of a bus error on a full file system.
    /// The names hold this service's own random namespace.
    fn release_shared_memory(&self) {
        if self.namespace.len() != 32
            || !self.namespace.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return;
        }
        let Ok(entries) = std::fs::read_dir("/dev/shm") else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if self.protocol.owns_shared_memory(&name, &self.namespace) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    fn stop_process(&mut self, done: &Flag) -> Result<()> {
        if let Err(error) = self.signal_group(libc::SIGTERM) {
            bail!(
                "stop DataStore process group: {}",
                sysutil::errno_text(&error)
            );
        }
        if done.wait(Some(Duration::from_secs(3))) {
            return Ok(());
        }
        self.forced = true;
        if let Err(error) = self.signal_group(libc::SIGKILL) {
            bail!(
                "kill DataStore process group: {}",
                sysutil::errno_text(&error)
            );
        }
        // The kill cannot fail, but releasing the store's mapped memory takes
        // a loaded machine longer than the process took to end by itself.
        if done.wait(Some(Duration::from_secs(60))) {
            return Ok(());
        }
        bail!("Unity.DataStore did not exit after SIGKILL")
    }
}
