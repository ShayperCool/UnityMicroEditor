use super::{
    Reader,
    adapt::{ExternalReference, Object, ObjectEntry},
    le16, le32, le64,
};
use crate::playerbake::Val;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Authored topology using GUID/local-ID references, so the same graph can be
/// compared with an Editor-imported catalog independently of native external
/// table indexes. Array order is part of the fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelGraphReference {
    #[serde(rename = "fileId")]
    pub file_id: String,
    pub guid: String,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelGraphProperty {
    pub property: String,
    pub targets: Vec<ModelGraphReference>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelGraphObject {
    pub file_id: String,
    pub class_id: i32,
    pub references: Vec<ModelGraphProperty>,
    pub scalars: BTreeMap<String, Value>,
}

fn local_id(object: &ModelGraphObject) -> i64 {
    object.file_id.parse().unwrap_or(0)
}

/// Hashes the compact JSON of the graph in numeric local-ID order. Input
/// graph array order is ignored; reference property and target order remain
/// significant. `project::native_model_graph_sha256` recomputes this value
/// from a stored graph and must encode it identically.
pub fn hash_model_graph(graph: &[ModelGraphObject]) -> String {
    let mut canonical: Vec<&ModelGraphObject> = graph.iter().collect();
    canonical.sort_by_key(|object| local_id(object));
    crate::sha256(&serde_json::to_vec(&canonical).unwrap_or_default())
}

// These inline tree hashes constrain offsets to layouts independently checked
// using Unity's binary2text and public ContentFile readback in 6000.6.3f1.
// Unproven graph layouts never qualify for an in-place model replacement.
fn model_graph_tree(class_id: i32) -> &'static str {
    match class_id {
        1 => "63343cbbd67883d5da96355cf43e2c6e357929a116e48e665d55d28a006ba868",
        4 => "a608a3ce2cf08b0e6f3c73bc2481ac5369c55bccfa02f573a281aaed25d7860f",
        23 => "e477f1d8f03c3656b5cfbfc9c221af430c06315a812a0dc764a514814a80aa59",
        33 => "7f3907445f792e4b1629c1a44de04bce6e7d587f1549c55410d14f80130d6514",
        _ => "",
    }
}

const ANIMATOR: i32 = 95;
const SKINNED_MESH_RENDERER: i32 = 137;

/// The pointers of a skinned renderer and an animator in the graph, in order;
/// a list for `m_Materials` and `m_Bones`.
pub const SKINNED_MESH_RENDERER_REFERENCES: [&str; 7] = [
    "m_GameObject",
    "m_Materials",
    "m_StaticBatchRoot",
    "m_ProbeAnchor",
    "m_Mesh",
    "m_Bones",
    "m_RootBone",
];
pub const ANIMATOR_REFERENCES: [&str; 3] = ["m_GameObject", "m_Avatar", "m_Controller"];
/// Their values beside the pointers.
pub const SKINNED_MESH_RENDERER_SCALARS: [&str; 15] = [
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
    "m_Quality",
    "m_UpdateWhenOffscreen",
    "m_SkinnedMotionVectors",
    "m_AABB",
    "m_BlendShapeWeights",
];
pub const ANIMATOR_SCALARS: [&str; 9] = [
    "m_Enabled",
    "m_CullingMode",
    "m_UpdateMode",
    "m_ApplyRootMotion",
    "m_LinearVelocityBlending",
    "m_StabilizeFeet",
    "m_HasTransformHierarchy",
    "m_KeepAnimatorStateOnDisable",
    "m_WriteDefaultValuesOnDisable",
];

fn typed_pointer(value: &Val, externals: &[ExternalReference]) -> Result<ModelGraphReference> {
    let (Some(Val::I(index)), Some(Val::I(local))) =
        (value.field("m_FileID"), value.field("m_PathID"))
    else {
        bail!("field is not a pointer");
    };
    let mut target = ModelGraphReference {
        file_id: local.to_string(),
        guid: String::new(),
    };
    if *index < 0 || *index as usize > externals.len() {
        bail!("external PPtr index {index} outside table");
    }
    if *local != 0 && *index != 0 {
        target.guid = externals[*index as usize - 1].guid.clone();
    }
    Ok(target)
}

/// A value of the graph: numbers as written, flags as 0 or 1.
fn typed_scalar(value: &Val) -> Result<Value> {
    Ok(match value {
        Val::Bool(flag) => Value::from(u32::from(*flag)),
        Val::I(number) => Value::from(*number),
        Val::U(number) => Value::from(*number),
        Val::F(number) if number.is_finite() => {
            Value::from(if *number == 0.0 { 0.0 } else { *number })
        }
        Val::Seq(items) => Value::Array(items.iter().map(typed_scalar).collect::<Result<_>>()?),
        Val::Map(fields) => {
            let mut map = serde_json::Map::new();
            for (name, field) in fields {
                map.insert(name.to_string(), typed_scalar(field)?);
            }
            Value::Object(map)
        }
        _ => bail!("unsupported value"),
    })
}

