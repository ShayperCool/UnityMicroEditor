//! Writes a scene in the Player format for a session that loads scenes with
//! the engine's own loader (see `project::native_scene`): the import worker
//! opens the scene as a build does and writes it, with what it refers to in
//! a file beside it. Both files are kept in the project's store of builds
//! under what they were made from and published with the scene's revision.
use super::{
    built_store,
    importer::NativeProjectImporter,
    player_objects::{Build, importer_identity},
    support::{Event, mkdir_all},
};
use crate::{
    cancel::Context,
    playerbake,
    project::{
        BUILTIN_RESOURCE_GUID, NativeContentDependency, NativeContentFile, NativeContentObject,
        NativeScene, NativeSceneAlias, NativeSceneFile, NeedsNativeScene,
    },
    sha256, sysutil,
    util::random_id,
};
use anyhow::{Context as _, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

/// The engine's resources a Player holds itself, by the name a written file
/// gives them.
const DEFAULT_RESOURCES: &str = "Library/unity default resources";
const DEFAULT_RESOURCES_GUID: &str = "0000000000000000e000000000000000";
const BUILTIN_EXTRA: &str = "Resources/unity_builtin_extra";
/// The name the worker gives the file of what a scene refers to.
const DEPENDENCIES_NAME: &str = "ume-deps";
/// How the worker names the file a source is published in (a shader).
const PUBLISHED_PREFIX: &str = "ume-asset/";

/// The names the tool's own writer gives the closure's streamed data: of
/// textures and meshes, and of audio.
const DEPENDENCIES_STREAM: &str = "ume-deps.resS";
const DEPENDENCIES_RESOURCE: &str = "ume-deps.resource";

/// Which writer writes a scene, by `UME_OWN_IMPORTER`: the import worker
/// (unset or `0`), the tool's own writer with the worker when it fails
/// (`1`), or the tool's own writer alone (`only`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Writer {
    Worker,
    Own,
    OwnOnly,
}

/// What the tool's own writer writes depends on its code: the hash of the
/// running program, taken once.
fn own_writer_identity() -> &'static str {
    static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    IDENTITY.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::read)
            .map(|program| sha256(&program))
            .unwrap_or_default()
    })
}

pub(super) fn writer() -> Writer {
    match std::env::var("UME_OWN_IMPORTER").as_deref() {
        Ok("only") => Writer::OwnOnly,
        Ok("") | Ok("0") | Err(_) => Writer::Worker,
        Ok(_) => Writer::Own,
    }
}

/// Assets written on their own: the file, its streamed data by name, each
/// object (its local ID, class, source GUID and file ID), and the sources'
/// objects a Player asks for.
struct WrittenAssets {
    file: Vec<u8>,
    resources: Vec<(String, Vec<u8>)>,
    objects: Vec<(String, String, String, String)>,
    wanted: BTreeSet<(String, String)>,
}

/// The scene's file, the file of what it refers to and the files of that
/// file's streamed data.
type WrittenNativeScene = (
    built_store::Built,
    built_store::Built,
    Vec<(String, Vec<u8>)>,
);

/// The global dependency a Player resolves a file of the engine's own by.
fn global(name: &str) -> Option<NativeContentDependency> {
    let guid = match name {
        DEFAULT_RESOURCES => DEFAULT_RESOURCES_GUID,
        BUILTIN_EXTRA => BUILTIN_RESOURCE_GUID,
        _ => return None,
    };
    Some(NativeContentDependency {
        guid: guid.into(),
        global: true,
    })
}

/// What a Player loads to hold assets written on their own
/// (`NativeProjectImporter::write_native_assets`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct NativeAssets {
    /// The file of the assets' objects and of what they refer to, then the
    /// published files it needs.
    #[serde(rename = "nativeContentFiles")]
    pub content: Vec<NativeContentFile>,
    /// Where each object an asset is named by went: a prefab or model by its
    /// root, any other asset by its objects, the main one first.
    pub aliases: Vec<NativeSceneAlias>,
}

