//! The native import coordinator: owned import contexts, the DataStore
//! service and AssetImportWorker protocol driver, the worker that writes
//! scenes in the Player format, the shaders built beside them and the
//! validated cache that feeds a `project::Session`.
mod artifact;
pub(crate) mod artifactdb;
mod baked;
mod build_settings;
mod built_store;
mod context;
mod datastore;
pub mod editor_compilation;
mod environment;
mod host_shaders;
mod import;
mod importer;
mod native_scene;
mod player_objects;
mod probe;
mod scene_worker;
mod scripted;
mod shader_compiler;
mod support;

pub use artifact::{
    ArtifactExtraction, INLINE_ARTIFACT, LibraryArtifactStore, MAX_ARTIFACT_BYTES, STORED_ARTIFACT,
    UdsArtifactStore,
};
pub use context::{
    ContextExtras, ProjectImportContext, ProjectImportSource, create_project_import_context,
    resolve_project_import_source, verify_project_import_sources,
};
pub use datastore::{DataStoreLogEvidence, DataStoreService, DataStoreStatus, read_data_store_log};
pub(crate) use environment::DEFAULT_ETC_COMPRESSORS;
#[cfg(test)]
pub(crate) use environment::read_editor_settings;
pub use environment::{
    WorkerEnvironment, native_environment_fingerprint, parse_worker_environment,
    read_worker_environment, run_native_environment_cli,
};
pub use import::{
    ImportExtras, ProjectImportContent, ProjectImportedAsset, ProjectImportedFile,
    extract_project_import_content, run_project_import, run_project_import_cli,
    run_project_import_with,
};
pub use importer::{
    NativeOptions, NativeProjectImporter, native_index, project_native_store,
    stage_native_resources, valid_native_guid,
};
pub use player_objects::{
    Build as PlayerBuild, ExternalObject as PlayerExternalObject, Plan as PlayerBuildPlan,
    PlayerObjects, Report as PlayerObjectReport, Request as PlayerObjectRequest, WrittenScene,
    importer_identity as player_importer_identity, open_sources as open_build_sources,
    plan_source as plan_player_build, write_player_objects, write_scene_objects,
};
pub use probe::{
    FrameEvidence, ImportEvidence, NativeImportJob, ProbeOptions, ProbeReport, capture_frame,
    receive_message, rss_kib, run_probe, validate_worker_version,
};
pub use shader_compiler::ShaderCompiler;
pub use support::{JsonF64, is_canceled, is_not_exist};

#[cfg(test)]
mod tests;

/// Diagnostics: the environment a build of Player objects runs in, for the
/// scripts with the given GUIDs.
pub fn debug_build_environment(project: &str, scripts: &[String]) -> anyhow::Result<String> {
    let environment = scripted::build_environment(project, &scripts.iter().cloned().collect())?;
    Ok(format!("{environment:#?}"))
}
