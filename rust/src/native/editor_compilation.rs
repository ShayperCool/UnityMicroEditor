//! An Editor compilation of a project made without the Editor: the
//! assemblies the Editor would compile for itself into
//! `Library/ScriptAssemblies`, compiled by the SDK's Roslyn with the Editor's
//! platform, defines and references, and the record of them in the form
//! Bee leaves (`Library/Bee/*E-inputdata.json`). The import worker runs the
//! Editor code of packages from it (shader build callbacks, scripted
//! importers, material inspectors), as it does from the compilation of a
//! staging Editor.
//!
//! The references follow what the pinned Editor gives its own compilation:
//! an assembly for the Editor platform alone compiles against the .NET
//! Framework 4.8 API and every engine and Editor module; any other against
//! the Player's API, the modules the project enables and every Editor
//! module. All get the scripting bootstrap, the Graphs assembly and the
//! extensions of the installed platforms.
use crate::{
    cancel::Context,
    compiler::{
        CompileAssembly, Compiler, NilVec, PlainCompile, deduplicate_references,
        order_compiler_assemblies,
    },
    packages::Snapshot,
    sha256,
    sourcegraph::{SourceEngineProfile, SourceGraphOptions, build_source_graph},
    sysutil,
};
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

/// Where the compilation is kept, relative to the project. It is the first
/// place the importers look for one.
pub const DIRECTORY: &str = "Library/UnityMicroEditor/editor-compilation";

/// The .NET Framework assemblies the Editor references for its own
/// assemblies, besides the facades.
const FRAMEWORK: [&str; 17] = [
    "mscorlib.dll",
    "System.dll",
    "System.Core.dll",
    "System.Runtime.Serialization.dll",
    "System.Xml.dll",
    "System.Xml.Linq.dll",
    "System.Numerics.dll",
    "System.Numerics.Vectors.dll",
    "System.Net.Http.dll",
    "System.IO.Compression.dll",
    "Microsoft.CSharp.dll",
    "System.Data.dll",
    "System.Data.DataSetExtensions.dll",
    "System.Drawing.dll",
    "System.IO.Compression.FileSystem.dll",
    "System.ComponentModel.Composition.dll",
    "System.Transactions.dll",
];

struct References {
    framework: Vec<String>,
    /// The BCL extensions' targeting pack, which Bee gives every assembly
    /// (`System.Runtime.CompilerServices.Unsafe` among them).
    extensions: Vec<String>,
    editor_modules: Vec<String>,
    runtime_modules: Vec<String>,
    extras: Vec<String>,
}

fn references(editor: &str, engine: &[String]) -> References {
    let data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
    let api = sysutil::join(&[&data, "UnityReferenceAssemblies/unity-4.8-api"]);
    let mut framework: Vec<String> = FRAMEWORK
        .iter()
        .map(|name| sysutil::join(&[&api, name]))
        .collect();
    framework.extend(crate::compiler::sys_glob(&sysutil::join(&[
        &api,
        "Facades/*.dll",
    ])));
    // Bee adds the BCL extensions' targeting pack to every assembly.
    let extensions = crate::compiler::sys_glob(&sysutil::join(&[
        &data,
        "BCLExtensions/TargetingPacks/netstandard2.1/ref/*.dll",
    ]));
    framework.extend(extensions.iter().cloned());
    let modules = sysutil::join(&[&data, "Managed/UnityEngine"]);
    let all: Vec<String> = crate::compiler::sys_glob(&sysutil::join(&[&modules, "UnityE*.dll"]))
        .into_iter()
        .filter(|path| sysutil::base(path) != "UnityEditor.MSBuild.dll")
        .collect();
    let mut runtime_modules: Vec<String> = all
        .iter()
        .filter(|path| sysutil::base(path).starts_with("UnityEditor"))
        .cloned()
        .collect();
    for path in engine {
        let name = sysutil::base(path);
        let editor_module = sysutil::join(&[&modules, &name]);
        if name.starts_with("UnityEngine") && std::path::Path::new(&editor_module).is_file() {
            runtime_modules.push(editor_module);
        }
    }
    let mut extras = vec![
        sysutil::join(&[&data, "Managed/Bootstrap/Unity.Scripting.dll"]),
        sysutil::join(&[&data, "Managed/UnityEditor.Graphs.dll"]),
    ];
    extras.extend(crate::compiler::sys_glob(&sysutil::join(&[
        &data,
        "PlaybackEngines/*/UnityEditor.*.Extensions.dll",
    ])));
    References {
        framework,
        extensions,
        editor_modules: all,
        runtime_modules,
        extras,
    }
}

