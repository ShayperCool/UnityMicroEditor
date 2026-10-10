//! The Shader source/include guard and the descriptor shapes the two shader
//! commands write. `crate::project` reads these descriptors with its own
//! equivalent types.
use super::{files, lower_hex};
use anyhow::{Context, Result, anyhow, bail};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    sync::OnceLock,
};

pub const BUILTIN_RESOURCE_GUID: &str = "0000000000000000f000000000000000";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShaderSourceRequirement {
    pub path: String,
    pub source_path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeContentObject {
    pub file_id: String,
    pub type_name: String,
}

/// NativeContentFile as the shader commands emit it: one Shader object and
/// empty (non-null) resource and dependency lists.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NativeContentFile {
    pub guid: String,
    pub path: String,
    pub sha256: String,
    pub objects: Vec<NativeContentObject>,
    pub resources: Vec<serde_json::Value>,
    pub dependencies: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompiledShaderSnapshot {
    pub schema_version: i64,
    pub compiled_source_revision_verified: bool,
    pub shader_name: String,
    pub source_path: String,
    pub archive_file_id: String,
    pub bundle_path: String,
    pub bundle_sha256: String,
    pub member_path: String,
    pub member_name: String,
    pub member_sha256: String,
    pub payload_sha256: String,
    pub type_tree_sha256: String,
    pub source_requirements: Vec<ShaderSourceRequirement>,
    pub missing_includes: Vec<String>,
}

/// NativeReplacement restricted to the fields a compiled Shader snapshot sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeReplacement {
    pub guid: String,
    pub source_path: String,
    pub source_sha256: String,
    pub meta_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub environment_sha256: String,
    pub content: NativeContentFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled_shader_snapshot: Option<CompiledShaderSnapshot>,
}

/// Records an immutable engine shader snapshot separately from authored
/// project imports. Its source object is verified in native engine resources;
/// its GPU payload comes from an exact compiled Player archive.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedBuiltinShader {
    pub schema_version: i64,
    pub unity_version: String,
    pub guid: String,
    pub file_id: String,
    pub shader_name: String,
    pub source_guid: String,
    pub source_file_id: String,
    pub archive_file_id: String,
    pub bundle_path: String,
    pub bundle_sha256: String,
    pub member_path: String,
    pub member_name: String,
    pub member_sha256: String,
    pub payload_sha256: String,
    pub type_tree_sha256: String,
    pub builtin_source_path: String,
    #[serde(rename = "builtinSourcePayloadSha256")]
    pub builtin_source_sha256: String,
    pub compiled_source_revision_verified: bool,
    pub content: NativeContentFile,
}

fn valid_native_id(id: &str) -> bool {
    id.parse::<i64>()
        .is_ok_and(|value| value != 0 && value.to_string() == id)
}

impl RetainedBuiltinShader {
    pub fn validate(&self, engine: &str) -> Result<()> {
        if self.schema_version != 1
            || self.compiled_source_revision_verified
            || engine != "6000.6.3f1"
            || self.unity_version != engine
            || self.guid != BUILTIN_RESOURCE_GUID
            || self.file_id != "108"
            || self.shader_name != "Skybox/Panoramic"
            || self.source_guid != "f57c2bc4f17ba48d9b9b6758dcfe812d"
            || self.source_file_id != "4800000"
            || !valid_native_id(&self.archive_file_id)
            || self.member_name.is_empty()
        {
            bail!("unsupported retained built-in Shader identity/version");
        }
        for (path, sha) in [
            (&self.bundle_path, &self.bundle_sha256),
            (&self.member_path, &self.member_sha256),
            (&self.builtin_source_path, &self.builtin_source_sha256),
            (&self.content.path, &self.content.sha256),
        ] {
            if !files::is_normalized_absolute(path) || !lower_hex(sha, 64) {
                bail!("retained built-in Shader requires exact native paths/hashes");
            }
        }
        if !lower_hex(&self.payload_sha256, 64)
            || !lower_hex(&self.type_tree_sha256, 64)
            || self.content.guid != self.guid
            || self.content.objects.len() != 1
            || self.content.objects[0].file_id != self.file_id
            || self.content.objects[0].type_name != "UnityEngine.Shader"
            || !self.content.resources.is_empty()
            || !self.content.dependencies.is_empty()
        {
            bail!("retained built-in Shader content must contain only its exact Shader object");
        }
        Ok(())
    }
}

