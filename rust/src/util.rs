//! Small helpers shared by the command implementations.
use crate::{ENGINE_VERSION, authoring::AuthoringError, hex, jsondoc, sysutil};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Resolves existing ancestors without creating files.
pub fn canonical_destination(path: &str) -> Result<String> {
    let mut missing: Vec<String> = Vec::new();
    let mut current = path.to_string();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(_) => {
                let mut resolved = sysutil::eval_symlinks(&current).map_err(|e| anyhow!("{e}"))?;
                for part in missing.iter().rev() {
                    resolved = sysutil::join(&[&resolved, part]);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => bail!("lstat {current}: {}", sysutil::errno_text(&error)),
        }
        let parent = sysutil::dir(&current);
        ensure!(parent != current, "no existing ancestor for {path}");
        missing.push(sysutil::base(&current));
        current = parent;
    }
}

pub fn within_directory(path: &str, parent: &str) -> bool {
    sysutil::rel(parent, path).is_some_and(|relative| {
        relative != ".." && !relative.starts_with("../") && !sysutil::is_abs(&relative)
    })
}

fn checked_tool_root(candidate: &str) -> Result<String> {
    let root = crate::paths::lexical_abs(Path::new(candidate))?
        .to_string_lossy()
        .into_owned();
    for file in [
        "engine-profile.json",
        "tools/LifecycleCompiler/Program.cs",
        "Packages/com.unitymicroeditor.runtime/package.json",
    ] {
        ensure!(
            fs::metadata(sysutil::join(&[&root, file])).is_ok_and(|info| info.is_file()),
            "invalid Unity Micro Editor installation: {root}"
        );
    }
    Ok(root)
}

/// Tool sources belong to the installation, never the target project.
pub fn micro_editor_root() -> Result<String> {
    if let Some(configured) = env::var("UNITY_MICRO_EDITOR_ROOT")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return checked_tool_root(&configured);
    }
    let mut starts = Vec::new();
    if let Ok(current) = env::current_dir() {
        starts.push(current.to_string_lossy().into_owned());
    }
    if let Ok(executable) = env::current_exe() {
        starts.push(sysutil::dir(&executable.to_string_lossy()));
    }
    for start in starts {
        let mut current = start;
        loop {
            for candidate in [
                current.clone(),
                sysutil::join(&[&current, "UnityMicroEditor"]),
            ] {
                if let Ok(root) = checked_tool_root(&candidate) {
                    return Ok(root);
                }
            }
            let parent = sysutil::dir(&current);
            if parent == current {
                break;
            }
            current = parent;
        }
    }
    bail!(
        "Unity Micro Editor sources not found; set UNITY_MICRO_EDITOR_ROOT to the installation directory"
    )
}

pub fn default_unity_editor() -> String {
    if let Some(configured) = env::var("UNITY_EDITOR").ok().filter(|v| !v.is_empty()) {
        return configured;
    }
    let home = env::var("HOME").unwrap_or_default();
    sysutil::join(&[&home, "Unity/Hub/Editor", ENGINE_VERSION, "Editor/Unity"])
}

/// The OS releases this lock even after an unclean exit.
pub fn lock_session(output: &str) -> Result<File> {
    let path = sysutil::join(&[output, ".session.lock"]);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| {
            anyhow!(
                "open session lock: open {path}: {}",
                sysutil::errno_text(&e)
            )
        })?;
    file.try_lock().map_err(|error| {
        let text = match error {
            fs::TryLockError::WouldBlock => "resource temporarily unavailable".to_string(),
            fs::TryLockError::Error(error) => sysutil::errno_text(&error),
        };
        anyhow!("Session output is already in use ({output}): {text}")
    })?;
    Ok(file)
}

