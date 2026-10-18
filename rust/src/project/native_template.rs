//! Native components the raw loader has no adapter for.
//!
//! The raw loader sets the fields of an engine component through its public
//! properties, one adapter per class. A Player cannot apply serialized data
//! to an engine object any other way: `JsonUtility` refuses engine types
//! outside the Editor. Any other native component is therefore built by
//! Unity's own code. The components of such a GameObject, as authored, go
//! into a template prefab that holds only that GameObject; the import worker
//! writes the template's Player objects into a ContentFile (see
//! `native::native_template`), and the Player makes the scene's GameObject
//! as a copy of the template, then adds what the loader adapts.
//!
//! A template holds nothing outside its GameObject. A templated component
//! that refers to another object of the scene, or to an asset, is refused
//! with the property that does.
use super::{
    expand::Expanded,
    manifest::supported_class,
    value::{self, Map, Value},
};
use crate::sha256;
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

/// Where the worker's import context holds the templates.
pub const TEMPLATE_FOLDER: &str = "Assets/UnityMicroEditorWorker";

/// The template of one GameObject: its prefab text and the local IDs of
/// its objects, all named by what the template holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTemplate {
    pub key: String,
    pub prefab: String,
    pub game_object: i64,
    /// The templated components in the order the template holds them,
    /// after its Transform: local ID, class and type name.
    pub components: Vec<(i64, i64, String)>,
}

impl NativeTemplate {
    /// The template's prefab in the worker's import context.
    pub fn path(&self) -> String {
        format!("{TEMPLATE_FOLDER}/template-{}.prefab", &self.key[..32])
    }

    /// The `.meta` of that prefab.
    pub fn meta(&self) -> String {
        let guid = &sha256(format!("ume-native-template\0{}", self.key).as_bytes())[..32];
        format!(
            "fileFormatVersion: 2\nguid: {guid}\nPrefabImporter:\n  externalObjects: {{}}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
        )
    }
}

/// The templates a preparation needs that the session does not hold yet.
#[derive(Debug)]
pub struct NeedsNativeTemplates(pub Vec<NativeTemplate>);

impl std::fmt::Display for NeedsNativeTemplates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} native component templates are not built",
            self.0.len()
        )
    }
}
impl std::error::Error for NeedsNativeTemplates {}

/// A templated component that refers to something outside its GameObject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedNativeReference {
    pub class_id: i64,
    pub type_name: String,
    pub property: String,
}

impl std::fmt::Display for UnsupportedNativeReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (class {}) refers to an object outside its GameObject at {}; a component without a runtime adapter is built from a template that holds only its GameObject",
            self.type_name, self.class_id, self.property
        )
    }
}
impl std::error::Error for UnsupportedNativeReference {}

/// Whether the loader makes a component of this class from a template.
pub(crate) fn templated(object: &Expanded) -> bool {
    !supported_class(object.class_id)
        && !object.stripped
        && !object.removed
        && object.data.contains_key("m_GameObject")
}

/// The templates of the GameObjects among `objects` that hold a component
/// without an adapter, by the GameObject's file ID, each with the file IDs
/// of its templated components in the template's order.
pub(crate) fn templates_of(
    objects: &[Expanded],
) -> Result<Vec<(String, NativeTemplate, Vec<String>)>> {
    // The loader gives a component to the GameObject its `m_GameObject`
    // names; the GameObject's list only orders them.
    let mut owned: BTreeMap<String, Vec<(usize, &Expanded)>> = BTreeMap::new();
    for (index, object) in objects.iter().enumerate() {
        if templated(object) {
            let owner = value::file_id(object.get("m_GameObject")).into_owned();
            owned.entry(owner).or_default().push((index, object));
        }
    }
    let mut templates = Vec::new();
    for owner in objects.iter().filter(|o| o.class_id == 1 && !o.removed) {
        let Some(mut components) = owned.remove(&owner.file_id) else {
            continue;
        };
        let mut listed: BTreeMap<String, usize> = BTreeMap::new();
        if let Value::Array(items) = owner.get("m_Component") {
            for (position, item) in items.iter().enumerate() {
                let reference = match item.field("component") {
                    Value::Null => item,
                    found => found,
                };
                listed
                    .entry(value::file_id(reference).into_owned())
                    .or_insert(position);
            }
        }
        components.sort_by_key(|(index, object)| {
            (
                listed.get(&object.file_id).copied().unwrap_or(usize::MAX),
                *index,
            )
        });
        let components: Vec<&Expanded> = components.into_iter().map(|(_, o)| o).collect();
        let (template, order) = template(owner, &components)?;
        templates.push((owner.file_id.clone(), template, order));
    }
    Ok(templates)
}

