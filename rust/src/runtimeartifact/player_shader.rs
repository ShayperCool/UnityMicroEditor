use super::{
    MAXIMUM_BYTES, Reader, Tree, be32, be64, le16, le32, le64, lower_hex, ordinary_guid, req, text,
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Identifies the source asset embedded by Unity in a compiled Player Shader.
/// The archive-local path ID is not a source file ID.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerShaderMetadata {
    pub guid: String,
    pub source_file_id: String,
    pub archive_file_id: String,
    pub name: String,
    #[serde(default)]
    pub platforms: Vec<u32>,
    pub compiled_bytes: usize,
    pub type_tree_sha256: String,
    pub payload_sha256: String,
    pub input_sha256: String,
    pub output_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub builtin_guid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub builtin_file_id: String,
}

pub(crate) struct ShaderType<'a> {
    pub class: i32,
    pub bytes: &'a [u8],
    pub tree: &'a [u8],
}

/// Keeps one exact compiled Shader payload and its verified inline tree from a
/// Linux SerializedFile. Every object reference must be null before the
/// unrelated external and object tables can be omitted. Scene objects and
/// their data never enter the output file. GPU programs are not compiled or
/// translated here.
pub fn extract_player_shader(
    input: &[u8],
    guid: &str,
    name: &str,
) -> Result<(Vec<u8>, PlayerShaderMetadata)> {
    extract(input, guid, name, 4800000)
}

/// Unity's Linux built-in-assets snapshot contains the exact Panoramic shader
/// under its build-pipeline source GUID. Alias only that verified shader to
/// the authored unity_builtin_extra identity; retain every compiled payload byte.
pub fn extract_builtin_player_shader(
    input: &[u8],
    id: i64,
) -> Result<(Vec<u8>, PlayerShaderMetadata)> {
    if id != 108 {
        bail!("unsupported compiled built-in Shader identity");
    }
    let (output, mut proof) = extract(
        input,
        "f57c2bc4f17ba48d9b9b6758dcfe812d",
        "Skybox/Panoramic",
        id,
    )?;
    proof.builtin_guid = "0000000000000000f000000000000000".into();
    proof.builtin_file_id = "108".into();
    Ok((output, proof))
}

