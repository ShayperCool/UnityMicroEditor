//! The inotify watcher over a project's source directories.
use super::{Wake, os_error, relevant_source};
use crate::{
    cancel::{self, Context, Flag, Source},
    sysutil,
};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::CString,
    fs::File,
    io::{ErrorKind, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

/// The events queue holds this many changes.
const CAPACITY: usize = 8192;
const MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_ONLYDIR;

/// A changed path with the time it was observed, or the error
/// that ended the watch.
#[derive(Debug, Clone)]
pub struct SourceChange {
    pub path: String,
    pub at: Instant,
    pub wall: SystemTime,
    pub error: Option<String>,
}
impl SourceChange {
    fn failed(error: String) -> Self {
        Self {
            path: String::new(),
            at: Instant::now(),
            wall: SystemTime::now(),
            error: Some(error),
        }
    }
}

struct Shared {
    inotify: File,
    /// Wakes the reader out of `poll` when the watcher closes.
    wakeup: File,
    queue: Mutex<VecDeque<SourceChange>>,
    space: Condvar,
    closed: Flag,
    ctx: Context,
    wake: Arc<Wake>,
}
impl Shared {
    fn stopped(&self) -> bool {
        self.closed.is_set() || self.ctx.error().is_some()
    }
    /// `send`: blocks while the channel is full, until the watch ends.
    fn send(&self, event: SourceChange) {
        let mut queue = self.queue.lock().unwrap();
        while queue.len() >= CAPACITY {
            if self.stopped() {
                return;
            }
            queue = self.space.wait(queue).unwrap();
        }
        queue.push_back(event);
        drop(queue);
        self.wake.notify();
    }
    fn interrupt(&self) {
        let _ = (&self.wakeup).write_all(&1u64.to_ne_bytes());
        drop(self.queue.lock().unwrap());
        self.space.notify_all();
    }
}

/// The inotify watcher over a project's source directories.
pub struct ProjectWatcher {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

fn add_watch(inotify: &File, path: &str) -> Result<i32> {
    let name = CString::new(path).map_err(|_| anyhow!("invalid argument"))?;
    // SAFETY: `name` is a valid NUL-terminated path for the duration of the call.
    let descriptor = unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), name.as_ptr(), MASK) };
    if descriptor < 0 {
        bail!(sysutil::errno_text(&std::io::Error::last_os_error()));
    }
    Ok(descriptor)
}

