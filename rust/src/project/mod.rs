//! Raw scene preparation: the catalog-aware session, its authored expansion,
//! native overlays and immutable revision publication with byte-identical manifests.
mod builtin_shader;
mod cache_paths;
mod compiled_shader_snapshot;
mod cubemap;
mod discovery;
mod expand;
mod graphics_settings;
mod image_config;
mod imported_bindings;
mod imported_source_closure;
pub mod lexpath;
mod manifest;
mod material;
mod material_variant;
mod mesh;
mod native;
mod native_model;
mod native_template;
mod native_ui;
mod netcode;
mod prefab_override_proof;
pub mod prepare;
mod scriptable;
mod session;
mod shader_sources;
mod shader_usage;
pub mod strconv;
mod texture;
mod types;
mod upgraded;
pub mod value;
mod worklist;

#[cfg(test)]
mod tests;

pub use crate::cache::CacheStats;
pub use builtin_shader::BUILTIN_RESOURCE_GUID;
pub use cache_paths::is_owned_cache_path;
pub use discovery::ControllerMask;
pub use expand::{apply_authored_override, resolve_authored_graph};
pub use graphics_settings::PlayerGraphicsSettings;
pub use manifest::UnsupportedClass;
pub(crate) use material::editor_version_stamp;
pub use material_variant::{InspectedMaterial, outdated_material};
pub use mesh::MESH_VERSION;
pub use native_model::native_model_graph_sha256;
pub use native_template::{NativeTemplate, NeedsNativeTemplates, UnsupportedNativeReference};
pub use session::{NeedsImportError, Options, Prepared, RefusedPrefab, Session, manifest_json};
pub(crate) use shader_sources::shader_without_comments;
pub use shader_sources::{collect_shader_source_requirements, validate_shader_source_requirements};
pub use shader_usage::ShaderMaterial;
pub use types::{
    AuthoredDependency, AuthoredGraph, AuthoredGraphObject, AuthoredIdentity,
    CompiledShaderSnapshot, Document, List, NativeContentDependency, NativeContentFile,
    NativeContentObject, NativeContentResource, NativeDeferredBinding, NativeMeshCPUProjection,
    NativeMeshCPURetention, NativeModelGraphObject, NativeModelGraphReference,
    NativeModelGraphTarget, NativeReplacement, NativeUIBinding, NativeUIBindingProof,
    PrefabOverrideDiagnostic, RetainedBuiltinShader, ShaderSourceRequirement,
};
pub use upgraded::UpgradedSource;
pub use value::{Map, Value};

/// Parses Unity text serialization into typed documents. YAML aliases,
/// duplicate keys and non-finite numbers are refused; fileIDs stay strings.
pub fn parse_unity_yaml(content: &[u8]) -> anyhow::Result<Vec<Document>> {
    Ok(crate::yaml::parse_unity_yaml(content)?
        .iter()
        .map(Document::from_yaml)
        .collect())
}
