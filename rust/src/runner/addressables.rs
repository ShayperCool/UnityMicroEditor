//! The project's Addressables entries, read from its settings and groups the
//! way the Editor's "Use Asset Database" play mode does: an address, GUID or
//! label resolves to an authored source asset, with no catalog and no bundles.
//! A scene is written as a content scene of its own; any other asset (a
//! prefab, model, material, texture, clip, ScriptableObject and the like) is
//! written beside the session's scene.
use super::{os_error, read_file, wrap};
use crate::{sysutil, yaml::metadata_guid};
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use yaml_rust2::{
    parser::{Event, Parser},
    scanner::TScalarStyle,
};

/// The Editor build settings name the Addressables default object under this key.
const CONFIG_OBJECT: &str = "com.unity.addressableassets";

/// One addressable asset the raw loader serves from its source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AddressableRoute {
    pub guid: String,
    pub address: String,
    pub labels: Vec<String>,
    pub path: String,
    pub scene: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    /// The assets served from sources, ordered by path and address.
    pub entries: Vec<AddressableRoute>,
    /// Addressable sources no Player object is written for here: code, and
    /// shaders, which a session publishes on their own. They stay with the
    /// host's built content.
    pub unsupported: Vec<String>,
    /// The settings and group assets that were read.
    pub sources: Vec<String>,
}

/// Reads the Addressables groups of `project`. A project without Addressables
/// settings yields an empty discovery.
pub fn discover(project: &str) -> Result<Discovery> {
    let mut found = Discovery::default();
    let build_settings = sysutil::join(&[project, "ProjectSettings/EditorBuildSettings.asset"]);
    let Ok(data) = std::fs::read(&build_settings) else {
        return Ok(found);
    };
    let objects = first_object(&data).map_err(|e| wrap(&build_settings, e))?;
    let default_object = guid_of(field(
        as_map(field(&objects, "m_configObjects")),
        CONFIG_OBJECT,
    ));
    if default_object.is_empty() {
        return Ok(found);
    }
    let index = AssetIndex::read(project)?;
    let mut read = |guid: &str, what: &str| -> Result<Map<String, Value>> {
        let Some(path) = index.files.get(guid) else {
            bail!("Addressables {what} {guid} is not an asset of this project");
        };
        let data = read_file(&sysutil::join(&[project, path]))?;
        let object = first_object(&data).map_err(|e| wrap(path, e))?;
        found.sources.push(path.clone());
        Ok(object)
    };
    let settings_guid = text(field(
        &read(&default_object, "default object")?,
        "m_AddressableAssetSettingsGuid",
    ));
    let settings = read(&settings_guid, "settings")?;
    let mut declared = Vec::new();
    for group in list(field(&settings, "m_GroupAssets")) {
        let guid = guid_of(group);
        // A removed group leaves an empty reference behind.
        if guid.is_empty() || !index.files.contains_key(&guid) {
            continue;
        }
        let group = read(&guid, "group")?;
        for entry in list(field(&group, "m_SerializeEntries")) {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            declared.push(Declared {
                guid: text(field(entry, "m_GUID")),
                address: text(field(entry, "m_Address")),
                labels: list(field(entry, "m_SerializedLabels"))
                    .iter()
                    .map(text)
                    .collect(),
            });
        }
    }
    let explicit: BTreeSet<&str> = declared.iter().map(|entry| entry.guid.as_str()).collect();
    for entry in &declared {
        // Unity rejects such addresses: brackets select a sub-object.
        if entry.address.contains('[') && entry.address.contains(']') {
            continue;
        }
        if let Some(path) = index.files.get(&entry.guid) {
            found.add(entry, &entry.guid, &entry.address, path);
        } else if let Some(folder) = index.folders.get(&entry.guid) {
            // A folder entry makes every asset below it addressable, except
            // the ones that have an entry of their own.
            let prefix = format!("{folder}/");
            for (path, guid) in index.paths.range(prefix.clone()..) {
                if !path.starts_with(&prefix) {
                    break;
                }
                let hidden = path[prefix.len()..]
                    .split('/')
                    .any(|part| part == "Editor" || part == "Resources");
                if hidden || explicit.contains(guid.as_str()) {
                    continue;
                }
                let address = format!("{}{}", entry.address, &path[folder.len()..]);
                found.add(entry, guid, &address, path);
            }
        }
        // Built-in entries (Resources, the build scene list) name no asset.
    }
    found.entries.sort_by(|a, b| {
        (a.path.as_str(), a.address.as_str()).cmp(&(b.path.as_str(), b.address.as_str()))
    });
    found.entries.dedup();
    found.unsupported.sort();
    found.unsupported.dedup();
    Ok(found)
}

struct Declared {
    guid: String,
    address: String,
    labels: Vec<String>,
}

