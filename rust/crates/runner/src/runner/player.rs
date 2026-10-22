//! The Player process: launch, readiness, reload
//! acknowledgements, reports and the stop escalation.
use super::{
    LaunchOptions, Wake,
    json::{members, read_any_map, read_document},
    os_error, text_error,
};
use crate::{
    cancel::Context,
    hotreload::{self, PlayerCode},
    jsondoc::{self, Target},
    native::stage_native_resources,
    project::{Map, Value, lexpath, value},
    sysutil,
};
use anyhow::{Result, anyhow, bail};
use std::{
    fs::{File, OpenOptions},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// The exit result is delivered once: `None` inside is a clean exit, otherwise
/// the exit-status error text.
pub struct Player {
    pid: i32,
    /// The absolute executable path.
    pub path: String,
    pub report: String,
    pub ready: String,
    pub exited: bool,
    log: Option<File>,
    done: Arc<Mutex<Option<Option<String>>>>,
    /// Set when the process has exited; `done` is emptied by its receiver.
    gone: Arc<AtomicBool>,
    wake: Arc<Wake>,
}

/// Must run on a thread that outlives the Player: the kernel
/// delivers the parent-death signal when the spawning thread ends.
pub fn start_player(
    wake: &Arc<Wake>,
    opts: &LaunchOptions,
    path: &str,
    manifest: &str,
    run_directory: &str,
    request: &str,
) -> Result<Player> {
    let path = lexpath::abs(path)?;
    stage_native_resources(manifest, run_directory)?;
    let report = sysutil::join(&[run_directory, "player-report.json"]);
    let ready = sysutil::join(&[run_directory, "player-report.json.ready.json"]);
    let log_path = sysutil::join(&[run_directory, "stdout.log"]);
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| os_error("open", &log_path, &e))?;
    let mut args: Vec<String> = [
        "-force-glcore",
        "-screen-width",
        &opts.width.to_string(),
        "-screen-height",
        &opts.height.to_string(),
        "-screen-fullscreen",
        "0",
        "-logFile",
        &sysutil::join(&[run_directory, "player.log"]),
        "--unity-cli-report",
        &report,
        "--unity-cli-raw-manifest",
        manifest,
        "--unity-cli-frames",
        &opts.frames.to_string(),
        "--unity-cli-fps",
        &opts.fps.to_string(),
        "--unity-cli-no-vsync",
    ]
    .map(String::from)
    .to_vec();
    if !opts.raw_routes.is_empty() {
        args.extend(["--unity-cli-raw-routes".into(), opts.raw_routes.clone()]);
    }
    if opts.interactive {
        args.push("--unity-cli-interactive".into());
        if opts.control {
            args.extend([
                "--unity-cli-control".into(),
                sysutil::join(&[run_directory, "control"]),
            ]);
        }
        if opts.watch {
            args.extend(["--unity-cli-raw-reload".into(), request.to_string()]);
        }
        if opts.hot_reload {
            let code_request = PlayerCode::request_path(run_directory);
            // A request or acknowledgement of an earlier process is not this one's.
            for stale in [code_request.clone(), format!("{code_request}.ack.json")] {
                let _ = std::fs::remove_file(stale);
            }
            args.extend([hotreload::PLAYER_ARGUMENT.into(), code_request]);
        }
    } else if !opts.screenshot.is_empty() {
        // A Player without a window draws no screen to capture.
        args.extend(["--unity-cli-screenshot".into(), opts.screenshot.clone()]);
    } else {
        args.push("-batchmode".into());
    }
    args.extend(opts.game_arguments.iter().cloned());
    let mut command = Command::new(&path);
    command
        .args(&args)
        // Play mode code finds the project's files by their paths relative
        // to it (`Assets/...`), as in the Editor.
        .current_dir(match opts.play_mode {
            true => opts.project.as_str(),
            false => run_directory,
        })
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|e| os_error("dup", &log_path, &e))?,
        )
        .stderr(
            log.try_clone()
                .map_err(|e| os_error("dup", &log_path, &e))?,
        )
        .process_group(0);
    if std::env::var_os("DISPLAY").is_none_or(|display| display.is_empty()) {
        command.env("DISPLAY", ":0");
    }
    if opts.play_mode {
        // The build target the Editor would have active (`UnityEditor.BuildTarget`).
        let (target, _) = active_build_target(&opts.project);
        command.env("UME_PLAY_MODE_BUILD_TARGET", target.to_string());
        // The game's native plugins bind to their own symbols, as zavod's
        // Editor images open them: Firebase's desktop library and another
        // gRPC of the game otherwise share one gRPC and crash.
        match plugin_deepbind(&opts.project) {
            Ok(library) => {
                let preload = match std::env::var("LD_PRELOAD") {
                    Ok(existing) if !existing.is_empty() => format!("{library}:{existing}"),
                    _ => library,
                };
                command.env("LD_PRELOAD", preload);
                command.env(
                    "UME_DEEPBIND_PREFIXES",
                    format!(
                        "{}/",
                        sysutil::join(&[&crate::compiler::player_data(&path), "Plugins"])
                    ),
                );
            }
            Err(error) => eprintln!(
                "[unity-micro-editor] the game's native plugins share their symbols in this Player: {error:#}"
            ),
        }
    }
    // A redirect does not reach the copies an inlining JIT made of a method.
    // Options the caller set for the Player's Mono are passed on as they are.
    if opts.interactive && opts.hot_reload && std::env::var_os(hotreload::MONO_ARGUMENTS).is_none()
    {
        command.env(hotreload::MONO_ARGUMENTS, hotreload::MONO_ARGUMENTS_VALUE);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            // `stop` interrupts the Player first.
            sysutil::default_stop_signals();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Signal self if the parent died before the request took effect.
            if libc::getppid() != parent {
                libc::kill(libc::getpid(), libc::SIGTERM);
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| os_error("fork/exec", &path, &e))?;
    let done = Arc::new(Mutex::new(None));
    let gone = Arc::new(AtomicBool::new(false));
    let player = Player {
        pid: child.id() as i32,
        path,
        report,
        ready,
        exited: false,
        log: Some(log),
        done: done.clone(),
        gone: gone.clone(),
        wake: wake.clone(),
    };
    let wake = wake.clone();
    std::thread::spawn(move || {
        let result = match child.wait() {
            Ok(status) => sysutil::exit_error(status),
            Err(error) => Some(format!("wait: {}", sysutil::errno_text(&error))),
        };
        gone.store(true, Ordering::SeqCst);
        *done.lock().unwrap() = Some(result);
        wake.notify();
    });
    Ok(player)
}

