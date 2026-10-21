//! The Linux standalone import environment of a project, which the worker's
//! Initialize message carries (`engine`). Automatic Linux graphics APIs were
//! observed as OpenGLCore 17 and Vulkan 21; of them a project that lists none
//! is imported for OpenGLCore alone, the API the Player runs on. Project
//! values are read from exact copied source, never from an Editor cache.
use super::support::{nul, read_file, stat, text, wrap};
use crate::{
    engine,
    flags::FlagSet,
    project::{Value, parse_unity_yaml, value::get},
    sha256, sysutil, util,
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io::Write};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerEnvironment {
    #[serde(deserialize_with = "nul")]
    pub unity_version: String,
    #[serde(deserialize_with = "nul")]
    pub settings_sha256: String,
    #[serde(deserialize_with = "nul")]
    pub build_target: u32,
    #[serde(deserialize_with = "nul")]
    pub subtarget: u32,
    #[serde(deserialize_with = "nul")]
    pub color_space: u32,
    #[serde(deserialize_with = "nul")]
    pub graphics_api_mask: u32,
    #[serde(deserialize_with = "nul")]
    pub shader_precision_model: u32,
    #[serde(deserialize_with = "nul")]
    pub texture_max_size_override: u32,
    #[serde(deserialize_with = "nul")]
    pub texture_compression_override: u32,
    #[serde(deserialize_with = "nul")]
    pub default_texture_compression_format: u32,
    /// `EditorSettings::GetEffectiveBc7TextureCompressor`: 1 BC7E, 2 ISPC.
    #[serde(deserialize_with = "nul")]
    pub bc7_compressor: u32,
    /// 1 when sprite atlases are importer-based (`m_SpritePackerMode: 5`).
    #[serde(deserialize_with = "nul")]
    pub sprite_atlas_v2: u32,
    /// SHA-256 of `ProjectSettings/EditorSettings.asset`; empty without one.
    #[serde(deserialize_with = "nul")]
    pub editor_settings_sha256: String,
}

pub fn run_native_environment_cli(args: &[String]) -> Result<()> {
    let mut flags = FlagSet::new("native-environment");
    flags.string("project", ".", "source Unity project root");
    flags.string(
        "editor",
        &util::default_unity_editor(),
        "pinned Unity executable",
    );
    flags.parse(args.iter().cloned())?;
    if !flags.args().is_empty() {
        bail!("use native-environment --project <source> [--editor <pinned Unity executable>]");
    }
    let (project, editor) = (flags.get_string("project"), flags.get_string("editor"));
    let fingerprint = native_environment_fingerprint(&project, &editor)?;
    let environment = read_worker_environment(&project)?;
    #[derive(Serialize)]
    struct Output<'a> {
        revision: &'a str,
        #[serde(rename = "environmentSha256")]
        sha256: String,
        environment: &'a WorkerEnvironment,
    }
    let mut data = util::marshal(&Output {
        revision: engine::supported(&environment.unity_version)?.environment_revision,
        sha256: fingerprint,
        environment: &environment,
    })?;
    data.push(b'\n');
    std::io::stdout().lock().write_all(&data)?;
    Ok(())
}

pub fn native_environment_fingerprint(project: &str, editor: &str) -> Result<String> {
    let environment = read_worker_environment(project)?;
    let engine = engine::supported(&environment.unity_version)?;
    let data = util::marshal(&environment)?;
    let mut payload = format!("{}\0{editor}\0", engine.environment_revision).into_bytes();
    payload.extend_from_slice(&data);
    payload.extend_from_slice(&engine.worker.initialize(Some(&environment)));
    Ok(sha256(&payload))
}

/// A missing PlayerSettings file is reported as `is_not_exist`.
pub fn read_worker_environment(project: &str) -> Result<WorkerEnvironment> {
    let filename = sysutil::join(&[project, "ProjectSettings/ProjectSettings.asset"]);
    let info = stat(&filename)?;
    if !info.is_regular() || info.size > 1 << 20 {
        bail!("native worker PlayerSettings is not bounded source text");
    }
    // The settings as a context holds them, so that the project and a
    // context made from it name one environment.
    let data = context_player_settings(&read_file(&filename)?);
    // The project's release selects the worker's protocol.
    let version = engine::project_version(project)
        .map_err(|error| anyhow!("native worker environment: {error:#}"))?;
    let engine = engine::supported(&version)
        .map_err(|error| anyhow!("native worker environment: {error:#}"))?;
    engine.worker.check_project(project)?;
    let mut environment = parse_worker_environment(&data)?;
    environment.unity_version = engine.version.into();
    environment.settings_sha256 = sha256(&data);
    let editor = read_editor_settings(project)?;
    environment.bc7_compressor = editor.bc7_compressor;
    environment.sprite_atlas_v2 = editor.sprite_atlas_v2;
    environment.editor_settings_sha256 = editor.sha256;
    Ok(environment)
}

/// What an import takes from `ProjectSettings/EditorSettings.asset`.
pub(crate) struct EditorImportSettings {
    /// Empty when the project has no such file.
    pub sha256: String,
    pub bc7_compressor: u32,
    pub sprite_atlas_v2: u32,
}

