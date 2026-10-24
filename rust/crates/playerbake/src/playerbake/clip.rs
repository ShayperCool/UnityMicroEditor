//! `AnimationClip` (class 74): authored curves to the Player's clip data.
//!
//! A legacy clip keeps its curves. Any other clip is evaluated by Mecanim from
//! `m_MuscleClip`, which the Editor derives during a build: every curve of a
//! binding goes to the streamed clip as cubic segments, or to the constant
//! clip when the whole binding never changes. A clip that does not ask for
//! high quality curves has the curves keyed on nearly every frame sampled
//! into the dense clip instead. Float curves named after the humanoid rig
//! drive it, and the build derives the root's start, stop and average speed
//! from them. The derivation below follows the pinned engine's output bit for
//! bit on the profiles it accepts.
use super::{
    binding,
    binding::HUMAN_CURVE,
    clip_tree, crc32, humanoid,
    serialized::External,
    source::{flag, float, get, int, list, null_reference, object, text},
    typetree::Val,
    unsupported, weighted,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::cmp::Ordering;

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

/// An object of another asset that a curve assigns or an event passes: a
/// sprite, a material, a mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTarget {
    pub guid: String,
    /// The object's file ID in its asset.
    pub file_id: i64,
    /// The `type` of the authored reference: 2 for a native asset file, 3
    /// for an imported one.
    pub kind: i64,
}

/// Where the baked clip finds an [`ObjectTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectPointer {
    /// Whether the object lives in the ContentFile of its own asset, which
    /// then becomes an external reference of the clip, or in the clip's file.
    pub external: bool,
    /// The object's local ID in that file.
    pub path_id: i64,
}

/// Resolves the scripts and objects a clip binds curves or events to, and
/// the clip an additive reference pose is taken from.
pub trait ClipSources {
    fn script(&self, guid: &str) -> Result<ScriptIdentity>;
    /// The authored `.anim` source of another clip. A caller that cannot
    /// bake this clip again when that one changes leaves it refused.
    fn clip(&self, guid: &str) -> Result<Vec<u8>> {
        Err(unsupported(format!(
            "additive reference pose from clip {guid}"
        )))
    }
    /// The pointer for an object of another asset. By default the object is
    /// expected under its authored file ID in the ContentFile of its asset,
    /// which the caller has to publish and load as a dependency; a caller
    /// that cannot do so for a target refuses it here.
    fn object(&self, target: &ObjectTarget) -> Result<ObjectPointer> {
        Ok(ObjectPointer {
            external: true,
            path_id: target.file_id,
        })
    }
    /// Whether the class of a script makes the field behind a property path
    /// discrete: an enum, or an `int` marked `[DiscreteEvaluation]`. The
    /// engine then keeps the curve apart as a curve of whole numbers,
    /// whatever the clip says. `None` when the class is not known; the clip's
    /// own mark is then all there is to go by.
    fn discrete(&self, _script: &str, _attribute: &str) -> Result<Option<bool>> {
        Ok(None)
    }
    /// Whether a script is an asset of its own that the clip's file refers
    /// to, by the script's GUID and `MonoScript` file ID, rather than an
    /// object the clip's file holds beside it.
    fn scripts_external(&self) -> bool {
        false
    }
}

/// For clips that may not refer to scripts.
pub struct NoScripts;
impl ClipSources for NoScripts {
    fn script(&self, guid: &str) -> Result<ScriptIdentity> {
        Err(unsupported(format!("reference to script {guid}")))
    }
}

/// Collects the scripts and assets a clip refers to and hands out pointers
/// to them.
struct Scripts<'a> {
    sources: &'a dyn ClipSources,
    used: Vec<(String, ScriptIdentity)>,
    /// The assets whose ContentFiles the clip points into.
    externals: Vec<External>,
}

/// Every script asset holds its `MonoScript` at this file ID.
const SCRIPT_FILE_ID: &str = "11500000";

impl<'a> Scripts<'a> {
    fn new(sources: &'a dyn ClipSources) -> Self {
        Self {
            sources,
            used: Vec::new(),
            externals: Vec::new(),
        }
    }