impl Discovery {
    fn add(&mut self, entry: &Declared, guid: &str, address: &str, path: &str) {
        let extension = sysutil::ext(path).to_ascii_lowercase();
        if matches!(
            extension.as_str(),
            ".cs"
                | ".asmdef"
                | ".asmref"
                | ".dll"
                | ".so"
                | ".rsp"
                | ".shader"
                | ".compute"
                | ".shadergraph"
                | ".shadersubgraph"
                | ".hlsl"
                | ".cginc"
                | ".raytrace"
        ) {
            self.unsupported.push(path.into());
            return;
        }
        let scene = extension == ".unity";
        self.entries.push(AddressableRoute {
            guid: guid.into(),
            address: address.into(),
            labels: entry.labels.clone(),
            path: path.into(),
            scene,
        });
    }
}

/// The GUIDs of the authored assets and folders under `Assets`.
#[derive(Default)]
struct AssetIndex {
    files: HashMap<String, String>,
    folders: HashMap<String, String>,
    paths: BTreeMap<String, String>,
}
impl AssetIndex {
    fn read(project: &str) -> Result<Self> {
        let mut index = Self::default();
        index.walk(&sysutil::join(&[project, "Assets"]), "Assets")?;
        Ok(index)
    }

    fn walk(&mut self, directory: &str, logical: &str) -> Result<()> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(os_error("open", directory, &error)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| os_error("read", directory, &e))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = format!("{directory}/{name}");
            let kind = entry
                .file_type()
                .map_err(|e| os_error("lstat", &path, &e))?;
            if kind.is_dir() {
                self.walk(&path, &format!("{logical}/{name}"))?;
                continue;
            }
            let Some(asset) = name.strip_suffix(".meta") else {
                continue;
            };
            let Ok(info) = std::fs::metadata(format!("{directory}/{asset}")) else {
                continue;
            };
            let Some(guid) = metadata_guid(&read_file(&path)?) else {
                continue;
            };
            let logical = format!("{logical}/{asset}");
            if info.is_dir() {
                self.folders.insert(guid, logical);
            } else {
                self.paths.insert(logical.clone(), guid.clone());
                self.files.insert(guid, logical);
            }
        }
        Ok(())
    }
}

/// The fields of the first object of a Unity text asset. Every scalar is kept
/// as its text: an address or GUID made of digits is not a number here.
fn first_object(data: &[u8]) -> Result<Map<String, Value>> {
    let text = std::str::from_utf8(data).map_err(|_| anyhow!("not a Unity text asset"))?;
    let mut lines = text.split_inclusive('\n');
    let mut body = String::new();
    let mut started = false;
    for line in &mut lines {
        if line.starts_with("--- ") {
            if started {
                break;
            }
            started = true;
        } else if started {
            body.push_str(line);
        }
    }
    let mut stack: Vec<(Value, Option<String>)> = Vec::new();
    let mut root = Value::Null;
    let mut parser = Parser::new_from_str(&body);
    loop {
        let (event, _) = parser.next_token().map_err(|e| anyhow!("yaml: {e}"))?;
        let value = match event {
            Event::StreamEnd | Event::DocumentEnd => break,
            Event::MappingStart(..) => {
                stack.push((Value::Object(Map::new()), None));
                continue;
            }
            Event::SequenceStart(..) => {
                stack.push((Value::Array(Vec::new()), None));
                continue;
            }
            Event::MappingEnd | Event::SequenceEnd => match stack.pop() {
                Some((value, _)) => value,
                None => bail!("yaml: unbalanced container"),
            },
            // An empty value is read as a plain `~`.
            Event::Scalar(text, TScalarStyle::Plain, ..) if text == "~" => Value::String("".into()),
            Event::Scalar(text, ..) => Value::String(text),
            _ => continue,
        };
        match stack.last_mut() {
            None => root = value,
            Some((Value::Array(items), _)) => items.push(value),
            Some((Value::Object(map), key)) => match (key.take(), value) {
                (Some(key), value) => {
                    map.insert(key, value);
                }
                (None, Value::String(text)) => *key = Some(text),
                (None, _) => bail!("yaml: non-scalar key"),
            },
            Some(_) => bail!("yaml: unbalanced container"),
        }
    }
    // The document is one `TypeName:` mapping around the fields.
    match root {
        Value::Object(types) => match types.into_iter().next() {
            Some((_, Value::Object(fields))) => Ok(fields),
            _ => Ok(Map::new()),
        },
        _ => Ok(Map::new()),
    }
}

