//! The files of a Player build beside its scenes, written by the tool
//! instead of an Editor build: `globalgamemanagers` (the settings managers
//! and those a build derives) and the files without type trees it names.
//!
//! The layouts and the defaults of the managers of a release come from a
//! reference capture (`tools/ReferenceCapture/ManagerCapture.cs`, run once
//! per release with a licensed Editor in an empty project) shipped as
//! `engine/<release>/managers.bin`. A project's `ProjectSettings` are
//! written over those defaults; what a build derives (its scenes, scripts,
//! resources and shader names) is given by the caller.
use super::{
    managers::{self, ManagerReport, Overlay, PointerOf},
    serialized,
    typetree::{Node, TypeTree, Val},
};
use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use std::{borrow::Cow, collections::BTreeMap, path::Path, sync::Arc};

/// The files `globalgamemanagers` refers to, in its order, with the GUID by
/// which the Player finds a file of the engine's own: a pointer's file ID is
/// the position here plus one.
pub const MANAGER_EXTERNALS: [(&str, &str); 4] = [
    ("globalgamemanagers.assets", ""),
    (
        "Library/unity default resources",
        "0000000000000000e000000000000000",
    ),
    (
        "Resources/unity_builtin_extra",
        "0000000000000000f000000000000000",
    ),
    ("resources.assets", ""),
];
pub const GLOBAL_ASSETS_FILE: i64 = 1;
pub const DEFAULT_RESOURCES_FILE: i64 = 2;
pub const BUILTIN_EXTRA_FILE: i64 = 3;
pub const RESOURCES_FILE: i64 = 4;

/// A build's managers in the order and with the local IDs an Editor build of
/// 6000.0 gives them (ID 16, the cluster input manager, is not written).
const MANAGER_ORDER: [(i32, i64); 20] = [
    (129, 1),
    (13, 2),
    (78, 3),
    (11, 4),
    (SCRIPT_MAPPER, 5),
    (MONO_MANAGER, 6),
    (GRAPHICS, 7),
    (5, 8),
    (98, 9),
    (55, 10),
    (BUILD_SETTINGS, 11),
    (managers::QUALITY, 12),
    (RESOURCE_MANAGER, 13),
    (126, 14),
    (19, 15),
    (300, 17),
    (310, 18),
    (1_403_656_975, 19),
    (937_362_698, 20),
    (655_991_488, 21),
];
/// The same of 6000.6, as an Editor build of Dungeon has them: the cluster
/// input manager is written, IDs 17 and 19 are not given.
const MANAGER_ORDER_6000_6: [(i32, i64); 20] = [
    (129, 1),
    (13, 2),
    (78, 3),
    (11, 4),
    (SCRIPT_MAPPER, 5),
    (MONO_MANAGER, 6),
    (GRAPHICS, 7),
    (5, 8),
    (98, 9),
    (55, 10),
    (BUILD_SETTINGS, 11),
    (managers::QUALITY, 12),
    (RESOURCE_MANAGER, 13),
    (126, 14),
    (19, 15),
    (176_606_843, 16),
    (300, 18),
    (1_403_656_975, 20),
    (937_362_698, 21),
    (655_991_488, 22),
];

/// The managers of a release's build, in order, with their local IDs.
fn manager_order(release: &str) -> &'static [(i32, i64)] {
    match release {
        "6000.6.3f1" => &MANAGER_ORDER_6000_6,
        _ => &MANAGER_ORDER,
    }
}
const SCRIPT_MAPPER: i32 = 94;
const MONO_MANAGER: i32 = 116;
const GRAPHICS: i32 = 30;
const BUILD_SETTINGS: i32 = 141;
const RESOURCE_MANAGER: i32 = 147;

