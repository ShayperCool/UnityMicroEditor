//! Writes a scene in the Player format for a session that loads scenes with
//! the engine's own loader (see `project::native_scene`): the import worker
//! opens the scene as a build does and writes it, with what it refers to in
//! a file beside it. Both files are kept in the project's store of builds
//! under what they were made from and published with the scene's revision.
use super::{
    built_store,
    importer::NativeProjectImporter,
    player_objects::{Build, importer_identity, write_scene_objects},
    support::{Event, mkdir_all},
};
use crate::{
    cancel::Context,
    playerbake,
    project::{
        BUILTIN_RESOURCE_GUID, List, NativeContentDependency, NativeContentFile,
        NativeContentObject, NativeScene, NativeSceneFile, NeedsNativeScene,
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
        let scripts = super::scripted::scene_environment(&project)?.sha256;
        let key = sha256(
            format!(
                "native-scene-v1\0{}\0{}\0{scripts}\0{}",
                importer_identity(),
                self.environment_sha256,
                needed.key
            )
            .as_bytes(),
        );
        let dependencies_key = sha256(format!("{key}\0dependencies").as_bytes());
        let (scene, dependencies) = match (
            built_store::load(&project, &key),
            built_store::load(&project, &dependencies_key),
        ) {
            (Some(scene), Some(dependencies)) => {
                Event::new("native-scene-reused")
                    .with("scene", &needed.scene)
                    .emit();
                (scene, dependencies)
            }
            _ => {
                let (scene, dependencies) = self.write_native_scene(ctx, &needed.scene)?;
                built_store::store(&project, &dependencies_key, &needed.scene, &dependencies)?;
                built_store::store(&project, &key, &needed.scene, &scene)?;
                (scene, dependencies)
            }
        };
        let dependencies_guid =
            sha256(format!("ume-native-scene-dependencies\0{key}").as_bytes())[..32].to_string();
        let file_sha = sha256(&dependencies.file);
        let mut content = NativeContentFile {
            guid: dependencies_guid.clone(),
            path: self.store_baked(&dependencies_guid, &file_sha, &dependencies.file)?,
            sha256: file_sha,
            ..Default::default()
        };
        for (file_id, type_name) in &dependencies.objects {
            content.objects.push(NativeContentObject {
                file_id: file_id.clone(),
                type_name: type_name.clone(),
            });
        }
        for name in playerbake::external_paths(&dependencies.file)? {
            let Some(dependency) = global(&name) else {
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
            dependencies: List::default(),
        };
        for name in playerbake::external_paths(&scene.file)? {
            let dependency = match global(&name) {
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
        self.session.set_native_scene(NativeScene {
            key: needed.key.clone(),
            scene: file,
            content: vec![content],
        });
        Ok(())
    }

    fn write_native_scene(
        &self,
        ctx: &Context,
        scene: &str,
    ) -> Result<(built_store::Built, built_store::Built)> {
        let output = sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]);
        mkdir_all(&output, 0o700)?;
        let build = Build {
            project: self.opts.project.clone(),
            editor: self.opts.editor.clone(),
            output,
            target: scene.to_string(),
            users: Vec::new(),
            timeout: Duration::from_secs(600),
            reload_list: String::new(),
            shader_cache: String::new(),
        };
        let written = write_scene_objects(ctx, &build, true)?;
        let Some(dependencies) = written.dependencies else {
            bail!("the worker wrote nothing the scene refers to");
        };
        // A script is found by its class; the Player gives its object
        // another type than the Editor does.
        let objects = written
            .report
            .dependencies
            .iter()
            .filter(|object| {
                object.class_name != "UnityEditor.MonoScript" && object.class_name != "?"
            })
            .map(|object| (object.index.to_string(), object.class_name.clone()))
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
                ..Default::default()
            },
        ))
    }
}
