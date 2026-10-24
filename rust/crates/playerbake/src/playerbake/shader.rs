//! Bakes a `.shader` into the `Shader` object a Player loads.
//!
//! The import worker parses a shader and leaves an object that holds the
//! parsed passes and the source of every program, but no compiled program. A
//! build compiles each program for every graphics API and keyword variant
//! with `UnityShaderCompiler` and stores the results in compressed tables.
//! This module does the same from the worker's object: it decides which
//! variants a build compiles (`variant`), asks a compiler for each, lays the
//! replies out as a build does (`blob`) and writes the object through the
//! Player's type tree (`object`).
//!
//! The build it reproduces is an AssetBundle build for Linux standalone,
//! started from the command line, of the shader and the materials that use
//! it. What such a build takes from the scene open in the Editor (fog and
//! lightmap use under automatic stripping) is therefore empty.
use super::{
    Baked, serialized,
    typetree::{TypeTree, Val},
    unsupported,
};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::{borrow::Cow, sync::LazyLock};

mod blob;
mod check;
mod link;
mod object;
mod variant;

pub use check::verify_shader_programs;
/// The class of a `ComputeShader` and the local file ID of the one object
/// of a `.compute` asset.
pub const COMPUTE_SHADER_CLASS: i32 = 72;
pub const COMPUTE_SHADER_LOCAL_ID: i64 = 7_200_000;
pub use link::{
    NoShaders, ShaderDependency, ShaderLinks, ShaderPass, ShaderPointer, ShaderSources,
    read_shader_links, read_shader_links_of, read_shader_links_with,
};

/// The local file ID of the `Shader` object of a `.shader` asset.
pub const SHADER_LOCAL_ID: i64 = 4_800_000;
const SHADER_CLASS: i32 = 48;

static SHADER_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-48.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `Shader` in the pinned engine.
pub fn shader_tree() -> Result<&'static TypeTree> {
    SHADER_TREE
        .as_ref()
        .map_err(|error| anyhow!("pinned Shader type tree: {error}"))
}

/// One program to compile: a pass's source for one graphics API with one
/// set of keywords. The fields are those of the compiler's `compileSnippet`
/// request, in its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetRequest {
    /// The release whose compiler is asked.
    pub release: ShaderRelease,
    /// The program source as the importer prepared it. It need not be text.
    pub source: Vec<u8>,
    pub asset_directory: String,
    pub asset_name: String,
    pub shader_name: String,
    pub pass_name: String,
    /// Two switches of a build that are off in every build seen.
    pub options: [i32; 2],
    pub build_target: i32,
    /// A length only other graphics APIs use.
    pub reserved: i32,
    /// The platform's defines that are on.
    pub enabled: Vec<String>,
    /// The variant's keywords, in the order of the shader's keyword list.
    pub keywords: Vec<String>,
    /// The keywords of the pass the variant leaves off, then the platform's
    /// defines that are off.
    pub disabled: Vec<String>,
    /// How many of `disabled` are keywords.
    pub keywords_off: u32,
    /// Declarations of everything that is off, for the compiler to prepend.
    pub constants: String,
    pub flags: u64,
    pub language: i32,
    /// The program stage; all stages travel with the first.
    pub program: i32,
    /// The graphics API by its compiler platform number.
    pub platform: u32,
    pub requirements: u64,
    /// The program stages of the pass, one bit each.
    pub program_mask: u32,
    pub start_line: i32,
    pub toolchain: i32,
}

/// The bytes that precede every item of the compiler's wire format.
pub const WIRE_MARKER: [u8; 4] = [0xe4, 0xd1, 0x0b, 0x0c];

fn wire_int(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&WIRE_MARKER);
    out.extend_from_slice(&value.to_le_bytes());
}
fn wire_long(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&WIRE_MARKER);
    out.extend_from_slice(&value.to_le_bytes());
}
fn wire_text(out: &mut Vec<u8>, text: &[u8]) {
    wire_long(out, text.len() as u64);
    out.extend_from_slice(text);
}
fn wire_list(out: &mut Vec<u8>, items: &[String]) {
    wire_int(out, items.len() as i32);
    for item in items {
        wire_text(out, item.as_bytes());
    }
}

