//! The Player process: launch, readiness, reload
//! acknowledgements, reports and the stop escalation.
use super::{
    LaunchOptions, Wake,
    json::{members, read_any_map, read_document},
    os_error, text_error,
};
use crate::{
    cancel::Context,
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
    sync::{Arc, Mutex},
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
    } else {
        args.push("-batchmode".into());
    }
    args.extend(opts.game_arguments.iter().cloned());
    let mut command = Command::new(&path);
    command
        .args(&args)
        .current_dir(run_directory)
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
    // SAFETY: only async-signal-safe calls between fork and exec.
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
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
    let player = Player {
        pid: child.id() as i32,
        path,
        report,
        ready,
        exited: false,
        log: Some(log),
        done: done.clone(),
        wake: wake.clone(),
    };
    let wake = wake.clone();
    std::thread::spawn(move || {
        let result = match child.wait() {
            Ok(status) => sysutil::exit_error(status),
            Err(error) => Some(format!("wait: {}", sysutil::errno_text(&error))),
        };
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
    pub fn wait_ready(&mut self, ctx: &Context, timeout: Duration) -> Result<()> {
        const TICK: Duration = Duration::from_millis(20);
        let limit = Instant::now() + timeout;
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
                bail!("Player readiness timeout; see {}", self.report);
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
                bail!(
                    "invalid Player ready report: pid={} errors={} exceptions={} loaded={}",
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
