//! The C# compiler pipeline. Cache keys, manifests and command lines are persisted formats.
mod baseline;
mod cli;
mod compile;
mod graph;
mod host;
mod metadata;
mod netcode;
mod nilable;
mod packages;
pub use packages::PlainCompile;
mod phases;
mod prepare;
mod revision;
mod script_types;
mod server;
mod service;
pub(crate) mod settings;
mod source_graph;
mod stamp;
mod sys;
#[cfg(test)]
mod tests;

pub use cli::run_compiler_prepare_cli;
pub use compile::{
    assembly_needs_compile, compiler_manifest, deduplicate_references, dirty_assemblies,
    editor_source, equal_strings, lifecycle_sources_match, read_assembly_configuration,
    source_dirty_assemblies, valid_lifecycle_proof, verify_cache_files,
};
pub use host::{player_assembly_paths, stage_host};
pub use metadata::{
    METADATA_GUID_BUFFER_SIZE, METADATA_GUID_MAXIMUM, MetadataGuidIdentity, MetadataGuidIndex,
};
pub use netcode::{HOST_CODEGEN_DIRECTORY, NetcodeProcessor, needs_netcode};
pub use nilable::{NilMap, NilVec};
pub use phases::{COMPILE_PHASES, PATCH_PHASES, Phases};
pub use prepare::{
    HOST_LACKS_PACKAGE, PublishBaseline, canonical_configuration, order_compiler_assemblies,
    prepare_compiler_graph, prepare_compiler_graph_with, publish_baseline,
    register_compiler_assembly_slots,
};
pub use revision::{CodeRevision, player_files};
pub use server::{Client as CompilerClient, CompilerServer, analyzer_failure, csc_command};
pub use service::{Reply as ServiceReply, Service, ServiceError};
pub use source_graph::{managed_plugin, remove_source_native_initializers};
pub use stamp::{HashCache, PlayerMap, Stamp};
pub use sys::{BUILD_WITHOUT_SERVERS, CommandError, compiler_command, glob, path_match};
pub use sys::{Walk, WalkEntry, glob_lossy, is_not_exist, walk_dir};

use nilable::{is_empty, is_false, nz};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

pub const UNITY_VERSION: &str = crate::ENGINE_VERSION;

/// The store the compilers of a project share. It is not copied between
/// checkouts: an assembly holds the paths of the sources it was compiled
/// from, and hot reload compares it with what the checkout compiles.
pub fn project_compiler_store(project: &str) -> String {
    crate::sysutil::join(&[project, "Library/UnityMicroEditor/compiler-store"])
}

impl Compiler {
    /// The directory of one kind of entry of the store.
    pub(crate) fn stored(&self, kind: &str) -> String {
        match self.store.is_empty() {
            true => crate::sysutil::join(&[&self.cache, kind]),
            false => crate::sysutil::join(&[&self.store, kind]),
        }
    }
}

/// An f64 as JSON text: shortest round-trip digits, exponent form outside 1e-6..1e21.
pub fn float_layout(value: f64) -> serde_json::Number {
    let magnitude = value.abs();
    let text = if magnitude != 0.0 && !(1e-6..1e21).contains(&magnitude) {
        // Every exponent carries a sign; only "e-0N" is trimmed to "e-N".
        format!("{value:e}")
            .replacen("e", "e+", 1)
            .replace("e+-", "e-")
    } else {
        format!("{value}")
    };
    text.parse().unwrap_or_else(|_| 0.into())
}