/// The type hashes of the managers that hold nothing a capture can write
/// (the delayed call manager, the runtime initializer manager and class
/// 1403656975, all of no bytes) or that only a build makes (the resource
/// manager), as an Editor build of the release writes them.
/// The same in 6000.0 and 6000.6.
fn fixed_hash(release: &str, class_id: i32) -> Option<&'static str> {
    let release = match release {
        "6000.6.3f1" => "6000.0.71f1",
        other => other,
    };
    match (release, class_id) {
        ("6000.0.71f1", 98) => Some("643e11867d6e720ee84a107eb84644bd"),
        ("6000.0.71f1", RESOURCE_MANAGER) => Some("0d9d880e077f33c76fc2c99bd629a50a"),
        ("6000.0.71f1", 300) => Some("e338a255460201b5f5fe96544385bb31"),
        ("6000.0.71f1", 1_403_656_975) => Some("6732968092ba7dfba8a0b019df3daadd"),
        _ => None,
    }
}

struct PackManager {
    class_id: i32,
    tree: Arc<TypeTree>,
    /// The defaults of an empty project, its pointers into
    /// `MANAGER_EXTERNALS`.
    value: Val,
    hash: [u8; 16],
}

/// The managers' layouts and defaults of a release.
pub struct ManagerPack {
    pub release: &'static str,
    managers: Vec<PackManager>,
}

/// The pack of each release the tool has one of.
fn pack_bytes(release: &str) -> Option<(&'static str, &'static [u8])> {
    match release {
        "6000.0.71f1" => Some((
            "6000.0.71f1",
            include_bytes!("engine/6000.0.71f1/managers.bin"),
        )),
        "6000.6.3f1" => Some((
            "6000.6.3f1",
            include_bytes!("engine/6000.6.3f1/managers.bin"),
        )),
        _ => None,
    }
}

/// Where a capture's tree and the bytes the Player writes part: 6000.0's
/// Player writes no `InsightsSettings` in `UnityConnectSettings`, and four
/// bytes after the last field its tree names in `PlayerSettings` (zero in
/// every build seen). The trees are corrected so that they describe the
/// bytes.
fn corrected(release: &str, tree: Arc<TypeTree>) -> Result<Arc<TypeTree>> {
    // 6000.6 also writes no `InsightsSettings`; its `PlayerSettings` ends
    // where the tree does.
    if !matches!(release, "6000.0.71f1" | "6000.6.3f1")
        || (release != "6000.0.71f1" && tree.class_id == 129)
    {
        return Ok(tree);
    }
    let mut nodes = tree.nodes.clone();
    match tree.class_id {
        310 => {
            let Some(start) = nodes
                .iter()
                .position(|node| node.level == 1 && node.name == "InsightsSettings")
            else {
                return Ok(tree);
            };
            let end = (start + 1..nodes.len())
                .find(|&at| nodes[at].level <= 1)
                .unwrap_or(nodes.len());
            nodes.drain(start..end);
        }
        129 => nodes.push(Node {
            type_name: "int".into(),
            name: "m_UnnamedPlayerTail".into(),
            level: 1,
            array: false,
            align: false,
            size: 4,
            version: 1,
            type_flags: 0,
            meta: 0,
        }),
        _ => return Ok(tree),
    }
    let hash: [u8; 16] = tree.entry[7..23].try_into()?;
    Ok(Arc::new(TypeTree::of_nodes(
        tree.class_id,
        None,
        hash,
        nodes,
    )?))
}

impl ManagerPack {
    /// The pack of `release`, or `None` for a release the tool has none of.
    pub fn of_release(release: &str) -> Result<Option<Self>> {
        let Some((release, data)) = pack_bytes(release) else {
            return Ok(None);
        };
        let (objects, externals) = serialized::read_with_externals(data)?;
        let mut managers = Vec::new();
        for object in objects {
            let class_id = object.tree.class_id;
            let hash: [u8; 16] = object.tree.entry[7..23].try_into()?;
            let tree = corrected(release, object.tree.clone())?;
            let mut value = tree
                .decode(&object.data)
                .with_context(|| format!("decode the captured manager of class {class_id}"))?;
            remap(&mut value, &mut |file, path| {
                if file == 0 {
                    return (0, path);
                }
                let name = externals.get(file as usize - 1).map(String::as_str);
                match MANAGER_EXTERNALS
                    .iter()
                    .position(|(external, _)| Some(*external) == name)
                {
                    Some(position) => (position as i64 + 1, path),
                    None => (0, 0),
                }
            });
            managers.push(PackManager {
                class_id,
                tree,
                value,
                hash,
            });
        }
        Ok(Some(Self { release, managers }))
    }

