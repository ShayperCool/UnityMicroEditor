//! The standalone Netcode for GameObjects IL postprocessor.
use super::{
    CompileAssembly, Compiler, LifecycleProof, NilMap, NilVec, RuntimeInitializer,
    compile::{valid_lifecycle_proof, verify_cache_files},
    join,
    nilable::{nz, unmarshal},
    player_managed,
    sys::{self, compiler_command},
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, micro_editor_root, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct NetcodeProcessor {
    pub helper: String,
    pub inputs: NilMap<String>,
    pub key: String,
    pub files: NilMap<String>,
}

pub fn needs_netcode(assembly: &CompileAssembly) -> bool {
    assembly
        .references
        .iter()
        .any(|name| name == "Unity.Netcode.Runtime")
}

/// Keys are sorted.
#[derive(Serialize)]
struct Request<'a> {
    assembly: &'a str,
    defines: &'a NilVec<String>,
    #[serde(rename = "lifecycleMethods")]
    lifecycle_methods: &'a [String],
    output: &'a str,
    references: &'a NilVec<String>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Outcome {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    schema_version: i64,
    #[serde(deserialize_with = "nz")]
    verified: bool,
    #[serde(deserialize_with = "nz")]
    assembly: String,
    #[serde(deserialize_with = "nz")]
    required: bool,
    #[serde(rename = "lifecycleMethodsVerified", deserialize_with = "nz")]
    lifecycle_methods_verified: i64,
    #[serde(rename = "runtimeInitializers")]
    runtime_initializers: NilVec<RuntimeInitializer>,
}

const CODEGEN_ARTIFACTS: &str = "Library/Bee/artifacts/StandaloneLinux64_CodeGen";
/// Where a host build keeps the unstripped CodeGen reference next to its data.
pub const HOST_CODEGEN_DIRECTORY: &str = "UnityMicroEditorCodeGen";
const NETCODE_RUNTIME: &str = "Unity.Netcode.Runtime.dll";

/// The project roots whose `Library` may hold Unity's package and build
/// caches: the target, the staging copy a host was built in, the shared
/// staging copy, then every earlier host stage of this project, newest first.
fn library_roots(project: &str, player: &str) -> Vec<String> {
    let mut roots = vec![project.to_string()];
    // <stage>/Builds/UnityMicroEditor/<phase>/Project.x86_64
    let mut stage = player.to_string();
    for _ in 0..4 {
        stage = sysutil::dir(&stage);
    }
    if !sys::absent(&join(&[
        &stage,
        "ProjectSettings/UnityMicroEditorStaging.json",
    ])) {
        roots.push(stage);
    }
    let shared = join(&[project, "Library/UnityMicroEditor/stage/project"]);
    if !roots.contains(&shared) && !sys::absent(&shared) {
        roots.push(shared);
    }
    let mut stages: Vec<(std::time::SystemTime, String)> = sys::glob_lossy(&join(&[
        project,
        "Library/UnityMicroEditor/preparations/*/host-stage/project",
    ]))
    .into_iter()
    .filter_map(|stage| Some((std::fs::metadata(&stage).ok()?.modified().ok()?, stage)))
    .collect();
    stages.sort_by(|a, b| b.cmp(a));
    for (_, stage) in stages {
        if !roots.contains(&stage) {
            roots.push(stage);
        }
    }
    roots
}

/// The runtime Player linker removes InternalsVisibleTo, so the processor
/// build references the unstripped runtime of Unity's CodeGen step. A host
/// carries its own copy; older hosts leave it in the Library that built them.
fn unstripped_netcode_runtime(player: &str, roots: &[String]) -> Result<String> {
    let mut candidates = vec![join(&[
        &super::player_data(player),
        HOST_CODEGEN_DIRECTORY,
        NETCODE_RUNTIME,
    ])];
    for root in roots {
        candidates.push(join(&[root, CODEGEN_ARTIFACTS, NETCODE_RUNTIME]));
    }
    match candidates.iter().find(|path| !sys::absent(path)) {
        Some(found) => Ok(found.clone()),
        None => bail!(
            "unstripped {NETCODE_RUNTIME} is missing; a host build produces it under {CODEGEN_ARTIFACTS}. Searched:\n{}",
            candidates.join("\n")
        ),
    }
}

impl Compiler {
    fn netcode_ready(
        &mut self,
        entry: &str,
        key: &str,
        inputs: NilMap<String>,
        files: NilMap<String>,
    ) -> Result<NetcodeProcessor> {
        let mut identity = key.as_bytes().to_vec();
        identity.extend_from_slice(&marshal(&files)?);
        let processor = NetcodeProcessor {
            helper: join(&[entry, "Unity.Netcode.Editor.CodeGen.dll"]),
            inputs,
            files,
            key: sha256(&identity),
        };
        self.netcode = Some(processor.clone());
        Ok(processor)
    }

