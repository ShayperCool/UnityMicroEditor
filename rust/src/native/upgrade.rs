//! Authored sources of an older serialization, upgraded by the engine.
//!
//! The session reads a Mesh of the serialization it knows
//! (`project::MESH_VERSION`). For an older one it asks the importer, which
//! has the engine load and save the source in a worker and gives the
//! session the fields of the text the engine wrote. A Mesh needs no other
//! source to load.
use super::{
    importer::NativeProjectImporter,
    player_objects::{Build, save_sources},
    support::Event,
};
use crate::{
    cancel::Context,
    playerbake::Unsupported,
    project::{MESH_VERSION, UpgradedSource, parse_unity_yaml, value},
    registry::meta_guid,
    sysutil,
    util::{file_hash, random_id},
};
use anyhow::{Result, anyhow};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

impl NativeProjectImporter {
    /// Supplies the session with the fields of a Mesh source of an older
    /// serialization as the engine saves it.
    pub(crate) fn upgrade_mesh(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        let started = Instant::now();
        let refused = |reason: String| {
            anyhow::Error::from(Unsupported(reason)).context(format!("upgrade native {logical}"))
        };
        let sources = self.project_sources()?;
        let Some(physical) = sources.physical(logical) else {
            return Err(anyhow!("{logical} has no file"));
        };
        let meta = std::fs::read(format!("{physical}.meta"))?;
        let documents = parse_unity_yaml(&std::fs::read(&physical)?)?;
        let [mesh] = &documents[..] else {
            return Err(refused(format!(
                "{logical}, which holds more than one object"
            )));
        };
        let version = |fields: &value::Map| value::number(value::get(fields, "serializedVersion"));
        if mesh.class_id != 43 || version(mesh.fields()) >= MESH_VERSION {
            return Err(refused(format!(
                "{logical}, which is no Mesh of an older serialization"
            )));
        }
        let guid = meta_guid(&meta)?;
        if let Some(kept) =
            self.kept_supply::<UpgradedSource>(&sources, "mesh-upgrade", &guid, |kept| {
                &kept.ingredient_sha256
            })
            && kept.source_path == logical
            && kept.file_id == mesh.file_id
        {
            self.session.set_upgraded_source(&kept)?;
            Event::new("native-source-upgrade-kept")
                .with("path", logical)
                .emit();
            return Ok(());
        }
        let saved = save_sources(
            ctx,
            &Build {
                project: self.opts.project.clone(),
                editor: self.opts.editor.clone(),
                output: sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]),
                target: logical.to_string(),
                users: vec![logical.to_string()],
                timeout: Duration::from_secs(600),
                reload_list: String::new(),
                shader_cache: String::new(),
            },
        )
        .map_err(|error| error.context(format!("upgrade native {logical}")))?;
        let text = saved
            .into_iter()
            .find_map(|(path, text)| (path == logical).then_some(text))
            .ok_or_else(|| anyhow!("the worker saved no text of {logical}"))?;
        // The engine saves the objects it loaded under local IDs of its own;
        // the source holds this one alone.
        let saved = parse_unity_yaml(&text)?;
        let [upgraded] = &saved[..] else {
            return Err(anyhow!(
                "the worker's text of {logical} holds {} objects, not its Mesh alone",
                saved.len()
            ));
        };
        if upgraded.class_id != 43 {
            return Err(anyhow!("the worker's text of {logical} holds no Mesh"));
        }
        if version(upgraded.fields()) < MESH_VERSION {
            return Err(refused(format!(
                "{logical}, which the engine leaves at an older serialization"
            )));
        }
        let mut ingredient_sha256 = BTreeMap::new();
        ingredient_sha256.insert(logical.to_string(), file_hash(&physical)?);
        ingredient_sha256.insert(
            format!("{logical}.meta"),
            file_hash(&format!("{physical}.meta"))?,
        );
        let supplied = UpgradedSource {
            guid: guid.clone(),
            source_path: logical.to_string(),
            file_id: mesh.file_id.clone(),
            ingredient_sha256,
            data_json: value::compact_map(upgraded.fields())?,
        };
        self.session.set_upgraded_source(&supplied)?;
        self.keep_supply("mesh-upgrade", &guid, &supplied);
        Event::new("native-source-upgraded")
            .with("path", logical)
            .with("from", &version(mesh.fields()))
            .with("to", &version(upgraded.fields()))
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(())
    }
}
