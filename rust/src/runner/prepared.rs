//! Launching an immutable prepared descriptor after every
//! artifact it names has been checked against its recorded digest.
use super::{
    Event, LaunchOptions, StopOnDrop, Wake,
    json::{First, members, read_document, set_list, set_string_map, set_strings},
    mkdir_all,
    player::{READY_BUDGET, require_player_report, start_player},
    routes::{RawContentRoutes, valid_scene_guid},
    text_error, wrap,
};
use crate::{
    ENGINE_VERSION,
    jsondoc::{self, Json, Member, Target},
    native::{resolve_project_import_source, stage_native_resources},
    project::{
        BUILTIN_RESOURCE_GUID, CompiledShaderSnapshot, NativeContentDependency,
        NativeContentObject, RetainedBuiltinShader, validate_shader_source_requirements,
    },
    sysutil,
    util::{file_hash, lock_session, random_id, valid_sha256},
};
use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedLaunch {
    pub schema_version: i64,
    pub project: String,
    pub player: String,
    pub player_sha256: String,
    pub manifest: String,
    pub manifest_sha256: String,
    pub content_routes: String,
    pub content_routes_sha256: String,
    pub assembly_hashes: BTreeMap<String, String>,
}
impl PreparedLaunch {
    /// Reads a prepared launch file.
    pub fn read(path: &str) -> Result<Self> {
        let document = read_document(path)?;
        let mut launch = Self::default();
        let mut hashes = None;
        let root = members(&document.json, Target::VALUE, "preparedLaunch").map_err(text_error)?;
        const FIELDS: [&str; 9] = [
            "schemaVersion",
            "project",
            "player",
            "playerSha256",
            "manifest",
            "manifestSha256",
            "contentRoutes",
            "contentRoutesSha256",
            "assemblyHashes",
        ];
        jsondoc::decode_struct(root, &FIELDS, |index, member| {
            let at = Target {
                strukt: "preparedLaunch",
                field: FIELDS[index],
            };
            let value = &member.value;
            match index {
                0 => jsondoc::set_int(&mut launch.schema_version, value, at),
                1 => jsondoc::set_string(&mut launch.project, value, at),
                2 => jsondoc::set_string(&mut launch.player, value, at),
                3 => jsondoc::set_string(&mut launch.player_sha256, value, at),
                4 => jsondoc::set_string(&mut launch.manifest, value, at),
                5 => jsondoc::set_string(&mut launch.manifest_sha256, value, at),
                6 => jsondoc::set_string(&mut launch.content_routes, value, at),
                7 => jsondoc::set_string(&mut launch.content_routes_sha256, value, at),
                _ => set_string_map(&mut hashes, value, at),
            }
        })
        .map_err(text_error)?;
        launch.assembly_hashes = hashes.unwrap_or_default();
        Ok(launch)
    }
}

/// `preparedScene.Compiler`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedCompiler {
    pub native_only: bool,
    pub native_slots_sha: String,
}
/// One `sourceDependencies` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedDependency {
    pub path: String,
    pub source_path: String,
    pub sha256: String,
}
/// A path with its digest: `importedAssetPacks` entries (`bundlePath`) and the
/// `resources` of native content (`path`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedFile {
    pub path: String,
    pub sha256: String,
}
/// One `nativeContentFiles` entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedNativeContent {
    pub guid: String,
    pub path: String,
    pub sha256: String,
    pub objects: Vec<NativeContentObject>,
    pub dependencies: Vec<NativeContentDependency>,
    pub resources: Vec<PreparedFile>,
}

/// The part of a published scene manifest that names files.
/// Absent and empty lists are not distinguished; nothing here is re-encoded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedScene {
    pub schema_version: i64,
    pub supported: bool,
    /// `projectRoot`.
    pub project: String,
    pub unity_version: String,
    pub assemblies: Vec<String>,
    pub compiler: PreparedCompiler,
    /// `sourceDependencies`.
    pub dependencies: Vec<PreparedDependency>,
    /// `importedAssetPacks`.
    pub imported_packs: Vec<PreparedFile>,
    /// `nativeContentFiles`.
    pub native_content: Vec<PreparedNativeContent>,
    pub compiled_shader_snapshots: Vec<CompiledShaderSnapshot>,
    pub builtin_shader_snapshots: Vec<RetainedBuiltinShader>,
}

