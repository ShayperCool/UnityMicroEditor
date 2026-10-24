//! Turning an exported compilation graph into a
//! verified compiler baseline. The graph is exported through Unity's public
//! CompilationPipeline APIs (or built by our source graph). Preparation
//! verifies the explicit project inputs and instruments sources with the
//! installed SDK; it does not execute an Editor or game code.
/// Begins the error of a package assembly the host does not provide and
/// the compiler could not build as game code.
pub const HOST_LACKS_PACKAGE: &str = "the host does not provide package assembly ";

use super::{
    AssemblyConfig, CompileAssembly, CompileManifest, CompileScript, Compiler, CompilerGraphExport,
    CompilerHostAssembly, CompilerPreparationResult, NilMap, NilVec,
    compile::{canonical_json, deduplicate_references, equal_strings},
    join,
    netcode::needs_netcode,
    nilable::unmarshal,
    source_graph::{RawMap, raw_value, remove_source_native_initializers, write_raw_map},
    sys::{self, Walk, compiler_command, managed_plugin},
};
use crate::{
    cancel::Context,
    editorbridge::validate_project_version,
    project::{is_owned_cache_path, lexpath::rune_lower},
    registry, sha256, sysutil,
    util::{
        agent_canonical, atomic_json, canonical_destination, file_hash, marshal, read_json_file,
        valid_sha256, within_directory,
    },
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
};

/// Seam for publishing the prepared baseline.
pub type PublishBaseline<'a> = &'a dyn Fn(&str, &CompilerPreparationResult) -> Result<()>;

/// One JSON line, linked into place without replacing a concurrent preparation
/// or an existing baseline. `agent::publish_agent_baseline` is this function.
pub fn publish_baseline<T: Serialize + ?Sized>(path: &str, value: &T) -> Result<()> {
    let mut data = marshal(value)?;
    data.push(b'\n');
    let directory = sysutil::dir(path);
    let mut file = tempfile::Builder::new()
        .prefix(".baseline-")
        .tempfile_in(&directory)
        .map_err(|e| sys::os_error("open", &join(&[&directory, ".baseline-"]), &e))?;
    file.write_all(&data)?;
    file.as_file().sync_all()?;
    fs::hard_link(file.path(), path).map_err(|e| {
        sys::os_error(
            "link",
            &format!("{} {path}", file.path().to_string_lossy()),
            &e,
        )
    })
}

/// The configuration with sorted object keys.
pub fn canonical_configuration(config: &AssemblyConfig) -> Result<Vec<u8>> {
    let canonical: serde_json::Value = serde_json::from_slice(&marshal(config)?)?;
    let mut encoded = Vec::new();
    canonical_json(&canonical, &mut encoded);
    Ok(encoded)
}

/// The per-assembly `compile.json` descriptor. Keys are sorted.
#[derive(Serialize)]
pub(crate) struct Descriptor<'a> {
    assemblies: [&'a CompileAssembly; 1],
    #[serde(rename = "assemblyConfiguration")]
    assembly_configuration: Box<RawValue>,
    backend: &'a str,
    #[serde(rename = "compilerInputs")]
    compiler_inputs: BTreeMap<&'static str, String>,
    #[serde(rename = "engineProfileSHA256")]
    engine_profile_sha256: &'a str,
    #[serde(rename = "packageResolutionSHA256")]
    package_resolution_sha256: &'a str,
}
pub(crate) fn descriptor<'a>(
    manifest: &'a CompileManifest,
    configuration: &[u8],
    assembly: &'a CompileAssembly,
) -> Result<Descriptor<'a>> {
    Ok(Descriptor {
        assemblies: [assembly],
        assembly_configuration: raw_value(configuration.to_vec())?,
        backend: &manifest.backend,
        compiler_inputs: BTreeMap::from([("assemblyConfiguration", sha256(configuration))]),
        engine_profile_sha256: &manifest.engine_profile_sha256,
        package_resolution_sha256: &manifest.package_resolution_sha256,
    })
}

