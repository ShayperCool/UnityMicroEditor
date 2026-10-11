//! `AnimationClip` (class 74): authored curves to the Player's clip data.
//!
//! A legacy clip keeps its curves. Any other clip is evaluated by Mecanim from
//! `m_MuscleClip`, which the Editor derives during a build: every curve of a
//! binding goes to the streamed clip as cubic segments, or to the constant
//! clip when the whole binding never changes. Float curves named after the
//! humanoid rig drive it, and the build derives the root's start, stop and
//! average speed from them. The derivation below follows the pinned engine's
//! output bit for bit on the profiles it accepts.
use super::{
    Baked, clip_tree, crc32, humanoid, script_tree,
    serialized::{self, Object},
    source::{flag, float, get, int, list, null_reference, object, text},
    typetree::Val,
    unsupported,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// A script a clip refers to: the class its `MonoScript` object names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptIdentity {
    pub class_name: String,
    pub namespace: String,
    pub assembly: String,
    pub execution_order: i64,
    /// The local file ID the baked file gives the `MonoScript` object.
    pub local_id: i64,
}

impl ScriptIdentity {
    /// The `MonoScript` object that names the class for the Player.
    /// `properties_hash` is the hash of the class's layout, if it is known.
    pub(super) fn object(&self, properties_hash: [u8; 16]) -> Result<Object<'static>> {
        let hash = properties_hash
            .iter()
            .enumerate()
            .map(|(index, byte)| (format!("bytes[{index}]").into(), Val::U(*byte as u64)))
            .collect();
        Ok(Object {
            local_id: self.local_id,
            tree: script_tree()?,
            data: script_tree()?.encode(&Val::map([
                ("m_Name", Val::str(&self.class_name)),
                ("m_ExecutionOrder", Val::I(self.execution_order)),
                ("m_PropertiesHash", Val::Map(hash)),
                ("m_ClassName", Val::str(&self.class_name)),
                ("m_Namespace", Val::str(&self.namespace)),
                ("m_AssemblyName", Val::str(&self.assembly)),
            ]))?,
            script: None,
        })
    }
}

/// Resolves the scripts a clip binds curves or events to, and the clip an
/// additive reference pose is taken from.
pub trait ClipSources {
    fn script(&self, guid: &str) -> Result<ScriptIdentity>;
    /// The authored `.anim` source of another clip. A caller that cannot
    /// bake this clip again when that one changes leaves it refused.
    fn clip(&self, guid: &str) -> Result<Vec<u8>> {
        Err(unsupported(format!(
            "additive reference pose from clip {guid}"
        )))
    }
}

/// For clips that may not refer to scripts.
pub struct NoScripts;
impl ClipSources for NoScripts {
    fn script(&self, guid: &str) -> Result<ScriptIdentity> {
        Err(unsupported(format!("reference to script {guid}")))
    }
}

/// A local file ID for a script's `MonoScript` object, stable per script.
pub fn script_local_id(guid: &str) -> i64 {
    let digest = crate::sha256(format!("playerbake-monoscript\0{guid}").as_bytes());
    // 60 bits keep it positive and away from Unity's small fixed IDs.
    i64::from_str_radix(&digest[..15], 16).unwrap_or(1) | (1 << 40)
}

/// Collects the scripts a clip refers to and hands out pointers to them.
struct Scripts<'a> {
    sources: &'a dyn ClipSources,
    used: Vec<(String, ScriptIdentity)>,
}

impl Scripts<'_> {
    /// The pointer for an authored object reference that has to be a script.
    fn pointer(&mut self, value: &Value, what: &str) -> Result<Val> {
        if null_reference(value)? {
            return Ok(Val::Zero);
        }
        let map = object(value, what)?;
        let guid = text(get(map, "guid"))?.to_ascii_lowercase();
        // Every script asset holds its MonoScript at this file ID.
        if text(get(map, "fileID"))? != "11500000" || guid.is_empty() {
            return Err(unsupported(format!("{what} that is not a script")));
        }
        let identity = match self.used.iter().find(|(known, _)| *known == guid) {
            Some((_, identity)) => identity.clone(),
            None => {
                let identity = self.sources.script(&guid)?;
                self.used.push((guid, identity.clone()));
                identity
            }
        };
        Ok(Val::map([("m_PathID", Val::I(identity.local_id))]))
    }
}

/// The muscle clip's size in memory with no curves, as the engine reports it.
const MUSCLE_CLIP_BASE_SIZE: u64 = 2528;
/// The custom type of a binding that drives a curve of the humanoid rig.
const HUMAN_CURVE: u8 = 8;
/// Baked motion capture is authored as text of tens of megabytes.
pub const MAX_SOURCE_BYTES: usize = 256 << 20;

#[derive(Debug, Clone, Copy)]
struct Key {
    time: f32,
    value: f32,
    in_slope: f32,
    out_slope: f32,
}

#[derive(Debug, Clone, Default)]
struct Curve {
    keys: Vec<Key>,
}

/// One animated property: a Transform channel or a single float.
struct Binding {
    path: u32,
    attribute: u32,
    type_id: i64,
    custom_type: u8,
    script: Val,
    curves: Vec<Curve>,
}

/// The clip an additive reference pose is taken from, and at what time.
struct Reference {
    guid: String,
    file_id: String,
    time: f32,
}