#[derive(Default)]
struct Ready {
    schema_version: i64,
    pid: i64,
    errors: i64,
    exceptions: i64,
    graphics_device_name: String,
    graphics_device_type: String,
    scene_loaded: bool,
}
/// The first message of a development Player's log that has the stack
/// trace of an error, an exception or an assertion below it, cut to 300
/// bytes. Development Players write every message with its trace.
pub(crate) fn first_logged_error(log: &str) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    const MARKS: [&str; 5] = [
        "UnityEngine.Debug:LogError",
        "UnityEngine.Debug:LogException",
        "UnityEngine.Debug:LogAssertion",
        "UnityEngine.Debug:ExtractStackTraceNoAlloc",
        "UnityEngine.Debug:LogErrorFormat",
    ];
    let file = std::fs::File::open(log).ok()?;
    let mut previous = String::new();
    for line in BufReader::new(file.take(64 << 20)).lines().take(200_000) {
        let line = line.ok()?;
        if MARKS.iter().any(|mark| line.starts_with(mark)) && !previous.trim().is_empty() {
            let message = previous.trim();
            let mut end = message.len().min(300);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            return Some(message[..end].to_string());
        }
        previous = line;
    }
    None
}

fn read_ready(path: &str) -> Result<Ready> {
    let document = read_document(path)?;
    let mut ready = Ready::default();
    let root = members(&document.json, Target::VALUE, "struct").map_err(text_error)?;
    const FIELDS: [&str; 7] = [
        "schemaVersion",
        "pid",
        "Errors",
        "Exceptions",
        "graphicsDeviceName",
        "graphicsDeviceType",
        "Scene",
    ];
    jsondoc::decode_struct(root, &FIELDS, |index, member| {
        let at = Target {
            strukt: "",
            field: FIELDS[index],
        };
        let value = &member.value;
        match index {
            0 => jsondoc::set_int(&mut ready.schema_version, value, at),
            1 => jsondoc::set_int(&mut ready.pid, value, at),
            2 => jsondoc::set_int(&mut ready.errors, value, at),
            3 => jsondoc::set_int(&mut ready.exceptions, value, at),
            4 => jsondoc::set_string(&mut ready.graphics_device_name, value, at),
            5 => jsondoc::set_string(&mut ready.graphics_device_type, value, at),
            _ => {
                let scene = members(value, at, "object")?;
                jsondoc::decode_struct(scene, &["isLoaded"], |_, member| {
                    jsondoc::set_bool(
                        &mut ready.scene_loaded,
                        &member.value,
                        Target {
                            strukt: "",
                            field: "Scene.isLoaded",
                        },
                    )
                })
            }
        }
    })
    .map_err(text_error)?;
    Ok(ready)
}

