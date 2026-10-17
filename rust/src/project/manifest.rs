//! Runtime manifest objects, scene validation and the asset closure.
use super::{
    expand::{Expanded, builtin_guid, transform, visit_map, visit_references},
    imported_bindings::AssetPreparation,
    lexpath,
    session::{Inner, asset_key, needs_import},
    value::{self, Map, Value, array, compact_map, file_id, get, map_file_id, number, object, str},
    worklist::AssetReferenceWorklist,
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) fn supported_class(class: i64) -> bool {
    matches!(
        class,
        1 | 4
            | 20
            | 23
            | 33
            | 54
            | 65
            | 108
            | 114
            | 135
            | 136
            | 104
            | 64
            | 81
            | 82
            | 95
            | 120
            | 137
            | 143
            | 198
            | 199
            | 205
            | 215
            | 222
            | 223
            | 224
            | 225
            | 29
            | 157
            | 196
            | 1660057539
    )
}

pub(crate) fn reference_list(data: &Map) -> Value {
    let mut result = Vec::new();
    visit_map(data, "", &mut |property, reference| {
        result.push(Value::Object(value::map([
            ("property", property.into()),
            ("fileId", map_file_id(reference).as_ref().into()),
            ("guid", value::text(reference, "guid").as_ref().into()),
            ("type", Value::Int(number(get(reference, "type")))),
        ])));
    });
    Value::Array(result)
}

/// Compact JSON of the scalar view of `map`, without building a filtered copy.
/// `skip` names top-level keys left out of the scalar view.
fn encode_scalar_map(map: &Map, skip: &[&str], out: &mut String) -> Result<()> {
    if map.contains_key("fileID") {
        out.push_str("null");
        return Ok(());
    }
    out.push('{');
    let mut first = true;
    for (key, item) in map {
        if skip.contains(&key.as_str()) || item.is_reference() {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        value::encode_string(key, out);
        out.push(':');
        encode_scalar(item, out)?;
    }
    out.push('}');
    Ok(())
}
fn encode_scalar(value: &Value, out: &mut String) -> Result<()> {
    match value {
        Value::Object(map) => encode_scalar_map(map, &[], out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                encode_scalar(item, out)?;
            }
            out.push(']');
            Ok(())
        }
        other => other.encode(out),
    }
}

const CAMERA_ALIASES: [(&str, &str); 5] = [
    ("near clip plane", "nearClipPlane"),
    ("far clip plane", "farClipPlane"),
    ("field of view", "fieldOfView"),
    ("orthographic size", "orthographicSize"),
    ("m_projectionMatrixMode", "ProjectionMatrixMode"),
];
const COMPONENT_METADATA: [&str; 7] = [
    "m_ObjectHideFlags",
    "m_GameObject",
    "m_Enabled",
    "m_EditorHideFlags",
    "m_Script",
    "m_Name",
    "m_EditorClassIdentifier",
];

pub(crate) fn order_scene_roots(mut objects: Vec<Value>) -> Result<Vec<Value>> {
    let mut ranks: HashMap<String, usize> = HashMap::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut metadata = 0;
    for v in &objects {
        let o = object(v);
        if !transform(number(get(o, "classId"))) {
            continue;
        }
        let data = match value::parse_use_number(&value::text(o, "dataJson"))? {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => bail!("json: cannot unmarshal dataJson into value of type object"),
        };
        if file_id(get(&data, "m_Father")) == "0" {
            owners.insert(
                value::text(o, "fileId").into_owned(),
                file_id(get(&data, "m_GameObject")).into_owned(),
            );
        }
    }
    for v in &objects {
        let o = object(v);
        if number(get(o, "classId")) != 1660057539 {
            continue;
        }
        metadata += 1;
        let data = value::parse_use_number(&value::text(o, "dataJson")).unwrap_or_default();
        for (index, reference) in array(data.field("m_Roots")).iter().enumerate() {
            if !str(reference.field("guid")).is_empty() {
                bail!("SceneRoots requires local authored references");
            }
            let id = file_id(reference).into_owned();
            if ranks.contains_key(&id) {
                bail!("SceneRoots contains duplicate {id}");
            }
            if owners.get(&id).is_none_or(String::is_empty) {
                bail!("SceneRoots contains non-root {id}");
            }
            ranks.insert(id, index);
        }
    }
    if metadata > 1 {
        bail!("scene has more than one SceneRoots document");
    }
    if metadata == 1 && ranks.len() != owners.len() {
        bail!("SceneRoots must list every root Transform");
    }
    if ranks.is_empty() {
        return Ok(objects);
    }
    let owner_ranks: HashMap<&str, usize> = ranks
        .iter()
        .map(|(id, rank)| (owners[id].as_str(), *rank))
        .collect();
    let rank = |v: &Value| -> (usize, u8) {
        let o = object(v);
        let id = value::text(o, "fileId");
        let class = number(get(o, "classId"));
        if class == 1 {
            if let Some(r) = owner_ranks.get(id.as_ref()) {
                return (*r, 0);
            }
        } else if transform(class)
            && let Some(r) = ranks.get(id.as_ref())
        {
            return (*r, 1);
        }
        (ranks.len(), 2)
    };
    objects.sort_by_cached_key(rank);
    Ok(objects)
}

