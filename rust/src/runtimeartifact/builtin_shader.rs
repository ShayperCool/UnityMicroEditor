use super::{
    Reader, be32, be64, le16, le32, le64,
    player_shader::{ShaderType, common_string, read_shader_identity},
    req, text,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Positioned reads: fill the whole buffer from an absolute offset or fail.
pub trait ReadAt {
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<()>;
}
impl ReadAt for [u8] {
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
        let source = usize::try_from(offset)
            .ok()
            .and_then(|offset| self.get(offset..))
            .and_then(|rest| rest.get(..buffer.len()))
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        buffer.copy_from_slice(source);
        Ok(())
    }
}
impl ReadAt for Vec<u8> {
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.as_slice().read_exact_at(buffer, offset)
    }
}
impl ReadAt for std::fs::File {
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buffer, offset)
    }
}

pub(crate) fn read_at<R: ReadAt + ?Sized>(input: &R, size: usize, offset: i64) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; size];
    input
        .read_exact_at(&mut buffer, offset as u64)
        .context("native resource ends before its declared data")?;
    Ok(buffer)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinShaderIdentity {
    pub unity_version: String,
    pub file_id: i64,
    pub name: String,
    pub payload_sha256: String,
}

/// Reads only the native resource metadata and one shader object. The Editor
/// resource is identity evidence and is never loaded by Unity.
pub fn inspect_builtin_shader<R: ReadAt + ?Sized>(
    input: &R,
    size: i64,
    id: i64,
) -> Result<BuiltinShaderIdentity> {
    let mut proof = BuiltinShaderIdentity::default();
    let header = read_at(input, 48, 0)?;
    if be32(&header[8..12]) != 23
        || header[16] != 0
        || !(64..=256 << 20).contains(&size)
        || be64(&header[24..32]) as i64 != size
    {
        bail!("built-in Shader source requires bounded native SerializedFile v23");
    }
    let metadata_size = be32(&header[20..24]) as i64;
    let data_offset = be64(&header[32..40]) as i64;
    if !(16..=4 << 20).contains(&metadata_size)
        || 48 + metadata_size > data_offset
        || data_offset > size
    {
        bail!("built-in Shader native metadata bounds are invalid");
    }
    let metadata = read_at(input, metadata_size as usize, 48)?;
    let mut r = Reader::new(&metadata);
    let version = req(
        r.cstring_bytes(64),
        |version| *version == b"6000.6.3f1",
        "built-in Shader source requires pinned engine resources",
    )?;
    req(
        r.u32(),
        |platform| *platform as i32 == -2,
        "built-in Shader identity must use native Editor resources",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "built-in Shader source requires inline native trees",
    )?;
    let count = req(
        r.u32(),
        |count| *count != 0 && *count <= 1024,
        "built-in Shader type count is invalid",
    )?;
    let mut types = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let class = r.u32()?;
        let flags = r.take(3)?;
        if le16(&flags[1..]) as i16 >= 0 || class == 114 || (class as i32) < 0 {
            r.take(16)?;
        }
        r.take(32)?;
        let length = req(
            r.u32(),
            |length| (16..=4 << 20).contains(length),
            "built-in Shader tree bounds are invalid",
        )?;
        let tree = r.take(length as usize)?;
        if le32(&tree[..4]) != 0x7474686d || le32(&tree[4..8]) != 23 {
            bail!("built-in Shader tree format is unsupported");
        }
        let (nodes, chars) = (le32(&tree[8..12]) as u64, le32(&tree[12..16]) as u64);
        if nodes == 0 || 16 + nodes * 32 + chars != tree.len() as u64 {
            bail!("built-in Shader tree layout is invalid");
        }
        let dependencies = req(
            r.u32(),
            |dependencies| *dependencies <= 1024,
            "built-in Shader type dependencies are invalid",
        )?;
        r.take(dependencies as usize * 4)?;
        types.push(ShaderType {
            class: class as i32,
            bytes: &[],
            tree,
        });
    }
    let objects = req(
        r.u32(),
        |objects| *objects != 0 && *objects <= 100000,
        "built-in Shader object count is invalid",
    )?;
    r.align();
    let mut seen = HashSet::new();
    for _ in 0..objects {
        let row = r.take(24)?;
        let local_id = le64(&row[..8]) as i64;
        let (offset, length) = (le64(&row[8..16]) as i64, le32(&row[16..20]) as i64);
        let type_index = le32(&row[20..24]);
        if local_id == 0
            || seen.contains(&local_id)
            || type_index >= count
            || offset < 0
            || length <= 0
            || offset > size - data_offset
            || length > size - data_offset - offset
        {
            bail!("built-in Shader object table is invalid");
        }
        seen.insert(local_id);
        if local_id != id {
            continue;
        }
        let kind = &types[type_index as usize];
        if kind.class != 48 || length > 4 << 20 {
            bail!("built-in identity is not a bounded native Shader");
        }
        let payload = read_at(input, length as usize, data_offset + offset)?;
        let mut name = builtin_shader_name(&payload, kind.tree)?;
        if name.is_empty() {
            name = read_shader_identity(&payload, kind.tree, false)?.proof.name;
        }
        proof = BuiltinShaderIdentity {
            unity_version: text(version),
            file_id: id,
            name,
            payload_sha256: crate::sha256(&payload),
        };
    }
    if proof.file_id == id && proof.name.is_empty() {
        bail!("native built-in Shader has an empty top-level name");
    }
    if proof.name.is_empty() {
        bail!("built-in Shader source identity is absent");
    }
    Ok(proof)
}

fn builtin_shader_name(payload: &[u8], tree: &[u8]) -> Result<String> {
    let count = le32(&tree[8..12]) as usize;
    let nodes = &tree[16..16 + count * 32];
    let chars = &tree[16 + count * 32..];
    let mut data = Reader::new(payload);
    for i in 1..count {
        let node = &nodes[i * 32..(i + 1) * 32];
        if node[2] != 1 {
            continue;
        }
        let offset = le32(&node[8..12]);
        let mut name = String::new();
        if offset & 0x80000000 != 0 {
            name = common_string(offset & 0x7fffffff)
                .unwrap_or_default()
                .to_string();
        } else if (offset as usize) < chars.len() {
            name = Reader::new(&chars[offset as usize..])
                .cstring(256)
                .unwrap_or_default();
        }
        if name == "m_Name" {
            return data.string();
        }
        let length = le32(&node[12..16]) as i32;
        if length < 0 {
            bail!("unverified built-in Shader identity prefix {name}");
        }
        data.take(length as usize)?;
        if le32(&node[20..24]) & 0x4000 != 0 {
            data.align();
        }
    }
    bail!("built-in Shader source has no native name")
}
