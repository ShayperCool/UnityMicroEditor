//! The parsed form of a shader (`m_ParsedForm`) out of its ShaderLab, as
//! values named as the Editor's `SerializedShader` names its fields.
use super::parse::{Command, Property, Token};
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

/// A state value: a number, or the property it is read from.
#[derive(Clone, Debug)]
struct Float {
    name: String,
    val: f64,
}

impl Float {
    fn of(val: f64) -> Self {
        Float {
            name: "<noninit>".into(),
            val,
        }
    }
    fn json(&self) -> Value {
        json!({"name": self.name, "val": self.val})
    }
}

#[derive(Clone, Debug)]
struct Blend {
    src: Float,
    dst: Float,
    src_alpha: Float,
    dst_alpha: Float,
    op: Float,
    op_alpha: Float,
    mask: Float,
}

impl Default for Blend {
    fn default() -> Self {
        Blend {
            src: Float::of(1.0),
            dst: Float::of(0.0),
            src_alpha: Float::of(1.0),
            dst_alpha: Float::of(0.0),
            op: Float::of(0.0),
            op_alpha: Float::of(0.0),
            mask: Float::of(15.0),
        }
    }
}

#[derive(Clone, Debug)]
struct StencilOp {
    pass: Float,
    fail: Float,
    zfail: Float,
    comp: Float,
}

impl Default for StencilOp {
    fn default() -> Self {
        StencilOp {
            pass: Float::of(0.0),
            fail: Float::of(0.0),
            zfail: Float::of(0.0),
            comp: Float::of(8.0),
        }
    }
}

/// The render state a pass inherits from its subshader and sets itself.
#[derive(Clone, Debug)]
struct State {
    blend: Vec<Blend>,
    separate_blend: bool,
    zclip: Float,
    ztest: Float,
    zwrite: Float,
    culling: Float,
    conservative: Float,
    offset_factor: Float,
    offset_units: Float,
    alpha_to_mask: Float,
    stencil: StencilOp,
    stencil_front: StencilOp,
    stencil_back: StencilOp,
    stencil_read: Float,
    stencil_write: Float,
    stencil_ref: Float,
    fog_mode: i64,
    fog_color: [Float; 4],
    fog_color_name: String,
    fog_density: Float,
    fog_start: Float,
    fog_end: Float,
    lighting: bool,
    tags: Vec<(String, String)>,
}

impl Default for State {
    fn default() -> Self {
        let named = |name: &str| Float {
            name: name.into(),
            val: 0.0,
        };
        State {
            blend: vec![Blend::default(); 8],
            separate_blend: false,
            zclip: Float::of(1.0),
            ztest: Float::of(4.0),
            zwrite: Float::of(1.0),
            culling: Float::of(2.0),
            conservative: Float::of(0.0),
            offset_factor: Float::of(0.0),
            offset_units: Float::of(0.0),
            alpha_to_mask: Float::of(0.0),
            stencil: StencilOp::default(),
            stencil_front: StencilOp::default(),
            stencil_back: StencilOp::default(),
            stencil_read: Float::of(255.0),
            stencil_write: Float::of(255.0),
            stencil_ref: Float::of(0.0),
            fog_mode: -1,
            fog_color: [
                Float::of(0.0),
                Float::of(0.0),
                Float::of(0.0),
                Float::of(0.0),
            ],
            fog_color_name: "unity_FogColor".into(),
            fog_density: named("unity_FogDensity"),
            fog_start: named("unity_FogStart"),
            fog_end: named("unity_FogEnd"),
            lighting: false,
            tags: Vec::new(),
        }
    }
}

/// A word or number of a state command, or the property in brackets.
fn float(token: &Token, words: &[(&str, f64)]) -> Result<Float> {
    match token {
        Token::Bracket(name) => Ok(Float {
            name: name.clone(),
            val: 0.0,
        }),
        Token::Num(number) => Ok(Float::of(number.parse()?)),
        Token::Ident(word) => words
            .iter()
            .find(|(w, _)| w.eq_ignore_ascii_case(word))
            .map(|(_, v)| Float::of(*v))
            .ok_or_else(|| anyhow::anyhow!("ShaderLab: unknown value {word}")),
        other => bail!("ShaderLab: unexpected value {other:?}"),
    }
}

