//! Tests of the native artifact readers and adapters and of the
//! `shader-extract` and `builtin-shader` commands. Tests that need real Unity
//! output are gated on fixture environment variables.
use serde_json::json;
use std::{env, fs, path::Path};
use unity_micro_editor::{
    playerbake::{self, Val},
    runtimeartifact::{
        self as ra, BuiltinShaderOptions, MeshCpuProjection, Metadata, shader_sources,
    },
    sha256,
};

fn le32(data: &[u8]) -> u32 {
    u32::from_le_bytes(data[..4].try_into().unwrap())
}
fn be64(data: &[u8]) -> u64 {
    u64::from_be_bytes(data[..8].try_into().unwrap())
}
fn put_le32(data: &mut [u8], value: u32) {
    data[..4].copy_from_slice(&value.to_le_bytes());
}

/// Fixture-backed tests are skipped when their variable is unset.
fn fixture(variable: &str) -> Option<String> {
    let value = env::var(variable).ok().filter(|value| !value.is_empty());
    if value.is_none() {
        eprintln!("skipped: set {variable}");
    }
    value
}

// ---- adapt ------------------------------------------------------------------

fn native_object_start(input: &[u8], id: i64) -> usize {
    let data_offset = be64(&input[32..40]) as usize;
    let needle = (id as u64).to_le_bytes();
    let index = input[48..data_offset]
        .windows(8)
        .position(|window| window == needle)
        .expect("fixture object table local ID missing");
    assert!(48 + index + 24 <= data_offset);
    data_offset
        + u64::from_le_bytes(input[48 + index + 8..48 + index + 16].try_into().unwrap()) as usize
}

#[test]
fn adapt_native_player_verified_files() {
    let Some(root) = fixture("UNITY_WORKER_NATIVE_FIXTURES") else {
        return;
    };
    for (profile, name, expected, objects, removed) in [
        (
            "fbx",
            "0-0-dec78b13583b5d38d519af0a8c636e82.bin",
            "18532c338a8adac73985b37c6ef0913ecf8bf6ff22378cb1bea049889b752a06",
            7,
            2,
        ),
        (
            "wav",
            "1-0-cf98ce63e0e5085e20444dc09b0590fc.bin",
            "a929841748d5617347b074475d6d3177d2e216fcffaa8a001dd8f331eb3c6ca4",
            1,
            1,
        ),
    ] {
        let input = fs::read(Path::new(&root).join(name)).unwrap();
        let (output, metadata) = ra::adapt(&input, profile).unwrap();
        assert_eq!(metadata.output_sha256, expected);
        assert_eq!(metadata.objects.len(), objects);
        assert_eq!(metadata.removed.len(), removed);
        let data_offset = be64(&input[32..40]) as usize;
        assert_eq!(input[data_offset..], output[data_offset..]);
        if profile == "fbx" {
            assert_eq!(metadata.external_references.len(), 1);
            assert_eq!(
                metadata.external_references[0].guid,
                "0000000000000000f000000000000000"
            );
            assert_eq!(metadata.model_graph_unsupported_reason, "");
            assert_eq!(metadata.model_graph.len(), 6);
            assert_eq!(
                metadata.model_topology_sha256,
                "c87a23c520dd51731c1a5c7fdb26fd647944937e469bcdcaf6a8644b923d83e9"
            );
        } else {
            assert_eq!(metadata.resources.len(), 1);
            assert_eq!(
                metadata.resources[0].virtual_path,
                "VirtualArtifacts/Primary/71705265666978617564696f70726f62.resource"
            );
            assert_eq!(metadata.resources[0].size, 16512);
        }
    }
}

#[test]
fn model_graph_preserves_authored_order_and_rejects_unsupported_layout() {
    let Some(root) = fixture("UNITY_WORKER_NATIVE_FIXTURES") else {
        return;
    };
    let input =
        fs::read(Path::new(&root).join("0-0-dec78b13583b5d38d519af0a8c636e82.bin")).unwrap();
    let (_, metadata) = ra::adapt(&input, "fbx").unwrap();
    let mut graph = metadata.model_graph.clone();
    let child = graph.last_mut().unwrap();
    assert_eq!(child.file_id, "7771091623220844444");
    assert_eq!(child.references[0].targets.len(), 3);
    child.references[0].targets.swap(0, 1);
    assert_ne!(
        ra::hash_model_graph(&graph),
        ra::hash_model_graph(&metadata.model_graph),
        "changing authored component order did not invalidate topology fingerprint"
    );
    let mut posed = input.clone();
    let entry_offset = native_object_start(&posed, -8679921383154817045);
    put_le32(&mut posed[entry_offset + 68..], 0x3f800000);
    let (_, posed_metadata) = ra::adapt(&posed, "fbx").unwrap();
    assert_ne!(
        posed_metadata.model_topology_sha256,
        metadata.model_topology_sha256
    );
    // Change only a type-tree version stamp while retaining valid tree bounds.
    let mut unverified = input.clone();
    unverified[122..124].copy_from_slice(&1u16.to_le_bytes());
    let (_, changed) = ra::adapt(&unverified, "fbx").unwrap();
    assert_eq!(changed.model_topology_sha256, "");
    assert_ne!(changed.model_graph_unsupported_reason, "");
}

#[test]
fn native_audio_rejects_unknown_load_types() {
    let Some(root) = fixture("UNITY_WORKER_NATIVE_FIXTURES") else {
        return;
    };
    let input =
        fs::read(Path::new(&root).join("1-0-cf98ce63e0e5085e20444dc09b0590fc.bin")).unwrap();
    let start = native_object_start(&input, 8300000);
    // Skip the Object header and the aligned name string.
    let name = le32(&input[start + 40..]) as usize;
    let load_offset = start + ((40 + 4 + name + 3) & !3);
    // Streaming reads the same resource; anything past the known load types
    // is refused.
    let mut streaming = input.clone();
    put_le32(&mut streaming[load_offset..], 2);
    assert!(ra::adapt(&streaming, "wav").is_ok());
    let mut changed = input.clone();
    put_le32(&mut changed[load_offset..], 3);
    assert!(ra::adapt(&changed, "wav").is_err());
}