    /// The pointer for an authored object reference that has to be a script.
    fn pointer(&mut self, value: &Value, what: &str) -> Result<Val> {
        if null_reference(value)? {
            return Ok(Val::Zero);
        }
        let map = object(value, what)?;
        let guid = text(get(map, "guid"))?.to_ascii_lowercase();
        if text(get(map, "fileID"))? != SCRIPT_FILE_ID || guid.is_empty() {
            return Err(unsupported(format!("{what} that is not a script")));
        }
        if self.sources.scripts_external() {
            let known = self.externals.iter().position(|e| e.guid == guid);
            let file = 1 + known.unwrap_or_else(|| {
                self.externals.push(External {
                    guid,
                    path: String::new(),
                });
                self.externals.len() - 1
            }) as i64;
            return Ok(Val::map([
                ("m_FileID", Val::I(file)),
                ("m_PathID", Val::I(11_500_000)),
            ]));
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

    /// The pointer for an authored reference to any object: a script as
    /// above, anything else through its asset's ContentFile.
    fn any(&mut self, value: &Value, what: &str) -> Result<Val> {
        if null_reference(value)? {
            return Ok(Val::Zero);
        }
        let map = object(value, what)?;
        let file_id = text(get(map, "fileID"))?;
        if file_id == SCRIPT_FILE_ID {
            return self.pointer(value, what);
        }
        let guid = text(get(map, "guid"))?.to_ascii_lowercase();
        // An object of the clip's own file, or one built into the engine,
        // has no asset a ContentFile is baked from.
        let hex = guid.len() == 32 && guid.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !hex || guid.starts_with("0000000000000000") {
            return Err(unsupported(format!(
                "{what} to an object outside the project's assets"
            )));
        }
        let Ok(file_id) = file_id.parse::<i64>() else {
            bail!("{what} has an invalid file ID");
        };
        let target = ObjectTarget {
            guid,
            file_id,
            kind: int(get(map, "type"), what)?,
        };
        let pointer = self.sources.object(&target)?;
        let mut file = 0;
        if pointer.external {
            let known = self.externals.iter().position(|e| e.guid == target.guid);
            file = 1 + known.unwrap_or_else(|| {
                self.externals.push(External {
                    guid: target.guid,
                    path: String::new(),
                });
                self.externals.len() - 1
            }) as i64;
        }
        Ok(Val::map([
            ("m_FileID", Val::I(file)),
            ("m_PathID", Val::I(pointer.path_id)),
        ]))
    }

    /// The GUID of a script reference, for asking about its class.
    fn guid(value: &Value) -> Result<String> {
        match value {
            Value::Object(map) => Ok(text(get(map, "guid"))?.to_ascii_lowercase()),
            _ => Ok(String::new()),
        }
    }
}

/// The muscle clip's size in memory with no curves, as the engine reports it.
const MUSCLE_CLIP_BASE_SIZE: u64 = 2528;
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

/// What the curves of a binding hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A Transform channel or a single float.
    Value,
    /// A float of whole numbers, which the engine keeps behind the others.
    Discrete,
    /// Positions in the clip's list of objects, one per key.
    Object,
}

/// One animated property.
struct Binding {
    path: u32,
    attribute: u32,
    type_id: i64,
    custom_type: u8,
    script: Val,
    kind: Kind,
    curves: Vec<Curve>,
}

/// Where the engine stores the curves of a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    Streamed,
    Dense,
    Constant,
}

impl Binding {
    fn store(&self, high_quality: bool, sample_rate: f32) -> Store {
        let constant = match (self.kind, &self.curves[..]) {
            // An object curve is streamed whatever it holds.
            (Kind::Object, _) => return Store::Streamed,
            // Whole numbers are compared exactly and their tangents ignored.
            (Kind::Discrete, [curve]) => {
                let first = curve.keys[0].value;
                curve.keys.iter().all(|key| key.value == first)
            }
            (_, [curve]) => constant_float(curve),
            (_, curves) => constant_vector(curves),
        };
        if constant {
            Store::Constant
        } else if !high_quality && dense(&self.curves, sample_rate) {
            Store::Dense
        } else {
            Store::Streamed
        }
    }
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
    /// In the order the engine adds them: position, rotation, Euler
    /// rotation, scale, floats, discrete floats and object curves, each in
    /// authored order.
    bindings: Vec<Binding>,
    /// The object of every key of the object curves, in binding order.
    objects: Vec<Val>,
    /// The first and last time of any curve or event.
    range: (f32, f32),
    /// A position or rotation curve on the root object.
    generic_root: bool,
    /// A float curve that drives the humanoid motion transform.
    motion: bool,
}

/// The bindings of a muscle clip in the order of its value array: streamed
/// bindings first, then dense and constant ones, each group in binding
/// order, and the dense clip sampled from the middle group.
struct Layout<'a> {
    streamed: Vec<&'a Binding>,
    dense: Vec<&'a Binding>,
    constant: Vec<&'a Binding>,
    frames: i32,
    sample_rate: f32,
    begin: f32,
    samples: Vec<f32>,
}