/// A field of the named outer struct.
fn outer(field: &str) -> Target<'_> {
    Target {
        strukt: "preparedScene",
        field,
    }
}
/// A field reached through an anonymous struct: type errors print no struct name.
fn inner(field: &str) -> Target<'_> {
    Target { strukt: "", field }
}
/// A field path below a project snapshot struct.
fn typed<'a>(strukt: &'a str, field: &'a str) -> Target<'a> {
    Target { strukt, field }
}
/// Replaces a list when the member was an array (`null` keeps the old one).
fn assign<T>(
    target: &mut Vec<T>,
    list: Option<Vec<T>>,
    result: Result<(), String>,
) -> Result<(), String> {
    if let Some(list) = list {
        *target = list;
    }
    result
}
/// The large snapshot descriptors of the project module, decoded by their own
/// serde rules; only the text of a type error is serde's rather than the document decoder's.
fn typed_list<T: DeserializeOwned>(
    target: &mut Vec<T>,
    member: &Member,
    source: &[u8],
) -> Result<(), String> {
    match serde_json::from_slice::<Option<Vec<T>>>(member.raw(source)) {
        Ok(Some(items)) => *target = items,
        Ok(None) => {}
        Err(error) => return Err(format!("json: {} {error}", member.key)),
    }
    Ok(())
}
fn file_entry(
    entry: &mut PreparedFile,
    value: &Json,
    at: Target<'_>,
    target_type: &str,
    fields: [&str; 2],
    paths: [&str; 2],
) -> Result<(), String> {
    let members = members(value, at, target_type)?;
    jsondoc::decode_struct(members, &fields, |index, member| match index {
        0 => jsondoc::set_string(&mut entry.path, &member.value, inner(paths[0])),
        _ => jsondoc::set_string(&mut entry.sha256, &member.value, inner(paths[1])),
    })
}
impl PreparedScene {
    /// Reads a prepared scene file.
    pub fn read(path: &str) -> Result<Self> {
        let document = read_document(path)?;
        Self::decode(&document.json, &document.data).map_err(text_error)
    }

