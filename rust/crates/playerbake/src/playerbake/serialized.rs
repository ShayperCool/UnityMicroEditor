//! Writes a SerializedFile of the pinned engine with inline type trees, the
//! form the Player loads as a ContentFile.
use super::typetree::{SCRIPT_CLASS, TypeTree};
use anyhow::{Result, bail};

/// Linux standalone, the runtime target of adapted native content.
const RUNTIME_TARGET: u32 = 24;

pub struct Object<'a> {
    pub local_id: i64,
    pub tree: &'a TypeTree,
    pub data: Vec<u8>,
    /// For an object of a script class: the local ID of the `MonoScript`
    /// object in the same file that names the class.
    pub script: Option<i64>,
}

/// Another source asset this file refers to, in external-reference order.
/// The loader has to pass its ContentFile at the same position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct External {
    pub guid: String,
}

/// A class of which objects of the file hold objects by managed reference:
/// its name parts as the registry of such an object names it, and its type
/// tree.
#[derive(Clone, Copy)]
pub struct ReferenceType<'a> {
    pub class_name: &'a str,
    pub namespace: &'a str,
    pub assembly: &'a str,
    pub blob: &'a [u8],
}

impl ReferenceType<'_> {
    fn is(&self, other: &ReferenceType<'_>) -> bool {
        (self.class_name, self.namespace, self.assembly)
            == (other.class_name, other.namespace, other.assembly)
    }

    /// The type's entry of a file's table of such types, at a position.
    pub fn entry(&self, position: i16) -> Vec<u8> {
        let mut entry = Vec::with_capacity(self.blob.len() + 96);
        entry.extend_from_slice(&(-1i32).to_le_bytes());
        entry.push(0);
        entry.extend_from_slice(&position.to_le_bytes());
        // Neither a script nor a layout hash, as a build leaves them.
        entry.extend_from_slice(&[0u8; 32]);
        entry.extend_from_slice(&xxhash_rust::xxh3::xxh3_128(self.blob).to_le_bytes());
        entry.extend_from_slice(&(self.blob.len() as u32).to_le_bytes());
        entry.extend_from_slice(self.blob);
        for part in [self.class_name, self.namespace, self.assembly] {
            entry.extend_from_slice(part.as_bytes());
            entry.push(0);
        }
        entry
    }
}

