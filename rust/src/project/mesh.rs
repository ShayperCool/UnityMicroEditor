//! Source-authored Unity Mesh bytes accepted by the raw loader.
use super::{
    expand::visit_map,
    value::{self, Map, array, get, map_file_id, number, object},
};
use anyhow::{Result, anyhow, bail};

/// Decoded byte count of a Unity hex scalar; errors name the offending byte or the odd length.
pub(crate) fn hex_len(value: &str) -> Result<usize> {
    let bytes = value.as_bytes();
    let invalid = |b: u8| {
        // Printable Latin-1 characters are shown quoted after the code point.
        if matches!(b, 0x20..=0x7e | 0xa1..=0xac | 0xae..=0xff) {
            anyhow!("hex: invalid byte: U+{:04X} '{}'", b, b as char)
        } else {
            anyhow!("hex: invalid byte: U+{:04X}", b)
        }
    };
    let pairs = bytes.len() / 2 * 2;
    if let Some(bad) = bytes[..pairs].iter().find(|b| !b.is_ascii_hexdigit()) {
        return Err(invalid(*bad));
    }
    if bytes.len() % 2 == 1 {
        let last = bytes[bytes.len() - 1];
        if !last.is_ascii_hexdigit() {
            return Err(invalid(last));
        }
        bail!("hex: odd length hex string");
    }
    Ok(bytes.len() / 2)
}

pub(crate) fn mesh_hex(value: &str) -> Result<usize> {
    // Unity's binary YAML scalar marker keeps digit-only buffers from being
    // read as numbers. It is a suffix, not part of the byte data.
    let value = value.strip_suffix('y').unwrap_or(value);
    let value = value.strip_suffix('u').unwrap_or(value);
    if value.len() > 128 << 20 {
        bail!("Mesh hex bytes exceed bounds");
    }
    hex_len(value).map_err(|e| anyhow!("invalid source Mesh hex bytes: {e}"))
}

/// The serialization of a Mesh the session and the Player read from its
/// text. 12 added the levels of detail (`m_MeshLodInfo`); the engine
/// upgrades an older Mesh when it loads it.
pub const MESH_VERSION: i64 = 12;

/// FBX remains imported by the native worker. The Player verifies the actual
/// public Mesh buffer/submesh/bounds readback before publishing the asset.
pub(crate) fn validate_authored_mesh(data: &Map) -> Result<()> {
    let n = |map: &Map, key: &str| number(get(map, key));
    if n(data, "serializedVersion") != 12 || n(data, "m_MeshCompression") != 0 {
        bail!("only uncompressed serializedVersion12 Mesh is supported");
    }
    let bindposes = array(get(data, "m_BindPose")).len();
    if bindposes > 1024
        || !value::str(get(data, "m_VariableBoneCountWeights").field("m_Data")).is_empty()
    {
        bail!("variable-count or oversized bone data is unsupported");
    }
    let bone_bounds = array(get(data, "m_BonesAABB")).len();
    if bindposes == 0
        && (!value::text(data, "m_BoneNameHashes").is_empty()
            || n(data, "m_RootBoneNameHash") != 0
            || bone_bounds != 0)
    {
        bail!("bone metadata without bindposes is unsupported");
    }
    if bindposes > 0
        && (!mesh_hex(&value::text(data, "m_BoneNameHashes")).is_ok_and(|len| len == 4 * bindposes)
            || bone_bounds != bindposes)
    {
        bail!("source bone metadata does not match bindpose count");
    }
    for shape in object(get(data, "m_Shapes")).values() {
        if !array(shape).is_empty() {
            bail!("blend shapes are unsupported");
        }
    }
    for (name, packed) in object(get(data, "m_CompressedMesh")) {
        if name == "m_UVInfo" {
            if number(packed) != 0 {
                bail!("compressed Mesh UV metadata is unsupported");
            }
            continue;
        }
        let packed = object(packed);
        if n(packed, "m_NumItems") != 0 || !value::text(packed, "m_Data").is_empty() {
            bail!("packed Mesh data is unsupported");
        }
    }
    let stream = object(get(data, "m_StreamData"));
    if n(stream, "offset") != 0 || n(stream, "size") != 0 || !value::text(stream, "path").is_empty()
    {
        bail!("streamed Mesh data is unsupported");
    }
    let vertex = object(get(data, "m_VertexData"));
    let vertices = n(vertex, "m_VertexCount");
    if n(vertex, "serializedVersion") != 3 || vertices <= 0 || vertices > 4 * 1024 * 1024 {
        bail!("invalid source Mesh vertex metadata");
    }
    let vertex_bytes = mesh_hex(&value::text(vertex, "_typelessdata"))?;
    if vertex_bytes as i64 != n(vertex, "m_DataSize") || vertex_bytes > 64 << 20 {
        bail!("source Mesh vertex bytes do not match m_DataSize");
    }
    let index_bytes = mesh_hex(&value::text(data, "m_IndexBuffer"))?;
    let index_format = n(data, "m_IndexFormat");
    if !(0..=1).contains(&index_format)
        || index_bytes == 0
        || index_bytes > 64 << 20
        || index_bytes % (2 + 2 * index_format as usize) != 0
    {
        bail!("invalid source Mesh index buffer");
    }
    let channels = array(get(vertex, "m_Channels"));
    if channels.len() != 14 {
        bail!("source Mesh requires the pinned 14 attribute channels");
    }
    for (index, channel) in channels.iter().enumerate() {
        let channel = object(channel);
        let dimension = n(channel, "dimension");
        if !(0..=4).contains(&dimension) {
            bail!("packed channel dimensions are unsupported");
        }
        if dimension == 0 {
            continue;
        }
        let format = n(channel, "format");
        if !(0..=3).contains(&n(channel, "stream"))
            || n(channel, "offset") < 0
            || !matches!(format, 0 | 2 | 10)
        {
            bail!(
                "only float32/UNorm8/UInt32 Mesh attributes in up to four streams are currently supported"
            );
        }
        if index >= 12
            && (bindposes == 0
                || dimension != 4
                || index == 12 && format != 0
                || index == 13 && format != 10)
        {
            bail!(
                "only fixed four-weight skinning with float32 weights and UInt32 bone indices is supported"
            );
        }
    }
    if n(object(&channels[0]), "dimension") != 3 {
        bail!("Mesh needs float32 position3");
    }
    if bindposes != 0
        && (n(object(&channels[12]), "dimension") != 4
            || n(object(&channels[13]), "dimension") != 4)
    {
        bail!("skinned Mesh lacks fixed four-weight channels");
    }
    if array(get(data, "m_SubMeshes")).is_empty() {
        bail!("Mesh has no submeshes");
    }
    let mut references = Ok(());
    visit_map(data, "", &mut |name, reference| {
        if map_file_id(reference) != "0" {
            references = Err(anyhow!(
                "native Mesh reference {name} requires a supported native importer"
            ));
        }
    });
    references
}
