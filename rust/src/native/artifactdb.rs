//! The records the main Editor commits to `Library/ArtifactDB` for the result
//! of an import. A worker resolves every asset another one loads (by path or
//! through a reference) from these tables, so the coordinator has to keep
//! them for the results it accepted.
use crate::{
    lmdb::{self, Record, Tables},
    protocol::{ImportAddress, ImportResponse, blob_array, blob_string, u32_at},
};
use anyhow::{Context, Result, ensure};

const CURRENT_REVISIONS: &str = "CurrentRevisions";
const RESULT_IDS: &str = "ImportAddressToImportResultIDs";
const DEPENDENCIES: &str = "ImportResultIDToArtifactDependencies";
const META_INFO: &str = "ImportResultIDToArtifactMetaInfo";
const IMPORT_STATS: &str = "ImportResultIDToImportStats";
const PROPERTIES: &str = "ImportResultIDPropertyIDToProperty";

/// The key of an import address in `CurrentRevisions` and
/// `ImportAddressToImportResultIDs`: XXH3-128 over the GUID, the importer
/// and the importer hash, low half first.
pub fn address_key(address: &ImportAddress) -> [u8; 16] {
    let mut hashed = [0; 36];
    hashed[..20].copy_from_slice(&address[..20]);
    hashed[20..].copy_from_slice(&address[24..40]);
    xxhash_rust::xxh3::xxh3_128(&hashed).to_le_bytes()
}

/// The properties of an ArtifactMetaInfo: their names and values.
fn properties(meta: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    ensure!(
        meta.len() >= 112,
        "ArtifactMetaInfo fixed part is incomplete"
    );
    let (_, count) = blob_array(meta, 96, 12)?;
    let start = (96 + i64::from(u32_at(meta, 96)? as i32)) as usize;
    (0..count)
        .map(|index| {
            let entry = start + index * 12;
            Ok((
                blob_string(meta, entry)?,
                blob_array(meta, entry + 4, 1)?.0.to_vec(),
            ))
        })
        .collect()
}

fn put(tables: &mut Tables, table: &str, key: Vec<u8>, value: Vec<u8>) {
    let records = tables.entry(table.into()).or_default();
    records.retain(|record| record.key != key);
    records.push(Record { key, value });
}

/// Adds the result of one import as the current revision of its address,
/// the way `ArtifactDBWriteTxn::AddArtifactMetaData` and
/// `SetCurrentRevision` do.
pub fn record(tables: &mut Tables, response: &ImportResponse) -> Result<()> {
    let id = super::environment::decode_hex(&response.result_id)
        .filter(|id| id.len() == 16)
        .context("import result identity is invalid")?;
    let [dependencies, meta, stats] = &response.artifact_blobs[..] else {
        anyhow::bail!("import response lacks its artifact records");
    };
    ensure!(
        dependencies.len() >= 72 && dependencies[40..56] == id[..],
        "ArtifactDependencies does not name the import result"
    );
    let key = address_key(&response.address).to_vec();
    let mut address = response.address[..40].to_vec();
    address[20..24].fill(0);

    // The new result leads the list of those known for the address.
    let mut ids = id.clone();
    if let Some(known) = tables
        .get(RESULT_IDS)
        .and_then(|records| records.iter().find(|record| record.key == key))
    {
        let (others, _) = blob_array(&known.value, 40, 16)?;
        for other in others
            .as_chunks::<16>()
            .0
            .iter()
            .filter(|other| other[..] != id[..])
        {
            ids.extend_from_slice(other);
        }
    }
    let mut list = address.clone();
    list.extend_from_slice(&8u32.to_le_bytes());
    list.extend_from_slice(&((ids.len() / 16) as u32).to_le_bytes());
    list.extend_from_slice(&ids);
    put(tables, RESULT_IDS, key.clone(), list);

    put(tables, DEPENDENCIES, id.clone(), dependencies.clone());
    put(tables, META_INFO, id.clone(), meta.clone());
    put(tables, IMPORT_STATS, id.clone(), stats.clone());
    for (name, value) in properties(meta)? {
        let mut property = name.into_bytes();
        property.extend_from_slice(&id);
        put(tables, PROPERTIES, property, value);
    }

    let mut revision = address;
    revision.extend_from_slice(&id);
    put(tables, CURRENT_REVISIONS, key, revision);
    Ok(())
}

/// Commits the result of one import to the ArtifactDB of the import context
/// while its worker is between two requests.
pub fn publish(project: &str, response: &ImportResponse) -> Result<u64> {
    let path = crate::sysutil::join(&[project, "Library", "ArtifactDB"]);
    lmdb::commit(&path, |tables| record(tables, response))
        .with_context(|| format!("commit import result to {path}"))
}