fn float<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<serde_json::Number, D::Error> {
    Ok(float_layout(
        Option::<f64>::deserialize(deserializer)?.unwrap_or(0.0),
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeInitializer {
    #[serde(deserialize_with = "nz")]
    pub assembly: String,
    #[serde(rename = "typeName", deserialize_with = "nz")]
    pub type_name: String,
    #[serde(rename = "methodName", deserialize_with = "nz")]
    pub method_name: String,
    #[serde(deserialize_with = "nz")]
    pub phase: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileUpdate {
    #[serde(deserialize_with = "nz")]
    pub player: String,
    pub assemblies: NilVec<String>,
    #[serde(rename = "runtimeInitializers")]
    pub runtime_initializers: NilVec<RuntimeInitializer>,
    #[serde(rename = "compiledAssemblies")]
    pub compiled_assemblies: NilVec<String>,
    #[serde(rename = "elapsedSeconds", deserialize_with = "float")]
    pub elapsed_seconds: serde_json::Number,
    #[serde(rename = "sourceHashes")]
    pub source_hashes: NilMap<String>,
    #[serde(rename = "declarationChanged")]
    pub declaration_changed: NilVec<String>,
    #[serde(
        rename = "cacheReused",
        deserialize_with = "nz",
        skip_serializing_if = "is_false"
    )]
    pub cache_reused: bool,
    #[serde(rename = "dllHashes", skip_serializing_if = "NilMap::skip")]
    pub dll_hashes: NilMap<String>,
    /// The Player holds other settings managers than the one before it, which
    /// only a new Player process reads.
    #[serde(
        rename = "settingsChanged",
        deserialize_with = "nz",
        skip_serializing_if = "is_false"
    )]
    pub settings_changed: bool,
}
impl Default for CompileUpdate {
    fn default() -> Self {
        Self {
            player: String::new(),
            assemblies: NilVec::nil(),
            runtime_initializers: NilVec::nil(),
            compiled_assemblies: NilVec::nil(),
            elapsed_seconds: 0.into(),
            source_hashes: NilMap::nil(),
            declaration_changed: NilVec::nil(),
            cache_reused: false,
            dll_hashes: NilMap::nil(),
            settings_changed: false,
        }
    }
}
impl CompileUpdate {
    pub fn elapsed(&self) -> f64 {
        self.elapsed_seconds.as_f64().unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileAssembly {
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(deserialize_with = "nz")]
    pub path: String,
    #[serde(deserialize_with = "nz")]
    pub sha256: String,
    pub sources: NilVec<String>,
    pub references: NilVec<String>,
    #[serde(rename = "assemblyInputHash", deserialize_with = "nz")]
    pub assembly_input_hash: String,
    #[serde(rename = "lifecycle", deserialize_with = "nz")]
    pub lifecycle: LifecycleProof,
    #[serde(
        rename = "netcodePostprocessor",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub netcode_postprocessor: String,
    /// The key of the lifecycle helper whose rewrite this DLL was compiled
    /// from; empty in a preparation made before the helper was recorded. A
    /// session with another helper compiles the assembly again before it
    /// starts: its other caches are keyed by the helper too.
    #[serde(
        rename = "lifecycleHelper",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub lifecycle_helper: String,
}

/// What the lifecycle helper verified of an assembly's sources: the
/// `[RuntimeInitializeOnLoadMethod]` bridges it added and the primary
/// MonoScript class of each source file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleProof {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub verified: bool,
    #[serde(rename = "runtimeInitializers")]
    pub runtime_initializers: NilVec<RuntimeInitializer>,
    #[serde(rename = "scriptTypes", skip_serializing_if = "NilMap::skip")]
    pub script_types: NilMap<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileManifest {
    #[serde(rename = "baseReferences", skip_serializing_if = "NilVec::skip")]
    pub base_references: NilVec<String>,
    #[serde(rename = "engineReferences", skip_serializing_if = "NilVec::skip")]
    pub engine_references: NilVec<String>,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub backend: String,
    #[serde(
        rename = "engineProfileSHA256",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub engine_profile_sha256: String,
    #[serde(
        rename = "packageResolutionSHA256",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub package_resolution_sha256: String,
    #[serde(deserialize_with = "nz")]
    pub schema: i64,
    #[serde(deserialize_with = "nz")]
    pub project: String,
    #[serde(deserialize_with = "nz")]
    pub unity: String,
    #[serde(
        rename = "nativeOnly",
        deserialize_with = "nz",
        skip_serializing_if = "is_false"
    )]
    pub native_only: bool,
    #[serde(
        rename = "nativeSlotsSha256",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub native_slots_sha: String,
    pub assemblies: NilVec<CompileAssembly>,
    #[serde(rename = "compilerInputs")]
    pub compiler_inputs: NilMap<String>,
    pub scripts: NilVec<CompileScript>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileScript {
    #[serde(deserialize_with = "nz")]
    pub guid: String,
    #[serde(deserialize_with = "nz")]
    pub path: String,
    #[serde(deserialize_with = "nz")]
    pub assembly: String,
    #[serde(rename = "typeName", deserialize_with = "nz")]
    pub type_name: String,
    #[serde(rename = "hostProvided", deserialize_with = "nz")]
    pub host_provided: bool,
}

