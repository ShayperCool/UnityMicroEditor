//! `AnimatorController` (class 91): the authored state machine graph to the
//! Player's `ControllerConstant`.
//!
//! The Editor objects of a `.controller` file (state machines, states,
//! transitions, blend trees) do not exist in a Player. A build flattens them:
//! every layer's states become one array, entry and exit nodes become selector
//! states, motions become blend tree node arrays over a clip list, and names
//! become CRC-32 hashes with a hash-to-string table beside them.
use super::{
    controller_tree, crc32, freeform,
    serialized::External,
    source::{flag, float, get, int, list, object, text},
    typetree::Val,
    unsupported,
};
use crate::yaml::Document;
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

/// Transition targets at or above this value address a selector state.
const SELECTOR_BASE: u64 = 30000;
const NONE: u64 = 0xffff_ffff;
/// The bits of the human pose mask each `AvatarMaskBodyPart` owns, in the
/// enum's order, as the pinned engine clears them for a disabled part.
const BODY_PART_BITS: [[u32; 3]; 13] = [
    [0x1, 0, 0],
    [0x3fe, 0xc000_0000, 0x1],
    [0x3f_fc00, 0, 0x6],
    [0x3fc0_0000, 0, 0x78],
    [0xc000_0000, 0x3f, 0x780],
    [0, 0x7fc0, 0x7800],
    [0, 0xff_8000, 0x7_8000],
    [0, 0x1000_0000, 0],
    [0, 0x2000_0000, 0],
    [0, 0x100_0000, 0],
    [0, 0x200_0000, 0],
    [0, 0x400_0000, 0],
    [0, 0x800_0000, 0],
];
const RESERVE_BASE: u64 = 512;
const RESERVE_FACTOR: u64 = 4;
const RESERVE_PER_LAYER: u64 = 256;

/// What a controller needs to know about a clip it plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipFacts {
    pub looping: bool,
    /// The clip's project path, such as `Assets/Run.anim`.
    pub path: String,
    pub name: String,
}

/// An authored `AvatarMask`.
#[derive(Debug, Clone, PartialEq)]
pub struct Mask {
    /// The humanoid body part switches, in `AvatarMaskBodyPart` order.
    pub body_parts: Vec<bool>,
    /// Transform paths and their weights, in authored order.
    pub transforms: Vec<(String, f32)>,
}

/// The other sources a controller's Player data is derived from.
pub trait ControllerSources {
    fn clip(&self, guid: &str, file_id: &str) -> Result<ClipFacts>;
    fn mask(&self, guid: &str) -> Result<Mask>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    file_id: String,
    guid: String,
}

fn reference(value: &Value) -> Result<Option<Reference>> {
    let map = match value {
        Value::Null => return Ok(None),
        Value::Object(map) => map,
        _ => bail!("expected an object reference"),
    };
    let file_id = text(get(map, "fileID"))?;
    if file_id.is_empty() || file_id == "0" {
        return Ok(None);
    }
    Ok(Some(Reference {
        file_id,
        guid: text(get(map, "guid"))?.to_ascii_lowercase(),
    }))
}

/// The engine writes a dot in a state or state machine name as an underscore,
/// since a dot separates the parts of a path.
fn clean(name: &str) -> String {
    name.replace('.', "_")
}

struct Machine {
    file_id: String,
    name: String,
    path: String,
    parent: Option<usize>,
}

struct State {
    file_id: String,
    machine: usize,
    name: String,
    path: String,
}

/// Where a transition leads, with the names the engine hashes for it.
struct Target {
    destination: u64,
    name: String,
    path: String,
}

struct Builder<'a> {
    documents: HashMap<&'a str, &'a Document>,
    sources: &'a dyn ControllerSources,
    names: BTreeMap<u32, String>,
    /// Every clip use, in the engine's order; a node refers to the first use.
    clips: Vec<Reference>,
    externals: Vec<External>,
    machines: Vec<Machine>,
    states: Vec<State>,
}

impl<'a> Builder<'a> {
    fn id(&mut self, name: &str) -> u64 {
        let hash = crc32(name);
        self.names.entry(hash).or_insert_with(|| name.to_string());
        hash as u64
    }

