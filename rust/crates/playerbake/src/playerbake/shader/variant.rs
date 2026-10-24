//! Which variants of a pass a build compiles, in which order, and the
//! request for each. The rules were read out of the pinned Editor
//! (`ShaderCompilation::PrepareVariantsAndRunCompilation`,
//! `ShaderVariantEnumerationUsage`, `ShouldShaderKeywordVariantBeStripped`,
//! `platform_caps_keywords`, `CompileShaderSnippet`) and confirmed against
//! the requests it sends in AssetBundle builds.
use super::{ShaderProject, ShaderSettings, SnippetRequest, number, sequence, text};
use crate::playerbake::{typetree::Val, unsupported};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};

/// What a build does with a pass.
pub(super) enum PassPlan {
    /// The pass does not reach a Player.
    Dropped,
    /// The pass holds no program of its own and is written as parsed.
    Untouched,
    /// The variants of every graphics API, in the order a build stores them.
    Compiled(Vec<Variant>),
}

pub(super) struct Variant {
    pub(super) request: SnippetRequest,
    /// The heading the pass lists the variant under: a hardware tier, or 3
    /// when the graphics API has no tier variants.
    pub(super) tier: usize,
}

/// The engine's platform defines, by bit.
const DEFINES: [&str; 39] = [
    "UNITY_NO_DXT5nm",
    "UNITY_NO_RGBM",
    "UNITY_USE_NATIVE_HDR",
    "UNITY_ENABLE_REFLECTION_BUFFERS",
    "UNITY_FRAMEBUFFER_FETCH_AVAILABLE",
    "UNITY_ENABLE_NATIVE_SHADOW_LOOKUPS",
    "UNITY_METAL_SHADOWS_USE_POINT_FILTERING",
    "UNITY_NO_CUBEMAP_ARRAY",
    "UNITY_NO_SCREENSPACE_SHADOWS",
    "UNITY_USE_DITHER_MASK_FOR_ALPHABLENDED_SHADOWS",
    "UNITY_PBS_USE_BRDF1",
    "UNITY_PBS_USE_BRDF2",
    "UNITY_PBS_USE_BRDF3",
    "UNITY_NO_FULL_STANDARD_SHADER",
    "UNITY_SPECCUBE_BOX_PROJECTION",
    "UNITY_SPECCUBE_BLENDING",
    "UNITY_ENABLE_DETAIL_NORMALMAP",
    "SHADER_API_MOBILE",
    "SHADER_API_DESKTOP",
    "UNITY_HARDWARE_TIER1",
    "UNITY_HARDWARE_TIER2",
    "UNITY_HARDWARE_TIER3",
    "UNITY_COLORSPACE_GAMMA",
    "UNITY_LIGHT_PROBE_PROXY_VOLUME",
    "UNITY_HALF_PRECISION_FRAGMENT_SHADER_REGISTERS",
    "UNITY_LIGHTMAP_DLDR_ENCODING",
    "UNITY_LIGHTMAP_RGBM_ENCODING",
    "UNITY_LIGHTMAP_FULL_HDR",
    "UNITY_VIRTUAL_TEXTURING",
    "UNITY_PRETRANSFORM_TO_DISPLAY_ORIENTATION",
    "UNITY_ASTC_NORMALMAP_ENCODING",
    "SHADER_API_GLES30",
    "SHADER_API_GLES31",
    "SHADER_API_GLES32",
    "UNITY_UNIFIED_SHADER_PRECISION_MODEL",
    "UNITY_PLATFORM_SUPPORTS_WAVE_32",
    "UNITY_PLATFORM_SUPPORTS_WAVE_64",
    "UNITY_NEEDS_RENDERPASS_FBFETCH_FALLBACK",
    "UNITY_PLATFORM_SUPPORTS_DEPTH_FETCH",
];
/// The defines a program is told about when they are off.
const SETTINGS_DEPENDENT: u64 = 0x7_fef9_df59;
/// The defines of 6000.0, in the order its requests list them: those of
/// 6000.6 it knows, each one told about when it is off.
const DEFINES_6000_0: [&str; 29] = [
    "UNITY_NO_DXT5nm",
    "UNITY_ENABLE_REFLECTION_BUFFERS",
    "UNITY_FRAMEBUFFER_FETCH_AVAILABLE",
    "UNITY_METAL_SHADOWS_USE_POINT_FILTERING",
    "UNITY_NO_SCREENSPACE_SHADOWS",
    "UNITY_USE_DITHER_MASK_FOR_ALPHABLENDED_SHADOWS",
    "UNITY_PBS_USE_BRDF1",
    "UNITY_PBS_USE_BRDF2",
    "UNITY_PBS_USE_BRDF3",
    "UNITY_SPECCUBE_BOX_PROJECTION",
    "UNITY_SPECCUBE_BLENDING",
    "UNITY_ENABLE_DETAIL_NORMALMAP",
    "SHADER_API_DESKTOP",
    "UNITY_HARDWARE_TIER1",
    "UNITY_HARDWARE_TIER2",
    "UNITY_HARDWARE_TIER3",
    "UNITY_COLORSPACE_GAMMA",
    "UNITY_LIGHT_PROBE_PROXY_VOLUME",
    "UNITY_LIGHTMAP_DLDR_ENCODING",
    "UNITY_LIGHTMAP_RGBM_ENCODING",
    "UNITY_LIGHTMAP_FULL_HDR",
    "UNITY_VIRTUAL_TEXTURING",
    "UNITY_PRETRANSFORM_TO_DISPLAY_ORIENTATION",
    "UNITY_ASTC_NORMALMAP_ENCODING",
    "SHADER_API_GLES30",
    "SHADER_API_GLES31",
    "SHADER_API_GLES32",
    "UNITY_UNIFIED_SHADER_PRECISION_MODEL",
    "UNITY_PLATFORM_SUPPORTS_DEPTH_FETCH",
];
/// The stages a program can be told apart by; the engine skips the sixth.
const STAGE_DEFINES: [(usize, &str); 6] = [
    (0, "SHADER_STAGE_VERTEX"),
    (1, "SHADER_STAGE_FRAGMENT"),
    (2, "SHADER_STAGE_HULL"),
    (3, "SHADER_STAGE_DOMAIN"),
    (4, "SHADER_STAGE_GEOMETRY"),
    (6, "SHADER_STAGE_RAY_TRACING"),
];
const STAGES: usize = 7;
/// OpenGL Core and Vulkan, the graphics APIs whose rules are reproduced.
/// Both compile all stages of a pass into one program.
const PLATFORMS: [u32; 2] = [15, 18];
const STEREO: [&str; 4] = [
    "STEREO_INSTANCING_ON",
    "UNITY_SINGLE_PASS_STEREO",
    "STEREO_MULTIVIEW_ON",
    "STEREO_CUBEMAP_RENDER_ON",
];
/// Linux standalone as the compiler knows it.
const BUILD_TARGET: i32 = 24;