impl<'a> Layout<'a> {
    fn new(
        bindings: &'a [Binding],
        high_quality: bool,
        sample_rate: f32,
        range: (f32, f32),
    ) -> Self {
        let group = |store| {
            let stored = |binding: &&Binding| binding.store(high_quality, sample_rate) == store;
            bindings.iter().filter(stored).collect::<Vec<_>>()
        };
        let mut layout = Self {
            streamed: group(Store::Streamed),
            dense: group(Store::Dense),
            constant: group(Store::Constant),
            frames: dense_frames(range, sample_rate),
            sample_rate,
            begin: range.0,
            samples: Vec::new(),
        };
        // One row of the dense clip per frame, from the first time of the
        // clip on.
        let curves: Vec<&Curve> = layout.dense.iter().flat_map(|b| &b.curves).collect();
        if !curves.is_empty() {
            for frame in 0..layout.frames {
                let time = frame as f32 / sample_rate + layout.begin;
                let row = curves.iter().map(|curve| evaluate_clamped(curve, time));
                layout.samples.extend(row);
            }
        }
        layout
    }

    fn bindings(&self) -> impl Iterator<Item = &'a Binding> + '_ {
        let groups = [&self.streamed, &self.dense, &self.constant];
        groups.into_iter().flatten().copied()
    }

