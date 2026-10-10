//! Restartable code revisions. A process restart must
//! retain the verified compiled graph, rather than rebuilding the old
//! preparation's changed assemblies on every launch. The entry owns no mutable
//! project content: it names an immutable Player and verifies every file
//! before that Player is selected again.
use super::{
    CompileManifest, CompileUpdate, Compiler, LifecycleProof, NilMap, NilVec,
    compile::{lifecycle_sources_match, valid_lifecycle_proof, verify_cache_files},
    join,
    netcode::needs_netcode,
    nilable::nz,
    player_managed,
    sys::{self, Walk},
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, read_json_file, within_directory},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::PermissionsExt};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeRevision {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub key: String,
    /// Relative to the compiler cache.
    #[serde(deserialize_with = "nz")]
    pub player: String,
    /// Relative to the Player directory.
    pub files: NilMap<String>,
    pub sources: NilMap<String>,
    pub proofs: NilMap<LifecycleProof>,
}

/// Slash-relative path to SHA256 of every file of a Player.
pub fn player_files(directory: &str) -> Result<NilMap<String>> {
    if sysutil::eval_symlinks(directory).ok() != Some(sysutil::clean(directory)) {
        bail!("compiler Player snapshot must be a canonical directory");
    }
    let mut files = NilMap::make();
    sys::walk_dir(directory, &mut |entry| {
        if entry.is_dir() {
            return Ok(Walk::Continue);
        }
        if !entry.kind.is_file() || files.len() >= 4096 {
            bail!("compiler Player snapshot contains an unsupported file");
        }
        let hash = file_hash(entry.path)?;
        let relative = sysutil::rel(directory, entry.path)
            .ok_or_else(|| anyhow!("Rel: can't make {} relative to {directory}", entry.path))?;
        files.insert(relative, hash);
        Ok(Walk::Continue)
    })?;
    if files.is_empty() {
        bail!("compiler Player snapshot has no files");
    }
    Ok(files)
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RevisionIdentity<'a> {
    mode: &'static str,
    player_name: String,
    player_files: &'a NilMap<String>,
    plan: &'a CompileManifest,
    sources: &'a NilMap<String>,
    configuration: &'a NilMap<String>,
    assembly_configs: &'a NilMap<String>,
    expected_types: &'a NilMap<NilMap<String>>,
    tools: &'a NilMap<String>,
    lifecycle: &'a NilMap<String>,
    netcode_postprocessor: &'a str,
}

impl Compiler {
    /// Identity of the complete compiled graph, including
    /// the baseline Player, every tool and every source.
    pub fn code_revision_key(&mut self, ctx: &Context, sources: &NilMap<String>) -> Result<String> {
        ctx.err()?;
        let player = if self.baseline_player.is_empty() {
            self.player.clone()
        } else {
            self.baseline_player.clone()
        };
        let files = player_files(&sysutil::dir(&player))?;
        let helper = self.prepare_lifecycle_helper(ctx)?;
        let helper_directory = sysutil::dir(&helper);
        let helper_files =
            read_json_file::<NilMap<String>>(&join(&[&helper_directory, "files.json"]))
                .ok()
                .filter(|files| {
                    !files.at("LifecycleCompiler.dll").is_empty()
                        && verify_cache_files(&helper_directory, files)
                })
                .ok_or_else(|| anyhow!("lifecycle revision helper payload is not verified"))?;
        let mut paths = vec![self.dotnet.clone()];
        paths.extend(self.base_references.iter().cloned());
        paths.extend(self.engine_references.iter().cloned());
        paths.extend(self.generators.iter().cloned());
        paths.extend(sys::glob(&join(&[&sysutil::dir(&self.csc), "*.dll"]))?);
        for generator in &self.generators {
            paths.extend(sys::glob(&join(&[&sysutil::dir(generator), "*.dll"]))?);
        }
        let mut tool_files = NilMap::make();
        for path in paths {
            if !tool_files.contains_key(&path) {
                let hash = file_hash(&path)?;
                tool_files.insert(path, hash);
            }
        }
        let mut netcode_key = String::new();
        if self.manifest.assemblies.iter().any(needs_netcode) {
            netcode_key = self.prepare_netcode_processor(ctx)?.key;
        }
        let encoded = marshal(&RevisionIdentity {
            mode: "code-revision-v1",
            player_name: sysutil::base(&player),
            player_files: &files,
            plan: &self.manifest,
            sources,
            configuration: &self.configuration,
            assembly_configs: &self.configuration_hashes,
            expected_types: &self.expected_types,
            tools: &tool_files,
            lifecycle: &helper_files,
            netcode_postprocessor: &netcode_key,
        })?;
        Ok(sha256(&encoded))
    }

