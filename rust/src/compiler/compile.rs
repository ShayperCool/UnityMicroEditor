//! The compiler state, configuration drift checks, references,
//! incremental compilation and the Roslyn/lifecycle helper invocations.
use super::{
    AssemblyConfig, CompileAssembly, CompileManifest, CompileUpdate, CompiledCache, Compiler,
    LifecycleCallback, LifecycleProof, NilMap, NilVec, RuntimeInitializer, float_layout, join,
    netcode::needs_netcode,
    nilable::{nz, unmarshal},
    player_data,
    sys::{self, Walk, compiler_command},
};
use crate::{
    cancel::Context,
    project::{is_owned_cache_path, lexpath::rune_lower},
    registry, sha256, sysutil,
    util::{
        atomic_json, canonical_destination, file_hash, marshal, micro_editor_root, read_json_file,
        valid_sha256, within_directory,
    },
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, value::RawValue};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::Instant,
};

fn raw<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}
/// Canonical JSON of a decoded value: object keys sorted at every level,
/// numbers verbatim.
pub(crate) fn canonical_json(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(flag) => out.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Value::Number(number) => out.extend_from_slice(number.to_string().as_bytes()),
        Value::String(text) => out.extend_from_slice(crate::jsondoc::quote(text).as_bytes()),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                canonical_json(item, out);
            }
            out.push(b']');
        }
        Value::Object(members) => {
            let sorted: BTreeMap<&String, &Value> = members.iter().collect();
            out.push(b'{');
            for (index, (key, member)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(crate::jsondoc::quote(key).as_bytes());
                out.push(b':');
                canonical_json(member, out);
            }
            out.push(b'}');
        }
    }
}

/// The compiler plan of a baseline and the file that
/// locates its assembly descriptors.
pub fn compiler_manifest(filename: &str) -> Result<(CompileManifest, String)> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Wrapper {
        compiler: Option<Box<RawValue>>,
        #[serde(deserialize_with = "nz")]
        manifest: String,
    }
    let mut filename = filename.to_string();
    let mut data = sys::read(&filename)?;
    let wrapper: Wrapper = unmarshal(&data)?;
    let embedded = wrapper.compiler.is_some();
    if let Some(compiler) = &wrapper.compiler {
        data = compiler.get().as_bytes().to_vec();
    } else {
        let schema = serde_json::from_slice::<Value>(&data)
            .ok()
            .and_then(|header| header.get("schema").and_then(Value::as_i64));
        if schema != Some(1) {
            if !wrapper.manifest.is_empty() {
                filename = wrapper.manifest.clone();
            }
            filename = join(&[&sysutil::dir(&filename), "Managed", "compile.json"]);
            data = sys::read(&filename)?;
        }
    }
    let manifest: CompileManifest = unmarshal(&data)?;
    if manifest.schema != 1
        || manifest.assemblies.is_empty() && !manifest.native_only
        || manifest.assemblies.len() > 256
        || manifest.compiler_inputs.is_empty()
        || manifest.native_only
            && (!manifest.assemblies.is_empty() || !valid_sha256(&manifest.native_slots_sha))
    {
        bail!("baseline lacks a verified raw compiler plan");
    }
    if !manifest.backend.is_empty() && manifest.backend != "authored_source"
        || manifest.backend == "authored_source"
            && (!valid_sha256(&manifest.engine_profile_sha256)
                || !valid_sha256(&manifest.package_resolution_sha256))
    {
        bail!("compiler source backend lacks exact engine/package provenance");
    }
    if manifest.native_only && manifest.scripts.iter().any(|script| !script.host_provided) {
        bail!("native-only baseline contains an unverified gameplay script");
    }
    if embedded && !wrapper.manifest.is_empty() {
        // An accepted DLL can remain in the compiler-preparation directory while
        // its new exact configuration lives in the sibling source-preparation.
        // Use the explicit scene locator, never scan arbitrary sibling caches.
        let locator = wrapper.manifest;
        if sysutil::eval_symlinks(&locator).ok().as_deref() != Some(locator.as_str())
            || !sysutil::is_abs(&locator)
            || sysutil::clean(&locator) != locator
            || !is_owned_cache_path(&manifest.project, &locator)
        {
            bail!("compiler scene locator must be canonical and inside owned project cache");
        }
        if !fs::metadata(&locator).is_ok_and(|stamp| stamp.is_file() && stamp.len() <= 32 << 20) {
            bail!("compiler scene locator must name a bounded regular manifest");
        }
        let content = sys::read(&locator)?;
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Scene {
            #[serde(rename = "schemaVersion", deserialize_with = "nz")]
            schema_version: i64,
            #[serde(deserialize_with = "nz")]
            supported: bool,
            #[serde(rename = "projectRoot", deserialize_with = "nz")]
            project_root: String,
        }
        if !unmarshal::<Scene>(&content).is_ok_and(|scene| {
            scene.schema_version == 1 && scene.supported && scene.project_root == manifest.project
        }) {
            bail!("compiler scene locator does not describe the same supported project");
        }
        filename = locator;
    }
    Ok((manifest, filename))
}

/// The exact configuration descriptor of a
/// baseline assembly and its canonical digest.
pub fn read_assembly_configuration(
    baseline_path: &str,
    assembly: &CompileAssembly,
    expected_manifest: Option<&CompileManifest>,
) -> Result<(AssemblyConfig, String)> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Descriptor {
        #[serde(deserialize_with = "nz")]
        backend: String,
        #[serde(rename = "engineProfileSHA256", deserialize_with = "nz")]
        engine_profile_sha256: String,
        #[serde(rename = "packageResolutionSHA256", deserialize_with = "nz")]
        package_resolution_sha256: String,
        #[serde(rename = "assemblyConfiguration", deserialize_with = "raw")]
        assembly_configuration: Option<Box<RawValue>>,
        #[serde(rename = "compilerInputs")]
        compiler_inputs: NilMap<String>,
        assemblies: NilVec<CompileAssembly>,
    }
    let mut starts = vec![sysutil::dir(baseline_path)];
    if sysutil::is_abs(&assembly.path) && sysutil::clean(&assembly.path) == assembly.path {
        // A scene/content baseline may embed the compiler plan elsewhere.
        // Its immutable DLL still locates the original verified descriptors.
        starts.push(sysutil::dir(&assembly.path));
    }
    let mut visited = BTreeSet::new();
    for start in starts {
        let mut folder = start;
        loop {
            if !visited.insert(folder.clone()) {
                break;
            }
            for cache_directory in [
                "compiler-cache/assemblies",
                "compiler-cache/baseline-assemblies",
                "baseline-assemblies",
            ] {
                let filename = join(&[
                    &folder,
                    cache_directory,
                    &assembly.assembly_input_hash,
                    "compile.json",
                ]);
                let data = match sys::read(&filename) {
                    Ok(data) => data,
                    Err(error) if sys::is_not_exist(&error) => continue,
                    Err(error) => return Err(error),
                };
                let descriptor: Descriptor = unmarshal(&data)?;
                if let Some(expected) = expected_manifest
                    && (descriptor.backend != expected.backend
                        || descriptor.engine_profile_sha256 != expected.engine_profile_sha256
                        || descriptor.package_resolution_sha256
                            != expected.package_resolution_sha256)
                {
                    bail!(
                        "exact configuration backend/profile differs from compiler baseline: {}",
                        assembly.name
                    );
                }
                let text = descriptor
                    .assembly_configuration
                    .as_ref()
                    .map_or("", |raw| raw.get());
                let configuration: AssemblyConfig = unmarshal(text.as_bytes())?;
                let canonical: Value = serde_json::from_str(text)?;
                let mut encoded = Vec::new();
                canonical_json(&canonical, &mut encoded);
                let digest = sha256(&encoded);
                let source_backend = descriptor.backend == "authored_source";
                if configuration.name != assembly.name
                    || !equal_strings(&configuration.sources, &assembly.sources)
                    || !equal_strings(&configuration.dependencies, &assembly.references)
                    || descriptor.compiler_inputs.at("assemblyConfiguration") != digest
                    || descriptor.assemblies.len() != 1
                    || descriptor.assemblies[0].sha256 != assembly.sha256
                    || (!source_backend
                        && (configuration.config.override_references
                            || !configuration.config.precompiled.is_empty()
                            || !configuration.compiler_references.is_empty()
                            || !configuration.compiler_options.is_empty()))
                    || (source_backend
                        && (!valid_sha256(&descriptor.engine_profile_sha256)
                            || !valid_sha256(&descriptor.package_resolution_sha256)))
                    || configuration.defines.is_empty()
                {
                    bail!(
                        "baseline assembly configuration/proof mismatch: {}",
                        assembly.name
                    );
                }
                return Ok((configuration, digest));
            }
            let parent = sysutil::dir(&folder);
            if parent == folder {
                break;
            }
            folder = parent;
        }
    }
    bail!(
        "exact assembly configuration cache is missing: {}",
        assembly.name
    )
}

