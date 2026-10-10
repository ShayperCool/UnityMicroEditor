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
/// The loader has to pass its ContentFile at the same position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct External {
    pub guid: String,
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
    // An external reference carries no GUID or path: the Player then takes
    // the file from the dependency list it is given, by position. With a GUID
    // it would look the file up itself and find nothing.
    for _ in externals {
        meta.push(0);
        meta.extend_from_slice(&[0u8; 16]);
        meta.extend_from_slice(&0u32.to_le_bytes());
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