/// A positive local ID named by `seed`.
fn local_id(seed: &str) -> i64 {
    let digest = sha256(seed.as_bytes());
    let value = i64::from_str_radix(&digest[..15], 16).unwrap_or(1);
    value.max(1)
}

fn template(owner: &Expanded, components: &[&Expanded]) -> Result<(NativeTemplate, Vec<String>)> {
    // The content the template is named by: each component's class and
    // fields, its own references aside.
    let mut content = String::from("ume-native-template-v1");
    for component in components {
        let mut data = component.data.clone();
        for key in [
            "m_GameObject",
            "m_CorrespondingSourceObject",
            "m_PrefabInstance",
            "m_PrefabAsset",
        ] {
            data.remove(key);
        }
        content.push_str(&format!(
            "\0{}\0{}\0{}",
            component.class_id,
            component.type_name,
            value::encode_json(&data)?
        ));
    }
    let key = sha256(content.as_bytes());
    let game_object = local_id(&format!("{key}\0GameObject"));
    let transform = local_id(&format!("{key}\0Transform"));
    let ids: Vec<i64> = (0..components.len())
        .map(|index| local_id(&format!("{key}\0component\0{index}")))
        .collect();
    // References within the GameObject lead to the template's objects.
    let mut local: BTreeMap<String, i64> = BTreeMap::new();
    local.insert(owner.file_id.clone(), game_object);
    for (component, id) in components.iter().zip(&ids) {
        local.insert(component.file_id.clone(), *id);
    }
    let mut documents = Vec::new();
    let mut listed = vec![reference(transform)];
    listed.extend(ids.iter().map(|id| reference(*id)));
    let owner_data = value::map([
        ("m_ObjectHideFlags", Value::Int(0)),
        (
            "m_Component",
            Value::Array(
                listed
                    .into_iter()
                    .map(|component| Value::Object(value::map([("component", component)])))
                    .collect(),
            ),
        ),
        ("m_Layer", Value::Int(0)),
        ("m_Name", "Template".into()),
        ("m_TagString", "Untagged".into()),
        ("m_IsActive", Value::Int(0)),
    ]);
    documents.push((1, game_object, "GameObject".to_string(), owner_data));
    let transform_data = value::map([
        ("m_ObjectHideFlags", Value::Int(0)),
        ("m_GameObject", reference(game_object)),
        (
            "m_LocalRotation",
            vector(&[("x", 0.0), ("y", 0.0), ("z", 0.0), ("w", 1.0)]),
        ),
        (
            "m_LocalPosition",
            vector(&[("x", 0.0), ("y", 0.0), ("z", 0.0)]),
        ),
        (
            "m_LocalScale",
            vector(&[("x", 1.0), ("y", 1.0), ("z", 1.0)]),
        ),
        ("m_Children", Value::Array(Vec::new())),
        ("m_Father", reference(0)),
    ]);
    documents.push((4, transform, "Transform".to_string(), transform_data));
    let mut listed_components = Vec::new();
    for (component, id) in components.iter().zip(&ids) {
        let mut data = component.data.clone();
        for key in [
            "m_CorrespondingSourceObject",
            "m_PrefabInstance",
            "m_PrefabAsset",
        ] {
            data.insert(key.into(), reference(0));
        }
        data.insert("m_GameObject".into(), reference(game_object));
        for (key, item) in data.iter_mut() {
            if key == "m_GameObject" {
                continue;
            }
            rebind(item, key, &local, component)?;
        }
        documents.push((component.class_id, *id, component.type_name.clone(), data));
        listed_components.push((*id, component.class_id, component.type_name.clone()));
    }
    let mut prefab = String::from("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n");
    for (class_id, id, type_name, data) in documents {
        prefab.push_str(&format!("--- !u!{class_id} &{id}\n"));
        prefab.push_str(&document_text(&type_name, &data)?);
        prefab.push('\n');
    }
    let order = components
        .iter()
        .map(|component| component.file_id.clone())
        .collect();
    Ok((
        NativeTemplate {
            key,
            prefab,
            game_object,
            components: listed_components,
        },
        order,
    ))
}

fn reference(id: i64) -> Value {
    Value::Object(value::map([("fileID", Value::Int(id))]))
}

fn vector(fields: &[(&str, f64)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(key, value)| (key.to_string(), Value::Float(*value)))
            .collect(),
    )
}

