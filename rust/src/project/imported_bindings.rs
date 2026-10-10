//! Deferred imported bindings selected for active owners only.
use super::{
    lexpath,
    native::{valid_native_guid, valid_native_id},
    session::{Inner, asset_key},
    value::{self, Map, Value, array, get, number, object, str},
    worklist::AssetReferenceWorklist,
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap};

pub(crate) struct AssetPreparation {
    pub assets: Vec<Value>,
    pub imported_bindings: Vec<Value>,
}

pub(crate) struct ImportedBindingClosure {
    by_owner: HashMap<String, Vec<Map>>,
    selected: BTreeMap<String, Map>,
}

/// `^[A-Za-z_][A-Za-z0-9_]*(?:(?:\.[A-Za-z_][A-Za-z0-9_]*)|(?:\[[0-9]+\]))*$`
fn binding_property(property: &str) -> bool {
    let bytes = property.as_bytes();
    let name = |i: &mut usize| {
        if !bytes
            .get(*i)
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        {
            return false;
        }
        while bytes
            .get(*i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            *i += 1;
        }
        true
    };
    let mut i = 0;
    if !name(&mut i) {
        return false;
    }
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                if !name(&mut i) {
                    return false;
                }
            }
            b'[' => {
                let digits = bytes[i + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if digits == 0 || bytes.get(i + 1 + digits) != Some(&b']') {
                    return false;
                }
                i += digits + 2;
            }
            _ => return false,
        }
    }
    true
}

impl Inner {
    pub(crate) fn imported_binding_closure(&self) -> ImportedBindingClosure {
        let mut bindings: Vec<Map> = self.catalog.deferred_bindings.clone();
        if self.catalog.schema_version == 0 && bindings.is_empty() {
            bindings.extend(
                array(get(&self.baseline, "importedAssetBindings"))
                    .iter()
                    .map(|v| object(v).clone()),
            );
        }
        let mut by_owner: HashMap<String, Vec<Map>> = HashMap::new();
        for binding in bindings {
            by_owner
                .entry(asset_key(
                    &value::text(&binding, "ownerGuid"),
                    &value::text(&binding, "ownerFileId"),
                ))
                .or_default()
                .push(binding);
        }
        ImportedBindingClosure {
            by_owner,
            selected: BTreeMap::new(),
        }
    }
}

impl ImportedBindingClosure {
    pub fn queue(
        &mut self,
        session: &Inner,
        owner: &Map,
        pending: &mut AssetReferenceWorklist,
    ) -> Result<()> {
        if value::text(owner, "kind") != "imported" {
            return Ok(());
        }
        let (owner_guid, owner_id) = (value::text(owner, "guid"), value::text(owner, "fileId"));
        let owner_key = asset_key(&owner_guid, &owner_id);
        let Some(bindings) = self.by_owner.get(&owner_key) else {
            return Ok(());
        };
        for binding in bindings {
            let property = value::text(binding, "propertyPath");
            if !valid_native_guid(&owner_guid)
                || !valid_native_id(&owner_id)
                || !binding_property(&property)
                || number(get(owner, "classId")) != 114
            {
                bail!(
                    "imported binding owner/property is invalid: {owner_guid}/{owner_id}.{property}"
                );
            }
            let (target_guid, target_id, target_path) = (
                value::text(binding, "targetGuid"),
                value::text(binding, "targetFileId"),
                value::text(binding, "targetPath"),
            );
            if target_id != "0" {
                let known = session.path_of(&target_guid);
                if !valid_native_guid(&target_guid)
                    || !valid_native_id(&target_id)
                    || known.is_empty()
                    || target_path != known
                    || lexpath::clean(&target_path) != target_path
                    || !lexpath::equal_fold(lexpath::ext(&target_path), ".prefab")
                {
                    bail!(
                        "imported binding target identity/path is invalid: {owner_guid}/{owner_id}.{property} -> {target_guid}/{target_id} ({target_path})"
                    );
                }
            }
            let key = format!("{owner_key}\0{property}");
            if let Some(existing) = self.selected.get(&key) {
                if value::text(existing, "targetGuid") != target_guid
                    || value::text(existing, "targetFileId") != target_id
                    || value::text(existing, "targetPath") != target_path
                {
                    bail!(
                        "imported binding property has conflicting target identities: {owner_guid}/{owner_id}.{property}"
                    );
                }
                continue;
            }
            self.selected.insert(key, binding.clone());
            if target_id != "0" {
                pending.add_identity(&target_guid, &target_id)?;
            }
            if self.selected.len() > 32768 {
                bail!("imported binding dependency closure exceeds supported bound");
            }
        }
        Ok(())
    }

    pub fn result(&self, assets: &BTreeMap<String, Map>) -> Result<Vec<Value>> {
        for binding in self.selected.values() {
            let (guid, id) = (
                value::text(binding, "targetGuid"),
                value::text(binding, "targetFileId"),
            );
            if id != "0" && !prepared_asset_identity(assets, &guid, &id) {
                bail!(
                    "imported binding target is absent from proven asset/prefab identities: {guid}/{id}"
                );
            }
        }
        Ok(self.selected.values().cloned().map(Value::Object).collect())
    }
}

pub(crate) fn prepared_asset_identity(
    assets: &BTreeMap<String, Map>,
    guid: &str,
    id: &str,
) -> bool {
    if let Some(asset) = assets.get(&asset_key(guid, id))
        && value::text(asset, "guid") == guid
        && value::text(asset, "fileId") == id
    {
        return true;
    }
    assets.values().any(|asset| {
        value::text(asset, "guid") == guid
            && value::text(asset, "kind") == "prefab"
            && array(get(asset, "objectAliases"))
                .iter()
                .chain(array(get(asset, "objects")))
                .any(|item| str(item.field("fileId")) == id)
    })
}