    fn document(&self, file_id: &str, class_id: i64, what: &str) -> Result<&'a Map<String, Value>> {
        match self.documents.get(file_id) {
            Some(document) if document.class_id == class_id && !document.stripped => {
                Ok(&document.data)
            }
            _ => bail!("{what} {file_id} is missing from the controller"),
        }
    }

    fn local(&self, value: &Value, what: &str) -> Result<Option<String>> {
        match reference(value)? {
            None => Ok(None),
            Some(target) if target.guid.is_empty() => Ok(Some(target.file_id)),
            Some(_) => Err(unsupported(format!("{what} in another asset"))),
        }
    }

    fn refuse_behaviours(&self, fields: &Map<String, Value>, key: &str) -> Result<()> {
        if !list(get(fields, key), key)?.is_empty() {
            return Err(unsupported("state machine behaviours"));
        }
        Ok(())
    }

    /// Collects a state machine and its children in the engine's order: a
    /// machine's own states, then each child machine in turn.
    fn collect(
        &mut self,
        file_id: &str,
        name: String,
        path: String,
        parent: Option<usize>,
    ) -> Result<()> {
        if self
            .machines
            .iter()
            .any(|machine| machine.file_id == file_id)
        {
            bail!("state machine {file_id} is used twice");
        }
        if self.machines.len() >= 4096 {
            return Err(unsupported("more state machines than the baker bounds"));
        }
        let fields = self.document(file_id, 1107, "state machine")?;
        self.refuse_behaviours(fields, "m_StateMachineBehaviours")?;
        let index = self.machines.len();
        self.machines.push(Machine {
            file_id: file_id.into(),
            name: name.clone(),
            path: path.clone(),
            parent,
        });
        let mut own = Vec::new();
        for child in list(get(fields, "m_ChildStates"), "m_ChildStates")? {
            let child = object(child, "m_ChildStates")?;
            let Some(state) = self.local(get(child, "m_State"), "state")? else {
                continue;
            };
            let state_fields = self.document(&state, 1102, "state")?;
            self.refuse_behaviours(state_fields, "m_StateMachineBehaviours")?;
            let state_name = clean(&text(get(state_fields, "m_Name"))?);
            own.push(State {
                path: format!("{path}.{state_name}"),
                file_id: state,
                machine: index,
                name: state_name,
            });
        }
        // The root machine's default state trades places with state zero.
        if parent.is_none()
            && let Some(default) = self.local(get(fields, "m_DefaultState"), "default state")?
            && let Some(position) = own.iter().position(|state| state.file_id == default)
        {
            own.swap(0, position);
        }
        self.states.extend(own);
        for child in list(get(fields, "m_ChildStateMachines"), "m_ChildStateMachines")? {
            let child = object(child, "m_ChildStateMachines")?;
            let Some(machine) = self.local(get(child, "m_StateMachine"), "state machine")? else {
                continue;
            };
            let child_fields = self.document(&machine, 1107, "state machine")?;
            let child_name = clean(&text(get(child_fields, "m_Name"))?);
            self.collect(
                &machine,
                child_name.clone(),
                format!("{path}.{child_name}"),
                Some(index),
            )?;
        }
        Ok(())
    }

    fn state_index(&self, file_id: &str, first: usize) -> Option<usize> {
        self.states[first..]
            .iter()
            .position(|state| state.file_id == file_id)
            .map(|index| index + first)
    }

    fn machine_index(&self, file_id: &str, first: usize) -> Option<usize> {
        self.machines[first..]
            .iter()
            .position(|machine| machine.file_id == file_id)
            .map(|index| index + first)
    }

    /// Resolves a transition's destination. `owner` is the machine whose exit
    /// an exit transition takes; indices are relative to the layer's first
    /// state and machine.
    fn target(
        &self,
        fields: &Map<String, Value>,
        owner: usize,
        first_state: usize,
        first_machine: usize,
    ) -> Result<Target> {
        if let Some(state) = self.local(get(fields, "m_DstState"), "destination state")? {
            let Some(index) = self.state_index(&state, first_state) else {
                bail!("transition leads to state {state} outside its layer");
            };
            return Ok(Target {
                destination: (index - first_state) as u64,
                name: self.states[index].name.clone(),
                path: self.states[index].path.clone(),
            });
        }
        if let Some(machine) =
            self.local(get(fields, "m_DstStateMachine"), "destination machine")?
        {
            let Some(index) = self.machine_index(&machine, first_machine) else {
                bail!("transition leads to state machine {machine} outside its layer");
            };
            return Ok(Target {
                destination: SELECTOR_BASE + 2 * (index - first_machine) as u64,
                name: self.machines[index].name.clone(),
                // The engine names a machine destination like an exit.
                path: "Exit".into(),
            });
        }
        if flag(get(fields, "m_IsExit"), "transition")? {
            return Ok(Target {
                destination: SELECTOR_BASE + 2 * (owner - first_machine) as u64 + 1,
                name: "Exit".into(),
                path: "Exit".into(),
            });
        }
        Err(unsupported("transition without a destination"))
    }

    /// The transitions the engine keeps: muted ones are dropped, and once any
    /// is solo only the solo ones remain.
    fn active(&self, value: &Value, class_id: i64) -> Result<Vec<&'a Map<String, Value>>> {
        let mut transitions = Vec::new();
        for item in list(value, "transitions")? {
            let Some(file_id) = self.local(item, "transition")? else {
                continue;
            };
            let fields = self.document(&file_id, class_id, "transition")?;
            if !flag(get(fields, "m_Mute"), "transition")? {
                transitions.push(fields);
            }
        }
        let mut solo = Vec::new();
        for fields in &transitions {
            if flag(get(fields, "m_Solo"), "transition")? {
                solo.push(*fields);
            }
        }
        Ok(if solo.is_empty() { transitions } else { solo })
    }

    fn conditions(&mut self, fields: &Map<String, Value>) -> Result<Val> {
        let mut result = Vec::new();
        for item in list(get(fields, "m_Conditions"), "m_Conditions")? {
            let condition = object(item, "m_Conditions")?;
            let event = text(get(condition, "m_ConditionEvent"))?;
            result.push(Val::map([
                (
                    "m_ConditionMode",
                    Val::I(int(get(condition, "m_ConditionMode"), "condition")?),
                ),
                ("m_EventID", Val::U(self.id(&event))),
                (
                    "m_EventThreshold",
                    // Unity's own spelling of the authored field.
                    Val::F(float(get(condition, "m_EventTreshold"), "condition")?),
                ),
            ]));
        }
        Ok(Val::Seq(result))
    }

    fn state_transition(
        &mut self,
        fields: &Map<String, Value>,
        source: (&str, &str),
        target: Target,
    ) -> Result<Val> {
        let name = text(get(fields, "m_Name"))?;
        let user = if name.is_empty() { 0 } else { self.id(&name) };
        let number = |key: &str| float(get(fields, key), "transition");
        let boolean = |key: &str| flag(get(fields, key), "transition");
        Ok(Val::map([
            ("m_ConditionConstantArray", self.conditions(fields)?),
            ("m_DestinationState", Val::U(target.destination)),
            (
                "m_FullPathID",
                Val::U(self.id(&format!("{} -> {}", source.1, target.path))),
            ),
            (
                "m_ID",
                Val::U(self.id(&format!("{} -> {}", source.0, target.name))),
            ),
            ("m_UserID", Val::U(user)),
            (
                "m_TransitionDuration",
                Val::F(number("m_TransitionDuration")?),
            ),
            ("m_TransitionOffset", Val::F(number("m_TransitionOffset")?)),
            ("m_ExitTime", Val::F(number("m_ExitTime")?)),
            ("m_HasExitTime", Val::Bool(boolean("m_HasExitTime")?)),
            (
                "m_HasFixedDuration",
                Val::Bool(boolean("m_HasFixedDuration")?),
            ),
            (
                "m_InterruptionSource",
                Val::I(int(get(fields, "m_InterruptionSource"), "transition")?),
            ),
            (
                "m_OrderedInterruption",
                Val::Bool(boolean("m_OrderedInterruption")?),
            ),
            (
                "m_CanTransitionToSelf",
                Val::Bool(boolean("m_CanTransitionToSelf")?),
            ),
        ]))
    }

    fn selector_transition(&mut self, fields: &Map<String, Value>, target: &Target) -> Result<Val> {
        Ok(Val::map([
            ("m_Destination", Val::U(target.destination)),
            ("m_ConditionConstantArray", self.conditions(fields)?),
        ]))
    }

    fn clip_reference(&self, value: &Value) -> Result<Option<Reference>> {
        match reference(value)? {
            Some(target) if !target.guid.is_empty() => Ok(Some(target)),
            _ => Ok(None),
        }
    }

    /// Records every clip use of a motion, in node order.
    fn register_clips(&mut self, motion: &Value, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(unsupported(
                "blend trees nested deeper than the baker bounds",
            ));
        }
        if let Some(clip) = self.clip_reference(motion)? {
            self.clips.push(clip);
        } else if let Some(tree) = self.local(motion, "motion")? {
            let fields = self.document(&tree, 206, "blend tree")?;
            for child in list(get(fields, "m_Childs"), "m_Childs")? {
                self.register_clips(get(object(child, "m_Childs")?, "m_Motion"), depth + 1)?;
            }
        }
        Ok(())
    }

    fn leaf(
        &mut self,
        clip: &Reference,
        time_scale: f32,
        cycle_offset: f32,
        mirror: bool,
        child: bool,
    ) -> Result<(Val, bool)> {
        let Some(index) = self.clips.iter().position(|used| used == clip) else {
            bail!("clip {} was not registered", clip.guid);
        };
        let facts = self.sources.clip(&clip.guid, &clip.file_id)?;
        if child {
            // The engine also records a blend tree child under this label.
            self.id(&format!("{}{}", facts.path, facts.name));
        }
        Ok((
            Val::map([
                ("m_BlendEventID", Val::U(NONE)),
                ("m_BlendEventYID", Val::U(NONE)),
                ("m_ClipID", Val::U(index as u64)),
                ("m_Duration", Val::F(1.0 / time_scale)),
                ("m_CycleOffset", Val::F(cycle_offset)),
                ("m_Mirror", Val::Bool(mirror)),
            ]),
            facts.looping,
        ))
    }

    /// Appends the nodes of a blend tree in preorder and reports whether all
    /// of its clips loop.
    fn tree_nodes(&mut self, file_id: &str, nodes: &mut Vec<Val>) -> Result<bool> {
        let fields = self.document(file_id, 206, "blend tree")?;
        let kind = int(get(fields, "m_BlendType"), "blend tree")?;
        if !(0..=4).contains(&kind) {
            bail!("blend tree {file_id} has an unknown blend type");
        }
        let parameter = self.id(&text(get(fields, "m_BlendParameter"))?);
        let parameter_y = self.id(&text(get(fields, "m_BlendParameterY"))?);
        let own = nodes.len();
        nodes.push(Val::Zero);
        let (mut indices, mut thresholds, mut positions, mut direct) =
            (Vec::new(), Vec::new(), Vec::<[f32; 2]>::new(), Vec::new());
        let mut looping = true;
        for child in list(get(fields, "m_Childs"), "m_Childs")? {
            let child = object(child, "m_Childs")?;
            let motion = get(child, "m_Motion");
            let direct_name = text(get(child, "m_DirectBlendParameter"))?;
            let direct_id = self.id(&direct_name);
            let index = nodes.len();
            if let Some(clip) = self.clip_reference(motion)? {
                let (node, loops) = self.leaf(
                    &clip,
                    float(get(child, "m_TimeScale"), "blend tree child")?,
                    float(get(child, "m_CycleOffset"), "blend tree child")?,
                    flag(get(child, "m_Mirror"), "blend tree child")?,
                    true,
                )?;
                nodes.push(node);
                looping &= loops;
            } else if let Some(tree) = self.local(motion, "motion")? {
                looping &= self.tree_nodes(&tree, nodes)?;
            } else {
                // A child without a motion leaves no node.
                continue;
            }
            indices.push(Val::U(index as u64));
            thresholds.push(Val::F(float(
                get(child, "m_Threshold"),
                "blend tree child",
            )?));
            let position = object(get(child, "m_Position"), "blend tree child position");
            if (1..=3).contains(&kind) {
                let position = position?;
                positions.push([
                    float(get(position, "x"), "blend tree child")?,
                    float(get(position, "y"), "blend tree child")?,
                ]);
            }
            if kind == 4 {
                direct.push(Val::U(direct_id));
            }
        }
        let planar = (1..=3).contains(&kind);
        let point = |[x, y]: [f32; 2]| Val::map([("x", Val::F(x)), ("y", Val::F(y))]);
        let mut planar_data = vec![(
            "m_ChildPositionArray",
            Val::Seq(positions.iter().copied().map(point).collect()),
        )];
        if kind == 2 || kind == 3 {
            let table = freeform::precompute(kind == 2, &positions);
            planar_data.extend([
                ("m_ChildMagnitudeArray", Val::floats(table.magnitudes)),
                (
                    "m_ChildPairVectorArray",
                    Val::Seq(table.pair_vectors.into_iter().map(point).collect()),
                ),
                ("m_ChildPairAvgMagInvArray", Val::floats(table.pair_scales)),
                (
                    "m_ChildNeighborListArray",
                    Val::Seq(
                        table
                            .neighbors
                            .into_iter()
                            .map(|list| {
                                Val::map([(
                                    "m_NeighborArray",
                                    Val::Seq(list.into_iter().map(|i| Val::U(i as u64)).collect()),
                                )])
                            })
                            .collect(),
                    ),
                ),
            ]);
        }
        nodes[own] = Val::map([
            ("m_BlendType", Val::I(kind)),
            (
                "m_BlendEventID",
                Val::U(if kind == 4 { NONE } else { parameter }),
            ),
            (
                "m_BlendEventYID",
                Val::U(if planar { parameter_y } else { NONE }),
            ),
            ("m_ChildIndices", Val::Seq(indices)),
            (
                "m_Blend1dData",
                Val::map([(
                    "m_ChildThresholdArray",
                    Val::Seq(if kind == 0 { thresholds } else { Vec::new() }),
                )]),
            ),
            (
                "m_Blend2dData",
                Val::Map(
                    planar_data
                        .into_iter()
                        .map(|(name, value)| (name.into(), value))
                        .collect(),
                ),
            ),
            (
                "m_BlendDirectData",
                Val::map([
                    ("m_ChildBlendEventIDArray", Val::Seq(direct)),
                    (
                        "m_NormalizedBlendValues",
                        Val::Bool(
                            kind == 4
                                && flag(get(fields, "m_NormalizedBlendValues"), "blend tree")?,
                        ),
                    ),
                ]),
            ),
            ("m_ClipID", Val::U(NONE)),
        ]);
        Ok(looping)
    }

    /// A state's motion as a blend tree constant; `None` when it has none.
    fn motion(&mut self, motion: &Value) -> Result<Option<(Val, bool)>> {
        let mut nodes = Vec::new();
        let looping = if let Some(clip) = self.clip_reference(motion)? {
            let (node, looping) = self.leaf(&clip, 1.0, 0.0, false, false)?;
            nodes.push(node);
            looping
        } else if let Some(tree) = self.local(motion, "motion")? {
            self.tree_nodes(&tree, &mut nodes)?
        } else {
            return Ok(None);
        };
        Ok(Some((
            Val::map([("m_NodeArray", Val::Seq(nodes))]),
            looping,
        )))
    }
}

