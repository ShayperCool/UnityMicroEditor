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

pub mod form;
pub mod parse;

/// The parsed form of a shader the compiler preprocessed.
pub fn parsed_form(reply: &PreprocessReply) -> anyhow::Result<serde_json::Value> {
    let (name, commands) = parse::shader(&reply.shader)?;
    let mut form = form::parsed_form(&name, &commands)?;
    form::complete(&mut form, &reply.snippets);
    Ok(form)
}

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

/// The compile information of a shader (`m_CompileInfo`): each snippet the
/// compiler cut out, by its ID, with the keyword sets, requirements and
/// hashes it reported.
pub fn compile_info(
    reply: &PreprocessReply,
    directory: &str,
    file: &str,
    source: &str,
) -> serde_json::Value {
    use serde_json::json;
    let mut snippets: Vec<&PreprocessedSnippet> = reply.snippets.iter().collect();
    snippets.sort_by_key(|s| s.header.first().copied().unwrap_or(0));
    let entries: Vec<serde_json::Value> = snippets
        .iter()
        .map(|snippet| {
            let number = |index: usize| snippet.header.get(index).copied().unwrap_or(0);
            let mut hash = serde_json::Map::new();
            for word in 0..4 {
                let bytes = (number(6 + word) as i32).to_le_bytes();
                for (index, byte) in bytes.iter().enumerate() {
                    hash.insert(format!("bytes[{}]", word * 4 + index), json!(byte));
                }
            }
            let zero_hash: serde_json::Map<String, serde_json::Value> =
                (0..16).map(|i| (format!("bytes[{i}]"), json!(0))).collect();
            let mut second = serde_json::Map::new();
            for (kind, field) in [
                ("keywordsBuiltin", "m_VariantsBuiltin"),
                ("keywordsUserGlobal", "m_VariantsUserGlobal"),
                ("keywordsUserLocal", "m_VariantsUserLocal"),
            ] {
                for program_type in 0..7u32 {
                    let sets: Vec<Vec<String>> = snippet
                        .keywords
                        .iter()
                        .filter(|(k, t, _)| k == kind && *t == program_type)
                        .flat_map(|(_, _, sets)| sets.iter())
                        .map(|set| set.split_whitespace().map(String::from).collect())
                        .collect();
                    second.insert(format!("{field}{program_type}"), json!(sets));
                }
            }
            let fields = [
                ("m_AssetName", json!(file)),
                ("m_AssetPath", json!(directory)),
                ("m_BaseRequirements", json!(snippet.base_requirements)),
                ("m_NonStrippedUserKeywords", json!(snippet.keyword_texts[0])),
                ("m_BuiltinKeywords", json!(snippet.keyword_texts[2])),
                ("m_Code", json!(snippet.code)),
                ("m_CodeHash", json!(zero_hash)),
                ("m_DynamicBranchKeywords", json!([])),
                ("m_ForceSyncCompilation", json!(false)),
                ("m_FromOther", json!(false)),
                ("m_HardwareTierVariantsMask", json!(number(2))),
                ("m_IncludesHash", json!(hash)),
                (
                    "m_KeywordTargetInfo",
                    json!(
                        snippet
                            .targets
                            .iter()
                            .map(|(k, r)| json!({"keywordName": k, "requirements": r}))
                            .collect::<Vec<_>>()
                    ),
                ),
                // The number before it is the compile flags of the
                // snippet's pragmas (4: `enable_d3d11_debug_symbols`).
                ("m_Language", json!(number(5))),
                ("m_PlatformMask", json!(number(1) as u32)),
                ("m_StartLine", json!(number(10))),
                ("m_TypesMask", json!(number(3))),
                ("m_UseDxcAPIs", json!(number(11))),
                ("m_NeverUseDxcAPIs", json!(number(12))),
            ];
            for (key, value) in fields {
                second.insert(key.into(), value);
            }
            json!({"first": number(0), "second": second})
        })
        .collect();
    json!({
        "m_Snippets": entries,
        "m_MeshComponentsFromSnippets": 0,
        "m_HasSurfaceShaders": has_surface_shaders(source),
        "m_HasFixedFunctionShaders": false,
    })
}

