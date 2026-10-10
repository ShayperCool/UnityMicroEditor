use super::{
    Reader,
    adapt::{ExternalReference, Object, ObjectEntry},
    le16, le32, le64,
};
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
    for entry in entries {
        let id = entry.id.to_string();
        if !retained.contains(id.as_str()) {
            continue;
        }
        match entry.class_id {
            21 | 43 | 74 | 90 => continue, // Separate identities/data hashes cover assets.
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