/// Whether another process holds the session lock of `output`, with its
/// PID when /proc/locks names it. Read from /proc/locks, so that the check
/// takes no lock itself: a lock taken here for a moment could be inherited
/// by a child that another thread forks meanwhile, and a runner started
/// next would find it held. Without /proc/locks the lock is tried.
pub fn session_lock_holder(output: &str) -> Option<Option<i32>> {
    use std::os::unix::fs::MetadataExt;
    let path = sysutil::join(&[output, ".session.lock"]);
    let info = fs::metadata(&path).ok()?;
    let (major, minor) = (libc::major(info.dev()), libc::minor(info.dev()));
    let Ok(locks) = fs::read_to_string("/proc/locks") else {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .ok()?;
        return matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)).then_some(None);
    };
    locks.lines().find_map(|line| {
        // `1: FLOCK  ADVISORY  WRITE 4242 fd:01:1234 0 EOF`; a waiter's
        // line has `->` after the number.
        let fields: Vec<&str> = line
            .split_whitespace()
            .filter(|field| *field != "->")
            .collect();
        if fields.get(1) != Some(&"FLOCK") {
            return None;
        }
        let (pid, at) = (fields.get(4)?, fields.get(5)?);
        let mut parts = at.split(':');
        let (lock_major, lock_minor, lock_inode) = (parts.next()?, parts.next()?, parts.next()?);
        let same = u32::from_str_radix(lock_major, 16).ok()? == major
            && u32::from_str_radix(lock_minor, 16).ok()? == minor
            && lock_inode.parse::<u64>().ok()? == info.ino();
        same.then(|| pid.parse::<i32>().ok())
    })
}

/// An OS error carrying operation, path and cause in its text; whether the path
/// was missing stays answerable through `downcast_ref::<sysutil::Error>()`.
pub(crate) fn os_error(operation: &str, path: &str, error: &io::Error) -> anyhow::Error {
    anyhow::Error::new(sysutil::Error::os(operation, path, error))
}

/// 64 lowercase hexadecimal characters.
pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// SHA256 of a bounded regular file.
/// The SHA-256 of a file, taken from [`crate::hashstore`] while the file is
/// as it was when hashed.
pub fn file_hash(filename: &str) -> Result<String> {
    crate::hashstore::file_sha256(filename)
}

pub fn random_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", std::process::id())
}

/// 16 random bytes in hexadecimal.
pub fn agent_id() -> String {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("system random source");
    hex(&bytes)
}

/// Compact JSON bytes with HTML escaping and declared field order, as persisted.
pub fn marshal<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    jsondoc::marshal(value).map_err(|e| anyhow!(e))
}

pub fn read_json_file<T: DeserializeOwned>(path: &str) -> Result<T> {
    let data = fs::read(path).map_err(|e| os_error("open", path, &e))?;
    Ok(serde_json::from_slice(&data)?)
}

/// Compact JSON plus newline, renamed into place, mode 0600.
pub fn atomic_json<T: Serialize + ?Sized>(path: &str, value: &T) -> Result<()> {
    let mut data = marshal(value)?;
    data.push(b'\n');
    let temporary = format!("{path}.{}.tmp", random_id());
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(&data));
    if let Err(error) = written {
        bail!("open {temporary}: {}", sysutil::errno_text(&error));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        bail!("rename {temporary} {path}: {}", sysutil::errno_text(&error));
    }
    Ok(())
}

/// One compact JSON line on stdout.
pub fn emit_event<T: Serialize + ?Sized>(value: &T) {
    if let Ok(mut data) = marshal(value) {
        data.push(b'\n');
        let written = io::stdout().lock().write_all(&data);
        // Die of SIGPIPE when standard output is a broken pipe; a runner
        // whose reader is gone must not keep its Player alive.
        if written.is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe) {
            // SAFETY: restoring the default disposition and raising the signal.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::raise(libc::SIGPIPE);
            }
        }
    }
}

pub fn agent_failure(code: &str, message: &str) -> AuthoringError {
    AuthoringError {
        code: code.into(),
        message: message.into(),
        details: None,
    }
}

/// The path must not cross a symlink or contain redundant parts.
pub fn agent_canonical(path: &str) -> Result<()> {
    ensure!(
        canonical_destination(path)? == path,
        "agent path is not canonical or crosses a symlink: {path}"
    );
    Ok(())
}