struct Layer<'a> {
    fields: &'a Map<String, Value>,
    name: String,
    /// The layer whose state machine this one follows, if it is synced.
    synced: Option<usize>,
    /// The first state and machine of the layer's own state machine.
    first_state: usize,
    first_machine: usize,
    state_count: usize,
    machine_count: usize,
    machine_index: usize,
}

/// The serialized `AnimatorController` object and the assets it refers to.
pub fn bake_controller_object(
    documents: &[Document],
    sources: &dyn ControllerSources,
) -> Result<(Vec<u8>, Vec<External>)> {
    let mut by_id = HashMap::new();
    for document in documents {
        by_id.insert(document.file_id.as_str(), document);
    }
    let mut controllers = documents.iter().filter(|document| document.class_id == 91);
    let (Some(controller), None) = (controllers.next(), controllers.next()) else {
        bail!("controller source does not hold exactly one AnimatorController");
    };
    if documents.iter().any(|document| document.class_id == 114) {
        return Err(unsupported("state machine behaviours"));
    }
    let fields = &controller.data;
    let mut builder = Builder {
        documents: by_id,
        sources,
        names: BTreeMap::new(),
        clips: Vec::new(),
        externals: Vec::new(),
        machines: Vec::new(),
        states: Vec::new(),
    };
    builder.id("GravityWeight");

    let mut layers: Vec<Layer<'_>> = Vec::new();
    let authored = list(get(fields, "m_AnimatorLayers"), "m_AnimatorLayers")?;
    let mut machine_count = 0;
    for item in authored {
        let layer = object(item, "m_AnimatorLayers")?;
        builder.refuse_behaviours(layer, "m_Behaviours")?;
        let name = text(get(layer, "m_Name"))?;
        let synced = int(get(layer, "m_SyncedLayerIndex"), "layer")?;
        let (first_state, first_machine) = (builder.states.len(), builder.machines.len());
        let mut entry = Layer {
            fields: layer,
            name: name.clone(),
            synced: None,
            first_state,
            first_machine,
            state_count: 0,
            machine_count: 0,
            machine_index: 0,
        };
        if synced >= 0 {
            let Some(source) = layers.get(synced as usize) else {
                return Err(unsupported("layer synced to a later layer"));
            };
            if source.synced.is_some() {
                return Err(unsupported("layer synced to a synced layer"));
            }
            entry.synced = Some(synced as usize);
        } else {
            let Some(root) = builder.local(get(layer, "m_StateMachine"), "layer state machine")?
            else {
                return Err(unsupported("layer without a state machine"));
            };
            builder.collect(&root, name.clone(), name, None)?;
            entry.state_count = builder.states.len() - first_state;
            entry.machine_count = builder.machines.len() - first_machine;
            entry.machine_index = machine_count;
            machine_count += 1;
        }
        layers.push(entry);
    }

    // The clip list holds every use, layer by layer in authored state order.
    for index in 0..layers.len() {
        let layer = &layers[index];
        if let Some(source) = layer.synced {
            let overrides = overrides(layer.fields)?;
            let source = &layers[source];
            for machine in source.first_machine..source.first_machine + source.machine_count {
                for state in authored_states(&builder, machine)? {
                    if let Some(motion) = overrides.get(&state) {
                        builder.register_clips(motion, 0)?;
                    }
                }
            }
        } else {
            for machine in layer.first_machine..layer.first_machine + layer.machine_count {
                for state in authored_states(&builder, machine)? {
                    let fields = builder.document(&state, 1102, "state")?;
                    builder.register_clips(get(fields, "m_Motion"), 0)?;
                }
            }
        }
    }

    let mut layer_constants = Vec::new();
    let mut machine_constants = Vec::new();
    for (index, layer) in layers.iter().enumerate() {
        let source = layer.synced.map_or(layer, |source| &layers[source]);
        let synced: Vec<usize> = (0..layers.len())
            .filter(|&other| layers[other].synced == Some(layer.synced.unwrap_or(index)))
            .collect();
        let order = match layer.synced {
            Some(_) => synced.iter().position(|&other| other == index).unwrap() + 1,
            None => 0,
        };
        let mut body = [0xffff_ffffu32, 0xffff_ffff, 0x7_ffff];
        let mut skeleton = Vec::new();
        match reference(get(layer.fields, "m_Mask"))? {
            None => {}
            Some(mask) if mask.guid.is_empty() => {
                return Err(unsupported("avatar mask stored inside the controller"));
            }
            Some(mask) => {
                let mask = sources.mask(&mask.guid)?;
                if mask.body_parts.len() != BODY_PART_BITS.len() {
                    return Err(unsupported("avatar mask with an unknown set of body parts"));
                }
                for (active, bits) in mask.body_parts.iter().zip(BODY_PART_BITS) {
                    if !active {
                        for (word, cleared) in body.iter_mut().zip(bits) {
                            *word &= !cleared;
                        }
                    }
                }
                for (path, weight) in &mask.transforms {
                    skeleton.push(Val::map([
                        ("m_PathHash", Val::U(crc32(path) as u64)),
                        ("m_Weight", Val::F(*weight)),
                    ]));
                }
            }
        }
        layer_constants.push(Val::map([
            ("m_StateMachineIndex", Val::U(source.machine_index as u64)),
            ("m_StateMachineSynchronizedLayerIndex", Val::U(order as u64)),
            (
                "m_BodyMask",
                Val::map([
                    ("word0", Val::U(body[0] as u64)),
                    ("word1", Val::U(body[1] as u64)),
                    ("word2", Val::U(body[2] as u64)),
                ]),
            ),
            ("m_SkeletonMask", Val::map([("m_Data", Val::Seq(skeleton))])),
            ("m_Binding", Val::U(builder.id(&layer.name))),
            (
                "(int&)m_LayerBlendingMode",
                Val::I(int(get(layer.fields, "m_BlendingMode"), "layer")?),
            ),
            (
                "m_DefaultWeight",
                Val::F(float(get(layer.fields, "m_DefaultWeight"), "layer")?),
            ),
            (
                "m_IKPass",
                Val::Bool(flag(get(layer.fields, "m_IKPass"), "layer")?),
            ),
            (
                "m_SyncedLayerAffectsTiming",
                Val::Bool(flag(
                    get(layer.fields, "m_SyncedLayerAffectsTiming"),
                    "layer",
                )?),
            ),
        ]));
        if layer.synced.is_some() {
            continue;
        }
        let mut synced_overrides = Vec::new();
        for &other in &synced {
            synced_overrides.push(overrides(layers[other].fields)?);
        }
        machine_constants.push(machine_constant(
            &mut builder,
            layer,
            &synced_overrides,
            flag(get(fields, "m_EvaluateTransitionsOnStart"), "controller")?,
        )?);
    }

    let (mut values, mut floats, mut ints, mut bools) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for item in list(get(fields, "m_AnimatorParameters"), "m_AnimatorParameters")? {
        let parameter = object(item, "m_AnimatorParameters")?;
        let kind = int(get(parameter, "m_Type"), "parameter")?;
        let index = match kind {
            1 => {
                floats.push(Val::F(float(
                    get(parameter, "m_DefaultFloat"),
                    "parameter",
                )?));
                floats.len() - 1
            }
            3 => {
                ints.push(Val::I(int(get(parameter, "m_DefaultInt"), "parameter")?));
                ints.len() - 1
            }
            // A trigger is stored with the booleans.
            4 | 9 => {
                bools.push(Val::Bool(flag(
                    get(parameter, "m_DefaultBool"),
                    "parameter",
                )?));
                bools.len() - 1
            }
            _ => bail!("controller parameter has an unknown type {kind}"),
        };
        let name = text(get(parameter, "m_Name"))?;
        values.push(Val::map([
            ("m_ID", Val::U(builder.id(&name))),
            ("m_Type", Val::I(kind)),
            ("m_Index", Val::U(index as u64)),
        ]));
    }

    let mut clips = Vec::new();
    for clip in builder.clips.clone() {
        let position = match builder
            .externals
            .iter()
            .position(|external| external.guid == clip.guid)
        {
            Some(position) => position,
            None => {
                builder.externals.push(External {
                    guid: clip.guid.clone(),
                });
                builder.externals.len() - 1
            }
        };
        let Ok(path_id) = clip.file_id.parse::<i64>() else {
            bail!("clip reference has an invalid file ID");
        };
        clips.push(Val::map([
            ("m_FileID", Val::I(position as i64 + 1)),
            ("m_PathID", Val::I(path_id)),
        ]));
    }
    let names = builder
        .names
        .iter()
        .map(|(hash, name)| {
            Val::map([
                ("first", Val::U(*hash as u64)),
                ("second", Val::Str(name.clone())),
            ])
        })
        .collect();
    let evaluate = flag(get(fields, "m_EvaluateTransitionsOnStart"), "controller")?;
    let has_layers = !layer_constants.is_empty();
    let constant = Val::map([
        ("m_LayerArray", Val::Seq(layer_constants)),
        ("m_StateMachineArray", Val::Seq(machine_constants)),
        ("m_Values", Val::map([("m_ValueArray", Val::Seq(values))])),
        (
            "m_DefaultValues",
            Val::map([
                ("m_FloatValues", Val::Seq(floats)),
                ("m_IntValues", Val::Seq(ints)),
                ("m_BoolValues", Val::Seq(bools)),
            ]),
        ),
    ]);
    // The Player reserves this much for the constant in memory before reading
    // it, and chains further blocks if that runs out. The engine's exact
    // figure depends on its allocator; this stays above it for every
    // reference controller, so one block is enough.
    let reserve = if has_layers {
        let serialized = controller_tree()?
            .encode(&Val::map([("m_Controller", constant.clone())]))?
            .len();
        RESERVE_BASE + RESERVE_FACTOR * serialized as u64 + RESERVE_PER_LAYER * layers.len() as u64
    } else {
        0
    };
    let object = Val::map([
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        ("m_EvaluateTransitionsOnStart", Val::Bool(evaluate)),
        ("m_ControllerSize", Val::U(reserve)),
        ("m_Controller", constant),
        ("m_TOS", Val::Seq(names)),
        ("m_AnimationClips", Val::Seq(clips)),
        ("m_MultiThreadedStateMachine", Val::Bool(true)),
    ]);
    Ok((controller_tree()?.encode(&object)?, builder.externals))
}

