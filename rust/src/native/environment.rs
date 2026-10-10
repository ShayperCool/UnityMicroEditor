//! The pinned 6000.6.3f1 Linux standalone importer profile. Automatic Linux
//! graphics APIs were observed as OpenGLCore 17 and Vulkan 21. Project values
//! are read from exact copied source, never from an Editor cache.
use super::support::{nul, read_file, stat, text, wrap};
use crate::{
    ENGINE_VERSION,
    flags::FlagSet,
    project::{Value, parse_unity_yaml, value::get},
    protocol::{initialize, put_u32},
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
}

/// Bump this revision whenever the pinned importer wire environment changes.
pub const WORKER_ENVIRONMENT_REVISION: &str = "6000.6.3f1-linux-native-environment-v2";

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
        revision: WORKER_ENVIRONMENT_REVISION,
        sha256: fingerprint,
        environment: &environment,
    })?;
    data.push(b'\n');
    std::io::stdout().lock().write_all(&data)?;
    Ok(())
}

pub fn native_environment_fingerprint(project: &str, editor: &str) -> Result<String> {
    let environment = read_worker_environment(project)?;
    let data = util::marshal(&environment)?;
    let mut payload = format!("{WORKER_ENVIRONMENT_REVISION}\0{editor}\0").into_bytes();
    payload.extend_from_slice(&data);
    payload.extend_from_slice(&environment.message(16));
    Ok(sha256(&payload))
}

/// A missing PlayerSettings file is reported as `is_not_exist`.
pub fn read_worker_environment(project: &str) -> Result<WorkerEnvironment> {
    let filename = sysutil::join(&[project, "ProjectSettings/ProjectSettings.asset"]);
    let info = stat(&filename)?;
    if !info.is_regular() || info.size > 1 << 20 {
        bail!("native worker PlayerSettings is not bounded source text");
    }
    let data = read_file(&filename)?;
    let version = read_file(&sysutil::join(&[
        project,
        "ProjectSettings/ProjectVersion.txt",
    ]));
    let pinned = format!("m_EditorVersion: {ENGINE_VERSION}");
    let contains = |haystack: &[u8], needle: &str| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    if !version.is_ok_and(|version| {
        contains(&version, &format!("{pinned}\n")) || contains(&version, &format!("{pinned}\r\n"))
    }) {
        bail!("native worker environment requires pinned project Unity version");
    }
    let mut environment = parse_worker_environment(&data)?;
    environment.settings_sha256 = sha256(&data);
    Ok(environment)
}

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
        unity_version: ENGINE_VERSION.into(),
        build_target: 24,
        subtarget: 2,
        color_space: color,
        graphics_api_mask: 1 << 17 | 1 << 21,
        shader_precision_model: precision,
        ..Default::default()
    };
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

impl WorkerEnvironment {
    /// The Initialize (16) or Prepare (17) message carrying this environment.
    pub fn message(&self, kind: u8) -> Vec<u8> {
        let mut payload = initialize(self.build_target as i32);
        payload[0] = kind;
        put_u32(&mut payload, 8, self.subtarget);
        payload[24] = 1; // The pinned worker enables texture import compression.
        put_u32(&mut payload, 28, self.color_space);
        put_u32(&mut payload, 32, self.graphics_api_mask);
        put_u32(&mut payload, 36, 1); // Unity 6 latest scripting runtime.
        let keys = [
            "graphics/shader-precision-model",
            "graphics/metal-needs-renderpass-fbfetch-fallback",
            "graphics/texture-import-overrides",
            "graphics/default-texture-compression-format",
        ];
        put_u32(&mut payload, 44, 96 - 44);
        put_u32(&mut payload, 48, keys.len() as u32);
        payload.resize(96 + keys.len() * 24, 0);
        for (index, key) in keys.iter().enumerate() {
            let start = 96 + index * 24;
            let relative = (payload.len() - start) as u32;
            put_u32(&mut payload, start, relative);
            // Unity stores enum/bool+1 in the first Hash128 byte for these keys.
            // Linux cannot require the Metal renderpass fallback, hence false+1.
            payload[start + 8] = 1;
            if index == 0 {
                payload[start + 8] =
                    payload[start + 8].wrapping_add(self.shader_precision_model as u8);
            }
            // Native producer packs valid marker, optional log2(maxSize)+1, and
            // compression override. This coordinator explicitly uses no overrides.
            if index == 3 {
                payload[start + 8] =
                    payload[start + 8].wrapping_add(self.default_texture_compression_format as u8);
            }
            payload.extend_from_slice(key.as_bytes());
            payload.push(0);
        }
        payload
    }
}
