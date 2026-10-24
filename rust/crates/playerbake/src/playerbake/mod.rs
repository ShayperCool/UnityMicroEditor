//! The Player's serialized files as this tool reads and writes them: the
//! file format and type trees, the shaders of a built-in render pipeline
//! baked from their sources, and the settings managers of a Player written
//! from the project's settings.
mod animation;
mod assemble;
pub mod audio;
mod authored;
mod binding;
mod clip;
mod compare;
mod controller;
mod font;
mod freeform;
mod game_files;
mod humanoid;
mod lighting;
mod managers;
mod mask;
mod mixer;
mod override_controller;
mod prefab;
mod scripts;
mod serialized;
mod shader;
mod shader_names;
pub mod shaderlab;
mod source;
pub mod spritemesh;
pub mod texcomp;
mod text_values;
mod texture;
pub use texture::decode_png;
mod typetree;
mod weighted;

pub use assemble::{
    Assembled, ScriptsElsewhere, ShaderBaker, external_path, write_closure, write_closure_baking,
    write_scene_closure,
};
pub use authored::{
    Converted, Kind, Library, MONO_SCRIPT, Reference, Renames, Resolve, Sources, Tags, Written,
    convert as convert_authored, full_name, imported_file_id, script_execution_orders,
    write_source,
};
pub use clip::{
    ClipObject, ClipSources, MAX_SOURCE_BYTES, NoScripts, ObjectPointer, ObjectTarget,
    ScriptIdentity, bake_clip_object, bake_clip_parts,
};
pub use compare::{Difference, Inspected, compare as compare_files, inspect as inspect_file};
pub use controller::{
    BehaviourScript, ClipFacts, Controller, ControllerSources, bake_controller_object,
    read_clip_facts,
};
pub use freeform::{Freeform, precompute as precompute_freeform};
pub use game_files::{
    BUILTIN_EXTRA_FILE, DEFAULT_RESOURCES_FILE, DerivedManagers, GLOBAL_ASSETS_FILE,
    MANAGER_EXTERNALS, ManagerPack, RESOURCES_FILE, plain_file, write_game_managers,
};
pub use managers::{
    ManagerReport, PlainFile, PointerOf, compare_managers, manager_trees, write_project_managers,
};
pub use mask::{Mask, bake_mask_object, mask_value, read_mask};
pub use mixer::{
    MixerObject, bake_mixer_objects, mixer_objects, mixer_tree, mixer_undefined_values,
};
pub use override_controller::{bake_override_object, override_value};
pub use prefab::Unpacked;
pub use scripts::{
    Layout, Layouts, Templates, assembly_script_file_id, first_difference, properties_hash,
};
pub use serialized::{
    External, FileFormat, Object, Read, ReferenceType, VERIFIED_FORMAT, external_paths,
    local_ids_of_class, read as read_serialized, read_of_class as read_serialized_of_class,
    read_with_externals as read_serialized_with_externals, write, write_in,
    write_in_with_references, write_with_references,
};
pub use shader::{
    COMPUTE_SHADER_CLASS, COMPUTE_SHADER_LOCAL_ID, FogKeep, LightmapKeep, NoShaders,
    SHADER_LOCAL_ID, ShaderDependency, ShaderLinks, ShaderMaterialUse, ShaderPass, ShaderPointer,
    ShaderProject, ShaderRelease, ShaderSettings, ShaderSources, ShaderTier, SnippetCompiler,
    SnippetReply, SnippetRequest, WIRE_MARKER, bake_shader, bake_shader_object,
    names_render_pipeline, read_shader_artifact, read_shader_links, read_shader_links_of,
    read_shader_links_with, shader_platforms, shader_tree, verify_shader_programs,
};
pub use shader_names::read_shader_names;
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

static MASK_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-319.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `AvatarMask` in the pinned engine.
pub fn mask_tree() -> Result<&'static TypeTree> {
    MASK_TREE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned AvatarMask type tree: {error}"))
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
