//! A .NET helper that stays alive for a watched session and answers one
//! request at a time, each as its one-shot run would: the lifecycle helper
//! (`--serve` of `tools/LifecycleCompiler`) and the patch builder
//! (`--serve` of `tools/HotReloadPatcher`).
//!
//! One JSON object per line in each direction:
//!
//! ```text
//! <- {"ready":true,"protocol":1}                                    once, at the start
//! -> {"id":7,"directory":"/abs","arguments":["/abs/request.json"]}
//! <- {"id":7,"exit":0,"output":"<standard output>","error":"<standard error>","kind":""}
//! ```
//!
//! A process that died, did not answer in time or answered out of protocol
//! is killed, and the caller runs the same request in a process of its own;
//! the next request starts another. After [`FAILURE_LIMIT`] failures in a
//! row the session stops using it and says so on standard error. The
//! process reads its standard input and ends at its end; it is in a process
//! group of its own and killed when the thread that started it or the
//! runner ends (`PR_SET_PDEATHSIG`), and when the handle is dropped.
use crate::{cancel::Context, sysutil};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

/// Failures in a row after which a session stops using a service.
pub const FAILURE_LIMIT: u32 = 3;
/// How long a new process gets to say that it is ready.
const READY_LIMIT: Duration = Duration::from_secs(60);
/// How much of a process's standard error is kept for a failure message.
const ERROR_TAIL: usize = 16 * 1024;
/// The protocol this driver speaks.
const PROTOCOL: i64 = 1;

/// One answer of the process: what the one-shot run would have exited with
/// and printed. `kind` says why a request failed: `diagnostics` for a
/// compile error of the sources, the only failure a caller may report
/// without running the request once more in a process of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Reply {
    pub id: u64,
    pub exit: i64,
    pub output: String,
    pub error: String,
    pub kind: String,
}

/// Why a service did not answer a request; the caller then runs it in a
/// process of its own, or, after [`ServiceError::TimedOut`], may report
/// the time limit as the run's failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    TimedOut(String),
    Failed(String),
}
impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(reason) | Self::Failed(reason) => f.write_str(reason),
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    id: u64,
    directory: &'a str,
    arguments: &'a [&'a str],
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Ready {
    ready: bool,
    protocol: i64,
}

struct Running {
    child: Child,
    input: ChildStdin,
    lines: Receiver<String>,
    errors: Arc<Mutex<Vec<u8>>>,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    next_id: u64,
    failures: u32,
    disabled: bool,
    started: u64,
}

pub struct Service {
    label: &'static str,
    dotnet: String,
    program: String,
    state: Mutex<State>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("label", &self.label)
            .field("program", &self.program)
            .finish()
    }
}

impl Service {
    /// The service of `dotnet <program> --serve`; nothing starts before the
    /// first call.
    pub fn new(label: &'static str, dotnet: &str, program: &str) -> Self {
        Self {
            label,
            dotnet: dotnet.into(),
            program: program.into(),
            state: Mutex::new(State::default()),
        }
    }

    /// The program the service runs.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The process that runs now.
    pub fn pid(&self) -> Option<u32> {
        let state = self.state.lock().unwrap();
        state.running.as_ref().map(|running| running.child.id())
    }

    /// How many processes this service has started.
    pub fn started(&self) -> u64 {
        self.state.lock().unwrap().started
    }

    /// Whether the session stopped using the service.
    pub fn disabled(&self) -> bool {
        self.state.lock().unwrap().disabled
    }

    /// Runs one request. An error means that the service could not answer
    /// it, for the reason given, and that the caller runs it in a process
    /// of its own.
    pub fn call(
        &self,
        ctx: &Context,
        directory: &str,
        arguments: &[&str],
        limit: Duration,
    ) -> Result<Reply, ServiceError> {
        let mut state = self.state.lock().unwrap();
        if state.disabled {
            return Err(ServiceError::Failed(format!(
                "the {} service is off",
                self.label
            )));
        }
        let result = self.exchange(&mut state, ctx, directory, arguments, limit);
        match &result {
            Ok(_) => state.failures = 0,
            Err(reason) => {
                stop(&mut state.running);
                if ctx.error().is_none() {
                    state.failures += 1;
                    if state.failures >= FAILURE_LIMIT && !state.disabled {
                        state.disabled = true;
                        eprintln!(
                            "[unity-micro-editor] the {} failed {} times in a row ({reason}); this session runs it once per request",
                            self.label, state.failures
                        );
                    }
                }
            }
        }
        result
    }