/// Whether a program of the source is a surface shader (`#pragma surface`,
/// not in a comment).
fn has_surface_shaders(source: &str) -> bool {
    let mut block_comment = false;
    for line in source.lines() {
        let mut text = line;
        if block_comment {
            match text.find("*/") {
                Some(end) => {
                    text = &text[end + 2..];
                    block_comment = false;
                }
                None => continue,
            }
        }
        let text = text.split("//").next().unwrap_or_default();
        if let Some(start) = text.find("/*") {
            block_comment = !text[start..].contains("*/");
        }
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.first() == Some(&"#pragma") && words.get(1) == Some(&"surface") {
            return true;
        }
    }
    false
}

/// The `Shader` object the Editor's import of a `.shader` leaves, as a value
/// shaped as `player-file dump` prints it: its parsed form, compile
/// information and messages, its asset's GUID and the default textures of
/// its `.meta` that name a property of the shader.
pub fn editor_object(
    reply: &PreprocessReply,
    request: &PreprocessRequest,
    guid: &str,
    meta: &[u8],
) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    let form = parsed_form(reply)?;
    let info = compile_info(reply, &request.directory, &request.file, &request.source);
    let words =
        super::managers::guid_words(guid).ok_or_else(|| anyhow::anyhow!("{guid} is not a GUID"))?;
    let mut guid_value = serde_json::Map::new();
    for (index, word) in words.iter().enumerate() {
        guid_value.insert(format!("data[{index}]"), json!(word));
    }
    let errors: Vec<serde_json::Value> = reply
        .messages
        .iter()
        .map(|message| {
            let number = |i: usize| message.header.get(i).copied().unwrap_or(0);
            json!({
                "compilerPlatform": number(1),
                "file": message.file,
                "line": number(2),
                "message": message.message,
                "messageDetails": "",
                "runtimeError": false,
                "warning": number(0) == 1,
            })
        })
        .collect();
    let properties: Vec<String> = form["m_PropInfo"]["m_Props"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["m_Type"] == 4)
        .filter_map(|p| p["m_Name"].as_str().map(String::from))
        .collect();
    let settings = super::font::importer_settings(meta, "ShaderImporter").unwrap_or_default();
    let mut defaults = Vec::new();
    for entry in settings
        .get("defaultTextures")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_object)
    {
        for (name, texture) in entry {
            if !properties.contains(name) {
                continue;
            }
            let reference = super::Reference::from_value(texture).filter(|r| r.file_id != 0);
            if reference.is_some() {
                anyhow::bail!("a default texture that names an asset is not imported yet");
            }
            defaults.push(json!({"first": name, "second": null}));
        }
    }
    defaults.sort_by(|a, b| a["first"].as_str().cmp(&b["first"].as_str()));
    // The Editor compiles a shader once after importing it unless it failed
    // or takes passes of other shaders.
    let use_pass = form["m_SubShaders"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["m_Passes"].as_array().into_iter().flatten())
        .any(|p| p["m_Type"] == 1);
    let smoke_test = errors.is_empty() && !use_pass;
    Ok(json!({
        "m_ObjectHideFlags": 8,
        "m_CorrespondingSourceObject": null,
        "m_PrefabInstance": null,
        "m_PrefabAsset": null,
        "m_Name": "",
        "m_ParsedForm": form,
        "platforms": [],
        "offsets": [],
        "compressedLengths": [],
        "decompressedLengths": [],
        "compressedBlob": [],
        "stageCounts": [],
        "m_Dependencies": [],
        "m_NonModifiableTextures": [],
        "m_ShaderIsBaked": false,
        "m_DefaultTextures": defaults,
        "m_CompileInfo": info,
        "m_AssetGUID": guid_value,
        "m_CompileSmokeTestAfterImport": smoke_test,
        "errors": errors,
    }))
}

/// The tree of the Editor's `Shader` of a release, as its imports write it.
pub fn editor_shader_tree(unity_version: &str) -> Option<super::TypeTree> {
    match unity_version {
        "6000.0.71f1" => {
            super::TypeTree::parse(&include_bytes!("../engine/6000.0.71f1/editor-type-48.bin")[..])
                .ok()
        }
        _ => None,
    }
}
