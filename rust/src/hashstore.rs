//! The SHA-256 of files, kept across processes while a file's metadata says
//! it is the file that was hashed: the way Unity's asset database and git
//! know what changed without reading it. A start of a session hashed some
//! 90 000 files, 3.5 GB, every time; now it reads their metadata.
//!
//! A hash is taken back only for the same device, inode, size, mode,
//! modification and change time. It is kept only when neither time was
//! within two seconds of the moment the file was read: a write in the same
//! tick of the clock could otherwise leave every field as it was. The change
//! time cannot be set back by a program, so a file rewritten in place under
//! its old modification time is hashed again.
//!
//! The hashes are appended, one line each, to a log in the user's cache
//! (`$XDG_CACHE_HOME/unity-micro-editor/file-hashes-v1.log`) that every
//! process reads at its first hash. `UME_HASH_CACHE=0` turns it off.
use crate::{cache::Identity, hex};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

/// What a file must have stayed as for its hash to be kept, in seconds.
const SETTLED: f64 = 2.0;
/// The largest file hashed, as for any input.
const LIMIT: u64 = 1024 * 1024 * 1024;
const VERSION: &str = "v1";

/// Kept hashes by path, and the log they are appended to.
pub(crate) struct Store {
    log: Option<PathBuf>,
    hashes: HashMap<PathBuf, (Identity, String)>,
    appender: Option<File>,
    loaded: bool,
}

impl Store {
    /// A store with its log at `log`, or only in memory.
    pub(crate) fn new(log: Option<PathBuf>) -> Self {
        Self {
            log,
            hashes: HashMap::new(),
            appender: None,
            loaded: false,
        }
    }

    fn load(&mut self) {
        if std::mem::replace(&mut self.loaded, true) {
            return;
        }
        let Some(log) = self.log.clone() else {
            return;
        };
        let Ok(file) = File::open(&log) else {
            return;
        };
        let mut lines = 0usize;
        for line in io::BufReader::new(file).split(b'\n').map_while(Result::ok) {
            lines += 1;
            if let Some((path, identity, sha)) = parse(&line) {
                self.hashes.insert(path, (identity, sha));
            }
        }
        // A log that holds mostly superseded lines is written again.
        if lines > 100_000 && lines > 4 * self.hashes.len() {
            self.compact(&log);
        }
    }

    fn compact(&mut self, log: &Path) {
        let temporary = log.with_extension(format!("{}.tmp", crate::util::random_id()));
        let written = File::create(&temporary).and_then(|file| {
            let mut writer = io::BufWriter::new(file);
            for (path, (identity, sha)) in &self.hashes {
                if let Some(line) = line(path, identity, sha) {
                    writer.write_all(&line)?;
                }
            }
            writer.flush()
        });
        match written.and_then(|()| std::fs::rename(&temporary, log)) {
            Ok(()) => self.appender = None,
            Err(_) => {
                let _ = std::fs::remove_file(&temporary);
            }
        }
    }

    /// The kept hash of a file as `identity` describes it now.
    pub(crate) fn kept(&mut self, path: &Path, identity: &Identity) -> Option<String> {
        self.load();
        self.hashes
            .get(path)
            .filter(|(kept, _)| kept == identity)
            .map(|(_, sha)| sha.clone())
    }

    /// Keeps the hash of a file that was read at `read` and was as
    /// `identity` describes it then and after.
    pub(crate) fn keep(&mut self, path: &Path, identity: Identity, sha: &str, read: SystemTime) {
        if !identity.settled_before(read, SETTLED) {
            return;
        }
        self.load();
        if self
            .hashes
            .get(path)
            .is_some_and(|(kept, kept_sha)| *kept == identity && kept_sha == sha)
        {
            return;
        }
        if let (Some(log), Some(text)) = (&self.log, line(path, &identity, sha)) {
            if self.appender.is_none()
                && let Some(directory) = log.parent()
            {
                let _ = std::fs::create_dir_all(directory);
                self.appender = OpenOptions::new().create(true).append(true).open(log).ok();
            }
            // One write of a short line: lines of several processes do not
            // interleave.
            if let Some(appender) = &mut self.appender
                && appender.write_all(&text).is_err()
            {
                self.appender = None;
            }
        }
        self.hashes
            .insert(path.to_path_buf(), (identity, sha.to_string()));
    }
}

