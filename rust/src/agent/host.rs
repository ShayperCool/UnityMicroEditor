//! The reusable Mono host built by the Editor in an
//! owned clone of the target project.
use super::{
    Any, AnyMap, Service, UNITY_VERSION, actions::request, error_text, fail, join, not_exist, nz,
    obj, operation_id, read_json, staging::SourceSnapshot, staging::phase_id, text, unmarshal_map,
};
use crate::{
    cancel::Context,
    compiler::{NilMap, Walk, WalkEntry, walk_dir},
    editorbridge::{self, Client},
    project::is_owned_cache_path,
    sysutil,
    util::{agent_canonical, agent_immutable_json, file_hash},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{os::unix::fs::PermissionsExt, time::Duration};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct HostPreparation {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    schema_version: i64,
    #[serde(deserialize_with = "nz")]
    project: String,
    #[serde(deserialize_with = "nz")]
    editor: String,
    #[serde(rename = "requestId", deserialize_with = "nz")]
    request_id: String,
    #[serde(rename = "stageProject", deserialize_with = "nz")]
    stage: String,
    #[serde(deserialize_with = "nz")]
    player: String,
    #[serde(rename = "sourceSnapshotSha256", deserialize_with = "nz")]
    snapshot_sha: String,
    artifacts: NilMap<String>,
    /// A JSON object map: `Null` is the absent map.
    evidence: Any,
}

/// Automatic host construction runs only in an owned
/// clone. Unity build preprocessors can author files (for example
/// Addressables link.xml); none of those changes are published to the target.
/// The caller holds the project action lock and reacquires its target Editor
/// client after this call.
pub(crate) fn prepare_agent_host(
    ctx: &Context,
    service: &Service,
    id: &str,
) -> Result<(String, AnyMap)> {
    let (project, editor) = (service.0.project.as_str(), service.0.editor.as_str());
    if !operation_id(id) || !sysutil::is_abs(project) || !sysutil::is_abs(editor) {
        return Err(fail(
            "invalid_request",
            "host preparation requires an explicit project, Editor and stable requestId",
        ));
    }
    let directory = join(&[
        project,
        "Library/UnityMicroEditor/preparations",
        id,
        "host-stage",
    ]);
    let stage = join(&[&directory, "project"]);
    if !is_owned_cache_path(project, &stage) {
        return Err(anyhow!("host stage is outside owned project caches"));
    }
    agent_canonical(&stage)?;
    agent_immutable_json(
        &join(&[&directory, "request.json"]),
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("project", project.into()),
            ("editor", editor.into()),
            ("requestId", id.into()),
            ("stageProject", stage.clone().into()),
            ("action", "build_host".into()),
            ("unityVersion", UNITY_VERSION.into()),
            ("development", true.into()),
        ])),
    )?;

    let snapshot_path = join(&[&directory, "source-snapshot.json"]);
    let snapshot: SourceSnapshot = match read_json(&snapshot_path, 8 << 20) {
        Ok(snapshot) => snapshot,
        Err(error) if not_exist(&error) => {
            let snapshot = super::snapshot_source(ctx, project)?;
            agent_immutable_json(&snapshot_path, &snapshot)?;
            snapshot
        }
        Err(error) => return Err(error),
    };
    if snapshot.project != project || snapshot.files.is_empty() {
        return Err(anyhow!(
            "host source snapshot belongs to another project or is empty"
        ));
    }
    super::verify_snapshot(ctx, &snapshot)?;
    // Source preservation is checked on failures too. A failed or cancelled
    // build must not turn a changed target into a reusable successful host.
    let outcome = build_host(
        ctx,
        service,
        id,
        &directory,
        &stage,
        &snapshot_path,
        &snapshot,
    );
    let check = Context::background().with_timeout(Duration::from_secs(120));
    let source = super::verify_snapshot(&check, &snapshot);
    check.cancel();
    match (outcome, source) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(source_error)) => Err(source_error),
        // Consumers reach the first typed error; without one the text lists both.
        (Err(error), Err(source_error)) => {
            if super::typed(&error).is_some() {
                Err(error)
            } else if super::typed(&source_error).is_some() {
                Err(source_error)
            } else if super::context_error(&error) {
                Err(error)
            } else if super::context_error(&source_error) {
                Err(source_error)
            } else {
                Err(anyhow!(
                    "{}\n{}",
                    error_text(&error),
                    error_text(&source_error)
                ))
            }
        }
    }
}

