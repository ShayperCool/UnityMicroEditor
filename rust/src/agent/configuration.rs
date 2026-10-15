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
    project::{is_owned_cache_path, lexpath::rune_lower, value},
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

/// How many request ids the recovery has for one state of the sources, and
/// how many other preparations of the project it looks at for one that fits.
const RECOVERY_ATTEMPTS: u32 = 16;
const KNOWN_PREPARATIONS: usize = 8;
/// The error code of a preparation whose sources changed while it ran.
pub(crate) const SUPERSEDED: &str = "preparation_superseded";

/// What the service knows of the preparation job a request id names.
enum Known {
    Nothing,
    Running,
    Confirmed(Box<super::Preparation>),
    /// The error code of its receipt.
    Failed(String),
}
/// How a rejection that needs a new graph is answered.
enum Plan {
    /// The session restarts on a preparation the project has.
    Continue(Box<super::Preparation>),
    /// The preparation with this request id is queued, or runs, or failed.
    Request(String),
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
    /// The graph: what a preparation is made from.
    hashes: BTreeMap<String, String>,
    /// The content of every tracked file, the scene and the sources too.
    contents: BTreeMap<String, String>,
    file_hashes: BTreeMap<String, String>,
}
/// Whether only the presence of a file names the graph, not its content: a
/// C# source (its `.meta` holds its identity and stays tracked whole), and the
/// selected scene. A session compiles the content of the sources it starts
/// on, and the runner loads the scene it has; a preparation made from other
/// content of them serves as well. A save of such a file while the
/// preparation of a recovery runs must not void that preparation and ask for
/// another one.
fn presence_only(path: &str) -> bool {
    rune_lower(sysutil::ext(path)) == ".cs"
}
impl Tracker {
    fn track(&mut self, path: &str, key: String, optional: bool) -> Result<()> {
        self.track_as(path, key, optional, presence_only(path))
    }
    fn track_as(&mut self, path: &str, key: String, optional: bool, presence: bool) -> Result<()> {
        let hash = match self.file_hashes.get(path).filter(|hash| !hash.is_empty()) {
            Some(hash) => hash.clone(),
            None => {
                agent_canonical(path)?;
                let hash = match lstat(path) {
                    Err(error) if not_exist(&error) && optional => "absent".to_string(),
                    Err(error) => return Err(error),
                    Ok(_) => file_hash(path)?,
                };
                self.file_hashes.insert(path.into(), hash.clone());
                hash
            }
        };
        let graph = match presence && hash != "absent" {
            true => "present".to_string(),
            false => hash.clone(),
        };
        self.hashes.insert(key.clone(), graph);
        self.contents.insert(key, hash);
        Ok(())
    }
}

/// Only declared public inputs participate:
/// authored roots, the selected scene, and registered package sources from the
/// active preparation's verified graph. Missing declared inputs have an
/// identity too, so repairing a failed preparation cannot remain stuck on a
/// prior durable job after a source is added or removed. A C# source and the
/// selected scene count by their presence (see `presence_only`).
pub fn configuration_revision(
    project: &str,
    selections: &[ConfigurationSelection],
) -> Result<String> {
    revisions(project, selections).map(|(graph, _)| graph)
}

/// The same inputs by their content, the sources and the scene too: a
/// failed preparation is not the answer for sources that were edited since.
#[cfg(test)]
pub fn sources_revision(project: &str, selections: &[ConfigurationSelection]) -> Result<String> {
    revisions(project, selections).map(|(_, content)| content)
}