    /// The layout of each manager the pack holds.
    pub fn trees(&self) -> BTreeMap<i32, Arc<TypeTree>> {
        self.managers
            .iter()
            .map(|manager| (manager.class_id, manager.tree.clone()))
            .collect()
    }

    fn manager(&self, class_id: i32) -> Option<&PackManager> {
        self.managers
            .iter()
            .find(|manager| manager.class_id == class_id)
    }
}

/// Rewrites every pointer of a value.
pub(super) fn remap(value: &mut Val, map: &mut impl FnMut(i64, i64) -> (i64, i64)) {
    match value {
        Val::Map(fields) => {
            if fields.len() == 2
                && fields[0].0 == "m_FileID"
                && fields[1].0 == "m_PathID"
                && let (Val::I(file), Val::I(path)) = (&fields[0].1, &fields[1].1)
            {
                let (file, path) = map(*file, *path);
                fields[0].1 = Val::I(file);
                fields[1].1 = Val::I(path);
                return;
            }
            for (_, field) in fields.iter_mut() {
                remap(field, map);
            }
        }
        Val::Seq(items) => items.iter_mut().for_each(|item| remap(item, map)),
        _ => {}
    }
}

fn pointer(file: i64, path: i64) -> Val {
    Val::Map(vec![
        (Cow::Borrowed("m_FileID"), Val::I(file)),
        (Cow::Borrowed("m_PathID"), Val::I(path)),
    ])
}

/// What a build derives for its managers, beside the project's settings.
pub struct DerivedManagers<'a> {
    /// The scenes of the build, by path, the first loaded at start.
    pub scenes: Vec<String>,
    /// The graphics APIs of the Player, by the engine's numbers (17 is
    /// OpenGL Core).
    pub graphics_apis: Vec<i64>,
    /// The `MonoScript`s the Player loads at its start, as pointers into
    /// `MANAGER_EXTERNALS`.
    pub scripts: Vec<(i64, i64)>,
    /// Each name `Resources.Load` finds and the object it gives.
    pub resources: Vec<(String, (i64, i64))>,
    /// Shaders `Shader.Find` finds without their being loaded, beside the
    /// engine's own: where each lies in the build, by name.
    pub shaders: Vec<((i64, i64), String)>,
    /// The path IDs of the shaders the Player's `unity_builtin_extra` holds.
    pub builtin_shaders: Vec<i64>,
    /// The project's shader settings, which the build records as the
    /// defines of each hardware tier.
    pub shader_settings: Option<&'a super::ShaderSettings>,
    /// Where an authored reference of a settings file leads.
    pub pointers: PointerOf<'a>,
}