impl SnippetRequest {
    /// The request as the compiler reads it after its command name: an
    /// integer is 32 bits, a length 64, a string its length and bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.source.len() + self.constants.len() + 2048);
        wire_text(&mut out, &self.source);
        wire_text(&mut out, self.asset_directory.as_bytes());
        wire_text(&mut out, self.asset_name.as_bytes());
        wire_text(&mut out, self.shader_name.as_bytes());
        wire_text(&mut out, self.pass_name.as_bytes());
        wire_int(&mut out, self.options[0]);
        wire_int(&mut out, self.options[1]);
        wire_int(&mut out, self.build_target);
        wire_int(&mut out, self.reserved);
        wire_list(&mut out, &self.enabled);
        wire_list(&mut out, &self.keywords);
        wire_list(&mut out, &self.disabled);
        wire_int(&mut out, self.keywords_off as i32);
        wire_text(&mut out, self.constants.as_bytes());
        // 6000.0's request ends in the flags (32 bits), the language, the
        // graphics API, the requirements, the stages and the start line.
        if self.release == ShaderRelease::Unity6000_0 {
            wire_int(&mut out, self.flags as i32);
            wire_int(&mut out, self.language);
            wire_int(&mut out, self.platform as i32);
            wire_long(&mut out, self.requirements);
            wire_int(&mut out, self.program_mask as i32);
            wire_int(&mut out, self.start_line);
            return out;
        }
        wire_long(&mut out, self.flags);
        wire_int(&mut out, self.language);
        wire_int(&mut out, self.program);
        wire_int(&mut out, self.platform as i32);
        wire_long(&mut out, self.requirements);
        wire_int(&mut out, self.program_mask as i32);
        wire_int(&mut out, self.start_line);
        wire_int(&mut out, self.toolchain);
        out
    }
}

/// A compiler's answer: what the program takes as input, and its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetReply {
    /// The reflection lines before the verdict.
    pub lines: Vec<Vec<u8>>,
    pub compiled: bool,
    pub program_type: i32,
    pub program: Vec<u8>,
}

pub trait SnippetCompiler {
    fn compile(&mut self, request: &SnippetRequest) -> Result<SnippetReply>;
}

/// What a hardware tier of the standalone target turns on in shaders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderTier {
    /// 0 low, 1 medium, 2 high.
    pub quality: u8,
    pub box_projection: bool,
    pub probe_blending: bool,
    pub detail_normal_map: bool,
    pub cascaded_shadows: bool,
    pub proxy_volumes: bool,
    pub dither_mask: bool,
}

impl Default for ShaderTier {
    /// The engine's settings of every standalone tier.
    fn default() -> Self {
        Self {
            quality: 2,
            box_projection: true,
            probe_blending: true,
            detail_normal_map: true,
            cascaded_shadows: true,
            proxy_volumes: true,
            dither_mask: true,
        }
    }
}

/// The lightmap modes a project with custom stripping keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightmapKeep {
    pub plain: bool,
    pub directional: bool,
    pub dynamic_plain: bool,
    pub dynamic_directional: bool,
    pub shadowmask: bool,
    pub subtractive: bool,
}

/// The fog modes a project with custom stripping keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FogKeep {
    pub linear: bool,
    pub exponential: bool,
    pub exponential_squared: bool,
}

/// The project settings a build of a shader depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderSettings {
    /// `m_ActiveColorSpace` of the Player settings.
    pub linear: bool,
    /// `shaderPrecisionModel` of the Player settings.
    pub unified_precision: bool,
    pub virtual_texturing: bool,
    /// The lightmap encoding of the standalone target: 0 low, 1 normal,
    /// 2 high.
    pub lightmap_encoding: u8,
    /// Whether deferred reflections have a shader.
    pub reflection_buffers: bool,
    pub tiers: [ShaderTier; 3],
    /// `None` is automatic stripping.
    pub lightmap_keep: Option<LightmapKeep>,
    pub fog_keep: Option<FogKeep>,
    /// 0 strips unused instancing variants, 1 all, 2 none.
    pub instancing_stripping: u8,
    pub batch_renderer_stripping: u8,
    /// Whether a quality level cross-fades levels of detail.
    pub lod_cross_fade: bool,
    /// Whether the project has the entities graphics package.
    pub hybrid_package: bool,
    /// The size a table of programs is cut at.
    pub chunk_megabytes: u32,
}

impl Default for ShaderSettings {
    /// A new project of the built-in pipeline in gamma colour space.
    fn default() -> Self {
        Self {
            linear: false,
            unified_precision: false,
            virtual_texturing: false,
            lightmap_encoding: 2,
            reflection_buffers: true,
            tiers: [ShaderTier::default(); 3],
            lightmap_keep: None,
            fog_keep: None,
            instancing_stripping: 0,
            batch_renderer_stripping: 0,
            lod_cross_fade: false,
            hybrid_package: false,
            chunk_megabytes: 16,
        }
    }
}

