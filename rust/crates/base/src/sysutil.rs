//! Lexical path, rooted-directory and process helpers shared by the Editor bridge
//! and the runtime controller. Paths are strings: Unix, UTF-8, lexical rules.
use crate::cancel::Canceled;
use std::{
    ffi::{CStr, CString, OsStr},
    fmt,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::ExitStatusExt},
    },
    process::ExitStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Other,
    Canceled,
    DeadlineExceeded,
    NotExist,
    Exist,
}
/// An error: a message plus a kind that callers can test (canceled, missing, existing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}
impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Other,
            message: message.into(),
        }
    }
    /// A message carrying a kind that callers can test.
    pub fn with_kind(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    /// An error for a failed OS operation on a path: `<operation> <path>: <errno text>`.
    pub fn os(operation: &str, path: &str, error: &io::Error) -> Self {
        let kind = match error.raw_os_error() {
            Some(libc::ENOENT) => ErrorKind::NotExist,
            Some(libc::EEXIST) => ErrorKind::Exist,
            _ => ErrorKind::Other,
        };
        Self {
            kind,
            message: format!("{operation} {path}: {}", errno_text(error)),
        }
    }
    /// The error with `prefix: ` in front of its message; the kind is kept.
    pub fn wrap(prefix: &str, error: &Error) -> Self {
        Self {
            kind: error.kind,
            message: format!("{prefix}: {}", error.message),
        }
    }
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Whether the operation was canceled.
    pub fn is_canceled(&self) -> bool {
        self.kind == ErrorKind::Canceled
    }
    /// Whether the operation's deadline passed.
    pub fn is_deadline_exceeded(&self) -> bool {
        self.kind == ErrorKind::DeadlineExceeded
    }
    pub fn is_not_exist(&self) -> bool {
        self.kind == ErrorKind::NotExist
    }
    pub fn is_exist(&self) -> bool {
        self.kind == ErrorKind::Exist
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<Canceled> for Error {
    fn from(error: Canceled) -> Self {
        Self {
            kind: match error {
                Canceled::Canceled => ErrorKind::Canceled,
                Canceled::DeadlineExceeded => ErrorKind::DeadlineExceeded,
            },
            message: error.to_string(),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// The OS error text: the C library message with a lower-case initial.
pub fn errno_text(error: &io::Error) -> String {
    let Some(code) = error.raw_os_error() else {
        return error.to_string();
    };
    let mut buffer = [0 as libc::c_char; 256];
    // SAFETY: the buffer is writable for its stated length and NUL-terminated on success.
    let text = unsafe {
        if libc::strerror_r(code, buffer.as_mut_ptr(), buffer.len()) != 0 {
            return format!("errno {code}");
        }
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_lowercase().chain(characters).collect(),
        None => text,
    }
}

pub fn is_abs(path: &str) -> bool {
    path.starts_with('/')
}
/// Lexically cleans a path: collapses separators, `.` and `..`; empty becomes `.`.
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if rooted => {}
                _ => parts.push(".."),
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}
/// Joins the non-empty parts and cleans the result.
pub fn join(parts: &[&str]) -> String {
    let parts: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return String::new();
    }
    clean(&parts.join("/"))
}
/// The cleaned path without its last element; `.` when there is no directory part.
pub fn dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => clean(&path[..index + 1]),
        None => ".".into(),
    }
}
/// The last element of the path; `.` for an empty path, `/` for the root.
pub fn base(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed.rsplit('/').next().unwrap().into()
}
/// The suffix from the final dot of the final path element.
pub fn ext(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rfind('.').map_or("", |index| &name[index..])
}
/// The relative path from base to target, lexically; `None` when there is none.
pub fn rel(base_path: &str, target: &str) -> Option<String> {
    let (base_path, target) = (clean(base_path), clean(target));
    if target == base_path {
        return Some(".".into());
    }
    let base_path = if base_path == "." { "" } else { &base_path };
    if is_abs(base_path) != is_abs(&target) {
        return None;
    }
    let from: Vec<&str> = base_path.split('/').filter(|p| !p.is_empty()).collect();
    let to: Vec<&str> = target.split('/').filter(|p| !p.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if from[common..].contains(&"..") {
        return None;
    }
    let mut parts = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    Some(parts.join("/"))
}
/// The containment test applied to the relative path from `root` to `path`.
pub fn within(root: &str, path: &str) -> bool {
    rel(root, path).is_some_and(|relative| {
        relative != ".." && !relative.starts_with("../") && !is_abs(&relative)
    })
}
/// A strict child of `Temp` or `Library/UnityMicroEditor` named by a clean absolute path.
pub fn is_owned_cache_path(project: &str, path: &str) -> bool {
    if !is_abs(project) || !is_abs(path) || clean(path) != path {
        return false;
    }
    [
        join(&[project, "Temp"]),
        join(&[project, "Library", "UnityMicroEditor"]),
    ]
    .iter()
    .any(|root| {
        rel(root, path).is_some_and(|relative| {
            relative != "."
                && relative != ".."
                && !is_abs(&relative)
                && !relative.starts_with("../")
        })
    })
}
/// Resolves symlinks of an existing path.
pub fn eval_symlinks(path: &str) -> Result<String> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved
            .into_os_string()
            .into_string()
            .map_err(|_| Error::new(format!("lstat {path}: path is not valid UTF-8"))),
        Err(error) => {
            // The error names the first component that could not be inspected.
            let cleaned = clean(path);
            let mut prefix = String::new();
            for part in cleaned.split('/').filter(|p| !p.is_empty()) {
                if is_abs(&cleaned) || !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if let Err(error) = std::fs::metadata(&prefix) {
                    return Err(Error::os("lstat", &prefix, &error));
                }
            }
            Err(Error::os("lstat", path, &error))
        }
    }
}
pub fn mkdir_all(path: &str) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| Error::os("mkdir", path, &error))
}