const ON_OFF: &[(&str, f64)] = &[("On", 1.0), ("Off", 0.0), ("True", 1.0), ("False", 0.0)];
const CULL: &[(&str, f64)] = &[("Off", 0.0), ("Front", 1.0), ("Back", 2.0)];
const COMPARE: &[(&str, f64)] = &[
    ("Disabled", 0.0),
    ("Never", 1.0),
    ("Less", 2.0),
    ("Equal", 3.0),
    ("LEqual", 4.0),
    ("Greater", 5.0),
    ("NotEqual", 6.0),
    ("GEqual", 7.0),
    ("Always", 8.0),
];
const BLEND: &[(&str, f64)] = &[
    ("Zero", 0.0),
    ("One", 1.0),
    ("DstColor", 2.0),
    ("SrcColor", 3.0),
    ("OneMinusDstColor", 4.0),
    ("SrcAlpha", 5.0),
    ("OneMinusSrcColor", 6.0),
    ("DstAlpha", 7.0),
    ("OneMinusDstAlpha", 8.0),
    ("SrcAlphaSaturate", 9.0),
    ("OneMinusSrcAlpha", 10.0),
];
const BLEND_OP: &[(&str, f64)] = &[
    ("Add", 0.0),
    ("Sub", 1.0),
    ("RevSub", 2.0),
    ("Min", 3.0),
    ("Max", 4.0),
];
const STENCIL_OP: &[(&str, f64)] = &[
    ("Keep", 0.0),
    ("Zero", 1.0),
    ("Replace", 2.0),
    ("IncrSat", 3.0),
    ("DecrSat", 4.0),
    ("Invert", 5.0),
    ("IncrWrap", 6.0),
    ("DecrWrap", 7.0),
];

/// The values of a command without its commas.
fn values(command: &Command) -> Vec<&Token> {
    command
        .args
        .iter()
        .filter(|t| !matches!(t, Token::Punct(',')))
        .collect()
}

