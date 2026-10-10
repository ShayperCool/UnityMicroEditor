//! The frozen inventory of a preparation's
//! generated output, separate from mutable authored source. Validation is
//! read-only and runs before a cached preparation can stop an accepted Player.
use super::{error_text, join, nz, read_json, regular};
use crate::{
    cancel::Context,
    compiler::{
        CompileAssembly, CompileManifest, NilMap, Walk, WalkEntry, compiler_manifest,
        read_assembly_configuration, walk_dir,
    },
    project::is_owned_cache_path,
    runner::{PreparedScene, resolve_prepared_source},
    sysutil,
    util::{agent_canonical, agent_immutable_json, file_hash, read_agent_file, valid_sha256},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreparationArtifacts {
    #[serde(deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub project: String,
    #[serde(deserialize_with = "nz")]
    pub baseline: String,
    #[serde(deserialize_with = "nz")]
    pub player_root: String,
    pub files: NilMap<String>,
}

/// Hashes everything the baseline names
/// and publishes the receipt at `output`.
pub fn capture_preparation_artifacts(
    ctx: &Context,
    project: &str,
    baseline: &str,
    output: &str,
) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct NativeImport {
        #[serde(deserialize_with = "nz")]
        catalog: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Descriptor {
        #[serde(deserialize_with = "nz")]
        player: String,
        #[serde(deserialize_with = "nz")]
        manifest: String,
        #[serde(deserialize_with = "nz")]
        compiler: CompileManifest,
        #[serde(deserialize_with = "nz")]
        native_import: NativeImport,
    }
    let descriptor: Descriptor = read_json(baseline, 32 << 20)?;
    let mut receipt = PreparationArtifacts {
        schema_version: 1,
        project: project.into(),
        baseline: baseline.into(),
        player_root: sysutil::dir(&descriptor.player),
        files: NilMap::make(),
    };
    let mut add = |path: &str, expected: &str| -> Result<()> {
        ctx.err()?;
        if !sysutil::is_abs(path) || !is_owned_cache_path(project, path) {
            return Err(anyhow!(
                "preparation output is outside owned caches: {path}"
            ));
        }
        agent_canonical(path)?;
        if !regular(path) {
            return Err(anyhow!("preparation output is not a regular file: {path}"));
        }
        let hash = file_hash(path)?;
        if !expected.is_empty() && hash != expected {
            return Err(anyhow!(
                "preparation output differs from its declared hash: {path}"
            ));
        }
        let old = receipt.files.at(path);
        if !old.is_empty() && old != hash {
            return Err(anyhow!(
                "preparation output changed during inventory: {path}"
            ));
        }
        receipt.files.insert(path, hash);
        if receipt.files.len() > 8192 {
            return Err(anyhow!("preparation output inventory exceeds 8192 files"));
        }
        Ok(())
    };
    add(baseline, "")?;
    add(&descriptor.manifest, "")?;
    if !descriptor.native_import.catalog.is_empty() {
        add(&descriptor.native_import.catalog, "")?;
    }
    let player_root = sysutil::dir(&descriptor.player);
    walk_dir(&player_root, &mut |entry: &WalkEntry| {
        if entry.kind.is_symlink() {
            return Err(anyhow!(
                "preparation output contains a symlink: {}",
                entry.path
            ));
        }
        if !entry.is_dir() {
            add(entry.path, "")?;
        }
        Ok(Walk::Continue)
    })?;
    for assembly in descriptor.compiler.assemblies.iter() {
        add(&assembly.path, &assembly.sha256)?;
        if !assembly.assembly_input_hash.is_empty()
            || descriptor.compiler.backend == "authored_source"
        {
            let (accepted, locator) = compiler_manifest(baseline)?;
            let configuration = preparation_assembly_descriptor(&locator, assembly, &accepted)?;
            add(&configuration, "")?;
        }
    }
    read_agent_file(&descriptor.manifest, 32 << 20)?;
    let manifest = PreparedScene::read(&descriptor.manifest)?;
    for pack in &manifest.imported_packs {
        add(&resolve_prepared_source(project, &pack.path)?, &pack.sha256)?;
    }
    for content in &manifest.native_content {
        add(
            &resolve_prepared_source(project, &content.path)?,
            &content.sha256,
        )?;
        for resource in &content.resources {
            add(
                &resolve_prepared_source(project, &resource.path)?,
                &resource.sha256,
            )?;
        }
    }
    agent_immutable_json(output, &receipt)?;
    Ok(output.into())
}

/// Uses the same exact digest/ancestor
/// locations as `read_assembly_configuration`. This never scans sibling output
/// trees or accepts a nearby descriptor.
pub(crate) fn preparation_assembly_descriptor(
    baseline: &str,
    assembly: &CompileAssembly,
    manifest: &CompileManifest,
) -> Result<String> {
    read_assembly_configuration(baseline, assembly, Some(manifest))?;
    let mut starts = vec![sysutil::dir(baseline)];
    if sysutil::is_abs(&assembly.path) && sysutil::clean(&assembly.path) == assembly.path {
        starts.push(sysutil::dir(&assembly.path));
    }
    let mut visited = HashSet::new();
    for start in starts {
        let mut folder = start;
        loop {
            if !visited.insert(folder.clone()) {
                break;
            }
            for relative in [
                "compiler-cache/assemblies",
                "compiler-cache/baseline-assemblies",
                "baseline-assemblies",
            ] {
                let path = join(&[
                    &folder,
                    relative,
                    &assembly.assembly_input_hash,
                    "compile.json",
                ]);
                match sysutil::lstat(&path) {
                    Ok(info) => {
                        if !info.is_regular() {
                            return Err(anyhow!("assembly descriptor is not a regular file"));
                        }
                        agent_canonical(&path)?;
                        return Ok(path);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(sysutil::Error::os("lstat", &path, &error).into()),
                }
            }
            let parent = sysutil::dir(&folder);
            if parent == folder {
                break;
            }
            folder = parent;
        }
    }
    Err(anyhow!(
        "exact assembly configuration cache is missing: {}",
        assembly.name
    ))
}

/// Every recorded file still has its
/// digest and the Player directory holds nothing unrecorded.
pub fn validate_preparation_artifacts(
    project: &str,
    baseline: &str,
    player: &str,
    path: &str,
) -> Result<()> {
    if !is_owned_cache_path(project, path) {
        return Err(anyhow!(
            "preparation artifact receipt is outside owned caches"
        ));
    }
    let receipt: PreparationArtifacts = read_json(path, 4 << 20)?;
    if receipt.schema_version != 1
        || receipt.project != project
        || receipt.baseline != baseline
        || receipt.player_root != sysutil::dir(player)
        || receipt.files.len() < 3
        || receipt.files.len() > 8192
        || receipt.files.at(baseline).is_empty()
        || receipt.files.at(player).is_empty()
    {
        return Err(anyhow!(
            "preparation artifact receipt identity/inventory is invalid"
        ));
    }
    // Visited in path order.
    for (file, expected) in &receipt.files {
        if !is_owned_cache_path(project, file) {
            return Err(anyhow!("preparation artifact is outside owned caches"));
        }
        agent_canonical(file)?;
        if !regular(file) {
            return Err(anyhow!(
                "cached preparation artifact is not a regular file: {file}"
            ));
        }
        match file_hash(file) {
            Ok(actual) if valid_sha256(expected) && actual == *expected => {}
            Ok(_) => {
                return Err(anyhow!("cached preparation artifact changed: {file}"));
            }
            Err(error) => {
                return Err(anyhow!(
                    "cached preparation artifact changed: {file}\n{}",
                    error_text(&error)
                ));
            }
        }
    }
    walk_dir(&receipt.player_root, &mut |entry: &WalkEntry| {
        if entry.kind.is_symlink() {
            return Err(anyhow!("cached preparation Player contains a symlink"));
        }
        if !entry.is_dir() && receipt.files.at(entry.path).is_empty() {
            return Err(anyhow!(
                "cached preparation Player contains an unrecorded file: {}",
                entry.path
            ));
        }
        Ok(Walk::Continue)
    })
}
