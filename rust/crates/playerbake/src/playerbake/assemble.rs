//! One Player file holding a set of sources and everything they refer to,
//! written by the tool: the counterpart of the file of a scene's
//! dependencies that Unity's worker writes with `WriteSerializedFile`.
use super::{
    FileFormat, Object,
    authored::{Library, MONO_SCRIPT, Reference, Sources, Tags, convert},
    texture::{self, SPRITE},
    typetree::{TypeTree, Val},
};
use anyhow::{Context as _, Result};
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
    /// Sources left out or written in part, with the reason.
    pub refused: Vec<(String, String)>,
    /// GUIDs of the sources other files hold, in external-reference order.
    pub externals: Vec<String>,
    pub sources: usize,
}

enum Source {
    Yaml(Vec<Document>),
    /// A text or binary file a `TextScriptImporter` makes a `TextAsset` of.
    Text(PathBuf),
    Image(PathBuf, Vec<u8>),
    Script(String),
}

/// GUIDs of the engine's own resource files, which stay external.
fn engine_resource(guid: &str) -> bool {
    guid.starts_with("0000000000000000")
}

pub fn write_closure(
    library: &Library,
    tags: &Tags,
    sources: &Sources,
    roots: &[&str],
    format: FileFormat,
    stream_name: &str,
) -> Result<Assembled> {
    let mut assembled = Assembled::default();
    // Sources by GUID, in the order they are reached.
    let mut order: Vec<String> = Vec::new();
    let mut loaded: HashMap<String, Source> = HashMap::new();
    let mut queue: VecDeque<String> = roots.iter().map(|g| g.to_string()).collect();
    let mut seen: HashSet<String> = queue.iter().cloned().collect();
    let mut unpacked = super::prefab::Unpacked::default();
    while let Some(guid) = queue.pop_front() {
        let Some(path) = sources.path(&guid) else {
            assembled
                .refused
                .push((guid, "no source with this GUID".into()));
            continue;
        };
        let source = match load(path, &guid, sources).and_then(|source| match source {
            Some(Source::Yaml(documents)) => {
                Ok(Some(Source::Yaml(unpacked.unpack(sources, documents)?)))
            }
            other => Ok(other),
        }) {
            Ok(Some(source)) => source,
            Ok(None) => {
                assembled.refused.push((
                    guid,
                    format!("{} is not written by the tool", path.display()),
                ));
                continue;
            }
            Err(error) => {
                assembled.refused.push((guid, format!("{error:#}")));
                continue;
            }
        };
        if let Source::Yaml(documents) = &source {
            for document in documents {
                for reference in references(&serde_json::Value::Object(document.data.clone())) {
                    if !reference.guid.is_empty()
                        && !engine_resource(&reference.guid)
                        && seen.insert(reference.guid.clone())
                    {
                        queue.push_back(reference.guid);
                    }
                }
            }
        }
        order.push(guid.clone());
        loaded.insert(guid, source);
    }
    // Local IDs: every object of every source, scripts last.
    let mut ids: HashMap<(String, i64), i64> = HashMap::new();
    let mut next = 1i64;
    let mut images: HashMap<String, texture::Textured> = HashMap::new();
    let texture_kind = library.class(texture::TEXTURE_2D).cloned();
    let sprite_kind = library.class(SPRITE).cloned();
    for guid in &order {
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
            Source::Text(_) => {
                ids.insert((guid.clone(), TEXT_FILE_ID), next);
                next += 1;
            }
        }
    }
    let mut externals: Vec<String> = Vec::new();
    let mut objects: Vec<(i64, Arc<TypeTree>, Val, Option<i64>)> = Vec::new();
    let script_tree = library
        .class(MONO_SCRIPT)
        .context("no type tree of MonoScript")?
        .tree
        .clone();
    for guid in &order {
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
                let offset = align(&mut assembled.stream);
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
                            .and_then(|r| sources.script_class(&r.guid))
                    } else {
                        None
                    };
                    let mut resolve = |reference: &Reference| -> Result<Option<(i32, i64)>> {
                        let owner = if reference.guid.is_empty() {
                            guid
                        } else {
                            &reference.guid
                        };
                        if let Some(id) = ids.get(&(owner.clone(), reference.file_id)) {
                            return Ok(Some((0, *id)));
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
                    match convert(library, tags, document, script.as_deref(), &mut resolve) {
                        Ok(mut converted) => {
                            // A texture's pixels go to the streamed data, as a
                            // build writes them.
                            if document.class_id == texture::TEXTURE_2D as i64
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
                            let script_id = document
                                .data
                                .get("m_Script")
                                .and_then(Reference::from_value)
                                .filter(|_| document.class_id == 114)
                                .and_then(|r| ids.get(&(r.guid, 11500000)).copied());
                            objects.push((
                                ids[&(guid.clone(), document.file_id.parse()?)],
                                converted.tree,
                                converted.value,
                                script_id,
                            ));
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
    // A reference to an object that was not written is empty, as a build
    // leaves a reference to a missing object.
    let present: HashSet<i64> = objects.iter().map(|(id, ..)| *id).collect();
    for (_, _, value, _) in objects.iter_mut() {
        clear_missing(value, &present);
    }
    let written: Vec<Object<'_>> = objects
        .iter()
        .map(|(local_id, tree, value, script)| {
            Ok(Object {
                local_id: *local_id,
                tree,
                data: tree.encode(value)?,
                script: *script,
            })
        })
        .collect::<Result<_>>()?;
    let references: Vec<super::External> = externals
        .iter()
        .map(|guid| super::External { guid: guid.clone() })
        .collect();
    assembled.file = super::write_in(format, &written, &references)?;
    assembled.externals = externals;
    assembled.sources = order.len();
    Ok(assembled)
}

const PREFAB_INSTANCE: i64 = 1001;
const TEXT_ASSET: i32 = 49;
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
fn written(document: &Document) -> bool {
    !document.stripped && document.class_id != PREFAB_INSTANCE
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
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if extension == "cs" {
        return Ok(sources.script_class(guid).map(Source::Script));
    }
    let meta = std::fs::read(format!("{}.meta", path.display()))?;
    if meta.windows(19).any(|w| w == b"TextScriptImporter:") {
        return Ok(Some(Source::Text(path.to_path_buf())));
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

/// Every reference a document's fields hold.
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