/// A name of underscores stands for "no keyword" in a group.
fn blank(name: &str) -> bool {
    name.bytes().all(|byte| byte == b'_')
}

/// The pass kind a `LightMode` tag selects in the built-in pipeline, and the
/// define a program of that kind is compiled with.
fn pass_define(light_mode: Option<&str>) -> Option<&'static str> {
    match light_mode?.to_ascii_uppercase().as_str() {
        "FORWARDBASE" => Some("UNITY_PASS_FORWARDBASE"),
        "FORWARDADD" => Some("UNITY_PASS_FORWARDADD"),
        "SHADOWCASTER" => Some("UNITY_PASS_SHADOWCASTER"),
        "DEFERRED" => Some("UNITY_PASS_DEFERRED"),
        "META" => Some("UNITY_PASS_META"),
        "MOTIONVECTORS" => Some("UNITY_PASS_MOTIONVECTORS"),
        "SRPDEFAULTUNLIT" => Some("UNITY_PASS_SRPDEFAULTUNLIT"),
        _ => None,
    }
}

/// A set of keywords by position in the shader's list. States are ordered
/// as the engine orders them: by their 64-bit words from the first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct State(Vec<u64>);

impl State {
    fn empty(keywords: usize) -> Self {
        Self(vec![0; keywords.div_ceil(64)])
    }
    fn has(&self, index: usize) -> bool {
        self.0[index >> 6] >> (index & 63) & 1 != 0
    }
    fn set(&mut self, index: usize) {
        self.0[index >> 6] |= 1 << (index & 63);
    }
    fn zip(&self, other: &State, operation: impl Fn(u64, u64) -> u64) -> State {
        State(
            self.0
                .iter()
                .zip(&other.0)
                .map(|(&a, &b)| operation(a, b))
                .collect(),
        )
    }
    fn union(&self, other: &State) -> State {
        self.zip(other, |a, b| a | b)
    }
    fn minus(&self, other: &State) -> State {
        self.zip(other, |a, b| a & !b)
    }
    fn common(&self, other: &State) -> State {
        self.zip(other, |a, b| a & b)
    }
    fn count(&self) -> i64 {
        self.0.iter().map(|word| word.count_ones() as i64).sum()
    }
    fn indices(&self) -> Vec<usize> {
        (0..self.0.len() * 64)
            .filter(|&index| self.has(index))
            .collect()
    }
}

type Groups = Vec<Vec<String>>;

