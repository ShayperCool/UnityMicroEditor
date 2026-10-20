//! Builds the templates of native components the raw loader has no adapter
//! for (see `project::native_template`): the import worker writes the Player
//! objects of each template prefab with Unity's own build code, so that any
//! engine component reaches the Player with every field it was authored
//! with. The written file is kept in the project's store of builds under
//! what it was made from, and published as derived content of the session.
use super::{
    built_store,
    importer::NativeProjectImporter,
    player_objects::{importer_identity, write_generated_objects},
    support::{Event, mkdir_all},
};
use crate::{
    cancel::Context,
    playerbake,
    project::{NativeContentFile, NativeContentObject, NativeTemplate},
    sha256, sysutil,
    util::random_id,
};
use anyhow::{Result, bail};
use std::{collections::BTreeMap, time::Duration};

impl NativeProjectImporter {
    /// Builds `templates` into one ContentFile and gives it to the session.
    pub(crate) fn build_native_templates(
        &mut self,
        ctx: &Context,
        templates: &[NativeTemplate],
    ) -> Result<()> {
        let mut keys: Vec<&str> = templates
            .iter()
            .map(|template| template.key.as_str())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let key = sha256(
            format!(
                "native-templates-v1\0{}\0{}\0{}",
                importer_identity(),
                self.environment_sha256,
                keys.join("\0")
            )
            .as_bytes(),
        );
        let guid = sha256(format!("ume-native-templates\0{key}").as_bytes())[..32].to_string();
        let project = self.opts.project.clone();
        let built = match built_store::load(&project, &key) {
            Some(built) => {
                Event::new("native-templates-reused")
                    .with("templates", &templates.len())
                    .emit();
                built
            }
            None => {
                let built = self.write_templates(ctx, templates)?;
                built_store::store(&project, &key, "native component templates", &built)?;
                built
            }
        };
        let file_sha = sha256(&built.file);
        let path = self.store_baked(&guid, &file_sha, &built.file)?;
        let mut content = NativeContentFile {
            guid,
            path,
            sha256: file_sha,
            ..Default::default()
        };
        for (file_id, type_name) in built.objects {
            content
                .objects
                .push(NativeContentObject { file_id, type_name });
        }
        self.session.set_derived_native_content(&content)
    }

    fn write_templates(
        &self,
        ctx: &Context,
        templates: &[NativeTemplate],
    ) -> Result<built_store::Built> {
        let started = std::time::Instant::now();
        let mut generated = BTreeMap::new();
        let mut write = Vec::new();
        for template in templates {
            let path = template.path();
            if generated.contains_key(&path) {
                continue;
            }
            generated.insert(
                path.clone(),
                (
                    template.prefab.clone().into_bytes(),
                    template.meta().into_bytes(),
                ),
            );
            write.push(path);
        }
        let output = sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]);
        mkdir_all(&output, 0o700)?;
        let build = super::player_objects::Build {
            project: self.opts.project.clone(),
            editor: self.opts.editor.clone(),
            output,
            target: write.first().cloned().unwrap_or_default(),
            users: Vec::new(),
            timeout: Duration::from_secs(600),
            reload_list: String::new(),
            shader_cache: String::new(),
        };
        let written = write_generated_objects(ctx, &build, generated, &write)?;
        // A template holds its GameObject alone: the file refers to nothing.
        if let Some(external) = written.externals.first() {
            bail!(
                "a native component template refers to {} {} of another file",
                external.class_name,
                external.local_id
            );
        }
        let mut objects = Vec::with_capacity(written.objects.len());
        let mut listed = Vec::with_capacity(written.objects.len());
        for object in &written.objects {
            let Some(reported) = written
                .report
                .objects
                .iter()
                .find(|reported| reported.index == object.local_id)
            else {
                bail!(
                    "the written template object {} is not in the build's report",
                    object.local_id
                );
            };
            listed.push((object.local_id.to_string(), reported.class_name.clone()));
            objects.push(playerbake::Object {
                local_id: object.local_id,
                tree: &object.tree,
                data: object.data.clone(),
                script: None,
            });
        }
        // Every template's GameObject and components are in the file.
        for template in templates {
            let mut wanted = vec![(template.game_object, "UnityEngine.GameObject".to_string())];
            wanted.extend(
                template
                    .components
                    .iter()
                    .map(|(id, _, _)| (*id, String::new())),
            );
            for (id, type_name) in wanted {
                let found = listed.iter().find(|(local, _)| *local == id.to_string());
                if found.is_none_or(|(_, found)| !type_name.is_empty() && *found != type_name) {
                    bail!("the build of a native component template left out object {id}");
                }
            }
        }
        objects.sort_by_key(|object| object.local_id);
        let file = playerbake::write(&objects, &[])?;
        Event::new("native-templates-built")
            .with("templates", &templates.len())
            .with("objects", &objects.len())
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(built_store::Built {
            file,
            objects: listed,
            externals: Vec::new(),
            external_paths: BTreeMap::new(),
            sources: Vec::new(),
            resources: Vec::new(),
        })
    }
}