struct Staging {
    path: String,
    published: bool,
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Prepares the compiler graph with the default baseline publication.
pub fn prepare_compiler_graph(
    ctx: &Context,
    project: &str,
    editor: &str,
    player: &str,
    graph_file: &str,
    output: &str,
) -> Result<CompilerPreparationResult> {
    prepare_compiler_graph_with(
        ctx,
        project,
        editor,
        player,
        graph_file,
        output,
        &publish_baseline,
    )
}

pub fn prepare_compiler_graph_with(
    ctx: &Context,
    project: &str,
    editor: &str,
    player: &str,
    graph_file: &str,
    output: &str,
    publish: PublishBaseline,
) -> Result<CompilerPreparationResult> {
    ctx.err()?;
    let project = crate::project::lexpath::abs(project)?;
    let project = sysutil::eval_symlinks(&project)?;
    let output = crate::project::lexpath::abs(output)?;
    if canonical_destination(&output).ok().as_deref() != Some(output.as_str())
        || !is_owned_cache_path(&project, &output)
    {
        bail!(
            "compiler preparation output must be under project Temp or Library/UnityMicroEditor without symlinks"
        );
    }
    if !sys::absent(&output) {
        bail!("compiler preparation output already exists; choose a new output path");
    }
    if !fs::read(join(&[&project, "ProjectSettings/ProjectVersion.txt"]))
        .is_ok_and(|version| validate_project_version(&version).is_ok())
    {
        bail!(
            "compiler preparation requires a project of a supported Unity release ({})",
            crate::release::supported_versions()
        );
    }
    let mut graph: CompilerGraphExport =
        read_json_file::<Option<_>>(graph_file)?.unwrap_or_default();
    let total = graph.assemblies.len() + graph.host_assemblies.len();
    if graph.schema_version != 1
        || graph.project != project
        || graph.unity != editor
        || total == 0
        || total > 256
        || graph.scripts.len() > 65536
        || graph.source_roots.len() > 4096
    {
        bail!("compiler export lacks an exact project/Unity runtime graph");
    }
    let own_graph = graph.backend == "authored_source";
    if own_graph
        && (!valid_sha256(&graph.engine_profile_sha256)
            || !valid_sha256(&graph.package_resolution_sha256))
    {
        bail!("Authored source graph lacks engine/package profile fingerprints");
    }
    sys::mkdir_all(&sysutil::dir(&output))?;
    let mut staging = Staging {
        path: sys::mkdir_temp(&sysutil::dir(&output), ".compiler-preparation-")?,
        published: false,
    };
    let staging_path = staging.path.clone();
    let player_folder = join(&[&staging_path, "Player"]);
    compiler_command(
        ctx,
        "",
        "cp",
        &[
            "-a",
            "--reflink=auto",
            &sysutil::dir(player),
            &player_folder,
        ],
    )?;
    let staged_player = join(&[&player_folder, &sysutil::base(player)]);
    let managed = super::player_managed(&staged_player);
    let slots_file = join(&[&sysutil::dir(&managed), "ScriptingAssemblies.json"]);
    let mut compiler = Compiler {
        project: project.clone(),
        editor: editor.into(),
        player: staged_player.clone(),
        cache: join(&[&staging_path, "compiler-cache"]),
        store: super::project_compiler_store(&project),
        configs: NilMap::make(),
        configuration_hashes: NilMap::make(),
        configuration: NilMap::make(),
        current_dlls: NilMap::make(),
        expected_types: NilMap::make(),
        manifest: CompileManifest {
            schema: 1,
            project: project.clone(),
            unity: editor.into(),
            native_only: graph.assemblies.is_empty(),
            scripts: graph.scripts.clone(),
            compiler_inputs: graph.compiler_inputs.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    if own_graph {
        compiler.manifest.backend = graph.backend.clone();
        compiler.manifest.engine_profile_sha256 = graph.engine_profile_sha256.clone();
        compiler.manifest.package_resolution_sha256 = graph.package_resolution_sha256.clone();
        compiler.manifest.base_references = graph.base_references.clone();
        compiler.manifest.engine_references = graph.engine_references.clone();
    }
    if compiler.manifest.compiler_inputs.is_nil() {
        compiler.manifest.compiler_inputs = NilMap::make();
    }
    compiler.initialize_sdk()?;
    compiler.play_mode = own_graph && graph.play_mode;
    compiler.manifest.play_mode = compiler.play_mode;
    if compiler.play_mode {
        // Play mode code replaces the host's builds of the game's and the
        // packages' assemblies and plugins; the Player's data keeps their
        // serialized layout.
        let names: BTreeSet<String> = graph
            .assemblies
            .iter()
            .map(|assembly| assembly.config.name.clone())
            .chain(graph.assemblies.iter().flat_map(|assembly| {
                assembly
                    .compiled_references
                    .iter()
                    .map(|path| sysutil::base(path))
                    .collect::<Vec<_>>()
            }))
            .collect();
        super::playmode::keep_host_layouts(&staged_player, names.iter().map(String::as_str))?;
    }
    if own_graph {
        if graph.base_references.is_empty() || graph.engine_references.is_empty() {
            bail!("Source compiler requires proven exact engine/BCL reference sets");
        }
        compiler.base_references = graph.base_references.clone();
        compiler.engine_references = graph.engine_references.clone();
        for path in graph.base_references.iter().chain(&graph.engine_references) {
            if !file_hash(path).is_ok_and(|hash| hash == graph.compiler_inputs.at(path)) {
                bail!("Source engine/BCL reference lacks exact input proof: {path}");
            }
        }
    }
    sys::mkdir_all(&compiler.cache)?;
    if own_graph {
        let mut plugins: BTreeMap<String, String> = BTreeMap::new();
        for assembly in &graph.assemblies {
            for path in &assembly.compiled_references {
                if !managed_plugin(path) {
                    bail!("Source precompiled reference is not managed PE metadata: {path}");
                }
                let name = sysutil::base(path);
                let hash = match file_hash(path) {
                    Ok(hash) if graph.compiler_inputs.at(path) == hash => hash,
                    _ => bail!("Source plugin lacks exact current input proof: {path}"),
                };
                if plugins
                    .get(&name)
                    .is_some_and(|previous| !previous.is_empty() && *previous != hash)
                {
                    bail!("Source plugins disagree on assembly filename: {name}");
                }
                let destination = join(&[&managed, &name]);
                // A plugin named as a facade of the Mono class library: the
                // Editor's build of this release keeps the facade in the
                // Player (6000.0 with a package's own System.Buffers.dll), and
                // the host holds that build.
                let facade = join(&[
                    &sysutil::dir(editor),
                    "Data/MonoBleedingEdge/lib/mono/unityaot-linux/Facades",
                    &name,
                ]);
                if let (Ok(existing), Ok(facade)) = (file_hash(&destination), file_hash(&facade))
                    && existing != hash
                    && existing == facade
                {
                    continue;
                }
                if compiler.play_mode {
                    // Play mode code takes the plugins the Editor takes,
                    // linked as the game's assemblies are.
                    // A precompiled library with a build of its own for the
                    // Player (Odin Inspector's `NoEditor` assemblies), which
                    // the host holds, keeps it: the Editor's build serves the
                    // Editor's tools (Odin's serializer tracks prefab changes
                    // through it).
                    if let Ok(held) = file_hash(&destination)
                        && held != hash
                        && plugins.insert(name.clone(), hash.clone()).is_none()
                    {
                        compiler.manifest.compiler_inputs.insert(&destination, held);
                        continue;
                    }
                    if plugins.insert(name.clone(), hash.clone()).is_none() {
                        let _ = fs::remove_file(&destination);
                        compiler.link_play_mode_assembly(
                            ctx,
                            &join(&[&staging_path, "PlayModePlugins", &name]),
                            name.strip_suffix(".dll").unwrap_or(&name),
                            path,
                            &destination,
                        )?;
                        // The host's own build of it, where the graph
                        // recorded one, is replaced.
                        let replaced: Vec<String> = compiler
                            .manifest
                            .compiler_inputs
                            .keys()
                            .filter(|recorded| {
                                sysutil::base(recorded) == name
                                    && sysutil::base(&sysutil::dir(recorded)) == "Managed"
                            })
                            .cloned()
                            .collect();
                        for recorded in replaced {
                            compiler.manifest.compiler_inputs.remove(&recorded);
                        }
                        compiler
                            .manifest
                            .compiler_inputs
                            .insert(&destination, file_hash(&destination)?);
                    }
                    continue;
                }
                plugins.insert(name.clone(), hash.clone());
                match file_hash(&destination) {
                    Ok(existing) if existing != hash => bail!(
                        "precompiled plugin differs from native host; prepare a matching native package profile: {name}"
                    ),
                    Err(error) if !sys::is_not_exist(&error) => return Err(error),
                    _ => {}
                }
                if sys::absent(&destination) {
                    agent_canonical(path)?;
                    compiler_command(ctx, "", "cp", &["--reflink=auto", "--", path, &destination])?;
                    agent_canonical(&destination)?;
                    if !file_hash(&destination).is_ok_and(|actual| actual == hash) {
                        bail!("plugin changed while staging: {path}");
                    }
                }
                compiler.manifest.compiler_inputs.insert(destination, hash);
            }
        }
        // The class library assemblies the plugins use that the host lacks
        // (`System.Web` of `nunit.framework`), from the Player's own profile,
        // as a build copies them.
        let library = join(&[
            &sysutil::dir(editor),
            "Data/MonoBleedingEdge/lib/mono/unityjit-linux",
        ]);
        let mut pending: Vec<String> = plugins.keys().map(|name| join(&[&managed, name])).collect();
        while let Some(file) = pending.pop() {
            let Ok(image) = sys::read(&file) else {
                continue;
            };
            for name in super::assembly_references(&image).unwrap_or_default() {
                let destination = join(&[&managed, &format!("{name}.dll")]);
                let source = join(&[&library, &format!("{name}.dll")]);
                if !sys::absent(&destination) || sys::absent(&source) {
                    continue;
                }
                compiler_command(
                    ctx,
                    "",
                    "cp",
                    &["--reflink=auto", "--", &source, &destination],
                )?;
                compiler
                    .manifest
                    .compiler_inputs
                    .insert(destination.clone(), file_hash(&destination)?);
                pending.push(destination);
            }
        }
        let slots: Vec<CompileAssembly> = plugins
            .keys()
            .map(|name| CompileAssembly {
                name: name.strip_suffix(".dll").unwrap_or(name).to_string(),
                ..Default::default()
            })
            .collect();
        let mut slots = slots;
        slots.sort_by(|a, b| a.name.cmp(&b.name));
        register_compiler_assembly_slots(&slots_file, &slots)?;
    }
    for exported in &graph.assemblies {
        let mut config = exported.config.clone();
        if !exported.references.is_empty() {
            if !config.dependencies.is_empty()
                && !equal_strings(&config.dependencies, &exported.references)
            {
                bail!(
                    "compiler export has conflicting dependency lists: {}",
                    config.name
                );
            }
            config.dependencies = exported.references.clone();
        }
        if config.name.is_empty()
            || sysutil::base(&config.name) != config.name
            || config.name.starts_with("UnityEditor") && !compiler.play_mode
            || !compiler.configs.value(&config.name).name.is_empty()
            || config.sources.is_empty()
            || config.defines.is_empty()
            || !own_graph
                && (config.config.override_references || !config.config.precompiled.is_empty())
        {
            bail!(
                "unsupported or duplicate runtime assembly configuration: {}",
                config.name
            );
        }
        if !compiler.play_mode
            && config
                .defines
                .iter()
                .any(|define| define == "UNITY_EDITOR" || define.starts_with("UNITY_EDITOR_"))
        {
            bail!("compiler export uses Editor-only defines: {}", config.name);
        }
        config.sources.sort();
        let configuration = canonical_configuration(&config)?;
        compiler
            .configuration_hashes
            .insert(&config.name, sha256(&configuration));
        if !own_graph {
            let actual = file_hash(&exported.path)
                .map_err(|error| anyhow!("runtime export DLL {}: {error}", exported.path))?;
            if !exported.sha256.is_empty() && exported.sha256 != actual {
                bail!("runtime exported DLL hash differs: {}", exported.path);
            }
            compiler
                .current_dlls
                .insert(&config.name, exported.path.clone());
        } else {
            // A source graph deliberately has no Editor-produced gameplay DLL.
            // Nonempty sentinels keep native reference selection/order exact until
            // each SDK output is available.
            compiler
                .current_dlls
                .insert(&config.name, format!("source-unbuilt:{}", config.name));
        }
        compiler.manifest.assemblies.push(CompileAssembly {
            name: config.name.clone(),
            sources: config.sources.clone(),
            references: config.dependencies.clone(),
            editor_only: compiler.play_mode && exported.editor_only,
            ..Default::default()
        });
        compiler.configs.insert(config.name.clone(), config);
    }
    // Package assemblies the host does not provide are built first, as the
    // Editor builds them, and join the staged host before game code is
    // compiled against it.
    let helper = compiler.prepare_lifecycle_helper(ctx)?;
    if own_graph {
        build_package_assemblies(
            ctx,
            &mut compiler,
            &mut graph,
            &staging_path,
            &helper,
            &managed,
            &slots_file,
        )?;
        // After the packages: the bridge serves what the host's packages
        // hold (Addressables), which a host without them gets built here.
        refresh_bridge(ctx, &compiler, &helper, &managed)?;
    }
    let mut ordered = order_compiler_assemblies(&compiler.manifest.assemblies)?;
    compiler.manifest.assemblies = ordered.clone();
    // Snapshot all baseline Assets scripts, including deliberately excluded
    // profiles. Future additions remain distinguishable from existing input.
    for root in ["Assets", "ProjectSettings", "Packages"] {
        let physical = join(&[&project, root]);
        if matches!(fs::metadata(&physical), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        sys::walk_dir(&physical, &mut |entry| {
            if entry.is_dir() {
                return Ok(Walk::Continue);
            }
            let filename = entry.path;
            let extension = rune_lower(sysutil::ext(filename));
            let tracked = root == "ProjectSettings"
                || matches!(
                    extension.as_str(),
                    ".cs" | ".asmdef" | ".asmref" | ".rsp" | ".dll"
                )
                || filename.ends_with(".cs.meta")
                || filename.ends_with(".asmdef.meta")
                || filename.ends_with(".asmref.meta")
                || filename.ends_with(".dll.meta")
                || filename == join(&[&project, "Packages/manifest.json"])
                || filename == join(&[&project, "Packages/packages-lock.json"])
                || sysutil::base(filename) == "package.json";
            if tracked {
                let hash = file_hash(filename)?;
                let expected = compiler.manifest.compiler_inputs.at(filename);
                if !expected.is_empty() && expected != hash {
                    bail!("compiler export is stale: {filename}");
                }
                compiler.manifest.compiler_inputs.insert(filename, hash);
            }
            Ok(Walk::Continue)
        })?;
    }
    let mut snapshots: BTreeMap<String, String> = BTreeMap::new();
    let mut source_hashes = NilMap::make();
    for assembly in &ordered {
        for source in &assembly.sources {
            let mut allowed_root = within_directory(source, &join(&[&project, "Assets"]))
                || within_directory(source, &join(&[&project, "Packages"]))
                || within_directory(source, &join(&[&project, "Library/PackageCache"]));
            if own_graph {
                allowed_root |= graph
                    .source_roots
                    .iter()
                    .any(|root| within_directory(source, &root.source));
            }
            if !sysutil::is_abs(source) || !allowed_root {
                bail!("runtime source is outside explicit project/package roots: {source}");
            }
            let data = sys::read(source)?;
            let hash = sha256(&data);
            let expected = compiler.manifest.compiler_inputs.at(source);
            if !expected.is_empty() && expected != hash {
                bail!("compiler source export is stale: {source}");
            }
            let snapshot = join(&[&staging_path, "Sources", &hash, &sysutil::base(source)]);
            sys::mkdir_all(&sysutil::dir(&snapshot))?;
            sys::write(&snapshot, &data)?;
            snapshots.insert(source.clone(), snapshot);
            source_hashes.insert(source, hash.clone());
            compiler.manifest.compiler_inputs.insert(source, hash);
            let meta = format!("{source}.meta");
            if let Ok(hash) = file_hash(&meta) {
                compiler.configuration.insert(meta.clone(), hash.clone());
                compiler.manifest.compiler_inputs.insert(meta, hash);
            }
        }
    }
    compiler.collect_configuration_inputs();
    compiler.validate_prepared_script_identities()?;
    let scripts = compiler.manifest.scripts.to_vec();
    for script in scripts {
        if !script.type_name.is_empty() && !script.host_provided {
            compiler.expect_script_type(&script.assembly, &script.type_name, &script.guid);
        }
    }
    if own_graph {
        compiler.derive_source_host_script_types(ctx, &helper, &graph, &managed)?;
    } else {
        compiler.filter_prepared_host_scripts(ctx, &helper, &graph)?;
    }
    compiler.validate_prepared_host_scripts(ctx, &helper, &managed, &graph)?;
    if !own_graph
        && (compiler.manifest.native_only
            || ordered.iter().any(|assembly| {
                assembly
                    .references
                    .iter()
                    .any(|name| name == "UnityMicroEditor")
            }))
    {
        compiler.validate_prepared_host_bridge(ctx, &helper, &managed)?;
    }
    let mut built: BTreeMap<String, String> = BTreeMap::new();
    let host_references = sys::glob_lossy(&join(&[&managed, "*.dll"]));
    let stem = |path: &str| {
        let base = sysutil::base(path);
        base.strip_suffix(".dll").unwrap_or(&base).to_string()
    };
    for index in 0..ordered.len() {
        let mut assembly = ordered[index].clone();
        let (base, engine) = match compiler.play_mode && assembly.editor_only {
            true => compiler.editor_only_references(),
            false => (
                compiler.base_references.to_vec(),
                compiler.engine_references.to_vec(),
            ),
        };
        let mut refs: Vec<String> = base;
        if own_graph {
            refs.push(join(&[&managed, "UnityMicroEditor.dll"]));
        }
        if !compiler
            .configs
            .value(&assembly.name)
            .config
            .no_engine_references
        {
            refs.extend(engine);
        }
        if !own_graph {
            for reference in &host_references {
                if compiler.current_dlls.at(&stem(reference)).is_empty() {
                    refs.push(reference.clone());
                }
            }
        }
        for exported in &graph.assemblies {
            if exported.config.name != assembly.name {
                continue;
            }
            for reference in &exported.compiled_references {
                let name = stem(reference);
                if name.starts_with("UnityEditor") && !compiler.play_mode {
                    bail!("Editor-only reference in runtime compiler export: {reference}");
                }
                if compiler.current_dlls.at(&name).is_empty() {
                    refs.push(reference.clone());
                }
            }
        }
        for dependency in &assembly.references {
            if let Some(dll) = built.get(dependency).filter(|dll| !dll.is_empty()) {
                refs.push(dll.clone());
                continue;
            }
            if !compiler.current_dlls.at(dependency).is_empty() {
                bail!("runtime dependency was not built in order: {dependency}");
            }
            let dll = join(&[&managed, &format!("{dependency}.dll")]);
            if file_hash(&dll).is_err() {
                if own_graph {
                    bail!(
                        "Authored runtime dependency {dependency} requires a verified native profile or SDK source assembly"
                    );
                }
                // Unity reports project/package assembly references separately
                // from compiledReferences. This export profile places their
                // output DLLs in the explicit project's ScriptAssemblies.
                let original = join(&[
                    &project,
                    "Library/ScriptAssemblies",
                    &format!("{dependency}.dll"),
                ]);
                if dependency.starts_with("UnityEditor") {
                    bail!("Editor-only runtime dependency: {dependency}");
                }
                let hash = file_hash(&original).map_err(|error| {
                    anyhow!(
                        "runtime dependency {dependency} has neither a host slot nor verified project DLL: {error}"
                    )
                })?;
                compiler
                    .manifest
                    .compiler_inputs
                    .insert(original.clone(), hash);
                sys::write(&dll, &sys::read(&original)?)?;
                // The materialized runtime package requires its own Mono slot.
                register_compiler_assembly_slots(
                    &slots_file,
                    &[CompileAssembly {
                        name: dependency.clone(),
                        ..Default::default()
                    }],
                )?;
            }
            refs.push(dll);
        }
        let refs = deduplicate_references(&refs);
        for reference in &refs {
            let hash = file_hash(reference)?;
            // Player-owned bridge references are independently staged on launch.
            if !compiler.host_bridge_reference(reference)
                && compiler.current_dlls.at(&stem(reference)).is_empty()
            {
                compiler.manifest.compiler_inputs.insert(reference, hash);
            }
        }
        let (dll, proof) = compiler
            .compile_assembly(
                ctx,
                &staging_path,
                &helper,
                &assembly,
                &refs,
                &snapshots,
                &source_hashes,
            )
            .map_err(|error| {
                // A package assembly the host does not provide is compiled
                // like game code, which not every package supports yet.
                let package = !assembly.sources.is_empty()
                    && assembly
                        .sources
                        .iter()
                        .all(|source| !within_directory(source, &join(&[&project, "Assets"])));
                match package {
                    true => error.context(format!("{HOST_LACKS_PACKAGE}{}", assembly.name)),
                    false => error,
                }
            })?;
        let mut previous_dll = compiler.current_dlls.at(&assembly.name).to_string();
        if own_graph {
            previous_dll.clear();
            if compiler.proofs.is_nil() {
                compiler.proofs = NilMap::make();
            }
            compiler.proofs.insert(&assembly.name, proof.clone());
        }
        compiler.validate_compiled_script_types(
            ctx,
            &helper,
            &assembly.name,
            &previous_dll,
            &dll,
        )?;
        built.insert(assembly.name.clone(), dll.clone());
        assembly.path = join(&[&managed, &format!("{}.dll", assembly.name)]);
        assembly.sha256 = file_hash(&dll)?;
        assembly.assembly_input_hash = sysutil::base(&sysutil::dir(&dll));
        assembly.lifecycle = proof.clone();
        assembly.lifecycle_helper = compiler.helper_key.clone();
        if needs_netcode(&assembly)
            && let Some(processor) = &compiler.netcode
        {
            assembly.netcode_postprocessor = processor.key.clone();
        }
        compiler.manifest.assemblies[index] = assembly.clone();
        ordered[index] = assembly.clone();
        if own_graph {
            for script in compiler.manifest.scripts.iter_mut() {
                if !script.host_provided && script.assembly == assembly.name {
                    script.type_name = proof.script_types.at(&script.path).to_string();
                }
            }
        }
        let built_stem = dll.strip_suffix(".dll").unwrap_or(&dll);
        for extension in [".dll", ".pdb"] {
            let data = sys::read(&format!("{built_stem}{extension}"))?;
            sys::write(
                &join(&[&managed, &format!("{}{extension}", assembly.name)]),
                &data,
            )?;
        }
        let configuration = canonical_configuration(&compiler.configs.value(&assembly.name))?;
        // The descriptor belongs to this preparation, the assembly to the
        // project's store: it is kept where the baseline's readers look,
        // under the preparation's own cache.
        let described = join(&[&compiler.cache, "assemblies", &assembly.assembly_input_hash]);
        sys::mkdir_all(&described)?;
        atomic_json(
            &join(&[&described, "compile.json"]),
            &descriptor(&compiler.manifest, &configuration, &assembly)?,
        )?;
    }
    register_compiler_assembly_slots(&slots_file, &ordered)?;
    if compiler.play_mode {
        compiler.write_play_mode_stand_ins(ctx, &staging_path, &managed)?;
    }
    // The Player reads the project's settings, not those the host was built
    // with; what each manager kept of the host is recorded beside it.
    let managers = super::settings::prepare_managers(&project, &staged_player)?;
    atomic_json(&join(&[&staging_path, "settings-managers.json"]), &managers)?;
    super::complete_player(ctx, &project, editor, &staged_player)?;
    if own_graph {
        remove_source_native_initializers(
            &join(&[&sysutil::dir(&managed), "RuntimeInitializeOnLoads.json"]),
            &ordered,
        )?;
    }
    if compiler.manifest.native_only {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Registration {
            names: NilVec<String>,
        }
        if !read_json_file::<Option<Registration>>(&slots_file).is_ok_and(|registration| {
            registration
                .unwrap_or_default()
                .names
                .iter()
                .any(|name| name == "UnityMicroEditor.dll")
        }) {
            bail!("native-only host lacks its runtime bridge registration");
        }
        compiler.manifest.native_slots_sha = file_hash(&slots_file)?;
    }
    for (filename, expected) in &compiler.manifest.compiler_inputs {
        if expected == "absent" {
            if sys::absent(filename) {
                continue;
            }
            bail!("absent compiler input appeared during preparation: {filename}");
        }
        // A C# source that was saved again while it was prepared is compiled
        // at the start of a session like any source edited after a
        // preparation: the session compares its content with the digest the
        // baseline records. Its presence, and every other input, is the graph.
        let source = rune_lower(sysutil::ext(filename)) == ".cs";
        if source && !sys::absent(filename) {
            continue;
        }
        if !file_hash(filename).is_ok_and(|actual| actual == *expected) {
            bail!("compiler input changed during preparation; baseline not published: {filename}");
        }
    }
    let result = CompilerPreparationResult {
        schema_version: 1,
        player: staged_player,
        compiler: compiler.manifest,
    };
    publish(&output, &result)?;
    staging.published = true;
    Ok(result)
}

#[derive(Serialize)]
struct SyntaxSource {
    path: String,
    #[serde(rename = "originalPath")]
    original_path: String,
}
#[derive(Serialize)]
struct SyntaxRequest {
    #[serde(rename = "assemblyName")]
    assembly_name: String,
    defines: NilVec<String>,
    #[serde(rename = "editorDefines")]
    editor_defines: NilVec<String>,
    sources: NilVec<SyntaxSource>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct SyntaxProof {
    player: NilMap<NilVec<String>>,
    editor: NilMap<NilVec<String>>,
}

fn exported_host_path(
    compiler: &Compiler,
    graph: &CompilerGraphExport,
    assembly: &CompilerHostAssembly,
) -> bool {
    if graph.backend == "authored_source" {
        sysutil::is_abs(&assembly.path) && sysutil::clean(&assembly.path) == assembly.path
    } else {
        assembly.path
            == join(&[
                &compiler.project,
                "Library/ScriptAssemblies",
                &format!("{}.dll", assembly.name),
            ])
    }
}
fn valid_source_root(path: &str, source: &str) -> bool {
    path.starts_with("Packages/") && sysutil::clean(path) == path && sysutil::is_abs(source)
}

impl Compiler {
    /// GetClass returns the Editor's type even for
    /// runtime asmdef sources wrapped in UNITY_EDITOR. Resolve active
    /// declarations with the public Player profile before requiring native
    /// slots/types; an active declaration still must match the native host and
    /// cannot be ignored to hide a stale deployment.
    pub fn filter_prepared_host_scripts(
        &mut self,
        ctx: &Context,
        helper: &str,
        graph: &CompilerGraphExport,
    ) -> Result<()> {
        let mut exported: BTreeMap<&str, &CompilerHostAssembly> = BTreeMap::new();
        for assembly in &graph.host_assemblies {
            if assembly.name.is_empty()
                || sysutil::base(&assembly.name) != assembly.name
                || assembly.name.starts_with("UnityEditor")
                || exported.contains_key(assembly.name.as_str())
                || !exported_host_path(self, graph, assembly)
            {
                bail!("duplicate package assembly preprocessing profile");
            }
            if canonical_destination(&assembly.path).ok().as_deref() != Some(assembly.path.as_str())
            {
                bail!("exported package declaration assembly contains a symlink");
            }
            if assembly.sha256.is_empty()
                || !file_hash(&assembly.path).is_ok_and(|expected| expected == assembly.sha256)
            {
                bail!(
                    "exported package declaration assembly digest differs: {}",
                    assembly.name
                );
            }
            self.manifest
                .compiler_inputs
                .insert(&assembly.path, assembly.sha256.clone());
            exported.insert(&assembly.name, assembly);
        }
        let mut requests: BTreeMap<String, SyntaxRequest> = BTreeMap::new();
        for script in &self.manifest.scripts {
            if !script.host_provided {
                continue;
            }
            let assembly = match exported.get(script.assembly.as_str()) {
                Some(assembly)
                    if assembly.sources.contains(&script.path)
                        && !assembly.defines.is_empty()
                        && assembly
                            .editor_defines
                            .iter()
                            .any(|define| define == "UNITY_EDITOR") =>
                {
                    *assembly
                }
                _ => bail!(
                    "package MonoScript lacks exact Player sources/defines; export a fresh graph: {}",
                    script.path
                ),
            };
            if assembly.sources.len() > 65536
                || assembly.defines.len() > 4096
                || assembly.editor_defines.len() > 4096
            {
                bail!("package preprocessing profile exceeds its input bound");
            }
            if !graph
                .source_roots
                .iter()
                .any(|root| within_directory(&script.path, &root.source))
            {
                bail!(
                    "package preprocessing source is outside registered package roots: {}",
                    script.path
                );
            }
            if assembly
                .defines
                .iter()
                .any(|define| define == "UNITY_EDITOR" || define.starts_with("UNITY_EDITOR_"))
            {
                bail!(
                    "package Player preprocessing contains Editor-only defines: {}",
                    script.assembly
                );
            }
            if canonical_destination(&script.path).ok().as_deref() != Some(script.path.as_str()) {
                bail!(
                    "package MonoScript source contains a symlink: {}",
                    script.path
                );
            }
            if !fs::metadata(&script.path)
                .is_ok_and(|info| info.is_file() && info.len() <= 16 * 1024 * 1024)
            {
                bail!(
                    "package MonoScript source is not a bounded regular file: {}",
                    script.path
                );
            }
            let expected = self.manifest.compiler_inputs.at(&script.path);
            let data = match sys::read(&script.path) {
                Ok(data) if !expected.is_empty() && sha256(&data) == expected => data,
                _ => bail!(
                    "package source differs from its exported preprocessing digest: {}",
                    script.path
                ),
            };
            let snapshot = join(&[
                &self.cache,
                "HostSources",
                &sha256(&data),
                &sysutil::base(&script.path),
            ]);
            sys::mkdir_all(&sysutil::dir(&snapshot))?;
            sys::write(&snapshot, &data)?;
            requests
                .entry(assembly.name.clone())
                .or_insert_with(|| SyntaxRequest {
                    assembly_name: assembly.name.clone(),
                    defines: assembly.defines.clone(),
                    editor_defines: assembly.editor_defines.clone(),
                    sources: NilVec::nil(),
                })
                .sources
                .push(SyntaxSource {
                    path: snapshot,
                    original_path: script.path.clone(),
                });
        }
        if requests.is_empty() {
            return Ok(());
        }
        let ordered: Vec<&SyntaxRequest> = requests.values().collect();
        let request_path = join(&[&self.cache, "host-source-declarations.json"]);
        atomic_json(&request_path, &ordered)?;
        let output = compiler_command(
            ctx,
            &self.cache,
            &self.dotnet,
            &[helper, "--source-type-declarations", &request_path],
        )
        .map_err(|error| anyhow!("Player package preprocessing proof: {error}"))?;
        let declarations = match unmarshal::<NilMap<SyntaxProof>>(&output) {
            Ok(declarations) if declarations.len() == requests.len() => declarations,
            _ => bail!("Player package preprocessing lacks exact assembly proof"),
        };
        let scripts = self.manifest.scripts.to_vec();
        self.manifest.scripts.truncate();
        for script in scripts {
            if script.host_provided {
                let proof = declarations.get(&script.assembly);
                if !proof
                    .and_then(|proof| proof.editor.get(&script.path))
                    .is_some_and(|names| names.contains(&script.type_name))
                {
                    bail!(
                        "exported package MonoScript type lacks its exact Editor source declaration: {}",
                        script.type_name
                    );
                }
                let Some(names) = proof.and_then(|proof| proof.player.get(&script.path)) else {
                    bail!(
                        "Player preprocessing omitted package source: {}",
                        script.path
                    );
                };
                if !names.contains(&script.type_name) {
                    continue;
                }
            }
            self.manifest.scripts.push(script);
        }
        Ok(())
    }

    /// Game code must compile against the bridge
    /// that will actually run in Player. Editor DLLs can contain newer public
    /// APIs than an older prepared native host; replacing the Player bridge
    /// with an Editor-compiled DLL would conceal drift.
    pub fn validate_prepared_host_bridge(
        &self,
        ctx: &Context,
        helper: &str,
        managed: &str,
    ) -> Result<()> {
        let host = join(&[managed, "UnityMicroEditor.dll"]);
        file_hash(&host).map_err(|error| {
            anyhow!(
                "native host lacks its runtime bridge; rebuild the matching host before compiler preparation: {error}"
            )
        })?;
        // Public Editor graphs report this project/package assembly by name rather
        // than in compiledReferences. SDK-only exported fixtures need no Editor DLL.
        let editor = join(&[
            &self.project,
            "Library/ScriptAssemblies/UnityMicroEditor.dll",
        ]);
        let before = match file_hash(&editor) {
            Ok(before) => before,
            Err(error) if sys::is_not_exist(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        let output = compiler_command(
            ctx,
            &self.cache,
            &self.dotnet,
            &[helper, "--public-type-definitions", &host, &editor],
        )
        .map_err(|error| anyhow!("verify native host runtime API: {error}"))?;
        let types = unmarshal::<NilMap<NilVec<String>>>(&output).unwrap_or_default();
        let listed = |dll: &str| types.get(dll).filter(|names| !names.is_nil());
        let (Some(available), Some(required), 2) = (listed(&host), listed(&editor), types.len())
        else {
            bail!("native host runtime API inventory lacks exact assembly metadata");
        };
        if !file_hash(&editor).is_ok_and(|after| after == before) {
            bail!(
                "Editor runtime bridge changed during host API verification; export a fresh graph"
            );
        }
        for name in required {
            if !available.contains(name) {
                bail!(
                    "prepared native host runtime bridge is stale: public type {name} exists in Editor but is absent from Player; rebuild the matching host before compiler preparation"
                );
            }
        }
        Ok(())
    }

    pub fn validate_prepared_script_identities(&mut self) -> Result<()> {
        let source_assemblies = self.graph_source_assemblies();
        let mut by_source = BTreeSet::new();
        let mut by_guid: BTreeMap<String, String> = BTreeMap::new();
        for script in &self.manifest.scripts {
            if script.guid.is_empty()
                || !script.host_provided
                    && source_assemblies
                        .get(&script.path)
                        .map_or("", String::as_str)
                        != script.assembly
                || by_source.contains(&script.path)
            {
                bail!(
                    "exported MonoScript has no exact unique runtime source/assembly: {}",
                    script.path
                );
            }
            let meta = format!("{}.meta", script.path);
            if canonical_destination(&meta).ok().as_deref() != Some(meta.as_str()) {
                bail!(
                    "MonoScript metadata path contains a symlink: {}",
                    script.path
                );
            }
            if !fs::metadata(&meta).is_ok_and(|info| info.is_file() && info.len() <= 1024 * 1024) {
                bail!(
                    "MonoScript metadata is not a bounded regular file: {}",
                    script.path
                );
            }
            let data = sys::read(&meta)
                .map_err(|error| anyhow!("runtime MonoScript metadata {}: {error}", script.path))?;
            match registry::meta_guid(&data) {
                Ok(guid) if guid == script.guid && !by_guid.contains_key(&guid) => {
                    by_guid.insert(guid, script.path.clone());
                }
                _ => bail!(
                    "exported MonoScript GUID differs from authored metadata or is duplicated: {}",
                    script.path
                ),
            }
            by_source.insert(script.path.clone());
        }
        for (source, assembly) in &source_assemblies {
            if by_source.contains(source) {
                continue;
            }
            let data = match sys::read(&format!("{source}.meta")) {
                Ok(data) => data,
                Err(error) if sys::is_not_exist(&error) => continue,
                Err(error) => bail!("runtime MonoScript metadata {source}: {error}"),
            };
            match registry::meta_guid(&data) {
                Ok(guid) if !by_guid.contains_key(&guid) => {
                    self.manifest.scripts.push(CompileScript {
                        guid: guid.clone(),
                        path: source.clone(),
                        assembly: assembly.clone(),
                        ..Default::default()
                    });
                    by_guid.insert(guid, source.clone());
                }
                _ => bail!("runtime source MonoScript identity is invalid or duplicated: {source}"),
            }
        }
        self.manifest.scripts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(())
    }

    /// Package MonoScripts use their native
    /// host's defining assembly. Editor and Player DLL bytes legitimately
    /// differ because of UNITY_EDITOR, so verify the exported Editor digest,
    /// the source/meta identity, and the exact native type definition
    /// separately; retain the actual host DLL digest in the baseline.
    pub fn validate_prepared_host_scripts(
        &mut self,
        ctx: &Context,
        helper: &str,
        managed: &str,
        graph: &CompilerGraphExport,
    ) -> Result<()> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Registration {
            names: NilVec<String>,
        }
        let registration: Registration = read_json_file::<Option<_>>(&join(&[
            &sysutil::dir(managed),
            "ScriptingAssemblies.json",
        ]))
        .map_err(|error| anyhow!("native host scripting registrations: {error}"))?
        .unwrap_or_default();
        for root in &graph.source_roots {
            if root.path.split('/').count() != 2 || !valid_source_root(&root.path, &root.source) {
                bail!("invalid compiler package source root");
            }
            if canonical_destination(&root.source).ok().as_deref() != Some(root.source.as_str()) {
                bail!("compiler package source root contains a symlink");
            }
            if !fs::metadata(&root.source).is_ok_and(|info| info.is_dir()) {
                bail!("compiler package source root is absent");
            }
        }
        let mut exported: BTreeMap<&str, &CompilerHostAssembly> = BTreeMap::new();
        for assembly in &graph.host_assemblies {
            if assembly.name.is_empty()
                || sysutil::base(&assembly.name) != assembly.name
                || assembly.name.starts_with("UnityEditor") && !self.play_mode
                || exported.contains_key(assembly.name.as_str())
                || !exported_host_path(self, graph, assembly)
            {
                bail!("invalid or duplicate exported host assembly identity");
            }
            exported.insert(&assembly.name, assembly);
        }
        let mut by_assembly: BTreeMap<String, String> = BTreeMap::new();
        let scripts = self.manifest.scripts.to_vec();
        for script in &scripts {
            if !script.host_provided {
                continue;
            }
            let Some(assembly) = exported
                .get(script.assembly.as_str())
                .filter(|_| !script.type_name.is_empty())
            else {
                bail!(
                    "package MonoScript lacks a public defining assembly/type: {}",
                    script.path
                );
            };
            let mut registered = false;
            for root in &graph.source_roots {
                if !valid_source_root(&root.path, &root.source) {
                    bail!("invalid compiler package source root");
                }
                registered |= within_directory(&script.path, &root.source);
            }
            if !registered {
                bail!(
                    "package MonoScript is outside its registered public source roots: {}",
                    script.path
                );
            }
            for path in [script.path.clone(), format!("{}.meta", script.path)] {
                if canonical_destination(&path).ok().as_deref() != Some(path.as_str()) {
                    bail!("package MonoScript source contains a symlink: {path}");
                }
                let expected = self.manifest.compiler_inputs.at(&path);
                if expected.is_empty() || !file_hash(&path).is_ok_and(|hash| hash == expected) {
                    bail!("package MonoScript source/meta differs from exported digest: {path}");
                }
            }
            if by_assembly.contains_key(&script.assembly) {
                continue;
            }
            let library = format!("{}.dll", assembly.name);
            if !registration.names.contains(&library) {
                bail!(
                    "native host package assembly {} has no native Mono slot; build matching package/profile host before preparation",
                    assembly.name
                );
            }
            if assembly.sha256.is_empty()
                || !file_hash(&assembly.path).is_ok_and(|expected| expected == assembly.sha256)
            {
                bail!(
                    "exported package assembly digest differs: {}",
                    assembly.name
                );
            }
            let host = join(&[managed, &library]);
            let hash = file_hash(&host).map_err(|error| {
                anyhow!(
                    "native host lacks package assembly {}; build matching package/profile host before preparation: {error}",
                    assembly.name
                )
            })?;
            self.manifest.compiler_inputs.insert(host.clone(), hash);
            by_assembly.insert(script.assembly.clone(), host);
        }
        if by_assembly.is_empty() {
            return Ok(());
        }
        let mut paths: Vec<&str> = by_assembly.values().map(String::as_str).collect();
        paths.sort_unstable();
        let definitions = self
            .type_definitions(ctx, helper, &paths)
            .map_err(|error| anyhow!("verify native package types: {error}"))?;
        let definitions = match definitions {
            definitions if definitions.len() == paths.len() => definitions,
            _ => bail!("native package type proof lacks exact assembly metadata"),
        };
        for script in &scripts {
            if script.host_provided
                && !by_assembly
                    .get(&script.assembly)
                    .and_then(|host| definitions.get(host))
                    .is_some_and(|names| names.contains(&script.type_name))
            {
                bail!(
                    "native host package assembly {} lacks exported MonoScript type {}; rebuild the matching host",
                    script.assembly,
                    script.type_name
                );
            }
        }
        Ok(())
    }
}

/// Dependency order, stable in the input order.
pub fn order_compiler_assemblies(
    assemblies: &[CompileAssembly],
) -> Result<NilVec<CompileAssembly>> {
    let mut by_name: BTreeMap<&str, &CompileAssembly> = BTreeMap::new();
    for assembly in assemblies {
        if by_name.insert(&assembly.name, assembly).is_some() {
            bail!("duplicate runtime assembly: {}", assembly.name);
        }
    }
    fn visit<'a>(
        name: &'a str,
        by_name: &BTreeMap<&'a str, &'a CompileAssembly>,
        state: &mut BTreeMap<&'a str, u8>,
        ordered: &mut NilVec<CompileAssembly>,
    ) -> Result<()> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(1) => bail!("cyclic runtime assembly graph: {name}"),
            _ => {}
        }
        state.insert(name, 1);
        let assembly = by_name[name];
        for dependency in &assembly.references {
            if by_name.contains_key(dependency.as_str()) {
                visit(dependency, by_name, state, ordered)?;
            }
        }
        state.insert(name, 2);
        ordered.push(assembly.clone());
        Ok(())
    }
    let mut state = BTreeMap::new();
    let mut ordered = NilVec::nil();
    for assembly in assemblies {
        visit(&assembly.name, &by_name, &mut state, &mut ordered)?;
    }
    Ok(ordered)
}

/// Appends Mono assembly slots to the
/// host's registration table, preserving every other field.
pub fn register_compiler_assembly_slots(
    filename: &str,
    assemblies: &[CompileAssembly],
) -> Result<()> {
    let mut doc: RawMap = read_json_file::<Option<_>>(filename)?.unwrap_or_default();
    let decoded = |key: &str| doc.get(key).map(|raw| raw.get().as_bytes());
    let parsed = decoded("names")
        .and_then(|raw| unmarshal::<NilVec<String>>(raw).ok())
        .zip(decoded("types").and_then(|raw| unmarshal::<NilVec<i64>>(raw).ok()));
    let (mut names, mut types) = match parsed {
        Some((names, types)) if names.len() == types.len() && names.len() <= 4096 => (names, types),
        _ => bail!("host Mono assembly registration lacks exact matching names/types"),
    };
    let mut seen = BTreeSet::new();
    for name in &names {
        if !seen.insert(name.clone()) {
            bail!("host Mono registration has duplicate assembly slot: {name}");
        }
    }
    for assembly in assemblies {
        let name = format!("{}.dll", assembly.name);
        if seen.insert(name.clone()) {
            names.push(name);
            types.push(16);
        }
    }
    doc.insert("names".into(), raw_value(marshal(&names)?)?);
    doc.insert("types".into(), raw_value(marshal(&types)?)?);
    write_raw_map(filename, &doc)
}

/// Builds the package assemblies of the graph that the host does not
/// provide (`packageBuild`) into the staged Player, in dependency order, and
/// hands them on as host assemblies: game code is compiled against them and
/// their scripts are the host's.
fn build_package_assemblies(
    ctx: &Context,
    compiler: &mut Compiler,
    graph: &mut CompilerGraphExport,
    staging_path: &str,
    helper: &str,
    managed: &str,
    slots_file: &str,
) -> Result<()> {
    let names: BTreeSet<String> = graph
        .assemblies
        .iter()
        .filter(|assembly| assembly.package_build)
        .map(|assembly| assembly.config.name.clone())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let packages = order_compiler_assemblies(
        &compiler
            .manifest
            .assemblies
            .iter()
            .filter(|assembly| names.contains(&assembly.name))
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    let games: Vec<CompileAssembly> = compiler
        .manifest
        .assemblies
        .iter()
        .filter(|assembly| !names.contains(&assembly.name))
        .cloned()
        .collect();
    compiler.manifest.assemblies = NilVec::from(games);
    for script in compiler.manifest.scripts.iter_mut() {
        if names.contains(&script.assembly) {
            script.host_provided = true;
        }
    }
    // 1. Each package assembly compiled from its sources as they are.
    let mut compiled: BTreeMap<String, String> = BTreeMap::new();
    let mut references: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for assembly in &packages {
        let config = compiler.configs.value(&assembly.name);
        let exported = graph
            .assemblies
            .iter()
            .find(|exported| exported.config.name == assembly.name)
            .cloned()
            .unwrap_or_default();
        let (base, engine) = match compiler.play_mode && exported.editor_only {
            true => compiler.editor_only_references(),
            false => (
                compiler.base_references.to_vec(),
                compiler.engine_references.to_vec(),
            ),
        };
        let mut refs: Vec<String> = base;
        if !config.config.no_engine_references {
            refs.extend(engine);
        }
        for reference in &exported.compiled_references {
            if sysutil::base(reference).starts_with("UnityEditor") && !compiler.play_mode {
                bail!(
                    "Editor-only reference in package assembly {}: {reference}",
                    assembly.name
                );
            }
            refs.push(reference.clone());
        }
        for dependency in &assembly.references {
            match compiled.get(dependency) {
                Some(dll) => refs.push(dll.clone()),
                None => {
                    let dll = join(&[managed, &format!("{dependency}.dll")]);
                    if file_hash(&dll).is_err() {
                        bail!(
                            "package assembly {} needs {dependency}, which neither the host nor a package build provides",
                            assembly.name
                        );
                    }
                    refs.push(dll);
                }
            }
        }
        let refs = deduplicate_references(&refs).to_vec();
        for source in &assembly.sources {
            let hash = file_hash(source)?;
            compiler.manifest.compiler_inputs.insert(source, hash);
        }
        let dll = compiler
            .compile_plain(
                ctx,
                staging_path,
                &super::packages::PlainCompile {
                    name: &assembly.name,
                    sources: &assembly.sources,
                    defines: &config.defines,
                    compiler_options: &config.compiler_options,
                    allow_unsafe: config.config.allow_unsafe,
                    references: &refs,
                    kind: "package-compiles",
                    project_view: "",
                },
            )
            .map_err(|error| error.context(format!("{HOST_LACKS_PACKAGE}{}", assembly.name)))?;
        compiled.insert(assembly.name.clone(), dll);
        references.insert(assembly.name.clone(), refs);
    }
    // 2. The post-processor assemblies of the packages, against those or
    // the host's assemblies they name.
    let editor = compiler.editor.clone();
    let data = join(&[&sysutil::dir(&editor), "Data"]);
    let pipeline = join(&[&data, "Managed/Unity.CompilationPipeline.Common.dll"]);
    let builtin = join(&[&data, "Tools/BuildPipeline/BuiltinILPPs"]);
    let mut codegen = Vec::new();
    let mut helpers = Vec::new();
    'codegen: for assembly in graph.codegen_assemblies.clone() {
        // Post-processors are Editor code: the Editor's engine and Editor
        // modules stand for the Player's.
        let mut refs: Vec<String> = compiler.base_references.to_vec();
        for pattern in [
            "UnityEngine.dll",
            "UnityEditor.dll",
            "UnityEngine.*.dll",
            "UnityEditor.*.dll",
        ] {
            refs.extend(sys::glob_lossy(&join(&[
                &data,
                "Managed/UnityEngine",
                pattern,
            ])));
        }
        refs.push(pipeline.clone());
        for precompiled in &assembly.precompiled {
            refs.push(precompiled.clone());
        }
        // Cecil comes with the installation's own post-processors when no
        // package of the project brings it.
        for name in [
            "Mono.Cecil.dll",
            "Mono.Cecil.Rocks.dll",
            "Mono.Cecil.Pdb.dll",
            "Mono.Cecil.Mdb.dll",
        ] {
            if !refs
                .iter()
                .any(|reference| sysutil::base(reference) == name)
            {
                refs.push(join(&[&builtin, name]));
            }
        }
        for reference in &assembly.references {
            let dll = match compiled.get(reference) {
                Some(dll) => dll.clone(),
                None => join(&[managed, &format!("{reference}.dll")]),
            };
            if file_hash(&dll).is_err() {
                eprintln!(
                    "[unity-micro-editor] post-processor {} is left out: it names {reference}, which the Player lacks",
                    assembly.name
                );
                continue 'codegen;
            }
            refs.push(dll);
        }
        let refs = deduplicate_references(&refs).to_vec();
        let dll = compiler.compile_plain(
            ctx,
            staging_path,
            &super::packages::PlainCompile {
                name: &assembly.name,
                sources: &assembly.sources,
                defines: &assembly.defines,
                compiler_options: &assembly.compiler_options,
                allow_unsafe: assembly.allow_unsafe,
                references: &refs,
                kind: "package-codegen",
                project_view: "",
            },
        )?;
        codegen.push(dll);
        for reference in refs {
            if !reference.contains("/NetStandard/") && !helpers.contains(&reference) {
                helpers.push(reference);
            }
        }
    }
    // 3. Every package assembly post-processed by all of them.
    let mut post = super::packages::PostProcessors::new(
        &editor,
        &join(&[staging_path, "ilpp"]),
        &codegen,
        &helpers,
    )?;
    let mut rows = Vec::new();
    for assembly in &packages {
        let config = compiler.configs.value(&assembly.name);
        let build = compiler.post_process_package(
            ctx,
            staging_path,
            helper,
            &assembly.name,
            &compiled[&assembly.name],
            &config.defines,
            &references[&assembly.name],
            &mut post,
        )?;
        let target = join(&[managed, &format!("{}.dll", assembly.name)]);
        let stem = build
            .dll
            .strip_suffix(".dll")
            .unwrap_or(&build.dll)
            .to_string();
        if compiler.play_mode {
            for to in [
                target.clone(),
                format!("{}.pdb", target.strip_suffix(".dll").unwrap_or(&target)),
            ] {
                let _ = fs::remove_file(&to);
            }
            compiler.link_play_mode_assembly(
                ctx,
                &join(&[staging_path, "PlayModePackages", &assembly.name]),
                &assembly.name,
                &build.dll,
                &target,
            )?;
        } else {
            for (from, to) in [
                (build.dll.clone(), target.clone()),
                (
                    format!("{stem}.pdb"),
                    format!("{}.pdb", target.strip_suffix(".dll").unwrap_or(&target)),
                ),
            ] {
                let _ = fs::remove_file(&to);
                sys::write(&to, &sys::read(&from)?)?;
            }
        }
        let ran: Vec<&str> = build
            .post_processors
            .lines()
            .filter_map(|line| line.strip_prefix("running "))
            .collect();
        eprintln!(
            "[unity-micro-editor] package assembly {} built for the host{}",
            assembly.name,
            match ran.is_empty() {
                true => String::new(),
                false => format!(", post-processed by {}", ran.join(", ")),
            }
        );
        compiler.current_dlls.remove(&assembly.name);
        // The host's own DLL of that name, where it had one, is replaced.
        let library = format!("{}.dll", assembly.name);
        let replaced: Vec<String> = compiler
            .manifest
            .compiler_inputs
            .keys()
            .filter(|path| {
                sysutil::base(path) == library && sysutil::base(&sysutil::dir(path)) == "Managed"
            })
            .cloned()
            .collect();
        for path in replaced {
            compiler.manifest.compiler_inputs.remove(&path);
        }
        rows.extend(build.initializers);
        let exported = graph
            .assemblies
            .iter()
            .find(|exported| exported.config.name == assembly.name)
            .cloned()
            .unwrap_or_default();
        let mut editor_defines = config.defines.clone();
        editor_defines.push("UNITY_EDITOR".into());
        editor_defines.push("UNITY_EDITOR_LINUX".into());
        graph.host_assemblies.push(CompilerHostAssembly {
            name: assembly.name.clone(),
            path: target.clone(),
            sha256: file_hash(&target)?,
            sources: assembly.sources.clone(),
            defines: config.defines.clone(),
            editor_defines,
            configuration: Some(Box::new(config.clone())),
            compiled_references: exported.compiled_references.clone(),
            response_files: exported.response_files.clone(),
        });
    }
    register_compiler_assembly_slots(slots_file, &packages)?;
    super::packages::add_native_initializers(
        &join(&[&sysutil::dir(managed), "RuntimeInitializeOnLoads.json"]),
        &rows,
    )
}

/// Puts the tool's runtime bridge, compiled from the installation's sources
/// with the SDK, in place of the one the host was built with, and its
/// `RuntimeInitializeOnLoad` methods in place of the host's rows for it. A
/// host then serves every revision of the tool; it only has to hold an
/// assembly of that name, which `ScriptingAssemblies.json` registers.
fn refresh_bridge(ctx: &Context, compiler: &Compiler, helper: &str, managed: &str) -> Result<()> {
    let target = join(&[managed, "UnityMicroEditor.dll"]);
    if sys::absent(&target) {
        bail!("the host Player holds no runtime bridge (UnityMicroEditor.dll)");
    }
    let dll = super::host::compile_bridge(
        ctx,
        &join(&[&compiler.project, "Library/UnityMicroEditor"]),
        &compiler.editor,
        managed,
    )?;
    // A staged copy of the host may be read-only.
    let _ = fs::remove_file(&target);
    sys::write(&target, &sys::read(&dll)?)?;
    let output = compiler_command(
        ctx,
        &compiler.cache,
        &compiler.dotnet,
        &[helper, "--runtime-initializers", &target],
    )
    .map_err(|error| anyhow!("runtime initializers of the bridge: {}", error.message))?;
    let rows: Vec<super::packages::InitializerRow> = serde_json::from_slice(&output)
        .map_err(|error| anyhow!("runtime initializers of the bridge: {error}"))?;
    super::packages::add_native_initializers(
        &join(&[&sysutil::dir(managed), "RuntimeInitializeOnLoads.json"]),
        &rows,
    )
}
