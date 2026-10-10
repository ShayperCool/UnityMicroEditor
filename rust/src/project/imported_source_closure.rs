//! Authored PPtrs of imported ScriptableObjects that extend the closure.
use super::{
    expand::{builtin_guid, visit_map},
    lexpath,
    native::{valid_native_guid, valid_native_id},
    session::{Inner, asset_key, needs_import},
    types::Document,
    value::{self, Map, get, map_file_id, number},
};
use anyhow::{Result, anyhow, bail};
use std::collections::{HashMap, HashSet};

fn trim_space(bytes: &[u8]) -> &[u8] {
    let text = String::from_utf8_lossy(bytes);
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    if matches!(text, std::borrow::Cow::Borrowed(_)) && start <= end {
        &bytes[start..end]
    } else {
        bytes.trim_ascii()
    }
}

impl Inner {
    /// Imported ScriptableObjects retain their native representation. Their
    /// authored PPtrs still define the dependency closure, including prefab
    /// pointers omitted from native packs and signed same-file subasset identities.
    pub(crate) fn imported_source_references(
        &mut self,
        asset: &Map,
        native_assets: &HashMap<String, Map>,
    ) -> Result<Vec<Map>> {
        let path = value::text(asset, "path").into_owned();
        if value::text(asset, "kind") != "imported"
            || number(get(asset, "classId")) != 114
            || !lexpath::equal_fold(lexpath::ext(&path), ".asset")
        {
            return Ok(Vec::new());
        }
        let (guid, id) = (
            value::text(asset, "guid").into_owned(),
            value::text(asset, "fileId").into_owned(),
        );
        let native = native_assets.get(&asset_key(&guid, &id));
        if !valid_native_guid(&guid)
            || !valid_native_id(&id)
            || self.path_of(&guid) != path
            || native.is_none_or(|native| {
                number(get(native, "classId")) != 114
                    || value::text(native, "path") != path
                    || value::text(native, "typeName") != value::text(asset, "typeName")
            })
        {
            bail!("imported ScriptableObject source identity is invalid: {guid}/{id} ({path})");
        }
        let physical = self.source(&path)?;
        let (content, source_sha) = self.read_text(&physical)?;
        let meta_path = self.source(&format!("{path}.meta"))?;
        let (meta, meta_sha) = self.read_text(&meta_path)?;
        if !self.native_catalog_source_matches(&path, &source_sha)
            || !self.native_catalog_source_matches(&format!("{path}.meta"), &meta_sha)
            || crate::yaml::metadata_guid(&meta).as_deref() != Some(guid.as_str())
        {
            return Err(needs_import(&path));
        }
        let trimmed = trim_space(
            content
                .strip_prefix(&[0xef, 0xbb, 0xbf])
                .unwrap_or(&content),
        );
        if !trimmed.starts_with(b"%YAML") && !trimmed.starts_with(b"--- !u!") {
            // Opaque native binary data has no authored YAML PPtrs to traverse.
            return Ok(Vec::new());
        }
        let documents = self.parse(&physical)?;
        let known: HashSet<&str> = documents.iter().map(|d| d.file_id.as_str()).collect();
        let selected: Option<&Document> = documents.iter().rfind(|d| d.file_id == id);
        let Some(selected) = selected.filter(|d| d.class_id == 114 && !d.stripped) else {
            bail!(
                "imported ScriptableObject source has no exact class114 object: {guid}/{id} ({path})"
            );
        };
        let mut references: Vec<Map> = Vec::new();
        let mut failure: Option<anyhow::Error> = None;
        visit_map(selected.fields(), "", &mut |property, reference| {
            if failure.is_some() {
                return;
            }
            let local_id = map_file_id(reference).into_owned();
            if local_id == "0" || builtin_guid(&value::text(reference, "guid")) {
                return;
            }
            let mut value = reference.clone();
            if value::text(&value, "guid").is_empty() {
                if !known.contains(local_id.as_str()) {
                    failure = Some(anyhow!(
                        "imported ScriptableObject {path}.{property} has unknown same-file localID {local_id}"
                    ));
                    return;
                }
                value.insert("guid".into(), guid.as_str().into());
            }
            let target_guid = value::text(&value, "guid").into_owned();
            let target = native_assets.get(&asset_key(&target_guid, &local_id));
            // Native packs already retain shaders, meshes, textures and other opaque
            // references. Only ScriptableObject callbacks and raw prefab references
            // extend this publication closure.
            if target.is_none_or(|t| number(get(t, "classId")) != 114)
                && !lexpath::equal_fold(lexpath::ext(self.path_of(&target_guid)), ".prefab")
            {
                return;
            }
            if references.len() == 32768 {
                failure = Some(anyhow!(
                    "imported ScriptableObject reference closure exceeds supported bound: {guid}/{id}"
                ));
                return;
            }
            references.push(value);
        });
        match failure {
            Some(error) => Err(error),
            None => Ok(references),
        }
    }
}