pub fn equal_strings(first: &[String], second: &[String]) -> bool {
    first == second
}

pub fn valid_lifecycle_proof(proof: &LifecycleProof, name: &str) -> bool {
    let mut phases: Vec<&str> = proof.phases.iter().map(String::as_str).collect();
    phases.sort_unstable();
    if proof.schema_version != 1
        || !proof.verified
        || !proof.scriptable_object_barrier
        || phases != ["Awake", "OnDestroy", "OnDisable", "OnEnable"]
    {
        return false;
    }
    if proof.runtime_initializers.iter().any(|initializer| {
        initializer.assembly != name
            || initializer.type_name.is_empty()
            || initializer.method_name.is_empty()
            || !(0..=4).contains(&initializer.phase)
    }) {
        return false;
    }
    if proof.callbacks != proof.methods.len() as i64 {
        return false;
    }
    if proof.methods.iter().any(|method| {
        method.type_name.is_empty()
            || method.source.is_empty()
            || !matches!(
                method.phase.as_str(),
                "Awake" | "OnEnable" | "OnDisable" | "OnDestroy"
            )
    }) {
        return false;
    }
    proof.script_types.iter().all(|(source, name)| {
        !source.is_empty()
            && !name.is_empty()
            && name.len() <= 1024
            && !name.contains(['\0', '\r', '\n'])
    })
}

pub fn lifecycle_sources_match(proof: &LifecycleProof, sources: &[String]) -> bool {
    proof
        .script_types
        .iter()
        .all(|(source, name)| !name.is_empty() && sources.contains(source))
        && proof
            .methods
            .iter()
            .all(|callback| sources.contains(&callback.source))
}

pub fn editor_source(filename: &str) -> bool {
    filename.split('/').any(|part| part == "Editor")
}

/// The first path of every file name wins.
pub fn deduplicate_references(paths: &[String]) -> NilVec<String> {
    let mut seen = BTreeSet::new();
    let mut result = NilVec::nil();
    for filename in paths {
        if seen.insert(sysutil::base(filename)) {
            result.push(filename.clone());
        }
    }
    result
}

pub fn dirty_assemblies(
    assemblies: &[CompileAssembly],
    previous: &NilMap<String>,
    current: &NilMap<String>,
) -> BTreeSet<String> {
    let mut dirty = BTreeSet::new();
    for assembly in assemblies {
        if assembly
            .sources
            .iter()
            .any(|source| current.at(source) != previous.at(source))
            || assembly
                .references
                .iter()
                .any(|dependency| dirty.contains(dependency))
        {
            dirty.insert(assembly.name.clone());
        }
    }
    dirty
}

pub fn source_dirty_assemblies(
    assemblies: &[CompileAssembly],
    previous: &NilMap<String>,
    current: &NilMap<String>,
) -> BTreeSet<String> {
    assemblies
        .iter()
        .filter(|assembly| {
            assembly
                .sources
                .iter()
                .any(|source| current.at(source) != previous.at(source))
        })
        .map(|assembly| assembly.name.clone())
        .collect()
}

pub fn assembly_needs_compile(
    assembly: &CompileAssembly,
    source_dirty: &BTreeSet<String>,
    declaration_changed: &BTreeMap<String, bool>,
) -> bool {
    source_dirty.contains(&assembly.name)
        || assembly.references.iter().any(|dependency| {
            declaration_changed
                .get(dependency)
                .copied()
                .unwrap_or(false)
        })
}

pub fn verify_cache_files(directory: &str, hashes: &NilMap<String>) -> bool {
    !hashes.is_empty()
        && hashes.iter().all(|(name, expected)| {
            sysutil::base(name) == *name
                && !expected.is_empty()
                && file_hash(&join(&[directory, name])).is_ok_and(|actual| actual == *expected)
        })
}

#[derive(Serialize)]
struct LifecycleSource {
    #[serde(rename = "OriginalPath")]
    original_path: String,
    #[serde(rename = "OutputPath")]
    output_path: String,
    #[serde(rename = "Path")]
    path: String,
}
/// Keys are sorted.
#[derive(Serialize)]
struct LifecycleRequest<'a> {
    #[serde(rename = "AllowUnsafe")]
    allow_unsafe: bool,
    #[serde(rename = "AssemblyName")]
    assembly_name: &'a str,
    #[serde(rename = "Defines")]
    defines: &'a NilVec<String>,
    #[serde(rename = "ExecutionOrders")]
    execution_orders: &'a BTreeMap<String, i32>,
    #[serde(rename = "MarkerPath")]
    marker_path: &'a str,
    #[serde(rename = "NativeBaseAssemblies")]
    native_base_assemblies: &'a NilVec<String>,
    #[serde(rename = "References")]
    references: &'a NilVec<String>,
    #[serde(rename = "Sources")]
    sources: &'a [LifecycleSource],
}
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct WireInitializer {
    #[serde(deserialize_with = "nz")]
    assembly: String,
    #[serde(deserialize_with = "nz")]
    type_name: String,
    #[serde(deserialize_with = "nz")]
    method_name: String,
    #[serde(deserialize_with = "nz")]
    phase: i64,
}
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct WireProof {
    #[serde(deserialize_with = "nz")]
    schema_version: i64,
    #[serde(deserialize_with = "nz")]
    verified: bool,
    phases: NilVec<String>,
    #[serde(deserialize_with = "nz")]
    scriptable_object_barrier: bool,
    callbacks: NilVec<LifecycleCallback>,
    runtime_initializers: NilVec<WireInitializer>,
    script_types: NilMap<String>,
}

/// One document read by `collect_expected_script_types`. The scene manifest is decoded
/// into the struct that already holds the baseline wrapper, so absent fields
/// (and array elements at the same index) keep their earlier values.
#[derive(Deserialize, Default)]
#[serde(default)]
struct BindingAsset {
    kind: Option<String>,
    guid: Option<String>,
    #[serde(rename = "typeName")]
    type_name: Option<String>,
}
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct BindingDocument {
    manifest: Option<String>,
    #[serde(rename = "projectRoot")]
    project_root: Option<String>,
    #[serde(deserialize_with = "present")]
    assets: Option<Option<Vec<Option<BindingAsset>>>>,
}
#[derive(Default, Clone)]
struct Binding {
    kind: String,
    guid: String,
    type_name: String,
}
#[derive(Default)]
struct Bindings {
    manifest: String,
    project_root: String,
    assets: Vec<Binding>,
}
impl Bindings {
    fn decode(&mut self, path: &str) -> Result<()> {
        let document: BindingDocument = read_json_file::<Option<_>>(path)?.unwrap_or_default();
        if let Some(manifest) = document.manifest {
            self.manifest = manifest;
        }
        if let Some(root) = document.project_root {
            self.project_root = root;
        }
        match document.assets {
            None => {}
            Some(None) => self.assets.clear(),
            Some(Some(list)) => {
                let previous = std::mem::take(&mut self.assets);
                for (index, asset) in list.into_iter().enumerate() {
                    let mut merged = previous.get(index).cloned().unwrap_or_default();
                    if let Some(asset) = asset {
                        if let Some(kind) = asset.kind {
                            merged.kind = kind;
                        }
                        if let Some(guid) = asset.guid {
                            merged.guid = guid;
                        }
                        if let Some(name) = asset.type_name {
                            merged.type_name = name;
                        }
                    }
                    self.assets.push(merged);
                }
            }
        }
        Ok(())
    }
}