/// The program source of a pass with what the importer found in it.
struct Snippet {
    code: Vec<u8>,
    asset_directory: String,
    asset_name: String,
    platform_mask: u32,
    tier_mask: u32,
    start_line: i32,
    stages: u32,
    language: i32,
    toolchains: bool,
    /// Keyword groups by stage: across all materials, per material, and the
    /// engine's own sets.
    global: Vec<Groups>,
    local: Vec<Groups>,
    builtin: Vec<Groups>,
    pruned: Vec<Vec<String>>,
    dynamic: Vec<String>,
    requirements: u64,
    keyword_requirements: Vec<(String, u64)>,
    /// The keywords declared with `multi_compile`, which no usage strips.
    kept: Vec<String>,
}

fn string(value: &Val) -> Result<String> {
    match value {
        Val::Str(text) => Ok(text.clone()),
        _ => Err(unsupported("a shader keyword that is not text")),
    }
}

fn strings(value: &Val, name: &str) -> Result<Vec<String>> {
    sequence(value, name)?.iter().map(string).collect()
}

fn groups(value: &Val, name: &str) -> Result<Vec<Groups>> {
    (0..STAGES)
        .map(|stage| {
            sequence(value, &format!("{name}{stage}"))?
                .iter()
                .map(|group| match group {
                    Val::Seq(names) => names.iter().map(string).collect(),
                    Val::Zero => Ok(Vec::new()),
                    _ => bail!("shader artifact keyword group is not a sequence"),
                })
                .collect()
        })
        .collect()
}

impl Snippet {
    fn read(value: &Val) -> Result<Self> {
        let code = match value.field("m_Code") {
            Some(Val::Str(code)) => code.clone().into_bytes(),
            Some(Val::Bytes(code)) => code.clone(),
            _ => bail!("shader artifact snippet lacks its source"),
        };
        let mut keyword_requirements = Vec::new();
        for target in sequence(value, "m_KeywordTargetInfo")? {
            keyword_requirements.push((
                text(target, "keywordName")?.to_string(),
                number(target, "requirements")? as u64,
            ));
        }
        Ok(Self {
            code,
            asset_directory: text(value, "m_AssetPath")?.to_string(),
            asset_name: text(value, "m_AssetName")?.to_string(),
            platform_mask: number(value, "m_PlatformMask")? as u32,
            tier_mask: number(value, "m_HardwareTierVariantsMask")? as u32,
            start_line: number(value, "m_StartLine")? as i32,
            stages: number(value, "m_TypesMask")? as u32,
            language: number(value, "m_Language")? as i32,
            // 6000.0 has no compiler toolchains and no pruned keywords.
            toolchains: match value.field("m_PerAPICompilerToolchain") {
                Some(_) => sequence(value, "m_PerAPICompilerToolchain")?
                    .iter()
                    .any(|toolchain| !matches!(toolchain, Val::U(0) | Val::I(0) | Val::Zero)),
                None => false,
            },
            global: groups(value, "m_VariantsUserGlobal")?,
            local: groups(value, "m_VariantsUserLocal")?,
            builtin: groups(value, "m_VariantsBuiltin")?,
            pruned: (0..STAGES)
                .map(|stage| {
                    let name = format!("m_PrunedKeywords{stage}");
                    match value.field(&name) {
                        Some(_) => strings(value, &name),
                        None => Ok(Vec::new()),
                    }
                })
                .collect::<Result<_>>()?,
            dynamic: strings(value, "m_DynamicBranchKeywords")?,
            requirements: number(value, "m_BaseRequirements")? as u64,
            keyword_requirements,
            kept: text(value, "m_NonStrippedUserKeywords")?
                .split_ascii_whitespace()
                .map(str::to_string)
                .collect(),
        })
    }

    /// The keyword groups of the one program a pass compiles to on a
    /// graphics API that takes all stages together: those of the first
    /// stage, then each group of a later stage that is not among them yet.
    fn merged(groups: &[Groups]) -> Groups {
        let mut out = groups[0].clone();
        let mut seen: BTreeSet<Vec<String>> = out.iter().cloned().collect();
        for stage in &groups[1..] {
            for group in stage {
                if seen.insert(group.clone()) {
                    out.push(group.clone());
                }
            }
        }
        out
    }
}

struct Pass<'a> {
    kind: i64,
    program_mask: u32,
    snippet: i64,
    name: &'a str,
    light_mode: Option<&'a str>,
}

struct Shader<'a> {
    release: super::ShaderRelease,
    name: &'a str,
    keywords: Vec<String>,
    snippets: BTreeMap<i64, Snippet>,
    dynamic: BTreeSet<String>,
}