/// Writes `globalgamemanagers` for `project` from the pack's defaults, the
/// project's settings and what the build derives.
pub fn write_game_managers(
    project: &Path,
    pack: &ManagerPack,
    derived: &DerivedManagers<'_>,
) -> Result<(Vec<u8>, Vec<ManagerReport>)> {
    let mut values: BTreeMap<i32, Val> = pack
        .managers
        .iter()
        .map(|manager| (manager.class_id, manager.value.clone()))
        .collect();
    let mut reports = Vec::new();
    let mut quality_names = None;
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
    for path in sources {
        let yaml = std::fs::read(&path)?;
        if !yaml.starts_with(b"%YAML") {
            continue;
        }
        let documents = crate::yaml::parse_unity_yaml_lexical(&yaml)
            .with_context(|| format!("parse {}", path.display()))?;
        for document in documents {
            let class_id = document.class_id as i32;
            let (Some(manager), Some(base)) = (pack.manager(class_id), values.remove(&class_id))
            else {
                continue;
            };
            let mut report = ManagerReport {
                class_id,
                source: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                ..Default::default()
            };
            let mut source = Value::Object(document.data.clone());
            if class_id == managers::QUALITY {
                quality_names = Some(managers::standalone_quality(&mut source)?);
            }
            legacy_defaults(class_id, &manager.tree, &mut source);
            let value = Overlay {
                tree: &manager.tree,
                report: &mut report,
                pointers: Some(derived.pointers),
                upgrade: true,
            }
            .value(0, base, &source, "")?;
            values.insert(class_id, value);
            reports.push(report);
        }
    }
    if let Some(player) = values.get_mut(&managers::PLAYER) {
        managers::settle_player_value(player, quality_names);
        // A host preloads nothing of the game, as the Editor's host build
        // clears the preloaded assets: the session brings the game's content.
        if let Some(field) = player.field_mut("preloadedAssets") {
            *field = Val::Seq(Vec::new());
        }
    }
    if let Some(build) = values.get_mut(&BUILD_SETTINGS) {
        if let Some(field) = build.field_mut("scenes") {
            *field = Val::Seq(derived.scenes.iter().map(|scene| Val::str(scene)).collect());
        }
        if let Some(field) = build.field_mut("m_GraphicsAPIs") {
            *field = Val::Seq(
                derived
                    .graphics_apis
                    .iter()
                    .map(|&api| Val::I(api))
                    .collect(),
            );
        }
    }
    if let Some(mono) = values.get_mut(&MONO_MANAGER)
        && let Some(field) = mono.field_mut("m_Scripts")
    {
        *field = Val::Seq(
            derived
                .scripts
                .iter()
                .map(|&(file, path)| pointer(file, path))
                .collect(),
        );
    }
    if let Some(mapper) = values.get_mut(&SCRIPT_MAPPER)
        && let Some(Val::Map(shaders)) = mapper.field_mut("m_Shaders")
        && let Some((_, Val::Seq(names))) = shaders
            .iter_mut()
            .find(|(name, _)| name == "m_ObjectToName")
    {
        shader_names(names, derived);
    }
    if let (Some(graphics), Some(settings)) = (values.get_mut(&GRAPHICS), derived.shader_settings)
        && let Some(field) = graphics.field_mut("m_ShaderDefinesPerShaderCompiler")
    {
        let tiers = super::shader::recorded_defines(settings);
        let words = |bits: u64| Val::Seq(vec![Val::U(bits & 0xffff_ffff), Val::U(bits >> 32)]);
        *field = Val::Seq(
            derived
                .graphics_apis
                .iter()
                .filter_map(|&api| compiler_platform(api))
                .map(|platform| {
                    Val::Map(vec![
                        (Cow::Borrowed("shaderPlatform"), Val::I(platform)),
                        (Cow::Borrowed("defines_Tier1"), words(tiers[0])),
                        (Cow::Borrowed("defines_Tier2"), words(tiers[1])),
                        (Cow::Borrowed("defines_Tier3"), words(tiers[2])),
                    ])
                })
                .collect(),
        );
    }
    let mut types: Vec<(i32, [u8; 16])> = Vec::new();
    let mut objects: Vec<(i64, usize, Vec<u8>)> = Vec::new();
    for &(class_id, local_id) in manager_order(pack.release) {
        let (hash, data) = match pack.manager(class_id) {
            Some(manager) => {
                let value = values.get(&class_id).unwrap_or(&manager.value);
                (manager.hash, manager.tree.encode(value)?)
            }
            None => {
                let Some(hash) = fixed_hash(pack.release, class_id) else {
                    bail!("no layout of manager class {class_id} for {}", pack.release);
                };
                let data = match class_id {
                    RESOURCE_MANAGER => resource_manager(&derived.resources),
                    _ => Vec::new(),
                };
                (hex16(hash)?, data)
            }
        };
        let index = match types.iter().position(|(class, _)| *class == class_id) {
            Some(index) => index,
            None => {
                types.push((class_id, hash));
                types.len() - 1
            }
        };
        objects.push((local_id, index, data));
    }
    let file = plain_file(
        pack.release,
        &types,
        &objects,
        &MANAGER_EXTERNALS.map(|(path, guid)| (path.to_string(), guid.to_string())),
    )?;
    Ok((file, reports))
}

/// The shader compiler platform of a graphics API: OpenGL Core alone,
/// the API of a host.
fn compiler_platform(api: i64) -> Option<i64> {
    (api == 17).then_some(15)
}