impl Player {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// A non-blocking receive of the exit result.
    pub fn take_done(&mut self) -> Option<Option<String>> {
        self.done.lock().unwrap().take()
    }

    /// Tells whether the process has exited, without taking the exit result
    /// from the loop that owns the Player.
    pub fn exit_probe(&self) -> impl Fn() -> bool + use<> {
        let gone = self.gone.clone();
        move || gone.load(Ordering::SeqCst)
    }

    fn wait_done(&mut self, timeout: Duration) -> bool {
        let limit = Instant::now() + timeout;
        loop {
            let seen = self.wake.seen();
            if self.take_done().is_some() {
                return true;
            }
            if Instant::now() >= limit {
                return false;
            }
            self.wake.wait(seen, Some(limit));
        }
    }

    /// Polls the ready report every 20 ms. When several outcomes are due at once
    /// the order is cancellation, Player exit, timeout, report.
    pub fn wait_ready(&mut self, ctx: &Context, budget: ReadyBudget) -> Result<()> {
        const TICK: Duration = Duration::from_millis(20);
        let started = Instant::now();
        let mut limit = started + budget.budget;
        let mut tick = Instant::now() + TICK;
        loop {
            let seen = self.wake.seen();
            if let Some(error) = ctx.error() {
                return Err(error.into());
            }
            if let Some(result) = self.take_done() {
                self.exited = true;
                bail!(
                    "Player exited before ready: {}",
                    result.as_deref().unwrap_or("<nil>")
                );
            }
            let now = Instant::now();
            if now >= limit {
                let ran = process_cpu(self.pid);
                match budget.verdict(now - started, ran) {
                    Some(reason) => bail!("{reason}; see {}", self.report),
                    // The Player has hardly run yet: the machine is busy
                    // with something else.
                    None => limit = (now + READY_RECHECK).min(started + budget.limit),
                }
                continue;
            }
            if now < tick {
                self.wake.wait(seen, Some(tick.min(limit)));
                continue;
            }
            // A ticker drops the ticks its receiver was too slow for.
            while tick <= now {
                tick += TICK;
            }
            let Ok(ready) = read_ready(&self.ready) else {
                continue;
            };
            if ready.schema_version != 1
                || ready.pid != i64::from(self.pid)
                || !ready.scene_loaded
                || ready.errors != 0
                || ready.exceptions != 0
            {
                // The counts alone leave the caller to find the log; the
                // first error it shows is what the caller acts on (mcp6-5).
                let log = sysutil::join(&[&sysutil::dir(&self.report), "player.log"]);
                let logged = match (
                    ready.errors + ready.exceptions > 0,
                    first_logged_error(&log),
                ) {
                    (true, Some(first)) => format!(
                        "; the Player logged errors or exceptions before it was ready, which a session does not take as started; the first: \"{first}\"; see {log}"
                    ),
                    (true, None) => format!(
                        "; the Player logged errors or exceptions before it was ready, which a session does not take as started; see {log}"
                    ),
                    (false, _) => format!("; see {log}"),
                };
                bail!(
                    "invalid Player ready report: pid={} errors={} exceptions={} loaded={}{logged}",
                    ready.pid,
                    ready.errors,
                    ready.exceptions,
                    ready.scene_loaded
                );
            }
            return require_hardware_graphics(
                &ready.graphics_device_name,
                &ready.graphics_device_type,
            );
        }
    }

