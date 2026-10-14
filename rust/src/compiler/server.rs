//! The Roslyn compiler server of the SDK, started and owned by one watched
//! session, so that a C# edit does not pay for a cold compiler.
//!
//! The server keeps the metadata of references by path and modification
//! time. A reference rewritten in place with its old modification time is
//! not read again, and the server then compiles against the old bytes. Every
//! path the driver passes is checked by content before each compile, and
//! [`CompilerServer::client`] ends the server and takes a new one when a
//! path arrives with other content than it had before.
//!
//! No client starts a server: the client's `DOTNET_HOST_PATH` names no
//! executable, so a client that reaches no server compiles in its own
//! process. The server is a child of the thread that runs the session, in a
//! process group of its own, killed when that thread or the runner ends
//! (`PR_SET_PDEATHSIG`) and when the handle is dropped. No server of the
//! machine is shared: the pipe name is the session's own.
use super::sys::{CommandError, Spawn, combined_output};
use crate::{cancel::Context, sha256, sysutil};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long one served compile may take before the server is taken for
/// hung, ended, and the assembly compiled in a process of its own.
pub const SERVED_COMPILE_LIMIT: Duration = Duration::from_secs(120);
/// How long a new server gets to listen.
const LISTEN_LIMIT: Duration = Duration::from_secs(10);
/// The server ends itself after this long without a compile. `-1` (no end)
/// makes it fail after its first request; a day is long enough for a session
/// and the owner ends it anyway.
const KEEPALIVE: &str = "-keepalive:86400";
/// Failures in a row after which a session compiles without the server.
const FAILURE_LIMIT: u32 = 3;

#[derive(Debug, Default)]
struct State {
    child: Option<Child>,
    pipe: String,
    count: u32,
    /// The content of every reference and analyzer path the running server
    /// was handed, by path.
    seen: BTreeMap<String, String>,
    failures: u32,
    disabled: bool,
}

#[derive(Debug)]
pub struct CompilerServer {
    dotnet: String,
    csc: String,
    server: String,
    /// `ume-<16 hex of the session output>-<runner pid>`.
    prefix: String,
    /// The path a client gets as `DOTNET_HOST_PATH`: nothing is there.
    no_host: String,
    state: Mutex<State>,
}

/// What a served compile is run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub pipe: String,
    pub arguments: [String; 2],
    pub no_host: String,
}

fn socket(pipe: &str) -> String {
    format!("/tmp/{pipe}")
}

impl CompilerServer {
    /// A server for the compiles of the session whose output is `output`;
    /// nothing starts before the first compile.
    pub fn new(dotnet: &str, csc: &str, output: &str) -> Self {
        let digest = sha256(output.as_bytes());
        Self {
            dotnet: dotnet.into(),
            csc: csc.into(),
            server: sysutil::join(&[&sysutil::dir(csc), "VBCSCompiler.dll"]),
            prefix: format!("ume-{}-{}", &digest[..16], std::process::id()),
            no_host: sysutil::join(&[output, ".no-compiler-server-host"]),
            state: Mutex::new(State::default()),
        }
    }

    /// The csc this server belongs to.
    pub fn csc(&self) -> &str {
        &self.csc
    }

    /// The name of the pipe the running server listens on; empty when none runs.
    pub fn pipe(&self) -> String {
        let state = self.state.lock().unwrap();
        match state.child.is_some() {
            true => state.pipe.clone(),
            false => String::new(),
        }
    }

    /// The process of the running server.
    pub fn pid(&self) -> Option<u32> {
        self.state.lock().unwrap().child.as_ref().map(Child::id)
    }

