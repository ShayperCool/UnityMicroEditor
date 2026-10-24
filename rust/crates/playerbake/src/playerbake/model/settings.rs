//! A model's import settings, as its `.meta` holds them (`ModelImporter`).
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;

/// How the importer comes by normals or tangents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Import,
    Calculate,
    None,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// `globalScale`, the scale factor of the import.
    pub global_scale: f32,
    /// `useFileScale`: the file's unit converted to metres.
    pub use_file_scale: bool,
    pub weld_vertices: bool,
    pub normals: Source,
    pub normal_smoothing_angle: f32,
    /// `normalCalculationMode`: 0 and 1 unweighted, 2 by area, 3 by angle,
    /// 4 by area and angle.
    pub normal_weighting: i64,
    pub tangents: Source,
    pub swap_uvs: bool,
    pub keep_quads: bool,
    pub readable: bool,
    /// `indexFormat`: 0 automatic, 1 16-bit, 2 32-bit.
    pub index_format: i64,
    pub import_blend_shapes: bool,
    /// `animationType`: 0 none, 1 legacy, 2 generic, 3 humanoid.
    pub animation_type: i64,
    pub import_animation: bool,
    /// The file IDs the `.meta` records for objects, by class ID and name:
    /// `fileIDToRecycleName` of the format before Unity 2019.3, or
    /// `internalIDToNameTable`.
    pub recorded: HashMap<(i64, String), i64>,
    /// Whether the IDs of new objects follow the format before 2019.3.
    pub legacy_ids: bool,
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(*flag as u8 as f64),
        _ => None,
    }
}

fn find<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut at = value;
    for key in path {
        at = at.get(*key)?;
    }
    Some(at)
}

impl Settings {
    pub fn read(meta: &str) -> Result<Self> {
        let value = ume_base::yaml::parse_yaml_value(meta).context("model .meta")?;
        let importer = value
            .get("ModelImporter")
            .context("the .meta has no ModelImporter")?;
        let get = |path: &[&str]| number(find(importer, path));
        let flag = |path: &[&str], default: bool| get(path).map_or(default, |n| n != 0.0);
        let source = |path: &[&str], default: Source| match get(path).map(|n| n as i64) {
            Some(0) => Source::Import,
            Some(1) | Some(3) | Some(4) => Source::Calculate,
            Some(2) => Source::None,
            _ => default,
        };
        let mut recorded = HashMap::new();
        let mut legacy_ids = false;
        // Before 2019.3: `fileIDToRecycleName`, the class in the ID.
        if let Some(Value::Object(table)) = importer.get("fileIDToRecycleName") {
            legacy_ids = !table.is_empty();
            for (id, name) in table {
                let Ok(id) = id.trim().parse::<i64>() else {
                    continue;
                };
                let name = match name {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                recorded.insert((id / 100_000, name), id);
            }
        }
        // Since 2019.3: pairs of `first: {class: name}` and `second: id`.
        if let Some(Value::Array(table)) = importer.get("internalIDToNameTable") {
            for entry in table {
                let (Some(Value::Object(first)), Some(id)) =
                    (entry.get("first"), number(entry.get("second")))
                else {
                    continue;
                };
                for (class, name) in first {
                    let (Ok(class), Value::String(name)) = (class.trim().parse::<i64>(), name)
                    else {
                        continue;
                    };
                    recorded.insert((class, name.clone()), id as i64);
                }
            }
        }
        Ok(Self {
            global_scale: get(&["meshes", "globalScale"]).unwrap_or(1.0) as f32,
            use_file_scale: flag(&["meshes", "useFileScale"], true),
            weld_vertices: flag(&["meshes", "weldVertices"], true),
            normals: source(&["tangentSpace", "normalImportMode"], Source::Import),
            normal_smoothing_angle: get(&["tangentSpace", "normalSmoothAngle"]).unwrap_or(60.0)
                as f32,
            normal_weighting: get(&["tangentSpace", "normalCalculationMode"]).unwrap_or(4.0) as i64,
            tangents: source(&["tangentSpace", "tangentImportMode"], Source::Calculate),
            swap_uvs: flag(&["meshes", "swapUVChannels"], false),
            keep_quads: flag(&["meshes", "keepQuads"], false),
            readable: flag(&["isReadable"], false) || flag(&["meshes", "isReadable"], false),
            index_format: get(&["meshes", "indexFormat"]).unwrap_or(0.0) as i64,
            import_blend_shapes: flag(&["meshes", "importBlendShapes"], true),
            animation_type: get(&["animationType"]).unwrap_or(2.0) as i64,
            import_animation: flag(&["importAnimation"], true),
            recorded,
            legacy_ids,
        })
    }
}