#[test]
fn adapt_real_project_preloaded_vorbis() {
    let Some(root) = fixture("UNITY_WORKER_PROJECT_AUDIO_FIXTURES") else {
        return;
    };
    let input =
        fs::read(Path::new(&root).join("03979594c803f4b0eb50ac92c7a2c1fe-0000.bin")).unwrap();
    let (output, meta) = ra::adapt(&input, "wav").unwrap();
    let audio = meta.audio.as_ref().unwrap();
    assert_eq!(meta.objects.len(), 1);
    assert_eq!(
        (
            audio.load_type,
            audio.compression_format,
            audio.channels,
            audio.frequency
        ),
        (1, 1, 1, 44100)
    );
    assert_eq!(audio.length, 1.0);
    assert_eq!(meta.resources.len(), 1);
    assert_eq!(meta.resources[0].size, 9184);
    let data_offset = be64(&input[32..40]) as usize;
    assert_eq!(input[data_offset..], output[data_offset..]);
    let resource =
        fs::read(Path::new(&root).join("03979594c803f4b0eb50ac92c7a2c1fe-0001.bin")).unwrap();
    assert_eq!(resource.len(), 9184);
}

#[test]
fn malformed_native_metadata_is_rejected() {
    let Some(root) = fixture("UNITY_WORKER_NATIVE_FIXTURES") else {
        return;
    };
    let input =
        fs::read(Path::new(&root).join("0-0-dec78b13583b5d38d519af0a8c636e82.bin")).unwrap();
    type Edit = fn(&mut [u8]);
    let edits: [(&str, Edit); 6] = [
        ("wrong-target", |data| put_le32(&mut data[54..], 24)),
        ("wrong-version", |data| {
            data[8..12].copy_from_slice(&26u32.to_be_bytes())
        }),
        ("wrong-file-size", |data| {
            data[24..32].copy_from_slice(&1u64.to_be_bytes())
        }),
        ("external-type-tree", |data| data[58] = 0),
        ("unknown-native-class", |data| put_le32(&mut data[63..], 28)),
        ("script-backed-type", |data| {
            data[68..70].copy_from_slice(&0u16.to_le_bytes())
        }),
    ];
    for (name, edit) in edits {
        let mut malformed = input.clone();
        edit(&mut malformed);
        assert!(ra::adapt(&malformed, "fbx").is_err(), "{name}");
    }
}

#[test]
fn reject_unsupported_profile_and_truncation() {
    for profile in ["", "shader", "fbx", "wav"] {
        assert!(ra::adapt(&[0u8; 32], profile).is_err(), "{profile}");
    }
    assert_eq!(
        ra::adapt(&[0u8; 32], "shader").unwrap_err().to_string(),
        "unsupported native runtime profile \"shader\""
    );
}

// ---- UnityFS members --------------------------------------------------------

#[test]
fn verify_unityfs_actual_shader_member() {
    let (Some(member_path), Some(bundle_path)) = (
        fixture("UNITY_PLAY_COMPILED_SHADER_FIXTURE"),
        fixture("UNITY_PLAY_COMPILED_SHADER_BUNDLE"),
    ) else {
        return;
    };
    let mut member = fs::read(member_path).unwrap();
    let archive = fs::read(bundle_path).unwrap();
    let name = "CAB-c7e0997e8a48cbbb3c94d6b2c3d64de1.sharedAssets";
    ra::verify_unityfs_member(&archive, name, &member).unwrap();
    member[0] ^= 1;
    assert!(ra::verify_unityfs_member(&archive, name, &member).is_err());
    assert!(ra::verify_unityfs_member(&archive, "different", &member).is_err());
}

fn raw_unityfs(member: &[u8]) -> Vec<u8> {
    let mut info = vec![0u8; 16];
    info.extend_from_slice(&1u32.to_be_bytes());
    info.extend_from_slice(&(member.len() as u32).to_be_bytes());
    info.extend_from_slice(&(member.len() as u32).to_be_bytes());
    info.extend_from_slice(&0u16.to_be_bytes());
    info.extend_from_slice(&1u32.to_be_bytes());
    info.extend_from_slice(&0u64.to_be_bytes());
    info.extend_from_slice(&(member.len() as u64).to_be_bytes());
    info.extend_from_slice(&4u32.to_be_bytes());
    info.extend_from_slice(b"shader.member\0");
    let mut archive = b"UnityFS\0".to_vec();
    archive.extend_from_slice(&8u32.to_be_bytes());
    archive.extend_from_slice(b"6000.6.3f1\0revision\0");
    let size_position = archive.len();
    archive.extend_from_slice(&0u64.to_be_bytes());
    archive.extend_from_slice(&(info.len() as u32).to_be_bytes());
    archive.extend_from_slice(&(info.len() as u32).to_be_bytes());
    archive.extend_from_slice(&0x40u32.to_be_bytes());
    while !archive.len().is_multiple_of(16) {
        archive.push(0);
    }
    archive.extend_from_slice(&info);
    archive.extend_from_slice(member);
    let size = archive.len() as u64;
    archive[size_position..size_position + 8].copy_from_slice(&size.to_be_bytes());
    archive
}

#[test]
fn verify_unityfs_raw_member_and_bounds() {
    let member = b"compiled shader fixture";
    let archive = raw_unityfs(member);
    ra::verify_unityfs_member(&archive, "shader.member", member).unwrap();
    for input in [&[][..], &archive[..10], &archive[..archive.len() - 1]] {
        assert!(ra::verify_unityfs_member(input, "shader.member", member).is_err());
    }
    assert_eq!(
        ra::verify_unityfs_member(&archive, "shader.member", b"compiled shader fixturE")
            .unwrap_err()
            .to_string(),
        "compiled Shader member bytes differ from the pinned UnityFS archive"
    );
    assert!(ra::verify_unityfs_member(&archive, "", member).is_err());
}

