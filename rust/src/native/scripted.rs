//! Importers written in C#: what a standalone worker needs to run the
//! `ScriptedImporter` of a package.
//!
//! The worker has no compiler. It loads the assemblies it finds in its
//! context's `Library/ScriptAssemblies`, and it gives an imported object a
//! `MonoScript` only if the source registry lists the class's script as a
//! script compilation asset at the path the assembly records for it. Those
//! paths are the ones the Editor compiled with, relative to the project, so a
//! package has to lie in the context where it lies in that project.
//!
//! The assemblies, the list of their sources and the package locations come
//! from an Editor compilation of this project: the staging clone a
//! preparation leaves behind, or the project's own `Library`. The Editor
//! records all three in `Library/Bee/*-inputdata.json`.
use super::support::{eval_symlinks, read_file, stat, wrap};
use crate::{registry::meta_guid, sha256, sysutil, util::within_directory};
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const BEE_LIMIT: i64 = 256 * 1024 * 1024;
const SCRIPT_LIMIT: usize = 3500;

/// The compiled code of the importers of one batch and where its sources lie.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptEnvironment {
    /// The project whose `Library` holds the assemblies.
    pub library: String,
    /// The assemblies to load, by name, in name order.
    pub assemblies: Vec<String>,
    /// The scripts and assembly definitions they were compiled from, by
    /// logical path, in path order.
    pub scripts: Vec<String>,
    /// `Packages/<name>` to the directory of the package, relative to the
    /// project.
    pub roots: BTreeMap<String, String>,
    /// The assemblies shipped compiled that those name, by their place
    /// relative to the project: of immutable packages for importers and
    /// inspectors, of any place for a build. The worker's runtime finds one
    /// that lies next to the assembly that names it.
    pub plugins: Vec<String>,
    /// Identifies the assemblies' contents.
    pub sha256: String,
}

impl ScriptEnvironment {
    /// Where a logical path lies relative to the project.
    pub fn physical(&self, logical: &str) -> String {
        for (root, directory) in &self.roots {
            if let Some(rest) = logical.strip_prefix(root.as_str())
                && (rest.is_empty() || rest.starts_with('/'))
            {
                return format!("{directory}{rest}");
            }
        }
        logical.to_string()
    }
    pub fn assembly_path(&self, name: &str) -> String {
        sysutil::join(&[
            &self.library,
            "Library/ScriptAssemblies",
            &format!("{name}.dll"),
        ])
    }
}

/// The GUID of the importer's script when a `.meta` selects a
/// `ScriptedImporter`.
pub fn scripted_importer(meta: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(meta).ok()?;
    let mut scripted = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            scripted = line.trim_end() == "ScriptedImporter:";
            continue;
        }
        if !scripted {
            continue;
        }
        let Some(rest) = line.strip_prefix("  script: {fileID: 11500000, guid: ") else {
            continue;
        };
        let guid = rest.strip_suffix(", type: 3}")?;
        return super::valid_native_guid(guid).then(|| guid.to_string());
    }
    None
}

/// The models whose importer makes a material of its own and names its
/// shader, by lower-case extension with its dot.
pub fn generated_material_profile(extension: &str) -> bool {
    matches!(extension, ".obj" | ".dae" | ".blend")
}

/// The program the model importer converts a `.blend` with. It starts
/// `blender -b <file> -P <Editor>/Data/Tools/Unity-BlenderToFBX.py -- <fbx>`
/// through the `PATH` of its process, in the Editor as in a worker, and
/// imports the FBX that the Editor's script makes Blender export. Without
/// Blender the artifact holds an import log and no model.
pub fn require_blender() -> Result<()> {
    let found = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            let candidate = directory.join("blender");
            // SAFETY: the path is a valid C string for the call.
            std::ffi::CString::new(candidate.as_os_str().as_encoded_bytes())
                .is_ok_and(|name| unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0)
                && candidate.is_file()
        })
    });
    if !found {
        bail!(
            "a .blend model is converted by Blender, which Unity's importer starts as `blender` from PATH; no such program was found"
        );
    }
    Ok(())
}