impl State {
    /// Applies the state commands among `commands`.
    fn apply(&mut self, commands: &[Command]) -> Result<()> {
        for command in commands {
            let args = values(command);
            let first = args.first().copied();
            let name = command.name.to_ascii_lowercase();
            match (name.as_str(), first) {
                ("cull", Some(value)) => self.culling = float(value, CULL)?,
                ("ztest", Some(value)) => self.ztest = float(value, COMPARE)?,
                ("zwrite", Some(value)) => self.zwrite = float(value, ON_OFF)?,
                ("zclip", Some(value)) => self.zclip = float(value, ON_OFF)?,
                ("alphatomask", Some(value)) => self.alpha_to_mask = float(value, ON_OFF)?,
                ("conservative", Some(value)) => self.conservative = float(value, ON_OFF)?,
                ("lighting", Some(value)) => self.lighting = float(value, ON_OFF)?.val != 0.0,
                ("offset", Some(value)) => {
                    self.offset_factor = float(value, &[])?;
                    if let Some(units) = args.get(1) {
                        self.offset_units = float(units, &[])?;
                    }
                }
                ("blend", Some(_)) => self.blend(&args)?,
                ("blendop", Some(_)) => {
                    let (target, args) = self.target(&args);
                    let Some(op) = args.first() else { continue };
                    let blend = &mut self.blend[target];
                    blend.op = float(op, BLEND_OP)?;
                    blend.op_alpha = match args.get(1) {
                        Some(alpha) => float(alpha, BLEND_OP)?,
                        None => blend.op.clone(),
                    };
                }
                ("colormask", Some(_)) => {
                    let (target, args) = self.target(&args);
                    let Some(mask) = args.first() else { continue };
                    self.blend[target].mask = color_mask(mask)?;
                }
                ("stencil", _) => {
                    for inner in command.block.iter().flatten() {
                        let Some(value) = values(inner).first().copied() else {
                            continue;
                        };
                        match inner.name.to_ascii_lowercase().as_str() {
                            "ref" => self.stencil_ref = float(value, &[])?,
                            "readmask" => self.stencil_read = float(value, &[])?,
                            "writemask" => self.stencil_write = float(value, &[])?,
                            "comp" => self.stencil.comp = float(value, COMPARE)?,
                            "pass" => self.stencil.pass = float(value, STENCIL_OP)?,
                            "fail" => self.stencil.fail = float(value, STENCIL_OP)?,
                            "zfail" => self.stencil.zfail = float(value, STENCIL_OP)?,
                            "compfront" => self.stencil_front.comp = float(value, COMPARE)?,
                            "passfront" => self.stencil_front.pass = float(value, STENCIL_OP)?,
                            "failfront" => self.stencil_front.fail = float(value, STENCIL_OP)?,
                            "zfailfront" => self.stencil_front.zfail = float(value, STENCIL_OP)?,
                            "compback" => self.stencil_back.comp = float(value, COMPARE)?,
                            "passback" => self.stencil_back.pass = float(value, STENCIL_OP)?,
                            "failback" => self.stencil_back.fail = float(value, STENCIL_OP)?,
                            "zfailback" => self.stencil_back.zfail = float(value, STENCIL_OP)?,
                            _ => {}
                        }
                    }
                }
                ("fog", _) => {
                    for inner in command.block.iter().flatten() {
                        let args = values(inner);
                        match inner.name.to_ascii_lowercase().as_str() {
                            "mode" => {
                                self.fog_mode = match args.first().map(|t| t.text()) {
                                    Some(mode) if mode.eq_ignore_ascii_case("Off") => 0,
                                    Some(mode) if mode.eq_ignore_ascii_case("Linear") => 1,
                                    Some(mode) if mode.eq_ignore_ascii_case("Exp") => 2,
                                    Some(mode) if mode.eq_ignore_ascii_case("Exp2") => 3,
                                    _ => -1,
                                }
                            }
                            "density" => {
                                if let Some(value) = args.first() {
                                    self.fog_density = float(value, &[])?;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ("tags", _) => {
                    for pair in command.block.iter().flatten() {
                        let value = pair.args.first().map(|t| t.text()).unwrap_or_default();
                        self.tags.push((pair.name.clone(), value));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The render target a command names (`Blend 1 ...`), and its other
    /// values.
    fn target<'a>(&self, args: &'a [&'a Token]) -> (usize, &'a [&'a Token]) {
        match args.first() {
            Some(Token::Num(n)) if args.len() > 1 => {
                (n.parse::<usize>().unwrap_or(0).min(7), &args[1..])
            }
            _ => (0, args),
        }
    }

    fn blend(&mut self, args: &[&Token]) -> Result<()> {
        let indexed = matches!(args.first(), Some(Token::Num(_))) && args.len() > 1;
        let (target, args) = self.target(args);
        let targets = [target];
        if indexed {
            self.separate_blend = true;
        }
        let (src, dst, src_alpha, dst_alpha) = match args {
            [off] if off.text().eq_ignore_ascii_case("Off") => (
                Float::of(1.0),
                Float::of(0.0),
                Float::of(1.0),
                Float::of(0.0),
            ),
            [src, dst] => {
                let (src, dst) = (float(src, BLEND)?, float(dst, BLEND)?);
                (src.clone(), dst.clone(), src, dst)
            }
            [src, dst, src_alpha, dst_alpha] => (
                float(src, BLEND)?,
                float(dst, BLEND)?,
                float(src_alpha, BLEND)?,
                float(dst_alpha, BLEND)?,
            ),
            _ => bail!("ShaderLab: Blend with {} values", args.len()),
        };
        for index in targets {
            let blend = &mut self.blend[index];
            blend.src = src.clone();
            blend.dst = dst.clone();
            blend.src_alpha = src_alpha.clone();
            blend.dst_alpha = dst_alpha.clone();
        }
        Ok(())
    }

    fn json(&self, name: &str, lod: i64, gpu_program: i64) -> Value {
        let blend = |b: &Blend| {
            json!({
                "srcBlend": b.src.json(), "destBlend": b.dst.json(),
                "srcBlendAlpha": b.src_alpha.json(), "destBlendAlpha": b.dst_alpha.json(),
                "blendOp": b.op.json(), "blendOpAlpha": b.op_alpha.json(),
                "colMask": b.mask.json(),
            })
        };
        let stencil = |s: &StencilOp| json!({"pass": s.pass.json(), "fail": s.fail.json(), "zFail": s.zfail.json(), "comp": s.comp.json()});
        let mut state = Map::new();
        state.insert("m_Name".into(), json!(name));
        for (index, b) in self.blend.iter().enumerate() {
            state.insert(format!("rtBlend{index}"), blend(b));
        }
        let fields = [
            ("rtSeparateBlend", json!(self.separate_blend)),
            ("zClip", self.zclip.json()),
            ("zTest", self.ztest.json()),
            ("zWrite", self.zwrite.json()),
            ("culling", self.culling.json()),
            ("conservative", self.conservative.json()),
            ("offsetFactor", self.offset_factor.json()),
            ("offsetUnits", self.offset_units.json()),
            ("alphaToMask", self.alpha_to_mask.json()),
            ("stencilOp", stencil(&self.stencil)),
            ("stencilOpFront", stencil(&self.stencil_front)),
            ("stencilOpBack", stencil(&self.stencil_back)),
            ("stencilReadMask", self.stencil_read.json()),
            ("stencilWriteMask", self.stencil_write.json()),
            ("stencilRef", self.stencil_ref.json()),
            ("fogStart", self.fog_start.json()),
            ("fogEnd", self.fog_end.json()),
            ("fogDensity", self.fog_density.json()),
            (
                "fogColor",
                json!({"name": self.fog_color_name, "x": self.fog_color[0].json(), "y": self.fog_color[1].json(), "z": self.fog_color[2].json(), "w": self.fog_color[3].json()}),
            ),
            ("fogMode", json!(self.fog_mode)),
            ("gpuProgramID", json!(gpu_program)),
            ("m_Tags", tags(&self.tags)),
            ("m_LOD", json!(lod)),
            ("lighting", json!(self.lighting)),
        ];
        for (key, value) in fields {
            state.insert(key.into(), value);
        }
        Value::Object(state)
    }
}

fn color_mask(token: &Token) -> Result<Float> {
    match token {
        Token::Bracket(_) | Token::Num(_) => float(token, &[]),
        Token::Ident(word) => {
            let mut mask = 0.0;
            for c in word.to_ascii_uppercase().chars() {
                mask += match c {
                    'R' => 8.0,
                    'G' => 4.0,
                    'B' => 2.0,
                    'A' => 1.0,
                    _ => bail!("ShaderLab: ColorMask {word}"),
                };
            }
            Ok(Float::of(mask))
        }
        other => bail!("ShaderLab: ColorMask {other:?}"),
    }
}

/// The names the engine registers for tags before it reads a shader: a
/// tag's name or value that matches one but for case is written as it.
const REGISTERED_TAGS: &[&str] = &[
    "QUEUE",
    "IGNOREPROJECTOR",
    "LIGHTMODE",
    "SHADOWSUPPORT",
    "FORCENOSHADOWCASTING",
    "PASSFLAGS",
    "REQUIREOPTIONS",
    "SHADOWCASTER",
    "SHADOWCOLLECTOR",
    "FORWARDBASE",
    "FORWARDADD",
    "DEFERRED",
    "PREPASSBASE",
    "PREPASSFINAL",
    "META",
    "DisableBatching",
    "LodFading",
    "Picking",
    "RenderPipeline",
    "RenderType",
    "SceneSelectionPass",
    "Vertex",
    "VertexLM",
    "VertexLMRGBM",
    "true",
];

fn registered(text: &str) -> String {
    REGISTERED_TAGS
        .iter()
        .find(|name| name.eq_ignore_ascii_case(text))
        .map(|name| name.to_string())
        .unwrap_or_else(|| text.to_string())
}

/// Tags as the Editor keeps them: by registered name, a later one of a
/// name replacing an earlier, in order of name.
fn tags(tags: &[(String, String)]) -> Value {
    let mut map: std::collections::BTreeMap<String, String> = Default::default();
    for (key, value) in tags {
        map.insert(registered(key), registered(value));
    }
    let pairs: Vec<Value> = map
        .iter()
        .map(|(k, v)| json!({"first": k, "second": v}))
        .collect();
    json!({ "tags": pairs })
}

fn property(property: &Property) -> Value {
    let kind = property.kind.to_ascii_lowercase();
    let (type_id, dimension) = match kind.as_str() {
        "color" => (0, 1),
        "vector" => (1, 1),
        "float" => (2, 1),
        "range" => (3, 1),
        // `Int` is a float, as before 2021.1; `Integer` an integer.
        "int" => (2, 1),
        "integer" => (5, 1),
        "2d" => (4, 2),
        "3d" => (4, 3),
        "cube" => (4, 4),
        "2darray" => (4, 5),
        "cubearray" => (4, 6),
        "any" => (4, -1),
        _ => (2, 1),
    };
    let number =
        |text: Option<&String>| -> f64 { text.and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0) };
    let mut defaults = [0.0f64; 4];
    match type_id {
        0 | 1 => {
            for (index, slot) in defaults.iter_mut().enumerate() {
                *slot = number(property.default.get(index));
            }
        }
        3 => {
            defaults[0] = number(property.default.first());
            defaults[1] = number(property.kind_args.first());
            defaults[2] = number(property.kind_args.get(1));
        }
        4 => {}
        _ => defaults[0] = number(property.default.first()),
    }
    let mut flags = 0;
    let mut attributes = Vec::new();
    for attribute in &property.attributes {
        let bit = match attribute.as_str() {
            "HideInInspector" => 1,
            "PerRendererData" => 2,
            "NoScaleOffset" => 4,
            "Normal" => 8,
            "HDR" => 16,
            "Gamma" => 32,
            "NonModifiableTextureData" => 64,
            "MainTexture" => 128,
            "MainColor" => 256,
            _ => 0,
        };
        match bit {
            0 => attributes.push(json!(attribute)),
            bit => flags |= bit,
        }
    }
    json!({
        "m_Name": property.name,
        "m_Description": property.description,
        "m_Attributes": attributes,
        "m_Type": type_id,
        "m_Flags": flags,
        "m_DefValue[0]": defaults[0],
        "m_DefValue[1]": defaults[1],
        "m_DefValue[2]": defaults[2],
        "m_DefValue[3]": defaults[3],
        "m_DefTexture": {
            "m_DefaultName": property.texture.clone().unwrap_or_default(),
            "m_TexDim": dimension,
        },
    })
}

/// The commands of a block that set state, of its passes' inheritance.
fn is_state(command: &Command) -> bool {
    !(command.is("Pass")
        || command.is("UsePass")
        || command.is("GrabPass")
        || command.is("SubShader")
        || command.is("Category")
        || command.is("LOD")
        || command.is("Name"))
}

/// The parsed form of a shader.
pub fn parsed_form(name: &str, commands: &[Command]) -> Result<Value> {
    let mut properties = Vec::new();
    let mut subshaders = Vec::new();
    let mut fallback = String::new();
    let mut custom_editor = String::new();
    let mut dependencies = Vec::new();
    let fallback_off = commands.iter().any(|c| {
        c.is("Fallback")
            && c.args
                .first()
                .is_some_and(|t| t.text().eq_ignore_ascii_case("Off"))
    });
    collect(
        commands,
        &State::default(),
        &mut subshaders,
        &mut properties,
        &mut fallback,
        &mut custom_editor,
        &mut dependencies,
    )?;
    Ok(json!({
        "m_Name": name,
        "m_PropInfo": {"m_Props": properties},
        "m_SubShaders": subshaders,
        "m_FallbackName": fallback,
        "m_CustomEditorName": custom_editor,
        "m_CustomEditorForRenderPipelines": [],
        "m_Dependencies": dependencies,
        "m_DisableNoSubshadersMessage": fallback_off,
    }))
}

#[allow(clippy::too_many_arguments)]
fn collect(
    commands: &[Command],
    inherited: &State,
    subshaders: &mut Vec<Value>,
    properties: &mut Vec<Value>,
    fallback: &mut String,
    custom_editor: &mut String,
    dependencies: &mut Vec<Value>,
) -> Result<()> {
    // A category's state applies to the subshaders in it.
    let mut state = inherited.clone();
    state.apply(
        &commands
            .iter()
            .filter(|c| is_state(c))
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    for command in commands {
        if command.is("Properties") {
            for item in command.block.iter().flatten() {
                if let Some(p) = &item.property {
                    properties.push(property(p));
                }
            }
        } else if command.is("SubShader") {
            subshaders.push(subshader(command.block.as_deref().unwrap_or(&[]), &state)?);
        } else if command.is("Category") {
            collect(
                command.block.as_deref().unwrap_or(&[]),
                &state,
                subshaders,
                properties,
                fallback,
                custom_editor,
                dependencies,
            )?;
        } else if command.is("Fallback") {
            let value = command.args.first().map(|t| t.text()).unwrap_or_default();
            if !value.eq_ignore_ascii_case("Off") {
                *fallback = value;
            }
        } else if command.is("CustomEditor") {
            *custom_editor = command.args.first().map(|t| t.text()).unwrap_or_default();
        } else if command.is("Dependency") {
            let args = values(command);
            dependencies.push(json!({
                "first": args.first().map(|t| t.text()).unwrap_or_default(),
                "second": args.get(2).or(args.get(1)).map(|t| t.text()).unwrap_or_default(),
            }));
        }
    }
    Ok(())
}

/// A subshader's commands apply in order: a pass takes the state and name
/// set before it.
fn subshader(commands: &[Command], inherited: &State) -> Result<Value> {
    let mut state = inherited.clone();
    let mut current_name = String::new();
    let lod = commands
        .iter()
        .find(|c| c.is("LOD"))
        .and_then(|c| c.args.first())
        .and_then(|t| t.text().parse().ok())
        .unwrap_or(0i64);
    let mut passes = Vec::new();
    for command in commands {
        if is_state(command) {
            state.apply(std::slice::from_ref(command))?;
        } else if command.is("Name") {
            current_name = command.args.first().map(|t| t.text()).unwrap_or_default();
        }
        if command.is("Pass") {
            let block = command.block.as_deref().unwrap_or(&[]);
            let mut pass_state = state.clone();
            pass_state.apply(block)?;
            let name = block
                .iter()
                .find(|c| c.is("Name"))
                .and_then(|c| c.args.first())
                .map(|t| t.text())
                .unwrap_or_else(|| current_name.clone());
            let gpu = block
                .iter()
                .find(|c| c.is("GpuProgramID"))
                .and_then(|c| c.args.first())
                .and_then(|t| t.text().parse().ok())
                .unwrap_or(0i64);
            passes.push(json!({
                "m_Type": 0,
                "m_State": pass_state.json(&name, lod, gpu),
                "m_Tags": tags(&[]),
                "m_Name": "",
                "m_UseName": "",
                "m_TextureName": "",
                "m_ProgramMask": if gpu != 0 { 6 } else { 0 },
            }));
        } else if command.is("UsePass") {
            passes.push(json!({
                "m_Type": 1,
                "m_UseName": command.args.first().map(|t| t.text()).unwrap_or_default(),
            }));
        }
    }
    Ok(json!({
        "m_Passes": passes,
        "m_Tags": tags(&state.tags),
        "m_LOD": lod,
    }))
}

/// The keywords every shader names first, for single-pass stereo rendering.
const STEREO_KEYWORDS: [&str; 4] = [
    "STEREO_INSTANCING_ON",
    "UNITY_SINGLE_PASS_STEREO",
    "STEREO_MULTIVIEW_ON",
    "STEREO_CUBEMAP_RENDER_ON",
];

/// The program fields of a pass, by program type.
const PROGRAMS: [&str; 6] = [
    "progVertex",
    "progFragment",
    "progGeometry",
    "progHull",
    "progDomain",
    "progRayTracing",
];

fn program(keywords: &[usize]) -> Value {
    json!({
        "m_CommonParameters": {
            "m_BufferParams": [], "m_ConstantBufferBindings": [], "m_ConstantBuffers": [],
            "m_MatrixParams": [], "m_Samplers": [], "m_TextureParams": [], "m_UAVParams": [],
            "m_VectorParams": [],
        },
        "m_ParameterBlobIndices": [],
        "m_PlayerSubPrograms": [],
        "m_SerializedKeywordStateMask": keywords,
        "m_SubPrograms": [],
    })
}

fn package_requirements() -> Value {
    json!({"m_CombinedStatus": 1, "m_Requirements": [], "m_StatusMessage": ""})
}

/// The state of a pass another shader's pass stands for (`UsePass`).
fn empty_state() -> Value {
    let zero = Float::of(0.0);
    let blend = Blend {
        src: Float::of(1.0),
        dst: zero.clone(),
        src_alpha: Float::of(1.0),
        dst_alpha: zero.clone(),
        op: zero.clone(),
        op_alpha: zero.clone(),
        mask: Float::of(15.0),
    };
    let stencil = StencilOp {
        pass: zero.clone(),
        fail: zero.clone(),
        zfail: zero.clone(),
        comp: zero.clone(),
    };
    let state = State {
        blend: vec![blend; 8],
        separate_blend: false,
        zclip: zero.clone(),
        ztest: zero.clone(),
        zwrite: zero.clone(),
        culling: zero.clone(),
        conservative: zero.clone(),
        offset_factor: zero.clone(),
        offset_units: zero.clone(),
        alpha_to_mask: zero.clone(),
        stencil: stencil.clone(),
        stencil_front: stencil.clone(),
        stencil_back: stencil,
        stencil_read: zero.clone(),
        stencil_write: zero.clone(),
        stencil_ref: zero.clone(),
        fog_mode: -1,
        fog_color: [zero.clone(), zero.clone(), zero.clone(), zero.clone()],
        fog_color_name: "<noninit>".into(),
        fog_density: zero.clone(),
        fog_start: zero.clone(),
        fog_end: zero,
        lighting: false,
        tags: Vec::new(),
    };
    state.json("", 0, -1)
}

/// Completes a parsed form with what its programs say: the keywords the
/// shader names (the stereo ones, then each program's in order of first
/// use), each pass's programs with the keywords they read, and the fields a
/// pass of another shader (`UsePass`) takes empty.
pub fn complete(form: &mut Value, snippets: &[super::PreprocessedSnippet]) {
    let mut names: Vec<String> = STEREO_KEYWORDS.iter().map(|k| k.to_string()).collect();
    for snippet in snippets {
        for (_, _, sets) in &snippet.keywords {
            for set in sets {
                for keyword in set.split_whitespace() {
                    if !keyword.chars().all(|c| c == '_') && !names.iter().any(|n| n == keyword) {
                        names.push(keyword.to_string());
                    }
                }
            }
        }
    }
    let index = |keyword: &str| names.iter().position(|n| n == keyword);
    let passes = form["m_SubShaders"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .flat_map(|subshader| {
            subshader["m_PackageRequirements"] = package_requirements();
            subshader["m_Passes"].as_array_mut().into_iter().flatten()
        });
    for pass in passes {
        let use_pass = pass["m_Type"] == 1;
        if use_pass {
            pass["m_State"] = empty_state();
            pass["m_Tags"] = json!({"tags": []});
            pass["m_Name"] = json!("");
            pass["m_TextureName"] = json!("");
            pass["m_ProgramMask"] = json!(0);
        }
        let gpu = pass["m_State"]["gpuProgramID"].as_i64().unwrap_or(0);
        let snippet = snippets.iter().find(|s| s.header.first() == Some(&gpu));
        let types = snippet.and_then(|s| s.header.get(3)).copied().unwrap_or(0);
        for (program_type, field) in PROGRAMS.iter().enumerate() {
            let keywords: Vec<usize> = match (snippet, types >> program_type & 1) {
                (Some(snippet), 1) => {
                    let mut used: Vec<usize> = (0..STEREO_KEYWORDS.len()).collect();
                    for (_, kind_type, sets) in &snippet.keywords {
                        if *kind_type as usize != program_type {
                            continue;
                        }
                        for keyword in sets.iter().flat_map(|s| s.split_whitespace()) {
                            if let Some(i) = index(keyword)
                                && !used.contains(&i)
                            {
                                used.push(i);
                            }
                        }
                    }
                    used.sort_unstable();
                    used
                }
                _ => Vec::new(),
            };
            pass[*field] = program(&keywords);
        }
        if snippet.is_some() {
            pass["m_ProgramMask"] = json!(types << 1);
        }
        let instancing = snippet.is_some_and(|s| {
            s.keywords
                .iter()
                .flat_map(|(_, _, sets)| sets.iter())
                .any(|set| set.split_whitespace().any(|k| k == "INSTANCING_ON"))
        });
        pass["m_HasInstancingVariant"] = json!(instancing);
        pass["m_HasProceduralInstancingVariant"] = json!(false);
        pass["m_NameIndices"] = json!([]);
        pass["m_PackageRequirements"] = package_requirements();
    }
    form["m_KeywordNames"] = json!(names);
    form["m_KeywordFlags"] = json!(vec![0; names.len()]);
}
