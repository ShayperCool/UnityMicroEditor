//! Proofs that a saved prefab override targets an object removed from a
//! complete, current native model inventory.
use super::{
    lexpath,
    native::{valid_native_guid, valid_native_hex, valid_native_id},
    session::Inner,
    types::{Document, PrefabOverrideDiagnostic},
    value::{self, array, file_id},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

fn native_model_resource_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "UnityEngine.Mesh"
            | "UnityEngine.Material"
            | "UnityEngine.AnimationClip"
            | "UnityEngine.Avatar"
            | "UnityEngine.Texture2D"
    )
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Inventory<'a> {
    objects: &'a [Document],
    identities: &'a BTreeMap<String, String>,
}

impl Inner {
    pub(crate) fn proven_missing_native_model_target(
        &self,
        guid: &str,
        id: &str,
    ) -> Option<PrefabOverrideDiagnostic> {
        if !valid_native_guid(guid) || !valid_native_id(id) {
            return None;
        }
        let proof = |backend: &str, path: &str, source: &str, meta: &str, inventory: &str| {
            PrefabOverrideDiagnostic {
                reason: "target_absent_from_verified_native_model".into(),
                proof_backend: backend.into(),
                proof_source_path: path.into(),
                proof_source_sha256: source.into(),
                proof_meta_sha256: meta.into(),
                proof_inventory_sha256: inventory.into(),
                ..PrefabOverrideDiagnostic::default()
            }
        };
        if let Some(native) = self.native_overlays.get(guid)
            && !native.model_graph.is_empty()
        {
            if native.content.objects.iter().any(|o| o.file_id == id) {
                return None;
            }
            return Some(proof(
                "verified_native_replacement",
                &native.source_path,
                &native.source_sha256,
                &native.meta_sha256,
                &native.model_graph_sha256,
            ));
        }
        let catalog = &self.catalog;
        if !catalog.success
            || !catalog.cooked_registry_verified
            || catalog.contains_scenes
            || catalog.scene_loader != "raw"
            || (catalog.backend != "unity_editor_public_api" && catalog.backend != "unity_editor")
        {
            return None;
        }
        for model in &catalog.model_prefabs {
            if model.guid != guid
                || model.path != self.path_of(guid)
                || model.objects.is_empty()
                || model.root_file_id.is_empty()
                || model.root_file_id == "0"
                || matches!(lexpath::ext(&model.path), ".prefab" | ".unity")
            {
                continue;
            }
            let meta_path = format!("{}.meta", model.path);
            let sha = |path: &str| {
                self.dependencies
                    .get(path)
                    .map_or("", |d| d.sha256.as_str())
            };
            let (source, meta) = (sha(&model.path), sha(&meta_path));
            if !valid_native_hex(source, 64)
                || !valid_native_hex(meta, 64)
                || self.critical_of(&model.path) != source
                || self.critical_of(&meta_path) != meta
                || !self.native_catalog_source_matches(&model.path, source)
                || !self.native_catalog_source_matches(&meta_path, meta)
            {
                return None;
            }
            let mut identities: BTreeMap<String, String> = BTreeMap::new();
            for asset in &catalog.assets {
                if value::text(asset, "guid") != guid {
                    continue;
                }
                let (asset_id, type_name) =
                    (value::text(asset, "fileId"), value::text(asset, "typeName"));
                if asset_id == id
                    || value::text(asset, "path") != model.path
                    || !valid_native_id(&asset_id)
                    || type_name.is_empty()
                    || identities
                        .get(asset_id.as_ref())
                        .is_some_and(|previous| !previous.is_empty() && *previous != type_name)
                {
                    return None;
                }
                identities.insert(asset_id.into_owned(), type_name.into_owned());
            }
            // Excluded components/clips still exist in the source model, even when
            // absent from the runtime graph. Their overrides are not stale.
            let named = |asset: &value::Map| {
                value::text(asset, "guid") == guid && value::text(asset, "fileId") == id
            };
            if catalog
                .excluded_runtime_assets
                .iter()
                .chain(&catalog.model_animation_clips)
                .chain(self.assets.values())
                .any(named)
            {
                return None;
            }
            let (mut root, mut seen) = (false, HashSet::new());
            for doc in &model.objects {
                if doc.file_id == id
                    || seen.contains(doc.file_id.as_str())
                    || identities.get(&doc.file_id).map_or("", String::as_str) != doc.type_name
                {
                    return None;
                }
                seen.insert(doc.file_id.as_str());
                if doc.file_id == model.root_file_id && doc.class_id == 1 {
                    root = true;
                }
            }
            if !root || identities.is_empty() {
                return None;
            }
            // The exported object graph must cover the complete live GameObject and
            // component inventory; an incomplete graph proves nothing.
            for (asset_id, type_name) in &identities {
                if !seen.contains(asset_id.as_str())
                    && !native_model_resource_type(type_name)
                    && !self.excluded_native_model_object(guid, asset_id)
                {
                    return None;
                }
            }
            for doc in &model.objects {
                if doc.class_id == 1 {
                    for component in array(doc.get("m_Component")) {
                        if !seen.contains(file_id(component.field("component")).as_ref()) {
                            return None;
                        }
                    }
                } else {
                    let owner = file_id(doc.get("m_GameObject"));
                    if owner != "0" && !seen.contains(owner.as_ref()) {
                        return None;
                    }
                }
            }
            let payload = value::encode_json(&Inventory {
                objects: &model.objects,
                identities: &identities,
            })
            .unwrap_or_default();
            return Some(proof(
                &catalog.backend,
                &model.path,
                source,
                meta,
                &crate::hex(&Sha256::digest(payload.as_bytes())),
            ));
        }
        None
    }

    pub(crate) fn native_catalog_source_matches(&self, path: &str, expected: &str) -> bool {
        let mut found = false;
        for dependency in &self.catalog.source_dependencies {
            if dependency.path != path {
                continue;
            }
            if dependency.sha256 != expected {
                return false;
            }
            found = true;
        }
        found
    }

    fn excluded_native_model_object(&self, guid: &str, id: &str) -> bool {
        self.catalog
            .excluded_runtime_assets
            .iter()
            .any(|item| value::text(item, "guid") == guid && value::text(item, "fileId") == id)
    }

    pub(crate) fn record_ignored_prefab_override(
        &mut self,
        proof: PrefabOverrideDiagnostic,
        path: &str,
        instance: &str,
        guid: &str,
        file_id: &str,
        property: &str,
    ) {
        let value = PrefabOverrideDiagnostic {
            path: path.into(),
            instance_file_id: instance.into(),
            target_guid: guid.into(),
            target_file_id: file_id.into(),
            property_path: property.into(),
            ..proof
        };
        if !self.ignored_diagnostics.contains(&value) {
            self.ignored_diagnostics.push(value);
        }
    }

    pub(crate) fn sorted_ignored_diagnostics(&self) -> Vec<PrefabOverrideDiagnostic> {
        let mut result = self.ignored_diagnostics.clone();
        result.sort_by_cached_key(|d| {
            [
                d.path.as_str(),
                &d.instance_file_id,
                &d.target_guid,
                &d.target_file_id,
                &d.property_path,
            ]
            .join("\0")
        });
        result
    }
}
