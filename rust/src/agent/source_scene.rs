//! The source/native-resource scene proof. It never
//! claims an Editor opened the target scene, nor fabricates public Editor
//! observations.
use super::{UNITY_VERSION, join, join_errors, nz, read_json, scene::builtin_lighting};
use crate::{
    cancel::Context,
    jsondoc::{self, Json},
    project::{is_owned_cache_path, parse_unity_yaml, value},
    runtimeartifact::{BuiltinLightingIdentity, inspect_builtin_lighting},
    sha256, sysutil,
    util::{agent_canonical, agent_immutable_json, file_hash, read_agent_file},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs::File;

fn is_empty(text: &str) -> bool {
    text.is_empty()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSceneProof {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub backend: String,
    #[serde(rename = "projectRoot", deserialize_with = "nz")]
    pub project_root: String,
    #[serde(rename = "unityVersion", deserialize_with = "nz")]
    pub unity_version: String,
    #[serde(rename = "scenePath", deserialize_with = "nz")]
    pub scene_path: String,
    #[serde(rename = "sceneSHA256", deserialize_with = "nz")]
    pub scene_sha256: String,
    #[serde(rename = "sceneMetaSha256", deserialize_with = "nz")]
    pub scene_meta_sha256: String,
    #[serde(rename = "builtinLighting", skip_serializing_if = "Option::is_none")]
    pub builtin_lighting: Option<BuiltinLightingIdentity>,
    #[serde(
        rename = "resourcePath",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub resource_path: String,
    #[serde(
        rename = "resourceSha256",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub resource_sha256: String,
}

/// Whether the scene needs the built-in lighting
/// resource; every other baked-data reference is refused.
pub(crate) fn inspect_source_scene(contents: &[u8]) -> Result<bool> {
    let documents = parse_unity_yaml(contents)?;
    let mut needs_builtin = false;
    for document in &documents {
        for property in [
            "m_LightingDataAsset",
            "m_LightingSettings",
            "m_OcclusionCullingData",
            "m_NavMeshData",
        ] {
            let Some(found) = document.fields().get(property) else {
                continue;
            };
            let reference = match found.as_object() {
                Some(reference) if !value::get(reference, "fileID").is_null() => reference,
                _ => {
                    return Err(anyhow!(
                        "scene baked-data property is not an exact authored PPtr: {property}"
                    ));
                }
            };
            if value::sprint(value::get(reference, "fileID")) == "0" {
                continue;
            }
            if builtin_lighting(document, property, reference) {
                needs_builtin = true;
                continue;
            }
            return Err(anyhow!(
                "raw source preparation does not support retained baked data: {property}"
            ));
        }
        if let Some(found) = document.fields().get("m_Lightmaps")
            && !found.as_array().is_some_and(Vec::is_empty)
        {
            return Err(anyhow!(
                "raw source preparation requires an empty authored lightmap array"
            ));
        }
    }
    Ok(needs_builtin)
}

fn inspect_resource(path: &str) -> Result<BuiltinLightingIdentity> {
    let file = File::open(path).map_err(|error| sysutil::Error::os("open", path, &error))?;
    let info = file
        .metadata()
        .ok()
        .filter(|info| info.is_file())
        .ok_or_else(|| anyhow!("builtin lighting resource is not a regular native file"))?;
    inspect_builtin_lighting(&file, info.len() as i64)
}

/// Writes the proof to `output` unless it is empty.
pub fn build_source_scene_proof(
    ctx: &Context,
    project: &str,
    editor: &str,
    scene: &str,
    output: &str,
) -> Result<SourceSceneProof> {
    let mut proof = SourceSceneProof {
        schema_version: 2,
        backend: "source_native_resource".into(),
        project_root: project.into(),
        unity_version: UNITY_VERSION.into(),
        scene_path: scene.into(),
        ..Default::default()
    };
    let path = join(&[project, scene]);
    agent_canonical(&path)?;
    let contents = read_agent_file(&path, 32 << 20)?;
    let needs_builtin = inspect_source_scene(&contents)?;
    proof.scene_sha256 = sha256(&contents);
    let meta = format!("{path}.meta");
    proof.scene_meta_sha256 = file_hash(&meta)?;
    if needs_builtin {
        ctx.err()?;
        proof.resource_path = join(&[&sysutil::dir(editor), "Data/Resources/unity_builtin_extra"]);
        agent_canonical(&proof.resource_path)?;
        proof.builtin_lighting = Some(inspect_resource(&proof.resource_path)?);
        proof.resource_sha256 = file_hash(&proof.resource_path)?;
    }
    if !file_hash(&path).is_ok_and(|current| current == proof.scene_sha256) {
        return Err(anyhow!(
            "scene changed during source/native lighting validation"
        ));
    }
    if !file_hash(&meta).is_ok_and(|current| current == proof.scene_meta_sha256) {
        return Err(anyhow!(
            "scene metadata changed during source/native lighting validation"
        ));
    }
    if !output.is_empty() {
        if !is_owned_cache_path(project, output) {
            return Err(anyhow!(
                "source scene proof output is outside owned project caches"
            ));
        }
        agent_immutable_json(output, &proof)?;
    }
    Ok(proof)
}

pub(crate) fn validate_source_scene_proof(
    contents: &[u8],
    project: &str,
    scene: &str,
    path: &str,
) -> Result<()> {
    let proof: SourceSceneProof = read_json(path, 1 << 20)?;
    if proof.schema_version != 2
        || proof.backend != "source_native_resource"
        || proof.project_root != project
        || proof.unity_version != UNITY_VERSION
        || proof.scene_path != scene
        || proof.scene_sha256 != sha256(contents)
    {
        return Err(anyhow!(
            "source scene proof differs from its exact authored scene/project/version"
        ));
    }
    let needs_builtin = inspect_source_scene(contents)?;
    let meta = format!("{}.meta", join(&[project, scene]));
    if !file_hash(&meta).is_ok_and(|current| current == proof.scene_meta_sha256) {
        return Err(anyhow!("source scene proof metadata identity changed"));
    }
    if !needs_builtin {
        if proof.builtin_lighting.is_some()
            || !proof.resource_path.is_empty()
            || !proof.resource_sha256.is_empty()
        {
            return Err(anyhow!(
                "source scene proof claims unused native lighting evidence"
            ));
        }
        return Ok(());
    }
    let Some(expected) = proof
        .builtin_lighting
        .as_ref()
        .filter(|_| !proof.resource_sha256.is_empty() && sysutil::is_abs(&proof.resource_path))
    else {
        return Err(anyhow!(
            "source scene proof lacks its exact native lighting resource"
        ));
    };
    agent_canonical(&proof.resource_path)?;
    if !file_hash(&proof.resource_path).is_ok_and(|current| current == proof.resource_sha256) {
        return Err(anyhow!("source scene proof native resource changed"));
    }
    let file = File::open(&proof.resource_path)
        .map_err(|error| sysutil::Error::os("open", &proof.resource_path, &error))?;
    let size = file.metadata()?.len() as i64;
    match inspect_builtin_lighting(&file, size) {
        Ok(actual) if actual == *expected => Ok(()),
        Ok(_) => Err(anyhow!(
            "source scene proof native lighting identity differs"
        )),
        Err(error) => Err(join_errors(
            "source scene proof native lighting identity differs",
            Some(&error),
        )),
    }
}

/// The schema and backend a proof file declares.
pub(crate) fn scene_proof_backend(path: &str) -> Result<(i64, String)> {
    let data = read_agent_file(path, 1 << 20)?;
    let (mut schema, mut backend) = (0i64, String::new());
    let at = |field| jsondoc::Target { strukt: "", field };
    match jsondoc::parse(&data).map_err(|error| anyhow!(error))? {
        Json::Null => {}
        Json::Object(members) => {
            jsondoc::decode_struct(&members, &["schemaVersion", "backend"], |field, member| {
                match field {
                    0 => jsondoc::set_int(&mut schema, &member.value, at("schemaVersion")),
                    _ => jsondoc::set_string(&mut backend, &member.value, at("backend")),
                }
            })
            .map_err(|error| anyhow!(error))?
        }
        other => {
            return Err(anyhow!(
                jsondoc::Target::VALUE.mismatch(other.kind(), "object")
            ));
        }
    }
    Ok((schema, backend))
}