    /// `stop`: SIGINT to the Player's process group, SIGTERM after 3 s, SIGKILL
    /// after 2 more, then at most 1 s for the kernel to reap it.
    pub fn stop(&mut self) {
        let _log = self.log.take();
        if self.exited {
            return;
        }
        if self.take_done().is_some() {
            self.exited = true;
            return;
        }
        let group = -i64::from(self.pid);
        let _ = sysutil::kill(group, libc::SIGINT);
        if self.wait_done(Duration::from_secs(3)) {
            self.exited = true;
            return;
        }
        let _ = sysutil::kill(group, libc::SIGTERM);
        if self.wait_done(Duration::from_secs(2)) {
            self.exited = true;
            return;
        }
        let _ = sysutil::kill(group, libc::SIGKILL);
        self.wait_done(Duration::from_secs(1));
    }
}

/// How long a started Player may take to report that it is ready.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReadyBudget {
    /// The time a Player that gets to run has.
    pub budget: Duration,
    /// The time a Player has on a machine too busy to run it: one that has
    /// had less than `ran` of processor time when `budget` is over keeps
    /// waiting, until it has had that much or this limit is reached.
    pub limit: Duration,
    pub ran: Duration,
}

/// A Player is ready some 5 s after its start, having used about as much
/// processor time. On a machine that swaps, the same start has been seen
/// to write not even its log within 45 s.
pub const READY_BUDGET: ReadyBudget = ReadyBudget {
    budget: Duration::from_secs(45),
    limit: Duration::from_secs(300),
    ran: Duration::from_secs(30),
};

/// How often a Player that is waited for beyond its budget is looked at again.
const READY_RECHECK: Duration = Duration::from_secs(1);

impl ReadyBudget {
    /// One fixed time, however little the Player got to run.
    pub fn fixed(budget: Duration) -> Self {
        Self {
            budget,
            limit: budget,
            ran: Duration::ZERO,
        }
    }

    /// Why a Player that is not ready `elapsed` after its start is given up,
    /// having had `ran` of processor time (`None` when that is unknown);
    /// `None` while it is waited for.
    pub fn verdict(&self, elapsed: Duration, ran: Option<Duration>) -> Option<String> {
        if elapsed < self.budget {
            return None;
        }
        let had = match ran {
            Some(ran) => format!("{:.1}s", ran.as_secs_f64()),
            None => "an unknown amount".into(),
        };
        if elapsed >= self.limit && self.limit > self.budget {
            return Some(format!(
                "Player start: not ready within {}s, the limit for a Player that hardly gets to run (budget {}s; it had {had} of processor time)",
                self.limit.as_secs(),
                self.budget.as_secs(),
            ));
        }
        if elapsed >= self.limit || ran.is_none_or(|ran| ran >= self.ran) {
            return Some(format!(
                "Player start: not ready within its {}s budget (it had {had} of processor time)",
                self.budget.as_secs(),
            ));
        }
        None
    }
}

/// The processor time a process has used, in user and kernel mode.
pub fn process_cpu(pid: i32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; the fields after
    // it start with the state, the third field of the line.
    let mut fields = stat.rsplit_once(')')?.1.split_ascii_whitespace().skip(11);
    let ticks = fields.next()?.parse::<u64>().ok()? + fields.next()?.parse::<u64>().ok()?;
    // SAFETY: `sysconf` reads a constant of the system.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (per_second > 0).then(|| Duration::from_secs_f64(ticks as f64 / per_second as f64))
}

/// A Player must render on a real GPU.
pub fn require_hardware_graphics(device: &str, kind: &str) -> Result<()> {
    if device.trim().is_empty() || kind.trim().is_empty() || kind.eq_ignore_ascii_case("Null") {
        bail!("Player did not report an active GPU renderer");
    }
    let name = lexpath::rune_lower(device);
    for software in [
        "llvmpipe",
        "softpipe",
        "lavapipe",
        "swiftshader",
        "software",
        "gdi generic",
        "microsoft basic render",
    ] {
        if name.contains(software) {
            bail!("Player uses a software renderer: {device}");
        }
    }
    Ok(())
}