    /// Package sources are built unchanged without
    /// UNITY_EDITOR. The helper shares the package's assembly identity for its
    /// declared InternalsVisibleTo API.
    pub fn prepare_netcode_processor(&mut self, ctx: &Context) -> Result<NetcodeProcessor> {
        if let Some(processor) = &self.netcode {
            if !self
                .hashes
                .verify_files(&sysutil::dir(&processor.helper), &processor.files)
            {
                bail!("Netcode postprocessor cached executable or dependency changed");
            }
            for (filename, expected) in &processor.inputs {
                if !self
                    .hashes
                    .file_hash(filename)
                    .is_ok_and(|actual| actual == *expected)
                {
                    bail!("Netcode postprocessor input changed during session: {filename}");
                }
            }
            return Ok(processor.clone());
        }
        // The target's own Library holds these only after its Editor resolved
        // and built the project. The host was built in an owned staging copy,
        // whose Library always has them.
        let roots = library_roots(&self.project, &self.player);
        let (mut packages, mut cecil_packages) = (Vec::new(), Vec::new());
        for root in &roots {
            packages = sys::glob_lossy(&join(&[
                root,
                "Library/PackageCache/com.unity.netcode.gameobjects@*/Editor/CodeGen",
            ]));
            cecil_packages = sys::glob_lossy(&join(&[
                root,
                "Library/PackageCache/com.unity.nuget.mono-cecil@*",
            ]));
            if packages.len() == 1 && cecil_packages.len() == 1 {
                break;
            }
        }
        if packages.len() != 1 || cecil_packages.len() != 1 {
            bail!(
                "Netcode requires one pinned package CodeGen and Mono.Cecil package in the Library/PackageCache of the project or its host staging copy"
            );
        }
        let unstripped_runtime = unstripped_netcode_runtime(&self.player, &roots)?;
        let tool_root = micro_editor_root()?;
        let source_root = join(&[&tool_root, "tools/NetcodePostProcessor"]);
        let mut inputs = NilMap::make();
        let mut copies: BTreeMap<String, String> = BTreeMap::new();
        let mut add = |filename: String, destination: String| -> Result<()> {
            inputs.insert(filename.clone(), file_hash(&filename)?);
            copies.insert(filename, destination);
            Ok(())
        };
        for name in ["NetcodePostProcessor.csproj", "Program.cs"] {
            add(join(&[&source_root, name]), name.into())?;
        }
        let sources = sys::glob_lossy(&join(&[&packages[0], "*.cs"]));
        if sources.len() != 8 {
            bail!("unsupported pinned NGO CodeGen source layout");
        }
        for filename in sources {
            let destination = join(&["CodeGen", &sysutil::base(&filename)]);
            add(filename, destination)?;
        }
        for name in [
            "Mono.Cecil.dll",
            "Mono.Cecil.Rocks.dll",
            "Mono.Cecil.Pdb.dll",
            "Mono.Cecil.Mdb.dll",
        ] {
            add(
                join(&[&cecil_packages[0], name]),
                join(&["Dependencies", name]),
            )?;
        }
        let data = join(&[&sysutil::dir(&self.editor), "Data"]);
        let common = join(&[
            &data,
            "Tools/BuildPipeline/Compilation/Unity.ILPP.Runner/Unity.CompilationPipeline.Common.dll",
        ]);
        let destination = join(&["Dependencies", &sysutil::base(&common)]);
        add(common, destination)?;
        let managed = player_managed(&self.player);
        for name in [
            "Unity.Netcode.Runtime.dll",
            "Unity.Collections.dll",
            "UnityEngine.CoreModule.dll",
            "UnityEngine.ScriptingModule.dll",
        ] {
            let filename = if name == "Unity.Netcode.Runtime.dll" {
                // The runtime Player linker removes InternalsVisibleTo. The
                // package CodeGen build uses its unstripped runtime reference;
                // target ILPP still resolves the actual Player DLLs from request.
                unstripped_runtime.clone()
            } else {
                join(&[&managed, name])
            };
            add(filename, join(&["Dependencies", name]))?;
        }
        for filename in [&self.dotnet, &self.csc] {
            inputs.insert(filename, file_hash(filename)?);
        }
        // Conditional package APIs depend on package version, not on the user's
        // assemblies. Keep this helper's compile mode pinned and in its cache key.
        let mut cache_inputs: BTreeMap<String, String> = BTreeMap::new();
        cache_inputs.insert("mode:netcode-runtime-v1".into(), "!UNITY_EDITOR".into());
        for (filename, hash) in &inputs {
            let name = copies
                .get(filename)
                .filter(|name| !name.is_empty())
                .unwrap_or(filename);
            cache_inputs.insert(name.clone(), hash.clone());
        }
        let key = sha256(&marshal(&cache_inputs)?);
        let entry = join(&[&self.cache, "netcode-processor", &key]);
        if let Ok(files) = read_json_file::<NilMap<String>>(&join(&[&entry, "files.json"]))
            && !files.at("Unity.Netcode.Editor.CodeGen.dll").is_empty()
            && verify_cache_files(&entry, &files)
        {
            return self.netcode_ready(&entry, &key, inputs, files);
        }
        sys::mkdir_all(&sysutil::dir(&entry))?;
        let staging = sys::mkdir_temp(&sysutil::dir(&entry), ".processor-build-")?;
        for (source, relative) in &copies {
            let payload = match sys::read(source) {
                Ok(payload) if sha256(&payload) == inputs.at(source) => payload,
                _ => bail!("Netcode processor input changed during build: {source}"),
            };
            let destination = join(&[&staging, relative]);
            sys::mkdir_all(&sysutil::dir(&destination))?;
            sys::write(&destination, &payload)?;
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
                &join(&[&staging, "NetcodePostProcessor.csproj"]),
                "--nologo",
                "-v:q",
                "-o",
                &payload,
                "-p:NuGetAudit=false",
                sys::BUILD_WITHOUT_SERVERS[0],
                sys::BUILD_WITHOUT_SERVERS[1],
            ],
        )
        .map_err(|error| anyhow!("build standalone NGO postprocessor: {error}"))?;
        let mut files = NilMap::make();
        for name in sys::regular_files(&payload)? {
            let hash = file_hash(&join(&[&payload, &name]))?;
            files.insert(name, hash);
        }
        if files.at("Unity.Netcode.Editor.CodeGen.dll").is_empty() {
            bail!("Netcode processor build produced no executable");
        }
        atomic_json(&join(&[&payload, "files.json"]), &files)?;
        sys::publish(&payload, &entry, || {
            read_json_file::<NilMap<String>>(&join(&[&entry, "files.json"])).is_ok_and(|files| {
                !files.at("Unity.Netcode.Editor.CodeGen.dll").is_empty()
                    && verify_cache_files(&entry, &files)
            })
        })?;
        // The entry another process published meanwhile lists its own files.
        let files = read_json_file::<NilMap<String>>(&join(&[&entry, "files.json"]))
            .map_err(|error| anyhow!("read the published Netcode processor: {error}"))?;
        self.netcode_ready(&entry, &key, inputs, files)
    }

    /// Runs the package's own IL postprocessors on a
    /// freshly compiled assembly and extends its lifecycle proof.
    pub fn postprocess_netcode(
        &mut self,
        ctx: &Context,
        folder: &str,
        dll: &str,
        assembly: &CompileAssembly,
        refs: &NilVec<String>,
        mut proof: LifecycleProof,
    ) -> Result<(String, LifecycleProof)> {
        let processor = self.prepare_netcode_processor(ctx)?;
        let processed = join(&[folder, "Netcode", &format!("{}.dll", assembly.name)]);
        sys::mkdir_all(&sysutil::dir(&processed))?;
        let mut lifecycle_methods: Vec<String> = proof
            .methods
            .iter()
            .map(|method| format!("{}.{}", method.type_name, method.phase))
            .chain(
                proof
                    .runtime_initializers
                    .iter()
                    .map(|method| format!("{}.{}", method.type_name, method.method_name)),
            )
            .collect();
        lifecycle_methods.sort();
        let request = join(&[folder, "netcode-request.json"]);
        atomic_json(
            &request,
            &Request {
                assembly: dll,
                defines: &self.configs.value(&assembly.name).defines,
                lifecycle_methods: &lifecycle_methods,
                output: &processed,
                references: refs,
            },
        )?;
        let output = compiler_command(ctx, folder, &self.dotnet, &[&processor.helper, &request])
            .map_err(|error| anyhow!("NGO IL postprocess {}: {error}", assembly.name))?;
        let result = match unmarshal::<Outcome>(&output) {
            Ok(result)
                if result.schema_version == 1
                    && result.verified
                    && result.assembly == assembly.name
                    && result.lifecycle_methods_verified == lifecycle_methods.len() as i64
                    && result.runtime_initializers.len() == usize::from(result.required) =>
            {
                result
            }
            _ => bail!("NGO postprocessor returned invalid verification proof"),
        };
        proof
            .runtime_initializers
            .append(result.runtime_initializers.into_vec());
        if !valid_lifecycle_proof(&proof, &assembly.name) {
            bail!("NGO runtime serialization initializer proof is invalid");
        }
        sys::write(&join(&[folder, "netcode-proof.json"]), &output)?;
        Ok((processed, proof))
    }
}
