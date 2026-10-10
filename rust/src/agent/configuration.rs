//! Recovery from a managed configuration change the running Player cannot absorb.
use super::{
    Any, Service,
    actions::{RuntimeOutcome, RuntimeValue},
    authored_scene, context_error, join, lstat, map_reply, marshal, not_exist, nz, obj, read_json,
    reply_bytes,
};
use crate::{
    agentcontrol::Reply,
    cancel::Context,
    compiler::{CompileManifest, CompilerGraphExport, NilMap, Walk, WalkEntry, walk_dir},
    jsondoc::{self, Json},
    project::{is_owned_cache_path, value},
    sha256, sysutil,
    util::{agent_canonical, file_hash, within_directory},
};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::collections::BTreeMap;

pub(crate) fn requires_new_graph(message: &str) -> bool {
    [
        "regenerate source graph",
        "fresh managed graph",
        "outside the prepared managed graph",
    ]
    .iter()
    .any(|text| message.contains(text))
}

#[derive(Debug, Clone, Default)]
pub struct ConfigurationSelection {
    pub scene: String,
    pub baseline: String,
}

pub(crate) fn managed_configuration_path(path: &str) -> bool {
    // Lowercased with Unicode case mapping; extensions here are ASCII.
    let mut path = path.to_lowercase();
    if sysutil::ext(&path) == ".meta" {
        path.truncate(path.len() - ".meta".len());
    }
    matches!(
        sysutil::ext(&path),
        ".cs" | ".asmdef" | ".asmref" | ".rsp" | ".dll"
    ) || sysutil::base(&path) == "package.json"
}

struct Tracker {
    hashes: BTreeMap<String, String>,
    file_hashes: BTreeMap<String, String>,
}
impl Tracker {
    fn track(&mut self, path: &str, key: String, optional: bool) -> Result<()> {
        if let Some(hash) = self.file_hashes.get(path).filter(|hash| !hash.is_empty()) {
            self.hashes.insert(key, hash.clone());
            return Ok(());
        }
        agent_canonical(path)?;
        if let Err(error) = lstat(path) {
            if not_exist(&error) && optional {
                self.hashes.insert(key, "absent".into());
                self.file_hashes.insert(path.into(), "absent".into());
                return Ok(());
            }
            return Err(error);
        }
        let hash = file_hash(path)?;
        self.hashes.insert(key, hash.clone());
        self.file_hashes.insert(path.into(), hash);
        Ok(())
    }
}

