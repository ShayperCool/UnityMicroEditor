//! The program the Editor generates for a fixed-function pass (one without
//! a program of its own: `SetTexture`, `Lighting`, `Material`, `Color`,
//! `AlphaTest`, ...), in the text its import writes, as read off the
//! 6000.0 Editor's imports of passes of each kind.
use super::parse::{Command, Token};
use anyhow::{Result, bail};
use std::fmt::Write as _;

/// A color or number of a fixed-function command: a property or a literal.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Property(String),
    Literal(Vec<f64>),
}

impl Value {
    fn of(args: &[&Token]) -> Option<Value> {
        match args {
            [Token::Bracket(name), ..] => Some(Value::Property(name.clone())),
            _ => {
                let numbers: Vec<f64> = args
                    .iter()
                    .filter_map(|t| match t {
                        Token::Num(n) => n.parse().ok(),
                        _ => None,
                    })
                    .collect();
                (!numbers.is_empty()).then_some(Value::Literal(numbers))
            }
        }
    }

    /// As the program names it, a literal as `half4(...)` or `fixed4(...)`.
    fn text(&self, kind: &str) -> String {
        match self {
            Value::Property(name) => name.clone(),
            Value::Literal(values) => {
                let parts: Vec<String> = values.iter().map(|v| number(*v)).collect();
                format!("{kind}({})", parts.join(","))
            }
        }
    }
}

/// A number as the generator prints it: the shortest form, as `%g`.
fn number(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value}");
        text
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Previous = 0,
    Texture = 1,
    Constant = 2,
    Primary = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arg {
    source: Source,
    alpha: bool,
    one_minus: bool,
}

