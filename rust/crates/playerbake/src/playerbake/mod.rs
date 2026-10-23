//! The Player's serialized files as this tool reads and writes them: the
//! file format and type trees, the shaders of a built-in render pipeline
//! baked from their sources, and the settings managers of a Player written
//! from the project's settings.
mod assemble;
mod authored;
mod compare;
mod managers;
mod prefab;
mod scripts;
mod serialized;
mod shader;
mod shader_names;
mod source;
pub mod spritemesh;
pub mod texcomp;
mod text_values;
mod texture;
pub use texture::decode_png;
mod typetree;

pub use assemble::{Assembled, write_closure};
pub use authored::{
    Converted, Kind, Library, MONO_SCRIPT, Reference, Renames, Resolve, Sources, Tags, Written,
    convert as convert_authored, full_name, script_execution_orders, write_source,
};
pub use compare::{Difference, Inspected, compare as compare_files, inspect as inspect_file};
pub use managers::{
    ManagerReport, PlainFile, compare_managers, manager_trees, write_project_managers,
};
pub use prefab::Unpacked;
pub use scripts::{Layout, Layouts, Templates, first_difference, properties_hash};
pub use serialized::{
    External, FileFormat, Object, Read, ReferenceType, VERIFIED_FORMAT, external_paths,
    local_ids_of_class, read as read_serialized, read_of_class as read_serialized_of_class,
    read_with_externals as read_serialized_with_externals, write, write_in, write_with_references,
};
pub use shader::{
    COMPUTE_SHADER_CLASS, COMPUTE_SHADER_LOCAL_ID, FogKeep, LightmapKeep, NoShaders,
    SHADER_LOCAL_ID, ShaderDependency, ShaderLinks, ShaderMaterialUse, ShaderPass, ShaderPointer,
    ShaderProject, ShaderSettings, ShaderSources, ShaderTier, SnippetCompiler, SnippetReply,
    SnippetRequest, WIRE_MARKER, bake_shader, bake_shader_object, names_render_pipeline,
    read_shader_artifact, read_shader_links, shader_platforms, shader_tree, verify_shader_programs,
};
pub use shader_names::read_shader_names;
pub use typetree::{TypeTree, Val};

use anyhow::Result;
use std::sync::LazyLock;

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