/// Only declared public inputs participate:
/// authored roots, the selected scene, and registered package sources from the
/// active preparation's verified graph. Missing declared inputs have an
/// identity too, so repairing a failed preparation cannot remain stuck on a
/// prior durable job after a source is added or removed.
pub fn configuration_revision(
    project: &str,
    selections: &[ConfigurationSelection],
) -> Result<String> {
    let mut tracker = Tracker {
        hashes: BTreeMap::new(),
        file_hashes: BTreeMap::new(),
    };
    for folder in ["Assets", "Packages", "ProjectSettings"] {
        let root = join(&[project, folder]);
        agent_canonical(&root)?;
        walk_dir(&root, &mut |entry: &WalkEntry| {
            if entry.is_dir() {
                return Ok(Walk::Continue);
            }
            if entry.kind.is_symlink() {
                return Err(anyhow!(
                    "configuration inventory contains a symlink: {}",
                    entry.path
                ));
            }
            let tracked = folder == "ProjectSettings"
                || folder == "Packages"
                || managed_configuration_path(entry.path);
            if !tracked {
                return Ok(Walk::Continue);
            }
            let logical = sysutil::rel(project, entry.path)
                .ok_or_else(|| anyhow!("Rel: can't make {} relative to {project}", entry.path))?;
            tracker.track(entry.path, logical, false)?;
            Ok(Walk::Continue)
        })?;
    }
    for selection in selections {
        if !selection.scene.is_empty() {
            if !authored_scene(&selection.scene) {
                return Err(anyhow!(
                    "configuration recovery requires an explicit authored Assets scene"
                ));
            }
            let path = join(&[project, &selection.scene]);
            tracker.track(&path, selection.scene.clone(), false)?;
            tracker.track(
                &format!("{path}.meta"),
                format!("{}.meta", selection.scene),
                true,
            )?;
        }
        if selection.baseline.is_empty() {
            continue;
        }
        if !sysutil::is_abs(&selection.baseline)
            || !is_owned_cache_path(project, &selection.baseline)
        {
            return Err(anyhow!(
                "configuration recovery baseline is outside owned project caches"
            ));
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Baseline {
            #[serde(deserialize_with = "nz")]
            project: String,
            #[serde(deserialize_with = "nz")]
            compiler: CompileManifest,
        }
        let baseline: Baseline = read_json(&selection.baseline, 32 << 20)?;
        if baseline.project != project
            || !baseline.compiler.project.is_empty() && baseline.compiler.project != project
        {
            return Err(anyhow!(
                "configuration recovery baseline belongs to another project"
            ));
        }
        let mut inputs: NilMap<String> = baseline.compiler.compiler_inputs;
        let graph_path = join(&[&sysutil::dir(&selection.baseline), "graph.json"]);
        let graph = match read_json::<CompilerGraphExport>(&graph_path, 32 << 20) {
            Ok(graph) => {
                if graph.schema_version != 1
                    || graph.project != project
                    || graph.source_roots.len() > 4096
                {
                    return Err(anyhow!(
                        "configuration recovery public graph identity is invalid"
                    ));
                }
                for (path, hash) in &graph.compiler_inputs {
                    inputs.insert(path, hash.clone());
                }
                graph
            }
            Err(error) if not_exist(&error) => CompilerGraphExport::default(),
            Err(error) => return Err(error),
        };
        let mut registered: BTreeMap<String, String> = BTreeMap::new();
        for root in graph.source_roots.iter() {
            let parts: Vec<&str> = root.path.split('/').collect();
            if parts.len() != 2
                || parts[0] != "Packages"
                || parts[1].is_empty()
                || parts[1] == "."
                || parts[1] == ".."
                || sysutil::clean(&root.path) != root.path
                || !sysutil::is_abs(&root.source)
                || sysutil::clean(&root.source) != root.source
                || root.source == "/"
            {
                return Err(anyhow!("invalid registered public package root"));
            }
            if let Some(previous) = registered
                .get(&root.path)
                .filter(|source| !source.is_empty())
            {
                if *previous != root.source {
                    return Err(anyhow!("conflicting registered public package roots"));
                }
                continue;
            }
            if within_directory(&root.source, &join(&[project, "Library"]))
                && !within_directory(&root.source, &join(&[project, "Library/PackageCache"]))
                || within_directory(&root.source, &join(&[project, "Temp"]))
            {
                return Err(anyhow!(
                    "registered package source crosses a private project cache"
                ));
            }
            agent_canonical(&root.source)?;
            registered.insert(root.path.clone(), root.source.clone());
            tracker
                .hashes
                .insert(format!("source-root:{}", root.path), root.source.clone());
            match lstat(&root.source) {
                Err(error) if not_exist(&error) => {
                    tracker
                        .hashes
                        .insert(format!("source-root-state:{}", root.path), "absent".into());
                    continue;
                }
                Err(error) => return Err(error),
                Ok(info) if !info.is_dir() => {
                    return Err(anyhow!(
                        "registered public package source is not a directory"
                    ));
                }
                Ok(_) => {}
            }
            walk_dir(&root.source, &mut |entry: &WalkEntry| {
                if entry.kind.is_symlink() {
                    return Err(anyhow!(
                        "registered package configuration contains a symlink: {}",
                        entry.path
                    ));
                }
                if entry.is_dir() {
                    if entry.path != root.source
                        && matches!(
                            sysutil::base(entry.path).as_str(),
                            ".git" | ".svn" | "Library" | "Temp" | "obj"
                        )
                    {
                        return Ok(Walk::SkipDir);
                    }
                    return Ok(Walk::Continue);
                }
                if !managed_configuration_path(entry.path) {
                    return Ok(Walk::Continue);
                }
                let logical = sysutil::rel(&root.source, entry.path).ok_or_else(|| {
                    anyhow!("Rel: can't make {} relative to {}", entry.path, root.source)
                })?;
                tracker.track(
                    entry.path,
                    format!("package:{}/{logical}", root.path),
                    false,
                )?;
                Ok(Walk::Continue)
            })?;
        }
        // Visited in key order.
        for path in inputs.keys() {
            if !managed_configuration_path(path) {
                continue;
            }
            if !sysutil::is_abs(path) || sysutil::clean(path) != *path {
                return Err(anyhow!(
                    "public compiler input is not absolute and normalized"
                ));
            }
            // Generated assemblies and compiler caches are outputs. Registered
            // PackageCache source files remain public package inputs.
            if (within_directory(path, &join(&[project, "Library"]))
                || within_directory(path, &join(&[project, "Temp"])))
                && !registered
                    .values()
                    .any(|source| within_directory(path, source))
            {
                continue;
            }
            tracker.track(path, format!("compiler-input:{path}"), true)?;
        }
    }
    Ok(sha256(&marshal(&tracker.hashes)?))
}

impl Service {
    /// A watch rejection keeps the old Player. A
    /// configuration-specific rejection queues a full public graph/host
    /// preparation; activation happens only after it has compiled and
    /// validated. Ordinary C# compile errors retain existing behavior.
    pub(crate) fn recover_configuration(
        &self,
        ctx: &Context,
        name: &str,
        raw: &[u8],
        outcome: RuntimeOutcome,
    ) -> RuntimeOutcome {
        if name != "session_control" {
            return outcome;
        }
        let waiting = matches!(
            jsondoc::parse(raw),
            Ok(Json::Object(members))
                if members.iter().rev().find(|member| member.key.eq_ignore_ascii_case("action"))
                    .is_some_and(|action| action.value.as_str() == Some("wait_update"))
        );
        if !waiting {
            return outcome;
        }
        let RuntimeValue::Controller(Reply::State(state)) = &outcome.value else {
            return outcome;
        };
        if state.phase != "ready"
            || !requires_new_graph(&state.update_error)
            || outcome.error.as_ref().is_some_and(context_error)
        {
            return outcome;
        }
        let (scene, baseline) = (self.scene(), self.baseline());
        if scene.is_empty() {
            return outcome;
        }
        let revision = match configuration_revision(
            &self.0.project,
            &[ConfigurationSelection {
                scene: scene.clone(),
                baseline,
            }],
        ) {
            Ok(revision) => revision,
            Err(error) => {
                return RuntimeOutcome {
                    value: outcome.value,
                    error: Some(error),
                };
            }
        };
        let identity = format!(
            "config-{}",
            &sha256(format!("{scene}\n{revision}").as_bytes())[..40]
        );
        let reply = || -> Result<_> {
            let arguments = marshal(&Any::Object(obj([
                ("scene", scene.clone().into()),
                ("requestId", identity.into()),
                ("activate", true.into()),
            ])))?;
            let preparation = self.handle(ctx, "project_prepare", &arguments)?;
            map_reply(obj([
                ("status", "result_pending".into()),
                ("phase", "preparing_configuration".into()),
                ("currentPlayer", value::raw(state.as_ref())?),
                (
                    "preparation",
                    Any::Raw(String::from_utf8_lossy(reply_bytes(&preparation)).into()),
                ),
                ("configurationRevision", revision.clone().into()),
            ]))
        };
        match reply() {
            Ok(value) => RuntimeOutcome::handler(value),
            Err(error) => RuntimeOutcome::failed(error),
        }
    }
}
