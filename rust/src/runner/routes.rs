//! Game arguments and the immutable additive content routes.
use super::{
    BaselineBuild, LaunchOptions,
    addressables::AddressableRoute,
    apply_compiled_bindings, assemblies_of,
    json::{First, members, read_document, set_list, set_strings},
    mkdir_all,
    noop::scene_runtime_equal,
    os_error, read_file, text_error, wrap,
};
use crate::{
    cancel::Context,
    compiler::{CompileUpdate, player_assembly_paths},
    jsondoc::{self, Json, Target},
    native::{NativeProjectImporter, apply_builtin_shaders},
    project::{self, Session, Value},
    registry, sha256, sysutil,
    util::{file_hash, marshal, valid_sha256},
};
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{ErrorKind, Write},
    os::unix::fs::OpenOptionsExt,
    sync::Arc,
};

/// A scene GUID is 32 lowercase hexadecimal characters.
pub fn valid_scene_guid(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RawSceneRoute {
    pub guid: String,
    pub manifest: String,
    pub sha256: String,
}

/// The content route snapshot; `scenes: None` is an absent list (`null`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RawContentRoutes {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    pub project: String,
    pub scenes: Option<Vec<RawSceneRoute>>,
    /// Addressable keys served from sources; scenes among them have a route.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addressables: Vec<AddressableRoute>,
}
impl RawContentRoutes {
    /// Reads and decodes a route snapshot file.
    pub fn read(path: &str) -> Result<Self> {
        let document = read_document(path)?;
        let mut routes = Self::default();
        let root =
            members(&document.json, Target::VALUE, "rawContentRoutes").map_err(text_error)?;
        let at = |field| Target {
            strukt: "rawContentRoutes",
            field,
        };
        jsondoc::decode_struct(
            root,
            &["schemaVersion", "project", "scenes", "addressables"],
            |index, member| match index {
                0 => jsondoc::set_int(
                    &mut routes.schema_version,
                    &member.value,
                    at("schemaVersion"),
                ),
                1 => jsondoc::set_string(&mut routes.project, &member.value, at("project")),
                2 => set_list(
                    &mut routes.scenes,
                    &member.value,
                    at("scenes"),
                    "array",
                    scene_route,
                ),
                _ => {
                    let mut list = None;
                    let result = set_list(
                        &mut list,
                        &member.value,
                        at("addressables"),
                        "array",
                        addressable_route,
                    );
                    routes.addressables = list.unwrap_or_default();
                    result
                }
            },
        )
        .map_err(text_error)?;
        Ok(routes)
    }
}
fn scene_route(route: &mut RawSceneRoute, value: &Json) -> Result<(), String> {
    let at = |field| Target {
        strukt: "rawSceneRoute",
        field,
    };
    let fields = members(
        value,
        Target {
            strukt: "rawContentRoutes",
            field: "scenes",
        },
        "rawSceneRoute",
    )?;
    let mut first = First::default();
    first.note(jsondoc::decode_struct(
        fields,
        &["guid", "manifest", "sha256"],
        |index, member| match index {
            0 => jsondoc::set_string(&mut route.guid, &member.value, at("scenes.guid")),
            1 => jsondoc::set_string(&mut route.manifest, &member.value, at("scenes.manifest")),
            _ => jsondoc::set_string(&mut route.sha256, &member.value, at("scenes.sha256")),
        },
    ));
    first.finish()
}

fn addressable_route(route: &mut AddressableRoute, value: &Json) -> Result<(), String> {
    let at = |field| Target {
        strukt: "addressableRoute",
        field,
    };
    let fields = members(
        value,
        Target {
            strukt: "rawContentRoutes",
            field: "addressables",
        },
        "addressableRoute",
    )?;
    let mut first = First::default();
    first.note(jsondoc::decode_struct(
        fields,
        &["guid", "address", "labels", "path", "scene"],
        |index, member| match index {
            0 => jsondoc::set_string(&mut route.guid, &member.value, at("guid")),
            1 => jsondoc::set_string(&mut route.address, &member.value, at("address")),
            2 => {
                let mut labels = None;
                let result = set_strings(&mut labels, &member.value, at("labels"));
                route.labels = labels.unwrap_or_default();
                result
            }
            3 => jsondoc::set_string(&mut route.path, &member.value, at("path")),
            _ => jsondoc::set_bool(&mut route.scene, &member.value, at("scene")),
        },
    ));
    first.finish()
}

