//! Audio mixers (`.mixer`). A source holds an `AudioMixerController` (241),
//! its groups (243), their effects (244) and its snapshots (245). A Player
//! holds the mixer, the groups and the snapshots; the effects and every value
//! of a snapshot move into the mixer's `AudioMixerConstant`, which the Editor
//! derives during a build (`BuildAudioMixerConstant`). This module restates
//! that derivation as it was observed on engine-built mixers.
//!
//! Every value a snapshot can hold has an index: the volume and the pitch of
//! each group, the groups taken depth first from the master, then per effect
//! its level (a send always has one, another effect only with its wet mix
//! enabled) followed by its parameters in the order the engine defines them.
use super::{
    crc32,
    native::{NativeObject, References, scalar},
    serialized::External,
    source::{get, list, object, text},
    typetree::{TypeTree, Val},
    unsupported,
};
use crate::yaml::Document;
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

pub(super) const MIXER: i64 = 241;
pub(super) const GROUP: i64 = 243;
const EFFECT: i64 = 244;
pub(super) const SNAPSHOT: i64 = 245;

const ATTENUATION: &str = "Attenuation";
const SEND: &str = "Send";
const RECEIVE: &str = "Receive";
const DUCK_VOLUME: &str = "Duck Volume";
const NONE: u64 = u32::MAX as u64;

/// The level of a send or of a wet mix no snapshot stores, in decibels.
const SILENT: f32 = -80.0;
/// `ParameterTransitionType` has seven members.
const TRANSITION_TYPES: i64 = 7;

/// A parameter of a built-in effect and the value a snapshot that does not
/// store it gets. `None`: the engine leaves that value uninitialized.
type Parameter = (&'static str, Option<f32>);

/// The effects built into the pinned engine: name, the type the Player knows
/// it by, and its parameters in the engine's order. The defaults are those of
/// an engine-built mixer; the Editor's own list of definitions differs from
/// them in the release time of the ducker.
static EFFECTS: [(&str, i64, &[Parameter]); 17] = [
    (ATTENUATION, -2, &[]),
    (SEND, -3, &[]),
    (RECEIVE, -4, &[]),
    (
        DUCK_VOLUME,
        -5,
        &[
            ("Threshold", Some(-10.0)),
            ("Ratio", Some(2.0)),
            ("Attack Time", Some(0.1)),
            ("Release Time", Some(0.3)),
            ("Make-up Gain", Some(0.0)),
            ("Knee", Some(10.0)),
            ("Sidechain Mix", Some(1.0)),
        ],
    ),
    (
        "Lowpass",
        3,
        &[("Cutoff freq", Some(5000.0)), ("Resonance", Some(1.0))],
    ),
    (
        "Highpass",
        5,
        &[("Cutoff freq", Some(5000.0)), ("Resonance", Some(1.0))],
    ),
    (
        "Echo",
        6,
        &[
            ("Delay", Some(500.0)),
            ("Decay", Some(0.0)),
            ("Max channels", Some(0.0)),
            ("Drymix", Some(1.0)),
            ("Wetmix", Some(1.0)),
        ],
    ),
    (
        "Flange",
        7,
        &[
            ("Drymix", Some(0.45)),
            ("Wetmix", Some(0.55)),
            ("Depth", Some(1.0)),
            ("Rate", Some(0.1)),
        ],
    ),
    ("Distortion", 8, &[("Level", Some(0.5))]),
    (
        "Normalize",
        9,
        &[
            ("Fade in time", Some(5000.0)),
            ("Lowest volume", Some(0.1)),
            ("Maximum amp", Some(20.0)),
        ],
    ),
    (
        "ParamEQ",
        10,
        &[
            ("Center freq", Some(8000.0)),
            ("Octave range", Some(1.0)),
            ("Frequency gain", Some(1.0)),
        ],
    ),
    (
        "Pitch Shifter",
        11,
        &[
            ("Pitch", Some(1.0)),
            ("FFT size", Some(1024.0)),
            ("Overlap", Some(4.0)),
            ("Max channels", Some(0.0)),
        ],
    ),
    (
        "Chorus",
        12,
        &[
            ("Dry mix", Some(0.5)),
            ("Wet mix tap 1", Some(0.5)),
            ("Wet mix tap 2", Some(0.5)),
            ("Wet mix tap 3", Some(0.5)),
            ("Delay", Some(40.0)),
            ("Rate", Some(0.8)),
            ("Depth", Some(0.03)),
            // Differs from one Editor run to the next.
            ("Feedback", None),
        ],
    ),
    (
        "Compressor",
        16,
        &[
            ("Threshold", Some(0.0)),
            ("Attack", Some(50.0)),
            ("Release", Some(50.0)),
            ("Make up gain", Some(0.0)),
        ],
    ),
    (
        "SFX Reverb",
        17,
        &[
            ("Dry Level", Some(0.0)),
            ("Room", Some(-10000.0)),
            ("Room HF", Some(0.0)),
            ("Decay Time", Some(1.0)),
            ("Decay HF Ratio", Some(0.5)),
            ("Reflections", Some(-10000.0)),
            ("Reflect Delay", Some(0.02)),
            ("Reverb", Some(0.0)),
            ("Reverb Delay", Some(0.04)),
            ("Diffusion", Some(100.0)),
            ("Density", Some(100.0)),
            ("HF Reference", Some(5000.0)),
            ("Room LF", Some(0.0)),
            ("LF Reference", Some(250.0)),
        ],
    ),
    ("Lowpass Simple", 18, &[("Cutoff freq", Some(5000.0))]),
    ("Highpass Simple", 22, &[("Cutoff freq", Some(1000.0))]),
];

/// A GUID as the engine holds it: four integers, each written in the source
/// as eight hexadecimal digits with the lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Guid([u32; 4]);

impl Guid {
    fn read(value: &Value, what: &str) -> Result<Self> {
        let source = text(value)?;
        let (words, rest) = source.as_bytes().as_chunks::<8>();
        if words.len() != 4 || !rest.is_empty() || !source.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{what} is not a GUID: {source:?}");
        }
        let mut guid = [0u32; 4];
        for (word, digits) in guid.iter_mut().zip(words) {
            let reversed: String = digits.iter().rev().map(|&digit| digit as char).collect();
            *word = u32::from_str_radix(&reversed, 16)?;
        }
        Ok(Guid(guid))
    }

    fn val(self) -> Val {
        Val::Map(
            ["data[0]", "data[1]", "data[2]", "data[3]"]
                .into_iter()
                .zip(self.0)
                .map(|(name, word)| (name.into(), Val::U(word as u64)))
                .collect(),
        )
    }
}