/// The Editor's table of shaders by name, `Library/ScriptMapper`.
///
/// A worker fills the table with the built-in shaders when it starts and
/// then replaces it with the one its source database holds. With none there
/// the table ends up empty, `Standard` is not found, and a model importer
/// that has to make a material copies the built-in default material under
/// another name and local ID than the Editor's.
pub fn shader_name_registry(project: &str) -> Result<Vec<u8>> {
    let mut failure = None;
    for library in libraries(project) {
        let path = sysutil::join(&[&library, "Library/ScriptMapper"]);
        match stat(&path) {
            Ok(info) if info.is_regular() && info.size > 0 && info.size <= 16 << 20 => {
                return read_file(&path);
            }
            Ok(_) => failure = failure.or(Some(anyhow!("{path} is not a bounded file"))),
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    Err(wrap(
        "a model whose importer makes its own material needs the shader name table of an Editor run of the project, which a preparation that builds its host leaves in the staging clone",
        failure.unwrap(),
    ))
}

/// The source formats a package's `ScriptedImporter` is verified for, by
/// lower-case extension with its dot.
pub fn scripted_profile(extension: &str) -> bool {
    extension == ".inputactions"
}

/// The projects whose `Library` may hold an Editor compilation of `project`,
/// the tool's own staging clone first.
fn libraries(project: &str) -> [String; 2] {
    [
        sysutil::join(&[project, "Library/UnityMicroEditor/stage/project"]),
        project.to_string(),
    ]
}

/// The Editor's record of its last compilation into `Library/ScriptAssemblies`.
fn compilation_record(library: &str) -> Result<String> {
    let directory = sysutil::join(&[library, "Library/Bee"]);
    let entries = std::fs::read_dir(&directory)
        .map_err(|error| anyhow!("no Editor compilation record in {directory}: {error}"))?;
    let mut newest: Option<((i64, i64), String)> = None;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // A Player build writes records of the same shape for its own output.
        if !name.ends_with("E-inputdata.json") {
            continue;
        }
        let path = sysutil::join(&[&directory, &name]);
        let Ok(info) = stat(&path) else { continue };
        if !info.is_regular() || info.size > BEE_LIMIT {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|(modified, _)| info.modified > *modified)
        {
            newest = Some((info.modified, path));
        }
    }
    newest
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow!("no Editor compilation record in {directory}"))
}

