//! The native import coordinator: owned import contexts, the DataStore service
//! and AssetImportWorker protocol driver, UDS artifact extraction and the
//! validated native cache that feeds a `project::Session`.
mod adopt;
mod artifact;
mod artifactdb;
mod atlas;
mod baked;
mod batch;
mod build_settings;
mod built_store;
mod builtin_shader;
mod context;
mod datastore;
pub mod editor_compilation;
mod environment;
mod graphics_settings;
mod host_shaders;
mod import;
mod importer;
mod inspector;
mod material_variant;
mod mesh_cpu;
mod native_template;
mod pipeline_models;
mod player_objects;
mod probe;
mod scripted;
mod shader_cache;
mod shader_compiler;
mod supplies;
mod support;
mod texture_profile;
mod ui;
mod upgrade;
mod warm;

pub use adopt::{read_cold_adoption_report, run_native_adopt_cli, validate_adoption_file};
pub use artifact::{ArtifactExtraction, MAX_ARTIFACT_BYTES, UdsArtifactStore};
pub use batch::{NATIVE_IMPORT_BATCH_SIZE, cold_native_profile, order_native_batch};
pub use builtin_shader::{apply_builtin_shaders, verify_builtin_shader_snapshot};
pub use context::{
    ContextExtras, ProjectImportContext, ProjectImportSource, create_project_import_context,
    resolve_project_import_source, verify_project_import_sources,
};
pub use datastore::{DataStoreLogEvidence, DataStoreService, DataStoreStatus, read_data_store_log};
pub use environment::{
    WORKER_ENVIRONMENT_REVISION, WorkerEnvironment, native_environment_fingerprint,
    parse_worker_environment, read_worker_environment, run_native_environment_cli,
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
pub use inspector::{
    InspectedMaterial, Inspection, InspectionOptions, MaterialInspection, VERIFIED_GRAPH_PACKAGE,
    VERIFIED_INSPECTORS, inspect_material_variants, inspection_cache, run_material_inspect_cli,
};
pub use player_objects::{
    Build as PlayerBuild, ExternalObject as PlayerExternalObject, Plan as PlayerBuildPlan,
    PlayerObjects, Report as PlayerObjectReport, Request as PlayerObjectRequest,
    importer_identity as player_importer_identity, open_sources as open_build_sources,
    plan_source as plan_player_build, write_player_objects, write_scene_objects,
};
pub use probe::{
    FrameEvidence, ImportEvidence, NativeImportJob, ProbeOptions, ProbeReport, capture_frame,
    receive_message, rss_kib, run_probe, validate_worker_version,
};
pub use shader_cache::verify_compiled_shader_cache;
pub use shader_compiler::ShaderCompiler;
pub use support::{JsonF64, is_canceled, is_not_exist};
pub use texture_profile::{
    native_texture_needs_warm, validate_native_configuration, warm_texture_configuration,
};
pub use ui::parse_native_ui_bindings;
pub use warm::{WarmNativeArtifact, extract_warm_native_ui};

#[cfg(test)]
mod tests;

/// Diagnostics: the environment a build of Player objects runs in, for the
/// scripts with the given GUIDs.
pub fn debug_build_environment(project: &str, scripts: &[String]) -> anyhow::Result<String> {
    let environment = scripted::build_environment(project, &scripts.iter().cloned().collect())?;
    Ok(format!("{environment:#?}"))
}
