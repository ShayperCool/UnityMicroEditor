//! What a file is without reading it, and the file map of a Player directory
//! that one content read and `stat` calls keep current.
use super::{
    NilMap,
    sys::{self, Walk},
};
use crate::{sysutil, util::file_hash};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// Device, inode, size and the two times of a file. A change of content
/// moves the change time, which no process can set back; a replaced file has
/// another inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified: i128,
    pub changed: i128,
}

impl Stamp {
    pub fn of(info: &fs::Metadata) -> Self {
        Self {
            device: info.dev(),
            inode: info.ino(),
            size: info.size(),
            modified: i128::from(info.mtime()) * 1_000_000_000 + i128::from(info.mtime_nsec()),
            changed: i128::from(info.ctime()) * 1_000_000_000 + i128::from(info.ctime_nsec()),
        }
    }

    /// The stamp of the path itself, without following a symbolic link.
    pub fn read(path: &str) -> Result<Self> {
        fs::symlink_metadata(path)
            .map(|info| Self::of(&info))
            .map_err(|error| sys::os_error("lstat", path, &error))
    }

    /// What `cp -a` keeps of a file: its size and its modification time.
    pub fn copied_from(&self, source: &Stamp) -> bool {
        self.size == source.size && self.modified == source.modified
    }
}

/// The files of a Player directory by slash-relative path, with the SHA-256
/// of each (what `player_files` gives) and the stamp each had when it was
/// hashed or written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerMap {
    pub directory: String,
    pub files: NilMap<String>,
    pub stamps: BTreeMap<String, Stamp>,
}

/// Every regular file of a canonical Player directory with its stamp, under
/// the rules of `player_files`: no other kind of file, at most 4096.
fn player_stamps(directory: &str) -> Result<BTreeMap<String, Stamp>> {
    if sysutil::eval_symlinks(directory).ok() != Some(sysutil::clean(directory)) {
        bail!("compiler Player snapshot must be a canonical directory");
    }
    let mut stamps = BTreeMap::new();
    sys::walk_dir(directory, &mut |entry| {
        if entry.is_dir() {
            return Ok(Walk::Continue);
        }
        if !entry.kind.is_file() || stamps.len() >= 4096 {
            bail!("compiler Player snapshot contains an unsupported file");
        }
        let relative = sysutil::rel(directory, entry.path)
            .ok_or_else(|| anyhow!("Rel: can't make {} relative to {directory}", entry.path))?;
        stamps.insert(relative, Stamp::read(entry.path)?);
        Ok(Walk::Continue)
    })?;
    if stamps.is_empty() {
        bail!("compiler Player snapshot has no files");
    }
    Ok(stamps)
}

impl PlayerMap {
    /// Reads and hashes every file. A file whose stamp moved while it was
    /// read is an error: the map would name bytes nobody can say were there.
    pub fn read(directory: &str) -> Result<Self> {
        let before = player_stamps(directory)?;
        let mut files = NilMap::make();
        for (relative, stamp) in &before {
            let path = sysutil::join(&[directory, relative]);
            let hash = file_hash(&path)?;
            if Stamp::read(&path)? != *stamp {
                bail!("compiler Player file changed while it was hashed: {path}");
            }
            files.insert(relative, hash);
        }
        if player_stamps(directory)? != before {
            bail!("compiler Player snapshot changed while it was hashed: {directory}");
        }
        Ok(Self {
            directory: directory.into(),
            files,
            stamps: before,
        })
    }

    /// Whether the directory still holds exactly the files of the map, each
    /// with the stamp it had: then the hashes are those of its bytes.
    pub fn still(&self) -> bool {
        player_stamps(&self.directory).is_ok_and(|stamps| stamps == self.stamps)
    }

    /// The map of `directory`, a `cp -a` of this map's directory, when it
    /// holds the same files with the size and modification time the copy
    /// keeps; `None` when anything else is there. Valid only while
    /// [`PlayerMap::still`] holds for the source of the copy.
    pub fn copied_to(&self, directory: &str) -> Option<Self> {
        let stamps = player_stamps(directory).ok()?;
        let same = stamps.len() == self.stamps.len()
            && stamps
                .iter()
                .zip(&self.stamps)
                .all(|((path, stamp), (source_path, source))| {
                    path == source_path && stamp.copied_from(source)
                });
        same.then(|| Self {
            directory: directory.into(),
            files: self.files.clone(),
            stamps,
        })
    }

