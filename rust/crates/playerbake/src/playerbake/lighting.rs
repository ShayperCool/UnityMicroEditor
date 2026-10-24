//! A scene's baked lighting. The Editor keeps what a bake made in the
//! scene's `LightingData.asset`, a binary file of Editor objects: the
//! `LightingDataAsset` and the `LightProbes` it names. A build hands the
//! scene its part of it: `LightmapSettings` names the probes and lists the
//! lightmaps, each renderer takes its lightmap and the place in it, each
//! light its baking output and each reflection probe its baked cubemap. The
//! probes are an object of the closure, as the `LightProbes` the asset holds
//! laid out for the Player.
use super::{
    animation::{Animation, Prebuilt},
    authored::Sources,
    serialized,
    typetree::Val,
};
use crate::yaml::Document;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::Path;

const LIGHTMAP_SETTINGS: i64 = 157;
const LIGHTING_DATA: i32 = 1120;
const LIGHT_PROBES: i32 = 258;
const LIGHT: i64 = 108;
const REFLECTION_PROBE: i64 = 215;

/// Whether a file is a binary serialized file rather than text.
fn binary(data: &[u8]) -> bool {
    data.len() > 48 && !data.starts_with(b"%YAML") && data[..8] == [0; 8]
}

/// The probes of a lighting data source, as an object of the closure.
pub(super) fn load(path: &Path) -> Result<Option<Animation>> {
    if path.extension().is_none_or(|e| e != "asset") {
        return Ok(None);
    }
    let data = std::fs::read(path)?;
    if !binary(&data) {
        return Ok(None);
    }
    let objects = serialized::read(&data)?;
    if !objects.iter().any(|o| o.tree.class_id == LIGHTING_DATA) {
        return Ok(None);
    }
    let mut animation = Animation {
        documents: Vec::new(),
        prebuilt: Vec::new(),
    };
    for object in objects.iter().filter(|o| o.tree.class_id == LIGHT_PROBES) {
        animation.documents.push(Document {
            class_id: LIGHT_PROBES as i64,
            file_id: object.local_id.to_string(),
            type_name: "LightProbes".into(),
            data: Map::new(),
            stripped: false,
        });
        animation.prebuilt.push((
            object.local_id,
            Prebuilt {
                value: object.decode()?,
                externals: Vec::new(),
            },
        ));
    }
    Ok(Some(animation))
}

/// The text of a scalar as the Editor writes it.
fn scalar(value: &Val) -> Value {
    Value::String(match value {
        Val::Bool(flag) => (*flag as u8).to_string(),
        Val::I(number) => number.to_string(),
        Val::U(number) => number.to_string(),
        Val::F(number) => number.to_string(),
        Val::D(number) => number.to_string(),
        Val::Str(text) => text.clone(),
        _ => String::new(),
    })
}

/// A value of the asset as a source document holds it; a pointer leads into
/// the asset itself or into the file of its external GUID.
fn authored(value: &Val, own: &str, guids: &[String]) -> Value {
    match value {
        Val::Map(fields) => {
            if let (Some(Val::I(file)), Some(Val::I(path))) =
                (value.field("m_FileID"), value.field("m_PathID"))
                && fields.len() == 2
            {
                if *path == 0 {
                    return serde_json::json!({ "fileID": "0" });
                }
                let guid = match *file {
                    0 => own.to_string(),
                    n => guids.get(n as usize - 1).cloned().unwrap_or_default(),
                };
                return serde_json::json!({
                    "fileID": path.to_string(), "guid": guid, "type": "2"
                });
            }
            Value::Object(
                fields
                    .iter()
                    .map(|(name, field)| (name.to_string(), authored(field, own, guids)))
                    .collect(),
            )
        }
        Val::Seq(items) => Value::Array(items.iter().map(|i| authored(i, own, guids)).collect()),
        other => scalar(other),
    }
}

fn number(value: Option<&Val>) -> i64 {
    match value {
        Some(Val::I(number)) => *number,
        Some(Val::U(number)) => *number as i64,
        _ => 0,
    }
}

