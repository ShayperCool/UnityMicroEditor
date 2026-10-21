//! A client of `UnityShaderCompiler`, the program the Editor ships beside its
//! main binary and starts for every shader program it compiles. The client
//! takes the Editor's side of the conversation: it listens on a local socket,
//! starts the compiler, which connects back, and sends one request per
//! program. No Editor process is involved.
//!
//! The wire format was read off the pinned Editor's own conversations: every
//! item is preceded by four marker bytes; an integer is 32 bits, a length 64,
//! a string its length and bytes.
use super::support::{abs, mkdir_all, os_error, stat, wrap};
use crate::{
    cancel::Context,
    playerbake::{SnippetCompiler, SnippetReply, SnippetRequest, WIRE_MARKER as MARKER},
    sysutil,
};
use anyhow::{Result, anyhow, bail};
use std::{
    io::{Read, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The protocol revision the pinned compiler answers `initializeCompiler`
/// with. Another revision may frame requests differently.
const PROTOCOL: u32 = 0xf404_c210;
/// The graphics APIs the compiler reports capabilities for.
const PLATFORMS: usize = 29;
/// A reflection line or program beyond this is not a compiler reply.
const ITEM_LIMIT: u64 = 256 << 20;

pub struct ShaderCompiler {
    child: Child,
    stream: UnixStream,
    ctx: Context,
    /// What each graphics API can do, by its compiler platform number.
    pub capabilities: Vec<u64>,
}

impl ShaderCompiler {
    /// Starts a compiler whose working directory is `project`: an include is
    /// resolved against it and then against the engine's own include folder.
    /// `logs` receives the compiler's log.
    pub fn start(ctx: &Context, editor: &str, project: &str, logs: &str) -> Result<Self> {
        let editor = abs(editor)?;
        let info = stat(&editor).map_err(|e| wrap("Unity installation", e))?;
        let installation = if info.is_dir() {
            editor
        } else {
            sysutil::dir(&editor)
        };
        let data = sysutil::join(&[&installation, "Data"]);
        let executable = sysutil::join(&[&data, "Tools", "UnityShaderCompiler"]);
        let info = stat(&executable).map_err(|e| wrap("UnityShaderCompiler executable", e))?;
        if !info.is_regular() || info.mode & 0o111 == 0 {
            bail!("UnityShaderCompiler must be a regular executable");
        }
        let logs = abs(logs)?;
        mkdir_all(&logs, 0o755)?;
        let name = format!(
            "ShaderCompilerIPC-{}-{}",
            std::process::id(),
            sysutil::new_id()
        );
        let socket = format!("/tmp/Unity-{name}.sock");
        let listener = UnixListener::bind(&socket).map_err(|e| os_error("bind", &socket, &e))?;
        let result = Self::launch(ctx, &listener, &executable, &data, project, &logs, &name);
        let _ = std::fs::remove_file(&socket);
        result
    }

    fn launch(
        ctx: &Context,
        listener: &UnixListener,
        executable: &str,
        data: &str,
        project: &str,
        logs: &str,
        name: &str,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .arg(data)
            .arg(sysutil::join(&[logs, "shadercompiler.log"]))
            .arg("0")
            .arg(format!("-local-ipc-stream={name}"))
            .arg(sysutil::join(&[data, "PlaybackEngines"]))
            // Requests are sent one at a time, so the compiler needs no pool.
            .arg("-no-job-workers")
            .current_dir(project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: `setsid` is async-signal-safe and touches no shared state.
        unsafe {
            command.pre_exec(|| match libc::setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let mut child = command.spawn().map_err(|error| {
            anyhow!(
                "launch UnityShaderCompiler: fork/exec {executable}: {}",
                sysutil::errno_text(&error)
            )
        })?;
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    stop(&mut child);
                    return Err(anyhow!("accept UnityShaderCompiler: {error}"));
                }
            }
            let failure = if ctx.error().is_some() {
                Some(anyhow!("shader compilation was cancelled"))
            } else if let Ok(Some(status)) = child.try_wait() {
                Some(anyhow!(
                    "UnityShaderCompiler exited before connecting: {status}"
                ))
            } else if Instant::now() >= deadline {
                Some(anyhow!("UnityShaderCompiler did not connect"))
            } else {
                None
            };
            if let Some(failure) = failure {
                stop(&mut child);
                return Err(failure);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        stream.set_nonblocking(false)?;
        let mut compiler = Self {
            child,
            stream,
            ctx: ctx.clone(),
            capabilities: Vec::new(),
        };
        // The compiler greets with an empty line.
        if !compiler.text()?.is_empty() {
            bail!("UnityShaderCompiler greeting is not the pinned protocol's");
        }
        compiler.initialize(data)?;
        Ok(compiler)
    }

    fn initialize(&mut self, data: &str) -> Result<()> {
        self.put_text(b"c:initializeCompiler")?;
        // The include roots: the project, then the engine's includes.
        self.put_int(2)?;
        self.put_text(b".")?;
        self.put_text(sysutil::join(&[data, "Resources", "CGIncludes"]).as_bytes())?;
        // No redirected include folders.
        self.put_int(0)?;
        self.put_text(br#"{"globalenvvar":{}}"#)?;
        self.stream.flush()?;
        let protocol = self.int()? as u32;
        if protocol != PROTOCOL {
            bail!(
                "UnityShaderCompiler speaks protocol {protocol:#x}, not the pinned {PROTOCOL:#x}"
            );
        }
        for _ in 0..PLATFORMS {
            let capabilities = self.long()?;
            self.int()?;
            self.capabilities.push(capabilities);
        }
        Ok(())
    }

    /// Bounds every call by the context: its deadline is the socket's.
    fn budget(&self) -> Result<()> {
        if self.ctx.error().is_some() {
            bail!("shader compilation was cancelled");
        }
        let left = match self.ctx.remaining() {
            Some(left) if left.is_zero() => bail!("shader compilation timed out"),
            // A single program takes seconds at most.
            left => left.unwrap_or(Duration::from_secs(120)),
        };
        self.stream.set_read_timeout(Some(left))?;
        self.stream.set_write_timeout(Some(left))?;
        Ok(())
    }

    fn take(&mut self, count: usize) -> Result<Vec<u8>> {
        self.budget()?;
        let mut data = vec![0u8; count];
        self.stream
            .read_exact(&mut data)
            .map_err(|error| anyhow!("read from UnityShaderCompiler: {error}"))?;
        Ok(data)
    }
    fn marker(&mut self) -> Result<()> {
        if self.take(4)? != MARKER {
            bail!("UnityShaderCompiler reply is not framed as the pinned protocol frames it");
        }
        Ok(())
    }
    fn int(&mut self) -> Result<i32> {
        self.marker()?;
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn long(&mut self) -> Result<u64> {
        self.marker()?;
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn text(&mut self) -> Result<Vec<u8>> {
        let length = self.long()?;
        if length > ITEM_LIMIT {
            bail!("UnityShaderCompiler reply item exceeds supported size");
        }
        self.take(length as usize)
    }
    fn put(&mut self, data: &[u8]) -> Result<()> {
        self.budget()?;
        self.stream
            .write_all(data)
            .map_err(|error| anyhow!("write to UnityShaderCompiler: {error}"))
    }
    fn put_int(&mut self, value: i32) -> Result<()> {
        self.put(&[&MARKER[..], &value.to_le_bytes()].concat())
    }
    fn put_text(&mut self, text: &[u8]) -> Result<()> {
        self.put(&[&MARKER[..], &(text.len() as u64).to_le_bytes(), text].concat())
    }
    fn request(&mut self, request: &SnippetRequest) -> Result<SnippetReply> {
        self.put_text(b"c:compileSnippet")?;
        self.put(&request.encode())?;
        self.stream.flush()?;
        let mut lines = Vec::new();
        let compiled = loop {
            let line = self.text()?;
            if let Some(verdict) = line.strip_prefix(b"shader: ") {
                break verdict == b"1";
            }
            if lines.len() >= 1 << 20 {
                bail!("UnityShaderCompiler reply has more lines than a reflection holds");
            }
            lines.push(line);
        };
        let program_type = self.int()?;
        let program = self.text()?;
        Ok(SnippetReply {
            lines,
            compiled,
            program_type,
            program,
        })
    }
}

impl SnippetCompiler for ShaderCompiler {
    fn compile(&mut self, request: &SnippetRequest) -> Result<SnippetReply> {
        self.request(request)
    }
}

fn stop(child: &mut Child) {
    // The compiler leads its own session, so its group is its own.
    let _ = sysutil::kill(-i64::from(child.id()), libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for ShaderCompiler {
    fn drop(&mut self) {
        // A compiler that is asked to leaves on its own; one that is busy or
        // gone is stopped.
        let asked = self
            .stream
            .set_write_timeout(Some(Duration::from_millis(200)))
            .is_ok()
            && self
                .stream
                .write_all(&[&MARKER[..], &10u64.to_le_bytes(), b"c:shutdown"].concat())
                .is_ok();
        let deadline = Instant::now() + Duration::from_secs(2);
        while asked && Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        stop(&mut self.child);
    }
}