/// Fields a settings file of an older serialized version lacks, which the
/// engine gives the values that keep its behaviour when it upgrades the
/// file: transforms synchronised with physics before each query, and the
/// angular speed limit of then.
fn legacy_defaults(class_id: i32, tree: &TypeTree, source: &mut Value) {
    let rules: &[(&str, &str)] = match class_id {
        55 => &[
            ("m_AutoSyncTransforms", "1"),
            ("m_DefaultMaxAngularSpeed", "7"),
        ],
        19 => &[("m_AutoSyncTransforms", "1")],
        _ => return,
    };
    let version = source
        .get("serializedVersion")
        .and_then(Value::as_str)
        .and_then(|version| version.parse::<u16>().ok())
        .unwrap_or(1);
    let Value::Object(fields) = source else {
        return;
    };
    if version >= tree.nodes[0].version {
        return;
    }
    for (name, value) in rules {
        fields
            .entry(name.to_string())
            .or_insert_with(|| Value::String(value.to_string()));
    }
}

/// The names `Shader.Find` knows: the engine's registry, each name pointing
/// where the build holds its shader or nowhere, then the shaders of the
/// build's own files.
fn shader_names(names: &mut Vec<Val>, derived: &DerivedManagers<'_>) {
    let placed: BTreeMap<&str, (i64, i64)> = derived
        .shaders
        .iter()
        .map(|(at, name)| (name.as_str(), *at))
        .collect();
    let mut known = std::collections::BTreeSet::new();
    for entry in names.iter_mut() {
        let name = match entry.field("second") {
            Some(Val::Str(name)) => name.clone(),
            _ => continue,
        };
        let Some(first) = entry.field_mut("first") else {
            continue;
        };
        let at = match (first.field("m_FileID"), first.field("m_PathID")) {
            (Some(Val::I(file)), Some(Val::I(path))) => (*file, *path),
            _ => continue,
        };
        let kept = match placed.get(name.as_str()) {
            Some(&placed) => placed,
            None if at.0 == BUILTIN_EXTRA_FILE && !derived.builtin_shaders.contains(&at.1) => {
                (0, 0)
            }
            None => at,
        };
        *first = pointer(kept.0, kept.1);
        known.insert(name);
    }
    for ((file, path), name) in &derived.shaders {
        if known.contains(name) {
            continue;
        }
        names.push(Val::Map(vec![
            (Cow::Borrowed("first"), pointer(*file, *path)),
            (Cow::Borrowed("second"), Val::str(name)),
        ]));
    }
}

fn hex16(text: &str) -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    if text.len() != 32 {
        bail!("not a 16-byte hash: {text}");
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16)?;
    }
    Ok(bytes)
}

