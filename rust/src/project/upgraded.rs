//! Authored sources of an older serialization, as the engine saves them.
//!
//! The session reads a source of the serialization it knows; the engine
//! upgrades an older one when it loads it. The native importer has the
//! engine load and save such a source in a worker and supplies the text
//! (`native::upgrade_source`); the session takes those fields in place of
//! the authored ones while every file they were made from is unchanged, and
//! asks the importer again otherwise.
use super::{
    native::valid_native_guid,
    session::{Inner, Session, needs_import},
    value::{self, Map, Value},
};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// The fields of one object of a source, as the engine saved them.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpgradedSource {
    pub guid: String,
    pub source_path: String,
    /// The local ID of the object.
    pub file_id: String,
    /// The hash of every file the fields were made from, by logical path:
    /// the source and its `.meta`.
    pub ingredient_sha256: BTreeMap<String, String>,
    /// The fields, as JSON.
    pub data_json: String,
}

impl Session {
    /// Takes the fields of an object of a source as the engine saved them.
    pub fn set_upgraded_source(&self, source: &UpgradedSource) -> Result<()> {
        if !valid_native_guid(&source.guid)
            || !source.ingredient_sha256.contains_key(&source.source_path)
        {
            bail!("upgraded source descriptor is invalid");
        }
        let Value::Object(fields) = value::decode_use_number(&source.data_json)? else {
            bail!("upgraded source fields are not an object");
        };
        self.lock()
            .upgraded_sources
            .insert(source.guid.clone(), (source.clone(), fields));
        Ok(())
    }
}

impl Inner {
    /// The fields the engine saved of the object `file_id` of `path`, while
    /// the files they were made from are unchanged; otherwise the importer
    /// is asked for them.
    pub(crate) fn upgraded_source_fields(&mut self, path: &str, file_id: &str) -> Result<Map> {
        let guid = self.guid_of(path).to_string();
        let Some((known, fields)) = self.upgraded_sources.get(&guid).cloned() else {
            return Err(needs_import(path));
        };
        if known.source_path != path || known.file_id != file_id {
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
        Ok(fields)
    }
}
