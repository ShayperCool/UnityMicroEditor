//! The shader of a material that a model importer generates.
//!
//! Such a material names a shader built into the engine: an object of the
//! Editor's `unity_builtin_extra`, by its path ID there. A Player holds that
//! file too, but only with the shaders its project always includes. Any other
//! built-in shader a build uses is copied into the file of the asset that
//! uses it (`resources.assets`, `sharedassets<n>.assets`), under a path ID
//! of that file. So the reference of an imported material leads nowhere in a
//! Player, and the material draws with the error shader.
//!
//! A ContentFile can reach an object of the Player's own files. The loader
//! (`ContentLoadBackend::BuildLocalRemappingSet` of the pinned Player) takes
//! an external reference without a GUID from the dependency list it is
//! given; one with a GUID it looks up itself, by the path name of the
//! reference, among the files the Player has open. The file of the adapted
//! model therefore names the Player's file with a GUID that is no asset's,
//! and the material points at the path ID the shader has there.
use super::{Reader, adapt::Metadata, be32, be64, le32, le64};
use crate::playerbake::{Val, read_serialized_with_externals};
use anyhow::{Result, bail};

/// The GUID of the Editor's `unity_builtin_extra`.
pub const BUILTIN_EXTRA_GUID: &str = "0000000000000000f000000000000000";

const MATERIAL: i32 = 21;
const MESH_RENDERER: i32 = 23;
const SKINNED_MESH_RENDERER: i32 = 137;

/// What the rewritten reference carries in place of a GUID. The Player only
/// asks whether it is zero; it is no asset's and no built-in resource's.
const HOST_FILE_MARKER: [u8; 16] = *b"UME:host-file\0\0\x01";

/// A generated material and the built-in shader it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinShaderUse {
    pub material: String,
    /// The path ID of the shader in the Editor's `unity_builtin_extra`.
    pub shader: i64,
    /// The keywords the material enables.
    pub keywords: Vec<String>,
}

/// Where the Player that loads the file holds a shader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostShader {
    /// In its `unity_builtin_extra`, under the Editor's path ID: the
    /// reference resolves as it is.
    Builtin,
    /// In one of its own serialized files, by the file's name in the data
    /// directory and the path ID there.
    Copy { file: String, path_id: i64 },
}

fn pointer(value: &Val) -> Option<(i64, i64)> {
    match (value.field("m_FileID")?, value.field("m_PathID")?) {
        (Val::I(file), Val::I(path)) => Some((*file, *path)),
        _ => None,
    }
}

/// How many pointers of a value lead into the external file `index`.
fn count_pointers(value: &Val, index: i64) -> usize {
    match value {
        Val::Map(fields) => {
            if let Some((file, _)) = pointer(value) {
                return usize::from(file == index);
            }
            fields
                .iter()
                .map(|(_, field)| count_pointers(field, index))
                .sum()
        }
        Val::Seq(items) => items.iter().map(|item| count_pointers(item, index)).sum(),
        _ => 0,
    }
}

/// Where the parts of an adapted file lie.
pub(super) struct Layout {
    pub metadata_end: usize,
    pub data_offset: usize,
    /// Local ID, start in the file and size of every object.
    pub objects: Vec<(i64, usize, usize)>,
    /// Start and end of every external reference in the file.
    pub externals: Vec<(usize, usize)>,
}

pub(super) fn layout(file: &[u8]) -> Result<Layout> {
    if file.len() < 64 || be32(&file[8..12]) != 23 || file[16] != 0 {
        bail!("not a little-endian SerializedFile v23");
    }
    let metadata_end = 48 + be32(&file[20..24]) as usize;
    let data_offset = be64(&file[32..40]) as usize;
    if be64(&file[24..32]) != file.len() as u64 || metadata_end > data_offset {
        bail!("SerializedFile header bounds are invalid");
    }
    if data_offset > file.len() {
        bail!("SerializedFile data lies outside the file");
    }
    let mut r = Reader::at(&file[..metadata_end], 48);
    r.cstring_bytes(64)?;
    r.u32()?; // Target platform.
    if r.take(1)?[0] != 1 {
        bail!("SerializedFile has no inline type trees");
    }
    let types = r.u32()?;
    for _ in 0..types {
        // Class, flags and script index, two hashes, then the sized tree and
        // its count of referenced types.
        r.take(4 + 3 + 32)?;
        let size = r.u32()? as usize;
        r.take(size)?;
        if r.u32()? != 0 {
            bail!("SerializedFile type refers to managed reference types");
        }
    }
    let count = r.u32()? as usize;
    r.align();
    let mut objects = Vec::with_capacity(count.min(100_000));
    for _ in 0..count {
        let row = r.take(24)?;
        let start = (le64(&row[8..16]) as usize).saturating_add(data_offset);
        let size = le32(&row[16..20]) as usize;
        if start > file.len() || size > file.len() - start {
            bail!("SerializedFile object lies outside the file");
        }
        objects.push((le64(&row[..8]) as i64, start, size));
    }
    if r.u32()? != 0 {
        bail!("SerializedFile holds script types");
    }
    let count = r.u32()? as usize;
    let mut externals = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let start = r.pos;
        r.cstring_bytes(256)?;
        r.take(20)?;
        r.cstring_bytes(4096)?;
        externals.push((start, r.pos));
    }
    Ok(Layout {
        metadata_end,
        data_offset,
        objects,
        externals,
    })
}

