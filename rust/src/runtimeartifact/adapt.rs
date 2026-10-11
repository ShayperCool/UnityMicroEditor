use super::{
    MAXIMUM_BYTES, Reader, be32, be64, le16, le32, le64, lower_hex,
    model_graph::{ModelGraphObject, hash_model_graph, read_model_graph},
    ordinary_guid, req,
    texture::TextureMetadata,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Object {
    #[serde(rename = "fileId")]
    pub file_id: String,
    #[serde(rename = "classId")]
    pub class_id: i32,
    #[serde(rename = "typeName")]
    pub type_name: String,
    #[serde(rename = "dataSHA256")]
    pub sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedObject {
    pub file_id: String,
    pub class_id: i32,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExternalReference {
    pub guid: String,
    #[serde(rename = "type")]
    pub kind: i32,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub virtual_path: String,
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioMetadata {
    pub load_type: u32,
    pub compression_format: u32,
    pub channels: u32,
    pub frequency: u32,
    pub length: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub profile: String,
    pub serialized_version: u32,
    pub source_target: i32,
    pub runtime_target: i32,
    #[serde(rename = "inputSHA256")]
    pub input_sha256: String,
    #[serde(rename = "outputSHA256")]
    pub output_sha256: String,
    #[serde(default)]
    pub objects: Vec<Object>,
    #[serde(default)]
    pub removed: Vec<RemovedObject>,
    #[serde(default)]
    pub external_references: Vec<ExternalReference>,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture: Option<TextureMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_graph: Vec<ModelGraphObject>,
    #[serde(
        default,
        rename = "modelTopologySHA256",
        skip_serializing_if = "String::is_empty"
    )]
    pub model_topology_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_graph_unsupported_reason: String,
}

pub(crate) fn fbx_class(class_id: i32) -> &'static str {
    match class_id {
        1 => "UnityEngine.GameObject",
        4 => "UnityEngine.Transform",
        20 => "UnityEngine.Camera",
        21 => "UnityEngine.Material",
        23 => "UnityEngine.MeshRenderer",
        33 => "UnityEngine.MeshFilter",
        43 => "UnityEngine.Mesh",
        74 => "UnityEngine.AnimationClip",
        90 => "UnityEngine.Avatar",
        95 => "UnityEngine.Animator",
        108 => "UnityEngine.Light",
        111 => "UnityEngine.Animation",
        137 => "UnityEngine.SkinnedMeshRenderer",
        205 => "UnityEngine.LODGroup",
        _ => "",
    }
}

pub(crate) struct ObjectEntry<'a> {
    pub id: i64,
    pub class_id: i32,
    pub offset: u64,
    pub size: u64,
    pub row: &'a [u8],
}