pub fn read_agent_file(path: &str, limit: u64) -> Result<Vec<u8>> {
    agent_canonical(path)?;
    let info = fs::symlink_metadata(path).map_err(|e| os_error("lstat", path, &e))?;
    ensure!(
        info.is_file() && info.len() <= limit,
        "expected a bounded regular file: {path}"
    );
    fs::read(path).map_err(|e| anyhow!("open {path}: {}", sysutil::errno_text(&e)))
}

/// Exactly one JSON value.
pub fn agent_read_json<T: DeserializeOwned>(path: &str, limit: u64) -> Result<T> {
    let data = read_agent_file(path, limit)?;
    let mut stream = serde_json::Deserializer::from_slice(&data).into_iter::<T>();
    let value = stream.next().context("EOF")??;
    let rest = &data[stream.byte_offset()..];
    ensure!(
        rest.iter()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')),
        "unexpected JSON content"
    );
    Ok(value)
}

/// Publishes once; an identical existing file is accepted.
pub fn agent_immutable_json<T: Serialize + ?Sized>(path: &str, value: &T) -> Result<()> {
    let mut data = marshal(value)?;
    data.push(b'\n');
    agent_canonical(path)?;
    let parent = sysutil::dir(path);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&parent)
        .map_err(|e| anyhow!("mkdir {parent}: {}", sysutil::errno_text(&e)))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".immutable-agent-")
        .tempfile_in(&parent)?;
    temporary.write_all(&data)?;
    temporary.as_file().sync_all()?;
    match fs::hard_link(temporary.path(), path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if read_agent_file(path, 80 << 20)? == data {
                return Ok(());
            }
            return Err(agent_failure(
                "request_id_conflict",
                "request identity already belongs to different arguments or result",
            )
            .into());
        }
        Err(error) => bail!(
            "link {} {path}: {}",
            temporary.path().display(),
            sysutil::errno_text(&error)
        ),
    }
    File::open(&parent)?.sync_all()?;
    Ok(())
}

/// The running program: its version, and the size and time of its file,
/// which a build of another revision changes. What is remembered across
/// starts as made by this program is made again by another.
pub fn tool_identity() -> Option<&'static str> {
    static TOOL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TOOL.get_or_init(|| {
        let info = fs::metadata(env::current_exe().ok()?).ok()?;
        let modified = info.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(format!(
            "{}\0{}\0{}",
            env!("CARGO_PKG_VERSION"),
            info.len(),
            modified.as_nanos()
        ))
    })
    .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_paths_and_immutable_json() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let target = format!("{root}/a/b/value.json");
        assert_eq!(canonical_destination(&target).unwrap(), target);
        assert!(within_directory(&target, &root));
        assert!(!within_directory(&root, &target));
        agent_immutable_json(&target, &serde_json::json!({"a": "<"})).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"{\"a\":\"\\u003c\"}\n");
        agent_immutable_json(&target, &serde_json::json!({"a": "<"})).unwrap();
        let conflict = agent_immutable_json(&target, &serde_json::json!({"a": 1})).unwrap_err();
        assert_eq!(
            conflict.downcast_ref::<AuthoringError>().unwrap().code,
            "request_id_conflict"
        );
        let value: serde_json::Value = agent_read_json(&target, 1 << 20).unwrap();
        assert_eq!(value["a"], "<");
        std::os::unix::fs::symlink(format!("{root}/a"), format!("{root}/link")).unwrap();
        assert!(agent_canonical(&format!("{root}/link/b/value.json")).is_err());
        assert!(valid_sha256(&file_hash(&target).unwrap()));
        assert!(!valid_sha256("ABC"));
        let lock = lock_session(&root).unwrap();
        assert!(lock_session(&root).is_err());
        drop(lock);
        atomic_json(&format!("{root}/atomic.json"), &[1, 2]).unwrap();
        assert_eq!(
            read_json_file::<Vec<i32>>(&format!("{root}/atomic.json")).unwrap(),
            [1, 2]
        );
    }
}
