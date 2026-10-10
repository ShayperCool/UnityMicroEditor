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
            &Any::Object(obj([
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
            ])),
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
        agent_immutable_json(&seed_path, &frozen)?;
        Ok(frozen)
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
        let (project, editor) = (self.0.project.as_str(), self.0.editor.as_str());
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
        let resolve = || packages::resolve(ctx, project, editor, packages::Options::default());
        let mut resolved = resolve()?;
        let bootstrap_id = phase_id(&id, "engine-profile-bootstrap");
        // A pinned native engine/host profile is bootstrapped once through the
        // explicit Editor backend. Subsequent target graphs never come from Editor.
        let mut bootstrap = Preparation::default();
        if host.is_empty() {
            if self.0.no_editor {
                return Err(fail(
                    "native_host_missing",
                    "no matching reusable Mono host was supplied; main Editor execution is disabled",
                ));
            }
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
        if !bootstrap.reference_graph.is_empty() {
            // Initial native bootstrap also warms defaults once. Ordinary source-only
            // preparations never start an Editor merely to satisfy component tools.
            defaults_profile = self.capture_component_defaults(ctx)?.profile_sha256;
        }
        let options = SourceGraphOptions {
            project: project.into(),
            editor: editor.into(),
            player: host.clone(),
            profile: Some(profile),
            packages: Some(resolved),
        };
        let graph = build_source_graph(ctx, &options)?;
        let graph_path = join(&[&directory, "graph.json"]);
        agent_immutable_json(&graph_path, &graph)?;
        let proof_path = join(&[&directory, "scene-proof.json"]);
        super::build_source_scene_proof(ctx, project, editor, &scene, &proof_path)?;
        let compiler_path = join(&[&directory, "compiler.json"]);
        if let Err(error) = stat(&compiler_path) {
            if !not_exist(&error) {
                return Err(error);
            }
            prepare_source_compiler_graph(ctx, &options, &compiler_path)?;
        }
        if native.is_empty() {
            if super::native::may_need_native(project)? {
                if self.0.no_editor {
                    return Err(fail(
                        "native_catalog_missing",
                        "source graph is prepared, but this content requires a verified native catalog",
                    ));
                }
                native = self.prepare_native(ctx, &scene, &id, &directory)?;
            } else {
                native = super::native::identity_catalog(project, &graph_path, &directory)?;
            }
        }
        let native = super::freeze_source_catalog(ctx, project, &directory, &native)?;
        let baseline = join(&[&directory, "current.json"]);
        if let Err(error) = stat(&baseline) {
            if !not_exist(&error) {
                return Err(error);
            }
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
        let mut result = Preparation {
            schema_version: 1,
            request_id: id,
            project: project.into(),
            scene,
            baseline,
            host_player: host,
            native_catalog: native,
            engine_profile: profile_path,
            package_snapshot: resolved.snapshot_path.clone(),
            host_evidence: bootstrap.host_evidence.clone(),
            artifact_receipt: artifacts,
            component_defaults_profile: defaults_profile,
            reference_graph: bootstrap.reference_graph.clone(),
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
            result.backends.insert(
                "nativeBootstrap",
                "unity_editor_public_api_once".to_string(),
            );
        }
        agent_immutable_json(&result_path, &result)?;
        Ok(result)
    }
}