impl Compiler {
    /// Validates a prepared baseline against the staged Player
    /// and the installed SDK.
    pub fn new(
        project: &str,
        baseline_manifest: &str,
        staged_player: &str,
        editor: &str,
        cache: &str,
    ) -> Result<Self> {
        let project = crate::project::lexpath::abs(project)?;
        let project = sysutil::eval_symlinks(&project)?;
        let cache = crate::project::lexpath::abs(cache)?;
        if canonical_destination(&cache).ok().as_deref() != Some(cache.as_str())
            || !is_owned_cache_path(&project, &cache)
        {
            bail!("Compiler cache must be under owned project Temp or Library/UnityMicroEditor");
        }
        let (manifest, baseline_path) = compiler_manifest(baseline_manifest)?;
        if manifest.project != project || manifest.unity != editor {
            bail!("compiler baseline project or Unity SDK differs from launch");
        }
        let mut compiler = Compiler {
            project,
            editor: editor.into(),
            cache,
            player: staged_player.into(),
            baseline_player: staged_player.into(),
            manifest: manifest.clone(),
            configs: NilMap::make(),
            configuration_hashes: NilMap::make(),
            sources: NilMap::make(),
            configuration: NilMap::make(),
            current_dlls: NilMap::make(),
            accepted_dll_hashes: NilMap::make(),
            proofs: NilMap::make(),
            ..Default::default()
        };
        let data = player_data(staged_player);
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Slots {
            names: NilVec<String>,
        }
        let slots_file = join(&[&data, "ScriptingAssemblies.json"]);
        let slots: BTreeSet<String> = read_json_file::<Option<Slots>>(&slots_file)
            .map_err(|error| anyhow!("registered Mono assembly slots are missing: {error}"))?
            .unwrap_or_default()
            .names
            .iter()
            .cloned()
            .collect();
        if manifest.native_only {
            if !file_hash(&slots_file).is_ok_and(|actual| actual == manifest.native_slots_sha)
                || !slots.contains("UnityMicroEditor.dll")
            {
                bail!("native-only host registrations differ from its verified compiler baseline");
            }
            file_hash(&join(&[&data, "Managed/UnityMicroEditor.dll"]))
                .map_err(|error| anyhow!("native-only host runtime bridge is missing: {error}"))?;
        }
        compiler.initialize_sdk()?;
        if manifest.backend == "authored_source" {
            if manifest.base_references.is_empty() || manifest.engine_references.is_empty() {
                bail!("Source baseline lacks exact engine/BCL reference sets");
            }
            compiler.base_references = manifest.base_references.clone();
            compiler.engine_references = manifest.engine_references.clone();
            for path in manifest
                .base_references
                .iter()
                .chain(&manifest.engine_references)
            {
                if !file_hash(path).is_ok_and(|hash| hash == manifest.compiler_inputs.at(path)) {
                    bail!("Source baseline engine/BCL reference differs: {path}");
                }
            }
        }
        let mut game_names = BTreeSet::new();
        for assembly in &manifest.assemblies {
            if !game_names.insert(assembly.name.clone()) {
                bail!(
                    "duplicate game assembly in compiler plan: {}",
                    assembly.name
                );
            }
        }
        let mut ordered = BTreeSet::new();
        let managed = join(&[&data, "Managed"]);
        for assembly in &manifest.assemblies {
            for dependency in &assembly.references {
                if game_names.contains(dependency) && !ordered.contains(dependency) {
                    bail!(
                        "managed graph is not in dependency order: {}",
                        assembly.name
                    );
                }
            }
            ordered.insert(assembly.name.clone());
            if assembly.name.is_empty()
                || sysutil::base(&assembly.name) != assembly.name
                || assembly.sources.is_empty()
                || !assembly.netcode_postprocessor.is_empty()
                    && (!needs_netcode(assembly) || !valid_sha256(&assembly.netcode_postprocessor))
                || !assembly.lifecycle_helper.is_empty()
                    && !valid_sha256(&assembly.lifecycle_helper)
                || !valid_lifecycle_proof(&assembly.lifecycle_barrier, &assembly.name)
                || !lifecycle_sources_match(&assembly.lifecycle_barrier, &assembly.sources)
            {
                bail!(
                    "compiler baseline assembly lacks verified lifecycle barrier: {}",
                    assembly.name
                );
            }
            if !slots.contains(&format!("{}.dll", assembly.name)) {
                bail!(
                    "game assembly lacks a registered native Mono slot: {}",
                    assembly.name
                );
            }
            let (config, hash) =
                read_assembly_configuration(&baseline_path, assembly, Some(&manifest))?;
            compiler.configs.insert(&assembly.name, config);
            compiler.configuration_hashes.insert(&assembly.name, hash);
            let dll = join(&[&managed, &format!("{}.dll", assembly.name)]);
            if !file_hash(&dll).is_ok_and(|hash| hash == assembly.sha256) {
                bail!(
                    "staged Player game DLL differs from compiler baseline: {}",
                    assembly.name
                );
            }
            compiler.current_dlls.insert(&assembly.name, dll);
            compiler
                .proofs
                .insert(&assembly.name, assembly.lifecycle_barrier.clone());
            compiler
                .accepted_dll_hashes
                .insert(&assembly.name, assembly.sha256.clone());
            for source in &assembly.sources {
                let digest = manifest.compiler_inputs.at(source);
                if digest.is_empty() {
                    bail!("baseline lacks runtime source digest: {source}");
                }
                compiler.sources.insert(source, digest.to_string());
                let meta = format!("{source}.meta");
                let hash = manifest.compiler_inputs.at(&meta);
                if !hash.is_empty() {
                    compiler
                        .configuration
                        .insert(meta.clone(), hash.to_string());
                }
            }
        }
        compiler.verify_staged_native_managed_inputs(&managed, &game_names)?;
        compiler.collect_configuration_inputs();
        compiler.collect_expected_script_types(baseline_manifest)?;
        compiler.source_graph_candidate(&[])?;
        let tools: Vec<String> = [compiler.dotnet.clone(), compiler.csc.clone()]
            .into_iter()
            .chain(compiler.generators.iter().cloned())
            .collect();
        for filename in tools {
            fs::metadata(&filename).map_err(|e| sys::os_error("stat", &filename, &e))?;
            let expected = manifest.compiler_inputs.at(&filename);
            if !expected.is_empty() && !file_hash(&filename).is_ok_and(|actual| actual == expected)
            {
                bail!("Unity compiler SDK input differs from baseline: {filename}");
            }
        }
        Ok(compiler)
    }

    /// Whether a session can start on the prepared baseline with the managed
    /// sources as they are on disk: the part of [`Compiler::new`] and of the
    /// startup compile that reads the graph, without a Player. The error is
    /// the one the runner's start would end with ("... requires a fresh
    /// managed graph"). It passes when every configuration input the
    /// baseline recorded (assembly definitions, references, packages and
    /// their manifest, project settings, script metadata) is as it was, no
    /// source of the graph is gone and `Assets` holds no managed
    /// configuration outside the graph; C# sources may differ in content
    /// and new ones may have been added, which the startup compile takes.
    ///
    /// A request identity or a receipt says what a preparation was asked
    /// for. This says what it was made from.
    pub fn graph_accepts_sources(project: &str, baseline_manifest: &str) -> Result<()> {
        let project = crate::project::lexpath::abs(project)?;
        let project = sysutil::eval_symlinks(&project)?;
        let (manifest, baseline_path) = match compiler_manifest(baseline_manifest) {
            Ok(read) => read,
            // A plan without a game assembly is no plan a session compiles
            // from, and it still says which managed inputs it was made
            // with: there is nothing but those to compare.
            Err(error) => {
                #[derive(Deserialize, Default)]
                #[serde(default)]
                struct Wrapper {
                    compiler: Option<CompileManifest>,
                }
                let embedded = sys::read(baseline_manifest)
                    .ok()
                    .and_then(|data| unmarshal::<Wrapper>(&data).ok())
                    .and_then(|wrapper| wrapper.compiler)
                    .filter(|manifest| manifest.schema == 1 && manifest.assemblies.is_empty());
                match embedded {
                    Some(manifest) => (manifest, baseline_manifest.to_string()),
                    None => return Err(error),
                }
            }
        };
        if manifest.project != project {
            bail!("compiler baseline belongs to another project");
        }
        let mut compiler = Compiler {
            project,
            manifest: manifest.clone(),
            configs: NilMap::make(),
            configuration_hashes: NilMap::make(),
            sources: NilMap::make(),
            configuration: NilMap::make(),
            ..Default::default()
        };
        for assembly in &manifest.assemblies {
            let (config, hash) =
                read_assembly_configuration(&baseline_path, assembly, Some(&manifest))?;
            compiler.configs.insert(&assembly.name, config);
            compiler.configuration_hashes.insert(&assembly.name, hash);
            // The plan's assembly is the slot a new source under Assets is
            // compiled into, as the runner's start takes it; without one
            // every addition misfit.
            let slot = match assembly.path.is_empty() {
                true => format!("{}.dll", assembly.name),
                false => assembly.path.clone(),
            };
            compiler.current_dlls.insert(&assembly.name, slot);
            for source in &assembly.sources {
                let digest = manifest.compiler_inputs.at(source);
                if digest.is_empty() {
                    bail!("baseline lacks runtime source digest: {source}");
                }
                if sys::absent(source) {
                    bail!(
                        "a source of the prepared managed graph is gone; regenerate source graph: {source}"
                    );
                }
                compiler.sources.insert(source, digest.to_string());
                let meta = format!("{source}.meta");
                let hash = manifest.compiler_inputs.at(&meta);
                if !hash.is_empty() {
                    compiler
                        .configuration
                        .insert(meta.clone(), hash.to_string());
                }
            }
        }
        compiler.collect_configuration_inputs();
        compiler.source_graph_candidate(&[]).map(drop)
    }