impl Inner {
    pub(crate) fn manifest_objects(&self, objects: &[Expanded]) -> Result<Vec<Value>> {
        let mut result = Vec::with_capacity(objects.len());
        for o in objects {
            let renamed: Map;
            let mut data = &o.data;
            if o.class_id == 20 {
                renamed = o
                    .data
                    .iter()
                    .map(|(key, item)| {
                        let name = CAMERA_ALIASES
                            .iter()
                            .find(|(from, _)| from == key)
                            .map_or(key.as_str(), |(_, to)| to);
                        (name.to_string(), item.clone())
                    })
                    .collect();
                data = &renamed;
            }
            let mut type_name = o.type_name.clone();
            let derived: Map;
            let mut skip: &[&str] = &[];
            if o.class_id == 114 {
                let script = object(get(data, "m_Script"));
                let guid = value::text(script, "guid");
                type_name = self.script_type(&guid).to_string();
                if guid == "0000000000000000e000000000000000" && map_file_id(script) == "19102" {
                    type_name = "UnityEngine.UIElements.UIDocument".into();
                }
                if type_name.is_empty() && !o.missing_script {
                    bail!(
                        "unresolved managed component type {guid} in {}",
                        o.source_path
                    );
                }
                if let Some(next) = super::netcode::derived_network_data(o, &type_name, data)? {
                    derived = next;
                    data = &derived;
                }
                skip = &COMPONENT_METADATA;
            }
            let full = compact_map(data)?;
            let mut scalars = String::new();
            encode_scalar_map(data, skip, &mut scalars)?;
            result.push(Value::Object(value::map([
                ("fileId", o.file_id.as_str().into()),
                ("classId", Value::Int(o.class_id)),
                (
                    "gameObjectId",
                    file_id(get(data, "m_GameObject")).as_ref().into(),
                ),
                ("sourcePath", o.source_path.as_str().into()),
                ("guid", o.guid.as_str().into()),
                ("typeName", type_name.into()),
                ("skipMissingScript", o.missing_script.into()),
                ("dataJson", full.into()),
                ("scalarDataJson", scalars.into()),
                ("references", reference_list(data)),
            ])));
        }
        Ok(result)
    }

