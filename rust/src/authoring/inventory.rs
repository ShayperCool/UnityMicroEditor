use super::store::*;
use crate::sha256;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventoryRequest {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub path_prefix: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub limit: i64,
    #[serde(default)]
    pub expected_revision: String,
    #[serde(default)]
    pub include_source_revision: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryEntry {
    pub path: String,
    pub extension: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub guid: String,
    pub size: u64,
    pub modified_utc: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub metadata_revision: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source_revision: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identity_issue: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryPage {
    pub project: String,
    pub unity_version: String,
    pub revision: String,
    pub entries: Vec<InventoryEntry>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}
impl Store {
    pub fn inventory(&self, request: &InventoryRequest) -> Result<InventoryPage> {
        if request.offset < 0
            || request.limit < 0
            || request.limit > 500
            || request.query.len() > 1024
            || request.extensions.len() > 64
        {
            return Err(failure(
                "invalid_request",
                "inventory offset must be nonnegative and limit at most 500",
                None,
            ));
        }
        let limit = if request.limit == 0 {
            100
        } else {
            request.limit as usize
        };
        if !request.path_prefix.is_empty() {
            if request.path_prefix != "Assets" && !request.path_prefix.starts_with("Assets/") {
                return Err(failure(
                    "invalid_path",
                    "inventory pathPrefix must be Assets or a normalized child",
                    None,
                ));
            }
            self.safe_path(&request.path_prefix, true)?;
        }
        let mut extensions = BTreeSet::new();
        for extension in &request.extensions {
            if extension.len() < 2
                || extension.len() > 64
                || !extension.starts_with('.')
                || extension.contains(['/', '\\'])
            {
                return Err(failure(
                    "invalid_request",
                    "inventory extensions must use .extension syntax",
                    None,
                ));
            }
            extensions.insert(extension.to_lowercase());
        }
        self.locked(|| {
            let version = self.current_version()?;
            let mut entries = Vec::new();
            let mut guids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for path in self.walk_assets()? {
                let info = fs::symlink_metadata(&path)?;
                self.check_canceled()?;
                if !info.is_file() || path.to_string_lossy().ends_with(".meta") { continue; }
                let date = chrono::DateTime::from_timestamp(info.mtime(), info.mtime_nsec() as u32).ok_or_else(|| failure("unsupported_asset", "file timestamp is outside supported range", None))?;
                let fraction = if info.mtime_nsec() == 0 { String::new() } else { format!(".{:09}", info.mtime_nsec()).trim_end_matches('0').into() };
                let relative = path.strip_prefix(&self.root).unwrap().to_string_lossy().into_owned();
                let extension = path_ext(&relative).to_lowercase();
                let mut item = InventoryEntry { path: relative.clone(), extension, guid: String::new(), size: info.len(), modified_utc: format!("{}{fraction}Z", date.format("%Y-%m-%dT%H:%M:%S")), metadata_revision: String::new(), source_revision: String::new(), identity_issue: String::new() };
                match self.safe_path(&format!("{relative}.meta"), false) {
                    Err(error) => item.identity_issue = error.to_string(),
                    Ok(meta_path) if !meta_path.exists() => item.identity_issue = "missing metadata".into(),
                    Ok(meta_path) => match read_limited(&meta_path) {
                        Err(error) => item.identity_issue = error.to_string(),
                        Ok(meta) => {
                            item.metadata_revision = sha256(&meta);
                            let text = std::str::from_utf8(&meta).unwrap();
                            let matches: Vec<_> = guid_pattern().captures_iter(text).collect();
                            if matches.len() != 1 { item.identity_issue = "metadata requires exactly one GUID".into(); }
                            else {
                                item.guid = matches[0][1].to_lowercase();
                                guids.entry(item.guid.clone()).or_default().push(entries.len());
                            }
                        }
                    },
                }
                entries.push(item);
            }
            for (guid, indices) in guids {
                if indices.len() > 1 { for index in indices { entries[index].identity_issue = format!("duplicate GUID {guid}"); } }
            }
            entries.sort_by(|a,b|a.path.cmp(&b.path));
            #[derive(Serialize)]
            struct Snapshot<'a> {
                #[serde(rename = "Version")] version: &'a str,
                #[serde(rename = "Entries")] entries: &'a [InventoryEntry],
            }
            let mut bytes = b"unitymicroeditor-inventory-v1\0".to_vec();
            bytes.extend(encode_json(&Snapshot { version: &version, entries: &entries })?);
            let revision = sha256(&bytes);
            if !request.expected_revision.is_empty() && request.expected_revision != revision {
                return Err(failure("revision_conflict", "Assets inventory changed between pages", Some(json!({"actualRevision": revision, "expectedRevision": request.expected_revision}))));
            }
            let query = request.query.to_lowercase();
            let filtered: Vec<_> = entries.into_iter().filter(|item| {
                (request.path_prefix.is_empty() || item.path == request.path_prefix || item.path.starts_with(&format!("{}/", request.path_prefix)))
                && (extensions.is_empty() || extensions.contains(&item.extension))
                && (query.is_empty() || item.path.to_lowercase().contains(&query))
            }).collect();
            let total = filtered.len();
            let offset = request.offset as usize;
            if offset > total { return Err(failure("invalid_request", "inventory offset exceeds filtered entry count", None)); }
            let end = (offset + limit).min(total);
            let mut entries: Vec<_> = filtered.into_iter().skip(offset).take(limit).collect();
            if request.include_source_revision {
                for item in &mut entries {
                    if item.identity_issue.is_empty() && [".unity", ".prefab", ".mat", ".asset", ".cs"].contains(&item.extension.as_str()) {
                        item.source_revision = self.read_source(&item.path)?.revision;
                    }
                }
            }
            Ok(InventoryPage { project: self.root.to_string_lossy().into(), unity_version: version, revision, entries, total, offset, next_offset: (end < total).then_some(end) })
        })
    }
}