/// The value of the first `key: value` line of a settings text.
fn scalar<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let value = line.trim_start().strip_prefix(key)?.strip_prefix(':')?;
        let value = value.trim();
        (!value.is_empty() && !value.contains(char::is_whitespace)).then_some(value)
    })
}

fn integer(text: &str, key: &str, default: i64) -> Result<i64> {
    match scalar(text, key) {
        None => Ok(default),
        Some(value) => value
            .parse()
            .map_err(|_| unsupported(format!("the project setting {key}: {value}"))),
    }
}

/// The entries of a list of mappings in a settings text, each flattened to
/// its `key: value` pairs.
fn entries(text: &str, key: &str) -> Vec<Vec<(String, String)>> {
    static PAIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w+):\s*([^,{}\s]+)").unwrap());
    let mut lines = text.lines();
    let mut indent = None;
    for line in lines.by_ref() {
        if line.trim() == format!("{key}:") {
            indent = Some(line.len() - line.trim_start().len());
            break;
        }
    }
    let Some(indent) = indent else {
        return Vec::new();
    };
    let mut out: Vec<Vec<(String, String)>> = Vec::new();
    for line in lines {
        let body = line.trim();
        if body.is_empty() {
            continue;
        }
        let depth = line.len() - line.trim_start().len();
        if depth < indent || depth == indent && !body.starts_with('-') {
            break;
        }
        let body = match body.strip_prefix("- ") {
            Some(rest) => {
                out.push(Vec::new());
                rest
            }
            None => body,
        };
        if let Some(entry) = out.last_mut() {
            for pair in PAIR.captures_iter(body) {
                entry.push((pair[1].to_string(), pair[2].to_string()));
            }
        }
    }
    out
}

fn entry<'a>(entry: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entry
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Whether a settings text points a pipeline field at an asset.
fn names_asset(text: &str, key: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .and_then(|rest| rest.trim_start().strip_prefix("{fileID:"))
            .is_some_and(|rest| !rest.trim_start().starts_with('0'))
    })
}

/// Whether the graphics or quality settings of a project name a scriptable
/// render pipeline asset. The pipeline's Editor scripts then decide which
/// variants a build keeps.
pub fn names_render_pipeline(graphics: &str, quality: &str) -> bool {
    names_asset(graphics, "m_CustomRenderPipeline") || names_asset(quality, "customRenderPipeline")
}