#[test]
fn unityfs_lz4_overlapping_match_and_malformed_offsets() {
    // One literal plus an overlapping offset-1 match, followed by a literal.
    let decoded = ra::unpack_unityfs_block(&[0x10, b'A', 1, 0, 0x10, b'B'], 6, 3).unwrap();
    assert_eq!(decoded, b"AAAAAB");
    for input in [
        &[0x10, b'A', 0, 0][..],
        &[0x10, b'A', 2, 0],
        &[0xf0],
        &[0x10],
        &[0x00, 1],
    ] {
        assert!(ra::unpack_unityfs_block(input, 6, 2).is_err(), "{input:?}");
    }
    assert_eq!(
        ra::unpack_unityfs_block(b"x", 1, 1)
            .unwrap_err()
            .to_string(),
        "UnityFS compression 1 is unsupported"
    );
}

// ---- textures ---------------------------------------------------------------

fn texture_resource_tree() -> Vec<u8> {
    let values: [(&str, u8, u8, i32, u32); 14] = [
        ("Texture2D", 0, 0, -1, 0),
        ("m_Width", 1, 0, 4, 0),
        ("m_Height", 1, 0, 4, 0),
        ("m_TextureFormat", 1, 0, 4, 0),
        ("m_MipCount", 1, 0, 4, 0),
        ("m_CompleteImageSize", 1, 0, 4, 0),
        ("image data", 1, 0, -1, 0),
        ("Array", 2, 1, -1, 0x4000),
        ("size", 3, 0, 4, 0),
        ("data", 3, 0, 1, 0),
        ("m_StreamData", 1, 0, -1, 0),
        ("offset", 2, 0, 8, 0),
        ("size", 2, 0, 4, 0),
        ("path", 2, 0, -1, 0x4000),
    ];
    let (mut nodes, mut chars) = (Vec::new(), Vec::new());
    for (name, level, flags, size, meta) in values {
        let mut data = [0u8; 32];
        (data[2], data[3]) = (level, flags);
        put_le32(&mut data[8..], chars.len() as u32);
        put_le32(&mut data[12..], size as u32);
        put_le32(&mut data[20..], meta);
        nodes.extend_from_slice(&data);
        chars.extend_from_slice(name.as_bytes());
        chars.push(0);
    }
    let mut tree = vec![0u8; 16];
    put_le32(&mut tree[8..], values.len() as u32);
    put_le32(&mut tree[12..], chars.len() as u32);
    tree.extend_from_slice(&nodes);
    tree.extend_from_slice(&chars);
    tree
}

#[test]
fn native_texture_tree_finds_stream_and_preserves_layout() {
    let path = "VirtualArtifacts/Primary/22222222222222222222222222222222.resource";
    let mut object = Vec::new();
    for value in [32u32, 16, 10, 6, 512, 4, 0x12345678] {
        object.extend_from_slice(&value.to_le_bytes());
    }
    object.extend_from_slice(&16u64.to_le_bytes());
    object.extend_from_slice(&256u32.to_le_bytes());
    object.extend_from_slice(&(path.len() as u32).to_le_bytes());
    object.extend_from_slice(path.as_bytes());
    while !object.len().is_multiple_of(4) {
        object.push(0);
    }
    let tree = texture_resource_tree();
    let (resources, meta) = ra::read_texture_resources(&object, &tree).unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].virtual_path, path);
    assert_eq!((resources[0].offset, resources[0].size), (16, 256));
    assert_eq!(
        (
            meta.width,
            meta.height,
            meta.format,
            meta.mip_count,
            meta.complete_image_size
        ),
        (32, 16, 10, 6, 512)
    );
    for variant in [
        "truncated",
        "extra tail",
        "invalid dimensions",
        "invalid resource alias",
    ] {
        let mut bad = object.clone();
        match variant {
            "truncated" => bad.truncate(bad.len() - 1),
            "extra tail" => bad.push(1),
            "invalid dimensions" => put_le32(&mut bad, 0),
            _ => bad[44] = b'!',
        }
        assert!(
            ra::read_texture_resources(&bad, &tree).is_err(),
            "{variant}"
        );
    }
}

#[test]
fn texture_projection_rejects_unverified_profile_and_header() {
    for profile in ["", "fbx", "uss", "png", "jpg", "jpeg"] {
        assert!(ra::adapt_texture(&[0u8; 64], profile).is_err(), "{profile}");
    }
    assert!(ra::adapt_shader(&[0u8; 64]).is_err());
}

/// Worker artifacts of one 16x8 PNG under four importer settings: a sprite,
/// a cubemap, a texture array and a volume.
#[test]
fn texture_projection_keeps_the_texture_of_its_shape_and_its_sprites() {
    let artifact =
        |name: &str| fs::read(format!("rust/tests/testdata/native/{name}.png.artifact")).unwrap();
    let objects = |metadata: &Metadata| -> Vec<(String, i32, String)> {
        metadata
            .objects
            .iter()
            .map(|object| {
                (
                    object.file_id.clone(),
                    object.class_id,
                    object.type_name.clone(),
                )
            })
            .collect()
    };
    let (output, metadata) = ra::adapt_texture(&artifact("sprite"), "png").unwrap();
    assert_eq!(
        objects(&metadata),
        [
            ("2800000".into(), 28, "UnityEngine.Texture2D".into()),
            ("21300000".into(), 213, "UnityEngine.Sprite".into()),
        ]
    );
    // The importer goes; nothing else does.
    assert_eq!(metadata.removed.len(), 1);
    assert_eq!(metadata.removed[0].class_id, 1006);
    let texture = metadata.texture.as_ref().unwrap();
    assert_eq!((texture.width, texture.height), (16, 8));
    assert_eq!(metadata.output_sha256, sha256(&output));
    for (name, id, class, type_name) in [
        ("cubemap", "8900000", 89, "UnityEngine.Cubemap"),
        ("array", "18700000", 187, "UnityEngine.Texture2DArray"),
        ("volume", "11700000", 117, "UnityEngine.Texture3D"),
    ] {
        let (_, metadata) = ra::adapt_texture(&artifact(name), "png").unwrap();
        assert_eq!(
            objects(&metadata),
            [(id.to_string(), class, type_name.to_string())],
            "{name}"
        );
        // An array and a volume name a graphics format, which the texture
        // metadata does not hold.
        assert_eq!(metadata.texture.is_some(), class == 89, "{name}");
    }
}

