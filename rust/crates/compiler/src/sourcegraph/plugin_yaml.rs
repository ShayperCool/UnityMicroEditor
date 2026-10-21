//! Typed decoding of plugin metadata YAML: scalar resolution, null handling,
//! repeated keys and type errors with their line numbers.
//!
//! Unsupported and refused explicitly: aliases, merge keys and explicit tags.
use crate::{
    authoring::yamlcodec::{self, FLOAT, INT, Kind, MERGE, NULL, Node, Resolved},
    compiler::NilVec,
};
use regex::bytes::Regex;
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Platform {
    pub first: BTreeMap<String, String>,
    pub enabled: i64,
    pub settings: BTreeMap<String, String>,
}
/// The `PluginImporter` mapping of a plugin `.meta` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Plugin {
    pub constraints: NilVec<String>,
    pub explicit: i64,
    pub platforms: Vec<Platform>,
}

// Unity's legacy PluginImporter emits an unquoted empty mapping key. Its
// public text serialization accepts that spelling; the YAML parser requires quotes.
static EMPTY_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m-u)^([ \t]+):([ \t]+[^\r\n]*)$").unwrap());

const UNPORTED: &str = "yaml: merge keys and explicit tags are not supported in plugin metadata";

struct Decoder<'a> {
    source: &'a str,
    errors: Vec<String>,
}
type Decoded<T> = Result<T, String>;
impl<'a> Decoder<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            errors: Vec::new(),
        }
    }
    fn finish(self) -> Decoded<()> {
        if self.errors.is_empty() {
            return Ok(());
        }
        Err(format!(
            "yaml: unmarshal errors:\n  {}",
            self.errors.join("\n  ")
        ))
    }
    fn line(&self, node: &Node) -> usize {
        self.source.as_bytes()[..node.start.min(self.source.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1
    }
    fn mismatch(&mut self, node: &Node, target: &str) {
        let mut value = String::new();
        if node.kind == Kind::Scalar {
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
    fn null(node: &Node) -> bool {
        node.kind == Kind::Scalar && node.tag == NULL
    }
    fn ported(node: &Node) -> Decoded<()> {
        if node.tagged {
            return Err(UNPORTED.into());
        }
        Ok(())
    }
    /// A `string` target; `None` leaves it untouched.
    fn string(&mut self, node: &Node) -> Decoded<Option<String>> {
        Self::ported(node)?;
        if Self::null(node) {
            return Ok(None);
        }
        if node.kind == Kind::Scalar {
            return Ok(Some(node.value.clone()));
        }
        self.mismatch(node, "string");
        Ok(None)
    }
    /// An `int` target; `None` leaves it untouched.
    fn int(&mut self, node: &Node) -> Decoded<Option<i64>> {
        Self::ported(node)?;
        if Self::null(node) {
            return Ok(None);
        }
        if node.kind == Kind::Scalar && (node.tag == INT || node.tag == FLOAT) {
            match yamlcodec::resolve_value(&node.value) {
                Resolved::Int(value) => return Ok(Some(value)),
                Resolved::Uint(value) if value <= i64::MAX as u64 => return Ok(Some(value as i64)),
                // Floats truncate; an out-of-range conversion is i64::MIN, as the recorded cases expect.
                Resolved::Float(value) if value <= i64::MAX as f64 => {
                    return Ok(Some(
                        if value >= i64::MAX as f64 || value < i64::MIN as f64 {
                            i64::MIN
                        } else {
                            value as i64
                        },
                    ));
                }
                _ => {}
            }
        }
        self.mismatch(node, "int");
        Ok(None)
    }
    /// The pairs of a mapping, or `None` when its repeated keys are refused.
    fn pairs<'n>(&mut self, node: &'n Node) -> Option<Vec<(&'n Node, &'n Node)>> {
        let pairs: Vec<(&Node, &Node)> = node
            .content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (&pair[0], &pair[1]))
            .collect();
        let before = self.errors.len();
        for (index, (first, _)) in pairs.iter().enumerate() {
            for (second, _) in &pairs[index + 1..] {
                if first.kind == second.kind && first.value == second.value {
                    self.errors.push(format!(
                        "line {}: mapping key {} already defined at line {}",
                        self.line(second),
                        crate::jsondoc::quoted_text(&second.value),
                        self.line(first)
                    ));
                }
            }
        }
        (self.errors.len() == before).then_some(pairs)
    }
    /// The known fields of a mapping decoded into a struct, in document order.
    fn fields<'n>(
        &mut self,
        node: &'n Node,
        known: &[&'static str],
    ) -> Decoded<Vec<(&'static str, &'n Node)>> {
        let mut fields = Vec::new();
        for (key, value) in self.pairs(node).unwrap_or_default() {
            if key.kind == Kind::Scalar && key.tag == MERGE {
                return Err(UNPORTED.into());
            }
            let Some(name) = self.string(key)? else {
                continue;
            };
            if let Some(name) = known.iter().find(|known| **known == name) {
                fields.push((*name, value));
            }
        }
        Ok(fields)
    }
    /// A string-to-string map target: `Some(None)` is the absent map of a null
    /// node, `None` leaves the target untouched.
    fn string_map(&mut self, node: &Node) -> Decoded<Option<Option<BTreeMap<String, String>>>> {
        Self::ported(node)?;
        if Self::null(node) {
            return Ok(Some(None));
        }
        if node.kind != Kind::Mapping {
            self.mismatch(node, "object");
            return Ok(None);
        }
        let Some(pairs) = self.pairs(node) else {
            return Ok(None);
        };
        let mut map = BTreeMap::new();
        for (key, value) in pairs {
            if key.kind == Kind::Scalar && key.tag == MERGE {
                return Err(UNPORTED.into());
            }
            let Some(key) = self.string(key)? else {
                continue;
            };
            let null = Self::null(value);
            match self.string(value)? {
                Some(text) => {
                    map.insert(key, text);
                }
                None if null => {
                    map.insert(key, String::new());
                }
                None => {}
            }
        }
        Ok(Some(Some(map)))
    }
    /// The anonymous `Second` struct of a platform.
    fn second(&mut self, node: &Node, platform: &mut Platform) -> Decoded<()> {
        Self::ported(node)?;
        if Self::null(node) {
            return Ok(());
        }
        if node.kind != Kind::Mapping {
            self.mismatch(node, "object");
            return Ok(());
        }
        for (name, value) in self.fields(node, &["enabled", "settings"])? {
            if name == "enabled" {
                if let Some(enabled) = self.int(value)? {
                    platform.enabled = enabled;
                }
            } else if let Some(settings) = self.string_map(value)? {
                platform.settings = settings.unwrap_or_default();
            }
        }
        Ok(())
    }
    fn platform(&mut self, node: &Node) -> Decoded<Option<Platform>> {
        Self::ported(node)?;
        if Self::null(node) {
            return Ok(None);
        }
        if node.kind != Kind::Mapping {
            self.mismatch(node, "sourcePluginPlatform");
            return Ok(None);
        }
        let mut platform = Platform::default();
        for (name, value) in self.fields(node, &["first", "second"])? {
            if name == "first" {
                if let Some(first) = self.string_map(value)? {
                    platform.first = first.unwrap_or_default();
                }
            } else {
                self.second(value, &mut platform)?;
            }
        }
        Ok(Some(platform))
    }
    /// A list-of-strings target.
    fn strings(&mut self, node: &Node) -> Decoded<Option<NilVec<String>>> {
        Self::ported(node)?;
        if Self::null(node) {
            return Ok(Some(NilVec::nil()));
        }
        if node.kind != Kind::Sequence {
            self.mismatch(node, "array");
            return Ok(None);
        }
        let mut items = NilVec::make();
        for item in &node.content {
            if let Some(text) = self.string(item)? {
                items.push(text);
            }
        }
        Ok(Some(items))
    }
}

fn platforms(source: &str, node: &Node) -> Decoded<Vec<Platform>> {
    match node.kind {
        Kind::Sequence => {
            let mut decoder = Decoder::new(source);
            let mut result = Vec::new();
            for item in &node.content {
                if let Some(platform) = decoder.platform(item)? {
                    result.push(platform);
                }
            }
            decoder.finish()?;
            Ok(result)
        }
        Kind::Mapping => {
            let mut result = Vec::new();
            for pair in node.content.as_chunks::<2>().0 {
                let name = pair[0].value.clone();
                let group = if name == "Linux64" { "Standalone" } else { "" };
                let mut platform = Platform {
                    first: BTreeMap::from([(group.to_string(), name)]),
                    ..Default::default()
                };
                let mut decoder = Decoder::new(source);
                decoder.second(&pair[1], &mut platform)?;
                decoder.finish()?;
                result.push(platform);
            }
            Ok(result)
        }
        Kind::Scalar => Err("unsupported PluginImporter platformData shape".into()),
    }
}

/// The `PluginImporter` of a plugin `.meta` file; `None` for GUID-only
/// metadata. The error text is wrapped by callers in `plugin metadata <path>: `.
pub(super) fn decode(content: &[u8]) -> Decoded<Option<Plugin>> {
    let content = EMPTY_KEY.replace_all(content, &b"${1}'':${2}"[..]);
    let Ok(source) = std::str::from_utf8(&content) else {
        return Err("yaml: invalid UTF-8".into());
    };
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Some(root) = yamlcodec::parse_first_document(source)
        .map_err(|error| format!("yaml: {}", error.message))?
    else {
        return Ok(None);
    };
    let mut decoder = Decoder::new(source);
    let mut plugin = None;
    let mut platform_data = None;
    Decoder::ported(&root)?;
    if !Decoder::null(&root) {
        if root.kind != Kind::Mapping {
            decoder.mismatch(&root, "object");
        } else {
            for (_, value) in decoder.fields(&root, &["PluginImporter"])? {
                Decoder::ported(value)?;
                if Decoder::null(value) {
                    plugin = None;
                    continue;
                }
                // The pointer is allocated before its target decodes.
                let target = plugin.get_or_insert_with(Plugin::default);
                if value.kind != Kind::Mapping {
                    decoder.mismatch(value, "object");
                    continue;
                }
                let known = [
                    "defineConstraints",
                    "isExplicitlyReferenced",
                    "platformData",
                ];
                for (name, field) in decoder.fields(value, &known)? {
                    match name {
                        "defineConstraints" => {
                            if let Some(constraints) = decoder.strings(field)? {
                                target.constraints = constraints;
                            }
                        }
                        "isExplicitlyReferenced" => {
                            if let Some(explicit) = decoder.int(field)? {
                                target.explicit = explicit;
                            }
                        }
                        _ => platform_data = Some(field),
                    }
                }
            }
        }
    }
    decoder.finish()?;
    let Some(mut plugin) = plugin else {
        return Ok(None);
    };
    if let Some(node) = platform_data {
        plugin.platforms = platforms(source, node)?;
    }
    Ok(Some(plugin))
}