/// A synced layer's replacement motions, by state file ID.
fn overrides(layer: &Map<String, Value>) -> Result<HashMap<String, &Value>> {
    let mut result = HashMap::new();
    for item in list(get(layer, "m_Motions"), "m_Motions")? {
        let entry = object(item, "m_Motions")?;
        if let Some(state) = reference(get(entry, "m_State"))? {
            result.insert(state.file_id, get(entry, "m_Motion"));
        }
    }
    Ok(result)
}

/// A machine's states in the order the file lists them.
fn authored_states(builder: &Builder<'_>, machine: usize) -> Result<Vec<String>> {
    let fields = builder.document(&builder.machines[machine].file_id, 1107, "state machine")?;
    let mut result = Vec::new();
    for child in list(get(fields, "m_ChildStates"), "m_ChildStates")? {
        if let Some(state) =
            builder.local(get(object(child, "m_ChildStates")?, "m_State"), "state")?
        {
            result.push(state);
        }
    }
    Ok(result)
}

fn machine_constant(
    builder: &mut Builder<'_>,
    layer: &Layer<'_>,
    synced: &[HashMap<String, &Value>],
    evaluate: bool,
) -> Result<Val> {
    let (first_state, first_machine) = (layer.first_state, layer.first_machine);
    let mut states = Vec::new();
    for index in first_state..first_state + layer.state_count {
        let (file_id, machine, name, path) = {
            let state = &builder.states[index];
            (
                state.file_id.clone(),
                state.machine,
                state.name.clone(),
                state.path.clone(),
            )
        };
        let fields = builder.document(&file_id, 1102, "state")?;
        let mut transitions = Vec::new();
        for transition in builder.active(get(fields, "m_Transitions"), 1101)? {
            let target = builder.target(transition, machine, first_state, first_machine)?;
            transitions.push(builder.state_transition(transition, (&name, &path), target)?);
        }
        let (mut indices, mut trees) = (Vec::new(), Vec::new());
        let mut looping = true;
        let base = builder.motion(get(fields, "m_Motion"))?;
        let replaced = synced.iter().map(|layer| layer.get(&file_id).copied());
        for (position, motion) in std::iter::once(base)
            .chain(
                replaced
                    .map(|motion| motion.map_or(Ok(None), |motion| builder.motion(motion)))
                    .collect::<Result<Vec<_>>>()?,
            )
            .enumerate()
        {
            match motion {
                Some((tree, loops)) => {
                    indices.push(Val::I(trees.len() as i64));
                    trees.push(tree);
                    if position == 0 {
                        looping = loops;
                    }
                }
                None => indices.push(Val::I(-1)),
            }
        }
        let machine_name = builder.machines[machine].name.clone();
        let parameter = |builder: &mut Builder<'_>, active: &str, name: &str| -> Result<Val> {
            Ok(Val::U(if flag(get(fields, active), "state")? {
                let name = text(get(fields, name))?;
                builder.id(&name)
            } else {
                0
            }))
        };
        let tag = text(get(fields, "m_Tag"))?;
        states.push(Val::map([
            ("m_TransitionConstantArray", Val::Seq(transitions)),
            ("m_BlendTreeConstantIndexArray", Val::Seq(indices)),
            ("m_BlendTreeConstantArray", Val::Seq(trees)),
            ("m_NameID", Val::U(builder.id(&name))),
            (
                "m_PathID",
                Val::U(builder.id(&format!("{machine_name}.{name}"))),
            ),
            ("m_FullPathID", Val::U(builder.id(&path))),
            ("m_TagID", Val::U(builder.id(&tag))),
            (
                "m_SpeedParamID",
                parameter(builder, "m_SpeedParameterActive", "m_SpeedParameter")?,
            ),
            (
                "m_MirrorParamID",
                parameter(builder, "m_MirrorParameterActive", "m_MirrorParameter")?,
            ),
            (
                "m_CycleOffsetParamID",
                parameter(
                    builder,
                    "m_CycleOffsetParameterActive",
                    "m_CycleOffsetParameter",
                )?,
            ),
            (
                "m_TimeParamID",
                parameter(builder, "m_TimeParameterActive", "m_TimeParameter")?,
            ),
            ("m_Speed", Val::F(float(get(fields, "m_Speed"), "state")?)),
            (
                "m_CycleOffset",
                Val::F(float(get(fields, "m_CycleOffset"), "state")?),
            ),
            (
                "m_IKOnFeet",
                Val::Bool(flag(get(fields, "m_IKOnFeet"), "state")?),
            ),
            (
                "m_WriteDefaultValues",
                Val::Bool(flag(get(fields, "m_WriteDefaultValues"), "state")?),
            ),
            ("m_Loop", Val::Bool(looping)),
            (
                "m_Mirror",
                Val::Bool(flag(get(fields, "m_Mirror"), "state")?),
            ),
        ]));
    }

    let root = builder.document(
        &builder.machines[first_machine].file_id,
        1107,
        "state machine",
    )?;
    let mut any = Vec::new();
    for transition in builder.active(get(root, "m_AnyStateTransitions"), 1101)? {
        let target = builder.target(transition, first_machine, first_state, first_machine)?;
        let path = target.path.clone();
        any.push(builder.state_transition(
            transition,
            ("AnyState", "Entry"),
            Target { path, ..target },
        )?);
    }

    let mut selectors = Vec::new();
    for machine in first_machine..first_machine + layer.machine_count {
        let (file_id, parent, path) = {
            let entry = &builder.machines[machine];
            (entry.file_id.clone(), entry.parent, entry.path.clone())
        };
        let fields = builder.document(&file_id, 1107, "state machine")?;
        let mut entry = Vec::new();
        for transition in builder.active(get(fields, "m_EntryTransitions"), 1109)? {
            let target = builder.target(transition, machine, first_state, first_machine)?;
            entry.push(builder.selector_transition(transition, &target)?);
        }
        // The entry node always ends in the default state, or in nothing.
        let default = match builder.local(get(fields, "m_DefaultState"), "default state")? {
            Some(default) => builder
                .state_index(&default, first_state)
                .map_or(NONE, |index| (index - first_state) as u64),
            None => NONE,
        };
        entry.push(Val::map([("m_Destination", Val::U(default))]));
        let mut exit = Vec::new();
        match parent {
            None => exit.push(Val::map([("m_Destination", Val::U(SELECTOR_BASE))])),
            Some(parent) => {
                let parent_fields =
                    builder.document(&builder.machines[parent].file_id, 1107, "state machine")?;
                for pair in pairs(get(parent_fields, "m_StateMachineTransitions"))? {
                    if reference(pair.0)?.is_some_and(|owner| owner.file_id == file_id) {
                        for transition in builder.active(pair.1, 1109)? {
                            let target =
                                builder.target(transition, parent, first_state, first_machine)?;
                            exit.push(builder.selector_transition(transition, &target)?);
                        }
                    }
                }
            }
        }
        let hash = crc32(&path) as u64;
        for (transitions, is_entry) in [(entry, true), (exit, false)] {
            selectors.push(Val::map([
                ("m_TransitionConstantArray", Val::Seq(transitions)),
                ("m_FullPathID", Val::U(hash)),
                ("m_IsEntry", Val::Bool(is_entry)),
            ]));
        }
    }
    Ok(Val::map([
        ("m_StateConstantArray", Val::Seq(states)),
        ("m_AnyStateTransitionConstantArray", Val::Seq(any)),
        ("m_SelectorStateConstantArray", Val::Seq(selectors)),
        ("m_SynchronizedLayerCount", Val::U(1 + synced.len() as u64)),
        ("m_EvaluateTransitionsOnStart", Val::Bool(evaluate)),
    ]))
}