impl Shader<'_> {
    fn index(&self, name: &str) -> Option<usize> {
        self.keywords.iter().position(|known| known == name)
    }
    fn state<'n>(&self, names: impl IntoIterator<Item = &'n String>) -> State {
        let mut state = State::empty(self.keywords.len());
        for name in names {
            if let Some(index) = self.index(name) {
                state.set(index);
            }
        }
        state
    }
    fn names(&self, state: &State) -> Vec<String> {
        state
            .indices()
            .into_iter()
            .map(|index| self.keywords[index].clone())
            .collect()
    }
    fn has(&self, state: &State, name: &str) -> bool {
        self.index(name).is_some_and(|index| state.has(index))
    }

    /// The keyword list the importer builds: the engine's stereo keywords,
    /// then per snippet and stage every keyword of its groups, once.
    fn space(&self) -> Vec<String> {
        let mut out: Vec<String> = STEREO.iter().map(|name| name.to_string()).collect();
        for snippet in self.snippets.values() {
            for stage in 0..STAGES {
                for groups in [
                    &snippet.global[stage],
                    &snippet.local[stage],
                    &snippet.builtin[stage],
                ] {
                    for name in groups.iter().flatten() {
                        if !blank(name) && !out.contains(name) {
                            out.push(name.clone());
                        }
                    }
                }
            }
        }
        out
    }
}

/// The keyword combinations of one program: an odometer over its groups and
/// the engine's own sets, narrowed to what the materials use.
struct Enumeration<'a> {
    shader: &'a Shader<'a>,
    /// Per group the keyword positions; `None` is "no keyword".
    groups: Vec<Vec<Option<usize>>>,
    dynamic_groups: Vec<Vec<Option<usize>>>,
    builtin: Vec<State>,
    usage: Option<Vec<State>>,
}

impl<'a> Enumeration<'a> {
    fn new(
        shader: &'a Shader<'a>,
        global: &Groups,
        local: &Groups,
        builtin: &Groups,
    ) -> Result<Self> {
        let mut groups = Vec::new();
        let mut dynamic_groups = Vec::new();
        for group in global.iter().chain(local) {
            let mut positions = Vec::with_capacity(group.len());
            let mut dynamic = false;
            for name in group {
                if blank(name) {
                    positions.push(None);
                    continue;
                }
                let Some(index) = shader.index(name) else {
                    bail!("shader keyword {name} is missing from the shader's list");
                };
                dynamic |= shader.dynamic.contains(name);
                positions.push(Some(index));
            }
            if dynamic {
                dynamic_groups.push(positions);
            } else {
                groups.push(positions);
            }
        }
        Ok(Self {
            shader,
            groups,
            dynamic_groups,
            builtin: builtin.iter().map(|set| shader.state(set)).collect(),
            usage: None,
        })
    }

    fn none(&self) -> State {
        State::empty(self.shader.keywords.len())
    }

    /// Every keyword the program knows.
    fn all(&self) -> State {
        let mut out = self.none();
        for set in &self.builtin {
            out = out.union(set);
        }
        for index in self
            .groups
            .iter()
            .chain(&self.dynamic_groups)
            .flatten()
            .flatten()
        {
            out.set(*index);
        }
        out
    }

    /// The variant that exists closest to a state: per group the first of
    /// its keywords the state holds, else none if the group allows that,
    /// else its first; and the best fitting of the engine's own sets.
    fn closest(&self, state: &State) -> State {
        let mut result = self.none();
        for group in &self.groups {
            let mut none = false;
            let mut hit = None;
            for keyword in group {
                match keyword {
                    None => none = true,
                    Some(index) if state.has(*index) => {
                        hit = Some(*index);
                        break;
                    }
                    Some(_) => {}
                }
            }
            match (hit, none, group.first()) {
                (Some(index), _, _) => result.set(index),
                (None, false, Some(Some(index))) => result.set(*index),
                _ => {}
            }
        }
        let mut best: Option<(&State, i64)> = None;
        for set in &self.builtin {
            let candidate = set.union(&result);
            let score = state.common(&candidate).count() - 16 * candidate.minus(state).count();
            if best.is_none_or(|(_, known)| score > known) {
                best = Some((set, score));
            }
        }
        match best {
            Some((set, _)) => result.union(set),
            None => result,
        }
    }

    /// Narrows the enumeration to the states the materials use and returns
    /// the size of the whole variant space.
    fn by_usage(&mut self, usages: &[State], mask: &State, kept: &State) -> u128 {
        let mut excluded = kept.clone();
        for set in &self.builtin {
            excluded = excluded.union(set);
        }
        let mut found = BTreeSet::new();
        found.insert(self.closest(&self.none()).minus(&excluded));
        for usage in usages {
            let state = usage.minus(&excluded).common(mask);
            let adjusted = self.closest(&state).minus(&excluded);
            if state.common(&adjusted) == state {
                found.insert(adjusted);
            }
        }
        self.usage = Some(found.into_iter().collect());
        self.groups
            .retain(|group| group.iter().flatten().any(|index| kept.has(*index)));
        self.count()
    }

    fn count(&self) -> u128 {
        let mut count =
            self.builtin.len().max(1) as u128 * self.usage.as_ref().map_or(1, Vec::len) as u128;
        for group in &self.groups {
            count = count.saturating_mul(group.len() as u128);
        }
        count
    }