/// Everything after the first `--` belongs to the game.
pub fn split_game_arguments(args: &[String]) -> (Vec<String>, Vec<String>) {
    match args.iter().position(|value| value == "--") {
        Some(index) => (args[..index].to_vec(), args[index + 1..].to_vec()),
        None => (args.to_vec(), Vec::new()),
    }
}

pub fn validate_game_arguments(args: &[String]) -> Result<()> {
    for value in args {
        if value.starts_with("--unity-cli-") {
            bail!("game arguments must not override the runner's --unity-cli-* protocol");
        }
        if value.contains('\0') {
            bail!("game arguments contain NUL");
        }
    }
    Ok(())
}

/// Parses `GUID=Assets/...unity` into the GUID and the path.
pub fn parse_content_scene(value: &str) -> Result<(String, String)> {
    match value.split_once('=') {
        Some((guid, path))
            if valid_scene_guid(guid)
                && path.starts_with("Assets/")
                && sysutil::clean(path) == path
                && sysutil::ext(path) == ".unity" =>
        {
            Ok((guid.into(), path.into()))
        }
        _ => bail!("content scene must be lowercase GUID=Assets/...unity: {value}"),
    }
}

/// One cached additive scene: its native importer and the scene it serves.
pub struct ContentImporter {
    pub importer: NativeProjectImporter,
    pub scene: String,
}
/// Content importers by scene GUID, kept for one runner lifetime.
pub type ContentImporters = BTreeMap<String, ContentImporter>;

/// Prepares every declared content scene and publishes their route snapshot.
/// `initializers` is the baseline's list of objects (`Value::Null` when absent).
/// On an error, `failed` is the GUID of the scene that could not be prepared.
#[allow(clippy::too_many_arguments)]
pub fn prepare_content_routes(
    ctx: &Context,
    opts: &LaunchOptions,
    baseline: &BaselineBuild,
    compiled: Option<&CompileUpdate>,
    initializers: &Value,
    script_types: &BTreeMap<String, String>,
    root: Option<&NativeProjectImporter>,
    importers: &mut ContentImporters,
    changed_paths: &[String],
    failed: &mut String,
) -> Result<String> {
    let mut routes = RawContentRoutes {
        schema_version: 1,
        project: opts.project.clone(),
        // The Player requires a scene list, even an empty one.
        scenes: (!opts.addressables.is_empty()).then(Vec::new),
        addressables: opts.addressables.clone(),
    };
    failed.clear();
    let mut seen = BTreeSet::new();
    let assemblies = player_assembly_paths(&baseline.player, &baseline.manifest)?;
    for declaration in &opts.content_scenes {
        let (guid, scene) = parse_content_scene(declaration)?;
        if !seen.insert(guid.clone()) {
            bail!("duplicate content scene GUID {guid}");
        }
        failed.clone_from(&guid);
        let metadata = read_file(&format!("{}.meta", sysutil::join(&[&opts.project, &scene])))?;
        if registry::meta_guid(&metadata).ok().as_deref() != Some(guid.as_str()) {
            bail!("content scene GUID disagrees with authored metadata: {scene}");
        }
        if !importers.contains_key(&guid) {
            // Global runtime-addressed prefabs belong to the root session. Follow
            // each additive scene's authored references without duplicating them.
            let session = Session::new(project::Options {
                project: opts.project.clone(),
                manifest: baseline.manifest.clone(),
                native_catalog: baseline.native_import.catalog.clone(),
                scene: scene.clone(),
                extra_assets: Vec::new(),
                script_types: script_types.clone(),
            })?;
            // The Player whose data the root importer reads, which the bound
            // of the output keeps; a compile's copy may be removed (caches6-1).
            let mut route_options =
                opts.native(root.map_or(&baseline.player, |root| &root.opts.player));
            route_options.output = sysutil::join(&[&opts.output, "content", &guid]);
            let importer = NativeProjectImporter::new(route_options, Arc::new(session))?;
            importers.insert(
                guid.clone(),
                ContentImporter {
                    importer,
                    scene: scene.clone(),
                },
            );
        }
        let cached = importers.get_mut(&guid).expect("content importer");
        if cached.importer.opts.project != opts.project || cached.scene != scene {
            bail!("a cached content route cannot switch source projects or scenes");
        }
        // Reuse parsed YAML and validated native entries. Metadata watcher
        // events still invalidate the GUID index, and Prepare checks sources.
        let session = cached.importer.session.clone();
        session.invalidate(changed_paths);
        session.set_managed_bindings(
            assemblies_of(&assemblies),
            script_types,
            Some(initializers.clone()),
        );
        if let Some(compiled) = compiled {
            apply_compiled_bindings(&session, compiled, script_types);
        }
        apply_builtin_shaders(&session, &opts.builtin_shaders)?;
        // What the root session knows of the script assemblies serves the
        // clips and controllers a content scene bakes too.
        if let Some(root) = root {
            cached.importer.script_assemblies = root.script_assemblies.clone();
            cached.importer.behaviour_code = root.behaviour_code.clone();
            cached
                .importer
                .set_behaviour_layouts(ctx, root.behaviour_layouts.clone())
                .map_err(|e| wrap(&format!("prepare content scene {scene}"), e))?;
        }
        let prepared = cached
            .importer
            .prepare(ctx)
            .map_err(|e| wrap(&format!("prepare content scene {scene}"), e))?;
        let hash = file_hash(&prepared.manifest)?;
        routes
            .scenes
            .get_or_insert_with(Vec::new)
            .push(RawSceneRoute {
                guid,
                manifest: prepared.manifest,
                sha256: hash,
            });
    }
    failed.clear();
    publish_content_routes(&opts.output, &routes)
}