/// Binds the generated materials of an adapted model to the shaders the host
/// Player holds. `resolve` says where the Player holds the shader of a
/// material, or refuses it. Returns `None` when no material of the file names
/// a built-in shader, or when every one resolves in the Player as it is.
///
/// Refused: a file that names another built-in object beside its shaders (a
/// renderer without a material of its own gets the engine's default
/// material), since one reference can lead into one file only, and materials
/// whose shaders lie in different files of the Player.
pub fn bind_builtin_shaders(
    file: &[u8],
    metadata: &mut Metadata,
    resolve: &mut dyn FnMut(&BuiltinShaderUse) -> Result<HostShader>,
) -> Result<Option<Vec<u8>>> {
    let Some(position) = metadata
        .external_references
        .iter()
        .position(|external| external.guid == BUILTIN_EXTRA_GUID)
    else {
        return Ok(None);
    };
    let index = position as i64 + 1;
    let (objects, _) = read_serialized_with_externals(file)?;
    let mut pointers = 0;
    let mut materials = Vec::new();
    for object in &objects {
        // The adapted file still lists the types of the objects it dropped.
        if !metadata
            .objects
            .iter()
            .any(|kept| kept.file_id == object.local_id.to_string())
            || !matches!(
                object.tree.class_id,
                MATERIAL | MESH_RENDERER | SKINNED_MESH_RENDERER
            )
        {
            continue;
        }
        let value = object.tree.decode(&object.data)?;
        pointers += count_pointers(&value, index);
        if object.tree.class_id != MATERIAL {
            continue;
        }
        let Some((file_index, shader)) = value.field("m_Shader").and_then(pointer) else {
            bail!("imported material has no shader reference");
        };
        if file_index != index {
            continue;
        }
        let name = match value.field("m_Name") {
            Some(Val::Str(name)) => name.clone(),
            _ => String::new(),
        };
        let mut keywords = Vec::new();
        if let Some(Val::Seq(valid)) = value.field("m_ValidKeywords") {
            for keyword in valid {
                if let Val::Str(keyword) = keyword {
                    keywords.push(keyword.clone());
                }
            }
        }
        let used = BuiltinShaderUse {
            material: name,
            shader,
            keywords,
        };
        let host = resolve(&used)?;
        materials.push((object, value, used, host));
    }
    if materials.is_empty() {
        return Ok(None);
    }
    let mut target: Option<&str> = None;
    let mut copies = 0;
    for (_, _, used, host) in &materials {
        let HostShader::Copy { file, .. } = host else {
            continue;
        };
        copies += 1;
        if target.is_some_and(|target| target != file) {
            bail!(
                "the materials of the model use built-in shaders that the host Player holds in different files ({} and {}, material {})",
                target.unwrap_or_default(),
                file,
                used.material
            );
        }
        target = Some(file);
    }
    let Some(target) = target.map(str::to_string) else {
        return Ok(None);
    };
    if copies != materials.len() || pointers != materials.len() {
        bail!(
            "the model refers to {} objects built into the engine and only {} of them are shaders the host Player holds in {target}; a part without a material of its own gets the engine's default material, which a Player does not hold",
            pointers,
            copies
        );
    }
    let layout = layout(file)?;
    let (start, end) = layout.externals[position];
    let mut metadata_bytes = file[48..start].to_vec();
    metadata_bytes.push(0);
    metadata_bytes.extend_from_slice(&HOST_FILE_MARKER);
    metadata_bytes.extend_from_slice(&0u32.to_le_bytes());
    metadata_bytes.extend_from_slice(target.as_bytes());
    metadata_bytes.push(0);
    metadata_bytes.extend_from_slice(&file[end..layout.metadata_end]);
    let mut data = file[layout.data_offset..].to_vec();
    for (object, value, _, host) in &mut materials {
        let HostShader::Copy { path_id, .. } = host else {
            continue;
        };
        let Some(shader) = value.field_mut("m_Shader") else {
            bail!("imported material has no shader reference");
        };
        *shader = Val::map([("m_FileID", Val::I(index)), ("m_PathID", Val::I(*path_id))]);
        let encoded = object.tree.encode(value)?;
        let Some(&(_, at, size)) = layout
            .objects
            .iter()
            .find(|(local_id, _, _)| *local_id == object.local_id)
        else {
            bail!("imported material is missing from the object table");
        };
        if encoded.len() != size {
            bail!("a rewritten shader reference changed the size of its material");
        }
        let at = at - layout.data_offset;
        data[at..at + size].copy_from_slice(&encoded);
        if let Some(kept) = metadata
            .objects
            .iter_mut()
            .find(|kept| kept.file_id == object.local_id.to_string())
        {
            kept.sha256 = crate::sha256(&encoded);
        }
    }
    let mut data_offset = 48 + metadata_bytes.len();
    while !data_offset.is_multiple_of(16) {
        data_offset += 1;
    }
    let mut output = file[..48].to_vec();
    output[20..24].copy_from_slice(&(metadata_bytes.len() as u32).to_be_bytes());
    output[24..32].copy_from_slice(&((data_offset + data.len()) as u64).to_be_bytes());
    output[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
    output.extend_from_slice(&metadata_bytes);
    output.resize(data_offset, 0);
    output.extend_from_slice(&data);
    // The loader is still given the global table for this reference; what
    // the file names in it is recorded beside the GUID it stands for.
    metadata.external_references[position].path = target;
    metadata.output_sha256 = crate::sha256(&output);
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtimeartifact::{ExternalReference, Object};

    /// The artifact a worker left for an `.obj` without a material of its
    /// own, adapted: one generated material that names `Standard`.
    fn adapted() -> (Vec<u8>, Metadata) {
        let artifact =
            include_bytes!("../../tests/testdata/native/generated-material.obj.artifact");
        crate::runtimeartifact::adapt(artifact, "fbx").unwrap()
    }

    fn standard(used: &BuiltinShaderUse) -> Result<HostShader> {
        assert_eq!(
            (used.material.as_str(), used.shader, used.keywords.len()),
            ("defaultMat", 46, 0)
        );
        Ok(HostShader::Copy {
            file: "resources.assets".into(),
            path_id: 5,
        })
    }

    #[test]
    fn a_generated_material_takes_its_shader_from_the_players_file() {
        let (file, mut metadata) = adapted();
        let before = metadata.clone();
        let output = bind_builtin_shaders(&file, &mut metadata, &mut standard)
            .unwrap()
            .unwrap();
        // The reference names the Player's file with a GUID, the material the
        // path ID there; nothing else of the file changes.
        let (objects, externals) = read_serialized_with_externals(&output).unwrap();
        assert_eq!(externals, ["resources.assets"]);
        let (was, _) = read_serialized_with_externals(&file).unwrap();
        assert_eq!(objects.len(), was.len());
        let mut changed = 0;
        for (object, old) in objects.iter().zip(&was) {
            assert_eq!(object.local_id, old.local_id);
            if object.data == old.data {
                continue;
            }
            changed += 1;
            assert_eq!(object.tree.class_id, MATERIAL);
            let value = object.tree.decode(&object.data).unwrap();
            assert_eq!(value.field("m_Shader").and_then(pointer), Some((1, 5)));
            let mut restored = value.clone();
            *restored.field_mut("m_Shader").unwrap() =
                Val::map([("m_FileID", Val::I(1)), ("m_PathID", Val::I(46))]);
            assert_eq!(object.tree.encode(&restored).unwrap(), old.data);
        }
        assert_eq!(changed, 1);
        let at = output
            .windows(HOST_FILE_MARKER.len())
            .position(|window| window == HOST_FILE_MARKER)
            .unwrap();
        assert_eq!(&output[at + 16..at + 20], [0u8; 4]);
        assert_eq!(&output[at + 20..at + 37], b"resources.assets\0");
        // The descriptor still asks the loader for the global table.
        assert_eq!(
            metadata.external_references,
            [ExternalReference {
                guid: BUILTIN_EXTRA_GUID.into(),
                kind: before.external_references[0].kind,
                path: "resources.assets".into(),
            }]
        );
        assert_eq!(metadata.output_sha256, crate::sha256(&output));
        let different: Vec<&Object> = metadata
            .objects
            .iter()
            .zip(&before.objects)
            .filter(|(now, was)| now != was)
            .map(|(now, _)| now)
            .collect();
        assert_eq!(different.len(), 1);
        assert_eq!(different[0].class_id, MATERIAL);
    }

    #[test]
    fn a_shader_the_player_holds_among_its_built_in_resources_stays() {
        let (file, mut metadata) = adapted();
        let before = metadata.clone();
        let output =
            bind_builtin_shaders(&file, &mut metadata, &mut |_| Ok(HostShader::Builtin)).unwrap();
        assert_eq!(output, None);
        assert_eq!(metadata, before);
    }

    #[test]
    fn a_refused_shader_refuses_the_model() {
        let (file, mut metadata) = adapted();
        let error = bind_builtin_shaders(&file, &mut metadata, &mut |used| {
            bail!("no shader {} for {}", used.shader, used.material)
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "no shader 46 for defaultMat");
    }

    #[test]
    fn a_file_without_built_in_references_is_left_alone() {
        let (file, mut metadata) = adapted();
        metadata.external_references.clear();
        let output = bind_builtin_shaders(&file, &mut metadata, &mut standard).unwrap();
        assert_eq!(output, None);
    }
}