static NULL: Value = Value::Null;
fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}
fn as_map(value: &Value) -> &Map<String, Value> {
    static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
    value.as_object().unwrap_or(&EMPTY)
}
fn list(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}
fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}
/// The GUID of a `{fileID, guid, type}` reference.
fn guid_of(reference: &Value) -> String {
    text(field(as_map(reference), "guid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(root: &str, path: &str, guid: &str, body: &str) {
        let full = format!("{root}/{path}");
        std::fs::create_dir_all(sysutil::dir(&full)).unwrap();
        std::fs::write(&full, body).unwrap();
        std::fs::write(
            format!("{full}.meta"),
            format!("fileFormatVersion: 2\nguid: {guid}\n"),
        )
        .unwrap();
    }
    fn folder(root: &str, path: &str, guid: &str) {
        let full = format!("{root}/{path}");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(
            format!("{full}.meta"),
            format!("fileFormatVersion: 2\nguid: {guid}\nfolderAsset: yes\n"),
        )
        .unwrap();
    }
    fn object(fields: &str) -> String {
        format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n{fields}"
        )
    }
    fn guid(digit: char) -> String {
        digit.to_string().repeat(32)
    }

    #[test]
    fn a_project_without_addressables_has_no_entries() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        assert_eq!(discover(root).unwrap(), Discovery::default());
        std::fs::create_dir_all(format!("{root}/ProjectSettings")).unwrap();
        std::fs::write(
            format!("{root}/ProjectSettings/EditorBuildSettings.asset"),
            "%YAML 1.1\n--- !u!1045 &1\nEditorBuildSettings:\n  m_Scenes: []\n  m_configObjects: {}\n",
        )
        .unwrap();
        assert_eq!(discover(root).unwrap(), Discovery::default());
    }

    #[test]
    fn groups_resolve_to_scenes_and_assets_but_not_code_or_shaders() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{root}/ProjectSettings")).unwrap();
        std::fs::write(
            format!("{root}/ProjectSettings/EditorBuildSettings.asset"),
            format!(
                "%YAML 1.1\n--- !u!1045 &1\nEditorBuildSettings:\n  m_configObjects:\n    {CONFIG_OBJECT}: {{fileID: 11400000, guid: {}, type: 2}}\n",
                guid('1')
            ),
        )
        .unwrap();
        let data = "Assets/AddressableAssetsData";
        asset(
            root,
            &format!("{data}/DefaultObject.asset"),
            &guid('1'),
            &object(&format!(
                "  m_AddressableAssetSettingsGuid: {}\n",
                guid('2')
            )),
        );
        asset(
            root,
            &format!("{data}/AddressableAssetSettings.asset"),
            &guid('2'),
            &object(&format!(
                "  m_GroupAssets:\n  - {{fileID: 11400000, guid: {}, type: 2}}\n  - {{fileID: 0}}\n",
                guid('3')
            )),
        );
        asset(
            root,
            &format!("{data}/AssetGroups/Main.asset"),
            &guid('3'),
            &object(&format!(
                "  m_SerializeEntries:\n  - m_GUID: {}\n    m_Address: Ship\n    m_SerializedLabels: []\n  - m_GUID: {}\n    m_Address: 42\n    m_SerializedLabels:\n    - actors\n  - m_GUID: {}\n    m_Address: Pack\n    m_SerializedLabels:\n    - pack\n  - m_GUID: {}\n    m_Address: icon\n    m_SerializedLabels: []\n  - m_GUID: Resources\n    m_Address: Resources\n    m_SerializedLabels: []\n",
                guid('a'),
                guid('b'),
                guid('c'),
                guid('d')
            )),
        );
        asset(root, "Assets/Scenes/Ship.unity", &guid('a'), "");
        asset(root, "Assets/Prefabs/Player.prefab", &guid('b'), "");
        asset(root, "Assets/Art/Icon.png", &guid('d'), "");
        folder(root, "Assets/Pack", &guid('c'));
        folder(root, "Assets/Pack/Editor", &guid('7'));
        asset(root, "Assets/Pack/Enemy.prefab", &guid('e'), "");
        asset(root, "Assets/Pack/Sub/Room.unity", &guid('f'), "");
        asset(root, "Assets/Pack/Sub/Note.txt", &guid('8'), "");
        asset(root, "Assets/Pack/Sub/Glow.shader", &guid('5'), "");
        asset(root, "Assets/Pack/Sub/Logic.cs", &guid('6'), "");
        asset(root, "Assets/Pack/Editor/Tool.prefab", &guid('9'), "");

        let found = discover(root).unwrap();
        let route = |g: char, address: &str, labels: &[&str], path: &str, scene| AddressableRoute {
            guid: guid(g),
            address: address.into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            path: path.into(),
            scene,
        };
        assert_eq!(
            found.entries,
            [
                route('d', "icon", &[], "Assets/Art/Icon.png", false),
                route(
                    'e',
                    "Pack/Enemy.prefab",
                    &["pack"],
                    "Assets/Pack/Enemy.prefab",
                    false
                ),
                route(
                    '8',
                    "Pack/Sub/Note.txt",
                    &["pack"],
                    "Assets/Pack/Sub/Note.txt",
                    false
                ),
                route(
                    'f',
                    "Pack/Sub/Room.unity",
                    &["pack"],
                    "Assets/Pack/Sub/Room.unity",
                    true
                ),
                route(
                    'b',
                    "42",
                    &["actors"],
                    "Assets/Prefabs/Player.prefab",
                    false
                ),
                route('a', "Ship", &[], "Assets/Scenes/Ship.unity", true),
            ]
        );
        assert_eq!(
            found.unsupported,
            ["Assets/Pack/Sub/Glow.shader", "Assets/Pack/Sub/Logic.cs"]
        );
        assert_eq!(
            found.sources,
            [
                format!("{data}/DefaultObject.asset"),
                format!("{data}/AddressableAssetSettings.asset"),
                format!("{data}/AssetGroups/Main.asset"),
            ]
        );
    }
}