    /// The states in the order the engine enumerates them.
    fn run(&self) -> Result<Vec<State>> {
        let total = self.count();
        if total > 1 << 20 {
            return Err(unsupported(format!(
                "a pass of {total} keyword combinations"
            )));
        }
        let groups = self.groups.len();
        let sets = self.builtin.len().max(1);
        let usages = self.usage.as_ref().map_or(1, Vec::len);
        let mut counters = vec![0usize; groups];
        let (mut set, mut usage) = (0usize, 0usize);
        let mut user = self.none();
        let mut out = Vec::with_capacity(total as usize);
        for _ in 0..total {
            // Steps to the next of the engine's sets; on the first of them
            // the groups' odometer moves on, its last group fastest.
            let mut next_set = |set: &mut usize, user: &mut State| -> usize {
                let current = *set;
                *set = if current + 1 < sets { current + 1 } else { 0 };
                if current != 0 || groups == 0 {
                    return current;
                }
                let mut state = self.none();
                for (group, &counter) in self.groups.iter().zip(counters.iter()) {
                    if let Some(index) = group[counter] {
                        state.set(index);
                    }
                }
                *user = state;
                counters[groups - 1] += 1;
                let mut place = groups - 1;
                while place > 0 && counters[place] == self.groups[place].len() {
                    counters[place] = 0;
                    counters[place - 1] += 1;
                    place -= 1;
                }
                current
            };
            let (chosen, mut state) = match &self.usage {
                None => {
                    let chosen = next_set(&mut set, &mut user);
                    (chosen, user.clone())
                }
                Some(states) => {
                    let current = usage;
                    usage = if current + 1 < usages { current + 1 } else { 0 };
                    // Only the first usage steps the sets; the others take
                    // the set already stepped to, as the engine does.
                    let chosen = if current == 0 {
                        next_set(&mut set, &mut user)
                    } else {
                        set
                    };
                    (chosen, user.union(&states[current]))
                }
            };
            if let Some(builtin) = self.builtin.get(chosen) {
                state = state.union(builtin);
            }
            out.push(state);
        }
        Ok(out)
    }
}

/// What the build as a whole uses of lightmaps, fog and instancing: the
/// project's stripping settings, or under automatic stripping the scene open
/// in the Editor, which in a build from the command line is empty.
struct Usage {
    lightmap_modes: u32,
    legacy_lightmap_modes: u32,
    dynamic_lightmap_modes: u32,
    fog_modes: u32,
    shadowmask: bool,
    shadowmask_mode: bool,
    subtractive: bool,
    strip_instancing: bool,
    keep_instancing: bool,
    batch_renderer: u32,
    hybrid: bool,
    lod_cross_fade: bool,
    instancing: bool,
}

impl Usage {
    fn new(settings: &ShaderSettings, instancing: bool) -> Self {
        let mut usage = Self {
            lightmap_modes: 0,
            legacy_lightmap_modes: 0,
            dynamic_lightmap_modes: 0,
            fog_modes: 0,
            shadowmask: false,
            shadowmask_mode: false,
            subtractive: false,
            strip_instancing: settings.instancing_stripping == 1,
            keep_instancing: settings.instancing_stripping == 2,
            batch_renderer: 1 << (settings.batch_renderer_stripping & 31),
            hybrid: settings.hybrid_package,
            lod_cross_fade: settings.lod_cross_fade,
            instancing,
        };
        if let Some(keep) = &settings.lightmap_keep {
            if keep.plain {
                usage.legacy_lightmap_modes |= 1;
                usage.lightmap_modes |= 1;
            }
            if keep.directional {
                usage.legacy_lightmap_modes |= 4;
                usage.lightmap_modes |= 2;
            }
            if keep.dynamic_plain {
                usage.dynamic_lightmap_modes |= 1;
            }
            if keep.dynamic_directional {
                usage.dynamic_lightmap_modes |= 2;
            }
            if keep.shadowmask {
                usage.shadowmask = true;
                usage.shadowmask_mode = true;
            }
            if keep.subtractive {
                usage.subtractive = true;
                usage.shadowmask = true;
            }
        }
        if let Some(keep) = &settings.fog_keep {
            usage.fog_modes = (keep.linear as u32) << 1
                | (keep.exponential as u32) << 2
                | (keep.exponential_squared as u32) << 3;
        }
        usage
    }

