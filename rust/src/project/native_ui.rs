//! Native UI binding proofs and Mesh CPU retention descriptors.
use super::{
    native::{valid_native_guid, valid_native_hex, valid_native_id},
    types::NativeContentFile,
    value,
};
use anyhow::{Result, bail};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::LazyLock,
};

static UI_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:\[[0-9]{1,5}\])?(?:\.[A-Za-z_][A-Za-z0-9_]*(?:\[[0-9]{1,5}\])?)*$").unwrap()
});
static UI_MANAGED_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^references\.RefIds\[([0-9]{1,5})\]\.data\.").unwrap());

pub(crate) fn validate_native_ui_bindings(
    file: &NativeContentFile,
    files: &BTreeMap<String, NativeContentFile>,
) -> Result<()> {
    if !file.ui_bindings.is_empty() && file.ui_binding_proof.is_none() {
        bail!("native UI bindings lack extraction proof");
    }
    if let Some(p) = &file.ui_binding_proof {
        let payload = value::encode_json(&file.ui_bindings)?;
        if p.schema_version != 1
            || p.mode != "native-ui-pptr-bindings-v1"
            || p.native_sha256 != file.sha256
            || !valid_native_hex(&p.reader_sha256, 64)
            || !valid_native_hex(&p.dump_sha256, 64)
            || p.bindings_sha256 != crate::hex(&Sha256::digest(payload.as_bytes()))
        {
            bail!("native UI binding extraction proof is invalid");
        }
    }
    let owners: HashMap<&str, &str> = file
        .objects
        .iter()
        .map(|o| (o.file_id.as_str(), o.type_name.as_str()))
        .collect();
    let dependencies: HashSet<&str> = file
        .dependencies
        .iter()
        .filter(|d| !d.global)
        .map(|d| d.guid.as_str())
        .collect();
    let mut seen = HashSet::new();
    if file.ui_bindings.len() > 4096 {
        bail!("native UI binding count exceeds bounds");
    }
    for binding in file.ui_bindings.iter() {
        let kind = owners
            .get(binding.owner_file_id.as_str())
            .copied()
            .unwrap_or("");
        if kind != "UnityEngine.UIElements.StyleSheet"
            && kind != "UnityEngine.UIElements.VisualTreeAsset"
        {
            bail!("native UI binding owner is not a published UI asset");
        }
        let local = binding.target_guid == file.guid;
        if !UI_PATH.is_match(&binding.property_path)
            || binding.property_path.len() > 1024
            || !valid_native_guid(&binding.target_guid)
            || !valid_native_id(&binding.target_file_id)
            || (!dependencies.contains(binding.target_guid.as_str()) && !local)
        {
            bail!("native UI binding identity/path/dependency is invalid");
        }
        if !seen.insert(format!(
            "{}/{}",
            binding.owner_file_id, binding.property_path
        )) {
            bail!("duplicate native UI binding property");
        }
        if binding.property_path.starts_with("references.") {
            let managed = UI_MANAGED_PATH.captures(&binding.property_path);
            let Some(managed) = managed.filter(|_| {
                valid_native_id(&binding.managed_reference_id)
                    && !binding.managed_reference_id.starts_with('-')
            }) else {
                bail!("native UI managed binding lacks an exact registry identity");
            };
            if managed[1].parse::<u32>().unwrap_or(u32::MAX) > 16383 {
                bail!("native UI registry index exceeds bounds");
            }
        } else if !binding.managed_reference_id.is_empty() {
            bail!("native UI nonmanaged binding has a registry identity");
        }
        let Some(target) = files.get(&binding.target_guid) else {
            bail!("native UI target content file is unavailable");
        };
        let mut found = false;
        for object in target.objects.iter() {
            if object.file_id == binding.target_file_id {
                if local && object.type_name != "UnityEngine.UIElements.StyleSheet" {
                    bail!("native UI local target must identify a stylesheet");
                }
                found = true;
            }
        }
        if !found {
            bail!("native UI target local ID is not published");
        }
    }
    Ok(())
}

pub(crate) fn validate_native_mesh_cpu_retention(file: &NativeContentFile) -> Result<()> {
    let types: HashMap<&str, &str> = file
        .objects
        .iter()
        .map(|o| (o.file_id.as_str(), o.type_name.as_str()))
        .collect();
    let mut seen = HashSet::new();
    for mesh in file.mesh_cpu_retention.iter() {
        if !valid_native_id(&mesh.file_id)
            || seen.contains(&mesh.file_id)
            || types.get(mesh.file_id.as_str()).copied() != Some("UnityEngine.Mesh")
            || mesh.original_is_readable
        {
            bail!(
                "native Mesh CPU retention must name unique originally unreadable Mesh objects: {}",
                mesh.file_id
            );
        }
        seen.insert(&mesh.file_id);
    }
    Ok(())
}
