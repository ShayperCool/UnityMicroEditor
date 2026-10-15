//! File, glob and process primitives with the error texts, permissions and
//! ordering the compiler pipeline relies on.
use crate::{
    cancel::{self, Context, Flag, Source},
    sysutil,
};
use anyhow::{Result, anyhow};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub fn os_error(operation: &str, path: &str, error: &io::Error) -> anyhow::Error {
    anyhow::Error::new(sysutil::Error::os(operation, path, error))
}
/// True when an error produced by these helpers means the path does not exist.
pub fn is_not_exist(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sysutil::Error>()
        .is_some_and(sysutil::Error::is_not_exist)
}
/// True when the path itself (not a link target) does not exist.
pub fn absent(path: &str) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}
/// Reads a whole file.
pub fn read(path: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|e| os_error("open", path, &e))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| os_error("read", path, &e))?;
    Ok(data)
}
/// Writes a whole file with mode 0600, truncating an existing one.
pub fn write(path: &str, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| os_error("open", path, &e))?;
    file.write_all(data)
        .map_err(|e| os_error("write", path, &e))
}
/// Creates a directory and its parents with mode 0700.
pub fn mkdir_all(path: &str) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| os_error("mkdir", path, &e))
}
/// Creates one directory with mode 0700.
pub fn mkdir(path: &str) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| os_error("mkdir", path, &e))
}
/// Creates a uniquely named directory under `directory` starting with `prefix`.
pub fn mkdir_temp(directory: &str, prefix: &str) -> Result<String> {
    let created = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(directory)
        .map_err(|e| os_error("mkdir", &sysutil::join(&[directory, prefix]), &e))?;
    Ok(created.keep().to_string_lossy().into_owned())
}
/// The switches of every `dotnet build` the tool starts. Without them
/// MSBuild starts a compiler server (VBCSCompiler) on a pipe that every
/// process of the user with this Roslyn shares, and may keep worker nodes:
/// processes of hundreds of megabytes in no group the tool ever signals,
/// alive for ten minutes after the last build that used them, and used in
/// turn by builds of other sessions. A build the tool starts leaves no
/// process behind; it costs about a second more, once per cached helper.
pub const BUILD_WITHOUT_SERVERS: [&str; 2] = ["--disable-build-servers", "-nodeReuse:false"];
/// Renames a file or directory.
pub fn rename(from: &str, to: &str) -> Result<()> {
    fs::rename(from, to).map_err(|e| os_error("rename", &format!("{from} {to}"), &e))
}
/// Publishes the cache entry built in the directory `payload` as `entry`.
/// A directory cannot be renamed onto one that is not empty, so an entry
/// that stands decides: one that passes `valid` was published by another
/// process meanwhile and is kept, with the payload dropped; any other is a
/// damaged entry of ours (a file cut short or changed: nothing is synced
/// before the rename) and is removed first. Left in place it would fail the
/// rename, and with it every compile of that state, until someone removed it
/// by hand.
pub fn publish(payload: &str, entry: &str, valid: impl Fn() -> bool) -> Result<()> {
    if fs::symlink_metadata(entry).is_ok() {
        if valid() {
            let _ = fs::remove_dir_all(payload);
            return Ok(());
        }
        fs::remove_dir_all(entry).map_err(|e| os_error("remove", entry, &e))?;
    }
    rename(payload, entry)
}
/// Names of the regular files in a directory, in name order.
pub fn regular_files(directory: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| os_error("open", directory, &e))? {
        let entry = entry.map_err(|e| os_error("readdirent", directory, &e))?;
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

pub enum Walk {
    Continue,
    SkipDir,
}
pub struct WalkEntry<'a> {
    pub path: &'a str,
    pub kind: fs::FileType,
}
impl WalkEntry<'_> {
    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }
}
/// Walks a tree for visitors that return the error they are handed:
/// lexical order, no symlink following, the root visited first.
pub fn walk_dir(root: &str, visit: &mut dyn FnMut(&WalkEntry) -> Result<Walk>) -> Result<()> {
    let info = fs::symlink_metadata(root).map_err(|e| os_error("lstat", root, &e))?;
    walk_entry(root, info.file_type(), visit)
}
fn walk_entry(
    path: &str,
    kind: fs::FileType,
    visit: &mut dyn FnMut(&WalkEntry) -> Result<Walk>,
) -> Result<()> {
    if matches!(visit(&WalkEntry { path, kind })?, Walk::SkipDir) || !kind.is_dir() {
        return Ok(());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|e| os_error("open", path, &e))? {
        let entry = entry.map_err(|e| os_error("readdirent", path, &e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = sysutil::join(&[path, &name]);
        let kind = entry
            .file_type()
            .map_err(|e| os_error("lstat", &child, &e))?;
        entries.push((name, child, kind));
    }
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for (_, child, kind) in entries {
        walk_entry(&child, kind, visit)?;
    }
    Ok(())
}

const BAD_PATTERN: &str = "syntax error in pattern";
fn rune(text: &[u8]) -> (u32, usize) {
    let limit = text.len().min(4);
    let valid = match std::str::from_utf8(&text[..limit]) {
        Ok(valid) => valid,
        Err(error) => std::str::from_utf8(&text[..error.valid_up_to()]).unwrap_or(""),
    };
    match valid.chars().next() {
        Some(character) => (character as u32, character.len_utf8()),
        None if text.is_empty() => (0xfffd, 0),
        None => (0xfffd, 1),
    }
}
fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while pattern.first() == Some(&b'*') {
        pattern = &pattern[1..];
        star = true;
    }
    let mut in_range = false;
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            b'\\' if index + 1 < pattern.len() => index += 1,
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => break,
            _ => {}
        }
        index += 1;
    }
    (star, &pattern[..index], &pattern[index..])
}
fn get_esc(mut chunk: &[u8]) -> Result<(u32, &[u8]), ()> {
    if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
        return Err(());
    }
    if chunk[0] == b'\\' {
        chunk = &chunk[1..];
        if chunk.is_empty() {
            return Err(());
        }
    }
    let (value, size) = rune(chunk);
    let rest = &chunk[size..];
    if (value == 0xfffd && size == 1) || rest.is_empty() {
        return Err(());
    }
    Ok((value, rest))
}
fn match_chunk<'a>(mut chunk: &[u8], mut text: &'a [u8]) -> Result<Option<&'a [u8]>, ()> {
    let mut failed = false;
    while !chunk.is_empty() {
        if !failed && text.is_empty() {
            failed = true;
        }
        match chunk[0] {
            b'[' => {
                let mut current = 0;
                if !failed {
                    let (value, size) = rune(text);
                    current = value;
                    text = &text[size..];
                }
                chunk = &chunk[1..];
                let mut negated = false;
                if chunk.first() == Some(&b'^') {
                    negated = true;
                    chunk = &chunk[1..];
                }
                let (mut matched, mut ranges) = (false, 0);
                loop {
                    if chunk.first() == Some(&b']') && ranges > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (low, rest) = get_esc(chunk)?;
                    chunk = rest;
                    let mut high = low;
                    if chunk[0] == b'-' {
                        let (value, rest) = get_esc(&chunk[1..])?;
                        high = value;
                        chunk = rest;
                    }
                    if low <= current && current <= high {
                        matched = true;
                    }
                    ranges += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            b'?' => {
                if !failed {
                    if text[0] == b'/' {
                        failed = true;
                    }
                    let (_, size) = rune(text);
                    text = &text[size..];
                }
                chunk = &chunk[1..];
            }
            first => {
                if first == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return Err(());
                    }
                }
                if !failed {
                    if chunk[0] != text[0] {
                        failed = true;
                    }
                    text = &text[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    Ok(if failed { None } else { Some(text) })
}
/// Matches a name against a glob pattern; an error for a malformed pattern.
pub fn path_match(pattern: &str, name: &str) -> Result<bool> {
    let bad = |()| anyhow!(BAD_PATTERN);
    let (mut pattern, mut name) = (pattern.as_bytes(), name.as_bytes());
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            return Ok(!name.contains(&b'/'));
        }
        let outcome = match_chunk(chunk, name);
        if let Ok(Some(remaining)) = outcome
            && (remaining.is_empty() || !pattern.is_empty())
        {
            name = remaining;
            continue;
        }
        outcome.map_err(bad)?;
        if star {
            let mut index = 0;
            while index < name.len() && name[index] != b'/' {
                if let Some(remaining) = match_chunk(chunk, &name[index + 1..]).map_err(bad)? {
                    if pattern.is_empty() && !remaining.is_empty() {
                        index += 1;
                        continue;
                    }
                    name = remaining;
                    continue 'pattern;
                }
                index += 1;
            }
        }
        while !pattern.is_empty() {
            let (_, chunk, rest) = scan_chunk(pattern);
            pattern = rest;
            match_chunk(chunk, b"").map_err(bad)?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}
fn has_meta(path: &str) -> bool {
    path.bytes()
        .any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
}
fn glob_directory(directory: &str, pattern: &str, matches: &mut Vec<String>) -> Result<()> {
    if !fs::metadata(directory).is_ok_and(|info| info.is_dir()) {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    for name in names {
        if path_match(pattern, &name)? {
            matches.push(sysutil::join(&[directory, &name]));
        }
    }
    Ok(())
}
/// Paths matching a glob pattern; an error for a malformed pattern.
pub fn glob(pattern: &str) -> Result<Vec<String>> {
    path_match(pattern, "")?;
    if !has_meta(pattern) {
        return Ok(match fs::symlink_metadata(pattern) {
            Ok(_) => vec![pattern.to_string()],
            Err(_) => Vec::new(),
        });
    }
    let split = pattern.rfind('/').map_or(0, |index| index + 1);
    let (directory, file) = pattern.split_at(split);
    let directory = match directory {
        "" => ".",
        "/" => "/",
        other => &other[..other.len() - 1],
    };
    let mut matches = Vec::new();
    if !has_meta(directory) {
        glob_directory(directory, file, &mut matches)?;
        return Ok(matches);
    }
    if directory == pattern {
        return Err(anyhow!(BAD_PATTERN));
    }
    for parent in glob(directory)? {
        glob_directory(&parent, file, &mut matches)?;
    }
    Ok(matches)
}
/// `glob` whose error is discarded.
pub fn glob_lossy(pattern: &str) -> Vec<String> {
    glob(pattern).unwrap_or_default()
}

/// A failed external command: the combined output comes next to the error.
#[derive(Debug)]
pub struct CommandError {
    pub message: String,
    pub output: Vec<u8>,
}
impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CommandError {}

pub(crate) struct Spawn<'a> {
    pub directory: &'a str,
    pub executable: &'a str,
    pub arguments: &'a [&'a str],
    /// Runs in its own process group, killed as a whole (SIGKILL) on cancellation.
    pub group: bool,
    /// How long the child and its output pipes get after cancellation before they are cut off.
    pub wait_delay: Option<Duration>,
    pub environment: &'a [(&'a str, &'a str)],
}
/// Finds an executable for a bare command name on `PATH`.
fn look_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':').find_map(|directory| {
        let candidate = sysutil::join(&[if directory.is_empty() { "." } else { directory }, name]);
        let info = fs::metadata(&candidate).ok()?;
        (!info.is_dir() && info.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}
fn combined_pipe() -> io::Result<(File, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: `ends` is a valid two-element array for the kernel to fill.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned exactly once.
    Ok(unsafe { (File::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}
/// Runs a command with stdout and stderr merged: the combined output and the
/// error text (`exec: ...` wording), if the command failed.
pub(crate) fn combined_output(ctx: &Context, spawn: &Spawn) -> (Vec<u8>, Option<String>) {
    let resolved = if spawn.executable.contains('/') {
        spawn.executable.to_string()
    } else {
        match look_path(spawn.executable) {
            Some(found) => found,
            None => {
                return (
                    Vec::new(),
                    Some(format!(
                        "exec: {}: executable file not found in $PATH",
                        crate::jsondoc::quoted_text(spawn.executable)
                    )),
                );
            }
        }
    };
    if let Some(error) = ctx.error() {
        return (Vec::new(), Some(error.to_string()));
    }
    let (mut reader, writer) = match combined_pipe() {
        Ok(pipe) => pipe,
        Err(error) => return (Vec::new(), Some(sysutil::errno_text(&error))),
    };
    let child = {
        let mut command = Command::new(&resolved);
        command
            .arg0(spawn.executable)
            .args(spawn.arguments)
            .stdin(Stdio::null());
        match writer.try_clone() {
            Ok(copy) => command.stdout(Stdio::from(copy)),
            Err(error) => return (Vec::new(), Some(sysutil::errno_text(&error))),
        };
        command.stderr(Stdio::from(writer));
        if !spawn.directory.is_empty() {
            command.current_dir(spawn.directory);
            // The child sees the new working directory in `PWD`.
            if let Ok(absolute) = crate::project::lexpath::abs(spawn.directory) {
                command.env("PWD", absolute);
            }
        }
        for (name, value) in spawn.environment {
            command.env(name, value);
        }
        if spawn.group {
            command.process_group(0);
        }
        // Dropping the command closes the parent's copies of the write end.
        command.spawn()
    };
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return (
                Vec::new(),
                Some(format!(
                    "fork/exec {resolved}: {}",
                    sysutil::errno_text(&error)
                )),
            );
        }
    };
    let pid = child.id() as i64;
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let (read_done, exited) = (Flag::new(), Flag::new());
    {
        let (buffer, read_done) = (buffer.clone(), read_done.clone());
        thread::spawn(move || {
            let mut chunk = [0u8; 65536];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => buffer.lock().unwrap().extend_from_slice(&chunk[..count]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            read_done.set();
        });
    }
    {
        let exited = exited.clone();
        thread::spawn(move || {
            // WNOWAIT leaves the zombie in place, so the PID stays reserved for
            // the signals below until `wait` reaps it.
            loop {
                // SAFETY: `info` is valid for writes; waitid has no other preconditions.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let status = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                if status == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            exited.set();
        });
    }
    cancel::select(&[Source::Flag(&exited), Source::Context(ctx)], None);
    let mut context_error = None;
    let mut timer: Option<Instant> = None;
    let mut pipes_closed = false;
    if !exited.is_set() {
        let target = if spawn.group { -pid } else { pid };
        match sysutil::kill(target, libc::SIGKILL) {
            Ok(()) => context_error = ctx.error().map(|error| error.to_string()),
            Err(error) => {
                context_error = Some(format!(
                    "canceling process: {}",
                    sysutil::errno_text(&error)
                ));
            }
        }
        if let Some(delay) = spawn.wait_delay {
            timer = Some(Instant::now() + delay);
            if !exited.wait(Some(delay)) {
                let _ = sysutil::kill(pid, libc::SIGKILL);
                // The pipe ends are closed here and waiting for output stops.
                pipes_closed = !read_done.is_set();
            }
        }
        exited.wait(None);
    }
    let mut error = match child.wait() {
        Ok(status) => sysutil::exit_error(status),
        Err(error) => Some(sysutil::errno_text(&error)),
    };
    if error.is_none() {
        error = context_error;
    }
    if !pipes_closed {
        match spawn.wait_delay {
            None => {
                read_done.wait(None);
            }
            Some(delay) => {
                let deadline = timer.unwrap_or_else(|| Instant::now() + delay);
                if !read_done.is_set()
                    && !read_done.wait(Some(deadline.saturating_duration_since(Instant::now())))
                    && error.is_none()
                {
                    error = Some("output did not close after the process ended".into());
                }
            }
        }
    }
    let output = buffer.lock().unwrap().clone();
    (output, error)
}

/// Runs an SDK tool in its own process group and kills
/// the whole group when the context is cancelled.
pub fn compiler_command(
    ctx: &Context,
    directory: &str,
    executable: &str,
    arguments: &[&str],
) -> std::result::Result<Vec<u8>, CommandError> {
    let (output, error) = combined_output(
        ctx,
        &Spawn {
            directory,
            executable,
            arguments,
            group: true,
            wait_delay: Some(Duration::from_secs(2)),
            environment: &[
                ("DOTNET_CLI_TELEMETRY_OPTOUT", "1"),
                ("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1"),
                ("DOTNET_NOLOGO", "1"),
            ],
        },
    );
    match error {
        None => Ok(output),
        Some(error) => Err(CommandError {
            message: format!(
                "{}: {error}\n{}",
                sysutil::base(executable),
                String::from_utf8_lossy(&output)
            ),
            output,
        }),
    }
}

fn pe_string(table: &[u8], start: u32) -> Option<()> {
    let start = start.checked_sub(4)?;
    (start as usize <= table.len()).then_some(())
}
fn c_string(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
}
/// A PE image that `debug/pe` accepts and whose CLI
/// header data directory (index 14) is populated.
pub fn managed_plugin(path: &str) -> bool {
    let Ok(data) = fs::read(path) else {
        return false;
    };
    managed_pe(&data).unwrap_or(false)
}
fn managed_pe(data: &[u8]) -> Option<bool> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?));
    data.get(..96)?;
    let mut base = 0usize;
    if data.starts_with(b"MZ") {
        let offset = u32_at(0x3c)? as usize;
        if data.get(offset..offset + 4) != Some(b"PE\0\0") {
            return None;
        }
        base = offset + 4;
    }
    data.get(base..base + 20)?;
    let machine = u16_at(base)?;
    if !matches!(
        machine,
        0x8664 | 0xaa64 | 0x1c4 | 0x14c | 0x5032 | 0x5064 | 0x5128 | 0
    ) {
        return None;
    }
    let sections = u16_at(base + 2)? as usize;
    let symbol_table = u32_at(base + 8)?;
    let symbol_count = u32_at(base + 12)?;
    let optional_size = u16_at(base + 16)? as usize;
    let mut strings: &[u8] = &[];
    if symbol_table > 0 {
        let offset = symbol_table.wrapping_add(18u32.wrapping_mul(symbol_count)) as usize;
        let length = u32_at(offset)?;
        if length > 4 {
            strings = data.get(offset + 4..offset + length as usize)?;
        }
        let mut auxiliary = 0usize;
        let mut symbols = Vec::new();
        for index in 0..symbol_count as usize {
            let at = symbol_table as usize + index * 18;
            let symbol = data.get(at..at + 18)?;
            if auxiliary == 0 {
                auxiliary = symbol[17] as usize;
            } else {
                auxiliary -= 1;
            }
            symbols.push(symbol);
        }
        if auxiliary != 0 {
            return None;
        }
        // removeAuxSymbols skips by each primary symbol's own aux count.
        let mut skip = 0u8;
        for symbol in symbols {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if symbol[..4] == [0; 4] {
                let offset = u32::from_le_bytes(symbol[4..8].try_into().ok()?);
                if offset != 0 {
                    pe_string(strings, offset)?;
                }
            }
            skip = symbol[17];
        }
    }
    let mut cursor = base + 20;
    let mut managed = false;
    if optional_size != 0 {
        if optional_size < 2 {
            return None;
        }
        let minimum = match u16_at(cursor)? {
            0x10b => 96,
            0x20b => 112,
            _ => return None,
        };
        if optional_size < minimum {
            return None;
        }
        data.get(cursor..cursor + minimum)?;
        let count = u32_at(cursor + minimum - 4)? as usize;
        if optional_size - minimum != count * 8 {
            return None;
        }
        data.get(cursor + minimum..cursor + minimum + count * 8)?;
        managed = count > 14
            && u32_at(cursor + minimum + 14 * 8)? != 0
            && u32_at(cursor + minimum + 14 * 8 + 4)? != 0;
        cursor += minimum + count * 8;
    }
    let mut relocations = Vec::new();
    for _ in 0..sections {
        let header = data.get(cursor..cursor + 40)?;
        if header[0] == b'/' {
            let text = std::str::from_utf8(c_string(&header[1..8])).ok()?;
            let index = crate::project::strconv::atoi(text).ok()?;
            pe_string(strings, index as u32)?;
        }
        relocations.push((
            u32::from_le_bytes(header[24..28].try_into().ok()?) as usize,
            u16::from_le_bytes(header[32..34].try_into().ok()?) as usize,
        ));
        cursor += 40;
    }
    for (offset, count) in relocations {
        if count > 0 {
            data.get(offset..offset + count * 10)?;
        }
    }
    Some(managed)
}