fn extract(
    input: &[u8],
    guid: &str,
    name: &str,
    output_id: i64,
) -> Result<(Vec<u8>, PlayerShaderMetadata)> {
    if !lower_hex(guid, 32) || name.is_empty() {
        bail!("compiled Shader requires exact lowercase source GUID and Shader name");
    }
    if input.len() < 64
        || input.len() > MAXIMUM_BYTES
        || be32(&input[8..12]) != 23
        || input[16] != 0
    {
        bail!("compiled Shader requires little-endian SerializedFile v23");
    }
    let (file_size, data_offset) = (be64(&input[24..32]), be64(&input[32..40]));
    let metadata_end = 48 + be32(&input[20..24]) as u64;
    if file_size != input.len() as u64 || metadata_end > data_offset || data_offset > file_size {
        bail!("compiled Shader SerializedFile header bounds are invalid");
    }
    let payload_bytes = file_size - data_offset;
    let mut r = Reader::new(&input[48..metadata_end as usize]);
    let version = req(
        r.cstring_bytes(64),
        |version| *version == b"6000.6.3f1",
        "compiled Shader uses an unverified Unity version",
    )?;
    req(
        r.u32(),
        |platform| *platform == 24,
        "compiled Shader must come from Linux Player target24",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "compiled Shader requires inline type trees",
    )?;
    let type_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 1024,
        "compiled Shader type count is invalid",
    )?;
    let mut types = Vec::with_capacity(type_count as usize);
    for _ in 0..type_count {
        let start = r.pos;
        let class = r.u32()?;
        let flags = r.take(3)?;
        let script = le16(&flags[1..]) as i16;
        if script >= 0 || class as i32 == 114 || (class as i32) < 0 {
            r.take(16)?;
        }
        r.take(32)?;
        let size = req(
            r.u32(),
            |size| *size >= 16 && *size as usize <= MAXIMUM_BYTES,
            "compiled Shader inline tree size is invalid",
        )?;
        let tree = r.take(size as usize)?;
        if le32(&tree[..4]) != 0x7474686d || le32(&tree[4..8]) != 23 {
            bail!("compiled Shader inline tree format is invalid");
        }
        let (count, chars) = (le32(&tree[8..12]) as u64, le32(&tree[12..16]) as u64);
        if count == 0 || 16 + count * 32 + chars != tree.len() as u64 {
            bail!("compiled Shader inline tree bounds are invalid");
        }
        let deps = req(
            r.u32(),
            |deps| *deps <= 1024,
            "compiled Shader type dependency count is invalid",
        )?;
        r.take(deps as usize * 4)?;
        if class == 48 && (flags[0] != 0 || script != -1 || deps != 0) {
            bail!("compiled Shader type has unsupported stripped/script dependencies");
        }
        types.push(ShaderType {
            class: class as i32,
            bytes: &r.data[start..r.pos],
            tree,
        });
    }
    let objects = req(
        r.u32(),
        |objects| *objects != 0 && *objects <= 100000,
        "compiled Shader object count is invalid",
    )?;
    r.align();
    let mut chosen: Option<(PlayerShaderMetadata, &ShaderType, &[u8], i64)> = None;
    let mut seen = HashSet::new();
    for _ in 0..objects {
        let row = r.take(24)?;
        let id = le64(&row[..8]) as i64;
        let (offset, size) = (le64(&row[8..16]), le32(&row[16..20]) as u64);
        let type_index = le32(&row[20..24]);
        if id == 0
            || seen.contains(&id)
            || type_index >= type_count
            || offset > payload_bytes
            || size > payload_bytes - offset
        {
            bail!("compiled Shader object identity/bounds are invalid");
        }
        seen.insert(id);
        let kind = &types[type_index as usize];
        if kind.class != 48 {
            continue;
        }
        let start = (data_offset + offset) as usize;
        let payload = &input[start..start + size as usize];
        let candidate = read_shader_identity(payload, kind.tree, true)
            .map_err(|error| anyhow!("compiled Shader {id}: {error}"))?;
        if candidate.proof.guid != guid {
            continue;
        }
        if candidate.name != name.as_bytes() || candidate.proof.source_file_id != "4800000" {
            bail!("compiled Shader embedded source GUID disagrees with name/local identity");
        }
        if chosen.is_some() {
            bail!("compiled Shader source GUID is duplicated in archive");
        }
        chosen = Some((candidate.proof, kind, payload, id));
    }
    let Some((mut proof, chosen_type, chosen_payload, chosen_id)) = chosen else {
        bail!("compiled Shader source GUID is absent from archive");
    };
    // The original tail is irrelevant to this proven reference-free Shader.
    // Preserve only its type record and payload; construct canonical empty tables.
    let mut metadata = Vec::new();
    metadata.extend_from_slice(version);
    metadata.push(0);
    metadata.extend_from_slice(&24u32.to_le_bytes());
    metadata.push(1);
    metadata.extend_from_slice(&1u32.to_le_bytes());
    metadata.extend_from_slice(chosen_type.bytes);
    metadata.extend_from_slice(&1u32.to_le_bytes());
    while metadata.len() % 4 != 0 {
        metadata.push(0);
    }
    let mut row = [0u8; 24];
    row[..8].copy_from_slice(&(output_id as u64).to_le_bytes());
    row[16..20].copy_from_slice(&(chosen_payload.len() as u32).to_le_bytes());
    metadata.extend_from_slice(&row);
    metadata.extend_from_slice(&[0u8; 4]); // Script references.
    metadata.extend_from_slice(&[0u8; 4]); // External references, all Shader PPtrs proven null.
    metadata.extend_from_slice(&[0u8; 4]); // Serialized reference types.
    metadata.push(0);
    let output_data = (48 + metadata.len() + 15) & !15;
    let mut output = vec![0u8; output_data + chosen_payload.len()];
    output[8..12].copy_from_slice(&23u32.to_be_bytes());
    output[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
    let total = output.len() as u64;
    output[24..32].copy_from_slice(&total.to_be_bytes());
    output[32..40].copy_from_slice(&(output_data as u64).to_be_bytes());
    output[48..48 + metadata.len()].copy_from_slice(&metadata);
    output[output_data..].copy_from_slice(chosen_payload);
    proof.archive_file_id = chosen_id.to_string();
    proof.type_tree_sha256 = crate::sha256(chosen_type.tree);
    proof.payload_sha256 = crate::sha256(chosen_payload);
    proof.input_sha256 = crate::sha256(input);
    proof.output_sha256 = crate::sha256(&output);
    Ok((output, proof))
}

/// Traverses every inline-tree field, including arrays, so retaining opaque GPU
/// blobs cannot conceal a live serialized Object reference.
pub fn read_player_shader(data: &[u8], tree: &[u8]) -> Result<PlayerShaderMetadata> {
    read_shader_identity(data, tree, true).map(|identity| identity.proof)
}

