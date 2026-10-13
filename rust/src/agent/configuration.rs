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

pub(crate) use crate::compiler::requires_new_graph;

/// Unity's package cache and the content store of `crate::packages`, below
/// the project.
const PACKAGE_CACHE: &str = "Library/PackageCache";
const PACKAGE_STORE: &str = "Library/UnityMicroEditor/packages/content";

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
            // Two places below `Library` hold package sources as they were
            // published: Unity's package cache, and the content store of
            // this tool's own resolver, which names each package by the
            // hash of its content. Everything else there is an output.
            let published = [PACKAGE_CACHE, PACKAGE_STORE]
                .iter()
                .any(|store| within_directory(&root.source, &join(&[project, store])));
            if within_directory(&root.source, &join(&[project, "Library"])) && !published
                || within_directory(&root.source, &join(&[project, "Temp"]))
            {
                return Err(anyhow!(
                    "the package {} of the prepared graph has its source in a private cache of the project: {}",
                    root.path,
                    root.source
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
        let action = match jsondoc::parse(raw) {
            Ok(Json::Object(members)) => members
                .iter()
                .rev()
                .find(|member| member.key.eq_ignore_ascii_case("action"))
                .and_then(|action| action.value.as_str().map(str::to_string))
                .unwrap_or_default(),
            _ => String::new(),
        };
        // A status poll must not hide the rejection either: it reports that a
        // new preparation is required and starts the same recovery. Both
        // waits report an edit alike.
        if action != "wait_update" && action != "wait" && action != "status" {
            return outcome;
        }
        let RuntimeValue::Controller(Reply::State(state)) = &outcome.value else {
            return outcome;
        };
        // An edit that hot reload refused is answered by a recompile of the
        // prepared graph, whatever its text says; it never needs a preparation.
        // A session that could not start because the sources have left the
        // prepared graph (a reset after such an edit) needs the same.
        let reason = match state.phase.as_str() {
            "ready" if !state.code_refused() && requires_new_graph(&state.update_error) => {
                state.update_error.clone()
            }
            "failed" if requires_new_graph(&state.last_error) => state.last_error.clone(),
            _ => return outcome,
        };
        if outcome.error.as_ref().is_some_and(context_error) {
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
            // The recovery cannot begin. A status still is the state of the
            // session, with what stands in its way; a wait fails with the
            // reason of the rejection and the calls that are left.
            Err(error) => {
                let unqueued = format!(
                    "the preparation this change needs could not be queued: {}. Call project_prepare with this scene and activate true, or session_control(reset)",
                    super::error_text(&error)
                );
                if action != "status" {
                    let failure = anyhow!("{reason}; {unqueued}");
                    return RuntimeOutcome {
                        value: outcome.value,
                        error: Some(failure),
                    };
                }
                let reply = super::jobs::state_members(state).and_then(|mut reply| {
                    reply.insert("preparationRequired".into(), true.into());
                    reply.insert("preparationReason".into(), reason.clone().into());
                    reply.insert("preparationError".into(), unqueued.into());
                    map_reply(reply)
                });
                return match reply {
                    Ok(value) => RuntimeOutcome::handler(value),
                    Err(error) => RuntimeOutcome::failed(error),
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
            // Sources that were prepared before under this identity (an edit
            // that was undone and made again, a branch switched back) have a
            // confirmed receipt: no job runs for them, so no job activates
            // their preparation either. The session continues on it here.
            let (preparation, continued) = match self.handle(ctx, "project_prepare", &arguments) {
                Ok(receipt) => match self.continue_on_confirmed(ctx, &receipt) {
                    Ok(continued) => (Ok(receipt), continued),
                    Err(error) => (Err(error), None),
                },
                Err(error) => (Err(error), None),
            };
            if action == "status" {
                let mut reply = match &continued {
                    Some(current) => super::jobs::state_members(current)?,
                    None => {
                        let mut reply = super::jobs::state_members(state)?;
                        reply.insert("preparationRequired".into(), true.into());
                        reply.insert("preparationReason".into(), reason.clone().into());
                        reply
                    }
                };
                // A failed recovery must not turn the status itself into an error.
                match &preparation {
                    Ok(preparation) => reply.insert(
                        "preparation".into(),
                        Any::Raw(String::from_utf8_lossy(reply_bytes(preparation)).into()),
                    ),
                    Err(error) => {
                        reply.insert("preparationError".into(), super::error_text(error).into())
                    }
                };
                reply.insert("configurationRevision".into(), revision.clone().into());
                return map_reply(reply);
            }
            let preparation = preparation?;
            let mut reply = obj([
                ("status", "result_pending".into()),
                ("phase", "preparing_configuration".into()),
                (
                    "currentPlayer",
                    value::raw(continued.as_ref().unwrap_or(state.as_ref()))?,
                ),
                (
                    "preparation",
                    Any::Raw(String::from_utf8_lossy(reply_bytes(&preparation)).into()),
                ),
                ("configurationRevision", revision.clone().into()),
            ]);
            if continued.is_some() {
                reply.insert(
                    "message".into(),
                    "these sources already have a preparation; the session is restarting on it, reissue the call"
                        .into(),
                );
            }
            map_reply(reply)
        };
        match reply() {
            Ok(value) => RuntimeOutcome::handler(value),
            Err(error) => RuntimeOutcome::failed(error),
        }
    }

    /// Makes a confirmed preparation that is not the selected one the
    /// selected one and restarts the session on it, as the job of a
    /// preparation does when it ends. Returns the state of the session after
    /// that, and `None` when there is nothing to do: the receipt is of a job
    /// that still runs, or its preparation is the selected one.
    fn continue_on_confirmed(
        &self,
        ctx: &Context,
        receipt: &crate::mcp::HandlerValue,
    ) -> Result<Option<crate::agentcontrol::State>> {
        let Some(controller) = &self.0.controller else {
            return Ok(None);
        };
        let preparation =
            match serde_json::from_slice::<Option<super::Preparation>>(reply_bytes(receipt)) {
                Ok(Some(preparation))
                    if preparation.schema_version == 1 && !preparation.baseline.is_empty() =>
                {
                    preparation
                }
                _ => return Ok(None),
            };
        if preparation.baseline == self.baseline() {
            return Ok(None);
        }
        // Never behind a project action that runs: the receipt stays, and
        // the next call finds it.
        let _guard = match self.0.mu.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        };
        // Another call can have done it meanwhile.
        if preparation.baseline == self.baseline() {
            return Ok(None);
        }
        let activate = || -> Result<()> {
            let directory = join(&[
                &self.0.project,
                "Library/UnityMicroEditor/preparations",
                &preparation.request_id,
            ]);
            super::validate_cached_preparation(
                &self.0.project,
                &self.0.editor,
                &preparation.scene,
                &preparation.request_id,
                &directory,
                &preparation,
            )?;
            let running = controller.status().phase != "stopped";
            if running {
                controller.stop(ctx).map_err(|failure| failure.error)?;
            }
            controller.configure_prepared(&preparation.baseline, &preparation.scene)?;
            self.select(&preparation.baseline, &preparation.scene);
            if running {
                controller.start()?;
            }
            Ok(())
        };
        match activate() {
            Ok(()) => Ok(Some(controller.status_reply())),
            Err(error) if context_error(&error) => Err(error),
            // The receipt stays the answer to its request, so the same call
            // would end here again: the caller is told what begins anew.
            Err(error) => Err(anyhow!(
                "the preparation {} that these sources already have could not be activated: {}. Call project_prepare with this scene, a new requestId and activate true",
                preparation.request_id,
                super::error_text(&error)
            )),
        }
    }
}
