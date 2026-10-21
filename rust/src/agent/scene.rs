//! Lighting validation of an authored scene and the baseline publication.
use super::{nz, unity_version};
use crate::{
    project::{BUILTIN_RESOURCE_GUID, Document, parse_unity_yaml, value},
    sha256,
    util::read_json_file,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

const BAKED: [&str; 4] = [
    "m_LightingDataAsset",
    "m_LightingSettings",
    "m_OcclusionCullingData",
    "m_NavMeshData",
];

/// True for the reference to Unity's built-in empty LightingData asset.
pub(crate) fn builtin_lighting(
    document: &Document,
    property: &str,
    reference: &value::Map,
) -> bool {
    document.class_id == 157
        && property == "m_LightingDataAsset"
        && value::sprint(value::get(reference, "fileID")) == "20201"
        && *value::get(reference, "guid") == value::Value::from(BUILTIN_RESOURCE_GUID)
}

/// Built-in LightingData is accepted only with
/// public Editor observations bound to the exact authored scene. A GUID alone
/// does not prove the asset is empty.
pub fn validate_scene_lighting(
    contents: &[u8],
    project: &str,
    scene: &str,
    proof_path: &str,
) -> Result<()> {
    let documents = parse_unity_yaml(contents)?;
    let mut needs_proof = false;
    for document in &documents {
        for property in BAKED {
            if let Some(reference) = value::get(document.fields(), property).as_object()
                && value::sprint(value::get(reference, "fileID")) != "0"
            {
                if builtin_lighting(document, property, reference) {
                    needs_proof = true;
                    continue;
                }
                return Err(anyhow!(
                    "first raw preparation does not support retained baked data: {property}"
                ));
            }
        }
        if value::get(document.fields(), "m_Lightmaps")
            .as_array()
            .is_some_and(|lightmaps| !lightmaps.is_empty())
        {
            return Err(anyhow!(
                "first raw preparation does not support baked lightmaps"
            ));
        }
    }
    if !needs_proof {
        return Ok(());
    }
    if proof_path.is_empty() {
        return Err(anyhow!(
            "built-in LightingData requires --scene-proof exported through public Editor APIs"
        ));
    }
    let (schema, backend) = super::source_scene::scene_proof_backend(proof_path)?;
    if schema == 2 && backend == "source_native_resource" {
        return super::source_scene::validate_source_scene_proof(
            contents, project, scene, proof_path,
        );
    }
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Proof {
        #[serde(deserialize_with = "nz")]
        schema_version: i64,
        #[serde(deserialize_with = "nz")]
        project_root: String,
        #[serde(deserialize_with = "nz")]
        unity_version: String,
        #[serde(deserialize_with = "nz")]
        scene_path: String,
        #[serde(rename = "sceneSHA256", deserialize_with = "nz")]
        scene_sha256: String,
        lightmap_count: Option<i64>,
        light_probe_count: Option<i64>,
        #[serde(deserialize_with = "nz")]
        active_scene_count: i64,
        is_playing: Option<bool>,
        scene_dirty: Option<bool>,
        #[serde(deserialize_with = "nz")]
        lighting_data_identity_known: bool,
        #[serde(deserialize_with = "nz")]
        lighting_data_guid: String,
        #[serde(deserialize_with = "nz")]
        lighting_data_file_id: i64,
    }
    let proof: Proof = read_json_file::<Option<Proof>>(proof_path)?.unwrap_or_default();
    if proof.schema_version != 1
        || proof.project_root != project
        || proof.unity_version != unity_version(project)?
        || proof.scene_path != scene
        || proof.scene_sha256 != sha256(contents)
        || proof.lightmap_count != Some(0)
        || proof.light_probe_count != Some(0)
        || proof.active_scene_count != 1
        || proof.is_playing != Some(false)
        || proof.scene_dirty != Some(false)
        || !proof.lighting_data_identity_known
        || proof.lighting_data_guid != BUILTIN_RESOURCE_GUID
        || proof.lighting_data_file_id != 20201
    {
        return Err(anyhow!(
            "scene lighting proof must match the exact saved scene, pinned Unity and project, with no baked lightmaps or probes"
        ));
    }
    Ok(())
}

/// Publishes without replacing a concurrent
/// preparation or an existing baseline. This is the publication the compiler
/// and source-graph preparations use as well.
pub fn publish_agent_baseline<T: Serialize + ?Sized>(path: &str, value: &T) -> Result<()> {
    crate::compiler::publish_baseline(path, value)
}