fn local(value: &Value, what: &str) -> Result<i64> {
    let map = match value {
        Value::Null => return Ok(0),
        Value::Object(map) => map,
        _ => bail!("{what} is not an object reference"),
    };
    if !text(get(map, "guid"))?.is_empty() {
        return Err(unsupported(format!("{what} refers to another source")));
    }
    match text(get(map, "fileID"))?.parse::<i64>() {
        Ok(id) => Ok(id),
        Err(_) => bail!("{what} has an invalid file ID"),
    }
}

fn locals(fields: &Map<String, Value>, key: &str) -> Result<Vec<i64>> {
    list(get(fields, key), key)?
        .iter()
        .map(|item| local(item, key))
        .collect()
}

fn flag(fields: &Map<String, Value>, key: &str) -> Result<bool> {
    match get(fields, key) {
        Value::String(source) => match scalar("bool", source, key)? {
            Val::Bool(flag) => Ok(flag),
            _ => bail!("{key} is not a flag"),
        },
        _ => Err(unsupported(format!("the source has no {key}"))),
    }
}

fn number(type_name: &str, fields: &Map<String, Value>, key: &str) -> Result<Val> {
    match get(fields, key) {
        Value::String(source) => scalar(type_name, source, key),
        _ => Err(unsupported(format!("the source has no {key}"))),
    }
}

fn finite(value: &Value, what: &str) -> Result<f32> {
    let Value::String(source) = value else {
        bail!("{what} is not a number");
    };
    match scalar("float", source, what)? {
        Val::F(number) if number.is_finite() => Ok(number),
        _ => Err(unsupported(format!("{what} is not finite"))),
    }
}

/// A snapshot's map from GUID to scalar.
fn guid_map<'a>(fields: &'a Map<String, Value>, key: &str) -> Result<Vec<(Guid, &'a Value)>> {
    let entries = match get(fields, key) {
        Value::Null => return Ok(Vec::new()),
        Value::Object(entries) => entries,
        Value::String(text) if text.is_empty() => return Ok(Vec::new()),
        _ => bail!("{key} is not a mapping"),
    };
    let mut pairs = Vec::with_capacity(entries.len());
    for (guid, value) in entries {
        pairs.push((Guid::read(&Value::String(guid.clone()), key)?, value));
    }
    // The engine keeps the map ordered by GUID.
    pairs.sort_by_key(|pair| pair.0);
    if pairs.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(unsupported(format!("{key} names a GUID twice")));
    }
    Ok(pairs)
}