    /// Whether a build leaves a variant out for what it enables.
    fn strips(&self, shader: &Shader<'_>, state: &State) -> bool {
        let has = |name: &str| shader.has(state, name);
        let exists = |name: &str| shader.index(name).is_some();
        // No stereo rendering in the build.
        if STEREO.iter().any(|name| has(name)) {
            return true;
        }
        if has("DOTS_INSTANCING_ON") {
            if self.batch_renderer & 2 != 0 {
                return true;
            }
            if has("DYNAMICLIGHTMAP_ON") || has("INSTANCING_ON") || has("PROCEDURAL_INSTANCING_ON")
            {
                return true;
            }
            if self.batch_renderer & 4 == 0 && !self.hybrid {
                return true;
            }
        }
        if has("INSTANCING_ON")
            && (self.strip_instancing || !self.instancing && !self.hybrid && !self.keep_instancing)
        {
            return true;
        }
        let (plain, legacy, dynamic) = (
            self.lightmap_modes,
            self.legacy_lightmap_modes,
            self.dynamic_lightmap_modes,
        );
        if plain | legacy == 0 && has("LIGHTMAP_ON") {
            return true;
        }
        if dynamic == 0 && has("DYNAMICLIGHTMAP_ON") {
            return true;
        }
        let directional = "DIRLIGHTMAP_COMBINED";
        if plain & 1 == 0
            && legacy & 3 == 0
            && exists(directional)
            && !has(directional)
            && has("LIGHTMAP_ON")
        {
            return true;
        }
        if dynamic & 1 == 0 && exists(directional) && !has(directional) && has("DYNAMICLIGHTMAP_ON")
        {
            return true;
        }
        if plain & 2 == 0 && legacy & 4 == 0 && has("LIGHTMAP_ON") && has(directional) {
            return true;
        }
        if dynamic & 2 == 0 && has("DYNAMICLIGHTMAP_ON") && has(directional) {
            return true;
        }
        if has(directional)
            && !has("LIGHTMAP_ON")
            && !has("DYNAMICLIGHTMAP_ON")
            && (exists("LIGHTMAP_ON") || exists("DYNAMICLIGHTMAP_ON"))
        {
            return true;
        }
        if !(self.shadowmask && self.shadowmask_mode) && has("SHADOWS_SHADOWMASK") {
            return true;
        }
        if !(self.shadowmask && (self.subtractive || self.shadowmask_mode))
            && has("LIGHTMAP_SHADOW_MIXING")
        {
            return true;
        }
        for (name, bit) in [("FOG_LINEAR", 2), ("FOG_EXP", 4), ("FOG_EXP2", 8)] {
            let unused = shader.dynamic.contains(name) || self.fog_modes & bit == 0;
            if unused && has(name) {
                return true;
            }
        }
        if has("EDITOR_VISUALIZATION") {
            return true;
        }
        has("LOD_FADE_CROSSFADE") && !self.lod_cross_fade
    }
}

/// The platform defines that are on for a hardware tier of the standalone
/// target, one bit each.
fn platform_defines(settings: &ShaderSettings, tier: usize) -> u64 {
    // Desktop, and a depth buffer programs can read.
    let mut bits = 1u64 << 18 | 1 << 38;
    if !settings.linear {
        bits |= 1 << 22;
    }
    if settings.reflection_buffers {
        bits |= 1 << 3;
    }
    bits |= 1 << (25 + settings.lightmap_encoding.min(2) as u64);
    if settings.virtual_texturing {
        bits |= 1 << 28;
    }
    if settings.unified_precision {
        bits |= 1 << 34;
    }
    let tier = &settings.tiers[tier];
    if !tier.cascaded_shadows {
        bits |= 1 << 8;
    }
    if tier.dither_mask {
        bits |= 1 << 9;
    }
    bits |= 1 << (12 - tier.quality.min(2) as u64);
    if tier.box_projection {
        bits |= 1 << 14;
    }
    if tier.probe_blending {
        bits |= 1 << 15;
    }
    if tier.detail_normal_map {
        bits |= 1 << 16;
    }
    if tier.proxy_volumes {
        bits |= 1 << 23;
    }
    bits
}

/// The compiler options some of the defines stand for.
fn compilation_flags(bits: u64) -> u64 {
    let mut flags = 0;
    for (bit, flag) in [
        (17, 0x1000_0000u64),
        (34, 0x8000_0000),
        (6, 0x4000),
        (37, 0x1_0000_0000),
    ] {
        if bits >> bit & 1 != 0 {
            flags |= flag;
        }
    }
    flags
}

/// The keywords of some groups that a state leaves off, in group order,
/// each once; dynamic keywords are never declared off.
fn off(shader: &Shader<'_>, lists: [&Groups; 3], state: &State, out: &mut Vec<String>) {
    for name in lists.into_iter().flatten().flatten() {
        let Some(index) = shader.index(name) else {
            continue;
        };
        if blank(name) || state.has(index) || shader.dynamic.contains(name) || out.contains(name) {
            continue;
        }
        out.push(name.clone());
    }
}

fn declarations(names: &[String], stage: Option<&str>) -> String {
    let body: String = names
        .iter()
        .map(|name| format!("static const int {name} = 0;\n"))
        .collect();
    match stage {
        Some(stage) => format!("#if defined({stage})\n{body}#endif\n"),
        None => body,
    }
}

