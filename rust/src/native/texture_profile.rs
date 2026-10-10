//! Cold TextureImporter delivery is proven for uncompressed ordinary textures,
//! including native resize/mips. Compression requires additional pinned native
//! compressor dependencies; until verified, only exact existing artifacts are used.
use super::{resolve_project_import_source, support::read_file};
use crate::{
    authoring::yamlcodec::{self, FLOAT, INT, Kind, MERGE, NULL, Node, Resolved},
    project::{Value, parse_unity_yaml, value::sprint},
    protocol::blob_string,
    sha256, sysutil,
    util::{file_hash, valid_sha256},
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashSet};

const EDITOR_SETTINGS: &str = "ProjectSettings/EditorSettings.asset";

#[derive(Default)]
struct Platform {
    target: String,
    overridden: i64,
    compression: Option<i64>,
    format: Option<i64>,
    crunched: i64,
}
#[derive(Default)]
struct Importer {
    texture_type: i64,
    texture_shape: i64,
    platforms: Vec<Platform>,
}

/// Typed decoding of the metadata fields this profile reads: scalar
/// resolution, null handling, duplicate fields and type errors.
struct Decoder<'a> {
    source: &'a str,
    errors: Vec<String>,
}
impl Decoder<'_> {
    fn line(&self, node: &Node) -> usize {
        self.source.as_bytes()[..node.start.min(self.source.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1
    }
    fn mismatch(&mut self, node: &Node, target: &str) {
        let mut value = String::new();
        if node.kind == Kind::Scalar && node.tag != yamlcodec::SEQ && node.tag != yamlcodec::MAP {
            let mut shown = node.value.clone();
            if shown.len() > 10 {
                let cut = (0..=7).rev().find(|&i| shown.is_char_boundary(i)).unwrap();
                shown = format!("{}...", &shown[..cut]);
            }
            value = format!(" `{shown}`");
        }
        self.errors.push(format!(
            "line {}: cannot unmarshal {}{value} into {target}",
            self.line(node),
            node.tag
        ));
    }
    fn is_null(node: &Node) -> bool {
        node.kind == Kind::Scalar && node.tag == NULL
    }
    /// An `int` field; `None` leaves the field untouched.
    fn int(&mut self, node: &Node) -> Option<i64> {
        if Self::is_null(node) {
            return None;
        }
        if node.kind == Kind::Scalar && (node.tag == INT || node.tag == FLOAT) {
            match yamlcodec::resolve_value(&node.value) {
                Resolved::Int(value) => return Some(value),
                Resolved::Uint(value) if value <= i64::MAX as u64 => return Some(value as i64),
                // Floats truncate; an out-of-range conversion is i64::MIN, as the recorded cases expect.
                Resolved::Float(value) if value <= i64::MAX as f64 => {
                    return Some(if value >= i64::MAX as f64 {
                        i64::MIN
                    } else {
                        value as i64
                    });
                }
                _ => {}
            }
        }
        self.mismatch(node, "int");
        None
    }
    fn string(&mut self, node: &Node) -> Option<String> {
        if Self::is_null(node) {
            return None;
        }
        if node.kind == Kind::Scalar {
            return Some(node.value.clone());
        }
        self.mismatch(node, "string");
        None
    }
    /// The fields of a mapping decoded into a struct, in document order.
    fn fields<'n>(
        &mut self,
        node: &'n Node,
        known: &[&'static str],
    ) -> Result<Vec<(&'static str, &'n Node)>> {
        // A mapping with repeated keys is refused before its fields are looked at.
        let pairs = node.content.as_chunks::<2>().0;
        let before = self.errors.len();
        for (index, first) in pairs.iter().enumerate() {
            for second in &pairs[index + 1..] {
                if first[0].kind == second[0].kind && first[0].value == second[0].value {
                    self.errors.push(format!(
                        "line {}: mapping key {} already defined at line {}",
                        self.line(&second[0]),
                        crate::jsondoc::quoted_text(&second[0].value),
                        self.line(&first[0])
                    ));
                }
            }
        }
        if self.errors.len() > before {
            return Ok(Vec::new());
        }
        let mut fields = Vec::new();
        for pair in pairs {
            let (key, value) = (&pair[0], &pair[1]);
            if key.tag == MERGE || key.anchored || value.anchored {
                bail!("yaml: merge keys and anchors are not supported in texture metadata");
            }
            if Self::is_null(key) {
                continue;
            }
            if key.kind != Kind::Scalar {
                self.mismatch(key, "string");
                continue;
            }
            let Some(name) = known.iter().find(|name| **name == key.value) else {
                continue;
            };
            fields.push((*name, value));
        }
        Ok(fields)
    }
    fn platform(&mut self, node: &Node) -> Result<Option<Platform>> {
        if Self::is_null(node) {
            return Ok(None);
        }
        if node.kind != Kind::Mapping {
            self.mismatch(node, "object");
            return Ok(None);
        }
        let mut platform = Platform::default();
        let known = [
            "buildTarget",
            "overridden",
            "textureCompression",
            "textureFormat",
            "crunchedCompression",
        ];
        for (name, value) in self.fields(node, &known)? {
            match name {
                "buildTarget" => {
                    if let Some(target) = self.string(value) {
                        platform.target = target;
                    }
                }
                "overridden" => {
                    if let Some(overridden) = self.int(value) {
                        platform.overridden = overridden;
                    }
                }
                "textureCompression" => {
                    if Self::is_null(value) {
                        platform.compression = None;
                    } else {
                        platform.compression = Some(self.int(value).unwrap_or_default());
                    }
                }
                "textureFormat" => {
                    if Self::is_null(value) {
                        platform.format = None;
                    } else {
                        platform.format = Some(self.int(value).unwrap_or_default());
                    }
                }
                _ => {
                    if let Some(crunched) = self.int(value) {
                        platform.crunched = crunched;
                    }
                }
            }
        }
        Ok(Some(platform))
    }
    fn importer(&mut self, node: &Node) -> Result<Option<Importer>> {
        if Self::is_null(node) {
            return Ok(None);
        }
        if node.kind != Kind::Mapping {
            self.mismatch(node, "object");
            // The pointer was allocated before its target failed to decode.
            return Ok(Some(Importer::default()));
        }
        let mut importer = Importer::default();
        let known = ["textureType", "textureShape", "platformSettings"];
        for (name, value) in self.fields(node, &known)? {
            match name {
                "textureType" => {
                    if let Some(kind) = self.int(value) {
                        importer.texture_type = kind;
                    }
                }
                "textureShape" => {
                    if let Some(shape) = self.int(value) {
                        importer.texture_shape = shape;
                    }
                }
                _ => {
                    if Self::is_null(value) {
                        importer.platforms.clear();
                    } else if value.kind != Kind::Sequence {
                        self.mismatch(value, "array");
                    } else {
                        importer.platforms.clear();
                        for item in &value.content {
                            if let Some(platform) = self.platform(item)? {
                                importer.platforms.push(platform);
                            }
                        }
                    }
                }
            }
        }
        Ok(Some(importer))
    }
}

fn texture_importer(data: &[u8]) -> Result<Option<Importer>> {
    let Ok(source) = std::str::from_utf8(data) else {
        bail!("yaml: invalid UTF-8");
    };
    let root = match yamlcodec::parse_tree(source) {
        Ok(root) => root,
        Err(error) if error.message == "empty YAML document" => return Ok(None),
        Err(error) => bail!("yaml: {}", error.message),
    };
    let mut decoder = Decoder {
        source,
        errors: Vec::new(),
    };
    let mut importer = None;
    if !Decoder::is_null(&root) {
        if root.kind != Kind::Mapping {
            decoder.mismatch(&root, "object");
        } else {
            for (_, value) in decoder.fields(&root, &["TextureImporter"])? {
                importer = decoder.importer(value)?;
            }
        }
    }
    if !decoder.errors.is_empty() {
        bail!("yaml: unmarshal errors:\n  {}", decoder.errors.join("\n  "));
    }
    Ok(importer)
}

pub fn native_png_needs_warm(project: &str, logical: &str) -> Result<bool> {
    if !sysutil::ext(logical).eq_ignore_ascii_case(".png") {
        return Ok(false);
    }
    let physical = resolve_project_import_source(project, &format!("{logical}.meta"))?;
    if !sysutil::stat(&physical).is_ok_and(|info| info.is_regular() && info.size <= 1 << 20) {
        bail!("native texture profile requires bounded source metadata");
    }
    let data = read_file(&physical)?;
    let importer = match texture_importer(&data)? {
        Some(importer)
            if importer.texture_type == 0
                && importer.texture_shape == 1
                && importer.platforms.len() <= 64 =>
        {
            importer
        }
        _ => return Ok(true),
    };
    let mut selected = None;
    let mut seen = HashSet::new();
    for (index, platform) in importer.platforms.iter().enumerate() {
        if platform.target.is_empty() || !seen.insert(platform.target.as_str()) {
            bail!("native texture metadata has a repeated or missing platform");
        }
        if platform.target == "DefaultTexturePlatform" {
            selected = Some(index);
        }
    }
    for (index, platform) in importer.platforms.iter().enumerate() {
        if platform.target == "Standalone" && platform.overridden != 0 {
            selected = Some(index);
        }
    }
    let Some(selected) = selected else {
        return Ok(true);
    };
    let platform = &importer.platforms[selected];
    Ok(platform.compression != Some(0) || platform.format != Some(-1) || platform.crunched != 0)
}

/// The compressor settings guard of a texture that needs the warm route; empty otherwise.
pub fn warm_texture_configuration(
    project: &str,
    logical: &str,
) -> Result<BTreeMap<String, String>> {
    if !native_png_needs_warm(project, logical)? {
        return Ok(BTreeMap::new());
    }
    let (sha, _) = read_bc7_compressor_settings(project)?;
    Ok(BTreeMap::from([(EDITOR_SETTINGS.to_string(), sha)]))
}

fn read_bc7_compressor_settings(project: &str) -> Result<(String, u8)> {
    let physical = resolve_project_import_source(project, EDITOR_SETTINGS)?;
    if !sysutil::stat(&physical).is_ok_and(|info| info.size <= 1 << 20) {
        bail!("texture compressor settings must be bounded source text");
    }
    let data = read_file(&physical)?;
    let documents = match parse_unity_yaml(&data) {
        Ok(documents)
            if documents.len() == 1
                && documents[0].class_id == 159
                && documents[0].type_name == "EditorSettings" =>
        {
            documents
        }
        _ => bail!("texture compressor requires one authored EditorSettings document"),
    };
    let bc7 = match documents[0].get("m_Bc7TextureCompressor") {
        // Pinned native EditorSettings::GetEffectiveBc7TextureCompressor maps
        // authored Default(0) to ISPC(2); other explicit enum values are kept.
        Value::Int(0) => 2,
        Value::Int(value @ 1..=2) => *value as u8,
        other => bail!(
            "unsupported pinned BC7 compressor setting: {}",
            sprint(other)
        ),
    };
    Ok((sha256(&data), bc7))
}

pub(crate) fn verify_warm_bc7_dependency(project: &str, dependencies: &[u8]) -> Result<()> {
    let (_, compressor) = read_bc7_compressor_settings(project)?;
    let actual = recorded_bc7_dependency(dependencies)?;
    let mut expected = [0u8; 16];
    expected[0] = 1;
    expected[1] = compressor;
    if actual != expected {
        bail!("warm native BC7 artifact was imported with different compressor settings");
    }
    Ok(())
}

/// The pinned ArtifactDB dependency blob contains 8-aligned 24-byte custom
/// dependency entries: relative string pointer, zero padding, Hash128. The
/// recorded key is matched through its pointer; an unlinked string occurrence
/// is no proof.
pub(crate) fn recorded_bc7_dependency(dependencies: &[u8]) -> Result<[u8; 16]> {
    if dependencies.len() < 128 || dependencies.len() > 1 << 20 {
        bail!("warm BC7 dependency blob exceeds pinned bounds");
    }
    let mut hash = [0u8; 16];
    let mut found = false;
    let mut field = 128;
    while field + 24 <= dependencies.len() {
        let entry = field;
        field += 8;
        if dependencies[entry + 4..entry + 8] != [0; 4] {
            continue;
        }
        if !blob_string(dependencies, entry).is_ok_and(|name| name == "graphics/bc7-compressor") {
            continue;
        }
        if found {
            bail!("warm BC7 compressor dependency is ambiguous");
        }
        hash.copy_from_slice(&dependencies[entry + 8..entry + 24]);
        found = true;
    }
    if !found || hash == [0; 16] {
        bail!("warm BC7 artifact lacks its recorded compressor dependency");
    }
    Ok(hash)
}

pub fn validate_native_configuration(
    project: &str,
    source: &str,
    configuration: &BTreeMap<String, String>,
) -> Result<()> {
    let expected = warm_texture_configuration(project, source)?;
    let guard =
        |map: &BTreeMap<String, String>| map.get(EDITOR_SETTINGS).cloned().unwrap_or_default();
    if !expected.is_empty() && guard(configuration) != guard(&expected) {
        bail!("native compressed texture cache lacks the current compressor settings guard");
    }
    for (logical, expected) in configuration {
        if logical != EDITOR_SETTINGS || !valid_sha256(expected) {
            bail!("unsupported native configuration source guard");
        }
        let physical = resolve_project_import_source(project, logical)?;
        if !file_hash(&physical).is_ok_and(|actual| actual == *expected) {
            bail!("native texture compressor settings changed");
        }
    }
    Ok(())
}