pub fn write(objects: &[Object<'_>], externals: &[External]) -> Result<Vec<u8>> {
    write_with_references(objects, externals, &[])
}

/// Writes a file whose objects hold others by managed reference: `held`
/// lists, per object, the types of what it holds, and may end early.
pub fn write_with_references(
    objects: &[Object<'_>],
    externals: &[External],
    held: &[Vec<ReferenceType<'_>>],
) -> Result<Vec<u8>> {
    if objects.is_empty() {
        bail!("a serialized file needs at least one object");
    }
    // One type per class and, for script classes, per script, in order of
    // first use. The Player finds the class of a script-backed object through
    // the script type table, which lists the `MonoScript` objects by ID.
    let mut types: Vec<(&TypeTree, Option<i64>)> = Vec::new();
    let mut scripts = std::collections::BTreeSet::new();
    for object in objects {
        if (object.tree.class_id == SCRIPT_CLASS) != object.script.is_some() {
            bail!("exactly the objects of script classes name a script");
        }
        if let Some(script) = object.script {
            if !objects.iter().any(|other| other.local_id == script) {
                bail!("script-backed object names a script outside its file");
            }
            scripts.insert(script);
        }
        if !types
            .iter()
            .any(|(tree, script)| tree.class_id == object.tree.class_id && *script == object.script)
        {
            types.push((object.tree, object.script));
        }
    }
    if scripts.len() > i16::MAX as usize {
        bail!("a serialized file holds more script types than its format indexes");
    }
    // The held types in order of first use, and for each type of the file
    // the ones its objects hold.
    let mut references: Vec<ReferenceType<'_>> = Vec::new();
    let mut dependencies: Vec<Vec<u32>> = vec![Vec::new(); types.len()];
    for (object, held) in objects.iter().zip(held) {
        let index = types
            .iter()
            .position(|(tree, script)| {
                tree.class_id == object.tree.class_id && *script == object.script
            })
            .unwrap();
        for kind in held {
            let position = match references.iter().position(|known| known.is(kind)) {
                Some(position) => position,
                None => {
                    references.push(*kind);
                    references.len() - 1
                }
            } as u32;
            if !dependencies[index].contains(&position) {
                dependencies[index].push(position);
            }
        }
    }
    if references.len() > i16::MAX as usize {
        bail!("a serialized file holds more managed types than its format indexes");
    }
    let mut meta = Vec::new();
    meta.extend_from_slice(crate::ENGINE_VERSION.as_bytes());
    meta.push(0);
    meta.extend_from_slice(&RUNTIME_TARGET.to_le_bytes());
    meta.push(1); // Inline type trees.
    meta.extend_from_slice(&(types.len() as u32).to_le_bytes());
    for ((tree, script), held) in types.iter().zip(&mut dependencies) {
        let start = meta.len();
        // An entry ends in the count of the types its objects hold, zero as
        // the tree's own entry has it.
        let Some((body, [0, 0, 0, 0])) = tree.entry.split_last_chunk::<4>() else {
            bail!("type table entry already names the types its objects hold");
        };
        meta.extend_from_slice(body);
        held.sort_unstable();
        meta.extend_from_slice(&(held.len() as u32).to_le_bytes());
        for position in held.iter() {
            meta.extend_from_slice(&position.to_le_bytes());
        }
        if let Some(script) = script {
            let index = scripts.iter().position(|known| known == script).unwrap() as i16;
            meta[start + 5..start + 7].copy_from_slice(&index.to_le_bytes());
        }
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
            .position(|(tree, script)| {
                tree.class_id == object.tree.class_id && *script == object.script
            })
            .unwrap();
        meta.extend_from_slice(&object.local_id.to_le_bytes());
        meta.extend_from_slice(&(data.len() as u64).to_le_bytes());
        meta.extend_from_slice(&(object.data.len() as u32).to_le_bytes());
        meta.extend_from_slice(&(index as u32).to_le_bytes());
        data.extend_from_slice(&object.data);
    }
    meta.extend_from_slice(&(scripts.len() as u32).to_le_bytes());
    for script in &scripts {
        while !(48 + meta.len()).is_multiple_of(4) {
            meta.push(0);
        }
        meta.extend_from_slice(&0i32.to_le_bytes()); // This file.
        meta.extend_from_slice(&script.to_le_bytes());
    }
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
    meta.extend_from_slice(&(references.len() as u32).to_le_bytes());
    for (position, kind) in references.iter().enumerate() {
        meta.extend_from_slice(&kind.entry(position as i16));
    }
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
    /// The local ID of the object's `MonoScript`, for a script-backed object.
    pub script: Option<i64>,
}

/// Reads the objects of a little-endian SerializedFile v23 with inline type
/// trees, such as an import worker artifact or a baked file.
pub fn read(file: &[u8]) -> Result<Vec<Read>> {
    Ok(read_with_externals(file)?.0)
}

/// The same with the path names of the files the objects refer to, in the
/// order of the file's external references: a `PPtr` with file ID `n` leads
/// into the file at position `n - 1`.
pub fn read_with_externals(file: &[u8]) -> Result<(Vec<Read>, Vec<String>)> {
    read_objects(file, None)
}

/// The objects of one class that is not a script's, out of a file that may
/// hold what `read` refuses beside them: objects of script classes whose
/// scripts lie in other files, as the artifact of a package's importer
/// does.
pub fn read_of_class(file: &[u8], class_id: i32) -> Result<Vec<Read>> {
    Ok(read_objects(file, Some(class_id))?.0)
}

/// The path names of the files a file refers to, in the order of its
/// external references, whatever its objects are: a scene's behaviours take
/// their script types from another file.
pub fn external_paths(file: &[u8]) -> Result<Vec<String>> {
    Ok(read_objects(file, Some(i32::MIN))?.1)
}

fn read_objects(file: &[u8], only: Option<i32>) -> Result<(Vec<Read>, Vec<String>)> {
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
        let class = i32::from_le_bytes(header[..4].try_into().unwrap());
        let mut scripted = class == SCRIPT_CLASS;
        let script = i16::from_le_bytes([header[5], header[6]]);
        if only.is_some() {
            // An importer written in C# is an object of a native class
            // with a script.
            scripted |= script >= 0;
        }
        if header[4] != 0 || (only.is_none() && scripted != (script >= 0)) {
            bail!("SerializedFile holds stripped types or a script type without a class");
        }
        take(&mut at, if scripted { 48 } else { 32 })?;
        let blob = le32(&mut at)? as usize;
        take(&mut at, blob)?;
        // The types the objects of this type hold by managed reference.
        let held = le32(&mut at)? as usize;
        if held > 4096 {
            bail!("SerializedFile type dependency count is invalid");
        }
        take(&mut at, 4 * held)?;
        let wanted = only.is_none_or(|only| only == class && !scripted);
        trees.push((
            wanted
                .then(|| TypeTree::parse(file[start..at].to_vec()).map(std::sync::Arc::new))
                .transpose()?,
            script,
        ));
    }
    let object_count = le32(&mut at)? as usize;
    let mut objects = Vec::new();
    let mut indices = Vec::new();
    for _ in 0..object_count {
        at = (at + 3) & !3;
        let row = take(&mut at, 24)?;
        let local_id = i64::from_le_bytes(row[..8].try_into().unwrap());
        let offset = u64::from_le_bytes(row[8..16].try_into().unwrap());
        let length = u32::from_le_bytes(row[16..20].try_into().unwrap()) as u64;
        let index = u32::from_le_bytes(row[20..24].try_into().unwrap()) as usize;
        let start = data_offset
            .checked_add(offset)
            .filter(|start| start + length <= size);
        let (Some(start), Some((tree, script))) = (start, trees.get(index)) else {
            bail!("SerializedFile object bounds are invalid");
        };
        let Some(tree) = tree else { continue };
        indices.push(*script);
        objects.push(Read {
            local_id,
            tree: tree.clone(),
            data: file[start as usize..(start + length) as usize].to_vec(),
            script: None,
        });
    }
    // Only the external references of a file are asked for.
    let names_only = only == Some(i32::MIN);
    if only.is_some() && !names_only {
        return Ok((objects, Vec::new()));
    }
    let script_count = le32(&mut at)? as usize;
    let mut scripts = Vec::new();
    for _ in 0..script_count {
        at = (at + 3) & !3;
        let row = take(&mut at, 12)?;
        if names_only {
            continue;
        }
        if row[..4] != [0; 4] {
            bail!("SerializedFile takes a script type from another file");
        }
        scripts.push(i64::from_le_bytes(row[4..].try_into().unwrap()));
    }
    for (object, index) in objects.iter_mut().zip(indices) {
        if index >= 0 {
            let Some(script) = scripts.get(index as usize) else {
                bail!("SerializedFile script type index is invalid");
            };
            object.script = Some(*script);
        }
    }
    let external_count = le32(&mut at)? as usize;
    if external_count > 4096 {
        bail!("SerializedFile external reference count is invalid");
    }
    let mut externals = Vec::with_capacity(external_count);
    for _ in 0..external_count {
        // An unused asset path, a GUID and a type precede the path name.
        while take(&mut at, 1)?[0] != 0 {}
        take(&mut at, 20)?;
        let start = at;
        while take(&mut at, 1)?[0] != 0 {}
        externals.push(String::from_utf8_lossy(&file[start..at - 1]).into_owned());
    }
    Ok((objects, externals))
}
