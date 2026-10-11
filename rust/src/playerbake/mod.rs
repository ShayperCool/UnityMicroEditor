//! Bakes authored native-format assets into the form the Player loads,
//! without an Editor or import worker. Unity does not import these assets: its
//! Editor reads the YAML and derives the Player data during a build. This
//! module reproduces that derivation for the pinned engine and refuses what it
//! cannot reproduce exactly, so the caller can fall back to an Editor build.
mod clip;
mod controller;
mod freeform;
mod humanoid;
mod managed;
mod override_controller;
mod serialized;
mod source;
mod typetree;

pub use clip::{
    ClipSources, MAX_SOURCE_BYTES, ModelClips, NoScripts, ScriptIdentity, bake_clip,
    bake_clip_object, bake_model_clips, script_local_id,
};
pub use controller::{
    Behaviour, BehaviourScript, ClipFacts, Controller, ControllerSources, Mask, bake_controller,
    bake_controller_object, read_clip_facts, read_mask,
};
pub use freeform::{Freeform, precompute as precompute_freeform};
pub use managed::{Field, FieldType, References, ScriptLayout, ScriptType};
pub use override_controller::{bake_override, bake_override_object};
pub use serialized::{External, Object, Read, read as read_serialized, write};
pub use typetree::{TypeTree, Val};

use anyhow::Result;
use std::sync::LazyLock;

static CLIP_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-74.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `AnimationClip` in the pinned engine.
pub fn clip_tree() -> Result<&'static TypeTree> {
    CLIP_TREE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned AnimationClip type tree: {error}"))
}

static SCRIPT_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-115.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `MonoScript` in the pinned engine.
pub fn script_tree() -> Result<&'static TypeTree> {
    SCRIPT_TREE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned MonoScript type tree: {error}"))
}

static CONTROLLER_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-91.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `AnimatorController` in the pinned engine.
pub fn controller_tree() -> Result<&'static TypeTree> {
    CONTROLLER_TREE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned AnimatorController type tree: {error}"))
}

static OVERRIDE_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-221.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `AnimatorOverrideController` in the pinned engine.
pub fn override_tree() -> Result<&'static TypeTree> {
    OVERRIDE_TREE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned AnimatorOverrideController type tree: {error}"))
}

/// An authored feature the baker does not reproduce. The asset stays on the
/// Editor-built path; nothing approximate is published.
#[derive(Debug)]
pub struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Unsupported {}

pub(crate) fn unsupported(reason: impl Into<String>) -> anyhow::Error {
    Unsupported(reason.into()).into()
}

/// A baked asset: one ContentFile and the identities it publishes.
#[derive(Debug, Clone, PartialEq)]
pub struct Baked {
    pub file: Vec<u8>,
    /// Local file ID and managed type name of each published object.
    pub objects: Vec<(String, String)>,
    pub externals: Vec<External>,
}

/// Unity's name hash: CRC-32 (IEEE) of the UTF-8 bytes.
pub fn crc32(text: &str) -> u32 {
    let mut crc = !0u32;
    for &byte in text.as_bytes() {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