/// Points the references of a templated component at the template's
/// objects; a reference outside the GameObject is refused.
fn rebind(
    item: &mut Value,
    property: &str,
    local: &BTreeMap<String, i64>,
    component: &Expanded,
) -> Result<()> {
    match item {
        Value::Object(map) if map.contains_key("fileID") => {
            let id = value::map_file_id(map).into_owned();
            let guid = value::text(map, "guid").into_owned();
            if id == "0" {
                return Ok(());
            }
            match (guid.is_empty(), local.get(&id)) {
                (true, Some(target)) => {
                    *item = reference(*target);
                    Ok(())
                }
                _ => Err(anyhow!(UnsupportedNativeReference {
                    class_id: component.class_id,
                    type_name: component.type_name.clone(),
                    property: property.to_string(),
                })),
            }
        }
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                rebind(child, &format!("{property}.{key}"), local, component)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                rebind(
                    child,
                    &format!("{property}.Array.data[{index}]"),
                    local,
                    component,
                )?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// One document of the template as Unity writes it as text.
fn document_text(type_name: &str, data: &Map) -> Result<String> {
    let json: serde_json::Value = serde_json::from_str(&value::encode_json(data)?)?;
    let mut outer = serde_json::Map::new();
    outer.insert(type_name.to_string(), json);
    let node = crate::authoring::yamlcodec::document_node(&serde_json::Value::Object(outer))
        .map_err(|error| anyhow!("template YAML: {error}"))?;
    crate::authoring::yamlcodec::encode_node(&node)
        .map_err(|error| anyhow!("template YAML: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(class_id: i64, file_id: &str, type_name: &str, data: Map) -> Expanded {
        Expanded {
            class_id,
            file_id: file_id.into(),
            type_name: type_name.into(),
            data,
            ..Expanded::default()
        }
    }

    fn scene(owner: &str, obstacle: &str, carve: Value) -> Vec<Expanded> {
        let component = |id: &str| Value::Object(value::map([("component", reference_to(id))]));
        vec![
            object(
                1,
                owner,
                "GameObject",
                value::map([
                    (
                        "m_Component",
                        Value::Array(vec![component("40"), component(obstacle), component("23")]),
                    ),
                    ("m_Name", "Door".into()),
                ]),
            ),
            object(
                4,
                "40",
                "Transform",
                value::map([("m_GameObject", reference_to(owner))]),
            ),
            object(
                208,
                obstacle,
                "NavMeshObstacle",
                value::map([
                    ("m_GameObject", reference_to(owner)),
                    ("m_Carve", Value::Int(1)),
                    ("m_Extents", carve),
                ]),
            ),
            object(
                23,
                "23",
                "MeshRenderer",
                value::map([("m_GameObject", reference_to(owner))]),
            ),
        ]
    }

    fn reference_to(id: &str) -> Value {
        Value::Object(value::map([("fileID", Value::Int(id.parse().unwrap()))]))
    }

    #[test]
    fn a_component_without_an_adapter_is_templated_with_its_game_object() {
        let extents = Value::Object(value::map([("x", Value::Float(0.5))]));
        let templates = templates_of(&scene("10", "30", extents.clone())).unwrap();
        assert_eq!(templates.len(), 1);
        let (owner, template, order) = &templates[0];
        assert_eq!(owner, "10");
        assert_eq!(order, &["30".to_string()]);
        assert_eq!(template.components.len(), 1);
        let (id, class_id, type_name) = &template.components[0];
        assert_eq!((*class_id, type_name.as_str()), (208, "NavMeshObstacle"));
        assert!(template.prefab.contains(&format!("--- !u!208 &{id}\n")));
        let obstacle = template.prefab.split("--- !u!208").nth(1).unwrap();
        assert!(obstacle.contains(&format!(
            "m_GameObject:
    fileID: {}",
            template.game_object
        )));
        assert!(!template.prefab.contains("MeshRenderer"));
        assert!(template.path().starts_with(TEMPLATE_FOLDER));
        // A template is named by what it holds, not by the scene's IDs.
        let other = templates_of(&scene("11", "31", extents)).unwrap();
        assert_eq!(other[0].1, *template);
        let changed = Value::Object(value::map([("x", Value::Float(2.0))]));
        assert_ne!(
            templates_of(&scene("10", "30", changed)).unwrap()[0].1.key,
            template.key
        );
    }

    #[test]
    fn a_templated_reference_outside_its_game_object_is_refused() {
        let error = templates_of(&scene("10", "30", reference_to("23"))).unwrap_err();
        let refused = error.downcast_ref::<UnsupportedNativeReference>().unwrap();
        assert_eq!(refused.class_id, 208);
        assert_eq!(refused.property, "m_Extents");
        // A reference to the GameObject itself stays inside the template.
        let own = templates_of(&scene("10", "30", reference_to("10"))).unwrap();
        assert!(
            own[0]
                .1
                .prefab
                .contains(&format!("m_Extents:\n    fileID: {}", own[0].1.game_object))
        );
    }
}
