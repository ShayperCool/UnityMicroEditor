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
//! process reads at its first hash. `UME_HASH_CACHE=0` turns it off. A
//! process holds 68 bytes for each, and once a day one writes the log again
//! without the files that are gone or changed since.
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

/// A kept hash: 68 bytes with its key for a file however long its path is
/// (a log holds some 350 000 of them).
#[derive(Clone, Copy)]
struct Kept {
    /// A digest of the identity the file had.
    identity: [u8; 16],
    sha: [u8; 32],
    /// The line of the log that holds it, for writing the log again.
    line: u32,
}

/// The key of a path: a 128-bit digest, as no two of a store's paths share.
/// SipHash, twice: a log of 350 000 lines loads in a fraction of a second.
fn key(path: &Path) -> [u8; 16] {
    digest16(path.as_os_str().as_encoded_bytes())
}

fn digest16(bytes: &[u8]) -> [u8; 16] {
    use std::hash::{DefaultHasher, Hasher};
    let mut key = [0u8; 16];
    for (half, seed) in [(0, 0u8), (8, 1u8)] {
        // `DefaultHasher::new` has fixed keys: the same digest in every
        // process.
        let mut hasher = DefaultHasher::new();
        hasher.write_u8(seed);
        hasher.write(bytes);
        key[half..half + 8].copy_from_slice(&hasher.finish().to_le_bytes());
    }
    key
}

fn identity_key(identity: &Identity) -> [u8; 16] {
    let mut bytes = [0u8; 64];
    for (at, field) in identity.fields().iter().enumerate() {
        bytes[at * 8..at * 8 + 8].copy_from_slice(&field.to_le_bytes());
    }
    digest16(&bytes)
}