impl Arg {
    fn code(&self) -> u8 {
        self.source as u8 | (self.alpha as u8) << 2 | (self.one_minus as u8) << 3
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Replace,
    Multiply,
    Add,
    Subtract,
    Lerp(Arg),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Combiner {
    scale: u8,
    a: Arg,
    op: Op,
    b: Arg,
}

impl Combiner {
    fn code(&self) -> String {
        let op = match self.op {
            Op::Replace => 0,
            Op::Multiply => 1,
            Op::Add => 2,
            Op::Subtract => 3,
            Op::Lerp(arg) => 0x80 | arg.code(),
        };
        format!(
            "{:02x}{:02x}{:02x}{:02x}",
            self.scale,
            self.a.code(),
            op,
            self.b.code()
        )
    }
}

#[derive(Debug, Clone)]
struct Stage {
    texture: String,
    cube: bool,
    color: Combiner,
    /// The alpha combiner, when the stage states one.
    alpha: Option<Combiner>,
    constant: Option<Value>,
}

/// What a fixed-function pass does.
#[derive(Debug, Clone, Default)]
pub struct FixedPass {
    lighting: bool,
    specular: bool,
    diffuse: Option<Value>,
    ambient: Option<Value>,
    specular_color: Option<Value>,
    emission: Option<Value>,
    shininess: Option<Value>,
    color_material: Option<String>,
    color: Option<Value>,
    stages: Vec<Stage>,
    alpha_test: Option<(String, Value)>,
    fog: bool,
    warnings: Vec<(usize, String)>,
}

fn values(command: &Command) -> Vec<&Token> {
    command
        .args
        .iter()
        .filter(|t| !matches!(t, Token::Punct(',')))
        .collect()
}

fn word(token: Option<&&Token>) -> String {
    token.map(|t| t.text()).unwrap_or_default()
}

/// One side of a `combine`: `[one -] source [alpha]`.
fn arg(tokens: &[Token], at: &mut usize, alpha: bool) -> Result<Arg> {
    let mut one_minus = false;
    if tokens
        .get(*at)
        .is_some_and(|t| matches!(t, Token::Ident(w) if w.eq_ignore_ascii_case("one")))
    {
        one_minus = true;
        *at += 2;
    }
    let source = match tokens.get(*at) {
        Some(Token::Ident(w)) => match w.to_ascii_lowercase().as_str() {
            "previous" => Source::Previous,
            "texture" => Source::Texture,
            "constant" => Source::Constant,
            "primary" => Source::Primary,
            other => bail!("ShaderLab: combine source {other}"),
        },
        other => bail!("ShaderLab: combine source {other:?}"),
    };
    *at += 1;
    let mut alpha = alpha;
    if tokens
        .get(*at)
        .is_some_and(|t| matches!(t, Token::Ident(w) if w.eq_ignore_ascii_case("alpha")))
    {
        alpha = true;
        *at += 1;
    }
    Ok(Arg {
        source,
        alpha,
        one_minus,
    })
}

/// A combiner: `a [op b] [Double|Quad]`.
fn combiner(tokens: &[Token], alpha: bool) -> Result<Combiner> {
    let mut at = 0;
    let a = arg(tokens, &mut at, alpha)?;
    let previous = Arg {
        source: Source::Previous,
        alpha,
        one_minus: false,
    };
    let (op, b) = match tokens.get(at) {
        Some(Token::Punct('*')) => {
            at += 1;
            (Op::Multiply, arg(tokens, &mut at, alpha)?)
        }
        Some(Token::Punct('+')) => {
            at += 1;
            (Op::Add, arg(tokens, &mut at, alpha)?)
        }
        Some(Token::Punct('-')) => {
            at += 1;
            (Op::Subtract, arg(tokens, &mut at, alpha)?)
        }
        // `dot3` combines as an addition in 6000.0.
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("dot3") => {
            at += 1;
            (Op::Add, arg(tokens, &mut at, alpha)?)
        }
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("lerp") => {
            at += 2;
            let by = arg(tokens, &mut at, alpha)?;
            at += 1;
            (Op::Lerp(by), arg(tokens, &mut at, alpha)?)
        }
        _ => (Op::Replace, previous),
    };
    let scale = match tokens.get(at) {
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("double") => 2,
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("quad") => 4,
        _ => 1,
    };
    Ok(Combiner { scale, a, op, b })
}

fn stage(command: &Command, cube: bool, warnings: &mut Vec<(usize, String)>) -> Result<Stage> {
    let texture = match command.args.first() {
        Some(Token::Bracket(name)) => name.clone(),
        other => bail!("ShaderLab: SetTexture of {other:?}"),
    };
    let mut constant = None;
    let mut color = None;
    let mut alpha = None;
    for inner in command.block.iter().flatten() {
        if inner.is("ConstantColor") {
            constant = Value::of(&values(inner));
        } else if inner.is("Combine") {
            if inner
                .args
                .iter()
                .any(|t| matches!(t, Token::Ident(w) if w.eq_ignore_ascii_case("dot3") || w.eq_ignore_ascii_case("dot3rgba")))
            {
                warnings.push((
                    inner.line,
                    "Support for SetTexture combiner dot3 has been removed".into(),
                ));
            }
            let split = inner
                .args
                .iter()
                .enumerate()
                .filter(|(_, t)| matches!(t, Token::Punct(',')))
                .map(|(i, _)| i)
                .find(|&i| {
                    // A comma inside `lerp (...)` belongs to it.
                    let open = inner.args[..i]
                        .iter()
                        .filter(|t| **t == Token::Punct('('))
                        .count();
                    let close = inner.args[..i]
                        .iter()
                        .filter(|t| **t == Token::Punct(')'))
                        .count();
                    open == close
                });
            match split {
                Some(i) => {
                    color = Some(combiner(&inner.args[..i], false)?);
                    alpha = Some(combiner(&inner.args[i + 1..], true)?);
                }
                None => color = Some(combiner(&inner.args, false)?),
            }
        } else if inner.is("Matrix") {
            bail!("ShaderLab: a SetTexture matrix is not generated yet");
        }
    }
    let color = color.unwrap_or(Combiner {
        scale: 1,
        a: Arg {
            source: Source::Texture,
            alpha: false,
            one_minus: false,
        },
        op: Op::Replace,
        b: Arg {
            source: Source::Previous,
            alpha: false,
            one_minus: false,
        },
    });
    Ok(Stage {
        texture,
        cube,
        color,
        alpha,
        constant,
    })
}

impl FixedPass {
    /// The fixed-function commands among a pass's, in order; `cube` says
    /// whether a texture property is a cube map.
    pub fn read(commands: &[Command], cube: &dyn Fn(&str) -> bool) -> Result<FixedPass> {
        let mut pass = FixedPass {
            fog: true,
            ..Default::default()
        };
        for command in commands {
            let args = values(command);
            let first = word(args.first());
            if command.is("Lighting") {
                pass.lighting = first.eq_ignore_ascii_case("On");
            } else if command.is("SeparateSpecular") {
                pass.specular = first.eq_ignore_ascii_case("On");
            } else if command.is("ColorMaterial") {
                pass.color_material = Some(first);
            } else if command.is("Color") {
                pass.color = Value::of(&args);
            } else if command.is("AlphaTest") {
                if let Some(value) = Value::of(&args[1.min(args.len())..]) {
                    pass.alpha_test = Some((first, value));
                }
            } else if command.is("Fog") {
                for inner in command.block.iter().flatten() {
                    if inner.is("Mode") {
                        pass.fog = !word(values(inner).first()).eq_ignore_ascii_case("Off");
                    }
                }
            } else if command.is("Material") {
                for inner in command.block.iter().flatten() {
                    let value = Value::of(&values(inner));
                    match inner.name.to_ascii_lowercase().as_str() {
                        "diffuse" => pass.diffuse = value,
                        "ambient" => pass.ambient = value,
                        "specular" => pass.specular_color = value,
                        "emission" => pass.emission = value,
                        "shininess" => pass.shininess = value,
                        _ => {}
                    }
                }
            } else if command.is("SetTexture") {
                let name = match command.args.first() {
                    Some(Token::Bracket(name)) => name.clone(),
                    _ => String::new(),
                };
                pass.stages
                    .push(stage(command, cube(&name), &mut pass.warnings)?);
            }
        }
        Ok(pass)
    }

    /// Whether the pass reads the fog keywords and the light keywords.
    pub fn keywords(&self) -> Vec<Vec<String>> {
        let mut sets = Vec::new();
        if self.fog {
            sets.push(
                ["_", "FOG_LINEAR", "FOG_EXP", "FOG_EXP2"]
                    .map(String::from)
                    .to_vec(),
            );
        }
        if self.lighting {
            sets.push(["_", "POINT", "SPOT"].map(String::from).to_vec());
        }
        sets
    }

    /// Whether the program reads the vertex color: what the combiners call
    /// primary (the first stage's previous) when the pass is unlit, the
    /// material's colors when it takes them from the vertices.
    fn uses_primary(&self) -> bool {
        if self.lighting {
            return self.color_material.is_some();
        }
        if self.stages.is_empty() {
            return self.color.is_none();
        }
        self.stages.iter().enumerate().any(|(index, stage)| {
            let combiners = [Some(stage.color), stage.alpha];
            combiners.iter().flatten().any(|c| {
                let mut args = vec![c.a];
                if c.op != Op::Replace {
                    args.push(c.b);
                }
                if let Op::Lerp(by) = c.op {
                    args.push(by);
                }
                args.iter().any(|a| {
                    a.source == Source::Primary || (index == 0 && a.source == Source::Previous)
                })
            })
        }) || self.color_material.is_some()
    }

    /// The program's source.
    pub fn code(&self) -> Result<String> {
        let mut out = String::new();
        let fog = self.fog;
        let lit = self.lighting;
        let specular = lit && self.specular;
        out.push_str("#pragma vertex vert\n#pragma fragment frag\n#pragma target 2.0\n#include \"UnityCG.cginc\"\n");
        if fog {
            out.push_str("#pragma multi_compile_fog\n#define USING_FOG (defined(FOG_LINEAR) || defined(FOG_EXP) || defined(FOG_EXP2))\n");
        }
        if lit {
            out.push_str(LIGHTING_PRELUDE);
            if specular {
                out.push_str(SPECULAR_TERM);
            }
            out.push_str(LIGHTING_TAIL);
        }
        // Uniforms.
        out.push_str("\n// uniforms\n");
        let material_default = "half4(0,0,0,1)".to_string();
        let mut uniforms: Vec<String> = Vec::new();
        let mut uniform = |line: String| {
            if !uniforms.contains(&line) {
                uniforms.push(line);
            }
        };
        if lit {
            for value in [
                &self.diffuse,
                &self.ambient,
                &self.specular_color,
                &self.emission,
            ]
            .into_iter()
            .flatten()
            {
                if let Value::Property(name) = value {
                    uniform(format!("half4 {name};"));
                }
            }
            if let Some(Value::Property(name)) = &self.shininess {
                uniform(format!("half {name};"));
            }
            uniform("int4 unity_VertexLightParams; // x: light count, y: zero, z: one (y/z needed by d3d9 vs loop instruction)".into());
        } else if let Some(Value::Property(name)) = &self.color {
            uniform(format!("half4 {name};"));
        }
        for stage in &self.stages {
            uniform(format!("float4 {}_ST;", stage.texture));
        }
        for line in &uniforms {
            out.push_str(line);
            out.push('\n');
        }
        // Vertex input.
        let primary = self.uses_primary();
        out.push_str("\n// vertex shader input data\nstruct appdata {\n  float3 pos : POSITION;\n");
        if lit {
            out.push_str("  float3 normal : NORMAL;\n");
        }
        if primary {
            out.push_str("  half4 color : COLOR;\n");
        }
        if !self.stages.is_empty() {
            out.push_str("  float3 uv0 : TEXCOORD0;\n");
        }
        out.push_str("  UNITY_VERTEX_INPUT_INSTANCE_ID\n};\n");
        // Interpolators.
        out.push_str(
            "\n// vertex-to-fragment interpolators\nstruct v2f {\n  fixed4 color : COLOR0;\n",
        );
        if specular {
            out.push_str("  #if ENABLE_SPECULAR\n  fixed3 specColor : COLOR1;\n  #endif\n");
        }
        for (index, stage) in self.stages.iter().enumerate() {
            let size = if stage.cube { 3 } else { 2 };
            let _ = writeln!(out, "  float{size} uv{index} : TEXCOORD{index};");
        }
        if fog {
            let _ = write!(
                out,
                "  #if USING_FOG\n    fixed fog : TEXCOORD{};\n  #endif\n",
                self.stages.len()
            );
        }
        out.push_str("  float4 pos : SV_POSITION;\n  UNITY_VERTEX_OUTPUT_STEREO\n};\n");
        // Vertex shader.
        out.push_str("\n// vertex shader\nv2f vert (appdata IN) {\n  v2f o;\n  UNITY_SETUP_INSTANCE_ID(IN);\n  UNITY_INITIALIZE_VERTEX_OUTPUT_STEREO(o);\n");
        let vertex_color = if lit {
            match &self.color_material {
                Some(_) => "IN.color".to_string(),
                None => "half4(0,0,0,1.1)".to_string(),
            }
        } else if let Some(color) = &self.color {
            color.text("half4")
        } else if primary {
            "IN.color".to_string()
        } else {
            "half4(0,0,0,1.1)".to_string()
        };
        let _ = writeln!(out, "  half4 color = {vertex_color};");
        if fog || lit {
            out.push_str("  float3 eyePos = mul (UNITY_MATRIX_MV, float4(IN.pos,1)).xyz;\n");
        }
        if lit {
            out.push_str("  half3 eyeNormal = normalize (mul ((float3x3)UNITY_MATRIX_IT_MV, IN.normal).xyz);\n");
        }
        out.push_str("  half3 viewDir = 0.0;\n");
        if lit {
            if specular {
                out.push_str("  viewDir = -normalize (eyePos);\n");
            }
            let material = |value: &Option<Value>| {
                value
                    .as_ref()
                    .map(|v| v.text("half4"))
                    .unwrap_or_else(|| material_default.clone())
            };
            let from_vertex = |channel: &str| {
                self.color_material
                    .as_deref()
                    .is_some_and(|m| m.eq_ignore_ascii_case(channel))
            };
            let diffuse = if from_vertex("AmbientAndDiffuse") || from_vertex("Diffuse") {
                "color".to_string()
            } else {
                material(&self.diffuse)
            };
            let ambient = if from_vertex("AmbientAndDiffuse") || from_vertex("Ambient") {
                "color".to_string()
            } else {
                material(&self.ambient)
            };
            let emission = if from_vertex("Emission") {
                "color".to_string()
            } else {
                material(&self.emission)
            };
            let shininess = self
                .shininess
                .as_ref()
                .map(|v| v.text("half"))
                .unwrap_or_else(|| "0".into());
            let _ = write!(
                out,
                "  // lighting\n  half3 lcolor = {emission}.rgb + {ambient}.rgb * glstate_lightmodel_ambient.rgb;\n  half3 specColor = 0.0;\n  half shininess = {shininess} * 128.0;\n  LIGHT_LOOP_ATTRIBUTE for (int il = 0; il < LIGHT_LOOP_LIMIT; ++il) {{\n    lcolor += computeOneLight(il, eyePos, eyeNormal, viewDir, {diffuse}, shininess, specColor);\n  }}\n  color.rgb = lcolor.rgb;\n  color.a = {diffuse}.a;\n"
            );
            if specular {
                let _ = writeln!(
                    out,
                    "  specColor *= {}.rgb;",
                    material(&self.specular_color)
                );
            }
        }
        out.push_str("  o.color = saturate(color);\n");
        if specular {
            out.push_str("  #if ENABLE_SPECULAR\n  o.specColor = saturate(specColor);\n  #endif\n");
        }
        out.push_str("  // compute texture coordinates\n");
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.cube {
                let _ = writeln!(out, "  o.uv{index} = IN.uv0;");
            } else {
                let t = &stage.texture;
                let _ = writeln!(out, "  o.uv{index} = IN.uv0.xy * {t}_ST.xy + {t}_ST.zw;");
            }
        }
        if fog {
            out.push_str("  // fog\n  #if USING_FOG\n    float fogCoord = length(eyePos.xyz); // radial fog distance\n    UNITY_CALC_FOG_FACTOR_RAW(fogCoord);\n    o.fog = saturate(unityFogFactor);\n  #endif\n");
        }
        out.push_str(
            "  // transform position\n  o.pos = UnityObjectToClipPos(IN.pos);\n  return o;\n}\n",
        );
        // Textures and what the fragment shader reads.
        if !self.stages.is_empty() {
            out.push_str("\n// textures\n");
            let mut declared: Vec<String> = Vec::new();
            let mut declare = |line: String| {
                if !declared.contains(&line) {
                    declared.push(line);
                }
            };
            for stage in &self.stages {
                let sampler = if stage.cube {
                    "samplerCUBE"
                } else {
                    "sampler2D"
                };
                declare(format!("{sampler} {};", stage.texture));
            }
            for stage in &self.stages {
                if let Some(Value::Property(name)) = &stage.constant {
                    declare(format!("fixed4 {name};"));
                }
            }
            if let Some((_, Value::Property(name))) = &self.alpha_test {
                declare(format!("fixed {name};"));
            }
            for line in &declared {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str("\n// fragment shader\nfixed4 frag (v2f IN) : SV_Target {\n  fixed4 col;\n");
        if self.stages.is_empty() {
            out.push_str("  col = IN.color;\n");
        } else {
            out.push_str("  fixed4 tex, tmp0, tmp1, tmp2;\n");
            for (index, stage) in self.stages.iter().enumerate() {
                let _ = writeln!(out, "  // SetTexture #{index}");
                let uv = if stage.cube { "xyz" } else { "xy" };
                let fetch = if stage.cube { "texCUBE" } else { "tex2D" };
                let _ = writeln!(
                    out,
                    "  tex = {fetch} ({}, IN.uv{index}.{uv});",
                    stage.texture
                );
                let mut temp = 0;
                let mut name = |arg: Arg, out: &mut String| -> String {
                    let base = match arg.source {
                        Source::Previous if index == 0 => "IN.color".to_string(),
                        Source::Previous => "col".to_string(),
                        Source::Texture => "tex".to_string(),
                        Source::Primary => "IN.color".to_string(),
                        Source::Constant => stage
                            .constant
                            .as_ref()
                            .map(|v| v.text("fixed4"))
                            .unwrap_or_else(|| "fixed4(0,0,0,0)".into()),
                    };
                    let base = if arg.alpha { format!("{base}.a") } else { base };
                    if arg.one_minus {
                        let tmp = format!("tmp{temp}");
                        temp += 1;
                        let _ = writeln!(out, "  {tmp} = 1.0 - {base};");
                        tmp
                    } else {
                        base
                    }
                };
                let mut expression = |c: &Combiner, out: &mut String| -> String {
                    let a = name(c.a, out);
                    match c.op {
                        Op::Replace => a,
                        Op::Multiply => format!("{a} * {}", name(c.b, out)),
                        Op::Add => format!("{a} + {}", name(c.b, out)),
                        Op::Subtract => format!("{a} - {}", name(c.b, out)),
                        Op::Lerp(by) => {
                            let b = name(c.b, out);
                            let by = name(Arg { alpha: false, ..by }, out);
                            format!("lerp ({b}, {a}, {by}.a)")
                        }
                    }
                };
                match stage.alpha {
                    None => {
                        let mut pre = String::new();
                        let e = expression(&stage.color, &mut pre);
                        out.push_str(&pre);
                        let _ = writeln!(out, "  col = {e};");
                        if stage.color.scale != 1 {
                            let _ = writeln!(out, "  col *= {};", stage.color.scale);
                        }
                    }
                    Some(alpha) => {
                        let mut pre = String::new();
                        let e = expression(&stage.color, &mut pre);
                        out.push_str(&pre);
                        let _ = writeln!(out, "  col.rgb = {e};");
                        if stage.color.scale != 1 {
                            let _ = writeln!(out, "  col *= {};", stage.color.scale);
                        }
                        let mut pre = String::new();
                        let e = expression(&alpha, &mut pre);
                        out.push_str(&pre);
                        let _ = writeln!(out, "  col.a = {e};");
                        if alpha.scale != 1 {
                            let _ = writeln!(out, "  col.a *= {};", alpha.scale);
                        }
                    }
                }
            }
        }
        if specular {
            out.push_str("  #if ENABLE_SPECULAR\n  // add specular color\n  col.rgb += IN.specColor;\n  #endif\n");
        }
        if let Some((compare, value)) = &self.alpha_test {
            let test = match compare.to_ascii_lowercase().as_str() {
                "greater" => Some("<="),
                "gequal" => Some("<"),
                "less" => Some(">="),
                "lequal" => Some(">"),
                "equal" => Some("!="),
                "notequal" => Some("=="),
                _ => None,
            };
            if let Some(test) = test {
                let value = match value {
                    Value::Property(name) => name.clone(),
                    Value::Literal(v) => number(v.first().copied().unwrap_or(0.0)),
                };
                let _ = writeln!(
                    out,
                    "  // alpha test\n  if (col.a {test} {value}) clip(-1);"
                );
            }
        }
        if fog {
            out.push_str("  // fog\n  #if USING_FOG\n    col.rgb = lerp (unity_FogColor.rgb, col.rgb, IN.fog);\n  #endif\n");
        }
        out.push_str("  return col;\n}\n");
        if !self.stages.is_empty() {
            out.push_str("\n// texenvs\n");
            for (index, stage) in self.stages.iter().enumerate() {
                let alpha = stage.alpha.unwrap_or(stage.color);
                let _ = write!(
                    out,
                    "//! TexEnv{index}: {} {} [{}]",
                    stage.color.code(),
                    alpha.code(),
                    stage.texture
                );
                match &stage.constant {
                    Some(Value::Property(name)) => {
                        let _ = write!(out, " [{name}]");
                    }
                    Some(Value::Literal(values)) => {
                        let bytes: String = values
                            .iter()
                            .map(|v| format!("{:02x}", (v.clamp(0.0, 1.0) * 255.0).round() as u8))
                            .collect();
                        let _ = write!(out, " [{bytes}]");
                    }
                    None => {}
                }
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Whether the pass is lit, which makes it a vertex-lit pass.
    pub fn lit(&self) -> bool {
        self.lighting
    }

    /// What the generator says of the pass: the lines of combiners it no
    /// longer supports.
    pub fn warnings(&self) -> &[(usize, String)] {
        &self.warnings
    }
}

const LIGHTING_PRELUDE: &str = "
// ES2.0/WebGL can not do loops with non-constant-expression iteration counts :(
#if defined(SHADER_API_GLES)
  #define LIGHT_LOOP_LIMIT 8
#else
  #define LIGHT_LOOP_LIMIT unity_VertexLightParams.x
#endif

// Some ES3 drivers (e.g. older Adreno) have problems with the light loop
#if defined(SHADER_API_GLES3) && !defined(SHADER_API_DESKTOP) && (defined(SPOT) || defined(POINT))
  #define LIGHT_LOOP_ATTRIBUTE UNITY_UNROLL
#else
  #define LIGHT_LOOP_ATTRIBUTE
#endif
#define ENABLE_SPECULAR 1

// Compile specialized variants for when positional (point/spot) and spot lights are present
#pragma multi_compile __ POINT SPOT

// Compute illumination from one light, given attenuation
half3 computeLighting (int idx, half3 dirToLight, half3 eyeNormal, half3 viewDir, half4 diffuseColor, half shininess, half atten, inout half3 specColor) {
  half NdotL = max(dot(eyeNormal, dirToLight), 0.0);
  // diffuse
  half3 color = NdotL * diffuseColor.rgb * unity_LightColor[idx].rgb;
";

const SPECULAR_TERM: &str = "  // specular
  if (NdotL > 0.0) {
    half3 h = normalize(dirToLight + viewDir);
    half HdotN = max(dot(eyeNormal, h), 0.0);
    half sp = saturate(pow(HdotN, shininess));
    specColor += (atten * sp) * unity_LightColor[idx].rgb;
  }
";

const LIGHTING_TAIL: &str = "  return color * atten;
}

// Compute attenuation & illumination from one light
half3 computeOneLight(int idx, float3 eyePosition, half3 eyeNormal, half3 viewDir, half4 diffuseColor, half shininess, inout half3 specColor) {
  float3 dirToLight = unity_LightPosition[idx].xyz;
  half att = 1.0;
  #if defined(POINT) || defined(SPOT)
    dirToLight -= eyePosition * unity_LightPosition[idx].w;
    // distance attenuation
    float distSqr = dot(dirToLight, dirToLight);
    att /= (1.0 + unity_LightAtten[idx].z * distSqr);
    if (unity_LightPosition[idx].w != 0 && distSqr > unity_LightAtten[idx].w) att = 0.0; // set to 0 if outside of range
    distSqr = max(distSqr, 0.000001); // don't produce NaNs if some vertex position overlaps with the light
    dirToLight *= rsqrt(distSqr);
    #if defined(SPOT)
      // spot angle attenuation
      half rho = max(dot(dirToLight, unity_SpotDirection[idx].xyz), 0.0);
      half spotAtt = (rho - unity_LightAtten[idx].x) * unity_LightAtten[idx].y;
      att *= saturate(spotAtt);
    #endif
  #endif
  att *= 0.5; // passed in light colors are 2x brighter than what used to be in FFP
  return min (computeLighting (idx, dirToLight, eyeNormal, viewDir, diffuseColor, shininess, att, specColor), 1.0);
}
";

#[cfg(test)]
mod tests {
    use super::super::{PreprocessReply, import};

    fn imported(shader: &str) -> super::super::Imported {
        let reply = PreprocessReply {
            ok: true,
            shader: shader.into(),
            ..Default::default()
        };
        import(&reply).unwrap()
    }

    #[test]
    fn a_pass_without_a_program_gets_the_editor_s() {
        // The pass of Spine's HiddenPass and Bouncemasters' DepthMask, whose
        // generated snippet the 6000.0 Editor numbers 11839.
        let shader = imported(
            "Shader \"Probe\" { SubShader { Tags {\"Queue\"=\"Geometry-1\"} Lighting Off Pass { ZWrite Off ColorMask 0 } } }",
        );
        assert!(shader.fixed);
        assert_eq!(shader.snippets[0].header[0], 11839);
        let pass = &shader.form["m_SubShaders"][0]["m_Passes"][0];
        assert_eq!(pass["m_State"]["gpuProgramID"], 11839);
        assert_eq!(pass["m_State"]["fogMode"], 0);
    }

    #[test]
    fn combiners_are_written_and_noted() {
        let shader = imported(
            "Shader \"Probe\" { Properties { _MainTex (\"T\", 2D) = \"white\" {} } SubShader { Pass { SetTexture [_MainTex] { combine texture * primary DOUBLE, texture } } } }",
        );
        let code = &shader.snippets[0].code;
        assert!(code.contains("  col.rgb = tex * IN.color;\n  col *= 2;\n  col.a = tex.a;\n"));
        assert!(code.ends_with("//! TexEnv0: 02010103 01050004 [_MainTex]\n"));
    }
}
