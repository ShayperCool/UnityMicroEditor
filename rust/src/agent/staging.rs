//! Authored source snapshots, owned staging clones and the source prefab action.
use super::{
    Any, AnyMap, Service, fail, join, json_kind, lstat, map_reply, marshal, not_exist, nz, obj,
    operation_id, read_json, store_error, text, to_serde, unmarshal_map,
};
use crate::{
    authoring::{
        NativeDefaultsTemplate, PrefabEditRequest, PublicationInput, StagedPublicationRequest,
        Store,
    },
    cancel::Context,
    compiler::{NilMap, NilVec, Walk, WalkEntry, compiler_command, walk_dir},
    jsondoc,
    mcp::HandlerValue,
    project::{is_owned_cache_path, lexpath, value},
    sha256, sysutil,
    util::{agent_canonical, agent_immutable_json, agent_read_json, file_hash, read_agent_file},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSnapshot {
    #[serde(deserialize_with = "nz")]
    pub project: String,
    pub inputs: NilVec<SnapshotInput>,
    pub files: NilMap<String>,
}
/// A `PublicationInput` as the snapshot stores it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SnapshotInput {
    #[serde(deserialize_with = "nz")]
    pub path: String,
    #[serde(deserialize_with = "nz")]
    pub expected_revision: String,
}
impl From<&SnapshotInput> for PublicationInput {
    fn from(input: &SnapshotInput) -> Self {
        PublicationInput {
            path: input.path.clone(),
            expected_revision: input.expected_revision.clone(),
        }
    }
}

pub(crate) fn phase_id(id: &str, phase: &str) -> String {
    format!("{}-{phase}", &sha256(id.as_bytes())[..32])
}

/// Every authored file with its hash, and the
/// dependency revision of every non-metadata source.
pub fn snapshot_source(ctx: &Context, project: &str) -> Result<SourceSnapshot> {
    let mut result = SourceSnapshot {
        project: project.into(),
        inputs: NilVec::nil(),
        files: NilMap::make(),
    };
    let store = Store::new(Path::new(project)).map_err(store_error)?;
    let mut sources: Vec<String> = Vec::new();
    for folder in ["Assets", "Packages", "ProjectSettings"] {
        walk_dir(&join(&[project, folder]), &mut |entry: &WalkEntry| {
            ctx.err()?;
            if entry.kind.is_symlink() {
                return Err(anyhow!(
                    "staging refuses authored source symlinks: {}",
                    entry.path
                ));
            }
            if entry.is_dir() {
                return Ok(Walk::Continue);
            }
            if !lstat(entry.path)?.is_regular() {
                return Err(anyhow!(
                    "staging source is not a regular file: {}",
                    entry.path
                ));
            }
            let logical = sysutil::rel(project, entry.path)
                .ok_or_else(|| anyhow!("Rel: can't make {} relative to {project}", entry.path))?;
            result.files.insert(logical.clone(), file_hash(entry.path)?);
            if !logical.ends_with(".meta") {
                sources.push(logical);
            }
            Ok(Walk::Continue)
        })?;
    }
    if sources.len() > 4096 {
        return Err(fail(
            "project_too_large",
            "staged publication currently supports at most 4096 source dependencies",
        ));
    }
    sources.sort();
    for path in sources {
        ctx.err()?;
        let revision = store.dependency_revision(&path).map_err(store_error)?;
        result.inputs.push(SnapshotInput {
            path,
            expected_revision: revision,
        });
    }
    Ok(result)
}

/// The authored inventory still has the snapshot's bytes.
pub fn verify_snapshot(ctx: &Context, snapshot: &SourceSnapshot) -> Result<()> {
    let current = snapshot_source(ctx, &snapshot.project)?;
    if marshal(&snapshot.files)? != marshal(&current.files)? {
        return Err(fail(
            "revision_conflict",
            "authored source inventory changed while Editor prepared staged content",
        ));
    }
    Ok(())
}

