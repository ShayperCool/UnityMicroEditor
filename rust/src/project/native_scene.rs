//! A scene the import worker writes in the Player format, which the Player
//! loads with the engine's own loader instead of building it from the
//! manifest's object records. Chosen with `UME_NATIVE_SCENE=1` while it
//! replaces the raw loader.
use super::{
    NativeContentFile, NativeSceneFile,
    session::Inner,
    value::{self, Map, Value},
};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Whether sessions write their scenes in the Player format.
pub fn native_scene_mode() -> bool {
    std::env::var_os("UME_NATIVE_SCENE").is_some_and(|value| value == "1")
}

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
}

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
        let mut pending = vec![scene.to_string()];
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
            let (content, _) = self.read_text(&physical)?;
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
        for file in &written.content {
            self.verify_native_artifacts(file)?;
        }
        let mut over = Map::new();
        over.insert("objects".into(), Value::Array(Vec::new()));
        over.insert("assets".into(), Value::Array(Vec::new()));
        over.insert("importedAssetBindings".into(), Value::Array(Vec::new()));
        over.insert("importedAssetPacks".into(), Value::Array(Vec::new()));
        over.insert("nativeContentFiles".into(), value::raw(&written.content)?);
        over.insert("nativeScene".into(), value::raw(&written.scene)?);
        Ok(over)
    }
}

impl super::Session {
    /// Holds a scene the worker wrote, for the preparation whose sources
    /// have `scene.key`.
    pub fn set_native_scene(&self, scene: NativeScene) {
        self.lock().native_scene = Some(scene);
    }
}
