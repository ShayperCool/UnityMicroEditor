use super::req;
use anyhow::{Result, bail};
use std::collections::HashSet;

const LIMIT: usize = 128 << 20;

struct BundleReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BundleReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos > self.data.len() || n > self.data.len() - self.pos {
            bail!("truncated UnityFS metadata");
        }
        let bytes = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }
    fn string(&mut self) -> Result<&'a [u8]> {
        if self.pos > self.data.len() {
            bail!("truncated UnityFS string");
        }
        match self.data[self.pos..].iter().position(|&byte| byte == 0) {
            Some(end) if end <= 4096 => Ok(&self.take(end + 1)?[..end]),
            _ => bail!("UnityFS string exceeds supported size"),
        }
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(super::be32(self.take(4)?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(super::be64(self.take(8)?))
    }
}

/// Proves that an extracted SerializedFile is the named member of the pinned
/// archive. No scene or engine object is instantiated. This bounded profile
/// supports uncompressed and LZ4 UnityFS version 7/8.
pub fn verify_unityfs_member(archive: &[u8], name: &str, member: &[u8]) -> Result<()> {
    if archive.len() > LIMIT || member.len() > LIMIT || name.is_empty() {
        bail!("UnityFS member verification exceeds the supported profile");
    }
    let mut r = BundleReader {
        data: archive,
        pos: 0,
    };
    req(
        r.string(),
        |signature| *signature == b"UnityFS",
        "compiled Shader origin is not a UnityFS archive",
    )?;
    req(
        r.u32(),
        |version| *version == 7 || *version == 8,
        "UnityFS archive version is unsupported",
    )?;
    r.string()?;
    r.string()?;
    req(
        r.u64(),
        |size| *size == archive.len() as u64,
        "UnityFS archive size is inconsistent",
    )?;
    let compressed = r.u32()? as usize;
    let uncompressed = req(
        r.u32(),
        |size| *size as usize <= LIMIT,
        "UnityFS block directory size exceeds the supported profile",
    )? as usize;
    let flags = req(
        r.u32(),
        |flags| flags & !0x3ff == 0 && flags & 0x40 != 0,
        "UnityFS archive flags are unsupported",
    )?;
    r.pos = (r.pos + 15) & !15;
    let mut info_pos = r.pos as i64;
    if flags & 0x80 != 0 {
        info_pos = archive.len() as i64 - compressed as i64;
    }
    if info_pos < r.pos as i64 || compressed as i64 > archive.len() as i64 - info_pos {
        bail!("UnityFS block directory bounds are invalid");
    }
    let info_pos = info_pos as usize;
    let info = unpack_unityfs_block(
        &archive[info_pos..info_pos + compressed],
        uncompressed,
        flags & 0x3f,
    )?;
    let mut directory = BundleReader {
        data: &info,
        pos: 0,
    };
    directory.take(16)?;
    let count = req(
        directory.u32(),
        |count| *count <= 4096,
        "UnityFS block count exceeds the supported profile",
    )?;
    let mut blocks = Vec::with_capacity(count as usize);
    let mut total = 0u64;
    for _ in 0..count {
        let u = directory.u32()?;
        let c = directory.u32()?;
        let f = req(
            directory.u16(),
            |flags| flags & !0x7f == 0,
            "UnityFS block flags are unsupported",
        )?;
        total += u as u64;
        if total > LIMIT as u64 {
            bail!("UnityFS content exceeds the supported size");
        }
        blocks.push((c as usize, u as usize, f));
    }
    let nodes = req(
        directory.u32(),
        |nodes| *nodes <= 4096,
        "UnityFS member count exceeds the supported profile",
    )?;
    let mut selected = None;
    let mut names: HashSet<&[u8]> = HashSet::new();
    for _ in 0..nodes {
        let o = directory.u64()?;
        let s = directory.u64()?;
        directory.u32()?;
        let n = req(
            directory.string(),
            |n| !n.is_empty() && !names.contains(n) && o <= total && s <= total - o,
            "UnityFS member identity/bounds are invalid",
        )?;
        names.insert(n);
        if n == name.as_bytes() {
            selected = Some((o as usize, s as usize));
        }
    }
    let selected =
        selected.filter(|(_, size)| directory.pos == info.len() && *size == member.len());
    let Some((selected_offset, selected_size)) = selected else {
        bail!("UnityFS exact member was not found or differs in size");
    };
    let mut data_pos = r.pos;
    let mut data_end = archive.len();
    if flags & 0x80 == 0 {
        data_pos += compressed;
    } else {
        data_end = info_pos;
    }
    if flags & 0x200 != 0 {
        data_pos = (data_pos + 15) & !15;
    }
    let mut content = Vec::with_capacity(total as usize);
    for (block_compressed, block_uncompressed, block_flags) in blocks {
        if data_pos > data_end || block_compressed > data_end - data_pos {
            bail!("UnityFS content block is truncated");
        }
        content.extend_from_slice(&unpack_unityfs_block(
            &archive[data_pos..data_pos + block_compressed],
            block_uncompressed,
            (block_flags & 0x3f) as u32,
        )?);
        data_pos += block_compressed;
    }
    if data_pos != data_end {
        bail!("UnityFS archive contains unexplained content bytes");
    }
    if &content[selected_offset..selected_offset + selected_size] != member {
        bail!("compiled Shader member bytes differ from the pinned UnityFS archive");
    }
    Ok(())
}

fn lz4_length(input: &[u8], pos: &mut usize, base: usize, size: usize) -> Result<usize> {
    let mut value = base;
    if base == 15 {
        loop {
            if *pos == input.len() {
                bail!("truncated UnityFS LZ4 length");
            }
            let x = input[*pos] as usize;
            *pos += 1;
            value += x;
            if value > size {
                bail!("UnityFS LZ4 length exceeds output");
            }
            if x != 255 {
                break;
            }
        }
    }
    Ok(value)
}

/// Raw (0) and LZ4/LZ4HC block (2, 3) payloads only; LZMA is refused.
pub fn unpack_unityfs_block(input: &[u8], size: usize, compression: u32) -> Result<Vec<u8>> {
    if size > LIMIT {
        bail!("UnityFS block exceeds supported size");
    }
    if compression == 0 {
        if input.len() != size {
            bail!("UnityFS raw block size is inconsistent");
        }
        return Ok(input.to_vec());
    }
    if compression != 2 && compression != 3 {
        bail!("UnityFS compression {compression} is unsupported");
    }
    let mut output = Vec::with_capacity(size);
    let mut pos = 0;
    while pos < input.len() {
        let token = input[pos];
        pos += 1;
        let literal = lz4_length(input, &mut pos, (token >> 4) as usize, size)?;
        if literal > input.len() - pos || literal > size - output.len() {
            bail!("UnityFS LZ4 literal bounds are invalid");
        }
        output.extend_from_slice(&input[pos..pos + literal]);
        pos += literal;
        if pos == input.len() {
            break;
        }
        if input.len() - pos < 2 {
            bail!("truncated UnityFS LZ4 offset");
        }
        let offset = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
        pos += 2;
        if offset == 0 || offset > output.len() {
            bail!("UnityFS LZ4 offset is invalid");
        }
        let matched = lz4_length(input, &mut pos, (token & 15) as usize, size)? + 4;
        if matched > size - output.len() {
            bail!("UnityFS LZ4 match exceeds output");
        }
        for _ in 0..matched {
            output.push(output[output.len() - offset]);
        }
    }
    if output.len() != size {
        bail!("UnityFS LZ4 output size differs");
    }
    Ok(output)
}