/// The anonymous `config` struct of an assembly configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssemblyOptions {
    #[serde(rename = "allowUnsafeCode", deserialize_with = "nz")]
    pub allow_unsafe: bool,
    #[serde(rename = "noEngineReferences", deserialize_with = "nz")]
    pub no_engine_references: bool,
    #[serde(rename = "overrideReferences", deserialize_with = "nz")]
    pub override_references: bool,
    #[serde(rename = "precompiledReferences")]
    pub precompiled: NilVec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssemblyConfig {
    #[serde(rename = "responseFiles", skip_serializing_if = "NilVec::skip")]
    pub response_files: NilVec<String>,
    #[serde(rename = "compilerReferences", skip_serializing_if = "NilVec::skip")]
    pub compiler_references: NilVec<String>,
    #[serde(rename = "compilerOptions", skip_serializing_if = "NilVec::skip")]
    pub compiler_options: NilVec<String>,
    #[serde(deserialize_with = "nz")]
    pub name: String,
    pub sources: NilVec<String>,
    pub dependencies: NilVec<String>,
    pub defines: NilVec<String>,
    #[serde(deserialize_with = "nz")]
    pub config: AssemblyOptions,
}

/// The graph exported through Unity's public
/// CompilationPipeline APIs or built by our own source graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompilerGraphExport {
    #[serde(
        rename = "unityVersion",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub unity_version: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub target: String,
    #[serde(
        rename = "scriptingBackend",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub scripting_backend: String,
    #[serde(rename = "baseReferences", skip_serializing_if = "NilVec::skip")]
    pub base_references: NilVec<String>,
    #[serde(rename = "engineReferences", skip_serializing_if = "NilVec::skip")]
    pub engine_references: NilVec<String>,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_false")]
    pub development: bool,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub backend: String,
    #[serde(
        rename = "engineProfileSha256",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub engine_profile_sha256: String,
    #[serde(
        rename = "packageResolutionSha256",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub package_resolution_sha256: String,
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub project: String,
    #[serde(deserialize_with = "nz")]
    pub unity: String,
    pub assemblies: NilVec<CompilerGraphAssembly>,
    pub scripts: NilVec<CompileScript>,
    #[serde(rename = "compilerInputs")]
    pub compiler_inputs: NilMap<String>,
    #[serde(rename = "hostAssemblies", skip_serializing_if = "NilVec::skip")]
    pub host_assemblies: NilVec<CompilerHostAssembly>,
    #[serde(rename = "sourceRoots", skip_serializing_if = "NilVec::skip")]
    pub source_roots: NilVec<CompilerSourceRoot>,
    /// The IL post-processor assemblies (`*.CodeGen`) of packages nobody
    /// edits, which the Editor runs over every assembly it compiles.
    #[serde(rename = "codegenAssemblies", skip_serializing_if = "Vec::is_empty")]
    pub codegen_assemblies: Vec<CodegenAssembly>,
}

/// One IL post-processor assembly of a package, compiled for the runner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodegenAssembly {
    pub name: String,
    pub sources: Vec<String>,
    /// The assemblies it names that a Player has: its runtime counterparts.
    pub references: Vec<String>,
    /// The precompiled assemblies it names, where the packages hold them.
    pub precompiled: Vec<String>,
    #[serde(rename = "allowUnsafe")]
    pub allow_unsafe: bool,
    /// Those of a Player assembly with the Editor's, as the Editor compiles
    /// post-processors.
    pub defines: Vec<String>,
    #[serde(rename = "compilerOptions")]
    pub compiler_options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompilerHostAssembly {
    #[serde(rename = "compiledReferences", skip_serializing_if = "NilVec::skip")]
    pub compiled_references: NilVec<String>,
    #[serde(rename = "responseFiles", skip_serializing_if = "NilVec::skip")]
    pub response_files: NilVec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Box<AssemblyConfig>>,
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(deserialize_with = "nz")]
    pub path: String,
    #[serde(deserialize_with = "nz")]
    pub sha256: String,
    pub sources: NilVec<String>,
    pub defines: NilVec<String>,
    #[serde(rename = "editorDefines")]
    pub editor_defines: NilVec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompilerSourceRoot {
    #[serde(deserialize_with = "nz")]
    pub path: String,
    #[serde(deserialize_with = "nz")]
    pub source: String,
}