impl ShaderSettings {
    /// Reads the settings from the project's files: `ProjectSettings.asset`,
    /// `GraphicsSettings.asset`, `QualitySettings.asset` and the package
    /// manifest. A file the project lacks holds the engine's defaults. A
    /// setting whose effect on a build is not reproduced is refused.
    pub fn read(player: &str, graphics: &str, quality: &str, manifest: &str) -> Result<Self> {
        if names_render_pipeline(graphics, quality) {
            return Err(unsupported(
                "a shader of a project with a scriptable render pipeline, whose keyword filters and pass kinds are not reproduced",
            ));
        }
        let depth = |line: &str| line.len() - line.trim_start().len();
        let mut lines = graphics
            .lines()
            .skip_while(|line| line.trim() != "m_ShaderBuildSettings:");
        if let Some(head) = lines.next() {
            for line in lines {
                if line.trim().is_empty() {
                    continue;
                }
                if depth(line) <= depth(head) {
                    break;
                }
                let Some((key, value)) = line.trim().split_once(':') else {
                    continue;
                };
                let listed = matches!(
                    key.trim_start_matches("- "),
                    "keywordDeclarationOverrides"
                        | "numInternalDefines"
                        | "defines"
                        | "compilerSettings"
                );
                if listed && !matches!(value.trim(), "[]" | "0") {
                    return Err(unsupported("a project with shader build settings"));
                }
            }
        }
        if scalar(player, "enable360StereoCapture") == Some("1") {
            return Err(unsupported("a project with 360 stereo capture"));
        }
        let mut settings = Self {
            linear: match integer(player, "m_ActiveColorSpace", 0)? {
                0 => false,
                1 => true,
                other => return Err(unsupported(format!("the colour space {other}"))),
            },
            unified_precision: integer(player, "shaderPrecisionModel", 0)? == 1,
            virtual_texturing: scalar(player, "virtualTexturingSupportEnabled") == Some("1"),
            reflection_buffers: true,
            ..Default::default()
        };
        for line in player.lines() {
            let key = line.trim().split(':').next().unwrap_or("");
            if key.contains("ShaderChunk")
                && !matches!(
                    key,
                    "m_DefaultShaderChunkSizeInMB" | "m_DefaultShaderChunkCount"
                )
            {
                return Err(unsupported(format!("the project setting {key}")));
            }
        }
        settings.chunk_megabytes =
            match u32::try_from(integer(player, "m_DefaultShaderChunkSizeInMB", 16)?) {
                Ok(size) if (1..=2047).contains(&size) => size,
                _ => return Err(unsupported("the project's shader chunk size")),
            };
        for encoding in entries(player, "m_BuildTargetGroupLightmapEncodingQuality") {
            if entry(&encoding, "m_BuildTarget") == Some("Standalone") {
                settings.lightmap_encoding = match entry(&encoding, "m_EncodingQuality") {
                    Some("0") => 0,
                    Some("1") => 1,
                    Some("2") => 2,
                    _ => return Err(unsupported("the project's lightmap encoding")),
                };
            }
        }
        // The mode of deferred reflections is the first field under its key.
        let mut lines = graphics.lines();
        while let Some(line) = lines.next() {
            if line.trim() == "m_DeferredReflections:" {
                let mode = lines.next().unwrap_or("").trim();
                if let Some(mode) = mode.strip_prefix("m_Mode:") {
                    settings.reflection_buffers = mode.trim() != "0";
                }
                break;
            }
        }
        for tier in entries(graphics, "m_TierSettings") {
            if entry(&tier, "m_BuildTarget") != Some("1")
                || entry(&tier, "m_Automatic").unwrap_or("0") != "0"
            {
                continue;
            }
            let flag = |key: &str| match entry(&tier, key) {
                Some("0") => Ok(false),
                Some("1") => Ok(true),
                _ => Err(unsupported(format!("the tier setting {key}"))),
            };
            let index = match entry(&tier, "m_Tier") {
                Some("0") => 0,
                Some("1") => 1,
                Some("2") => 2,
                _ => return Err(unsupported("a tier setting of an unknown tier")),
            };
            settings.tiers[index] = ShaderTier {
                quality: match entry(&tier, "standardShaderQuality") {
                    Some("0") => 0,
                    Some("1") => 1,
                    Some("2") => 2,
                    _ => return Err(unsupported("the tier setting standardShaderQuality")),
                },
                box_projection: flag("useReflectionProbeBoxProjection")?,
                probe_blending: flag("useReflectionProbeBlending")?,
                detail_normal_map: flag("useDetailNormalMap")?,
                cascaded_shadows: flag("useCascadedShadowMaps")?,
                proxy_volumes: flag("enableLPPV")?,
                dither_mask: flag("useDitherMaskForAlphaBlendedShadows")?,
            };
        }
        let keep = |key: &str| Ok::<bool, anyhow::Error>(integer(graphics, key, 1)? != 0);
        if integer(graphics, "m_LightmapStripping", 0)? != 0 {
            settings.lightmap_keep = Some(LightmapKeep {
                plain: keep("m_LightmapKeepPlain")?,
                directional: keep("m_LightmapKeepDirCombined")?,
                dynamic_plain: keep("m_LightmapKeepDynamicPlain")?,
                dynamic_directional: keep("m_LightmapKeepDynamicDirCombined")?,
                shadowmask: keep("m_LightmapKeepShadowMask")?,
                subtractive: keep("m_LightmapKeepSubtractive")?,
            });
        }
        if integer(graphics, "m_FogStripping", 0)? != 0 {
            settings.fog_keep = Some(FogKeep {
                linear: keep("m_FogKeepLinear")?,
                exponential: keep("m_FogKeepExp")?,
                exponential_squared: keep("m_FogKeepExp2")?,
            });
        }
        let small = |key: &str| match integer(graphics, key, 0)? {
            value @ 0..=2 => Ok(value as u8),
            _ => Err(unsupported(format!("the project setting {key}"))),
        };
        settings.instancing_stripping = small("m_InstancingStripping")?;
        settings.batch_renderer_stripping = small("m_BrgStripping")?;
        settings.lod_cross_fade = quality.lines().any(|line| {
            line.split_once("enableLODCrossFade:")
                .is_some_and(|(_, value)| value.trim_start().starts_with('1'))
        });
        let manifest = manifest.to_ascii_lowercase();
        settings.hybrid_package = manifest.contains("com.unity.rendering.hybrid")
            || manifest.contains("com.unity.entities.graphics");
        Ok(settings)
    }
}

