use super::{
    MAXIMUM_BYTES, Reader, Tree, adapt::fbx_class, be32, be64, le16, le32, le64,
    player_shader::common_string, req,
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeshCpuFlags {
    pub file_id: String,
    pub is_readable_offset: usize,
    pub keep_vertices_offset: usize,
    pub keep_indices_offset: usize,
    pub original_is_readable: bool,
    pub original_keep_vertices: bool,
    pub original_keep_indices: bool,
    pub payload_sha256: String,
    pub type_tree_sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeshCpuProjection {
    pub schema_version: i64,
    pub mode: String,
    pub input_sha256: String,
    pub output_sha256: String,
    pub meshes: Vec<MeshCpuFlags>,
}

/// Prepares an already projected native model for public Physics.BakeMesh.
/// Only its three CPU-retention booleans may change; all vertices, indices,
/// submeshes, physics blobs, bounds and other objects retain their imported
/// bytes. The loader restores authored unreadability after bake.
pub fn retain_model_mesh_cpu(input: &[u8]) -> Result<(Vec<u8>, MeshCpuProjection)> {
    let mut proof = MeshCpuProjection {
        schema_version: 1,
        mode: "native-model-mesh-cpu-v1".into(),
        input_sha256: crate::sha256(input),
        ..Default::default()
    };
    if input.len() < 64
        || input.len() > MAXIMUM_BYTES
        || be32(&input[8..12]) != 23
        || input[16] != 0
        || be64(&input[24..32]) != input.len() as u64
    {
        bail!("Mesh CPU projection requires bounded little-endian SerializedFile v23");
    }
    let data_offset = be64(&input[32..40]);
    let metadata_end = 48 + be32(&input[20..24]) as u64;
    if metadata_end > data_offset || data_offset > input.len() as u64 {
        bail!("Mesh CPU projection header bounds are invalid");
    }
    let payload = input.len() as u64 - data_offset;
    let mut r = Reader::new(&input[48..metadata_end as usize]);
    req(
        r.cstring_bytes(64),
        |version| *version == b"0.0.0" || *version == b"6000.6.3f1",
        "Mesh CPU projection requires pinned Unity 6000.6.3f1",
    )?;
    req(
        r.u32(),
        |target| *target == 24,
        "Mesh CPU projection requires an already projected Linux Player file",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "Mesh CPU projection requires inline native type trees",
    )?;
    let type_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 256,
        "Mesh CPU projection type count is invalid",
    )?;
    let mut classes = Vec::with_capacity(type_count as usize);
    let mut trees = Vec::with_capacity(type_count as usize);
    for _ in 0..type_count {
        let class = r.u32()?;
        classes.push(class as i32);
        req(
            r.take(3),
            |flags| {
                flags[0] == 0 && le16(&flags[1..]) as i16 == -1 && class as i32 >= 0 && class != 114
            },
            "Mesh CPU projection contains an unsupported model type",
        )?;
        r.take(32)?;
        let size = req(
            r.u32(),
            |size| *size >= 16 && *size as usize <= MAXIMUM_BYTES,
            "Mesh CPU projection type tree is missing",
        )?;
        trees.push(r.take(size as usize)?);
        req(
            r.u32(),
            |dependencies| *dependencies == 0,
            "Mesh CPU projection has unsupported serialized dependencies",
        )?;
    }
    let count = req(
        r.u32(),
        |count| *count != 0 && *count <= 100000,
        "Mesh CPU projection object count is invalid",
    )?;
    r.align();
    let mut output = input.to_vec();
    let mut seen = HashSet::new();
    for _ in 0..count {
        let row = r.take(24)?;
        let id = le64(&row[..8]) as i64;
        let (offset, size, type_index) = (
            le64(&row[8..16]),
            le32(&row[16..20]) as u64,
            le32(&row[20..24]),
        );
        if id == 0
            || seen.contains(&id)
            || type_index >= type_count
            || offset > payload
            || size > payload - offset
        {
            bail!("Mesh CPU projection object identity or bounds are invalid");
        }
        seen.insert(id);
        let type_index = type_index as usize;
        if fbx_class(classes[type_index]).is_empty() {
            bail!("Mesh CPU projection retained an unsupported native model object");
        }
        if classes[type_index] != 43 {
            continue;
        }
        let start = (data_offset + offset) as usize;
        let data = &input[start..start + size as usize];
        let mut flags = read_mesh_cpu_flags(data, trees[type_index])
            .map_err(|error| anyhow!("Mesh {id} CPU retention layout: {error}"))?;
        if flags.original_is_readable {
            continue;
        }
        flags.file_id = id.to_string();
        flags.payload_sha256 = crate::sha256(data);
        flags.type_tree_sha256 = crate::sha256(trees[type_index]);
        flags.is_readable_offset += start;
        flags.keep_vertices_offset += start;
        flags.keep_indices_offset += start;
        output[flags.is_readable_offset] = 1;
        output[flags.keep_vertices_offset] = 1;
        output[flags.keep_indices_offset] = 1;
        proof.meshes.push(flags);
    }
    let mut masked = output.clone();
    for flags in &proof.meshes {
        masked[flags.is_readable_offset] = flags.original_is_readable as u8;
        masked[flags.keep_vertices_offset] = flags.original_keep_vertices as u8;
        masked[flags.keep_indices_offset] = flags.original_keep_indices as u8;
    }
    if input != masked.as_slice() {
        bail!("Mesh CPU projection changed bytes outside declared boolean flags");
    }
    proof.output_sha256 = crate::sha256(&output);
    Ok((output, proof))
}

struct Walk<'a> {
    tree: Tree<'a>,
    r: Reader<'a>,
    budget: i64,
    found: usize,
    seen: [bool; 3],
    result: MeshCpuFlags,
}