    pub fn code_revision_path(&self, key: &str) -> String {
        join(&[&self.cache, "code-revisions", &format!("{key}.json")])
    }

    pub fn save_code_revision(
        &self,
        key: &str,
        update: &CompileUpdate,
        proofs: &NilMap<LifecycleProof>,
    ) -> Result<()> {
        let player = sysutil::rel(&self.cache, &update.player)
            .filter(|_| within_directory(&update.player, &self.cache))
            .ok_or_else(|| anyhow!("compiled revision Player is outside owned compiler cache"))?;
        let files = player_files(&sysutil::dir(&update.player))?;
        let entry = CodeRevision {
            schema_version: 1,
            key: key.into(),
            player,
            files,
            sources: update.source_hashes.clone(),
            proofs: proofs.clone(),
        };
        let path = self.code_revision_path(key);
        sys::mkdir_all(&sysutil::dir(&path))?;
        atomic_json(&path, &entry)
    }

    /// A verified complete revision, or `None`.
    pub fn restore_code_revision(
        &mut self,
        key: &str,
        sources: &NilMap<String>,
    ) -> Option<CompileUpdate> {
        let filename = self.code_revision_path(key);
        let info = fs::symlink_metadata(&filename).ok()?;
        if !info.is_file() || info.len() > 8 * 1024 * 1024 {
            return None;
        }
        let entry = read_json_file::<Option<CodeRevision>>(&filename)
            .ok()?
            .unwrap_or_default();
        if entry.schema_version != 1
            || entry.key != key
            || entry.sources != *sources
            || entry.proofs.len() != self.manifest.assemblies.len()
            || sysutil::is_abs(&entry.player)
            || sysutil::clean(&entry.player) != entry.player
        {
            return None;
        }
        let player = join(&[&self.cache, &entry.player]);
        if sysutil::eval_symlinks(&player).ok().as_deref() != Some(player.as_str())
            || !within_directory(&player, &self.cache)
            || sysutil::base(&player) != sysutil::base(&self.player)
        {
            return None;
        }
        let info = fs::metadata(&player).ok()?;
        if !info.is_file() || info.permissions().mode() & 0o111 == 0 {
            return None;
        }
        let directory = sysutil::dir(&player);
        if player_files(&directory).ok()? != entry.files {
            return None;
        }
        let managed = player_managed(&player);
        let mut update = CompileUpdate {
            player,
            cache_reused: true,
            source_hashes: sources.clone(),
            compiled_assemblies: NilVec::make(),
            runtime_initializers: NilVec::make(),
            declaration_changed: NilVec::make(),
            dll_hashes: NilMap::make(),
            ..Default::default()
        };
        for assembly in &self.manifest.assemblies {
            let proof = entry.proofs.get(&assembly.name)?;
            if !valid_lifecycle_proof(proof, &assembly.name)
                || !lifecycle_sources_match(proof, &assembly.sources)
            {
                return None;
            }
            let dll = join(&[&managed, &format!("{}.dll", assembly.name)]);
            let relative = sysutil::rel(&directory, &dll)?;
            let hash = entry.files.at(&relative);
            if hash.is_empty() {
                return None;
            }
            update.dll_hashes.insert(&assembly.name, hash.to_string());
            update.assemblies.push(dll);
            update
                .runtime_initializers
                .append(proof.runtime_initializers.iter().cloned());
        }
        // Assignment waits until the complete descriptor and Player have passed.
        self.proofs = entry.proofs;
        Some(update)
    }

    /// Installs a compiled or restored revision as current.
    pub fn use_code_revision(&mut self, update: &CompileUpdate, proofs: NilMap<LifecycleProof>) {
        self.player = update.player.clone();
        self.sources = update.source_hashes.clone();
        self.proofs = proofs;
        self.startup_complete = true;
        if self.netcode_processed.is_nil() {
            self.netcode_processed = NilMap::make();
        }
        self.accepted_dll_hashes = update.dll_hashes.clone();
        for (index, assembly) in self.manifest.assemblies.iter().enumerate() {
            self.current_dlls
                .insert(&assembly.name, update.assemblies[index].clone());
            if needs_netcode(assembly) {
                self.netcode_processed.insert(&assembly.name, true);
            }
        }
    }
}
