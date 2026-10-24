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

/// A clip the import makes of a take (`clipAnimations`).
#[derive(Debug, Clone, Default)]
pub struct ClipSettings {
    pub name: String,
    /// The clip's file ID as the `.meta` records it (`internalID`), or 0.
    pub internal_id: i64,
    pub take: String,
    /// First and last frame at the file's rate; `None` for the take's range.
    pub frames: Option<(f64, f64)>,
    pub loop_time: bool,
    pub loop_blend: bool,
    pub loop_blend_orientation: bool,
    pub loop_blend_position_y: bool,
    pub loop_blend_position_xz: bool,
    pub keep_original_orientation: bool,
    pub keep_original_position_y: bool,
    pub keep_original_position_xz: bool,
    pub height_from_feet: bool,
    pub mirror: bool,
    pub cycle_offset: f32,
    pub level: f32,
    pub orientation_offset_y: f32,
    pub wrap_mode: i64,
    /// Whether the clip has events or extra curves, which the tool does not
    /// import yet.
    pub extras: bool,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// The model's GUID.
    pub guid: String,
    /// `externalObjects`: materials the model's own are replaced by, by
    /// name, as references.
    pub external_materials: HashMap<String, Value>,
    /// Whether the import makes materials of the file's own
    /// (`materialImportMode`, before it `importMaterials`).
    pub import_materials: bool,
    /// The format before material descriptions (no `materialImportMode`)
    /// with materials kept as assets (`materialLocation` 0): the import
    /// finds `.mat` assets in `Materials` folders, named by `materialName`
    /// (0 the diffuse texture's name, 1 the material's, 2 the model's and
    /// the material's) and searched by `materialSearch` (0 beside the model,
    /// 1 up to `Assets`, 2 everywhere).
    pub legacy_materials: Option<(i64, i64)>,
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
    /// `preserveHierarchy`: the top node never becomes the root.
    pub preserve_hierarchy: bool,
    /// `sortHierarchyByName`.
    pub sort_by_name: bool,
    /// `nodeNameCollisionStrategy`: 1 per parent, 0 across the file.
    pub name_collisions: i64,
    /// A humanoid's extra root (`hasExtraRoot`, unless only with an avatar
    /// and there is none).
    pub extra_root: bool,
    /// Whether the import makes an avatar of the model: `avatarSetup` 1, or
    /// before it `copyAvatar` 0 with a rig.
    pub create_avatar: bool,
    pub human: super::avatar::Description,
    /// The clips of the takes; empty for one clip per take.
    pub clips: Vec<ClipSettings>,
    /// `animationCompression`: 0 off, 1 keyframe reduction, 2 and 3 more.
    pub animation_compression: i64,
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
                let id = match entry.get("second") {
                    Some(Value::Number(n)) => n.as_i64(),
                    Some(Value::String(t)) => t.trim().parse::<i64>().ok(),
                    _ => None,
                };
                let (Some(Value::Object(first)), Some(id)) = (entry.get("first"), id) else {
                    continue;
                };
                for (class, name) in first {
                    let (Ok(class), Value::String(name)) = (class.trim().parse::<i64>(), name)
                    else {
                        continue;
                    };
                    recorded.insert((class, name.clone()), id);
                }
            }
        }
        let mut clips = Vec::new();
        if let Some(Value::Array(list)) = find(importer, &["animations", "clipAnimations"]) {
            for clip in list {
                let get = |key: &str| number(clip.get(key));
                let flag = |key: &str| get(key).is_some_and(|n| n != 0.0);
                let text = |key: &str| match clip.get(key) {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                let listed = |key: &str| matches!(clip.get(key), Some(Value::Array(items)) if !items.is_empty());
                clips.push(ClipSettings {
                    name: text("name"),
                    internal_id: match clip.get("internalID") {
                        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                        Some(Value::String(t)) => t.trim().parse().unwrap_or(0),
                        _ => 0,
                    },
                    take: text("takeName"),
                    frames: get("firstFrame").zip(get("lastFrame")),
                    loop_time: flag("loopTime"),
                    loop_blend: flag("loopBlend"),
                    loop_blend_orientation: flag("loopBlendOrientation"),
                    loop_blend_position_y: flag("loopBlendPositionY"),
                    loop_blend_position_xz: flag("loopBlendPositionXZ"),
                    keep_original_orientation: flag("keepOriginalOrientation"),
                    keep_original_position_y: flag("keepOriginalPositionY"),
                    keep_original_position_xz: flag("keepOriginalPositionXZ"),
                    height_from_feet: flag("heightFromFeet"),
                    mirror: flag("mirror"),
                    cycle_offset: get("cycleOffset").unwrap_or(0.0) as f32,
                    level: get("level").unwrap_or(0.0) as f32,
                    orientation_offset_y: get("orientationOffsetY").unwrap_or(0.0) as f32,
                    wrap_mode: get("wrapMode").unwrap_or(0.0) as i64,
                    extras: listed("events") || listed("curves"),
                });
            }
        }
        let description = |key: &str, default: f64| {
            number(find(importer, &["humanDescription", key])).unwrap_or(default)
        };
        let human = super::avatar::Description {
            arm_twist: description("armTwist", 0.5) as f32,
            fore_arm_twist: description("foreArmTwist", 0.5) as f32,
            upper_leg_twist: description("upperLegTwist", 0.5) as f32,
            leg_twist: description("legTwist", 0.5) as f32,
            arm_stretch: description("armStretch", 0.05) as f32,
            leg_stretch: description("legStretch", 0.05) as f32,
            feet_spacing: description("feetSpacing", 0.0) as f32,
            global_scale: get(&["meshes", "globalScale"]).unwrap_or(1.0) as f32,
            has_translation_dof: description("hasTranslationDoF", 0.0) != 0.0,
            has_extra_root: description("hasExtraRoot", 0.0) != 0.0,
            skeleton_has_parents: description("skeletonHasParents", 1.0) != 0.0,
        };
        let create_avatar = match get(&["avatarSetup"]) {
            Some(setup) => setup == 1.0,
            None => !flag(&["copyAvatar"], false),
        };
        let mut external_materials = HashMap::new();
        if let Some(Value::Array(list)) = importer.get("externalObjects") {
            for entry in list {
                let (Some(first), Some(second)) = (entry.get("first"), entry.get("second")) else {
                    continue;
                };
                let is_material = first
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.ends_with("Material"));
                if let (true, Some(Value::String(name))) = (is_material, first.get("name")) {
                    external_materials.insert(name.clone(), second.clone());
                }
            }
        }
        let legacy_materials = (get(&["materials", "materialImportMode"]).is_none()
            && flag(&["materials", "importMaterials"], true)
            && get(&["materials", "materialLocation"]).unwrap_or(0.0) == 0.0)
            .then(|| {
                (
                    get(&["materials", "materialName"]).unwrap_or(0.0) as i64,
                    get(&["materials", "materialSearch"]).unwrap_or(1.0) as i64,
                )
            });
        let import_materials = match get(&["materials", "materialImportMode"]) {
            Some(mode) => mode != 0.0,
            None => flag(&["materials", "importMaterials"], true),
        };
        // A `.meta` of the format before 2019.3 knows neither setting: its
        // models kept their order and named repeats across the file.
        let current = importer.get("internalIDToNameTable").is_some();
        let extra_root = description("hasExtraRoot", 0.0) != 0.0
            && !(flag(&["addHumanoidExtraRootOnlyWhenUsingAvatar"], true)
                && get(&["avatarSetup"]) == Some(0.0));
        Ok(Self {
            preserve_hierarchy: flag(&["meshes", "preserveHierarchy"], false),
            sort_by_name: flag(&["meshes", "sortHierarchyByName"], current),
            name_collisions: get(&["meshes", "nodeNameCollisionStrategy"]).unwrap_or(if current {
                1.0
            } else {
                0.0
            }) as i64,
            extra_root,
            guid: value
                .get("guid")
                .map(|g| match g {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
            external_materials,
            import_materials,
            legacy_materials,
            create_avatar,
            human,
            clips,
            animation_compression: get(&["animations", "animationCompression"]).unwrap_or(1.0)
                as i64,
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

impl Settings {
    /// The material of a node's slot: one the `.meta` maps its material's
    /// name to, the one the import makes of it, else the engine's default
    /// material.
    pub fn material(
        &self,
        node: &ufbx::Node,
        slot: usize,
        embedded: &HashMap<String, i64>,
        found: &HashMap<String, Value>,
    ) -> Value {
        let name = node
            .materials
            .get(slot)
            .map(|m| m.element.name.to_string())
            .or_else(|| {
                node.mesh
                    .as_ref()
                    .and_then(|m| m.materials.get(slot).map(|m| m.element.name.to_string()))
            })
            .unwrap_or_default();
        if let Some(reference) = self.external_materials.get(&name) {
            return reference.clone();
        }
        if let Some(reference) = found.get(&name) {
            return reference.clone();
        }
        match embedded.get(&name) {
            Some(id) => serde_json::json!({
                "fileID": id.to_string(),
                "guid": self.guid,
                "type": "3"
            }),
            None => serde_json::json!({
                "fileID": "10303",
                "guid": "0000000000000000f000000000000000",
                "type": "0"
            }),
        }
    }
}