struct Settings {
    root: humanoid::Settings,
    /// Set only when the pose takes effect: the clip asks for one and
    /// names a clip.
    reference: Option<Reference>,
    fields: Vec<(&'static str, Val)>,
}

/// The curves of a clip and what the engine concludes from where they sit.
struct Curves {
    /// The authored curve lists, which only a legacy clip keeps.
    retained: Vec<(&'static str, Val)>,
    bindings: Vec<Binding>,
    /// A position or rotation curve on the root object.
    generic_root: bool,
    /// A float curve that drives the humanoid motion transform.
    motion: bool,
}

/// The bindings of a muscle clip in the order of its value array: streamed
/// bindings first, then constant ones, each group in binding order.
struct Layout<'a> {
    animated: Vec<&'a Binding>,
    fixed: Vec<&'a Binding>,
}

impl<'a> Layout<'a> {
    fn new(bindings: &'a [Binding], high_quality: bool, sample_rate: f32) -> Result<Self> {
        let (animated, fixed): (Vec<&Binding>, Vec<&Binding>) =
            bindings
                .iter()
                .partition(|binding| match &binding.curves[..] {
                    [curve] => !constant_float(curve),
                    curves => !constant_vector(curves),
                });
        // The samples of a dense clip are not reproduced. For Transform
        // curves it is not even established which of them the engine
        // samples, so any that could qualify is refused as well.
        if !high_quality {
            for binding in &animated {
                let dense = |curve: &Curve| dense_candidate(curve, sample_rate);
                if binding.curves.iter().any(dense) {
                    return Err(unsupported(if binding.type_id == 4 {
                        "Transform curves the engine may store in the dense clip"
                    } else {
                        "float curves the engine stores in the dense clip"
                    }));
                }
            }
        }
        Ok(Self { animated, fixed })
    }

    fn bindings(&self) -> impl Iterator<Item = &'a Binding> + '_ {
        self.animated.iter().chain(&self.fixed).copied()
    }

    fn animated_curves(&self) -> Vec<&'a Curve> {
        self.animated.iter().flat_map(|b| &b.curves).collect()
    }

    fn fixed_curves(&self) -> Vec<&'a Curve> {
        self.fixed.iter().flat_map(|b| &b.curves).collect()
    }

    /// The value array at a time.
    fn sample(&self, time: f32) -> Vec<f32> {
        let animated = self.animated.iter().flat_map(|b| &b.curves);
        let fixed = self.fixed.iter().flat_map(|b| &b.curves);
        animated
            .map(|curve| evaluate(curve, time))
            .chain(fixed.map(|curve| curve.keys[0].value))
            .collect()
    }
}

fn vector(value: &Value, names: &[&str], what: &str) -> Result<Vec<f32>> {
    let map = object(value, what)?;
    names
        .iter()
        .map(|name| float(get(map, name), what))
        .collect()
}