/// The entries of a serialized map: `first`/`second` pairs.
fn pairs(value: &Value) -> Result<Vec<(&Value, &Value)>> {
    let mut result = Vec::new();
    // Unity writes an empty map as `{}`.
    if matches!(value, Value::Object(map) if map.is_empty()) {
        return Ok(result);
    }
    for item in list(value, "map")? {
        let pair = object(item, "map")?;
        result.push((get(pair, "first"), get(pair, "second")));
    }
    Ok(result)
}

/// Reads what a controller needs from an authored `.anim` source.
pub fn read_clip_facts(yaml: &[u8], path: &str) -> Result<ClipFacts> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let Some(clip) = documents.iter().find(|document| document.class_id == 74) else {
        bail!("animation source does not hold an AnimationClip");
    };
    let looping = match get(&clip.data, "m_AnimationClipSettings") {
        Value::Object(settings) => flag(get(settings, "m_LoopTime"), "animation clip")?,
        _ => false,
    };
    Ok(ClipFacts {
        looping,
        path: path.to_string(),
        name: text(get(&clip.data, "m_Name"))?,
    })
}

/// Reads an authored `.mask` source.
pub fn read_mask(yaml: &[u8]) -> Result<Mask> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let Some(mask) = documents.iter().find(|document| document.class_id == 319) else {
        bail!("mask source does not hold an AvatarMask");
    };
    // The body part switches are a hex dump of little-endian 32-bit integers.
    let dump = text(get(&mask.data, "m_Mask"))?;
    let mut body_parts = Vec::new();
    for word in dump.as_bytes().as_chunks::<8>().0 {
        let Ok(word) = u32::from_str_radix(&String::from_utf8_lossy(word.as_slice()), 16) else {
            bail!("avatar mask body parts are not a hex dump");
        };
        body_parts.push(word.swap_bytes() != 0);
    }
    let mut transforms = Vec::new();
    for item in list(get(&mask.data, "m_Elements"), "m_Elements")? {
        let element = object(item, "m_Elements")?;
        transforms.push((
            text(get(element, "m_Path"))?,
            float(get(element, "m_Weight"), "avatar mask")?,
        ));
    }
    Ok(Mask {
        body_parts,
        transforms,
    })
}

/// Bakes an authored `.controller` source into a ContentFile whose external
/// references are the clips it plays.
pub fn bake_controller(yaml: &[u8], sources: &dyn ControllerSources) -> Result<super::Baked> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let Some(controller) = documents.iter().find(|document| document.class_id == 91) else {
        bail!("controller source does not hold an AnimatorController");
    };
    let Ok(local_id) = controller.file_id.parse::<i64>() else {
        bail!("controller has an invalid file ID");
    };
    let (data, externals) = bake_controller_object(&documents, sources)?;
    let file = super::serialized::write(
        &[super::serialized::Object {
            local_id,
            tree: controller_tree()?,
            data,
        }],
        &externals,
    )?;
    Ok(super::Baked {
        file,
        objects: vec![(
            controller.file_id.clone(),
            "UnityEngine.RuntimeAnimatorController".to_string(),
        )],
        externals,
    })
}