/// Compiles the Editor's assemblies of `project` and replaces the kept
/// compilation with them. Returns the directory that now stands for the
/// project's `Library`, or the reason no compilation could be made.
pub fn ensure(
    ctx: &Context,
    project: &str,
    editor: &str,
    player: &str,
    profile: &SourceEngineProfile,
    packages: &Snapshot,
) -> Result<String> {
    let graph = build_source_graph(
        ctx,
        &SourceGraphOptions {
            project: project.into(),
            editor: editor.into(),
            player: player.into(),
            profile: Some(profile.clone()),
            packages: Some(packages.clone()),
            editor_domain: true,
        },
    )?;
    let mut compiler = Compiler {
        project: project.into(),
        editor: editor.into(),
        cache: sysutil::join(&[project, "Library/UnityMicroEditor/editor-compilation-cache"]),
        ..Default::default()
    };
    compiler.initialize_sdk()?;
    // The Editor's own analyzers mark the assemblies whose code-load
    // callbacks it runs; an Editor refuses such an assembly without them.
    let tools = sysutil::join(&[&sysutil::dir(editor), "Data/Tools/BuildPipeline"]);
    for analyzer in [
        "Unity.Analyzers.Common/Unity.Analyzers.Common.dll",
        "Unity.Analyzers.LongLiving/Unity.Analyzers.LongLiving.dll",
    ] {
        compiler.generators.push(sysutil::join(&[&tools, analyzer]));
    }
    let staging = sysutil::join(&[
        project,
        "Library/UnityMicroEditor/editor-compilation-staging",
    ]);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let known = references(editor, &graph.engine_references);
    // The compilation is laid out as the Editor's is, and compiled from
    // there: Unity's source generators record each script's path relative
    // to the project (`./Library/PackageCache/<name>@<hash>/...`), and the
    // import worker finds a script's class by that path.
    let library = sysutil::join(&[&staging, "library"]);
    let mut places: Vec<(String, String)> = Vec::new();
    let mut packages_record = Vec::new();
    for directory in [
        "Library/ScriptAssemblies",
        "Library/PackageCache",
        "Library/Bee",
    ] {
        std::fs::create_dir_all(sysutil::join(&[&library, directory]))?;
    }
    for root in graph.source_roots.iter() {
        let name = root.path.trim_start_matches("Packages/").to_string();
        let package = packages
            .packages
            .iter()
            .find(|package| package.logical_root == root.path);
        let immutable = package.is_some_and(|package| {
            matches!(package.source.as_str(), "registry" | "builtin" | "git")
        });
        // A package nobody edits lies in the cache, as the Editor places
        // it; a local or embedded one where it is.
        let resolved = match immutable {
            true => {
                let resolved = format!(
                    "Library/PackageCache/{}",
                    cache_name(&name, package.expect("immutable package"), &root.source)
                );
                std::os::unix::fs::symlink(&root.source, sysutil::join(&[&library, &resolved]))
                    .map_err(|error| anyhow!("place package {name}: {error}"))?;
                places.push((format!("{}/", root.source), format!("{resolved}/")));
                resolved
            }
            false => {
                places.push((format!("{}/", root.source), format!("{}/", root.source)));
                root.source.clone()
            }
        };
        packages_record.push(serde_json::json!({"Name": name, "ResolvedPath": resolved}));
    }
    std::os::unix::fs::symlink(
        sysutil::join(&[project, "Assets"]),
        sysutil::join(&[&library, "Assets"]),
    )?;
    places.push((format!("{project}/"), String::new()));
    let through_view = |source: &String| -> String {
        places
            .iter()
            .find_map(|(from, to)| {
                source
                    .strip_prefix(from.as_str())
                    .map(|rest| format!("{to}{rest}"))
            })
            .unwrap_or_else(|| source.clone())
    };
    let order = order_compiler_assemblies(
        &graph
            .assemblies
            .iter()
            .map(|assembly| CompileAssembly {
                name: assembly.config.name.clone(),
                sources: assembly.config.sources.clone(),
                references: assembly.config.dependencies.clone(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )?;
    let by_name: BTreeMap<&str, &crate::compiler::CompilerGraphAssembly> = graph
        .assemblies
        .iter()
        .map(|assembly| (assembly.config.name.as_str(), assembly))
        .collect();
    let mut built: BTreeMap<String, String> = BTreeMap::new();
    let mut left_out: BTreeMap<String, String> = BTreeMap::new();
    for assembly in order.iter() {
        let exported = by_name[assembly.name.as_str()];
        let config = &exported.config;
        if let Some(missing) = assembly
            .references
            .iter()
            .find(|dependency| left_out.contains_key(*dependency))
        {
            left_out.insert(assembly.name.clone(), format!("needs {missing}"));
            continue;
        }
        let mut refs: Vec<String> = match exported.editor_only {
            true => known.framework.clone(),
            false => graph
                .base_references
                .iter()
                .cloned()
                .chain(known.extensions.iter().cloned())
                .collect(),
        };
        if !config.config.no_engine_references {
            refs.extend(match exported.editor_only {
                true => known.editor_modules.clone(),
                false => known.runtime_modules.clone(),
            });
        }
        refs.extend(known.extras.iter().cloned());
        refs.extend(config.compiler_references.iter().cloned());
        for dependency in assembly.references.iter() {
            if let Some(dll) = built.get(dependency) {
                refs.push(dll.clone());
            }
        }
        let refs = deduplicate_references(&refs).to_vec();
        // As Bee compiles: the project's own directory is `.` in recorded paths.
        let mut options = config.compiler_options.to_vec();
        options.push(format!("-pathmap:{library}=."));
        match compiler.compile_plain(
            ctx,
            &staging,
            &PlainCompile {
                name: &assembly.name,
                sources: &config.sources.iter().map(through_view).collect::<Vec<_>>(),
                defines: &config.defines,
                compiler_options: &options,
                allow_unsafe: config.config.allow_unsafe,
                references: &refs,
                kind: "editor-compiles",
                project_view: &library,
            },
        ) {
            Ok(dll) => {
                built.insert(assembly.name.clone(), dll);
            }
            Err(error) => {
                ctx.err()?;
                let text = format!("{error:#}");
                let first = text
                    .lines()
                    .find(|line| line.contains("error CS"))
                    .or_else(|| text.lines().find(|line| !line.trim().is_empty()))
                    .unwrap_or("")
                    .to_string();
                left_out.insert(assembly.name.clone(), first);
            }
        }
    }
    for (name, reason) in &left_out {
        eprintln!("[unity-micro-editor] Editor assembly {name} is left out: {reason}");
    }
    publish(
        project,
        &graph,
        &library,
        packages_record,
        &order,
        &built,
        &staging,
    )
}

fn publish(
    project: &str,
    graph: &crate::compiler::CompilerGraphExport,
    library: &str,
    packages: Vec<serde_json::Value>,
    order: &NilVec<CompileAssembly>,
    built: &BTreeMap<String, String>,
    staging: &str,
) -> Result<String> {
    let assemblies_directory = sysutil::join(&[library, "Library/ScriptAssemblies"]);
    let roots: Vec<(String, String)> = graph
        .source_roots
        .iter()
        .map(|root| (root.source.clone(), root.path.clone()))
        .collect();
    let logical = |path: &str| -> String {
        for (source, logical) in &roots {
            if let Some(rest) = path.strip_prefix(&format!("{source}/")) {
                return format!("{logical}/{rest}");
            }
        }
        match path.strip_prefix(&format!("{project}/")) {
            Some(rest) => rest.to_string(),
            None => path.to_string(),
        }
    };
    let listed: Vec<&CompileAssembly> = order
        .iter()
        .filter(|assembly| built.contains_key(&assembly.name))
        .collect();
    let index: BTreeMap<&str, usize> = listed
        .iter()
        .enumerate()
        .map(|(position, assembly)| (assembly.name.as_str(), position))
        .collect();
    let mut records = Vec::new();
    for assembly in &listed {
        let dll = &built[&assembly.name];
        for (from, name) in [
            (dll.clone(), format!("{}.dll", assembly.name)),
            (
                format!("{}.pdb", dll.strip_suffix(".dll").unwrap_or(dll)),
                format!("{}.pdb", assembly.name),
            ),
        ] {
            std::fs::copy(&from, sysutil::join(&[&assemblies_directory, &name]))
                .map_err(|error| anyhow!("copy {from}: {error}"))?;
        }
        let exported = graph
            .assemblies
            .iter()
            .find(|exported| exported.config.name == assembly.name)
            .expect("compiled from the graph");
        records.push(serde_json::json!({
            "Name": assembly.name,
            "SourceFiles": assembly.sources.iter().map(|source| logical(source)).collect::<Vec<_>>(),
            "References": assembly
                .references
                .iter()
                .filter_map(|dependency| index.get(dependency.as_str()).copied())
                .collect::<Vec<_>>(),
            "PrebuiltReferences": exported
                .config
                .compiler_references
                .iter()
                .map(|reference| logical(reference))
                .collect::<Vec<_>>(),
            "Asmdef": asmdef(&assembly.name, &assembly.sources).map(|path| logical(&path)).unwrap_or_default(),
        }));
    }
    let record = serde_json::json!({
        "BeeBuildProgramCommon.Data.ConfigurationData": {"Packages": packages},
        "ScriptCompilationBuildProgram.Data.ScriptCompilationData": {
            "Assemblies": records,
            "OutputDirectory": "Library/ScriptAssemblies",
        },
    });
    std::fs::write(
        sysutil::join(&[library, "Library/Bee/UnityMicroEditorE-inputdata.json"]),
        serde_json::to_vec(&record)?,
    )?;
    let target = sysutil::join(&[project, DIRECTORY]);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(library, &target)
        .map_err(|error| anyhow!("publish the Editor compilation: {error}"))?;
    let _ = std::fs::remove_dir_all(staging);
    Ok(target)
}

/// The assembly definition an assembly is compiled from: the `.asmdef` of
/// that name in the nearest directory above one of its sources.
fn asmdef(name: &str, sources: &[String]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    for source in sources {
        let mut directory = sysutil::dir(source);
        while seen.insert(directory.clone()) {
            for candidate in crate::compiler::sys_glob(&sysutil::join(&[&directory, "*.asmdef"])) {
                let defines = std::fs::read(&candidate)
                    .ok()
                    .and_then(|content| crate::sourcegraph::decode_asmdef(&content).ok())
                    .is_some_and(|definition| definition.name == name);
                if defines {
                    return Some(candidate);
                }
            }
            let parent = sysutil::dir(&directory);
            if parent == directory {
                break;
            }
            directory = parent;
        }
    }
    None
}

/// The name the Editor gives a package's directory in `Library/PackageCache`,
/// which the import worker relies on: an engine module by its name alone, a
/// built-in package with the first twelve digits of the hash the
/// installation ships beside it (`<name>.sha1`), any other with twelve
/// digits of its content.
fn cache_name(name: &str, package: &crate::packages::Package, source: &str) -> String {
    if package.source == "builtin" && package.kind == "module" {
        return name.to_string();
    }
    if package.source == "builtin"
        && let Ok(hash) = std::fs::read_to_string(sysutil::join(&[
            &sysutil::dir(source),
            &format!("{name}.sha1"),
        ]))
        && hash.trim().len() >= 12
    {
        return format!("{name}@{}", &hash.trim()[..12]);
    }
    let digest = match package.content_hash.len() >= 12 {
        true => package.content_hash.clone(),
        false => sha256(source.as_bytes()),
    };
    format!("{name}@{}", &digest[..12])
}
