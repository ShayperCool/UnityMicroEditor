use super::{
    MAXIMUM_BYTES, Reader,
    adapt::{ExternalReference, Layout, Metadata, Object, RemovedObject, project_table},
    be32, be64, le16, le32, le64, ordinary_guid, req,
    texture::read_texture_resources,
};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

/// Preserves the actual UI/Font importer payload and its type metadata. Only
/// the known importer object entries and the Editor target are changed.
/// Runtime compatibility is checked through ContentFile.GetObject by the caller.
pub fn adapt_ui(input: &[u8], profile: &str) -> Result<(Vec<u8>, Metadata)> {
    if !matches!(profile, "uxml" | "uss" | "ttf" | "otf") {
        bail!("unsupported UI runtime profile {profile:?}");
    }
    adapt_warm_builtin(input, profile)
}

/// The source formats TextureImporter reads, by lower-case extension.
pub fn texture_profile(profile: &str) -> bool {
    matches!(
        profile,
        "png" | "jpg" | "jpeg" | "tga" | "tif" | "tiff" | "bmp" | "psd" | "gif" | "exr" | "hdr"
    )
}

/// Retains Unity's imported Texture2D, including its chosen native pixel
/// format and mip chain. It never resizes or recompresses source pixels.
pub fn adapt_texture(input: &[u8], profile: &str) -> Result<(Vec<u8>, Metadata)> {
    if !texture_profile(profile) {
        bail!("unsupported texture runtime profile {profile:?}");
    }
    adapt_warm_builtin(input, profile)
}

/// An Editor Shader artifact contains parsed HLSL but no platform programs.
/// Changing its target cannot supply the missing GPU compilation stage.
pub fn adapt_shader(_input: &[u8]) -> Result<(Vec<u8>, Metadata)> {
    bail!("native Editor Shader requires platform GPU compilation before runtime adaptation")
}