/// `ResourceManager`: each name with the object it gives, sorted by name as
/// a build sorts them, and no preloaded dependencies.
fn resource_manager(resources: &[(String, (i64, i64))]) -> Vec<u8> {
    let mut sorted: Vec<&(String, (i64, i64))> = resources.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut out = Vec::new();
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for (name, (file, path)) in sorted {
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out.extend_from_slice(&(*file as i32).to_le_bytes());
        out.extend_from_slice(&path.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// A Player file without type trees, as a build writes it: the release's
/// version, the Linux target, the classes with their type hashes, the
/// objects (local ID, class index, data) and the files it refers to.
pub fn plain_file(
    unity_version: &str,
    types: &[(i32, [u8; 16])],
    objects: &[(i64, usize, Vec<u8>)],
    externals: &[(String, String)],
) -> Result<Vec<u8>> {
    const HEADER: usize = 48;
    let format = serialized::FileFormat::of_release(unity_version)
        .with_context(|| format!("the files of Unity {unity_version}"))?;
    let mut meta = Vec::new();
    meta.extend_from_slice(unity_version.as_bytes());
    meta.push(0);
    meta.extend_from_slice(&24u32.to_le_bytes());
    meta.push(0);
    meta.extend_from_slice(&(types.len() as u32).to_le_bytes());
    for (class_id, hash) in types {
        if *class_id == 114 {
            bail!("a file without type trees holds no script class here");
        }
        meta.extend_from_slice(&class_id.to_le_bytes());
        meta.push(0);
        meta.extend_from_slice(&(-1i16).to_le_bytes());
        meta.extend_from_slice(hash);
    }
    meta.extend_from_slice(&(objects.len() as u32).to_le_bytes());
    let mut data = Vec::new();
    for (local_id, index, bytes) in objects {
        while !(HEADER + meta.len()).is_multiple_of(4) {
            meta.push(0);
        }
        while !data.len().is_multiple_of(8) {
            data.push(0);
        }
        meta.extend_from_slice(&local_id.to_le_bytes());
        meta.extend_from_slice(&(data.len() as u64).to_le_bytes());
        meta.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        meta.extend_from_slice(&(*index as u32).to_le_bytes());
        data.extend_from_slice(bytes);
    }
    meta.extend_from_slice(&0u32.to_le_bytes());
    meta.extend_from_slice(&(externals.len() as u32).to_le_bytes());
    for (path, guid) in externals {
        meta.push(0);
        meta.extend_from_slice(&serialized::engine_guid(guid));
        meta.extend_from_slice(&0i32.to_le_bytes());
        meta.extend_from_slice(path.as_bytes());
        meta.push(0);
    }
    meta.extend_from_slice(&0u32.to_le_bytes());
    meta.push(0);
    let mut data_offset = HEADER + meta.len();
    while !data_offset.is_multiple_of(16) {
        data_offset += 1;
    }
    let mut out = vec![0u8; HEADER];
    out[8..12].copy_from_slice(&format.version.to_be_bytes());
    out[20..24].copy_from_slice(&(meta.len() as u32).to_be_bytes());
    out[24..32].copy_from_slice(&((data_offset + data.len()) as u64).to_be_bytes());
    out[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
    out.extend_from_slice(&meta);
    out.resize(data_offset, 0);
    out.extend_from_slice(&data);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_of_6000_0_decodes_with_its_corrected_trees() {
        let pack = ManagerPack::of_release("6000.0.71f1").unwrap().unwrap();
        for class_id in [129, 310, 30, 141, 116, 94] {
            let manager = pack.manager(class_id).unwrap();
            let data = manager.tree.encode(&manager.value).unwrap();
            assert_eq!(manager.tree.decode(&data).unwrap(), manager.value);
        }
        let connect = pack.manager(310).unwrap();
        assert!(connect.value.field("InsightsSettings").is_none());
        assert!(ManagerPack::of_release("6000.1.0f1").unwrap().is_none());
    }

    #[test]
    fn the_capture_of_6000_6_decodes_with_its_corrected_trees() {
        let pack = ManagerPack::of_release("6000.6.3f1").unwrap().unwrap();
        for class_id in [129, 310, 30, 141, 116, 94, 176_606_843] {
            let manager = pack.manager(class_id).unwrap();
            let data = manager.tree.encode(&manager.value).unwrap();
            assert_eq!(manager.tree.decode(&data).unwrap(), manager.value);
        }
    }

    #[test]
    fn resources_are_sorted_by_name_with_aligned_names() {
        let data = resource_manager(&[("b".into(), (4, 9)), ("ab".into(), (4, 2))]);
        let mut expected = 2u32.to_le_bytes().to_vec();
        expected.extend_from_slice(&2u32.to_le_bytes());
        expected.extend_from_slice(b"ab\0\0");
        expected.extend_from_slice(&4i32.to_le_bytes());
        expected.extend_from_slice(&2i64.to_le_bytes());
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(b"b\0\0\0");
        expected.extend_from_slice(&4i32.to_le_bytes());
        expected.extend_from_slice(&9i64.to_le_bytes());
        expected.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(data, expected);
    }

    #[test]
    fn a_plain_file_reads_back_with_its_objects() {
        let file = plain_file(
            "6000.0.71f1",
            &[(5, [7; 16])],
            &[(8, 0, vec![1, 2, 3])],
            &MANAGER_EXTERNALS.map(|(path, guid)| (path.to_string(), guid.to_string())),
        )
        .unwrap();
        let parsed = managers::PlainFile::parse(&file).unwrap();
        assert_eq!(parsed.objects.len(), 1);
        assert_eq!(parsed.objects[0].local_id, 8);
        assert_eq!(parsed.objects[0].data, vec![1, 2, 3]);
        assert_eq!(parsed.classes(), &[5]);
    }
}
