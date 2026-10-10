//! Verifies the archived source and deterministic projection when a compiled
//! Shader cache entry is replayed. The Session then checks the authored source,
//! include closure, environment and physical runtime artifact independently.
use super::support::{os_error, read_limited};
use crate::{project::NativeReplacement, runtimeartifact, sha256, sysutil, util::valid_sha256};
use anyhow::{Result, bail};
use std::fs::File;

pub fn verify_compiled_shader_cache(entry: &NativeReplacement) -> Result<()> {
    let Some(p) = &entry.compiled_shader_snapshot else {
        return Ok(());
    };
    if p.schema_version != 1
        || p.compiled_source_revision_verified
        || p.source_path != entry.source_path
        || p.member_name.is_empty()
        || !sysutil::is_abs(&p.bundle_path)
        || !sysutil::is_abs(&p.member_path)
        || !valid_sha256(&p.bundle_sha256)
        || !valid_sha256(&p.member_sha256)
    {
        bail!("compiled Shader cache provenance profile is invalid");
    }
    let archive = bounded_shader_cache_file(&p.bundle_path, 128 << 20)?;
    let member = bounded_shader_cache_file(&p.member_path, 64 << 20)?;
    if sha256(&archive) != p.bundle_sha256 || sha256(&member) != p.member_sha256 {
        bail!("compiled Shader cache archive/member digest differs");
    }
    runtimeartifact::verify_unityfs_member(&archive, &p.member_name, &member)?;
    let (_, proof) = runtimeartifact::extract_player_shader(&member, &entry.guid, &p.shader_name)?;
    if proof.archive_file_id != p.archive_file_id
        || proof.source_file_id != "4800000"
        || proof.payload_sha256 != p.payload_sha256
        || proof.type_tree_sha256 != p.type_tree_sha256
        || proof.output_sha256 != entry.content.sha256
    {
        bail!("compiled Shader cache projection proof differs from the archived object");
    }
    let content = &entry.content;
    if content.guid != entry.guid
        || content.objects.len() != 1
        || content.objects[0].file_id != proof.source_file_id
        || content.objects[0].type_name != "UnityEngine.Shader"
        || !content.resources.is_empty()
        || !content.dependencies.is_empty()
    {
        bail!("compiled Shader cache runtime identity differs from the archived object");
    }
    Ok(())
}

fn bounded_shader_cache_file(path: &str, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| os_error("open", path, &e))?;
    let data = read_limited(file, limit + 1, path)?;
    if data.len() as u64 > limit {
        bail!("compiled Shader cache input exceeds supported size: {path}");
    }
    Ok(data)
}