/// Changes whenever the Editor compiled again. Cheap enough for a cache key.
pub fn script_environment_identity(project: &str) -> String {
    for library in libraries(project) {
        if let Ok(record) = compilation_record(&library)
            && let Ok(data) = read_file(&record)
        {
            return sha256(&data);
        }
    }
    String::new()
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "BeeBuildProgramCommon.Data.ConfigurationData")]
    configuration: Configuration,
    #[serde(rename = "ScriptCompilationBuildProgram.Data.ScriptCompilationData")]
    compilation: Compilation,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Configuration {
    #[serde(default)]
    packages: Vec<Package>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Package {
    name: String,
    resolved_path: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Compilation {
    assemblies: Vec<Assembly>,
    #[serde(default)]
    output_directory: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Assembly {
    name: String,
    #[serde(default)]
    source_files: Vec<String>,
    #[serde(default)]
    references: Vec<usize>,
    #[serde(default)]
    prebuilt_references: Vec<String>,
    #[serde(default)]
    asmdef: String,
}

/// Whether compiled code names the assembly at `path`. Metadata holds the
/// name of every referenced assembly as a string of its own; a namespace of
/// the same spelling also counts, which errs towards refusing.
fn names_assembly(code: &[u8], path: &str) -> bool {
    let file = sysutil::base(path);
    let name = file.strip_suffix(".dll").unwrap_or(&file);
    let mut needle = Vec::with_capacity(name.len() + 2);
    needle.push(0);
    needle.extend_from_slice(name.as_bytes());
    needle.push(0);
    !name.is_empty() && code.windows(needle.len()).any(|window| window == needle)
}

/// The package root `Packages/<name>` of a logical path.
fn package_root(logical: &str) -> Option<&str> {
    let rest = logical.strip_prefix("Packages/")?;
    let name = rest.split('/').next().filter(|name| !name.is_empty())?;
    Some(&logical[..9 + name.len()])
}

/// The compiled code a worker run needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptSelection {
    /// The GUIDs of scripts whose assemblies are loaded.
    pub scripts: BTreeSet<String>,
    /// File names of package scripts whose assemblies are loaded, such as
    /// `LitShader.cs` for a class of that name.
    pub files: BTreeSet<String>,
    /// Whether the precompiled assemblies those assemblies name are put
    /// next to them instead of the run being refused.
    pub plugins: bool,
}

/// What an environment is read for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// The importers and inspectors of some assets.
    Importers,
    /// A build of Player objects: the classes of the given scripts, and
    /// every class a build calls for a shader.
    Build,
}

/// The interfaces of the callbacks a build runs on shaders. An assembly
/// that implements one names it.
const SHADER_CALLBACKS: [&str; 2] = ["IPreprocessShaders", "IPreprocessComputeShaders"];

/// The environment in which the selected scripts run, or why this project
/// has none.
pub fn selected_environment(
    project: &str,
    selection: &ScriptSelection,
) -> Result<ScriptEnvironment> {
    environment(project, selection, Purpose::Importers)
}

/// The environment in which the Editor's build code writes Player objects:
/// the assemblies of the given scripts (the classes of the settings a build
/// reads, the importers of what it writes), every assembly that defines a
/// shader callback of a build, and what those refer to, the assemblies
/// packages ship compiled included.
pub fn build_environment(project: &str, scripts: &BTreeSet<String>) -> Result<ScriptEnvironment> {
    let selection = ScriptSelection {
        scripts: scripts.clone(),
        ..Default::default()
    };
    environment(project, &selection, Purpose::Build)
}

/// Where the Editor compilation of a project found each package, as
/// `Packages/<name>` to its directory, and the project whose `Library` holds
/// that compilation.
pub fn package_directories(project: &str) -> Result<(String, BTreeMap<String, String>)> {
    let mut failure = None;
    for library in libraries(project) {
        let read = eval_symlinks(&library).and_then(|library| {
            let (_, record) = read_record(&library)?;
            Ok((library, package_map(&record)))
        });
        match read {
            Ok(found) => return Ok(found),
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    Err(wrap(
        "the packages of a project are found through an Editor compilation of it, which a preparation that builds its host leaves in the staging clone",
        failure.unwrap(),
    ))
}

fn read_record(library: &str) -> Result<(String, Record)> {
    let record_path = compilation_record(library)?;
    let record: Record = serde_json::from_slice(&read_file(&record_path)?)
        .map_err(|error| anyhow!("Editor compilation record {record_path}: {error}"))?;
    if record.compilation.output_directory != "Library/ScriptAssemblies" {
        bail!("Editor compilation record {record_path} is not of the script assemblies");
    }
    Ok((record_path, record))
}

fn package_map(record: &Record) -> BTreeMap<String, String> {
    record
        .configuration
        .packages
        .iter()
        .map(|package| {
            (
                format!("Packages/{}", package.name),
                package.resolved_path.trim_end_matches('/').to_string(),
            )
        })
        .collect()
}

fn environment(
    project: &str,
    selection: &ScriptSelection,
    purpose: Purpose,
) -> Result<ScriptEnvironment> {
    let mut failure: Option<anyhow::Error> = None;
    for library in libraries(project) {
        match read_environment(&library, selection, purpose) {
            Ok(environment) => return Ok(environment),
            Err(error) => {
                failure = match (purpose, failure) {
                    // A project that has no staging clone fails by its own
                    // Library.
                    (Purpose::Importers, Some(first)) if !super::is_not_exist(&first) => {
                        Some(first)
                    }
                    (Purpose::Importers, _) => Some(error),
                    // The staging clone is tried first; what the project's
                    // own `Library` lacks is the more telling reason.
                    (Purpose::Build, Some(first)) => Some(anyhow!("{first:#}; {error:#}")),
                    (Purpose::Build, None) => Some(error),
                }
            }
        }
    }
    Err(wrap(
        match purpose {
            Purpose::Importers => {
                "a scripted importer runs on the script assemblies of an Editor compilation of the project, which a preparation that builds its host leaves in the staging clone"
            }
            Purpose::Build => {
                "the Editor's build code runs on the script assemblies of an Editor compilation of the project, which a preparation that builds its host leaves in the staging clone"
            }
        },
        failure.unwrap(),
    ))
}

fn read_environment(
    library: &str,
    selection: &ScriptSelection,
    purpose: Purpose,
) -> Result<ScriptEnvironment> {
    let importers = &selection.scripts;
    let library = eval_symlinks(library)?;
    let (record_path, record) = read_record(&library)?;
    let packages = package_map(&record);
    let assemblies = &record.compilation.assemblies;
    // The assembly of each importer: the one compiled from its script.
    let mut wanted: BTreeSet<&str> = importers.iter().map(String::as_str).collect();
    let mut selected = BTreeSet::new();
    let mut files: BTreeSet<&str> = selection.files.iter().map(String::as_str).collect();
    for (index, assembly) in assemblies.iter().enumerate() {
        for source in &assembly.source_files {
            if package_root(source).is_some_and(|root| packages.contains_key(root))
                && files.remove(sysutil::base(source).as_str())
            {
                selected.insert(index);
            }
        }
    }
    if let Some(missing) = files.first() {
        bail!("no compiled package assembly is made from a script {missing}");
    }
    'search: for (index, assembly) in assemblies.iter().enumerate() {
        if wanted.is_empty() {
            break;
        }
        for source in &assembly.source_files {
            let Some(directory) = package_root(source).and_then(|root| {
                packages
                    .get(root)
                    .map(|directory| format!("{directory}{}", &source[root.len()..]))
            }) else {
                continue;
            };
            let Ok(meta) = read_file(&sysutil::join(&[&library, &format!("{directory}.meta")]))
            else {
                continue;
            };
            if let Ok(guid) = meta_guid(&meta)
                && wanted.remove(guid.as_str())
            {
                selected.insert(index);
                if wanted.is_empty() {
                    break 'search;
                }
            }
        }
    }
    if let Some(missing) = wanted.first() {
        bail!("no compiled package assembly defines the importer script {missing}");
    }
    if purpose == Purpose::Build {
        for (index, assembly) in assemblies.iter().enumerate() {
            if selected.contains(&index) || assembly.name.contains(['/', '\\', '\0']) {
                continue;
            }
            // An assembly the compilation lists but did not leave behind
            // defines nothing a worker could load.
            let Ok(code) = read_file(&sysutil::join(&[
                &library,
                "Library/ScriptAssemblies",
                &format!("{}.dll", assembly.name),
            ])) else {
                continue;
            };
            if SHADER_CALLBACKS
                .iter()
                .any(|name| names_assembly(&code, name))
            {
                selected.insert(index);
            }
        }
    }
    let mut pending: Vec<usize> = selected.iter().copied().collect();
    while let Some(index) = pending.pop() {
        for &reference in &assemblies[index].references {
            if reference >= assemblies.len() {
                bail!("Editor compilation record {record_path} refers to an unknown assembly");
            }
            if selected.insert(reference) {
                pending.push(reference);
            }
        }
    }
    let mut environment = ScriptEnvironment {
        library: library.clone(),
        ..Default::default()
    };
    let mut scripts = BTreeSet::new();
    let mut plugins = BTreeSet::new();
    let mut identity = String::from("script-environment-v1\0");
    for &index in &selected {
        let assembly = &assemblies[index];
        if assembly.name.is_empty() || assembly.name.contains(['/', '\\', '\0']) {
            bail!("Editor compilation record {record_path} names an invalid assembly");
        }
        let dll = environment.assembly_path(&assembly.name);
        let data = read_file(&dll)
            .map_err(|error| wrap(&format!("importer assembly {}", assembly.name), error))?;
        // An assembly shipped compiled with a package or the project is
        // found by the Editor through the asset database, which a standalone
        // worker does not have. Every assembly is compiled against all of
        // those that are referenced automatically; one it really uses is
        // named in its metadata.
        for plugin in assembly
            .prebuilt_references
            .iter()
            .filter(|path| !sysutil::is_abs(path) || within_directory(path, &library))
            .filter(|path| names_assembly(&data, path))
        {
            // A build takes one from wherever the compilation found it;
            // anything else only from an immutable package, and only when
            // asked to.
            let directory = package_root(plugin)
                .and_then(|root| packages.get(root).map(|directory| (root, directory)));
            let place = match (purpose, directory) {
                (Purpose::Build, Some((root, directory))) => {
                    format!("{directory}{}", &plugin[root.len()..])
                }
                (Purpose::Build, None) => plugin.clone(),
                (Purpose::Importers, Some((root, directory)))
                    if selection.plugins && directory.starts_with("Library/PackageCache/") =>
                {
                    format!("{directory}{}", &plugin[root.len()..])
                }
                (Purpose::Importers, _) => bail!(
                    "importer assembly {} needs the precompiled assembly {plugin}, which a standalone worker does not load",
                    assembly.name
                ),
            };
            plugins.insert(place);
        }
        for source in assembly
            .source_files
            .iter()
            .chain((!assembly.asmdef.is_empty()).then_some(&assembly.asmdef))
        {
            let directory = package_root(source).and_then(|root| {
                packages
                    .get(root)
                    .filter(|directory| directory.starts_with("Library/PackageCache/"))
                    .map(|directory| (root, directory))
            });
            let Some((root, directory)) = directory else {
                bail!(
                    "importer assembly {} is compiled from {source}, which is not in an immutable package",
                    assembly.name
                );
            };
            environment
                .roots
                .insert(root.to_string(), directory.to_string());
            scripts.insert(source.clone());
        }
        identity.push_str(&format!("{}\0{}\0", assembly.name, sha256(&data)));
        environment.assemblies.push(assembly.name.clone());
    }
    if scripts.len() > SCRIPT_LIMIT {
        bail!("importer assemblies are compiled from more scripts than one import context holds");
    }
    // A build reads files of any package: what a shader includes, what the
    // settings refer to.
    if purpose == Purpose::Build {
        for (root, directory) in &packages {
            if directory.starts_with("Library/PackageCache/") {
                environment.roots.insert(root.clone(), directory.clone());
            }
        }
    }
    environment.assemblies.sort();
    environment.scripts = scripts.into_iter().collect();
    environment.plugins = plugins.into_iter().collect();
    for (root, directory) in &environment.roots {
        identity.push_str(&format!("{root}\0{directory}\0"));
    }
    for plugin in &environment.plugins {
        let data = read_file(&sysutil::join(&[&library, plugin]))
            .map_err(|error| wrap(&format!("precompiled assembly {plugin}"), error))?;
        identity.push_str(&format!("plugin\0{plugin}\0{}\0", sha256(&data)));
    }
    environment.sha256 = sha256(identity.as_bytes());
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_importer_script_of_a_scripted_meta() {
        let meta = b"fileFormatVersion: 2\nguid: ffb4a475822727cc2b0c4490e726e878\nScriptedImporter:\n  internalIDToNameTable: []\n  script: {fileID: 11500000, guid: 8404be70184654265930450def6a9037, type: 3}\n  generateWrapperCode: 0\n";
        assert_eq!(
            scripted_importer(meta).as_deref(),
            Some("8404be70184654265930450def6a9037")
        );
        assert_eq!(
            scripted_importer(b"fileFormatVersion: 2\nguid: ffb4a475822727cc2b0c4490e726e878\n"),
            None
        );
        // A script named by another importer's settings is not an importer.
        assert_eq!(
            scripted_importer(
                b"guid: ffb4a475822727cc2b0c4490e726e878\nMonoImporter:\n  script: {fileID: 11500000, guid: 8404be70184654265930450def6a9037, type: 3}\n"
            ),
            None
        );
    }

    #[test]
    fn finds_a_referenced_assembly_by_its_name_in_metadata() {
        let code = b"\0System\0Unity.Collections\0InputAction\0";
        assert!(names_assembly(code, "Packages/x/Unity.Collections.dll"));
        assert!(!names_assembly(
            code,
            "Packages/x/Unity.Collections.LowLevel.ILSupport.dll"
        ));
        assert!(!names_assembly(code, "Packages/x/Collections.dll"));
    }

    #[test]
    fn places_package_paths_where_the_editor_compiled_them() {
        let environment = ScriptEnvironment {
            roots: BTreeMap::from([(
                "Packages/com.unity.inputsystem".to_string(),
                "Library/PackageCache/com.unity.inputsystem@7a4e1a2a8194".to_string(),
            )]),
            ..Default::default()
        };
        assert_eq!(
            environment.physical("Packages/com.unity.inputsystem/InputSystem/A.cs"),
            "Library/PackageCache/com.unity.inputsystem@7a4e1a2a8194/InputSystem/A.cs"
        );
        assert_eq!(
            environment.physical("Packages/com.unity.inputsystem.extra/A.cs"),
            "Packages/com.unity.inputsystem.extra/A.cs"
        );
        assert_eq!(environment.physical("Assets/A.cs"), "Assets/A.cs");
    }
}