/// 16 random bytes in hex.
pub fn new_id() -> String {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: the pointer and length describe the unfilled tail of `bytes`.
        let count = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            assert!(
                error.kind() == io::ErrorKind::Interrupted,
                "system randomness unavailable: {error}"
            );
            continue;
        }
        filled += count as usize;
    }
    crate::hex(&bytes)
}
/// ASCII letters, digits or hyphens, 1 to 64 bytes.
pub fn valid_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub mode: u32,
    pub size: i64,
    pub device: u64,
    pub inode: u64,
    pub modified: (i64, i64),
}
impl Stat {
    fn from(raw: &libc::stat) -> Self {
        Self {
            mode: raw.st_mode,
            size: raw.st_size,
            device: raw.st_dev,
            inode: raw.st_ino,
            modified: (raw.st_mtime, raw.st_mtime_nsec),
        }
    }
    pub fn is_regular(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }
    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }
    /// Whether both describe the same file (same device and inode).
    pub fn same_file(&self, other: &Stat) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}
fn c_path(path: &str) -> io::Result<CString> {
    CString::new(path).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}
fn retry<T: PartialEq + From<i8>>(mut call: impl FnMut() -> T) -> io::Result<T> {
    loop {
        let result = call();
        if result != T::from(-1) {
            return Ok(result);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
pub fn fstat(file: &impl AsRawFd) -> io::Result<Stat> {
    // SAFETY: an all-zero `stat` is a valid value and the kernel fills it.
    let mut raw: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for the borrow and `raw` is writable.
    retry(|| unsafe { libc::fstat(file.as_raw_fd(), &mut raw) })?;
    Ok(Stat::from(&raw))
}
fn stat_at(directory: libc::c_int, path: &str, flags: libc::c_int) -> io::Result<Stat> {
    let name = c_path(path)?;
    // SAFETY: as in `fstat`; `name` is a NUL-terminated string.
    let mut raw: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: see above.
    retry(|| unsafe { libc::fstatat(directory, name.as_ptr(), &mut raw, flags) })?;
    Ok(Stat::from(&raw))
}
/// Stats a path without following a final symlink.
pub fn lstat(path: &str) -> io::Result<Stat> {
    stat_at(libc::AT_FDCWD, path, libc::AT_SYMLINK_NOFOLLOW)
}
pub fn stat(path: &str) -> io::Result<Stat> {
    stat_at(libc::AT_FDCWD, path, 0)
}

/// A directory root for the single-component names these modules use: every lookup is
/// relative to the directory descriptor and never follows a symlink.
pub struct Root {
    directory: OwnedFd,
}
fn component(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == ".." || name.contains('/') {
        // Names leaving the root are reported as "path escapes from parent".
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    c_path(name)
}
impl Root {
    /// Opens the directory at `path` as a root.
    pub fn open(path: &str) -> Result<Self> {
        let name = c_path(path).map_err(|error| Error::os("open", path, &error))?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: `name` is NUL-terminated; a successful result is a new descriptor.
        let descriptor = retry(|| unsafe { libc::open(name.as_ptr(), flags) })
            .map_err(|error| Error::os("open", path, &error))?;
        // SAFETY: the descriptor was just returned by `open` and is owned here.
        Ok(Self {
            directory: unsafe { OwnedFd::from_raw_fd(descriptor) },
        })
    }
    /// Opens a sub-root along a relative path free of symlinks.
    pub fn open_root(&self, relative: &str) -> Result<Self> {
        let mut current: Option<OwnedFd> = None;
        for part in relative.split('/').filter(|p| !p.is_empty() && *p != ".") {
            let name = component(part).map_err(|error| Error::os("openat", relative, &error))?;
            let parent = current.as_ref().unwrap_or(&self.directory).as_raw_fd();
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: `parent` stays open for the call and `name` is NUL-terminated.
            let descriptor = retry(|| unsafe { libc::openat(parent, name.as_ptr(), flags) })
                .map_err(|error| Error::os("openat", relative, &error))?;
            // SAFETY: the descriptor was just returned by `openat` and is owned here.
            current = Some(unsafe { OwnedFd::from_raw_fd(descriptor) });
        }
        match current {
            Some(directory) => Ok(Self { directory }),
            None => self
                .directory
                .try_clone()
                .map(|directory| Self { directory })
                .map_err(|error| Error::os("openat", relative, &error)),
        }
    }
    /// Opens a file in the root; `O_NOFOLLOW` and `O_CLOEXEC` are always applied.
    pub fn open_file(&self, name: &str, flags: libc::c_int, mode: libc::mode_t) -> Result<File> {
        let path = component(name).map_err(|error| Error::os("openat", name, &error))?;
        let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: the directory descriptor is open and `path` is NUL-terminated.
        let descriptor = retry(|| unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                path.as_ptr(),
                flags,
                mode as libc::c_uint,
            )
        })
        .map_err(|error| Error::os("openat", name, &error))?;
        // SAFETY: the descriptor was just returned by `openat` and is owned here.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
    /// Stats a name in the root without following a symlink.
    pub fn lstat(&self, name: &str) -> Result<Stat> {
        component(name)
            .and_then(|_| stat_at(self.directory.as_raw_fd(), name, libc::AT_SYMLINK_NOFOLLOW))
            .map_err(|error| Error::os("lstatat", name, &error))
    }
    /// Hard-links a name in the root; fails with an `Exist` error instead of replacing.
    pub fn link(&self, old: &str, new: &str) -> Result<()> {
        let describe = |error: io::Error| {
            let mut wrapped = Error::os("linkat", old, &error);
            wrapped.message = format!("linkat {old} {new}: {}", errno_text(&error));
            wrapped
        };
        let (from, to) = (
            component(old).map_err(describe)?,
            component(new).map_err(describe)?,
        );
        let directory = self.directory.as_raw_fd();
        // SAFETY: both names are NUL-terminated and the descriptor is open.
        retry(|| unsafe { libc::linkat(directory, from.as_ptr(), directory, to.as_ptr(), 0) })
            .map(drop)
            .map_err(describe)
    }
    /// Removes a file in the root.
    pub fn remove(&self, name: &str) -> Result<()> {
        let path = component(name).map_err(|error| Error::os("unlinkat", name, &error))?;
        // SAFETY: the directory descriptor is open and `path` is NUL-terminated.
        retry(|| unsafe { libc::unlinkat(self.directory.as_raw_fd(), path.as_ptr(), 0) })
            .map(drop)
            .map_err(|error| Error::os("unlinkat", name, &error))
    }
    fn reopen(&self) -> Result<OwnedFd> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: the directory descriptor is open and the name is a literal C string.
        let descriptor =
            retry(|| unsafe { libc::openat(self.directory.as_raw_fd(), c".".as_ptr(), flags) })
                .map_err(|error| Error::os("openat", ".", &error))?;
        // SAFETY: the descriptor was just returned by `openat` and is owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
    /// Entry names of the root directory itself, in directory order.
    pub fn names(&self) -> Result<Vec<String>> {
        let descriptor = self.reopen()?;
        let failure = |error: io::Error| Error::os("readdirent", ".", &error);
        // SAFETY: `fdopendir` takes ownership of the descriptor on success only.
        let stream = unsafe { libc::fdopendir(descriptor.as_raw_fd()) };
        if stream.is_null() {
            return Err(failure(io::Error::last_os_error()));
        }
        std::mem::forget(descriptor);
        let mut names = Vec::new();
        loop {
            // SAFETY: `stream` is a live directory stream used by this thread only;
            // errno is cleared to tell the end of the directory from a failure.
            let entry = unsafe {
                *libc::__errno_location() = 0;
                libc::readdir(stream)
            };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: the stream is closed exactly once.
                unsafe { libc::closedir(stream) };
                return match error.raw_os_error() {
                    Some(0) | None => Ok(names),
                    Some(_) => Err(failure(error)),
                };
            }
            // SAFETY: `d_name` is a NUL-terminated name inside the returned entry.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if !matches!(name.to_bytes(), b"." | b"..") {
                names.push(name.to_string_lossy().into_owned());
            }
        }
    }
    /// Reopens the root directory and syncs it.
    pub fn sync(&self) -> Result<()> {
        let descriptor = self.reopen()?;
        // SAFETY: the descriptor is open for the call.
        retry(|| unsafe { libc::fsync(descriptor.as_raw_fd()) })
            .map(drop)
            .map_err(|error| Error::os("sync", ".", &error))
    }
}

/// Start time and parent of a live process from `/proc/<pid>/stat`; the text
/// after the final `)` is split on whitespace.
pub fn parse_proc_stat(data: &[u8]) -> std::result::Result<(String, i64), &'static str> {
    let index = data
        .iter()
        .rposition(|&byte| byte == b')')
        .ok_or("invalid process identity")?;
    let text = String::from_utf8_lossy(&data[index + 1..]);
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 20 || fields[0] == "Z" {
        return Err("process is exited");
    }
    let parent = fields[1].parse().map_err(|_| "invalid process parent")?;
    Ok((fields[19].to_string(), parent))
}
pub fn os_str(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}
/// Gives a process the default handling of the signals it is stopped with.
/// For the child between fork and exec: a tool that was started in the
/// background of a shell without job control (`nohup tool &` in a script)
/// inherits SIGINT and SIGQUIT ignored, and an ignored signal stays ignored
/// across exec, where a shell script cannot even trap it. Such a child would
/// be stopped only by the escalation that follows the grace period.
///
/// # Safety
/// Async-signal-safe; changes the dispositions of the calling process.
pub unsafe fn default_stop_signals() {
    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        // SAFETY: `signal` with `SIG_DFL` installs no handler.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
}

