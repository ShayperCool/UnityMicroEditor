//! Verified static model projections supplied by the native adapter.
use super::{
    expand::builtin_guid,
    lexpath,
    native::{native_content_class, valid_native_id},
    session::Inner,
    strconv,
    types::{Document, NativeModelGraphObject, NativeModelGraphTarget, NativeReplacement},
    value::{self, Map, Value, sprint},
};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

fn static_model_references(class: i32) -> Option<&'static [&'static str]> {
    Some(match class {
        1 => &["m_Component"],
        4 => &["m_GameObject", "m_Children", "m_Father"],
        33 => &["m_GameObject", "m_Mesh"],
        23 => &[
            "m_GameObject",
            "m_Materials",
            "m_StaticBatchRoot",
            "m_ProbeAnchor",
            "m_LightProbeVolumeOverride",
            "m_AdditionalVertexStreams",
        ],
        137 => &crate::runtimeartifact::SKINNED_MESH_RENDERER_REFERENCES,
        95 => &crate::runtimeartifact::ANIMATOR_REFERENCES,
        _ => return None,
    })
}
fn static_model_scalars(class: i32) -> Option<&'static [&'static str]> {
    Some(match class {
        1 => &[
            "m_Name",
            "m_Layer",
            "m_TagString",
            "m_IsActive",
            "m_StaticEditorFlags",
        ],
        4 => &["m_LocalPosition", "m_LocalRotation", "m_LocalScale"],
        33 => &[],
        23 => &[
            "m_Enabled",
            "m_CastShadows",
            "m_ReceiveShadows",
            "m_DynamicOccludee",
            "m_MotionVectors",
            "m_LightProbeUsage",
            "m_ReflectionProbeUsage",
            "m_RenderingLayerMask",
            "m_SortingLayerID",
            "m_SortingOrder",
        ],
        137 => &crate::runtimeartifact::SKINNED_MESH_RENDERER_SCALARS,
        95 => &crate::runtimeartifact::ANIMATOR_SCALARS,
        _ => return None,
    })
}

/// SHA-256 of the graph ordered by numeric local ID, as the adapter computes it.
pub fn native_model_graph_sha256(graph: &[NativeModelGraphObject]) -> String {
    let mut canonical: Vec<&NativeModelGraphObject> = graph.iter().collect();
    canonical.sort_by_key(|g| strconv::parse_int(&g.file_id, 10).unwrap_or(0));
    // The same compact JSON as `runtimeartifact::hash_model_graph`.
    serde_json::to_vec(&canonical).map_or(String::new(), |data| crate::hex(&Sha256::digest(data)))
}

fn numeric(value: &Value) -> bool {
    !matches!(
        value,
        Value::Bool(_) | Value::String(_) | Value::Object(_) | Value::Array(_)
    )
}
fn native_integer(value: &Value, minimum: i64, maximum: i64) -> bool {
    if value.is_null() || !numeric(value) {
        return false;
    }
    value::decimal(&sprint(value))
        .and_then(|n| n.int64())
        .is_some_and(|n| (minimum..=maximum).contains(&n))
}
fn native_vector(value: &Value, quaternion: bool) -> bool {
    let Value::Object(vector) = value else {
        return false;
    };
    let wanted: &[&str] = if quaternion {
        &["x", "y", "z", "w"]
    } else {
        &["x", "y", "z"]
    };
    vector.len() == wanted.len()
        && wanted.iter().all(|axis| {
            vector.get(*axis).is_some_and(|value| {
                numeric(value)
                    && strconv::parse_float32(&sprint(value)).is_ok_and(|n| n.is_finite())
            })
        })
}
fn validate_static_model_scalars(graph: &NativeModelGraphObject) -> Result<()> {
    static NONE: Map = Map::new();
    let scalars = graph.scalars.as_ref().unwrap_or(&NONE);
    let Some(wanted) = static_model_scalars(graph.class_id).filter(|w| w.len() == scalars.len())
    else {
        bail!(
            "native model class {} has unsupported scalar fields",
            graph.class_id
        );
    };
    for name in wanted {
        let valid = scalars.get(*name).is_some_and(|value| match *name {
            "m_Name" | "m_TagString" => matches!(value, Value::String(_)),
            "m_LocalPosition" | "m_LocalRotation" | "m_LocalScale" => {
                native_vector(value, *name == "m_LocalRotation")
            }
            "m_Layer" => native_integer(value, 0, 31),
            "m_Enabled"
            | "m_IsActive"
            | "m_ReceiveShadows"
            | "m_DynamicOccludee"
            | "m_UpdateWhenOffscreen"
            | "m_SkinnedMotionVectors"
            | "m_ApplyRootMotion"
            | "m_LinearVelocityBlending"
            | "m_StabilizeFeet"
            | "m_HasTransformHierarchy"
            | "m_KeepAnimatorStateOnDisable"
            | "m_WriteDefaultValuesOnDisable" => native_integer(value, 0, 1),
            "m_Quality" => native_integer(value, 0, 4),
            "m_CullingMode" | "m_UpdateMode" => native_integer(value, 0, 3),
            "m_AABB" => match value {
                Value::Object(bounds) => {
                    bounds.len() == 2
                        && ["m_Center", "m_Extent"].iter().all(|name| {
                            bounds
                                .get(*name)
                                .is_some_and(|vector| native_vector(vector, false))
                        })
                }
                _ => false,
            },
            "m_BlendShapeWeights" => match value {
                Value::Array(weights) => weights.iter().all(|weight| {
                    numeric(weight)
                        && strconv::parse_float32(&sprint(weight)).is_ok_and(|n| n.is_finite())
                }),
                _ => false,
            },
            "m_CastShadows" => native_integer(value, 0, 3),
            "m_MotionVectors" | "m_LightProbeUsage" | "m_ReflectionProbeUsage" => {
                native_integer(value, 0, 2)
            }
            "m_SortingLayerID" => native_integer(value, i32::MIN as i64, i32::MAX as i64),
            "m_SortingOrder" => native_integer(value, i16::MIN as i64, i16::MAX as i64),
            _ => native_integer(value, 0, u32::MAX as i64),
        });
        if !valid {
            bail!("native model scalar {name} is invalid at {}", graph.file_id);
        }
    }
    Ok(())
}

