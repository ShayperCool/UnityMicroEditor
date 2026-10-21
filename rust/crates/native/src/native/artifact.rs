//! Reads the artifacts of a stopped run without invoking Unity or taking
//! native database locks: a caller-owned DataStore snapshot (only the
//! observed Linux x64 6000.6.3f1 StorageInfoTable/ContentDataTable layout is
//! decoded), or the files a 6000.0 worker writes under `Library/Artifacts`.
use super::support::{abs, create_new, eval_symlinks, nul, os_error, wrap};
use crate::{
    hex,
    lmdb::{self, Record},
    protocol::ArtifactFile,
    registry::{ContentHash, native_hash},
    sha256, sysutil,
    util::{canonical_destination, within_directory},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs::File, io::Write, os::unix::fs::FileExt};

pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

pub struct UdsArtifactStore {
    directory: String,
    storage: HashMap<Vec<u8>, Vec<u8>>,
    pub(crate) content: HashMap<Vec<u8>, Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactExtraction {
    #[serde(deserialize_with = "nul")]
    pub raw_hash_key: String,
    #[serde(deserialize_with = "nul")]
    pub storage_type: u32,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub source_path: String,
    #[serde(deserialize_with = "nul")]
    pub offset: u64,
    #[serde(deserialize_with = "nul")]
    pub bytes: u64,
    #[serde(deserialize_with = "nul")]
    pub sha256: String,
    #[serde(deserialize_with = "nul")]
    pub content_key_verified: bool,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub output: String,
}

impl UdsArtifactStore {
    pub fn open(directory: &str) -> Result<Self> {
        let directory = abs(directory)?;
        let database = sysutil::join(&[&directory, "uds.db"]);
        let storage = lmdb::read_table(&database, "StorageInfoTable")
            .map_err(|e| wrap("UDS storage table", e))?;
        let content = lmdb::read_table(&database, "ContentDataTable")
            .map_err(|e| wrap("UDS content table", e))?;
        Self::from_records(&directory, &storage, &content)
    }

    pub fn from_records(directory: &str, storage: &[Record], content: &[Record]) -> Result<Self> {
        let mut store = Self {
            directory: directory.into(),
            storage: HashMap::new(),
            content: HashMap::new(),
        };
        for record in storage {
            if record.key.len() != 16 || record.value.len() != 32 {
                bail!("UDS storage record has unexpected size");
            }
            if store
                .storage
                .insert(record.key.clone(), record.value.clone())
                .is_some()
            {
                bail!("duplicate UDS storage hash");
            }
        }
        for record in content {
            if record.key.len() != 16 || record.value.len() as u64 > MAX_ARTIFACT_BYTES {
                bail!("UDS content record is outside bounds");
            }
            if store
                .content
                .insert(record.key.clone(), record.value.clone())
                .is_some()
            {
                bail!("duplicate UDS content hash");
            }
        }
        Ok(store)
    }

    /// Uses the exact 16 raw bytes from a worker artifact hash, not a guessed
    /// string/endianness conversion. The native UDS memory hasher uses XXH3-128,
    /// stored as low/high little-endian words; it is verified before content is
    /// returned. The proof is filled as far as reading got, also on failure.
    pub fn read(&self, hash: &[u8]) -> (ArtifactExtraction, Result<Vec<u8>>) {
        let mut proof = ArtifactExtraction {
            raw_hash_key: hex(hash),
            ..Default::default()
        };
        let content = self.read_into(hash, &mut proof);
        (proof, content)
    }
    fn read_into(&self, hash: &[u8], proof: &mut ArtifactExtraction) -> Result<Vec<u8>> {
        if hash.len() != 16 {
            bail!("UDS artifact hash must be 16 bytes");
        }
        let Some(info) = self.storage.get(hash) else {
            bail!("artifact hash has no UDS storage entry");
        };
        let word = |at: usize| u64::from_le_bytes(info[at..at + 8].try_into().unwrap());
        proof.storage_type = u32::from_le_bytes(info[..4].try_into().unwrap());
        proof.offset = word(16);
        proof.bytes = word(24);
        if proof.bytes == 0 || proof.bytes > MAX_ARTIFACT_BYTES {
            bail!("UDS artifact size is outside probe bounds");
        }
        let content = match proof.storage_type {
            2 => {
                proof.source_path =
                    sysutil::join(&[&self.directory, &format!("UDSData_{}.bin", word(8))]);
                let path = &proof.source_path;
                let file = File::open(path).map_err(|e| os_error("open", path, &e))?;
                let info = file.metadata().map_err(|e| os_error("stat", path, &e))?;
                if !info.is_file()
                    || proof.offset > info.len()
                    || proof.bytes > info.len() - proof.offset
                {
                    bail!("UDS artifact slice exceeds its data file");
                }
                let mut content = vec![0; proof.bytes as usize];
                file.read_exact_at(&mut content, proof.offset)
                    .map_err(|e| os_error("read", path, &e))?;
                content
            }
            3 => match self.content.get(hash) {
                Some(value) if proof.offset == 0 && value.len() as u64 == proof.bytes => {
                    value.clone()
                }
                _ => bail!("inline UDS artifact does not match storage size"),
            },
            kind => bail!("UDS storage type {kind} is not yet supported"),
        };
        if native_hash(&content) != hash {
            bail!("UDS content bytes do not match their native XXH3-128 key");
        }
        proof.content_key_verified = true;
        proof.sha256 = sha256(&content);
        Ok(content)
    }

