use super::files::{atomic_json, canonical, ensure_cache, inside, join, parent, read_json};
use super::git::commit_pattern;
use super::{Context, Options, Package, Snapshot};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a resolution is identified by: every authored input and every selected package.
#[derive(Serialize)]
struct Identity<'a> {
    project: &'a str,
    editor: &'a str,
    manifest: &'a str,
    lock: &'a str,
    packages: &'a [Package],
    inputs: &'a BTreeMap<String, String>,
}
pub(crate) fn resolution_hash(snapshot: &Snapshot) -> Result<String> {
    Ok(crate::sha256(&serde_json::to_vec(&Identity {
        project: &snapshot.project,
        editor: &snapshot.editor,
        manifest: &snapshot.manifest_hash,
        lock: &snapshot.lock_hash,
        packages: &snapshot.packages,
        inputs: &snapshot.inputs,
    })?))
}
/// `current.json`: names the snapshot of the most recent successful resolution.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pointer {
    pub schema_version: i64,
    pub project: String,
    pub editor: String,
    pub resolution_hash: String,
    pub snapshot_path: String,
}
impl Pointer {
    pub(crate) fn publish(&self, cache: &str) -> Result<()> {
        atomic_json(&join(&[cache, "current.json"]), self)
    }
}

/// Reads the most recently resolved immutable snapshot, rechecking authored hashes and
/// package content without network access or an Editor call. If inputs changed, call
/// `resolve` again to obtain a new snapshot/proposed lock.
pub fn load_current(
    ctx: &Context,
    project: &str,
    editor: &str,
    options: Options,
) -> Result<Snapshot> {
    let project = canonical(project)?;
    let editor = if editor.is_empty() {
        String::new()
    } else {
        canonical(editor)?
    };
    let cache = ensure_cache(&project, &options.cache_root)?;
    let (pointer, _): (Pointer, _) = read_json(&join(&[&cache, "current.json"]))?;
    if pointer.schema_version != 1
        || pointer.project != project
        || pointer.editor != editor
        || !inside(&join(&[&cache, "snapshots"]), &pointer.snapshot_path)
    {
        bail!("current package snapshot identity/path mismatch");
    }
    if canonical(&pointer.snapshot_path)? != pointer.snapshot_path {
        bail!("package snapshot symlink unsupported");
    }
    let (snapshot, _): (Snapshot, _) = read_json(&pointer.snapshot_path)?;
    if snapshot.schema_version != 1
        || snapshot.project != project
        || snapshot.editor != editor
        || snapshot.resolution_hash != pointer.resolution_hash
        || snapshot.snapshot_path != pointer.snapshot_path
    {
        bail!("package snapshot identity mismatch");
    }
    if snapshot.packages.len() > 1024 {
        bail!("package snapshot exceeds package bound");
    }
    if resolution_hash(&snapshot)? != snapshot.resolution_hash {
        bail!("package snapshot content digest mismatch");
    }
    let project_cache = join(&[&project, "Library", "PackageCache"]);
    let content = join(&[&cache, "content"]);
    for p in &snapshot.packages {
        if canonical(&p.path)? != p.path {
            bail!("resolved package path changed");
        }
        match p.source.as_str() {
            "registry" => {
                if !inside(&project_cache, &p.path) && !inside(&content, &p.path) {
                    bail!("registry cache outside owned sources: {}", p.name);
                }
            }
            "git" => {
                let mut proof_root = &p.path;
                if p.proof == "verified_git_cache_copy" {
                    if !inside(&project_cache, &p.path)
                        || p.commit_content_hash.is_empty()
                        || !commit_pattern().is_match(&p.revision)
                    {
                        bail!("Git materialized copy outside explicit project cache");
                    }
                    proof_root = &p.commit_path;
                }
                if !inside(&content, proof_root) {
                    bail!("Git package outside verified content cache");
                }
            }
            "builtin" => {
                let base = parent(&editor);
                let mut engine_root = &p.path;
                if p.proof == "verified_engine_cache_copy" {
                    if !inside(&project_cache, &p.path) || p.profile_content_hash.is_empty() {
                        bail!("builtin materialized copy outside explicit project cache");
                    }
                    engine_root = &p.profile_path;
                }
                let data = join(&[
                    &base,
                    "Data",
                    "Resources",
                    "PackageManager",
                    "BuiltInPackages",
                ]);
                let plain = join(&[&base, "Resources", "PackageManager", "BuiltInPackages"]);
                if !inside(&data, engine_root) && !inside(&plain, engine_root) {
                    bail!("builtin package outside selected engine profile");
                }
            }
            "embedded" => {
                if !inside(&join(&[&project, "Packages"]), &p.path) {
                    bail!("embedded package outside project Packages");
                }
            }
            // Explicit local package roots are author-supplied inputs.
            "local" => {}
            other => bail!("unknown package source {other:?}"),
        }
    }
    snapshot.validate(ctx)?;
    Ok(snapshot)
}
