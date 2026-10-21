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
        NativeScene, NativeSceneFile, NeedsNativeScene,
    },
    sha256, sysutil,
    util::random_id,
};
use anyhow::{Result, bail};
use std::time::Duration;

/// The engine's resources a Player holds itself, by the name a written file
/// gives them.
const DEFAULT_RESOURCES: &str = "Library/unity default resources";
const DEFAULT_RESOURCES_GUID: &str = "0000000000000000e000000000000000";
const BUILTIN_EXTRA: &str = "Resources/unity_builtin_extra";
/// The name the worker gives the file of what a scene refers to.
const DEPENDENCIES_NAME: &str = "ume-deps";
/// How the worker names the file a source is published in (a shader).
const PUBLISHED_PREFIX: &str = "ume-asset/";

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

impl NativeProjectImporter {
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
        let key = sha256(
            format!(
                "native-scene-v4\0{}\0{}\0{scripts}\0{}",
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
                let (scene, mut dependencies, resources) =
                    self.write_native_scene(ctx, &needed.scene, &includes)?;
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
        // sources: the root of an included prefab, every object of the other
        // included assets. The scene finds the others itself; an object of a
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
            let named = dependencies
                .roots
                .get(guid)
                .is_none_or(|root| root == source_id);
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
        });
        Ok(())
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
                ..Default::default()
            },
            written.resources,
        ))
    }
}