    fn decode(json: &Json, source: &[u8]) -> Result<Self, String> {
        let mut scene = Self::default();
        let root = members(json, Target::VALUE, "preparedScene")?;
        const FIELDS: [&str; 11] = [
            "schemaVersion",
            "supported",
            "projectRoot",
            "unityVersion",
            "assemblies",
            "compiler",
            "sourceDependencies",
            "importedAssetPacks",
            "nativeContentFiles",
            "compiledShaderSnapshots",
            "builtinShaderSnapshots",
        ];
        jsondoc::decode_struct(root, &FIELDS, |index, member| {
            let at = outer(FIELDS[index]);
            let value = &member.value;
            match index {
                0 => jsondoc::set_int(&mut scene.schema_version, value, at),
                1 => jsondoc::set_bool(&mut scene.supported, value, at),
                2 => jsondoc::set_string(&mut scene.project, value, at),
                3 => jsondoc::set_string(&mut scene.unity_version, value, at),
                4 => {
                    let mut list = None;
                    let result = set_strings(&mut list, value, at);
                    assign(&mut scene.assemblies, list, result)
                }
                5 => {
                    let fields = members(value, at, "object")?;
                    jsondoc::decode_struct(
                        fields,
                        &["nativeOnly", "nativeSlotsSha256"],
                        |index, member| match index {
                            0 => jsondoc::set_bool(
                                &mut scene.compiler.native_only,
                                &member.value,
                                inner("compiler.nativeOnly"),
                            ),
                            _ => jsondoc::set_string(
                                &mut scene.compiler.native_slots_sha,
                                &member.value,
                                inner("compiler.nativeSlotsSha256"),
                            ),
                        },
                    )
                }
                6 => {
                    let mut list = None;
                    let result = set_list(&mut list, value, at, "array", |entry, item| {
                        let entry: &mut PreparedDependency = entry;
                        let fields = members(item, at, "object")?;
                        jsondoc::decode_struct(
                            fields,
                            &["path", "sourcePath", "sha256"],
                            |index, member| match index {
                                0 => jsondoc::set_string(
                                    &mut entry.path,
                                    &member.value,
                                    inner("sourceDependencies.path"),
                                ),
                                1 => jsondoc::set_string(
                                    &mut entry.source_path,
                                    &member.value,
                                    inner("sourceDependencies.sourcePath"),
                                ),
                                _ => jsondoc::set_string(
                                    &mut entry.sha256,
                                    &member.value,
                                    inner("sourceDependencies.sha256"),
                                ),
                            },
                        )
                    });
                    assign(&mut scene.dependencies, list, result)
                }
                7 => {
                    let mut list = None;
                    let result = set_list(&mut list, value, at, "array", |entry, item| {
                        file_entry(
                            entry,
                            item,
                            at,
                            "object",
                            ["bundlePath", "sha256"],
                            ["importedAssetPacks.bundlePath", "importedAssetPacks.sha256"],
                        )
                    });
                    assign(&mut scene.imported_packs, list, result)
                }
                8 => {
                    let mut list = None;
                    let result = set_list(&mut list, value, at, "array", native_content);
                    assign(&mut scene.native_content, list, result)
                }
                9 => typed_list(&mut scene.compiled_shader_snapshots, member, source),
                _ => typed_list(&mut scene.builtin_shader_snapshots, member, source),
            }
        })?;
        Ok(scene)
    }
}
fn native_content(entry: &mut PreparedNativeContent, value: &Json) -> Result<(), String> {
    let fields = members(value, outer("nativeContentFiles"), "object")?;
    jsondoc::decode_struct(
        fields,
        &[
            "guid",
            "path",
            "sha256",
            "objects",
            "dependencies",
            "resources",
        ],
        |index, member| {
            let value = &member.value;
            match index {
                0 => jsondoc::set_string(&mut entry.guid, value, inner("nativeContentFiles.guid")),
                1 => jsondoc::set_string(&mut entry.path, value, inner("nativeContentFiles.path")),
                2 => jsondoc::set_string(
                    &mut entry.sha256,
                    value,
                    inner("nativeContentFiles.sha256"),
                ),
                3 => {
                    let mut list = None;
                    let at = inner("nativeContentFiles.objects");
                    let result = set_list(
                        &mut list,
                        value,
                        at,
                        "array",
                        |entry: &mut NativeContentObject, item| {
                            let fields = members(item, at, "NativeContentObject")?;
                            jsondoc::decode_struct(
                                fields,
                                &["fileId", "typeName"],
                                |index, member| match index {
                                    0 => jsondoc::set_string(
                                        &mut entry.file_id,
                                        &member.value,
                                        typed(
                                            "NativeContentObject",
                                            "nativeContentFiles.objects.fileId",
                                        ),
                                    ),
                                    _ => jsondoc::set_string(
                                        &mut entry.type_name,
                                        &member.value,
                                        typed(
                                            "NativeContentObject",
                                            "nativeContentFiles.objects.typeName",
                                        ),
                                    ),
                                },
                            )
                        },
                    );
                    assign(&mut entry.objects, list, result)
                }
                4 => {
                    let mut list = None;
                    let at = inner("nativeContentFiles.dependencies");
                    let result = set_list(
                        &mut list,
                        value,
                        at,
                        "array",
                        |entry: &mut NativeContentDependency, item| {
                            let fields = members(item, at, "NativeContentDependency")?;
                            jsondoc::decode_struct(fields, &["guid", "global"], |index, member| {
                                match index {
                                    0 => jsondoc::set_string(
                                        &mut entry.guid,
                                        &member.value,
                                        typed(
                                            "NativeContentDependency",
                                            "nativeContentFiles.dependencies.guid",
                                        ),
                                    ),
                                    _ => jsondoc::set_bool(
                                        &mut entry.global,
                                        &member.value,
                                        typed(
                                            "NativeContentDependency",
                                            "nativeContentFiles.dependencies.global",
                                        ),
                                    ),
                                }
                            })
                        },
                    );
                    assign(&mut entry.dependencies, list, result)
                }
                _ => {
                    let mut list = None;
                    let at = inner("nativeContentFiles.resources");
                    let result = set_list(&mut list, value, at, "array", |entry, item| {
                        file_entry(
                            entry,
                            item,
                            at,
                            "object",
                            ["path", "sha256"],
                            [
                                "nativeContentFiles.resources.path",
                                "nativeContentFiles.resources.sha256",
                            ],
                        )
                    });
                    assign(&mut entry.resources, list, result)
                }
            }
        },
    )
}