    /// The active Player is a copy of the prepared native host. Match native DLL
    /// fingerprints against that copy, rather than checking only the original
    /// cache paths. Our own bridge is intentionally refreshed by `stage_host`.
    pub fn verify_staged_native_managed_inputs(
        &self,
        managed: &str,
        game_names: &BTreeSet<String>,
    ) -> Result<()> {
        let mut expected: BTreeMap<String, String> = BTreeMap::new();
        for (path, hash) in &self.manifest.compiler_inputs {
            if sysutil::ext(path) != ".dll"
                || sysutil::base(&sysutil::dir(path)) != "Managed"
                || !sysutil::base(&sysutil::dir(&sysutil::dir(path))).ends_with("_Data")
            {
                continue;
            }
            let name = sysutil::base(path);
            if name == "UnityMicroEditor.dll"
                || game_names.contains(name.strip_suffix(".dll").unwrap_or(&name))
            {
                continue;
            }
            if let Some(previous) = expected.get(&name)
                && !previous.is_empty()
                && previous != hash
            {
                bail!("native managed inputs disagree on their baseline digest: {name}");
            }
            expected.insert(name, hash.clone());
        }
        for (name, hash) in &expected {
            let path = join(&[managed, name]);
            if canonical_destination(&path).ok().as_deref() != Some(path.as_str()) {
                bail!("staged native managed input contains a symlink: {name}");
            }
            if !file_hash(&path).is_ok_and(|actual| actual == *hash) {
                bail!(
                    "staged native managed input differs from its verified compiler baseline: {name}"
                );
            }
        }
        Ok(())
    }

    /// Locates Roslyn, the Mono BCL, the engine modules and
    /// Unity's source generators inside the installation.
    pub fn initialize_sdk(&mut self) -> Result<()> {
        let data = join(&[&sysutil::dir(&self.editor), "Data"]);
        self.dotnet = join(&[&data, "DotNetSdk", "dotnet"]);
        let mut compilers = sys::glob_lossy(&join(&[
            &data,
            "DotNetSdk",
            "sdk",
            "*",
            "Roslyn",
            "bincore",
            "csc.dll",
        ]));
        if compilers.is_empty() {
            bail!("installed Unity Roslyn SDK is missing");
        }
        compilers.sort();
        self.csc = compilers.pop().unwrap_or_default();
        let bcl = join(&[&data, "MonoBleedingEdge", "lib", "mono", "unityaot-linux"]);
        let mut base = sys::glob_lossy(&join(&[&bcl, "*.dll"]));
        base.extend(sys::glob_lossy(&join(&[&bcl, "Facades", "*.dll"])));
        self.base_references = deduplicate_references(&base);
        let engine = join(&[
            &data,
            "PlaybackEngines",
            "LinuxStandaloneSupport",
            "Variations",
            "mono",
            "Managed",
        ]);
        let mut references = NilVec::nil();
        references.append(sys::glob_lossy(&join(&[&engine, "UnityEngine*.dll"])));
        references.append(sys::glob_lossy(&join(&[&engine, "Unity.Scripting.dll"])));
        self.engine_references = references;
        for name in [
            "Unity.Properties.SourceGenerator.dll",
            "Unity.SourceGenerators.dll",
            "Unity.UIToolkit.SourceGenerator.dll",
        ] {
            self.generators.push(join(&[
                &data,
                "Tools/BuildPipeline/Unity.SourceGenerators",
                name,
            ]));
        }
        Ok(())
    }

    pub fn collect_configuration_inputs(&mut self) {
        let inputs = self.manifest.compiler_inputs.clone();
        let assets = join(&[&self.project, "Assets"]);
        let settings = format!("{}/", join(&[&self.project, "ProjectSettings"]));
        let source_backend = self.manifest.backend == "authored_source";
        for (filename, hash) in &inputs {
            let extension = rune_lower(sysutil::ext(filename));
            if extension == ".asmdef" && within_directory(filename, &assets) {
                #[derive(Deserialize, Default)]
                #[serde(default)]
                struct Definition {
                    #[serde(deserialize_with = "nz")]
                    name: String,
                }
                if read_json_file::<Option<Definition>>(filename).is_ok_and(|definition| {
                    definition.unwrap_or_default().name == "UnityMicroEditor"
                }) {
                    self.host_roots.push(sysutil::dir(filename));
                }
            }
            let dll_reference = extension == ".dll" || rune_lower(filename).ends_with(".dll.meta");
            // The host-owned bridge is independently rebuilt/verified by
            // stage_host and references() uses that staged DLL. Its old Player
            // source location is not a compiler input for this revision.
            if dll_reference && self.host_bridge_reference(filename) {
                continue;
            }
            let mut configuration = dll_reference
                || matches!(extension.as_str(), ".asmdef" | ".asmref" | ".rsp")
                || sysutil::base(filename) == "package.json"
                || filename.ends_with(".asmdef.meta")
                || filename.ends_with(".asmref.meta")
                || filename.starts_with(&settings)
                || *filename == join(&[&self.project, "Packages/manifest.json"])
                || *filename == join(&[&self.project, "Packages/packages-lock.json"]);
            if source_backend
                && self.sources.at(filename).is_empty()
                && !self.host_bridge_reference(filename)
            {
                configuration = true;
            }
            if configuration
                && (source_backend || dll_reference || within_directory(filename, &self.project))
            {
                self.configuration.insert(filename, hash.clone());
            }
        }
    }

    pub fn host_bridge_reference(&self, filename: &str) -> bool {
        let filename = filename.strip_suffix(".meta").unwrap_or(filename);
        sysutil::base(filename) == "UnityMicroEditor.dll"
            && sysutil::base(&sysutil::dir(filename)) == "Managed"
            && sysutil::base(&sysutil::dir(&sysutil::dir(filename))).ends_with("_Data")
            && !within_directory(filename, &join(&[&self.project, "Assets"]))
            && !within_directory(filename, &join(&[&self.project, "Packages"]))
            && !within_directory(filename, &join(&[&self.project, "Library/PackageCache"]))
    }

    fn absolute(&self, filename: &str) -> String {
        if sysutil::is_abs(filename) {
            sysutil::clean(filename)
        } else {
            join(&[&self.project, filename])
        }
    }

    pub fn check_configuration(&self, changed: &[String]) -> Result<()> {
        self.check_fixed_configuration()?;
        let tracked = self.graph_source_assemblies();
        for filename in changed {
            let filename = self.absolute(filename);
            let unowned = !self.host_owned(&filename) && !editor_source(&filename);
            if filename.ends_with(".cs")
                && !tracked.contains_key(&filename)
                && self.manifest.compiler_inputs.at(&filename).is_empty()
                && unowned
            {
                bail!("C# source is outside the prepared managed graph: {filename}");
            }
            let lower = rune_lower(&filename);
            if (rune_lower(sysutil::ext(&filename)) == ".dll" || lower.ends_with(".dll.meta"))
                && self.configuration.at(&filename).is_empty()
                && unowned
            {
                bail!(
                    "DLL reference/import configuration is outside the prepared managed graph: {filename}"
                );
            }
            let extension = rune_lower(sysutil::ext(&filename));
            let configuration = matches!(extension.as_str(), ".asmdef" | ".asmref" | ".rsp")
                || sysutil::base(&filename) == "package.json"
                || filename.ends_with(".asmdef.meta")
                || filename.ends_with(".asmref.meta")
                || filename.ends_with(".rsp.meta");
            let untracked_script_meta = filename.ends_with(".cs.meta")
                && self.configuration.at(&filename).is_empty()
                && self.manifest.compiler_inputs.at(&filename).is_empty();
            if ((configuration && self.configuration.at(&filename).is_empty())
                || untracked_script_meta)
                && unowned
            {
                bail!(
                    "managed configuration/metadata is outside the prepared managed graph: {filename}"
                );
            }
        }
        sys::walk_dir(&join(&[&self.project, "Assets"]), &mut |entry| {
            let filename = entry.path;
            if entry.is_dir() && (self.host_owned(filename) || sysutil::base(filename) == "Editor")
            {
                return Ok(Walk::SkipDir);
            }
            if entry.is_dir()
                || !self.manifest.compiler_inputs.at(filename).is_empty()
                || tracked.get(filename).is_some_and(|name| !name.is_empty())
                || editor_source(filename)
            {
                return Ok(Walk::Continue);
            }
            match rune_lower(sysutil::ext(filename)).as_str() {
                ".cs" | ".asmdef" | ".asmref" | ".rsp" | ".dll" => {
                    bail!("new C# source/configuration requires a fresh managed graph: {filename}")
                }
                ".meta"
                    if [".asmdef.meta", ".asmref.meta", ".rsp.meta", ".dll.meta"]
                        .iter()
                        .any(|suffix| filename.ends_with(suffix)) =>
                {
                    bail!(
                        "new managed configuration metadata requires a fresh managed graph: {filename}"
                    )
                }
                _ => {}
            }
            Ok(Walk::Continue)
        })
    }

