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
        meta.extend_from_slice(&tree.entry);
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

/// An object of a SerializedFile with its type tree.
pub struct Read {
    pub local_id: i64,
    pub tree: std::sync::Arc<TypeTree>,
    pub data: Vec<u8>,
}

/// Reads the objects of a little-endian SerializedFile v23 with inline type
/// trees and no script types, such as an import worker artifact.
pub fn read(file: &[u8]) -> Result<Vec<Read>> {
    let be32 = |at: usize| {
        file.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    let be64 = |at: usize| {
        file.get(at..at + 8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    };
    let (Some(23), Some(0), Some(meta_size), Some(size), Some(data_offset)) =
        (be32(8), file.get(16).copied(), be32(20), be64(24), be64(32))
    else {
        bail!("not a little-endian SerializedFile v23");
    };
    let meta_end = 48 + meta_size as usize;
    if size != file.len() as u64 || meta_end as u64 > data_offset || data_offset > size {
        bail!("SerializedFile header bounds are invalid");
    }
    let mut at = 48;
    let take = |at: &mut usize, count: usize| -> Result<&[u8]> {
        let Some(bytes) = file
            .get(*at..*at + count)
            .filter(|_| *at + count <= meta_end)
        else {
            bail!("SerializedFile metadata is truncated");
        };
        *at += count;
        Ok(bytes)
    };
    let le32 = |at: &mut usize| -> Result<u32> {
        Ok(u32::from_le_bytes(take(at, 4)?.try_into().unwrap()))
    };
    while take(&mut at, 1)?[0] != 0 {}
    le32(&mut at)?; // Target platform.
    if take(&mut at, 1)?[0] != 1 {
        bail!("SerializedFile has no inline type trees");
    }
    let type_count = le32(&mut at)? as usize;
    if type_count > 4096 {
        bail!("SerializedFile type count is invalid");
    }
    let mut trees = Vec::with_capacity(type_count);
    for _ in 0..type_count {
        let start = at;
        let header = take(&mut at, 7)?;
        if header[4] != 0 || header[5..7] != [0xff, 0xff] || header[..4] == 114u32.to_le_bytes() {
            bail!("SerializedFile holds stripped or script-backed types");
        }
        take(&mut at, 32)?;
        let blob = le32(&mut at)? as usize;
        take(&mut at, blob)?;
        if le32(&mut at)? != 0 {
            bail!("SerializedFile type has serialized reference dependencies");
        }
        trees.push(std::sync::Arc::new(TypeTree::parse(
            file[start..at].to_vec(),
        )?));
    }
    let object_count = le32(&mut at)? as usize;
    at = (at + 3) & !3;
    let mut objects = Vec::new();
    for _ in 0..object_count {
        let row = take(&mut at, 24)?;
        let local_id = i64::from_le_bytes(row[..8].try_into().unwrap());
        let offset = u64::from_le_bytes(row[8..16].try_into().unwrap());
        let length = u32::from_le_bytes(row[16..20].try_into().unwrap()) as u64;
        let index = u32::from_le_bytes(row[20..24].try_into().unwrap()) as usize;
        let start = data_offset
            .checked_add(offset)
            .filter(|start| start + length <= size);
        let (Some(start), Some(tree)) = (start, trees.get(index)) else {
            bail!("SerializedFile object bounds are invalid");
        };
        objects.push(Read {
            local_id,
            tree: tree.clone(),
            data: file[start as usize..(start + length) as usize].to_vec(),
        });
    }
    Ok(objects)
}