/// Watches a directory and everything below it, in walk order
/// (names in byte order); symlinks are refused.
fn add_tree(inotify: &File, directories: &mut BTreeMap<i32, String>, root: &str) -> Result<()> {
    let info = sysutil::lstat(root).map_err(|e| os_error("lstat", root, &e))?;
    let mut pending = vec![(root.to_string(), info.is_symlink(), info.is_dir())];
    while let Some((path, symlink, directory)) = pending.pop() {
        if symlink {
            bail!("source watch rejects symlinks: {path}");
        }
        if !directory {
            continue;
        }
        directories.insert(add_watch(inotify, &path)?, path.clone());
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(|e| os_error("open", &path, &e))? {
            let entry = entry.map_err(|e| os_error("readdirent", &path, &e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry
                .file_type()
                .map_err(|e| os_error("lstat", &sysutil::join(&[&path, &name]), &e))?;
            entries.push((name, kind.is_symlink(), kind.is_dir()));
        }
        // The stack is popped from the end: push in reverse for lexical order.
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        for (name, symlink, directory) in entries {
            pending.push((sysutil::join(&[&path, &name]), symlink, directory));
        }
    }
    Ok(())
}

impl ProjectWatcher {
    /// Watches `Assets`, `Packages` and `ProjectSettings`
    /// until `ctx` ends or the watcher is dropped. Every queued change bumps `wake`.
    pub fn new(ctx: &Context, wake: &Arc<Wake>, project: &str) -> Result<Self> {
        // SAFETY: plain system calls; the descriptors are owned by the `File`s.
        let inotify = unsafe {
            let fd = libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK);
            if fd < 0 {
                bail!(sysutil::errno_text(&std::io::Error::last_os_error()));
            }
            File::from_raw_fd(fd)
        };
        // SAFETY: as above.
        let wakeup = unsafe {
            let fd = libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK);
            if fd < 0 {
                bail!(sysutil::errno_text(&std::io::Error::last_os_error()));
            }
            File::from_raw_fd(fd)
        };
        let mut directories = BTreeMap::new();
        for name in ["Assets", "Packages", "ProjectSettings"] {
            let root = sysutil::join(&[project, name]);
            if std::fs::metadata(&root).is_err_and(|e| e.kind() == ErrorKind::NotFound) {
                continue;
            }
            add_tree(&inotify, &mut directories, &root)?;
        }
        let shared = Arc::new(Shared {
            inotify,
            wakeup,
            queue: Mutex::new(VecDeque::new()),
            space: Condvar::new(),
            closed: Flag::new(),
            ctx: ctx.clone(),
            wake: wake.clone(),
        });
        let reader = {
            let shared = shared.clone();
            std::thread::spawn(move || read(&shared, directories))
        };
        // Interrupts the reader when the context ends or the watch is closed.
        let closer = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                cancel::select(
                    &[Source::Context(&shared.ctx), Source::Flag(&shared.closed)],
                    None,
                );
                shared.interrupt();
            })
        };
        Ok(Self {
            shared,
            threads: vec![reader, closer],
        })
    }

    /// A receive from the events channel that does not block.
    pub fn try_recv(&self) -> Option<SourceChange> {
        let event = self.shared.queue.lock().unwrap().pop_front();
        if event.is_some() {
            self.shared.space.notify_one();
        }
        event
    }

    /// Whether a change waits to be received.
    pub fn pending(&self) -> bool {
        !self.shared.queue.lock().unwrap().is_empty()
    }

    /// Waits for the next change for at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<SourceChange> {
        let limit = Instant::now() + timeout;
        loop {
            let seen = self.shared.wake.seen();
            if let Some(event) = self.try_recv() {
                return Some(event);
            }
            if Instant::now() >= limit {
                return None;
            }
            self.shared.wake.wait(seen, Some(limit));
        }
    }
}
impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        self.shared.closed.set();
        self.shared.interrupt();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn read(shared: &Shared, mut directories: BTreeMap<i32, String>) {
    let mut buffer = vec![0u8; 256 << 10];
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: shared.inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: shared.wakeup.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `descriptors` is a valid array of two entries.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if shared.stopped() {
            return;
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return shared.send(SourceChange::failed(format!(
                "read project-inotify: {}",
                sysutil::errno_text(&error)
            )));
        }
        let n = match (&shared.inotify).read(&mut buffer) {
            Ok(0) => return shared.send(SourceChange::failed("EOF".into())),
            Ok(n) => n,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                continue;
            }
            Err(error) => {
                return shared.send(SourceChange::failed(format!(
                    "read project-inotify: {}",
                    sysutil::errno_text(&error)
                )));
            }
        };
        let mut offset = 0;
        while offset + 16 <= n {
            let word = |at: usize| {
                u32::from_le_bytes(buffer[offset + at..offset + at + 4].try_into().unwrap())
            };
            let descriptor = word(0) as i32;
            let mask = word(4);
            let size = word(12) as usize;
            if size > n - offset - 16 {
                return shared.send(SourceChange::failed("truncated inotify event".into()));
            }
            let raw = &buffer[offset + 16..offset + 16 + size];
            let end = raw.iter().rposition(|&byte| byte != 0).map_or(0, |i| i + 1);
            let name = String::from_utf8_lossy(&raw[..end]).into_owned();
            offset += 16 + size;
            if mask & libc::IN_Q_OVERFLOW != 0 {
                return shared.send(SourceChange::failed(
                    "inotify queue overflow; restart to rescan source cache".into(),
                ));
            }
            let Some(directory) = directories.get(&descriptor) else {
                continue;
            };
            let path = sysutil::join(&[directory, &name]);
            if mask & libc::IN_ISDIR != 0
                && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
                && let Err(error) = add_tree(&shared.inotify, &mut directories, &path)
            {
                return shared.send(SourceChange::failed(format!("{error:#}")));
            }
            if mask & libc::IN_IGNORED != 0 {
                directories.remove(&descriptor);
                continue;
            }
            if mask & libc::IN_ISDIR != 0 || relevant_source(&path) {
                shared.send(SourceChange {
                    path,
                    at: Instant::now(),
                    wall: SystemTime::now(),
                    error: None,
                });
            }
        }
    }
}