    fn curves(group: &[&'a Binding]) -> Vec<&'a Curve> {
        group.iter().flat_map(|binding| &binding.curves).collect()
    }

    /// Whether every curve is one the first versions of this baker placed:
    /// streamed or constant values.
    fn plain(&self) -> bool {
        self.dense.is_empty() && self.bindings().all(|binding| binding.kind == Kind::Value)
    }

    /// The value array at a time, as the engine evaluates the muscle clip.
    fn sample(&self, time: f32) -> Vec<f32> {
        let mut values: Vec<f32> = Self::curves(&self.streamed)
            .into_iter()
            .map(|curve| evaluate(curve, time))
            .collect();
        let width = Self::curves(&self.dense).len();
        if width > 0 {
            // Between two frames of the dense clip the engine interpolates.
            let position = (time - self.begin) * self.sample_rate;
            let whole = position.trunc();
            let frame = if whole.abs() < 2147483648.0 {
                whole as i64
            } else {
                i64::MIN
            };
            let last = self.frames as i64 - 1;
            let lower = (frame.max(0).min(last) as usize) * width;
            let upper = ((frame.max(-1) + 1).min(last) as usize) * width;
            let part = position.fract();
            for index in 0..width {
                let (from, to) = (self.samples[lower + index], self.samples[upper + index]);
                values.push((to - from) * part + from);
            }
        }
        let constant = Self::curves(&self.constant);
        values.extend(constant.into_iter().map(|curve| curve.keys[0].value));
        values
    }
}

/// The number of frames the engine gives the dense clip: one per sample
/// period of the clip's range, rounded up, and one more.
fn dense_frames(range: (f32, f32), sample_rate: f32) -> i32 {
    let span = (range.1 - range.0) * sample_rate;
    let rounded = if span >= 0.0 { span + 0.99999994 } else { span };
    // The engine's conversion turns a number out of range into the lowest.
    let count = if rounded.abs() < 2147483648.0 {
        rounded as i32
    } else {
        i32::MIN
    };
    count.max(1) + 1
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

/// What a curve of the source holds.
struct Read {
    /// The serialized `AnimationCurve`, as authored.
    serialized: Val,
    /// The keys of each component as a build takes them: without keys for
    /// a curve it drops.
    curves: Vec<Curve>,
    /// The times of the first and the last authored key.
    range: Option<(f32, f32)>,
}

/// Reads an `AnimationCurve`. `build` holds the clip's sample rate and
/// whether all its curves stay streamed: a build samples weighted tangents
/// into plain keys at that rate, and a legacy clip, which has none, keeps
/// them as authored.
fn read_curve(
    value: &Value,
    names: &[&'static str],
    what: &str,
    build: Option<(f32, bool)>,
) -> Result<Read> {
    let map = object(value, what)?;
    let width = names.len().max(1);
    let mut curves = vec![Curve::default(); width];
    let mut keys = Vec::new();
    let mut modes = Vec::new();
    let mut sources = vec![Vec::new(); width];
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
        modes.push(weighted);
        let values = part(get(key, "value"))?;
        let in_slopes = part(get(key, "inSlope"))?;
        let out_slopes = part(get(key, "outSlope"))?;
        let in_weights = part(get(key, "inWeight"))?;
        let out_weights = part(get(key, "outWeight"))?;
        for index in 0..width {
            if !values[index].is_finite() {
                return Err(unsupported(format!("{what} has a non-finite value")));
            }
            curves[index].keys.push(Key {
                time,
                value: values[index],
                in_slope: in_slopes[index],
                out_slope: out_slopes[index],
            });
            sources[index].push(weighted::Key {
                time,
                value: values[index],
                in_slope: in_slopes[index],
                out_slope: out_slopes[index],
                in_weight: in_weights[index],
                out_weight: out_weights[index],
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
    let ends = |keys: &[Key]| Some((keys.first()?.time, keys.last()?.time));
    let range = ends(&curves[0].keys);
    if let Some((sample_rate, streamed)) = build
        && let Some(converted) = weighted::convert(&sources, &modes, sample_rate, streamed, what)?
    {
        for (curve, plain) in curves.iter_mut().zip(converted) {
            let key = |[time, value, in_slope, out_slope]: weighted::Plain| Key {
                time,
                value,
                in_slope,
                out_slope,
            };
            curve.keys = plain.into_iter().map(key).collect();
        }
    }
    Ok(Read {
        serialized,
        curves,
        range,
    })
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

/// Whether the engine samples the changing curves of a binding into the
/// dense clip when the clip does not ask for high quality curves: no key is
/// stepped, the samples of all frames take no more bytes than the streamed
/// keys would, and no two keys are closer than a frame, less half a
/// millisecond. A single key qualifies.
fn dense(curves: &[Curve], sample_rate: f32) -> bool {
    let keys = &curves[0].keys;
    let smooth = |index: usize| {
        let finite = |curve: &Curve| {
            curve.keys[index].in_slope.is_finite() && curve.keys[index].out_slope.is_finite()
        };
        curves.iter().all(finite)
    };
    // Bytes of one frame, and of one streamed key with its time and index.
    let (frame_bytes, key_bytes) = match curves.len() {
        1 => (4.0, 28),
        3 => (12.0, 60),
        _ => (16.0, 76),
    };
    let frame = 1.0 / sample_rate;
    let span = keys[keys.len() - 1].time - keys[0].time;
    // The engine's comparisons, which a number that is none passes.
    let exceeds = |left: f32, right: f32| left.partial_cmp(&right) == Some(Ordering::Greater);
    smooth(0)
        && !exceeds(span / frame * frame_bytes, (key_bytes * keys.len()) as f32)
        && (1..keys.len()).all(|index| {
            let spare = (keys[index].time - keys[index - 1].time).abs() - frame;
            smooth(index) && !exceeds(-0.0005, spare)
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
/// single-precision order of operations, and the value at its start. A
/// tangent of positive infinity holds the segment at its first key, one of
/// negative infinity at its second.
fn segment(lhs: &Key, rhs: &Key) -> [f32; 4] {
    if lhs.out_slope == f32::INFINITY || rhs.in_slope == f32::INFINITY {
        return [0.0, 0.0, 0.0, lhs.value];
    }
    if lhs.out_slope == f32::NEG_INFINITY || rhs.in_slope == f32::NEG_INFINITY {
        return [0.0, 0.0, 0.0, rhs.value];
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

/// The cubic of a segment at an offset from its start; `-0` plus a zero term
/// is `+0`.
fn cubic([a, b, c, d]: [f32; 4], t: f32) -> f32 {
    t * (t * (t * a + b) + c) + d
}

/// Samples the streamed form of a curve the way the engine does: always
/// through the cubic of the frame in effect.
fn evaluate(curve: &Curve, time: f32) -> f32 {
    let keys = &curve.keys;
    let last = keys.len() - 1;
    if time < keys[0].time {
        cubic([0.0, 0.0, 0.0, keys[0].value], 1.0)
    } else if time >= keys[last].time {
        cubic([0.0, 0.0, 0.0, keys[last].value], time - keys[last].time)
    } else {
        let index = keys.partition_point(|key| key.time <= time) - 1;
        cubic(
            segment(&keys[index], &keys[index + 1]),
            time - keys[index].time,
        )
    }
}

/// Samples an authored curve for the dense clip, held at its ends. Unlike
/// the streamed form, the time of the last key still lies in a segment: one
/// from that key to itself.
fn evaluate_clamped(curve: &Curve, time: f32) -> f32 {
    let keys = &curve.keys;
    let last = keys.len() - 1;
    if last == 0 {
        keys[0].value
    } else if time > keys[last].time {
        cubic([0.0, 0.0, 0.0, keys[last].value], time - keys[last].time)
    } else if keys[0].time > time {
        // The engine starts this stretch a thousand seconds early.
        cubic([0.0, 0.0, 0.0, keys[0].value], time - (time - 1000.0))
    } else {
        let index = keys.partition_point(|key| key.time <= time) - 1;
        let next = (index + 1).min(last);
        cubic(segment(&keys[index], &keys[next]), time - keys[index].time)
    }
}

/// The streamed clip of curves; `held` marks the object curves, whose first
/// key only sets the value the clip starts with.
fn streamed(curves: &[(&Curve, bool)]) -> Vec<u32> {
    // (time, curve index, coefficients and value)
    let mut entries: Vec<(f32, usize, [f32; 4])> = Vec::new();
    for (index, (curve, held)) in curves.iter().enumerate() {
        for (position, key) in curve.keys.iter().enumerate().skip(*held as usize) {
            let coefficients = match curve.keys.get(position + 1) {
                Some(next) if !held => segment(key, next),
                _ => [0.0, 0.0, 0.0, key.value],
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
        for (index, (curve, _)) in curves.iter().enumerate() {
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
        let parameter = scripts.any(
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

/// A baked `AnimationClip` object and what its file has to hold beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipObject {
    /// The object encoded with the pinned engine's tree.
    pub data: Vec<u8>,
    /// The object's fields, for a release's own tree.
    pub value: Val,
    /// The scripts the clip points to, each of which needs a `MonoScript`
    /// object with its local ID in the same file.
    pub scripts: Vec<ScriptIdentity>,
    /// The assets the clip points into, in external-reference order.
    pub externals: Vec<External>,
}

/// The serialized `AnimationClip` object of an authored `.anim` document
/// with the scripts and assets it points to.
pub fn bake_clip_parts(
    fields: &Map<String, Value>,
    sources: &dyn ClipSources,
) -> Result<ClipObject> {
    let mut scripts = Scripts::new(sources);
    let value = clip_object(fields, &mut scripts)?;
    Ok(ClipObject {
        data: clip_tree()?.encode(&value)?,
        value,
        scripts: scripts.used.into_iter().map(|(_, script)| script).collect(),
        externals: scripts.externals,
    })
}

/// [`bake_clip_parts`] for a clip that points into no other asset.
pub fn bake_clip_object(
    fields: &Map<String, Value>,
    sources: &dyn ClipSources,
) -> Result<(Vec<u8>, Vec<ScriptIdentity>)> {
    let baked = bake_clip_parts(fields, sources)?;
    if !baked.externals.is_empty() {
        bail!("animation clip refers to objects of other assets");
    }
    Ok((baked.data, baked.scripts))
}

/// A quaternion is held as a whole: an infinite tangent on one component of
/// a segment holds the others with it, at the first key when it is positive
/// and at the second when negative.
fn hold_together(curves: &mut [Curve]) {
    for index in 1..curves[0].keys.len() {
        let ends = |curve: &Curve| [curve.keys[index - 1].out_slope, curve.keys[index].in_slope];
        let any = |slope: f32| curves.iter().any(|curve| ends(curve).contains(&slope));
        let held = [f32::INFINITY, f32::NEG_INFINITY]
            .into_iter()
            .find(|slope| any(*slope));
        if let Some(slope) = held {
            for curve in curves.iter_mut() {
                curve.keys[index - 1].out_slope = slope;
            }
        }
    }
}

/// Widens a range by the first and last time of a list.
fn widen(range: &mut (f32, f32), first: f32, last: f32) {
    range.0 = range.0.min(first);
    range.1 = range.1.max(last);
}

/// The keys of an object reference curve: the serialized form, which a
/// legacy clip keeps, and the times with the pointer of each key.
fn read_objects(value: &Value, scripts: &mut Scripts<'_>) -> Result<(Val, Vec<(f32, Val)>)> {
    let what = "m_PPtrCurves";
    let mut keys = Vec::new();
    let mut previous = f32::NEG_INFINITY;
    for item in list(value, what)? {
        let key = object(item, what)?;
        let time = float(get(key, "time"), what)?;
        if !time.is_finite() || time <= previous {
            return Err(unsupported(format!(
                "{what} has keys that are not in increasing time order"
            )));
        }
        previous = time;
        let pointer = scripts.any(get(key, "value"), "object reference curve")?;
        keys.push((time, pointer));
    }
    let serialized = keys
        .iter()
        .map(|(time, pointer)| Val::map([("time", Val::F(*time)), ("value", pointer.clone())]))
        .collect();
    Ok((Val::Seq(serialized), keys))
}

fn read_curves(
    fields: &Map<String, Value>,
    scripts: &mut Scripts<'_>,
    legacy: bool,
    sample_rate: f32,
) -> Result<Curves> {
    let what = "animation clip";
    if !list(get(fields, "m_CompressedRotationCurves"), what)?.is_empty() {
        return Err(unsupported("compressed rotation curves"));
    }
    let (xyz, xyzw): (&[&'static str], &[&'static str]) = (&["x", "y", "z"], &["x", "y", "z", "w"]);
    let mut retained: Vec<(&'static str, Val)> = Vec::new();
    let mut bindings: Vec<Binding> = Vec::new();
    let mut range = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut generic_root, mut motion) = (false, false);
    // A clip marked as compressed holds its rotations in the compressed
    // curves alone: a build takes no notice of the plain ones.
    let compressed = flag(get(fields, "m_Compressed"), what)?;
    let mut unused: Vec<(f32, f32)> = Vec::new();
    let build = (!legacy).then_some((sample_rate, high_quality(fields)?));
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
            let Read {
                serialized: curve,
                mut curves,
                range: ends,
            } = read_curve(get(entry, "curve"), names, field, build)?;
            let Some((first, last)) = ends else {
                return Err(unsupported(format!("{field} has a curve without keys")));
            };
            if compressed && field == "m_RotationCurves" {
                unused.push((first, last));
                continue;
            }
            widen(&mut range, first, last);
            serialized.push(Val::map([
                ("curve", curve),
                ("path", Val::Str(path.clone())),
            ]));
            // Moving or turning the root object is root motion; scaling
            // it is not. A curve the build drops below still counts.
            generic_root |= path.is_empty() && field != "m_ScaleCurves";
            // Weighted keys that weight no segment: the build drops the curve.
            if curves[0].keys.is_empty() {
                continue;
            }
            if field == "m_RotationCurves" {
                hold_together(&mut curves);
            }
            bindings.push(Binding {
                path: crc32(&path),
                attribute,
                type_id: 4,
                custom_type,
                script: Val::Zero,
                kind: Kind::Value,
                curves,
            });
        }
        retained.push((field, Val::Seq(serialized)));
    }
    let mut floats = Vec::new();
    // The engine adds the discrete floats after all the others.
    let mut discrete = Vec::new();
    for item in list(get(fields, "m_FloatCurves"), "m_FloatCurves")? {
        let entry = object(item, "m_FloatCurves")?;
        let path = text(get(entry, "path"))?;
        let attribute = text(get(entry, "attribute"))?;
        let class_id = int(get(entry, "classID"), "m_FloatCurves")?;
        let Read {
            serialized: curve,
            curves,
            range: ends,
        } = read_curve(get(entry, "curve"), &[], "m_FloatCurves", build)?;
        let Some((first, last)) = ends else {
            return Err(unsupported("m_FloatCurves has a curve without keys"));
        };
        widen(&mut range, first, last);
        let script = scripts.pointer(get(entry, "script"), "float curve script")?;
        let flags = int(get(entry, "flags"), "m_FloatCurves")?;
        floats.push(Val::map([
            ("curve", curve),
            ("attribute", Val::Str(attribute.clone())),
            ("path", Val::Str(path.clone())),
            ("classID", Val::I(class_id)),
            ("script", script.clone()),
            ("flags", Val::I(flags)),
        ]));
        if legacy {
            continue;
        }
        let scripted = class_id == binding::MONO_BEHAVIOUR;
        if scripted != (script != Val::Zero) {
            return Err(unsupported(format!(
                "float curve {attribute} of class {class_id} with an unexpected script reference"
            )));
        }
        let Some(generic) = binding::float(class_id, &attribute)? else {
            continue;
        };
        // The first flag marks a curve of whole numbers. What decides is
        // the class: a field it makes discrete is one whatever the mark
        // says, and a marked curve on any other property is left out.
        let marked = flags & 1 != 0;
        let mut whole = flags & 5 == 5;
        if scripted && generic.custom_type == 0 {
            if attribute.contains("managedReferences[") {
                return Err(unsupported(format!(
                    "float curve {attribute} into a managed reference"
                )));
            }
            // The engine does not look for a field behind an array element.
            let by_class = match attribute.contains('[') {
                true => Some(false),
                false => {
                    let guid = Scripts::guid(get(entry, "script"))?;
                    scripts.sources.discrete(&guid, &attribute)?
                }
            };
            match by_class {
                Some(by_class) => whole |= by_class,
                None if marked => {
                    return Err(unsupported(format!(
                        "discrete curve {attribute} of a script whose class is not known"
                    )));
                }
                None => {}
            }
        }
        let moves = generic.custom_type == HUMAN_CURVE && humanoid::motion_curve(generic.attribute);
        // A motion curve counts whatever becomes of it: left out below, or
        // kept as a curve of whole numbers.
        motion |= moves;
        if (!whole && marked) || curves[0].keys.is_empty() {
            continue;
        }
        let target = if whole { &mut discrete } else { &mut bindings };
        target.push(Binding {
            path: crc32(&path),
            attribute: generic.attribute,
            type_id: class_id,
            custom_type: generic.custom_type,
            script,
            kind: if whole { Kind::Discrete } else { Kind::Value },
            curves,
        });
    }
    retained.push(("m_FloatCurves", Val::Seq(floats)));
    bindings.append(&mut discrete);
    let mut references = Vec::new();
    let mut objects = Vec::new();
    for item in list(get(fields, "m_PPtrCurves"), "m_PPtrCurves")? {
        let entry = object(item, "m_PPtrCurves")?;
        let path = text(get(entry, "path"))?;
        let attribute = text(get(entry, "attribute"))?;
        let class_id = int(get(entry, "classID"), "m_PPtrCurves")?;
        let (curve, keys) = read_objects(get(entry, "curve"), scripts)?;
        let script = scripts.pointer(get(entry, "script"), "object reference curve script")?;
        references.push(Val::map([
            ("curve", curve),
            ("attribute", Val::Str(attribute.clone())),
            ("path", Val::Str(path.clone())),
            ("classID", Val::I(class_id)),
            ("script", script.clone()),
            ("flags", Val::I(int(get(entry, "flags"), "m_PPtrCurves")?)),
        ]));
        // A curve without keys animates nothing.
        let (Some(first), Some(last), false) = (keys.first(), keys.last(), legacy) else {
            continue;
        };
        // An object stays for a frame after the key that sets it.
        widen(&mut range, first.0, last.0 + 1.0 / sample_rate);
        if (class_id == binding::MONO_BEHAVIOUR) != (script != Val::Zero) {
            return Err(unsupported(format!(
                "object reference curve {attribute} of class {class_id} with an unexpected \
                 script reference"
            )));
        }
        let generic = binding::object(class_id, &attribute)?;
        // Each key holds the position of its object in the clip's list.
        let key = |(index, (time, _)): (usize, &(f32, Val))| Key {
            time: *time,
            value: (objects.len() + index) as f32,
            in_slope: f32::INFINITY,
            out_slope: f32::INFINITY,
        };
        bindings.push(Binding {
            path: crc32(&path),
            attribute: generic.attribute,
            type_id: class_id,
            custom_type: generic.custom_type,
            script,
            kind: Kind::Object,
            curves: vec![Curve {
                keys: keys.iter().enumerate().map(key).collect(),
            }],
        });
        objects.extend(keys.into_iter().map(|(_, pointer)| pointer));
    }
    retained.push(("m_PPtrCurves", Val::Seq(references)));
    // The Editor's copies of the float curves count for the range as well.
    let what = "m_EditorCurves";
    for item in list(get(fields, what), what)? {
        let keys = match get(object(item, what)?, "curve") {
            Value::Object(curve) => list(get(curve, "m_Curve"), what)?,
            _ => &[],
        };
        if let (Some(first), Some(last)) = (keys.first(), keys.last()) {
            let time = |key: &Value| float(get(object(key, what)?, "time"), what);
            widen(&mut range, time(first)?, time(last)?);
        }
    }
    // The first and the last event count as well, in authored order.
    let events = list(get(fields, "m_Events"), "m_Events")?;
    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        let time = |event: &Value| float(get(object(event, "m_Events")?, "time"), "m_Events");
        let (first, last) = (time(first)?, time(last)?);
        if first.is_finite() {
            range.0 = range.0.min(first);
        }
        if last.is_finite() {
            range.1 = range.1.max(last);
        }
    }
    // Whether such a rotation curve still counts for the range of the clip
    // has no reference to compare.
    if unused
        .iter()
        .any(|(first, last)| *first < range.0 || range.1 < *last)
    {
        return Err(unsupported(
            "rotation curves of a compressed clip that outlast its other curves",
        ));
    }
    Ok(Curves {
        retained,
        bindings,
        objects,
        range,
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
    let mut scripts = Scripts::new(sources);
    // The reference is read like any clip, without its own reference pose.
    let sample_rate = float(get(fields, "m_SampleRate"), what)?;
    let curves = read_curves(fields, &mut scripts, false, sample_rate)?;
    if flag(get(fields, "m_Legacy"), what)? || curves.bindings.is_empty() {
        return Err(unsupported(format!(
            "{what} from a clip without a muscle clip"
        )));
    }
    let high_quality = high_quality(fields)?;
    let other = Layout::new(&curves.bindings, high_quality, sample_rate, curves.range);
    // The pose has only been compared for streamed and constant values.
    if !layout.plain() || !other.plain() {
        return Err(unsupported(format!(
            "{what} with dense, discrete or object reference curves"
        )));
    }
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

fn clip_object(fields: &Map<String, Value>, scripts: &mut Scripts<'_>) -> Result<Val> {
    let what = "animation clip";
    let legacy = flag(get(fields, "m_Legacy"), what)?;
    let settings = settings(fields)?;
    let high_quality = high_quality(fields)?;
    let sample_rate = float(get(fields, "m_SampleRate"), what)?;
    let Curves {
        retained,
        bindings,
        objects,
        range,
        generic_root,
        motion,
    } = read_curves(fields, scripts, legacy, sample_rate)?;

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
    // Without events and curves, or with none it does not leave out, the
    // engine leaves the muscle clip unbuilt. Events alone make it build an
    // empty one, over the range of the curves left out as well.
    let eventful = !list(get(fields, "m_Events"), "m_Events")?.is_empty();
    if legacy || (bindings.is_empty() && !eventful) {
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
        return Ok(Val::Map(named(object)));
    }
    let layout = Layout::new(&bindings, high_quality, sample_rate, range);
    // The streamed clip counts the curves of whole numbers and of objects
    // apart from the others, which they follow.
    let held = |binding: &Binding| binding.kind != Kind::Value;
    let stepped = layout.streamed.iter().filter(|b| held(b)).count();
    let animated: Vec<(&Curve, bool)> = layout
        .streamed
        .iter()
        .flat_map(|binding| {
            let object = binding.kind == Kind::Object;
            binding.curves.iter().map(move |curve| (curve, object))
        })
        .collect();
    if animated.len() > u16::MAX as usize {
        return Err(unsupported("more streamed curves than the format indexes"));
    }
    let stream = streamed(&animated);
    let fixed = Layout::curves(&layout.constant);
    // Each human curve points at its place in the value array; of several
    // bindings to one curve the last counts.
    let mut index: humanoid::Index = [-1; humanoid::CURVE_COUNT];
    let mut position = 0;
    for binding in layout.bindings() {
        if binding.custom_type == HUMAN_CURVE && binding.kind == Kind::Value {
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
    let dense_curves = Layout::curves(&layout.dense).len();
    let size = MUSCLE_CLIP_BASE_SIZE
        + 4 * (stream.len() + layout.samples.len() + fixed.len()) as u64
        + 8 * position as u64;
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
                    ("curveCount", Val::U((animated.len() - stepped) as u64)),
                    ("discreteCurveCount", Val::U(stepped as u64)),
                ]),
            ),
            (
                "m_DenseClip",
                Val::map([
                    ("m_FrameCount", Val::I(layout.frames as i64)),
                    ("m_CurveCount", Val::U(dense_curves as u64)),
                    ("m_SampleRate", Val::F(sample_rate)),
                    ("m_BeginTime", Val::F(layout.begin)),
                    ("m_SampleArray", Val::floats(layout.samples.iter().copied())),
                ]),
            ),
            (
                "m_ConstantClip",
                Val::map([(
                    "data",
                    Val::floats(fixed.iter().map(|curve| curve.keys[0].value)),
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
                ("isPPtrCurve", Val::U((binding.kind == Kind::Object) as u64)),
                (
                    "isIntCurve",
                    Val::U((binding.kind == Kind::Discrete) as u64),
                ),
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
        Val::map([
            ("genericBindings", Val::Seq(generic)),
            ("pptrCurveMapping", Val::Seq(objects)),
        ]),
    ));
    Ok(Val::Map(named(object)))
}

fn named(fields: Vec<(&'static str, Val)>) -> Vec<(std::borrow::Cow<'static, str>, Val)> {
    fields
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect()
}
