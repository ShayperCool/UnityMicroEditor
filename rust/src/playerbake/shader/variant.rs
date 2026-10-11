//! Which variants of a pass a build compiles, and the request for each.
use super::{ShaderProject, SnippetRequest, number, sequence, text};
use crate::playerbake::{typetree::Val, unsupported};
use anyhow::Result;

/// The program source of a pass with what the importer found in it.
pub(super) struct Snippet {
    code: Vec<u8>,
    asset_directory: String,
    asset_name: String,
    start_line: i32,
    requirements: u64,
    /// Whether the pass declares any keyword.
    keywords: bool,
}

impl Snippet {
    pub(super) fn read(value: &Val) -> Result<Self> {
        let code = match value.field("m_Code") {
            Some(Val::Str(code)) => code.clone().into_bytes(),
            Some(Val::Bytes(code)) => code.clone(),
            _ => anyhow::bail!("shader artifact snippet lacks its source"),
        };
        let mut keywords = false;
        if let Val::Map(fields) = value {
            for (name, field) in fields {
                let listed = name.starts_with("m_Variants")
                    || name.starts_with("m_PrunedKeywords")
                    || name == "m_DynamicBranchKeywords";
                if listed && !matches!(field, Val::Zero) && !sequence(value, name)?.is_empty() {
                    keywords = true;
                }
            }
        }
        Ok(Self {
            code,
            asset_directory: text(value, "m_AssetPath")?.to_string(),
            asset_name: text(value, "m_AssetName")?.to_string(),
            start_line: number(value, "m_StartLine")? as i32,
            requirements: number(value, "m_BaseRequirements")? as u64,
            keywords,
        })
    }
}

pub(super) struct Variant {
    pub(super) request: SnippetRequest,
    /// The variant's keywords as positions in the shader's keyword list.
    pub(super) keyword_indices: Vec<u16>,
}

const ENABLED: [&str; 11] = [
    "UNITY_ENABLE_REFLECTION_BUFFERS",
    "UNITY_USE_DITHER_MASK_FOR_ALPHABLENDED_SHADOWS",
    "UNITY_PBS_USE_BRDF1",
    "UNITY_SPECCUBE_BOX_PROJECTION",
    "UNITY_SPECCUBE_BLENDING",
    "UNITY_ENABLE_DETAIL_NORMALMAP",
    "SHADER_API_DESKTOP",
    "UNITY_COLORSPACE_GAMMA",
    "UNITY_LIGHT_PROBE_PROXY_VOLUME",
    "UNITY_LIGHTMAP_FULL_HDR",
    "UNITY_PLATFORM_SUPPORTS_DEPTH_FETCH",
];
const DISABLED: [&str; 18] = [
    "UNITY_NO_DXT5nm",
    "UNITY_FRAMEBUFFER_FETCH_AVAILABLE",
    "UNITY_METAL_SHADOWS_USE_POINT_FILTERING",
    "UNITY_NO_SCREENSPACE_SHADOWS",
    "UNITY_PBS_USE_BRDF2",
    "UNITY_PBS_USE_BRDF3",
    "UNITY_HARDWARE_TIER1",
    "UNITY_HARDWARE_TIER2",
    "UNITY_HARDWARE_TIER3",
    "UNITY_LIGHTMAP_DLDR_ENCODING",
    "UNITY_LIGHTMAP_RGBM_ENCODING",
    "UNITY_VIRTUAL_TEXTURING",
    "UNITY_PRETRANSFORM_TO_DISPLAY_ORIENTATION",
    "UNITY_ASTC_NORMALMAP_ENCODING",
    "SHADER_API_GLES30",
    "SHADER_API_GLES31",
    "SHADER_API_GLES32",
    "UNITY_UNIFIED_SHADER_PRECISION_MODEL",
];

#[allow(clippy::too_many_arguments)]
pub(super) fn enumerate(
    snippet: &Snippet,
    shader_name: &str,
    pass_name: &str,
    program_mask: u32,
    platform: u32,
    project: &ShaderProject,
    _keyword_names: &[String],
) -> Result<Vec<Variant>> {
    if snippet.keywords {
        return Err(unsupported("a pass with keyword variants"));
    }
    if project.linear {
        return Err(unsupported("a shader of a project in linear colour space"));
    }
    let disabled: Vec<String> = DISABLED.iter().map(|name| name.to_string()).collect();
    let constants = disabled
        .iter()
        .map(|name| format!("static const int {name} = 0;\n"))
        .collect();
    Ok(vec![Variant {
        request: SnippetRequest {
            source: snippet.code.clone(),
            asset_directory: snippet.asset_directory.clone(),
            asset_name: snippet.asset_name.clone(),
            shader_name: shader_name.to_string(),
            pass_name: pass_name.to_string(),
            leading: [0; 3],
            build_target: 24,
            enabled: ENABLED.iter().map(|name| name.to_string()).collect(),
            keywords: Vec::new(),
            disabled,
            keywords_off: 0,
            constants,
            middle: (0, 0, 0),
            platform,
            requirements: snippet.requirements,
            program_mask,
            start_line: snippet.start_line,
            trailing: 1,
        },
        keyword_indices: Vec::new(),
    }])
}
