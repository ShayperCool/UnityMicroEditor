//! `AnimationClip` (class 74): authored curves to the Player's clip data.
//!
//! A legacy clip keeps its curves. Any other clip is evaluated by Mecanim from
//! `m_MuscleClip`, which the Editor derives during a build: every curve of a
//! binding goes to the streamed clip as cubic segments, or to the constant
//! clip when the whole binding never changes. The derivation below follows
//! the pinned engine's output bit for bit on the profiles it accepts.
use super::{
    Baked, clip_tree, crc32,
    serialized::{self, Object},
    source::{flag, float, get, int, list, null_reference, object, text},
    typetree::Val,
    unsupported,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// The muscle clip's size in memory with no curves, as the engine reports it.
const MUSCLE_CLIP_BASE_SIZE: u64 = 2528;
const HUMAN_CURVE_COUNT: usize = 200;

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
    curves: Vec<Curve>,
}

struct Settings {
    start: f32,
    stop: f32,
    fields: Vec<(&'static str, Val)>,
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

/// Attributes the engine binds to the humanoid rig instead of to an Animator
/// parameter. Conservative: a parameter that merely looks like one is refused.
fn humanoid_attribute(name: &str) -> bool {
    const PREFIXES: [&str; 12] = [
        "RootT.",
        "RootQ.",
        "MotionT.",
        "MotionQ.",
        "LeftFootT.",
        "LeftFootQ.",
        "RightFootT.",
        "RightFootQ.",
        "LeftHandT.",
        "LeftHandQ.",
        "RightHandT.",
        "RightHandQ.",
    ];
    const MARKS: [&str; 13] = [
        "Stretched",
        "Stretch",
        "Spread",
        "Front-Back",
        "Left-Right",
        "In-Out",
        "Down-Up",
        "Up-Down",
        "Twist",
        "TDOF",
        "Jaw Close",
        "LeftHand.",
        "RightHand.",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || MARKS.iter().any(|mark| name.contains(mark))
}

/// The engine's binding of a float curve: attribute hash and custom type.
fn float_binding(class_id: i64, attribute: &str) -> Result<(u32, u8)> {
    match (class_id, attribute) {
        (95, name) if !humanoid_attribute(name) => Ok((crc32(name), 0)),
        (95, name) => Err(unsupported(format!("humanoid curve {name}"))),
        (1, "m_IsActive") => Ok((crc32(attribute), 0)),
        _ => Err(unsupported(format!(
            "float curve binding of class {class_id}: {attribute}"
        ))),
    }
}

/// Whether the engine stores a binding as one value per curve. Its keys are
/// compared as vectors: every key stays within the engine's comparison
/// epsilon of the first value and no tangent bends the curve. A stepped key
/// has an infinite tangent, so a held binding stays streamed.
fn constant(curves: &[Curve]) -> bool {
    const EPSILON: f32 = 0.000001;
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

fn identity_transform() -> Val {
    Val::map([
        ("q", Val::map([("w", Val::F(1.0))])),
        (
            "s",
            Val::map([("x", Val::F(1.0)), ("y", Val::F(1.0)), ("z", Val::F(1.0))]),
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
    if flag(get(map, "m_HasAdditiveReferencePose"), what)?
        || !null_reference(get(map, "m_AdditiveReferencePoseClip"))?
    {
        return Err(unsupported("additive reference pose"));
    }
    let number = |key: &str| float(get(map, key), what);
    let boolean = |key: &str| flag(get(map, key), what);
    Ok(Settings {
        start: number("m_StartTime")?,
        stop: number("m_StopTime")?,
        fields: vec![
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

fn events(fields: &Map<String, Value>) -> Result<Val> {
    let what = "m_Events";
    let mut result = Vec::new();
    for item in list(get(fields, what), what)? {
        let event = object(item, what)?;
        if !null_reference(get(event, "objectReferenceParameter"))? {
            return Err(unsupported("animation event with an object reference"));
        }
        result.push(Val::map([
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

/// The serialized `AnimationClip` object of an authored `.anim` document.
pub fn bake_clip_object(fields: &Map<String, Value>) -> Result<Vec<u8>> {
    let what = "animation clip";
    let legacy = flag(get(fields, "m_Legacy"), what)?;
    if !list(get(fields, "m_CompressedRotationCurves"), what)?.is_empty() {
        return Err(unsupported("compressed rotation curves"));
    }
    if !list(get(fields, "m_PPtrCurves"), what)?.is_empty() {
        return Err(unsupported("object reference curves"));
    }
    if flag(get(fields, "m_HasGenericRootTransform"), what)?
        || flag(get(fields, "m_HasMotionFloatCurves"), what)?
    {
        return Err(unsupported("root motion curves"));
    }
    let settings = settings(fields)?;
    let (xyz, xyzw): (&[&'static str], &[&'static str]) = (&["x", "y", "z"], &["x", "y", "z", "w"]);
    let mut retained: Vec<(&'static str, Val)> = Vec::new();
    let mut bindings: Vec<Binding> = Vec::new();
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
            bindings.push(Binding {
                path: crc32(&path),
                attribute,
                type_id: 4,
                custom_type,
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
        if !null_reference(get(entry, "script"))? {
            return Err(unsupported(format!(
                "float curve {attribute} is bound to a script"
            )));
        }
        if !legacy {
            let (hash, custom_type) = float_binding(class_id, &attribute)?;
            bindings.push(Binding {
                path: crc32(&path),
                attribute: hash,
                type_id: class_id,
                custom_type,
                curves,
            });
        }
        floats.push(Val::map([
            ("curve", curve),
            ("attribute", Val::Str(attribute)),
            ("path", Val::Str(path)),
            ("classID", Val::I(class_id)),
            ("flags", Val::I(int(get(entry, "flags"), "m_FloatCurves")?)),
        ]));
    }
    retained.push(("m_FloatCurves", Val::Seq(floats)));

    let mut object: Vec<(&'static str, Val)> = vec![
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        ("m_Legacy", Val::Bool(legacy)),
        (
            "m_Compressed",
            Val::Bool(flag(get(fields, "m_Compressed"), what)?),
        ),
        (
            "m_UseHighQualityCurve",
            Val::Bool(flag(get(fields, "m_UseHighQualityCurve"), what)?),
        ),
        (
            "m_SampleRate",
            Val::F(float(get(fields, "m_SampleRate"), what)?),
        ),
        ("m_WrapMode", Val::I(int(get(fields, "m_WrapMode"), what)?)),
        ("m_Bounds", bounds(fields)?),
        ("m_Events", events(fields)?),
    ];
    let sample_rate = float(get(fields, "m_SampleRate"), what)?;
    let human_pose = || {
        let goal = Val::map([("m_X", identity_transform())]);
        let hand = || {
            Val::map([
                ("m_GrabX", identity_transform()),
                ("m_DoFArray", Val::floats([0.0; 20])),
            ])
        };
        Val::map([
            ("m_RootX", identity_transform()),
            ("m_GoalArray", Val::Seq(vec![goal; 4])),
            ("m_LeftHandPose", hand()),
            ("m_RightHandPose", hand()),
            ("m_DoFArray", Val::floats([0.0; 55])),
            ("m_TDoFArray", Val::Seq(vec![Val::Zero; 21])),
        ])
    };
    let mut muscle: Vec<(&'static str, Val)> = vec![
        ("m_DeltaPose", human_pose()),
        ("m_StartX", identity_transform()),
        ("m_StopX", identity_transform()),
        ("m_LeftFootStartX", identity_transform()),
        ("m_RightFootStartX", identity_transform()),
        ("m_StartTime", Val::F(settings.start)),
        ("m_StopTime", Val::F(settings.stop)),
        (
            "m_IndexArray",
            Val::Seq(vec![Val::I(-1); HUMAN_CURVE_COUNT]),
        ),
    ];
    muscle.extend(settings.fields);
    // Without curves the engine leaves the muscle clip unbuilt.
    if legacy || bindings.is_empty() {
        if legacy {
            object.extend(retained);
        }
        object.push(("m_MuscleClip", Val::Map(named(muscle))));
        return clip_tree()?.encode(&Val::Map(named(object)));
    }
    // A binding is streamed unless all of its curves are constant; streamed
    // bindings come first, both groups in the order above.
    let (animated, fixed): (Vec<&Binding>, Vec<&Binding>) = bindings
        .iter()
        .partition(|binding| !constant(&binding.curves));
    let animated_curves: Vec<&Curve> = animated.iter().flat_map(|b| &b.curves).collect();
    let fixed_curves: Vec<&Curve> = fixed.iter().flat_map(|b| &b.curves).collect();
    if animated_curves.len() > u16::MAX as usize {
        return Err(unsupported("more streamed curves than the format indexes"));
    }
    let stream = streamed(&animated_curves);
    let all_curves = || animated_curves.iter().chain(&fixed_curves);
    let span =
        |start: f32, stop: f32| Val::map([("m_Start", Val::F(start)), ("m_Stop", Val::F(stop))]);
    let delta: Vec<Val> = animated_curves
        .iter()
        .map(|curve| {
            span(
                evaluate(curve, settings.start),
                evaluate(curve, settings.stop),
            )
        })
        .chain(fixed_curves.iter().map(|curve| {
            let value = curve.keys[0].value;
            span(value, value)
        }))
        .collect();
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
    let generic: Vec<Val> = animated
        .iter()
        .chain(&fixed)
        .map(|binding| {
            Val::map([
                ("path", Val::U(binding.path as u64)),
                ("attribute", Val::U(binding.attribute as u64)),
                ("typeID", Val::I(binding.type_id)),
                ("customType", Val::U(binding.custom_type as u64)),
            ])
        })
        .collect();
    object.push(("m_MuscleClipSize", Val::U(size)));
    object.push(("m_MuscleClip", Val::Map(named(muscle))));
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

/// Bakes an authored `.anim` source into a ContentFile.
pub fn bake_clip(yaml: &[u8]) -> Result<Baked> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let [document] = documents.as_slice() else {
        return Err(unsupported("animation source with several objects"));
    };
    if document.class_id != 74 || document.stripped {
        bail!("animation source does not hold an AnimationClip");
    }
    let local_id: i64 = document.file_id.parse().context("animation clip file ID")?;
    let data = bake_clip_object(&document.data)?;
    let file = serialized::write(
        &[Object {
            local_id,
            tree: clip_tree()?,
            data,
        }],
        &[],
    )?;
    Ok(Baked {
        file,
        objects: vec![(
            document.file_id.clone(),
            "UnityEngine.AnimationClip".to_string(),
        )],
        externals: Vec::new(),
    })
}