    /// The arguments of a compile through the server whose references and
    /// analyzers are `inputs` (path to SHA-256 of the content the driver
    /// checked), or `None` when this compile runs without it. Starts a
    /// server when none runs, and a new one under a new name when a path
    /// arrives with other content than the running one was handed.
    pub fn client(&self, inputs: &BTreeMap<String, String>) -> Option<Client> {
        let mut state = self.state.lock().unwrap();
        if state.disabled {
            return None;
        }
        if let Some(child) = state.child.as_mut()
            && !matches!(child.try_wait(), Ok(None))
        {
            // It ended by itself; its name may be taken by now.
            end(&mut state);
        }
        let changed = inputs
            .iter()
            .any(|(path, hash)| state.seen.get(path).is_some_and(|known| known != hash));
        if changed {
            end(&mut state);
        }
        if state.child.is_none()
            && let Err(error) = self.start(&mut state)
        {
            failed(&mut state, &error);
            return None;
        }
        for (path, hash) in inputs {
            state.seen.insert(path.clone(), hash.clone());
        }
        Some(Client {
            pipe: state.pipe.clone(),
            arguments: [format!("-shared:{}", state.pipe), KEEPALIVE.to_string()],
            no_host: self.no_host.clone(),
        })
    }

    /// A served compile failed where the compile in a process of its own
    /// did not, or did not end in time: the server is ended, and the next
    /// compile starts another.
    pub fn failed(&self, reason: &str) {
        let mut state = self.state.lock().unwrap();
        end(&mut state);
        failed(&mut state, reason);
    }

    /// A served compile gave a usable result.
    pub fn succeeded(&self) {
        self.state.lock().unwrap().failures = 0;
    }

