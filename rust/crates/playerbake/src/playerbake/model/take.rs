//! A model's takes as the Editor's form of an `AnimationClip`: the local
//! position, rotation and scale of every node a take animates, sampled at
//! each frame of the file's rate in Unity's space, as an authored `.anim`
//! holds curves. The clip baker then makes the Player's clip of it.
//!
//! The importer reduces the keys it samples (`animationCompression`); this
//! keeps every frame, which plays the same motion with more keys.
use super::{
    settings::{ClipSettings, Settings},
    space::Space,
};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

/// A node the takes may animate: its path below the model's root.
pub struct Animated<'a> {
    pub node: &'a ufbx::Node,
    pub path: &'a str,
}

/// The clips of a model's takes, as the import settings name them, by name.
pub fn clip_settings(scene: &ufbx::Scene, settings: &Settings) -> Vec<ClipSettings> {
    if !settings.clips.is_empty() {
        return settings.clips.clone();
    }
    scene
        .anim_stacks
        .iter()
        .map(|stack| ClipSettings {
            name: stack.element.name.to_string(),
            take: stack.element.name.to_string(),
            keep_original_position_y: true,
            ..Default::default()
        })
        .collect()
}

fn vector3(v: [f32; 3]) -> Value {
    json!({ "x": v[0], "y": v[1], "z": v[2] })
}

fn vector4(v: [f32; 4]) -> Value {
    json!({ "x": v[0], "y": v[1], "z": v[2], "w": v[3] })
}

/// The keys of a curve of `N` components sampled at `times`, each with the
/// slopes through its neighbours.
fn keys<const N: usize>(times: &[f32], values: &[[f32; N]], wrap: fn([f32; N]) -> Value) -> Value {
    let count = values.len();
    let slope = |a: usize, b: usize| -> [f32; N] {
        let dt = times[b] - times[a];
        std::array::from_fn(|k| {
            if dt > 0.0 {
                (values[b][k] - values[a][k]) / dt
            } else {
                0.0
            }
        })
    };
    let weights = [1.0f32 / 3.0; N];
    let list: Vec<Value> = (0..count)
        .map(|i| {
            let tangent = match (i, count) {
                (_, 1) => [0.0; N],
                (0, _) => slope(0, 1),
                (i, n) if i == n - 1 => slope(n - 2, n - 1),
                (i, _) => slope(i - 1, i + 1),
            };
            json!({
                "serializedVersion": "3",
                "time": times[i],
                "value": wrap(values[i]),
                "inSlope": wrap(tangent),
                "outSlope": wrap(tangent),
                "tangentMode": "0",
                "weightedMode": "0",
                "inWeight": wrap(weights),
                "outWeight": wrap(weights),
            })
        })
        .collect();
    Value::Array(list)
}

fn curve(keys: Value, path: &str) -> Value {
    json!({
        "curve": {
            "serializedVersion": "2",
            "m_Curve": keys,
            "m_PreInfinity": "2",
            "m_PostInfinity": "2",
            "m_RotationOrder": "4",
        },
        "path": path,
    })
}