/// Relative `file:` packages of the source
/// project become absolute in its clone.
fn normalize_staged_package_paths(source: &str, stage: &str) -> Result<()> {
    let path = join(&[stage, "Packages/manifest.json"]);
    let data = read_agent_file(&path, 1 << 20)?;
    let mut manifest = unmarshal_map(&data)?.unwrap_or_default();
    let Some(Any::Object(dependencies)) = manifest.get_mut("dependencies") else {
        return Err(anyhow!("staged package manifest lacks dependencies"));
    };
    let mut changed = false;
    // Visited in key order.
    for location in dependencies.values_mut() {
        let Any::String(text) = location else {
            continue;
        };
        let Some(physical) = text.strip_prefix("file:") else {
            continue;
        };
        if sysutil::is_abs(physical) {
            continue;
        }
        let canonical = lexpath::eval_symlinks(&join(&[source, "Packages", physical]))?;
        *location = format!("file:{canonical}").into();
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let mut encoded = jsondoc::marshal_indent(&Any::Object(manifest)).map_err(|e| anyhow!(e))?;
    encoded.push(b'\n');
    fs::write(&path, encoded).map_err(|error| sysutil::Error::os("open", &path, &error))?;
    Ok(())
}

/// An owned copy of the authored roots below `directory`,
/// with the runtime package installed and its completion recorded once.
pub fn clone_staging(
    ctx: &Context,
    source: &str,
    directory: &str,
) -> Result<(String, SourceSnapshot)> {
    let stage = join(&[directory, "project"]);
    agent_canonical(&stage)?;
    let marker = join(&[directory, "clone-complete.json"]);
    match read_json::<SourceSnapshot>(&marker, 8 << 20) {
        Ok(snapshot) => {
            if !sysutil::lstat(&stage).is_ok_and(|info| info.is_dir()) {
                return Err(anyhow!(
                    "completed staging project is missing or not a regular directory"
                ));
            }
            if snapshot.project != source {
                return Err(anyhow!("staging cache belongs to another project"));
            }
            return Ok((stage, snapshot));
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }
    if !matches!(sysutil::lstat(&stage), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(fail(
            "staging_incomplete",
            "staging directory exists without a completed snapshot; inspect it and use a new operationId",
        ));
    }
    let snapshot = snapshot_source(ctx, source)?;
    sysutil::mkdir_all(&stage)?;
    compiler_command(
        ctx,
        "",
        "cp",
        &[
            "-a",
            "--reflink=auto",
            "--",
            &join(&[source, "Assets"]),
            &join(&[source, "Packages"]),
            &join(&[source, "ProjectSettings"]),
            &stage,
        ],
    )?;
    normalize_staged_package_paths(source, &stage)?;
    super::install_agent_package(&stage)?;
    // A copied staging-derived target can carry a prior tool marker. Replace
    // only the marker inside this newly owned clone; retain the source bytes.
    let staging_marker = join(&[&stage, "ProjectSettings/UnityMicroEditorStaging.json"]);
    if snapshot
        .files
        .contains_key("ProjectSettings/UnityMicroEditorStaging.json")
    {
        agent_canonical(&staging_marker)?;
        fs::remove_file(&staging_marker)
            .map_err(|error| sysutil::Error::os("remove", &staging_marker, &error))?;
    }
    agent_immutable_json(
        &staging_marker,
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("sourceProject", source.into()),
        ])),
    )?;
    verify_snapshot(ctx, &snapshot)?;
    agent_immutable_json(&marker, &snapshot)?;
    Ok((stage, snapshot))
}

/// The one staging copy every Editor stage of `project` runs in. Its
/// `Library` and build caches outlive a preparation, so only the first host
/// build of a project is cold.
pub(crate) fn shared_stage(project: &str) -> String {
    join(&[project, "Library/UnityMicroEditor/stage/project"])
}

const STAGE_ROOTS: [&str; 3] = ["Assets", "Packages", "ProjectSettings"];
const STAGE_MARKER: &str = "ProjectSettings/UnityMicroEditorStaging.json";