fn line(path: &Path, identity: &Identity, sha: &str) -> Option<Vec<u8>> {
    let path = path.to_str().filter(|path| !path.contains(['\n', '\t']))?;
    let fields: Vec<String> = identity.fields().iter().map(i64::to_string).collect();
    let text = format!("{VERSION}\t{}\t{sha}\t{path}\n", fields.join("\t"));
    (text.len() < 4000).then(|| text.into_bytes())
}

fn parse(line: &[u8]) -> Option<(PathBuf, Identity, String)> {
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.splitn(11, '\t');
    if parts.next()? != VERSION {
        return None;
    }
    let mut fields = [0i64; 8];
    for field in &mut fields {
        *field = parts.next()?.parse().ok()?;
    }
    let sha = parts.next()?;
    let path = parts.next()?;
    if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) || path.is_empty() {
        return None;
    }
    Some((path.into(), Identity::from_fields(fields), sha.to_string()))
}

fn default_log() -> Option<PathBuf> {
    if cfg!(test) || std::env::var_os("UME_HASH_CACHE").is_some_and(|value| value == "0") {
        return None;
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(cache.join("unity-micro-editor/file-hashes-v1.log"))
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::new(default_log())));

/// The hash a file holds now, taken from the store when its metadata is the
/// same as when it was hashed.
pub(crate) fn kept(path: &Path, identity: &Identity) -> Option<String> {
    STORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .kept(path, identity)
}

/// Keeps a hash computed from a file read at `read`.
pub(crate) fn keep(path: &Path, identity: Identity, sha: &str, read: SystemTime) {
    STORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keep(path, identity, sha, read);
}

/// The SHA-256 of a regular file of at most 1 GiB, in hex.
pub fn file_sha256(filename: &str) -> Result<String> {
    let path = Path::new(filename);
    // Reported as opening it would be: callers tell a missing file by it.
    let info = std::fs::metadata(path).map_err(|e| crate::util::os_error("open", filename, &e))?;
    let before = Identity::of(&info);
    ensure!(
        before.is_regular() && before.size <= LIMIT,
        "compiler input is not a bounded regular file: {filename}"
    );
    if let Some(sha) = kept(path, &before) {
        return Ok(sha);
    }
    let read = SystemTime::now();
    let file = File::open(filename).map_err(|e| crate::util::os_error("open", filename, &e))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file.take(LIMIT + 1), &mut hasher)?;
    let sha = hex(&hasher.finalize());
    if Identity::read(path).is_ok_and(|after| after == before) {
        keep(path, before, &sha, read);
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write_settled(path: &Path, text: &str) -> Identity {
        std::fs::write(path, text).unwrap();
        // As if the file had been written three seconds ago.
        let old = SystemTime::now() - Duration::from_secs(3);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        Identity::read(path).unwrap()
    }

    #[test]
    fn a_kept_hash_is_taken_back_while_the_file_is_as_it_was() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("hashes.log");
        let file = root.path().join("a.txt");
        // The change time is now, so the store waits for the file to settle.
        let identity = write_settled(&file, "one");
        let mut store = Store::new(Some(log.clone()));
        store.keep(&file, identity, &"a".repeat(64), SystemTime::now());
        assert_eq!(store.kept(&file, &identity), None);
        let later = SystemTime::now() + Duration::from_secs(3);
        store.keep(&file, identity, &"a".repeat(64), later);
        assert_eq!(store.kept(&file, &identity), Some("a".repeat(64)));
        // Another process reads the log.
        let mut other = Store::new(Some(log.clone()));
        assert_eq!(other.kept(&file, &identity), Some("a".repeat(64)));
        // A changed file has another identity.
        let changed = write_settled(&file, "two!");
        assert_ne!(changed, identity);
        assert_eq!(other.kept(&file, &changed), None);
    }

    #[test]
    fn a_damaged_log_line_is_left_out() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("hashes.log");
        std::fs::write(
            &log,
            "v1\t1\t2\nnot a line\nv2\t1\t2\t3\t4\t5\t6\t7\t8\tsha\t/x\n",
        )
        .unwrap();
        let mut store = Store::new(Some(log));
        store.load();
        assert!(store.hashes.is_empty());
    }
}