/// The file at an absolute path has the recorded digest;
/// `observed` remembers verified files so that declarations cannot disagree.
pub fn require_prepared_file(
    path: &str,
    expected: &str,
    observed: &mut BTreeMap<String, String>,
) -> Result<()> {
    if !sysutil::is_abs(path) || !valid_sha256(expected) {
        bail!("prepared artifact needs an absolute path and exact SHA256: {path}");
    }
    if let Some(previous) = observed.get(path) {
        if previous != expected {
            bail!("prepared artifacts disagree on content: {path}");
        }
        return Ok(());
    }
    let actual = file_hash(path).map_err(|e| wrap(&format!("read prepared artifact {path}"), e))?;
    if actual != expected {
        bail!("prepared artifact changed: {path} (hash {actual}, expected {expected})");
    }
    observed.insert(path.into(), expected.into());
    Ok(())
}

/// The physical file of a manifest path.
pub fn resolve_prepared_source(project: &str, path: &str) -> Result<String> {
    if sysutil::is_abs(path) {
        return Ok(path.into());
    }
    if path.is_empty() || sysutil::clean(path) != path || path.starts_with("../") {
        bail!("prepared source path is invalid: {path}");
    }
    if path.starts_with("Packages/") {
        return resolve_project_import_source(project, path);
    }
    Ok(sysutil::join(&[project, path]))
}

pub fn validate_prepared_scene(
    launch: &PreparedLaunch,
    path: &str,
    expected: &str,
    observed: &mut BTreeMap<String, String>,
) -> Result<PreparedScene> {
    require_prepared_file(path, expected, observed)?;
    let scene = PreparedScene::read(path)?;
    if scene.schema_version != 1
        || !scene.supported
        || scene.project != launch.project
        || scene.unity_version != ENGINE_VERSION
    {
        bail!("prepared manifest does not match the supported source project/engine");
    }
    if scene.compiler.native_only {
        if !scene.assemblies.is_empty()
            || !valid_sha256(&scene.compiler.native_slots_sha)
            || !sysutil::is_abs(&launch.player)
        {
            bail!(
                "native-only prepared manifest requires its verified host assembly registrations and no game assemblies"
            );
        }
        let base = sysutil::base(&launch.player);
        let stem = base
            .strip_suffix(sysutil::ext(&launch.player))
            .unwrap_or(&base);
        let registrations = sysutil::join(&[
            &sysutil::dir(&launch.player),
            &format!("{stem}_Data"),
            "ScriptingAssemblies.json",
        ]);
        require_prepared_file(&registrations, &scene.compiler.native_slots_sha, observed)?;
        if !registered_names(&registrations)?
            .iter()
            .any(|name| name == "UnityMicroEditor.dll")
        {
            bail!("native-only prepared host has no registered runtime bridge");
        }
    } else if scene.assemblies.is_empty() {
        bail!(
            "prepared manifest with no game assemblies requires a verified native-only compiler baseline"
        );
    }
    for assembly in &scene.assemblies {
        let resolved = resolve_prepared_source(&launch.project, assembly)?;
        let hash = launch
            .assembly_hashes
            .get(&resolved)
            .map_or("", String::as_str);
        require_prepared_file(&resolved, hash, observed)?;
    }
    for dependency in &scene.dependencies {
        let resolved = match dependency.source_path.as_str() {
            "" => resolve_prepared_source(&launch.project, &dependency.path)?,
            source => source.to_string(),
        };
        require_prepared_file(&resolved, &dependency.sha256, observed)?;
    }
    for snapshot in &scene.compiled_shader_snapshots {
        if snapshot.schema_version != 1
            || snapshot.compiled_source_revision_verified
            || snapshot.source_path.is_empty()
        {
            bail!("prepared compiled Shader snapshot provenance is unsupported");
        }
        validate_shader_source_requirements(
            &launch.project,
            &snapshot.source_path,
            &snapshot.source_requirements,
            &snapshot.missing_includes,
        )?;
    }
    if scene.builtin_shader_snapshots.len() > 1 {
        bail!("duplicate prepared retained built-in Shader snapshot");
    }
    if !scene.builtin_shader_snapshots.is_empty() {
        bail!("a retained built-in Shader snapshot is no longer supported");
    }
    for snapshot in &scene.builtin_shader_snapshots {
        let found = scene.native_content.iter().any(|content| {
            content.guid == snapshot.guid
                && content.path == snapshot.content.path
                && content.sha256 == snapshot.content.sha256
                && content.objects.len() == 1
                && content.objects[0].file_id == snapshot.file_id
                && content.objects[0].type_name == "UnityEngine.Shader"
                && content.resources.is_empty()
                && content.dependencies.is_empty()
        });
        if !found {
            bail!("prepared retained built-in Shader has no matching content descriptor");
        }
    }
    for content in &scene.native_content {
        if content.guid == BUILTIN_RESOURCE_GUID && scene.builtin_shader_snapshots.len() != 1 {
            bail!(
                "prepared retained built-in content requires verified Shader snapshot provenance"
            );
        }
    }
    for pack in &scene.imported_packs {
        let resolved = resolve_prepared_source(&launch.project, &pack.path)?;
        require_prepared_file(&resolved, &pack.sha256, observed)?;
    }
    for content in &scene.native_content {
        let resolved = resolve_prepared_source(&launch.project, &content.path)?;
        require_prepared_file(&resolved, &content.sha256, observed)?;
        for resource in &content.resources {
            let resolved = resolve_prepared_source(&launch.project, &resource.path)?;
            require_prepared_file(&resolved, &resource.sha256, observed)?;
        }
    }
    Ok(scene)
}