// ---- Mesh CPU retention -----------------------------------------------------

fn assert_mesh_cpu_projection(input: &[u8]) -> (Vec<u8>, MeshCpuProjection) {
    let (output, proof) = ra::retain_model_mesh_cpu(input).unwrap();
    assert_eq!(output.len(), input.len());
    assert_eq!(proof.input_sha256, sha256(input));
    assert_eq!(proof.output_sha256, sha256(&output));
    assert!(!proof.meshes.is_empty());
    let mut masked = output.clone();
    for mesh in &proof.meshes {
        assert!(!mesh.original_is_readable);
        assert!(!mesh.file_id.is_empty() && !mesh.payload_sha256.is_empty());
        assert!(!mesh.type_tree_sha256.is_empty());
        for (offset, value) in [
            (mesh.is_readable_offset, mesh.original_is_readable),
            (mesh.keep_vertices_offset, mesh.original_keep_vertices),
            (mesh.keep_indices_offset, mesh.original_keep_indices),
        ] {
            assert_eq!(output[offset], 1, "Mesh CPU flag was not retained");
            masked[offset] = value as u8;
        }
    }
    assert_eq!(input, masked.as_slice());
    let (second, second_proof) = ra::retain_model_mesh_cpu(&output).unwrap();
    assert_eq!(second, output);
    assert!(second_proof.meshes.is_empty());
    (output, proof)
}

#[test]
fn retain_native_runtime_mesh_cpu_actual_file() {
    let Some(path) = fixture("UNITY_WORKER_MODEL_CPU_FIXTURE") else {
        return;
    };
    let (_, proof) = assert_mesh_cpu_projection(&fs::read(&path).unwrap());
    eprintln!("actual native model={path} meshes={}", proof.meshes.len());
}

fn mesh_cpu_fixture_tree() -> Vec<u8> {
    let strings = [
        "Mesh",
        "Base",
        "string",
        "m_Name",
        "vector",
        "prefix",
        "Array",
        "int",
        "size",
        "UInt8",
        "data",
        "bool",
        "m_IsReadable",
        "m_KeepVertices",
        "m_KeepIndices",
    ];
    let mut chars = Vec::new();
    let mut offsets = std::collections::HashMap::new();
    for text in strings {
        offsets.insert(text, chars.len() as u32);
        chars.extend_from_slice(text.as_bytes());
        chars.push(0);
    }
    let mut nodes = Vec::new();
    for (level, array, kind, name, size, flags) in [
        (0u8, 0u8, "Mesh", "Base", -1i32, 0u32),
        (1, 0, "string", "m_Name", -1, 0),
        (2, 1, "Array", "Array", -1, 0x4000),
        (3, 0, "int", "size", 4, 0),
        (3, 0, "UInt8", "data", 1, 0),
        (1, 0, "vector", "prefix", -1, 0),
        (2, 1, "Array", "Array", -1, 0x4000),
        (3, 0, "int", "size", 4, 0),
        (3, 0, "UInt8", "data", 1, 0),
        (1, 0, "bool", "m_IsReadable", 1, 0),
        (1, 0, "bool", "m_KeepVertices", 1, 0),
        (1, 0, "bool", "m_KeepIndices", 1, 0),
    ] {
        let mut node = [0u8; 32];
        node[..2].copy_from_slice(&1u16.to_le_bytes());
        (node[2], node[3]) = (level, array);
        put_le32(&mut node[4..], offsets[kind]);
        put_le32(&mut node[8..], offsets[name]);
        put_le32(&mut node[12..], size as u32);
        put_le32(&mut node[20..], flags);
        nodes.extend_from_slice(&node);
    }
    let mut tree = vec![0u8; 16];
    put_le32(&mut tree, 0x7474686d);
    put_le32(&mut tree[4..], 23);
    put_le32(&mut tree[8..], (nodes.len() / 32) as u32);
    put_le32(&mut tree[12..], chars.len() as u32);
    tree.extend_from_slice(&nodes);
    tree.extend_from_slice(&chars);
    tree
}

fn mesh_cpu_prefix() -> Vec<u8> {
    let mut prefix = 8u32.to_le_bytes().to_vec();
    prefix.extend_from_slice(b"Variable");
    prefix.extend_from_slice(&3u32.to_le_bytes());
    prefix.extend_from_slice(&[8, 4, 2, 0]);
    prefix.extend_from_slice(&[0, 0, 1, 255]);
    prefix
}

#[test]
fn mesh_cpu_flags_use_inline_layout_and_preserve_variable_prefix() {
    let tree = mesh_cpu_fixture_tree();
    let prefix = mesh_cpu_prefix();
    let flags = ra::read_mesh_cpu_flags(&prefix, &tree).unwrap();
    assert_eq!(
        (
            flags.is_readable_offset,
            flags.keep_vertices_offset,
            flags.keep_indices_offset
        ),
        (20, 21, 22)
    );
    assert!(!flags.original_is_readable && !flags.original_keep_vertices);
    assert!(flags.original_keep_indices);
    for kind in [
        "bool-value",
        "missing-flag",
        "nested-flag",
        "bad-size",
        "truncated",
    ] {
        let (mut data, mut bad_tree) = (prefix.clone(), tree.clone());
        match kind {
            "bool-value" => data[20] = 2,
            "missing-flag" => put_le32(&mut bad_tree[16 + 9 * 32 + 8..], 0),
            "nested-flag" => bad_tree[16 + 9 * 32 + 2] = 2,
            "bad-size" => put_le32(&mut bad_tree[16 + 9 * 32 + 12..], 2),
            _ => data.truncate(21),
        }
        assert!(ra::read_mesh_cpu_flags(&data, &bad_tree).is_err(), "{kind}");
    }
}

