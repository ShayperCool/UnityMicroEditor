//! Explicit external catalogs are read-only inputs.
//! Their JSON bytes are preserved, including signed 64-bit identities, while a
//! target-owned snapshot is frozen. Catalog semantics and all referenced
//! content still require downstream checks.
use super::{defaults::immutable_file, fail, join, not_exist, nz, read_json};
use crate::{
    cancel::Context,
    jsondoc::{self, Json},
    project::is_owned_cache_path,
    sha256, sysutil,
    util::{agent_canonical, agent_immutable_json},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::Read;

const SOURCE_CATALOG_MAXIMUM: i64 = 32 << 20;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct SourceCatalogReceipt {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    schema_version: i64,
    #[serde(deserialize_with = "nz")]
    project: String,
    #[serde(rename = "sourcePath", deserialize_with = "nz")]
    source_path: String,
    #[serde(rename = "sourceSHA256", deserialize_with = "nz")]
    source_sha256: String,
    #[serde(rename = "snapshotPath", deserialize_with = "nz")]
    snapshot_path: String,
    #[serde(rename = "snapshotSHA256", deserialize_with = "nz")]
    snapshot_sha256: String,
}

/// The catalog path a preparation may record. An
/// external catalog is copied once into `directory`; an owned one is used as is.
pub fn freeze_source_catalog(
    ctx: &Context,
    project: &str,
    directory: &str,
    input_path: &str,
) -> Result<String> {
    ctx.err()?;
    if input_path.is_empty() {
        return Ok(String::new());
    }
    if !sysutil::is_abs(input_path) || agent_canonical(input_path).is_err() {
        return Err(anyhow!(
            "source catalog input must be a canonical absolute regular file"
        ));
    }
    let info = super::lstat(input_path)?;
    if !info.is_regular() || info.size > SOURCE_CATALOG_MAXIMUM {
        return Err(anyhow!(
            "source catalog input must be a bounded regular file"
        ));
    }
    if is_owned_cache_path(project, input_path) {
        return Ok(input_path.into());
    }
    let snapshot_path = join(&[directory, "native-input-catalog.json"]);
    let receipt_path = join(&[directory, "native-input-receipt.json"]);
    for path in [directory, snapshot_path.as_str(), receipt_path.as_str()] {
        if !sysutil::is_abs(path)
            || !is_owned_cache_path(project, path)
            || agent_canonical(path).is_err()
        {
            return Err(anyhow!(
                "source catalog snapshot output must be canonical and target-owned"
            ));
        }
    }
    let content = read_source_catalog(ctx, input_path)?;
    // One valid UTF-8 JSON value that is an object.
    let object = std::str::from_utf8(&content).is_ok()
        && matches!(jsondoc::parse(&content), Ok(Json::Object(_)));
    if !object {
        return Err(anyhow!(
            "source catalog input must contain one valid UTF-8 JSON object"
        ));
    }
    let source_hash = sha256(&content);
    let receipt = SourceCatalogReceipt {
        schema_version: 1,
        project: project.into(),
        source_path: input_path.into(),
        source_sha256: source_hash.clone(),
        snapshot_path: snapshot_path.clone(),
        snapshot_sha256: source_hash.clone(),
    };
    match read_json::<SourceCatalogReceipt>(&receipt_path, 1 << 20) {
        Ok(previous) => {
            if previous != receipt {
                return Err(fail(
                    "request_id_conflict",
                    "external source catalog identity or content changed after its frozen snapshot",
                ));
            }
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }
    // An interrupted first publication may have a snapshot without a receipt.
    // It must still match these exact source bytes before retrying publication.
    match read_source_catalog(ctx, &snapshot_path) {
        Ok(previous) => {
            if previous != content {
                return Err(fail(
                    "request_id_conflict",
                    "external source catalog content changed after its frozen snapshot",
                ));
            }
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }
    immutable_file(&snapshot_path, &content)?;
    let current = read_source_catalog(ctx, input_path)?;
    if sha256(&current) != source_hash {
        return Err(fail(
            "revision_conflict",
            "external source catalog changed while its snapshot was being published",
        ));
    }
    agent_immutable_json(&receipt_path, &receipt)?;
    ctx.err()?;
    Ok(snapshot_path)
}

/// Bounds reads through a directory descriptor and
/// rejects replacement, symlinks, special files, growth, or changed bytes
/// during either source sample.
pub(crate) fn read_source_catalog(ctx: &Context, path: &str) -> Result<Vec<u8>> {
    ctx.err()?;
    if !sysutil::is_abs(path) || agent_canonical(path).is_err() {
        return Err(anyhow!("source catalog path is not canonical"));
    }
    let before = super::lstat(path)?;
    if !before.is_regular() || before.size > SOURCE_CATALOG_MAXIMUM {
        return Err(anyhow!("source catalog is not a bounded regular file"));
    }
    let root = sysutil::Root::open(&sysutil::dir(path))?;
    let name = sysutil::base(path);
    let file = root.open_file(
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )?;
    if !sysutil::fstat(&file).is_ok_and(|opened| {
        opened.is_regular()
            && before.same_file(&opened)
            && before.size == opened.size
            && before.modified == opened.modified
    }) {
        return Err(anyhow!("source catalog identity changed while opening it"));
    }
    let mut content = Vec::new();
    (&file)
        .take(SOURCE_CATALOG_MAXIMUM as u64 + 1)
        .read_to_end(&mut content)?;
    let unchanged = match (sysutil::fstat(&file), root.lstat(&name)) {
        (Ok(after), Ok(current)) => {
            agent_canonical(path).is_ok()
                && after.is_regular()
                && current.is_regular()
                && before.same_file(&after)
                && before.same_file(&current)
                && content.len() as i64 == before.size
                && before.size == after.size
                && before.size == current.size
                && before.modified == after.modified
                && before.modified == current.modified
        }
        _ => false,
    };
    if !unchanged {
        return Err(anyhow!("source catalog changed during its bounded read"));
    }
    ctx.err()?;
    Ok(content)
}
