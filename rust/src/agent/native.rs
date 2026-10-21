//! Project settings references, the native import catalog produced by an owned
//! staging Editor, and the catalog seed of a source preparation.
use super::{
    Any, AnyMap, Service, actions::request, fail, join, lstat, marshal, not_exist, obj, read_json,
    read_map, staging::phase_id, text, text_of, unity_version, unmarshal_map,
};
use crate::{
    cancel::Context,
    compiler::{CompilerGraphExport, Walk, WalkEntry, compiler_command, walk_dir},
    editorbridge::{self, Client},
    packages,
    project::{BUILTIN_RESOURCE_GUID, is_owned_cache_path, lexpath, parse_unity_yaml, value},
    sysutil,
    util::{agent_canonical, agent_immutable_json, file_hash, read_agent_file, within_directory},
};
use anyhow::{Result, anyhow};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::Duration,
};

/// The single document of a project setting, empty when absent.
fn setting(project: &str, path: &str) -> Result<AnyMap> {
    let data = match read_agent_file(&join(&[project, path]), 32 << 20) {
        Ok(data) => data,
        Err(error) if not_exist(&error) => return Ok(AnyMap::new()),
        Err(error) => return Err(error),
    };
    let mut documents = parse_unity_yaml(&data)?;
    if documents.len() != 1 {
        return Err(anyhow!(
            "project setting requires exactly one authored document: {path}"
        ));
    }
    Ok(documents.remove(0).data.unwrap_or_default())
}

/// `{guid, fileId, type}` of an authored reference.
fn reference(value: Option<&Any>) -> AnyMap {
    let empty = AnyMap::new();
    let map = value.and_then(Any::as_object).unwrap_or(&empty);
    obj([
        ("guid", text(map, "guid").into()),
        (
            "fileId",
            match map.get("fileID") {
                None | Some(Any::Null) => "0".to_string(),
                Some(id) => value::sprint(id),
            }
            .into(),
        ),
        ("type", map.get("type").cloned().unwrap_or(Any::Null)),
    ])
}