/// A whole projected file around the fixture tree, so that the projection
/// itself (not only the flag reader) runs without external fixtures.
#[test]
fn mesh_cpu_projection_changes_only_three_flags() {
    let tree = mesh_cpu_fixture_tree();
    let mut metadata = b"6000.6.3f1\0".to_vec();
    metadata.extend_from_slice(&24u32.to_le_bytes());
    metadata.push(1);
    metadata.extend_from_slice(&1u32.to_le_bytes());
    metadata.extend_from_slice(&43u32.to_le_bytes());
    metadata.extend_from_slice(&[0, 0xff, 0xff]);
    metadata.extend_from_slice(&[0u8; 32]);
    metadata.extend_from_slice(&(tree.len() as u32).to_le_bytes());
    metadata.extend_from_slice(&tree);
    metadata.extend_from_slice(&0u32.to_le_bytes());
    metadata.extend_from_slice(&1u32.to_le_bytes());
    while !metadata.len().is_multiple_of(4) {
        metadata.push(0);
    }
    let prefix = mesh_cpu_prefix();
    metadata.extend_from_slice(&4300000u64.to_le_bytes());
    metadata.extend_from_slice(&0u64.to_le_bytes());
    metadata.extend_from_slice(&(prefix.len() as u32).to_le_bytes());
    metadata.extend_from_slice(&0u32.to_le_bytes());
    let data_offset = (48 + metadata.len() + 15) & !15;
    let mut input = vec![0u8; data_offset];
    input[8..12].copy_from_slice(&23u32.to_be_bytes());
    input[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
    input[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
    input[48..48 + metadata.len()].copy_from_slice(&metadata);
    input.extend_from_slice(&prefix);
    let size = input.len() as u64;
    input[24..32].copy_from_slice(&size.to_be_bytes());
    let (_, proof) = assert_mesh_cpu_projection(&input);
    assert_eq!(proof.meshes[0].file_id, "4300000");
    assert_eq!(proof.meshes[0].is_readable_offset, data_offset + 20);
    assert_eq!(proof.mode, "native-model-mesh-cpu-v1");
}

// ---- UI assets --------------------------------------------------------------

#[test]
fn adapt_ui_retains_real_importer_payload_and_class_identity() {
    let Some(root) = fixture("UNITY_WORKER_UI_FIXTURES") else {
        return;
    };
    for (profile, file, objects) in [
        ("uss", "b54a76024e0db49348ae426002a8a519.bin", 1),
        ("uxml", "87785bfd71a0a425fb905aaf7743f114.bin", 2),
    ] {
        let input = fs::read(Path::new(&root).join(file)).unwrap();
        let (output, proof) = ra::adapt_ui(&input, profile).unwrap();
        assert_eq!(proof.objects.len(), objects);
        assert_eq!(proof.removed.len(), 1);
        assert_eq!(proof.removed[0].class_id, 2089858483);
        let offset = be64(&input[32..40]) as usize;
        assert_eq!(input[offset..], output[offset..]);
        assert_eq!(
            proof.external_references[0].guid,
            "0000000000000000e000000000000000"
        );
        let id = if profile == "uxml" {
            1068529831357515114
        } else {
            7433441132597879392
        };
        let start = native_object_start(&input, id);
        let mut bad = input.clone();
        bad[start + 64..start + 72].copy_from_slice(&12385u64.to_le_bytes());
        assert!(ra::adapt_ui(&bad, profile).is_err());
    }
}

#[test]
fn adapt_ui_rejects_malformed_headers_and_unsupported_profiles() {
    for profile in ["", "fbx", "uss", "uxml", "ttf", "otf"] {
        assert!(ra::adapt_ui(&[0u8; 64], profile).is_err(), "{profile}");
    }
}

// ---- Player shaders ---------------------------------------------------------

#[test]
fn extract_player_shader_rejects_malformed_metadata() {
    for input in [&[][..], &[0u8; 64]] {
        assert!(
            ra::extract_player_shader(input, "2cbfcd0349c944ee788f6e2919b9d66c", "Sample/Screen")
                .is_err()
        );
    }
}

#[test]
fn player_shader_references_cannot_be_hidden_by_fixed_struct_size() {
    // Fixed-size wrappers still need traversal. A dependency hidden in such a
    // wrapper would become dangling after the original tables are removed.
    let strings = b"Shader\0Base\0Wrapper\0wrapper\0PPtr<Texture>\0texture\0";
    let mut nodes = [0u8; 3 * 32];
    for (index, (level, kind, name, size)) in
        [(0u8, 0u32, 7u32, 12u32), (1, 12, 20, 12), (2, 28, 42, 12)]
            .into_iter()
            .enumerate()
    {
        let node = &mut nodes[index * 32..(index + 1) * 32];
        node[2] = level;
        put_le32(&mut node[4..], kind);
        put_le32(&mut node[8..], name);
        put_le32(&mut node[12..], size);
    }
    let mut tree = vec![0u8; 16];
    put_le32(&mut tree[8..], 3);
    put_le32(&mut tree[12..], strings.len() as u32);
    tree.extend_from_slice(&nodes);
    tree.extend_from_slice(strings);
    let mut payload = [0u8; 12];
    payload[4..].copy_from_slice(&12345u64.to_le_bytes());
    let error = ra::read_player_shader(&payload, &tree)
        .unwrap_err()
        .to_string();
    assert!(error.contains("live dependency"), "{error}");
    assert_eq!(
        error,
        "compiled Shader has a live dependency at Base.wrapper.texture"
    );
    // Trees without a node table, or cut short of it, are refused.
    for (bad, expected) in [
        (
            &tree[..8],
            "compiled Shader inline tree is shorter than its header",
        ),
        (
            &tree[..16],
            "compiled Shader inline tree node table is out of range",
        ),
        (&[0u8; 64][..], "compiled Shader inline tree has no nodes"),
    ] {
        assert_eq!(
            ra::read_player_shader(&payload, bad)
                .unwrap_err()
                .to_string(),
            expected
        );
    }
}

// ---- built-in shader and lighting -------------------------------------------

#[test]
fn builtin_shader_native_identity_and_compiled_payload() {
    let (Some(member_path), Some(source_path)) = (
        fixture("UNITY_PLAY_BUILTIN_SHADER_FIXTURE"),
        fixture("UNITY_PLAY_BUILTIN_SHADER_SOURCE"),
    ) else {
        return;
    };
    let member = fs::read(member_path).unwrap();
    let (projected, proof) = ra::extract_builtin_player_shader(&member, 108).unwrap();
    assert_eq!(proof.builtin_guid, "0000000000000000f000000000000000");
    assert_eq!(proof.builtin_file_id, "108");
    assert_eq!(proof.archive_file_id, "2756170372350126523");
    assert_eq!(proof.name, "Skybox/Panoramic");
    assert_eq!(
        proof.payload_sha256,
        "8fe06b73b38fb9ceaebc86e5a9a83661ba7bbe20ca0dc88f663d9c20758b7ee1"
    );
    let (again, reread) = ra::extract_builtin_player_shader(&projected, 108).unwrap();
    assert_eq!(projected, again);
    assert_eq!(reread.archive_file_id, "108");
    assert_eq!(reread.payload_sha256, proof.payload_sha256);
    for id in [0, 101, 4800000] {
        assert!(ra::extract_builtin_player_shader(&member, id).is_err());
    }
    let file = fs::File::open(source_path).unwrap();
    let size = file.metadata().unwrap().len() as i64;
    let identity = ra::inspect_builtin_shader(&file, size, 108).unwrap();
    assert_eq!(identity.name, proof.name);
    assert_eq!(identity.file_id, 108);
    assert_eq!(
        identity.payload_sha256,
        "1873d28b8300bbba76b8e69975db0d2afac29dfed413a129cd758e05ba015d0a"
    );
}

#[test]
fn builtin_shader_inspector_rejects_invalid_headers() {
    for data in [vec![], vec![0u8; 48], vec![0u8; 80]] {
        assert!(ra::inspect_builtin_shader(&data, data.len() as i64, 108).is_err());
    }
    assert_eq!(
        ra::inspect_builtin_shader(&[][..], 0, 108)
            .unwrap_err()
            .to_string(),
        "native resource ends before its declared data"
    );
}

#[test]
fn builtin_lighting_sdk_identity() {
    let Some(path) = fixture("UNITY_BUILTIN_LIGHTING_RESOURCE") else {
        return;
    };
    let file = fs::File::open(path).unwrap();
    let size = file.metadata().unwrap().len() as i64;
    let proof = ra::inspect_builtin_lighting(&file, size).unwrap();
    eprintln!("{}", serde_json::to_string(&proof).unwrap());
    assert_eq!((proof.file_id, proof.class_id), (20201, 1120));
    assert_eq!((proof.lightmap_count, proof.light_probe_count), (0, 0));
}

#[test]
fn builtin_lighting_rejects_malformed_and_unverified_native_files() {
    for data in [
        vec![],
        vec![0u8; 64],
        b"same named object does not prove native lighting content".to_vec(),
    ] {
        assert!(ra::inspect_builtin_lighting(&data, data.len() as i64).is_err());
    }
}

// ---- shader-extract ---------------------------------------------------------

fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn guarded_shader_extraction_actual_archive_and_immutable_output() {
    let (Some(input), Some(bundle)) = (
        fixture("UNITY_PLAY_COMPILED_SHADER_FIXTURE"),
        fixture("UNITY_PLAY_COMPILED_SHADER_BUNDLE"),
    ) else {
        return;
    };
    let data = fs::read(&input).unwrap();
    let archive = fs::read(&bundle).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let project = fs::canonicalize(temporary.path()).unwrap();
    let project = project.to_str().unwrap();
    let source = "Assets/Screen.shader";
    let shader = format!("{project}/{source}");
    fs::create_dir_all(format!("{project}/Assets")).unwrap();
    fs::write(
        &shader,
        "Shader \"Sample/Screen\" {\n#include \"Optional.hlsl\"\n}\n",
    )
    .unwrap();
    fs::write(
        format!("{shader}.meta"),
        "fileFormatVersion: 2\nguid: 2cbfcd0349c944ee788f6e2919b9d66c\nShaderImporter:\n",
    )
    .unwrap();
    let origin = format!("{project}/origin.json");
    fs::write(
        &origin,
        json!([{"source": bundle, "sha256": sha256(&archive), "nodes": [{"name": "CAB-c7e0997e8a48cbbb3c94d6b2c3d64de1.sharedAssets", "size": data.len(), "path": input, "sha256": sha256(&data)}]}])
            .to_string(),
    )
    .unwrap();
    let output = format!("{project}/Temp/Shader/runtime.bin");
    let environment = "a".repeat(64);
    let digest = sha256(&data);
    let args = arguments(&[
        "--input",
        &input,
        "--input-sha256",
        &digest,
        "--guid",
        "2cbfcd0349c944ee788f6e2919b9d66c",
        "--shader-name",
        "Sample/Screen",
        "--output",
        &output,
        "--project",
        project,
        "--source",
        source,
        "--origin",
        &origin,
        "--environment-sha256",
        &environment,
    ]);
    let mut stdout = Vec::new();
    ra::shader_extract(&args, &mut stdout).unwrap();
    let replacement: shader_sources::NativeReplacement =
        serde_json::from_slice(&fs::read(format!("{output}.replacement.json")).unwrap()).unwrap();
    let snapshot = replacement.compiled_shader_snapshot.unwrap();
    assert!(!snapshot.compiled_source_revision_verified);
    assert_eq!(replacement.environment_sha256, environment);
    assert_eq!(replacement.content.objects[0].file_id, "4800000");
    shader_sources::validate_shader_source_requirements(
        project,
        source,
        &snapshot.source_requirements,
        &snapshot.missing_includes,
    )
    .unwrap();
    let before = fs::read(&output).unwrap();
    assert!(
        ra::shader_extract(&args, &mut stdout).is_err(),
        "replaced an immutable Shader revision"
    );
    assert_eq!(before, fs::read(&output).unwrap());
    fs::write(format!("{project}/Assets/Optional.hlsl"), "changed").unwrap();
    assert!(
        shader_sources::validate_shader_source_requirements(
            project,
            source,
            &snapshot.source_requirements,
            &snapshot.missing_includes,
        )
        .is_err(),
        "guard accepted a later optional include"
    );
}

#[test]
fn shader_extraction_rejects_incomplete_profile() {
    let hash = "a".repeat(64);
    for args in [
        arguments(&[]),
        arguments(&["--input", "relative"]),
        arguments(&[
            "--input",
            "/input",
            "--output",
            "/outside/runtime.bin",
            "--input-sha256",
            &hash,
        ]),
        arguments(&[
            "--input",
            "/input",
            "--output",
            "/Temp/runtime.bin",
            "--input-sha256",
            &hash,
            "--project",
            "/project",
        ]),
    ] {
        assert!(
            ra::shader_extract(&args, &mut Vec::new()).is_err(),
            "{args:?}"
        );
    }
}

// ---- shader source guard, commands and report shapes ------------------------

#[test]
fn shader_source_guard_tracks_includes_and_absent_files() {
    let temporary = tempfile::tempdir().unwrap();
    let project = fs::canonicalize(temporary.path()).unwrap();
    let project = project.to_str().unwrap();
    fs::create_dir_all(format!("{project}/Assets/Sub")).unwrap();
    fs::create_dir_all(format!(
        "{project}/Library/PackageCache/com.example@1.2.3/Runtime"
    ))
    .unwrap();
    fs::write(
        format!("{project}/Assets/Screen.shader"),
        "Shader \"S\" {\n // #include \"Commented.hlsl\"\n /* #include \"Block.hlsl\" */\n  #  include \"Sub/A.hlsl\"\n#include_with_pragmas <Packages/com.example/Runtime/P.hlsl>\n#include \"Missing.hlsl\"\n string s = \"// not a comment\";\n}\n",
    )
    .unwrap();
    fs::write(
        format!("{project}/Assets/Sub/A.hlsl"),
        "#include \"../Screen.shader\"\n#include \"B.hlsl\"\n",
    )
    .unwrap();
    fs::write(format!("{project}/Assets/Sub/B.hlsl"), "float b;\n").unwrap();
    fs::write(
        format!("{project}/Library/PackageCache/com.example@1.2.3/Runtime/P.hlsl"),
        "float p;\n",
    )
    .unwrap();
    let (requirements, missing) =
        shader_sources::collect_shader_source_requirements(project, "Assets/Screen.shader")
            .unwrap();
    let paths: Vec<&str> = requirements.iter().map(|item| item.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Assets/Screen.shader",
            "Assets/Sub/A.hlsl",
            "Assets/Sub/B.hlsl",
            "Packages/com.example/Runtime/P.hlsl"
        ]
    );
    assert_eq!(
        requirements[3].source_path,
        format!("{project}/Library/PackageCache/com.example@1.2.3/Runtime/P.hlsl")
    );
    assert_eq!(requirements[2].sha256, sha256(b"float b;\n"));
    assert_eq!(missing, ["Assets/Missing.hlsl"]);
    shader_sources::validate_shader_source_requirements(
        project,
        "Assets/Screen.shader",
        &requirements,
        &missing,
    )
    .unwrap();
    fs::write(format!("{project}/Assets/Sub/B.hlsl"), "float c;\n").unwrap();
    assert!(
        shader_sources::validate_shader_source_requirements(
            project,
            "Assets/Screen.shader",
            &requirements,
            &missing
        )
        .is_err()
    );
    for (source, text) in [
        ("Assets/Macro.shader", "#include MACRO\n"),
        ("Assets/Escape.shader", "#include \"../../outside.hlsl\"\n"),
        ("Assets/Open.shader", "/* never closed\n"),
    ] {
        fs::write(format!("{project}/{source}"), text).unwrap();
        assert!(
            shader_sources::collect_shader_source_requirements(project, source).is_err(),
            "{source}"
        );
    }
    for source in [
        "Assets/Absent.shader",
        "Assets/Sub/A.hlsl",
        "Packages/com.example/Runtime/P.shader",
        "Assets/../Assets/Screen.shader",
    ] {
        assert!(
            shader_sources::collect_shader_source_requirements(project, source).is_err(),
            "{source}"
        );
    }
}

#[test]
fn builtin_shader_command_requires_canonical_absolute_paths() {
    let options = BuiltinShaderOptions {
        project: "/project".into(),
        input: "/project/in".into(),
        bundle: "/project/bundle".into(),
        builtin: "/project/../builtin".into(),
        output: "/project/Temp/out".into(),
        ..Default::default()
    };
    assert_eq!(
        ra::extract_builtin(&options, &mut Vec::new())
            .unwrap_err()
            .to_string(),
        "built-in Shader extraction requires canonical absolute paths"
    );
    assert!(ra::extract_builtin(&BuiltinShaderOptions::default(), &mut Vec::new()).is_err());
}

#[test]
fn reports_json_shape() {
    // Lists are always arrays; empty optional members are omitted.
    let text = serde_json::to_string(&Metadata::default()).unwrap();
    assert_eq!(
        text,
        r#"{"profile":"","serializedVersion":0,"sourceTarget":0,"runtimeTarget":0,"inputSHA256":"","outputSHA256":"","objects":[],"removed":[],"externalReferences":[],"resources":[]}"#
    );
    assert_eq!(
        serde_json::from_str::<Metadata>(&text).unwrap(),
        Metadata::default()
    );
    assert_eq!(ra::hash_model_graph(&[]), sha256(b"[]"));
}

#[test]
fn model_graph_hash_orders_by_numeric_local_id() {
    let object = |id: &str| ra::ModelGraphObject {
        file_id: id.into(),
        class_id: 33,
        ..Default::default()
    };
    let ordered = [object("-5"), object("9"), object("10")];
    let shuffled = [object("10"), object("-5"), object("9")];
    assert_eq!(
        ra::hash_model_graph(&ordered),
        ra::hash_model_graph(&shuffled)
    );
    assert_eq!(
        ra::hash_model_graph(&ordered),
        sha256(serde_json::to_string(&ordered).unwrap().as_bytes())
    );
    assert_eq!(
        serde_json::to_string(&ordered[..1]).unwrap(),
        r#"[{"fileId":"-5","classId":33,"references":[],"scalars":{}}]"#
    );
}

#[test]
fn float32_report_members_use_shortest_round_trip_text() {
    for (length, expected) in [
        (1.0f32, "1.0"),
        (1.5e-7, "1.5e-7"),
        (1e21, "1e+21"),
        (0.1, "0.1"),
        (123456.79, "123456.79"),
    ] {
        let audio = ra::AudioMetadata {
            length,
            ..Default::default()
        };
        let text = serde_json::to_string(&audio).unwrap();
        assert!(
            text.ends_with(&format!("\"length\":{expected}}}")),
            "{text}"
        );
        assert_eq!(
            serde_json::from_str::<ra::AudioMetadata>(&text).unwrap(),
            audio
        );
    }
}

// ---- scripted importers and video --------------------------------------------

/// A worker artifact kept with the tests; each is byte-identical to the one
/// the Editor wrote for the same source.
fn native_fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("rust/tests/testdata/native")
            .join(name),
    )
    .unwrap()
}

