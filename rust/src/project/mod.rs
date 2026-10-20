//! Scene preparation: the session that keys a scene by its sources, has the
//! import worker write it in the Player format, publishes its shaders and
//! immutable revisions of its manifest; and the authored expansion the
//! source tools read.
mod builtin_shader;
mod cache_paths;
mod expand;
pub mod lexpath;
mod material;
mod native;
mod native_scene;
mod prefab_override_proof;
pub mod prepare;
mod session;
mod shader_sources;
mod shader_usage;
pub mod strconv;
mod types;
pub mod value;

#[cfg(test)]
mod tests;

pub use crate::cache::CacheStats;
pub use builtin_shader::BUILTIN_RESOURCE_GUID;
pub use cache_paths::is_owned_cache_path;
pub use expand::{apply_authored_override, resolve_authored_graph};
pub(crate) use material::editor_version_stamp;
pub use native_scene::{NativeScene, NeedsNativeScene};
pub use session::{NeedsImportError, Options, Prepared, RefusedPrefab, Session, manifest_json};
pub(crate) use shader_sources::shader_without_comments;
pub use shader_sources::{collect_shader_source_requirements, validate_shader_source_requirements};
pub use shader_usage::ShaderMaterial;
pub use types::{
    AuthoredDependency, AuthoredGraph, AuthoredGraphObject, AuthoredIdentity,
    CompiledShaderSnapshot, Document, List, NativeContentDependency, NativeContentFile,
    NativeContentObject, NativeContentResource, NativeDeferredBinding, NativeMeshCPUProjection,
    NativeMeshCPURetention, NativeModelGraphObject, NativeModelGraphReference,
    NativeModelGraphTarget, NativeReplacement, NativeSceneAlias, NativeSceneFile, NativeUIBinding,
    NativeUIBindingProof, PrefabOverrideDiagnostic, RetainedBuiltinShader, ShaderSourceRequirement,
};
pub use value::{Map, Value};

/// Parses Unity text serialization into typed documents. YAML aliases,
/// duplicate keys and non-finite numbers are refused; fileIDs stay strings.
pub fn parse_unity_yaml(content: &[u8]) -> anyhow::Result<Vec<Document>> {
    Ok(crate::yaml::parse_unity_yaml(content)?
        .iter()
        .map(Document::from_yaml)
        .collect())
}