/// The file ID a scene object has once its prefab instances are unpacked:
/// an object of an instance takes the ID of the instance and of the object
/// in its prefab together.
fn scene_object(identifier: &Val) -> i64 {
    let object = number(identifier.field("targetObject"));
    match number(identifier.field("targetPrefab")) {
        0 => object,
        instance => (instance ^ object) & 0x7fff_ffff_ffff_ffff,
    }
}

/// Hands a scene, its prefab instances unpacked, what its lighting data
/// holds for it.
pub(super) fn settle_scene(documents: &mut [Document], sources: &Sources) -> Result<()> {
    let Some(settings) = documents
        .iter()
        .position(|d| d.class_id == LIGHTMAP_SETTINGS && !d.stripped)
    else {
        return Ok(());
    };
    let Some(reference) = documents[settings]
        .data
        .get("m_LightingDataAsset")
        .and_then(super::Reference::from_value)
    else {
        return Ok(());
    };
    let Some(path) = sources.path(&reference.guid) else {
        return Ok(());
    };
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if !binary(&data) {
        return Ok(());
    }
    let guids = serialized::external_guids(&data)?;
    let objects = serialized::read(&data)?;
    let Some(asset) = objects
        .iter()
        .find(|o| o.tree.class_id == LIGHTING_DATA)
        .map(|o| o.decode())
        .transpose()?
    else {
        return Ok(());
    };
    let own = reference.guid.as_str();
    let field = |name: &str| asset.field(name).unwrap_or(&Val::Zero);
    let fields = &mut documents[settings].data;
    fields.insert(
        "m_LightProbes".into(),
        authored(field("m_LightProbes"), own, &guids),
    );
    fields.insert(
        "m_Lightmaps".into(),
        authored(field("m_Lightmaps"), own, &guids),
    );
    fields.insert("m_LightmapsMode".into(), scalar(field("m_LightmapsMode")));
    fields.insert(
        "m_EnlightenSceneMapping".into(),
        authored(field("m_EnlightenSceneMapping"), own, &guids),
    );

    let positions: std::collections::HashMap<String, usize> = documents
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.stripped)
        .map(|(position, d)| (d.file_id.clone(), position))
        .collect();
    let items = |name: &str| match asset.field(name) {
        Some(Val::Seq(items)) => items.as_slice(),
        _ => &[],
    };
    // Each renderer its lightmap and its place in it.
    for (identifier, baked) in items("m_LightmappedRendererDataIDs")
        .iter()
        .zip(items("m_LightmappedRendererData"))
    {
        let Some(&position) = positions.get(&scene_object(identifier).to_string()) else {
            continue;
        };
        let renderer = &mut documents[position].data;
        for (from, to) in [
            ("lightmapIndex", "m_LightmapIndex"),
            ("lightmapIndexDynamic", "m_LightmapIndexDynamic"),
            ("lightmapST", "m_LightmapTilingOffset"),
            ("lightmapSTDynamic", "m_LightmapTilingOffsetDynamic"),
        ] {
            if let Some(value) = baked.field(from) {
                renderer.insert(to.into(), authored(value, own, &guids));
            }
        }
    }
    // Each light what its bake made of it.
    for (identifier, output) in items("m_Lights").iter().zip(items("m_LightBakingOutputs")) {
        if let Some(&position) = positions.get(&scene_object(identifier).to_string())
            && documents[position].class_id == LIGHT
        {
            documents[position]
                .data
                .insert("m_BakingOutput".into(), authored(output, own, &guids));
        }
    }
    // Each reflection probe its baked cubemap.
    for (identifier, cubemap) in items("m_BakedReflectionProbes")
        .iter()
        .zip(items("m_BakedReflectionProbeCubemaps"))
    {
        if let Some(&position) = positions.get(&scene_object(identifier).to_string())
            && documents[position].class_id == REFLECTION_PROBE
        {
            documents[position]
                .data
                .insert("m_BakedTexture".into(), authored(cubemap, own, &guids));
        }
    }
    Ok(())
}