fn pointer(value: &Val) -> (i64, i64) {
    let number = |name: &str| match value.field(name) {
        Some(Val::I(number)) => *number,
        Some(Val::U(number)) => *number as i64,
        _ => 0,
    };
    (number("m_FileID"), number("m_PathID"))
}

#[test]
fn scripted_import_objects_point_at_scripts_of_their_own_file() {
    let input = native_fixture("probe.inputactions.artifact");
    let (output, metadata) = ra::adapt_scripted(&input).unwrap();
    // The importer object stays behind; the asset and its two references to
    // the one action keep the local IDs the Editor gave them.
    assert_eq!(metadata.removed.len(), 1);
    assert_eq!(metadata.removed[0].class_id, 2089858483);
    let listed: Vec<(&str, &str)> = metadata
        .objects
        .iter()
        .map(|object| (object.file_id.as_str(), object.type_name.as_str()))
        .collect();
    assert_eq!(
        listed,
        [
            (
                "-944628639613478452",
                "UnityEngine.InputSystem.InputActionAsset"
            ),
            (
                "1078111047217735087",
                "UnityEngine.InputSystem.InputActionReference"
            ),
            (
                "2019923273695298903",
                "UnityEngine.InputSystem.InputActionReference"
            ),
        ]
    );
    assert_eq!(metadata.output_sha256, sha256(&output));
    assert!(metadata.external_references.is_empty());
    let written = playerbake::read_serialized(&output).unwrap();
    // The scripts come first, one per class.
    assert_eq!(written.len(), 5);
    let mut classes = Vec::new();
    for script in &written[..2] {
        assert_eq!(script.tree.class_id, 115);
        let value = script.tree.decode(&script.data).unwrap();
        let text = |name: &str| match value.field(name) {
            Some(Val::Str(text)) => text.clone(),
            _ => String::new(),
        };
        assert_eq!(text("m_AssemblyName"), "Unity.InputSystem");
        assert_eq!(text("m_Namespace"), "UnityEngine.InputSystem");
        classes.push((script.local_id, text("m_ClassName")));
    }
    classes.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(classes[0].1, "InputActionAsset");
    assert_eq!(classes[1].1, "InputActionReference");
    for object in &written[2..] {
        assert_eq!(object.tree.class_id, 114);
        let class = if object.local_id == -944628639613478452 {
            classes[0].0
        } else {
            classes[1].0
        };
        assert_eq!(object.script, Some(class));
        let value = object.tree.decode(&object.data).unwrap();
        assert_eq!(pointer(value.field("m_Script").unwrap()), (0, class));
        if object.local_id != -944628639613478452 {
            assert_eq!(
                pointer(value.field("m_Asset").unwrap()),
                (0, -944628639613478452)
            );
        }
    }
    // The same artifact gives the same file.
    assert_eq!(ra::adapt_scripted(&input).unwrap().0, output);
    // The other profiles do not take script-backed objects.
    assert!(ra::adapt(&input, "text").is_err());
}