/// The leading integer token of `text` after white space, `None` when there is none.
fn sscan_int(text: &str) -> Option<i64> {
    let text = text.trim_start_matches([' ', '\t', '\r', '\n']);
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let lower = rest.to_ascii_lowercase();
    let (radix, digits) = if let Some(digits) = lower.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = lower.strip_prefix("0b") {
        (2, digits)
    } else if let Some(digits) = lower.strip_prefix("0o") {
        (8, digits)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, &lower[1..])
    } else {
        (10, lower.as_str())
    };
    let token: String = digits
        .chars()
        .take_while(|c| c.is_digit(radix) || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    if token.is_empty() {
        // A lone "0" before a non-digit still scans as zero.
        return (radix == 8 && !lower.starts_with("0o")).then_some(0);
    }
    let magnitude = i64::from_str_radix(&token, radix).ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// The render pipeline, its global settings, the preloaded assets and their GUIDs.
pub(crate) type References = (AnyMap, Vec<Any>, Vec<Any>, Vec<String>);

/// The render pipeline, its global settings and the preloaded assets of the project
/// settings, plus their distinct asset GUIDs.
pub(crate) fn project_references(project: &str) -> Result<References> {
    let graphics = setting(project, "ProjectSettings/GraphicsSettings.asset")?;
    let quality = setting(project, "ProjectSettings/QualitySettings.asset")?;
    let player = setting(project, "ProjectSettings/ProjectSettings.asset")?;
    let mut pipeline = reference(graphics.get("m_CustomRenderPipeline"));
    let levels = value::array(value::get(&quality, "m_QualitySettings"));
    let mut index: i64 = 0;
    for candidate in [
        value::get(&quality, "m_CurrentQuality"),
        value::get(&quality, "m_PerPlatformDefaultQuality").field("Standalone"),
    ] {
        if !candidate.is_null()
            && let Some(scanned) = sscan_int(&value::sprint(candidate))
        {
            index = scanned;
        }
    }
    if !levels.is_empty() {
        if index < 0 || index as usize >= levels.len() {
            return Err(anyhow!(
                "Standalone quality index is outside authored settings"
            ));
        }
        let candidate = reference(
            levels[index as usize]
                .as_object()
                .and_then(|level| level.get("customRenderPipeline")),
        );
        if candidate["fileId"] != Any::from("0") {
            pipeline = candidate;
        }
    }
    let mut global = Vec::new();
    if let Some(settings) = value::get(&graphics, "m_RenderPipelineGlobalSettingsMap").as_object() {
        for (type_name, entry) in settings {
            let entry = reference(Some(entry));
            if entry["fileId"] != Any::from("0") {
                global.push(Any::Object(obj([
                    ("pipelineTypeName", type_name.clone().into()),
                    ("guid", entry["guid"].clone()),
                    ("fileId", entry["fileId"].clone()),
                ])));
            }
        }
    }
    let preloads: Vec<Any> = value::array(value::get(&player, "preloadedAssets"))
        .iter()
        .map(|entry| Any::Object(reference(Some(entry))))
        .collect();
    let mut seen = BTreeSet::new();
    let mut guids = Vec::new();
    let mut include = |entry: &AnyMap| {
        let guid = text(entry, "guid");
        if !guid.is_empty()
            && entry.get("fileId") != Some(&Any::from("0"))
            && guid != BUILTIN_RESOURCE_GUID
            && guid != "0000000000000000e000000000000000"
            && seen.insert(guid.clone())
        {
            guids.push(guid);
        }
    };
    include(&pipeline);
    for entry in global.iter().chain(&preloads) {
        include(value::object(entry));
    }
    Ok((pipeline, global, preloads, guids))
}

/// Authored content or settings that only the native
/// importer can provide.
pub(crate) fn may_need_native(project: &str) -> Result<bool> {
    const NATIVE: [&str; 35] = [
        ".fbx",
        ".obj",
        ".dae",
        ".blend",
        ".wav",
        ".ogg",
        ".mp3",
        ".aiff",
        ".aif",
        ".flac",
        ".anim",
        ".controller",
        ".overridecontroller",
        ".mat",
        ".shader",
        ".shadergraph",
        ".compute",
        ".inputactions",
        ".webm",
        ".ttf",
        ".otf",
        ".mixer",
        ".png",
        ".jpg",
        ".jpeg",
        ".bmp",
        ".tga",
        ".tif",
        ".tiff",
        ".gif",
        ".dds",
        ".psd",
        ".exr",
        ".hdr",
        ".spriteatlas",
    ];
    let mut needed = false;
    let walked = walk_dir(&join(&[project, "Assets"]), &mut |entry: &WalkEntry| {
        if entry.kind.is_symlink() {
            return Err(anyhow!("native preparation refuses authored symlinks"));
        }
        if !entry.is_dir() && NATIVE.contains(&sysutil::ext(entry.path).to_lowercase().as_str()) {
            needed = true;
        }
        Ok(Walk::Continue)
    });
    let settings = project_references(project).map(|(_, _, _, settings)| settings);
    walked?;
    Ok(needed || !settings?.is_empty())
}

/// Seeds a new stage from the importer cache of a project nobody has open:
/// the stopped Editor this service owned, or a target without any Editor. A
/// live AssetDB is never copied. Returns whether the stage is warm and whether
/// that cache came from the owned Editor.
pub(crate) fn seed_stage_library(
    ctx: &Context,
    service: &Service,
    stage: &str,
) -> Result<(bool, bool)> {
    let project = service.0.project.as_str();
    let mut owned = false;
    if let Some(client) = service.client() {
        let connection = client.own_process_or_attach(ctx)?;
        if connection.owned {
            client.close()?;
            service.set_client(None);
            owned = true;
        }
    }
    let seeded = lstat(&join(&[stage, ".micro-editor-library-seed.json"])).is_ok();
    let warm = seeded
        || (lstat(&join(&[stage, "Library"])).is_err()
            && lstat(&join(&[project, "Library/ArtifactDB"])).is_ok()
            && !crate::editorbridge::project_is_open(project)?);
    if warm {
        seed_native_library(ctx, project, stage, owned)?;
    }
    Ok((warm, owned))
}

pub(crate) fn exclude_library_seed_entry(name: &str) -> bool {
    matches!(
        name,
        "UnityMicroEditor" | "PackageManager" | "ScriptAssemblies" | "Bee"
    ) || name.ends_with(".pid")
        || name.ends_with(".lock")
        || name.ends_with("-lock")
}

/// Copies the importer cache of a stopped, owned
/// Editor into a staging clone, once.
pub(crate) fn seed_native_library(
    ctx: &Context,
    source: &str,
    stage: &str,
    owned: bool,
) -> Result<()> {
    let library = join(&[stage, "Library"]);
    let marker = join(&[stage, ".micro-editor-library-seed.json"]);
    match read_map(&marker, 65536) {
        Ok(existing) => {
            if existing.get("sourceProject") != Some(&Any::from(source))
                || existing.get("stageProject") != Some(&Any::from(stage))
            {
                return Err(anyhow!("native Library seed belongs to another project"));
            }
            let stopped = |key: &str| existing.get(key) == Some(&Any::Bool(true));
            if existing.get("seedPolicy") != Some(&Any::Float(2.0))
                || !(stopped("sourceEditorOwnedAndStopped") || stopped("sourceEditorStopped"))
            {
                return Err(fail(
                    "staging_incomplete",
                    "Library seed predates transient cache exclusions; use a new preparation requestId",
                ));
            }
            return Ok(());
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }
    if !matches!(sysutil::lstat(&library), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(fail(
            "staging_incomplete",
            "native staging Library exists without a completed seed record; use a new preparation requestId",
        ));
    }
    agent_canonical(&library)?;
    sysutil::mkdir_all(&library)?;
    let source_library = join(&[source, "Library"]);
    let mut names: Vec<String> = fs::read_dir(&source_library)
        .map_err(|error| sysutil::Error::os("open", &source_library, &error))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        // Tool stages recurse into this source Library. Compiler/UPM state and
        // process locks carry the source path or stopped process identity. Unity
        // regenerates them for the clone; retain imported assets and shaders.
        if exclude_library_seed_entry(&name) {
            continue;
        }
        let path = join(&[&source_library, &name]);
        agent_canonical(&path)?;
        compiler_command(
            ctx,
            "",
            "cp",
            &["-a", "--reflink=auto", "--", &path, &library],
        )?;
    }
    let mut record = obj([
        ("schemaVersion", 1.into()),
        ("seedPolicy", 2.into()),
        ("sourceProject", source.into()),
        ("stageProject", stage.into()),
        ("sourceEditorOwnedAndStopped", owned.into()),
        ("copyMode", "reflink-auto".into()),
        ("excluded", "Library/UnityMicroEditor".into()),
        (
            "excludedTransientCaches",
            [
                "PackageManager",
                "ScriptAssemblies",
                "Bee",
                "*.pid",
                "*.lock",
                "*-lock",
            ]
            .map(Any::from)
            .to_vec()
            .into(),
        ),
    ]);
    // A project nobody has open is as consistent as one whose owned Editor
    // stopped; the record says which of the two this seed came from.
    if !owned {
        record.insert("sourceEditorStopped".into(), true.into());
    }
    agent_immutable_json(&marker, &Any::Object(record))
}

/// Binds a native catalog and the project settings
/// references into the seed manifest of a source preparation.
pub(crate) fn attach_native_seed(
    seed: &mut AnyMap,
    project: &str,
    catalog_path: &str,
) -> Result<()> {
    if catalog_path.is_empty() {
        return Ok(());
    }
    let catalog = read_map(catalog_path, 32 << 20)?;
    let (pipeline, global, preloads, _) = project_references(project)?;
    let member = |key: &str| catalog.get(key).cloned().unwrap_or(Any::Null);
    seed.insert("importedAssetCatalog".into(), catalog_path.into());
    seed.insert("importedAssetPacks".into(), member("packs"));
    seed.insert("importedAssetBindings".into(), member("deferredBindings"));
    seed.insert("renderPipelineGuid".into(), pipeline["guid"].clone());
    seed.insert("renderPipelineFileId".into(), pipeline["fileId"].clone());
    // Project settings references must join scene references in the source
    // asset closure before manifest publication.
    let mut selected = BTreeSet::new();
    let mut add = |entry: &AnyMap| {
        let (guid, file_id) = (text(entry, "guid"), text(entry, "fileId"));
        if !guid.is_empty() && file_id != "0" {
            selected.insert(format!("{guid}:{file_id}"));
        }
    };
    add(&pipeline);
    for entry in global.iter().chain(&preloads) {
        add(value::object(entry));
    }
    seed.insert("renderPipelineGlobalSettings".into(), global.into());
    seed.insert("preloadedAssets".into(), preloads.into());
    let mut assets = Vec::new();
    for asset in value::array(value::get(&catalog, "assets")) {
        let key = format!("{}:{}", text_of(asset, "guid"), text_of(asset, "fileId"));
        if selected.remove(&key) {
            assets.push(asset.clone());
        }
    }
    // Remaining preloads can be source prefabs or game ScriptableObjects. The
    // same source asset closure resolves them; no native identity is invented.
    seed.insert("assets".into(), assets.into());
    Ok(())
}

/// A catalog that only registers package source roots.
pub(crate) fn identity_catalog(project: &str, graph_path: &str, directory: &str) -> Result<String> {
    let graph: CompilerGraphExport = read_json(graph_path, 32 << 20)?;
    if graph.project != project || graph.schema_version != 1 {
        return Err(anyhow!("source identity graph belongs to another project"));
    }
    if graph.source_roots.is_empty() {
        return Ok(String::new());
    }
    for root in graph.source_roots.iter() {
        if !root.path.starts_with("Packages/")
            || sysutil::clean(&root.path) != root.path
            || !sysutil::is_abs(&root.source)
        {
            return Err(anyhow!("invalid registered package root in public graph"));
        }
        if !lexpath::eval_symlinks(&root.source).is_ok_and(|canonical| canonical == root.source) {
            return Err(anyhow!(
                "registered package root is unavailable or not canonical"
            ));
        }
    }
    let empty = || Any::Array(Vec::new());
    let catalog = obj([
        ("schemaVersion", 1.into()),
        ("project", project.into()),
        ("engineVersion", unity_version(project)?.into()),
        ("backend", "unity_editor_public_identity".into()),
        ("sourceRoots", value::raw(&graph.source_roots)?),
        ("sourceDependencies", empty()),
        ("assets", empty()),
        ("packs", empty()),
        ("modelPrefabs", empty()),
        ("deferredBindings", empty()),
        ("nativeAssetsPresent", false.into()),
        ("containsScenes", false.into()),
        ("sceneLoader", "raw".into()),
        ("cookedRegistryVerified", true.into()),
        ("success", true.into()),
    ]);
    let path = join(&[directory, "package-identities.json"]);
    agent_immutable_json(&path, &Any::Object(catalog))?;
    Ok(path)
}

impl Service {
    /// Imports the scene's native content in the owned staging
    /// copy and publishes the verified catalog. Empty when the project has no
    /// native assets.
    pub(crate) fn prepare_native(
        &self,
        ctx: &Context,
        scene: &str,
        id: &str,
        directory: &str,
    ) -> Result<String> {
        let project = self.0.project.as_str();
        let (stage, snapshot) = super::sync_staging(ctx, project, &self.0.editor)?;
        seed_stage_library(ctx, self, &stage)?;
        let (_, _, _, settings) = project_references(project)?;
        let client = Client::new(
            &self.lifetime(),
            editorbridge::Options {
                project: stage.clone(),
                editor: self.0.editor.clone(),
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
        self.stage(
            "import_native_content",
            "importing the scene's native assets in the owned staging copy",
            &join(&[client.mailbox(), "editor.log"]),
        );
        let arguments = marshal(&Any::Object(obj([
            ("scenes", vec![Any::from(scene)].into()),
            (
                "settingsGuids",
                settings
                    .into_iter()
                    .map(Any::from)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            ("assetPaths", Any::Array(Vec::new())),
        ])))?;
        let (response, data) = request(
            &client,
            ctx,
            &phase_id(id, "native"),
            "native_prepare",
            &arguments,
        );
        let data =
            match data {
                Ok(data) => data,
                Err(error) => {
                    if response.error.as_ref().is_some_and(|failure| {
                        failure.message.contains("No native assets were found")
                    }) {
                        return Ok(String::new());
                    }
                    return Err(error);
                }
            };
        client.close()?;
        super::verify_snapshot(ctx, &snapshot)?;
        let result = unmarshal_map(&data)?.unwrap_or_default();
        let catalog_path = text(&result, "catalogPath");
        if !is_owned_cache_path(project, &catalog_path) {
            return Err(anyhow!(
                "native exporter catalog is outside owned project cache"
            ));
        }
        let mut catalog = read_map(&catalog_path, 32 << 20)?;
        let is = |key: &str, expected: Any| catalog.get(key) == Some(&expected);
        if !is("schemaVersion", Any::Float(1.0))
            || !is("success", true.into())
            || !is("engineVersion", unity_version(project)?.into())
            || !is("containsScenes", false.into())
            || !is("sceneLoader", "raw".into())
            || !is("cookedRegistryVerified", true.into())
        {
            return Err(anyhow!(
                "native catalog lacks the pinned public runtime/cooked registry contract"
            ));
        }
        let mut source_roots: BTreeMap<String, String> = BTreeMap::new();
        let mut resolved: Option<packages::Snapshot> = None;
        if let Some(Any::Array(roots)) = catalog.get_mut("sourceRoots") {
            for root in roots {
                let (logical, mut physical) = (text_of(root, "path"), text_of(root, "source"));
                if !logical.starts_with("Packages/")
                    || sysutil::clean(&logical) != logical
                    || !sysutil::is_abs(&physical)
                {
                    return Err(anyhow!("invalid native package source root"));
                }
                if within_directory(&physical, &stage) {
                    let relative = sysutil::rel(&stage, &physical)
                        .ok_or_else(|| anyhow!("Rel: can't make {physical} relative to {stage}"))?;
                    physical = join(&[project, &relative]);
                    // The target's own PackageCache exists only when its Editor
                    // resolved it. Our resolver holds the same verified content.
                    if lstat(&physical).is_err() {
                        if resolved.is_none() {
                            resolved = Some(packages::resolve(
                                ctx,
                                project,
                                &self.0.editor,
                                packages::Options::default().with_runtime_package(project)?,
                            )?);
                        }
                        if let Some(package) = resolved
                            .iter()
                            .flat_map(|snapshot| snapshot.packages.iter())
                            .find(|package| package.logical_root == logical)
                        {
                            physical = package.path.clone();
                        }
                    }
                }
                let canonical = lexpath::eval_symlinks(&physical).map_err(|error| {
                    anyhow!("native package root missing from authored target: {logical}: {error}")
                })?;
                if let Some(root) = root.as_object_mut() {
                    root.insert("source".into(), canonical.clone().into());
                }
                source_roots.insert(logical, canonical);
            }
        }
        if let Some(Any::Array(dependencies)) = catalog.get_mut("sourceDependencies") {
            for dependency in dependencies {
                let logical = text_of(dependency, "path");
                if sysutil::clean(&logical) != logical || sysutil::is_abs(&logical) {
                    return Err(anyhow!("invalid native dependency path"));
                }
                let physical =
                    if logical.starts_with("Assets/") || logical.starts_with("ProjectSettings/") {
                        join(&[project, &logical])
                    } else if logical.starts_with("Packages/") {
                        let parts: Vec<&str> = logical.splitn(3, '/').collect();
                        let root = (parts.len() == 3)
                            .then(|| source_roots.get(&format!("{}/{}", parts[0], parts[1])))
                            .flatten()
                            .filter(|root| !root.is_empty());
                        let Some(root) = root else {
                            return Err(anyhow!("native dependency lacks an exact package root"));
                        };
                        join(&[root, parts[2]])
                    } else {
                        return Err(anyhow!(
                            "native dependency is outside authored/package roots"
                        ));
                    };
                if !file_hash(&physical).is_ok_and(|hash| hash == text_of(dependency, "sha256")) {
                    return Err(anyhow!("native source dependency changed: {logical}"));
                }
                if let Some(dependency) = dependency.as_object_mut() {
                    dependency.insert("physicalPath".into(), physical.clone().into());
                    if logical.starts_with("Packages/") {
                        dependency.insert("sourcePath".into(), physical.into());
                    }
                }
            }
        }
        let packs = value::array(value::get(&catalog, "packs"));
        if packs.is_empty()
            && (catalog.get("nativeAssetsPresent") != Some(&Any::Bool(false))
                || !value::array(value::get(&catalog, "assets")).is_empty())
        {
            return Err(anyhow!("native catalog has no verified packs"));
        }
        for pack in packs {
            let path = text_of(pack, "bundlePath");
            if !is_owned_cache_path(project, &path) {
                return Err(anyhow!("native bundle is outside owned persistent cache"));
            }
            if !file_hash(&path).is_ok_and(|hash| hash == text_of(pack, "sha256")) {
                return Err(anyhow!(
                    "native bundle changed after verified registry readback"
                ));
            }
        }
        catalog.insert("project".into(), project.into());
        catalog.insert("backend".into(), "unity_editor_public_api".into());
        catalog.insert("importProject".into(), stage.into());
        let destination = join(&[directory, "native-catalog.json"]);
        agent_immutable_json(&destination, &Any::Object(catalog))?;
        Ok(destination)
    }
}