struct Effect<'a> {
    id: i64,
    group: usize,
    name: &'static str,
    kind: i64,
    parameters: &'static [Parameter],
    fields: &'a Map<String, Value>,
}

/// One value of a snapshot.
struct Slot {
    guid: Guid,
    /// `None` when the engine has no defined default for it.
    default: Option<f32>,
    what: String,
}

fn names(items: impl Iterator<Item = String>) -> Val {
    let mut buffer = Vec::new();
    for name in items {
        buffer.extend(name.bytes().map(|byte| Val::I(byte as i8 as i64)));
        buffer.push(Val::I(0));
    }
    buffer.push(Val::I(0));
    Val::Seq(buffer)
}

fn unsigned(values: impl IntoIterator<Item = u64>) -> Val {
    Val::Seq(values.into_iter().map(Val::U).collect())
}

/// The Player objects of a mixer source: the mixer, its groups and its
/// snapshots under their authored file IDs, and the other mixers it refers to.
pub(super) fn bake(
    documents: &[Document],
    trees: [&TypeTree; 3],
) -> Result<(Vec<NativeObject>, Vec<External>)> {
    let mut by_id: HashMap<i64, &Document> = HashMap::new();
    let mut ids = Vec::new();
    for document in documents {
        if document.stripped || !matches!(document.class_id, MIXER | GROUP | EFFECT | SNAPSHOT) {
            return Err(unsupported(format!(
                "mixer source with an object of class {} ({})",
                document.class_id, document.type_name
            )));
        }
        let Ok(id) = document.file_id.parse::<i64>() else {
            bail!("object has an invalid file ID");
        };
        if by_id.insert(id, document).is_some() {
            bail!("mixer source holds two objects with file ID {id}");
        }
        if document.class_id != EFFECT {
            ids.push(id);
        }
    }
    let mut mixers = documents
        .iter()
        .filter(|document| document.class_id == MIXER);
    let (Some(mixer), None) = (mixers.next(), mixers.next()) else {
        return Err(unsupported("mixer source without exactly one mixer"));
    };
    let mixer_id: i64 = mixer.file_id.parse()?;
    let of_class = |id: i64, class: i64, what: &str| -> Result<&Document> {
        match by_id.get(&id) {
            Some(document) if document.class_id == class => Ok(document),
            _ => Err(unsupported(format!(
                "{what} refers to {id}, which is not an object of class {class} of the mixer"
            ))),
        }
    };
    // Every group and snapshot of the file reaches the Player and has to
    // belong to this mixer.
    for document in documents {
        if matches!(document.class_id, GROUP | SNAPSHOT)
            && local(get(&document.data, "m_AudioMixer"), "m_AudioMixer")? != mixer_id
        {
            return Err(unsupported(format!(
                "{} {} does not belong to the mixer of its source",
                document.type_name, document.file_id
            )));
        }
    }

    // Groups, depth first from the master.
    let master = local(get(&mixer.data, "m_MasterGroup"), "m_MasterGroup")?;
    let mut groups: Vec<(&Document, i64)> = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = vec![(master, -1i64)];
    while let Some((id, parent)) = pending.pop() {
        let group = of_class(id, GROUP, "a group list")?;
        if !visited.insert(id) {
            return Err(unsupported(format!("group {id} has two parents")));
        }
        let index = groups.len() as i64;
        groups.push((group, parent));
        for child in locals(&group.data, "m_Children")?.into_iter().rev() {
            pending.push((child, index));
        }
    }

    let mut slots: Vec<Slot> = Vec::new();
    let mut group_constants = Vec::new();
    for (group, parent) in &groups {
        let name = text(get(&group.data, "m_Name"))?;
        group_constants.push(Val::map([
            ("parentConstantIndex", Val::I(*parent)),
            ("volumeIndex", Val::U(slots.len() as u64)),
            ("pitchIndex", Val::U(slots.len() as u64 + 1)),
            ("mute", Val::Bool(flag(&group.data, "m_Mute")?)),
            ("solo", Val::Bool(flag(&group.data, "m_Solo")?)),
            (
                "bypassEffects",
                Val::Bool(flag(&group.data, "m_BypassEffects")?),
            ),
        ]));
        for (key, default, what) in [("m_Volume", 0.0, "volume"), ("m_Pitch", 1.0, "pitch")] {
            slots.push(Slot {
                guid: Guid::read(get(&group.data, key), key)?,
                default: Some(default),
                what: format!("the {what} of group {name:?}"),
            });
        }
    }

    // Effects, group by group. An attenuation at the end of a group is the
    // group's own fader and a send without a target sends nothing: neither
    // is an effect of the Player.
    let mut effects: Vec<Effect> = Vec::new();
    let mut owned = HashSet::new();
    for (index, (group, _)) in groups.iter().enumerate() {
        let mut listed = Vec::new();
        for id in locals(&group.data, "m_Effects")? {
            let effect = of_class(id, EFFECT, "an effect list")?;
            if !owned.insert(id) {
                return Err(unsupported(format!("effect {id} is listed twice")));
            }
            let authored = text(get(&effect.data, "m_EffectName"))?;
            let Some(&(name, kind, parameters)) =
                EFFECTS.iter().find(|effect| effect.0 == authored)
            else {
                return Err(unsupported(format!(
                    "effect {authored:?}, which is not built into the engine"
                )));
            };
            listed.push(Effect {
                id,
                group: index,
                name,
                kind,
                parameters,
                fields: &effect.data,
            });
        }
        if listed
            .last()
            .is_some_and(|effect| effect.name == ATTENUATION)
        {
            listed.pop();
        }
        for effect in listed {
            let target = local(get(effect.fields, "m_SendTarget"), "m_SendTarget")?;
            if target != 0 {
                of_class(target, EFFECT, "a send target")?;
                if effect.name != SEND {
                    return Err(unsupported(format!(
                        "{} effect with a send target",
                        effect.name
                    )));
                }
            } else if effect.name == SEND {
                continue;
            }
            effects.push(effect);
        }
    }
    let effect_index: HashMap<i64, usize> = effects
        .iter()
        .enumerate()
        .map(|(index, effect)| (effect.id, index))
        .collect();
    let mut effect_constants = Vec::new();
    let mut side_chains = 0u64;
    for (index, effect) in effects.iter().enumerate() {
        let target = local(get(effect.fields, "m_SendTarget"), "m_SendTarget")?;
        if target != 0 && !owned.contains(&target) {
            return Err(unsupported(format!(
                "send to effect {target}, which no group of the mixer lists"
            )));
        }
        let wet = effect.name == SEND || flag(effect.fields, "m_EnableWetMix")?;
        let level = if wet {
            slots.push(Slot {
                guid: Guid::read(get(effect.fields, "m_MixLevel"), "m_MixLevel")?,
                default: Some(SILENT),
                what: format!("the level of a {} effect", effect.name),
            });
            slots.len() as u64 - 1
        } else {
            NONE
        };
        // The source names each parameter; the engine takes them by name.
        let mut authored: HashMap<String, Guid> = HashMap::new();
        for item in list(get(effect.fields, "m_Parameters"), "m_Parameters")? {
            let parameter = object(item, "m_Parameters")?;
            let name = text(get(parameter, "m_ParameterName"))?;
            let guid = Guid::read(get(parameter, "m_GUID"), "m_GUID")?;
            if authored.insert(name.clone(), guid).is_some() {
                return Err(unsupported(format!(
                    "{} effect with two parameters named {name:?}",
                    effect.name
                )));
            }
        }
        if authored.len() != effect.parameters.len() {
            return Err(unsupported(format!(
                "{} effect with {} parameters, the engine defines {}",
                effect.name,
                authored.len(),
                effect.parameters.len()
            )));
        }
        let mut indices = Vec::new();
        for &(name, default) in effect.parameters {
            let Some(&guid) = authored.get(name) else {
                return Err(unsupported(format!(
                    "{} effect without its parameter {name:?}",
                    effect.name
                )));
            };
            indices.push(slots.len() as u64);
            slots.push(Slot {
                guid,
                default,
                what: format!("{name:?} of a {} effect", effect.name),
            });
        }
        if matches!(effect.name, RECEIVE | DUCK_VOLUME) {
            side_chains += 1;
        }
        effect_constants.push(Val::map([
            ("type", Val::I(effect.kind)),
            ("groupConstantIndex", Val::U(effect.group as u64)),
            (
                "sendTargetEffectIndex",
                Val::U(effect_index.get(&target).map_or(NONE, |&at| at as u64)),
            ),
            ("wetMixLevelIndex", Val::U(level)),
            (
                "prevEffectIndex",
                Val::U(index.checked_sub(1).map_or(NONE, |at| at as u64)),
            ),
            ("bypass", Val::Bool(flag(effect.fields, "m_Bypass")?)),
            ("parameterIndices", unsigned(indices)),
        ]));
    }
    let mut slot_index: HashMap<Guid, usize> = HashMap::new();
    for (index, slot) in slots.iter().enumerate() {
        if slot.guid == Guid([0; 4]) || slot_index.insert(slot.guid, index).is_some() {
            return Err(unsupported(format!(
                "{} shares its GUID with another value or has none",
                slot.what
            )));
        }
    }

    // Snapshots, in the mixer's order. A value the snapshot does not store
    // takes its default; what it stores for a GUID without an index is left.
    let snapshot_ids = locals(&mixer.data, "m_Snapshots")?;
    if snapshot_ids.is_empty() {
        return Err(unsupported("mixer without snapshots"));
    }
    let mut snapshots = Vec::new();
    let mut snapshot_constants = Vec::new();
    for &id in &snapshot_ids {
        let snapshot = of_class(id, SNAPSHOT, "the snapshot list")?;
        if snapshots
            .iter()
            .any(|other: &&Document| other.file_id == snapshot.file_id)
        {
            return Err(unsupported(format!("snapshot {id} is listed twice")));
        }
        let name = text(get(&snapshot.data, "m_Name"))?;
        let stored: HashMap<Guid, &Value> = guid_map(&snapshot.data, "m_FloatValues")?
            .into_iter()
            .collect();
        let mut values = Vec::with_capacity(slots.len());
        for slot in &slots {
            values.push(match (stored.get(&slot.guid), slot.default) {
                (Some(value), _) => finite(value, &slot.what)?,
                (None, Some(default)) => default,
                (None, None) => {
                    return Err(unsupported(format!(
                        "snapshot {name:?} does not store {}, for which the engine has no defined default",
                        slot.what
                    )));
                }
            });
        }
        let mut types = Vec::new();
        let mut indices = Vec::new();
        for (guid, kind) in guid_map(&snapshot.data, "m_TransitionOverrides")? {
            let Some(&index) = slot_index.get(&guid) else {
                continue;
            };
            let Value::String(kind) = kind else {
                bail!("a transition override is not a number");
            };
            match scalar("int", kind, "a transition override")? {
                Val::I(kind) if (0..TRANSITION_TYPES).contains(&kind) => types.push(kind as u64),
                _ => {
                    return Err(unsupported(format!(
                        "snapshot {name:?} with the unknown transition type {kind}"
                    )));
                }
            }
            indices.push(index as u64);
        }
        snapshot_constants.push(Val::map([
            ("nameHash", Val::U(crc32(&name) as u64)),
            ("values", Val::floats(values)),
            ("transitionTypes", unsigned(types)),
            ("transitionIndices", unsigned(indices)),
        ]));
        snapshots.push(snapshot);
    }

    // The engine lists the index of every exposed parameter it finds, and
    // beside them as many names from the head of the authored list.
    let mut exposed_names = Vec::new();
    let mut exposed_indices = Vec::new();
    for item in list(
        get(&mixer.data, "m_ExposedParameters"),
        "m_ExposedParameters",
    )? {
        let parameter = object(item, "m_ExposedParameters")?;
        exposed_names.push(crc32(&text(get(parameter, "name"))?) as u64);
        if let Some(&index) = slot_index.get(&Guid::read(get(parameter, "guid"), "guid")?) {
            exposed_indices.push(index as u64);
        }
    }
    exposed_names.truncate(exposed_indices.len());

    let guid_of = |document: &Document, key: &str| Guid::read(get(&document.data, key), key);
    let mut group_guids = Vec::new();
    for (group, _) in &groups {
        group_guids.push(guid_of(group, "m_GroupID")?.val());
    }
    let mut effect_guids = Vec::new();
    for effect in &effects {
        effect_guids.push(Guid::read(get(effect.fields, "m_EffectID"), "m_EffectID")?.val());
    }
    let mut snapshot_guids = Vec::new();
    for snapshot in &snapshots {
        snapshot_guids.push(guid_of(snapshot, "m_SnapshotID")?.val());
    }
    let name_of = |document: &Document| text(get(&document.data, "m_Name"));
    let group_names: Vec<String> = groups
        .iter()
        .map(|(g, _)| name_of(g))
        .collect::<Result<_>>()?;
    let snapshot_names: Vec<String> = snapshots
        .iter()
        .map(|s| name_of(s))
        .collect::<Result<_>>()?;
    let constant = Val::map([
        ("groups", Val::Seq(group_constants)),
        ("groupGUIDs", Val::Seq(group_guids)),
        ("effects", Val::Seq(effect_constants)),
        ("effectGUIDs", Val::Seq(effect_guids)),
        ("numSideChainBuffers", Val::U(side_chains)),
        ("snapshots", Val::Seq(snapshot_constants)),
        ("snapshotGUIDs", Val::Seq(snapshot_guids)),
        ("groupNameBuffer", names(group_names.into_iter())),
        ("snapshotNameBuffer", names(snapshot_names.into_iter())),
        // Names of effects from native plugins, of which none is baked.
        ("pluginEffectNameBuffer", Val::Seq(vec![Val::I(0)])),
        ("exposedParameterNames", unsigned(exposed_names)),
        ("exposedParameterIndices", unsigned(exposed_indices)),
    ]);

    let mut references = References::new(&ids);
    let [mixer_tree, group_tree, snapshot_tree] = trees;
    let mut objects = Vec::new();
    for document in documents {
        let fields = &document.data;
        let name = Val::Str(text(get(fields, "m_Name"))?);
        let (tree, value) = match document.class_id {
            MIXER => {
                // A mixer plays into a group of another mixer, never of its own.
                let output = get(fields, "m_OutputGroup");
                if matches!(output, Value::Object(map) if text(get(map, "guid"))?.is_empty())
                    && local(output, "m_OutputGroup")? != 0
                {
                    return Err(unsupported("mixer that plays into a group of its own"));
                }
                let mut listed = Vec::new();
                for item in list(get(fields, "m_Snapshots"), "m_Snapshots")? {
                    listed.push(references.pointer(item, "m_Snapshots")?);
                }
                let start = get(fields, "m_StartSnapshot");
                if local(start, "m_StartSnapshot")? != 0 {
                    of_class(
                        local(start, "m_StartSnapshot")?,
                        SNAPSHOT,
                        "m_StartSnapshot",
                    )?;
                }
                let value = Val::map([
                    ("m_Name", name),
                    (
                        "m_OutputGroup",
                        references.pointer(output, "m_OutputGroup")?,
                    ),
                    (
                        "m_MasterGroup",
                        references.pointer(get(fields, "m_MasterGroup"), "m_MasterGroup")?,
                    ),
                    ("m_Snapshots", Val::Seq(listed)),
                    (
                        "m_StartSnapshot",
                        references.pointer(start, "m_StartSnapshot")?,
                    ),
                    (
                        "m_SuspendThreshold",
                        number("float", fields, "m_SuspendThreshold")?,
                    ),
                    (
                        "m_EnableSuspend",
                        Val::Bool(flag(fields, "m_EnableSuspend")?),
                    ),
                    ("m_UpdateMode", number("int", fields, "m_UpdateMode")?),
                    ("m_MixerConstant", constant.clone()),
                ]);
                (mixer_tree, value)
            }
            GROUP => {
                let mut children = Vec::new();
                for child in list(get(fields, "m_Children"), "m_Children")? {
                    of_class(local(child, "m_Children")?, GROUP, "m_Children")?;
                    children.push(references.pointer(child, "m_Children")?);
                }
                let value = Val::map([
                    ("m_Name", name),
                    (
                        "m_AudioMixer",
                        references.pointer(get(fields, "m_AudioMixer"), "m_AudioMixer")?,
                    ),
                    ("m_GroupID", guid_of(document, "m_GroupID")?.val()),
                    ("m_Children", Val::Seq(children)),
                ]);
                (group_tree, value)
            }
            SNAPSHOT => {
                let value = Val::map([
                    ("m_Name", name),
                    (
                        "m_AudioMixer",
                        references.pointer(get(fields, "m_AudioMixer"), "m_AudioMixer")?,
                    ),
                    ("m_SnapshotID", guid_of(document, "m_SnapshotID")?.val()),
                ]);
                (snapshot_tree, value)
            }
            _ => continue,
        };
        objects.push(NativeObject {
            file_id: document.file_id.clone(),
            class_id: document.class_id as i32,
            data: tree.encode(&value)?,
        });
    }
    Ok((objects, references.into_externals()))
}