/// Starts `command` as a process that the kernel kills when this process
/// ends, however it ends: a worker of a release whose engine does not watch
/// its parent (6000.0 has no `-parentPid`) outlived a session killed by a
/// signal, with its memory. The kernel ties a child to the thread that
/// started it (`PR_SET_PDEATHSIG`), so every such child is started by one
/// thread that lives as long as the process.
pub fn spawn_tied(command: std::process::Command) -> io::Result<std::process::Child> {
    use std::{
        os::unix::process::CommandExt,
        sync::{Mutex, OnceLock, mpsc},
    };
    type Request = (
        std::process::Command,
        mpsc::Sender<io::Result<std::process::Child>>,
    );
    static SPAWNER: OnceLock<Mutex<mpsc::Sender<Request>>> = OnceLock::new();
    let spawner = SPAWNER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("ume-spawner".into())
            .spawn(move || {
                // SAFETY: getpid has no preconditions.
                let parent = unsafe { libc::getpid() };
                for (mut command, reply) in receiver {
                    // SAFETY: the hook runs between fork and exec and makes
                    // only async-signal-safe system calls.
                    unsafe {
                        command.pre_exec(move || {
                            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong)
                                != 0
                            {
                                return Err(io::Error::last_os_error());
                            }
                            // The process ended before the call above.
                            if libc::getppid() != parent {
                                libc::kill(libc::getpid(), libc::SIGKILL);
                            }
                            Ok(())
                        });
                    }
                    let _ = reply.send(command.spawn());
                }
            })
            .expect("start the spawner thread");
        Mutex::new(sender)
    });
    let (reply, answer) = mpsc::channel();
    spawner
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .send((command, reply))
        .map_err(|_| io::Error::other("the spawner thread ended"))?;
    answer
        .recv()
        .map_err(|_| io::Error::other("the spawner thread ended"))?
}