fn read_typed_graph_object(
    id: &str,
    class_id: i32,
    value: &Val,
    externals: &[ExternalReference],
) -> Result<ModelGraphObject> {
    let (pointers, values): (&[&str], &[&str]) = match class_id {
        SKINNED_MESH_RENDERER => (
            &SKINNED_MESH_RENDERER_REFERENCES,
            &SKINNED_MESH_RENDERER_SCALARS,
        ),
        _ => (&ANIMATOR_REFERENCES, &ANIMATOR_SCALARS),
    };
    let mut references = Vec::with_capacity(pointers.len());
    for name in pointers {
        let field = value
            .field(name)
            .ok_or_else(|| anyhow::anyhow!("no field {name}"))?;
        let targets = match field {
            Val::Seq(items) => items
                .iter()
                .map(|item| typed_pointer(item, externals))
                .collect::<Result<_>>()?,
            _ => vec![typed_pointer(field, externals)?],
        };
        references.push(ModelGraphProperty {
            property: name.to_string(),
            targets,
        });
    }
    let mut scalars = BTreeMap::new();
    for name in values {
        let field = value
            .field(name)
            .ok_or_else(|| anyhow::anyhow!("no field {name}"))?;
        scalars.insert(name.to_string(), typed_scalar(field)?);
    }
    Ok(ModelGraphObject {
        file_id: id.to_string(),
        class_id,
        references,
        scalars,
    })
}

/// Err carries the reason the graph is not proven, never a hard failure.
pub(crate) fn read_model_graph(
    input: &[u8],
    data_offset: usize,
    entries: &[ObjectEntry],
    objects: &[Object],
    trees: &HashMap<i32, String>,
    externals: &[ExternalReference],
) -> Result<Vec<ModelGraphObject>, String> {
    let retained: HashSet<&str> = objects
        .iter()
        .map(|object| object.file_id.as_str())
        .collect();
    let mut graph = Vec::new();
    // Read by their inline type trees, which name every field.
    let mut decoded = None;
    for entry in entries {
        let id = entry.id.to_string();
        if !retained.contains(id.as_str()) {
            continue;
        }
        match entry.class_id {
            21 | 43 | 74 | 90 => continue, // Separate identities/data hashes cover assets.
            ANIMATOR | SKINNED_MESH_RENDERER => {
                if decoded.is_none() {
                    decoded = Some(
                        crate::playerbake::read_serialized_with_externals(input)
                            .map_err(|error| format!("model types unreadable: {error}"))?
                            .0,
                    );
                }
                let object = decoded
                    .as_ref()
                    .and_then(|objects| objects.iter().find(|object| object.local_id == entry.id))
                    .ok_or_else(|| format!("model object {id} unreadable"))?;
                let value = object
                    .tree
                    .decode(&object.data)
                    .map_err(|error| format!("model object {id} unreadable: {error}"))?;
                graph.push(
                    read_typed_graph_object(&id, entry.class_id, &value, externals).map_err(
                        |error| {
                            format!(
                                "native class {} local ID {} graph: {}",
                                entry.class_id, id, error
                            )
                        },
                    )?,
                );
                continue;
            }
            1 | 4 | 23 | 33 => {
                if trees.get(&entry.class_id).map(String::as_str)
                    != Some(model_graph_tree(entry.class_id))
                {
                    return Err(format!(
                        "unproven native class {} type tree",
                        entry.class_id
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "native class {} model graph has not been verified",
                    entry.class_id
                ));
            }
        }
        let start = data_offset + entry.offset as usize;
        let object = &input[start..start + entry.size as usize];
        match read_model_graph_object(&id, entry.class_id, object, externals) {
            Ok(record) => graph.push(record),
            Err(error) => {
                return Err(format!(
                    "native class {} local ID {} graph: {}",
                    entry.class_id, id, error
                ));
            }
        }
    }
    graph.sort_by_key(local_id);
    Ok(graph)
}

struct GraphReader<'a> {
    r: Reader<'a>,
    externals: &'a [ExternalReference],
    references: Vec<ModelGraphProperty>,
}

