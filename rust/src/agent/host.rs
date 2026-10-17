//! The reusable Mono host built by the Editor in the
//! owned staging copy of the target project.
use super::{
    Any, AnyMap, Service, UNITY_VERSION, actions::request, error_text, fail, join, not_exist, nz,
    obj, operation_id, read_json, regular, staging::SourceSnapshot, staging::phase_id, text,
    unmarshal_any, unmarshal_map,
};
use crate::{
    cancel::Context,
    compiler::{NilMap, Walk, WalkEntry, walk_dir},
    editorbridge::{self, Client},
    packages,
    project::is_owned_cache_path,
    sysutil,
    util::{agent_canonical, agent_immutable_json, file_hash},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

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

/// Automatic host construction runs only in the owned staging copy, which
/// every preparation of the project shares: a rebuild starts from the import,
/// script and Player build caches of the build before it. Unity build
/// preprocessors can author files (for example Addressables link.xml); none
/// of those changes are published to the target.
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
    let stage = super::staging::shared_stage(project);
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
    let snapshot: SourceSnapshot = match read_json(&snapshot_path, super::staging::SNAPSHOT_MAXIMUM)
    {
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
                || (cached.stage != stage && cached.stage != join(&[directory, "project"]))
                || cached.snapshot_sha != snapshot_sha
                || *cached.evidence.field("backend") != Any::from("unity_editor")
                || !cached.evidence.field("sourceSnapshotVerified").is_true()
            {
                return Err(anyhow!(
                    "cached host result lacks exact source/stage/Editor provenance"
                ));
            }
            // A host from before the shared stage stays in its own clone.
            let artifacts =
                snapshot_host_artifacts(ctx, project, &cached.stage, &host_phase, &cached.player)?;
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

    let (synced, stage_snapshot) = super::sync_staging(ctx, project)?;
    if synced != stage || snapshot.files != stage_snapshot.files {
        return Err(anyhow!(
            "host stage no longer matches its immutable authored source snapshot"
        ));
    }
    let (warm, owned_warm) = super::native::seed_stage_library(ctx, service, stage)?;
    super::verify_snapshot(ctx, snapshot)?;
    let client = stage_editor(ctx, service, stage)?;
    let _close = Close(client.clone());
    service.stage(
        "build_host",
        "building the reusable Mono host Player in the owned staging copy",
        &join(&[client.mailbox(), "editor.log"]),
    );
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
    // Unity's CodeGen step leaves the unstripped Netcode runtime only in the
    // stage Library. The processor build needs it wherever this host is used,
    // so it travels with the host.
    let codegen = join(&[
        stage,
        "Library/Bee/artifacts/StandaloneLinux64_CodeGen/Unity.Netcode.Runtime.dll",
    ]);
    if regular(&codegen) {
        let directory = join(&[
            &crate::compiler::player_data(&host),
            crate::compiler::HOST_CODEGEN_DIRECTORY,
        ]);
        agent_canonical(&directory)?;
        sysutil::mkdir_all(&directory)?;
        let destination = join(&[&directory, "Unity.Netcode.Runtime.dll"]);
        std::fs::copy(&codegen, &destination)
            .map_err(|error| sysutil::Error::os("copy", &destination, &error))?;
    }
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
    // The same stage Editor exports the public Player graph: the target
    // project never has to run an Editor or declare the runtime package.
    service.stage(
        "export_engine_graph",
        "exporting the public Player assembly graph from the staging copy",
        &join(&[client.mailbox(), "editor.log"]),
    );
    export_stage_graph(&client, ctx, id, &join(&[directory, STAGE_GRAPH]))?;
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
        ("warmLibraryFromOwnedStoppedEditor", owned_warm.into()),
        ("warmLibraryFromStoppedProject", warm.into()),
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

pub(crate) const STAGE_GRAPH: &str = "stage-graph.json";

struct Close(Client);
impl Drop for Close {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

fn stage_editor(ctx: &Context, service: &Service, stage: &str) -> Result<Client> {
    if service.0.no_editor {
        return Err(fail(
            "editor_disabled",
            "Editor execution is disabled; provide a compatible cached engine/host/native profile",
        ));
    }
    let lifetime = service.0.lifetime.clone().unwrap_or_else(|| ctx.clone());
    Ok(Client::new(
        &lifetime,
        editorbridge::Options {
            project: stage.into(),
            editor: service.0.editor.clone(),
            startup_timeout: Duration::from_secs(600),
            ..Default::default()
        },
    )?)
}

fn export_stage_graph(client: &Client, ctx: &Context, id: &str, path: &str) -> Result<()> {
    if regular(path) {
        return Ok(());
    }
    let graph = request(
        client,
        ctx,
        &phase_id(id, "stage-graph"),
        "export_graph",
        b"{}",
    )
    .1?;
    agent_immutable_json(path, &unmarshal_any(&graph)?)
}

/// The public Player graph exported by an Editor that runs in the owned
/// stage, as that Editor reported it: its paths still name the stage. A host
/// build leaves this file in `directory`; a supplied host has the stage
/// brought up to date for the export alone.
pub(crate) fn stage_graph(
    ctx: &Context,
    service: &Service,
    id: &str,
    directory: &str,
) -> Result<(String, String)> {
    let path = join(&[directory, STAGE_GRAPH]);
    if regular(&path) {
        // A graph from before the shared stage names its own clone.
        let legacy = join(&[directory, "project"]);
        let stage = if sysutil::lstat(&legacy).is_ok_and(|info| info.is_dir()) {
            legacy
        } else {
            super::staging::shared_stage(&service.0.project)
        };
        return Ok((stage, path));
    }
    let (stage, _) = super::sync_staging(ctx, &service.0.project)?;
    super::native::seed_stage_library(ctx, service, &stage)?;
    let client = stage_editor(ctx, service, &stage)?;
    let _close = Close(client.clone());
    service.stage(
        "export_engine_graph",
        "exporting the public Player assembly graph from the owned staging copy",
        &join(&[client.mailbox(), "editor.log"]),
    );
    request(
        &client,
        ctx,
        &phase_id(id, "stage-refresh"),
        "refresh",
        b"{}",
    )
    .1?;
    export_stage_graph(&client, ctx, id, &path)?;
    client.close()?;
    Ok((stage, path))
}

/// Rewrites a graph exported in `stage` so that it describes `project`. The
/// stage holds the authored roots byte for byte, so their paths move to the
/// project. A registered package root inside the stage's own PackageCache
/// moves to the root our resolver verified for the same package; anything
/// else below the stage `Library` stays there, where it exists.
pub(crate) fn retarget_stage_graph(
    graph: &Any,
    stage: &str,
    project: &str,
    resolved: &packages::Snapshot,
) -> Result<Any> {
    let prefix = format!("{stage}/");
    let mut roots: Vec<(String, String)> = Vec::new();
    if let Any::Array(entries) = graph.field("sourceRoots") {
        for entry in entries {
            let (logical, source) = (
                super::text_of(entry, "path"),
                super::text_of(entry, "source"),
            );
            if !source.starts_with(&format!("{prefix}Library/")) {
                continue;
            }
            if let Some(package) = resolved
                .packages
                .iter()
                .find(|package| package.logical_root == logical)
            {
                roots.push((source, package.path.clone()));
            }
        }
    }
    let path = |text: &str| -> Option<String> {
        if text == stage {
            return Some(project.into());
        }
        let relative = text.strip_prefix(&prefix)?;
        if ["Assets", "Packages", "ProjectSettings"]
            .iter()
            .any(|root| relative == *root || relative.starts_with(&format!("{root}/")))
        {
            return Some(join(&[project, relative]));
        }
        roots.iter().find_map(|(source, target)| {
            if text == source {
                return Some(target.clone());
            }
            let rest = text.strip_prefix(source)?.strip_prefix('/')?;
            Some(join(&[target, rest]))
        })
    };
    fn walk(value: &Any, path: &dyn Fn(&str) -> Option<String>) -> Any {
        match value {
            Any::String(text) => path(text).map_or_else(|| value.clone(), Any::String),
            Any::Array(items) => Any::Array(items.iter().map(|item| walk(item, path)).collect()),
            Any::Object(members) => Any::Object(
                members
                    .iter()
                    .map(|(key, member)| {
                        (path(key).unwrap_or_else(|| key.clone()), walk(member, path))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut retargeted = walk(graph, &path);
    if retargeted.field("project").as_str() != Some(project) {
        return Err(anyhow!(
            "staged Player graph does not belong to its owned stage"
        ));
    }
    // The staged manifest declares the runtime package and absolute local
    // paths. Authored package inputs carry the target's own bytes.
    if let Some(Any::Object(inputs)) = retargeted
        .as_object_mut()
        .and_then(|graph| graph.get_mut("compilerInputs"))
    {
        for name in ["Packages/manifest.json", "Packages/packages-lock.json"] {
            let authored = join(&[project, name]);
            if !inputs.contains_key(&authored) {
                continue;
            }
            match file_hash(&authored) {
                Ok(hash) => inputs.insert(authored, hash.into()),
                Err(_) => inputs.remove(&authored),
            };
        }
    }
    Ok(retargeted)
}

/// The Addressables output of the host build, by kind. Build processors run
/// in the staging copy, so remote groups land in that copy's `ServerData` and
/// local groups in the host's StreamingAssets, never in the target project.
/// The content is not rebuilt when assets change. Local groups are as old as
/// their host; the staging copy is shared, so its `ServerData` holds the
/// remote groups of the latest host build.
pub(crate) fn host_content(host: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let has_files = |directory: &str| {
        std::fs::read_dir(directory).is_ok_and(|mut entries| entries.next().is_some())
    };
    let local = join(&[&crate::compiler::player_data(host), "StreamingAssets/aa"]);
    if has_files(&local) {
        found.insert("addressablesStreamingAssets".to_string(), local);
    }
    // <stage>/Builds/UnityMicroEditor/<phase>/Project.x86_64
    let mut stage = host.to_string();
    for _ in 0..4 {
        stage = sysutil::dir(&stage);
    }
    if regular(&join(&[
        &stage,
        "ProjectSettings/UnityMicroEditorStaging.json",
    ])) {
        let remote = join(&[&stage, "ServerData/StandaloneLinux64"]);
        if has_files(&remote) {
            found.insert("addressablesRemoteBuild".to_string(), remote);
        }
    }
    found
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

/// The public Player graph of a supplied host, without an Editor: the build
/// proof the host build left beside the Player (`UnityMicroEditorSourceProfile.json`)
/// holds, per host assembly, the configuration, sources, defines and compiled
/// references that an Editor export of the graph would report. Its logical
/// paths are placed where our resolver put the packages of `project`; an
/// assembly of a package the project does not resolve keeps its logical
/// path, and the engine profile then does not prove it. The graph has no
/// game assemblies: the profile takes its defines and references from the
/// host assemblies alone.
pub(crate) fn proof_graph(
    project: &str,
    editor: &str,
    host: &str,
    resolved: &packages::Snapshot,
) -> Result<Any> {
    let data = crate::compiler::player_data(host);
    let proof_path = join(&[&data, "UnityMicroEditorSourceProfile.json"]);
    let proof: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&proof_path)
            .map_err(|error| anyhow!("host build proof {proof_path}: {error}"))?,
    )?;
    for (field, expected) in [
        ("backend", "unity_editor_build_sources"),
        ("unityVersion", UNITY_VERSION),
        ("target", "StandaloneLinux64"),
        ("scriptingBackend", "Mono2x"),
    ] {
        if proof.get(field).and_then(|value| value.as_str()) != Some(expected) {
            return Err(anyhow!(
                "host build proof is not a Linux Mono build of {UNITY_VERSION}: {field}"
            ));
        }
    }
    let physical = |logical: &str| -> String {
        if sysutil::is_abs(logical) {
            return logical.to_string();
        }
        if let Some(rest) = logical.strip_prefix("Assets/") {
            return join(&[project, "Assets", rest]);
        }
        resolved
            .packages
            .iter()
            .filter_map(|package| {
                let rest = logical.strip_prefix(&format!("{}/", package.logical_root))?;
                Some(join(&[&package.path, rest]))
            })
            .next()
            .unwrap_or_else(|| logical.to_string())
    };
    let paths = |value: Option<&serde_json::Value>| -> Vec<serde_json::Value> {
        value
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|item| physical(item).into())
                    .collect()
            })
            .unwrap_or_default()
    };
    let managed = join(&[&data, "Managed"]);
    let mut hosts = Vec::new();
    if let Some(assemblies) = proof.get("assemblies").and_then(|value| value.as_object()) {
        for (name, assembly) in assemblies {
            let mut configuration = assembly
                .get("configuration")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let sources = paths(configuration.get("sources"));
            let responses = paths(configuration.get("responseFiles"));
            let defines = configuration
                .get("defines")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            if let Some(object) = configuration.as_object_mut() {
                object.insert("sources".into(), sources.clone().into());
                object.insert("responseFiles".into(), responses.clone().into());
            }
            let references: Vec<serde_json::Value> = assembly
                .get("references")
                .and_then(|value| value.as_object())
                .map(|references| references.keys().map(|key| physical(key).into()).collect())
                .unwrap_or_default();
            let mut editor_defines = defines.as_array().cloned().unwrap_or_default();
            for define in ["UNITY_EDITOR", "UNITY_EDITOR_64", "UNITY_EDITOR_LINUX"] {
                editor_defines.push(define.into());
            }
            hosts.push(serde_json::json!({
                "name": name,
                "path": join(&[&managed, &format!("{name}.dll")]),
                "sha256": assembly.get("dllSha256").cloned().unwrap_or_default(),
                "sources": sources,
                "defines": defines,
                "editorDefines": editor_defines,
                "configuration": configuration,
                "compiledReferences": references,
                "responseFiles": responses,
            }));
        }
    }
    let roots: Vec<serde_json::Value> = resolved
        .packages
        .iter()
        .map(|package| serde_json::json!({"path": package.logical_root, "source": package.path}))
        .collect();
    let graph = serde_json::json!({
        "schemaVersion": 1,
        "backend": "unity_editor",
        "development": true,
        "unityVersion": UNITY_VERSION,
        "target": "StandaloneLinux64",
        "scriptingBackend": "Mono2x",
        "project": project,
        "unity": editor,
        "assemblies": [],
        "scripts": [],
        "compilerInputs": {},
        "hostAssemblies": hosts,
        "sourceRoots": roots,
    });
    unmarshal_any(&serde_json::to_vec(&graph)?)
}