    /// Reports the first stale input in path order.
    pub fn check_fixed_configuration(&self) -> Result<()> {
        for (filename, expected) in &self.configuration {
            if expected == "absent" {
                if sys::absent(filename) {
                    continue;
                }
                bail!(
                    "previously absent managed input appeared; regenerate source graph: {filename}"
                );
            }
            if !file_hash(filename).is_ok_and(|actual| actual == *expected) {
                bail!(
                    "managed assembly configuration/GUID changed; regenerate source graph: {filename}"
                );
            }
        }
        for source in self.sources.keys() {
            let meta = format!("{source}.meta");
            if self.configuration.at(&meta).is_empty() && !sys::absent(&meta) {
                bail!("new MonoScript metadata requires a fresh managed graph: {source}");
            }
        }
        Ok(())
    }

    pub fn host_owned(&self, filename: &str) -> bool {
        self.host_roots
            .iter()
            .any(|root| within_directory(filename, root))
    }

    /// The compiler reference list of one assembly.
    pub fn references(
        &self,
        assembly: &CompileAssembly,
        built: &BTreeMap<String, String>,
    ) -> Result<NilVec<String>> {
        let config = self.configs.value(&assembly.name);
        let mut refs: Vec<String> = self.base_references.to_vec();
        if !config.config.no_engine_references {
            refs.extend(self.engine_references.iter().cloned());
        }
        let managed = sysutil::dir(self.current_dlls.at(&assembly.name));
        if self.manifest.backend == "authored_source" {
            // Instrumented SDK output requires the source-owned lifecycle marker
            // and public barrier ABI, independently of authored assembly imports.
            refs.push(join(&[&managed, "UnityMicroEditor.dll"]));
            refs.extend(config.compiler_references.iter().cloned());
            for dependency in &assembly.references {
                let mut filename = built.get(dependency).cloned().unwrap_or_default();
                if filename.is_empty() {
                    filename = self.current_dlls.at(dependency).to_string();
                }
                if filename.is_empty() {
                    filename = join(&[&managed, &format!("{dependency}.dll")]);
                }
                if dependency.starts_with("UnityEditor") {
                    bail!("Editor dependency cannot be a Player reference: {dependency}");
                }
                file_hash(&filename).map_err(|error| {
                    anyhow!("Source dependency {dependency} is unavailable: {error}")
                })?;
                refs.push(filename);
            }
            return Ok(deduplicate_references(&refs));
        }
        for filename in sys::glob_lossy(&join(&[&managed, "*.dll"])) {
            let base = sysutil::base(&filename);
            let name = base.strip_suffix(".dll").unwrap_or(&base);
            if name.starts_with("UnityEditor") {
                bail!("Editor assembly cannot be a runtime compiler reference: {filename}");
            }
            if !self.current_dlls.contains_key(name) {
                refs.push(filename);
            }
        }
        for dependency in &assembly.references {
            if let Some(filename) = built.get(dependency) {
                refs.push(filename.clone());
            } else if let Some(filename) = self.current_dlls.get(dependency) {
                refs.push(filename.clone());
            }
        }
        Ok(deduplicate_references(&refs))
    }

    /// `None` for `changed_paths` is the startup compilation of the
    /// whole prepared graph; `Ok(None)` means nothing had to be rebuilt.
    pub fn compile(
        &mut self,
        ctx: &Context,
        changed_paths: Option<&[String]>,
    ) -> Result<Option<CompileUpdate>> {
        ctx.err()?;
        self.last_phases.clear();
        if let Some(changed) = changed_paths
            && !self.compiler_changes(changed)
        {
            return Ok(None);
        }
        let started = Instant::now();
        let mut candidate = self.source_graph_candidate(changed_paths.unwrap_or(&[]))?;
        candidate.last_phases.clear();
        candidate.last_phases.lap("graph_candidate", started);
        match candidate.compile_graph(ctx, changed_paths) {
            Ok(update) => {
                // Graph extensions become visible together with their verified DLLs.
                // Rejected edits retain the last accepted graph and immutable Player.
                *self = candidate;
                Ok(update)
            }
            Err(error) => {
                self.last_phases = candidate.last_phases;
                Err(error)
            }
        }
    }