impl NativeProjectImporter {
    /// Writes `assets` (logical paths) in the Player format with what they
    /// refer to, as a scene's dependencies are written, and no scene: what a
    /// Player of Play mode code asks for by path while it runs. A worker of
    /// its own writes them; it starts without the project's library, so the
    /// closure of the assets is imported for it.
    pub fn write_native_assets(
        &mut self,
        ctx: &Context,
        assets: &[String],
    ) -> Result<NativeAssets> {
        let written = match writer() {
            Writer::Worker => self.worker_native_assets(ctx, assets),
            Writer::OwnOnly => self.own_native_assets(ctx, assets),
            Writer::Own => self.own_native_assets(ctx, assets).or_else(|error| {
                Event::new("native-assets-own-failed")
                    .with("assets", &assets)
                    .with("error", &format!("{error:#}"))
                    .emit();
                self.worker_native_assets(ctx, assets)
            }),
        }?;
        self.publish_native_assets(ctx, assets, written)
    }

    /// The assets' file as the tool's own writer writes it.
    fn own_native_assets(&self, ctx: &Context, assets: &[String]) -> Result<WrittenAssets> {
        let Some((first, rest)) = assets.split_first() else {
            bail!("no assets to write");
        };
        let (library, format) = self.own_library(ctx, first, rest)?;
        let root = Path::new(&self.opts.project);
        let editor = sysutil::dir(&self.opts.editor);
        let builtin_extra = sysutil::join(&[&editor, "Data/Resources/unity_builtin_extra"]);
        let included: Vec<String> = {
            let mut session = self.session.lock();
            session.index_guids()?;
            assets
                .iter()
                .map(|path| session.guid_of(path).to_string())
                .filter(|guid| !guid.is_empty())
                .collect()
        };
        let roots: Vec<&str> = included.iter().map(String::as_str).collect();
        let assembled = playerbake::write_scene_closure(
            &library,
            &playerbake::Tags::of_project(root)?,
            &playerbake::Sources::of_project(root)?,
            &roots,
            None,
            format,
            DEPENDENCIES_STREAM,
            Some(Path::new(&builtin_extra)),
        )?;
        // A prefab by its root, another asset by all its objects.
        let mut wanted = BTreeSet::new();
        for (_, class, guid, file_id) in &assembled.objects {
            if !included.contains(guid) || class == "UnityEditor.MonoScript" {
                continue;
            }
            match assembled.roots.get(guid) {
                Some(root) if root != file_id => {}
                _ => {
                    wanted.insert((guid.clone(), file_id.to_string()));
                }
            }
        }
        let mut resources = Vec::new();
        if !assembled.stream.is_empty() {
            resources.push((DEPENDENCIES_STREAM.to_string(), assembled.stream));
        }
        if !assembled.resource.is_empty() {
            resources.push((DEPENDENCIES_RESOURCE.to_string(), assembled.resource));
        }
        Ok(WrittenAssets {
            file: assembled.file,
            resources,
            objects: assembled
                .objects
                .into_iter()
                .map(|(id, class, guid, file_id)| {
                    (id.to_string(), class, guid, file_id.to_string())
                })
                .collect(),
            wanted,
        })
    }