/// Accepts only little-endian, inline-type-tree SerializedFile v23 native FBX
/// or preloaded PCM/Vorbis WAV artifacts. Shader/script/other import profiles
/// fail closed. Known importer/prefab/preview object table entries are removed;
/// local IDs and every retained object's data stay unchanged. Native import
/// content hashes must be verified by the caller before using this projection.
pub fn adapt(input: &[u8], profile: &str) -> Result<(Vec<u8>, Metadata)> {
    let mut result = Metadata::default();
    if profile != "fbx" && profile != "wav" {
        bail!("unsupported native runtime profile {profile:?}");
    }
    if input.len() < 64
        || input.len() > MAXIMUM_BYTES
        || be32(&input[8..12]) != 23
        || input[16] != 0
    {
        bail!(
            "native runtime projection requires little-endian SerializedFile v23, at most 64 MiB"
        );
    }
    let file_size = be64(&input[24..32]);
    let data_offset = be64(&input[32..40]);
    let metadata_size = be32(&input[20..24]) as u64;
    let metadata_end = 48 + metadata_size;
    if file_size != input.len() as u64 || metadata_end > data_offset || data_offset > file_size {
        bail!("native SerializedFile header bounds are invalid");
    }
    let (metadata_end, data_offset) = (metadata_end as usize, data_offset as usize);
    let payload = input.len() - data_offset;
    let mut r = Reader::new(&input[48..metadata_end]);
    req(
        r.cstring_bytes(64),
        |version| *version == b"0.0.0" || *version == b"6000.6.3f1",
        "artifact must be from the pinned Unity 6000.6.3f1 format",
    )?;
    let platform_offset = 48 + r.pos;
    req(
        r.u32(),
        |platform| *platform as i32 == -2,
        "artifact must declare native Editor target -2 before runtime projection",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "native runtime projection requires inline type trees",
    )?;
    let type_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 256,
        "artifact type count is invalid",
    )?;
    let mut classes = Vec::with_capacity(type_count as usize);
    let mut type_trees: HashMap<i32, String> = HashMap::new();
    for _ in 0..type_count {
        let class_id = r.u32()?;
        classes.push(class_id as i32);
        req(
            r.take(3),
            |flags| {
                flags[0] == 0
                    && le16(&flags[1..]) as i16 == -1
                    && class_id != 114
                    && class_id as i32 != -1
            },
            "stripped or script-backed native content is outside supported profiles",
        )?;
        r.take(32)?; // Structure and inline type tree content hashes.
        let size = req(r.u32(), |size| *size >= 16, "native type tree is missing")?;
        let blob = req(
            r.take(size as usize),
            |blob| le32(&blob[..4]) == 0x7474686d && le32(&blob[4..8]) == 23,
            "unsupported inline native type tree format",
        )?;
        let nodes = le32(&blob[8..12]) as u64;
        let chars = le32(&blob[12..16]) as u64;
        if nodes == 0 || 16 + nodes * 32 + chars != blob.len() as u64 {
            bail!("native type tree node/string bounds are invalid");
        }
        type_trees.insert(class_id as i32, crate::sha256(blob));
        req(
            r.u32(),
            |dependencies| *dependencies == 0,
            "native serialized-reference dependencies are unsupported",
        )?;
    }
    let count_offset = 48 + r.pos;
    let object_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 100000,
        "native object table count is invalid",
    )?;
    r.align();
    let table_start = 48 + r.pos;
    let mut entries = Vec::with_capacity(object_count as usize);
    let mut ids = HashSet::with_capacity(object_count as usize);
    for _ in 0..object_count {
        let row = r.take(24)?;
        let (id, offset, size) = (
            le64(&row[..8]) as i64,
            le64(&row[8..16]),
            le32(&row[16..20]) as u64,
        );
        let type_index = le32(&row[20..24]);
        if type_index >= type_count
            || id == 0
            || ids.contains(&id)
            || offset > payload as u64
            || size > payload as u64 - offset
            || size < 4
        {
            bail!("native object identity or data bounds are invalid");
        }
        ids.insert(id);
        entries.push(ObjectEntry {
            id,
            class_id: classes[type_index as usize],
            offset,
            size,
            row,
        });
    }
    let table_end = 48 + r.pos;
    req(
        r.u32(),
        |scripts| *scripts == 0,
        "script type references are unsupported in native FBX/WAV profiles",
    )?;
    let externals = req(
        r.u32(),
        |externals| *externals <= 1024,
        "native external reference table is invalid",
    )?;
    for _ in 0..externals {
        r.cstring_bytes(256)?;
        let guid = r.take(16)?;
        let kind = req(
            r.u32(),
            |kind| *kind <= 3,
            "native external reference type is invalid",
        )?;
        let path = r.cstring(4096)?;
        result.external_references.push(ExternalReference {
            guid: ordinary_guid(guid),
            kind: kind as i32,
            path,
        });
    }
    req(
        r.u32(),
        |types| *types == 0,
        "native serialized reference types are unsupported",
    )?;
    r.cstring_bytes(4096)?;
    if r.data[r.pos..].iter().any(|&byte| byte != 0) {
        bail!("unrecognized native metadata suffix");
    }
    result.profile = profile.to_string();
    (
        result.serialized_version,
        result.source_target,
        result.runtime_target,
    ) = (23, -2, 24);
    let mut retained = Vec::new();
    for entry in &entries {
        let start = data_offset + entry.offset as usize;
        let object = &input[start..start + entry.size as usize];
        // 1041 is the model importer, 1108 the Editor's preview of a clip and
        // 41386430 the import log.
        let reason = if entry.class_id == 1001480554
            || profile == "fbx" && matches!(entry.class_id, 1041 | 1108 | 41386430)
            || profile == "wav" && entry.class_id == 1020
        {
            "editor-only"
        } else if le32(&object[..4]) & 16 != 0 {
            "dont-save-in-build"
        } else {
            ""
        };
        let local_id = entry.id.to_string();
        if !reason.is_empty() {
            result.removed.push(RemovedObject {
                file_id: local_id,
                class_id: entry.class_id,
                reason: reason.to_string(),
            });
            continue;
        }
        let mut type_name = fbx_class(entry.class_id);
        if profile == "wav" {
            type_name = "";
            if entry.class_id == 83 {
                type_name = "UnityEngine.AudioClip";
                if type_trees.get(&83).map(String::as_str)
                    != Some("d4440ab8f27cef5c3b79c9fc5a65fbe519405155874da1e123d252cd94dd1fe2")
                {
                    bail!(
                        "native AudioClip type tree differs from the pinned verified runtime layout"
                    );
                }
                let (resource, audio) = read_native_audio_resource(object)?;
                result.resources.push(resource);
                result.audio = Some(audio);
            }
        }
        if type_name.is_empty() {
            bail!(
                "native class {} is unsupported in {} runtime profile",
                entry.class_id,
                profile
            );
        }
        result.objects.push(Object {
            file_id: local_id,
            class_id: entry.class_id,
            type_name: type_name.to_string(),
            sha256: crate::sha256(object),
        });
        retained.extend_from_slice(entry.row);
    }
    if profile == "fbx" {
        match read_model_graph(
            input,
            data_offset,
            &entries,
            &result.objects,
            &type_trees,
            &result.external_references,
        ) {
            Ok(graph) => {
                result.model_topology_sha256 = hash_model_graph(&graph);
                result.model_graph = graph;
            }
            Err(reason) => result.model_graph_unsupported_reason = reason,
        }
    }
    if result.objects.is_empty() || profile == "wav" && result.objects.len() != 1 {
        bail!("native runtime profile has no usable objects or unexpected audio objects");
    }
    let output = project_table(
        input,
        &retained,
        result.removed.len(),
        result.objects.len(),
        Layout {
            metadata_end,
            table_start,
            table_end,
            platform_offset,
            count_offset,
        },
    );
    result.input_sha256 = crate::sha256(input);
    result.output_sha256 = crate::sha256(&output);
    Ok((output, result))
}

