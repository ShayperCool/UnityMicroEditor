//! Writes a SerializedFile of the pinned engine with inline type trees, the
//! form the Player loads as a ContentFile.
use super::typetree::TypeTree;
use anyhow::{Result, bail};

/// Linux standalone, the runtime target of adapted native content.
const RUNTIME_TARGET: u32 = 24;

pub struct Object<'a> {
    pub local_id: i64,
    pub tree: &'a TypeTree,
    pub data: Vec<u8>,
}

/// Another source asset this file refers to, in external-reference order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct External {
    pub guid: String,
    /// Unity's file kind: 2 for a native-format asset, 3 for an imported one.
    pub kind: u32,
}

fn native_guid(guid: &str) -> Result<[u8; 16]> {
    let bytes = guid.as_bytes();
    if bytes.len() != 32 {
        bail!("external reference GUID is invalid");
    }
    let mut result = [0u8; 16];
    for (index, pair) in bytes.chunks(2).enumerate() {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => bail!("external reference GUID is invalid"),
        };
        // Unity stores GUID nibbles swapped within each byte.
        result[index] = digit(pair[1])? << 4 | digit(pair[0])?;
    }
    Ok(result)
}

pub fn write(objects: &[Object<'_>], externals: &[External]) -> Result<Vec<u8>> {
    if objects.is_empty() {
        bail!("a serialized file needs at least one object");
    }
    let mut types: Vec<&TypeTree> = Vec::new();
    for object in objects {
        if !types
            .iter()
            .any(|tree| tree.class_id == object.tree.class_id)
        {
            types.push(object.tree);
        }
    }
    let mut meta = Vec::new();
    meta.extend_from_slice(crate::ENGINE_VERSION.as_bytes());
    meta.push(0);
    meta.extend_from_slice(&RUNTIME_TARGET.to_le_bytes());
    meta.push(1); // Inline type trees.
    meta.extend_from_slice(&(types.len() as u32).to_le_bytes());
    for tree in &types {
        meta.extend_from_slice(tree.entry);
    }
    meta.extend_from_slice(&(objects.len() as u32).to_le_bytes());
    // The object table is aligned relative to the file, whose header is 48 bytes.
    while !(48 + meta.len()).is_multiple_of(4) {
        meta.push(0);
    }
    let mut data = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for object in objects {
        if object.local_id == 0 || !seen.insert(object.local_id) {
            bail!("object local identities must be unique and non-zero");
        }
        while !data.len().is_multiple_of(16) {
            data.push(0);
        }
        let index = types
            .iter()
            .position(|tree| tree.class_id == object.tree.class_id)
            .unwrap();
        meta.extend_from_slice(&object.local_id.to_le_bytes());
        meta.extend_from_slice(&(data.len() as u64).to_le_bytes());
        meta.extend_from_slice(&(object.data.len() as u32).to_le_bytes());
        meta.extend_from_slice(&(index as u32).to_le_bytes());
        data.extend_from_slice(&object.data);
    }
    meta.extend_from_slice(&0u32.to_le_bytes()); // Script types.
    meta.extend_from_slice(&(externals.len() as u32).to_le_bytes());
    for external in externals {
        meta.push(0);
        meta.extend_from_slice(&native_guid(&external.guid)?);
        meta.extend_from_slice(&external.kind.to_le_bytes());
        meta.push(0);
    }
    meta.extend_from_slice(&0u32.to_le_bytes()); // Serialized reference types.
    meta.push(0); // User information.
    let mut data_offset = 48 + meta.len();
    while !data_offset.is_multiple_of(16) {
        data_offset += 1;
    }
    let size = data_offset + data.len();
    let mut out = vec![0u8; 48];
    out[8..12].copy_from_slice(&23u32.to_be_bytes());
    out[20..24].copy_from_slice(&(meta.len() as u32).to_be_bytes());
    out[24..32].copy_from_slice(&(size as u64).to_be_bytes());
    out[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
    out.extend_from_slice(&meta);
    out.resize(data_offset, 0);
    out.extend_from_slice(&data);
    Ok(out)
}