impl Inner {
    /// Consumes only the native adapter's verified static model projection. It
    /// supplies no guessed hierarchy, names, poses or materials.
    pub(crate) fn static_native_model_documents(
        &self,
        replacement: &NativeReplacement,
    ) -> Result<Vec<Document>> {
        let mut objects: BTreeMap<&str, &str> = BTreeMap::new();
        let mut graph_objects: BTreeMap<&str, &NativeModelGraphObject> = BTreeMap::new();
        let type_of = |objects: &BTreeMap<&str, &str>, id: &str| -> String {
            objects.get(id).copied().unwrap_or("").to_string()
        };
        for value in replacement.content.objects.iter() {
            if !valid_native_id(&value.file_id) || !type_of(&objects, &value.file_id).is_empty() {
                bail!(
                    "native model object ID is invalid or duplicated: {}",
                    value.file_id
                );
            }
            if !matches!(
                native_content_class(&value.type_name),
                1 | 4 | 23 | 33 | 21 | 43 | 74 | 90 | 95 | 137
            ) {
                bail!(
                    "native model type {} is not a verified static profile",
                    value.type_name
                );
            }
            objects.insert(&value.file_id, &value.type_name);
        }
        for graph in replacement.model_graph.iter() {
            if !valid_native_id(&graph.file_id)
                || graph_objects.contains_key(graph.file_id.as_str())
                || native_content_class(&type_of(&objects, &graph.file_id)) != graph.class_id
            {
                bail!(
                    "native model graph identity/type is invalid: {}",
                    graph.file_id
                );
            }
            if static_model_references(graph.class_id).is_none() {
                bail!("native model graph class {} is unsupported", graph.class_id);
            }
            graph_objects.insert(&graph.file_id, graph);
        }
        if graph_objects.is_empty() {
            bail!("native model has no verified static hierarchy");
        }
        for (id, type_name) in &objects {
            let class = native_content_class(type_name);
            // Assets of the model, whose own data the content holds.
            if !matches!(class, 21 | 43 | 74 | 90) && !graph_objects.contains_key(id) {
                bail!("native model graph omits object {id}");
            }
        }
        let dependencies: HashSet<&str> = replacement
            .content
            .dependencies
            .iter()
            .map(|d| d.guid.as_str())
            .collect();
        let target_type = |target: &NativeModelGraphTarget| -> Result<String> {
            if target.file_id == "0" {
                if !target.guid.is_empty() {
                    bail!("null model pointer has a GUID");
                }
                return Ok(String::new());
            }
            if !valid_native_id(&target.file_id) {
                bail!("invalid model pointer local ID {}", target.file_id);
            }
            if target.guid.is_empty() || target.guid == replacement.guid {
                let known = type_of(&objects, &target.file_id);
                if known.is_empty() {
                    bail!("dangling native model local reference {}", target.file_id);
                }
                return Ok(known);
            }
            // The adapted file drops a renderer's authored material, which
            // the scene binds from its source.
            if !dependencies.contains(target.guid.as_str())
                && target.file_id == "2100000"
                && lexpath::equal_fold(lexpath::ext(self.path_of(&target.guid)), ".mat")
            {
                return Ok("UnityEngine.Material".into());
            }
            if !dependencies.contains(target.guid.as_str()) {
                bail!(
                    "native model external pointer has no declared dependency: {}",
                    target.guid
                );
            }
            if builtin_guid(&target.guid) {
                return Ok("builtin".into());
            }
            if let Some(dependency) = self.native_overlays.get(&target.guid)
                && let Some(object) = dependency
                    .content
                    .objects
                    .iter()
                    .find(|o| o.file_id == target.file_id)
            {
                return Ok(object.type_name.clone());
            }
            bail!(
                "native model external pointer does not resolve to a verified runtime object: {}/{}",
                target.guid,
                target.file_id
            );
        };
        let mut references: HashMap<&str, HashMap<&str, &[NativeModelGraphTarget]>> =
            HashMap::new();
        let mut documents = Vec::with_capacity(replacement.model_graph.len());
        for graph in replacement.model_graph.iter() {
            validate_static_model_scalars(graph)?;
            let wanted = static_model_references(graph.class_id).unwrap_or(&[]);
            if graph.references.len() != wanted.len() {
                bail!(
                    "native model references do not match verified class {}",
                    graph.class_id
                );
            }
            let mut data: Map = graph.scalars.clone().unwrap_or_default();
            if graph.class_id == 4 {
                // Unity strips this Inspector bookkeeping from Player data. Keep an
                // override target for authored prefab hints; runtime rotation and
                // the verified fingerprint use m_LocalRotation exclusively.
                data.insert(
                    "m_LocalEulerAnglesHint".into(),
                    Value::Object(value::map([
                        ("x", Value::Int(0)),
                        ("y", Value::Int(0)),
                        ("z", Value::Int(0)),
                    ])),
                );
            }
            let mut properties: HashMap<&str, &[NativeModelGraphTarget]> = HashMap::new();
            for (index, property) in graph.references.iter().enumerate() {
                let name = property.property.as_str();
                if name != wanted[index] {
                    bail!(
                        "native model reference property/order differs at {}",
                        graph.file_id
                    );
                }
                let list = matches!(
                    name,
                    "m_Component" | "m_Children" | "m_Materials" | "m_Bones"
                );
                if !list && property.targets.len() != 1 {
                    bail!("native model scalar reference has wrong cardinality: {name}");
                }
                let mut values = Vec::with_capacity(property.targets.len());
                let mut seen: HashSet<&str> = HashSet::new();
                for target in property.targets.iter() {
                    let type_name = target_type(target)?;
                    let local = target.guid.is_empty() || target.guid == replacement.guid;
                    let null = target.file_id == "0";
                    let mut wanted_type = "";
                    match name {
                        "m_Component" => {
                            if !local
                                || null
                                || !matches!(
                                    type_name.as_str(),
                                    "UnityEngine.Transform"
                                        | "UnityEngine.MeshRenderer"
                                        | "UnityEngine.MeshFilter"
                                        | "UnityEngine.SkinnedMeshRenderer"
                                        | "UnityEngine.Animator"
                                )
                            {
                                bail!(
                                    "native GameObject has unsupported component {}",
                                    target.file_id
                                );
                            }
                            if !seen.insert(&target.file_id) {
                                bail!("native GameObject repeats component {}", target.file_id);
                            }
                        }
                        "m_GameObject" => {
                            wanted_type = "UnityEngine.GameObject";
                            if !local || null {
                                bail!("native component owner must be local");
                            }
                        }
                        "m_Children" => {
                            wanted_type = "UnityEngine.Transform";
                            if !local || null || !seen.insert(&target.file_id) {
                                bail!("native child pointer invalid or duplicated");
                            }
                        }
                        "m_Bones" | "m_RootBone" => {
                            wanted_type = "UnityEngine.Transform";
                            if !local {
                                bail!("native model bone must be local");
                            }
                        }
                        "m_Avatar" => wanted_type = "UnityEngine.Avatar",
                        "m_Controller" => wanted_type = "UnityEngine.RuntimeAnimatorController",
                        "m_Father" | "m_StaticBatchRoot" | "m_ProbeAnchor" => {
                            wanted_type = "UnityEngine.Transform";
                            if !local && name == "m_Father" {
                                bail!("native model parent must be local");
                            }
                        }
                        "m_Mesh" | "m_AdditionalVertexStreams" => wanted_type = "UnityEngine.Mesh",
                        "m_Materials" => wanted_type = "UnityEngine.Material",
                        "m_LightProbeVolumeOverride" => wanted_type = "UnityEngine.GameObject",
                        _ => {}
                    }
                    if !null
                        && !wanted_type.is_empty()
                        && type_name != wanted_type
                        && type_name != "builtin"
                    {
                        bail!("native model {name} target type differs: {type_name}");
                    }
                    let mut reference =
                        value::map([("fileID", Value::Number(target.file_id.clone()))]);
                    if !local && !null {
                        reference.insert("guid".into(), target.guid.as_str().into());
                        reference.insert("type".into(), Value::Int(3));
                    }
                    values.push(if name == "m_Component" {
                        Value::Object(value::map([("component", Value::Object(reference))]))
                    } else {
                        Value::Object(reference)
                    });
                }
                properties.insert(name, &property.targets);
                if list {
                    data.insert(name.into(), Value::Array(values));
                } else {
                    data.insert(name.into(), values.swap_remove(0));
                }
            }
            references.insert(&graph.file_id, properties);
            documents.push(Document {
                class_id: graph.class_id as i64,
                file_id: graph.file_id.clone(),
                type_name: type_of(&objects, &graph.file_id),
                data: Some(data),
                stripped: false,
            });
        }
        let targets = |id: &str, property: &str| -> &[NativeModelGraphTarget] {
            references
                .get(id)
                .and_then(|p| p.get(property))
                .copied()
                .unwrap_or(&[])
        };
        let mut owners: HashMap<&str, &str> = HashMap::new();
        let mut parents: HashMap<&str, &str> = HashMap::new();
        let mut root_count = 0;
        for (id, graph) in &graph_objects {
            if graph.class_id == 1 {
                let mut transforms = 0;
                for target in targets(id, "m_Component") {
                    if graph_objects
                        .get(target.file_id.as_str())
                        .is_some_and(|c| c.class_id == 4)
                    {
                        transforms += 1;
                    }
                    // Every published component profile carries exactly one owner.
                    let owner = targets(&target.file_id, "m_GameObject")
                        .first()
                        .map_or("", |t| t.file_id.as_str());
                    if owners.contains_key(target.file_id.as_str()) || owner != *id {
                        bail!(
                            "native model component ownership disagrees at {}",
                            target.file_id
                        );
                    }
                    owners.insert(&target.file_id, id);
                }
                if transforms != 1 {
                    bail!("native GameObject must own exactly one Transform: {id}");
                }
            } else if graph.class_id == 4 {
                let parent = targets(id, "m_Father")[0].file_id.as_str();
                parents.insert(id, parent);
                if parent == "0" {
                    root_count += 1;
                }
            }
        }
        if root_count != 1 {
            bail!("native model must contain one authored root Transform, got {root_count}");
        }
        for (id, graph) in &graph_objects {
            if graph.class_id != 1 && !owners.contains_key(id) {
                bail!("native component is absent from its owner's component list: {id}");
            }
            if graph.class_id != 4 {
                continue;
            }
            for child in targets(id, "m_Children") {
                if parents.get(child.file_id.as_str()).copied().unwrap_or("") != *id {
                    bail!("native child/parent links disagree: {}", child.file_id);
                }
            }
            let parent = parents[id];
            if parent != "0"
                && !targets(parent, "m_Children")
                    .iter()
                    .any(|c| c.file_id == *id)
            {
                bail!("native parent omits its child: {id}");
            }
            let mut visited: HashSet<&str> = HashSet::new();
            let mut cursor = *id;
            while cursor != "0" {
                if !visited.insert(cursor) {
                    bail!("native model hierarchy is cyclic: {id}");
                }
                cursor = parents.get(cursor).copied().unwrap_or("");
                if cursor.is_empty() {
                    // A missing parent reads as "", which never equals "0"
                    // and is revisited on the next step.
                    bail!("native model hierarchy is cyclic: {id}");
                }
            }
        }
        Ok(documents)
    }
}
