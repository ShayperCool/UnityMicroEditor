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
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

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
    /// The package (`Packages/<name>`) each assembly is compiled from, by
    /// assembly name.
    pub assembly_roots: BTreeMap<String, String>,
    /// Identifies the assemblies' contents.
    pub sha256: String,
    /// The hash of each assembly's contents, by assembly name, and of each
    /// precompiled one, by its place: what an import that needs one of
    /// their classes was made with.
    pub assembly_sha256: BTreeMap<String, String>,
    /// The assembly each script is compiled into, by logical path.
    pub script_assemblies: BTreeMap<String, String>,
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

/// The Editor's register of scripts, `Library/MonoManager.asset`, listing
/// the engine's own scripts, which lie in its `unity default resources`.
///
/// The main Editor keeps the register in the source database, and a worker
/// takes it from there. An importer's `AddObjectToAsset` takes an object of
/// a script class only when the class's script is registered or listed as a
/// compilation asset in the source registry; with no register, the objects
/// of the engine's script classes are refused ("Check that the definition is
/// in a file of the same name"): UI Toolkit's `VisualTreeAsset` and
/// `StyleSheet`, `PanelSettings`, `Tile`. The Editor's register also lists
/// the project's scripts, which the worker finds through the source registry
/// without it.
pub fn script_register(editor: &str) -> Result<Arc<Vec<u8>>> {
    static KNOWN: Mutex<BTreeMap<String, Arc<Vec<u8>>>> = Mutex::new(BTreeMap::new());
    let mut known = KNOWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(register) = known.get(editor) {
        return Ok(register.clone());
    }
    let path = sysutil::join(&[
        &sysutil::dir(editor),
        "Data/Resources/unity default resources",
    ]);
    let file = read_file(&path)?;
    let scripts = crate::playerbake::read_serialized_of_class(&file, 115)
        .map_err(|error| wrap(&format!("read the scripts of {path}"), error))?;
    if scripts.is_empty() {
        bail!("{path} holds no scripts");
    }
    let mut text = String::from(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!116 &1\nMonoManager:\n  m_ObjectHideFlags: 0\n  serializedVersion: 3\n  m_ScriptHashes: {}\n  m_RuntimeClassHashes: {}\n  m_Scripts:\n",
    );
    for script in &scripts {
        text.push_str(&format!(
            "  - {{fileID: {}, guid: 0000000000000000e000000000000000, type: 0}}\n",
            script.local_id
        ));
    }
    let register = Arc::new(text.into_bytes());
    known.insert(editor.to_string(), register.clone());
    Ok(register)
}

