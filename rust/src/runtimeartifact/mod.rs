//! Reads Unity 6000.6.3f1 native files: the built-in lighting data a scene
//! may name, and the readers that serve it.
mod builtin_lighting;

pub use builtin_lighting::{BuiltinLightingIdentity, ReadAt, inspect_builtin_lighting};

use anyhow::{Result, bail};

/// Accepts a read only when it succeeded and passes the check; either failure
/// is reported as the same refusal.
pub(crate) fn req<T>(
    value: Result<T>,
    accept: impl FnOnce(&T) -> bool,
    message: &str,
) -> Result<T> {
    match value {
        Ok(value) if accept(&value) => Ok(value),
        _ => bail!("{message}"),
    }
}

/// Unity writes names as UTF-8. Undecodable bytes become U+FFFD, so a damaged
/// name stays reportable; identity checks that must be exact compare the raw
/// bytes instead.
pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}
pub(crate) fn be64(data: &[u8]) -> u64 {
    u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ])
}
pub(crate) fn le16(data: &[u8]) -> u16 {
    u16::from_le_bytes([data[0], data[1]])
}
pub(crate) fn le32(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}
pub(crate) fn le64(data: &[u8]) -> u64 {
    u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ])
}

pub(crate) struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        if self.pos > self.data.len() || size > self.data.len() - self.pos {
            bail!("native serialized data exceeds its bounds");
        }
        let result = &self.data[self.pos..self.pos + size];
        self.pos += size;
        Ok(result)
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(le32(self.take(4)?))
    }
    pub fn cstring_bytes(&mut self, maximum: usize) -> Result<&'a [u8]> {
        if self.pos > self.data.len() {
            bail!("native string offset exceeds data bounds");
        }
        let rest = &self.data[self.pos..];
        match rest.iter().position(|&byte| byte == 0) {
            Some(end) if end <= maximum => {
                self.pos += end + 1;
                Ok(&rest[..end])
            }
            _ => bail!("native string is missing or exceeds bounds"),
        }
    }
    pub fn align(&mut self) {
        self.pos = (self.pos + 3) & !3;
    }
}
