use super::{store::*, yaml_edit::json_numbers, yamlcodec};
use crate::{
    graph::{AuthoredGraph, AuthoredIdentity, SourceProject},
    sha256,
    yaml::{Document, parse_unity_yaml_with_comments},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HierarchyRequest {
    pub path: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub limit: i64,
    #[serde(default)]
    pub expected_revision: String,
    #[serde(default)]
    pub include_fields: Option<bool>,
    #[serde(default)]
    pub resolve_prefabs: Option<bool>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    pub file_id: String,
    pub class_id: i64,
    #[serde(rename = "type")]
    pub type_name: String,
    pub fields: Option<Map<String, Value>>,
    pub stripped: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameObject {
    pub file_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub transform_id: String,
    pub name: String,
    pub parent_id: String,
    pub child_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_child_transform_ids: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub unresolved_parent_transform_id: String,
    pub active: bool,
    pub components: Vec<Component>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefab_instance_id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stripped: bool,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectProvenance {
    pub source_path: String,
    pub source_guid: String,
    pub identities: Vec<AuthoredIdentity>,
    pub local_ids: Vec<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyPage {
    pub path: String,
    pub guid: String,
    pub revision: String,
    pub roots: Vec<String>,
    pub objects: Vec<GameObject>,
    pub unresolved_prefab_instances: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_root_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<BTreeMap<String, ObjectProvenance>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_revisions: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}
pub(crate) fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&Value::Null)
}
/// A string or signed integer fileID, otherwise "0".
pub(crate) fn id(value: &Value) -> String {
    match &value["fileID"] {
        Value::String(v) => v.clone(),
        Value::Number(v) if v.is_i64() => v.to_string(),
        _ => "0".into(),
    }
}
/// Display text of a parsed Unity YAML scalar (`<nil>` for null).
pub(crate) fn sprint(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".into(),
        Value::String(text) => text.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) if yamlcodec::is_float(number) => number
            .to_string()
            .parse()
            .map_or_else(|_| number.to_string(), yamlcodec::format_g),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}
fn ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().map(id).collect())
        .unwrap_or_default()
}
fn raw_root_owner(docs: &[Document], root: &str) -> String {
    let find = |id: &str| docs.iter().find(|d| d.file_id == id);
    let Some(doc) = find(root) else {
        return "0".into();
    };
    let owner_of = |doc: &Document| {
        let owner = id(field(&doc.data, "m_GameObject"));
        if find(&owner).is_some_and(|d| d.class_id == 1) {
            owner
        } else {
            "0".into()
        }
    };
    if [4, 224].contains(&doc.class_id) {
        return owner_of(doc);
    }
    if doc.class_id != 1001 {
        return "0".into();
    }
    let mut owner = "0".to_string();
    for alias in docs {
        if ![4, 224].contains(&alias.class_id)
            || id(field(&alias.data, "m_PrefabInstance")) != root
            || !alias.data.contains_key("m_Father")
            || id(field(&alias.data, "m_Father")) != "0"
        {
            continue;
        }
        let candidate = owner_of(alias);
        if candidate == "0" {
            continue;
        }
        if owner != "0" && owner != candidate {
            return "0".into();
        }
        owner = candidate;
    }
    owner
}
pub(crate) fn hierarchy(source: &Source, docs: &[Document], query: &str) -> HierarchyPage {
    let mut page = HierarchyPage {
        path: source.path.clone(),
        guid: source.guid.clone(),
        revision: source.revision.clone(),
        roots: vec![],
        objects: vec![],
        unresolved_prefab_instances: vec![],
        unresolved_root_ids: vec![],
        provenance: None,
        dependency_revisions: None,
        source_revision: None,
        total: 0,
        offset: 0,
        next_offset: None,
    };
    let mut transforms = BTreeMap::new();
    let documents: BTreeMap<_, _> = docs.iter().map(|doc| (doc.file_id.as_str(), doc)).collect();
    for doc in docs {
        if [4, 224].contains(&doc.class_id) {
            transforms.insert(id(field(&doc.data, "m_GameObject")), doc);
        }
        if doc.class_id == 1001 {
            page.unresolved_prefab_instances.push(doc.file_id.clone());
        }
    }
    for doc in docs {
        if doc.class_id != 1 {
            continue;
        }
        let instance = id(field(&doc.data, "m_PrefabInstance"));
        let mut object = GameObject {
            file_id: doc.file_id.clone(),
            transform_id: String::new(),
            name: field(&doc.data, "m_Name")
                .as_str()
                .unwrap_or_default()
                .into(),
            parent_id: "0".into(),
            child_ids: vec![],
            unresolved_child_transform_ids: vec![],
            unresolved_parent_transform_id: String::new(),
            active: sprint(field(&doc.data, "m_IsActive")) == "1",
            components: vec![],
            prefab_instance_id: if instance == "0" {
                String::new()
            } else {
                instance
            },
            stripped: doc.stripped,
        };
        if let Some(transform) = transforms.get(&doc.file_id) {
            object.transform_id = transform.file_id.clone();
            if let Some(parent) = documents.get(id(field(&transform.data, "m_Father")).as_str()) {
                object.parent_id = id(field(&parent.data, "m_GameObject"));
                if parent.stripped && object.parent_id == "0" {
                    object.unresolved_parent_transform_id = parent.file_id.clone();
                }
            }
            for child in ids(field(&transform.data, "m_Children")) {
                if let Some(child) = documents.get(child.as_str()) {
                    let owner = id(field(&child.data, "m_GameObject"));
                    if owner != "0" {
                        object.child_ids.push(owner);
                    } else {
                        object
                            .unresolved_child_transform_ids
                            .push(child.file_id.clone());
                    }
                }
            }
        }
        if object.parent_id == "0" && object.unresolved_parent_transform_id.is_empty() {
            page.roots.push(doc.file_id.clone());
        }
        if let Some(items) = field(&doc.data, "m_Component").as_array() {
            for item in items {
                if let Some(component) = documents.get(id(&item["component"]).as_str()) {
                    object.components.push(Component {
                        file_id: component.file_id.clone(),
                        class_id: component.class_id,
                        type_name: component.type_name.clone(),
                        fields: match json_numbers(&Value::Object(component.data.clone())) {
                            Value::Object(fields) => Some(fields),
                            _ => None,
                        },
                        stripped: component.stripped,
                    });
                }
            }
        }
        if query.is_empty()
            || object.name.to_lowercase().contains(&query.to_lowercase())
            || doc.file_id == query
        {
            page.objects.push(object);
        }
    }
    for doc in docs {
        if doc.class_id == 1660057539 {
            page.roots.clear();
            for root in ids(field(&doc.data, "m_Roots")) {
                let owner = raw_root_owner(docs, &root);
                if owner != "0" {
                    page.roots.push(owner);
                } else {
                    page.unresolved_root_ids.push(root);
                }
            }
        }
    }
    page
}
impl Store {
    /// The recursively expanded authored graph, its dependency revisions and
    /// the revision binding that whole dependency set.
    pub(crate) fn resolved_graph(
        &self,
        path: &str,
    ) -> Result<(AuthoredGraph, BTreeMap<String, String>, String)> {
        self.asset_path(path, &[".unity", ".prefab"])?;
        let graph = SourceProject::new(&self.root)
            .and_then(|mut project| project.authored_graph(path))
            .map_err(|error| failure("prefab_resolution_failed", format!("{error:#}"), None))?;
        let mut revisions = BTreeMap::new();
        for dependency in &graph.dependencies {
            if dependency.path.ends_with(".meta") {
                continue;
            }
            revisions.insert(
                dependency.path.clone(),
                self.dependency_revision_inner(&dependency.path)?,
            );
        }
        let mut encoded = b"unitymicroeditor-resolved-graph-v1\0".to_vec();
        encoded.extend(encode_json(&revisions)?);
        Ok((graph, revisions, sha256(&encoded)))
    }
    /// Raw authored records without prefab resolution or paging.
    pub(crate) fn raw_hierarchy(&self, path: &str, query: &str) -> Result<HierarchyPage> {
        self.asset_path(path, &[".unity", ".prefab"])?;
        let source = self.read_source(path)?;
        let docs = parse_unity_yaml_with_comments(source.content.as_bytes())
            .map_err(|error| failure("invalid_yaml", format!("{error:#}"), None))?;
        Ok(hierarchy(&source, &docs, query))
    }
    pub fn hierarchy(&self, request: &HierarchyRequest) -> Result<HierarchyPage> {
        if request.offset < 0 || request.limit < 0 || request.limit > 500 {
            return Err(failure(
                "invalid_request",
                "hierarchy offset must be nonnegative and limit at most 500",
                None,
            ));
        }
        let limit = if request.limit == 0 {
            100
        } else {
            request.limit as usize
        };
        let resolve = request.resolve_prefabs != Some(false);
        self.locked(|| {
            let mut provenance = BTreeMap::new();
            let mut page = if resolve {
                let source = self.read_source(&request.path)?;
                let (graph, revisions, graph_revision) = self.resolved_graph(&request.path)?;
                if !request.expected_revision.is_empty()
                    && request.expected_revision != graph_revision
                {
                    return Err(failure(
                        "revision_conflict",
                        "authored prefab dependency graph changed between pages",
                        Some(json!({"actualRevision": graph_revision})),
                    ));
                }
                let mut locals: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for (local, expanded) in graph.local_ids {
                    locals.entry(expanded).or_default().push(local);
                }
                let mut docs = Vec::new();
                for object in graph.objects {
                    let mut local_ids = locals
                        .get(&object.document.file_id)
                        .cloned()
                        .unwrap_or_default();
                    local_ids.sort();
                    provenance.insert(
                        object.document.file_id.clone(),
                        ObjectProvenance {
                            source_path: object.source_path,
                            source_guid: object.guid,
                            identities: object.identities,
                            local_ids,
                        },
                    );
                    let mut doc = object.document;
                    doc.stripped = false;
                    docs.push(doc);
                }
                let mut page = hierarchy(&source, &docs, &request.query);
                page.revision = graph_revision;
                page.dependency_revisions = Some(revisions);
                page.source_revision = Some(source.revision.clone());
                page
            } else {
                let page = self.raw_hierarchy(&request.path, &request.query)?;
                if !request.expected_revision.is_empty()
                    && request.expected_revision != page.revision
                {
                    return Err(failure(
                        "revision_conflict",
                        "authored scene or metadata changed between hierarchy pages",
                        Some(
                            json!({"actualRevision": page.revision, "expectedRevision": request.expected_revision}),
                        ),
                    ));
                }
                page
            };
            page.total = page.objects.len();
            page.offset = request.offset as usize;
            if page.offset > page.total {
                return Err(failure(
                    "invalid_request",
                    "hierarchy offset exceeds filtered object count",
                    None,
                ));
            }
            let end = (page.offset + limit).min(page.total);
            page.objects = page
                .objects
                .into_iter()
                .skip(page.offset)
                .take(limit)
                .collect();
            let mut visible = BTreeMap::new();
            for object in &mut page.objects {
                if let Some(value) = provenance.get(&object.file_id) {
                    visible.insert(object.file_id.clone(), value.clone());
                }
                for component in &mut object.components {
                    if let Some(value) = provenance.get(&component.file_id) {
                        visible.insert(component.file_id.clone(), value.clone());
                    }
                    if request.include_fields == Some(false) {
                        component.fields = None;
                    }
                }
            }
            if resolve {
                page.provenance = Some(visible);
            }
            page.next_offset = (end < page.total).then_some(end);
            Ok(page)
        })
    }
}