fn vector_val(values: &[f32], names: &[&'static str]) -> Val {
    Val::Map(
        names
            .iter()
            .zip(values)
            .map(|(name, value)| ((*name).into(), Val::F(*value)))
            .collect(),
    )
}

/// The serialized `AnimationCurve` and its per-component keys.
fn read_curve(value: &Value, names: &[&'static str], what: &str) -> Result<(Val, Vec<Curve>)> {
    let map = object(value, what)?;
    let width = names.len().max(1);
    let mut curves = vec![Curve::default(); width];
    let mut keys = Vec::new();
    let part = |value: &Value| -> Result<Vec<f32>> {
        if names.is_empty() {
            Ok(vec![float(value, what)?])
        } else {
            vector(value, names, what)
        }
    };
    let part_val = |values: &[f32]| {
        if names.is_empty() {
            Val::F(values[0])
        } else {
            vector_val(values, names)
        }
    };
    let mut previous = f32::NEG_INFINITY;
    for item in list(get(map, "m_Curve"), what)? {
        let key = object(item, what)?;
        let time = float(get(key, "time"), what)?;
        if !time.is_finite() || time <= previous {
            return Err(unsupported(format!(
                "{what} has keys that are not in increasing time order"
            )));
        }
        previous = time;
        let weighted = int(get(key, "weightedMode"), what)?;
        if weighted != 0 {
            return Err(unsupported(format!("{what} uses weighted tangents")));
        }
        let values = part(get(key, "value"))?;
        let in_slopes = part(get(key, "inSlope"))?;
        let out_slopes = part(get(key, "outSlope"))?;
        let in_weights = part(get(key, "inWeight"))?;
        let out_weights = part(get(key, "outWeight"))?;
        for index in 0..width {
            let slope_ok = |slope: f32| slope.is_finite() || slope == f32::INFINITY;
            if !values[index].is_finite()
                || !slope_ok(in_slopes[index])
                || !slope_ok(out_slopes[index])
            {
                return Err(unsupported(format!(
                    "{what} has a non-finite value or a negative infinite tangent"
                )));
            }
            curves[index].keys.push(Key {
                time,
                value: values[index],
                in_slope: in_slopes[index],
                out_slope: out_slopes[index],
            });
        }
        keys.push(Val::map([
            ("time", Val::F(time)),
            ("value", part_val(&values)),
            ("inSlope", part_val(&in_slopes)),
            ("outSlope", part_val(&out_slopes)),
            ("weightedMode", Val::I(weighted)),
            ("inWeight", part_val(&in_weights)),
            ("outWeight", part_val(&out_weights)),
        ]));
    }
    let serialized = Val::map([
        ("m_Curve", Val::Seq(keys)),
        (
            "m_PreInfinity",
            Val::I(int(get(map, "m_PreInfinity"), what)?),
        ),
        (
            "m_PostInfinity",
            Val::I(int(get(map, "m_PostInfinity"), what)?),
        ),
        (
            "m_RotationOrder",
            Val::I(int(get(map, "m_RotationOrder"), what)?),
        ),
    ]);
    Ok((serialized, curves))
}

/// The engine's binding of a float curve: attribute and custom type. An
/// Animator float named exactly like a curve of the humanoid rig drives that
/// curve, whatever its path; any other name is a parameter.
fn float_binding(class_id: i64, attribute: &str, scripted: bool) -> Result<(u32, u8)> {
    if scripted != (class_id == 114) {
        return Err(unsupported(format!(
            "float curve {attribute} of class {class_id} with an unexpected script reference"
        )));
    }
    let hash = crc32(attribute);
    match (class_id, attribute) {
        // A field of a script: the hash of its property path.
        (114, _) => Ok((hash, 0)),
        (95, _) => Ok(match humanoid::curve_index(hash) {
            Some(index) => (index, HUMAN_CURVE),
            None => (hash, 0),
        }),
        (1, "m_IsActive") => Ok((hash, 0)),
        _ => Err(unsupported(format!(
            "float curve binding of class {class_id}: {attribute}"
        ))),
    }
}

/// The engine's comparison epsilon for a curve that never changes.
const EPSILON: f32 = 0.000001;

/// Whether the engine stores a float curve as one value: it is streamed as
/// soon as a value or a tangent exceeds the epsilon. A stepped key has an
/// infinite tangent, so a held curve stays streamed.
fn constant_float(curve: &Curve) -> bool {
    let first = curve.keys[0].value;
    !curve.keys.iter().any(|key| {
        EPSILON < (key.value - first).abs()
            || EPSILON < key.in_slope.abs()
            || EPSILON < key.out_slope.abs()
    })
}

/// Whether the engine would sample a changing curve into the dense clip
/// when the clip does not ask for high quality curves: no key is stepped,
/// four bytes a frame take no more than the 28 bytes a streamed key takes,
/// and no two keys are closer than a frame, less half a millisecond.
fn dense_candidate(curve: &Curve, sample_rate: f32) -> bool {
    use std::cmp::Ordering::Greater;
    let keys = &curve.keys;
    let frame = 1.0 / sample_rate;
    let span = keys[keys.len() - 1].time - keys[0].time;
    let streamed = (28 * keys.len()) as f32;
    !keys
        .iter()
        .any(|key| !key.in_slope.is_finite() || !key.out_slope.is_finite())
        && (span / frame * 4.0).partial_cmp(&streamed) != Some(Greater)
        && keys.windows(2).all(|pair| {
            let spare = (pair[1].time - pair[0].time).abs() - frame;
            (-0.0005f32).partial_cmp(&spare) != Some(Greater)
        })
}

/// Whether the engine stores a Transform binding as one value per curve. Its
/// keys are compared as vectors: every key stays within the epsilon of the
/// first value and no tangent bends the curve.
fn constant_vector(curves: &[Curve]) -> bool {
    let near_zero = |parts: &mut dyn Iterator<Item = f32>| {
        parts.map(|part| part * part).sum::<f32>() < EPSILON * EPSILON
    };
    (0..curves[0].keys.len()).all(|index| {
        near_zero(&mut curves.iter().map(|c| c.keys[index].value - c.keys[0].value))
            && near_zero(&mut curves.iter().map(|c| c.keys[index].in_slope))
            && near_zero(&mut curves.iter().map(|c| c.keys[index].out_slope))
    })
}

/// Cubic coefficients of the segment between two keys, in the engine's
/// single-precision order of operations, and the value at its start.
fn segment(lhs: &Key, rhs: &Key) -> [f32; 4] {
    if lhs.out_slope == f32::INFINITY || rhs.in_slope == f32::INFINITY {
        return [0.0, 0.0, 0.0, lhs.value];
    }
    let dx = (rhs.time - lhs.time).max(0.0001);
    let dy = rhs.value - lhs.value;
    let reciprocal = 1.0 / dx;
    let length = reciprocal * reciprocal;
    let d1 = lhs.out_slope * dx;
    let d2 = rhs.in_slope * dx;
    [
        (d1 + d2 - dy - dy) * length * reciprocal,
        (dy + dy + dy - d1 - d1 - d2) * length,
        lhs.out_slope,
        lhs.value,
    ]
}

/// Samples the streamed form of a curve the way the engine does: always
/// through the cubic of the frame in effect, so `-0` plus a zero term is `+0`.
fn evaluate(curve: &Curve, time: f32) -> f32 {
    let keys = &curve.keys;
    let last = keys.len() - 1;
    let ([a, b, c, d], t) = if time < keys[0].time {
        ([0.0, 0.0, 0.0, keys[0].value], 1.0)
    } else if time >= keys[last].time {
        ([0.0, 0.0, 0.0, keys[last].value], time - keys[last].time)
    } else {
        let index = keys.partition_point(|key| key.time <= time) - 1;
        (
            segment(&keys[index], &keys[index + 1]),
            time - keys[index].time,
        )
    };
    t * (t * (t * a + b) + c) + d
}

fn streamed(curves: &[&Curve]) -> Vec<u32> {
    // (time, curve index, coefficients and value)
    let mut entries: Vec<(f32, usize, [f32; 4])> = Vec::new();
    for (index, curve) in curves.iter().enumerate() {
        for (position, key) in curve.keys.iter().enumerate() {
            let coefficients = match curve.keys.get(position + 1) {
                Some(next) => segment(key, next),
                None => [0.0, 0.0, 0.0, key.value],
            };
            entries.push((key.time, index, coefficients));
        }
    }
    entries.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut data = Vec::new();
    if !curves.is_empty() {
        // The frame before every key holds each curve at its first value.
        data.push(f32::MIN.to_bits());
        data.push(curves.len() as u32);
        for (index, curve) in curves.iter().enumerate() {
            data.push(index as u32);
            data.extend([0.0f32, 0.0, 0.0, curve.keys[0].value].map(f32::to_bits));
        }
        let mut start = 0;
        while start < entries.len() {
            let time = entries[start].0;
            let mut end = start;
            while end < entries.len() && entries[end].0 == time {
                end += 1;
            }
            data.push(time.to_bits());
            data.push((end - start) as u32);
            for (_, index, coefficients) in &entries[start..end] {
                data.push(*index as u32);
                data.extend(coefficients.map(f32::to_bits));
            }
            start = end;
        }
    }
    data.push(f32::INFINITY.to_bits());
    data.push(0);
    data
}

fn transform_val(transform: &humanoid::Transform) -> Val {
    Val::map([
        ("t", vector_val(&transform.0, &["x", "y", "z"])),
        ("q", vector_val(&transform.1, &["x", "y", "z", "w"])),
        ("s", vector_val(&[1.0; 3], &["x", "y", "z"])),
    ])
}

fn pose_val(pose: &humanoid::Pose) -> Val {
    let goal = |goal: &humanoid::Transform| Val::map([("m_X", transform_val(goal))]);
    let hand = |fingers: &[f32]| {
        Val::map([
            ("m_GrabX", transform_val(&humanoid::IDENTITY)),
            ("m_DoFArray", Val::floats(fingers.iter().copied())),
        ])
    };
    let translation = |translation: &[f32; 3]| vector_val(translation, &["x", "y", "z"]);
    Val::map([
        ("m_RootX", transform_val(&pose.root)),
        (
            "m_GoalArray",
            Val::Seq(pose.goals.iter().map(goal).collect()),
        ),
        ("m_LeftHandPose", hand(&pose.left_fingers)),
        ("m_RightHandPose", hand(&pose.right_fingers)),
        ("m_DoFArray", Val::floats(pose.muscles)),
        (
            "m_TDoFArray",
            Val::Seq(pose.translations.iter().map(translation).collect()),
        ),
    ])
}

fn settings(fields: &Map<String, Value>) -> Result<Settings> {
    let what = "m_AnimationClipSettings";
    let map = match get(fields, what) {
        Value::Object(map) => map,
        Value::Null => bail!("animation clip has no {what}"),
        _ => bail!("{what} is not a mapping"),
    };
    let number = |key: &str| float(get(map, key), what);
    let boolean = |key: &str| flag(get(map, key), what);
    // The flag without a clip, or a clip without the flag, changes nothing.
    let clip = get(map, "m_AdditiveReferencePoseClip");
    let mut reference = None;
    if boolean("m_HasAdditiveReferencePose")? && !null_reference(clip)? {
        let clip = object(clip, what)?;
        reference = Some(Reference {
            guid: text(get(clip, "guid"))?.to_ascii_lowercase(),
            file_id: text(get(clip, "fileID"))?,
            time: number("m_AdditiveReferencePoseTime")?,
        });
    }
    Ok(Settings {
        root: humanoid::Settings {
            start: number("m_StartTime")?,
            stop: number("m_StopTime")?,
            orientation_offset_y: number("m_OrientationOffsetY")?,
            mirror: boolean("m_Mirror")?,
            loop_blend_orientation: boolean("m_LoopBlendOrientation")?,
            loop_blend_position_y: boolean("m_LoopBlendPositionY")?,
            loop_blend_position_xz: boolean("m_LoopBlendPositionXZ")?,
            keep_original_orientation: boolean("m_KeepOriginalOrientation")?,
        },
        reference,
        fields: vec![
            ("m_StartTime", Val::F(number("m_StartTime")?)),
            ("m_StopTime", Val::F(number("m_StopTime")?)),
            (
                "m_OrientationOffsetY",
                Val::F(number("m_OrientationOffsetY")?),
            ),
            ("m_Level", Val::F(number("m_Level")?)),
            ("m_CycleOffset", Val::F(number("m_CycleOffset")?)),
            ("m_Mirror", Val::Bool(boolean("m_Mirror")?)),
            ("m_LoopTime", Val::Bool(boolean("m_LoopTime")?)),
            ("m_LoopBlend", Val::Bool(boolean("m_LoopBlend")?)),
            (
                "m_LoopBlendOrientation",
                Val::Bool(boolean("m_LoopBlendOrientation")?),
            ),
            (
                "m_LoopBlendPositionY",
                Val::Bool(boolean("m_LoopBlendPositionY")?),
            ),
            (
                "m_LoopBlendPositionXZ",
                Val::Bool(boolean("m_LoopBlendPositionXZ")?),
            ),
            ("m_StartAtOrigin", Val::Bool(true)),
            (
                "m_KeepOriginalOrientation",
                Val::Bool(boolean("m_KeepOriginalOrientation")?),
            ),
            (
                "m_KeepOriginalPositionY",
                Val::Bool(boolean("m_KeepOriginalPositionY")?),
            ),
            (
                "m_KeepOriginalPositionXZ",
                Val::Bool(boolean("m_KeepOriginalPositionXZ")?),
            ),
            ("m_HeightFromFeet", Val::Bool(boolean("m_HeightFromFeet")?)),
        ],
    })
}

fn events(fields: &Map<String, Value>, scripts: &mut Scripts<'_>) -> Result<Val> {
    let what = "m_Events";
    let mut result = Vec::new();
    for item in list(get(fields, what), what)? {
        let event = object(item, what)?;
        let parameter = scripts.pointer(
            get(event, "objectReferenceParameter"),
            "animation event object reference",
        )?;
        result.push(Val::map([
            ("objectReferenceParameter", parameter),
            ("time", Val::F(float(get(event, "time"), what)?)),
            ("functionName", Val::Str(text(get(event, "functionName"))?)),
            ("data", Val::Str(text(get(event, "data"))?)),
            (
                "floatParameter",
                Val::F(float(get(event, "floatParameter"), what)?),
            ),
            (
                "intParameter",
                Val::I(int(get(event, "intParameter"), what)?),
            ),
            (
                "messageOptions",
                Val::I(int(get(event, "messageOptions"), what)?),
            ),
        ]));
    }
    Ok(Val::Seq(result))
}

fn bounds(fields: &Map<String, Value>) -> Result<Val> {
    let Value::Object(map) = get(fields, "m_Bounds") else {
        return Ok(Val::Zero);
    };
    let axes = ["x", "y", "z"];
    Ok(Val::map([
        (
            "m_Center",
            vector_val(&vector(get(map, "m_Center"), &axes, "m_Bounds")?, &axes),
        ),
        (
            "m_Extent",
            vector_val(&vector(get(map, "m_Extent"), &axes, "m_Bounds")?, &axes),
        ),
    ]))
}

/// The serialized `AnimationClip` object of an authored `.anim` document and
/// the scripts it points to, each of which needs a `MonoScript` object with
/// its local ID in the same file.
pub fn bake_clip_object(
    fields: &Map<String, Value>,
    sources: &dyn ClipSources,
) -> Result<(Vec<u8>, Vec<ScriptIdentity>)> {
    let mut scripts = Scripts {
        sources,
        used: Vec::new(),
    };
    let data = clip_object(fields, &mut scripts)?;
    Ok((
        data,
        scripts
            .used
            .into_iter()
            .map(|(_, identity)| identity)
            .collect(),
    ))
}

fn read_curves(
    fields: &Map<String, Value>,
    scripts: &mut Scripts<'_>,
    legacy: bool,
) -> Result<Curves> {
    let what = "animation clip";
    if !list(get(fields, "m_CompressedRotationCurves"), what)?.is_empty() {
        return Err(unsupported("compressed rotation curves"));
    }
    if !list(get(fields, "m_PPtrCurves"), what)?.is_empty() {
        return Err(unsupported("object reference curves"));
    }
    let (xyz, xyzw): (&[&'static str], &[&'static str]) = (&["x", "y", "z"], &["x", "y", "z", "w"]);
    let mut retained: Vec<(&'static str, Val)> = Vec::new();
    let mut bindings: Vec<Binding> = Vec::new();
    let (mut generic_root, mut motion) = (false, false);
    // Bindings follow the engine's order: position, rotation, Euler rotation,
    // scale and then floats, each in authored order.
    for (field, names, attribute, custom_type) in [
        ("m_PositionCurves", xyz, 1u32, 0u8),
        ("m_RotationCurves", xyzw, 2, 0),
        ("m_EulerCurves", xyz, 4, 4),
        ("m_ScaleCurves", xyz, 3, 0),
    ] {
        let mut serialized = Vec::new();
        for item in list(get(fields, field), field)? {
            let entry = object(item, field)?;
            let path = text(get(entry, "path"))?;
            let (curve, curves) = read_curve(get(entry, "curve"), names, field)?;
            if curves[0].keys.is_empty() {
                return Err(unsupported(format!("{field} has a curve without keys")));
            }
            // Moving or turning the root object is root motion; scaling
            // it is not.
            generic_root |= path.is_empty() && field != "m_ScaleCurves";
            bindings.push(Binding {
                path: crc32(&path),
                attribute,
                type_id: 4,
                custom_type,
                script: Val::Zero,
                curves,
            });
            serialized.push(Val::map([("curve", curve), ("path", Val::Str(path))]));
        }
        retained.push((field, Val::Seq(serialized)));
    }
    let mut floats = Vec::new();
    for item in list(get(fields, "m_FloatCurves"), "m_FloatCurves")? {
        let entry = object(item, "m_FloatCurves")?;
        let path = text(get(entry, "path"))?;
        let attribute = text(get(entry, "attribute"))?;
        let class_id = int(get(entry, "classID"), "m_FloatCurves")?;
        let (curve, curves) = read_curve(get(entry, "curve"), &[], "m_FloatCurves")?;
        if curves[0].keys.is_empty() {
            return Err(unsupported("m_FloatCurves has a curve without keys"));
        }
        let script = scripts.pointer(get(entry, "script"), "float curve script")?;
        let flags = int(get(entry, "flags"), "m_FloatCurves")?;
        if !legacy {
            // The first flag marks a curve of whole numbers, which the
            // engine keeps apart from the float curves.
            if flags & 1 != 0 {
                return Err(unsupported(format!("discrete curve {attribute}")));
            }
            let (hash, custom_type) = float_binding(class_id, &attribute, script != Val::Zero)?;
            motion |= custom_type == HUMAN_CURVE && humanoid::motion_curve(hash);
            bindings.push(Binding {
                path: crc32(&path),
                attribute: hash,
                type_id: class_id,
                custom_type,
                script: script.clone(),
                curves,
            });
        }
        floats.push(Val::map([
            ("curve", curve),
            ("attribute", Val::Str(attribute)),
            ("path", Val::Str(path)),
            ("classID", Val::I(class_id)),
            ("script", script),
            ("flags", Val::I(flags)),
        ]));
    }
    retained.push(("m_FloatCurves", Val::Seq(floats)));
    Ok(Curves {
        retained,
        bindings,
        generic_root,
        motion,
    })
}

/// Whether the clip asks for every changing curve to stay streamed, which is
/// the engine's default when the source does not say.
fn high_quality(fields: &Map<String, Value>) -> Result<bool> {
    match get(fields, "m_UseHighQualityCurve") {
        Value::Null => Ok(true),
        value => flag(value, "animation clip"),
    }
}

/// The value of each curve of the clip in the pose its additive reference
/// names: the reference clip's value at the reference time where that clip
/// animates the same binding, zero elsewhere.
fn reference_pose(
    reference: &Reference,
    layout: &Layout<'_>,
    sources: &dyn ClipSources,
) -> Result<Vec<f32>> {
    let what = "additive reference pose";
    if layout.bindings().any(|binding| binding.script != Val::Zero) {
        return Err(unsupported(format!("{what} of a clip with script curves")));
    }
    if reference.guid.is_empty() {
        return Err(unsupported(format!("{what} from a clip without a source")));
    }
    let yaml = sources.clip(&reference.guid)?;
    if yaml.len() > MAX_SOURCE_BYTES {
        return Err(unsupported(format!("{what} from a source too large")));
    }
    let documents = crate::yaml::parse_unity_yaml_lexical_within(&yaml, MAX_SOURCE_BYTES)?;
    let [document] = documents.as_slice() else {
        return Err(unsupported(format!(
            "{what} from a source of several objects"
        )));
    };
    if document.class_id != 74 || document.stripped || document.file_id != reference.file_id {
        return Err(unsupported(format!(
            "{what} from an object that is not the clip"
        )));
    }
    let fields = &document.data;
    let mut scripts = Scripts {
        sources,
        used: Vec::new(),
    };
    // The reference is read like any clip, without its own reference pose.
    let curves = read_curves(fields, &mut scripts, false)?;
    if flag(get(fields, "m_Legacy"), what)? || curves.bindings.is_empty() {
        return Err(unsupported(format!(
            "{what} from a clip without a muscle clip"
        )));
    }
    let sample_rate = float(get(fields, "m_SampleRate"), what)?;
    let other = Layout::new(&curves.bindings, high_quality(fields)?, sample_rate)?;
    let values = other.sample(reference.time);
    let key = |binding: &Binding| {
        (
            binding.path,
            binding.attribute,
            binding.type_id,
            binding.custom_type,
        )
    };
    let rotation = |binding: &Binding| binding.type_id == 4 && matches!(binding.attribute, 2 | 4);
    let mut pose = Vec::new();
    for binding in layout.bindings() {
        // Of several equal bindings in the reference, the last one counts.
        let mut position = 0;
        let mut found = None;
        for candidate in other.bindings() {
            if key(candidate) == key(binding) {
                found = Some(position);
            }
            position += candidate.curves.len();
        }
        match found {
            Some(position) => pose.extend(&values[position..position + binding.curves.len()]),
            // The engine converts between a quaternion and an Euler curve
            // of one object; that conversion is not reproduced.
            None if rotation(binding)
                && other
                    .bindings()
                    .any(|candidate| rotation(candidate) && candidate.path == binding.path) =>
            {
                return Err(unsupported(format!(
                    "{what} between Euler and quaternion rotation curves"
                )));
            }
            None => pose.extend(std::iter::repeat_n(0.0, binding.curves.len())),
        }
    }
    Ok(pose)
}

/// The muscle clip fields a build derives from the human curves; for a clip
/// without curves they stay at rest.
fn derived_fields(derived: Option<&humanoid::Derived>) -> Vec<(&'static str, Val)> {
    let rest = transform_val(&humanoid::IDENTITY);
    let Some(derived) = derived else {
        return vec![
            ("m_DeltaPose", pose_val(&humanoid::Pose::identity())),
            ("m_StartX", rest.clone()),
            ("m_StopX", rest.clone()),
            ("m_LeftFootStartX", rest.clone()),
            ("m_RightFootStartX", rest),
        ];
    };
    vec![
        ("m_DeltaPose", pose_val(&derived.delta_pose)),
        ("m_StartX", transform_val(&derived.start)),
        ("m_StopX", transform_val(&derived.stop)),
        ("m_LeftFootStartX", transform_val(&derived.left_foot_start)),
        (
            "m_RightFootStartX",
            transform_val(&derived.right_foot_start),
        ),
        (
            "m_AverageSpeed",
            vector_val(&derived.average_speed, &["x", "y", "z"]),
        ),
        (
            "m_AverageAngularSpeed",
            Val::F(derived.average_angular_speed),
        ),
    ]
}

fn clip_object(fields: &Map<String, Value>, scripts: &mut Scripts<'_>) -> Result<Vec<u8>> {
    let what = "animation clip";
    let legacy = flag(get(fields, "m_Legacy"), what)?;
    let settings = settings(fields)?;
    let Curves {
        retained,
        bindings,
        generic_root,
        motion,
    } = read_curves(fields, scripts, legacy)?;
    let high_quality = high_quality(fields)?;
    let sample_rate = float(get(fields, "m_SampleRate"), what)?;

    let mut object: Vec<(&'static str, Val)> = vec![
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        ("m_Legacy", Val::Bool(legacy)),
        (
            "m_Compressed",
            Val::Bool(flag(get(fields, "m_Compressed"), what)?),
        ),
        ("m_UseHighQualityCurve", Val::Bool(high_quality)),
        ("m_SampleRate", Val::F(sample_rate)),
        ("m_WrapMode", Val::I(int(get(fields, "m_WrapMode"), what)?)),
        ("m_Bounds", bounds(fields)?),
        ("m_Events", events(fields, scripts)?),
    ];
    let mut muscle = settings.fields;
    // Without curves the engine leaves the muscle clip unbuilt.
    if legacy || bindings.is_empty() {
        // A build derives the root motion flags and the reference pose from
        // the muscle clip. What it makes of them without one is not known.
        if flag(get(fields, "m_HasGenericRootTransform"), what)?
            || flag(get(fields, "m_HasMotionFloatCurves"), what)?
        {
            return Err(unsupported(
                "root motion flags on a clip without a muscle clip",
            ));
        }
        if settings.reference.is_some() {
            return Err(unsupported(
                "additive reference pose on a clip without a muscle clip",
            ));
        }
        if legacy {
            object.extend(retained);
        }
        muscle.extend(derived_fields(None));
        muscle.push((
            "m_IndexArray",
            Val::Seq(vec![Val::I(-1); humanoid::CURVE_COUNT]),
        ));
        object.push(("m_MuscleClip", Val::Map(named(muscle))));
        return clip_tree()?.encode(&Val::Map(named(object)));
    }
    let layout = Layout::new(&bindings, high_quality, sample_rate)?;
    let animated_curves = layout.animated_curves();
    let fixed_curves = layout.fixed_curves();
    if animated_curves.len() > u16::MAX as usize {
        return Err(unsupported("more streamed curves than the format indexes"));
    }
    let stream = streamed(&animated_curves);
    let all_curves = || animated_curves.iter().chain(&fixed_curves);
    // Each human curve points at its place in the value array; of several
    // bindings to one curve the last counts.
    let mut index: humanoid::Index = [-1; humanoid::CURVE_COUNT];
    let mut position = 0;
    for binding in layout.bindings() {
        if binding.custom_type == HUMAN_CURVE {
            index[binding.attribute as usize] = position;
        }
        position += binding.curves.len() as i32;
    }
    let derived = humanoid::derive(&index, &settings.root, &|time| layout.sample(time))?;
    muscle.extend(derived_fields(Some(&derived)));
    muscle.push((
        "m_IndexArray",
        Val::Seq(
            index
                .iter()
                .map(|&position| Val::I(position as i64))
                .collect(),
        ),
    ));
    let delta: Vec<Val> = layout
        .sample(settings.root.start)
        .into_iter()
        .zip(layout.sample(settings.root.stop))
        .map(|(start, stop)| Val::map([("m_Start", Val::F(start)), ("m_Stop", Val::F(stop))]))
        .collect();
    if let Some(reference) = &settings.reference {
        let pose = reference_pose(reference, &layout, scripts.sources)?;
        muscle.push(("m_ValueArrayReferencePose", Val::floats(pose)));
    }
    let mut dense = Vec::new();
    if all_curves().next().is_some() {
        let begin = all_curves()
            .map(|curve| curve.keys[0].time)
            .fold(f32::INFINITY, f32::min);
        let end = all_curves()
            .map(|curve| curve.keys[curve.keys.len() - 1].time)
            .fold(f32::NEG_INFINITY, f32::max);
        dense = vec![
            (
                "m_FrameCount",
                Val::I(((end - begin) * sample_rate).floor() as i64 + 2),
            ),
            ("m_SampleRate", Val::F(sample_rate)),
            ("m_BeginTime", Val::F(begin)),
        ];
    }
    let curve_count = animated_curves.len() + fixed_curves.len();
    let size = MUSCLE_CLIP_BASE_SIZE
        + 4 * stream.len() as u64
        + 4 * fixed_curves.len() as u64
        + 8 * curve_count as u64;
    muscle.push((
        "m_Clip",
        Val::map([
            (
                "m_StreamedClip",
                Val::map([
                    (
                        "data",
                        Val::Seq(stream.iter().map(|&word| Val::U(word as u64)).collect()),
                    ),
                    ("curveCount", Val::U(animated_curves.len() as u64)),
                ]),
            ),
            ("m_DenseClip", Val::Map(named(dense))),
            (
                "m_ConstantClip",
                Val::map([(
                    "data",
                    Val::floats(fixed_curves.iter().map(|curve| curve.keys[0].value)),
                )]),
            ),
        ]),
    ));
    muscle.push(("m_ValueArrayDelta", Val::Seq(delta)));
    let generic: Vec<Val> = layout
        .bindings()
        .map(|binding| {
            Val::map([
                ("path", Val::U(binding.path as u64)),
                ("attribute", Val::U(binding.attribute as u64)),
                ("script", binding.script.clone()),
                ("typeID", Val::I(binding.type_id)),
                ("customType", Val::U(binding.custom_type as u64)),
            ])
        })
        .collect();
    object.push(("m_MuscleClipSize", Val::U(size)));
    object.push(("m_MuscleClip", Val::Map(named(muscle))));
    // Both flags follow from the curves; the authored values are not used.
    object.push(("m_HasGenericRootTransform", Val::Bool(generic_root)));
    object.push(("m_HasMotionFloatCurves", Val::Bool(motion)));
    object.push((
        "m_ClipBindingConstant",
        Val::map([("genericBindings", Val::Seq(generic))]),
    ));
    clip_tree()?.encode(&Val::Map(named(object)))
}

fn named(fields: Vec<(&'static str, Val)>) -> Vec<(std::borrow::Cow<'static, str>, Val)> {
    fields
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect()
}

/// Bakes an authored `.anim` source into a ContentFile. Scripts the clip
/// points to are written into the same file as `MonoScript` objects, which
/// the Player ties to the loaded class by name.
pub fn bake_clip(yaml: &[u8], sources: &dyn ClipSources) -> Result<Baked> {
    if yaml.len() > MAX_SOURCE_BYTES {
        return Err(unsupported("animation source larger than the baker reads"));
    }
    let documents = crate::yaml::parse_unity_yaml_lexical_within(yaml, MAX_SOURCE_BYTES)?;
    let [document] = documents.as_slice() else {
        return Err(unsupported("animation source with several objects"));
    };
    if document.class_id != 74 || document.stripped {
        bail!("animation source does not hold an AnimationClip");
    }
    let local_id: i64 = document.file_id.parse().context("animation clip file ID")?;
    let (data, scripts) = bake_clip_object(&document.data, sources)?;
    let mut objects = Vec::new();
    for script in &scripts {
        if script.local_id == local_id {
            bail!("script object identity collides with its clip");
        }
        // A clip is bound to a script by name alone, so the hash of the
        // class's layout stays empty.
        objects.push(script.object([0; 16])?);
    }
    objects.push(Object {
        local_id,
        tree: clip_tree()?,
        data,
        script: None,
    });
    let file = serialized::write(&objects, &[])?;
    Ok(Baked {
        file,
        objects: vec![(
            document.file_id.clone(),
            "UnityEngine.AnimationClip".to_string(),
        )],
        externals: Vec::new(),
    })
}

/// Turns a decoded editor-form clip into the mapping an authored `.anim`
/// parses to. Both come from the same serialization, so the field names
/// agree; scalars become the text Unity would have written.
fn authored(value: &Val) -> Value {
    match value {
        Val::Zero => Value::Null,
        Val::Bool(flag) => Value::String((*flag as u8).to_string()),
        Val::I(number) => Value::String(number.to_string()),
        Val::U(number) => Value::String(number.to_string()),
        // The shortest text that reads back as the same single.
        Val::D(number) => Value::String(number.to_string()),
        Val::F(number) => Value::String(if number.is_infinite() {
            if *number > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_string()
        } else {
            number.to_string()
        }),
        Val::Str(text) => Value::String(text.clone()),
        Val::Seq(items) => Value::Array(items.iter().map(authored).collect()),
        Val::Map(fields) => {
            // An object pointer; only an empty one has an authored form here.
            if let (Some(file), Some(path)) = (value.field("m_FileID"), value.field("m_PathID")) {
                let null = matches!((file, path), (Val::I(0), Val::I(0)));
                let id = if null { "0" } else { "1" };
                return Value::Object(Map::from_iter([(
                    "fileID".to_string(),
                    Value::String(id.to_string()),
                )]));
            }
            Value::Object(
                fields
                    .iter()
                    .map(|(name, field)| (name.to_string(), authored(field)))
                    .collect(),
            )
        }
    }
}

/// The clips of an imported model, baked from the import worker's artifact.
pub struct ModelClips {
    /// One ContentFile holding every clip that could be baked.
    pub baked: Option<Baked>,
    /// Name and loop setting of each baked clip, by local file ID.
    pub facts: Vec<(String, String, bool)>,
    /// Local file ID and reason of each clip the baker refuses.
    pub refused: Vec<(String, String)>,
}

/// Bakes the animation clips an imported model holds. The worker leaves them
/// in the Editor's form, which is the form an authored `.anim` has.
pub fn bake_model_clips(artifact: &[u8]) -> Result<ModelClips> {
    let mut result = ModelClips {
        baked: None,
        facts: Vec::new(),
        refused: Vec::new(),
    };
    let mut objects = Vec::new();
    let mut identities = Vec::new();
    for object in serialized::read(artifact)? {
        if object.tree.class_id != 74 {
            continue;
        }
        let decoded = object.tree.decode(&object.data)?;
        let Value::Object(fields) = authored(&decoded) else {
            bail!("model animation clip is not a structure");
        };
        let file_id = object.local_id.to_string();
        let mut scripts = Scripts {
            sources: &NoScripts,
            used: Vec::new(),
        };
        match clip_object(&fields, &mut scripts) {
            Ok(data) => {
                let looping = match get(&fields, "m_AnimationClipSettings") {
                    Value::Object(settings) => flag(get(settings, "m_LoopTime"), "clip")?,
                    _ => false,
                };
                result
                    .facts
                    .push((file_id.clone(), text(get(&fields, "m_Name"))?, looping));
                identities.push((file_id, "UnityEngine.AnimationClip".to_string()));
                objects.push(Object {
                    local_id: object.local_id,
                    tree: clip_tree()?,
                    data,
                    script: None,
                });
            }
            Err(error) if error.is::<super::Unsupported>() => {
                result.refused.push((file_id, error.to_string()));
            }
            Err(error) => return Err(error),
        }
    }
    if !objects.is_empty() {
        result.baked = Some(Baked {
            file: serialized::write(&objects, &[])?,
            objects: identities,
            externals: Vec::new(),
        });
    }
    Ok(result)
}
