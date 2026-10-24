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
use anyhow::{Context as _, Result, anyhow, bail};
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
/// The graphics APIs the compiler reports capabilities for, by release.
fn platforms(version: &str) -> usize {
    if version.starts_with("6000.0.") {
        28
    } else {
        29
    }
}
/// A reflection line or program beyond this is not a compiler reply.
const ITEM_LIMIT: u64 = 256 << 20;

pub struct ShaderCompiler {
    /// The release of the installation.
    version: String,
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
        Self::start_with_folders(ctx, editor, project, logs, &[])
    }

    /// Starts a compiler that also finds each of `folders`, a logical
    /// folder such as `Packages/<name>` with the directory it lies in, the
    /// way the Editor tells it where the project's packages lie.
    pub fn start_with_folders(
        ctx: &Context,
        editor: &str,
        project: &str,
        logs: &str,
        folders: &[(String, String)],
    ) -> Result<Self> {
        let editor = abs(editor)?;
        let info = stat(&editor).map_err(|e| wrap("Unity installation", e))?;
        let installation = if info.is_dir() {
            editor
        } else {
            sysutil::dir(&editor)
        };
        let data = sysutil::join(&[&installation, "Data"]);
        let version = crate::engine::editor_version(&sysutil::join(&[&installation, "Unity"]))
            .unwrap_or_default();
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
        let result = Self::launch(
            ctx,
            &listener,
            &executable,
            &data,
            project,
            &logs,
            &name,
            version,
            folders,
        );
        let _ = std::fs::remove_file(&socket);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn launch(
        ctx: &Context,
        listener: &UnixListener,
        executable: &str,
        data: &str,
        project: &str,
        logs: &str,
        name: &str,
        version: String,
        folders: &[(String, String)],
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
        let mut child = crate::sysutil::spawn_tied(command).map_err(|error| {
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
            version,
            child,
            stream,
            ctx: ctx.clone(),
            capabilities: Vec::new(),
        };
        // The compiler greets with an empty line.
        if !compiler.text()?.is_empty() {
            bail!("UnityShaderCompiler greeting is not the pinned protocol's");
        }
        compiler.initialize(data, folders)?;
        Ok(compiler)
    }

    fn initialize(&mut self, data: &str, folders: &[(String, String)]) -> Result<()> {
        self.put_text(b"c:initializeCompiler")?;
        // The include roots: the project, then the engine's includes.
        self.put_int(2)?;
        self.put_text(b".")?;
        // 6000.0 keeps them at the top of `Data`.
        let includes = match self.version.starts_with("6000.0.") {
            true => sysutil::join(&[data, "CGIncludes"]),
            false => sysutil::join(&[data, "Resources", "CGIncludes"]),
        };
        self.put_text(includes.as_bytes())?;
        // The redirected folders, each logical then physical.
        self.put_int(folders.len() as i32)?;
        for (logical, physical) in folders {
            self.put_text(logical.as_bytes())?;
            self.put_text(physical.as_bytes())?;
        }
        // 6000.0 sends no environment.
        match self.version.starts_with("6000.0.") {
            true => self.put_text(b"")?,
            false => self.put_text(br#"{"globalenvvar":{}}"#)?,
        }
        self.stream.flush()?;
        let protocol = self.int()? as u32;
        if protocol != PROTOCOL {
            bail!(
                "UnityShaderCompiler speaks protocol {protocol:#x}, not the pinned {PROTOCOL:#x}"
            );
        }
        for _ in 0..platforms(&self.version) {
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

impl ShaderCompiler {
    /// Preprocesses a `.shader` as the Editor's import does (`c:preprocess`).
    pub fn preprocess(
        &mut self,
        request: &crate::playerbake::shaderlab::PreprocessRequest,
    ) -> Result<crate::playerbake::shaderlab::PreprocessReply> {
        use crate::playerbake::shaderlab::{
            PreprocessMessage, PreprocessReply, PreprocessedSnippet, line_numbers,
        };
        self.put_text(b"c:preprocess")?;
        self.put_text(request.source.as_bytes())?;
        self.put_text(request.directory.as_bytes())?;
        self.put_text(request.file.as_bytes())?;
        self.put_int(request.surface_only as i32)?;
        self.put_int(request.platform)?;
        self.put_int(request.api_mask)?;
        for list in [&request.platform_keywords, &request.defines] {
            self.put_int(list.len() as i32)?;
            for keyword in list {
                self.put_text(keyword.as_bytes())?;
            }
        }
        self.stream.flush()?;
        let mut reply = PreprocessReply::default();
        let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
        let count = |line: &str| -> Result<usize> {
            let n = line_numbers(line).last().copied().unwrap_or(0);
            usize::try_from(n)
                .ok()
                .filter(|n| *n < 1 << 20)
                .ok_or_else(|| anyhow!("UnityShaderCompiler line {line} counts no items"))
        };
        for _ in 0..1 << 22 {
            let line = text(self.text()?);
            if line.starts_with("snip:") {
                reply.snippets.push(PreprocessedSnippet {
                    header: line_numbers(&line),
                    code: text(self.text()?),
                    ..Default::default()
                });
            } else if line.starts_with("keywordsEnd:") {
                let snippet = reply
                    .snippets
                    .last_mut()
                    .context("UnityShaderCompiler ends the keywords of no program")?;
                for slot in &mut snippet.keyword_texts {
                    *slot = text(self.text()?);
                }
                snippet.base_requirements = self.long()?;
                let targets = self.int()?;
                for _ in 0..targets.clamp(0, 1 << 16) {
                    let name = text(self.text()?);
                    snippet.targets.push((name, self.long()?));
                }
            } else if line.starts_with("keywords") {
                let (kind, _) = line.split_once(':').unwrap_or_default();
                let numbers = line_numbers(&line);
                let mut sets = Vec::new();
                for _ in 0..count(&line)? {
                    sets.push(text(self.text()?));
                }
                let snippet = reply
                    .snippets
                    .last_mut()
                    .context("UnityShaderCompiler names keywords of no program")?;
                let program_type = numbers.get(1).copied().unwrap_or(0) as u32;
                snippet
                    .keywords
                    .push((kind.to_string(), program_type, sets));
            } else if line.starts_with("includes:") || line.starts_with("filepaths:") {
                let mut items = Vec::new();
                for _ in 0..count(&line)? {
                    items.push(text(self.text()?));
                }
                match line.starts_with("includes:") {
                    true => reply.includes = items,
                    false => reply.filepaths = items,
                }
            } else if line.starts_with("err:") {
                let header = line_numbers(&line);
                // A line of -1 ends the program messages.
                if header.get(1) == Some(&-1) {
                    self.long()?;
                    continue;
                }
                let file = text(self.text()?);
                let message = text(self.text()?);
                reply.messages.push(PreprocessMessage {
                    header,
                    file,
                    message,
                });
            } else if line.starts_with("shader:") {
                reply.ok = line_numbers(&line).first() == Some(&1);
                reply.shader_line = line;
                reply.shader = text(self.text()?);
                return Ok(reply);
            }
        }
        bail!("UnityShaderCompiler preprocessed without end")
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