/// The final report of a finished Player is clean.
pub fn require_player_report(path: &str) -> Result<()> {
    let document = read_document(path)?;
    let (mut errors, mut exceptions, mut exit_code) = (0, 0, 0);
    let mut fatal = String::new();
    let root = members(&document.json, Target::VALUE, "object").map_err(text_error)?;
    const FIELDS: [&str; 4] = ["Errors", "Exceptions", "fatalError", "exitCode"];
    jsondoc::decode_struct(root, &FIELDS, |index, member| {
        let at = Target {
            strukt: "",
            field: FIELDS[index],
        };
        match index {
            0 => jsondoc::set_int(&mut errors, &member.value, at),
            1 => jsondoc::set_int(&mut exceptions, &member.value, at),
            2 => jsondoc::set_string(&mut fatal, &member.value, at),
            _ => jsondoc::set_int(&mut exit_code, &member.value, at),
        }
    })
    .map_err(text_error)?;
    if errors != 0 || exceptions != 0 || !fatal.is_empty() || exit_code != 0 {
        bail!("Player errors={errors} exceptions={exceptions} exit={exit_code}: {fatal}");
    }
    Ok(())
}

/// The outcome of waiting for a reload acknowledgement: the acknowledgement is
/// returned together with the rejection error.
pub struct ReloadAck {
    /// `None` is an absent map.
    pub ack: Option<Map>,
    pub error: Option<anyhow::Error>,
}

/// Polls every 10 ms for the acknowledgement of `id`.
pub fn wait_reload_ack(ctx: &Context, path: &str, id: &str, timeout: Duration) -> ReloadAck {
    const TICK: Duration = Duration::from_millis(10);
    let limit = Instant::now() + timeout;
    let fail = |error: anyhow::Error| ReloadAck {
        ack: None,
        error: Some(error),
    };
    loop {
        if let Some(error) = ctx.error() {
            return fail(error.into());
        }
        let now = Instant::now();
        if now >= limit {
            return fail(anyhow!("Player reload acknowledgement timed out"));
        }
        // The context is observed at the latest on the next tick.
        crate::cancel::select(
            &[crate::cancel::Source::Context(ctx)],
            Some(TICK.min(limit - now)),
        );
        if ctx.error().is_some() || Instant::now() >= limit {
            continue;
        }
        let Ok(Some(ack)) = read_any_map(path) else {
            continue;
        };
        if ack.get("requestId").and_then(Value::as_str) != Some(id) {
            continue;
        }
        if !ack.get("success").is_some_and(Value::is_true) {
            let reason = value::sprint(ack.get("fatalError").unwrap_or(&Value::Null));
            return ReloadAck {
                ack: Some(ack),
                error: Some(anyhow!("Player rejected revision: {reason}")),
            };
        }
        return ReloadAck {
            ack: Some(ack),
            error: None,
        };
    }
}

/// The preload library that opens the game's native plugins with
/// `RTLD_DEEPBIND` (`tools/PluginDeepbind`), built by the system's C compiler
/// once per source into the project's compiler store.
fn plugin_deepbind(project: &str) -> Result<String> {
    let source = sysutil::join(&[
        &crate::util::micro_editor_root()?,
        "tools/PluginDeepbind/plugin-deepbind.c",
    ]);
    let code = std::fs::read(&source).map_err(|e| os_error("read", &source, &e))?;
    let entry = sysutil::join(&[
        &crate::compiler::project_compiler_store(project),
        "plugin-deepbind",
        &crate::sha256(&code)[..32],
    ]);
    let library = sysutil::join(&[&entry, "ume-plugin-deepbind.so"]);
    if std::path::Path::new(&library).is_file() {
        return Ok(library);
    }
    std::fs::create_dir_all(&entry).map_err(|e| os_error("mkdir", &entry, &e))?;
    let building = format!("{library}.{}", std::process::id());
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o", &building, &source, "-ldl"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("run cc: {e}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&building);
        bail!(
            "cc could not build {source}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    std::fs::rename(&building, &library).map_err(|e| os_error("rename", &library, &e))?;
    Ok(library)
}