/// The `names` of a Player's `ScriptingAssemblies.json`.
fn registered_names(path: &str) -> Result<Vec<String>> {
    let document = read_document(path)?;
    let mut names = None;
    let root = members(&document.json, Target::VALUE, "object").map_err(text_error)?;
    let mut first = First::default();
    first.note(jsondoc::decode_struct(root, &["names"], |_, member| {
        set_strings(
            &mut names,
            &member.value,
            Target {
                strukt: "",
                field: "names",
            },
        )
    }));
    first.finish().map_err(text_error)?;
    Ok(names.unwrap_or_default())
}

/// The `--prepared` launch mode of `run`.
pub fn run_prepared_player(opts: &mut LaunchOptions) -> Result<()> {
    if !sysutil::is_abs(&opts.prepared) {
        bail!("--prepared requires an absolute launch descriptor path");
    }
    let launch = PreparedLaunch::read(&opts.prepared)?;
    if launch.schema_version != 1 || launch.project != opts.project {
        bail!("prepared launch belongs to a different project or schema");
    }
    let mut observed = BTreeMap::new();
    require_prepared_file(&launch.player, &launch.player_sha256, &mut observed)?;
    validate_prepared_scene(
        &launch,
        &launch.manifest,
        &launch.manifest_sha256,
        &mut observed,
    )?;
    if !launch.content_routes.is_empty() {
        require_prepared_file(
            &launch.content_routes,
            &launch.content_routes_sha256,
            &mut observed,
        )?;
        let routes = RawContentRoutes::read(&launch.content_routes)?;
        if routes.schema_version != 1 || routes.project != launch.project {
            bail!("prepared content routes do not match source project");
        }
        let mut seen = BTreeSet::new();
        for route in routes.scenes.as_deref().unwrap_or(&[]) {
            if !valid_scene_guid(&route.guid) || !seen.insert(route.guid.as_str()) {
                bail!("prepared content routes contain an invalid or duplicate GUID");
            }
            validate_prepared_scene(&launch, &route.manifest, &route.sha256, &mut observed)?;
        }
    }
    mkdir_all(&opts.output)?;
    let _lock = lock_session(&opts.output)?;
    let ctx = crate::packages::signal_context()?;
    let wake = Wake::bridge(&ctx);
    let run_directory = sysutil::join(&[&opts.output, "runs", &random_id()]);
    mkdir_all(&run_directory)?;
    if !launch.content_routes.is_empty() {
        let routes = RawContentRoutes::read(&launch.content_routes)?;
        for route in routes.scenes.as_deref().unwrap_or(&[]) {
            stage_native_resources(&route.manifest, &run_directory)?;
        }
    }
    opts.raw_routes = launch.content_routes.clone();
    let started = Instant::now();
    let mut player = StopOnDrop(Some(start_player(
        &wake.wake,
        opts,
        &launch.player,
        &launch.manifest,
        &run_directory,
        "",
    )?));
    let player = player.0.as_mut().ok_or_else(|| anyhow!("no Player"))?;
    player.wait_ready(&ctx, READY_BUDGET)?;
    Event::new("ready")
        .with("pid", &player.pid())
        .with("manifest", &launch.manifest)
        .seconds("startupSeconds", started.elapsed().as_secs_f64())
        .with("report", &player.report)
        .with("prepared", &true)
        .emit();
    loop {
        let seen = wake.wake.seen();
        if let Some(result) = player.take_done() {
            player.exited = true;
            if let Some(error) = result {
                bail!(error);
            }
            return require_player_report(&player.report);
        }
        if ctx.error().is_some() {
            return Ok(());
        }
        wake.wake.wait(seen, None);
    }
}
