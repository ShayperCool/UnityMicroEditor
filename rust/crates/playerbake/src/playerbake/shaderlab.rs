//! Imports a `.shader` as the Editor does, without the Editor: the shader
//! compiler (`UnityShaderCompiler`, which ships beside the Editor and runs
//! without it) preprocesses the source, cutting each program out into a
//! snippet and leaving ShaderLab that names the programs by ID
//! (`GpuProgramID`); the tool parses that ShaderLab into the shader's parsed
//! form and lays the snippets out as its compile information.
//!
//! The conversation was read off a 6000.0.71f1 import worker's own
//! (`c:preprocess`): the request is the source, its directory and file name,
//! whether only surface shaders are wanted, the build platform, the mask of
//! graphics APIs and two lists of keywords; the reply is a run of lines,
//! some followed by items of their own, ending in `shader: <ok> ...` and the
//! ShaderLab.

/// What `c:preprocess` is asked with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreprocessRequest {
    pub source: String,
    /// The source's directory and file name, as the project names them.
    pub directory: String,
    pub file: String,
    pub surface_only: bool,
    /// The build platform (24: Linux standalone).
    pub platform: i32,
    /// The graphics APIs the build targets, as a mask of compiler platforms.
    pub api_mask: i32,
    /// Keywords the platform defines, and keywords the project's settings
    /// define for every program.
    pub platform_keywords: Vec<String>,
    pub defines: Vec<String>,
}

/// A program the compiler cut out of the source.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PreprocessedSnippet {
    /// The numbers of the `snip:` line: the program ID, the platform mask,
    /// the hardware tier mask, the types mask, the language, a flag, the
    /// hash of the included files as four 32-bit words, the start line and
    /// two flags.
    pub header: Vec<i64>,
    pub code: String,
    /// Per kind (`keywordsUserGlobal`, `keywordsUserLocal`,
    /// `keywordsBuiltin`) and program type, the keyword sets.
    pub keywords: Vec<(String, u32, Vec<String>)>,
    /// The three texts after `keywordsEnd`.
    pub keyword_texts: [String; 3],
    pub base_requirements: u64,
    /// Each keyword with the requirements it brings.
    pub targets: Vec<(String, u64)>,
}

/// A message of the compiler about the source.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PreprocessMessage {
    /// The numbers of the `err:` line.
    pub header: Vec<i64>,
    pub file: String,
    pub message: String,
}

/// What `c:preprocess` answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PreprocessReply {
    pub snippets: Vec<PreprocessedSnippet>,
    /// The files the programs include, and the paths tried for them.
    pub includes: Vec<String>,
    pub filepaths: Vec<String>,
    pub messages: Vec<PreprocessMessage>,
    /// Whether the source was preprocessed (the first number of `shader:`).
    pub ok: bool,
    pub shader_line: String,
    /// The ShaderLab with the programs cut out.
    pub shader: String,
}

/// The numbers of a reply line after its name.
pub fn line_numbers(line: &str) -> Vec<i64> {
    line.split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|number| number.parse().ok())
        .collect()
}

/// The keywords a Linux standalone build defines for the platform, and those
/// whose definition the build's settings choose, as a 6000.0 import asks.
const STANDALONE_PLATFORM_KEYWORDS: &[&str] =
    &["SHADER_API_DESKTOP", "UNITY_PLATFORM_SUPPORTS_DEPTH_FETCH"];
const STANDALONE_DEFINES: &[&str] = &[
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
];

impl PreprocessRequest {
    /// The request a 6000.0 import of `path` (as the project names it) makes
    /// for a Linux standalone target with OpenGL Core and Vulkan.
    pub fn standalone(path: &str, source: String) -> Self {
        let (directory, file) = path.rsplit_once('/').unwrap_or(("", path));
        PreprocessRequest {
            source,
            directory: directory.to_string(),
            file: file.to_string(),
            surface_only: false,
            platform: 24,
            api_mask: 0x48000,
            platform_keywords: STANDALONE_PLATFORM_KEYWORDS
                .iter()
                .map(|k| k.to_string())
                .collect(),
            defines: STANDALONE_DEFINES.iter().map(|k| k.to_string()).collect(),
        }
    }
}