#[test]
fn video_clip_takes_its_frames_from_the_source_or_the_transcoded_resource() {
    let guid = "3766e5c8770fec4a8ad750da9b84d09f";
    let alias = format!("VirtualArtifacts/Primary/{guid}.resource");
    for (name, from_source, size) in [
        ("vp8.webm.artifact", true, 3337),
        ("vp8-transcoded.webm.artifact", false, 1092),
    ] {
        let input = native_fixture(name);
        let (output, metadata, source) = ra::adapt_video(&input, guid).unwrap();
        assert_eq!(source, from_source, "{name}");
        assert_eq!(metadata.objects.len(), 1);
        assert_eq!(metadata.objects[0].file_id, "32900000");
        assert_eq!(metadata.objects[0].type_name, "UnityEngine.Video.VideoClip");
        assert_eq!(metadata.removed.len(), 1);
        assert_eq!(metadata.removed[0].class_id, 1127);
        assert_eq!(metadata.resources.len(), 1);
        assert_eq!(metadata.resources[0].virtual_path, alias);
        assert_eq!(
            (metadata.resources[0].offset, metadata.resources[0].size),
            (0, size)
        );
        let written = playerbake::read_serialized(&output).unwrap();
        assert_eq!(written.len(), 1);
        let value = written[0].tree.decode(&written[0].data).unwrap();
        let resource = value.field("m_ExternalResources").unwrap();
        assert_eq!(resource.field("m_Source"), Some(&Val::Str(alias.clone())));
    }
    // A transcoded clip names the resource of its own GUID and no other.
    let transcoded = native_fixture("vp8-transcoded.webm.artifact");
    assert!(ra::adapt_video(&transcoded, "00000000000000000000000000000001").is_err());
    // An artifact without a clip is what the engine leaves of a video it
    // cannot read.
    let error = ra::adapt_video(&native_fixture("probe.inputactions.artifact"), guid).unwrap_err();
    assert!(
        format!("{error:#}").contains("SerializedFile") || format!("{error:#}").contains("VP8")
    );
}
