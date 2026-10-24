//! One Player file holding a set of sources and everything they refer to,
//! written by the tool: the counterpart of the file of a scene's
//! dependencies that Unity's worker writes with `WriteSerializedFile`.
use super::{
    FileFormat, Object,
    authored::{Library, MONO_SCRIPT, Reference, Sources, Tags, convert},
    texture::{self, SPRITE},
    typetree::{TypeTree, Val},
};
use anyhow::{Context as _, Result, bail};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use ume_base::yaml::Document;

/// What was written and what was not.
#[derive(Default)]
pub struct Assembled {
    pub file: Vec<u8>,
    /// Streamed texture data, to be saved under the name given.
    pub stream: Vec<u8>,
    /// Encoded audio, to be saved beside it under the name with the
    /// extension `.resource` (`resource_name`).
    pub resource: Vec<u8>,
    /// Sources left out or written in part, with the reason.
    pub refused: Vec<(String, String)>,
    /// GUIDs of the sources other files hold, in external-reference order.
    pub externals: Vec<String>,
    pub sources: usize,
    /// How many objects took a field from the class's sample, by class or
    /// script and field: the fields their sources lack.
    pub derived: std::collections::BTreeMap<String, usize>,
    /// With a scene: its own objects in a file of their own, which refers to
    /// `file` first (`ume-deps`) and then to `scene_externals`.
    pub scene: Option<Vec<u8>>,
    pub scene_externals: Vec<String>,
    /// Each object of `file`: its local ID, its class's managed name, and the
    /// source it comes from (GUID and file ID).
    pub objects: Vec<(i64, String, String, i64)>,
    /// Of each root that is a prefab, the file ID of its root game object.
    pub roots: std::collections::BTreeMap<String, i64>,
}

/// The path a written file names an external reference by, as the worker's
/// files do: the engine's own resources by their place, any other source
/// (a shader, which the session publishes) as `ume-asset/<GUID>`.
pub fn external_path(guid: &str) -> String {
    match guid {
        BUILTIN_EXTRA => "Resources/unity_builtin_extra".into(),
        DEFAULT_RESOURCES => "Library/unity default resources".into(),
        other => format!("ume-asset/{other}"),
    }
}

/// The managed name of an engine class, as the worker reports its objects.
fn managed_class_name(type_name: &str) -> String {
    match type_name {
        "AnimatorController" => "UnityEditor.Animations.AnimatorController".into(),
        "AudioMixerController" | "AudioMixerGroupController" | "AudioMixerSnapshotController" => {
            format!("UnityEditor.Audio.{type_name}")
        }
        "MonoScript" => "UnityEditor.MonoScript".into(),
        "SpriteAtlas" => "UnityEngine.U2D.SpriteAtlas".into(),
        other => format!("UnityEngine.{other}"),
    }
}

/// Rewrites every pointer of a value.
fn map_pointers(value: &mut Val, map: &mut impl FnMut(i64, i64) -> (i64, i64)) {
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
                map_pointers(field, map);
            }
        }
        Val::Seq(items) => items.iter_mut().for_each(|item| map_pointers(item, map)),
        _ => {}
    }
}

pub fn write_closure(
    library: &Library,
    tags: &Tags,
    sources: &Sources,
    roots: &[&str],
    format: FileFormat,
    stream_name: &str,
    builtin_extra: Option<&Path>,
) -> Result<Assembled> {
    write_scene_closure(
        library,
        tags,
        sources,
        roots,
        None,
        format,
        stream_name,
        builtin_extra,
    )
}

enum Source {
    Yaml(Vec<Document>),
    /// A text or binary file a `TextScriptImporter` makes a `TextAsset` of.
    Text(PathBuf),
    /// An audio file an `AudioImporter` makes an `AudioClip` of.
    Audio(PathBuf, super::audio::Settings),
    Image(PathBuf, Vec<u8>),
    Script(String),
    /// A shader baked for the Player, with the sources it refers to.
    Shader(Vec<u8>, Vec<super::External>),
    /// A plugin's compiled assembly, by its name: the script classes
    /// references name in it by their file IDs.
    Assembly(String),
}

/// GUIDs of the engine's own resource files, which stay external.
fn engine_resource(guid: &str) -> bool {
    guid.starts_with("0000000000000000")
}

/// Bakes the shader of a GUID: its Player object and the sources it refers
/// to, in the order its pointers name them.
pub type ShaderBaker<'a> = &'a dyn Fn(&str) -> Result<(Vec<u8>, Vec<super::External>)>;

/// Writes the closure of `roots`; with `scene`, one of them, the scene's own
/// objects (and its lighting settings) go to a file of their own that refers
/// to the closure's, as a build writes a scene beside what it refers to.
#[allow(clippy::too_many_arguments)]
pub fn write_scene_closure(
    library: &Library,
    tags: &Tags,
    sources: &Sources,
    roots: &[&str],
    scene: Option<&str>,
    format: FileFormat,
    stream_name: &str,
    builtin_extra: Option<&Path>,
) -> Result<Assembled> {
    write_closure_baking(
        library,
        tags,
        sources,
        roots,
        scene,
        format,
        stream_name,
        builtin_extra,
        None,
        None,
    )
}

/// The `MonoScript`s of another file of a build, by class (`Namespace.Class`)
/// and local ID, and the path the file is named by: the scripts a build
/// keeps in `globalgamemanagers.assets`, which its other files refer to.
pub struct ScriptsElsewhere<'a> {
    pub path: &'a str,
    pub scripts: &'a HashMap<String, i64>,
}

/// How an external reference to another file of a build is told apart from
/// the GUID of a source.
const ELSEWHERE: &str = "file:";