impl GraphReader<'_> {
    fn ptr(&mut self) -> Result<ModelGraphReference> {
        let data = self.r.take(12)?;
        let index = le32(&data[..4]) as i32;
        let local = le64(&data[4..]) as i64;
        let mut target = ModelGraphReference {
            file_id: local.to_string(),
            guid: String::new(),
        };
        if index < 0 || index as usize > self.externals.len() {
            bail!("external PPtr index {index} outside table");
        }
        if local != 0 && index != 0 {
            target.guid = self.externals[index as usize - 1].guid.clone();
        }
        Ok(target)
    }
    fn one(&mut self, property: &str) -> Result<()> {
        let target = self.ptr()?;
        self.references.push(ModelGraphProperty {
            property: property.to_string(),
            targets: vec![target],
        });
        Ok(())
    }
    fn list(&mut self, property: &str) -> Result<()> {
        let count = self.r.u32()?;
        if count > 100000 || count as u64 * 12 > (self.r.data.len() - self.r.pos) as u64 {
            bail!("reference array count is out of bounds");
        }
        let mut targets = Vec::with_capacity(count as usize);
        for _ in 0..count {
            targets.push(self.ptr()?);
        }
        self.references.push(ModelGraphProperty {
            property: property.to_string(),
            targets,
        });
        Ok(())
    }
}

fn read_model_graph_object(
    id: &str,
    class_id: i32,
    object: &[u8],
    externals: &[ExternalReference],
) -> Result<ModelGraphObject> {
    let mut g = GraphReader {
        r: Reader::at(object, 40),
        externals,
        references: Vec::new(),
    };
    let mut scalars: BTreeMap<String, Value> = BTreeMap::new();
    match class_id {
        1 => {
            g.list("m_Component")?;
            let layer = g.r.u32()?;
            let name = g.r.string()?;
            let tag = g.r.string()?;
            g.r.take(16)?; // Editor icon reference and navigation layer.
            let static_flags = g.r.u32()?;
            let active = g.r.take(1)?;
            if active[0] > 1 {
                bail!("invalid GameObject active flag");
            }
            scalars.insert("m_Name".into(), Value::from(name));
            scalars.insert("m_Layer".into(), Value::from(layer));
            scalars.insert("m_TagString".into(), Value::from(tag));
            scalars.insert("m_IsActive".into(), Value::from(active[0] as u32));
            scalars.insert("m_StaticEditorFlags".into(), Value::from(static_flags));
        }
        4 => {
            g.one("m_GameObject")?;
            g.r.pos = 96;
            g.list("m_Children")?;
            g.one("m_Father")?;
            if g.r.pos + 12 != object.len() {
                bail!("unexpected Transform payload suffix");
            }
            for (name, offset, count) in [
                ("m_LocalRotation", 52usize, 4usize),
                ("m_LocalPosition", 68, 3),
                ("m_LocalScale", 80, 3),
            ] {
                let mut value: BTreeMap<&str, Value> = BTreeMap::new();
                for (index, axis) in ["x", "y", "z", "w"].into_iter().take(count).enumerate() {
                    let mut number = f32::from_bits(le32(&object[offset + index * 4..]));
                    if !number.is_finite() {
                        bail!("non-finite Transform {name}");
                    }
                    if number == 0.0 {
                        number = 0.0; // Negative zero is the same pose; store one spelling.
                    }
                    value.insert(axis, Value::from(number));
                }
                scalars.insert(name.into(), serde_json::to_value(value)?);
            }
        }
        33 => {
            g.one("m_GameObject")?;
            g.one("m_Mesh")?;
            if g.r.pos != object.len() {
                bail!("unexpected MeshFilter payload suffix");
            }
        }
        23 => {
            g.one("m_GameObject")?;
            g.r.pos = 84;
            g.list("m_Materials")?;
            g.r.take(4)?;
            for property in [
                "m_StaticBatchRoot",
                "m_ProbeAnchor",
                "m_LightProbeVolumeOverride",
            ] {
                g.one(property)?;
            }
            if object.len() < 208 {
                bail!("unexpected MeshRenderer payload suffix");
            }
            for (name, offset) in [
                ("m_Enabled", 52usize),
                ("m_CastShadows", 53),
                ("m_ReceiveShadows", 54),
                ("m_DynamicOccludee", 55),
                ("m_MotionVectors", 57),
                ("m_LightProbeUsage", 58),
                ("m_ReflectionProbeUsage", 59),
            ] {
                scalars.insert(name.into(), Value::from(object[offset] as u32));
            }
            scalars.insert(
                "m_RenderingLayerMask".into(),
                Value::from(le32(&object[76..80])),
            );
            scalars.insert(
                "m_SortingLayerID".into(),
                Value::from(le32(&object[object.len() - 24..]) as i32),
            );
            scalars.insert(
                "m_SortingOrder".into(),
                Value::from(le16(&object[object.len() - 18..]) as i16),
            );
            g.r.pos = object.len() - 12;
            g.one("m_AdditionalVertexStreams")?;
        }
        _ => {}
    }
    Ok(ModelGraphObject {
        file_id: id.to_string(),
        class_id,
        references: g.references,
        scalars,
    })
}