#[allow(clippy::too_many_arguments)]
fn request(
    shader: &Shader<'_>,
    snippet: &Snippet,
    pass: &Pass<'_>,
    merged: (&Groups, &Groups),
    platform: u32,
    state: &State,
    bits: u64,
    requirements: u64,
) -> SnippetRequest {
    let on = |name: &str| {
        DEFINES
            .iter()
            .position(|define| *define == name)
            .is_some_and(|bit| bits >> bit & 1 != 0)
    };
    let mut enabled: Vec<String> = match shader.release {
        super::ShaderRelease::Unity6000_0 => DEFINES_6000_0
            .iter()
            .filter(|name| on(name))
            .map(|name| name.to_string())
            .collect(),
        super::ShaderRelease::Unity6000_6 => (0..DEFINES.len())
            .filter(|&bit| bits >> bit & 1 != 0)
            .map(|bit| DEFINES[bit].to_string())
            .collect(),
    };
    if let Some(define) = pass_define(pass.light_mode) {
        enabled.push(define.to_string());
    }
    let mut disabled = Vec::new();
    off(
        shader,
        [merged.0, merged.1, &snippet.builtin[0]],
        state,
        &mut disabled,
    );
    disabled.extend(snippet.pruned[0].iter().cloned());
    let keywords_off = disabled.len();
    match shader.release {
        super::ShaderRelease::Unity6000_0 => disabled.extend(
            DEFINES_6000_0
                .iter()
                .filter(|name| !on(name))
                .map(|name| name.to_string()),
        ),
        super::ShaderRelease::Unity6000_6 => disabled.extend(
            (0..DEFINES.len())
                .filter(|&bit| bits >> bit & 1 == 0 && SETTINGS_DEPENDENT >> bit & 1 != 0)
                .map(|bit| DEFINES[bit].to_string()),
        ),
    }
    // Each stage is told which of its own keywords are off.
    let mut constants = String::new();
    for (stage, define) in STAGE_DEFINES {
        let mut names = Vec::new();
        off(
            shader,
            [
                &snippet.global[stage],
                &snippet.local[stage],
                &snippet.builtin[stage],
            ],
            state,
            &mut names,
        );
        names.extend(snippet.pruned[stage].iter().cloned());
        if !names.is_empty() {
            constants.push_str(&declarations(&names, Some(define)));
        }
    }
    constants.push_str(&declarations(&disabled[keywords_off..], None));
    // 6000.0 declares nothing for the compiler to prepend.
    if shader.release == super::ShaderRelease::Unity6000_0 {
        constants.clear();
    }
    SnippetRequest {
        release: shader.release,
        source: snippet.code.clone(),
        asset_directory: snippet.asset_directory.clone(),
        asset_name: snippet.asset_name.clone(),
        shader_name: shader.name.to_string(),
        pass_name: pass.name.to_string(),
        options: [0, 0],
        build_target: BUILD_TARGET,
        reserved: 0,
        enabled,
        keywords: shader.names(state),
        disabled,
        keywords_off: keywords_off as u32,
        constants,
        flags: compilation_flags(bits),
        language: snippet.language,
        program: 0,
        platform,
        requirements,
        program_mask: pass.program_mask,
        start_line: snippet.start_line,
        toolchain: 1,
    }
}

fn compile(
    shader: &Shader<'_>,
    snippet: &Snippet,
    pass: &Pass<'_>,
    project: &ShaderProject,
    usage: &Usage,
) -> Result<Vec<Variant>> {
    if snippet.toolchains {
        return Err(unsupported("a pass that selects a compiler toolchain"));
    }
    if snippet.stages & 1 == 0 {
        return Err(unsupported("a pass without a vertex program"));
    }
    let settings = &project.settings;
    let global = Snippet::merged(&snippet.global);
    let local = Snippet::merged(&snippet.local);
    let materials: Vec<State> = project
        .materials
        .iter()
        .map(|material| shader.state(&material.keywords))
        .collect();
    let kept = shader.state(&snippet.kept);
    let mut requirements_of: BTreeMap<usize, u64> = BTreeMap::new();
    for (name, requirements) in &snippet.keyword_requirements {
        if let Some(index) = shader.index(name) {
            *requirements_of.entry(index).or_default() |= requirements;
        }
    }
    let mut variants = Vec::new();
    // A pass is enumerated by what materials use until one graphics API has
    // no more than one combination; every later API of the pass then gets
    // all combinations, unstripped. A build visits the APIs by their number,
    // whatever order the project lists them in.
    let mut by_usage = true;
    let mut platforms = project.platforms.clone();
    platforms.sort_unstable();
    for platform in platforms {
        if !PLATFORMS.contains(&platform) {
            return Err(unsupported(format!(
                "the graphics API with compiler platform {platform}"
            )));
        }
        if snippet.platform_mask >> platform & 1 == 0 {
            continue;
        }
        let mut enumeration = Enumeration::new(shader, &global, &local, &snippet.builtin[0])?;
        let full = if by_usage {
            let mut mask = shader.state(STEREO.map(String::from).iter());
            for stage in 0..STAGES {
                let stage = Enumeration::new(
                    shader,
                    &snippet.global[stage],
                    &snippet.local[stage],
                    &snippet.builtin[stage],
                )?;
                mask = mask.union(&stage.all());
            }
            enumeration.by_usage(&materials, &mask, &kept)
        } else {
            enumeration.count()
        };
        if full <= 1 {
            by_usage = false;
        }
        let tiered = settings.tiers[0] != settings.tiers[1]
            || settings.tiers[1] != settings.tiers[2]
            || snippet.tier_mask >> platform & 1 != 0;
        let mut seen = BTreeSet::new();
        for state in enumeration.run()? {
            if by_usage && usage.strips(shader, &state) {
                continue;
            }
            if !seen.insert(state.clone()) {
                continue;
            }
            let mut requirements = snippet.requirements;
            for index in state.indices() {
                requirements |= requirements_of.get(&index).copied().unwrap_or(0);
            }
            for tier in 0..if tiered { 3 } else { 1 } {
                let mut bits = platform_defines(settings, tier);
                if tiered {
                    bits |= 1 << (19 + tier);
                }
                variants.push(Variant {
                    request: request(
                        shader,
                        snippet,
                        pass,
                        (&global, &local),
                        platform,
                        &state,
                        bits,
                        requirements,
                    ),
                    tier: if tiered { tier } else { 3 },
                });
            }
        }
    }
    Ok(variants)
}

