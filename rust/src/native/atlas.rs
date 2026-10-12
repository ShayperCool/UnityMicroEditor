//! The importer-based sprite atlases of a project (`.spriteatlasv2`).
//!
//! No scene refers to an atlas: a sprite belongs to one because the atlas
//! lists its texture, a sprite of it or a folder above it. A build then writes
//! the sprite with a pointer to the atlas. So an atlas is imported for the
//! sake of the textures it packs, and always together with them: the worker
//! packs the sprites of their artifacts, and the adapter binds those sprites
//! to the packed result.
use super::{
    NativeProjectImporter, resolve_project_import_source,
    support::{lower_ext, read_file},
    texture_profile::{native_texture_needs_warm, texture_source},
};
use crate::{
    project::{Value, parse_unity_yaml},
    registry::meta_guid,
    sha256, sysutil,
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const ATLAS_EXTENSION: &str = ".spriteatlasv2";

/// One source with its identity as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceRevision {
    pub path: String,
    pub guid: String,
    pub source_sha: String,
    pub meta_sha: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AtlasSource {
    pub revision: SourceRevision,
    /// The texture sources it packs, by path.
    pub members: Vec<SourceRevision>,
    /// Why the atlas cannot be imported; empty when it can.
    pub refusal: String,
}

impl AtlasSource {
    /// What the packed atlas is made from besides its own source: every
    /// member and its metadata, by path.
    pub fn ingredients(&self) -> BTreeMap<String, String> {
        let mut ingredients = BTreeMap::new();
        for member in &self.members {
            ingredients.insert(member.path.clone(), member.source_sha.clone());
            ingredients.insert(format!("{}.meta", member.path), member.meta_sha.clone());
        }
        ingredients
    }

    /// The atlas and its members in the order of one import batch.
    pub fn batch(&self) -> Vec<String> {
        let mut batch: Vec<String> = self.members.iter().map(|m| m.path.clone()).collect();
        batch.push(self.revision.path.clone());
        batch
    }
}

fn revision(project: &str, logical: &str) -> Result<SourceRevision> {
    let source = resolve_project_import_source(project, logical)?;
    let meta = read_file(&format!("{source}.meta"))?;
    Ok(SourceRevision {
        path: logical.into(),
        guid: meta_guid(&meta)?,
        source_sha: sha256(&read_file(&source)?),
        meta_sha: sha256(&meta),
    })
}

/// The files below `Assets`, by logical path. Hidden entries and those the
/// Editor ignores (`name~`) are left out; links are not followed.
fn asset_files(project: &str) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut pending = vec!["Assets".to_string()];
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        let physical = sysutil::join(&[project, &directory]);
        let Ok(entries) = std::fs::read_dir(&physical) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 1 << 20 {
                bail!("project holds more files than a sprite atlas search covers");
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.ends_with('~') {
                continue;
            }
            let logical = format!("{directory}/{name}");
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(logical),
                Ok(kind) if kind.is_file() => files.push(logical),
                _ => {}
            }
        }
    }
    files.sort();
    Ok(files)
}

fn zero(value: &Value) -> bool {
    matches!(value, Value::Int(0) | Value::Null) || value.as_str() == Some("0")
}

/// The atlases of a project with the textures they pack. `path_of` gives the
/// source a GUID belongs to, or nothing.
pub(crate) fn read_atlas_sources(
    project: &str,
    path_of: &mut dyn FnMut(&str) -> String,
) -> Result<Vec<AtlasSource>> {
    let files = asset_files(project)?;
    let mut atlases = Vec::new();
    for path in files
        .iter()
        .filter(|path| lower_ext(path) == ATLAS_EXTENSION)
    {
        let mut atlas = AtlasSource {
            revision: revision(project, path)?,
            members: Vec::new(),
            refusal: String::new(),
        };
        let data = read_file(&resolve_project_import_source(project, path)?)?;
        let documents = parse_unity_yaml(&data);
        let asset = match &documents {
            Ok(documents)
                if documents.len() == 1
                    && documents[0].class_id == 612988286
                    && documents[0].type_name == "SpriteAtlasAsset" =>
            {
                &documents[0]
            }
            _ => {
                atlas.refusal = "its source is not one SpriteAtlasAsset document".into();
                atlases.push(atlas);
                continue;
            }
        };
        if !zero(asset.get("m_IsVariant")) || !zero(asset.get("m_MasterAtlas").field("fileID")) {
            atlas.refusal = "a variant atlas is not supported".into();
        }
        // A build writes the sprites of such an atlas without a pointer to
        // it, for the game to bind when it loads the atlas itself.
        let meta = read_file(&format!(
            "{}.meta",
            resolve_project_import_source(project, path)?
        ))?;
        if String::from_utf8_lossy(&meta)
            .lines()
            .any(|line| line.trim() == "bindAsDefault: 0")
        {
            atlas.refusal = "an atlas that is not included in the build is not supported".into();
        }
        if !zero(asset.get("m_ScriptablePacker").field("fileID")) {
            atlas.refusal = "an atlas with a scriptable packer runs project code".into();
        }
        let mut members = BTreeSet::new();
        let packables = match asset.get("m_ImporterData").field("packables") {
            Value::Array(packables) => &packables[..],
            _ => &[],
        };
        for packable in packables {
            let guid = packable.field("guid").as_str().unwrap_or_default();
            let source = path_of(guid);
            if source.is_empty() {
                // The Editor packs what it finds and leaves the rest out.
                continue;
            }
            if texture_source(&source) {
                members.insert(source);
                continue;
            }
            // A folder stands for every texture below it.
            let prefix = format!("{source}/");
            let below: Vec<&String> = files
                .iter()
                .filter(|file| file.starts_with(&prefix) && texture_source(file))
                .collect();
            if below.is_empty() && !files.iter().any(|file| file.starts_with(&prefix)) {
                atlas.refusal =
                    format!("it packs {source}, which is neither a texture nor a folder");
            }
            members.extend(below.into_iter().cloned());
        }
        for member in members {
            atlas.members.push(revision(project, &member)?);
        }
        if atlas.members.is_empty() && atlas.refusal.is_empty() {
            atlas.refusal = "it packs no texture".into();
        }
        atlases.push(atlas);
    }
    Ok(atlases)
}