/// The same, with the shaders the sources name baked into the file by
/// `shaders`, as a build holds them, where a session publishes them apart.
#[allow(clippy::too_many_arguments)]
pub fn write_closure_baking(
    library: &Library,
    tags: &Tags,
    sources: &Sources,
    roots: &[&str],
    scene: Option<&str>,
    format: FileFormat,
    stream_name: &str,
    builtin_extra: Option<&Path>,
    shaders: Option<ShaderBaker<'_>>,
    elsewhere: Option<ScriptsElsewhere<'_>>,
) -> Result<Assembled> {
    let mut assembled = Assembled::default();
    // Sources by GUID, in the order they are reached.
    let mut order: Vec<String> = Vec::new();
    let mut loaded: HashMap<String, Source> = HashMap::new();
    let mut queue: VecDeque<String> = roots.iter().map(|g| g.to_string()).collect();
    let mut seen: HashSet<String> = queue.iter().cloned().collect();
    let mut unpacked = super::prefab::Unpacked::default();
    let renames = sources.renames();
    // Sources the tool does not write, but shaders, which the session
    // publishes: references to them are written empty.
    let mut left_out: HashSet<String> = HashSet::new();
    // The package script of each class of the old `UnityEngine.UI.dll`.
    let mut legacy_ui: HashMap<i64, Option<String>> = HashMap::new();
    while let Some(guid) = queue.pop_front() {
        let Some(path) = sources.path(&guid) else {
            assembled
                .refused
                .push((guid, "no source with this GUID".into()));
            continue;
        };
        let baked = match (shaders, is_shader(path)) {
            (Some(bake), true) => Some(bake(&guid)),
            _ => None,
        };
        if let Some(baked) = baked {
            match baked {
                Ok((data, externals)) => {
                    for external in &externals {
                        if !engine_resource(&external.guid) && seen.insert(external.guid.clone()) {
                            queue.push_back(external.guid.clone());
                        }
                    }
                    order.push(guid.clone());
                    loaded.insert(guid, Source::Shader(data, externals));
                }
                Err(error) => {
                    left_out.insert(guid.clone());
                    assembled.refused.push((guid, format!("{error:#}")));
                }
            }
            continue;
        }
        let source = match load(path, &guid, sources).and_then(|source| match source {
            Some(Source::Yaml(documents)) => {
                let mut documents = unpacked.unpack(sources, documents)?;
                legacy_ui_scripts(&mut documents, library, sources, &mut legacy_ui);
                Ok(Some(Source::Yaml(documents)))
            }
            other => Ok(other),
        }) {
            Ok(Some(source)) => source,
            Ok(None) => {
                if !is_shader(path) {
                    left_out.insert(guid.clone());
                }
                assembled.refused.push((
                    guid,
                    format!("{} is not written by the tool", path.display()),
                ));
                continue;
            }
            Err(error) => {
                if !is_shader(path) {
                    left_out.insert(guid.clone());
                }
                assembled.refused.push((guid, format!("{error:#}")));
                continue;
            }
        };
        // What an unwritten object (an instance of a prefab, which unpacking
        // has merged) names is not a dependency, nor what a field the Player
        // lacks does (a font asset's `m_SourceFontFile_EditorRef`).
        if let Source::Yaml(documents) = &source {
            for document in documents.iter().filter(|d| written(d)) {
                for reference in dependencies(library, &renames, sources, document) {
                    if !reference.guid.is_empty()
                        && !engine_resource(&reference.guid)
                        && seen.insert(reference.guid.clone())
                    {
                        queue.push_back(reference.guid);
                    }
                }
            }
        }
        if let Source::Yaml(documents) = &source
            && roots.contains(&guid.as_str())
            && scene != Some(guid.as_str())
            && let Some(root) = root_game_object(documents)
        {
            assembled.roots.insert(guid.clone(), root);
        }
        order.push(guid.clone());
        loaded.insert(guid, source);
    }
    // The objects of the Editor's built-in resources the sources name are
    // written beside them, as a build copies them; their shaders stay
    // external.
    if let Some(path) = builtin_extra {
        let mut wanted: Vec<i64> = Vec::new();
        for source in loaded.values() {
            if let Source::Yaml(documents) = source {
                for document in documents.iter().filter(|d| written(d)) {
                    for reference in dependencies(library, &renames, sources, document) {
                        if reference.guid == BUILTIN_EXTRA {
                            wanted.push(reference.file_id);
                        }
                    }
                }
            }
        }
        if !wanted.is_empty() {
            match builtin_documents(path, &wanted) {
                Ok(documents) => {
                    order.push(BUILTIN_EXTRA.to_string());
                    loaded.insert(BUILTIN_EXTRA.to_string(), Source::Yaml(documents));
                }
                Err(error) => assembled
                    .refused
                    .push((BUILTIN_EXTRA.to_string(), format!("{error:#}"))),
            }
        }
    }
    // The file IDs references name in each plugin assembly.
    let mut assembly_ids: HashMap<String, std::collections::BTreeSet<i64>> = HashMap::new();
    for source in loaded.values() {
        if let Source::Yaml(documents) = source {
            for document in documents {
                for reference in references(&serde_json::Value::Object(document.data.clone())) {
                    if matches!(loaded.get(&reference.guid), Some(Source::Assembly(_))) {
                        assembly_ids
                            .entry(reference.guid.clone())
                            .or_default()
                            .insert(reference.file_id);
                    }
                }
            }
        }
    }
    // The script class of each (assembly, file ID) a reference names.
    let mut assembly_classes: HashMap<(String, i64), String> = HashMap::new();
    // Local IDs: every object of every source, the scripts' first, as the
    // worker numbers them; the Player then has a behaviour's script loaded
    // before the behaviour.
    let (scripts_first, rest): (Vec<&String>, Vec<&String>) = order
        .iter()
        .partition(|guid| matches!(loaded[*guid], Source::Script(_) | Source::Assembly(_)));
    let sequence: Vec<&String> = scripts_first.into_iter().chain(rest).collect();
    let mut ids: HashMap<(String, i64), i64> = HashMap::new();
    let mut next = 1i64;
    let mut images: HashMap<String, texture::Textured> = HashMap::new();
    let texture_kind = library.class(texture::TEXTURE_2D).cloned();
    let sprite_kind = library.class(SPRITE).cloned();
    // The clips, each encoded by a process of its own, side by side.
    let resource_name = match stream_name.strip_suffix(".resS") {
        Some(stem) => format!("{stem}.resource"),
        None => format!("{stream_name}.resource"),
    };
    let wanted: Vec<(&String, &PathBuf, &super::audio::Settings)> = order
        .iter()
        .filter_map(|guid| match &loaded[guid] {
            Source::Audio(path, settings) => Some((guid, path, settings)),
            _ => None,
        })
        .collect();
    let mut clips: HashMap<String, std::result::Result<super::audio::Encoded, String>> =
        HashMap::new();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    for batch in wanted.chunks(workers.max(1)) {
        let encoded: Vec<_> = std::thread::scope(|scope| {
            let running: Vec<_> = batch
                .iter()
                .map(|(_, path, settings)| {
                    scope.spawn(move || super::audio::encode(path, settings))
                })
                .collect();
            running
                .into_iter()
                .map(|thread| match thread.join() {
                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                    Err(_) => Err("the encoder panicked".to_string()),
                })
                .collect()
        });
        for ((guid, ..), result) in batch.iter().zip(encoded) {
            clips.insert((*guid).clone(), result);
        }
    }
    for &guid in &sequence {
        match &loaded[guid] {
            Source::Yaml(documents) => {
                for document in documents.iter().filter(|d| written(d)) {
                    ids.insert((guid.clone(), document.file_id.parse()?), next);
                    next += 1;
                }
            }
            Source::Image(path, meta) => {
                let Some(texture_kind) = &texture_kind else {
                    assembled
                        .refused
                        .push((guid.clone(), "no type tree of Texture2D".into()));
                    continue;
                };
                // The offset is settled when the data is appended.
                match texture::convert(
                    path,
                    meta,
                    guid,
                    texture_kind,
                    sprite_kind.as_ref(),
                    (stream_name, 0),
                ) {
                    Ok(textured) => {
                        for (file_id, _, _) in &textured.objects {
                            ids.insert((guid.clone(), *file_id), next);
                            next += 1;
                        }
                        images.insert(guid.clone(), textured);
                    }
                    Err(error) => assembled.refused.push((guid.clone(), format!("{error:#}"))),
                }
            }
            Source::Script(_) => {
                ids.insert((guid.clone(), 11500000), next);
                next += 1;
            }
            Source::Assembly(assembly) => {
                for file_id in assembly_ids.get(guid).into_iter().flatten() {
                    match library.assembly_script(assembly, *file_id) {
                        Some(name) => {
                            ids.insert((guid.clone(), *file_id), next);
                            next += 1;
                            assembly_classes.insert((guid.clone(), *file_id), name);
                        }
                        None => assembled.refused.push((
                            guid.clone(),
                            format!("no script class of {assembly} has file ID {file_id}"),
                        )),
                    }
                }
            }
            Source::Text(_) => {
                ids.insert((guid.clone(), TEXT_FILE_ID), next);
                next += 1;
            }
            Source::Shader(..) => {
                ids.insert((guid.clone(), SHADER_FILE_ID), next);
                next += 1;
            }
            Source::Audio(..) => match clips.get(guid) {
                Some(Ok(_)) => {
                    ids.insert((guid.clone(), super::audio::AUDIO_CLIP_FILE_ID), next);
                    next += 1;
                }
                Some(Err(error)) => assembled.refused.push((guid.clone(), error.clone())),
                None => {}
            },
        }
    }
    let mut shader_textures = ShaderTextures::default();
    let mut externals: Vec<String> = Vec::new();
    let mut objects: Vec<(i64, Arc<TypeTree>, Val, Option<i64>)> = Vec::new();
    let mut held_of: HashMap<i64, Vec<(String, String, String)>> = HashMap::new();
    let script_tree = library
        .class(MONO_SCRIPT)
        .context("no type tree of MonoScript")?
        .tree
        .clone();
    for &guid in &sequence {
        match &loaded[guid] {
            Source::Script(name) => {
                let Some(value) = library.script_object(name) else {
                    assembled
                        .refused
                        .push((guid.clone(), format!("no MonoScript of {name}")));
                    continue;
                };
                objects.push((
                    ids[&(guid.clone(), 11500000)],
                    script_tree.clone(),
                    value.clone(),
                    None,
                ));
            }
            Source::Assembly(_) => {
                let mut classes: Vec<(&i64, &String)> = assembly_classes
                    .iter()
                    .filter(|((owner, _), _)| owner == guid)
                    .map(|((_, file_id), name)| (file_id, name))
                    .collect();
                classes.sort();
                for (file_id, name) in classes {
                    let Some(value) = library.script_object(name) else {
                        assembled
                            .refused
                            .push((guid.clone(), format!("no MonoScript of {name}")));
                        continue;
                    };
                    objects.push((
                        ids[&(guid.clone(), *file_id)],
                        script_tree.clone(),
                        value.clone(),
                        None,
                    ));
                }
            }
            Source::Audio(path, settings) => {
                let Some(Ok(clip)) = clips.remove(guid) else {
                    continue;
                };
                let Some(kind) = library.class(AUDIO_CLIP) else {
                    assembled
                        .refused
                        .push((guid.clone(), "no type tree of AudioClip".into()));
                    continue;
                };
                let offset = align_to(&mut assembled.resource, 32);
                assembled.resource.extend_from_slice(&clip.data);
                let mut value = kind.sample.clone();
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(field) = value.field_mut("m_Name") {
                    *field = Val::Str(name);
                }
                let length = clip.samples as f32 / clip.frequency as f32;
                for (field, number) in [
                    ("m_LoadType", settings.load_type as f64),
                    ("m_Channels", clip.channels as f64),
                    ("m_Frequency", clip.frequency as f64),
                    ("m_BitsPerSample", 16.0),
                    ("m_Length", length as f64),
                    ("m_IsTrackerFormat", 0.0),
                    ("m_Ambisonic", settings.ambisonic as i64 as f64),
                    ("m_SubsoundIndex", 0.0),
                    ("m_PreloadAudioData", settings.preload as i64 as f64),
                    (
                        "m_LoadInBackground",
                        settings.load_in_background as i64 as f64,
                    ),
                    ("m_Legacy3D", settings.legacy_3d as i64 as f64),
                    ("m_CompressionFormat", settings.compression as f64),
                ] {
                    set_number(&mut value, field, number);
                }
                if let Some(Val::Map(fields)) = value.field_mut("m_Resource") {
                    for (name, field) in fields.iter_mut() {
                        match name.as_ref() {
                            "m_Source" => *field = Val::Str(resource_name.clone()),
                            "m_Offset" => *field = Val::U(offset as u64),
                            "m_Size" => *field = Val::U(clip.data.len() as u64),
                            _ => {}
                        }
                    }
                }
                objects.push((
                    ids[&(guid.clone(), super::audio::AUDIO_CLIP_FILE_ID)],
                    kind.tree.clone(),
                    value,
                    None,
                ));
            }
            Source::Shader(data, shader_externals) => {
                let Some(kind) = library.class(SHADER) else {
                    assembled
                        .refused
                        .push((guid.clone(), "no type tree of Shader".into()));
                    continue;
                };
                let mut value = kind.tree.decode(data)?;
                // What the shader names lies in this file, or in another.
                let mut failed = None;
                map_pointers(&mut value, &mut |file, path| {
                    if file == 0 {
                        return (0, path);
                    }
                    let Some(external) = shader_externals.get(file as usize - 1) else {
                        failed = Some(format!(
                            "a pointer into file {file}, which it does not name"
                        ));
                        return (0, 0);
                    };
                    if let Some(id) = ids.get(&(external.guid.clone(), path)) {
                        return (0, *id);
                    }
                    if !engine_resource(&external.guid) {
                        return (0, 0);
                    }
                    let position = match externals.iter().position(|g| *g == external.guid) {
                        Some(position) => position,
                        None => {
                            externals.push(external.guid.clone());
                            externals.len() - 1
                        }
                    };
                    (position as i64 + 1, path)
                });
                if let Some(why) = failed {
                    assembled.refused.push((guid.clone(), why));
                    continue;
                }
                objects.push((
                    ids[&(guid.clone(), SHADER_FILE_ID)],
                    kind.tree.clone(),
                    value,
                    None,
                ));
            }
            Source::Text(path) => {
                let Some(kind) = library.class(TEXT_ASSET) else {
                    assembled
                        .refused
                        .push((guid.clone(), "no type tree of TextAsset".into()));
                    continue;
                };
                let mut value = kind.sample.clone();
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(field) = value.field_mut("m_Name") {
                    *field = Val::Str(name);
                }
                if let Some(field) = value.field_mut("m_Script") {
                    *field = Val::Bytes(std::fs::read(path)?);
                }
                objects.push((
                    ids[&(guid.clone(), TEXT_FILE_ID)],
                    kind.tree.clone(),
                    value,
                    None,
                ));
            }
            Source::Image(..) => {
                let Some(textured) = images.remove(guid) else {
                    continue;
                };
                // A readable texture keeps its pixels in its object.
                let offset = match textured.data.is_empty() {
                    true => 0,
                    false => align(&mut assembled.stream),
                };
                assembled.stream.extend_from_slice(&textured.data);
                let texture_id = ids[&(guid.clone(), texture::TEXTURE_FILE_ID)];
                for (file_id, class, mut value) in textured.objects {
                    if class == texture::TEXTURE_2D
                        && let Some(Val::Map(fields)) = value.field_mut("m_StreamData")
                    {
                        for (name, field) in fields.iter_mut() {
                            if name == "offset" {
                                *field = Val::U(offset as u64);
                            }
                        }
                    }
                    if class == SPRITE
                        && let Some(render) = value.field_mut("m_RD")
                        && let Some(pointer) = render.field_mut("texture")
                    {
                        *pointer = pointer_to(0, texture_id);
                    }
                    let tree = library.class(class).context("no type tree")?.tree.clone();
                    objects.push((ids[&(guid.clone(), file_id)], tree, value, None));
                }
            }
            Source::Yaml(documents) => {
                let mut failed = None;
                for document in documents.iter().filter(|d| written(d)) {
                    let script = if document.class_id == 114 {
                        document
                            .data
                            .get("m_Script")
                            .and_then(Reference::from_value)
                            .and_then(|r| {
                                assembly_classes
                                    .get(&(r.guid.clone(), r.file_id))
                                    .cloned()
                                    .or_else(|| sources.script_class(&r.guid))
                            })
                    } else {
                        None
                    };
                    // An asset's own script object whose script the Player lacks
                    // (URP's Editor-only `AssetVersion` beside a material) is left
                    // out, as a build leaves it out.
                    if document.class_id == 114
                        && script.is_none()
                        && document
                            .data
                            .get("m_GameObject")
                            .and_then(Reference::from_value)
                            .is_none_or(|owner| owner.file_id == 0)
                    {
                        continue;
                    }
                    let mut resolve = |reference: &Reference| -> Result<Option<(i32, i64)>> {
                        let owner = if reference.guid.is_empty() {
                            guid
                        } else {
                            &reference.guid
                        };
                        if let Some(id) = ids.get(&(owner.clone(), reference.file_id)) {
                            return Ok(Some((0, *id)));
                        }
                        // A reference to an asset the project lacks is null,
                        // as the worker writes it (the engine's own files
                        // have no source), and so is one to a source left
                        // out or to an object a written source lacks.
                        if (sources.path(owner).is_none() && !engine_resource(owner))
                            || left_out.contains(owner)
                            || (loaded.contains_key(owner) && !engine_resource(owner))
                        {
                            return Ok(None);
                        }
                        let position = match externals.iter().position(|g| g == owner) {
                            Some(position) => position,
                            None => {
                                externals.push(owner.clone());
                                externals.len() - 1
                            }
                        };
                        Ok(Some((position as i32 + 1, reference.file_id)))
                    };
                    match convert(
                        library,
                        tags,
                        &renames,
                        document,
                        script.as_deref(),
                        &mut resolve,
                    ) {
                        Ok(mut converted) => {
                            let class = script
                                .clone()
                                .unwrap_or_else(|| converted.tree.nodes[0].type_name.clone());
                            for field in &converted.derived {
                                *assembled
                                    .derived
                                    .entry(format!("{class} {field}"))
                                    .or_default() += 1;
                            }
                            if document.class_id == MATERIAL
                                && let Some(shader) = document
                                    .data
                                    .get("m_Shader")
                                    .and_then(Reference::from_value)
                                && let Some(names) =
                                    shader_textures.get(sources, builtin_extra, &shader)
                            {
                                keep_textures(&mut converted.value, &names);
                            }
                            // A texture's pixels go to the streamed data, as a
                            // build writes them, but a readable one's.
                            let readable = matches!(
                                converted.value.field("m_IsReadable"),
                                Some(Val::Bool(true) | Val::U(1) | Val::I(1))
                            );
                            if document.class_id == texture::TEXTURE_2D as i64
                                && !readable
                                && let Some(Val::Bytes(pixels)) = converted
                                    .value
                                    .field_mut("image data")
                                    .map(|field| std::mem::replace(field, Val::Bytes(Vec::new())))
                            {
                                let offset = align(&mut assembled.stream);
                                assembled.stream.extend_from_slice(&pixels);
                                if let Some(field) = converted.value.field_mut("image data") {
                                    *field = Val::Bytes(Vec::new());
                                }
                                if let Some(field) = converted.value.field_mut("m_StreamData") {
                                    *field = Val::Map(vec![
                                        (Cow::Borrowed("offset"), Val::U(offset as u64)),
                                        (Cow::Borrowed("size"), Val::U(pixels.len() as u64)),
                                        (Cow::Borrowed("path"), Val::Str(stream_name.to_string())),
                                    ]);
                                }
                            }
                            // A built-in sprite's render data is the built-in
                            // resources' object, by its file ID.
                            if guid == BUILTIN_EXTRA
                                && document.class_id == SPRITE as i64
                                && let Some(words) = super::managers::guid_words(BUILTIN_EXTRA)
                                && let Some(key) = converted.value.field_mut("m_RenderDataKey")
                            {
                                *key = Val::Map(vec![
                                    (
                                        Cow::Borrowed("first"),
                                        Val::Map(
                                            words
                                                .iter()
                                                .enumerate()
                                                .map(|(i, w)| {
                                                    (
                                                        Cow::Owned(format!("data[{i}]")),
                                                        Val::U(*w as u64),
                                                    )
                                                })
                                                .collect(),
                                        ),
                                    ),
                                    (Cow::Borrowed("second"), Val::I(document.file_id.parse()?)),
                                ]);
                            }
                            let script_id = document
                                .data
                                .get("m_Script")
                                .and_then(Reference::from_value)
                                .filter(|_| document.class_id == 114)
                                .and_then(|r| ids.get(&(r.guid, r.file_id)).copied());
                            let id = ids[&(guid.clone(), document.file_id.parse()?)];
                            if !converted.held.is_empty() {
                                held_of.insert(id, converted.held);
                            }
                            objects.push((id, converted.tree, converted.value, script_id));
                        }
                        Err(error) => {
                            failed.get_or_insert(format!(
                                "{} &{}: {error:#}",
                                document.type_name, document.file_id
                            ));
                        }
                    }
                }
                if let Some(why) = failed {
                    assembled.refused.push((guid.clone(), why));
                }
            }
        }
    }
    // Scripts another file of the build holds are taken from it.
    let mut moved: HashMap<i64, (i64, i64)> = HashMap::new();
    let mut moved_names: HashMap<i64, String> = HashMap::new();
    if let Some(elsewhere) = &elsewhere {
        let marker = format!("{ELSEWHERE}{}", elsewhere.path);
        for (id, tree, value, _) in &objects {
            if tree.class_id != MONO_SCRIPT {
                continue;
            }
            let text = |name: &str| match value.field(name) {
                Some(Val::Str(text)) => text.clone(),
                _ => String::new(),
            };
            let name = super::full_name(&text("m_Namespace"), &text("m_ClassName"));
            if let Some(&other) = elsewhere.scripts.get(&name) {
                let position = match externals.iter().position(|g| *g == marker) {
                    Some(position) => position,
                    None => {
                        externals.push(marker.clone());
                        externals.len() - 1
                    }
                };
                moved.insert(*id, (position as i64 + 1, other));
                moved_names.insert(*id, name);
            }
        }
        objects.retain(|(id, ..)| !moved.contains_key(id));
        for (_, _, value, _) in objects.iter_mut() {
            map_pointers(value, &mut |file, path| match (file, moved.get(&path)) {
                (0, Some(&elsewhere)) => elsewhere,
                _ => (file, path),
            });
        }
    }
    // A reference to an object that was not written is empty, as a build
    // leaves a reference to a missing object.
    let present: HashSet<i64> = objects.iter().map(|(id, ..)| *id).collect();
    for (_, tree, value, _) in objects.iter_mut() {
        clear_missing(value, &present);
        // A transform lists no child that was not written (the root of a
        // missing prefab's instance): the Player follows every entry.
        if matches!(tree.class_id, 4 | 224)
            && let Some(Val::Seq(children)) = value.field_mut("m_Children")
        {
            children.retain(|child| {
                !matches!(child.field("m_PathID"), Some(Val::I(0)))
                    || !matches!(child.field("m_FileID"), Some(Val::I(0)))
            });
        }
    }
    // Where each object comes from.
    let origin: HashMap<i64, (String, i64)> = ids
        .iter()
        .map(|((guid, file_id), local)| (*local, (guid.clone(), *file_id)))
        .collect();
    // The class of a script-backed object, by its MonoScript's names.
    let mut script_names: HashMap<i64, String> = moved_names;
    script_names.extend(
        objects
            .iter()
            .filter(|(_, tree, ..)| tree.class_id == MONO_SCRIPT)
            .map(|(id, _, value, _)| {
                let text = |name: &str| match value.field(name) {
                    Some(Val::Str(text)) => text.clone(),
                    _ => String::new(),
                };
                (
                    *id,
                    super::full_name(&text("m_Namespace"), &text("m_ClassName")),
                )
            }),
    );
    let in_scene = |id: i64, tree: &TypeTree| -> bool {
        scene.is_some_and(|scene| {
            origin.get(&id).is_some_and(|(guid, _)| guid == scene)
                || tree.class_id == LIGHTING_SETTINGS
        })
    };
    let (mut scene_objects, objects): (Vec<_>, Vec<_>) = objects
        .into_iter()
        .partition(|(id, tree, ..)| in_scene(*id, tree));
    let paths = |guids: &[String]| -> Vec<super::External> {
        guids
            .iter()
            .map(|guid| match guid.strip_prefix(ELSEWHERE) {
                Some(path) => super::External {
                    guid: String::new(),
                    path: path.to_string(),
                },
                None => super::External {
                    guid: guid.clone(),
                    path: external_path(guid),
                },
            })
            .collect()
    };
    let deps: HashSet<i64> = objects.iter().map(|(id, ..)| *id).collect();
    let mut objects = objects;
    // What the closure keeps of the scene is left out of it.
    for (_, _, value, _) in objects.iter_mut() {
        map_pointers(
            value,
            &mut |file, path| match file == 0 && !deps.contains(&path) {
                true => (0, 0),
                false => (file, path),
            },
        );
    }
    // The classes each object holds by managed reference, with their trees.
    let held_trees: HashMap<i64, Vec<super::typetree::ReferenceTree>> = held_of
        .iter()
        .map(|(id, held)| {
            let trees = held
                .iter()
                .filter_map(|(class, namespace, assembly)| {
                    let kind = library.managed(class, namespace, assembly)?;
                    Some((
                        class.clone(),
                        namespace.clone(),
                        assembly.clone(),
                        kind.tree.clone(),
                    ))
                })
                .collect();
            (*id, trees)
        })
        .collect();
    let no_trees = Vec::new();
    let trees_of = |id: &i64| held_trees.get(id).unwrap_or(&no_trees);
    let held_types = |objects: &[(i64, Arc<TypeTree>, Val, Option<i64>)]| {
        objects
            .iter()
            .map(|(id, ..)| {
                trees_of(id)
                    .iter()
                    .map(|(class, namespace, assembly, tree)| super::ReferenceType {
                        class_name: class,
                        namespace,
                        assembly,
                        blob: tree.blob(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let written: Vec<Object<'_>> = objects
        .iter()
        .map(|(local_id, tree, value, script)| {
            let (script_file, script) = match script.and_then(|id| moved.get(&id)) {
                Some(&(file, other)) => (file as i32, Some(other)),
                None => (0, *script),
            };
            Ok(Object {
                local_id: *local_id,
                tree,
                data: tree.encode_with(value, trees_of(local_id))?,
                script,
                script_file,
            })
        })
        .collect::<Result<_>>()?;
    assembled.file = super::write_in_with_references(
        format,
        &written,
        &paths(&externals),
        &held_types(&objects),
    )?;
    assembled.objects = objects
        .iter()
        .filter_map(|(id, tree, _, script)| {
            let (guid, file_id) = origin.get(id)?;
            let class = match script {
                Some(script) => script_names.get(script).cloned().unwrap_or_default(),
                None => managed_class_name(&tree.nodes[0].type_name),
            };
            Some((*id, class, guid.clone(), *file_id))
        })
        .collect();
    if scene.is_some() {
        // The scene's objects, numbered from 1 in the order a build writes
        // them: by class, the script-backed last.
        scene_objects.sort_by_key(|(id, tree, ..)| {
            let class = tree.class_id;
            (
                class == 114,
                if class == LIGHTING_SETTINGS {
                    i32::MAX
                } else {
                    class
                },
                *id,
            )
        });
        let local: HashMap<i64, i64> = scene_objects
            .iter()
            .enumerate()
            .map(|(index, (id, ..))| (*id, index as i64 + 1))
            .collect();
        // The closure's file is the scene's first external reference.
        let mut scene_externals: Vec<String> = Vec::new();
        for (_, _, value, _) in scene_objects.iter_mut() {
            map_pointers(value, &mut |file, path| match file {
                0 if path == 0 => (0, 0),
                0 => match local.get(&path) {
                    Some(own) => (0, *own),
                    None if deps.contains(&path) => (1, path),
                    None => (0, 0),
                },
                n => {
                    let guid = &externals[n as usize - 1];
                    let position = match scene_externals.iter().position(|known| known == guid) {
                        Some(position) => position,
                        None => {
                            scene_externals.push(guid.clone());
                            scene_externals.len() - 1
                        }
                    };
                    (position as i64 + 2, path)
                }
            });
        }
        let written: Vec<Object<'_>> = scene_objects
            .iter()
            .map(|(id, tree, value, script)| {
                Ok(Object {
                    local_id: local[id],
                    tree,
                    data: tree.encode_with(value, trees_of(id))?,
                    script: *script,
                    script_file: script.map_or(0, |_| 1),
                })
            })
            .collect::<Result<_>>()?;
        let mut references = vec![super::External {
            guid: String::new(),
            path: "ume-deps".into(),
        }];
        references.extend(paths(&scene_externals));
        assembled.scene = Some(super::write_in_with_references(
            format,
            &written,
            &references,
            &held_types(&scene_objects),
        )?);
        assembled.scene_externals = scene_externals;
    }
    assembled.externals = externals;
    assembled.sources = order.len();
    Ok(assembled)
}

/// A scene's lighting settings, which a build writes into the scene.
const LIGHTING_SETTINGS: i32 = 850_595_691;

const PREFAB_INSTANCE: i64 = 1001;
const MATERIAL: i64 = 21;

/// The texture properties each shader declares, read once.
#[derive(Default)]
struct ShaderTextures {
    builtin: Option<HashMap<i64, HashSet<String>>>,
    project: HashMap<String, Option<HashSet<String>>>,
}

impl ShaderTextures {
    fn get(
        &mut self,
        sources: &Sources,
        builtin_extra: Option<&Path>,
        shader: &Reference,
    ) -> Option<HashSet<String>> {
        if shader.guid == BUILTIN_EXTRA {
            let builtin = self.builtin.get_or_insert_with(|| {
                builtin_extra
                    .and_then(|path| builtin_shader_textures(path).ok())
                    .unwrap_or_default()
            });
            return builtin.get(&shader.file_id).cloned();
        }
        self.project
            .entry(shader.guid.clone())
            .or_insert_with(|| {
                let path = sources.path(&shader.guid)?;
                if path.extension().is_none_or(|e| e != "shader") {
                    return None;
                }
                Some(shader_source_textures(&std::fs::read_to_string(path).ok()?))
            })
            .clone()
    }
}

/// A build drops the textures of a material its shader does not declare,
/// so that they do not come along into the build.
fn keep_textures(material: &mut Val, names: &HashSet<String>) {
    let Some(properties) = material.field_mut("m_SavedProperties") else {
        return;
    };
    if let Some(Val::Seq(textures)) = properties.field_mut("m_TexEnvs") {
        textures.retain(|entry| match entry.field("first") {
            Some(Val::Str(name)) => names.contains(name),
            Some(Val::Bytes(name)) => names.contains(&*String::from_utf8_lossy(name)),
            _ => true,
        });
    }
}

/// The texture properties of the built-in shaders, by local ID.
fn builtin_shader_textures(path: &Path) -> Result<HashMap<i64, HashSet<String>>> {
    let file = std::fs::read(path)?;
    let objects = super::read_serialized_of_class(&file, SHADER)?;
    let mut result = HashMap::new();
    for object in objects {
        let Ok(value) = object.tree.decode(&object.data) else {
            continue;
        };
        let props = value
            .field("m_ParsedForm")
            .and_then(|f| f.field("m_PropInfo"))
            .and_then(|f| f.field("m_Props"));
        let Some(Val::Seq(props)) = props else {
            continue;
        };
        let names = props
            .iter()
            .filter(|p| matches!(p.field("m_Type"), Some(Val::I(4)) | Some(Val::U(4))))
            .filter_map(|p| match p.field("m_Name") {
                Some(Val::Str(name)) => Some(name.clone()),
                Some(Val::Bytes(name)) => Some(String::from_utf8_lossy(name).into_owned()),
                _ => None,
            })
            .collect();
        result.insert(object.local_id, names);
    }
    Ok(result)
}

/// The texture properties a ShaderLab source declares in its `Properties`
/// block: `_Name ("Label", 2D) = "white" {}` and the like.
fn shader_source_textures(source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(start) = source.find("Properties") else {
        return names;
    };
    let Some(open) = source[start..].find('{') else {
        return names;
    };
    let mut depth = 0;
    let mut end = source.len();
    for (offset, c) in source[start + open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = start + open + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    for line in source[start + open + 1..end].lines() {
        // Attributes such as [NoScaleOffset] come before the name.
        let mut rest = line.trim();
        while let Some(stripped) = rest.strip_prefix('[') {
            rest = stripped.split_once(']').map_or("", |(_, r)| r).trim_start();
        }
        let Some((name, declaration)) = rest.split_once('(') else {
            continue;
        };
        let kind = declaration
            .rsplit_once(',')
            .map(|(_, kind)| kind.split(')').next().unwrap_or("").trim())
            .unwrap_or("");
        if matches!(
            kind,
            "2D" | "3D" | "Cube" | "2DArray" | "CubeArray" | "Any" | "Rect"
        ) {
            names.insert(name.trim().to_string());
        }
    }
    names
}

pub const BUILTIN_EXTRA: &str = "0000000000000000f000000000000000";
const DEFAULT_RESOURCES: &str = "0000000000000000e000000000000000";
const SHADER: i32 = 48;
/// The file ID of the `Shader` of a `.shader` source.
const SHADER_FILE_ID: i64 = 4_800_000;

/// The wanted objects of the Editor's `unity_builtin_extra` and those they
/// refer to in it, but shaders, as documents in the form of authored YAML.
fn builtin_documents(path: &Path, wanted: &[i64]) -> Result<Vec<Document>> {
    let file = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (objects, externals) = super::read_serialized_with_externals(&file)?;
    let by_id: HashMap<i64, &super::Read> = objects.iter().map(|o| (o.local_id, o)).collect();
    let mut pending: Vec<i64> = wanted.to_vec();
    let mut taken: HashSet<i64> = HashSet::new();
    let mut documents = Vec::new();
    while let Some(id) = pending.pop() {
        let Some(object) = by_id.get(&id) else {
            continue;
        };
        if object.tree.class_id == SHADER || !taken.insert(id) {
            continue;
        }
        let value = object.tree.decode(&object.data)?;
        let serde_json::Value::Object(data) = yaml_form(&value, &externals) else {
            bail!("built-in object {id} is not a structure");
        };
        for reference in references(&serde_json::Value::Object(data.clone())) {
            if reference.guid == BUILTIN_EXTRA {
                pending.push(reference.file_id);
            }
        }
        documents.push(Document {
            class_id: object.tree.class_id as i64,
            file_id: id.to_string(),
            type_name: object.tree.nodes[0].type_name.clone(),
            data,
            stripped: false,
        });
    }
    Ok(documents)
}

/// A decoded object in the form the YAML parser gives authored values:
/// scalars as their text, references as `{fileID, guid}`, typeless data as
/// its size with the bytes in hexadecimal beside it.
fn yaml_form(value: &Val, externals: &[String]) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Val::Zero => Value::String("0".into()),
        Val::Bool(flag) => Value::String((*flag as u8).to_string()),
        Val::I(n) => Value::String(n.to_string()),
        Val::U(n) => Value::String(n.to_string()),
        Val::F(f) => Value::String(f.to_string()),
        Val::D(d) => Value::String(d.to_string()),
        Val::Str(text) => Value::String(text.clone()),
        Val::Bytes(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        Val::Seq(items) => Value::Array(items.iter().map(|i| yaml_form(i, externals)).collect()),
        Val::Map(fields) => {
            let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
            if let (2, Some(file), Some(Val::I(id))) =
                (fields.len(), get("m_FileID"), get("m_PathID"))
            {
                let file = match file {
                    Val::I(n) => *n,
                    _ => 0,
                };
                let guid = match file {
                    0 => BUILTIN_EXTRA,
                    n if externals
                        .get(n as usize - 1)
                        .is_some_and(|p| p.contains("unity default resources")) =>
                    {
                        DEFAULT_RESOURCES
                    }
                    _ => "",
                };
                let mut reference = serde_json::Map::new();
                reference.insert("fileID".into(), Value::String(id.to_string()));
                if *id != 0 {
                    reference.insert("guid".into(), Value::String(guid.into()));
                }
                return Value::Object(reference);
            }
            // A GUID is written as its 32 digits, each word's lowest nibble
            // first.
            if fields.len() == 4
                && fields
                    .iter()
                    .enumerate()
                    .all(|(i, (k, _))| *k == format!("data[{i}]"))
            {
                let digits: String = fields
                    .iter()
                    .map(|(_, word)| {
                        let word = match word {
                            Val::U(n) => *n as u32,
                            Val::I(n) => *n as u32,
                            _ => 0,
                        };
                        format!("{word:08x}").chars().rev().collect::<String>()
                    })
                    .collect();
                return Value::String(digits);
            }
            let mut map = serde_json::Map::new();
            for (name, field) in fields {
                // A texture's pixels and a mesh's vertex data are written as
                // their size, the bytes beside it.
                if let (true, Val::Bytes(bytes)) =
                    (name == "image data" || name == "m_DataSize", field)
                {
                    map.insert(name.to_string(), Value::String(bytes.len().to_string()));
                    map.insert(
                        "_typelessdata".into(),
                        Value::String(bytes.iter().map(|b| format!("{b:02x}")).collect()),
                    );
                    continue;
                }
                map.insert(name.to_string(), yaml_form(field, externals));
            }
            Value::Object(map)
        }
    }
}
const TEXT_ASSET: i32 = 49;
const AUDIO_CLIP: i32 = 83;
const TEXT_FILE_ID: i64 = 4900000;

fn clear_missing(value: &mut Val, present: &HashSet<i64>) {
    match value {
        Val::Map(fields) => {
            let file = fields
                .iter()
                .find(|(k, _)| k == "m_FileID")
                .map(|(_, v)| v.clone());
            if fields.len() == 2
                && file == Some(Val::I(0))
                && let Some((_, Val::I(id))) = fields.iter_mut().find(|(k, _)| k == "m_PathID")
            {
                if !present.contains(id) {
                    *id = 0;
                }
                return;
            }
            for (_, field) in fields.iter_mut() {
                clear_missing(field, present);
            }
        }
        Val::Seq(items) => items
            .iter_mut()
            .for_each(|item| clear_missing(item, present)),
        _ => {}
    }
}

/// Documents a build writes: not the prefab instance records and their
/// stripped stand-ins, which a build resolves into the objects they stand
/// for.
/// A scene's occlusion culling settings and its list of roots are the
/// Editor's, which a build leaves out of the scene.
const OCCLUSION_CULLING_SETTINGS: i64 = 29;
const SCENE_ROOTS: i64 = 1_660_057_539;

fn written(document: &Document) -> bool {
    !document.stripped
        && ![PREFAB_INSTANCE, OCCLUSION_CULLING_SETTINGS, SCENE_ROOTS].contains(&document.class_id)
}

fn align_to(stream: &mut Vec<u8>, alignment: usize) -> usize {
    while !stream.len().is_multiple_of(alignment) {
        stream.push(0);
    }
    stream.len()
}

/// Sets a number field of a sample in the kind of number it holds.
fn set_number(value: &mut Val, name: &str, number: f64) {
    if let Some(field) = value.field_mut(name) {
        *field = match field {
            Val::Bool(_) => Val::Bool(number != 0.0),
            Val::U(_) => Val::U(number as u64),
            Val::F(_) => Val::F(number as f32),
            Val::D(_) => Val::D(number),
            _ => Val::I(number as i64),
        };
    }
}

fn align(stream: &mut Vec<u8>) -> usize {
    while !stream.len().is_multiple_of(16) {
        stream.push(0);
    }
    stream.len()
}

fn pointer_to(file: i32, local: i64) -> Val {
    Val::Map(vec![
        (Cow::Borrowed("m_FileID"), Val::I(file as i64)),
        (Cow::Borrowed("m_PathID"), Val::I(local)),
    ])
}

fn load(path: &Path, guid: &str, sources: &Sources) -> Result<Option<Source>> {
    if let Some(documents) = sources.imported(guid) {
        return Ok(Some(Source::Yaml(documents.clone())));
    }
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if extension == "cs" {
        return Ok(sources.script_class(guid).map(Source::Script));
    }
    let meta = std::fs::read(format!("{}.meta", path.display()))?;
    if extension == "dll" && meta.windows(15).any(|w| w == b"PluginImporter:") {
        let name = path.file_stem().map(|n| n.to_string_lossy().into_owned());
        return Ok(name.map(Source::Assembly));
    }
    if meta.windows(21).any(|w| w == b"TrueTypeFontImporter:") {
        let settings = super::font::importer_settings(&meta, "TrueTypeFontImporter")?;
        return Ok(Some(Source::Yaml(super::font::documents(path, &settings)?)));
    }
    if meta.windows(19).any(|w| w == b"TextScriptImporter:") {
        return Ok(Some(Source::Text(path.to_path_buf())));
    }
    if meta.windows(14).any(|w| w == b"AudioImporter:") {
        let settings = super::audio::settings(&meta)?;
        return Ok(Some(Source::Audio(path.to_path_buf(), settings)));
    }
    if meta.windows(16).any(|w| w == b"TextureImporter:") {
        return Ok(Some(Source::Image(path.to_path_buf(), meta)));
    }
    let data = std::fs::read(path)?;
    if data.starts_with(b"%YAML") {
        return Ok(Some(Source::Yaml(
            ume_base::yaml::parse_unity_yaml_lexical(&data)?,
        )));
    }
    Ok(None)
}

/// The GUIDs of the `UnityEngine.UI.dll` that held uGUI's scripts before
/// uGUI was a package (Unity 2019.2), which prefabs made then still name:
/// that of the Editor's extension of Unity 5 (`GUISystem`) and the later one.
const LEGACY_UI_ASSEMBLIES: [&str; 2] = [
    "f5f67c52d1564df4a8936ccd202a3bd8",
    "f70555f144d8491a825f0804e09c671c",
];

/// Points the references of documents to scripts of the old
/// `UnityEngine.UI.dll` at the package's scripts of the same classes, as the
/// Editor finds them: the class whose file ID the reference names, in the
/// package's assembly.
fn legacy_ui_scripts(
    documents: &mut [Document],
    library: &Library,
    sources: &Sources,
    known: &mut HashMap<i64, Option<String>>,
) {
    fn visit(
        value: &mut serde_json::Value,
        library: &Library,
        sources: &Sources,
        known: &mut HashMap<i64, Option<String>>,
    ) {
        match value {
            serde_json::Value::Object(fields) => {
                if fields
                    .get("guid")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|guid| LEGACY_UI_ASSEMBLIES.contains(&guid))
                    && let Some(file_id) = fields
                        .get("fileID")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| id.parse::<i64>().ok())
                {
                    let script = known
                        .entry(file_id)
                        .or_insert_with(|| {
                            let class = library.assembly_script("UnityEngine.UI", file_id)?;
                            sources.script_guid(&class)
                        })
                        .clone();
                    if let Some(guid) = script {
                        fields.insert("guid".into(), serde_json::Value::String(guid));
                        fields.insert(
                            "fileID".into(),
                            serde_json::Value::String("11500000".into()),
                        );
                    }
                    return;
                }
                for field in fields.values_mut() {
                    visit(field, library, sources, known);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item, library, sources, known);
                }
            }
            _ => {}
        }
    }
    for document in documents {
        for value in document.data.values_mut() {
            visit(value, library, sources, known);
        }
    }
}

fn is_shader(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("shader"))
}

/// The game object of a prefab's transform that has no parent.
fn root_game_object(documents: &[Document]) -> Option<i64> {
    let transform = documents.iter().filter(|d| written(d)).find(|d| {
        (d.class_id == 4 || d.class_id == 224)
            && d.data
                .get("m_Father")
                .and_then(Reference::from_value)
                .is_none_or(|father| father.file_id == 0)
    })?;
    Some(
        transform
            .data
            .get("m_GameObject")
            .and_then(Reference::from_value)?
            .file_id,
    )
}

/// The references of the fields of a document the Player reads.
fn dependencies(
    library: &Library,
    renames: &super::authored::Renames,
    sources: &Sources,
    document: &Document,
) -> Vec<Reference> {
    let script = if document.class_id == 114 {
        document
            .data
            .get("m_Script")
            .and_then(Reference::from_value)
            .and_then(|r| sources.script_class(&r.guid))
    } else {
        None
    };
    let read = super::authored::fields_read(library, renames, document, script.as_deref());
    document
        .data
        .iter()
        .filter(|(name, _)| read.as_ref().is_none_or(|read| read.contains(*name)))
        .flat_map(|(_, value)| references(value))
        .collect()
}

/// Every reference a value holds.
fn references(value: &serde_json::Value) -> Vec<Reference> {
    let mut found = Vec::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Object(fields) => {
                if let Some(reference) = Reference::from_value(value)
                    && fields.contains_key("fileID")
                {
                    found.push(reference);
                    continue;
                }
                pending.extend(fields.values());
            }
            serde_json::Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    found
}
