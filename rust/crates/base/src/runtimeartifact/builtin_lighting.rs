use super::{Reader, be32, be64, le16, le32, le64, req, text};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// What the lighting reader reads from: a file or bytes in memory.
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

fn read_at<R: ReadAt + ?Sized>(input: &R, size: usize, offset: i64) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; size];
    input
        .read_exact_at(&mut buffer, offset as u64)
        .context("native resource ends before its declared data")?;
    Ok(buffer)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinLightingIdentity {
    pub unity_version: String,
    pub file_id: i64,
    pub class_id: i32,
    pub type_tree_sha256: String,
    pub payload_sha256: String,
    pub lightmap_count: i64,
    pub light_probe_count: i64,
}

// Filled from the immutable native engine resource and checked independently
// against the public Editor and standalone binary2text readbacks.
const BUILTIN_LIGHTING_TREE_SHA: &str =
    "ff2620f5eb07441a22bc1862441790fcbc524bfb5f0c5713cfa135b29c581818";
const BUILTIN_LIGHTING_PAYLOAD_SHA: &str =
    "c4102142e2fc2b9e09e5e541122478e2883bbeb7d75c6c42ff2e5027bb919f4a";

/// Reads the exact native resource object. Its accepted tree/payload
/// fingerprints are independently checked against binary2text and the public
/// Editor observations for the pinned engine. A same-ID resource is
/// insufficient: changed layouts or data fail closed.
pub fn inspect_builtin_lighting<R: ReadAt + ?Sized>(
    input: &R,
    size: i64,
) -> Result<BuiltinLightingIdentity> {
    let object = read_builtin_resource_object(input, size, 20201)?;
    let proof = BuiltinLightingIdentity {
        unity_version: object.version,
        file_id: 20201,
        class_id: object.class,
        type_tree_sha256: crate::sha256(&object.tree),
        payload_sha256: crate::sha256(&object.payload),
        lightmap_count: 0,
        light_probe_count: 0,
    };
    if object.class != 1120 {
        bail!("native builtin20201 is not the verified LightingDataAsset");
    }
    if proof.type_tree_sha256 != BUILTIN_LIGHTING_TREE_SHA
        || proof.payload_sha256 != BUILTIN_LIGHTING_PAYLOAD_SHA
    {
        bail!(
            "native builtin lighting data differs from its independently verified empty-lightmaps/probes profile"
        );
    }
    Ok(proof)
}

struct BuiltinResourceObject {
    version: String,
    class: i32,
    tree: Vec<u8>,
    payload: Vec<u8>,
}

fn read_builtin_resource_object<R: ReadAt + ?Sized>(
    input: &R,
    size: i64,
    wanted: i64,
) -> Result<BuiltinResourceObject> {
    let header = read_at(input, 48, 0)?;
    if !(64..=256 << 20).contains(&size)
        || be32(&header[8..12]) != 23
        || header[16] != 0
        || be64(&header[24..32]) as i64 != size
    {
        bail!("builtin resource requires bounded little-endian native SerializedFile v23");
    }
    let metadata_size = be32(&header[20..24]) as i64;
    let data_offset = be64(&header[32..40]) as i64;
    if !(16..=4 << 20).contains(&metadata_size)
        || 48 + metadata_size > data_offset
        || data_offset > size
    {
        bail!("builtin resource metadata bounds are invalid");
    }
    let metadata = read_at(input, metadata_size as usize, 48)?;
    let mut r = Reader::new(&metadata);
    let version = req(
        r.cstring_bytes(64),
        |version| std::str::from_utf8(version).is_ok_and(crate::release::is_supported),
        "builtin resource engine version is not a supported release",
    )?;
    req(
        r.u32(),
        |platform| *platform as i32 == -2,
        "builtin resource requires native Editor resource metadata",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "builtin resource type trees are absent",
    )?;
    let count = req(
        r.u32(),
        |count| *count != 0 && *count <= 1024,
        "builtin resource type count is invalid",
    )?;
    let mut types = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let class = r.u32()?;
        let flags = req(
            r.take(3),
            |flags| flags[0] == 0,
            "stripped native builtin types are unsupported",
        )?;
        if le16(&flags[1..]) as i16 >= 0 || class == 114 || (class as i32) < 0 {
            r.take(16)?;
        }
        r.take(32)?;
        let length = req(
            r.u32(),
            |length| (16..=4 << 20).contains(length),
            "builtin type tree bounds are invalid",
        )?;
        let tree = req(
            r.take(length as usize),
            |tree| le32(&tree[..4]) == 0x7474686d && le32(&tree[4..8]) == 23,
            "builtin type tree format is unsupported",
        )?;
        let (nodes, chars) = (le32(&tree[8..12]) as u64, le32(&tree[12..16]) as u64);
        if nodes == 0 || nodes > 65536 || 16 + nodes * 32 + chars != tree.len() as u64 {
            bail!("builtin type tree layout is invalid");
        }
        let dependencies = req(
            r.u32(),
            |dependencies| *dependencies <= 1024,
            "builtin type dependencies are invalid",
        )?;
        r.take(dependencies as usize * 4)?;
        types.push((class as i32, tree));
    }
    let objects = req(
        r.u32(),
        |objects| *objects != 0 && *objects <= 100000,
        "builtin object count is invalid",
    )?;
    r.align();
    let mut seen = HashSet::new();
    let mut result = None;
    for _ in 0..objects {
        let row = r.take(24)?;
        let id = le64(&row[..8]) as i64;
        let (offset, length) = (le64(&row[8..16]) as i64, le32(&row[16..20]) as i64);
        let type_index = le32(&row[20..24]);
        if id == 0
            || seen.contains(&id)
            || type_index >= count
            || offset < 0
            || length <= 0
            || offset > size - data_offset
            || length > size - data_offset - offset
        {
            bail!("builtin object identities or payload bounds are invalid");
        }
        seen.insert(id);
        if id != wanted {
            continue;
        }
        if length > 4 << 20 {
            bail!("selected builtin resource object exceeds its bound");
        }
        let payload = read_at(input, length as usize, data_offset + offset)?;
        let (class, tree) = types[type_index as usize];
        result = Some(BuiltinResourceObject {
            version: text(version),
            class,
            tree: tree.to_vec(),
            payload,
        });
    }
    match result {
        Some(result) => Ok(result),
        None => bail!("builtin resource object {wanted} is absent"),
    }
}