fn adapt_warm_builtin(input: &[u8], profile: &str) -> Result<(Vec<u8>, Metadata)> {
    let mut result = Metadata::default();
    let texture = texture_profile(profile);
    let font = matches!(profile, "ttf" | "otf");
    if input.len() < 64
        || input.len() > MAXIMUM_BYTES
        || be32(&input[8..12]) != 23
        || input[16] != 0
    {
        bail!("UI runtime projection requires little-endian SerializedFile v23");
    }
    let file_size = be64(&input[24..32]);
    let data_offset = be64(&input[32..40]);
    let metadata_end = 48 + be32(&input[20..24]) as u64;
    if file_size != input.len() as u64 || metadata_end > data_offset || data_offset > file_size {
        bail!("UI SerializedFile header bounds are invalid");
    }
    let (metadata_end, data_offset) = (metadata_end as usize, data_offset as usize);
    let payload = (input.len() - data_offset) as u64;
    let mut r = Reader::new(&input[48..metadata_end]);
    req(
        r.cstring_bytes(64),
        |version| *version == b"0.0.0" || *version == b"6000.6.3f1",
        "UI artifact must use the pinned Unity version",
    )?;
    let platform_offset = 48 + r.pos;
    req(
        r.u32(),
        |platform| *platform as i32 == -2,
        "UI artifact is not an Editor-target native artifact",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "UI artifact requires inline type trees",
    )?;
    let type_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 256,
        "UI artifact type count is invalid",
    )?;
    let mut classes = Vec::with_capacity(type_count as usize);
    let mut trees: HashMap<i32, &[u8]> = HashMap::new();
    for _ in 0..type_count {
        let class = r.u32()?;
        classes.push(class as i32);
        let flags = req(
            r.take(3),
            |flags| flags[0] == 0,
            "stripped UI types are unsupported",
        )?;
        if le16(&flags[1..]) as i16 >= 0 || class == 114 || (class as i32) < 0 {
            r.take(16)?;
        }
        r.take(32)?;
        let size = req(r.u32(), |size| *size >= 16, "UI type tree is missing")?;
        let blob = req(
            r.take(size as usize),
            |blob| le32(&blob[..4]) == 0x7474686d && le32(&blob[4..8]) == 23,
            "UI type tree format is unsupported",
        )?;
        let (nodes, chars) = (le32(&blob[8..12]) as u64, le32(&blob[12..16]) as u64);
        if nodes == 0 || 16 + nodes * 32 + chars != blob.len() as u64 {
            bail!("UI type tree bounds are invalid");
        }
        trees.insert(class as i32, blob);
        let dependencies = req(
            r.u32(),
            |dependencies| *dependencies <= 1024,
            "UI type dependencies are invalid",
        )?;
        r.take(dependencies as usize * 4)?;
    }
    let count_offset = 48 + r.pos;
    let count = req(
        r.u32(),
        |count| *count != 0 && *count <= 1024,
        "UI object table is invalid",
    )?;
    r.align();
    let table_start = 48 + r.pos;
    let mut entries = Vec::with_capacity(count as usize);
    let mut ids = HashSet::new();
    for _ in 0..count {
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
            || offset > payload
            || size > payload - offset
            || size < 4
        {
            bail!("UI object identity or bounds are invalid");
        }
        ids.insert(id);
        entries.push((id, classes[type_index as usize], offset, size, row));
    }
    let table_end = 48 + r.pos;
    let scripts = req(
        r.u32(),
        |scripts| *scripts <= 256,
        "UI script references are invalid",
    )?;
    r.take(scripts as usize * 12)?;
    let externals = req(
        r.u32(),
        |externals| *externals <= 256,
        "UI external reference table is invalid",
    )?;
    for _ in 0..externals {
        r.cstring_bytes(256)?;
        let guid = r.take(16)?;
        let kind = req(
            r.u32(),
            |kind| *kind <= 3,
            "UI external reference type is invalid",
        )?;
        let path = r.cstring(4096)?;
        result.external_references.push(ExternalReference {
            guid: ordinary_guid(guid),
            kind: kind as i32,
            path,
        });
    }
    // Managed UXML serialized-data type trees and their reference table remain
    // intact. They are interpreted by the native Unity serializer, not recreated.
    let mut retained = Vec::new();
    for &(entry_id, class_id, offset, size, row) in &entries {
        let start = data_offset + offset as usize;
        let data = &input[start..start + size as usize];
        let id = entry_id.to_string();
        if class_id == 2089858483
            || class_id == 1001480554
            || font && class_id == 1042
            || texture && class_id == 1006
        {
            result.removed.push(RemovedObject {
                file_id: id,
                class_id,
                reason: "editor-only".into(),
            });
            continue;
        }
        if le32(&data[..4]) & 16 != 0 {
            result.removed.push(RemovedObject {
                file_id: id,
                class_id,
                reason: "dont-save-in-build".into(),
            });
            continue;
        }
        let mut type_name = "";
        if class_id == 114 && (profile == "uxml" || profile == "uss") {
            if data.len() < 80 || le32(&data[60..64]) != 1 {
                bail!("UI object has no verified builtin MonoScript reference");
            }
            let script = le64(&data[64..72]);
            if script == 11998 {
                type_name = "UnityEngine.UIElements.StyleSheet";
            } else if script == 11995 && profile == "uxml" {
                type_name = "UnityEngine.UIElements.VisualTreeAsset";
            } else {
                bail!("unsupported UI builtin MonoScript {script}");
            }
            let mut object = Reader::at(data, 72);
            object.string_bytes()?;
            let expected = format!("UnityEngine.dll::{type_name}");
            req(
                object.string_bytes(),
                |identifier| *identifier == expected.as_bytes(),
                "UI serialized class identifier differs from builtin script identity",
            )?;
        } else if texture && class_id == 28 && entry_id == 2800000 {
            type_name = "UnityEngine.Texture2D";
            let (resources, metadata) =
                read_texture_resources(data, trees.get(&28).copied().unwrap_or_default())?;
            result.resources.extend(resources);
            result.texture = Some(metadata);
        } else if font {
            type_name = match class_id {
                128 => "UnityEngine.Font",
                21 => "UnityEngine.Material",
                28 => "UnityEngine.Texture2D",
                _ => "",
            };
        }
        if type_name.is_empty() {
            bail!("native class {class_id} is unsupported in UI profile {profile}");
        }
        result.objects.push(Object {
            file_id: id,
            class_id,
            type_name: type_name.into(),
            sha256: crate::sha256(data),
        });
        retained.extend_from_slice(row);
    }
    if result.objects.is_empty() {
        bail!("UI artifact has no usable objects");
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
    result.profile = profile.to_string();
    (
        result.serialized_version,
        result.source_target,
        result.runtime_target,
    ) = (23, -2, 24);
    result.input_sha256 = crate::sha256(input);
    result.output_sha256 = crate::sha256(&output);
    Ok((output, result))
}