/// The graphics APIs the project's shaders are compiled for, by compiler
/// platform number, from the Player settings: those the project lists for
/// Linux standalone, else OpenGL Core, the one the Player runs on. A build
/// of a project that lists none compiles Vulkan as well, which no session
/// loads.
pub fn shader_platforms(player: &str) -> Result<Vec<u32>> {
    for target in entries(player, "m_BuildTargetGraphicsAPIs") {
        if entry(&target, "m_BuildTarget") != Some("LinuxStandaloneSupport")
            || entry(&target, "m_Automatic") == Some("1")
        {
            continue;
        }
        // The list is the renderers' numbers, four hex bytes each.
        let listed = entry(&target, "m_APIs").unwrap_or("");
        let refused = || unsupported(format!("the Linux graphics API list {listed:?}"));
        if listed.is_empty() || !listed.len().is_multiple_of(8) || !listed.is_ascii() {
            return Err(refused());
        }
        let mut platforms = Vec::new();
        for index in (0..listed.len()).step_by(8) {
            let platform = match &listed[index..index + 8] {
                "11000000" => 15,
                "15000000" => 18,
                _ => return Err(refused()),
            };
            if platforms.contains(&platform) {
                return Err(refused());
            }
            platforms.push(platform);
        }
        return Ok(platforms);
    }
    Ok(vec![15])
}

/// A material that uses the shader, as far as a build of the shader reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShaderMaterialUse {
    /// The keywords the material enables, known to the shader or not.
    pub keywords: Vec<String>,
    /// `m_EnableInstancingVariants`.
    pub instancing: bool,
}

/// The release a shader is baked for: the Player's layout of `Shader`, the
/// compiler's requests and the tables of programs differ between them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShaderRelease {
    /// 6000.0.71f1.
    Unity6000_0,
    /// 6000.6.3f1, the verified release.
    #[default]
    Unity6000_6,
}

impl ShaderRelease {
    pub fn of(unity_version: &str) -> Option<Self> {
        match unity_version {
            "6000.0.71f1" => Some(ShaderRelease::Unity6000_0),
            version if version == crate::ENGINE_VERSION => Some(ShaderRelease::Unity6000_6),
            _ => None,
        }
    }

    /// The Player's layout of `Shader`.
    pub fn tree(self) -> Result<&'static TypeTree> {
        match self {
            ShaderRelease::Unity6000_6 => shader_tree(),
            ShaderRelease::Unity6000_0 => SHADER_TREE_6000_0
                .as_ref()
                .map_err(|error| anyhow!("6000.0 Shader type tree: {error}")),
        }
    }
}

static SHADER_TREE_6000_0: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/6000.0.71f1/type-48.bin")[..])
        .map_err(|error| error.to_string())
});

/// What a bake takes from the project beside the shader itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShaderProject {
    pub release: ShaderRelease,
    /// The graphics APIs of the build by compiler platform number (OpenGL
    /// Core is 15, Vulkan 18). Their order does not matter.
    pub platforms: Vec<u32>,
    pub settings: ShaderSettings,
    /// The materials built with the shader. A `shader_feature` keyword is
    /// compiled only in the combinations they enable.
    pub materials: Vec<ShaderMaterialUse>,
}

/// The `Shader` object of an import worker artifact, as a value.
pub struct ShaderArtifact {
    pub value: Val,
}

/// Reads the `Shader` object out of the artifact the import worker leaves
/// for a `.shader`.
pub fn read_shader_artifact(file: &[u8]) -> Result<ShaderArtifact> {
    let objects = serialized::read_of_class(file, SHADER_CLASS)?;
    let mut shaders = objects.iter();
    let (Some(shader), None) = (shaders.next(), shaders.next()) else {
        bail!("shader artifact does not hold exactly one Shader object");
    };
    if shader.local_id != SHADER_LOCAL_ID {
        bail!("shader artifact holds its Shader under another file ID");
    }
    let value = shader.tree.decode(&shader.data)?;
    // The value is complete only if it writes back to the same bytes.
    if shader.tree.encode(&value)? != shader.data {
        bail!("shader artifact is not laid out as its type tree states");
    }
    Ok(ShaderArtifact { value })
}