    pub fn compile_graph(
        &mut self,
        ctx: &Context,
        changed_paths: Option<&[String]>,
    ) -> Result<Option<CompileUpdate>> {
        let started = Instant::now();
        ctx.err()?;
        // Initial startup proves the complete prepared graph. A watcher already
        // reports topology/configuration edits; existing scene and native-asset
        // changes cannot alter C# DLLs and should avoid any compiler scan.
        if let Some(changed) = changed_paths
            && !self.compiler_changes(changed)
        {
            return Ok(None);
        }
        let mut lap = Instant::now();
        self.check_configuration(changed_paths.unwrap_or(&[]))?;
        lap = self.last_phases.lap("configuration_check", lap);
        let assemblies = self.manifest.assemblies.to_vec();
        let mut current_sources = NilMap::make();
        for assembly in &assemblies {
            for source in &assembly.sources {
                current_sources.insert(source, file_hash(source)?);
            }
        }
        lap = self.last_phases.lap("source_hashes", lap);
        let mut dirty = dirty_assemblies(&assemblies, &self.sources, &current_sources);
        // A preparation holds the output of the lifecycle rewrite that made
        // it. Another build of the helper writes other text for the same
        // source (line directives, the paths of caller information), so the
        // Player would load one rewrite and the first edit compile another:
        // the patch builder then sees differences nobody made. Such
        // assemblies are compiled again before the session starts, like the
        // caches of assemblies, code revisions and the helper itself, which
        // all miss when the helper changes.
        let mut forced_lifecycle = BTreeSet::new();
        if !self.startup_complete {
            let (helper_key, _) = self.lifecycle_helper_key()?;
            for assembly in &assemblies {
                if assembly.lifecycle_helper != helper_key {
                    forced_lifecycle.insert(assembly.name.clone());
                    dirty.insert(assembly.name.clone());
                }
            }
        }
        // The old raw baseline predates NGO IL postprocessing. Rebuild its
        // networking assemblies once even when the source bytes are unchanged.
        let mut forced_netcode = BTreeSet::new();
        for assembly in &assemblies {
            let network = needs_netcode(assembly);
            if network
                && !self.netcode_processed.on(&assembly.name)
                && !assembly.netcode_postprocessor.is_empty()
            {
                let processor = self.prepare_netcode_processor(ctx)?;
                if processor.key == assembly.netcode_postprocessor {
                    self.netcode_processed.insert(&assembly.name, true);
                }
            }
            if network && !self.netcode_processed.on(&assembly.name) {
                forced_netcode.insert(assembly.name.clone());
                dirty.insert(assembly.name.clone());
            }
            if assembly
                .references
                .iter()
                .any(|dependency| dirty.contains(dependency))
            {
                dirty.insert(assembly.name.clone());
            }
        }
        if dirty.is_empty() {
            self.startup_complete = true;
            return Ok(None);
        }
        let revision_key = self.code_revision_key(ctx, &current_sources)?;
        lap = self.last_phases.lap("revision_key", lap);
        let restored = match self.startup_complete {
            true => None,
            false => self.restore_code_revision(&revision_key, &current_sources),
        };
        lap = self.last_phases.lap("revision_restore", lap);
        if let Some(mut update) = restored {
            self.check_configuration(&[])?;
            for (source, expected) in &current_sources {
                if !file_hash(source).is_ok_and(|actual| actual == *expected) {
                    bail!("runtime source changed during revision reuse: {source}");
                }
            }
            let proofs = self.proofs.clone();
            self.use_code_revision(&update, proofs);
            update.elapsed_seconds = float_layout(started.elapsed().as_secs_f64());
            return Ok(Some(update));
        }
        sys::mkdir_all(&self.cache)?;
        let staging = sys::mkdir_temp(&self.cache, ".code-revision-")?;
        // Staging remains an owned diagnostic on rejection; it is never used
        // by Player or published as a successful compiler revision.
        let mut snapshots = BTreeMap::new();
        for assembly in &assemblies {
            if !dirty.contains(&assembly.name) {
                continue;
            }
            for source in &assembly.sources {
                let data = match sys::read(source) {
                    Ok(data) if sha256(&data) == current_sources.at(source) => data,
                    _ => bail!("runtime source changed while snapshotting: {source}"),
                };
                let destination = join(&[
                    &staging,
                    "Sources",
                    &sha256(source.as_bytes()),
                    &sysutil::base(source),
                ]);
                sys::mkdir_all(&sysutil::dir(&destination))?;
                sys::write(&destination, &data)?;
                snapshots.insert(source.clone(), destination);
            }
        }
        let helper = self.prepare_lifecycle_helper(ctx)?;
        self.last_phases.lap("snapshots", lap);
        let mut built: BTreeMap<String, String> = BTreeMap::new();
        let mut proofs: NilMap<LifecycleProof> = NilMap::make();
        let mut direct_dirty =
            source_dirty_assemblies(&assemblies, &self.sources, &current_sources);
        direct_dirty.extend(forced_netcode);
        direct_dirty.extend(forced_lifecycle);
        let mut declarations_changed: BTreeMap<String, bool> = BTreeMap::new();
        let mut rebuilt = BTreeSet::new();
        let mut compiled = NilVec::nil();
        for assembly in &assemblies {
            if !assembly_needs_compile(assembly, &direct_dirty, &declarations_changed) {
                built.insert(
                    assembly.name.clone(),
                    self.current_dlls.at(&assembly.name).to_string(),
                );
                proofs.insert(&assembly.name, self.proofs.value(&assembly.name));
                continue;
            }
            let started = Instant::now();
            let refs = self.references(assembly, &built)?;
            self.last_phases.lap("references", started);
            let (dll, proof) = self.compile_assembly(
                ctx,
                &staging,
                &helper,
                assembly,
                &refs,
                &snapshots,
                &current_sources,
            )?;
            built.insert(assembly.name.clone(), dll.clone());
            proofs.insert(&assembly.name, proof);
            let previous = self.current_dlls.at(&assembly.name).to_string();
            let started = Instant::now();
            self.validate_compiled_script_types(ctx, &helper, &assembly.name, &previous, &dll)?;
            let started = self.last_phases.lap("type_definitions", started);
            rebuilt.insert(assembly.name.clone());
            // A fingerprint is an optimization. Unknown metadata retains the
            // original conservative dependent rebuild instead of guessing.
            let changed = self
                .declarations_changed(ctx, &helper, &previous, &dll)
                .unwrap_or(true);
            self.last_phases.lap("declaration_hashes", started);
            declarations_changed.insert(assembly.name.clone(), changed);
            compiled.push(assembly.name.clone());
        }
        lap = Instant::now();
        self.check_configuration(&[])?;
        for (filename, expected) in &current_sources {
            if !file_hash(filename).is_ok_and(|hash| hash == *expected) {
                bail!(
                    "runtime source changed during compilation; no Player revision published: {filename}"
                );
            }
        }
        lap = self.last_phases.lap("final_check", lap);
        let player_folder = join(&[&staging, "Player"]);
        sys::mkdir(&player_folder)?;
        compiler_command(
            ctx,
            "",
            "cp",
            &[
                "-a",
                "--reflink=auto",
                &format!("{}/.", sysutil::dir(&self.player)),
                &player_folder,
            ],
        )
        .map_err(|error| anyhow!("stage immutable Player: {error}"))?;
        lap = self.last_phases.lap("player_copy", lap);
        let player = join(&[&player_folder, &sysutil::base(&self.player)]);
        let managed = super::player_managed(&player);
        let mut update = CompileUpdate {
            player,
            compiled_assemblies: compiled,
            source_hashes: current_sources,
            runtime_initializers: NilVec::make(),
            dll_hashes: NilMap::make(),
            ..Default::default()
        };
        for assembly in &assemblies {
            let destination = join(&[&managed, &format!("{}.dll", assembly.name)]);
            if rebuilt.contains(&assembly.name) {
                let source = &built[&assembly.name];
                sys::write(&destination, &sys::read(source)?)?;
                let stem = source.strip_suffix(".dll").unwrap_or(source);
                if let Ok(data) = sys::read(&format!("{stem}.pdb")) {
                    let target = destination.strip_suffix(".dll").unwrap_or(&destination);
                    sys::write(&format!("{target}.pdb"), &data)?;
                }
            }
            update
                .dll_hashes
                .insert(&assembly.name, file_hash(&destination)?);
            update.assemblies.push(destination);
            if declarations_changed.get(&assembly.name) == Some(&true) {
                update.declaration_changed.push(assembly.name.clone());
            }
            update.runtime_initializers.append(
                proofs
                    .value(&assembly.name)
                    .runtime_initializers
                    .iter()
                    .cloned(),
            );
        }
        update.elapsed_seconds = float_layout(started.elapsed().as_secs_f64());
        atomic_json(&join(&[&staging, "compile-update.json"]), &update)?;
        lap = self.last_phases.lap("player_dlls", lap);
        self.save_code_revision(&revision_key, &update, &proofs)?;
        self.last_phases.lap("save_revision", lap);
        self.use_code_revision(&update, proofs);
        Ok(Some(update))
    }