fn sha_bytes(sha: &str) -> Option<[u8; 32]> {
    let mut bytes = [0u8; 32];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(sha.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// How often a process prunes the log of files that are gone, in seconds.
const PRUNE_EVERY: u64 = 24 * 3600;

/// Kept hashes by path, and the log they are appended to.
pub(crate) struct Store {
    log: Option<PathBuf>,
    hashes: HashMap<[u8; 16], Kept>,
    lines: u32,
    appender: Option<File>,
    loaded: bool,
}

impl Store {
    /// A store with its log at `log`, or only in memory.
    pub(crate) fn new(log: Option<PathBuf>) -> Self {
        Self {
            log,
            hashes: HashMap::new(),
            lines: 0,
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
        for line in io::BufReader::new(file).split(b'\n').map_while(Result::ok) {
            if let Some((path, identity, sha)) = parse(&line)
                && let Some(sha) = sha_bytes(&sha)
            {
                self.hashes.insert(
                    key(&path),
                    Kept {
                        identity: identity_key(&identity),
                        sha,
                        line: self.lines,
                    },
                );
            }
            self.lines = self.lines.saturating_add(1);
        }
        let lines = self.lines as usize;
        // A log that holds mostly superseded lines is written again, and
        // once a day without the files that are gone.
        let stamp = log.with_extension("pruned");
        let pruned = std::fs::metadata(&stamp)
            .and_then(|info| info.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age.as_secs() < PRUNE_EVERY);
        let newest = || -> HashMap<[u8; 16], u32> {
            self.hashes
                .iter()
                .map(|(key, kept)| (*key, kept.line))
                .collect()
        };
        if lines > 100_000 && lines > 4 * self.hashes.len() {
            let newest = newest();
            if let Some(lines) = rewrite(&log, &newest, false) {
                self.appender = None;
                self.renumber(&lines);
            }
        } else if !pruned && lines > 10_000 {
            // A pass over every file of the log takes seconds: it runs beside
            // the process, which keeps what it loaded.
            let _ = File::create(&stamp);
            let newest = newest();
            let _ = std::thread::Builder::new()
                .name("ume-hash-log".into())
                .spawn(move || rewrite(&log, &newest, true));
        }
    }

    /// Takes the line numbers of a log written again.
    fn renumber(&mut self, lines: &HashMap<[u8; 16], u32>) {
        self.hashes
            .retain(|path_key, entry| match lines.get(path_key) {
                Some(line) => {
                    entry.line = *line;
                    true
                }
                None => false,
            });
        self.lines = lines.len() as u32;
    }

    #[cfg(test)]
    fn compact(&mut self, log: &Path, prune: bool) {
        let newest = self
            .hashes
            .iter()
            .map(|(key, kept)| (*key, kept.line))
            .collect();
        if let Some(lines) = rewrite(log, &newest, prune) {
            self.appender = None;
            self.renumber(&lines);
        }
    }

    /// The kept hash of a file as `identity` describes it now.
    pub(crate) fn kept(&mut self, path: &Path, identity: &Identity) -> Option<String> {
        self.load();
        self.hashes
            .get(&key(path))
            .filter(|kept| kept.identity == identity_key(identity))
            .map(|kept| hex(&kept.sha))
    }

    /// Keeps the hash of a file that was read at `read` and was as
    /// `identity` describes it then and after.
    pub(crate) fn keep(&mut self, path: &Path, identity: Identity, sha: &str, read: SystemTime) {
        if !identity.settled_before(read, SETTLED) {
            return;
        }
        let Some(sha_bytes) = sha_bytes(sha) else {
            return;
        };
        self.load();
        let path_key = key(path);
        let identity_digest = identity_key(&identity);
        if self
            .hashes
            .get(&path_key)
            .is_some_and(|kept| kept.identity == identity_digest && kept.sha == sha_bytes)
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
        self.hashes.insert(
            path_key,
            Kept {
                identity: identity_digest,
                sha: sha_bytes,
                line: self.lines,
            },
        );
        self.lines = self.lines.saturating_add(1);
    }
}

/// Writes the log again with the line `newest` names for each path, with
/// `prune` only of the files that are as the line says, and returns the new
/// line of each path kept. Lines another process appends meanwhile are lost,
/// which costs it a hash.
fn rewrite(
    log: &Path,
    newest: &HashMap<[u8; 16], u32>,
    prune: bool,
) -> Option<HashMap<[u8; 16], u32>> {
    let temporary = log.with_extension(format!("{}.tmp", crate::util::random_id()));
    let mut lines = HashMap::new();
    let written = File::open(log).and_then(|source| {
        let mut writer = io::BufWriter::new(File::create(&temporary)?);
        for (at, line) in io::BufReader::new(source)
            .split(b'\n')
            .map_while(Result::ok)
            .enumerate()
        {
            let Some((path, identity, _)) = parse(&line) else {
                continue;
            };
            let path_key = key(&path);
            if newest.get(&path_key) != Some(&(at as u32))
                || prune && !Identity::read(&path).is_ok_and(|now| now == identity)
            {
                continue;
            }
            writer.write_all(&line)?;
            writer.write_all(b"\n")?;
            lines.insert(path_key, lines.len() as u32);
        }
        writer.flush()
    });
    match written.and_then(|()| std::fs::rename(&temporary, log)) {
        Ok(()) => Some(lines),
        Err(_) => {
            let _ = std::fs::remove_file(&temporary);
            None
        }
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

    #[test]
    fn a_written_log_keeps_the_newest_line_of_each_file_there() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("hashes.log");
        let (one, two) = (root.path().join("one"), root.path().join("two"));
        let first = write_settled(&one, "1");
        let gone = write_settled(&two, "2");
        let later = SystemTime::now() + Duration::from_secs(3);
        let mut store = Store::new(Some(log.clone()));
        store.keep(&one, first, &"a".repeat(64), later);
        store.keep(&two, gone, &"b".repeat(64), later);
        let second = write_settled(&one, "11");
        store.keep(&one, second, &"c".repeat(64), later);
        std::fs::remove_file(&two).unwrap();
        let mut other = Store::new(Some(log.clone()));
        other.load();
        other.compact(&log, true);
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        assert_eq!(other.kept(&one, &second), Some("c".repeat(64)));
        assert_eq!(other.kept(&two, &gone), None);
        let mut third = Store::new(Some(log));
        assert_eq!(third.kept(&one, &second), Some("c".repeat(64)));
        assert_eq!(third.kept(&one, &first), None);
    }
}