pub(crate) const EDITOR_SETTINGS: &str = "ProjectSettings/EditorSettings.asset";

/// A project without the file gets the values the Editor writes into a new
/// one: the default BC7 compressor and no sprite packing.
pub(crate) fn read_editor_settings(project: &str) -> Result<EditorImportSettings> {
    let filename = sysutil::join(&[project, EDITOR_SETTINGS]);
    let info = match stat(&filename) {
        Ok(info) => info,
        Err(error) if super::support::is_not_exist(&error) => {
            return Ok(EditorImportSettings {
                sha256: String::new(),
                bc7_compressor: BC7_ISPC,
                sprite_atlas_v2: 0,
            });
        }
        Err(error) => return Err(error),
    };
    if !info.is_regular() || info.size > 1 << 20 {
        bail!("native worker EditorSettings is not bounded source text");
    }
    let data = read_file(&filename)?;
    let documents = match parse_unity_yaml(&data) {
        Ok(documents)
            if documents.len() == 1
                && documents[0].class_id == 159
                && documents[0].type_name == "EditorSettings" =>
        {
            documents
        }
        _ => bail!("native worker requires one authored EditorSettings document"),
    };
    let settings = documents[0].fields();
    let bc7_compressor = match get(settings, "m_Bc7TextureCompressor") {
        // `EditorSettings::GetEffectiveBc7TextureCompressor` reads the
        // authored Default (0) as ISPC; explicit values are kept.
        Value::Int(0) | Value::Null => BC7_ISPC,
        Value::Int(value @ 1..=2) => *value as u32,
        other => bail!(
            "unsupported pinned BC7 compressor setting: {}",
            crate::project::value::sprint(other)
        ),
    };
    let sprite_atlas_v2 = match get(settings, "m_SpritePackerMode") {
        Value::Int(5) => 1,
        Value::Int(0..=6) | Value::Null => 0,
        other => bail!(
            "unsupported sprite packer mode: {}",
            crate::project::value::sprint(other)
        ),
    };
    Ok(EditorImportSettings {
        sha256: sha256(&data),
        bc7_compressor,
        sprite_atlas_v2,
    })
}

/// `EditorSettings::Bc7TextureCompressor::Ispc`.
const BC7_ISPC: u32 = 2;

pub fn parse_worker_environment(data: &[u8]) -> Result<WorkerEnvironment> {
    if data.len() > 1 << 20 {
        bail!("native worker PlayerSettings exceeds supported bounds");
    }
    let documents = parse_unity_yaml(data).map_err(|error| {
        wrap(
            "native worker requires one authored PlayerSettings document",
            anyhow!(text(&error)),
        )
    })?;
    if documents.len() != 1
        || documents[0].class_id != 129
        || documents[0].type_name != "PlayerSettings"
    {
        bail!("native worker requires one authored PlayerSettings document");
    }
    let settings = documents[0].fields();
    let color = match get(settings, "m_ActiveColorSpace") {
        Value::Int(color @ 0..=1) => *color as u32,
        _ => bail!("native worker PlayerSettings color space is missing or unsupported"),
    };
    let precision = match get(settings, "shaderPrecisionModel") {
        Value::Int(precision @ 0..=1) => *precision as u32,
        _ => bail!("native worker PlayerSettings shader precision is missing or unsupported"),
    };
    let mut environment = WorkerEnvironment {
        unity_version: crate::ENGINE_VERSION.into(),
        build_target: 24,
        subtarget: 2,
        color_space: color,
        graphics_api_mask: 1 << 17,
        shader_precision_model: precision,
        bc7_compressor: BC7_ISPC,
        ..Default::default()
    };
    // The encodings a texture importer asks for are sent as the standalone
    // defaults; a project that sets one for the standalone target was not
    // compared with the Editor.
    for (field, what) in [
        ("m_BuildTargetNormalMapEncoding", "normal map encoding"),
        (
            "m_BuildTargetGroupLightmapEncodingQuality",
            "lightmap encoding quality",
        ),
        (
            "m_BuildTargetGroupHDRCubemapEncodingQuality",
            "HDR cubemap encoding quality",
        ),
    ] {
        let entries = match get(settings, field) {
            Value::Array(entries) if entries.len() <= 64 => entries,
            Value::Null => continue,
            _ => bail!("native worker {what} settings are unsupported"),
        };
        for item in entries {
            match item.field("m_BuildTarget") {
                Value::String(name) if name != "Standalone" => {}
                _ => bail!("native worker requires the default standalone {what}"),
            }
        }
    }
    let apis = match get(settings, "m_BuildTargetGraphicsAPIs") {
        Value::Array(apis) if apis.len() <= 64 => apis,
        _ => {
            bail!("native worker PlayerSettings graphics API overrides are missing or unsupported")
        }
    };
    let mut seen = HashSet::new();
    for item in apis {
        let name = match item.field("m_BuildTarget") {
            Value::String(name) if item.as_object().is_some() && !name.is_empty() => name,
            _ => bail!("native worker PlayerSettings graphics target is invalid or repeated"),
        };
        if !seen.insert(name.as_str()) {
            bail!("native worker PlayerSettings graphics target is invalid or repeated");
        }
        if name != "LinuxStandaloneSupport" {
            continue;
        }
        let automatic = match item.field("m_Automatic") {
            Value::Int(automatic @ 0..=1) => *automatic,
            _ => bail!("native worker Linux graphics automatic setting is invalid"),
        };
        if automatic == 1 {
            continue;
        }
        let raw = match item.field("m_APIs") {
            Value::String(raw) if raw.ends_with('i') && raw.len() <= 8 * 8 + 1 => raw,
            _ => bail!("native worker Linux graphics API encoding is unsupported"),
        };
        let bytes = match decode_hex(&raw[..raw.len() - 1]) {
            Some(bytes) if !bytes.is_empty() && bytes.len().is_multiple_of(4) => bytes,
            _ => bail!("native worker Linux graphics API encoding is invalid"),
        };
        environment.graphics_api_mask = 0;
        for chunk in bytes.as_chunks::<4>().0 {
            let api = u32::from_le_bytes(*chunk);
            if api != 17 && api != 21 || environment.graphics_api_mask & (1 << api) != 0 {
                bail!("native worker Linux graphics API is unsupported or repeated");
            }
            environment.graphics_api_mask |= 1 << api;
        }
    }
    let formats = match get(settings, "m_BuildTargetDefaultTextureCompressionFormat") {
        Value::Array(formats) if formats.len() <= 64 => formats,
        _ => bail!("native worker default texture compression settings are missing or unsupported"),
    };
    let mut seen = HashSet::new();
    for item in formats {
        let name = match item.field("m_BuildTarget") {
            Value::String(name) if item.as_object().is_some() && !name.is_empty() => name,
            _ => bail!("native worker default texture compression target is invalid or repeated"),
        };
        if !seen.insert(name.as_str()) {
            bail!("native worker default texture compression target is invalid or repeated");
        }
        if name != "Standalone" && name != "LinuxStandaloneSupport" {
            continue;
        }
        if !matches!(item.field("m_Formats"), Value::String(raw) if raw == "00000000i") {
            bail!(
                "native worker Linux default texture compression requires the verified DXTC profile"
            );
        }
    }
    Ok(environment)
}