pub(crate) struct ShaderIdentity {
    pub proof: PlayerShaderMetadata,
    /// Undecoded name bytes, for exact comparison with a requested name.
    pub name: Vec<u8>,
}

enum Stop {
    SourceNameFound,
    Failed(anyhow::Error),
}
impl From<anyhow::Error> for Stop {
    fn from(error: anyhow::Error) -> Self {
        Stop::Failed(error)
    }
}
macro_rules! stop {
    ($($arg:tt)*) => {
        return Err(Stop::Failed(anyhow!($($arg)*)))
    };
}

struct Walk<'a> {
    tree: Tree<'a>,
    r: Reader<'a>,
    compiled: bool,
    budget: i64,
    proof: PlayerShaderMetadata,
    name: Vec<u8>,
}

impl<'a> Walk<'a> {
    fn label(&self, index: usize, field: usize) -> Result<&'a [u8]> {
        let offset = self.tree.offset(index, field);
        if offset & 0x80000000 != 0 {
            return common_string(offset & 0x7fffffff)
                .map(str::as_bytes)
                .ok_or_else(|| {
                    anyhow!(
                        "compiled Shader uses an unverified common string offset {}",
                        offset & 0x7fffffff
                    )
                });
        }
        let chars = self.tree.chars;
        if offset as usize >= chars.len() {
            bail!(
                "compiled Shader tree string offset is invalid: node {} field {} offset {:x} string bytes {}",
                index,
                field,
                offset,
                chars.len()
            );
        }
        self.tree
            .local(offset)
            .ok_or_else(|| anyhow!("compiled Shader tree string is unterminated"))
    }
    fn walk(&mut self, i: usize, parent: &str) -> Result<(), Stop> {
        self.budget -= 1;
        if self.budget < 0 {
            stop!("compiled Shader tree exceeds traversal budget");
        }
        let next = self.tree.end(i);
        let type_name = self.label(i, 4)?;
        let name = self.label(i, 8)?;
        let path = if parent.is_empty() {
            text(name)
        } else {
            format!("{parent}.{}", text(name))
        };
        let start = self.r.pos;
        let size = self.tree.byte_size(i);
        if type_name.starts_with(b"PPtr<") {
            if size != 12 {
                stop!("compiled Shader PPtr layout is unsupported");
            }
            let value = self.r.take(12)?;
            if self.compiled && value.iter().any(|&byte| byte != 0) {
                stop!("compiled Shader has a live dependency at {path}");
            }
        } else if type_name == b"string" {
            let value = self.r.string_bytes()?;
            if path == "Base.m_ParsedForm.m_Name" || path == "Shader.m_ParsedForm.m_Name" {
                self.proof.name = text(value);
                self.name = value.to_vec();
                if !self.compiled {
                    return Err(Stop::SourceNameFound);
                }
            }
        } else if self.tree.is_array(i) {
            let length = req(
                self.r.u32(),
                |length| *length as usize <= MAXIMUM_BYTES,
                "compiled Shader array size is invalid",
            )?;
            if !self.tree.array_shape(i, next) {
                stop!("compiled Shader array tree is malformed");
            }
            let element = i + 2;
            let element_type = self.label(element, 4)?;
            let byte_size = self.tree.byte_size(element);
            if byte_size == 1
                && self.tree.end(element) == element + 1
                && !self.tree.aligned(element)
            {
                self.r.take(length as usize)?;
                if name == b"Array"
                    && (parent == "Base.compressedBlob" || parent == "Shader.compressedBlob")
                {
                    self.proof.compiled_bytes = length as usize;
                }
            } else {
                if length > 200000 {
                    stop!("compiled Shader structured array is oversized");
                }
                let platforms = (parent == "Base.platforms" || parent == "Shader.platforms")
                    && byte_size == 4
                    && (element_type == b"unsigned int" || element_type == b"UInt32");
                for _ in 0..length {
                    let position = self.r.pos;
                    self.walk(element, &path)?;
                    if platforms {
                        let value = self.r.data.get(position..position + 4).ok_or_else(|| {
                            anyhow!("compiled Shader platform element is shorter than four bytes")
                        })?;
                        self.proof.platforms.push(le32(value));
                    }
                }
            }
        } else if name == b"m_AssetGUID" && type_name == b"GUID" {
            self.proof.guid = ordinary_guid(self.r.take(16)?);
        } else if name == b"m_AssetLocalIdentifierInFile" && type_name == b"SInt64" {
            self.proof.source_file_id = (le64(self.r.take(8)?) as i64).to_string();
        } else if i + 1 < next {
            let mut child = i + 1;
            while child < next {
                self.walk(child, &path)?;
                child = self.tree.end(child);
            }
        } else if size >= 0 {
            self.r.take(size as usize)?;
            if name == b"m_ShaderIsBaked" && (size != 1 || self.r.data[start] != 1) {
                stop!("Shader object is not Player-baked");
            }
        } else {
            stop!("compiled Shader has unsupported dynamic leaf {path}");
        }
        if self.tree.aligned(i) {
            self.r.align();
            if self.r.pos > self.r.data.len() {
                stop!("compiled Shader alignment exceeds payload");
            }
        }
        Ok(())
    }
}