impl<'a> Walk<'a> {
    // Unknown common-string offsets read as "" and so match no expected name.
    fn label(&self, index: usize, field: usize) -> &'a [u8] {
        let offset = self.tree.offset(index, field);
        if offset & 0x80000000 != 0 {
            return common_string(offset & 0x7fffffff)
                .unwrap_or_default()
                .as_bytes();
        }
        self.tree.local(offset).unwrap_or_default()
    }
    fn walk(&mut self, index: usize) -> Result<()> {
        self.budget -= 1;
        if self.budget < 0 {
            bail!("Mesh prefix exceeds type tree traversal budget");
        }
        let next = self.tree.end(index);
        let (name, type_name) = (self.label(index, 8), self.label(index, 4));
        let position = self.r.pos;
        let size = self.tree.byte_size(index);
        let flag = match name {
            b"m_IsReadable" => Some(0),
            b"m_KeepVertices" => Some(1),
            b"m_KeepIndices" => Some(2),
            _ => None,
        };
        if let Some(flag) = flag {
            if self.tree.level(index) != 1
                || type_name != b"bool"
                || size != 1
                || next != index + 1
                || self.seen[flag]
            {
                bail!("Mesh CPU flags must be unique top-level bool fields");
            }
            let value = req(
                self.r.take(1),
                |value| value[0] <= 1,
                "Mesh CPU flag is not a serialized bool",
            )?[0]
                == 1;
            self.seen[flag] = true;
            self.found += 1;
            match flag {
                0 => {
                    (
                        self.result.is_readable_offset,
                        self.result.original_is_readable,
                    ) = (position, value)
                }
                1 => {
                    (
                        self.result.keep_vertices_offset,
                        self.result.original_keep_vertices,
                    ) = (position, value)
                }
                _ => {
                    (
                        self.result.keep_indices_offset,
                        self.result.original_keep_indices,
                    ) = (position, value)
                }
            }
        } else if type_name == b"string" {
            self.r.string_bytes()?;
        } else if self.tree.is_array(index) {
            let length = self.r.u32().ok().filter(|length| {
                *length as usize <= MAXIMUM_BYTES && self.tree.array_shape(index, next)
            });
            let Some(length) = length else {
                bail!("Mesh prefix array is oversized or malformed");
            };
            let element = index + 2;
            let stride = self.tree.byte_size(element);
            if stride >= 0 && !self.tree.aligned(element) {
                let bytes = length as u64 * stride as u64;
                if bytes > MAXIMUM_BYTES as u64 {
                    bail!("Mesh prefix array exceeds byte bounds");
                }
                self.r.take(bytes as usize)?;
            } else {
                for _ in 0..length {
                    self.walk(element)?;
                }
            }
        } else if index + 1 < next {
            let mut child = index + 1;
            while child < next {
                self.walk(child)?;
                if self.found == 3 {
                    return Ok(());
                }
                child = self.tree.end(child);
            }
        } else if size >= 0 {
            self.r.take(size as usize)?;
        } else {
            bail!("Mesh prefix has an unsupported dynamic leaf");
        }
        if self.tree.aligned(index) {
            self.r.align();
            if self.r.pos > self.r.data.len() {
                bail!("Mesh prefix alignment exceeds payload");
            }
        }
        Ok(())
    }
}

/// Locates the three CPU-retention booleans through the inline tree. Offsets
/// are relative to the object payload.
pub fn read_mesh_cpu_flags(data: &[u8], tree: &[u8]) -> Result<MeshCpuFlags> {
    if tree.len() < 16 || le32(&tree[..4]) != 0x7474686d || le32(&tree[4..8]) != 23 {
        bail!("Mesh type tree format is unverified");
    }
    let count = le32(&tree[8..12]) as usize;
    let chars_size = le32(&tree[12..16]) as u64;
    if count == 0 || count > 10000 || (16 + count * 32) as u64 + chars_size != tree.len() as u64 {
        bail!("Mesh type tree bounds are invalid");
    }
    let mut walk = Walk {
        tree: Tree {
            nodes: &tree[16..16 + count * 32],
            chars: &tree[16 + count * 32..],
            count,
        },
        r: Reader::new(data),
        budget: 2000000,
        found: 0,
        seen: [false; 3],
        result: MeshCpuFlags::default(),
    };
    walk.walk(0)?;
    if walk.found != 3 {
        bail!("Mesh inline tree lacks the three CPU-retention flags");
    }
    Ok(walk.result)
}