fn include_directive() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?m-u)^\s*#\s*(?:include|include_with_pragmas)\b([^\r\n]*)").unwrap()
    })
}
fn include_literal() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"(?-u)^\s*(?:"([^"\r\n]+)"|<([^>\r\n]+)>)\s*$"#).unwrap())
}

fn valid_shader_source(logical: &str) -> bool {
    files::normalize(logical) == logical
        && !logical.contains('\\')
        && (logical.starts_with("Assets/") || logical.starts_with("Packages/"))
}

enum Resolve {
    Missing(anyhow::Error),
    Failed(anyhow::Error),
}

fn shader_source_path(project: &str, logical: &str) -> Result<String, Resolve> {
    if !valid_shader_source(logical) {
        return Err(Resolve::Failed(anyhow!(
            "Shader source/include path is invalid: {logical}"
        )));
    }
    let mut resolved = files::canonical(&format!("{project}/{logical}"));
    let missing = |result: &io::Result<String>| matches!(result, Err(error) if error.kind() == io::ErrorKind::NotFound);
    if missing(&resolved) && logical.starts_with("Packages/") {
        let parts: Vec<&str> = logical.split('/').collect();
        if parts.len() < 3 || parts[1].contains(['*', '?', '[']) {
            return Err(Resolve::Failed(anyhow!(
                "Shader package path is incomplete or invalid: {logical}"
            )));
        }
        let cache = format!("{project}/Library/PackageCache");
        let prefix = format!("{}@", parts[1]);
        let candidates: Vec<String> = fs::read_dir(&cache)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                    .filter(|name| name.starts_with(&prefix))
                    .collect()
            })
            .unwrap_or_default();
        let candidate = match &candidates[..] {
            [] => return Err(Resolve::Missing(anyhow!("file does not exist"))),
            [candidate] => candidate,
            _ => {
                return Err(Resolve::Failed(anyhow!(
                    "Shader package source is ambiguous: {logical}"
                )));
            }
        };
        resolved = files::canonical(&format!("{cache}/{candidate}/{}", parts[2..].join("/")));
    }
    let canonical = match resolved {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Resolve::Missing(error.into()));
        }
        Err(error) => return Err(Resolve::Failed(error.into())),
    };
    if !files::within(project, &canonical) {
        return Err(Resolve::Failed(anyhow!(
            "Shader include leaves current project: {logical}"
        )));
    }
    Ok(canonical)
}

fn read_shader_source(physical: &str) -> Result<Vec<u8>> {
    files::read_bounded(physical, 4 << 20)
        .context("Shader source/include cannot be read or exceeds supported size")
}

/// Preserves line boundaries and quoted paths while removing comments. A regex
/// comment stripper would mistake // inside a quoted string for a comment.
fn shader_without_comments(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = input.to_vec();
    let (mut quoted, mut block, mut line) = (false, false, false);
    let mut i = 0;
    while i < out.len() {
        let c = out[i];
        if line {
            if c == b'\n' || c == b'\r' {
                line = false;
            } else {
                out[i] = b' ';
            }
        } else if block {
            if c == b'*' && i + 1 < out.len() && out[i + 1] == b'/' {
                (out[i], out[i + 1]) = (b' ', b' ');
                block = false;
                i += 1;
            } else if c != b'\n' && c != b'\r' {
                out[i] = b' ';
            }
        } else if quoted {
            if c == b'\\' && i + 1 < out.len() {
                i += 1;
            } else if c == b'"' {
                quoted = false;
            }
        } else if c == b'"' {
            quoted = true;
        } else if c == b'/' && i + 1 < out.len() && (out[i + 1] == b'*' || out[i + 1] == b'/') {
            (block, line) = (out[i + 1] == b'*', out[i + 1] == b'/');
            (out[i], out[i + 1]) = (b' ', b' ');
            i += 1;
        }
        i += 1;
    }
    if block {
        bail!("unterminated Shader source block comment");
    }
    Ok(out)
}