/// Sends a signal; a PID outside the kernel's range names no process.
pub fn kill(pid: i64, signal: libc::c_int) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
    // SAFETY: `kill` has no memory-safety preconditions.
    match unsafe { libc::kill(pid, signal) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// The text of a failed exit (`exit status N` or the signal name); `None` for a successful exit.
pub fn exit_error(status: ExitStatus) -> Option<String> {
    if status.success() {
        return None;
    }
    if let Some(code) = status.code() {
        return Some(format!("exit status {code}"));
    }
    let signal = status.signal().unwrap_or(0);
    const NAMES: [&str; 32] = [
        "",
        "hangup",
        "interrupt",
        "quit",
        "illegal instruction",
        "trace/breakpoint trap",
        "aborted",
        "bus error",
        "floating point exception",
        "killed",
        "user defined signal 1",
        "segmentation fault",
        "user defined signal 2",
        "broken pipe",
        "alarm clock",
        "terminated",
        "stack fault",
        "child exited",
        "continued",
        "stopped (signal)",
        "stopped",
        "stopped (tty input)",
        "stopped (tty output)",
        "urgent I/O condition",
        "CPU time limit exceeded",
        "file size limit exceeded",
        "virtual timer expired",
        "profiling timer expired",
        "window changed",
        "I/O possible",
        "power failure",
        "bad system call",
    ];
    let name = match NAMES.get(signal as usize) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => format!("signal {signal}"),
    };
    let core = if status.core_dumped() {
        " (core dumped)"
    } else {
        ""
    };
    Some(format!("signal: {name}{core}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run by the test below in a process of its own: starts a tied child
    /// and exits at once, as a process killed by a signal ends.
    #[test]
    fn tied_child_helper() {
        if std::env::var_os("UME_TIED_HELPER").is_none() {
            return;
        }
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        let child = spawn_tied(command).unwrap();
        println!("child {}", child.id());
        std::process::exit(0);
    }

    #[test]
    fn a_tied_child_ends_with_its_process() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sysutil::tests::tied_child_helper",
                "--nocapture",
            ])
            .env("UME_TIED_HELPER", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let pid: i64 = text
            .lines()
            .find_map(|line| line.strip_prefix("child "))
            .unwrap_or_else(|| panic!("no child in {text}"))
            .trim()
            .parse()
            .unwrap();
        let gone = |pid: i64| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_or(true, |stat| stat.contains(") Z "))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gone(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone(pid), "the child {pid} outlived its process");
    }

    #[test]
    fn lexical_paths_are_cleaned() {
        for (input, cleaned) in [
            ("", "."),
            ("/", "/"),
            ("//a//b/./c/..", "/a/b"),
            ("/../a", "/a"),
            ("a/../..", ".."),
            ("a/", "a"),
            ("../../a", "../../a"),
            ("./", "."),
        ] {
            assert_eq!(clean(input), cleaned, "{input:?}");
        }
        assert_eq!(join(&["/a", "", "../b", "c/"]), "/b/c");
        assert_eq!(join(&["", ""]), "");
        assert_eq!(
            (dir("/a/b"), dir("/a"), dir("a"), dir("/a/b/")),
            ("/a".into(), "/".into(), ".".into(), "/a/b".into())
        );
        assert_eq!(
            (base("/a/b/"), base("/"), base("")),
            ("b".into(), "/".into(), ".".into())
        );
        assert_eq!(
            (ext("/a.b/c"), ext("a/b.tar.gz"), ext("a/.unity")),
            ("", ".gz", ".unity")
        );
        assert_eq!(rel("/a/b", "/a/b/c/d").as_deref(), Some("c/d"));
        assert_eq!(rel("/a/b", "/a/c").as_deref(), Some("../c"));
        assert_eq!(rel("/a/b", "/a/b").as_deref(), Some("."));
        assert_eq!(rel("/a", "b"), None);
        assert_eq!(rel("/", "/a").as_deref(), Some("a"));
        assert!(within("/a/b", "/a/b") && within("/a/b", "/a/b/..c") && !within("/a/b", "/a/bc"));
    }

    #[test]
    fn owned_cache_paths_are_strict_clean_children() {
        for (path, owned) in [
            ("/p/Temp/agent", true),
            ("/p/Library/UnityMicroEditor/agent", true),
            ("/p/Temp", false),
            ("/p/Library/UnityMicroEditor", false),
            ("/p/Library/ScriptAssemblies/agent", false),
            ("/p/Library/UnityMicroEditor-other/agent", false),
            ("/p/Temp/agent/", false),
            ("/p/Temp//agent", false),
            ("/p/Temp/../Temp/agent", false),
            ("Temp/agent", false),
        ] {
            assert_eq!(is_owned_cache_path("/p", path), owned, "{path}");
        }
        assert!(!is_owned_cache_path("p", "/p/Temp/agent"));
    }

    #[test]
    fn identifiers_and_process_fields() {
        let id = new_id();
        assert!(id.len() == 32 && valid_id(&id) && id != new_id());
        assert!(
            valid_id("a-Z09") && !valid_id("") && !valid_id("a_b") && !valid_id(&"a".repeat(65))
        );
        let stat = b"12 (a) b) S 7 12 12 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 987654 1 1";
        assert_eq!(parse_proc_stat(stat), Ok(("987654".into(), 7)));
        assert!(
            parse_proc_stat(b"12 (a) Z 7 12 12 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 9 1 1")
                .is_err()
        );
    }
}