fn revisions(project: &str, selections: &[ConfigurationSelection]) -> Result<(String, String)> {
    let mut tracker = Tracker {
        hashes: BTreeMap::new(),
        contents: BTreeMap::new(),
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
            tracker.track_as(&path, selection.scene.clone(), false, true)?;
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
    Ok((
        sha256(&marshal(&tracker.hashes)?),
        sha256(&marshal(&tracker.contents)?),
    ))
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
        // The runner marked its answer as not its last: it is still working
        // on source changes, and the rejection may be taken back by the
        // next one. No preparation is queued on such an answer.
        if state.update_pending {
            return outcome;
        }
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
        let revision = match revisions(
            &self.0.project,
            &[ConfigurationSelection {
                scene: scene.clone(),
                baseline,
            }],
        ) {
            Ok(revisions) => revisions,
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
        // The preparation that answers this rejection: one the project
        // already has, or the request that makes it.
        let (revision, sources) = revision;
        let plan = self.plan_recovery(&scene, &revision, &sources);
        let reply = || -> Result<_> {
            let (preparation, continued) = match &plan {
                Plan::Continue(known) => match self.continue_on(known, &revision) {
                    Ok(current) => (super::json_reply(known), current),
                    Err(error) => (Err(error), None),
                },
                Plan::Request(identity) => {
                    let arguments = marshal(&Any::Object(obj([
                        ("scene", scene.clone().into()),
                        ("requestId", identity.clone().into()),
                        ("activate", true.into()),
                    ])))?;
                    // The receipt of a failed job answers these sources for
                    // as long as they stay as they are. The failure may not be
                    // theirs (an Editor that did not exit in time, a full
                    // disk), so the reply names the call that tries again.
                    let prepared = self.handle(ctx, "project_prepare", &arguments).map_err(|error| {
                        match context_error(&error) {
                            true => error,
                            false => anyhow!(
                                "{}; the preparation of these sources ({identity}) failed and is not tried again while they stay as they are: call project_prepare with this scene, a new requestId and activate true to prepare them again",
                                super::error_text(&error)
                            ),
                        }
                    });
                    (prepared, None)
                }
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
            reply.insert(
                "message".into(),
                match continued {
                    Some(_) => "these sources already have a preparation; the session is restarting on it, reissue the call",
                    None if matches!(plan, Plan::Continue(_)) => "these sources already have a preparation; the session restarts on it when the project action that runs has ended, reissue the call",
                    None => "the project is being prepared for the saved sources; the Player keeps its code until then and the session continues on the new preparation by itself, reissue the call",
                }
                .into(),
            );
            map_reply(reply)
        };
        match reply() {
            Ok(value) => RuntimeOutcome::handler(value),
            Err(error) => RuntimeOutcome::failed(error),
        }
    }

    /// The request id of the preparation of these sources. The first is a
    /// function of the scene and the sources; the next ones exist because a
    /// receipt is durable and its name can be wrong about it: see
    /// `plan_recovery`.
    pub(crate) fn recovery_identity(scene: &str, revision: &str, attempt: u32) -> String {
        let named = match attempt {
            0 => format!("{scene}\n{revision}"),
            attempt => format!("{scene}\n{revision}\n{attempt}"),
        };
        format!("config-{}", &sha256(named.as_bytes())[..40])
    }

    /// Whether `id` is a request of the recovery for the sources as they are
    /// on disk now. A request that is not was made for sources that have
    /// changed since: its preparation answers no rejection of the session.
    /// True when the sources cannot be read, so that nothing is held back
    /// on a guess.
    pub(crate) fn recovery_current(&self, id: &str, scene: &str) -> bool {
        let selection = ConfigurationSelection {
            scene: scene.into(),
            baseline: self.baseline(),
        };
        match configuration_revision(&self.0.project, &[selection]) {
            Ok(revision) => (0..RECOVERY_ATTEMPTS)
                .any(|attempt| Self::recovery_identity(scene, &revision, attempt) == id),
            Err(_) => true,
        }
    }

    /// The file that holds the content revision of the sources a request
    /// of the recovery was made for.
    fn recovery_sources_path(&self, id: &str) -> String {
        join(&[
            &self.0.project,
            "Library/UnityMicroEditor/action-jobs",
            &sha256(id.as_bytes()),
            "sources.txt",
        ])
    }
    pub(crate) fn recovery_sources(&self, id: &str) -> Option<String> {
        std::fs::read_to_string(self.recovery_sources_path(id)).ok()
    }
    /// Records the content of the sources a request of the recovery is made
    /// for, once.
    pub(crate) fn note_recovery_sources(&self, id: &str, sources: &str) {
        let path = self.recovery_sources_path(id);
        if std::fs::metadata(&path).is_ok() {
            return;
        }
        let _ = std::fs::create_dir_all(sysutil::dir(&path))
            .and_then(|()| std::fs::write(&path, sources));
    }

    /// What the service knows of the preparation job `id`.
    fn known_preparation(&self, id: &str) -> Known {
        let receipt = {
            let jobs = self.0.jobs.lock().unwrap();
            match jobs.jobs.get(id) {
                Some(job) => {
                    let job = job.lock().unwrap();
                    if job.active {
                        return Known::Running;
                    }
                    job.receipt.clone()
                }
                None => None,
            }
        };
        let receipt = match receipt {
            Some(receipt) => receipt,
            None => {
                let path = join(&[
                    &self.0.project,
                    "Library/UnityMicroEditor/action-jobs",
                    &sha256(id.as_bytes()),
                    "result.json",
                ]);
                match super::jobs::JobReceipt::read(&path) {
                    Ok(receipt) => receipt,
                    Err(_) => return Known::Nothing,
                }
            }
        };
        if let Some(error) = &receipt.error {
            return Known::Failed(error.code.clone());
        }
        match serde_json::from_slice::<Option<super::Preparation>>(&receipt.result) {
            Ok(Some(preparation))
                if preparation.schema_version == 1 && !preparation.baseline.is_empty() =>
            {
                Known::Confirmed(Box::new(preparation))
            }
            _ => Known::Failed("invalid_job_receipt".into()),
        }
    }

    /// Whether the session can continue on a preparation the project has:
    /// it is of this scene, it is not the one the session runs on (the
    /// runner has just rejected the sources for that one), and its graph
    /// accepts the sources on disk. The last is the check a start of the
    /// runner makes, made before anything is stopped: a preparation is not
    /// taken on the word of its request id.
    fn fits(&self, preparation: &super::Preparation, scene: &str) -> bool {
        preparation.schema_version == 1
            && preparation.project == self.0.project
            && preparation.scene == scene
            && !preparation.baseline.is_empty()
            && preparation.baseline != self.baseline()
            && !self
                .0
                .continued
                .lock()
                .unwrap()
                .1
                .contains(&preparation.baseline)
            && crate::compiler::Compiler::graph_accepts_sources(
                &self.0.project,
                &preparation.baseline,
            )
            .is_ok()
    }

    /// Decides how a rejection that needs a new graph is answered.
    ///
    /// The request id of a recovery names the sources it was computed from,
    /// and the job reads the sources later: an edit that is taken back in
    /// between is prepared under the name of the edit. Such a receipt says
    /// "confirmed" for sources its preparation was not made from. So a
    /// confirmed receipt counts only when its preparation fits the sources,
    /// and one that does not, or whose job says that the sources changed
    /// under it, is passed over for the next name. A receipt with any other
    /// error is the answer to its request: the same sources would fail the
    /// same way, and a repair changes the name.
    ///
    /// When no request of these sources has a preparation that fits, another
    /// preparation of the project may: the one a session started on, after
    /// an edit that left its graph was undone. The newest are looked at.
    fn plan_recovery(&self, scene: &str, revision: &str, sources: &str) -> Plan {
        // What the recovery continued on for these very sources and the
        // runner rejected again is not tried a second time: the check here
        // and the runner's own would otherwise pass the session back and
        // forth between two preparations.
        {
            let mut continued = self.0.continued.lock().unwrap();
            if continued.0 != revision {
                *continued = (revision.to_string(), Vec::new());
            }
        }
        let mut request = None;
        for attempt in 0..RECOVERY_ATTEMPTS {
            let id = Self::recovery_identity(scene, revision, attempt);
            match self.known_preparation(&id) {
                Known::Running => return Plan::Request(id),
                Known::Confirmed(preparation) if self.fits(&preparation, scene) => {
                    return Plan::Continue(preparation);
                }
                Known::Confirmed(_) => {}
                Known::Failed(code) if code == SUPERSEDED => {}
                // A failure answers the sources it was made from; an edit of
                // them since (the fix of a compile error) is tried anew.
                Known::Failed(_)
                    if self
                        .recovery_sources(&id)
                        .is_some_and(|made| made != sources) => {}
                Known::Failed(_) => return Plan::Request(id),
                Known::Nothing => {
                    self.note_recovery_sources(&id, sources);
                    request = Some(id);
                    break;
                }
            }
        }
        let directory = join(&[&self.0.project, "Library/UnityMicroEditor/preparations"]);
        let mut known: Vec<(std::time::SystemTime, super::Preparation)> =
            std::fs::read_dir(&directory)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let result = entry.path().join("result.json");
                    let modified = std::fs::metadata(&result).ok()?.modified().ok()?;
                    let preparation: super::Preparation =
                        read_json(&result.to_string_lossy(), 1 << 20).ok()?;
                    Some((modified, preparation))
                })
                .collect();
        known.sort_by_key(|known| std::cmp::Reverse(known.0));
        // The newest of this scene: those of other scenes do not fit anyway.
        for (_, preparation) in known
            .into_iter()
            .filter(|(_, preparation)| preparation.scene == scene)
            .take(KNOWN_PREPARATIONS)
        {
            if self.fits(&preparation, scene) {
                return Plan::Continue(Box::new(preparation));
            }
        }
        // Every name is taken by a receipt that does not fit: the sources
        // changed under each of their jobs. The last one is asked again and
        // answers with what it is.
        Plan::Request(
            request
                .unwrap_or_else(|| Self::recovery_identity(scene, revision, RECOVERY_ATTEMPTS - 1)),
        )
    }

    /// Makes a preparation the project has the selected one and restarts the
    /// session on it, as the job of a preparation does when it ends. Returns
    /// the state of the session after that.
    ///
    /// The restart runs under the lifetime of the service, not under the
    /// caller's request: a caller that gives up while the old Player is
    /// being stopped would otherwise leave a stopped session, with nothing
    /// selected and nothing started.
    ///
    /// `None`: a project action runs, and the next call decides anew.
    fn continue_on(
        &self,
        preparation: &super::Preparation,
        revision: &str,
    ) -> Result<Option<crate::agentcontrol::State>> {
        let Some(controller) = &self.0.controller else {
            return Ok(None);
        };
        // Never behind a project action that runs.
        let _guard = match self.0.mu.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        };
        // Another call can have done it meanwhile.
        if preparation.baseline == self.baseline() {
            return Ok(Some(controller.status_reply()));
        }
        {
            let mut continued = self.0.continued.lock().unwrap();
            if continued.0 != revision {
                *continued = (revision.to_string(), Vec::new());
            }
            continued.1.push(preparation.baseline.clone());
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
                preparation,
            )?;
            let lifetime = self.lifetime();
            let running = controller.status().phase != "stopped";
            self.0
                .swapping
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let swapped = (|| -> Result<()> {
                if running {
                    controller
                        .stop(&lifetime)
                        .map_err(|failure| failure.error)?;
                }
                controller.configure_prepared(&preparation.baseline, &preparation.scene)?;
                self.select(&preparation.baseline, &preparation.scene);
                if running {
                    controller.start()?;
                }
                Ok(())
            })();
            self.0
                .swapping
                .store(false, std::sync::atomic::Ordering::SeqCst);
            swapped
        };
        match activate() {
            Ok(()) => Ok(Some(controller.status_reply())),
            Err(error) if context_error(&error) => Err(error),
            // It is not tried again for these sources: the next call
            // requests a preparation instead.
            Err(error) => Err(anyhow!(
                "the preparation {} that fits these sources could not be activated: {}. The Player keeps its code; the next call requests a new preparation",
                preparation.request_id,
                super::error_text(&error)
            )),
        }
    }
}