/// Brings the shared staging copy of `source` up to date with its authored
/// roots and installs the runtime package there. Files whose bytes already
/// match keep their timestamps, so Unity reimports only what changed; whatever
/// an earlier stage Editor authored in those roots is removed. An interrupted
/// run is repaired by the next one.
pub fn sync_staging(ctx: &Context, source: &str) -> Result<(String, SourceSnapshot)> {
    let stage = shared_stage(source);
    if !is_owned_cache_path(source, &stage) {
        return Err(anyhow!("shared stage is outside owned project caches"));
    }
    agent_canonical(&stage)?;
    let exists = match sysutil::lstat(&stage) {
        Ok(info) if info.is_dir() => true,
        Ok(_) => return Err(anyhow!("shared staging project is not a regular directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(sysutil::Error::os("lstat", &stage, &error).into()),
    };
    if exists {
        if crate::editorbridge::project_is_open(&stage)? {
            return Err(fail(
                "staging_busy",
                "an Editor still has the shared staging copy open; wait for it or stop it before another preparation",
            ));
        }
        // A project copied together with its Library carries the stage of the
        // original, whose caches hold that path.
        let foreign = match super::read_map(&join(&[&stage, STAGE_MARKER]), 65536) {
            Ok(marker) => marker.get("sourceProject") != Some(&Any::from(source)),
            Err(error) if not_exist(&error) => false,
            Err(error) => return Err(error),
        };
        if foreign {
            let directory = sysutil::dir(&stage);
            fs::remove_dir_all(&directory)
                .map_err(|error| sysutil::Error::os("remove", &directory, &error))?;
        }
    }
    let snapshot = snapshot_source(ctx, source)?;
    sysutil::mkdir_all(&stage)?;
    for root in STAGE_ROOTS {
        prune_staged(ctx, source, &stage, root, &snapshot)?;
        walk_dir(&join(&[source, root]), &mut |entry: &WalkEntry| {
            if entry.is_dir() {
                let logical = sysutil::rel(source, entry.path).ok_or_else(|| {
                    anyhow!("Rel: can't make {} relative to {source}", entry.path)
                })?;
                sysutil::mkdir_all(&join(&[&stage, &logical]))?;
            }
            Ok(Walk::Continue)
        })?;
    }
    for (logical, hash) in snapshot.files.iter() {
        ctx.err()?;
        let (from, to) = (join(&[source, logical]), join(&[&stage, logical]));
        match sysutil::lstat(&to) {
            Ok(info) if info.is_regular() && file_hash(&to)? == *hash => continue,
            Ok(_) => {
                fs::remove_file(&to).map_err(|error| sysutil::Error::os("remove", &to, &error))?
            }
            Err(_) => {}
        }
        // Reflinked where the filesystem can; the source timestamp tells Unity
        // an unchanged file apart from an edited one.
        fs::copy(&from, &to).map_err(|error| sysutil::Error::os("copy", &to, &error))?;
        let modified = fs::metadata(&from)
            .and_then(|info| info.modified())
            .map_err(|error| sysutil::Error::os("stat", &from, &error))?;
        fs::File::open(&to)
            .and_then(|file| file.set_modified(modified))
            .map_err(|error| sysutil::Error::os("utimens", &to, &error))?;
    }
    normalize_staged_package_paths(source, &stage)?;
    super::install_agent_package(&stage)?;
    let staging_marker = join(&[&stage, STAGE_MARKER]);
    if snapshot.files.contains_key(STAGE_MARKER) {
        fs::remove_file(&staging_marker)
            .map_err(|error| sysutil::Error::os("remove", &staging_marker, &error))?;
    }
    agent_immutable_json(
        &staging_marker,
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("sourceProject", source.into()),
        ])),
    )?;
    verify_snapshot(ctx, &snapshot)?;
    Ok((stage, snapshot))
}

