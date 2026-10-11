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

/// A pointer of a catalog object to an object loaded from its authored text.
pub(crate) struct AuthoredPointer {
    pub owner: String,
    pub property: String,
    pub target: String,
    pub target_id: String,
    pub target_path: String,
}

impl Inner {
    /// Whether a ScriptableObject source the catalog holds has been edited
    /// since and is therefore loaded from its authored text, like one the
    /// catalog never held. Sources the engine itself loads stay native.
    pub(crate) fn authored_scriptable(&mut self, path: &str) -> Result<bool> {
        if let Some(known) = self.authored_scriptables.get(path) {
            return Ok(*known);
        }
        let mut held = self
            .catalog
            .assets
            .iter()
            .filter(|asset| value::text(asset, "path") == path)
            .map(|asset| number(get(asset, "classId")) == 114)
            .peekable();
        let mut authored = lexpath::equal_fold(lexpath::ext(path), ".asset")
            && self.critical.contains_key(path)
            && held.peek().is_some()
            && held.all(|scriptable| scriptable);
        if authored {
            let guid = self.guid_of(path).to_string();
            let engine_loaded = ["preloadedAssets", "renderPipelineGlobalSettings"]
                .iter()
                .flat_map(|collection| value::array(get(&self.baseline, collection)))
                .any(|item| value::str(item.field("guid")) == guid)
                || self.baseline_text("renderPipelineGuid") == guid;
            authored = !guid.is_empty() && !engine_loaded;
        }
        if authored {
            let physical = self.source(path)?;
            let (_, source_sha) = self.read_text(&physical)?;
            let meta = self.source(&format!("{path}.meta"))?;
            let (_, meta_sha) = self.read_text(&meta)?;
            let meta_path = format!("{path}.meta");
            let stale = |known: &str, current: &str| !known.is_empty() && known != current;
            authored = stale(self.critical_of(path), &source_sha)
                || stale(self.critical_of(&meta_path), &meta_sha)
                || !self.native_catalog_source_matches(path, &source_sha)
                || !self.native_catalog_source_matches(&meta_path, &meta_sha);
        }
        if authored {
            let physical = self.source(path)?;
            authored = self
                .parse(&physical)
                .is_ok_and(|documents| documents.iter().all(|d| d.class_id == 114 && !d.stripped));
        }
        self.authored_scriptables.insert(path.to_string(), authored);
        Ok(authored)
    }

    /// The pointers to bind when the scene loads: those of a catalog
    /// ScriptableObject, or of the state machine behaviours of a catalog
    /// controller, to objects now loaded from their authored text, and those a
    /// controller baked in this session leaves null. Each is an owner file
    /// ID, a property path and a target.
    pub(crate) fn authored_scriptable_references(
        &mut self,
        asset: &Map,
    ) -> Result<Vec<AuthoredPointer>> {
        let path = value::text(asset, "path").into_owned();
        let class = number(get(asset, "classId"));
        let controller = class == 91 && lexpath::equal_fold(lexpath::ext(&path), ".controller");
        let scriptable = class == 114 && lexpath::equal_fold(lexpath::ext(&path), ".asset");
        if value::text(asset, "kind") != "imported" || !(controller || scriptable) {
            return Ok(Vec::new());
        }
        let (guid, id) = (
            value::text(asset, "guid").into_owned(),
            value::text(asset, "fileId").into_owned(),
        );
        // Content baked in this session names the pointers its file leaves
        // null. They are taken once, with the controller itself.
        if let Some(overlay) = self.native_overlays.get(&guid) {
            let deferred = match class {
                91 => overlay.deferred_bindings.to_vec(),
                _ => Vec::new(),
            };
            let mut result = Vec::new();
            for binding in deferred {
                let target_path = self.path_of(&binding.target_guid).to_string();
                if target_path.is_empty() {
                    bail!(
                        "{path} refers to asset {}, which is not a project source",
                        binding.target_guid
                    );
                }
                result.push(AuthoredPointer {
                    owner: binding.owner_file_id,
                    property: binding.property_path,
                    target: binding.target_guid,
                    target_id: binding.target_file_id,
                    target_path,
                });
            }
            return Ok(result);
        }
        let physical = self.source(&path)?;
        let (content, _) = self.read_text(&physical)?;
        if !trim_space(&content).starts_with(b"%YAML") {
            return Ok(Vec::new());
        }
        let documents = self.parse(&physical)?;
        let mut pointers = Vec::new();
        for owner in documents.iter() {
            if owner.class_id != 114 || !(controller || owner.file_id == id) {
                continue;
            }
            visit_map(owner.fields(), "", &mut |property, reference| {
                let target = value::text(reference, "guid").into_owned();
                let local = map_file_id(reference).into_owned();
                if local != "0" && !target.is_empty() && target != guid {
                    pointers.push((owner.file_id.clone(), property.to_string(), target, local));
                }
            });
        }
        let mut result = Vec::new();
        for (owner, property, target, local) in pointers {
            let target_path = self.path_of(&target).to_string();
            if target_path.is_empty() || !self.authored_scriptable(&target_path)? {
                continue;
            }
            // A behaviour the catalog does not publish cannot be reached; its
            // controller has to be baked with the pointer left to the Player.
            if !self.catalog.assets.iter().any(|known| {
                value::text(known, "guid") == guid && value::text(known, "fileId") == owner
            }) {
                return Err(needs_import(&path));
            }
            result.push(AuthoredPointer {
                owner,
                property,
                target,
                target_id: local,
                target_path,
            });
        }
        Ok(result)
    }

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
