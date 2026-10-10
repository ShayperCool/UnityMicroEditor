//! The shared asset reference worklist with its unique-identity bound.
use super::{
    expand::builtin_guid,
    session::asset_key,
    value::{self, Map, map_file_id},
};
use anyhow::{Result, bail};
use std::collections::HashSet;

pub(crate) const ASSET_REFERENCE_CLOSURE_MAXIMUM: usize = 32768;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AssetReference {
    pub guid: String,
    pub file_id: String,
}

/// The bound counts unique identities over the whole traversal, including
/// processed entries, rather than duplicate PPtrs.
#[derive(Default)]
pub(crate) struct AssetReferenceWorklist {
    pub items: Vec<AssetReference>,
    pub seen: HashSet<String>,
}
impl AssetReferenceWorklist {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add_identity(&mut self, guid: &str, id: &str) -> Result<()> {
        if guid.is_empty() || builtin_guid(guid) || id.is_empty() || id == "0" {
            return Ok(());
        }
        let key = asset_key(guid, id);
        if self.seen.contains(&key) {
            return Ok(());
        }
        if self.seen.len() >= ASSET_REFERENCE_CLOSURE_MAXIMUM {
            bail!(
                "asset dependency closure exceeds supported unique-reference bound ({ASSET_REFERENCE_CLOSURE_MAXIMUM})"
            );
        }
        self.seen.insert(key);
        self.items.push(AssetReference {
            guid: guid.into(),
            file_id: id.into(),
        });
        Ok(())
    }
    pub fn add(&mut self, reference: &Map) -> Result<()> {
        self.add_identity(&value::text(reference, "guid"), &map_file_id(reference))
    }
    pub fn add_all(&mut self, references: &[Map]) -> Result<()> {
        references.iter().try_for_each(|r| self.add(r))
    }
    pub fn pop(&mut self) -> Option<AssetReference> {
        self.items.pop()
    }
}