    /// The map of `directory`, made by `cp -a` of the directory of
    /// `source` a moment ago: the hashes of the source when its directory
    /// is still as they were taken (checked after the copy, so also during
    /// it) and the copy kept every file's size and modification time; what
    /// a full read gives otherwise.
    pub fn after_copy(source: Option<Arc<PlayerMap>>, directory: &str) -> Result<Self> {
        if let Some(source) = source
            && source.still()
            && let Some(map) = source.copied_to(directory)
        {
            return Ok(map);
        }
        Self::read(directory)
    }

    /// Records a file this process wrote into the directory: the hash of
    /// the bytes it wrote and the stamp the file has now.
    pub fn wrote(&mut self, path: &str, hash: String) -> Result<()> {
        let relative = sysutil::rel(&self.directory, path)
            .filter(|relative| !relative.starts_with("../"))
            .ok_or_else(|| anyhow!("Rel: can't make {path} relative to {}", self.directory))?;
        self.stamps.insert(relative.clone(), Stamp::read(path)?);
        self.files.insert(relative, hash);
        Ok(())
    }

    /// The hash of a file of the directory by its absolute path; empty when unknown.
    pub fn hash_of(&self, path: &str) -> &str {
        sysutil::rel(&self.directory, path).map_or("", |relative| self.files.at(&relative))
    }
}

/// How long after its last change a file's stamp is trusted: a file
/// written in the same tick of the clock as it was read could change again
/// without moving its times (the rule of git's index).
const SETTLED: i128 = 2_000_000_000;

#[derive(Debug)]
struct Known {
    stamp: Stamp,
    hash: String,
}

#[derive(Debug, Default)]
struct Hashes {
    known: BTreeMap<String, Known>,
    reads: u64,
}

/// The SHA-256 of files that do not change, read once per runner and
/// answered from then on while `stat` gives the same stamp. A file is kept
/// only when its stamp did not move while it was read and both its times
/// were at least two seconds old when the read began; any other file is
/// read every time. Off (`Default`) every hash is a read; one process's
/// clones share the entries.
#[derive(Debug, Clone, Default)]
pub struct HashCache(Option<Arc<Mutex<Hashes>>>);

fn now() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as i128)
}

impl HashCache {
    pub fn on() -> Self {
        Self(Some(Arc::default()))
    }

    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// How many files were read so far.
    pub fn reads(&self) -> u64 {
        self.0
            .as_ref()
            .map_or(0, |hashes| hashes.lock().unwrap().reads)
    }

    /// What `util::file_hash` gives for `path`.
    pub fn file_hash(&self, path: &str) -> Result<String> {
        let Some(hashes) = &self.0 else {
            return file_hash(path);
        };
        let stamp = |path: &str| fs::metadata(path).ok().map(|info| Stamp::of(&info));
        let Some(before) = stamp(path) else {
            return file_hash(path);
        };
        if let Some(known) = hashes.lock().unwrap().known.get(path)
            && known.stamp == before
        {
            return Ok(known.hash.clone());
        }
        let started = now();
        let hash = file_hash(path)?;
        let mut hashes = hashes.lock().unwrap();
        hashes.reads += 1;
        match stamp(path) {
            Some(after)
                if after == before
                    && after.modified <= started - SETTLED
                    && after.changed <= started - SETTLED =>
            {
                hashes.known.insert(
                    path.into(),
                    Known {
                        stamp: after,
                        hash: hash.clone(),
                    },
                );
            }
            _ => {
                hashes.known.remove(path);
            }
        }
        Ok(hash)
    }

    /// What `verify_cache_files` gives for the files of a cache entry.
    pub fn verify_files(&self, directory: &str, hashes: &NilMap<String>) -> bool {
        !hashes.is_empty()
            && hashes.iter().all(|(name, expected)| {
                sysutil::base(name) == *name
                    && !expected.is_empty()
                    && self
                        .file_hash(&sysutil::join(&[directory, name]))
                        .is_ok_and(|actual| actual == *expected)
            })
    }
}
