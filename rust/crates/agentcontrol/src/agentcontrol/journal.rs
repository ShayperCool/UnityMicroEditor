//! Durable request records and receipts under `<output>/.agent-requests`.
use super::{
    CONTROL_ARTIFACT_MAXIMUM, Error, RequestRecord, Result, allowed_operation,
    canonical_destination, open_owned_directory, owned_path, publish_root_bounded_immutable,
    read_root_bounded,
};
use crate::{
    jsondoc::{self, Json, Member, Target},
    sysutil::{self, Root, valid_id},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Arc};

const REQUEST_JOURNAL_LIMIT: i64 = 256 * 1024;
const RECORD_SUFFIX: &str = ".request-record.json";

fn root(output: &str, create: bool) -> Result<Root> {
    let directory = sysutil::join(&[output, ".agent-requests"]);
    canonical_destination(&directory)?;
    if create {
        sysutil::mkdir_all(&directory)?;
    }
    open_owned_directory(output, &directory)
}

// A journal record is a JSON object or `null`; `None` for any error.
fn object(data: &[u8]) -> Option<Json> {
    jsondoc::parse(data)
        .ok()
        .filter(|json| matches!(json, Json::Null | Json::Object(_)))
}
fn members(json: &Json) -> &[Member] {
    json.members().unwrap_or(&[])
}

impl RequestRecord {
    /// Encoded in the persisted format: capitalized field names, base64 byte fields.
    pub(super) fn encode(&self) -> Vec<u8> {
        format!(
            "{{\"ID\":{},\"Operation\":{},\"SessionID\":{},\"Directory\":{},\"Revision\":{},\"Hash\":{},\"PID\":{},\"Arguments\":\"{}\",\"Envelope\":\"{}\"}}",
            jsondoc::quote(&self.id),
            jsondoc::quote(&self.operation),
            jsondoc::quote(&self.session_id),
            jsondoc::quote(&self.directory),
            jsondoc::quote(&self.revision),
            jsondoc::quote(&self.hash),
            self.pid,
            STANDARD.encode(&self.arguments),
            STANDARD.encode(&self.envelope),
        )
        .into_bytes()
    }
    pub(super) fn decode(data: &[u8]) -> Option<Self> {
        const FIELDS: [&str; 9] = [
            "ID",
            "Operation",
            "SessionID",
            "Directory",
            "Revision",
            "Hash",
            "PID",
            "Arguments",
            "Envelope",
        ];
        let json = object(data)?;
        let mut record = RequestRecord::default();
        let (mut arguments, mut envelope) = (None, None);
        let at = Target::VALUE;
        jsondoc::decode_struct(members(&json), &FIELDS, |field, member| {
            let value = &member.value;
            match field {
                0 => jsondoc::set_string(&mut record.id, value, at),
                1 => jsondoc::set_string(&mut record.operation, value, at),
                2 => jsondoc::set_string(&mut record.session_id, value, at),
                3 => jsondoc::set_string(&mut record.directory, value, at),
                4 => jsondoc::set_string(&mut record.revision, value, at),
                5 => jsondoc::set_string(&mut record.hash, value, at),
                6 => jsondoc::set_int(&mut record.pid, value, at),
                7 => jsondoc::set_bytes(&mut arguments, value, at),
                _ => jsondoc::set_bytes(&mut envelope, value, at),
            }
        })
        .ok()?;
        record.arguments = arguments.unwrap_or_default();
        record.envelope = envelope.unwrap_or_default();
        Some(record)
    }
}

pub(super) fn load(output: &str) -> Result<HashMap<String, Arc<RequestRecord>>> {
    let mut requests = HashMap::new();
    let root = match root(output, false) {
        Ok(root) => root,
        Err(error) if error.is_not_exist() => return Ok(requests),
        Err(error) => return Err(error),
    };
    for name in root.names()? {
        if !name.ends_with(RECORD_SUFFIX) {
            continue;
        }
        if requests.len() >= 4096 {
            return Err(Error::new(
                "persistent runtime request journal exceeds 4096 entries",
            ));
        }
        let data = read_root_bounded(&root, &name, REQUEST_JOURNAL_LIMIT)?;
        let record = RequestRecord::decode(&data)
            .filter(|record| name == format!("{}{RECORD_SUFFIX}", record.id));
        let Some(record) = record else {
            return Err(Error::new(
                "persistent runtime request journal has an invalid record",
            ));
        };
        validate(output, &record)?;
        requests.insert(record.id.clone(), Arc::new(record));
    }
    Ok(requests)
}

