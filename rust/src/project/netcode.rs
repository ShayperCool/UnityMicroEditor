//! Derived NetworkObject identity for source prefabs not yet validated by Editor.
use super::{
    expand::{Expanded, target_key},
    lexpath, strconv,
    value::{Map, Value, get, number},
};
use anyhow::{Result, bail};

pub(crate) fn xx_hash32(input: &[u8]) -> u32 {
    xxhash_rust::xxh32::xxh32(input, 0)
}

/// NetworkObject.OnValidate stores XXHash32(GlobalObjectId.ToString()). Source
/// prefabs can still contain zero before Editor validation; the published
/// descriptor carries the derived value while authored YAML stays untouched.
/// `None` leaves `source` as the published data.
pub(crate) fn derived_network_data(
    record: &Expanded,
    type_name: &str,
    source: &Map,
) -> Result<Option<Map>> {
    if type_name != "Unity.Netcode.NetworkObject" || number(get(source, "GlobalObjectIdHash")) != 0
    {
        return Ok(None);
    }
    if lexpath::ext(&record.source_path) != ".prefab" {
        bail!(
            "NetworkObject has no validated hash and is not an authored source prefab: {}/{}",
            record.source_path,
            record.file_id
        );
    }
    let local = record.file_id.rsplit(':').next().unwrap_or(&record.file_id);
    if !record.targets.contains(&target_key(&record.guid, local)) {
        bail!(
            "NetworkObject lacks an exact source prefab identity: {}/{}",
            record.source_path,
            record.file_id
        );
    }
    let Ok(identifier) = strconv::parse_int(local, 10) else {
        bail!("NetworkObject source file ID is not int64: {local}");
    };
    let global = format!(
        "GlobalObjectId_V1-3-{}-{}-0",
        record.guid, identifier as u64
    );
    let hash = xx_hash32(global.as_bytes());
    if hash == 0 {
        bail!("NetworkObject derived hash is zero: {global}");
    }
    let mut result = source.clone();
    result.insert("GlobalObjectIdHash".into(), Value::Int(hash as i64));
    Ok(Some(result))
}