    /// The assets' file as the import worker writes it.
    fn worker_native_assets(&self, ctx: &Context, assets: &[String]) -> Result<WrittenAssets> {
        let output = sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]);
        mkdir_all(&output, 0o700)?;
        let build = Build {
            project: self.opts.project.clone(),
            editor: self.opts.editor.clone(),
            output,
            target: String::new(),
            users: assets.to_vec(),
            timeout: Duration::from_secs(3600),
            reload_list: String::new(),
            shader_cache: sysutil::join(&[
                &self.opts.project,
                "Library/UnityMicroEditor/shader-cache",
            ]),
        };
        let written = self
            .asset_worker
            .get_or_init(super::scene_worker::SceneWorker::default)
            .write(ctx, &build)?;
        let Some(file) = written.dependencies else {
            bail!("the worker wrote nothing of {}", assets.join(", "));
        };
        let report = &written.report;
        let wanted: BTreeSet<(String, String)> = report
            .include_roots
            .iter()
            .chain(&report.include_objects)
            .map(|(guid, id)| (guid.clone(), id.to_string()))
            .collect();
        Ok(WrittenAssets {
            file,
            resources: written.resources,
            objects: report
                .dependencies
                .iter()
                .map(|object| {
                    (
                        object.index.to_string(),
                        object.class_name.clone(),
                        object.guid.clone(),
                        object.local_id.to_string(),
                    )
                })
                .collect(),
            wanted,
        })
    }

    /// Stores the assets' file and gives what a Player loads of it.
    fn publish_native_assets(
        &mut self,
        ctx: &Context,
        assets: &[String],
        written: WrittenAssets,
    ) -> Result<NativeAssets> {
        let WrittenAssets {
            file,
            resources,
            objects,
            wanted,
        } = written;
        let file_sha = sha256(&file);
        let guid = sha256(format!("ume-native-assets\0{file_sha}").as_bytes())[..32].to_string();
        let directory = sysutil::join(&[
            &self.opts.native_dir(),
            "baked",
            &format!("{guid}-{file_sha}"),
        ]);
        mkdir_all(&directory, 0o700)?;
        for (name, data) in &resources {
            let path = sysutil::join(&[&directory, name]);
            if std::fs::metadata(&path).is_err() {
                super::support::write_immutable_file(&path, data)?;
            }
        }
        let path = sysutil::join(&[&directory, "dependencies"]);
        if std::fs::metadata(&path).is_err() {
            super::support::write_immutable_file(&path, &file)?;
        }
        let mut content = NativeContentFile {
            guid: guid.clone(),
            path,
            sha256: file_sha,
            ..Default::default()
        };
        let mut aliases = Vec::new();
        for (index, class_name, source_guid, local_id) in objects {
            let source = (source_guid, local_id);
            if class_name == "UnityEditor.MonoScript" || !wanted.contains(&source) {
                continue;
            }
            content.objects.push(NativeContentObject {
                file_id: index.clone(),
                type_name: class_name,
            });
            aliases.push(NativeSceneAlias {
                guid: source.0,
                file_id: source.1,
                file: guid.clone(),
                local_id: index,
            });
        }
        if aliases.is_empty() {
            bail!("the worker wrote no object of {}", assets.join(", "));
        }
        let mut published = Vec::new();
        for name in playerbake::external_paths(&file)? {
            if let Some(dependency) = global(&name) {
                content.dependencies.push(dependency);
                continue;
            }
            let Some(shader) = name.strip_prefix(PUBLISHED_PREFIX) else {
                bail!("the assets refer to {name}, which no published file stands for");
            };
            published.push(shader.to_string());
            content.dependencies.push(NativeContentDependency {
                guid: shader.to_string(),
                global: false,
            });
        }
        // A shader the session has not published yet is built now.
        let mut attempted = BTreeSet::new();
        let shaders = loop {
            let error = match self.session.published_native_content(&published) {
                Ok(files) => break files,
                Err(error) => error,
            };
            let Some(needed) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<crate::project::NeedsImportError>())
            else {
                return Err(error);
            };
            let source = needed
                .path
                .strip_suffix(".meta")
                .unwrap_or(&needed.path)
                .to_string();
            if !attempted.insert(source.clone()) {
                return Err(error);
            }
            self.import_baked(ctx, &source)?;
        };
        let mut files = vec![content];
        files.extend(shaders);
        Ok(NativeAssets {
            content: files,
            aliases,
        })
    }

    /// Writes the scene `needed` asks for, or takes it from the store, and
    /// gives it to the session.
    pub(crate) fn build_native_scene(
        &mut self,
        ctx: &Context,
        needed: &NeedsNativeScene,
    ) -> Result<()> {
        let project = self.opts.project.clone();
        let sources = super::player_objects::open_sources(&project)?;
        let includes = self.session.native_scene_includes()?;
        // The assemblies, and the scripts the worker ties to their classes.
        let environment = super::player_objects::scene_closure(&sources, &needed.scene, &includes)?
            .environment(&project)?;
        let scripts = sha256(
            format!("{}\0{}", environment.sha256, environment.scripts.join("\0")).as_bytes(),
        );
        // The writers' files differ: each keeps its own.
        let version = match writer() {
            Writer::Worker => "native-scene-v4".to_string(),
            Writer::Own | Writer::OwnOnly => {
                format!("native-scene-own-v1\0{}", own_writer_identity())
            }
        };
        let key = sha256(
            format!(
                "{version}\0{}\0{}\0{scripts}\0{}",
                importer_identity(),
                self.environment_sha256,
                needed.key
            )
            .as_bytes(),
        );
        let dependencies_key = sha256(format!("{key}\0dependencies").as_bytes());
        let resource_key =
            |name: &str| sha256(format!("{dependencies_key}\0resource\0{name}").as_bytes());
        let kept = || -> Option<_> {
            let scene = built_store::load(&project, &key)?;
            let dependencies = built_store::load(&project, &dependencies_key)?;
            let mut resources = Vec::new();
            for name in &dependencies.resources {
                resources.push((
                    name.clone(),
                    built_store::load(&project, &resource_key(name))?.file,
                ));
            }
            Some((scene, dependencies, resources))
        };
        let (scene, dependencies, resources) = match kept() {
            Some(kept) => {
                Event::new("native-scene-reused")
                    .with("scene", &needed.scene)
                    .emit();
                kept
            }
            None => {
                let written = match writer() {
                    Writer::Worker => self.write_native_scene(ctx, &needed.scene, &includes),
                    Writer::OwnOnly => self.write_own_native_scene(ctx, &needed.scene, &includes),
                    Writer::Own => self
                        .write_own_native_scene(ctx, &needed.scene, &includes)
                        .or_else(|error| {
                            Event::new("native-scene-own-failed")
                                .with("scene", &needed.scene)
                                .with("error", &format!("{error:#}"))
                                .emit();
                            self.write_native_scene(ctx, &needed.scene, &includes)
                        }),
                };
                let (scene, mut dependencies, resources) = written?;
                for (name, data) in &resources {
                    let built = built_store::Built {
                        file: data.clone(),
                        ..Default::default()
                    };
                    built_store::store(&project, &resource_key(name), &needed.scene, &built)?;
                }
                dependencies.resources = resources.iter().map(|(name, _)| name.clone()).collect();
                built_store::store(&project, &dependencies_key, &needed.scene, &dependencies)?;
                built_store::store(&project, &key, &needed.scene, &scene)?;
                (scene, dependencies, resources)
            }
        };
        let dependencies_guid =
            sha256(format!("ume-native-scene-dependencies\0{key}").as_bytes())[..32].to_string();
        let file_sha = sha256(&dependencies.file);
        // The file reads its streamed data from files of the names it gives
        // them beside it: each content gets a directory of its own.
        let directory = sysutil::join(&[
            &self.opts.native_dir(),
            "baked",
            &format!("{dependencies_guid}-{file_sha}"),
        ]);
        mkdir_all(&directory, 0o700)?;
        for (name, data) in &resources {
            let path = sysutil::join(&[&directory, name]);
            if std::fs::metadata(&path).is_err() {
                super::support::write_immutable_file(&path, data)?;
            }
        }
        let path = sysutil::join(&[&directory, "dependencies"]);
        if std::fs::metadata(&path).is_err() {
            super::support::write_immutable_file(&path, &dependencies.file)?;
        }
        let mut content = NativeContentFile {
            guid: dependencies_guid.clone(),
            path,
            sha256: file_sha,
            ..Default::default()
        };
        // The Player reads back the objects the manifest names by their
        // sources: the root of an included prefab, the objects the worker
        // reported of an addressed asset, every object of the other included
        // assets. The scene finds the others itself; an object of a
        // prefab may be a component whose script the build left missing,
        // which a built Player only warns about.
        let included: Vec<String> = {
            let mut session = self.session.lock();
            session.index_guids()?;
            includes
                .iter()
                .map(|path| session.guid_of(path).to_string())
                .collect()
        };
        for ((file_id, type_name), source) in dependencies.objects.iter().zip(&dependencies.sources)
        {
            let (guid, source_id) = source.split_once('/').unwrap_or((source, ""));
            // Of an addressed asset, what the worker reported it by: a
            // prefab's root, or the objects of another asset a Player can
            // load (not an Editor script object beside a material).
            let named = match (dependencies.roots.get(guid), dependencies.assets.get(guid)) {
                (Some(root), _) => root == source_id,
                (None, Some(objects)) => objects.iter().any(|id| id == source_id),
                (None, None) => true,
            };
            if named && included.iter().any(|known| known == guid) {
                content.objects.push(NativeContentObject {
                    file_id: file_id.clone(),
                    type_name: type_name.clone(),
                });
            }
        }
        // A descriptor names at least one object.
        if content.objects.is_empty()
            && let Some((file_id, type_name)) = dependencies.objects.first()
        {
            content.objects.push(NativeContentObject {
                file_id: file_id.clone(),
                type_name: type_name.clone(),
            });
        }
        let mut published: Vec<String> = Vec::new();
        let mut resolve = |name: &str| -> Option<NativeContentDependency> {
            if let Some(dependency) = global(name) {
                return Some(dependency);
            }
            let guid = name.strip_prefix(PUBLISHED_PREFIX)?;
            if !published.iter().any(|known| known == guid) {
                published.push(guid.to_string());
            }
            Some(NativeContentDependency {
                guid: guid.to_string(),
                global: false,
            })
        };
        for name in playerbake::external_paths(&dependencies.file)? {
            let Some(dependency) = resolve(&name) else {
                bail!(
                    "what the scene refers to lies in {name}, which no published file stands for"
                );
            };
            content.dependencies.push(dependency);
        }
        let scene_guid = sha256(format!("ume-native-scene\0{key}").as_bytes())[..32].to_string();
        let scene_sha = sha256(&scene.file);
        let mut file = NativeSceneFile {
            path: self.store_baked(&scene_guid, &scene_sha, &scene.file)?,
            sha256: scene_sha,
            ..Default::default()
        };
        for name in playerbake::external_paths(&scene.file)? {
            let dependency = match resolve(&name) {
                Some(dependency) => dependency,
                None if name == DEPENDENCIES_NAME => NativeContentDependency {
                    guid: dependencies_guid.clone(),
                    global: false,
                },
                None => {
                    bail!("the written scene refers to {name}, which no published file stands for")
                }
            };
            file.dependencies.push(dependency);
        }
        // Where each source object went, for what the manifest names by its
        // source: the render pipeline asset and the preloaded assets.
        let objects = dependencies
            .sources
            .iter()
            .zip(&dependencies.objects)
            .map(|(source, (index, _))| (source.clone(), index.clone()))
            .collect();
        self.session.set_native_scene(NativeScene {
            key: needed.key.clone(),
            scene: file,
            content: vec![content],
            published,
            objects,
            roots: dependencies.roots.clone(),
            assets: dependencies.assets.clone(),
        });
        Ok(())
    }

    /// Writes the scene as the worker does, with the tool's own writer and no
    /// worker: the engine's classes from the release's pack, the scripts laid
    /// out from the Editor compilation the worker would load, against the
    /// session's Player. Shaders stay external, published by the session as
    /// for the worker's files; what else the writer does not import yet
    /// (models, animator controllers, ...) is reported and left out.
    fn write_own_native_scene(
        &self,
        ctx: &Context,
        scene: &str,
        includes: &[String],
    ) -> Result<WrittenNativeScene> {
        let started = std::time::Instant::now();
        let project = self.opts.project.clone();
        let (library, format) = self.own_library(ctx, scene, includes)?;
        let root = Path::new(&project);
        let editor = sysutil::dir(&self.opts.editor);
        let builtin_extra = sysutil::join(&[&editor, "Data/Resources/unity_builtin_extra"]);
        let (scene_guid, included) = {
            let mut session = self.session.lock();
            session.index_guids()?;
            let included: Vec<String> = includes
                .iter()
                .map(|path| session.guid_of(path).to_string())
                .collect();
            (session.guid_of(scene).to_string(), included)
        };
        if scene_guid.is_empty() {
            bail!("{scene} has no GUID");
        }
        let mut roots: Vec<&str> = vec![&scene_guid];
        roots.extend(
            included
                .iter()
                .map(String::as_str)
                .filter(|g| !g.is_empty()),
        );
        let assembled = playerbake::write_scene_closure(
            &library,
            &playerbake::Tags::of_project(root)?,
            &playerbake::Sources::of_project(root)?,
            &roots,
            Some(&scene_guid),
            format,
            DEPENDENCIES_STREAM,
            Some(Path::new(&builtin_extra)),
        )?;
        self.finish_own_native_scene(scene, &included, assembled, started)
    }

    /// The engine's classes of the project's release and its scripts laid
    /// out as the worker would lay them out (the Editor compilation's
    /// assemblies and the plugins it names, against the session's Player),
    /// made once for what those assemblies hold.
    fn own_library(
        &self,
        ctx: &Context,
        scene: &str,
        includes: &[String],
    ) -> Result<(Arc<playerbake::Library>, playerbake::FileFormat)> {
        let project = self.opts.project.clone();
        if self.opts.player.is_empty() {
            bail!("the tool's own writer lays scripts out against a Player, and there is none");
        }
        let version = crate::engine::project_version(&project)?;
        let format = playerbake::FileFormat::of_release(&version)
            .with_context(|| format!("the tool writes no files of {version}"))?;
        let sources = super::player_objects::open_sources(&project)?;
        let environment = super::player_objects::scene_closure(&sources, scene, includes)?
            .environment(&project)?;
        let identity = format!("{version}\0{}", environment.sha256);
        if let Some((known, library)) = self.own_library.lock().unwrap().as_ref()
            && *known == identity
        {
            return Ok((library.clone(), format));
        }
        let mut library = playerbake::Library::of_release(&version)
            .with_context(|| format!("no pack of the engine classes of {version}"))??;
        let templates = playerbake::Templates::of_release(&version)
            .with_context(|| format!("no script templates of {version}"))?;
        let mut assemblies = BTreeMap::new();
        for name in &environment.assemblies {
            assemblies.insert(name.clone(), environment.assembly_path(name));
        }
        for plugin in &environment.plugins {
            let path = super::context::resolve_script_source(&environment.library, plugin)?;
            let name = sysutil::base(plugin);
            let name = name.strip_suffix(".dll").unwrap_or(&name).to_string();
            assemblies.insert(name, path);
        }
        let layouts = crate::compiler::script_layouts_with(
            ctx,
            &project,
            &self.opts.editor,
            &self.opts.player,
            assemblies,
        )?;
        let layouts: playerbake::Layouts = serde_json::from_slice(&layouts)?;
        let layout = playerbake::Layout::new(&layouts, &templates);
        let root = Path::new(&project);
        for (name, error) in
            library.add_scripts(&layout, &playerbake::script_execution_orders(root))?
        {
            Event::new("native-scene-script-not-laid-out")
                .with("script", &name)
                .with("error", &error)
                .emit();
        }
        let editor = sysutil::dir(&self.opts.editor);
        playerbake::texcomp::use_editor(Path::new(&editor));
        playerbake::audio::use_editor(Path::new(&editor));
        let library = Arc::new(library);
        *self.own_library.lock().unwrap() = Some((identity, library.clone()));
        Ok((library, format))
    }

    /// The scene's file and its closure's, out of what the writer wrote.
    fn finish_own_native_scene(
        &self,
        scene: &str,
        included: &[String],
        assembled: playerbake::Assembled,
        started: std::time::Instant,
    ) -> Result<WrittenNativeScene> {
        let Some(scene_file) = assembled.scene else {
            bail!("the tool wrote no file of {scene}");
        };
        let listed: Vec<_> = assembled
            .objects
            .iter()
            .filter(|(_, class, ..)| class != "UnityEditor.MonoScript")
            .collect();
        let mut assets: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (_, _, guid, file_id) in &listed {
            if included.contains(guid) && !assembled.roots.contains_key(guid) {
                assets
                    .entry(guid.clone())
                    .or_default()
                    .push(file_id.to_string());
            }
        }
        let refused: Vec<String> = assembled
            .refused
            .iter()
            .map(|(guid, why)| format!("{guid}: {why}"))
            .collect();
        Event::new("native-scene-written")
            .with("scene", &scene)
            .with("writer", "own")
            .with("refused", &refused)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        let mut resources = Vec::new();
        if !assembled.stream.is_empty() {
            resources.push((DEPENDENCIES_STREAM.to_string(), assembled.stream));
        }
        if !assembled.resource.is_empty() {
            resources.push((DEPENDENCIES_RESOURCE.to_string(), assembled.resource));
        }
        Ok((
            built_store::Built {
                file: scene_file,
                ..Default::default()
            },
            built_store::Built {
                file: assembled.file,
                objects: listed
                    .iter()
                    .map(|(id, class, ..)| (id.to_string(), class.clone()))
                    .collect(),
                sources: listed
                    .iter()
                    .map(|(_, _, guid, file_id)| format!("{guid}/{file_id}"))
                    .collect(),
                roots: assembled
                    .roots
                    .iter()
                    .map(|(guid, id)| (guid.clone(), id.to_string()))
                    .collect(),
                assets,
                ..Default::default()
            },
            resources,
        ))
    }

    fn write_native_scene(
        &self,
        ctx: &Context,
        scene: &str,
        includes: &[String],
    ) -> Result<WrittenNativeScene> {
        let output = sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]);
        mkdir_all(&output, 0o700)?;
        let build = Build {
            project: self.opts.project.clone(),
            editor: self.opts.editor.clone(),
            output,
            target: scene.to_string(),
            // A scene's build includes these assets instead.
            users: includes.to_vec(),
            // A first write imports everything the scene refers to and
            // compiles its shaders.
            timeout: Duration::from_secs(3600),
            reload_list: String::new(),
            shader_cache: sysutil::join(&[
                &self.opts.project,
                "Library/UnityMicroEditor/shader-cache",
            ]),
        };
        let written = self
            .scene_worker
            .get_or_init(|| super::scene_worker::for_project(&self.opts.project))
            .write(ctx, &build)?;
        let Some(dependencies) = written.dependencies else {
            bail!("the worker wrote nothing the scene refers to");
        };
        // A script is found by its class; the Player gives its object
        // another type than the Editor does.
        let listed: Vec<_> = written
            .report
            .dependencies
            .iter()
            .filter(|object| {
                object.class_name != "UnityEditor.MonoScript" && object.class_name != "?"
            })
            .collect();
        let objects = listed
            .iter()
            .map(|object| (object.index.to_string(), object.class_name.clone()))
            .collect();
        let sources = listed
            .iter()
            .map(|object| format!("{}/{}", object.guid, object.local_id))
            .collect();
        Event::new("native-scene-written")
            .with("scene", &scene)
            .with("imports", &written.imports)
            .seconds("elapsedSeconds", written.seconds)
            .emit();
        Ok((
            built_store::Built {
                file: written.scene,
                ..Default::default()
            },
            built_store::Built {
                file: dependencies,
                objects,
                sources,
                roots: written
                    .report
                    .include_roots
                    .iter()
                    .map(|(guid, id)| (guid.clone(), id.to_string()))
                    .collect(),
                assets: written.report.include_objects.iter().fold(
                    std::collections::BTreeMap::new(),
                    |mut assets: std::collections::BTreeMap<String, Vec<String>>, (guid, id)| {
                        assets.entry(guid.clone()).or_default().push(id.to_string());
                        assets
                    },
                ),
                ..Default::default()
            },
            written.resources,
        ))
    }
}