impl NativeProjectImporter {
    /// Reads the project's atlases and records, for every atlas and member,
    /// the batch it is imported in and what its import depends on beyond its
    /// own source. Returns the pending sources that are not part of an atlas
    /// batch, and the atlas batches that have to be imported: those of an
    /// atlas a pending or published texture belongs to, unless everything of
    /// it is published in its current revision.
    pub(crate) fn atlas_batches(
        &mut self,
        pending: Vec<String>,
    ) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let session = self.session.clone();
        let atlases = {
            let mut inner = session.lock();
            inner.index_guids()?;
            read_atlas_sources(&self.opts.project, &mut |guid| {
                inner.path_of(guid).to_string()
            })?
        };
        self.atlas_batch.clear();
        self.atlas_identity.clear();
        let wanted: BTreeSet<String> = pending.into_iter().collect();
        let mut owner: BTreeMap<String, String> = BTreeMap::new();
        let mut batches = Vec::new();
        let mut batched = BTreeSet::new();
        for atlas in &atlases {
            let relevant = wanted.contains(&atlas.revision.path)
                || self.entries.contains_key(&atlas.revision.guid)
                || atlas.members.iter().any(|member| {
                    wanted.contains(&member.path) || self.entries.contains_key(&member.guid)
                });
            if !atlas.refusal.is_empty() {
                if relevant {
                    bail!("sprite atlas {}: {}", atlas.revision.path, atlas.refusal);
                }
                continue;
            }
            let batch = atlas.batch();
            let ingredients = atlas.ingredients();
            let members: String = ingredients
                .iter()
                .map(|(path, sha)| format!("\0{path}\0{sha}"))
                .collect();
            self.atlas_identity.insert(
                atlas.revision.path.clone(),
                format!("atlas-members{members}"),
            );
            for member in &atlas.members {
                if let Some(other) = owner.insert(member.path.clone(), atlas.revision.path.clone())
                {
                    if relevant {
                        bail!(
                            "{} is packed by two sprite atlases, {other} and {}",
                            member.path,
                            atlas.revision.path
                        );
                    }
                    continue;
                }
                self.atlas_identity.insert(
                    member.path.clone(),
                    format!(
                        "atlas\0{}\0{}\0{}",
                        atlas.revision.guid, atlas.revision.source_sha, atlas.revision.meta_sha
                    ),
                );
                self.atlas_batch.insert(member.path.clone(), batch.clone());
            }
            self.atlas_batch
                .insert(atlas.revision.path.clone(), batch.clone());
            if !relevant {
                continue;
            }
            let published = |revision: &SourceRevision| {
                self.entries.get(&revision.guid).filter(|entry| {
                    entry.source_sha256 == revision.source_sha
                        && entry.meta_sha256 == revision.meta_sha
                })
            };
            let current = published(&atlas.revision)
                .is_some_and(|entry| entry.ingredient_sha256 == ingredients)
                && atlas
                    .members
                    .iter()
                    .all(|member| published(member).is_some() && !wanted.contains(&member.path));
            if current {
                continue;
            }
            if batch.len() > super::NATIVE_IMPORT_BATCH_SIZE {
                bail!(
                    "sprite atlas {} packs {} textures; one import batch holds {}",
                    atlas.revision.path,
                    atlas.members.len(),
                    super::NATIVE_IMPORT_BATCH_SIZE - 1
                );
            }
            for member in &atlas.members {
                if native_texture_needs_warm(&self.opts.project, &member.path)? {
                    bail!(
                        "sprite atlas {} packs {}, whose importer settings a standalone worker is not verified for",
                        atlas.revision.path,
                        member.path
                    );
                }
            }
            batched.extend(batch.iter().cloned());
            batches.push(batch);
        }
        let mut rest: BTreeSet<String> = wanted.difference(&batched).cloned().collect();
        // A texture bound to an atlas that no longer packs it is imported
        // again on its own.
        for entry in self.entries.values() {
            if texture_source(&entry.source_path)
                && !owner.contains_key(&entry.source_path)
                && entry.content.dependencies.iter().any(|d| !d.global)
            {
                rest.insert(entry.source_path.clone());
            }
        }
        rest.retain(|path| lower_ext(path) != ATLAS_EXTENSION);
        Ok((rest.into_iter().collect(), batches))
    }
}