/// The outer `responseFiles` field shadows the one embedded in the assembly
/// configuration, which is therefore neither read nor written and stays absent in `config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "GraphAssemblyWire", into = "GraphAssemblyWire")]
pub struct CompilerGraphAssembly {
    pub response_files: NilVec<String>,
    pub config: AssemblyConfig,
    pub path: String,
    pub sha256: String,
    pub references: NilVec<String>,
    pub compiled_references: NilVec<String>,
    /// An assembly of a package nobody edits (registry, built-in, Git) that
    /// the host does not provide: built as the Editor builds it, without the
    /// lifecycle rewrite, post-processed and added to the staged host.
    pub package_build: bool,
    /// In an Editor-domain graph: an assembly for the Editor platform alone,
    /// which the Editor compiles against the .NET Framework API.
    pub editor_only: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct GraphAssemblyWire {
    #[serde(rename = "responseFiles", skip_serializing_if = "NilVec::skip")]
    response_files: NilVec<String>,
    #[serde(rename = "compilerReferences", skip_serializing_if = "NilVec::skip")]
    compiler_references: NilVec<String>,
    #[serde(rename = "compilerOptions", skip_serializing_if = "NilVec::skip")]
    compiler_options: NilVec<String>,
    #[serde(deserialize_with = "nz")]
    name: String,
    sources: NilVec<String>,
    dependencies: NilVec<String>,
    defines: NilVec<String>,
    #[serde(deserialize_with = "nz")]
    config: AssemblyOptions,
    #[serde(deserialize_with = "nz")]
    path: String,
    #[serde(deserialize_with = "nz")]
    sha256: String,
    references: NilVec<String>,
    #[serde(rename = "compiledReferences")]
    compiled_references: NilVec<String>,
    #[serde(
        rename = "packageBuild",
        deserialize_with = "nz",
        skip_serializing_if = "is_false"
    )]
    package_build: bool,
    #[serde(
        rename = "editorOnly",
        deserialize_with = "nz",
        skip_serializing_if = "is_false"
    )]
    editor_only: bool,
}
impl From<GraphAssemblyWire> for CompilerGraphAssembly {
    fn from(wire: GraphAssemblyWire) -> Self {
        Self {
            response_files: wire.response_files,
            config: AssemblyConfig {
                response_files: NilVec::nil(),
                compiler_references: wire.compiler_references,
                compiler_options: wire.compiler_options,
                name: wire.name,
                sources: wire.sources,
                dependencies: wire.dependencies,
                defines: wire.defines,
                config: wire.config,
            },
            path: wire.path,
            sha256: wire.sha256,
            references: wire.references,
            compiled_references: wire.compiled_references,
            package_build: wire.package_build,
            editor_only: wire.editor_only,
        }
    }
}
impl From<CompilerGraphAssembly> for GraphAssemblyWire {
    fn from(assembly: CompilerGraphAssembly) -> Self {
        Self {
            response_files: assembly.response_files,
            compiler_references: assembly.config.compiler_references,
            compiler_options: assembly.config.compiler_options,
            name: assembly.config.name,
            sources: assembly.config.sources,
            dependencies: assembly.config.dependencies,
            defines: assembly.config.defines,
            config: assembly.config.config,
            path: assembly.path,
            sha256: assembly.sha256,
            references: assembly.references,
            compiled_references: assembly.compiled_references,
            package_build: assembly.package_build,
            editor_only: assembly.editor_only,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompilerPreparationResult {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub player: String,
    #[serde(deserialize_with = "nz")]
    pub compiler: CompileManifest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompiledCache {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub key: String,
    #[serde(deserialize_with = "nz")]
    pub name: String,
    pub files: NilMap<String>,
    #[serde(deserialize_with = "nz")]
    pub proof: LifecycleProof,
    #[serde(
        rename = "netcodeKey",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub netcode_key: String,
}

/// `Clone` is a deep copy: the copy replaces the original on success and is
/// dropped on failure.
#[derive(Debug, Clone, Default)]
pub struct Compiler {
    pub project: String,
    pub editor: String,
    pub cache: String,
    /// Where what is named by its inputs is kept: compiled assemblies, the
    /// helpers built from the tool's sources and what they read from
    /// assemblies. The compilers of a project share it (see
    /// [`project_compiler_store`]), so a preparation, the next one and the
    /// sessions after them compile a state of the sources once. Empty: in
    /// `cache`.
    pub store: String,
    pub player: String,
    pub dotnet: String,
    pub csc: String,
    pub helper: String,
    /// The cache key of `helper`: its sources and the SDK that builds them.
    pub helper_key: String,
    pub manifest: CompileManifest,
    pub configs: NilMap<AssemblyConfig>,
    pub configuration_hashes: NilMap<String>,
    pub sources: NilMap<String>,
    pub configuration: NilMap<String>,
    pub current_dlls: NilMap<String>,
    pub accepted_dll_hashes: NilMap<String>,
    pub proofs: NilMap<LifecycleProof>,
    pub base_references: NilVec<String>,
    pub engine_references: NilVec<String>,
    pub generators: NilVec<String>,
    pub host_roots: Vec<String>,
    pub expected_types: NilMap<NilMap<String>>,
    pub netcode: Option<NetcodeProcessor>,
    pub netcode_processed: NilMap<bool>,
    pub baseline_player: String,
    pub startup_complete: bool,
    /// Where the time of the last `compile` went.
    pub last_phases: Phases,
    /// The file map of the baseline Player directory, kept by every clone
    /// of this compiler: a compile that fails must not lose it.
    pub baseline_map: Arc<Mutex<Option<Arc<PlayerMap>>>>,
    /// The file map of the directory of `player`, when this process made
    /// or read it.
    pub player_map: Option<Arc<PlayerMap>>,
    /// The compiler server of a watched session; `None` compiles every
    /// assembly in a process of its own.
    pub compiler_server: Option<Arc<CompilerServer>>,
    /// `UME_COMPILER_VERIFY=1`: every served compile is compared with a
    /// compile in a process of its own.
    pub verify_compiles: bool,
    /// Whether the lifecycle helper runs as a service of the session.
    pub serve_helper: bool,
    /// The service of the lifecycle helper, kept by every clone of this
    /// compiler and bound to the helper it was started from.
    pub helper_service: Arc<Mutex<Option<Arc<Service>>>>,
    /// The content hashes of inputs that do not change during a session:
    /// configuration inputs, references, tools. Off, every check reads.
    pub hashes: HashCache,
    /// The hash of the settings managers the next code revision's Player
    /// holds (`settings::player_managers_hash`), set by each compile.
    pub settings_key: String,
}

/// `UME_COMPILER_COLD=1`: a session starts no process that outlives one
/// request and keeps no hash between two compiles.
pub fn compiler_cold() -> bool {
    std::env::var_os("UME_COMPILER_COLD").is_some_and(|value| value == "1")
}

impl Compiler {
    /// Gives a compiler the processes and caches that a watched session
    /// keeps between two compiles, unless `UME_COMPILER_COLD=1`. Nothing
    /// starts here: each starts at its first use, on the calling thread,
    /// whose end ends it.
    pub fn serve(&mut self, output: &str) {
        self.verify_compiles =
            std::env::var_os("UME_COMPILER_VERIFY").is_some_and(|value| value == "1");
        if compiler_cold() {
            return;
        }
        self.serve_helper = true;
        self.hashes = HashCache::on();
        self.compiler_server = Some(Arc::new(CompilerServer::new(
            &self.dotnet,
            &self.csc,
            output,
        )));
    }
}

/// `<player dir>/<player stem>_Data`.
/// Whether a rejected update names a managed configuration change the
/// prepared graph cannot absorb. Such an update needs a new preparation; an
/// ordinary C# compile error does not.
pub fn requires_new_graph(message: &str) -> bool {
    [
        "regenerate source graph",
        "fresh managed graph",
        "outside the prepared managed graph",
    ]
    .iter()
    .any(|text| message.contains(text))
}

pub fn player_data(player: &str) -> String {
    let base = crate::sysutil::base(player);
    let stem = base
        .strip_suffix(crate::sysutil::ext(&base))
        .unwrap_or(&base)
        .to_string();
    crate::sysutil::join(&[&crate::sysutil::dir(player), &format!("{stem}_Data")])
}
pub fn player_managed(player: &str) -> String {
    crate::sysutil::join(&[&player_data(player), "Managed"])
}
pub(crate) fn join(parts: &[&str]) -> String {
    crate::sysutil::join(parts)
}

/// The paths a glob pattern matches, in name order.
pub fn sys_glob(pattern: &str) -> Vec<String> {
    sys::glob_lossy(pattern)
}
