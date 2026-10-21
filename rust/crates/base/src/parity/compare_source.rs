//! `parity-compare-source` checks a complete authored hierarchy against the public Editor
//! `SerializedObject` readback captured by `SourceAuthoringReference.cs.txt`.
use crate::{
    ENGINE_VERSION,
    flags::FlagSet,
    project::{
        self, AuthoredIdentity, Map, Value,
        value::{self, sprint},
    },
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, HashMap, HashSet};

static NO_FIELDS: Map = Map::new();

/// A missing or `null` member is the type's empty value.
fn nul<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A JSON document of the harness, read leniently: unknown members are ignored.
macro_rules! document {
    ($(#[$meta:meta])* struct $name:ident { $($(#[$field_meta:meta])* $field:ident: $kind:ty,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, Deserialize)]
        #[serde(default, rename_all = "camelCase")]
        struct $name { $(#[serde(deserialize_with = "nul")] $(#[$field_meta])* $field: $kind,)* }
    };
}

document! {
    struct Component {
        file_id: String,
        class_id: i64,
        #[serde(rename = "type")]
        kind: String,
        /// Absent unless the hierarchy was read with `includeFields: true`.
        fields: Option<Map>,
    }
}
document! {
    struct Object {
        file_id: String,
        name: String,
        parent_id: String,
        active: bool,
        components: Vec<Component>,
    }
}
document! {
    /// A `scene_hierarchy` reply of our authoring tools.
    struct Hierarchy {
        objects: Vec<Object>,
        total: i64,
        next_offset: Option<i64>,
        provenance: HashMap<String, Provenance>,
    }
}
document! {
    struct Provenance {
        identities: Vec<AuthoredIdentity>,
    }
}
document! {
    /// One public `SerializedProperty`; vectors and colors are `|`-separated.
    struct Property {
        path: String,
        #[serde(rename = "type")]
        kind: String,
        value: String,
        reference_guid: String,
        reference_file_id: String,
    }
}
document! {
    struct EditorComponent {
        #[serde(rename = "type")]
        kind: String,
        source_file_id: String,
        prefab_source_guid: String,
        prefab_source_file_id: String,
        properties: Vec<Property>,
    }
}
document! {
    struct EditorObject {
        path: String,
        source_file_id: String,
        prefab_source_guid: String,
        prefab_source_file_id: String,
        active: bool,
        components: Vec<EditorComponent>,
    }
}
document! {
    struct EditorAsset {
        path: String,
        guid: String,
        file_id: String,
        #[serde(rename = "type")]
        kind: String,
        properties: Vec<Property>,
    }
}
document! {
    struct EditorReport {
        schema_version: i64,
        unity_version: String,
        scene_dirty: bool,
        objects: Vec<EditorObject>,
        assets: Vec<EditorAsset>,
    }
}
document! {
    /// The data of one `asset_read` reply.
    struct SourceAsset {
        path: String,
        guid: String,
        content: String,
        truncated: bool,
        next_offset: Option<i64>,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Comparison {
    success: bool,
    objects: usize,
    components: usize,
    assets: usize,
    prefab_bindings: usize,
    properties: usize,
    unrepresented_properties: usize,
    differences: Vec<String>,
}

/// Finds the hierarchy in a reply or one of its envelopes and requires it to be complete.
fn read_hierarchy(text: &str) -> Result<Hierarchy, String> {
    let envelope: BTreeMap<String, Box<RawValue>> = serde_json::from_str::<Option<_>>(text)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let member = |key: &str| envelope.get(key).map(|value| value.get());
    if member("success") == Some("false") {
        return Err("source operation reported failure".into());
    }
    if member("isError") == Some("true") || member("error").is_some_and(|error| error != "null") {
        return Err("source operation returned an error".into());
    }
    if envelope.contains_key("objects") {
        // Number literals stay as written: 64-bit identities and counters exceed f64.
        let hierarchy: Hierarchy =
            value::decode_use_number(text).map_err(|error| error.to_string())?;
        if hierarchy.next_offset.is_some() || hierarchy.total != hierarchy.objects.len() as i64 {
            return Err("source hierarchy is incomplete or paged".into());
        }
        let fieldless = |object: &Object| object.components.iter().any(|c| c.fields.is_none());
        if hierarchy.objects.iter().any(fieldless) {
            return Err("source comparison requires includeFields: true".into());
        }
        return Ok(hierarchy);
    }
    match ["result", "structuredContent", "data"]
        .into_iter()
        .find_map(member)
    {
        Some(nested) => read_hierarchy(nested),
        None => Err("no source hierarchy in reply".into()),
    }
}

/// Resolves a public `SerializedProperty` path in authored fields; `None` when the source
/// does not represent that property.
fn source_property(fields: &Map, path: &str) -> Option<Value> {
    enum Node<'a> {
        Fields(&'a Map),
        Value(&'a Value),
        /// The key of a single-entry mapping.
        Name(&'a str),
    }
    let path = path.replace(".Array.data[", "[");
    if let Some(array) = path.strip_suffix(".Array.size") {
        return match source_property(fields, array)? {
            Value::Array(items) => Some(Value::Int(items.len() as i64)),
            _ => None,
        };
    }
    let mut node = Node::Fields(fields);
    for part in path.split('.') {
        let (key, mut indexes) = part.split_at(part.find('[').unwrap_or(part.len()));
        if !key.is_empty() {
            let mapping = match node {
                Node::Fields(mapping) | Node::Value(Value::Object(mapping)) => mapping,
                _ => return None,
            };
            node = match mapping.get(key) {
                Some(value) => Node::Value(value),
                // Public Material SerializedProperty exposes Unity's YAML single-key
                // dictionary entries as first/second pairs.
                None if mapping.len() == 1 && path.starts_with("m_SavedProperties.m_") => {
                    let (name, entry) = mapping.iter().next()?;
                    match key {
                        "first" => Node::Name(name),
                        "second" => Node::Value(entry),
                        _ => return None,
                    }
                }
                None => return None,
            };
        }
        while !indexes.is_empty() {
            let close = indexes.find(']')?;
            let index: usize = indexes.strip_prefix('[')?[..close - 1].parse().ok()?;
            let Node::Value(Value::Array(items)) = node else {
                return None;
            };
            node = Node::Value(items.get(index)?);
            indexes = &indexes[close + 1..];
        }
    }
    Some(match node {
        Node::Fields(mapping) => Value::Object(mapping.clone()),
        Node::Value(value) => value.clone(),
        Node::Name(name) => Value::String(name.into()),
    })
}

/// A number or the text of one, as authored sources hold both.
fn numeric(value: &Value) -> Option<f64> {
    sprint(value).parse().ok()
}

fn equal_number(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
}

/// Whether an authored value is what the Editor read back. Integers compare exactly,
/// floats with a relative tolerance of `1e-6`.
fn property_equal(source: &Value, property: &Property) -> bool {
    match property.kind.as_str() {
        "Integer" | "Boolean" | "Enum" | "ArraySize" => {
            let literal = match source {
                Value::Bool(flag) => u8::from(*flag).to_string(),
                other => sprint(other),
            };
            match (literal.parse::<i64>(), property.value.parse::<i64>()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        }
        "Float" => match (numeric(source), property.value.parse::<f64>()) {
            (Some(a), Ok(b)) => equal_number(a, b),
            _ => false,
        },
        // Unity writes empty serialized string slots as an empty YAML scalar, which the
        // YAML reader represents as null. Public stringValue is "".
        "String" => match source {
            Value::Null => property.value.is_empty(),
            other => sprint(other) == property.value,
        },
        "Vector2" | "Vector3" | "Vector4" | "Quaternion" | "Color" => {
            let Value::Object(mapping) = source else {
                return false;
            };
            let keys = if property.kind == "Color" {
                ["r", "g", "b", "a"]
            } else {
                ["x", "y", "z", "w"]
            };
            let parts: Vec<&str> = property.value.split('|').collect();
            (2..=keys.len()).contains(&parts.len())
                && parts.iter().zip(keys).all(|(part, key)| {
                    match (numeric(value::get(mapping, key)), part.parse::<f64>()) {
                        (Some(a), Ok(b)) => equal_number(a, b),
                        _ => false,
                    }
                })
        }
        "ObjectReference" => {
            let Value::Object(mapping) = source else {
                return false;
            };
            let id = sprint(value::get(mapping, "fileID"));
            if id == "0" {
                return property.reference_file_id == "0";
            }
            if let Some(guid) = value::get(mapping, "guid")
                .as_str()
                .filter(|guid| !guid.is_empty())
            {
                return guid == property.reference_guid && id == property.reference_file_id;
            }
            // Expanded prefab composite IDs are runtime read identities. Their scene
            // ownership is checked through complete object/component paths.
            id.contains(':') || id == property.reference_file_id
        }
        _ => true,
    }
}

/// The `Parent/Child` path of an authored object.
fn path_of(
    objects: &HashMap<&str, &Object>,
    id: &str,
    seen: &mut HashSet<String>,
) -> Result<String, String> {
    let object = objects
        .get(id)
        .filter(|_| seen.insert(id.to_owned()))
        .ok_or("source parent absent or cyclic")?;
    if object.parent_id.is_empty() || object.parent_id == "0" {
        return Ok(object.name.clone());
    }
    let parent = path_of(objects, &object.parent_id, seen)?;
    Ok(format!("{parent}/{}", object.name))
}

impl Comparison {
    /// An object or component expanded from a prefab (a composite `instance:source`
    /// identity) must resolve to the prefab source the Editor reports.
    fn check_prefab_binding(
        &mut self,
        hierarchy: &Hierarchy,
        id: &str,
        guid: &str,
        file_id: &str,
        label: &str,
    ) {
        if !id.contains(':') {
            return;
        }
        let bound = hierarchy.provenance.get(id).is_some_and(|provenance| {
            provenance.identities.iter().any(|identity| {
                !guid.is_empty() && identity.guid == guid && identity.file_id == file_id
            })
        });
        if bound {
            self.prefab_bindings += 1;
        } else {
            self.differences
                .push(format!("{label} public prefab source GUID/fileID differs"));
        }
    }

    fn finish(&mut self) {
        self.differences.sort();
        self.success = self.differences.is_empty();
    }
}

fn compare(hierarchy: &Hierarchy, reference: &EditorReport) -> Comparison {
    let mut result = Comparison::default();
    if reference.schema_version != 1
        || reference.unity_version != ENGINE_VERSION
        || reference.scene_dirty
    {
        result
            .differences
            .push("reference must be a saved scene in the pinned Editor".into());
        return result;
    }
    let by_id: HashMap<&str, &Object> = hierarchy
        .objects
        .iter()
        .map(|object| (object.file_id.as_str(), object))
        .collect();
    let mut sources: BTreeMap<String, &Object> = BTreeMap::new();
    for object in &hierarchy.objects {
        match path_of(&by_id, &object.file_id, &mut HashSet::new()) {
            Ok(path) => {
                if sources.insert(path.clone(), object).is_some() {
                    result
                        .differences
                        .push(format!("ambiguous source path {path}"));
                }
            }
            Err(error) => result.differences.push(error),
        }
    }
    for public in &reference.objects {
        let path = &public.path;
        let Some(source) = sources.remove(path) else {
            result
                .differences
                .push(format!("Editor object absent from source: {path}"));
            continue;
        };
        result.objects += 1;
        result.check_prefab_binding(
            hierarchy,
            &source.file_id,
            &public.prefab_source_guid,
            &public.prefab_source_file_id,
            path,
        );
        if source.active != public.active {
            result
                .differences
                .push(format!("{path} active state differs"));
        }
        if !source.file_id.contains(':') && source.file_id != public.source_file_id {
            result
                .differences
                .push(format!("{path} ordinary GameObject fileID differs"));
        }
        if source.components.len() != public.components.len() {
            result
                .differences
                .push(format!("{path} component count differs"));
            continue;
        }
        for (actual, public) in source.components.iter().zip(&public.components) {
            let label = format!("{path}/{}", public.kind);
            result.components += 1;
            result.check_prefab_binding(
                hierarchy,
                &actual.file_id,
                &public.prefab_source_guid,
                &public.prefab_source_file_id,
                &label,
            );
            // A MonoBehaviour's public type is its script class, not its native class.
            if actual.class_id != 114 && public.kind != format!("UnityEngine.{}", actual.kind) {
                result.differences.push(format!(
                    "{path} component order/type differs: {} vs {}",
                    public.kind, actual.kind
                ));
                continue;
            }
            if !actual.file_id.contains(':') && actual.file_id != public.source_file_id {
                result
                    .differences
                    .push(format!("{label} ordinary component fileID differs"));
            }
            let fields = actual.fields.as_ref().unwrap_or(&NO_FIELDS);
            for property in &public.properties {
                let Some(value) = source_property(fields, &property.path) else {
                    result.unrepresented_properties += 1;
                    continue;
                };
                if property.kind == "ObjectReference"
                    && value
                        .as_object()
                        .is_some_and(|target| sprint(value::get(target, "fileID")).contains(':'))
                {
                    result.unrepresented_properties += 1;
                    continue;
                }
                result.properties += 1;
                if !property_equal(&value, property) {
                    result.differences.push(format!(
                        "{path}/{}.{} differs: source={} Editor={} ref={}/{}",
                        actual.kind,
                        property.path,
                        sprint(&value),
                        property.value,
                        property.reference_guid,
                        property.reference_file_id
                    ));
                }
            }
        }
    }
    for path in sources.keys() {
        result
            .differences
            .push(format!("source object absent from Editor: {path}"));
    }
    result.finish();
    result
}

/// Adds the serialized assets to a comparison. An error means the asset evidence is
/// incomplete, as opposed to different.
fn compare_assets(
    result: &mut Comparison,
    source: &[SourceAsset],
    reference: &[EditorAsset],
) -> Result<(), String> {
    let mut assets: HashMap<&str, &SourceAsset> = HashMap::new();
    for asset in source {
        if asset.truncated || asset.next_offset.is_some() || asset.path.is_empty() {
            return Err("asset source comparison requires complete asset_read pages".into());
        }
        if assets.insert(&asset.path, asset).is_some() {
            return Err("duplicate source asset path".into());
        }
    }
    for public in reference {
        let path = &public.path;
        let asset = assets
            .remove(path.as_str())
            .ok_or_else(|| format!("missing source asset {path}"))?;
        let documents = project::parse_unity_yaml(asset.content.as_bytes())
            .map_err(|error| format!("{path}: {error}"))?;
        let selected = documents
            .iter()
            .rfind(|document| document.file_id == public.file_id)
            .filter(|_| asset.guid == public.guid);
        let Some(selected) = selected else {
            result
                .differences
                .push(format!("{path} asset GUID/fileID differs"));
            continue;
        };
        result.assets += 1;
        if selected.class_id != 114 && public.kind != format!("UnityEngine.{}", selected.type_name)
        {
            result
                .differences
                .push(format!("{path} native asset type differs"));
        }
        for property in &public.properties {
            let Some(value) = source_property(selected.fields(), &property.path) else {
                result.unrepresented_properties += 1;
                continue;
            };
            result.properties += 1;
            if !property_equal(&value, property) {
                result.differences.push(format!(
                    "{path}.{} differs: source={} Editor={}",
                    property.path,
                    sprint(&value),
                    property.value
                ));
            }
        }
    }
    if !assets.is_empty() {
        return Err("source asset absent from Editor readback".into());
    }
    result.finish();
    Ok(())
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))
}

fn decode<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
    serde_json::from_str(&read(path)?).map_err(|error| format!("{path}: {error}"))
}

/// Whether source and Editor agree; an error means the evidence cannot be compared.
fn run(flags: &FlagSet) -> Result<bool, String> {
    let hierarchy = read_hierarchy(&read(&flags.get_string("source"))?)?;
    let reference: EditorReport = decode(&flags.get_string("editor"))?;
    let mut result = compare(&hierarchy, &reference);
    let assets_path = flags.get_string("assets");
    if !assets_path.is_empty() || !reference.assets.is_empty() {
        let assets: Vec<SourceAsset> = if assets_path.is_empty() {
            Vec::new()
        } else {
            decode::<Option<_>>(&assets_path)?.unwrap_or_default()
        };
        compare_assets(&mut result, &assets, &reference.assets)?;
    }
    super::emit(&flags.get_string("output"), &super::pretty(&result)?)
        .map_err(|error| error.to_string())?;
    Ok(result.success)
}

/// Exit status 0 when source and Editor agree, 1 otherwise.
pub fn main(args: Vec<String>) -> i32 {
    let mut flags = FlagSet::new("unity-micro-editor parity-compare-source");
    flags.string("source", "", "complete scene_hierarchy reply JSON");
    flags.string("editor", "", "public Editor source readback JSON");
    flags.string("output", "", "optional report path");
    flags.string(
        "assets",
        "",
        "optional JSON array of complete asset_read data objects",
    );
    if let Err(status) = super::parse(&mut flags, args) {
        return status;
    }
    match run(&flags) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("source and public Editor differ");
            1
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GUID: &str = "11111111111111111111111111111111";
    const OTHER_GUID: &str = "22222222222222222222222222222222";

    fn hierarchy(value: serde_json::Value) -> Hierarchy {
        read_hierarchy(&value.to_string()).unwrap()
    }

    fn from<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    fn property(kind: &str, value: &str) -> Property {
        Property {
            kind: kind.into(),
            value: value.into(),
            ..Property::default()
        }
    }

    fn source_value(json: &str) -> Value {
        value::parse_use_number(json).unwrap()
    }

    #[test]
    fn source_readback_detects_native_value_ownership_and_hierarchy_differences() {
        let source = hierarchy(json!({"total": 1, "objects": [{
            "fileId": "10", "name": "Box", "parentId": "0", "active": true,
            "components": [{"fileId": "11", "classId": 65, "type": "BoxCollider", "fields": {
                "m_GameObject": {"fileID": "10"}, "m_IsTrigger": 1, "m_Size": {"x": 3, "y": 1, "z": 1},
            }}],
        }]}));
        let mut reference: EditorReport = from(json!({
            "schemaVersion": 1, "unityVersion": "6000.6.3f1",
            "objects": [{"path": "Box", "sourceFileId": "10", "active": true, "components": [{
                "type": "UnityEngine.BoxCollider", "sourceFileId": "11", "properties": [
                    {"path": "m_GameObject", "type": "ObjectReference", "referenceFileId": "10"},
                    {"path": "m_IsTrigger", "type": "Boolean", "value": "1"},
                    {"path": "m_Size", "type": "Vector3", "value": "3|1|1"},
                ],
            }]}],
        }));
        let valid = compare(&source, &reference);
        assert!(
            valid.success && valid.properties == 3,
            "valid public readback differs: {valid:?}"
        );

        reference.objects[0].components[0].properties[2].value = "4|1|1".into();
        let wrong_size = compare(&source, &reference);
        assert!(!wrong_size.success);
        assert!(wrong_size.differences.join(" ").contains("m_Size"));
        reference.objects[0].components[0].properties[2].value = "3|1|1".into();

        reference.objects[0].components[0].properties[0].reference_file_id = "99".into();
        assert!(
            !compare(&source, &reference).success,
            "wrong component owner passed comparison"
        );
        reference.objects[0].components[0].properties[0].reference_file_id = "10".into();

        reference.objects[0].path = "Different/Box".into();
        assert!(
            !compare(&source, &reference).success,
            "different parent path passed comparison"
        );
    }

    #[test]
    fn source_readback_preserves_integer_precision() {
        let source = read_hierarchy(
            r#"{"objects":[{"fileId":"1","components":[{"fileId":"2","fields":{"counter":9007199254740993}}]}],"total":1}"#,
        )
        .unwrap();
        let fields = source.objects[0].components[0].fields.as_ref().unwrap();
        let value = &fields["counter"];
        assert_eq!(
            sprint(value),
            "9007199254740993",
            "integer rounded while reading MCP source"
        );
        assert!(property_equal(
            value,
            &property("Integer", "9007199254740993")
        ));
        assert!(!property_equal(
            value,
            &property("Integer", "9007199254740992")
        ));
        assert!(
            !property_equal(&Value::Int(1000000000), &property("Integer", "1000000001")),
            "unequal integer passed approximate float comparison"
        );
    }

    #[test]
    fn serialized_asset_readback_checks_identity_values_and_material_pairs() {
        let content = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!21 &2100000\nMaterial:\n  m_Name: Checker\n  m_SavedProperties:\n    m_Colors:\n    - _Color: {r: 0.2, g: 0.6, b: 0.9, a: 1}\n";
        let mut source: Vec<SourceAsset> =
            from(json!([{"path": "Assets/Checker.mat", "guid": GUID, "content": content}]));
        let mut public: Vec<EditorAsset> = from(json!([{
            "path": "Assets/Checker.mat", "guid": GUID, "fileId": "2100000", "type": "UnityEngine.Material",
            "properties": [
                {"path": "m_Name", "type": "String", "value": "Checker"},
                {"path": "m_SavedProperties.m_Colors.Array.data[0].first", "type": "String", "value": "_Color"},
                {"path": "m_SavedProperties.m_Colors.Array.data[0].second", "type": "Color", "value": "0.2|0.6|0.9|1"},
            ],
        }]));
        let mut result = Comparison::default();
        assert_eq!(compare_assets(&mut result, &source, &public), Ok(()));
        assert!(
            result.success && result.assets == 1 && result.properties == 3,
            "valid serialized material differs {result:?}"
        );

        public[0].properties[2].value = "0.2|0.7|0.9|1".into();
        let mut result = Comparison::default();
        assert_eq!(compare_assets(&mut result, &source, &public), Ok(()));
        assert!(!result.success, "incorrect public material color accepted");

        public[0].guid = OTHER_GUID.into();
        let mut result = Comparison::default();
        assert_eq!(compare_assets(&mut result, &source, &public), Ok(()));
        assert!(!result.success, "incorrect public asset GUID accepted");

        source[0].truncated = true;
        assert!(
            compare_assets(&mut result, &source, &public).is_err(),
            "truncated asset snapshot accepted"
        );
    }

    #[test]
    fn prefab_public_source_bindings_must_match_resolved_provenance() {
        let source = hierarchy(json!({
            "total": 1,
            "objects": [{"fileId": "500:100", "name": "Instance", "parentId": "0", "active": true, "components": [
                {"fileId": "500:101", "classId": 4, "type": "Transform", "fields": {}},
            ]}],
            "provenance": {
                "500:100": {"identities": [{"guid": GUID, "fileId": "100"}]},
                "500:101": {"identities": [{"guid": GUID, "fileId": "101"}]},
            },
        }));
        let mut public: EditorReport = from(json!({
            "schemaVersion": 1, "unityVersion": "6000.6.3f1",
            "objects": [{
                "path": "Instance", "active": true, "prefabSourceGuid": GUID, "prefabSourceFileId": "100",
                "components": [{"type": "UnityEngine.Transform", "prefabSourceGuid": GUID, "prefabSourceFileId": "101"}],
            }],
        }));
        let bound = compare(&source, &public);
        assert!(
            bound.success && bound.prefab_bindings == 2,
            "public source bindings differ {bound:?}"
        );
        public.objects[0].components[0].prefab_source_file_id = "999".into();
        assert!(
            !compare(&source, &public).success,
            "wrong public component prefab binding passed"
        );
        public.objects[0].components[0].prefab_source_file_id = "101".into();
        public.objects[0].prefab_source_guid = OTHER_GUID.into();
        assert!(
            !compare(&source, &public).success,
            "wrong public GameObject prefab GUID passed"
        );
    }

    #[test]
    fn source_readback_rejects_paged_or_fieldless_snapshots() {
        for data in [
            r#"{"objects":[],"total":1,"nextOffset":1}"#,
            r#"{"objects":[{"fileId":"1","components":[{"fileId":"2","fields":null}]}],"total":1}"#,
            r#"{"result":{"structuredContent":{"success":false,"error":{"code":"failed"}}}}"#,
        ] {
            assert!(
                read_hierarchy(data).is_err(),
                "incomplete/error snapshot accepted {data}"
            );
        }
        let Value::Object(fields) = source_value(r#"{"materials":[{"fileID":"5"}]}"#) else {
            unreachable!()
        };
        assert_eq!(
            source_property(&fields, "materials.Array.data[0].fileID"),
            Some(Value::String("5".into())),
            "public array path did not resolve"
        );
        assert_eq!(
            source_property(&fields, "materials.Array.size"),
            Some(Value::Int(1))
        );
        assert_eq!(source_property(&fields, "materials.Array.data[1]"), None);
        assert!(
            !property_equal(
                &source_value(r#"{"x":1,"y":2,"z":3,"w":4}"#),
                &property("Vector4", "1|2|3|4|5")
            ),
            "malformed vector reference accepted"
        );
        assert!(property_equal(&Value::Null, &property("String", "")));
        assert!(
            !property_equal(&Value::Null, &property("String", "nonempty")),
            "empty Unity serialized string normalization is incorrect"
        );
    }

    #[test]
    fn hierarchy_is_found_inside_reply_envelopes() {
        let reply = json!({"result": {"structuredContent": {"success": true, "data": {"objects": [], "total": 0}}}});
        assert!(read_hierarchy(&reply.to_string()).is_ok());
        assert!(read_hierarchy(r#"{"result":{"content":[]}}"#).is_err());
    }
}
