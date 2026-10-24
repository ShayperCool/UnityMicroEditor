//! Sources of a project's scripted importers (`ScriptedImporter`, the game's
//! Editor code) imported with no Editor: a Player of Play mode code runs the
//! importers (`Runtime/ScriptedImports.cs` of the bridge, asked by
//! `UME_SCRIPTED_IMPORTS`) and answers with the objects each adds, as their
//! serialized fields; the tool makes the documents of an asset of them, with
//! the file IDs the Editor gives them, which its writer writes as any
//! authored asset.
use crate::{
    cancel::Context,
    playerbake::{self, Sources},
    sysutil,
};
use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};
use ume_base::yaml::Document;

/// The keys of a `ScriptedImporter` entry of a `.meta` that are the
/// importer's own and not its settings.
const IMPORTER_KEYS: [&str; 7] = [
    "internalIDToNameTable",
    "externalObjects",
    "serializedVersion",
    "userData",
    "assetBundleName",
    "assetBundleVariant",
    "script",
];

/// A source a scripted importer imports: its GUID, where it lies, the
/// importer's class and its settings.
struct Pending {
    guid: String,
    path: String,
    importer: String,
    settings: Map<String, Value>,
}

/// The sources of `sources` a scripted importer imports.
fn pending(sources: &Sources) -> Vec<Pending> {
    let mut found = Vec::new();
    for (guid, path) in sources.all() {
        let Ok(meta) = std::fs::read(format!("{}.meta", path.display())) else {
            continue;
        };
        if !meta
            .windows(17)
            .any(|window| window == b"ScriptedImporter:")
        {
            continue;
        }
        // A `.meta` is plain YAML, without the headers of an asset's documents.
        let Some(Value::Object(importer)) =
            ume_base::yaml::parse_yaml_value(&String::from_utf8_lossy(&meta))
                .ok()
                .and_then(|meta| meta.get("ScriptedImporter").cloned())
        else {
            continue;
        };
        let Some(script) = importer
            .get("script")
            .and_then(|script| script.get("guid"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(class) = sources.script_class(script) else {
            continue;
        };
        let settings = importer
            .iter()
            .filter(|(key, _)| !IMPORTER_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        found.push(Pending {
            guid: guid.to_string(),
            path: path.to_string_lossy().into_owned(),
            importer: class,
            settings,
        });
    }
    found
}

/// A serialized value of `JsonUtility` as the lexical value of a text
/// asset: numbers and booleans as their text, a reference by instance ID as
/// a reference to the object of the asset the importer added under that ID.
fn lexical(value: &Value, references: &HashMap<String, i64>) -> Value {
    match value {
        Value::Bool(flag) => Value::String(if *flag { "1" } else { "0" }.into()),
        Value::Number(number) => Value::String(number.to_string()),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| lexical(item, references)).collect())
        }
        Value::Object(fields) => {
            if fields.len() == 1
                && let Some(id) = fields.get("instanceID")
            {
                let file_id = references.get(&id.to_string()).copied().unwrap_or(0);
                let mut reference = Map::new();
                reference.insert("fileID".into(), Value::String(file_id.to_string()));
                return Value::Object(reference);
            }
            Value::Object(
                fields
                    .iter()
                    .map(|(key, field)| (key.clone(), lexical(field, references)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// The documents of one source out of what its importer answered.
fn documents(sources: &Sources, answer: &Value) -> Result<Vec<Document>> {
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    let objects = answer
        .get("objects")
        .and_then(Value::as_array)
        .context("the importer answered no objects")?;
    let mut taken = HashSet::new();
    let mut ids: Vec<i64> = Vec::new();
    for object in objects {
        let identifier = object
            .get("identifier")
            .and_then(Value::as_str)
            .unwrap_or("");
        ids.push(playerbake::imported_file_id(
            "MonoBehaviour",
            identifier,
            &mut taken,
        ));
    }
    // An instance ID the importer answered, by the file ID of its object.
    let mut references: HashMap<String, i64> = HashMap::new();
    if let Some(Value::Object(by_instance)) = answer.get("references") {
        for (instance, identifier) in by_instance {
            if let Some(position) = objects
                .iter()
                .position(|object| object.get("identifier") == Some(identifier))
            {
                references.insert(instance.clone(), ids[position]);
            }
        }
    }
    let mut documents = Vec::new();
    for (object, file_id) in objects.iter().zip(ids) {
        let class = object.get("class").and_then(Value::as_str).unwrap_or("");
        let Some(script) = sources.script_guid(class) else {
            bail!("no script source of the imported class {class}");
        };
        let json: Value =
            serde_json::from_str(object.get("json").and_then(Value::as_str).unwrap_or("{}"))?;
        let mut data = Map::new();
        data.insert("m_ObjectHideFlags".into(), Value::String("0".into()));
        let mut script_reference = Map::new();
        script_reference.insert("fileID".into(), Value::String("11500000".into()));
        script_reference.insert("guid".into(), Value::String(script));
        script_reference.insert("type".into(), Value::String("3".into()));
        data.insert("m_Script".into(), Value::Object(script_reference));
        data.insert(
            "m_Name".into(),
            object
                .get("name")
                .cloned()
                .unwrap_or(Value::String(String::new())),
        );
        if let Value::Object(fields) = lexical(&json, &references) {
            data.extend(fields);
        }
        documents.push(Document {
            class_id: 114,
            file_id: file_id.to_string(),
            type_name: "MonoBehaviour".into(),
            data,
            stripped: false,
        });
    }
    Ok(documents)
}

/// The documents of each source imported, by GUID, and the sources that
/// failed with the reason.
pub type ScriptedImports = (HashMap<String, Vec<Document>>, Vec<(String, String)>);

/// Runs the scripted importers of the project's sources in `player`, a
/// Player of Play mode code, and gives the documents of each source they
/// import, and those that failed with the reason.
pub fn import_scripted(
    ctx: &Context,
    project: &str,
    player: &str,
    sources: &Sources,
    directory: &str,
) -> Result<ScriptedImports> {
    let wanted = pending(sources);
    let mut imported = HashMap::new();
    let mut refused = Vec::new();
    if wanted.is_empty() {
        return Ok((imported, refused));
    }
    sysutil::mkdir_all(directory)?;
    let request = sysutil::join(&[directory, "scripted-imports.json"]);
    let output = sysutil::join(&[directory, "scripted-imports.answer.json"]);
    let _ = std::fs::remove_file(&output);
    let imports: Vec<Value> = wanted
        .iter()
        .map(|pending| {
            // The importer reads its asset by its path from the project's
            // directory, where the Player runs; a package's lies elsewhere.
            let logical = Path::new(&pending.path)
                .strip_prefix(project)
                .map(|relative| relative.to_string_lossy().into_owned())
                .unwrap_or_default();
            let asset_path = if !logical.is_empty() && !logical.starts_with("Library/") {
                logical
            } else {
                pending.path.clone()
            };
            serde_json::json!({
                "guid": pending.guid,
                "assetPath": asset_path,
                "importer": pending.importer,
                "settings": pending.settings,
            })
        })
        .collect();
    std::fs::write(
        &request,
        serde_json::to_vec(&serde_json::json!({"output": output, "imports": imports}))?,
    )?;
    let log = sysutil::join(&[directory, "scripted-imports.log"]);
    let mut child = std::process::Command::new(player)
        .args(["-batchmode", "-nographics", "-logFile", &log])
        .current_dir(project)
        .env("UME_SCRIPTED_IMPORTS", &request)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("start {player} for the scripted importers"))?;
    let started = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if ctx.err().is_err() || started.elapsed() > Duration::from_secs(300) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the Player running the scripted importers did not finish; see {log}");
        }
        // The answer is complete once written; the Player may take long to quit.
        if std::fs::metadata(&output).is_ok_and(|info| info.len() > 0)
            && started.elapsed() > Duration::from_secs(2)
            && serde_json::from_slice::<Value>(&std::fs::read(&output).unwrap_or_default()).is_ok()
        {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let answer: Value = serde_json::from_slice(
        &std::fs::read(&output)
            .with_context(|| format!("the scripted importers answered nothing; see {log}"))?,
    )?;
    let answers = answer
        .get("imports")
        .and_then(Value::as_array)
        .context("the scripted importers' answer lists no imports")?;
    for answer in answers {
        let guid = answer
            .get("guid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match documents(sources, answer) {
            Ok(documents) if !guid.is_empty() => {
                if let Some(pending) = wanted.iter().find(|pending| pending.guid == guid) {
                    let _ = keep(project, pending, &documents);
                }
                imported.insert(guid, documents);
            }
            Ok(_) => refused.push((guid, "an answer without its source".into())),
            Err(error) => refused.push((guid, format!("{error:#}"))),
        }
    }
    Ok((imported, refused))
}

/// Where the documents a scripted importer made are kept for the sessions
/// of the project, which write the assets that name them.
fn kept_directory(project: &str) -> String {
    sysutil::join(&[project, "Library/UnityMicroEditor/scripted-imports"])
}

fn hash_of(path: &str) -> String {
    std::fs::read(path)
        .map(|data| crate::sha256(&data))
        .unwrap_or_default()
}

fn keep(project: &str, pending: &Pending, documents: &[Document]) -> Result<()> {
    let directory = kept_directory(project);
    sysutil::mkdir_all(&directory)?;
    let documents: Vec<Value> = documents
        .iter()
        .map(|document| {
            serde_json::json!({
                "classId": document.class_id,
                "fileId": document.file_id,
                "typeName": document.type_name,
                "data": document.data,
            })
        })
        .collect();
    let record = serde_json::json!({
        "guid": pending.guid,
        "path": pending.path,
        "sourceSha256": hash_of(&pending.path),
        "metaSha256": hash_of(&format!("{}.meta", pending.path)),
        "importer": pending.importer,
        "documents": documents,
    });
    let path = sysutil::join(&[&directory, &format!("{}.json", pending.guid)]);
    let partial = format!("{path}.{}.tmp", crate::util::random_id());
    std::fs::write(&partial, serde_json::to_vec(&record)?)?;
    std::fs::rename(&partial, &path)?;
    Ok(())
}

/// The documents scripted importers made of the project's sources that are
/// unchanged since, by GUID: what a session's writer writes such a source as.
/// A source imported by an importer edited since keeps its documents until
/// a preparation imports it again.
pub fn kept(project: &str) -> HashMap<String, Vec<Document>> {
    let mut found = HashMap::new();
    let Ok(entries) = std::fs::read_dir(kept_directory(project)) else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(record) = std::fs::read(entry.path())
            .map_err(anyhow::Error::from)
            .and_then(|data| Ok(serde_json::from_slice::<Value>(&data)?))
        else {
            continue;
        };
        let text = |key: &str| {
            record
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let path = text("path");
        if path.is_empty()
            || hash_of(&path) != text("sourceSha256")
            || hash_of(&format!("{path}.meta")) != text("metaSha256")
        {
            continue;
        }
        let documents: Vec<Document> = record
            .get("documents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|document| {
                Some(Document {
                    class_id: document.get("classId")?.as_i64()?,
                    file_id: document.get("fileId")?.as_str()?.to_string(),
                    type_name: document.get("typeName")?.as_str()?.to_string(),
                    data: document.get("data")?.as_object()?.clone(),
                    stripped: false,
                })
            })
            .collect();
        found.insert(text("guid"), documents);
    }
    found
}
