//! A scene the import worker writes in the Player format, which the Player
//! loads with the engine's own loader.
use super::{
    NativeContentFile, NativeSceneFile,
    session::Inner,
    value::{self, Map, Value},
};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// The scene a preparation needs in the Player format, by what it is made
/// of: the hash of the scene and of every asset it reaches, with their
/// `.meta` files.
#[derive(Debug)]
pub struct NeedsNativeScene {
    pub scene: String,
    pub key: String,
}

impl std::fmt::Display for NeedsNativeScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the scene {} is not written in the Player format",
            self.scene
        )
    }
}
impl std::error::Error for NeedsNativeScene {}

/// A written scene and the content files it refers to.
#[derive(Clone, Debug)]
pub struct NativeScene {
    pub key: String,
    pub scene: NativeSceneFile,
    pub content: Vec<NativeContentFile>,
    /// The sources whose objects the scene or its content refer to in the
    /// files the session publishes for them (the shaders), by GUID.
    pub published: Vec<String>,
    /// The local ID in the first content file of each source object it
    /// holds, by `<GUID>/<local ID>`.
    pub objects: std::collections::BTreeMap<String, String>,
    /// The local ID of the root GameObject of each included prefab, by GUID.
    pub roots: std::collections::BTreeMap<String, String>,
    /// The local IDs of the objects of each other included asset, main
    /// object first, by GUID.
    pub assets: std::collections::BTreeMap<String, Vec<String>>,
}

/// Text sources above this size are read without the cache.
const LARGE_TEXT: u64 = 16 * 1024 * 1024;

/// The extensions of the text assets whose references the closure follows.
fn followed(path: &str) -> bool {
    matches!(
        super::lexpath::lower_ext(path).as_str(),
        ".unity"
            | ".prefab"
            | ".mat"
            | ".asset"
            | ".controller"
            | ".overridecontroller"
            | ".anim"
            | ".mask"
            | ".physicmaterial"
            | ".physicsmaterial2d"
            | ".rendertexture"
            | ".mixer"
            | ".playable"
            | ".lighting"
            | ".spriteatlasv2"
            | ".terrainlayer"
            | ".flare"
            | ".cubemap"
            | ".preset"
    )
}

impl Inner {
    /// The manifest fields of a scene in the Player format. Every source it
    /// is made of is a dependency of the revision.
    pub(crate) fn native_scene_over(&mut self, scene: &str) -> Result<Map> {
        static GUID: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
            regex::bytes::Regex::new(r"guid: ([0-9a-fA-F]{32})").unwrap()
        });
        // What the project's settings name is written beside the scene.
        let mut pending = vec![scene.to_string()];
        let mut guids = vec![self.baseline_text("renderPipelineGuid")];
        for key in ["preloadedAssets"] {
            for entry in value::array(value::get(&self.baseline, key)) {
                if let Value::Object(fields) = entry {
                    guids.push(value::text(fields, "guid").into_owned());
                }
            }
        }
        for guid in guids {
            let path = self.path_of(&guid).to_string();
            if !path.is_empty() {
                pending.push(path);
            }
        }
        pending.extend(self.extra_assets.iter().cloned());
        let mut seen = BTreeSet::new();
        while let Some(path) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            self.depend(&path)?;
            if !followed(&path) {
                continue;
            }
            let physical = self.source(&path)?;
            // A large clip is read past the cache of text sources, which
            // keeps only files it can hold.
            let size = std::fs::metadata(&physical)
                .map_err(|e| super::session::path_error(&physical, e))?
                .len();
            let content: std::sync::Arc<[u8]> = if size > LARGE_TEXT {
                std::fs::read(&physical)
                    .map_err(|e| super::session::path_error(&physical, e))?
                    .into()
            } else {
                self.read_text(&physical)?.0
            };
            if !content.starts_with(b"%YAML") {
                continue;
            }
            for capture in GUID.captures_iter(&content) {
                let guid = String::from_utf8_lossy(&capture[1]).to_ascii_lowercase();
                let target = self.path_of(&guid).to_string();
                // A script's class is in the compiled assemblies; a GUID no
                // source has is missing in the Editor too.
                if target.is_empty() || super::lexpath::lower_ext(&target) == ".cs" {
                    continue;
                }
                pending.push(target);
            }
        }
        let mut key = Sha256::new();
        key.update(b"native-scene-v1\0");
        key.update(scene.as_bytes());
        for dependency in self.dependencies.values() {
            key.update(b"\0");
            key.update(dependency.path.as_bytes());
            key.update(b"\0");
            key.update(dependency.sha256.as_bytes());
        }
        let key = crate::hex(&key.finalize());
        let Some(written) = self
            .native_scene
            .as_ref()
            .filter(|written| written.key == key)
        else {
            return Err(NeedsNativeScene {
                scene: scene.to_string(),
                key,
            }
            .into());
        };
        let written = written.clone();
        let mut content = written.content.clone();
        let mut pending = written.published.clone();
        let mut included = BTreeSet::new();
        while let Some(guid) = pending.pop() {
            if !included.insert(guid.clone()) {
                continue;
            }
            let file = self.published_content(&guid)?;
            pending.extend(
                file.dependencies
                    .iter()
                    .filter(|dependency| !dependency.global)
                    .map(|dependency| dependency.guid.clone()),
            );
            content.push(file);
        }
        for file in &content {
            self.verify_native_artifacts(file)?;
        }
        let mut over = Map::new();
        over.insert("objects".into(), Value::Array(Vec::new()));
        over.insert("assets".into(), Value::Array(Vec::new()));
        over.insert("importedAssetBindings".into(), Value::Array(Vec::new()));
        over.insert("importedAssetPacks".into(), Value::Array(Vec::new()));
        over.insert("nativeContentFiles".into(), value::raw(&content)?);
        self.native_project_objects(&written, &mut over)?;
        Ok(over)
    }
}

