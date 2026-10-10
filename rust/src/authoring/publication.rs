use super::{edit::nullable, script::validate_script, store::*, yaml_edit::TextAsset};
use crate::{ENGINE_VERSION, hex, sha256};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::Path,
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationInput {
    pub path: String,
    pub expected_revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationOutput {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StagedPublicationRequest {
    #[serde(default)]
    pub operation_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub inputs: Vec<PublicationInput>,
    #[serde(default, deserialize_with = "nullable")]
    pub outputs: Vec<PublicationOutput>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationResult {
    #[serde(default)]
    pub operation_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub assets: Vec<MutationResult>,
    #[serde(default)]
    pub changed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationUndoRequest {
    pub operation_id: String,
    pub original_operation_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub expected_revisions: BTreeMap<String, String>,
}
fn source_root(path: &str) -> bool {
    ["Assets/", "Packages/", "ProjectSettings/"]
        .iter()
        .any(|root| path.starts_with(root))
}
fn hash_regular(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dependency must be a regular file",
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}
impl Store {
    pub(crate) fn dependency_revision_inner(&self, path: &str) -> Result<String> {
        if !source_root(path) || path.ends_with(".meta") {
            return Err(failure(
                "invalid_path",
                "dependency must be a source file under Assets, ProjectSettings or Packages",
                None,
            ));
        }
        let absolute = self.safe_path(path, false)?;
        let meta_path = self.safe_path(&format!("{path}.meta"), false)?;
        let content_hash = match hash_regular(&absolute) {
            Ok(hash) => hash,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(meta_path)
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
                {
                    return Ok("absent".into());
                }
                return Err(failure(
                    "invalid_identity",
                    "absent source has existing metadata",
                    Some(json!({"path":path})),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                return Err(failure(
                    "invalid_path",
                    "dependency must be a regular file",
                    None,
                ));
            }
            Err(e) => return Err(e.into()),
        };
        let meta_hash = match hash_regular(&meta_path) {
            Ok(hash) => hash,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Sha256::digest([]).into(),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                return Err(failure(
                    "invalid_path",
                    "dependency must be a regular file",
                    None,
                ));
            }
            Err(e) => return Err(e.into()),
        };
        let mut hash = Sha256::new();
        hash.update(b"unitymicroeditor-authored-v1\0");
        hash.update(content_hash);
        hash.update(meta_hash);
        Ok(hex(&hash.finalize()))
    }
    pub fn dependency_revision(&self, path: &str) -> Result<String> {
        self.locked(|| self.dependency_revision_inner(path))
    }
    /// A publication boundary, not an Editor execution service. Every dependency
    /// is checked before any write; staged content is validated and published
    /// through the same recoverable journal as source edits.
    pub fn publish_staged(&self, request: &StagedPublicationRequest) -> Result<PublicationResult> {
        self.publish_staged_guarded(request, None)
    }
    pub fn publish_staged_with_guard(
        &self,
        request: &StagedPublicationRequest,
        before_publish: &dyn Fn() -> Result<()>,
    ) -> Result<PublicationResult> {
        self.publish_staged_guarded(request, Some(before_publish))
    }
    pub(crate) fn publish_staged_guarded(
        &self,
        request: &StagedPublicationRequest,
        before_publish: Option<&dyn Fn() -> Result<()>>,
    ) -> Result<PublicationResult> {
        if request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || request.operation_id.trim() != request.operation_id
            || request.inputs.is_empty()
            || request.inputs.len() > 4096
            || request.outputs.is_empty()
            || request.outputs.len() > 128
        {
            return Err(failure(
                "invalid_request",
                "staged publication requires operationId, 1..4096 inputs and 1..128 outputs",
                None,
            ));
        }
        let mut inputs = BTreeMap::new();
        for input in &request.inputs {
            if input.expected_revision.is_empty()
                || inputs
                    .insert(input.path.clone(), input.expected_revision.clone())
                    .is_some()
            {
                return Err(failure(
                    "invalid_request",
                    "each input must have a unique path and expectedRevision",
                    None,
                ));
            }
            if !source_root(&input.path) {
                return Err(failure(
                    "invalid_path",
                    "publication input is outside project source roots",
                    None,
                ));
            }
        }
        let mut seen = BTreeSet::new();
        let mut total = 0;
        for output in &request.outputs {
            self.asset_path(
                &output.path,
                &[".unity", ".prefab", ".mat", ".asset", ".cs"],
            )?;
            if !seen.insert(output.path.clone()) || !inputs.contains_key(&output.path) {
                return Err(failure(
                    "invalid_request",
                    "every output requires one corresponding input revision",
                    Some(json!({"path":output.path})),
                ));
            }
            total += output.content.len() + output.metadata.as_ref().map_or(0, String::len);
            if output.content.is_empty()
                || output.content.len() > MAX_SOURCE
                || output.content.contains('\0')
                || total > 64 << 20
            {
                return Err(failure(
                    "invalid_request",
                    "outputs must be UTF-8 text without NUL, at most 32 MiB each and 64 MiB total",
                    None,
                ));
            }
            if output
                .metadata
                .as_ref()
                .is_some_and(|meta| meta.len() > MAX_SOURCE)
            {
                return Err(failure(
                    "invalid_identity",
                    "staged metadata must be UTF-8 at most 32 MiB",
                    None,
                ));
            }
        }
        let fingerprint = sha256(&encode_json(request)?);
        self.locked(||{
            if self.current_version()?!=ENGINE_VERSION{return Err(failure("unsupported_unity_version","staged publication requires the verified Unity profile",None));}
            let receipt_path=self.receipt_path(&request.operation_id);
            if let Some(data)=optional_read(&receipt_path)?{
                let receipt:Journal=serde_json::from_slice(&data)?;
                if receipt.fingerprint!=fingerprint||receipt.batch_result.is_none(){return Err(failure("idempotency_conflict","operationId was already used for different arguments",None));}
                if receipt.state!="committed"{return Err(failure("rolled_back","interrupted staged publication was rolled back; use a new operationId",None));}
                let mut result=receipt.batch_result.unwrap();result.replayed=true;result.assets.iter_mut().for_each(replayed_numbers);return Ok(result);
            }
            let check_inputs=||{
                for input in &request.inputs{
                    let actual=self.dependency_revision_inner(&input.path)?;
                    if actual!=input.expected_revision{return Err(failure("revision_conflict","staged authoring input changed",Some(json!({"path":input.path,"expectedRevision":input.expected_revision,"actualRevision":actual}))));}
                }Ok(())
            };
            check_inputs()?;
            let mut result=PublicationResult {operation_id:request.operation_id.clone(),assets:vec![],changed:false,replayed:false};
            let mut transaction=Journal {operation_id:request.operation_id.clone(),fingerprint,state:"prepared".into(),files:vec![],result:MutationResult::default(),batch_result:None};
            let mut overlay=BTreeMap::new();let mut old_sources=BTreeMap::new();let mut new_guids=BTreeMap::new();
            for output in &request.outputs{
                self.check_canceled()?;
                let source=if inputs[&output.path]=="absent"{Source {path:output.path.clone(),revision:"absent".into(),..Source::default()}}else{self.read_source(&output.path)?};
                old_sources.insert(output.path.clone(),source.clone());
                let metadata=output.metadata.clone().unwrap_or_else(||source.metadata.clone());
                if source.revision!="absent"&&metadata!=source.metadata{return Err(failure("protected_property","staged publication must preserve existing metadata byte-for-byte",Some(json!({"path":output.path}))));}
                let matches:Vec<_>=guid_pattern().captures_iter(&metadata).collect();
                if matches.len()!=1||matches[0][1].bytes().all(|b|b==b'0'){return Err(failure("invalid_identity","each staged asset requires exactly one nonzero metadata GUID",Some(json!({"path":output.path}))));}
                let guid=matches[0][1].to_lowercase();
                if source.revision=="absent"{
                    self.check_guid_available(&guid)?;
                    if let Some(previous)=new_guids.insert(guid.clone(),output.path.clone()){return Err(failure("duplicate_guid","staged outputs reuse one GUID",Some(json!({"path":output.path,"otherPath":previous}))));}
                }
                if output.path.ends_with(".cs"){validate_script(&output.path,&output.content)?;}else{let asset=TextAsset::parse(&output.content)?;asset.validate(&guid)?;if output.path.ends_with(".prefab"){asset.validate_prefab_root()?;}}
                let next=Source {path:output.path.clone(),guid:guid.clone(),revision:revision(output.content.as_bytes(),metadata.as_bytes()),content:output.content.clone(),metadata:metadata.clone()};
                let changed=source.content!=output.content||source.metadata!=metadata;
                result.assets.push(MutationResult {operation_id:request.operation_id.clone(),path:output.path.clone(),guid,previous_revision:source.revision.clone(),revision:next.revision.clone(),changed,..MutationResult::default()});result.changed|=changed;
                overlay.insert(output.path.clone(),next);
                for (relative,before,after) in [(output.path.clone(),source.content.as_bytes(),output.content.as_bytes()),(format!("{}.meta",output.path),source.metadata.as_bytes(),metadata.as_bytes())]{
                    let absolute=self.safe_path(&relative,false)?;
                    if !absolute.parent().unwrap().is_dir(){return Err(failure("invalid_path","output parent directory must already exist",Some(json!({"path":relative}))));}
                    if source.revision=="absent"||before!=after{
                        let mode=fs::metadata(absolute).map(|info|info.permissions().mode()&0o777).unwrap_or(0o644);
                        transaction.files.push(JournalFile {path:relative,before:before.to_vec(),after:after.to_vec(),existed:source.revision!="absent",mode});
                    }
                }
            }
            for (path,source) in old_sources{
                if source.revision=="absent"||path.ends_with(".cs"){continue;}
                let before=TextAsset::parse(&source.content)?;let after=TextAsset::parse(&overlay[&path].content)?;
                let removed=before.docs.iter().filter(|doc|!after.docs.iter().any(|after|after.document.file_id==doc.document.file_id)).map(|doc|doc.document.file_id.clone()).collect::<HashSet<_>>();
                if !removed.is_empty(){self.check_external_references_overlay(&source,&removed,&overlay)?;}
            }
            transaction.batch_result=Some(result.clone());
            if let Some(guard)=before_publish{guard()?;}
            check_inputs()?;
            write_journal(&receipt_path,&transaction)?;
            for file in &transaction.files{
                let absolute=self.safe_path(&file.path,false)?;
                atomic_write(&absolute,&file.after,file.mode).map_err(|e|failure("outcome_unknown","staged publication interrupted; retry recovers the journal",Some(json!({"operationId":request.operation_id,"cause":e.to_string()}))))?;
            }
            transaction.state="committed".into();
            write_journal(&receipt_path,&transaction).map_err(|_|failure("outcome_unknown","staged bytes written but receipt was not committed",Some(json!({"operationId":request.operation_id}))))?;
            Ok(result)
        })
    }
    pub fn undo_publication(&self, request: &PublicationUndoRequest) -> Result<PublicationResult> {
        let original = self.locked(|| {
            if request.original_operation_id.is_empty() || request.original_operation_id.len() > 128
            {
                return Err(failure(
                    "invalid_request",
                    "originalOperationId is required",
                    None,
                ));
            }
            let data = optional_read(&self.receipt_path(&request.original_operation_id))?
                .ok_or_else(|| {
                    failure("not_found", "original publication journal is absent", None)
                })?;
            let receipt: Journal = serde_json::from_slice(&data)?;
            if receipt.state != "committed" || receipt.batch_result.is_none() {
                return Err(failure(
                    "invalid_request",
                    "original operation is not a committed multi-file publication",
                    None,
                ));
            }
            Ok(receipt)
        })?;
        let result = original.batch_result.unwrap();
        let revisions: BTreeMap<_, _> = result
            .assets
            .into_iter()
            .map(|asset| (asset.path, asset.revision))
            .collect();
        let mut publication = StagedPublicationRequest {
            operation_id: request.operation_id.clone(),
            inputs: vec![],
            outputs: vec![],
        };
        for file in original.files {
            if !file.existed {
                return Err(failure(
                    "unsupported_undo",
                    "new-file creation cannot be erased by publication undo",
                    None,
                ));
            }
            if file.path.ends_with(".meta") {
                continue;
            }
            let original_revision = revisions.get(&file.path).cloned().unwrap_or_default();
            if request
                .expected_revisions
                .get(&file.path)
                .map_or("", String::as_str)
                != original_revision
            {
                return Err(failure(
                    "revision_conflict",
                    "undo requires the exact original publication revisions",
                    Some(json!({"path":file.path,"originalRevision":original_revision})),
                ));
            }
            publication.inputs.push(PublicationInput {
                path: file.path.clone(),
                expected_revision: original_revision,
            });
            publication.outputs.push(PublicationOutput {
                path: file.path,
                content: String::from_utf8(file.before)
                    .map_err(|e| failure("invalid_request", e.to_string(), None))?,
                metadata: None,
            });
        }
        if publication.outputs.is_empty() {
            return Err(failure(
                "unsupported_undo",
                "original publication changed no existing source files",
                None,
            ));
        }
        self.publish_staged(&publication)
    }
}