/// What a build does with every pass of the shader, in the order the parsed
/// form lists them.
pub(super) fn plan(artifact: &Val, project: &ShaderProject) -> Result<Vec<PassPlan>> {
    let parsed = artifact
        .field("m_ParsedForm")
        .ok_or_else(|| anyhow!("shader artifact lacks its parsed form"))?;
    let info = artifact
        .field("m_CompileInfo")
        .ok_or_else(|| anyhow!("shader artifact lacks its compile information"))?;
    let mut snippets = BTreeMap::new();
    for entry in sequence(info, "m_Snippets")? {
        let snippet = entry
            .field("second")
            .ok_or_else(|| anyhow!("shader artifact snippet is incomplete"))?;
        snippets.insert(number(entry, "first")?, Snippet::read(snippet)?);
    }
    let keywords = strings(parsed, "m_KeywordNames")?;
    let mut dynamic = BTreeSet::new();
    for snippet in snippets.values() {
        for name in &snippet.dynamic {
            if keywords.contains(name) {
                dynamic.insert(name.clone());
            }
        }
    }
    let shader = Shader {
        release: project.release,
        name: text(parsed, "m_Name")?,
        keywords,
        snippets,
        dynamic,
    };
    if shader.space() != shader.keywords {
        return Err(unsupported(
            "a shader whose keyword list is not the one its passes declare",
        ));
    }
    let instancing = project.materials.iter().any(|material| material.instancing);
    let usage = Usage::new(&project.settings, instancing);
    let mut plans = Vec::new();
    for subshader in sequence(parsed, "m_SubShaders")? {
        for pass in sequence(subshader, "m_Passes")? {
            let state = pass
                .field("m_State")
                .ok_or_else(|| anyhow!("shader pass lacks its state"))?;
            let tags = state
                .field("m_Tags")
                .ok_or_else(|| anyhow!("shader pass lacks its tags"))?;
            let mut light_mode = None;
            for tag in sequence(tags, "tags")? {
                if text(tag, "first")? == "LIGHTMODE" {
                    light_mode = Some(text(tag, "second")?);
                }
            }
            let pass = Pass {
                kind: number(pass, "m_Type")?,
                program_mask: number(pass, "m_ProgramMask")? as u32,
                snippet: number(state, "gpuProgramID")?,
                name: text(state, "m_Name")?,
                light_mode,
            };
            plans.push(match pass.kind {
                0 => {
                    let Some(snippet) = shader.snippets.get(&pass.snippet) else {
                        return Err(unsupported("a pass without a program to compile"));
                    };
                    if pass.program_mask & 0x40 != 0 {
                        return Err(unsupported("a ray tracing pass"));
                    }
                    let mode = pass.light_mode.map(str::to_ascii_uppercase);
                    match mode.as_deref() {
                        // A pass for the Editor's own views.
                        Some("PICKING" | "SCENESELECTIONPASS") => PassPlan::Dropped,
                        // The pass lightmap baking reads is kept only for
                        // realtime global illumination.
                        Some("META") if usage.dynamic_lightmap_modes == 0 => PassPlan::Dropped,
                        _ => PassPlan::Compiled(compile(&shader, snippet, &pass, project, &usage)?),
                    }
                }
                // A pass taken from another shader and a grab pass hold no
                // program.
                1 | 2 => PassPlan::Untouched,
                other => return Err(unsupported(format!("a pass of kind {other}"))),
            });
        }
    }
    Ok(plans)
}