/// Lays a value read with one tree out for another: the fields the target
/// lacks are dropped, as the engine drops Editor-only data in a build. A
/// field only the target has cannot be filled and is an error.
fn project(tree: &TypeTree, index: usize, value: &Val) -> Result<Val> {
    let node = &tree.nodes[index];
    let children = tree.children(index);
    if children.is_empty() {
        return Ok(value.clone());
    }
    if let Some((_, element)) = tree.element(index) {
        return match value {
            Val::Seq(items) => Ok(Val::Seq(
                items
                    .iter()
                    .map(|item| project(tree, element, item))
                    .collect::<Result<_>>()?,
            )),
            Val::Str(_) | Val::Bytes(_) | Val::Zero => Ok(value.clone()),
            _ => bail!("shader artifact field {} is not a sequence", node.name),
        };
    }
    let Val::Map(fields) = value else {
        bail!("shader artifact field {} is not a structure", node.name);
    };
    let mut result = Vec::with_capacity(children.len());
    for child in children {
        let name = &tree.nodes[child].name;
        let Some((_, field)) = fields.iter().find(|(key, _)| key == name) else {
            bail!("shader artifact lacks the Player field {name}");
        };
        result.push((Cow::Owned(name.clone()), project(tree, child, field)?));
    }
    Ok(Val::Map(result))
}

fn text<'a>(value: &'a Val, name: &str) -> Result<&'a str> {
    match value.field(name) {
        Some(Val::Str(text)) => Ok(text),
        _ => Err(unsupported(format!("shader field {name} that is not text"))),
    }
}

fn sequence<'a>(value: &'a Val, name: &str) -> Result<&'a [Val]> {
    match value.field(name) {
        Some(Val::Seq(items)) => Ok(items),
        Some(Val::Zero) => Ok(&[]),
        _ => bail!("shader artifact lacks the sequence {name}"),
    }
}

fn number(value: &Val, name: &str) -> Result<i64> {
    match value.field(name) {
        Some(Val::I(number)) => Ok(*number),
        Some(Val::U(number)) => Ok(*number as i64),
        Some(Val::Bool(flag)) => Ok(*flag as i64),
        _ => bail!("shader artifact lacks the number {name}"),
    }
}

/// LZ4 in its high compression form at level 9, into a buffer a kilobyte
/// larger than the input, as a build compresses a table of programs. The
/// reference implementation is linked, since the bytes depend on its match
/// search.
fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let size = i32::try_from(data.len())
        .ok()
        .filter(|size| *size < i32::MAX - 0x400)
        .ok_or_else(|| anyhow!("shader program table is too large"))?;
    let capacity = size + 0x400;
    let mut out = vec![0u8; capacity as usize];
    // SAFETY: both buffers outlive the call and are passed with their sizes.
    let written = unsafe {
        lz4_sys::LZ4_compress_HC(
            data.as_ptr().cast(),
            out.as_mut_ptr().cast(),
            size,
            capacity,
            9,
        )
    };
    if written <= 0 {
        // A build stores a length of zero here.
        return Err(unsupported(
            "a table of shader programs that does not compress",
        ));
    }
    out.truncate(written as usize);
    Ok(out)
}

/// Bakes the worker's artifact of a `.shader` into a ContentFile holding the
/// Player's `Shader` object. Every program is compiled by `compiler`.
pub fn bake_shader(
    artifact: &[u8],
    project_settings: &ShaderProject,
    sources: &dyn ShaderSources,
    compiler: &mut dyn SnippetCompiler,
) -> Result<Baked> {
    let (data, externals) = bake_shader_object(artifact, project_settings, sources, compiler)?;
    let file = serialized::write(
        &[serialized::Object {
            local_id: SHADER_LOCAL_ID,
            tree: project_settings.release.tree()?,
            data,
            script: None,
            script_file: 0,
        }],
        &externals,
    )?;
    Ok(Baked {
        file,
        objects: vec![(SHADER_LOCAL_ID.to_string(), "UnityEngine.Shader".into())],
        externals,
    })
}

/// The bytes of the Player's `Shader` object alone, and the other sources
/// it refers to, in the order of its external references.
pub fn bake_shader_object(
    artifact: &[u8],
    project_settings: &ShaderProject,
    sources: &dyn ShaderSources,
    compiler: &mut dyn SnippetCompiler,
) -> Result<(Vec<u8>, Vec<serialized::External>)> {
    let artifact = read_shader_artifact(artifact)?;
    let tree = project_settings.release.tree()?;
    let mut value = project(tree, 0, &artifact.value)?;
    let externals = object::assemble(
        &artifact.value,
        &mut value,
        project_settings,
        sources,
        compiler,
    )?;
    Ok((tree.encode(&value)?, externals))
}