pub(crate) struct Layout {
    pub metadata_end: usize,
    pub table_start: usize,
    pub table_end: usize,
    pub platform_offset: usize,
    pub count_offset: usize,
}

/// Rewrites only the object table, its count, the target and the metadata size.
pub(crate) fn project_table(
    input: &[u8],
    retained: &[u8],
    removed: usize,
    objects: usize,
    layout: Layout,
) -> Vec<u8> {
    let mut output = input.to_vec();
    let removed_bytes = 24 * removed;
    let new_metadata_end = layout.metadata_end - removed_bytes;
    let new_tail_start = layout.table_start + retained.len();
    output[layout.table_start..new_tail_start].copy_from_slice(retained);
    output[new_tail_start..new_metadata_end]
        .copy_from_slice(&input[layout.table_end..layout.metadata_end]);
    output[new_metadata_end..layout.metadata_end].fill(0);
    output[layout.platform_offset..layout.platform_offset + 4]
        .copy_from_slice(&24u32.to_le_bytes());
    output[layout.count_offset..layout.count_offset + 4]
        .copy_from_slice(&(objects as u32).to_le_bytes());
    let metadata_size = (layout.metadata_end - 48) as u32 - removed_bytes as u32;
    output[20..24].copy_from_slice(&metadata_size.to_be_bytes());
    output
}

/// Reads the stream resource and format of one native AudioClip payload laid
/// out as the pinned type tree describes.
pub fn read_native_audio_resource(object: &[u8]) -> Result<(Resource, AudioMetadata)> {
    let mut r = Reader::at(object, 40); // Object flags + three Editor-extension PPtrs.
    r.string_bytes()?;
    let fields = req(
        r.take(24),
        |fields| le32(&fields[0..4]) <= 1 && le32(&fields[12..16]) == 16,
        "native WAV runtime profile requires non-streaming PCM16 or preloaded Vorbis",
    )?;
    let load_type = le32(&fields[0..4]);
    let channels = le32(&fields[4..8]);
    let frequency = le32(&fields[8..12]);
    let length = f32::from_bits(le32(&fields[16..20]));
    let finite_positive = length > 0.0 && !length.is_infinite();
    if channels == 0
        || channels > 8
        || !(8000..=384000).contains(&frequency)
        || !finite_positive
        || fields[20] != 0
        || fields[21] != 0
    {
        bail!("native WAV channel/rate/duration or tracker/ambisonic layout is unsupported");
    }
    // Subsound index + preload/background/legacy3D flags + alignment.
    req(
        r.take(8),
        |options| le32(&options[..4]) == 0 && options[4] == 1 && options[5] == 0,
        "native WAV runtime profile requires preloaded, synchronous single-subsound audio",
    )?;
    let path = req(
        r.string(),
        |path| path.starts_with("VirtualArtifacts/Primary/") && path.ends_with(".resource"),
        "native WAV resource path is outside the primary artifact namespace",
    )?;
    let guid = path
        .strip_prefix("VirtualArtifacts/Primary/")
        .and_then(|rest| rest.strip_suffix(".resource"))
        .unwrap_or_default();
    if !lower_hex(guid, 32) {
        bail!("native WAV resource GUID is invalid");
    }
    let stream = r.take(16)?;
    let (offset, size) = (le64(&stream[..8]), le64(&stream[8..]));
    req(
        r.u32(),
        |compression| {
            (load_type == 0 && *compression == 0 || load_type == 1 && *compression == 1)
                && offset == 0
                && size != 0
                && size <= MAXIMUM_BYTES as u64
        },
        "native WAV profile requires verified preloaded PCM/Vorbis combination and a bounded zero resource offset",
    )
    .map(|compression| {
        (
            Resource {
                virtual_path: path.clone(),
                offset,
                size,
            },
            AudioMetadata {
                load_type,
                compression_format: compression,
                channels,
                frequency,
                length,
            },
        )
    })
}