/// The Editor's fields of the clip of a take, or `None` when the file has
/// no such take.
pub fn clip(
    scene: &ufbx::Scene,
    nodes: &[Animated<'_>],
    clip: &ClipSettings,
    space: &Space,
) -> Option<Map<String, Value>> {
    // A take the file no longer has: the file's first, as the importer
    // falls back to it.
    let stack = scene
        .anim_stacks
        .iter()
        .find(|stack| stack.element.name.as_ref() == clip.take.as_str())
        .or_else(|| scene.anim_stacks.iter().next())?;
    let rate = match scene.settings.frames_per_second {
        rate if rate > 0.0 => rate,
        _ => 30.0,
    };
    let (first, last) = clip.frames.unwrap_or((
        (stack.time_begin * rate).round(),
        (stack.time_end * rate).round(),
    ));
    let frames = ((last - first).round().max(0.0)) as usize + 1;
    // The nodes the take animates.
    let mut animated: HashSet<u32> = HashSet::new();
    for layer in stack.layers.iter() {
        for prop in layer.anim_props.iter() {
            if matches!(
                prop.prop_name.as_ref(),
                "Lcl Translation" | "Lcl Rotation" | "Lcl Scaling"
            ) {
                animated.insert(prop.element.element_id);
            }
        }
    }
    let times: Vec<f32> = (0..frames).map(|f| (f as f64 / rate) as f32).collect();
    let (mut positions, mut rotations, mut scales) = (Vec::new(), Vec::new(), Vec::new());
    for node in nodes {
        if !animated.contains(&node.node.element.element_id) {
            continue;
        }
        let mut position = Vec::with_capacity(frames);
        let mut rotation: Vec<[f32; 4]> = Vec::with_capacity(frames);
        let mut scale = Vec::with_capacity(frames);
        for frame in 0..frames {
            let time = (first + frame as f64) / rate;
            let t = ufbx::evaluate_transform(&stack.anim, node.node, time);
            position.push(space.position([
                t.translation.x as f32,
                t.translation.y as f32,
                t.translation.z as f32,
            ]));
            let mut q = space.rotation([
                t.rotation.x as f32,
                t.rotation.y as f32,
                t.rotation.z as f32,
                t.rotation.w as f32,
            ]);
            // A quaternion and its negation turn alike: keep to the side of
            // the frame before.
            if let Some(before) = rotation.last() {
                let dot: f32 = (0..4).map(|k| before[k] * q[k]).sum();
                if dot < 0.0 {
                    q = q.map(|c| -c);
                }
            }
            rotation.push(q);
            scale.push([t.scale.x as f32, t.scale.y as f32, t.scale.z as f32]);
        }
        positions.push(curve(keys(&times, &position, vector3), node.path));
        rotations.push(curve(keys(&times, &rotation, vector4), node.path));
        scales.push(curve(keys(&times, &scale, vector3), node.path));
    }
    let flag = |on: bool| Value::String((on as u8).to_string());
    let stop = times.last().copied().unwrap_or(0.0);
    let fields = json!({
        "m_Name": clip.name,
        "m_Legacy": "0",
        "m_Compressed": "0",
        "m_UseHighQualityCurve": "1",
        "m_RotationCurves": rotations,
        "m_CompressedRotationCurves": [],
        "m_EulerCurves": [],
        "m_PositionCurves": positions,
        "m_ScaleCurves": scales,
        "m_FloatCurves": [],
        "m_PPtrCurves": [],
        "m_SampleRate": rate as f32,
        "m_WrapMode": clip.wrap_mode.to_string(),
        "m_Bounds": { "m_Center": vector3([0.0; 3]), "m_Extent": vector3([0.0; 3]) },
        "m_AnimationClipSettings": {
            "serializedVersion": "2",
            "m_AdditiveReferencePoseClip": { "fileID": "0" },
            "m_AdditiveReferencePoseTime": "0",
            "m_StartTime": "0",
            "m_StopTime": stop,
            "m_OrientationOffsetY": clip.orientation_offset_y,
            "m_Level": clip.level,
            "m_CycleOffset": clip.cycle_offset,
            "m_HasAdditiveReferencePose": "0",
            "m_LoopTime": flag(clip.loop_time),
            "m_LoopBlend": flag(clip.loop_blend),
            "m_LoopBlendOrientation": flag(clip.loop_blend_orientation),
            "m_LoopBlendPositionY": flag(clip.loop_blend_position_y),
            "m_LoopBlendPositionXZ": flag(clip.loop_blend_position_xz),
            "m_KeepOriginalOrientation": flag(clip.keep_original_orientation),
            "m_KeepOriginalPositionY": flag(clip.keep_original_position_y),
            "m_KeepOriginalPositionXZ": flag(clip.keep_original_position_xz),
            "m_HeightFromFeet": flag(clip.height_from_feet),
            "m_Mirror": flag(clip.mirror),
        },
        "m_EditorCurves": [],
        "m_EulerEditorCurves": [],
        "m_HasGenericRootTransform": "0",
        "m_HasMotionFloatCurves": "0",
        "m_Events": [],
    });
    match fields {
        Value::Object(map) => Some(map),
        _ => None,
    }
}
