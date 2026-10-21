use super::{
    PublicationInput, PublicationOutput, PublicationResult, StagedPublicationRequest,
    component_defaults::NativeDefaultsTemplate,
    edit::nullable,
    hierarchy::id,
    prefab_linking::owner_of,
    store::*,
    yaml_edit::{TextAsset, ids_from_list, items},
};
use crate::{
    graph::{AuthoredGraph, GraphObject},
    sha256,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::DirBuilderExt,
};
const MAX_PLAN: u64 = 96 << 20;
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrefabEditRequest {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub expected_revision: String,
    #[serde(default)]
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefab_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_file_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub completely: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub component_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub property: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub fields: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults_template: Option<NativeDefaultsTemplate>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrefabEditResult {
    pub backend: String,
    pub operation_id: String,
    pub action: String,
    pub publication: PublicationResult,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub created_ids: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub generated_path: String,
    /// True when an existing plan without created IDs was read back (omitted),
    /// as opposed to an empty map for a new plan. Callers that embed the map in
    /// a reply need the distinction.
    #[serde(skip)]
    pub created_ids_nil: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrefabSourcePlan {
    #[serde(default)]
    schema_version: i64,
    #[serde(default)]
    fingerprint: String,
    request: PrefabEditRequest,
    publication: StagedPublicationRequest,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    created_ids: BTreeMap<String, String>,
}
pub(crate) struct PrefabPlanner<'a> {
    pub store: &'a Store,
    pub source: Source,
    pub asset: TextAsset,
    pub graph: AuthoredGraph,
    /// Expanded fileID to its position in `graph.objects`.
    pub objects: HashMap<String, usize>,
    pub inputs: BTreeMap<String, String>,
    pub outputs: BTreeMap<String, PublicationOutput>,
    pub created: BTreeMap<String, String>,
}
pub(crate) fn component_ids(data: &Map<String, Value>) -> Vec<String> {
    items(data.get("m_Component").unwrap_or(&Value::Null))
        .iter()
        .map(|entry| id(&entry["component"]))
        .collect()
}
impl Store {
    /// PrefabEdit plans only authored serialization. Immutable plans and the existing
    /// publication journal allow exact operation replay without reopening Unity.
    /// `before_publish` is deliberately not persisted: each caller/retry binds the
    /// current external native-default context immediately before writing.
    pub fn prefab_edit(
        &self,
        request: &PrefabEditRequest,
        before_publish: Option<&dyn Fn() -> Result<()>>,
    ) -> Result<PrefabEditResult> {
        self.asset_path(&request.path, &[".unity", ".prefab"])?;
        if request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || request.operation_id.trim() != request.operation_id
            || request.expected_revision.is_empty()
        {
            return Err(failure(
                "invalid_request",
                "prefab source authoring requires operationId and expectedRevision",
                None,
            ));
        }
        if ![
            "instantiate",
            "create_prefab",
            "create_variant",
            "set_override",
            "apply",
            "revert",
            "unpack",
            "add_component",
        ]
        .contains(&request.action.as_str())
        {
            return Err(failure(
                "unsupported_operation",
                "unsupported source prefab operation",
                None,
            ));
        }
        let fingerprint = sha256(&encode_json(request)?);
        let (plan, existing_plan) = self.locked(|| {
            self.safe_path("Library/UnityMicroEditor/authoring/prefab-plans", true)?;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(self.state.join("prefab-plans"))?;
            let filename = self.safe_path(
                &format!(
                    "Library/UnityMicroEditor/authoring/prefab-plans/{}.json",
                    sha256(request.operation_id.as_bytes())
                ),
                false,
            )?;
            match File::open(&filename) {
                Ok(file) => {
                    let bounded = file
                        .metadata()
                        .is_ok_and(|info| info.is_file() && info.len() <= MAX_PLAN);
                    if !bounded {
                        return Err(failure(
                            "invalid_plan",
                            "prefab plan exceeds bounded regular source plan profile",
                            None,
                        ));
                    }
                    let mut data = Vec::new();
                    file.take(MAX_PLAN + 1).read_to_end(&mut data)?;
                    let plan = serde_json::from_slice::<PrefabSourcePlan>(&data)
                        .ok()
                        .filter(|plan| {
                            data.len() as u64 <= MAX_PLAN
                                && plan.schema_version == 1
                                && plan.publication.operation_id == request.operation_id
                        })
                        .ok_or_else(|| {
                            failure("invalid_plan", "invalid durable prefab source plan", None)
                        })?;
                    if plan.fingerprint != fingerprint {
                        return Err(failure(
                            "idempotency_conflict",
                            "operationId was already used for another prefab request",
                            None,
                        ));
                    }
                    Ok((plan, true))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let mut planner = PrefabPlanner::new(self, request)?;
                    planner.build(request)?;
                    let plan = PrefabSourcePlan {
                        schema_version: 1,
                        fingerprint: fingerprint.clone(),
                        request: request.clone(),
                        publication: planner.publication(&request.operation_id),
                        created_ids: planner.created,
                    };
                    let mut data = encode_json(&plan)?;
                    if data.len() as u64 > MAX_PLAN {
                        return Err(failure(
                            "response_too_large",
                            "prefab source plan exceeds96 MiB",
                            None,
                        ));
                    }
                    data.push(b'\n');
                    atomic_write(&filename, &data, 0o600)?;
                    Ok((plan, false))
                }
                Err(error) => Err(error.into()),
            }
        })?;
        let publication = self.publish_staged_guarded(&plan.publication, before_publish)?;
        Ok(PrefabEditResult {
            backend: "raw_source".into(),
            operation_id: request.operation_id.clone(),
            action: request.action.clone(),
            publication,
            created_ids_nil: existing_plan && plan.created_ids.is_empty(),
            created_ids: plan.created_ids,
            generated_path: request.output_path.clone(),
        })
    }
}
impl<'a> PrefabPlanner<'a> {
    pub(crate) fn new(store: &'a Store, request: &PrefabEditRequest) -> Result<Self> {
        let source = store.read_source(&request.path)?;
        if source.revision != request.expected_revision {
            return Err(failure(
                "revision_conflict",
                "prefab source or metadata changed",
                Some(json!({"actualRevision": source.revision})),
            ));
        }
        let asset = TextAsset::parse(&source.content)?;
        let (graph, revisions, _) = store.resolved_graph(&request.path)?;
        if revisions.get(&request.path) != Some(&source.revision) {
            return Err(failure(
                "revision_conflict",
                "source changed during prefab resolution",
                None,
            ));
        }
        let objects = graph
            .objects
            .iter()
            .enumerate()
            .map(|(index, object)| (object.document.file_id.clone(), index))
            .collect();
        Ok(Self {
            store,
            source,
            asset,
            graph,
            objects,
            inputs: revisions,
            outputs: BTreeMap::new(),
            created: BTreeMap::new(),
        })
    }
    pub(crate) fn publication(&self, operation_id: &str) -> StagedPublicationRequest {
        StagedPublicationRequest {
            operation_id: operation_id.into(),
            inputs: self
                .inputs
                .iter()
                .map(|(path, revision)| PublicationInput {
                    path: path.clone(),
                    expected_revision: revision.clone(),
                })
                .collect(),
            outputs: self.outputs.values().cloned().collect(),
        }
    }
    pub(crate) fn write_asset(
        &mut self,
        path: &str,
        content: String,
        metadata: Option<String>,
    ) -> Result<()> {
        self.store.asset_path(path, &[".unity", ".prefab"])?;
        if self.inputs.get(path).is_none_or(String::is_empty) {
            let revision = self.store.dependency_revision_inner(path)?;
            self.inputs.insert(path.into(), revision);
        }
        self.outputs.insert(
            path.into(),
            PublicationOutput {
                path: path.into(),
                content,
                metadata,
            },
        );
        Ok(())
    }
    pub(crate) fn write_scene(&mut self) -> Result<()> {
        let (path, content) = (self.source.path.clone(), self.asset.text());
        self.write_asset(&path, content, None)
    }
    pub(crate) fn include_graph(
        &mut self,
        path: &str,
    ) -> Result<(AuthoredGraph, BTreeMap<String, String>)> {
        let (graph, revisions, _) = self.store.resolved_graph(path)?;
        for (path, revision) in &revisions {
            if self
                .inputs
                .get(path)
                .is_some_and(|old| !old.is_empty() && old != revision)
            {
                return Err(failure(
                    "revision_conflict",
                    "prefab dependencies changed while planning",
                    None,
                ));
            }
            self.inputs.insert(path.clone(), revision.clone());
        }
        Ok((graph, revisions))
    }
    pub(crate) fn object(&self, file_id: &str) -> Option<&GraphObject> {
        self.objects
            .get(file_id)
            .map(|&index| &self.graph.objects[index])
    }
    /// The position in `graph.objects` of the single GameObject addressed.
    pub(crate) fn target(&self, file_id: &str, path: &str) -> Result<usize> {
        let mut found = None;
        for value in &self.graph.objects {
            let index = self.objects[&value.document.file_id];
            let object = &self.graph.objects[index];
            if object.document.class_id != 1 {
                continue;
            }
            let mut matches = false;
            if !file_id.is_empty() {
                let expanded = self.graph.local_ids.get(file_id);
                matches = object.document.file_id == file_id
                    || expanded.is_some_and(|expanded| {
                        !expanded.is_empty() && &object.document.file_id == expanded
                    });
            }
            if !path.is_empty() {
                let logical = self.object_path(&object.document.file_id, &mut HashSet::new())?;
                matches = if file_id.is_empty() {
                    logical == path
                } else {
                    matches && logical == path
                };
            }
            if matches {
                if found.is_some() {
                    return Err(failure(
                        "ambiguous_target",
                        "prefab target path/fileID identifies multiple GameObjects",
                        None,
                    ));
                }
                found = Some(index);
            }
        }
        found.ok_or_else(|| {
            failure(
                "not_found",
                "prefab target requires an existing GameObject fileId or targetPath",
                None,
            )
        })
    }
    pub(crate) fn transform(&self, owner: &str) -> Option<usize> {
        self.graph
            .objects
            .iter()
            .find(|value| [4, 224].contains(&value.document.class_id) && owner_of(value) == owner)
            .map(|value| self.objects[&value.document.file_id])
    }
    fn object_path(&self, owner: &str, seen: &mut HashSet<String>) -> Result<String> {
        if !seen.insert(owner.to_string()) {
            return Err(failure(
                "invalid_graph",
                "cycle while resolving prefab target path",
                None,
            ));
        }
        let object = self
            .object(owner)
            .filter(|object| object.document.class_id == 1)
            .ok_or_else(|| failure("invalid_graph", "prefab hierarchy owner is absent", None))?;
        let name = object
            .document
            .data
            .get("m_Name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let transform = self
            .transform(owner)
            .ok_or_else(|| failure("invalid_graph", "prefab GameObject lacks transform", None))?;
        let parent = id(self.graph.objects[transform]
            .document
            .data
            .get("m_Father")
            .unwrap_or(&Value::Null));
        if parent == "0" {
            return Ok(name.to_string());
        }
        let parent_transform = self
            .object(&parent)
            .ok_or_else(|| failure("invalid_graph", "prefab parent transform is absent", None))?;
        let prefix = self.object_path(&owner_of(parent_transform), seen)?;
        Ok(format!("{prefix}/{name}"))
    }
    /// Expanded IDs of a GameObject subtree: owners, components and transforms.
    pub(crate) fn selected(&self, owner: &str) -> Result<HashSet<String>> {
        let mut selected = HashSet::new();
        let mut pending = vec![owner.to_string()];
        while let Some(owner) = pending.pop() {
            if !selected.insert(owner.clone()) {
                return Err(failure(
                    "invalid_graph",
                    "duplicate or cyclic prefab subtree",
                    None,
                ));
            }
            let object = self
                .object(&owner)
                .filter(|object| object.document.class_id == 1)
                .ok_or_else(|| failure("invalid_graph", "prefab child owner is absent", None))?;
            for component in component_ids(&object.document.data) {
                if !self.objects.contains_key(&component) {
                    return Err(failure("invalid_graph", "prefab component is absent", None));
                }
                selected.insert(component);
            }
            let transform = self
                .transform(&owner)
                .ok_or_else(|| failure("invalid_graph", "prefab transform is absent", None))?;
            let mut owners = Vec::new();
            for child in ids_from_list(
                self.graph.objects[transform]
                    .document
                    .data
                    .get("m_Children")
                    .unwrap_or(&Value::Null),
            ) {
                let transform = self.object(&child).ok_or_else(|| {
                    failure("invalid_graph", "prefab child transform is absent", None)
                })?;
                owners.push(owner_of(transform));
            }
            pending.extend(owners.into_iter().rev());
        }
        Ok(selected)
    }
    /// The fileID of the authored PrefabInstance that owns an expanded object.
    pub(crate) fn instance(&self, owner: &str) -> Result<String> {
        let prefix = owner.split(':').next().unwrap_or_default();
        if self
            .asset
            .find(prefix)
            .is_some_and(|doc| doc.class_id() == 1001)
        {
            return Ok(prefix.to_string());
        }
        for local in &self.asset.docs {
            if local.class_id() == 1
                && self.graph.local_ids.get(local.id()).map(String::as_str) == Some(owner)
            {
                let instance = id(local.field("m_PrefabInstance"));
                if self
                    .asset
                    .find(&instance)
                    .is_some_and(|doc| doc.class_id() == 1001)
                {
                    return Ok(instance);
                }
            }
        }
        Err(failure(
            "not_prefab_instance",
            "target is not part of an authored prefab instance",
            None,
        ))
    }
    pub(crate) fn build(&mut self, request: &PrefabEditRequest) -> Result<()> {
        match request.action.as_str() {
            "instantiate" => self.instantiate(request),
            "create_variant" => self.create_variant(request),
            "create_prefab" => self.create_prefab(request),
            "set_override" => self.set_override(request),
            "apply" | "revert" => self.apply_revert(request),
            "unpack" => self.unpack(request),
            "add_component" => self.add_component(request),
            _ => Err(failure("internal_error", "unsupported prefab action", None)),
        }
    }
}
