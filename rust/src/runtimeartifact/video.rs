//! The artifact of a video: one `VideoClip` whose frames lie in a resource.
//!
//! On Linux the engine reads VP8 in a WebM container and nothing else, in the
//! Editor and in a Player alike. Imported without transcoding, the clip names
//! no resource: its frames are the source file itself, which a build copies
//! next to the clip. The projection does the same by naming the resource the
//! runner mounts for the source's GUID.
use super::adapt::{Metadata, Object, RemovedObject, Resource};
use crate::playerbake::{self, Val};
use anyhow::{Result, bail};

const VIDEO_CLIP: i32 = 329;
/// `VideoClipImporter`, the importer object of the artifact.
const VIDEO_CLIP_IMPORTER: i32 = 1127;
/// The engine's number for VP8, the one format a Linux Player decodes.
const VP8: i64 = 4;

fn number(value: Option<&Val>) -> Option<i64> {
    match value? {
        Val::I(number) => Some(*number),
        Val::U(number) => Some(*number as i64),
        Val::Zero => Some(0),
        _ => None,
    }
}

/// Projects a video artifact. The flag tells whether the clip's resource is
/// the source file rather than a transcoded one of the artifact.
pub fn adapt_video(input: &[u8], guid: &str) -> Result<(Vec<u8>, Metadata, bool)> {
    if input.len() > super::MAXIMUM_BYTES {
        bail!("native runtime projection requires an artifact of at most 64 MiB");
    }
    let mut result = Metadata {
        profile: "video".into(),
        serialized_version: 23,
        source_target: -2,
        runtime_target: 24,
        ..Default::default()
    };
    let alias = format!("VirtualArtifacts/Primary/{guid}.resource");
    let mut clip = None;
    let mut from_source = false;
    let objects = playerbake::read_serialized(input)?;
    for object in &objects {
        let class_id = object.tree.class_id;
        let local_id = object.local_id.to_string();
        // 1001480554 is the Editor's record of the asset, 41386430 the import log.
        if matches!(class_id, VIDEO_CLIP_IMPORTER | 1001480554 | 41386430) {
            result.removed.push(RemovedObject {
                file_id: local_id,
                class_id,
                reason: "editor-only".into(),
            });
            continue;
        }
        if class_id != VIDEO_CLIP || clip.is_some() {
            bail!(
                "native class {class_id} is unsupported in video runtime profile; on Linux the engine imports only VP8 video in a .webm file"
            );
        }
        let mut value = object.tree.decode(&object.data)?;
        if number(value.field("m_Format")) != Some(VP8)
            || number(value.field("m_FrameCount")).is_none_or(|frames| frames <= 0)
        {
            bail!("a Linux Player plays only VP8 video with frames");
        }
        if !matches!(value.field("m_VideoShaders"), Some(Val::Seq(shaders)) if shaders.is_empty())
            && !matches!(value.field("m_VideoShaders"), Some(Val::Zero))
        {
            bail!("a video clip with its own shaders is unsupported");
        }
        let Some(resource) = value.field_mut("m_ExternalResources") else {
            bail!("video clip names no resource");
        };
        let (offset, size) = (
            number(resource.field("m_Offset")),
            number(resource.field("m_Size")),
        );
        let source = match resource.field("m_Source") {
            Some(Val::Str(source)) => source.clone(),
            Some(Val::Zero) => String::new(),
            _ => bail!("video clip names no resource"),
        };
        let (Some(0), Some(size)) = (offset, size.filter(|size| *size > 0)) else {
            bail!("video clip resource bounds are unsupported");
        };
        if source.is_empty() {
            from_source = true;
            match resource.field_mut("m_Source") {
                Some(source) => *source = Val::str(&alias),
                None => bail!("video clip names no resource"),
            }
        } else if source != alias {
            bail!("video resource path is outside the primary artifact namespace");
        }
        let data = object.tree.encode(&value)?;
        result.objects.push(Object {
            file_id: local_id,
            class_id,
            type_name: "UnityEngine.Video.VideoClip".into(),
            sha256: crate::sha256(&data),
        });
        result.resources.push(Resource {
            virtual_path: alias.clone(),
            offset: 0,
            size: size as u64,
        });
        clip = Some((object, data));
    }
    let Some((object, data)) = clip else {
        bail!(
            "the import made no video clip; on Linux the engine imports only VP8 video in a .webm file"
        );
    };
    let output = playerbake::write(
        &[playerbake::Object {
            local_id: object.local_id,
            tree: &object.tree,
            data,
            script: None,
        }],
        &[],
    )?;
    result.input_sha256 = crate::sha256(input);
    result.output_sha256 = crate::sha256(&output);
    Ok((output, result, from_source))
}
