//! Which entries of a render pipeline's global settings a Player build
//! keeps, decided by the pipeline's build code in a worker. See
//! `project::graphics_settings`.
use super::{importer::NativeProjectImporter, player_objects::player_settings, support::Event};
use crate::{
    cancel::Context, playerbake::Unsupported, project::PlayerGraphicsSettings, registry::meta_guid,
};
use anyhow::{Result, anyhow};
use std::{collections::BTreeSet, time::Instant};

/// Whether a source holds a render pipeline's graphics settings container.
pub(super) fn holds_graphics_settings(text: &[u8]) -> bool {
    let has = |needle: &[u8]| text.windows(needle.len()).any(|window| window == needle);
    has(b"\n    m_SettingsList:") && has(b"\n    m_RuntimeSettings:")
}

impl NativeProjectImporter {
    /// Supplies the session with the entries of a global settings asset a
    /// Player build keeps.
    pub(crate) fn import_player_graphics_settings(
        &mut self,
        ctx: &Context,
        logical: &str,
    ) -> Result<()> {
        let started = Instant::now();
        let sources = self.project_sources()?;
        let Some(physical) = sources.physical(logical) else {
            return Err(anyhow!("{logical} has no file"));
        };
        let guid = meta_guid(&std::fs::read(format!("{physical}.meta"))?)?;
        if let Some(kept) = self.kept_supply::<PlayerGraphicsSettings>(
            &sources,
            "graphics-settings",
            &guid,
            |kept| &kept.ingredient_sha256,
        ) && kept.source_path == logical
        {
            self.session.set_player_graphics_settings(&kept)?;
            Event::new("native-graphics-settings-kept")
                .with("path", logical)
                .emit();
            return Ok(());
        }
        // The batch of a build of one of the pipeline's shaders holds the
        // pipeline's assets, its global settings and its scripts.
        let Some((target, _)) = self
            .pipeline_shader_sources()?
            .into_iter()
            .find(|(path, _)| path.ends_with(".shader"))
        else {
            return Err(anyhow::Error::from(Unsupported(format!(
                "the global settings {logical} of a project whose pipeline names no shader source"
            ))));
        };
        let build = self.build_of(&target, Vec::new());
        let (entries, ingredients) = player_settings(ctx, &build, &[logical.to_string()])
            .map_err(|error| error.context(format!("strip native {logical}")))?;
        let (kept, stripped): (BTreeSet<i64>, BTreeSet<i64>) = (
            entries.iter().filter(|e| e.kept).map(|e| e.rid).collect(),
            entries.iter().filter(|e| !e.kept).map(|e| e.rid).collect(),
        );
        // The session reads authored files only.
        let mut ingredient_sha256: std::collections::BTreeMap<String, String> = ingredients
            .into_iter()
            .filter(|(path, _)| path.starts_with("Assets/"))
            .collect();
        ingredient_sha256
            .entry(logical.to_string())
            .or_insert(crate::util::file_hash(&physical)?);
        let supplied = PlayerGraphicsSettings {
            guid: guid.clone(),
            source_path: logical.to_string(),
            ingredient_sha256,
            kept,
            stripped,
        };
        self.session.set_player_graphics_settings(&supplied)?;
        self.keep_supply("graphics-settings", &guid, &supplied);
        let stripped: Vec<&str> = entries
            .iter()
            .filter(|e| !e.kept)
            .map(|e| e.type_name.as_str())
            .collect();
        Event::new("native-graphics-settings-stripped")
            .with("path", logical)
            .with("kept", &entries.iter().filter(|e| e.kept).count())
            .with("stripped", &stripped.join(" "))
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(())
    }
}