/// The projects whose `Library` may hold an Editor compilation of `project`:
/// the one the tool compiled without an Editor, the tool's own staging clone,
/// the project itself.
fn libraries(project: &str) -> [String; 3] {
    [
        sysutil::join(&[project, super::editor_compilation::DIRECTORY]),
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
/// The record names the checkout it was made in, and the socket of one
/// Editor run: they are left out, so that two checkouts of the same sources
/// (a worktree and the one it was copied from) name one environment.
pub fn script_environment_identity(project: &str) -> String {
    record_data(project).map_or_else(String::new, |(library, data)| {
        sha256(&location_free_record(&data, &library, project))
    })
}

/// The identity before it left out the checkout: the key under which the
/// caches of earlier versions hold what they made.
pub fn legacy_script_environment_identity(project: &str) -> String {
    record_data(project).map_or_else(String::new, |(_, data)| sha256(&data))
}

fn record_data(project: &str) -> Option<(String, Vec<u8>)> {
    libraries(project).into_iter().find_map(|library| {
        let data = read_file(&compilation_record(&library).ok()?).ok()?;
        Some((library, data))
    })
}

/// The record with the places of its checkout written as placeholders, and
/// the name of the Editor run's post-processing socket left out.
fn location_free_record(data: &[u8], library: &str, project: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(data)
        .replace(library, "$LIBRARY")
        .replace(project, "$PROJECT");
    let socket = regex::Regex::new(r"/tmp/ilpp\.sock-[0-9a-f]+").expect("socket pattern");
    socket
        .replace_all(&text, "$$SOCKET")
        .into_owned()
        .into_bytes()
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
    /// Full names of the classes UXML documents create, which the importer
    /// writes the serialized data of; their assemblies are loaded, those of
    /// the project too.
    pub elements: BTreeSet<String>,
    /// Whether the scripts may be the project's own, and an assembly of a
    /// package the project holds in place is loaded without its sources:
    /// the classes of a scene's scripts.
    pub all: bool,
    /// Of the scripts, the scripted importers: every script of their
    /// assemblies and of those they refer to is an asset, since an
    /// importer adds objects of any of their classes.
    pub importers: BTreeSet<String>,
}

/// The full names of the custom elements a UXML document creates: each tag
/// in the namespace its prefix or the default namespace gives, but those of
/// UI Toolkit itself and the document's own markup.
pub fn uxml_elements(text: &[u8]) -> BTreeSet<String> {
    static NAMESPACE: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"\bxmlns(?::([A-Za-z_][\w.-]*))?\s*=\s*"([^"]*)""#).unwrap()
    });
    static TAG: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r"<([A-Za-z_][\w.-]*)(?::([A-Za-z_][\w.-]*))?[\s/>]").unwrap()
    });
    let mut prefixes = BTreeMap::new();
    for capture in NAMESPACE.captures_iter(text) {
        let prefix = capture
            .get(1)
            .map(|prefix| String::from_utf8_lossy(prefix.as_bytes()).into_owned())
            .unwrap_or_default();
        prefixes.insert(prefix, String::from_utf8_lossy(&capture[2]).into_owned());
    }
    let mut found = BTreeSet::new();
    for capture in TAG.captures_iter(text) {
        let first = String::from_utf8_lossy(&capture[1]).into_owned();
        let (namespace, name) = match capture.get(2) {
            Some(name) => (
                prefixes.get(&first).cloned().unwrap_or_default(),
                String::from_utf8_lossy(name.as_bytes()).into_owned(),
            ),
            None => (prefixes.get("").cloned().unwrap_or_default(), first),
        };
        // The document's own markup, by its local name in any namespace.
        if matches!(
            name.as_str(),
            "UXML" | "Style" | "Template" | "Instance" | "AttributeOverrides"
        ) {
            continue;
        }
        let full = match (namespace.is_empty(), name.contains('.')) {
            (_, true) | (true, false) => name,
            (false, false) => format!("{namespace}.{name}"),
        };
        if full.contains('.')
            && !full.starts_with("UnityEngine.UIElements.")
            && !full.starts_with("UnityEditor.UIElements.")
        {
            found.insert(full);
        }
    }
    found
}

/// Whether compiled code holds a string of its own, as metadata holds the
/// name and the namespace of each type.
fn holds_string(code: &[u8], text: &str) -> bool {
    let mut needle = Vec::with_capacity(text.len() + 2);
    needle.push(0);
    needle.extend_from_slice(text.as_bytes());
    needle.push(0);
    !text.is_empty() && code.windows(needle.len()).any(|window| window == needle)
}

/// What an environment is read for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// The importers and inspectors of some assets.
    Importers,
    /// A build of Player objects: the classes of the given scripts, and
    /// every class a build calls for a shader.
    Build,
    /// The inspectors of material variants and the importers of the shaders
    /// they are inspected with.
    Inspectors,
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

