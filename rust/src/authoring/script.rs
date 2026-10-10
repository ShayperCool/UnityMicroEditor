use super::store::*;
use crate::hex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::Path,
    sync::OnceLock,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScriptRequest {
    pub path: String,
    pub expected_revision: String,
    pub operation_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UndoRequest {
    pub path: String,
    pub original_operation_id: String,
    pub expected_revision: String,
    pub operation_id: String,
}
pub(crate) fn new_guid(guid: &str) -> Result<String> {
    if !guid.is_empty() {
        if guid.len() != 32
            || !guid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || guid.bytes().all(|b| b == b'0')
        {
            return Err(failure(
                "invalid_identity",
                "new script GUID must be nonzero lowercase hexadecimal",
                None,
            ));
        }
        return Ok(guid.into());
    }
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}
pub(crate) fn identifier(name: &str) -> bool {
    static IDENTIFIER: OnceLock<Regex> = OnceLock::new();
    IDENTIFIER
        .get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap())
        .is_match(name)
}
pub(crate) fn validate_script(path: &str, content: &str) -> Result<()> {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let base = name.strip_suffix(".cs").unwrap_or(name);
    if !identifier(base) {
        return Err(failure(
            "invalid_script",
            "script filename must be a valid simple C# identifier",
            None,
        ));
    }
    if content.is_empty() || content.len() > 1 << 20 || content.contains('\0') {
        return Err(failure(
            "invalid_script",
            "script content must be nonempty UTF-8 without NUL, at most 1 MiB",
            None,
        ));
    }
    Ok(())
}
impl Store {
    pub fn read_script(&self, path: &str) -> Result<Source> {
        self.asset_path(path, &[".cs"])?;
        self.read(path)
    }
    pub fn create_script(&self, request: &ScriptRequest) -> Result<MutationResult> {
        self.asset_path(&request.path, &[".cs"])?;
        validate_script(&request.path, &request.content)?;
        if request.expected_revision != "absent" {
            return Err(failure(
                "invalid_request",
                "new script creation requires expectedRevision absent",
                None,
            ));
        }
        if !request.guid.is_empty() {
            new_guid(&request.guid)?;
        }
        self.mutate(&request.path, &request.expected_revision, &request.operation_id, request, || {
            let path = self.asset_path(&request.path, &[".cs"])?;
            let meta_path = std::path::PathBuf::from(format!("{}.meta", path.display()));
            for candidate in [&path, &meta_path] {
                match fs::symlink_metadata(candidate) {
                    Ok(_) => return Err(failure("revision_conflict", "new script or its metadata already exists", Some(json!({"path": request.path, "actualRevision": "present"})))),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                }
            }
            if !path.parent().unwrap().is_dir() {
                return Err(failure("invalid_path", "script parent directory must already exist", None));
            }
            let guid = new_guid(&request.guid)?;
            self.check_guid_available(&guid)?;
            let meta = format!("fileFormatVersion: 2\nguid: {guid}\nMonoImporter:\n  externalObjects: {{}}\n  serializedVersion: 2\n  defaultReferences: []\n  executionOrder: 0\n  icon: {{fileID: 0}}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n");
            Ok(MutationPlan { source: Source { path: request.path.clone(), revision: "absent".into(), ..Source::default() }, content: request.content.as_bytes().to_vec(), metadata: meta.into_bytes(), created: BTreeMap::new(), changes: vec![Change { action: "create_script".into(), after: Some(json!(request.path)), ..Change::default() }] })
        })
    }
    pub fn edit_script(&self, request: &ScriptRequest) -> Result<MutationResult> {
        self.asset_path(&request.path, &[".cs"])?;
        validate_script(&request.path, &request.content)?;
        if !request.guid.is_empty() {
            return Err(failure(
                "protected_property",
                "editing a script preserves its existing metadata GUID",
                None,
            ));
        }
        self.mutate(
            &request.path,
            &request.expected_revision,
            &request.operation_id,
            request,
            || {
                let source = self.read_source(&request.path)?;
                Ok(MutationPlan {
                    metadata: source.metadata.as_bytes().to_vec(),
                    source,
                    content: request.content.as_bytes().to_vec(),
                    created: BTreeMap::new(),
                    changes: vec![Change {
                        action: "edit_script".into(),
                        ..Change::default()
                    }],
                })
            },
        )
    }
    pub fn undo(&self, request: &UndoRequest) -> Result<MutationResult> {
        self.asset_path(
            &request.path,
            &[".unity", ".prefab", ".asset", ".mat", ".cs"],
        )?;
        if request.original_operation_id.is_empty() || request.original_operation_id.len() > 128 {
            return Err(failure(
                "invalid_request",
                "originalOperationId is required",
                None,
            ));
        }
        self.mutate(&request.path, &request.expected_revision, &request.operation_id, request, || {
            let source = self.read_source(&request.path)?;
            let data = optional_read(&self.receipt_path(&request.original_operation_id))?.ok_or_else(|| failure("not_found", "original operation journal is absent", None))?;
            let original: Journal = serde_json::from_slice(&data)?;
            if original.state != "committed" || original.result.path != request.path {
                return Err(failure("invalid_request", "original operation did not commit the requested file", None));
            }
            if source.revision != original.result.revision {
                return Err(failure("revision_conflict", "undo would overwrite changes made after the original operation", Some(json!({"actualRevision": source.revision, "originalRevision": original.result.revision}))));
            }
            let mut content = source.content.as_bytes().to_vec();
            let mut metadata = source.metadata.as_bytes().to_vec();
            for file in original.files {
                if !file.existed {
                    return Err(failure("unsupported_undo", "new-file creation cannot be undone through source_undo", None));
                }
                if file.path == request.path { content = file.before; }
                else if file.path == format!("{}.meta", request.path) { metadata = file.before; }
                else { return Err(failure("invalid_request", "original operation includes an unexpected file", None)); }
            }
            if !request.path.ends_with(".cs") {
                let current = super::yaml_edit::TextAsset::parse(&source.content)?;
                let restored = super::yaml_edit::TextAsset::parse(std::str::from_utf8(&content).map_err(|error| failure("invalid_yaml", error.to_string(), None))?)?;
                restored.validate(&source.guid)?;
                restored.preserve_prefab_documents(&current)?;
                let removed = current.docs.iter().filter(|before| !restored.docs.iter().any(|after| before.document.file_id == after.document.file_id)).map(|doc| doc.document.file_id.clone()).collect::<std::collections::HashSet<_>>();
                if !removed.is_empty() { self.check_external_references(&source, &removed)?; }
            }
            Ok(MutationPlan { source, content, metadata, created: BTreeMap::new(), changes: vec![Change { action: "undo".into(), before: Some(json!(request.original_operation_id)), ..Change::default() }] })
        })
    }
    pub fn undo_script(&self, request: &UndoRequest) -> Result<MutationResult> {
        self.asset_path(&request.path, &[".cs"])?;
        self.undo(request)
    }
}