/// `ProjectSettings.asset` as an import context holds it. A build inside a
/// worker compiles shaders for the APIs the worker's own Player settings
/// list, and automatic Linux APIs are OpenGLCore and Vulkan. The Player
/// runs on OpenGLCore, so a project that leaves the choice to the engine is
/// given that one API; a list of the project's own stays as written.
pub(crate) fn context_player_settings(data: &[u8]) -> Vec<u8> {
    const KEY: &str = "m_BuildTargetGraphicsAPIs:";
    let Ok(text) = std::str::from_utf8(data) else {
        return data.to_vec();
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let depth = |line: &str| line.len() - line.trim_start().len();
    let ending = |line: &str| if line.ends_with("\r\n") { "\r\n" } else { "\n" };
    let Some(key) = lines.iter().position(|line| {
        line.trim()
            .strip_prefix(KEY)
            .is_some_and(|rest| matches!(rest.trim(), "" | "[]"))
    }) else {
        return data.to_vec();
    };
    let (indent, end) = (depth(lines[key]), ending(lines[key]));
    let pad = " ".repeat(indent);
    let entry = format!(
        "{pad}- m_BuildTarget: LinuxStandaloneSupport{end}{pad}  m_APIs: 11000000i{end}{pad}  m_Automatic: 0{end}"
    );
    // The entries of the list, each from its dash to the next.
    let mut last = key + 1;
    let mut linux = None;
    let mut index = key + 1;
    while index < lines.len() {
        let body = lines[index].trim();
        if !body.is_empty() {
            let at = depth(lines[index]);
            if at < indent || at == indent && !body.starts_with('-') {
                break;
            }
            if at == indent && body == "- m_BuildTarget: LinuxStandaloneSupport" {
                linux = Some(index);
            }
            last = index + 1;
        }
        index += 1;
    }
    let mut out = String::with_capacity(text.len() + entry.len());
    match linux {
        Some(start) => {
            let stop = (start + 1..last)
                .find(|&line| depth(lines[line]) == indent && !lines[line].trim().is_empty())
                .unwrap_or(last);
            if !lines[start..stop]
                .iter()
                .any(|line| line.trim() == "m_Automatic: 1")
            {
                return data.to_vec();
            }
            out.extend(lines[..start].iter().copied());
            out.push_str(&entry);
            out.extend(lines[stop..].iter().copied());
        }
        None => {
            out.extend(lines[..key].iter().copied());
            out.push_str(&format!("{pad}{KEY}{end}"));
            out.extend(lines[key + 1..last].iter().copied());
            out.push_str(&entry);
            out.extend(lines[last..].iter().copied());
        }
    }
    out.into_bytes()
}

/// Decodes a hex string; `None` for an odd length or a non-hex digit.
pub(crate) fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            Some((high << 4 | low) as u8)
        })
        .collect()
}