fn build_host(
    ctx: &Context,
    service: &Service,
    id: &str,
    directory: &str,
    stage: &str,
    snapshot_path: &str,
    snapshot: &SourceSnapshot,
) -> Result<(String, AnyMap)> {
    let (project, editor) = (service.0.project.as_str(), service.0.editor.as_str());
    let snapshot_sha = file_hash(snapshot_path)?;
    let result_path = join(&[directory, "result.json"]);
    let host_phase = phase_id(id, "host");
    match read_json::<HostPreparation>(&result_path, 8 << 20) {
        Ok(cached) => {
            if cached.schema_version != 1
                || cached.project != project
                || cached.editor != editor
                || cached.request_id != id
                || cached.stage != stage
                || cached.snapshot_sha != snapshot_sha
                || *cached.evidence.field("backend") != Any::from("unity_editor")
                || !cached.evidence.field("sourceSnapshotVerified").is_true()
            {
                return Err(anyhow!(
                    "cached host result lacks exact source/stage/Editor provenance"
                ));
            }
            let artifacts =
                snapshot_host_artifacts(ctx, project, stage, &host_phase, &cached.player)?;
            if artifacts != cached.artifacts {
                return Err(anyhow!(
                    "cached staged host artifacts differ from their confirmed result"
                ));
            }
            let evidence = match cached.evidence {
                Any::Object(map) => map,
                _ => AnyMap::new(),
            };
            return Ok((cached.player, evidence));
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }

    let mut warm = false;
    if let Some(client) = service.client() {
        let connection = client.own_process_or_attach(ctx)?;
        if connection.owned {
            client.close()?;
            service.set_client(None);
            warm = true;
            super::verify_snapshot(ctx, snapshot)?;
        }
    }
    let (cloned, clone_snapshot) = super::clone_staging(ctx, project, directory)?;
    if cloned != stage || snapshot.files != clone_snapshot.files {
        return Err(anyhow!(
            "host clone no longer matches its immutable authored source snapshot"
        ));
    }
    if warm {
        super::native::seed_native_library(ctx, project, stage)?;
    }
    let lifetime = service.0.lifetime.clone().unwrap_or_else(|| ctx.clone());
    let client = Client::new(
        &lifetime,
        editorbridge::Options {
            project: stage.into(),
            editor: editor.into(),
            startup_timeout: Duration::from_secs(600),
            ..Default::default()
        },
    )?;
    struct Close(Client);
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = self.0.close();
        }
    }
    let _close = Close(client.clone());
    request(
        &client,
        ctx,
        &phase_id(id, "host-refresh"),
        "refresh",
        b"{}",
    )
    .1?;
    let (response, data) = request(&client, ctx, &host_phase, "build_host", b"{}");
    let data = data?;
    if response.project != stage
        || response.backend != "unity_editor"
        || response.unity_version != UNITY_VERSION
        || response.editor != editor
    {
        return Err(anyhow!(
            "host response does not belong to the pinned owned stage/Editor backend"
        ));
    }
    let build = unmarshal_map(&data)?.unwrap_or_default();
    let is = |key: &str, expected: &str| build.get(key) == Some(&Any::from(expected));
    let yes = |key: &str| build.get(key).is_some_and(Any::is_true);
    if !is("backend", "unity_editor")
        || !is("buildResult", "Succeeded")
        || !is("purpose", "raw_host")
        || !is("target", "StandaloneLinux64")
        || !is("scriptingBackend", "Mono2x")
        || !yes("hostDevelopment")
        || !yes("development")
        || !is("graphicsApi", "OpenGLCore")
    {
        return Err(anyhow!(
            "host build lacks its successful pinned native Mono/development profile"
        ));
    }
    let host = text(&build, "outputPath");
    let artifacts = snapshot_host_artifacts(ctx, project, stage, &host_phase, &host)?;
    let stage_manifest = join(&[stage, "Packages/manifest.json"]);
    let stage_package_hash = match file_hash(&stage_manifest) {
        Ok(hash) if is("packageProfile", &hash) => hash,
        _ => {
            return Err(anyhow!(
                "host build package profile differs from its staging manifest"
            ));
        }
    };
    client.close()?;
    let final_artifacts = snapshot_host_artifacts(ctx, project, stage, &host_phase, &host)?;
    if artifacts != final_artifacts {
        return Err(anyhow!(
            "staged host artifacts changed while its owned Editor closed"
        ));
    }
    if !file_hash(&stage_manifest).is_ok_and(|profile| profile == stage_package_hash) {
        return Err(anyhow!(
            "staged host package profile changed while its owned Editor closed"
        ));
    }
    super::verify_snapshot(ctx, snapshot)?;
    let evidence = obj([
        ("backend", "unity_editor".into()),
        ("purpose", "isolated_raw_host".into()),
        ("sourceProject", project.into()),
        ("stageProject", stage.into()),
        ("receipt", response.receipt.clone().into()),
        ("requestId", response.request_id.clone().into()),
        ("sourceSnapshot", snapshot_path.into()),
        ("sourceSnapshotSha256", snapshot_sha.clone().into()),
        ("sourceSnapshotVerified", true.into()),
        (
            "sourcePackageProfile",
            snapshot.files.at("Packages/manifest.json").into(),
        ),
        ("stagePackageProfile", stage_package_hash.into()),
        ("warmLibraryFromOwnedStoppedEditor", warm.into()),
        ("build", Any::Object(build)),
    ]);
    let record = HostPreparation {
        schema_version: 1,
        project: project.into(),
        editor: editor.into(),
        request_id: id.into(),
        stage: stage.into(),
        player: host.clone(),
        snapshot_sha,
        artifacts,
        evidence: Any::Object(evidence.clone()),
    };
    agent_immutable_json(&result_path, &record)?;
    Ok((host, evidence))
}