    fn start(&self, state: &mut State) -> Result<(), String> {
        state.count += 1;
        state.pipe = format!("{}-{}", self.prefix, state.count);
        let socket = socket(&state.pipe);
        let _ = fs::remove_file(&socket);
        let mut command = Command::new(&self.dotnet);
        command
            .args(["exec", &self.server, &format!("-pipename:{}", state.pipe)])
            .current_dir(sysutil::dir(&self.server))
            .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
            .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
            .env("DOTNET_NOLOGO", "1")
            .env("DOTNET_HOST_PATH", &self.dotnet)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // SAFETY: getpid has no preconditions.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the hook runs between fork and exec and makes only
        // async-signal-safe system calls without allocating.
        unsafe {
            command.pre_exec(move || {
                // Bound to the thread that runs the session's compiles.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::kill(libc::getpid(), libc::SIGKILL);
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| format!("start the compiler server: {error}"))?;
        state.child = Some(child);
        let started = Instant::now();
        loop {
            if fs::symlink_metadata(&socket).is_ok() {
                return Ok(());
            }
            let child = state.child.as_mut().expect("started server");
            if !matches!(child.try_wait(), Ok(None)) {
                end(state);
                return Err("the compiler server ended before it listened".into());
            }
            if started.elapsed() >= LISTEN_LIMIT {
                end(state);
                return Err(format!(
                    "the compiler server did not listen within {}s",
                    LISTEN_LIMIT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn failed(state: &mut State, reason: &str) {
    state.failures += 1;
    if state.failures >= FAILURE_LIMIT && !state.disabled {
        state.disabled = true;
        eprintln!(
            "[unity-micro-editor] the compiler server failed {} times in a row ({reason}); this session compiles without it",
            state.failures
        );
    }
}

/// Ends the running server and forgets what it was handed. A server that
/// was asked to end by `-shutdown` from its own parent never answers, so it
/// is ended by a signal and reaped.
fn end(state: &mut State) {
    if let Some(mut child) = state.child.take() {
        // Signalled only while it is not reaped: until then its id, which
        // is the id of its group, cannot name anybody else's processes.
        let group = -(child.id() as i64);
        if matches!(child.try_wait(), Ok(None)) {
            let _ = sysutil::kill(group, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if matches!(child.try_wait(), Ok(None)) {
            let _ = sysutil::kill(group, libc::SIGKILL);
        }
        let _ = child.wait();
    }
    if !state.pipe.is_empty() {
        let _ = fs::remove_file(socket(&state.pipe));
    }
    state.seen.clear();
}

impl Drop for CompilerServer {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            end(state);
        }
    }
}

/// Whether a listing names a failure of the analyzers or generators rather
/// than of the sources; such a compile is not used.
pub fn analyzer_failure(listing: &str) -> bool {
    listing.contains("CS8032") || listing.contains("CS8784") || listing.contains("CS8785")
}

/// Runs `dotnet <command>`, the path of `csc.dll` and its arguments: through
/// the server when there is one and it answers usably, else in a process of
/// its own. A served compile that
/// failed or named an analyzer failure is run again in a process of its own
/// and that result counts, so that nothing the server did reaches the user
/// as a compile error. `verify` compiles a served result once more without
/// the server into the same paths and fails when a byte of `outputs`
/// differs (`UME_COMPILER_VERIFY=1`).
#[allow(clippy::too_many_arguments)]
pub fn csc_command(
    ctx: &Context,
    server: Option<&CompilerServer>,
    verify: bool,
    directory: &str,
    dotnet: &str,
    command: &[&str],
    inputs: &BTreeMap<String, String>,
    outputs: &[String],
) -> Result<Vec<u8>, CommandError> {
    let csc = |ctx: &Context, client: Option<&Client>| {
        let mut arguments: Vec<&str> = vec![];
        if let Some(client) = client {
            arguments.extend(client.arguments.iter().map(String::as_str));
        }
        run(
            ctx,
            directory,
            dotnet,
            &arguments,
            command,
            client.map(|client| client.no_host.as_str()),
        )
    };
    let Some(server) = server else {
        return csc(ctx, None);
    };
    let Some(client) = server.client(inputs) else {
        return csc(ctx, None);
    };
    let limited = ctx.with_timeout(SERVED_COMPILE_LIMIT);
    let served = csc(&limited, Some(&client));
    let expired = limited.error().is_some() && ctx.error().is_none();
    limited.cancel();
    if ctx.error().is_some() {
        return served;
    }
    let usable = match &served {
        Ok(output) => !analyzer_failure(&String::from_utf8_lossy(output)),
        Err(_) => false,
    };
    if usable {
        server.succeeded();
        if verify {
            return verify_served(outputs, served, || csc(ctx, None));
        }
        return served;
    }
    let cold = csc(ctx, None);
    if expired {
        server.failed("a compile did not end in time");
    } else if cold
        .as_ref()
        .is_ok_and(|output| !analyzer_failure(&String::from_utf8_lossy(output)))
    {
        server.failed("a compile failed that compiles in a process of its own");
    }
    cold
}

/// `dotnet <csc> <shared...> <arguments...>`.
fn run(
    ctx: &Context,
    directory: &str,
    dotnet: &str,
    shared: &[&str],
    command: &[&str],
    no_host: Option<&str>,
) -> Result<Vec<u8>, CommandError> {
    let mut arguments: Vec<&str> = command.iter().take(1).copied().collect();
    arguments.extend(shared);
    arguments.extend(command.iter().skip(1));
    let mut environment = vec![
        ("DOTNET_CLI_TELEMETRY_OPTOUT", "1"),
        ("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1"),
        ("DOTNET_NOLOGO", "1"),
    ];
    if let Some(no_host) = no_host {
        environment.push(("DOTNET_HOST_PATH", no_host));
    }
    let (output, error) = combined_output(
        ctx,
        &Spawn {
            directory,
            executable: dotnet,
            arguments: &arguments,
            group: true,
            wait_delay: Some(Duration::from_secs(2)),
            environment: &environment,
        },
    );
    match error {
        None => Ok(output),
        Some(error) => Err(CommandError {
            message: format!(
                "{}: {error}\n{}",
                sysutil::base(dotnet),
                String::from_utf8_lossy(&output)
            ),
            output,
        }),
    }
}

fn verify_served(
    outputs: &[String],
    served: Result<Vec<u8>, CommandError>,
    cold: impl Fn() -> Result<Vec<u8>, CommandError>,
) -> Result<Vec<u8>, CommandError> {
    let fail = |message: String| CommandError {
        message,
        output: Vec::new(),
    };
    let mut kept = Vec::new();
    for output in outputs {
        kept.push(fs::read(output).ok());
        let _ = fs::remove_file(output);
    }
    cold()?;
    for (output, served) in outputs.iter().zip(kept) {
        let cold = fs::read(output).ok();
        if cold != served {
            return Err(fail(format!(
                "UME_COMPILER_VERIFY: the compiler server and a compile in a process of its own wrote different bytes to {output}"
            )));
        }
    }
    served
}
