//! The active preparation coordinator. Target graphs
//! come from authored sources and resolved packages; the Editor only bootstraps
//! a pinned native engine/host profile once.
use super::{
    Any, AnyMap, Preparation, Service, authored_scene, bootstrap, fail, join, not_exist, nz, obj,
    operation_id, read_json, staging::phase_id, stat, text,
};
use crate::{
    cancel::Context,
    packages,
    project::is_owned_cache_path,
    sourcegraph::{
        SourceGraphOptions, build_source_graph, capture_engine_profile, load_engine_profile,
        prepare_source_compiler_graph,
    },
    sysutil,
    util::{agent_id, agent_immutable_json},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};

fn is_empty(text: &str) -> bool {
    text.is_empty()
}
/// `rebuildHost: true` skips the reuse of an earlier host.
fn rebuild_host(args: &AnyMap) -> bool {
    args.get("rebuildHost").is_some_and(Any::is_true)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PreparationSeed {
    #[serde(deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub scene: String,
    #[serde(deserialize_with = "nz")]
    pub host_player: String,
    #[serde(deserialize_with = "nz")]
    pub native_catalog: String,
    #[serde(deserialize_with = "nz")]
    pub engine_profile: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub reference_graph: String,
    /// The requestId of the earlier preparation whose host was found and reused.
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub reused_preparation: String,
}

impl Service {
    /// Binds caller options separately from resolved
    /// defaults. Activating another preparation must not change the meaning of
    /// a retry with the same request ID.
    pub(crate) fn source_preparation_seed(
        &self,
        args: &AnyMap,
        directory: &str,
    ) -> Result<PreparationSeed> {
        agent_immutable_json(
            &join(&[directory, "request.json"]),
            &Any::Object(
                obj([
                    ("schemaVersion", 2.into()),
                    ("backend", "authored_source".into()),
                    ("project", self.0.project.clone().into()),
                    ("editor", self.0.editor.clone().into()),
                    ("scene", text(args, "scene").into()),
                    ("hostPlayer", text(args, "hostPlayer").into()),
                    ("nativeCatalog", text(args, "nativeCatalog").into()),
                    ("engineProfile", text(args, "engineProfile").into()),
                    ("configuredHostPlayer", self.0.host.clone().into()),
                    (
                        "configuredEngineProfile",
                        self.0.engine_profile_path.clone().into(),
                    ),
                    ("mainEditorAllowed", (!self.0.no_editor).into()),
                ])
                .into_iter()
                // Recorded only when set: earlier requests have no such member.
                .chain(rebuild_host(args).then(|| ("rebuildHost".to_string(), true.into())))
                .collect(),
            ),
        )?;
        let seed_path = join(&[directory, "seed.json"]);
        match read_json::<PreparationSeed>(&seed_path, 1 << 20) {
            Ok(frozen) => {
                if frozen.schema_version != 1 {
                    return Err(fail(
                        "invalid_preparation_seed",
                        "source preparation seed schema is invalid",
                    ));
                }
                return Ok(frozen);
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        let previous = self.accepted_preparation_seed();
        let mut frozen = PreparationSeed {
            schema_version: 1,
            scene: text(args, "scene"),
            host_player: text(args, "hostPlayer"),
            native_catalog: text(args, "nativeCatalog"),
            engine_profile: text(args, "engineProfile"),
            reference_graph: String::new(),
            reused_preparation: String::new(),
        };
        if frozen.scene.is_empty() {
            frozen.scene = self.scene();
        }
        if frozen.host_player.is_empty() {
            frozen.host_player = self.0.host.clone();
        }
        if frozen.host_player.is_empty() {
            frozen.host_player = previous.host_player.clone();
        }
        if frozen.native_catalog.is_empty() {
            frozen.native_catalog = previous.native_catalog.clone();
        }
        // A new requestId is a new preparation, not a new host. The newest host
        // an earlier request built for this project is reused unless the caller
        // asks for a rebuild; later stages still verify it against the sources.
        let mut discovered = Preparation::default();
        if frozen.host_player.is_empty() && !rebuild_host(args) {
            discovered = self.discover_host();
            frozen.host_player = discovered.host_player.clone();
            frozen.reused_preparation = discovered.request_id.clone();
            if frozen.native_catalog.is_empty() && discovered.scene == frozen.scene {
                frozen.native_catalog = discovered.native_catalog.clone();
            }
        }
        if frozen.engine_profile.is_empty() {
            frozen.engine_profile = self.0.engine_profile_path.clone();
        }
        if frozen.engine_profile.is_empty() {
            frozen.engine_profile = previous.engine_profile.clone();
        }
        if previous.host_player == frozen.host_player {
            if !previous.reference_graph.is_empty() {
                frozen.reference_graph = previous.reference_graph.clone();
            } else if !previous.baseline.is_empty()
                && previous.backends.at("preparation") == "unity_editor_public_api"
            {
                frozen.reference_graph = join(&[&sysutil::dir(&previous.baseline), "graph.json"]);
            }
        }
        if frozen.reference_graph.is_empty() && !discovered.host_player.is_empty() {
            frozen.reference_graph = discovered.reference_graph.clone();
        }
        agent_immutable_json(&seed_path, &frozen)?;
        Ok(frozen)
    }

    /// The newest confirmed preparation or engine bootstrap of this project
    /// and Editor whose host Player still exists with its compiled-source
    /// proof. Its content is verified where it is used, not here.
    fn discover_host(&self) -> Preparation {
        let project = self.0.project.as_str();
        let root = join(&[project, "Library/UnityMicroEditor/preparations"]);
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Preparation::default();
        };
        let mut candidates: Vec<(std::time::SystemTime, String)> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().into_string().ok()?;
                let result = entry.path().join("result.json");
                let modified = std::fs::symlink_metadata(&result).ok()?.modified().ok()?;
                operation_id(&name).then_some((modified, name))
            })
            .collect();
        candidates.sort_by(|a, b| b.cmp(a));
        for (_, name) in candidates.into_iter().take(256) {
            let directory = join(&[&root, &name]);
            let Ok(result) = read_json::<Preparation>(&join(&[&directory, "result.json"]), 1 << 20)
            else {
                continue;
            };
            let request =
                super::read_map(&join(&[&directory, "request.json"]), 1 << 20).unwrap_or_default();
            let host = result.host_player.as_str();
            if result.schema_version != 1
                || result.project != project
                || result.request_id != name
                || text(&request, "editor") != self.0.editor
                || !sysutil::is_abs(host)
                || !sysutil::lstat(host)
                    .is_ok_and(|info| info.is_regular() && info.mode & 0o111 != 0)
                || bootstrap::source_engine_profile_cache_path(project, host).is_err()
            {
                continue;
            }
            return result;
        }
        Preparation::default()
    }

    /// The identity problems `project_inspect` reports, checked before any
    /// Editor or compiler runs. Missing metadata only stops a preparation
    /// that needs an Editor stage: that Editor would generate the identity in
    /// its own copy, and the copy would no longer match the target.
    pub(crate) fn check_authored_identity(&self, editor_stage: bool) -> Result<()> {
        let store = crate::authoring::Store::new(std::path::Path::new(&self.0.project))
            .map_err(super::store_error)?;
        let info = store.inspect().map_err(super::store_error)?;
        let issues: Vec<&String> = info
            .identity_issues
            .iter()
            .filter(|issue| editor_stage || !issue.ends_with(": missing metadata"))
            .collect();
        if issues.is_empty() {
            return Ok(());
        }
        Err(super::fail_with(
            "invalid_identity",
            &format!(
                "authored metadata has {} identity issue(s), the first is: {}; fix them (project_inspect lists all) before preparation",
                issues.len(),
                issues[0]
            ),
            obj([
                ("issueCount", (issues.len() as i64).into()),
                (
                    "issues",
                    issues
                        .iter()
                        .take(64)
                        .map(|issue| Any::from(issue.as_str()))
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ]),
        ))
    }

    /// The result of the currently activated preparation.
    fn accepted_preparation_seed(&self) -> Preparation {
        let baseline = self.baseline();
        if baseline.is_empty() || !is_owned_cache_path(&self.0.project, &baseline) {
            return Preparation::default();
        }
        let result: Preparation =
            read_json(&join(&[&sysutil::dir(&baseline), "result.json"]), 1 << 20)
                .unwrap_or_default();
        if result.project != self.0.project || result.baseline != baseline {
            return Preparation::default();
        }
        result
    }

    pub(crate) fn prepare(&self, ctx: &Context, args: &AnyMap) -> Result<Preparation> {
        self.prepare_source(ctx, args)
    }

    pub(crate) fn prepare_source(&self, ctx: &Context, args: &AnyMap) -> Result<Preparation> {
        let project = self.0.project.as_str();
        let mut id = text(args, "requestId");
        if id.is_empty() {
            id = agent_id();
        }
        if !operation_id(&id) {
            return Err(fail(
                "invalid_request",
                "invalid source preparation requestId",
            ));
        }
        let directory = join(&[project, "Library/UnityMicroEditor/preparations", &id]);
        let seed = self.source_preparation_seed(args, &directory)?;
        let outcome = self.prepare_seeded(ctx, id, &directory, &seed);
        // A host found by discovery was built for earlier sources. When the
        // stages that bind the host to the current project reject it, say how
        // to get a new one instead of only why it failed. Later failures, such
        // as a C# compile error, are the project's own, except a package
        // assembly the host lacks that could not be compiled as game code.
        let stage = self.0.progress.lock().unwrap().stage.clone();
        match outcome {
            Err(error)
                if !seed.reused_preparation.is_empty()
                    && (matches!(
                        stage.as_str(),
                        "capture_engine_profile" | "build_source_graph"
                    ) || (stage == "compile_scripts"
                        && format!("{error:#}").contains(crate::compiler::HOST_LACKS_PACKAGE)))
                    && super::typed(&error).is_none()
                    && !super::context_error(&error) =>
            {
                Err(super::fail_with(
                    "host_reuse_failed",
                    &format!(
                        "the host reused from preparation {} does not fit the current project: {}; prepare again with a new requestId and rebuildHost: true",
                        seed.reused_preparation,
                        super::error_text(&error)
                    ),
                    obj([
                        ("reusedPreparation", seed.reused_preparation.clone().into()),
                        ("hostPlayer", seed.host_player.clone().into()),
                    ]),
                ))
            }
            outcome => outcome,
        }
    }

    fn prepare_seeded(
        &self,
        ctx: &Context,
        id: String,
        directory: &str,
        seed: &PreparationSeed,
    ) -> Result<Preparation> {
        let (project, editor) = (self.0.project.as_str(), self.0.editor.as_str());
        let directory = directory.to_string();
        let (scene, mut host, mut native, mut profile_path) = (
            seed.scene.clone(),
            seed.host_player.clone(),
            seed.native_catalog.clone(),
            seed.engine_profile.clone(),
        );
        if !authored_scene(&scene) {
            return Err(fail(
                "invalid_request",
                "source preparation requires an explicit saved Assets/...unity scene",
            ));
        }
        let result_path = join(&[&directory, "result.json"]);
        match read_json::<Preparation>(&result_path, 1 << 20) {
            Ok(result) => {
                super::validate_cached_preparation(
                    project, editor, &scene, &id, &directory, &result,
                )?;
                return Ok(result);
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        // Fail on what needs no work to detect before anything is resolved or built.
        if host.is_empty() && self.0.no_editor {
            return Err(fail(
                "native_host_missing",
                "no matching reusable Mono host was supplied; main Editor execution is disabled",
            ));
        }
        self.stage(
            "validate_sources",
            "checking authored metadata identities",
            "",
        );
        self.check_authored_identity(host.is_empty())?;
        // The Player session reads the authored PlayerSettings. No Editor runs
        // in the target any more, so nothing would generate them there.
        if !super::regular(&join(&[project, "ProjectSettings/ProjectSettings.asset"])) {
            return Err(fail(
                "project_settings_missing",
                "the project has no ProjectSettings/ProjectSettings.asset; open it in the Unity Editor once so that Unity writes its settings, the tool does not create authored files in the target",
            ));
        }
        self.stage("resolve_packages", "", "");
        let resolve = || {
            packages::resolve(
                ctx,
                project,
                editor,
                packages::Options::default().with_runtime_package()?,
            )
        };
        let mut resolved = resolve()?;
        let bootstrap_id = phase_id(&id, "engine-profile-bootstrap");
        // A pinned native engine/host profile is bootstrapped once through the
        // explicit Editor backend. Subsequent target graphs never come from Editor.
        let mut bootstrap = Preparation::default();
        if host.is_empty() {
            bootstrap = self.prepare_engine_bootstrap(
                ctx,
                &obj([
                    ("scene", scene.clone().into()),
                    ("requestId", bootstrap_id.clone().into()),
                    ("nativeCatalog", native.clone().into()),
                ]),
            )?;
            host = bootstrap.host_player.clone();
            native = bootstrap.native_catalog.clone();
            resolved = resolve()?;
        }
        self.stage(
            "capture_engine_profile",
            "verifying the host's engine profile",
            "",
        );
        if profile_path.is_empty() {
            profile_path = bootstrap::source_engine_profile_cache_path(project, &host)?;
        }
        let profile = match load_engine_profile(ctx, &profile_path, editor, &host) {
            Ok(profile) => profile,
            Err(error) if not_exist(&error) => {
                let mut reference = if !bootstrap.reference_graph.is_empty() {
                    bootstrap.reference_graph.clone()
                } else {
                    seed.reference_graph.clone()
                };
                if reference.is_empty() {
                    if self.0.no_editor {
                        return Err(fail(
                            "engine_profile_missing",
                            "capture the pinned engine/native host facts once before source-only preparation",
                        ));
                    }
                    bootstrap = self.prepare_engine_bootstrap(
                        ctx,
                        &obj([
                            ("scene", scene.clone().into()),
                            ("hostPlayer", host.clone().into()),
                            ("nativeCatalog", native.clone().into()),
                            ("requestId", bootstrap_id.clone().into()),
                        ]),
                    )?;
                    reference = bootstrap.reference_graph.clone();
                    native = bootstrap.native_catalog.clone();
                    resolved = resolve()?;
                }
                self.stage(
                    "capture_engine_profile",
                    "capturing the host's engine profile",
                    "",
                );
                capture_engine_profile(
                    ctx,
                    project,
                    editor,
                    &host,
                    &reference,
                    &profile_path,
                    Some(&resolved),
                )?
            }
            Err(error) => return Err(error),
        };
        if bootstrap.reference_graph.is_empty() {
            bootstrap =
                bootstrap::recover_engine_bootstrap(ctx, project, &scene, &bootstrap_id, &host)?;
            if native.is_empty() {
                native = bootstrap.native_catalog.clone();
            }
        }
        let mut defaults_profile = String::new();
        // Component defaults serve the component tools, not the baseline:
        // they are warmed only when an Editor runs anyway, for a host build.
        if !bootstrap.reference_graph.is_empty() && !bootstrap.host_evidence.is_empty() {
            // Initial native bootstrap also warms defaults once. Ordinary source-only
            // preparations never start an Editor merely to satisfy component tools.
            self.stage("capture_component_defaults", "", "");
            defaults_profile = self.capture_component_defaults(ctx)?.profile_sha256;
        }
        let editor_profile = profile.clone();
        let options = SourceGraphOptions {
            project: project.into(),
            editor: editor.into(),
            player: host.clone(),
            profile: Some(profile),
            packages: Some(resolved),
            editor_domain: false,
        };
        self.stage("build_source_graph", "", "");
        let graph = build_source_graph(ctx, &options)?;
        let graph_path = join(&[&directory, "graph.json"]);
        agent_immutable_json(&graph_path, &graph)?;
        let proof_path = join(&[&directory, "scene-proof.json"]);
        self.stage("prove_scene", &scene, "");
        super::build_source_scene_proof(ctx, project, editor, &scene, &proof_path)?;
        let compiler_path = join(&[&directory, "compiler.json"]);
        if let Err(error) = stat(&compiler_path) {
            if !not_exist(&error) {
                return Err(error);
            }
            self.stage("compile_scripts", "", "");
            prepare_source_compiler_graph(ctx, &options, &compiler_path)?;
        }
        if native.is_empty() {
            // The import worker brings native content while the baseline is
            // published; a catalog the Editor imports is used only when the
            // host was built here, where the Editor runs anyway.
            if !bootstrap.host_evidence.is_empty() && super::native::may_need_native(project)? {
                if self.0.no_editor {
                    return Err(fail(
                        "native_catalog_missing",
                        "source graph is prepared, but this content requires a verified native catalog",
                    ));
                }
                self.check_authored_identity(true)?;
                native = self.prepare_native(ctx, &scene, &id, &directory)?;
            } else {
                native = super::native::identity_catalog(project, &graph_path, &directory)?;
            }
        }
        // The import worker runs the Editor code of packages from an Editor
        // compilation; without a staging Editor the tool makes one.
        if bootstrap.host_evidence.is_empty()
            && std::env::var_os("UME_EDITOR_COMPILATION").is_none_or(|value| value != "0")
        {
            self.stage("compile_editor_assemblies", "", "");
            crate::native::editor_compilation::ensure(
                ctx,
                project,
                editor,
                &host,
                &editor_profile,
                options.packages.as_ref().expect("resolved packages"),
            )?;
        }
        let native = super::freeze_source_catalog(ctx, project, &directory, &native)?;
        let baseline = join(&[&directory, "current.json"]);
        if let Err(error) = stat(&baseline) {
            if !not_exist(&error) {
                return Err(error);
            }
            self.stage("publish_baseline", "", "");
            super::prepare_agent_project_with_native(
                ctx,
                project,
                editor,
                &compiler_path,
                &scene,
                &baseline,
                &proof_path,
                &native,
            )?;
        }
        let resolved = options.packages.as_ref().expect("resolved packages");
        resolved.validate(ctx)?;
        let artifacts = super::capture_preparation_artifacts(
            ctx,
            project,
            &baseline,
            &join(&[&directory, "artifacts.json"]),
        )?;
        let host_player = host;
        let mut result = Preparation {
            schema_version: 1,
            request_id: id,
            project: project.into(),
            scene,
            baseline,
            host_player: host_player.clone(),
            native_catalog: native,
            engine_profile: profile_path,
            package_snapshot: resolved.snapshot_path.clone(),
            host_evidence: bootstrap.host_evidence.clone(),
            artifact_receipt: artifacts,
            component_defaults_profile: defaults_profile,
            reference_graph: bootstrap.reference_graph.clone(),
            host_content: super::host::host_content(&host_player),
            backends: [
                ("sceneLoader", "raw_source"),
                ("compiler", "dotnet_sdk"),
                ("packages", "package_resolver"),
                ("preparation", "authored_source"),
                ("sceneProof", "source_native_resource"),
                ("build", "unity_editor_proxy"),
            ]
            .map(|(key, value)| (key, value.to_string()))
            .into(),
        };
        if result.reference_graph.is_empty() {
            result.reference_graph = seed.reference_graph.clone();
        }
        if !bootstrap.reference_graph.is_empty() {
            let how = bootstrap.backends.at("nativeBootstrap");
            result.backends.insert(
                "nativeBootstrap",
                match how.is_empty() {
                    true => "unity_editor_public_api_once".to_string(),
                    false => how.to_string(),
                },
            );
        }
        if !seed.reused_preparation.is_empty() {
            result
                .backends
                .insert("hostSource", format!("reused:{}", seed.reused_preparation));
        }
        agent_immutable_json(&result_path, &result)?;
        Ok(result)
    }
}