/// Callers other than the tests pass trees whose bounds were already checked.
pub(crate) fn read_shader_identity(
    data: &[u8],
    tree: &[u8],
    compiled: bool,
) -> Result<ShaderIdentity> {
    if tree.len() < 16 {
        bail!("compiled Shader inline tree is shorter than its header");
    }
    let count = le32(&tree[8..12]) as usize;
    if count == 0 {
        bail!("compiled Shader inline tree has no nodes");
    }
    if count > 10000 {
        bail!("compiled Shader tree is oversized");
    }
    if 16 + count * 32 > tree.len() {
        bail!("compiled Shader inline tree node table is out of range");
    }
    let mut walk = Walk {
        tree: Tree {
            nodes: &tree[16..16 + count * 32],
            chars: &tree[16 + count * 32..],
            count,
        },
        r: Reader::new(data),
        compiled,
        budget: 2000000,
        proof: PlayerShaderMetadata::default(),
        name: Vec::new(),
    };
    match walk.walk(0, "") {
        Ok(()) => {}
        Err(Stop::SourceNameFound) if !compiled => {
            return Ok(ShaderIdentity {
                proof: walk.proof,
                name: walk.name,
            });
        }
        Err(Stop::SourceNameFound) => bail!("native Shader source name found"),
        Err(Stop::Failed(error)) => return Err(error),
    }
    if walk.r.pos != data.len() {
        bail!("compiled Shader payload does not match its entire inline tree");
    }
    let proof = &walk.proof;
    if proof.guid.is_empty()
        || proof.source_file_id.is_empty()
        || proof.name.is_empty()
        || proof.compiled_bytes == 0
        || proof.platforms.len() != 1
        || proof.platforms[0] != 15
    {
        bail!("compiled Shader requires embedded source identity and GLCore GPU program data");
    }
    Ok(ShaderIdentity {
        proof: walk.proof,
        name: walk.name,
    })
}

/// Unity's common type-tree string offsets are part of the serialized format.
/// This narrow subset is independently readable with the installation's
/// binary2text tool; unknown names are rejected rather than guessed.
pub(crate) fn common_string(offset: u32) -> Option<&'static str> {
    Some(match offset {
        1093 => "m_CorrespondingSourceObject",
        1121 => "m_PrefabInstance",
        1138 => "m_PrefabAsset",
        646 => "PPtr<Prefab>",
        519 => "m_Type",
        526 => "m_Version",
        288 => "m_ByteSize",
        349 => "m_Enabled",
        387 => "m_Index",
        395 => "m_IsArray",
        416 => "m_MetaFlag",
        49 => "Array",
        55 => "Base",
        76 => "bool",
        81 => "char",
        86 => "ColorRGBA",
        106 => "data",
        117 => "double",
        155 => "first",
        161 => "float",
        208 => "GUID",
        222 => "int",
        231 => "long long",
        241 => "map",
        245 => "Matrix4x4f",
        427 => "m_Name",
        434 => "m_ObjectHideFlags",
        536 => "Object",
        543 => "pair",
        633 => "PPtr<Object>",
        688 => "PPtr<Texture>",
        702 => "PPtr<Texture2D>",
        778 => "second",
        789 => "short",
        795 => "size",
        800 => "SInt16",
        807 => "SInt32",
        814 => "SInt64",
        821 => "SInt8",
        840 => "string",
        894 => "TypelessData",
        907 => "UInt16",
        914 => "UInt32",
        921 => "UInt64",
        928 => "UInt8",
        934 => "unsigned int",
        947 => "unsigned long long",
        966 => "unsigned short",
        981 => "vector",
        988 => "Vector2f",
        997 => "Vector3f",
        1006 => "Vector4f",
        1161 => "Hash128",
        _ => return None,
    })
}