    /// Whether any changed path can affect managed code.
    pub fn compiler_changes(&self, changed: &[String]) -> bool {
        for filename in changed {
            let filename = self.absolute(filename);
            if self.configuration.contains_key(&filename)
                || within_directory(&filename, &join(&[&self.project, "ProjectSettings"]))
                || filename == join(&[&self.project, "Packages/manifest.json"])
                || filename == join(&[&self.project, "Packages/packages-lock.json"])
                || sysutil::base(&filename) == "package.json"
            {
                return true;
            }
            if fs::metadata(&filename).is_ok_and(|info| info.is_dir()) {
                return true;
            }
            match sysutil::ext(&filename) {
                // An extensionless path may be a removed or renamed directory with C#.
                ".cs" | ".asmdef" | ".asmref" | ".rsp" | ".dll" | "" => return true,
                ".meta"
                    if [
                        ".cs.meta",
                        ".asmdef.meta",
                        ".asmref.meta",
                        ".dll.meta",
                        ".rsp.meta",
                    ]
                    .iter()
                    .any(|extension| filename.ends_with(extension)) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Compares the declaration fingerprints of two DLLs.
    pub fn declarations_changed(
        &self,
        ctx: &Context,
        helper: &str,
        previous_dll: &str,
        next_dll: &str,
    ) -> Result<bool> {
        let output = compiler_command(
            ctx,
            &self.cache,
            &self.dotnet,
            &[helper, "--declaration-hashes", previous_dll, next_dll],
        )?;
        let hashes = unmarshal::<NilMap<String>>(&output).unwrap_or_default();
        if hashes.len() != 2
            || hashes.at(previous_dll).len() != 64
            || hashes.at(next_dll).len() != 64
        {
            bail!("managed declaration fingerprint has no exact input proof");
        }
        Ok(hashes.at(previous_dll) != hashes.at(next_dll))
    }

    pub fn collect_expected_script_types(&mut self, baseline: &str) -> Result<()> {
        self.expected_types = NilMap::make();
        let mut doc = Bindings::default();
        doc.decode(baseline)?;
        if !doc.manifest.is_empty() {
            if !sysutil::is_abs(&doc.manifest) {
                doc.manifest = join(&[&sysutil::dir(baseline), &doc.manifest]);
            }
            let manifest = doc.manifest.clone();
            doc.decode(&manifest)?;
        }
        if !doc.assets.is_empty() && doc.project_root != self.project {
            bail!("script binding baseline belongs to another project");
        }
        let mut by_guid: BTreeMap<String, String> = BTreeMap::new();
        let scripts = self.manifest.scripts.to_vec();
        for script in scripts {
            if !script.guid.is_empty() {
                by_guid.insert(script.guid.clone(), script.assembly.clone());
            }
            if !script.type_name.is_empty() && !script.host_provided {
                self.expect_script_type(&script.assembly, &script.type_name, &script.guid);
            }
        }
        for asset in &doc.assets {
            if asset.kind != "script" || asset.type_name.is_empty() {
                continue;
            }
            let assembly = by_guid.get(&asset.guid).cloned().unwrap_or_default();
            if !self.current_dlls.at(&assembly).is_empty() {
                self.expect_script_type(&assembly, &asset.type_name, &asset.guid);
            }
        }
        Ok(())
    }

    /// The compiler's GUID map chooses the defining
    /// assembly. Metadata TypeDef names omit any optional assembly-qualified
    /// suffix from the manifest.
    pub fn expect_script_type(&mut self, assembly: &str, name: &str, guid: &str) {
        let name = match name.find(',') {
            Some(index) => name[..index].trim(),
            None => name,
        };
        if !self.expected_types.contains_key(assembly) {
            self.expected_types.insert(assembly, NilMap::make());
        }
        if let Some(types) = self.expected_types.entry_mut(assembly) {
            types.insert(name, guid.to_string());
        }
    }

    pub fn validate_compiled_script_types(
        &self,
        ctx: &Context,
        helper: &str,
        assembly: &str,
        previous_dll: &str,
        next_dll: &str,
    ) -> Result<()> {
        let has_scripts = self
            .expected_types
            .get(assembly)
            .is_some_and(|types| !types.is_empty())
            || self.manifest.scripts.iter().any(|script| {
                script.assembly == assembly && !script.host_provided && !script.guid.is_empty()
            });
        if !has_scripts {
            return Ok(());
        }
        let listed = |definitions: &NilMap<NilVec<String>>, dll: &str| {
            definitions
                .get(dll)
                .filter(|names| !names.is_nil())
                .cloned()
        };
        if previous_dll.is_empty() {
            let output = compiler_command(
                ctx,
                &self.cache,
                &self.dotnet,
                &[helper, "--type-definitions", next_dll],
            )?;
            let definitions = unmarshal::<NilMap<NilVec<String>>>(&output).unwrap_or_default();
            return match listed(&definitions, next_dll) {
                Some(next) if definitions.len() == 1 => {
                    self.validate_required_script_types(assembly, &[], &next)
                }
                _ => bail!("SDK source output lacks exact type definition proof: {assembly}"),
            };
        }
        let output = compiler_command(
            ctx,
            &self.cache,
            &self.dotnet,
            &[helper, "--type-definitions", previous_dll, next_dll],
        )
        .map_err(|error| anyhow!("validate baseline script types in {assembly}: {error}"))?;
        let definitions = unmarshal::<NilMap<NilVec<String>>>(&output).unwrap_or_default();
        match (
            listed(&definitions, previous_dll),
            listed(&definitions, next_dll),
        ) {
            (Some(previous), Some(next)) if definitions.len() == 2 => {
                self.validate_required_script_types(assembly, &previous, &next)
            }
            _ => bail!("compiled DLL has no exact baseline/new TypeDef proof: {assembly}"),
        }
    }

    /// With several missing types, names the first by name.
    pub fn validate_required_script_types(
        &self,
        assembly: &str,
        previous: &[String],
        next: &[String],
    ) -> Result<()> {
        let mut required: BTreeMap<String, String> = self
            .expected_types
            .get(assembly)
            .map(|types| (**types).clone())
            .unwrap_or_default();
        // Compiler scripts cover the entire baseline project, including scripts
        // absent from the current scene. Unity script files conventionally declare
        // their primary class with the filename; use only names proven in old DLL.
        let proof = self.proofs.get(assembly);
        for script in &self.manifest.scripts {
            if script.assembly != assembly || script.host_provided || script.guid.is_empty() {
                continue;
            }
            if let Some(name) = proof.and_then(|proof| proof.script_types.get(&script.path))
                && !name.is_empty()
            {
                required.insert(name.clone(), script.guid.clone());
            }
            let file = sysutil::base(&script.path);
            let base = file
                .strip_suffix(sysutil::ext(&script.path))
                .unwrap_or(&file);
            for name in previous {
                let mut simple = &name[name.rfind(['.', '+']).map_or(0, |index| index + 1)..];
                if let Some(index) = simple.find('`') {
                    simple = &simple[..index];
                }
                if simple == base {
                    required.insert(name.clone(), script.guid.clone());
                }
            }
        }
        for (name, guid) in &required {
            if !next.contains(name) {
                bail!(
                    "compiled assembly {assembly} removed or renamed baseline script type {name} (GUID {guid}); regenerate script bindings before replacing Player"
                );
            }
        }
        Ok(())
    }

    /// The cache key of the lifecycle helper: the hashes of its two sources
    /// and of the SDK's compiler and launcher, with the hashes themselves.
    pub fn lifecycle_helper_key(&self) -> Result<(String, BTreeMap<String, String>)> {
        let tool_root = micro_editor_root()?;
        let source_root = join(&[&tool_root, "tools/LifecycleCompiler"]);
        let mut inputs = BTreeMap::new();
        for filename in [
            join(&[&source_root, "LifecycleCompiler.csproj"]),
            join(&[&source_root, "Program.cs"]),
            self.csc.clone(),
            self.dotnet.clone(),
        ] {
            let hash = file_hash(&filename)?;
            inputs.insert(filename, hash);
        }
        Ok((sha256(&marshal(&inputs)?), inputs))
    }

    /// Builds (or reuses) our Roslyn helper with the
    /// installed SDK. The cache entry is keyed by its sources and the SDK.
    pub fn prepare_lifecycle_helper(&mut self, ctx: &Context) -> Result<String> {
        if !self.helper.is_empty() {
            return Ok(self.helper.clone());
        }
        let tool_root = micro_editor_root()?;
        let source_root = join(&[&tool_root, "tools/LifecycleCompiler"]);
        let (key, inputs) = self.lifecycle_helper_key()?;
        let entry = join(&[&self.cache, "lifecycle", &key]);
        let helper = join(&[&entry, "LifecycleCompiler.dll"]);
        let verified = || {
            read_json_file::<NilMap<String>>(&join(&[&entry, "files.json"]))
                .is_ok_and(|metadata| verify_cache_files(&entry, &metadata))
        };
        if verified() {
            self.helper = helper.clone();
            self.helper_key = key;
            return Ok(helper);
        }
        sys::mkdir_all(&sysutil::dir(&entry))?;
        let staging = sys::mkdir_temp(&sysutil::dir(&entry), ".helper-build-")?;
        for name in ["LifecycleCompiler.csproj", "Program.cs"] {
            let source = join(&[&source_root, name]);
            let data = match sys::read(&source) {
                Ok(data) if sha256(&data) == inputs[&source] => data,
                _ => bail!("lifecycle helper source changed during build"),
            };
            sys::write(&join(&[&staging, name]), &data)?;
        }
        sys::write(
            &join(&[&staging, "NuGet.Config"]),
            b"<configuration><packageSources><clear /></packageSources></configuration>\n",
        )?;
        let payload = join(&[&staging, "payload"]);
        compiler_command(
            ctx,
            &staging,
            &self.dotnet,
            &[
                "build",
                &join(&[&staging, "LifecycleCompiler.csproj"]),
                "--nologo",
                "-v:q",
                "-o",
                &payload,
                &format!("-p:RoslynPath={}", sysutil::dir(&self.csc)),
                "-p:NuGetAudit=false",
                sys::BUILD_WITHOUT_SERVERS[0],
                sys::BUILD_WITHOUT_SERVERS[1],
            ],
        )
        .map_err(|error| anyhow!("build SDK lifecycle helper: {error}"))?;
        let mut metadata = NilMap::make();
        for name in sys::regular_files(&payload)? {
            let hash = file_hash(&join(&[&payload, &name]))?;
            metadata.insert(name, hash);
        }
        if metadata.at("LifecycleCompiler.dll").is_empty() {
            bail!("SDK helper build produced no verified executable");
        }
        atomic_json(&join(&[&payload, "files.json"]), &metadata)?;
        sys::publish(&payload, &entry, verified)?;
        self.helper = helper.clone();
        self.helper_key = key;
        Ok(helper)
    }

    /// Instruments, compiles and caches one assembly. The
    /// cache key covers the configuration, sources, references and tools.
    #[allow(clippy::too_many_arguments)]
    pub fn compile_assembly(
        &mut self,
        ctx: &Context,
        staging: &str,
        helper: &str,
        assembly: &CompileAssembly,
        refs: &NilVec<String>,
        snapshots: &BTreeMap<String, String>,
        hashes: &NilMap<String>,
    ) -> Result<(String, LifecycleProof)> {
        let config = self.configs.value(&assembly.name);
        let mut lap = Instant::now();
        let mut identity: BTreeMap<String, String> = BTreeMap::new();
        identity.insert(
            "configuration".into(),
            self.configuration_hashes.at(&assembly.name).to_string(),
        );
        for source in &assembly.sources {
            identity.insert(source.clone(), hashes.at(source).to_string());
            let meta = format!("{source}.meta");
            let expected = self.configuration.at(&meta);
            if !expected.is_empty() {
                identity.insert(meta.clone(), expected.to_string());
            }
        }
        for reference in refs {
            identity.insert(
                format!("reference:{}", sysutil::base(reference)),
                file_hash(reference)?,
            );
        }
        for executable in [helper.to_string(), self.csc.clone()]
            .into_iter()
            .chain(self.generators.iter().cloned())
        {
            let hash = file_hash(&executable)?;
            identity.insert(executable, hash);
        }
        let network = needs_netcode(assembly);
        let mut netcode_key = String::new();
        if network {
            netcode_key = self.prepare_netcode_processor(ctx)?.key;
            identity.insert("netcodePostprocessor".into(), netcode_key.clone());
        }
        let key = sha256(&marshal(&identity)?);
        lap = self.last_phases.lap("assembly_identity", lap);
        let entry = join(&[&self.cache, "assemblies", &key]);
        let library = format!("{}.dll", assembly.name);
        // The entry as it stands, when every part of it holds.
        let cached = || {
            read_json_file::<Option<CompiledCache>>(&join(&[&entry, "compile-cache.json"]))
                .ok()
                .flatten()
                .filter(|cached| {
                    cached.schema_version == 1
                        && cached.key == key
                        && cached.name == assembly.name
                        && cached.netcode_key == netcode_key
                        && valid_lifecycle_proof(&cached.proof, &assembly.name)
                        && lifecycle_sources_match(&cached.proof, &assembly.sources)
                        && verify_cache_files(&entry, &cached.files)
                        && !cached.files.at(&library).is_empty()
                })
        };
        if let Some(cached) = cached() {
            self.last_phases.lap("assembly_cache", lap);
            return Ok((join(&[&entry, &library]), cached.proof));
        }
        let folder = join(&[staging, "Compile", &assembly.name]);
        sys::mkdir_all(&folder)?;
        let mut sources = Vec::new();
        let mut execution_orders: BTreeMap<String, i32> = BTreeMap::new();
        for (index, source) in assembly.sources.iter().enumerate() {
            sources.push(LifecycleSource {
                path: snapshots.get(source).cloned().unwrap_or_default(),
                original_path: source.clone(),
                output_path: join(&[
                    &folder,
                    "Lifecycle",
                    &index.to_string(),
                    &sysutil::base(source),
                ]),
            });
            if let Ok(data) = sys::read(&format!("{source}.meta")) {
                let properties = registry::meta_properties(&data)?;
                if let Some(Ok(value)) = properties
                    .get("MonoImporter.executionOrder")
                    .map(|value| <[u8; 4]>::try_from(value.as_slice()))
                {
                    execution_orders.insert(source.clone(), i32::from_le_bytes(value));
                }
            }
        }
        let marker = join(&[&folder, "UnityPlayLifecycleMarker.cs"]);
        let request = join(&[&folder, "lifecycle-request.json"]);
        atomic_json(
            &request,
            &LifecycleRequest {
                allow_unsafe: config.config.allow_unsafe,
                assembly_name: &assembly.name,
                defines: &config.defines,
                execution_orders: &execution_orders,
                marker_path: &marker,
                native_base_assemblies: &config.native_base,
                references: refs,
                sources: &sources,
            },
        )?;
        lap = self.last_phases.lap("assembly_cache", lap);
        let output = compiler_command(ctx, staging, &self.dotnet, &[helper, &request])
            .map_err(|error| anyhow!("verify lifecycle {}: {error}", assembly.name))?;
        lap = self.last_phases.lap("lifecycle_rewrite", lap);
        let wire: WireProof = unmarshal(&output)?;
        let mut proof = LifecycleProof {
            schema_version: wire.schema_version,
            verified: wire.verified,
            phases: wire.phases,
            scriptable_object_barrier: wire.scriptable_object_barrier,
            runtime_initializers: NilVec::make(),
            callbacks: wire.callbacks.len() as i64,
            methods: wire.callbacks,
            script_types: wire.script_types,
        };
        for initializer in wire.runtime_initializers.into_vec() {
            proof.runtime_initializers.push(RuntimeInitializer {
                assembly: initializer.assembly,
                type_name: initializer.type_name,
                method_name: initializer.method_name,
                phase: initializer.phase,
            });
        }
        if !valid_lifecycle_proof(&proof, &assembly.name)
            || !lifecycle_sources_match(&proof, &assembly.sources)
        {
            bail!("SDK lifecycle proof is invalid for {}", assembly.name);
        }
        let mut dll = join(&[&folder, &library]);
        let mut arguments: Vec<String> = [
            "-nologo",
            "-target:library",
            "-nostdlib+",
            "-langversion:9.0",
            "-deterministic+",
            "-debug:portable",
            "-optimize-",
            if config.config.allow_unsafe {
                "-unsafe+"
            } else {
                "-unsafe-"
            },
        ]
        .map(String::from)
        .to_vec();
        arguments.push(format!("-out:{dll}"));
        arguments.push(format!("-define:{}", config.defines.join(";")));
        arguments.extend(config.compiler_options.iter().cloned());
        arguments.extend(
            refs.iter()
                .map(|reference| format!("-reference:{reference}")),
        );
        arguments.extend(
            self.generators
                .iter()
                .map(|generator| format!("-analyzer:{generator}")),
        );
        let generated = join(&[&folder, "Generated"]);
        sys::mkdir(&generated)?;
        let additional = join(&[
            &folder,
            &format!("{}.UnityAdditionalFile.txt", assembly.name),
        ]);
        sys::write(&additional, self.project.as_bytes())?;
        arguments.push(format!("-generatedfilesout:{generated}"));
        arguments.push(format!("-additionalfile:{additional}"));
        arguments.extend(sources.iter().map(|source| source.output_path.clone()));
        arguments.push(marker);
        let response = join(&[&folder, &format!("{}.rsp", assembly.name)]);
        let mut text = String::new();
        for argument in &arguments {
            text.push('"');
            text.push_str(&argument.replace('"', "\\\""));
            text.push_str("\"\n");
        }
        sys::write(&response, text.as_bytes())?;
        let (output, error) = match compiler_command(
            ctx,
            &self.project,
            &self.dotnet,
            &[&self.csc, &format!("@{response}")],
        ) {
            Ok(output) => (output, None),
            Err(error) => (error.output, Some(error.message)),
        };
        let listing = String::from_utf8_lossy(&output);
        if error.is_some()
            || listing.contains("CS8032")
            || listing.contains("CS8784")
            || listing.contains("CS8785")
        {
            bail!(
                "compile {} failed: {}\n{listing}",
                assembly.name,
                error.as_deref().unwrap_or("<nil>")
            );
        }
        fs::metadata(&dll).map_err(|e| sys::os_error("stat", &dll, &e))?;
        lap = self.last_phases.lap("csc", lap);
        if network {
            (dll, proof) = self.postprocess_netcode(ctx, &folder, &dll, assembly, refs, proof)?;
        }
        lap = self.last_phases.lap("netcode", lap);
        sys::mkdir_all(&sysutil::dir(&entry))?;
        let payload = sys::mkdir_temp(&sysutil::dir(&entry), ".assembly-")?;
        let mut files = NilMap::make();
        let stem = dll.strip_suffix(".dll").unwrap_or(&dll);
        for filename in [dll.clone(), format!("{stem}.pdb")] {
            let data = sys::read(&filename)?;
            let name = sysutil::base(&filename);
            sys::write(&join(&[&payload, &name]), &data)?;
            files.insert(name, sha256(&data));
        }
        atomic_json(
            &join(&[&payload, "compile-cache.json"]),
            &CompiledCache {
                schema_version: 1,
                key: key.clone(),
                name: assembly.name.clone(),
                files,
                proof: proof.clone(),
                netcode_key: netcode_key.clone(),
            },
        )?;
        // Another process may have published the same state meanwhile: its
        // entry is the one to use then, with the proof it recorded.
        sys::publish(&payload, &entry, || cached().is_some())?;
        let proof = cached().map_or(proof, |cached| cached.proof);
        self.last_phases.lap("assembly_cache", lap);
        Ok((join(&[&entry, &library]), proof))
    }
}