    fn exchange(
        &self,
        state: &mut State,
        ctx: &Context,
        directory: &str,
        arguments: &[&str],
        limit: Duration,
    ) -> Result<Reply, ServiceError> {
        let failed = ServiceError::Failed;
        if let Some(running) = state.running.as_mut()
            && !matches!(running.child.try_wait(), Ok(None))
        {
            let tail = running.tail();
            stop(&mut state.running);
            state.failures += 1;
            eprintln!(
                "[unity-micro-editor] the {} ended between two requests{tail}",
                self.label
            );
        }
        if state.running.is_none() {
            state.running = Some(self.start(ctx).map_err(failed)?);
            state.started += 1;
        }
        state.next_id += 1;
        let id = state.next_id;
        let running = state.running.as_mut().expect("running service");
        let mut line = serde_json::to_vec(&Request {
            id,
            directory,
            arguments,
        })
        .map_err(|error| failed(error.to_string()))?;
        line.push(b'\n');
        running
            .input
            .write_all(&line)
            .and_then(|()| running.input.flush())
            .map_err(|error| failed(format!("the {} does not read: {error}", self.label)))?;
        let deadline = Instant::now() + limit;
        let answer = running.receive(ctx, deadline).map_err(|reason| {
            let text = format!(
                "the {} gave no answer: {reason}{}",
                self.label,
                running.tail()
            );
            match Instant::now() >= deadline && ctx.error().is_none() {
                true => ServiceError::TimedOut(text),
                false => failed(text),
            }
        })?;
        let reply: Reply = serde_json::from_str(&answer).map_err(|error| {
            failed(format!(
                "the {} answered out of protocol: {error}",
                self.label
            ))
        })?;
        if reply.id != id {
            return Err(failed(format!(
                "the {} answered request {} to request {id}",
                self.label, reply.id
            )));
        }
        Ok(reply)
    }

    fn start(&self, ctx: &Context) -> Result<Running, String> {
        let mut command = Command::new(&self.dotnet);
        command
            .args([self.program.as_str(), "--serve"])
            .current_dir(sysutil::dir(&self.program))
            .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
            .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
            .env("DOTNET_NOLOGO", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // SAFETY: getpid has no preconditions.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the hook runs between fork and exec and makes only
        // async-signal-safe system calls without allocating.
        unsafe {
            command.pre_exec(move || {
                // Bound to the thread that runs the session's windows.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::kill(libc::getpid(), libc::SIGKILL);
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("start the {}: {error}", self.label))?;
        let input = child.stdin.take().expect("piped standard input");
        let output = child.stdout.take().expect("piped standard output");
        let mut error = child.stderr.take().expect("piped standard error");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let errors = Arc::new(Mutex::new(Vec::new()));
        {
            let errors = errors.clone();
            thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(count) = error.read(&mut chunk) {
                    if count == 0 {
                        break;
                    }
                    let mut kept = errors.lock().unwrap();
                    kept.extend_from_slice(&chunk[..count]);
                    let excess = kept.len().saturating_sub(ERROR_TAIL);
                    kept.drain(..excess);
                }
            });
        }
        let mut running = Running {
            child,
            input,
            lines,
            errors,
        };
        let ready = running
            .receive(ctx, Instant::now() + READY_LIMIT)
            .map_err(|reason| {
                format!(
                    "the {} did not start: {reason}{}",
                    self.label,
                    running.tail()
                )
            });
        let ready = match ready {
            Ok(line) => line,
            Err(error) => {
                stop(&mut Some(running));
                return Err(error);
            }
        };
        match serde_json::from_str::<Ready>(&ready) {
            Ok(Ready {
                ready: true,
                protocol: PROTOCOL,
            }) => Ok(running),
            _ => {
                let tail = running.tail();
                stop(&mut Some(running));
                Err(format!(
                    "the {} does not serve requests: {}{tail}",
                    self.label,
                    crate::hotreload::bounded(&ready, 256)
                ))
            }
        }
    }
}

impl Running {
    /// The next line, or why none came.
    fn receive(&mut self, ctx: &Context, deadline: Instant) -> Result<String, String> {
        loop {
            if let Some(error) = ctx.error() {
                return Err(error.to_string());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("no answer in time".into());
            }
            let wait = (deadline - now).min(Duration::from_millis(50));
            match self.lines.recv_timeout(wait) {
                Ok(line) => return Ok(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the process closed its output".into());
                }
            }
        }
    }

    /// The end of what the process wrote to standard error, as a clause.
    fn tail(&self) -> String {
        let kept = self.errors.lock().unwrap();
        let text = String::from_utf8_lossy(&kept);
        let text = text.trim();
        match text.is_empty() {
            true => String::new(),
            false => format!("; standard error: {text}"),
        }
    }
}

/// Ends the process: its input closes, which ends a process that waits for
/// a request; one that does not end within a second is killed with its group.
fn stop(running: &mut Option<Running>) {
    let Some(Running {
        mut child, input, ..
    }) = running.take()
    else {
        return;
    };
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(1);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    // Signalled only while it is not reaped: until then its id, which is
    // the id of its group, cannot name anybody else's processes.
    if matches!(child.try_wait(), Ok(None)) {
        let _ = sysutil::kill(-(child.id() as i64), libc::SIGKILL);
    }
    let _ = child.wait();
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            stop(&mut state.running);
        }
    }
}
