//! A streaming index of top-level `.meta` GUIDs.
use crate::{hex, registry, sysutil};
use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
};

/// This bound applies only to the identity index. MonoScript/configuration
/// metadata still uses its smaller limit and full accepted content hashes.
pub const METADATA_GUID_MAXIMUM: i64 = 32 * 1024 * 1024;
pub const METADATA_GUID_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataGuidIdentity {
    pub guid: String,
    pub sha256: String,
    pub bytes: i64,
}

/// Owns the reusable line buffer.
pub struct MetadataGuidIndex {
    buffer: Vec<u8>,
}
impl Default for MetadataGuidIndex {
    fn default() -> Self {
        Self::new()
    }
}
enum Fragment {
    Line,
    BufferFull,
    End,
    Failed(io::Error),
}
/// Newline-terminated slices over a fixed buffer; a longer line is returned in pieces.
struct SliceReader<'a, R> {
    buffer: &'a mut [u8],
    input: R,
    read: usize,
    written: usize,
    error: Option<Option<io::Error>>,
}
impl<R: Read> SliceReader<'_, R> {
    fn read_slice(&mut self) -> (std::ops::Range<usize>, Fragment) {
        let mut searched = 0;
        loop {
            if let Some(index) = self.buffer[self.read + searched..self.written]
                .iter()
                .position(|&b| b == b'\n')
            {
                let start = self.read;
                self.read += searched + index + 1;
                return (start..self.read, Fragment::Line);
            }
            if let Some(error) = self.error.take() {
                let range = self.read..self.written;
                self.read = self.written;
                return (range, error.map_or(Fragment::End, Fragment::Failed));
            }
            if self.written - self.read >= self.buffer.len() {
                self.read = self.written;
                return (0..self.buffer.len(), Fragment::BufferFull);
            }
            searched = self.written - self.read;
            if self.read > 0 {
                self.buffer.copy_within(self.read..self.written, 0);
                self.written -= self.read;
                self.read = 0;
            }
            let mut empty = 0;
            loop {
                match self.input.read(&mut self.buffer[self.written..]) {
                    Ok(0) => {
                        // A blocking `Read` signals end of input this way.
                        self.error = Some(None);
                        break;
                    }
                    Ok(count) => {
                        self.written += count;
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        empty += 1;
                        if empty >= 100 {
                            self.error = Some(Some(error));
                            break;
                        }
                    }
                    Err(error) => {
                        self.error = Some(Some(error));
                        break;
                    }
                }
            }
        }
    }
}
impl MetadataGuidIndex {
    pub fn new() -> Self {
        Self {
            buffer: vec![0; METADATA_GUID_BUFFER_SIZE],
        }
    }

    /// `read` hashes the complete bounded file, but retains only its top-level
    /// GUID. Model importer settings can have very long lines; their fragments
    /// are discarded without constructing an importer document.
    pub fn read(&mut self, filename: &str) -> Result<MetadataGuidIdentity> {
        if sysutil::eval_symlinks(filename).ok().as_deref() != Some(filename) {
            bail!("asset metadata has a missing or symlink path: {filename}");
        }
        let before = match sysutil::lstat(filename) {
            Ok(before) if before.is_regular() && before.size <= METADATA_GUID_MAXIMUM => before,
            _ => bail!("asset metadata is not a bounded regular file: {filename}"),
        };
        let file = File::open(filename)
            .map_err(|error| anyhow!(sysutil::Error::os("open", filename, &error)))?;
        if !sysutil::fstat(&file).is_ok_and(|opened| {
            opened.is_regular()
                && before.same_file(&opened)
                && before.size == opened.size
                && before.modified == opened.modified
        }) {
            bail!("asset metadata changed before GUID indexing: {filename}");
        }
        let result = self
            .scan((&file).take(before.size as u64 + 1))
            .map_err(|error| anyhow!("asset metadata GUID indexing {filename}: {error}"))?;
        let unchanged = |stat: io::Result<sysutil::Stat>| {
            stat.is_ok_and(|stat| {
                before.same_file(&stat)
                    && before.size == stat.size
                    && before.modified == stat.modified
            })
        };
        if sysutil::eval_symlinks(filename).ok().as_deref() != Some(filename)
            || !sysutil::lstat(filename).is_ok_and(|current| current.is_regular())
            || !unchanged(sysutil::fstat(&file))
            || !unchanged(sysutil::lstat(filename))
            || result.bytes != before.size
        {
            bail!("asset metadata changed while GUID indexing: {filename}");
        }
        Ok(result)
    }

    /// `scan`: the identity of one metadata stream.
    pub fn scan(&mut self, input: impl Read) -> Result<MetadataGuidIdentity> {
        let mut result = MetadataGuidIdentity::default();
        let mut reader = SliceReader {
            buffer: &mut self.buffer,
            input,
            read: 0,
            written: 0,
            error: None,
        };
        let mut hash = Sha256::new();
        let (mut first_fragment, mut seen_guid) = (true, false);
        loop {
            let (range, outcome) = reader.read_slice();
            let fragment = &reader.buffer[range];
            result.bytes += fragment.len() as i64;
            if result.bytes > METADATA_GUID_MAXIMUM {
                bail!("metadata exceeds GUID index byte limit");
            }
            hash.update(fragment);
            if first_fragment && fragment.starts_with(b"guid:") {
                if seen_guid {
                    bail!("source .meta has duplicate top-level GUID fields");
                }
                if matches!(outcome, Fragment::BufferFull) {
                    bail!("top-level GUID field exceeds its line bound");
                }
                seen_guid = true;
                result.guid = registry::meta_guid(fragment)?;
            }
            match outcome {
                Fragment::BufferFull => first_fragment = false,
                Fragment::Line => first_fragment = true,
                Fragment::End => break,
                Fragment::Failed(error) => bail!("{}", sysutil::errno_text(&error)),
            }
        }
        if !seen_guid {
            bail!("source .meta must contain one nonzero 32-hex GUID");
        }
        result.sha256 = hex(&hash.finalize());
        Ok(result)
    }
}