/// The environment in which a scene is written: the assemblies of the
/// project's Editor compilation that define the given scripts (the classes
/// of the scene's objects and the importers of its assets), with those they
/// refer to.
pub fn scene_environment(
    project: &str,
    scripts: &BTreeSet<String>,
    importers: &BTreeSet<String>,
) -> Result<ScriptEnvironment> {
    let selection = ScriptSelection {
        scripts: scripts.union(importers).cloned().collect(),
        importers: importers.clone(),
        all: true,
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
                    (Purpose::Importers | Purpose::Inspectors, Some(first))
                        if !super::is_not_exist(&first) =>
                    {
                        Some(first)
                    }
                    (Purpose::Importers | Purpose::Inspectors, _) => Some(error),
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
            Purpose::Inspectors => {
                "a material inspector runs on the script assemblies of an Editor compilation of the project, which a preparation that builds its host leaves in the staging clone"
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
    let mut of_scripts = BTreeSet::new();
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
    if let Some(missing) = files.first()
        && purpose != Purpose::Inspectors
    {
        bail!("no compiled package assembly is made from a script {missing}");
    }
    let mut of_importers: Vec<usize> = Vec::new();
    'search: for (index, assembly) in assemblies.iter().enumerate() {
        if wanted.is_empty() {
            break;
        }
        for source in &assembly.source_files {
            let directory = package_root(source).and_then(|root| {
                packages
                    .get(root)
                    .map(|directory| format!("{directory}{}", &source[root.len()..]))
            });
            // A scene's scripts may be the project's own.
            let Some(directory) = directory.or_else(|| {
                (selection.all && source.starts_with("Assets/")).then(|| source.clone())
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
                of_scripts.insert(index);
                if selection.importers.contains(&guid) {
                    of_importers.push(index);
                }
                if wanted.is_empty() {
                    break 'search;
                }
            }
        }
    }
    // A scene may hold a behaviour whose script is missing, as the Editor
    // keeps it.
    if let Some(missing) = wanted.first()
        && !selection.all
    {
        bail!("no compiled package assembly defines the importer script {missing}");
    }
    // The assembly that defines each element of a document.
    let mut elements: BTreeSet<&str> = selection.elements.iter().map(String::as_str).collect();
    for (index, assembly) in assemblies.iter().enumerate() {
        if elements.is_empty() {
            break;
        }
        let Ok(code) = read_file(&sysutil::join(&[
            &library,
            "Library/ScriptAssemblies",
            &format!("{}.dll", assembly.name),
        ])) else {
            continue;
        };
        let defined: Vec<&str> = elements
            .iter()
            .copied()
            .filter(|full| {
                let (namespace, name) = full.rsplit_once('.').unwrap_or(("", full));
                holds_string(&code, name) && holds_string(&code, namespace)
            })
            .collect();
        for full in defined {
            elements.remove(full);
            selected.insert(index);
        }
    }
    if let Some(missing) = elements.first() {
        bail!("no compiled assembly of the project defines the UXML element {missing}");
    }
    if purpose == Purpose::Inspectors {
        let immutable = |source: &String| {
            package_root(source)
                .and_then(|root| packages.get(root))
                .is_some_and(|directory| directory.starts_with("Library/PackageCache/"))
        };
        for (index, assembly) in assemblies.iter().enumerate() {
            if !selected.contains(&index)
                && !assembly.name.contains(['/', '\\', '\0'])
                && assembly
                    .references
                    .iter()
                    .any(|reference| of_scripts.contains(reference))
                && assembly.source_files.iter().all(immutable)
                && stat(&sysutil::join(&[
                    &library,
                    "Library/ScriptAssemblies",
                    &format!("{}.dll", assembly.name),
                ]))
                .is_ok()
            {
                selected.insert(index);
            }
        }
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
    // The assemblies of the importers, those of immutable packages that
    // refer to them (the targets and nodes of a shader graph, whose
    // importer adds objects of their classes), and those they refer to.
    let importer_assemblies: BTreeSet<usize> = of_importers.iter().copied().collect();
    if !importer_assemblies.is_empty() {
        let immutable = |source: &String| {
            package_root(source)
                .and_then(|root| packages.get(root))
                .is_some_and(|directory| directory.starts_with("Library/PackageCache/"))
        };
        for (index, assembly) in assemblies.iter().enumerate() {
            if !importer_assemblies.contains(&index)
                && !assembly.name.contains(['/', '\\', '\0'])
                && assembly
                    .references
                    .iter()
                    .any(|reference| importer_assemblies.contains(reference))
                && assembly.source_files.iter().all(immutable)
                && stat(&sysutil::join(&[
                    &library,
                    "Library/ScriptAssemblies",
                    &format!("{}.dll", assembly.name),
                ]))
                .is_ok()
            {
                of_importers.push(index);
            }
        }
    }
    let mut whole: BTreeSet<usize> = BTreeSet::new();
    while let Some(index) = of_importers.pop() {
        if index >= assemblies.len() {
            bail!("Editor compilation record {record_path} refers to an unknown assembly");
        }
        if whole.insert(index) {
            of_importers.extend(assemblies[index].references.iter().copied());
        }
    }
    selected.extend(whole.iter().copied());
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
                (Purpose::Importers | Purpose::Inspectors, Some((root, directory)))
                    if selection.plugins && directory.starts_with("Library/PackageCache/") =>
                {
                    format!("{directory}{}", &plugin[root.len()..])
                }
                (Purpose::Importers | Purpose::Inspectors, _) => bail!(
                    "importer assembly {} needs the precompiled assembly {plugin}, which a standalone worker does not load",
                    assembly.name
                ),
            };
            plugins.insert(place);
        }
        for source in &assembly.source_files {
            environment
                .script_assemblies
                .insert(source.clone(), assembly.name.clone());
        }
        for source in assembly
            .source_files
            .iter()
            .chain((!assembly.asmdef.is_empty()).then_some(&assembly.asmdef))
        {
            // A scene needs the scripts its objects name as assets, with
            // every other script of their assemblies: the worker ties a
            // script to its class among the scripts of its assembly, and
            // with some of them missing it ties it to another class. The
            // assemblies they refer to are loaded without their scripts.
            if selection.all
                && !whole.contains(&index)
                && !of_scripts.contains(&index)
                && !source.starts_with("Assets/")
            {
                continue;
            }
            let directory = package_root(source).and_then(|root| {
                packages
                    .get(root)
                    .filter(|directory| directory.starts_with("Library/PackageCache/"))
                    .map(|directory| (root, directory))
            });
            // The elements of a document may be the project's own.
            if directory.is_none()
                && (!selection.elements.is_empty() || selection.all)
                && source.starts_with("Assets/")
            {
                scripts.insert(source.clone());
                continue;
            }
            // A scene's scripts may refer to an assembly of a package held
            // in place: it is loaded, its sources are no assets here.
            if directory.is_none() && selection.all {
                continue;
            }
            let Some((root, directory)) = directory else {
                bail!(
                    "importer assembly {} is compiled from {source}, which is not in an immutable package",
                    assembly.name
                );
            };
            environment
                .roots
                .insert(root.to_string(), directory.to_string());
            environment
                .assembly_roots
                .entry(assembly.name.clone())
                .or_insert_with(|| root.to_string());
            scripts.insert(source.clone());
        }
        let hash = sha256(&data);
        identity.push_str(&format!("{}\0{hash}\0", assembly.name));
        environment.assemblies.push(assembly.name.clone());
        environment
            .assembly_sha256
            .insert(assembly.name.clone(), hash);
    }
    if scripts.len() > SCRIPT_LIMIT {
        bail!("importer assemblies are compiled from more scripts than one import context holds");
    }
    // A build reads files of any package: what a shader includes, what the
    // settings refer to.
    if purpose != Purpose::Importers {
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
        let hash = sha256(&data);
        identity.push_str(&format!("plugin\0{plugin}\0{hash}\0"));
        environment.assembly_sha256.insert(plugin.clone(), hash);
    }
    environment.sha256 = sha256(identity.as_bytes());
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_names_its_custom_elements_by_namespace() {
        let document =
            br#"<ui:UXML xmlns:ui="UnityEngine.UIElements" xmlns:d="Dungeon" xmlns="Game.Hud">
    <Style src="Terminal.uss" />
    <ui:VisualElement name="a"><d:CornerBrackets /></ui:VisualElement>
    <d:SegmentBar></d:SegmentBar>
    <Gauge/>
    <Other.Space.Meter />
    <ui:Template src="T.uxml" name="t" />
</ui:UXML>"#;
        assert_eq!(
            uxml_elements(document).into_iter().collect::<Vec<_>>(),
            [
                "Dungeon.CornerBrackets",
                "Dungeon.SegmentBar",
                "Game.Hud.Gauge",
                "Other.Space.Meter",
            ]
        );
        // Without a default namespace an unprefixed tag is markup.
        assert!(uxml_elements(br#"<UXML><Style src="a.uss"/></UXML>"#).is_empty());
    }

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
