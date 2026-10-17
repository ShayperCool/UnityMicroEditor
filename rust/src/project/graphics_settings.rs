//! The graphics settings a Player build keeps of a render pipeline's global
//! settings.
//!
//! A global settings asset holds its entries in a container with two lists:
//! the Editor's (`m_SettingsList`) and the Player's (`m_RuntimeSettings`).
//! A build fills the Player's list from the Editor's when it serializes the
//! asset for the Player, leaving out the entries that are not available in a
//! Player build or that the pipeline's strippers remove; the Player has no
//! Editor list. The text keeps whatever the last save wrote there. The native
//! importer has the pipeline's build code decide in a worker
//! (`native::player_settings`); the session publishes the asset as a build
//! serializes it while every file the decision was made from is unchanged,
//! and asks the importer again otherwise.
use super::{
    native::valid_native_guid,
    session::{Inner, Session, needs_import},
    value::{Map, Value, array, number},
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// Which entries of a global settings asset a Player build keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlayerGraphicsSettings {
    pub guid: String,
    pub source_path: String,
    /// The hash of every authored file the decision was made from, by
    /// logical path: the asset, the pipeline's assets and settings.
    pub ingredient_sha256: BTreeMap<String, String>,
    /// The managed reference IDs of the entries kept and stripped.
    pub kept: BTreeSet<i64>,
    pub stripped: BTreeSet<i64>,
}

impl Session {
    /// Takes which entries of a global settings asset a build keeps.
    pub fn set_player_graphics_settings(&self, settings: &PlayerGraphicsSettings) -> Result<()> {
        if !valid_native_guid(&settings.guid)
            || !settings
                .ingredient_sha256
                .contains_key(&settings.source_path)
            || !settings.kept.is_disjoint(&settings.stripped)
        {
            bail!("player graphics settings descriptor is invalid");
        }
        self.lock()
            .player_graphics_settings
            .insert(settings.guid.clone(), settings.clone());
        Ok(())
    }
}

/// The managed reference ID of a list item (`{rid: n}`).
fn rid(item: &Value) -> Option<i64> {
    item.as_object()
        .filter(|item| item.len() == 1)
        .map(|item| number(item.get("rid").unwrap_or(&Value::Null)))
}

impl Inner {
    /// Rewrites the fields of a global settings asset as a build serializes
    /// them for the Player: the Player's list holds the Editor's entries a
    /// build keeps, in their order, the Editor's list is gone, and so are
    /// the stripped entries of the reference registry with what they name.
    /// Fields that hold no settings container are left alone.
    pub(crate) fn player_graphics_settings(&mut self, path: &str, data: &mut Map) -> Result<()> {
        let Some(container) = data.get("m_Settings").and_then(Value::as_object) else {
            return Ok(());
        };
        let (Some(editor), Some(_)) = (
            container.get("m_SettingsList").and_then(Value::as_object),
            container
                .get("m_RuntimeSettings")
                .and_then(Value::as_object),
        ) else {
            return Ok(());
        };
        let entries = array(editor.get("m_List").unwrap_or(&Value::Null)).to_vec();
        let guid = self.guid_of(path).to_string();
        let Some(known) = self.player_graphics_settings.get(&guid).cloned() else {
            return Err(needs_import(path));
        };
        if known.source_path != path {
            return Err(needs_import(path));
        }
        for (ingredient, expected) in &known.ingredient_sha256 {
            let current = self
                .source(ingredient)
                .and_then(|physical| self.read(&physical, false))
                .map(|(_, hash)| hash);
            if current.ok().as_ref() != Some(expected) {
                return Err(needs_import(path));
            }
        }
        let mut kept = Vec::new();
        let mut decided = BTreeSet::new();
        for entry in &entries {
            let Some(id) = rid(entry) else {
                bail!("global settings {path} list an entry without a managed reference ID");
            };
            if known.kept.contains(&id) {
                kept.push(entry.clone());
            } else if !known.stripped.contains(&id) {
                // An entry the decision did not see: the text changed.
                return Err(needs_import(path));
            }
            decided.insert(id);
        }
        if decided.len() != known.kept.len() + known.stripped.len() {
            return Err(needs_import(path));
        }
        let Some(Value::Object(container)) = data.get_mut("m_Settings") else {
            unreachable!("checked above");
        };
        container.remove("m_SettingsList");
        if let Some(Value::Object(runtime)) = container.get_mut("m_RuntimeSettings") {
            runtime.insert("m_List".into(), Value::Array(kept));
        }
        if let Some(Value::Object(references)) = data.get_mut("references")
            && let Some(Value::Array(registry)) = references.get_mut("RefIds")
        {
            registry.retain(|entry| {
                let id = number(entry.field("rid"));
                !known.stripped.contains(&id)
            });
        }
        Ok(())
    }
}
