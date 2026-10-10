//! Retained engine shader snapshots: every provenance link is recomputed from
//! the archived bytes before a Session may publish the projection.
use super::support::{os_error, read_file, read_json, stat, wrap};
use crate::{
    project::{RetainedBuiltinShader, Session},
    runtimeartifact, sha256, sysutil,
};
use anyhow::{Result, bail};
use std::fs::File;

fn builtin_shader_file(path: &str, limit: i64) -> Result<Vec<u8>> {
    let info = stat(path)?;
    if !info.is_regular() || info.size > limit {
        bail!("built-in Shader evidence file is oversized or not regular");
    }
    read_file(path)
}

pub fn verify_builtin_shader_snapshot(snapshot: &RetainedBuiltinShader) -> Result<()> {
    snapshot.validate("6000.6.3f1")?;
    let member = builtin_shader_file(&snapshot.member_path, 64 << 20)?;
    if sha256(&member) != snapshot.member_sha256 {
        bail!("compiled built-in Shader member changed");
    }
    let bundle = builtin_shader_file(&snapshot.bundle_path, 128 << 20)?;
    if sha256(&bundle) != snapshot.bundle_sha256 {
        bail!("compiled built-in Shader archive changed");
    }
    runtimeartifact::verify_unityfs_member(&bundle, &snapshot.member_name, &member)?;
    let (projected, proof) = runtimeartifact::extract_builtin_player_shader(&member, 108)?;
    if proof.guid != snapshot.source_guid
        || proof.source_file_id != snapshot.source_file_id
        || proof.archive_file_id != snapshot.archive_file_id
        || proof.name != snapshot.shader_name
        || proof.payload_sha256 != snapshot.payload_sha256
        || proof.type_tree_sha256 != snapshot.type_tree_sha256
        || proof.output_sha256 != snapshot.content.sha256
    {
        bail!("compiled built-in Shader provenance does not match its exact native payload");
    }
    let actual = builtin_shader_file(&snapshot.content.path, 64 << 20)?;
    if actual != projected {
        bail!("retained built-in Shader differs from its verified compiled source");
    }
    let path = &snapshot.builtin_source_path;
    let engine = File::open(path).map_err(|e| os_error("open", path, &e))?;
    let size = engine
        .metadata()
        .map_err(|e| os_error("stat", path, &e))?
        .len() as i64;
    let identity = runtimeartifact::inspect_builtin_shader(&engine, size, 108)?;
    if identity.unity_version != snapshot.unity_version
        || identity.name != snapshot.shader_name
        || identity.payload_sha256 != snapshot.builtin_source_sha256
    {
        bail!("native engine built-in Shader identity changed");
    }
    Ok(())
}

/// Applies the runner's `--builtin-shader` values.
pub fn apply_builtin_shaders(session: &Session, builtin_shaders: &[String]) -> Result<()> {
    if builtin_shaders.len() > 1 {
        bail!("only one verified built-in Shader snapshot is supported");
    }
    for path in builtin_shaders {
        if !sysutil::is_abs(path) {
            bail!("--builtin-shader requires an absolute immutable snapshot path");
        }
        let snapshot: RetainedBuiltinShader = read_json(path)?;
        verify_builtin_shader_snapshot(&snapshot)
            .map_err(|e| wrap("built-in Shader snapshot", e))?;
        session.retain_builtin_shader(&snapshot)?;
    }
    Ok(())
}
