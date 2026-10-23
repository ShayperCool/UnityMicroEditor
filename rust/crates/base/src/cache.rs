use crate::{
    hex,
    yaml::{Document, MAX_TEXT_BYTES, parse_unity_yaml},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) size: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
    pub(crate) mode: u32,
}
impl Identity {
    pub fn of(info: &fs::Metadata) -> Self {
        Self {
            device: info.dev(),
            inode: info.ino(),
            size: info.size(),
            mtime: info.mtime(),
            mtime_ns: info.mtime_nsec(),
            ctime: info.ctime(),
            ctime_ns: info.ctime_nsec(),
            mode: info.mode(),
        }
    }
    pub fn read(path: &Path) -> Result<Self> {
        let info = fs::metadata(path)
            .map_err(|e| crate::project::lexpath::os_error("stat", &path.to_string_lossy(), e))?;
        Ok(Self::of(&info))
    }
    pub(crate) fn is_regular(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }
    /// Whether neither the content nor the metadata of the file changed in
    /// the `margin` before `time`: a later write in the same tick of the
    /// clock would leave every field as it is.
    pub(crate) fn settled_before(&self, time: std::time::SystemTime, margin: f64) -> bool {
        let Ok(now) = time.duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        let seconds = |whole: i64, nanos: i64| whole as f64 + nanos as f64 / 1e9;
        let latest = seconds(self.mtime, self.mtime_ns).max(seconds(self.ctime, self.ctime_ns));
        now.as_secs_f64() - latest >= margin
    }
    /// The fields as a line of text, and back.
    pub fn fields(&self) -> [i64; 8] {
        [
            self.device as i64,
            self.inode as i64,
            self.size as i64,
            self.mtime,
            self.mtime_ns,
            self.ctime,
            self.ctime_ns,
            self.mode as i64,
        ]
    }
    pub(crate) fn from_fields(fields: [i64; 8]) -> Self {
        Self {
            device: fields[0] as u64,
            inode: fields[1] as u64,
            size: fields[2] as u64,
            mtime: fields[3],
            mtime_ns: fields[4],
            ctime: fields[5],
            ctime_ns: fields[6],
            mode: fields[7] as u32,
        }
    }
}
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub file_reads: u64,
    pub file_hits: u64,
    pub bytes_read: u64,
    pub yaml_parses: u64,
    pub yaml_hits: u64,
}
pub(crate) struct FileRecord {
    pub(crate) identity: Identity,
    pub(crate) content: Option<Arc<[u8]>>,
    pub(crate) digest: String,
}
struct YamlRecord {
    digest: String,
    documents: Arc<[Document]>,
}
#[derive(Default)]
pub struct SourceCache {
    pub(crate) files: HashMap<PathBuf, FileRecord>,
    documents: HashMap<PathBuf, YamlRecord>,
    pub(crate) observed: HashMap<PathBuf, Identity>,
    pub stats: CacheStats,
}
impl SourceCache {
    pub fn begin(&mut self) {
        self.stats = CacheStats::default();
        self.observed.clear();
    }
    pub fn invalidate(&mut self, path: &Path) {
        self.files.remove(path);
        self.documents.remove(path);
    }
    pub fn validate_observed(&self) -> Result<()> {
        for (path, identity) in &self.observed {
            ensure!(
                Identity::read(path).is_ok_and(|actual| actual == *identity),
                "source changed during preparation, retry: {}",
                path.display()
            );
        }
        Ok(())
    }
    pub fn read(&mut self, path: &Path, text: bool) -> Result<(Option<Arc<[u8]>>, String)> {
        let before = Identity::read(path)?;
        ensure!(
            before.is_regular(),
            "source is not a regular file: {}",
            path.display()
        );
        ensure!(
            !text || before.size <= MAX_TEXT_BYTES as u64,
            "text source exceeds {MAX_TEXT_BYTES} bytes: {}",
            path.display()
        );
        if let Some(previous) = self.observed.get(path) {
            ensure!(
                *previous == before,
                "source changed during preparation, retry: {}",
                path.display()
            );
        }
        self.observed.insert(path.into(), before);
        if let Some(old) = self.files.get(path)
            && old.identity == before
            && (!text || old.content.is_some())
        {
            self.stats.file_hits += 1;
            return Ok((old.content.clone(), old.digest.clone()));
        }
        // A file only hashed is not read while its metadata is as it was.
        if !text && let Some(digest) = crate::hashstore::kept(path, &before) {
            self.files.insert(
                path.into(),
                FileRecord {
                    identity: before,
                    content: None,
                    digest: digest.clone(),
                },
            );
            self.stats.file_hits += 1;
            return Ok((None, digest));
        }
        let read = std::time::SystemTime::now();
        let mut file = File::open(path)
            .map_err(|e| crate::project::lexpath::os_error("open", &path.to_string_lossy(), e))?;
        let mut hasher = Sha256::new();
        let content = if text {
            let mut content = Vec::with_capacity(before.size as usize);
            file.by_ref()
                .take(MAX_TEXT_BYTES as u64 + 1)
                .read_to_end(&mut content)?;
            ensure!(
                content.len() <= MAX_TEXT_BYTES,
                "text source grew while reading: {}",
                path.display()
            );
            hasher.update(&content);
            Some(Arc::from(content))
        } else {
            let mut buffer = vec![0u8; 128 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            None
        };
        let after = Identity::read(path)?;
        ensure!(
            before == after,
            "source changed while reading, retry: {}",
            path.display()
        );
        let opened = file.metadata()?;
        if opened.dev() != after.device || opened.ino() != after.inode {
            bail!("source replaced while reading, retry: {}", path.display());
        }
        let digest = hex(&hasher.finalize());
        crate::hashstore::keep(path, after, &digest, read);
        self.files.insert(
            path.into(),
            FileRecord {
                identity: after,
                // With `UME_MEMORY=low` a text is read again when needed.
                content: content.clone().filter(|_| crate::memory::keeps_sources()),
                digest: digest.clone(),
            },
        );
        self.stats.file_reads += 1;
        self.stats.bytes_read += before.size;
        Ok((content, digest))
    }
    pub fn parse(&mut self, path: &Path) -> Result<Vec<Document>> {
        let (content, digest) = self.read(path, true)?;
        if let Some(old) = self.documents.get(path)
            && old.digest == digest
        {
            self.stats.yaml_hits += 1;
            return Ok(old.documents.to_vec());
        }
        let docs = parse_unity_yaml(&content.context("text cache has no content")?)
            .with_context(|| path.display().to_string())?;
        if crate::memory::keeps_sources() {
            self.documents.insert(
                path.into(),
                YamlRecord {
                    digest,
                    documents: Arc::from(docs.clone()),
                },
            );
        }
        self.stats.yaml_parses += 1;
        Ok(docs)
    }
}