/// Fake-process plumbing shared by the bridge and controller tests.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::{
        io::Write,
        process::{Command, Stdio},
        sync::atomic::{AtomicBool, Ordering},
    };

    /// Writes an executable through a child shell. The write descriptor never
    /// exists in this process, so a concurrent fork in another test cannot hold
    /// it open and make the following exec fail with "text file busy".
    pub fn write_executable(path: &str, content: &str) {
        let mut shell = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "sh", path])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        shell
            .stdin
            .take()
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
        assert!(shell.wait().unwrap().success());
    }
    pub fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
    /// Command line that re-executes this test binary as one helper test.
    pub fn helper_arguments(test: &str) -> [String; 5] {
        ["--exact", test, "--nocapture", "--test-threads=1", "--"].map(String::from)
    }
    /// Runs one test of this binary in a process that ignores SIGINT and
    /// SIGQUIT, as one started in the background of a script does, and
    /// panics unless it passes. Returns how long it took.
    pub fn pass_ignoring_interrupts(test: &str) -> std::time::Duration {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(helper_arguments(test));
        // SAFETY: `signal` is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGQUIT, libc::SIG_IGN);
                Ok(())
            });
        }
        let started = std::time::Instant::now();
        let output = command.output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && text.contains("1 passed"),
            "{text}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        started.elapsed()
    }
    /// A fresh symlink-free directory.
    pub fn temp_dir() -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(directory.path()).unwrap();
        (directory, path.into_os_string().into_string().unwrap())
    }

    static SIGNALLED: AtomicBool = AtomicBool::new(false);
    extern "C" fn record(_: libc::c_int) {
        SIGNALLED.store(true, Ordering::SeqCst);
    }
    /// Records SIGINT and SIGTERM instead of dying, for a helper process.
    pub fn notify_signals() {
        let handler = record as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
    }
    pub fn signalled() -> bool {
        SIGNALLED.swap(false, Ordering::SeqCst)
    }
}