impl Inner {
    /// Names where the worker wrote the render pipeline asset and the
    /// preloaded assets, which the manifest names by their sources: in the
    /// file of what the scene refers to.
    fn native_project_objects(&self, written: &NativeScene, over: &mut Map) -> Result<()> {
        let file = &written.content[0].guid;
        let mut sources = vec![(
            self.baseline_text("renderPipelineGuid"),
            self.baseline_text("renderPipelineFileId"),
        )];
        for entry in value::array(value::get(&self.baseline, "preloadedAssets")) {
            if let Value::Object(fields) = entry {
                sources.push((
                    value::text(fields, "guid").into_owned(),
                    value::text(fields, "fileId").into_owned(),
                ));
            }
        }
        // What the game asks for by address: a prefab or model by its root,
        // any other asset by those of its objects the file holds (a shader
        // is published on its own, an editor-only object is left out), the
        // main one first.
        for path in self.extra_assets.clone() {
            let guid = self.guid_of(&path).to_string();
            if let Some(root) = written.roots.get(&guid) {
                sources.push((guid, root.clone()));
                continue;
            }
            let held: Vec<String> = written
                .assets
                .get(&guid)
                .into_iter()
                .flatten()
                .filter(|id| written.objects.contains_key(&format!("{guid}/{id}")))
                .cloned()
                .collect();
            if held.is_empty() {
                return Err(super::RefusedPrefab::new(
                    &path,
                    anyhow::anyhow!("the worker wrote no object of the addressed asset"),
                )
                .into());
            }
            sources.extend(held.into_iter().map(|id| (guid.clone(), id)));
        }
        let mut scene = written.scene.clone();
        for (guid, file_id) in sources {
            if guid.is_empty() {
                continue;
            }
            let Some(local_id) = written.objects.get(&format!("{guid}/{file_id}")) else {
                anyhow::bail!(
                    "the worker wrote no object {file_id} of {guid}, which the project's settings name"
                );
            };
            scene.aliases.push(super::NativeSceneAlias {
                guid,
                file_id,
                file: file.clone(),
                local_id: local_id.clone(),
            });
        }
        over.insert("nativeScene".into(), value::raw(&scene)?);
        // The global settings a build strips by the pipeline's own rules;
        // the host Player holds the project's, as its build stripped them.
        over.insert(
            "renderPipelineGlobalSettings".into(),
            Value::Array(Vec::new()),
        );
        Ok(())
    }

    /// The file the session publishes for a source, current with it; one
    /// that is missing or was made from an older source is imported.
    fn published_content(&mut self, guid: &str) -> Result<NativeContentFile> {
        if let Some(file) = self.derived_native.get(guid) {
            return Ok(file.clone());
        }
        let path = self.path_of(guid).to_string();
        if path.is_empty() {
            anyhow::bail!(
                "the scene refers to the published content of {guid}, which no source has"
            );
        }
        let physical = self.source(&path)?;
        let (_, source) = self.read(&physical, false)?;
        let meta = self.source(&format!("{path}.meta"))?;
        let (_, meta) = self.read(&meta, true)?;
        self.depend(&path)?;
        let overlay = self
            .native_overlays
            .get(guid)
            .filter(|overlay| overlay.source_sha256 == source && overlay.meta_sha256 == meta)
            .map(|overlay| (overlay.content.clone(), overlay.ingredient_sha256.clone()));
        let Some((content, ingredients)) = overlay else {
            return Err(super::session::needs_import(&path));
        };
        for (ingredient, expected) in &ingredients {
            let physical = self.source(ingredient)?;
            let (_, actual) = self.read(&physical, ingredient.ends_with(".meta"))?;
            if actual != *expected {
                return Err(super::session::needs_import(&path));
            }
        }
        Ok(content)
    }
}

impl super::Session {
    /// The assets a build holds beside the scene, which the project's
    /// settings name: the render pipeline asset and the preloaded assets, by
    /// path.
    pub fn native_scene_includes(&self) -> Result<Vec<String>> {
        let mut s = self.lock();
        s.index_guids()?;
        let mut guids = vec![s.baseline_text("renderPipelineGuid")];
        for key in ["preloadedAssets"] {
            for entry in value::array(value::get(&s.baseline, key)) {
                if let Value::Object(fields) = entry {
                    guids.push(value::text(fields, "guid").into_owned());
                }
            }
        }
        let mut paths = Vec::new();
        for guid in guids {
            let path = s.path_of(&guid).to_string();
            if !path.is_empty() && !paths.contains(&path) {
                paths.push(path);
            }
        }
        // The prefabs the game asks for by address.
        for path in &s.extra_assets {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        Ok(paths)
    }

    /// Holds a scene the worker wrote, for the preparation whose sources
    /// have `scene.key`.
    pub fn set_native_scene(&self, scene: NativeScene) {
        self.lock().native_scene = Some(scene);
    }
}