    pub(crate) fn validate(&self, objects: &[Expanded]) -> Result<()> {
        let mut by_id: HashMap<&str, &Expanded> = HashMap::new();
        for o in objects {
            if by_id.insert(&o.file_id, o).is_some() {
                bail!("duplicate expanded identity {}", o.file_id);
            }
            if !supported_class(o.class_id) {
                bail!(
                    "class {} {} has no runtime adapter",
                    o.class_id,
                    o.type_name
                );
            }
        }
        for o in objects {
            let mut failure: Option<anyhow::Error> = None;
            visit_map(&o.data, "", &mut |property, reference| {
                if failure.is_some() {
                    return;
                }
                let (id, guid) = (map_file_id(reference), value::text(reference, "guid"));
                if id == "0" || builtin_guid(&guid) || o.missing_script && property == "m_Script" {
                    return;
                }
                if !guid.is_empty() {
                    if self.path_of(&guid).is_empty() {
                        failure = Some(anyhow::anyhow!(
                            "unresolved GUID {guid} at {}.{property}",
                            o.file_id
                        ));
                    }
                } else if !by_id.contains_key(id.as_ref()) {
                    failure = Some(anyhow::anyhow!(
                        "dangling scene reference {id} at {}.{property}",
                        o.file_id
                    ));
                }
            });
            if let Some(failure) = failure {
                return Err(failure);
            }
            if !matches!(o.class_id, 1 | 104 | 29 | 157 | 196 | 1660057539)
                && by_id
                    .get(file_id(o.get("m_GameObject")).as_ref())
                    .is_none_or(|owner| owner.class_id != 1)
            {
                bail!("component {} requires a GameObject", o.file_id);
            }
            if o.class_id == 1 {
                let mut transforms = 0;
                for v in array(o.get("m_Component")) {
                    let id = file_id(v.field("component"));
                    let Some(c) = by_id.get(id.as_ref()) else {
                        bail!("GameObject {} component {id} is missing", o.file_id);
                    };
                    if transform(c.class_id) {
                        transforms += 1;
                    }
                    if file_id(c.get("m_GameObject")) != o.file_id {
                        bail!("component {id} has a different GameObject owner");
                    }
                }
                if transforms != 1 {
                    bail!("GameObject {} requires exactly one Transform", o.file_id);
                }
            }
            if transform(o.class_id) {
                let mut seen: HashSet<&str> = HashSet::new();
                let mut current = Some(o);
                while let Some(node) = current {
                    if !seen.insert(&node.file_id) {
                        bail!("Transform cycle at {}", o.file_id);
                    }
                    let parent = file_id(node.get("m_Father"));
                    if parent == "0" {
                        break;
                    }
                    current = by_id.get(parent.as_ref()).copied();
                    if current.is_some_and(|c| !transform(c.class_id)) {
                        bail!("Transform {} parent is not a Transform", o.file_id);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_assets(
        &mut self,
        scene_objects: &[Expanded],
    ) -> Result<AssetPreparation> {
        let mut bindings = self.imported_binding_closure();
        self.authored_scriptables.clear();
        let mut assets: BTreeMap<String, Map> = self.assets.clone();
        let catalog_assets: HashMap<String, Map> = self
            .catalog
            .assets
            .iter()
            .map(|a| {
                (
                    asset_key(&value::text(a, "guid"), &value::text(a, "fileId")),
                    a.clone(),
                )
            })
            .collect();
        let mut pending = AssetReferenceWorklist::new();
        for collection in ["preloadedAssets", "renderPipelineGlobalSettings"] {
            for item in array(get(&self.baseline, collection)) {
                pending.add_identity(&str(item.field("guid")), &str(item.field("fileId")))?;
            }
        }
        pending.add_identity(
            &self.baseline_text("renderPipelineGuid"),
            &self.baseline_text("renderPipelineFileId"),
        )?;
        for path in self.extra_assets.clone() {
            let guid = self.guid_of(&path).to_string();
            if guid.is_empty() {
                bail!("runtime-addressed prefab has no source GUID: {path}");
            }
            let (objects, _) = self.expand(&path, "", &[])?;
            let mut root = String::new();
            for o in &objects {
                if transform(o.class_id) && file_id(o.get("m_Father")) == "0" {
                    if !root.is_empty() {
                        bail!("runtime-addressed prefab needs exactly one root: {path}");
                    }
                    root = file_id(o.get("m_GameObject")).into_owned();
                }
            }
            if root.is_empty() || root == "0" {
                bail!("runtime-addressed prefab has no root: {path}");
            }
            pending.add_identity(&guid, &root)?;
        }
        for o in scene_objects {
            let mut failure = Ok(());
            let mut add = |_: &str, reference: &Map| {
                if failure.is_ok() {
                    failure = pending.add(reference);
                }
            };
            if o.missing_script && !o.data.contains_key("fileID") {
                // A missing script's own MonoScript pointer is not a dependency:
                // Exactly the top-level `m_Script` key is dropped before visiting.
                for (key, item) in &o.data {
                    if key != "m_Script" {
                        visit_references(item, key, &mut add);
                    }
                }
            } else {
                visit_map(&o.data, "", &mut add);
            }
            failure?;
        }
        let initial: Vec<String> = assets.keys().cloned().collect();
        for key in initial {
            let Some(mut a) = assets.remove(&key) else {
                continue;
            };
            if value::text(&a, "kind") == "imported"
                && self.authored_scriptable(&value::text(&a, "path"))?
            {
                // Found again below, from its authored text.
                pending.add_identity(&value::text(&a, "guid"), &value::text(&a, "fileId"))?;
                continue;
            }
            self.refresh_asset(&mut a)?;
            let current = asset_key(&value::text(&a, "guid"), &value::text(&a, "fileId"));
            if current != key && assets.contains_key(&current) {
                bail!("refreshed asset identity collides with another descriptor: {current}");
            }
            bindings.queue(self, &a, &mut pending)?;
            self.rebind_authored_scriptables(&a, &mut bindings, &mut pending)?;
            let imported = self.imported_source_references(&a, &catalog_assets)?;
            pending.add_all(&imported)?;
            queue_descriptor_references(&a, &mut pending)?;
            assets.insert(current, a);
        }
        while let Some(reference) = pending.pop() {
            let (guid, id) = (reference.guid, reference.file_id);
            let key = asset_key(&guid, &id);
            if assets.contains_key(&key) {
                continue;
            }
            if assets
                .values()
                .any(|a| value::text(a, "guid") == guid && prefab_has_object(a, &id))
            {
                continue;
            }
            let native = match catalog_assets.get(&key) {
                Some(native) if self.authored_scriptable(&value::text(native, "path"))? => None,
                other => other,
            };
            if let Some(native) = native {
                let mut a = native.clone();
                a.insert("kind".into(), "imported".into());
                let path = value::text(&a, "path").into_owned();
                assets.insert(key.clone(), a.clone());
                self.depend(&path)?;
                bindings.queue(self, &a, &mut pending)?;
                self.rebind_authored_scriptables(&a, &mut bindings, &mut pending)?;
                let imported = self.imported_source_references(&a, &catalog_assets)?;
                pending.add_all(&imported)?;
                continue;
            }
            let path = self.path_of(&guid).to_string();
            if path.is_empty() {
                // No asset of the project or its packages has this GUID:
                // Unity reads such a reference as missing, and so does the
                // Player here.
                continue;
            }
            // The authored text keeps fields that exist in the Editor only,
            // such as those of a pipeline's global settings that name its
            // Editor tooling in Editor folders. The Player's serialization
            // has no such field, so the reference is not followed.
            if path.split('/').any(|part| part == "Editor") {
                continue;
            }
            let extension = lexpath::lower_ext(&path);
            for existing in assets.values() {
                let kind = value::text(existing, "kind");
                if value::text(existing, "guid") == guid
                    && kind != "imported"
                    && !(extension == ".asset" && kind == "scriptableObject")
                {
                    bail!("raw asset {path} has no object {id}");
                }
            }
            let mut a = value::map([
                ("guid", guid.as_str().into()),
                ("fileId", id.as_str().into()),
                ("path", path.as_str().into()),
                ("name", lexpath::stem(&path).into()),
                ("references", Value::Array(Vec::new())),
                ("dataJson", "{}".into()),
                ("importerJson", "{}".into()),
            ]);
            match extension.as_str() {
                ".cs" => {
                    let type_name = self.script_type(&guid).to_string();
                    if type_name.is_empty() {
                        bail!("new script {path} requires compilation type metadata");
                    }
                    a.insert("kind".into(), "script".into());
                    a.insert("classId".into(), Value::Int(115));
                    a.insert("typeName".into(), type_name.into());
                    a.insert("executionOrder".into(), Value::Int(0));
                }
                ".prefab" => {
                    a.insert("kind".into(), "prefab".into());
                }
                ".asset" => {
                    a.insert("kind".into(), "scriptableObject".into());
                }
                ".mat" => {
                    a.insert("kind".into(), "authoredMaterial".into());
                }
                ".png" | ".jpg" | ".jpeg" => {
                    a.insert("kind".into(), "texture".into());
                }
                ".cubemap" => {
                    a.insert("kind".into(), "cubemap".into());
                }
                _ => return Err(needs_import(&path)),
            }
            self.refresh_asset(&mut a)?;
            let valid = value::text(&a, "fileId") == id
                || value::text(&a, "kind") == "prefab" && prefab_objects_contain(&a, &id);
            if !valid {
                bail!("raw asset {path} has no object {id}");
            }
            queue_descriptor_references(&a, &mut pending)?;
            assets.insert(asset_key(&guid, &value::text(&a, "fileId")), a);
        }
        let imported_bindings = bindings.result(&assets)?;
        let mut list: Vec<Map> = assets.into_values().collect();
        list.sort_by(|a, b| {
            (value::text(a, "guid"), value::text(a, "fileId"))
                .cmp(&(value::text(b, "guid"), value::text(b, "fileId")))
        });
        Ok(AssetPreparation {
            assets: list.into_iter().map(Value::Object).collect(),
            imported_bindings,
        })
    }

    fn rebind_authored_scriptables(
        &mut self,
        owner: &Map,
        bindings: &mut super::imported_bindings::ImportedBindingClosure,
        pending: &mut AssetReferenceWorklist,
    ) -> Result<()> {
        let guid = value::text(owner, "guid");
        for pointer in self.authored_scriptable_references(owner)? {
            bindings.rebind(
                (&guid, &pointer.owner),
                &pointer.property,
                (&pointer.target, &pointer.target_id, &pointer.target_path),
            )?;
            // The Player resolves both ends by their published identities.
            pending.add_identity(&guid, &pointer.owner)?;
            pending.add_identity(&pointer.target, &pointer.target_id)?;
        }
        Ok(())
    }

    /// Runtime prefab descriptors and custom ScriptableObjects remain raw and are
    /// refreshed as authored YAML. Imported descriptors remain immutable.
    pub(crate) fn refresh_asset(&mut self, a: &mut Map) -> Result<()> {
        let path = value::text(a, "path").into_owned();
        let kind = value::text(a, "kind").into_owned();
        match kind.as_str() {
            "authoredMaterial" => self.refresh_authored_material(a),
            "texture" => self.refresh_authored_texture(a),
            "cubemap" => self.refresh_authored_cubemap(a),
            "script" => {
                let type_name = self.script_type(&value::text(a, "guid")).to_string();
                if !type_name.is_empty() {
                    a.insert("typeName".into(), type_name.into());
                }
                Ok(())
            }
            "prefab" => {
                let (objects, aliases) = self.expand(&path, "", &[])?;
                self.validate(&objects)?;
                let roots: Vec<&Expanded> = objects
                    .iter()
                    .filter(|o| transform(o.class_id) && file_id(o.get("m_Father")) == "0")
                    .collect();
                if roots.len() != 1 {
                    bail!("runtime prefab {path} needs one root");
                }
                let root = file_id(roots[0].get("m_GameObject")).into_owned();
                a.insert("fileId".into(), root.as_str().into());
                a.insert("rootFileId".into(), root.into());
                a.insert("classId".into(), Value::Int(1));
                let converted = self.manifest_objects(&objects)?;
                a.insert("objects".into(), Value::Array(converted));
                let out = aliases
                    .iter()
                    .map(|(file, target)| {
                        Value::Object(value::map([
                            ("fileId", file.as_str().into()),
                            ("objectId", target.as_str().into()),
                        ]))
                    })
                    .collect();
                a.insert("objectAliases".into(), Value::Array(out));
                Ok(())
            }
            "scriptableObject" | "mesh" => {
                let physical = self.source(&path)?;
                self.depend(&path)?;
                let documents = self.parse(&physical)?;
                if documents.len() == 1 && documents[0].class_id == 43 {
                    let data = documents[0].fields();
                    super::mesh::validate_authored_mesh(data)
                        .map_err(|e| e.context(format!("raw Mesh {path}")))?;
                    a.insert("kind".into(), "mesh".into());
                    a.insert("classId".into(), Value::Int(43));
                    a.insert("typeName".into(), "UnityEngine.Mesh".into());
                    a.insert("fileId".into(), documents[0].file_id.as_str().into());
                    a.insert("name".into(), value::text(data, "m_Name").as_ref().into());
                    let encoded = compact_map(data);
                    a.insert(
                        "dataJson".into(),
                        encoded.as_deref().unwrap_or_default().into(),
                    );
                    a.insert("references".into(), reference_list(data));
                    return encoded.map(|_| ());
                }
                if kind == "mesh" {
                    bail!("raw Mesh {path} changed its source object type");
                }
                self.refresh_scriptable_object(a, &documents)
            }
            _ => Ok(()),
        }
    }
}

fn prefab_objects_contain(asset: &Map, id: &str) -> bool {
    array(get(asset, "objectAliases"))
        .iter()
        .chain(array(get(asset, "objects")))
        .any(|item| str(item.field("fileId")) == id)
}
fn prefab_has_object(asset: &Map, id: &str) -> bool {
    value::text(asset, "kind") == "prefab" && prefab_objects_contain(asset, id)
}

fn queue_descriptor_references(a: &Map, pending: &mut AssetReferenceWorklist) -> Result<()> {
    for reference in array(get(a, "references")) {
        pending.add_identity(
            &str(reference.field("guid")),
            &str(reference.field("fileId")),
        )?;
    }
    for v in array(get(a, "objects")) {
        let skip = v.field("skipMissingScript").is_true();
        for reference in array(v.field("references")) {
            if skip && str(reference.field("property")) == "m_Script" {
                continue;
            }
            pending.add_identity(
                &str(reference.field("guid")),
                &str(reference.field("fileId")),
            )?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn references_of(value: &Value) -> Vec<Map> {
    let mut result = Vec::new();
    visit_references(value, "", &mut |_, reference| {
        result.push(reference.clone())
    });
    result
}