struct Closure<'a> {
    project: &'a str,
    source: &'a str,
    seen: BTreeSet<String>,
    missing: BTreeSet<String>,
    files: BTreeMap<String, ShaderSourceRequirement>,
}

impl Closure<'_> {
    fn visit(&mut self, logical: &str) -> Result<()> {
        if self.seen.contains(logical) {
            return Ok(());
        }
        if self.seen.len() >= 2048 {
            bail!("Shader include closure exceeds the supported size");
        }
        self.seen.insert(logical.to_string());
        let physical = match shader_source_path(self.project, logical) {
            Ok(physical) => physical,
            Err(Resolve::Missing(_)) if logical != self.source => {
                self.missing.insert(logical.to_string());
                return Ok(());
            }
            Err(Resolve::Missing(error) | Resolve::Failed(error)) => {
                bail!("resolve Shader source/include {logical}: {error}")
            }
        };
        let data = read_shader_source(&physical)?;
        self.files.insert(
            logical.to_string(),
            ShaderSourceRequirement {
                path: logical.to_string(),
                source_path: physical,
                sha256: crate::sha256(&data),
            },
        );
        let cleaned = shader_without_comments(&data)
            .map_err(|error| anyhow!("read Shader includes {logical}: {error}"))?;
        for directive in include_directive().captures_iter(&cleaned) {
            let rest = directive.get(1).map_or(&b""[..], |found| found.as_bytes());
            let Some(literal) = include_literal().captures(rest) else {
                bail!(
                    "Shader snapshot requires a literal include in {logical}: {}",
                    String::from_utf8_lossy(rest).trim()
                );
            };
            let name = literal
                .get(1)
                .or_else(|| literal.get(2))
                .map_or(&b""[..], |found| found.as_bytes());
            let Ok(name) = std::str::from_utf8(name) else {
                bail!("Shader include path is not valid UTF-8 in {logical}");
            };
            if name.contains('\\') || name.starts_with('/') {
                bail!("Shader include path is unsupported in {logical}: {name}");
            }
            let mut included = files::normalize(name);
            if !included.starts_with("Assets/") && !included.starts_with("Packages/") {
                included = files::normalize(&format!("{}/{name}", files::parent(logical)));
            }
            if !valid_shader_source(&included) {
                bail!("Shader include escapes source roots in {logical}: {name}");
            }
            self.visit(&included)?;
        }
        Ok(())
    }
}

/// Snapshots all literal includes, including conditional branches. Missing
/// optional includes are recorded as absent and must remain absent. This is a
/// guard on later edits, not build provenance.
pub fn collect_shader_source_requirements(
    project: &str,
    source: &str,
) -> Result<(Vec<ShaderSourceRequirement>, Vec<String>)> {
    if !project.starts_with('/') {
        bail!("Shader source project must be an absolute path: {project}");
    }
    let canonical =
        files::canonical(project).with_context(|| format!("resolve project {project}"))?;
    if !valid_shader_source(source)
        || !source.starts_with("Assets/")
        || !source.ends_with(".shader")
    {
        bail!("Shader source path is invalid: {source}");
    }
    let mut closure = Closure {
        project: &canonical,
        source,
        seen: BTreeSet::new(),
        missing: BTreeSet::new(),
        files: BTreeMap::new(),
    };
    closure.visit(source)?;
    Ok((
        closure.files.into_values().collect(),
        closure.missing.into_iter().collect(),
    ))
}

pub fn validate_shader_source_requirements(
    project: &str,
    source: &str,
    expected: &[ShaderSourceRequirement],
    missing: &[String],
) -> Result<()> {
    let (actual, absent) = collect_shader_source_requirements(project, source)?;
    let mut owned = expected.to_vec();
    owned.sort_by(|left, right| left.path.cmp(&right.path));
    if actual != owned {
        bail!("compiled Shader snapshot source/include closure changed or was incomplete");
    }
    let mut wanted = missing.to_vec();
    wanted.sort();
    if absent != wanted {
        bail!("compiled Shader snapshot optional include availability changed");
    }
    Ok(())
}