/// Recorded reference cases: `recorded-cases.json` holds each input with the
/// expected output or error.
#[cfg(any(test, feature = "testing"))]
pub mod oracle {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::Value;

    pub struct Case {
        input: Value,
        output: Vec<u8>,
        pub error: String,
    }
    impl Case {
        pub fn text(&self, key: &str) -> &str {
            self.input[key]
                .as_str()
                .unwrap_or_else(|| panic!("recorded input {key}"))
        }
        pub fn bytes(&self, key: &str) -> Vec<u8> {
            STANDARD.decode(self.text(key)).unwrap()
        }
        pub fn bytes_output(&self) -> Vec<u8> {
            self.output.clone()
        }
        pub fn number(&self, key: &str) -> i64 {
            self.text(key).parse().unwrap()
        }
    }
    /// Runs `actual` on every case of one kind and reports all disagreements.
    pub fn compare(kind: &str, mut actual: impl FnMut(&Case) -> Result<Vec<u8>, String>) {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/tests/testdata/bridge/recorded-cases.json"
        );
        let cases: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let (mut count, mut failures) = (0, Vec::new());
        for entry in cases
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] == kind)
        {
            let case = Case {
                input: entry["input"].clone(),
                output: STANDARD.decode(entry["output"].as_str().unwrap()).unwrap(),
                error: entry["error"].as_str().unwrap().to_string(),
            };
            count += 1;
            let expected = match case.error.is_empty() {
                true => Ok(case.output.clone()),
                false => Err(case.error.clone()),
            };
            let outcome = actual(&case);
            if outcome != expected {
                let show = |value: &Result<Vec<u8>, String>| match value {
                    Ok(bytes) => format!(
                        "ok  {}",
                        String::from_utf8_lossy(bytes)
                            .chars()
                            .take(400)
                            .collect::<String>()
                    ),
                    Err(error) => format!("err {}", error.chars().take(400).collect::<String>()),
                };
                let input: String = entry["input"].to_string().chars().take(300).collect();
                failures.push(format!(
                    "{kind} #{count} {input}\n   expected: {}\n   actual:   {}",
                    show(&expected),
                    show(&outcome)
                ));
            }
        }
        assert!(count > 0, "no recorded cases of kind {kind}");
        assert!(
            failures.is_empty(),
            "{} of {count} cases differ from the recorded output:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