    pub fn extract(&self, hash: &[u8], output: &str) -> (ArtifactExtraction, Result<()>) {
        let (mut proof, content) = self.read(hash);
        let result = content.and_then(|content| {
            write_outside(&content, output, &self.directory, "DataStore", &mut proof)
        });
        (proof, result)
    }
}

/// Writes extracted content to the new file `output`, which must not lie in
/// the store it came from.
fn write_outside(
    content: &[u8],
    output: &str,
    store: &str,
    what: &str,
    proof: &mut ArtifactExtraction,
) -> Result<()> {
    let output = abs(output)?;
    let canonical = canonical_destination(&output)?;
    let directory = eval_symlinks(store)?;
    if within_directory(&canonical, &directory) {
        bail!("artifact extraction must not write into its input {what}");
    }
    let mut file = create_new(&output, 0o600)?;
    file.write_all(content)
        .map_err(|e| os_error("write", &output, &e))?;
    drop(file);
    proof.output = output;
    Ok(())
}

impl crate::engine::ArtifactStore for UdsArtifactStore {
    fn extract(&self, file: &ArtifactFile, output: &str) -> (ArtifactExtraction, Result<()>) {
        UdsArtifactStore::extract(self, &file.hash, output)
    }
}

/// The artifacts a worker writes into its context itself (6000.0): a stored
/// file is `Library/Artifacts/<first two hex digits>/<hex>` of its content
/// key, a small one is inline in its metadata. Both are checked against the
/// key, which is the release's content hash.
pub struct LibraryArtifactStore {
    directory: String,
    hash: ContentHash,
}

/// `ArtifactFile::kind` of a file kept inline in the metadata.
pub const INLINE_ARTIFACT: u32 = 1;
/// `ArtifactFile::kind` of a file kept under `Library/Artifacts`.
pub const STORED_ARTIFACT: u32 = 0;

impl LibraryArtifactStore {
    /// The store of `<project>/Library/Artifacts`.
    pub fn open(project: &str, hash: ContentHash) -> Result<Self> {
        let directory = sysutil::join(&[&abs(project)?, "Library", "Artifacts"]);
        let info = std::fs::metadata(&directory).map_err(|e| os_error("stat", &directory, &e))?;
        if !info.is_dir() {
            bail!("{directory} is not a directory");
        }
        Ok(Self { directory, hash })
    }

    pub fn read(&self, file: &ArtifactFile) -> (ArtifactExtraction, Result<Vec<u8>>) {
        let mut proof = ArtifactExtraction {
            raw_hash_key: hex(&file.hash),
            storage_type: file.kind,
            ..Default::default()
        };
        let content = self.read_into(file, &mut proof);
        (proof, content)
    }

    fn read_into(&self, file: &ArtifactFile, proof: &mut ArtifactExtraction) -> Result<Vec<u8>> {
        let content = match file.kind {
            INLINE_ARTIFACT => file.inline_data.clone(),
            STORED_ARTIFACT => {
                let name = hex(&file.hash);
                proof.source_path = sysutil::join(&[&self.directory, &name[..2], &name]);
                let path = &proof.source_path;
                let stored = File::open(path).map_err(|e| os_error("open", path, &e))?;
                let info = stored.metadata().map_err(|e| os_error("stat", path, &e))?;
                if !info.is_file() || info.len() > MAX_ARTIFACT_BYTES {
                    bail!("stored artifact is outside probe bounds");
                }
                let mut content = vec![0; info.len() as usize];
                stored
                    .read_exact_at(&mut content, 0)
                    .map_err(|e| os_error("read", path, &e))?;
                content
            }
            kind => bail!("artifact file kind {kind} is not yet supported"),
        };
        proof.bytes = content.len() as u64;
        if content.is_empty() || proof.bytes > MAX_ARTIFACT_BYTES {
            bail!("artifact size is outside probe bounds");
        }
        if self.hash.of(&content) != file.hash {
            bail!("artifact bytes do not match their native content key");
        }
        proof.content_key_verified = true;
        proof.sha256 = sha256(&content);
        Ok(content)
    }
}

impl crate::engine::ArtifactStore for LibraryArtifactStore {
    fn extract(&self, file: &ArtifactFile, output: &str) -> (ArtifactExtraction, Result<()>) {
        let (mut proof, content) = self.read(file);
        let result = content.and_then(|content| {
            write_outside(
                &content,
                output,
                &self.directory,
                "Library/Artifacts",
                &mut proof,
            )
        });
        (proof, result)
    }
}