/// The digests of the staged host build.
pub(crate) fn snapshot_host_artifacts(
    ctx: &Context,
    project: &str,
    stage: &str,
    phase: &str,
    player: &str,
) -> Result<NilMap<String>> {
    let directory = join(&[stage, "Builds/UnityMicroEditor", phase]);
    if player != join(&[&directory, "Project.x86_64"]) || !is_owned_cache_path(project, player) {
        return Err(anyhow!(
            "host Player is outside its exact owned stage build output"
        ));
    }
    agent_canonical(player)?;
    let executable = std::fs::symlink_metadata(player).is_ok_and(|info| {
        info.is_file() && info.len() != 0 && info.permissions().mode() & 0o111 != 0
    });
    if !executable {
        return Err(anyhow!(
            "staged host Player must be a nonempty executable regular file"
        ));
    }
    let mut result: NilMap<String> = NilMap::make();
    walk_dir(&directory, &mut |entry: &WalkEntry| {
        ctx.err()?;
        if entry.kind.is_symlink() {
            return Err(anyhow!("staged host artifacts contain a symlink"));
        }
        if entry.is_dir() {
            return Ok(Walk::Continue);
        }
        if result.len() >= 65536 {
            return Err(anyhow!("staged host exceeds its artifact inventory bound"));
        }
        if !super::regular(entry.path) {
            return Err(anyhow!("staged host artifact is not a regular file"));
        }
        let logical = match sysutil::rel(&directory, entry.path) {
            Some(logical) if !logical.starts_with("..") => logical,
            _ => return Err(anyhow!("staged host artifact escaped its owned output")),
        };
        result.insert(logical, file_hash(entry.path)?);
        Ok(Walk::Continue)
    })?;
    if result
        .at("Project_Data/Managed/UnityMicroEditor.dll")
        .is_empty()
        || result
            .at("Project_Data/ScriptingAssemblies.json")
            .is_empty()
    {
        return Err(anyhow!(
            "staged host lacks its native runtime bridge/Mono registrations"
        ));
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Slots {
        #[serde(deserialize_with = "nz")]
        names: Vec<String>,
    }
    let slots: Slots = read_json(
        &join(&[&directory, "Project_Data/ScriptingAssemblies.json"]),
        2 << 20,
    )?;
    if slots
        .names
        .iter()
        .any(|name| name == "UnityMicroEditor.dll")
    {
        return Ok(result);
    }
    Err(anyhow!(
        "staged host has no registered native runtime bridge slot"
    ))
}