/// Removes what the staged `relative` directory holds beyond the snapshot.
fn prune_staged(
    ctx: &Context,
    source: &str,
    stage: &str,
    relative: &str,
    snapshot: &SourceSnapshot,
) -> Result<()> {
    ctx.err()?;
    let directory = join(&[stage, relative]);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(sysutil::Error::os("open", &directory, &error).into()),
    };
    for entry in entries {
        let entry = entry.map_err(|error| sysutil::Error::os("readdirent", &directory, &error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let (logical, path) = (join(&[relative, &name]), join(&[&directory, &name]));
        let kind = entry
            .file_type()
            .map_err(|error| sysutil::Error::os("lstat", &path, &error))?;
        if !kind.is_dir() {
            if logical != STAGE_MARKER && !snapshot.files.contains_key(&logical) {
                fs::remove_file(&path)
                    .map_err(|error| sysutil::Error::os("remove", &path, &error))?;
            }
        } else if sysutil::lstat(&join(&[source, &logical])).is_ok_and(|info| info.is_dir()) {
            prune_staged(ctx, source, stage, &logical, snapshot)?;
        } else {
            fs::remove_dir_all(&path)
                .map_err(|error| sysutil::Error::os("remove", &path, &error))?;
        }
    }
    Ok(())
}

/// Decodes the arguments into a `PrefabEditRequest`:
/// members are applied in key order, unknown members are ignored and names
/// match case-insensitively after an exact match.
pub(crate) fn decode_prefab_request(args: &AnyMap) -> Result<PrefabEditRequest, String> {
    const FIELDS: [&str; 17] = [
        "action",
        "path",
        "expectedRevision",
        "operationId",
        "fileId",
        "targetPath",
        "prefabPath",
        "outputPath",
        "parentFileId",
        "parentPath",
        "name",
        "completely",
        "componentType",
        "property",
        "value",
        "fields",
        "defaultsTemplate",
    ];
    let fold = |name: &str| {
        name.to_lowercase()
            .replace('\u{17f}', "s")
            .replace('\u{212a}', "k")
    };
    let mut request = PrefabEditRequest::default();
    let mut first: Option<String> = None;
    for (key, member) in args {
        let Some(field) = FIELDS
            .iter()
            .position(|field| *field == key)
            .or_else(|| FIELDS.iter().position(|field| fold(field) == fold(key)))
        else {
            continue;
        };
        let at = jsondoc::Target {
            strukt: "PrefabEditRequest",
            field: FIELDS[field],
        };
        let string = |target: &mut String| match member {
            Any::Null => Ok(()),
            Any::String(text) => {
                *target = text.clone();
                Ok(())
            }
            other => Err(at.mismatch(json_kind(other), "string")),
        };
        let outcome = match field {
            0 => string(&mut request.action),
            1 => string(&mut request.path),
            2 => string(&mut request.expected_revision),
            3 => string(&mut request.operation_id),
            4 => string(&mut request.file_id),
            5 => string(&mut request.target_path),
            6 => string(&mut request.prefab_path),
            7 => string(&mut request.output_path),
            8 => string(&mut request.parent_file_id),
            9 => string(&mut request.parent_path),
            10 => string(&mut request.name),
            11 => match member {
                Any::Null => Ok(()),
                Any::Bool(flag) => {
                    request.completely = *flag;
                    Ok(())
                }
                other => Err(at.mismatch(json_kind(other), "bool")),
            },
            12 => string(&mut request.component_type),
            13 => string(&mut request.property),
            14 => {
                request.value = (!member.is_null()).then(|| to_serde(member));
                Ok(())
            }
            15 => match member {
                Any::Null => {
                    request.fields = BTreeMap::new();
                    Ok(())
                }
                Any::Object(fields) => {
                    for (name, field) in fields {
                        request.fields.insert(name.clone(), to_serde(field));
                    }
                    Ok(())
                }
                other => Err(at.mismatch(json_kind(other), "object")),
            },
            _ => match member {
                Any::Null => {
                    request.defaults_template = None;
                    Ok(())
                }
                Any::Object(_) => {
                    serde_json::from_value::<NativeDefaultsTemplate>(to_serde(member))
                        .map(|template| request.defaults_template = Some(template))
                        .map_err(|error| error.to_string())
                }
                other => Err(at.mismatch(json_kind(other), "authoring.NativeDefaultsTemplate")),
            },
        };
        if let Err(error) = outcome {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(request), Err)
}

impl Service {
    /// A revision-checked source prefab action. No Editor runs.
    pub(crate) fn prefab_edit(&self, ctx: &Context, args: &AnyMap) -> Result<HandlerValue> {
        let project = self.0.project.as_str();
        let id = text(args, "operationId");
        if !operation_id(&id) {
            return Err(fail(
                "invalid_request",
                "source prefab authoring requires operationId of at most 64 ASCII letters/digits/hyphens",
            ));
        }
        let mut request = decode_prefab_request(args).map_err(|error| {
            fail(
                "invalid_request",
                format!("invalid source prefab arguments: {error}"),
            )
        })?;
        let mut profile_sha = String::new();
        let mut guard_profile = None;
        if request.action == "add_component" {
            let name = super::defaults::component_defaults_name(&request.component_type)?;
            let (profile, hash) = super::component_defaults_profile(ctx, project, &self.0.editor)?;
            let catalog = super::defaults::read_defaults_catalog(project, &profile, &hash)?;
            let template = catalog.templates.get(&name).cloned().unwrap_or_default();
            request.component_type = name;
            request.defaults_template = Some(template);
            profile_sha = hash.clone();
            guard_profile = Some(hash);
        } else if request.defaults_template.is_some() {
            return Err(fail(
                "invalid_request",
                "defaultsTemplate is supplied by the versioned component cache, not the caller",
            ));
        }
        let store = Store::new(Path::new(project)).map_err(store_error)?;
        // Previously accepted Editor-backed plans remain replayable through the
        // same source journal. This migration path never creates a legacy plan or
        // starts an Editor; it requires the existing immutable request receipt.
        let directory = join(&[
            project,
            "Library/UnityMicroEditor/authoring-stages",
            &sha256(id.as_bytes()),
        ]);
        match agent_read_json::<StagedPublicationRequest>(
            &join(&[&directory, "publication.json"]),
            80 << 20,
        ) {
            Ok(legacy) => {
                if legacy.operation_id != id {
                    return Err(fail(
                        "invalid_plan",
                        "legacy publication belongs to another operation",
                    ));
                }
                let request_path = join(&[&directory, "request.json"]);
                if sysutil::lstat(&request_path).is_err() {
                    return Err(fail(
                        "invalid_plan",
                        "legacy publication has no immutable request receipt",
                    ));
                }
                agent_immutable_json(
                    &request_path,
                    &Any::Object(obj([
                        ("schemaVersion", 1.into()),
                        ("project", project.into()),
                        ("arguments", Any::Object(args.clone())),
                    ])),
                )?;
                let result = store.publish_staged(&legacy).map_err(store_error)?;
                return map_reply(obj([
                    ("backend", "raw_source".into()),
                    ("planOrigin", "unity_editor_staged_legacy".into()),
                    ("operationId", id.into()),
                    ("publication", value::raw(&result)?),
                    ("editorStarted", false.into()),
                ]));
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        let guard = guard_profile.map(|hash| self.component_defaults_guard(ctx, hash));
        let result = match &guard {
            Some(guard) => store.prefab_edit(&request, Some(guard)),
            None => store.prefab_edit(&request, None),
        }
        .map_err(store_error)?;
        let created = if result.created_ids_nil {
            Any::Null
        } else {
            value::raw(&result.created_ids)?
        };
        let mut response = obj([
            ("backend", result.backend.clone().into()),
            ("operationId", result.operation_id.clone().into()),
            ("action", result.action.clone().into()),
            ("publication", value::raw(&result.publication)?),
            ("createdIds", created),
            ("generatedPath", result.generated_path.clone().into()),
            ("editorStarted", false.into()),
        ]);
        if !profile_sha.is_empty() {
            response.insert("defaultsBackend".into(), "versioned_component_cache".into());
            response.insert("profileSha256".into(), profile_sha.into());
        }
        map_reply(response)
    }
}

/// An Editor working in a clone may neither
/// change existing authored metadata nor leave metadata for unplanned assets.
pub fn validate_staged_metadata(
    snapshot: &SourceSnapshot,
    stage: &str,
    new_assets: &std::collections::BTreeSet<String>,
) -> Result<()> {
    for (logical, expected) in &snapshot.files {
        if !logical.starts_with("Assets/") || !logical.ends_with(".meta") {
            continue;
        }
        let path = join(&[stage, logical]);
        agent_canonical(&path)?;
        if !file_hash(&path).is_ok_and(|actual| actual == *expected) {
            return Err(fail(
                "invalid_identity",
                format!("Editor changed or removed existing authored metadata: {logical}"),
            ));
        }
    }
    walk_dir(&join(&[stage, "Assets"]), &mut |entry: &WalkEntry| {
        if entry.is_dir() || !entry.path.ends_with(".meta") {
            return Ok(Walk::Continue);
        }
        let logical = sysutil::rel(stage, entry.path)
            .ok_or_else(|| anyhow!("Rel: can't make {} relative to {stage}", entry.path))?;
        if snapshot.files.contains_key(&logical) {
            return Ok(Walk::Continue);
        }
        if !new_assets.contains(logical.strip_suffix(".meta").unwrap_or(&logical)) {
            return Err(fail(
                "invalid_identity",
                format!("Editor generated orphan or unplanned metadata: {logical}"),
            ));
        }
        Ok(Walk::Continue)
    })
}