/// Route snapshots must remain immutable because a running Player retains both
/// their manifests and hashes until its next restart.
pub fn publish_content_routes(output: &str, routes: &RawContentRoutes) -> Result<String> {
    let mut data = marshal(routes)?;
    data.push(b'\n');
    let directory = sysutil::join(&[output, "content-routes"]);
    mkdir_all(&directory)?;
    let path = sysutil::join(&[&directory, &format!("{}.json", sha256(&data))]);
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if std::fs::read(&path).ok().as_deref() != Some(data.as_slice()) {
                bail!("published content routes were modified");
            }
            return Ok(path);
        }
        Err(error) => return Err(os_error("open", &path, &error)),
    };
    if let Err(error) = file.write_all(&data) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(os_error("write", &path, &error));
    }
    Ok(path)
}

pub fn read_verified_content_routes(path: &str) -> Result<RawContentRoutes> {
    let routes = RawContentRoutes::read(path)?;
    let invalid = || anyhow!("invalid content route snapshot");
    if routes.schema_version != 1 || !sysutil::is_abs(&routes.project) {
        return Err(invalid());
    }
    let scenes = routes.scenes.as_ref().ok_or_else(invalid)?;
    let mut seen = BTreeSet::new();
    for scene in scenes {
        if !valid_scene_guid(&scene.guid)
            || !sysutil::is_abs(&scene.manifest)
            || !valid_sha256(&scene.sha256)
            || !seen.insert(scene.guid.as_str())
        {
            bail!("invalid or duplicate content scene route");
        }
        if file_hash(&scene.manifest).ok().as_deref() != Some(scene.sha256.as_str()) {
            bail!("published content manifest changed: {}", scene.manifest);
        }
    }
    Ok(routes)
}

/// Prepare every content scene after a watched source change. A root scene can
/// remain equal while a ship scene, its prefab graph, or native content changes.
pub fn content_routes_runtime_equal(previous: &str, next: &str) -> Result<bool> {
    if previous.is_empty() && next.is_empty() {
        return Ok(true);
    }
    if previous.is_empty() || next.is_empty() {
        return Ok(false);
    }
    let a = read_verified_content_routes(previous)?;
    let b = read_verified_content_routes(next)?;
    let (old_scenes, new_scenes) = (
        a.scenes.as_deref().unwrap_or(&[]),
        b.scenes.as_deref().unwrap_or(&[]),
    );
    if a.project != b.project
        || a.addressables != b.addressables
        || old_scenes.len() != new_scenes.len()
    {
        return Ok(false);
    }
    let old: BTreeMap<&str, &RawSceneRoute> = old_scenes
        .iter()
        .map(|scene| (scene.guid.as_str(), scene))
        .collect();
    for scene in new_scenes {
        let Some(before) = old.get(scene.guid.as_str()) else {
            return Ok(false);
        };
        if !scene_runtime_equal(&before.manifest, &scene.manifest)? {
            return Ok(false);
        }
    }
    Ok(true)
}