/// Makes the `STB_GNU_UNIQUE` symbols of the native plugins of a Player
/// (`<Player>_Data/Plugins`) weak ones, in place; returns how many changed.
/// The loader keeps one instance of a unique symbol for the whole process
/// whatever the library was opened with, so two gRPCs of different versions
/// (Firebase's desktop library and the game's own) shared the tables of
/// gRPC's metadata and crashed in the second one's parser. A weak symbol,
/// with the plugins opened with `RTLD_DEEPBIND`, stays each library's own.
pub fn isolate_plugin_symbols(player: &str) -> Result<usize> {
    let directory = sysutil::join(&[&crate::compiler::player_data(player), "Plugins"]);
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(0);
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "so") || !path.is_file() {
            continue;
        }
        let mut image =
            std::fs::read(&path).map_err(|e| os_error("read", &path.to_string_lossy(), &e))?;
        let count = weaken_unique_symbols(&mut image);
        if count > 0 {
            // A staged copy may share its blocks with the host's (`cp --reflink`).
            let temporary = path.with_extension("so.isolating");
            std::fs::write(&temporary, &image)
                .map_err(|e| os_error("write", &temporary.to_string_lossy(), &e))?;
            std::fs::set_permissions(&temporary, std::fs::metadata(&path)?.permissions())?;
            std::fs::rename(&temporary, &path)
                .map_err(|e| os_error("rename", &path.to_string_lossy(), &e))?;
            changed += count;
        }
    }
    Ok(changed)
}

/// Rewrites the binding of every `STB_GNU_UNIQUE` (10) symbol of an ELF64
/// little-endian image's `.dynsym` to `STB_WEAK` (2); the count of them.
pub(super) fn weaken_unique_symbols(image: &mut [u8]) -> usize {
    let read = |image: &[u8], at: usize, width: usize| -> Option<u64> {
        let bytes = image.get(at..at + width)?;
        Some(
            bytes
                .iter()
                .rev()
                .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)),
        )
    };
    if image.get(..6) != Some(b"\x7fELF\x02\x01".as_slice()) {
        return 0;
    }
    let (Some(sections), Some(entry_size), Some(count)) = (
        read(image, 0x28, 8),
        read(image, 0x3a, 2),
        read(image, 0x3c, 2),
    ) else {
        return 0;
    };
    let mut changed = 0;
    for index in 0..count as usize {
        let header = sections as usize + index * entry_size as usize;
        // SHT_DYNSYM
        if read(image, header + 4, 4) != Some(11) {
            continue;
        }
        let (Some(offset), Some(size), Some(stride)) = (
            read(image, header + 0x18, 8),
            read(image, header + 0x20, 8),
            read(image, header + 0x38, 8),
        ) else {
            continue;
        };
        if stride < 24 {
            continue;
        }
        let mut at = offset as usize;
        while at + stride as usize <= (offset + size) as usize && at + 5 <= image.len() {
            let info = image[at + 4];
            if info >> 4 == 10 {
                image[at + 4] = (2 << 4) | (info & 0x0f);
                changed += 1;
            }
            at += stride as usize;
        }
    }
    changed
}

/// The build target an Editor of the project would have active: the one of
/// `--compile-for`, as `UnityEditor.BuildTarget` numbers and names it.
pub fn active_build_target(project: &str) -> (u32, &'static str) {
    match crate::sourcegraph::compile_for_group(project)
        .ok()
        .flatten()
        .as_deref()
    {
        Some("Android") => (13, "Android"),
        Some("iPhone") => (9, "iOS"),
        Some("WebGL") => (20, "WebGL"),
        _ => (24, "StandaloneLinux64"),
    }
}

/// Play mode code checks that the Addressables content it reads was built
/// for the active build target. The host's content, which stands for it, is
/// a Linux build; the session's copy of its settings names the active target.
pub fn adopt_addressables_target(player: &str, project: &str) -> Result<()> {
    let settings = sysutil::join(&[
        &crate::compiler::player_data(player),
        "StreamingAssets/aa/settings.json",
    ]);
    let Ok(text) = std::fs::read_to_string(&settings) else {
        return Ok(());
    };
    let key = "\"m_buildTarget\":\"";
    let Some(start) = text.find(key).map(|at| at + key.len()) else {
        return Ok(());
    };
    let Some(length) = text[start..].find('"') else {
        return Ok(());
    };
    let (_, name) = active_build_target(project);
    if &text[start..start + length] == name {
        return Ok(());
    }
    let adopted = format!("{}{name}{}", &text[..start], &text[start + length..]);
    let temporary = format!("{settings}.adopting");
    std::fs::write(&temporary, adopted).map_err(|e| os_error("write", &temporary, &e))?;
    std::fs::rename(&temporary, &settings).map_err(|e| os_error("rename", &settings, &e))
}
