//! Native bootstrap captures engine facts only.
//! Target SDK assemblies and the playable baseline are always produced by the
//! independent source coordinator.
use super::{
    Any, AnyMap, Preparation, Service, actions::request, authored_scene, join, join_errors,
    not_exist, obj, operation_id, read_json, staging::phase_id, stat, text, unmarshal_any,
};
use crate::{
    cancel::Context,
    compiler::{NilMap, Walk, WalkEntry, walk_dir},
    project::is_owned_cache_path,
    sha256,
    sourcegraph::managed_directory,
    sysutil,
    util::{agent_canonical, agent_immutable_json, file_hash, read_agent_file},
};
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

impl Service {
    pub(crate) fn prepare_engine_bootstrap(
        &self,
        ctx: &Context,
        args: &AnyMap,
    ) -> Result<Preparation> {
        let project = self.0.project.as_str();
        let (id, scene, mut host) = (
            text(args, "requestId"),
            text(args, "scene"),
            text(args, "hostPlayer"),
        );
        if !operation_id(&id) || !authored_scene(&scene) {
            return Err(super::fail(
                "invalid_request",
                "engine bootstrap requires a stable requestId and explicit Assets/...unity scene",
            ));
        }
        let directory = join(&[project, "Library/UnityMicroEditor/preparations", &id]);
        agent_immutable_json(
            &join(&[&directory, "request.json"]),
            &Any::Object(obj([
                ("schemaVersion", 2.into()),
                ("backend", "native_engine_bootstrap".into()),
                ("project", project.into()),
                ("scene", scene.clone().into()),
                ("hostPlayer", host.clone().into()),
                ("editor", self.0.editor.clone().into()),
                ("nativeCatalog", text(args, "nativeCatalog").into()),
            ])),
        )?;
        let result_path = join(&[&directory, "result.json"]);
        match read_json::<Preparation>(&result_path, 1 << 20) {
            Ok(result) => {
                validate_engine_bootstrap(ctx, project, &scene, &id, &directory, &result)?;
                return Ok(result);
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        let mut client = self.editor_client(ctx)?;
        let phase = |name: &str| phase_id(&id, name);
        request(&client, ctx, &phase("initial-refresh"), "refresh", b"{}").1?;
        let mut host_evidence = String::new();
        if host.is_empty() {
            let (player, evidence) = super::host::prepare_agent_host(ctx, self, &id)?;
            host = player;
            host_evidence = join(&[&directory, "host-evidence.json"]);
            agent_immutable_json(&host_evidence, &Any::Object(evidence))?;
            client = self.editor_client(ctx)?;
        }
        request(&client, ctx, &phase("refresh"), "refresh", b"{}").1?;
        let graph = request(&client, ctx, &phase("graph"), "export_graph", b"{}").1?;
        let graph_path = join(&[&directory, "graph.json"]);
        agent_immutable_json(&graph_path, &unmarshal_any(&graph)?)?;
        let mut native = text(args, "nativeCatalog");
        if native.is_empty() {
            let candidate = join(&[&directory, "native-catalog.json"]);
            match stat(&candidate) {
                Ok(_) => native = candidate,
                Err(error) if !not_exist(&error) => return Err(error),
                Err(_) => {
                    native = if super::native::may_need_native(project)? {
                        self.prepare_native(ctx, &scene, &id, &directory)?
                    } else {
                        super::native::identity_catalog(project, &graph_path, &directory)?
                    };
                }
            }
        }
        let files =
            engine_bootstrap_files(ctx, project, &host, &graph_path, &native, &host_evidence)?;
        let artifacts = join(&[&directory, "artifacts.json"]);
        agent_immutable_json(&artifacts, &files)?;
        let result = Preparation {
            schema_version: 1,
            request_id: id,
            project: project.into(),
            scene,
            host_player: host,
            native_catalog: native,
            host_evidence,
            reference_graph: graph_path,
            artifact_receipt: artifacts,
            backends: [
                ("preparation", "native_engine_bootstrap"),
                ("nativeBootstrap", "unity_editor_public_api_once"),
            ]
            .map(|(key, value)| (key, value.to_string()))
            .into(),
            ..Default::default()
        };
        agent_immutable_json(&result_path, &result)?;
        Ok(result)
    }
}

/// The digests of everything a bootstrap produced
/// or was given.
pub(crate) fn engine_bootstrap_files(
    ctx: &Context,
    project: &str,
    host: &str,
    graph: &str,
    catalog: &str,
    evidence: &str,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let mut add = |path: &str, supplied_input: bool| -> Result<()> {
        ctx.err()?;
        if !sysutil::is_abs(path)
            || (!supplied_input && !is_owned_cache_path(project, path))
            || agent_canonical(path).is_err()
        {
            return Err(anyhow!(
                "engine bootstrap artifact is not canonical or its generated output is outside owned caches"
            ));
        }
        if !super::regular(path) {
            return Err(anyhow!(
                "engine bootstrap artifact is not a regular file: {path}"
            ));
        }
        if result.len() >= 8192 {
            return Err(anyhow!(
                "engine bootstrap artifact inventory exceeds 8192 files"
            ));
        }
        result.insert(path.to_string(), file_hash(path)?);
        Ok(())
    };
    // Explicit host/catalog inputs may live outside the target cache. This
    // inventory only reads them; generated graph and receipt writes stay owned.
    add(host, true)?;
    walk_dir(&sysutil::dir(host), &mut |entry: &WalkEntry| {
        if entry.kind.is_symlink() {
            return Err(anyhow!("engine bootstrap host contains a symlink"));
        }
        if !entry.is_dir() {
            add(entry.path, true)?;
        }
        Ok(Walk::Continue)
    })?;
    for (path, supplied_input) in [(graph, false), (catalog, true), (evidence, false)] {
        if !path.is_empty() {
            add(path, supplied_input)?;
        }
    }
    Ok(result)
}

pub(crate) fn validate_engine_bootstrap(
    ctx: &Context,
    project: &str,
    scene: &str,
    id: &str,
    directory: &str,
    result: &Preparation,
) -> Result<()> {
    if result.schema_version != 1
        || result.project != project
        || result.scene != scene
        || result.request_id != id
        || !result.baseline.is_empty()
        || result.reference_graph != join(&[directory, "graph.json"])
        || result.artifact_receipt != join(&[directory, "artifacts.json"])
        || result.backends.at("preparation") != "native_engine_bootstrap"
        || result.native_catalog.is_empty()
    {
        return Err(anyhow!(
            "cached native engine bootstrap has an invalid identity"
        ));
    }
    let accepted: NilMap<String> = read_json(&result.artifact_receipt, 4 << 20)?;
    let current = engine_bootstrap_files(
        ctx,
        project,
        &result.host_player,
        &result.reference_graph,
        &result.native_catalog,
        &result.host_evidence,
    )?;
    if accepted.len() != current.len() || current.len() < 3 {
        return Err(anyhow!(
            "cached native engine bootstrap artifact inventory changed"
        ));
    }
    // Visited in path order.
    for (path, hash) in &current {
        if accepted.at(path) != hash {
            return Err(anyhow!(
                "cached native engine bootstrap artifact changed: {path}"
            ));
        }
    }
    Ok(())
}

/// Profile capture can finish before defaults
/// warming fails. A retry must keep that request's native-bootstrap obligation
/// even when the profile now exists.
pub(crate) fn recover_engine_bootstrap(
    ctx: &Context,
    project: &str,
    scene: &str,
    id: &str,
    host: &str,
) -> Result<Preparation> {
    let directory = join(&[project, "Library/UnityMicroEditor/preparations", id]);
    let result: Preparation = match read_json(&join(&[&directory, "result.json"]), 1 << 20) {
        Ok(result) => result,
        Err(error) if not_exist(&error) => return Ok(Preparation::default()),
        Err(error) => return Err(error),
    };
    if result.host_player != host {
        return Err(anyhow!(
            "cached native bootstrap host differs from this source preparation"
        ));
    }
    validate_engine_bootstrap(ctx, project, scene, id, &directory, &result)?;
    Ok(result)
}

/// Unity's launch executable is shared by
/// different builds. Include the exact host location and compiled-source proof
/// rather than using that executable's digest as the identity of every host in
/// this project.
pub(crate) fn source_engine_profile_cache_path(project: &str, host: &str) -> Result<String> {
    if !sysutil::is_abs(host) || agent_canonical(host).is_err() {
        return Err(anyhow!("engine profile requires a canonical absolute host"));
    }
    let host_data = read_agent_file(host, 32 << 20)?;
    let proof_path = join(&[
        &sysutil::dir(&managed_directory(host)),
        "UnityMicroEditorSourceProfile.json",
    ]);
    let proof_data = read_agent_file(&proof_path, 32 << 20).map_err(|error| {
        join_errors("host lacks its compiled-source profile proof", Some(&error))
    })?;
    let identity = super::marshal(&BTreeMap::from([
        ("host", host.to_string()),
        ("executableSHA256", sha256(&host_data)),
        ("sourceProofSHA256", sha256(&proof_data)),
    ]))?;
    Ok(join(&[
        project,
        "Library/UnityMicroEditor/engine-profiles",
        &sha256(&identity),
        "profile.json",
    ]))
}
