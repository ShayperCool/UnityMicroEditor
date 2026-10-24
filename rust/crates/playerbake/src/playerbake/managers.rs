//! The Player's `globalgamemanagers` written from a project's
//! `ProjectSettings` without an Editor build. The Player holds one object of
//! each settings class (tags and layers, physics, input, time, quality and
//! the rest) in a file without type trees, laid out by the Player type tree
//! of its class. A manager's authored fields are written over a base file the
//! engine wrote, so a field the Player has and the source lacks keeps the
//! base value, and an Editor-only source field is dropped.
//!
//! Experimental: the base is the `globalgamemanagers` of a Player built for
//! the same engine, which also supplies the managers a build derives (scripts,
//! resources, build settings) and the asset references.
use super::{
    source::text,
    text_values::{hex_items, scalar},
    typetree::{TypeTree, Val},
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{borrow::Cow, collections::BTreeMap, path::Path, sync::Arc};

/// A little-endian SerializedFile v22 or later without type trees, as a
/// Player build writes it. The metadata is kept as it is; only object data
/// and its table rows change.
pub struct PlainFile {
    /// The format, 22 (6000.0) or 23.
    version: u32,
    meta: Vec<u8>,
    /// Class ID per type table entry.
    types: Vec<i32>,
    pub objects: Vec<PlainObject>,
}

pub struct PlainObject {
    pub local_id: i64,
    pub class_id: i32,
    pub data: Vec<u8>,
    /// Where the object's table row sits in the metadata.
    row: usize,
}

const HEADER: usize = 48;

impl PlainFile {
    pub fn parse(file: &[u8]) -> Result<Self> {
        let be32 = |at: usize| {
            file.get(at..at + 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        };
        let be64 = |at: usize| {
            file.get(at..at + 8)
                .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        };
        let (Some(version), Some(0), Some(meta_size), Some(size), Some(data_offset)) =
            (be32(8), file.get(16).copied(), be32(20), be64(24), be64(32))
        else {
            bail!("not a little-endian SerializedFile");
        };
        if !(22..=23).contains(&version) || size != file.len() as u64 {
            bail!("unsupported SerializedFile version {version} or size");
        }
        let meta_end = HEADER + meta_size as usize;
        if meta_end as u64 > data_offset {
            bail!("SerializedFile header bounds are invalid");
        }
        let meta = &file[HEADER..meta_end];
        let mut at = 0;
        let take = |at: &mut usize, count: usize| -> Result<&[u8]> {
            let Some(bytes) = meta.get(*at..*at + count) else {
                bail!("SerializedFile metadata is truncated");
            };
            *at += count;
            Ok(bytes)
        };
        let le32 = |at: &mut usize| -> Result<u32> {
            Ok(u32::from_le_bytes(take(at, 4)?.try_into().unwrap()))
        };
        while take(&mut at, 1)?[0] != 0 {}
        le32(&mut at)?; // Target platform.
        if take(&mut at, 1)?[0] != 0 {
            bail!("SerializedFile has inline type trees");
        }
        let type_count = le32(&mut at)? as usize;
        let mut types = Vec::with_capacity(type_count);
        for _ in 0..type_count {
            let header = take(&mut at, 7)?;
            let class = i32::from_le_bytes(header[..4].try_into().unwrap());
            take(
                &mut at,
                if class == super::typetree::SCRIPT_CLASS {
                    32
                } else {
                    16
                },
            )?;
            types.push(class);
        }
        let object_count = le32(&mut at)? as usize;
        let mut objects = Vec::with_capacity(object_count);
        for _ in 0..object_count {
            at = ((HEADER + at + 3) & !3) - HEADER;
            let row = at;
            let bytes = take(&mut at, 24)?;
            let local_id = i64::from_le_bytes(bytes[..8].try_into().unwrap());
            let offset = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
            let length = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as u64;
            let index = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
            let start = data_offset + offset;
            let (Some(&class_id), true) = (types.get(index), start + length <= size) else {
                bail!("SerializedFile object bounds are invalid");
            };
            objects.push(PlainObject {
                local_id,
                class_id,
                data: file[start as usize..(start + length) as usize].to_vec(),
                row,
            });
        }
        Ok(Self {
            version,
            meta: meta.to_vec(),
            types,
            objects,
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut meta = self.meta.clone();
        let mut data = Vec::new();
        for object in &self.objects {
            while !data.len().is_multiple_of(16) {
                data.push(0);
            }
            let row = object.row;
            meta[row + 8..row + 16].copy_from_slice(&(data.len() as u64).to_le_bytes());
            meta[row + 16..row + 20].copy_from_slice(&(object.data.len() as u32).to_le_bytes());
            data.extend_from_slice(&object.data);
        }
        let mut data_offset = HEADER + meta.len();
        while !data_offset.is_multiple_of(16) {
            data_offset += 1;
        }
        let mut out = vec![0u8; HEADER];
        out[8..12].copy_from_slice(&self.version.to_be_bytes());
        out[20..24].copy_from_slice(&(meta.len() as u32).to_be_bytes());
        out[24..32].copy_from_slice(&((data_offset + data.len()) as u64).to_be_bytes());
        out[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
        out.extend_from_slice(&meta);
        out.resize(data_offset, 0);
        out.extend_from_slice(&data);
        out
    }

    pub fn classes(&self) -> &[i32] {
        &self.types
    }
}

macro_rules! manager_trees {
    ($($id:literal),*) => {
        [$(($id, &include_bytes!(concat!("engine/type-", $id, ".bin"))[..])),*]
    };
}

/// The Player type trees of the settings managers, as the pinned Editor
/// writes them for a Linux Player (`ContentBuildInterface.WriteSerializedFile`
/// over the objects of `ProjectSettings`).
pub fn manager_trees() -> Result<BTreeMap<i32, Arc<TypeTree>>> {
    manager_trees!(
        5, 11, 13, 19, 30, 47, 55, 78, 126, 129, 176606843, 655991488, 937362698
    )
    .into_iter()
    .map(|(class_id, entry)| Ok((class_id, Arc::new(TypeTree::parse(entry)?))))
    .collect()
}

/// What became of one manager.
#[derive(Debug, Default, serde::Serialize)]
pub struct ManagerReport {
    pub class_id: i32,
    pub source: String,
    /// Authored fields written over the base.
    pub written: usize,
    /// Fields kept from the base, with the reason.
    pub kept: Vec<String>,
}

/// Writes the managers authored in `project/ProjectSettings` over `base`.
/// Each manager the base holds and a source authors is decoded with its
/// tree, overlaid and encoded again; a base object the trees do not
/// reproduce byte for byte is refused before anything is written.
pub fn write_project_managers(
    project: &Path,
    base: &[u8],
    trees: &BTreeMap<i32, Arc<TypeTree>>,
) -> Result<(Vec<u8>, Vec<ManagerReport>)> {
    let mut file = PlainFile::parse(base)?;
    let settings = project.join("ProjectSettings");
    let mut sources = Vec::new();
    for entry in
        std::fs::read_dir(&settings).with_context(|| format!("read {}", settings.display()))?
    {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "asset")
        {
            sources.push(path);
        }
    }
    sources.sort();
    let mut reports = Vec::new();
    let mut quality_names = None;
    for path in sources {
        let yaml = std::fs::read(&path)?;
        // Text sources only; a project that forces binary serialization
        // keeps its settings in the Editor's own form.
        if !yaml.starts_with(b"%YAML") {
            continue;
        }
        let documents = crate::yaml::parse_unity_yaml_lexical(&yaml)
            .with_context(|| format!("parse {}", path.display()))?;
        for document in documents {
            let class_id = document.class_id as i32;
            let (Some(tree), Some(object)) = (
                trees.get(&class_id),
                file.objects
                    .iter_mut()
                    .find(|object| object.class_id == class_id),
            ) else {
                continue;
            };
            let base_value = tree.decode(&object.data).with_context(|| {
                format!("decode base manager of class {class_id} with its tree")
            })?;
            if tree.encode(&base_value)? != object.data {
                bail!("the tree of class {class_id} does not reproduce the base manager");
            }
            let mut report = ManagerReport {
                class_id,
                source: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                ..Default::default()
            };
            let mut source = Value::Object(document.data.clone());
            if class_id == QUALITY {
                quality_names = Some(standalone_quality(&mut source)?);
            }
            let value = Overlay {
                tree,
                report: &mut report,
                pointers: None,
                upgrade: false,
            }
            .value(0, base_value, &source, "")?;
            object.data = tree.encode(&value)?;
            reports.push(report);
        }
    }
    settle_player(&mut file, trees, quality_names)?;
    Ok((file.write(), reports))
}

/// Where an authored reference (`{fileID, guid, type}`) leads in the
/// written file: its file and path ID, or `None` for an asset the file does
/// not reach.
pub type PointerOf<'a> = &'a dyn Fn(&Value) -> Option<(i64, i64)>;

pub(super) struct Overlay<'a> {
    pub(super) tree: &'a TypeTree,
    pub(super) report: &'a mut ManagerReport,
    /// Without it, references keep the base's pointers.
    pub(super) pointers: Option<PointerOf<'a>>,
    /// Whether a structure the source wrote at another serialized version
    /// takes its fields by name (with the upgrades `upgraded` knows) instead
    /// of keeping the base: a base of defaults has no value worth keeping.
    pub(super) upgrade: bool,
}

impl Overlay<'_> {
    fn keep(&mut self, base: Val, path: &str, why: &str) -> Result<Val> {
        self.report.kept.push(format!("{path}: {why}"));
        Ok(base)
    }

    pub(super) fn value(
        &mut self,
        index: usize,
        base: Val,
        source: &Value,
        path: &str,
    ) -> Result<Val> {
        let node = &self.tree.nodes[index];
        let children = self.tree.children(index);
        if let Some(value) = self.upgraded(index, &base, source) {
            self.report.written += 1;
            self.report.kept.push(format!("{path}: upgraded"));
            return Ok(value);
        }
        if children.is_empty() {
            if node.size == 0 {
                return Ok(base);
            }
            let Value::String(source) = source else {
                return self.keep(base, path, "not a scalar");
            };
            return match scalar(&node.type_name, source, path) {
                Ok(value) => {
                    self.report.written += 1;
                    Ok(value)
                }
                Err(error) => self.keep(base, path, &error.to_string()),
            };
        }
        if node.type_name == "string" {
            self.report.written += 1;
            return Ok(match source {
                Value::Null => Val::Str(String::new()),
                Value::String(source) => Val::Str(source.clone()),
                _ => return self.keep(base, path, "not a string"),
            });
        }
        if node.type_name == "GUID" {
            // Each of the four words as eight hexadecimal digits, the
            // lowest nibble first.
            let Some(words) = source.as_str().and_then(guid_words) else {
                return self.keep(base, path, "not a GUID");
            };
            self.report.written += 1;
            let fields = children
                .iter()
                .zip(words)
                .map(|(&child, word)| {
                    (
                        Cow::Owned(self.tree.nodes[child].name.clone()),
                        Val::U(word as u64),
                    )
                })
                .collect();
            return Ok(Val::Map(fields));
        }
        if node.type_name.starts_with("PPtr<") {
            // An asset reference needs the Player file of its asset.
            let Some(pointers) = self.pointers else {
                return self.keep(base, path, "object reference");
            };
            let empty = source
                .get("fileID")
                .and_then(|id| text(id).ok())
                .is_none_or(|id| id == "0");
            let (file, path_id) = match empty {
                true => (0, 0),
                false => match pointers(source) {
                    Some(pointer) => pointer,
                    None => {
                        self.report
                            .kept
                            .push(format!("{path}: an asset the Player lacks"));
                        (0, 0)
                    }
                },
            };
            self.report.written += 1;
            let names: Vec<String> = children
                .iter()
                .map(|&child| self.tree.nodes[child].name.clone())
                .collect();
            return Ok(Val::Map(vec![
                (Cow::Owned(names[0].clone()), Val::I(file)),
                (Cow::Owned(names[1].clone()), Val::I(path_id)),
            ]));
        }
        if let Some((_, element)) = self.tree.element(index) {
            // A build lists the objects it holds itself: it drops missing
            // ones and adds what the render pipeline needs.
            if self.tree.nodes[element].type_name.starts_with("PPtr<") && self.pointers.is_none() {
                return self.keep(base, path, "sequence of object references");
            }
            if node.type_name != "vector" {
                return self.keep(base, path, &format!("{} container", node.type_name));
            }
            let items = match source {
                Value::Null => return Ok(Val::Seq(Vec::new())),
                Value::String(dump) if self.tree.children(element).is_empty() => {
                    let item = &self.tree.nodes[element].type_name;
                    return match hex_items(item, dump, path) {
                        Ok(items) => {
                            self.report.written += 1;
                            Ok(Val::Seq(items))
                        }
                        Err(error) => self.keep(base, path, &error.to_string()),
                    };
                }
                Value::Array(items) => items,
                _ => return self.keep(base, path, "not a sequence"),
            };
            let mut bases = match base {
                Val::Seq(items) => items,
                _ => Vec::new(),
            }
            .into_iter();
            let mut values = Vec::with_capacity(items.len());
            for (position, item) in items.iter().enumerate() {
                let base = bases.next().unwrap_or(Val::Zero);
                values.push(self.value(element, base, item, &format!("{path}[{position}]"))?);
            }
            return Ok(Val::Seq(values));
        }
        let Value::Object(fields) = source else {
            return self.keep(base, path, "not a mapping");
        };
        let version = match fields.get("serializedVersion") {
            None => 1,
            Some(version) => text(version)?.parse::<u16>().unwrap_or(0),
        };
        if version != node.version {
            let why = format!(
                "serialized version {version}, the engine writes {}",
                node.version
            );
            if !self.upgrade {
                return self.keep(base, path, &why);
            }
            self.report
                .kept
                .push(format!("{path}: {why}, taken by name"));
        }
        let mut base_fields = match base {
            Val::Map(fields) => fields,
            _ => Vec::new(),
        };
        let mut values = Vec::with_capacity(children.len());
        for child in children {
            let name = self.tree.nodes[child].name.clone();
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            let base = base_fields
                .iter()
                .position(|(key, _)| *key == name)
                .map(|position| base_fields.swap_remove(position).1)
                .unwrap_or(Val::Zero);
            let value = match fields.get(&name) {
                Some(field) => self.value(child, base, field, &child_path)?,
                None => self.keep(base, &child_path, "not authored")?,
            };
            values.push((Cow::Owned(name), value));
        }
        Ok(Val::Map(values))
    }
}

impl Overlay<'_> {
    /// A field a source of an older serialized version wrote in another
    /// form, as the engine upgrades it: `TimeManager`'s fixed timestep, a
    /// float of seconds before it became a count of 1/141120000ths of a
    /// second (the single's value, rounded down).
    fn upgraded(&self, index: usize, base: &Val, source: &Value) -> Option<Val> {
        let node = &self.tree.nodes[index];
        if node.type_name != "RationalTime" {
            return None;
        }
        let seconds: f32 = source.as_str()?.parse().ok()?;
        const RATE: u64 = 141_120_000;
        let mut value = base.clone();
        *value.field_mut("m_Count")? = Val::I((seconds as f64 * RATE as f64).floor() as i64);
        let rate = value.field_mut("m_Rate")?;
        *rate.field_mut("m_Denominator")? = Val::U(1);
        *rate.field_mut("m_Numerator")? = Val::U(RATE);
        Some(value)
    }
}

/// The top-level fields in which the managers of two files differ, decoded
/// with their trees, for a comparison with an Editor build.
pub fn compare_managers(
    ours: &[u8],
    theirs: &[u8],
    trees: &BTreeMap<i32, Arc<TypeTree>>,
) -> Result<Vec<String>> {
    let (ours, theirs) = (PlainFile::parse(ours)?, PlainFile::parse(theirs)?);
    let mut differences = Vec::new();
    for object in &ours.objects {
        let Some(other) = theirs
            .objects
            .iter()
            .find(|other| other.class_id == object.class_id)
        else {
            differences.push(format!("class {} only in ours", object.class_id));
            continue;
        };
        if other.data == object.data {
            continue;
        }
        let Some(tree) = trees.get(&object.class_id) else {
            differences.push(format!("class {} differs, no tree", object.class_id));
            continue;
        };
        let (Val::Map(a), Val::Map(b)) = (tree.decode(&object.data)?, tree.decode(&other.data)?)
        else {
            bail!("manager is not a structure");
        };
        for ((name, x), (_, y)) in a.iter().zip(&b) {
            if x != y {
                differences.push(format!(
                    "class {} {name}: ours {x:?} theirs {y:?}",
                    object.class_id
                ));
            }
        }
    }
    Ok(differences)
}

/// A `Standalone` build keeps the quality levels not excluded for that
/// platform, starts on the platform's default level, and names the kept
/// levels in PlayerSettings. The source is narrowed before it is written, so
/// the kept levels line up with those of a base built the same way.
const STANDALONE: &str = "Standalone";
pub(super) const QUALITY: i32 = 47;
pub(super) const PLAYER: i32 = 129;

pub(super) fn standalone_quality(source: &mut Value) -> Result<Vec<Val>> {
    let Some(levels) = source.get("m_QualitySettings").and_then(Value::as_array) else {
        bail!("QualitySettings has no levels");
    };
    let current = source
        .get("m_PerPlatformDefaultQuality")
        .and_then(|defaults| defaults.get(STANDALONE))
        .or_else(|| source.get("m_CurrentQuality"))
        .and_then(Value::as_str)
        .and_then(|level| level.parse::<usize>().ok());
    let kept: Vec<(usize, Value)> = levels
        .iter()
        .cloned()
        .enumerate()
        .filter(|(_, level)| {
            !level
                .get("excludedTargetPlatforms")
                .and_then(Value::as_array)
                .is_some_and(|platforms| platforms.iter().any(|name| name == STANDALONE))
        })
        .collect();
    if kept.is_empty() {
        bail!("every quality level is excluded for {STANDALONE}");
    }
    let current = current
        .and_then(|current| kept.iter().position(|(position, _)| *position == current))
        .unwrap_or(0);
    let names = kept
        .iter()
        .map(|(_, level)| {
            Val::Str(
                level
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            )
        })
        .collect();
    source["m_QualitySettings"] = Value::Array(kept.into_iter().map(|(_, level)| level).collect());
    source["m_CurrentQuality"] = Value::String(current.to_string());
    Ok(names)
}

/// What a session's Player takes from PlayerSettings beyond the authored
/// fields: the names of the kept quality levels, and what a host build sets
/// (`AgentEditorBridge.BuildHost`): it runs in the background. An authored
/// target pixel density of 0 is written as 30, as the Editor does (observed
/// for 6000.6.3f1; the rule behind it is not known).
fn settle_player(
    file: &mut PlainFile,
    trees: &BTreeMap<i32, Arc<TypeTree>>,
    quality_names: Option<Vec<Val>>,
) -> Result<()> {
    let (Some(tree), Some(player)) = (
        trees.get(&PLAYER),
        file.objects
            .iter_mut()
            .find(|object| object.class_id == PLAYER),
    ) else {
        return Ok(());
    };
    let mut value = tree.decode(&player.data)?;
    settle_player_value(&mut value, quality_names);
    player.data = tree.encode(&value)?;
    Ok(())
}

pub(super) fn settle_player_value(value: &mut Val, quality_names: Option<Vec<Val>>) {
    if let (Some(names), Some(field)) = (quality_names, value.field_mut("qualitySettingsNames")) {
        *field = Val::Seq(names);
    }
    if let Some(field) = value.field_mut("runInBackground") {
        *field = Val::Bool(true);
    }
    if let Some(field) = value.field_mut("targetPixelDensity")
        && *field == Val::I(0)
    {
        *field = Val::I(30);
    }
}

pub(super) fn guid_words(text: &str) -> Option<[u32; 4]> {
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut words = [0u32; 4];
    for (word, chunk) in words.iter_mut().zip(text.as_bytes().chunks(8)) {
        let reversed: String = chunk.iter().rev().map(|&byte| byte as char).collect();
        *word = u32::from_str_radix(&reversed, 16).ok()?;
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_words_reverse_the_nibbles_of_each_word() {
        // productGUID of a project as the Editor writes it, and the words its
        // Player build holds.
        let words = guid_words("4504f9cdc84a5409a93e67483d2ebc8e").unwrap();
        assert_eq!(&words[..3], &[0xdc9f4054, 0x9045a48c, 0x8476e39a]);
        assert!(guid_words("4504f9cd").is_none());
        assert!(guid_words("zz04f9cdc84a5409a93e67483d2ebc8e").is_none());
    }

    #[test]
    fn standalone_quality_keeps_levels_not_excluded_for_standalone() {
        let mut source = serde_json::json!({
            "m_CurrentQuality": "0",
            "m_QualitySettings": [
                {"name": "Mobile", "excludedTargetPlatforms": ["Standalone"]},
                {"name": "Low", "excludedTargetPlatforms": []},
                {"name": "PC", "excludedTargetPlatforms": ["Android", "iPhone"]},
            ],
            "m_PerPlatformDefaultQuality": {"Android": "0", "Standalone": "2"},
        });
        let names = standalone_quality(&mut source).unwrap();
        assert_eq!(names, vec![Val::str("Low"), Val::str("PC")]);
        assert_eq!(source["m_CurrentQuality"], "1");
        assert_eq!(source["m_QualitySettings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn standalone_quality_refuses_when_every_level_is_excluded() {
        let mut source = serde_json::json!({
            "m_QualitySettings": [{"name": "Mobile", "excludedTargetPlatforms": ["Standalone"]}],
        });
        assert!(standalone_quality(&mut source).is_err());
    }
}