pub(super) fn validate(output: &str, record: &RequestRecord) -> Result<()> {
    if !valid_id(&record.id)
        || !valid_id(&record.session_id)
        || !valid_id(&record.revision)
        || record.pid < 1
        || !allowed_operation(&record.operation)
        || !owned_path(output, &record.directory)
        || record.envelope.len() > 65536
    {
        return Err(Error::new(
            "persistent runtime request record has an invalid identity or ownership",
        ));
    }
    canonical_destination(&record.directory)?;
    if record.hash != crate::sha256(&record.envelope) {
        return Err(Error::new(
            "persistent runtime request envelope hash differs",
        ));
    }
    const FIELDS: [&str; 6] = [
        "schemaVersion",
        "requestId",
        "expectedPid",
        "expectedRevision",
        "operation",
        "arguments",
    ];
    let (mut schema, mut pid) = (0, 0);
    let (mut id, mut revision, mut operation) = (String::new(), String::new(), String::new());
    let mut arguments: &[u8] = &[];
    let at = Target::VALUE;
    let agrees = object(&record.envelope).is_some_and(|json| {
        jsondoc::decode_struct(members(&json), &FIELDS, |field, member| {
            let value = &member.value;
            match field {
                0 => jsondoc::set_int(&mut schema, value, at),
                1 => jsondoc::set_string(&mut id, value, at),
                2 => jsondoc::set_int(&mut pid, value, at),
                3 => jsondoc::set_string(&mut revision, value, at),
                4 => jsondoc::set_string(&mut operation, value, at),
                _ => {
                    arguments = member.raw(&record.envelope);
                    Ok(())
                }
            }
        })
        .is_ok()
    });
    if !agrees
        || schema != 1
        || id != record.id
        || pid != record.pid
        || revision != record.revision
        || operation != record.operation
        || arguments != record.arguments
    {
        return Err(Error::new(
            "persistent runtime request envelope disagrees with its record",
        ));
    }
    Ok(())
}

pub(super) fn persist_request(output: &str, record: &RequestRecord) -> Result<()> {
    validate(output, record)?;
    let root = root(output, true)?;
    publish_root_bounded_immutable(
        &root,
        &format!("{}{RECORD_SUFFIX}", record.id),
        &record.encode(),
        REQUEST_JOURNAL_LIMIT,
    )?;
    root.sync()
}

pub(super) fn read_receipt(output: &str, record: &RequestRecord) -> Result<Vec<u8>> {
    let root = root(output, false)?;
    read_root_bounded(
        &root,
        &format!("{}.receipt.json", record.id),
        CONTROL_ARTIFACT_MAXIMUM,
    )
}

pub(super) fn persist_receipt(output: &str, record: &RequestRecord, data: &[u8]) -> Result<()> {
    let root = root(output, true)?;
    publish_root_bounded_immutable(
        &root,
        &format!("{}.receipt.json", record.id),
        data,
        CONTROL_ARTIFACT_MAXIMUM,
    )?;
    root.sync()
}

pub(super) fn decode_receipt(
    data: &[u8],
    record: &RequestRecord,
    operation: &str,
) -> Result<Value> {
    const FIELDS: [&str; 8] = [
        "schemaVersion",
        "requestId",
        "requestSha256",
        "pid",
        "success",
        "revision",
        "result",
        "error",
    ];
    let (mut schema, mut pid) = (0, 0);
    let (mut id, mut hash, mut revision) = (String::new(), String::new(), String::new());
    let mut success: Option<bool> = None;
    let (mut result, mut failure) = (Value::Null, Value::Null);
    let at = Target::VALUE;
    let decoded = object(data).is_some_and(|json| {
        jsondoc::decode_struct(members(&json), &FIELDS, |field, member| {
            let value = &member.value;
            match field {
                0 => jsondoc::set_int(&mut schema, value, at),
                1 => jsondoc::set_string(&mut id, value, at),
                2 => jsondoc::set_string(&mut hash, value, at),
                3 => jsondoc::set_int(&mut pid, value, at),
                4 => match value {
                    Json::Null => {
                        success = None;
                        Ok(())
                    }
                    Json::Bool(flag) => {
                        success = Some(*flag);
                        Ok(())
                    }
                    other => Err(at.mismatch(other.kind(), "bool")),
                },
                5 => jsondoc::set_string(&mut revision, value, at),
                6 => jsondoc::to_value(value, true).map(|value| result = value),
                _ => jsondoc::to_value(value, true).map(|value| failure = value),
            }
        })
        .is_ok()
    });
    let compatible = decoded
        && schema == 1
        && id == record.id
        && hash == record.hash
        && pid == record.pid
        && valid_id(&revision)
        && match success {
            None => false,
            Some(true) => operation == "status" || revision == record.revision,
            Some(false) => !failure.is_null(),
        };
    if !compatible {
        return Err(Error::new(format!(
            "Player returned an incompatible response identity for {}",
            record.id
        )));
    }
    // Inserted as they are: `json!` would re-serialize the decoded numbers and
    // renormalize their text.
    let mut reply = Map::new();
    reply.insert("requestId".into(), json!(record.id));
    reply.insert("pid".into(), json!(pid));
    reply.insert("revision".into(), json!(revision));
    reply.insert("success".into(), json!(success));
    reply.insert("result".into(), result);
    reply.insert("error".into(), failure);
    Ok(Value::Object(reply))
}
