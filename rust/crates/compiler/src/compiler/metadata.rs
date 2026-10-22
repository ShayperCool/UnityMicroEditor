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

/// The names in the AssemblyRef table of a managed PE image (ECMA-335
/// II.22.5); `None` for anything else.
pub fn assembly_references(image: &[u8]) -> Option<Vec<String>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?));
    let pe = u32_at(0x3c)? as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(coff + 2)? as usize;
    let optional = coff + 20;
    let directories = match u16_at(optional)? {
        0x10b => optional + 96,
        0x20b => optional + 112,
        _ => return None,
    };
    let cli_rva = u32_at(directories + 14 * 8)?;
    let table = optional + u16_at(coff + 16)? as usize;
    let offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|index| {
            let section = table + index * 40;
            let (size, address) = (u32_at(section + 8)?, u32_at(section + 12)?);
            let raw = u32_at(section + 20)?;
            (rva >= address && rva < address + size.max(u32_at(section + 16)?))
                .then(|| (raw + rva - address) as usize)
        })
    };
    let cli = offset(cli_rva)?;
    let root = offset(u32_at(cli + 8)?)?;
    if u32_at(root)? != 0x424a_5342 {
        return None;
    }
    let version = u32_at(root + 12)? as usize;
    let streams = u16_at(root + 16 + version + 2)? as usize;
    let mut at = root + 16 + version + 4;
    let (mut tables, mut strings) = (None, None);
    for _ in 0..streams {
        let (stream, _) = (u32_at(at)? as usize, u32_at(at + 4)?);
        let name_start = at + 8;
        let end = name_start + image.get(name_start..)?.iter().position(|&b| b == 0)?;
        match image.get(name_start..end)? {
            b"#~" | b"#-" => tables = Some(root + stream),
            b"#Strings" => strings = Some(root + stream),
            _ => {}
        }
        at = name_start + (end - name_start + 4) / 4 * 4;
    }
    let (tables, strings) = (tables?, strings?);
    let heaps = *image.get(tables + 6)?;
    let valid = u64::from_le_bytes(image.get(tables + 8..tables + 16)?.try_into().ok()?);
    let mut rows = [0usize; 64];
    let mut at = tables + 24;
    for (index, count) in rows.iter_mut().enumerate() {
        if valid & (1 << index) != 0 {
            *count = u32_at(at)? as usize;
            at += 4;
        }
    }
    let string = if heaps & 1 != 0 { 4 } else { 2 };
    let guid = if heaps & 2 != 0 { 4 } else { 2 };
    let blob = if heaps & 4 != 0 { 4 } else { 2 };
    let index = |table: usize| if rows[table] > 0xffff { 4 } else { 2 };
    let coded = |tables: &[usize], bits: u32| {
        let most = tables.iter().map(|&table| rows[table]).max().unwrap_or(0);
        if most >= 1 << (16 - bits) { 4 } else { 2 }
    };
    let type_def_or_ref = coded(&[2, 1, 0x1b], 2);
    let has_constant = coded(&[4, 8, 0x17], 2);
    let has_custom_attribute = coded(
        &[
            6, 4, 1, 2, 8, 9, 0xa, 0, 0xe, 0x17, 0x14, 0x11, 0x1a, 0x1b, 0x20, 0x23, 0x26, 0x27,
            0x28, 0x2a, 0x2c, 0x2b,
        ],
        5,
    );
    let has_field_marshal = coded(&[4, 8], 1);
    let has_decl_security = coded(&[2, 6, 0x20], 2);
    let member_ref_parent = coded(&[2, 1, 0x1a, 6, 0x1b], 3);
    let has_semantics = coded(&[0x14, 0x17], 1);
    let method_def_or_ref = coded(&[6, 0xa], 1);
    let member_forwarded = coded(&[4, 6], 1);
    let custom_attribute_type = coded(&[6, 0xa], 3);
    // The row size of every table before AssemblyRef (0x23).
    let sizes: [usize; 0x23] = [
        2 + string + guid * 3,
        coded(&[0, 0x1a, 0x23, 1], 2) + string * 2,
        4 + string * 2 + type_def_or_ref + index(4) + index(6),
        index(4),
        2 + string + blob,
        index(6),
        4 + 2 + 2 + string + blob + index(8),
        index(8),
        2 + 2 + string,
        index(2) + type_def_or_ref,
        member_ref_parent + string + blob,
        2 + has_constant + blob,
        has_custom_attribute + custom_attribute_type + blob,
        has_field_marshal + blob,
        2 + has_decl_security + blob,
        2 + 4 + index(2),
        4 + index(4),
        blob,
        index(2) + index(0x14),
        index(0x14),
        2 + string + type_def_or_ref,
        index(2) + index(0x17),
        index(0x17),
        2 + string + blob,
        2 + index(6) + has_semantics,
        index(2) + method_def_or_ref * 2,
        string,
        blob,
        2 + member_forwarded + string + index(0x1a),
        4 + index(4),
        4 + 4,
        4,
        4 + 2 * 4 + 4 + blob + string * 2,
        4,
        4 * 3,
    ];
    let mut start = at;
    for (table, size) in sizes.iter().enumerate() {
        start += rows[table] * size;
    }
    let row = 2 * 4 + 4 + blob + string * 2 + blob;
    let read_index = |at: usize, width: usize| -> Option<usize> {
        Some(if width == 4 {
            u32_at(at)? as usize
        } else {
            u16_at(at)? as usize
        })
    };
    let mut names = Vec::with_capacity(rows[0x23]);
    for reference in 0..rows[0x23] {
        let name = read_index(start + reference * row + 12 + blob, string)?;
        let text = image.get(strings + name..)?;
        let end = text.iter().position(|&b| b == 0)?;
        names.push(String::from_utf8_lossy(&text[..end]).into_owned());
    }
    Some(names)
}
