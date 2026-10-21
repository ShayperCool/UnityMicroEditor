use crate::{ENGINE_VERSION, hex, sha256};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
pub(crate) const MAX_SOURCE: usize = 32 << 20;
pub(crate) type Result<T> = std::result::Result<T, AuthoringError>;

pub use crate::util::AuthoringError;
pub(crate) fn failure(
    code: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> AuthoringError {
    AuthoringError {
        code: code.into(),
        message: message.into(),
        details,
    }
}
/// A replayed receipt is decoded as untyped JSON, so the before/after numbers of
/// its changes are f64 values.
pub(crate) fn replayed_numbers(result: &mut MutationResult) {
    fn convert(value: &mut Value) {
        match value {
            Value::Number(number) => {
                if let Ok(text) = crate::jsondoc::float_text(&number.to_string()) {
                    *number = serde_json::Number::from_string_unchecked(text);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(convert),
            Value::Object(members) => members.values_mut().for_each(convert),
            _ => {}
        }
    }
    for change in &mut result.changes {
        change.before.iter_mut().for_each(convert);
        change.after.iter_mut().for_each(convert);
    }
}
pub(crate) fn guid_pattern() -> &'static Regex {
    static GUID: OnceLock<Regex> = OnceLock::new();
    GUID.get_or_init(|| Regex::new(r"(?m)^guid:[ \t\r\n\x0c]*([0-9a-fA-F]{32})[ \t]*\r?$").unwrap())
}
/// The GUID the Editor reads from a `.meta`: its first `guid:` line. A
/// committed merge conflict can leave two; the Editor keeps the first.
pub(crate) fn meta_guid_of(text: &str) -> Option<String> {
    guid_pattern()
        .captures(text)
        .map(|capture| capture[1].to_lowercase())
}
pub(crate) fn revision(data: &[u8], meta: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"unitymicroeditor-authored-v1\0");
    hash.update(Sha256::digest(data));
    hash.update(Sha256::digest(meta));
    hex(&hash.finalize())
}
/// Encodes with HTML escaping (`&`, `<`, `>`, U+2028, U+2029) for persisted request fingerprints.
/// Serialize typed requests directly so their declared field order is retained.
pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let text = serde_json::to_string(value)?;
    Ok(text
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .into_bytes())
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    pub revision: String,
    pub content: String,
    pub metadata: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub property: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MutationResult {
    pub operation_id: String,
    pub path: String,
    pub guid: String,
    pub previous_revision: String,
    pub revision: String,
    pub changed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub created_ids: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
}
mod byte_string {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        bytes: &[u8],
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Vec<u8>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JournalFile {
    pub path: String,
    #[serde(default, with = "byte_string", skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<u8>,
    #[serde(with = "byte_string")]
    pub after: Vec<u8>,
    pub existed: bool,
    pub mode: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Journal {
    pub operation_id: String,
    pub fingerprint: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<JournalFile>,
    pub result: MutationResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_result: Option<super::PublicationResult>,
}
pub(crate) struct MutationPlan {
    pub source: Source,
    pub content: Vec<u8>,
    pub metadata: Vec<u8>,
    pub created: BTreeMap<String, String>,
    pub changes: Vec<Change>,
}
#[derive(Debug, Clone)]
pub struct Store {
    pub(crate) root: PathBuf,
    pub(crate) state: PathBuf,
    cancel: Option<Arc<AtomicBool>>,
}
/// The extension: the suffix from the final dot of the final path element.
pub(crate) fn path_ext(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rfind('.').map_or("", |index| &name[index..])
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub unity_version: String,
    pub source_writes: bool,
    pub operations: Vec<String>,
    pub guarantees: Vec<String>,
    pub limitations: Vec<String>,
}
impl Store {
    pub fn new(project: &Path) -> Result<Self> {
        if project.as_os_str().is_empty() {
            return Err(failure(
                "invalid_project",
                "an explicit Unity project path is required",
                None,
            ));
        }
        // Make the path absolute, then resolve symlinks.
        let absolute = std::path::absolute(project)?;
        let root = PathBuf::from(
            crate::sysutil::eval_symlinks(&crate::sysutil::clean(&absolute.to_string_lossy()))
                .map_err(|error| failure("internal_error", error.message(), None))?,
        );
        let store = Self {
            state: root.join("Library/UnityMicroEditor/authoring"),
            root,
            cancel: None,
        };
        for name in ["Assets", "ProjectSettings"] {
            let path = store.safe_path(name, true)?;
            if !path.is_dir() {
                return Err(failure(
                    "invalid_project",
                    format!("missing Unity project directory {name}"),
                    None,
                ));
            }
        }
        let path = store.safe_path("ProjectSettings/ProjectVersion.txt", false)?;
        let text = String::from_utf8(read_limited(&path)?).unwrap();
        // The last declaration decides whether the project has a version at all.
        if text
            .split('\n')
            .filter_map(|line| line.strip_prefix("m_EditorVersion:"))
            .next_back()
            .is_none_or(|version| version.trim().is_empty())
        {
            return Err(failure(
                "invalid_project",
                "ProjectVersion.txt has no m_EditorVersion",
                None,
            ));
        }
        Ok(store)
    }
    /// A handle whose operations stop with a `canceled` error once `flag` is set.
    pub fn with_cancellation(&self, flag: Arc<AtomicBool>) -> Self {
        Self {
            cancel: Some(flag),
            ..self.clone()
        }
    }
    pub(crate) fn check_canceled(&self) -> Result<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(failure("internal_error", "canceled", None));
        }
        Ok(())
    }
    pub(crate) fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn current_version(&self) -> Result<String> {
        let path = self.safe_path("ProjectSettings/ProjectVersion.txt", false)?;
        let text = String::from_utf8(read_limited(&path)?).unwrap();
        for line in text.split('\n') {
            if let Some(version) = line.strip_prefix("m_EditorVersion:") {
                return Ok(version.trim().into());
            }
        }
        Err(failure(
            "invalid_project",
            "ProjectVersion.txt has no m_EditorVersion",
            None,
        ))
    }
    pub(crate) fn safe_path(&self, relative: &str, allow_dir: bool) -> Result<PathBuf> {
        if relative.is_empty()
            || relative.contains(['\\', '\0'])
            || relative.starts_with('/')
            || relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(failure(
                "invalid_path",
                "use a normalized project-relative path",
                Some(json!({"path": relative})),
            ));
        }
        let mut path = self.root.clone();
        let parts: Vec<_> = relative.split('/').collect();
        for (index, part) in parts.iter().enumerate() {
            path.push(part);
            let info = match fs::symlink_metadata(&path) {
                Ok(info) => info,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if info.is_symlink() {
                return Err(failure(
                    "invalid_path",
                    "symlink paths are unsupported",
                    Some(json!({"path": relative})),
                ));
            }
            if index + 1 < parts.len() && !info.is_dir() {
                return Err(failure(
                    "invalid_path",
                    "path ancestor is not a directory",
                    None,
                ));
            }
            if index + 1 == parts.len() && info.is_dir() && !allow_dir {
                return Err(failure("invalid_path", "expected a file", None));
            }
        }
        Ok(path)
    }
    pub(crate) fn asset_path(&self, relative: &str, extensions: &[&str]) -> Result<PathBuf> {
        if !relative.starts_with("Assets/") {
            return Err(failure(
                "invalid_path",
                "authored assets must be beneath Assets",
                None,
            ));
        }
        let path = self.safe_path(relative, false)?;
        let extension = path_ext(relative).to_lowercase();
        if !extensions.contains(&extension.as_str()) {
            return Err(failure(
                "unsupported_asset",
                "unsupported authored asset extension",
                Some(json!({"path": relative})),
            ));
        }
        Ok(path)
    }
    pub(crate) fn read_source(&self, relative: &str) -> Result<Source> {
        let path = self.asset_path(relative, &[".unity", ".prefab", ".asset", ".mat", ".cs"])?;
        if !path.exists() {
            return Err(failure(
                "not_found",
                "authored asset does not exist",
                Some(json!({"path": relative})),
            ));
        }
        let data = read_limited(&path)?;
        let metadata_path = self.safe_path(&format!("{relative}.meta"), false)?;
        let meta = read_limited(&metadata_path).map_err(|error| {
            failure(
                "invalid_identity",
                "asset requires an existing valid .meta file",
                Some(json!({"path": relative, "cause": error.to_string()})),
            )
        })?;
        let Some(guid) = meta_guid_of(std::str::from_utf8(&meta).unwrap()) else {
            return Err(failure(
                "invalid_identity",
                "asset metadata requires a GUID",
                Some(json!({"path": relative})),
            ));
        };
        Ok(Source {
            path: relative.into(),
            guid,
            revision: revision(&data, &meta),
            content: String::from_utf8(data).unwrap(),
            metadata: String::from_utf8(meta).unwrap(),
        })
    }
    pub fn read(&self, relative: &str) -> Result<Source> {
        self.locked(|| self.read_source(relative))
    }
    pub(crate) fn locked<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        self.safe_path("Library/UnityMicroEditor/authoring", true)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.state)?;
        let path = self.safe_path("Library/UnityMicroEditor/authoring/lock", false)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    self.check_canceled()?;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        self.check_canceled()?;
        self.recover()?;
        // Dropping the file unlocks even if the action fails or unwinds.
        action()
    }
    pub(crate) fn receipt_path(&self, operation_id: &str) -> PathBuf {
        self.state
            .join(format!("{}.json", sha256(operation_id.as_bytes())))
    }
    fn recover(&self) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(&self.state)?.collect::<io::Result<_>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_name().to_string_lossy().ends_with(".json") {
                continue;
            }
            if entry.file_type()?.is_symlink() {
                return Err(failure(
                    "invalid_path",
                    "symlink authoring journal is unsupported",
                    None,
                ));
            }
            let data = fs::read(entry.path())?;
            let mut journal: Journal = serde_json::from_slice(&data).map_err(|_| {
                failure(
                    "recovery_required",
                    "invalid authoring journal",
                    Some(json!({"path": entry.path()})),
                )
            })?;
            if ["committed", "rolled_back"].contains(&journal.state.as_str()) {
                continue;
            }
            if journal.state != "prepared" {
                return Err(failure(
                    "recovery_required",
                    "unknown journal state",
                    Some(json!({"operationId": journal.operation_id})),
                ));
            }
            let mut all_after = true;
            for file in &journal.files {
                let path = self.safe_path(&file.path, false)?;
                let current = optional_read(&path)?;
                let is_after = current.as_deref() == Some(file.after.as_slice());
                let is_before = match &current {
                    None => !file.existed,
                    Some(current) => file.existed && *current == file.before,
                };
                if !is_after && !is_before {
                    return Err(failure(
                        "outcome_unknown",
                        "external changes prevent transaction recovery",
                        Some(json!({"operationId": journal.operation_id, "path": file.path})),
                    ));
                }
                all_after &= is_after;
            }
            if all_after {
                journal.state = "committed".into();
            } else {
                for file in &journal.files {
                    let path = self.safe_path(&file.path, false)?;
                    if file.existed {
                        atomic_write(&path, &file.before, file.mode)?;
                    } else {
                        match fs::remove_file(&path) {
                            Ok(()) => File::open(path.parent().unwrap())?.sync_all()?,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                journal.state = "rolled_back".into();
            }
            write_journal(&entry.path(), &journal)?;
        }
        Ok(())
    }
    pub(crate) fn mutate<T: Serialize>(
        &self,
        path: &str,
        expected: &str,
        operation_id: &str,
        request: &T,
        build: impl FnOnce() -> Result<MutationPlan>,
    ) -> Result<MutationResult> {
        self.mutate_with_guard(path, expected, operation_id, request, None, build)
    }
    pub(crate) fn mutate_with_guard<T: Serialize>(
        &self,
        path: &str,
        expected: &str,
        operation_id: &str,
        request: &T,
        guard: Option<&dyn Fn() -> Result<()>>,
        build: impl FnOnce() -> Result<MutationPlan>,
    ) -> Result<MutationResult> {
        if expected.is_empty()
            || operation_id.is_empty()
            || operation_id.len() > 128
            || operation_id.trim() != operation_id
        {
            return Err(failure(
                "invalid_request",
                "writes require expectedRevision and operationId of 1 to 128 characters",
                None,
            ));
        }
        let fingerprint = sha256(&encode_json(request)?);
        self.locked(|| {
            let version = self.current_version()?;
            if version != ENGINE_VERSION {
                return Err(failure("unsupported_unity_version", "source writes require the verified Unity profile", Some(json!({"projectVersion": version, "verifiedVersion": ENGINE_VERSION}))));
            }
            let journal_path = self.receipt_path(operation_id);
            if let Some(old) = optional_read(&journal_path)? {
                let receipt: Journal = serde_json::from_slice(&old)?;
                if receipt.fingerprint != fingerprint {
                    return Err(failure("idempotency_conflict", "operationId was already used for different arguments", Some(json!({"operationId": operation_id}))));
                }
                if receipt.state != "committed" {
                    return Err(failure("rolled_back", "interrupted operation was rolled back; use a new operationId", None));
                }
                let mut result = MutationResult { replayed: true, ..receipt.result };
                replayed_numbers(&mut result);
                return Ok(result);
            }
            let plan = build()?;
            if plan.source.revision != expected {
                return Err(failure("revision_conflict", "authored asset or metadata changed", Some(json!({"path": path, "expectedRevision": expected, "actualRevision": plan.source.revision}))));
            }
            let guid = if plan.source.guid.is_empty() {
                guid_pattern().captures(std::str::from_utf8(&plan.metadata).unwrap()).map(|c| c[1].to_string()).unwrap_or_default()
            } else { plan.source.guid.clone() };
            let result = MutationResult {
                operation_id: operation_id.into(), path: path.into(), guid,
                previous_revision: expected.into(), revision: revision(&plan.content, &plan.metadata),
                changed: plan.source.content.as_bytes() != plan.content || plan.source.metadata.as_bytes() != plan.metadata,
                created_ids: plan.created, changes: plan.changes, replayed: false,
            };
            let mut journal = Journal { operation_id: operation_id.into(), fingerprint, state: "prepared".into(), files: vec![], result: result.clone(), batch_result: None };
            for (relative, before, after) in [(path.to_string(), plan.source.content.as_bytes(), plan.content.as_slice()), (format!("{path}.meta"), plan.source.metadata.as_bytes(), plan.metadata.as_slice())] {
                let absolute = self.safe_path(&relative, false)?;
                let current = optional_read(&absolute)?;
                let existed = current.is_some();
                let mode = if existed { fs::metadata(&absolute)?.permissions().mode() & 0o777 } else { 0o644 };
                if (plan.source.revision == "absent" && existed) || (plan.source.revision != "absent" && current.as_deref() != Some(before)) {
                    return Err(failure("revision_conflict", "source changed while preparing the mutation", Some(json!({"path": relative}))));
                }
                if !existed || before != after {
                    journal.files.push(JournalFile { path: relative, before: before.to_vec(), after: after.to_vec(), existed, mode });
                }
            }
            if let Some(guard) = guard {
                guard()?;
                if plan.source.revision != "absent" && !self.read_source(path).is_ok_and(|current| current.revision == expected) {
                    return Err(failure("revision_conflict", "source or metadata changed during publication guard", Some(json!({"path":path}))));
                }
                for file in &journal.files {
                    let current = optional_read(&self.safe_path(&file.path, false)?)?;
                    let unchanged = if file.existed { current.as_deref() == Some(file.before.as_slice()) } else { current.is_none() };
                    if !unchanged {
                        return Err(failure("revision_conflict", "source changed during publication guard", Some(json!({"path":file.path}))));
                    }
                }
            }
            // Durable intent precedes every source write; receipt follows every write.
            write_journal(&journal_path, &journal)?;
            for file in &journal.files {
                let path = self.safe_path(&file.path, false)?;
                atomic_write(&path, &file.after, file.mode).map_err(|error| failure("outcome_unknown", "write interrupted; retrying recovers the journal", Some(json!({"operationId": operation_id, "cause": error.to_string()}))))?;
            }
            journal.state = "committed".into();
            write_journal(&journal_path, &journal).map_err(|error| failure("outcome_unknown", "source written but result journal was not committed", Some(json!({"operationId": operation_id, "cause": error.to_string()}))))?;
            Ok(result)
        })
    }
    pub(crate) fn walk_assets(&self) -> Result<Vec<PathBuf>> {
        fn visit(path: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
            let mut children = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_key(|entry| entry.file_name());
            for entry in children {
                entries.push(entry.path());
                if entry.file_type()?.is_dir() {
                    visit(&entry.path(), entries)?;
                }
            }
            Ok(())
        }
        let mut entries = Vec::new();
        visit(&self.root.join("Assets"), &mut entries)?;
        Ok(entries)
    }
    pub(crate) fn check_guid_available(&self, guid: &str) -> Result<()> {
        for path in self.walk_assets()? {
            let info = fs::symlink_metadata(&path)?;
            if info.is_symlink() || info.is_dir() || !path.to_string_lossy().ends_with(".meta") {
                continue;
            }
            let text = String::from_utf8(read_limited(&path)?).unwrap();
            for capture in guid_pattern().captures_iter(&text) {
                if capture[1].eq_ignore_ascii_case(guid) {
                    return Err(failure(
                        "duplicate_guid",
                        "new script GUID is already used",
                        Some(json!({"guid": guid, "path": path.strip_prefix(&self.root).unwrap()})),
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn inspect(&self) -> Result<ProjectInfo> {
        self.locked(|| self.inspect_unlocked())
    }
    pub(crate) fn inspect_unlocked(&self) -> Result<ProjectInfo> {
        {
            let version = self.current_version()?;
            let mut result = ProjectInfo {
                project: self.root.to_string_lossy().into(),
                unity_version: version.clone(),
                verified_unity_version: ENGINE_VERSION.into(),
                version_matches: version == ENGINE_VERSION,
                scenes: vec![],
                prefabs: vec![],
                scripts: vec![],
                assembly_definitions: vec![],
                identity_issues: vec![],
            };
            let mut guids = BTreeMap::new();
            for path in self.walk_assets()? {
                let relative = path
                    .strip_prefix(&self.root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let info = fs::symlink_metadata(&path)?;
                if info.is_symlink() {
                    result
                        .identity_issues
                        .push(format!("{relative}: symlink excluded"));
                    continue;
                }
                if info.is_dir() {
                    continue;
                }
                match path_ext(&relative).to_lowercase().as_str() {
                    ".unity" => result.scenes.push(relative),
                    ".prefab" => result.prefabs.push(relative),
                    ".cs" => result.scripts.push(relative),
                    ".asmdef" => result.assembly_definitions.push(relative),
                    ".meta" => {
                        let text = String::from_utf8(read_limited(&path)?).unwrap();
                        let Some(guid) = meta_guid_of(&text) else {
                            result
                                .identity_issues
                                .push(format!("{relative}: metadata requires a GUID"));
                            continue;
                        };
                        if let Some(previous) = guids.insert(guid.clone(), relative.clone()) {
                            result
                                .identity_issues
                                .push(format!("duplicate GUID {guid}: {previous} and {relative}"));
                        }
                    }
                    _ => (),
                }
            }
            for collection in [
                &mut result.scenes,
                &mut result.prefabs,
                &mut result.scripts,
                &mut result.assembly_definitions,
            ] {
                collection.sort();
                for relative in collection.iter() {
                    if fs::symlink_metadata(self.root.join(format!("{relative}.meta")))
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
                    {
                        result
                            .identity_issues
                            .push(format!("{relative}: missing metadata"));
                    }
                }
            }
            result.identity_issues.sort();
            Ok(result)
        }
    }
    pub fn capabilities(&self) -> Capabilities {
        let version = self
            .current_version()
            .unwrap_or_else(|_| "unavailable".into());
        let list = |items: &[&str]| items.iter().map(|item| item.to_string()).collect();
        Capabilities {
            source_writes: version == ENGINE_VERSION,
            unity_version: version,
            operations: list(&[
                "paged source/native asset inventory",
                "authored and recursively resolved scene/prefab hierarchy",
                "existing serialized scalar/reference/collection property updates",
                "plain GameObject creation/deletion/reparenting",
                "explicit RectTransform creation",
                "supported native and MonoBehaviour component attachment/removal",
                "source prefab instantiation and scalar/reference override set/revert",
                "explicit material/ScriptableObject/prefab creation",
                "revision-checked staged multi-file publication",
                "script read/create/edit",
                "preview scene changes without source writes",
                "undo existing-file and multi-file edits without intervening changes",
            ]),
            guarantees: list(&[
                "project-bound normalized paths",
                "preserve existing GUIDs and fileIDs",
                "preserve YAML bytes outside edited property/document spans",
                "expected asset and metadata revision",
                "idempotency survives server restart",
                "project-wide writer lock",
                "recoverable file transactions",
            ]),
            limitations: list(&[
                "source authoring does not imply compiler or Player success",
                "direct stripped-object writes and generic prefab-instance property writes remain protected; use explicit override operations",
                "source prefab create/variant/apply/revert/partial and complete unpack use prefab_edit; native-model prefabs and unsupported structural GameObject changes fail explicitly",
                "reparent preserves local transform only",
                "low-level native attachment requires explicit fields or a revision-bound template; component_add reads cached native defaults and companions",
                "MonoBehaviour attachment requires an existing script GUID and explicit fields",
                "native components are limited to NativeComponentTypes; cached Canvas can replace an ordinary Transform preserving its identity, inherited Transform replacement requires unpack",
                "Unity Editor and external writers do not participate in the service lock; multi-file publication does not isolate external readers",
                "cross-asset references to deleted objects are checked in Assets text YAML and refused; they are not rewritten",
                "package/SDK component reference validation remains the importer and runtime responsibility",
                "undo cannot erase newly created files",
                "inventory revision binds file stat values and metadata; use sourceRevision for exact authored byte guards",
            ]),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub project: String,
    pub unity_version: String,
    pub verified_unity_version: String,
    pub version_matches: bool,
    pub scenes: Vec<String>,
    pub prefabs: Vec<String>,
    pub scripts: Vec<String>,
    pub assembly_definitions: Vec<String>,
    pub identity_issues: Vec<String>,
}
pub(crate) fn read_limited(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let info = file.metadata()?;
    if !info.is_file() || info.len() > MAX_SOURCE as u64 {
        return Err(failure(
            "unsupported_asset",
            "expected a regular text file at most 32 MiB",
            None,
        ));
    }
    let mut data = Vec::with_capacity(info.len() as usize);
    file.take(MAX_SOURCE as u64 + 1).read_to_end(&mut data)?;
    if data.len() > MAX_SOURCE || std::str::from_utf8(&data).is_err() {
        return Err(failure(
            "unsupported_asset",
            "expected a UTF-8 text file at most 32 MiB",
            None,
        ));
    }
    Ok(data)
}
pub(crate) fn optional_read(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().unwrap();
    let mut temp = tempfile::Builder::new()
        .prefix(".ume-authoring-")
        .tempfile_in(parent)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub(crate) fn write_journal(path: &Path, journal: &Journal) -> Result<()> {
    let mut bytes = encode_json(journal)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, 0o600)
}
