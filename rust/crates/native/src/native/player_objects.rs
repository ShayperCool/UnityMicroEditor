//! The Player form of an asset, written by the Editor's own build code
//! inside a standalone worker.
//!
//! A worker is the Editor binary, so it holds everything a build runs: the
//! enumeration of shader variants, the keyword filters of a render pipeline
//! asset, the `IPreprocessShaders` strippers of the packages, the shader
//! compiler pool and the object writer. `tools/WorkerScripts` holds a
//! `ScriptedImporter` that reaches them through `ContentBuildInterface`, the
//! entry the scriptable build pipeline uses. The tool compiles it with the
//! SDK's Roslyn, stages it into the import context as a script of the
//! project and hands it a request file to import after the assets the
//! request names.
use super::{
    ProbeOptions,
    build_settings::{BuildSettings, Sources, build_settings, include_closure, material_parents},
    context::BuildFiles,
    import::{BuildImport, run_build_import},
    scripted::{build_environment, package_directories, scripted_importer},
    support::{lower_ext, mkdir_all, read_file, stat, wrap, write_file},
};
use crate::{
    cancel::Context,
    compiler::{compiler_command, glob_lossy},
    playerbake::{self, Unsupported, Val, read_serialized_with_externals},
    sha256, sysutil,
};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

/// The importer's source, compiled into every context that needs it.
pub const IMPORTER_SOURCE: &str =
    include_str!("../../../../../tools/WorkerScripts/PlayerObjectImporter.cs");
/// Where the context holds it while it is compiled.
pub const IMPORTER_PATH: &str = "Assets/UnityMicroEditorWorker/PlayerObjectImporter.cs";
pub const IMPORTER_ASSEMBLY: &str = "UnityMicroEditor.Worker";
/// The assembly definition the importer is compiled from, for a worker that
/// loads only the assemblies the project's definitions make
/// (`Engine::assemblies_from_definitions`), and the GUIDs of both sources.
pub const IMPORTER_DEFINITION_PATH: &str =
    "Assets/UnityMicroEditorWorker/UnityMicroEditor.Worker.asmdef";
const IMPORTER_GUID: &str = "d1f2a7c4e8b94f0a9c3e5b7d1a2f4c60";
const IMPORTER_DEFINITION_GUID: &str = "d1f2a7c4e8b94f0a9c3e5b7d1a2f4c61";
const IMPORTER_DEFINITION: &str = r#"{
    "name": "UnityMicroEditor.Worker",
    "references": [],
    "includePlatforms": ["Editor"],
    "excludePlatforms": [],
    "allowUnsafeCode": false,
    "overrideReferences": false,
    "precompiledReferences": [],
    "autoReferenced": false,
    "defineConstraints": [],
    "versionDefines": [],
    "noEngineReferences": false
}
"#;
/// The asset the importer imports: the request. The worker picks the
/// importer by the extension.
pub const REQUEST_PATH: &str = "Assets/UnityMicroEditorWorker/request.umeplayer";
/// The name the artifact gives the written file.
const PLAYER_FILE: &str = "player";
/// The name the artifact gives the file of what a written scene refers to.
const DEPENDENCIES_FILE: &str = "deps";
/// How often a batch is extended by an asset the pipeline's objects load by
/// path and run again.
const RELOAD_ROUNDS: usize = 24;

/// Identifies the importer's code, for the key of anything made with it.
pub fn importer_identity() -> String {
    sha256(IMPORTER_SOURCE.as_bytes())
}

/// What a request asks for, by logical asset path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The assets whose Player objects are written.
    pub write: Vec<String>,
    /// The assets those refer to whose objects go into the same file.
    pub embed: Vec<String>,
    /// The one type of object to write of them, when not all.
    pub only: String,
    /// The assets that use them, such as the materials of a shader.
    pub users: Vec<String>,
    /// The assets a build reads beside them, which have to load.
    pub needs: Vec<String>,
    /// The assets whose objects are saved as the Editor writes them as
    /// text, once the engine has loaded them.
    pub save: Vec<String>,
    /// Render pipeline global settings whose entries a Player build keeps
    /// are reported.
    pub settings: Vec<String>,
    /// The scene written as a build writes it; a request that names one
    /// writes nothing else.
    pub scenes: Vec<String>,
    /// A scene written with what it refers to in a file beside it.
    pub scenes_with_dependencies: Vec<String>,
    /// Assets whose objects go into that file though the scene does not
    /// refer to them.
    pub includes: Vec<String>,
}

impl Request {
    pub fn encode(&self) -> Result<String> {
        let mut text = String::new();
        if !self.only.is_empty() {
            text.push_str(&format!("only {}\n", self.only));
        }
        for (verb, paths) in [
            ("need", &self.needs),
            ("use", &self.users),
            ("write", &self.write),
            ("embed", &self.embed),
            ("save", &self.save),
            ("settings", &self.settings),
            ("scene", &self.scenes),
            ("include", &self.includes),
            ("scene-with-dependencies", &self.scenes_with_dependencies),
        ] {
            for path in paths {
                if path.contains(['\n', '\r']) {
                    bail!("asset path holds a line break: {path:?}");
                }
                text.push_str(&format!("{verb} {path}\n"));
            }
        }
        Ok(text)
    }
}

/// An object another file holds, which a written object refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalObject {
    /// The path name the written file gives the other file.
    pub file: String,
    pub guid: String,
    /// 0 for an object built into the engine, 3 for an imported asset.
    pub file_type: i32,
    /// The object's local ID in its own asset.
    pub local_id: i64,
    /// The local ID the written file refers to it by.
    pub index: i64,
    pub class_name: String,
}

/// An object of the written file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenObject {
    /// Its local ID in the written file.
    pub index: i64,
    pub class_name: String,
    /// The asset it is of, and its local ID there.
    pub guid: String,
    pub local_id: i64,
}

/// What the importer says about its run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Every written object.
    pub objects: Vec<WrittenObject>,
    /// The objects a scene refers to, written into the file beside it.
    pub dependencies: Vec<WrittenObject>,
    /// The root GameObject of each included prefab, by GUID.
    pub include_roots: Vec<(String, i64)>,
    /// The objects of each other included asset, the main one first.
    pub include_objects: Vec<(String, i64)>,
    pub externals: Vec<ExternalObject>,
    /// The render pipeline asset of every quality level; empty for none.
    pub quality_pipelines: Vec<String>,
    /// The render pipeline assets the build is made with.
    pub build_pipelines: Vec<String>,
    /// A build callback that was taken without its constructor.
    pub unconstructed: Vec<String>,
    /// What the compiler said about the written shaders.
    pub messages: Vec<String>,
    /// The classes that strip shader variants, with their assemblies.
    pub shader_preprocessors: Vec<String>,
    pub compute_preprocessors: Vec<String>,
    /// The volume profiles the worker's asset search finds, by path.
    pub volume_profiles: Vec<String>,
    /// The text the Editor writes of each saved asset, by path.
    pub saved: Vec<(String, Vec<u8>)>,
    /// The entries of global settings a Player build keeps or strips.
    pub settings: Vec<SettingsEntry>,
}

/// One entry of a render pipeline's global settings, by its managed
/// reference ID, and whether a Player build keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsEntry {
    pub path: String,
    pub rid: i64,
    pub type_name: String,
    pub kept: bool,
}

fn external(statement: &str) -> Option<ExternalObject> {
    // `<file> <index> <class> <path>`; the file is `ume-external/<guid>/<type>/<local id>`.
    let mut fields = statement.splitn(4, ' ');
    let (file, index, class_name) = (fields.next()?, fields.next()?, fields.next()?);
    let mut parts = file.strip_prefix("ume-external/")?.split('/');
    let (guid, file_type, local_id) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || guid.len() != 32 {
        return None;
    }
    Some(ExternalObject {
        file: file.to_string(),
        guid: guid.to_ascii_lowercase(),
        file_type: file_type.parse().ok()?,
        local_id: local_id.parse().ok()?,
        index: index.parse().ok()?,
        class_name: class_name.to_string(),
    })
}

/// Reads the importer's report. A run that failed is an error with the
/// importer's own reason.
pub fn parse_report(text: &str) -> Result<Report> {
    let mut lines = text.lines();
    if lines.next() != Some("ume-player-objects 1") {
        bail!("the worker's build report is not of this tool's importer");
    }
    let mut report = Report::default();
    let mut done = false;
    for line in lines {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let malformed = || anyhow!("the worker's build report holds a malformed line: {line:?}");
        match verb {
            "error" => bail!(Unsupported(format!(
                "a build the Editor's code refuses in a worker: {rest}"
            ))),
            "object" | "dependency" => {
                let mut fields = rest.split(' ');
                let index = fields.next().and_then(|index| index.parse().ok());
                let class_name = fields.next();
                let guid = fields.next().filter(|guid| guid.len() == 32);
                let local_id = fields.next().and_then(|id| id.parse().ok());
                let (Some(index), Some(class_name), Some(guid), Some(local_id)) =
                    (index, class_name, guid, local_id)
                else {
                    return Err(malformed());
                };
                let object = WrittenObject {
                    index,
                    class_name: class_name.to_string(),
                    guid: guid.to_ascii_lowercase(),
                    local_id,
                };
                if verb == "object" {
                    report.objects.push(object);
                } else {
                    report.dependencies.push(object);
                }
            }
            "external" => report.externals.push(external(rest).ok_or_else(malformed)?),
            "include-root" | "include-object" => {
                let (guid, id) = rest.split_once(' ').ok_or_else(malformed)?;
                let id = id.parse().map_err(|_| malformed())?;
                let list = match verb {
                    "include-root" => &mut report.include_roots,
                    _ => &mut report.include_objects,
                };
                list.push((guid.to_ascii_lowercase(), id));
            }
            "quality" => {
                let (_, path) = rest.split_once(' ').ok_or_else(malformed)?;
                report.quality_pipelines.push(match path {
                    "none" => String::new(),
                    path => path.to_string(),
                });
            }
            "build-pipeline" => report.build_pipelines.push(rest.to_string()),
            "callback-unconstructed" => report.unconstructed.push(rest.to_string()),
            "shader-message" => report.messages.push(rest.to_string()),
            "shader-preprocessor" => report.shader_preprocessors.push(rest.to_string()),
            "compute-preprocessor" => report.compute_preprocessors.push(rest.to_string()),
            "volume-profile" => {
                let (_, path) = rest.split_once(' ').ok_or_else(malformed)?;
                report.volume_profiles.push(path.to_string());
            }
            "saved" => {
                use base64::Engine as _;
                let (text, path) = rest.split_once(' ').ok_or_else(malformed)?;
                let text = base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(|_| malformed())?;
                report.saved.push((path.to_string(), text));
            }
            "settings" => {
                let mut fields = rest.splitn(4, ' ');
                let kept = match fields.next() {
                    Some("kept") => true,
                    Some("stripped") => false,
                    _ => return Err(malformed()),
                };
                let rid = fields.next().and_then(|rid| rid.parse().ok());
                let (Some(rid), Some(type_name), Some(path)) = (rid, fields.next(), fields.next())
                else {
                    return Err(malformed());
                };
                report.settings.push(SettingsEntry {
                    path: path.to_string(),
                    rid,
                    type_name: type_name.to_string(),
                    kept,
                });
            }
            "done" => done = true,
            // What was named and found, and what a later revision adds.
            _ => {}
        }
    }
    if !done {
        bail!("the worker's build report is incomplete");
    }
    Ok(report)
}

/// The text of the report an artifact's primary file holds.
pub fn read_report(primary: &[u8]) -> Result<Report> {
    let (objects, _) = read_serialized_with_externals(primary)?;
    for object in &objects {
        if object.tree.class_id != 49 {
            continue;
        }
        let value = object.tree.decode(&object.data)?;
        let text = match value.field("m_Script") {
            Some(Val::Str(text)) => text.clone(),
            Some(Val::Bytes(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            _ => bail!("the worker's build report holds no text"),
        };
        return parse_report(&text);
    }
    bail!("the worker's artifact holds no build report")
}

/// Compiles the importer into the context's script assemblies, where the
/// worker loads it. The worker picks it by the extension its attribute
/// names. It is compiled like a script of the project, Unity's source
/// generator included. Returns the sources the context's `SourceAssetDB`
/// has to register as script compilation assets for the worker to load the
/// assembly: the importer and its assembly definition where the release
/// loads only the assemblies its definitions make, none otherwise.
pub fn stage_importer(ctx: &Context, editor: &str, project: &str) -> Result<Vec<String>> {
    let installation = match stat(editor).map_err(|e| wrap("Unity installation", e))? {
        info if info.is_dir() => editor.to_string(),
        _ => sysutil::dir(editor),
    };
    let data = sysutil::join(&[&installation, "Data"]);
    let crate::compiler::UnitySdk { dotnet, csc } = crate::compiler::unity_sdk(&data)?;
    let generator = sysutil::join(&[
        &crate::compiler::source_generators(&data),
        "Unity.SourceGenerators.dll",
    ]);
    let source = sysutil::join(&[project, IMPORTER_PATH]);
    mkdir_all(&sysutil::dir(&source), 0o700)?;
    write_file(&source, IMPORTER_SOURCE.as_bytes(), 0o600)?;
    let mut registered = Vec::new();
    if crate::engine::of_project(project)?.assemblies_from_definitions {
        let script_meta = format!(
            "fileFormatVersion: 2\nguid: {IMPORTER_GUID}\nMonoImporter:\n  externalObjects: {{}}\n  serializedVersion: 2\n  defaultReferences: []\n  executionOrder: 0\n  icon: {{instanceID: 0}}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
        );
        write_file(&format!("{source}.meta"), script_meta.as_bytes(), 0o600)?;
        let definition = sysutil::join(&[project, IMPORTER_DEFINITION_PATH]);
        write_file(&definition, IMPORTER_DEFINITION.as_bytes(), 0o600)?;
        let definition_meta = format!(
            "fileFormatVersion: 2\nguid: {IMPORTER_DEFINITION_GUID}\nAssemblyDefinitionImporter:\n  externalObjects: {{}}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
        );
        write_file(
            &format!("{definition}.meta"),
            definition_meta.as_bytes(),
            0o600,
        )?;
        registered = vec![
            IMPORTER_PATH.to_string(),
            IMPORTER_DEFINITION_PATH.to_string(),
        ];
    }
    let work = sysutil::join(&[project, "Temp", "UnityMicroEditorWorker"]);
    mkdir_all(&work, 0o700)?;
    let output = sysutil::join(&[
        project,
        "Library/ScriptAssemblies",
        &format!("{IMPORTER_ASSEMBLY}.dll"),
    ]);
    mkdir_all(&sysutil::dir(&output), 0o700)?;
    // The generator takes the project's directory from this file.
    let additional = sysutil::join(&[&work, "UnityAdditionalFile.txt"]);
    write_file(&additional, project.as_bytes(), 0o600)?;
    let mut arguments: Vec<String> = [
        "-nologo",
        "-target:library",
        "-nostdlib+",
        "-langversion:9.0",
        "-deterministic+",
        "-debug-",
        "-optimize+",
        "-define:UNITY_EDITOR",
    ]
    .map(String::from)
    .to_vec();
    // The version symbols the Editor compiles with, which the importer
    // selects the release's API by.
    let versions =
        crate::release::version_defines(&crate::release::installed_version(&sysutil::join(&[
            &installation,
            "Unity",
        ]))?)?;
    arguments.extend(versions.iter().map(|define| format!("-define:{define}")));
    arguments.push(format!("-out:{output}"));
    for pattern in [
        "Managed/UnityEngine/*.dll",
        "UnityReferenceAssemblies/unity-4.8-api/*.dll",
        "UnityReferenceAssemblies/unity-4.8-api/Facades/*.dll",
    ] {
        let mut references = glob_lossy(&sysutil::join(&[&data, pattern]));
        references.sort();
        arguments.extend(references.iter().map(|path| format!("-reference:{path}")));
    }
    arguments.push(format!("-analyzer:{generator}"));
    arguments.push(format!("-additionalfile:{additional}"));
    arguments.push(IMPORTER_PATH.to_string());
    let response = sysutil::join(&[&work, "compile.rsp"]);
    let mut text = String::new();
    for argument in &arguments {
        text.push('"');
        text.push_str(&argument.replace('"', "\\\""));
        text.push_str("\"\n");
    }
    write_file(&response, text.as_bytes(), 0o600)?;
    let listing = match compiler_command(ctx, project, &dotnet, &[&csc, &format!("@{response}")]) {
        Ok(output) => output,
        Err(error) => bail!("compile the worker's build importer: {}", error.message),
    };
    let listing = String::from_utf8_lossy(&listing);
    // The compiler goes on when a generator does not load; the assembly
    // would then not be the one a project's compilation makes.
    if ["CS8032", "CS8784", "CS8785"]
        .iter()
        .any(|code| listing.contains(code))
        || !stat(&output).is_ok_and(|info| info.is_regular() && info.size > 0)
    {
        bail!("compile the worker's build importer: {listing}");
    }
    Ok(registered)
}

/// What to write and where.
#[derive(Clone, Debug, Default)]
pub struct Build {
    pub project: String,
    pub editor: String,
    /// A new directory under the project's owned cache; every worker run
    /// gets a context beneath it.
    pub output: String,
    /// The asset whose Player objects are written.
    pub target: String,
    /// The materials that use it.
    pub users: Vec<String>,
    pub timeout: Duration,
    /// A file that keeps, from one build to the next, the assets the
    /// pipeline's objects were found to load by path.
    pub reload_list: String,
    /// A directory that keeps the shader variants the workers compiled, for
    /// the next build to start with; none when empty.
    pub shader_cache: String,
}

/// Everything one worker run imports for a build, found from the sources
/// without starting anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The assets of the batch in import order, the request last.
    pub batch: Vec<String>,
    pub request: Request,
    /// The scripts whose assemblies the worker loads, by GUID.
    pub scripts: BTreeSet<String>,
    /// The volume profiles the pipeline's build code has to find.
    pub volume_profiles: Vec<String>,
    /// The shader sources the target names as a fallback or for a pass,
    /// and those they name.
    pub linked: Vec<String>,
    /// The hash of every file of the project the result depends on, by
    /// logical path. Files of an immutable package are left out: the
    /// package manifest and its lock file stand for them.
    pub ingredients: BTreeMap<String, String>,
}

/// The files that state which packages a project has.
const PACKAGE_FILES: [&str; 2] = ["Packages/manifest.json", "Packages/packages-lock.json"];

/// Whether a shader name stands for a shader built into the Editor.
type Builtin<'a> = &'a dyn Fn(&str) -> Result<bool>;

/// The shader sources a shader text names, each once, those they name in
/// turn included. A shader built into the Editor whose passes alone a text
/// uses is left to the worker, which finds it by name and copies the
/// passes in: nothing built from the text needs it.
fn linked_shaders(
    sources: &Sources,
    builtin: Builtin<'_>,
    target: &str,
    text: &[u8],
) -> Result<Vec<String>> {
    let mut linked: Vec<String> = Vec::new();
    let mut pending = vec![(target.to_string(), text.to_vec())];
    while let Some((from, text)) = pending.pop() {
        for (name, only_passes) in super::baked::linked_shader_links(&text) {
            let path = match &sources.shaders_named(&name)?[..] {
                [] if only_passes && builtin(&name)? => continue,
                [] => bail!(Unsupported(format!(
                    "the shader {name:?} that {from} names, which no source of the project or its packages declares"
                ))),
                [one] => one.clone(),
                _ => bail!(Unsupported(format!(
                    "the shader {name:?} that {from} names, which several sources declare"
                ))),
            };
            if path == target || linked.contains(&path) {
                continue;
            }
            if linked.len() >= 64 {
                bail!(Unsupported(format!(
                    "a shader that names more than 64 others ({target})"
                )));
            }
            pending.push((path.clone(), sources.read(&path)?));
            linked.push(path);
        }
    }
    Ok(linked)
}

/// Whether a file is a source of the project itself, which can change. A
/// file of a cached package cannot: its directory is named after its
/// content, and the package files of the project stand for it.
pub(super) fn authored(sources: &Sources, project: &str, logical: &str) -> bool {
    sources.physical(logical).is_some_and(|physical| {
        ["Assets", "ProjectSettings", "Packages"]
            .iter()
            .any(|root| crate::util::within_directory(&physical, &sysutil::join(&[project, root])))
    })
}

/// Plans the build of `target`. `texts` are the shader texts it stands for,
/// each with the file its relative includes start from: its own source for
/// a `.shader` or `.compute`, the generated shader for a graph.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    sources: &Sources,
    project: &str,
    settings: &BuildSettings,
    target: &str,
    users: &[String],
    texts: &[(&str, &[u8])],
    graph: &[String],
    builtin: Builtin<'_>,
) -> Result<Plan> {
    let mut plan = Plan {
        scripts: settings.scripts.clone(),
        volume_profiles: settings.volume_profiles.clone(),
        ..Default::default()
    };
    for (path, hash) in &settings.ingredients {
        let source = path.strip_suffix(".meta").unwrap_or(path);
        if authored(sources, project, source) {
            plan.ingredients.insert(path.clone(), hash.clone());
        }
    }
    for path in PACKAGE_FILES {
        match read_file(&sysutil::join(&[project, path])) {
            Ok(data) => {
                plan.ingredients.insert(path.to_string(), sha256(&data));
            }
            Err(error) if super::is_not_exist(&error) => {}
            Err(error) => return Err(error),
        }
    }
    let mut owned: Vec<(String, Vec<u8>)> = texts
        .iter()
        .map(|(from, text)| (from.to_string(), text.to_vec()))
        .collect();
    for (from, text) in texts {
        for path in linked_shaders(sources, builtin, from, text)? {
            if !plan.linked.contains(&path) && path != target {
                owned.push((path.clone(), sources.read(&path)?));
                plan.linked.push(path);
            }
        }
    }
    let borrowed: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(from, text)| (from.as_str(), text.as_slice()))
        .collect();
    let includes = include_closure(sources, &borrowed)?;
    let mut parents: Vec<String> = Vec::new();
    for user in users {
        for parent in material_parents(sources, &sources.read(user)?)? {
            if !parents.contains(&parent) && !users.contains(&parent) {
                parents.push(parent);
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut add = |plan: &mut Plan, path: &str| -> Result<()> {
        if !seen.insert(path.to_string()) {
            return Ok(());
        }
        let Some(physical) = sources.physical(path) else {
            bail!(Unsupported(format!(
                "{path}, which lies in a package outside the project"
            )));
        };
        // A file without a `.meta` is not an asset yet; the Editor would
        // give it an identity of its own choosing.
        let meta = read_file(&format!("{physical}.meta")).map_err(|error| {
            if super::is_not_exist(&error) {
                anyhow::Error::from(Unsupported(format!("{path}, which has no .meta")))
            } else {
                error
            }
        })?;
        if let Some(importer) = scripted_importer(&meta) {
            plan.scripts.insert(importer);
        }
        if authored(sources, project, path) && path != target {
            plan.ingredients
                .insert(path.to_string(), crate::util::file_hash(&physical)?);
            plan.ingredients
                .insert(format!("{path}.meta"), sha256(&meta));
        }
        plan.batch.push(path.to_string());
        Ok(())
    };
    for path in settings
        .batch
        .iter()
        .chain(&includes)
        .chain(graph)
        .chain(&plan.linked.clone())
        .chain(std::iter::once(&target.to_string()))
        .chain(&parents)
        .chain(users)
    {
        add(&mut plan, path)?;
    }
    plan.request = Request {
        write: vec![target.to_string()],
        // A graph also holds a material made of its default values, which
        // nothing here delivers.
        only: if lower_ext(target) == ".shadergraph" {
            "UnityEngine.Shader".into()
        } else {
            String::new()
        },
        // A shader of a package is no source a session publishes, so the
        // file has to hold it, as a bundle holds what no bundle is named
        // for. One under `Assets` is published as a file of its own.
        embed: plan
            .linked
            .iter()
            .filter(|path| path.starts_with("Packages/"))
            .cloned()
            .collect(),
        users: users.to_vec(),
        needs: settings
            .needs
            .iter()
            .chain(&parents)
            .filter(|path| *path != target && !users.contains(path))
            .cloned()
            .collect(),
        save: Vec::new(),
        settings: Vec::new(),
        scenes: Vec::new(),
        scenes_with_dependencies: Vec::new(),
        includes: Vec::new(),
    };
    if plan.batch.len() >= 4000 {
        bail!(Unsupported(format!(
            "a build of {target} that reads {} assets, more than one import context holds",
            plan.batch.len()
        )));
    }
    plan.batch.push(REQUEST_PATH.to_string());
    Ok(plan)
}

/// The Player objects the Editor's build code wrote for an asset.
pub struct PlayerObjects {
    /// The written objects with their type trees.
    pub objects: Vec<playerbake::Read>,
    /// The objects of other files they refer to, in the order of the
    /// file's references: a `PPtr` with file ID `n` leads to entry `n - 1`.
    pub externals: Vec<ExternalObject>,
    /// The source of each of those, where the project or a package has one.
    pub external_paths: BTreeMap<String, String>,
    pub report: Report,
    pub ingredients: BTreeMap<String, String>,
    /// How many assets the worker imported, and how long the run took.
    pub imports: usize,
    pub seconds: f64,
    /// The part of it spent on recording imports in the ArtifactDB.
    pub database_seconds: f64,
}

impl PlayerObjects {
    /// The written objects of the asset with a GUID.
    pub fn objects_of(&self, guid: &str) -> Vec<&playerbake::Read> {
        self.report
            .objects
            .iter()
            .filter(|written| written.guid == guid)
            .filter_map(|written| {
                self.objects
                    .iter()
                    .find(|object| object.local_id == written.index)
            })
            .collect()
    }
}

/// The asset an object of the pipeline tried to load by path and did not
/// find, when that is why a run stopped.
fn reloaded_path(error: &anyhow::Error) -> Option<String> {
    let text = format!("{error:#}");
    let rest = &text[text.find("Cannot load. Incorrect path: ")? + 29..];
    let path = rest.lines().next()?.trim();
    (path.starts_with("Assets/") || path.starts_with("Packages/")).then(|| path.to_string())
}

fn read_reload_list(sources: &Sources, path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    read_file(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Vec<String>>(&data).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|logical| {
            sources
                .physical(logical)
                .is_some_and(|physical| stat(&physical).is_ok_and(|info| info.is_regular()))
        })
        .collect()
}

/// The texts of a graph's generated shader, read from the artifact of its
/// import: the program sources, and a line per shader it names in the form
/// a `.shader` source has.
pub(super) fn artifact_texts(artifact: &[u8]) -> Result<Vec<u8>> {
    let shaders = playerbake::read_serialized_of_class(artifact, 48)?;
    if shaders.is_empty() {
        bail!("the artifact of the shader graph holds no Shader");
    }
    let mut text = Vec::new();
    fn collect(value: &Val, text: &mut Vec<u8>) {
        match value {
            Val::Str(string) => {
                text.extend_from_slice(string.as_bytes());
                text.push(b'\n');
            }
            Val::Bytes(bytes) if bytes.windows(8).any(|window| window == b"#include") => {
                text.extend_from_slice(bytes);
                text.push(b'\n');
            }
            Val::Seq(items) => items.iter().for_each(|item| collect(item, text)),
            Val::Map(fields) => fields.iter().for_each(|(name, field)| {
                // What the shader names: its fallback and the passes it uses.
                if let (true, Val::Str(named)) = (
                    matches!(name.as_ref(), "m_FallbackName" | "m_UseName"),
                    field,
                ) {
                    if !named.is_empty() {
                        let verb = if name == "m_UseName" {
                            "UsePass"
                        } else {
                            "Fallback"
                        };
                        text.extend_from_slice(format!("{verb} \"{named}\"\n").as_bytes());
                    }
                } else {
                    collect(field, text)
                }
            }),
            _ => {}
        }
    }
    for shader in &shaders {
        collect(&shader.tree.decode(&shader.data)?, &mut text);
    }
    Ok(text)
}

fn references(data: &[u8]) -> Vec<String> {
    static GUID: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"guid[\\"]*:\s*[\\"]*([0-9a-fA-F]{32})"#).unwrap()
    });
    let mut found = Vec::new();
    for capture in GUID.captures_iter(data) {
        let guid = String::from_utf8_lossy(&capture[1]).to_ascii_lowercase();
        if !found.contains(&guid) {
            found.push(guid);
        }
    }
    found
}

/// The sources a shader graph refers to by GUID (sub graphs, the files of
/// its custom functions, its default textures), each after those it refers
/// to itself.
pub fn graph_sources(sources: &Sources, target: &str) -> Result<Vec<String>> {
    fn visit(
        sources: &Sources,
        path: &str,
        seen: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        if !seen.insert(path.to_string()) {
            return Ok(());
        }
        if seen.len() > 512 {
            bail!(Unsupported(
                "a shader graph that refers to more than 512 sources".into()
            ));
        }
        if matches!(lower_ext(path).as_str(), ".shadergraph" | ".shadersubgraph") {
            for guid in references(&sources.read(path)?) {
                if let Some(next) = sources.path_of(&guid)
                    && !matches!(lower_ext(next).as_str(), ".cs" | ".dll")
                    && sources
                        .physical(next)
                        .is_some_and(|physical| stat(&physical).is_ok_and(|info| info.is_regular()))
                {
                    visit(sources, next, seen, order)?;
                }
            }
        }
        order.push(path.to_string());
        Ok(())
    }
    let (mut seen, mut order) = (BTreeSet::new(), Vec::new());
    visit(sources, target, &mut seen, &mut order)?;
    order.pop();
    Ok(order)
}

pub(super) fn job_workers() -> usize {
    std::thread::available_parallelism().map_or(2, |count| count.get().clamp(2, 32))
}

struct Run {
    files: Vec<(String, Vec<u8>)>,
    imports: usize,
    database_seconds: f64,
}

/// One worker run over a batch; returns the files of the artifact of the
/// last asset.
fn run(
    ctx: &Context,
    build: &Build,
    output: &str,
    batch: &[String],
    scripts: &BTreeSet<String>,
    files: &BuildFiles,
    templates: &[String],
) -> Result<Run> {
    let environment = build_environment(&build.project, scripts)?;
    run_in(ctx, build, output, batch, &environment, files, templates)
}

/// One worker run over a batch with the given compiled code.
fn run_in(
    ctx: &Context,
    build: &Build,
    output: &str,
    batch: &[String],
    environment: &super::scripted::ScriptEnvironment,
    files: &BuildFiles,
    templates: &[String],
) -> Result<Run> {
    // A graph in the batch is written out from the templates of its
    // targets, which its importer reads as plain files.
    let mut files = files.clone();
    if batch
        .iter()
        .any(|path| matches!(lower_ext(path).as_str(), ".shadergraph" | ".shadersubgraph"))
    {
        files.files = templates.to_vec();
    }
    let files = &files;
    let import_context = ctx.with_timeout(build.timeout);
    let (report, error) = run_build_import(
        &import_context,
        ProbeOptions {
            project: build.project.clone(),
            editor: build.editor.clone(),
            output: output.to_string(),
            timeout: build.timeout,
            job_workers: job_workers(),
            ..Default::default()
        },
        batch,
        &BuildImport {
            environment,
            files,
            shader_cache: &build.shader_cache,
            extras: None,
        },
    );
    import_context.cancel();
    if !build.shader_cache.is_empty() {
        let _ = super::import::copy_missing(
            &sysutil::join(&[output, "project/Library/ShaderCache"]),
            &build.shader_cache,
        );
    }
    if let Some(error) = error {
        return Err(error);
    }
    let report = report.ok_or_else(|| anyhow!("worker run left no report"))?;
    // Every source of the batch but those registered without an import (a
    // precompiled assembly for a 6000.0 worker) has its artifact.
    let imported = report
        .project_context
        .as_ref()
        .map_or(batch.len(), |context| context.imported().count());
    let asset = report
        .content
        .as_ref()
        .filter(|content| content.source_unchanged && content.assets.len() == imported)
        .and_then(|content| content.assets.last())
        .ok_or_else(|| {
            anyhow!(
                "native worker produced no artifact for {}",
                batch.last().map_or("", String::as_str)
            )
        })?;
    let mut files = Vec::new();
    for file in &asset.files {
        let data = read_file(&file.path)?;
        if sha256(&data) != file.sha256 {
            bail!("native artifact of {} changed", asset.path);
        }
        files.push((file.name.trim_start_matches('.').to_string(), data));
    }
    Ok(Run {
        files,
        imports: report.imports.len(),
        database_seconds: report
            .stages
            .get("artifactDatabase")
            .map_or(0.0, |seconds| seconds.0),
    })
}

/// The sources of a project as a build finds them.
pub fn open_sources(project: &str) -> Result<Sources> {
    let (library, packages) = package_directories(project)?;
    Sources::open(project, &library, &packages)
}

/// The plan of the build of a `.shader` or `.compute` source, whose text
/// is its own. `None` for a shader graph, whose text only an import gives.
/// `editor` is the Editor whose workers build it.
pub fn plan_source(
    sources: &Sources,
    project: &str,
    editor: &str,
    reload_list: &str,
    target: &str,
    users: &[String],
) -> Result<Plan> {
    let settings = plan_settings(sources, reload_list)?;
    plan_source_with(sources, &settings, project, editor, target, users)
}

/// The settings every build of a project shares, which a plan reads.
pub fn plan_settings(sources: &Sources, reload_list: &str) -> Result<BuildSettings> {
    build_settings(sources, &read_reload_list(sources, reload_list))
}

/// [`plan_source`] with the settings of [`plan_settings`], which several
/// plans of one pass share.
pub fn plan_source_with(
    sources: &Sources,
    settings: &BuildSettings,
    project: &str,
    editor: &str,
    target: &str,
    users: &[String],
) -> Result<Plan> {
    let text = sources.read(target)?;
    let graph = graph_sources(sources, target)?;
    let texts: Vec<(&str, &[u8])> = if lower_ext(target) == ".shadergraph" {
        Vec::new()
    } else {
        vec![(target, &text[..])]
    };
    plan(
        sources,
        project,
        settings,
        target,
        users,
        &texts,
        &graph,
        &|name| Ok(super::host_shaders::editor_builtin_shader_names(editor)?.contains(name)),
    )
}

/// The text of the shader a graph is written out as, found by importing
/// the graph. Its importer leaves out an include whose file is not an asset
/// of the context and notes the path instead, so the graph is imported
/// again with those files until nothing is left out.
/// The includes a graph's importer left out of the shader it wrote, as it
/// notes them in the shader's text.
pub(super) static MISSING: std::sync::LazyLock<regex::bytes::Regex> =
    std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r"(?m-u)missing include file: ([^\r\n()]*[^\r\n() ])").unwrap()
    });

fn graph_text(
    ctx: &Context,
    build: &Build,
    sources: &Sources,
    graph: &[String],
    templates: &[String],
) -> Result<Vec<u8>> {
    let mut known: Vec<String> = Vec::new();
    for round in 0..4 {
        // The known files as a text that includes them, to follow what
        // they include in turn.
        let listing: String = known
            .iter()
            .map(|path| format!("#include \"{path}\"\n"))
            .collect();
        let mut batch = include_closure(sources, &[(build.target.as_str(), listing.as_bytes())])?;
        batch.extend(graph.iter().cloned());
        batch.push(build.target.clone());
        let mut scripts = BTreeSet::new();
        for path in &batch {
            let physical = sources
                .physical(path)
                .ok_or_else(|| anyhow!("source {path} lies outside the project"))?;
            if let Some(importer) = scripted_importer(&read_file(&format!("{physical}.meta"))?) {
                scripts.insert(importer);
            }
        }
        let run = run(
            ctx,
            build,
            &sysutil::join(&[&build.output, &format!("graph-{round}")]),
            &batch,
            &scripts,
            &BuildFiles::default(),
            templates,
        )
        .map_err(|error| wrap("import the shader graph", error))?;
        let Some((_, artifact)) = run.files.iter().find(|(name, _)| name.is_empty()) else {
            bail!("native worker produced no artifact for {}", build.target);
        };
        let mut text = artifact_texts(artifact)?;
        let mut added = false;
        let mut unresolved = None;
        for found in MISSING.captures_iter(&text) {
            let path = String::from_utf8_lossy(&found[1]).into_owned();
            if known.contains(&path) {
                continue;
            }
            if sources
                .physical(&path)
                .is_some_and(|physical| stat(&physical).is_ok_and(|info| info.is_regular()))
            {
                known.push(path);
                added = true;
            } else {
                unresolved = Some(path);
            }
        }
        if added {
            continue;
        }
        if let Some(path) = unresolved {
            bail!(Unsupported(format!(
                "a shader graph whose shader includes {path}, which no source of the project or its packages holds"
            )));
        }
        if MISSING.is_match(&text) {
            bail!(Unsupported(
                "a shader graph whose importer leaves an include out although its file is an asset of the worker".into()
            ));
        }
        text.extend_from_slice(listing.as_bytes());
        return Ok(text);
    }
    bail!(Unsupported(
        "a shader graph whose includes did not settle in four imports".into()
    ))
}

/// Has the Editor's build code write the Player objects of an asset, in a
/// standalone worker that imports the asset, what uses it and everything a
/// build of the project reads beside them.
pub fn write_player_objects(ctx: &Context, build: &Build) -> Result<PlayerObjects> {
    let started = Instant::now();
    let sources = open_sources(&build.project)?;
    let mut reloaded = read_reload_list(&sources, &build.reload_list);
    let text = sources.read(&build.target)?;
    let graph = graph_sources(&sources, &build.target)?;
    mkdir_all(&build.output, 0o700)?;
    let templates = sources.graph_templates();
    // A graph's shader exists only once the graph is imported: a run of
    // its own gives the text, whose includes and named shaders the build
    // has to hold.
    let generated;
    let texts: Vec<(&str, &[u8])> = if lower_ext(&build.target) == ".shadergraph" {
        generated = graph_text(ctx, build, &sources, &graph, &templates)?;
        vec![(build.target.as_str(), &generated[..])]
    } else {
        vec![(build.target.as_str(), &text[..])]
    };
    for round in 0..=RELOAD_ROUNDS {
        let settings = build_settings(&sources, &reloaded)?;
        let plan = plan(
            &sources,
            &build.project,
            &settings,
            &build.target,
            &build.users,
            &texts,
            &graph,
            &|name| {
                Ok(super::host_shaders::editor_builtin_shader_names(&build.editor)?.contains(name))
            },
        )?;
        let request = plan.request.encode()?;
        let identity = sha256(format!("{}\0{request}", importer_identity()).as_bytes());
        let files = BuildFiles {
            generated: BTreeMap::from([(
                REQUEST_PATH.to_string(),
                (
                    request.into_bytes(),
                    format!("fileFormatVersion: 2\nguid: {}\n", &identity[..32]).into_bytes(),
                ),
            )]),
            files: Vec::new(),
        };
        let output = sysutil::join(&[&build.output, &format!("build-{round}")]);
        let run = match run(
            ctx,
            build,
            &output,
            &plan.batch,
            &plan.scripts,
            &files,
            &templates,
        ) {
            Ok(run) => run,
            Err(error) => {
                // An object of the pipeline that finds a field empty loads
                // the asset by a path its class states. Nothing in the
                // sources leads there, so the engine's own word is taken.
                match reloaded_path(&error) {
                    Some(path)
                        if round < RELOAD_ROUNDS
                            && !reloaded.contains(&path)
                            && sources.physical(&path).is_some_and(|physical| {
                                stat(&physical).is_ok_and(|info| info.is_regular())
                            }) =>
                    {
                        reloaded.push(path);
                        continue;
                    }
                    _ => return Err(error),
                }
            }
        };
        if !build.reload_list.is_empty()
            && reloaded != read_reload_list(&sources, &build.reload_list)
        {
            mkdir_all(&sysutil::dir(&build.reload_list), 0o700)?;
            // Builds run side by side: each adds what it found to the list
            // as it is now, and a reader never sees a partial file.
            let mut merged = read_reload_list(&sources, &build.reload_list);
            for path in &reloaded {
                if !merged.contains(path) {
                    merged.push(path.clone());
                }
            }
            let partial = format!("{}.{}.partial", build.reload_list, crate::util::random_id());
            write_file(&partial, &serde_json::to_vec_pretty(&merged)?, 0o600)?;
            std::fs::rename(&partial, &build.reload_list)?;
        }
        let file = |name: &str| {
            run.files
                .iter()
                .find(|(file, _)| file == name)
                .map(|(_, data)| data.as_slice())
        };
        let Some(primary) = file("") else {
            bail!("the worker's build left no report");
        };
        let report = read_report(primary)?;
        let Some(player) = file(PLAYER_FILE) else {
            bail!("the worker's build left no file");
        };
        // The pipeline's build code searches the project for volume
        // profiles. What it found has to be what the sources hold.
        let mut found: Vec<&String> = report
            .volume_profiles
            .iter()
            .filter(|path| path.starts_with("Assets/"))
            .collect();
        found.sort();
        let mut expected: Vec<&String> = plan.volume_profiles.iter().collect();
        expected.sort();
        if found != expected {
            bail!(Unsupported(format!(
                "a project whose volume profiles the worker's asset search finds otherwise ({found:?}) than its sources state ({expected:?})"
            )));
        }
        let (objects, names) = read_serialized_with_externals(player)?;
        let mut externals = Vec::with_capacity(names.len());
        let mut external_paths = BTreeMap::new();
        for name in &names {
            let Some(known) = report.externals.iter().find(|known| known.file == *name) else {
                bail!("the written file refers to {name}, which the build's report does not list");
            };
            if let Some(path) = sources.path_of(&known.guid) {
                external_paths.insert(known.guid.clone(), path.to_string());
            }
            externals.push(known.clone());
        }
        // A shader the target names and the worker did not find would be
        // missing from the object without a word.
        for linked in &plan.linked {
            let guid = sources.guid_of(linked).unwrap_or_default();
            if texts
                .iter()
                .any(|(from, text)| linked_shaders_direct(&sources, from, text).contains(linked))
                && !externals.iter().any(|external| external.guid == guid)
                && !report.objects.iter().any(|object| object.guid == guid)
            {
                bail!(
                    "the build of {} does not refer to {linked}, which it names",
                    build.target
                );
            }
        }
        return Ok(PlayerObjects {
            objects,
            externals,
            external_paths,
            report,
            ingredients: plan.ingredients,
            imports: run.imports,
            seconds: started.elapsed().as_secs_f64(),
            database_seconds: run.database_seconds,
        });
    }
    bail!("the build of {} did not settle", build.target)
}

/// The assets a scene refers to, each after what it refers to in turn, the
/// scene last, and the scripts whose classes they need: those of their
/// objects and the importers of those a script imports. Scripts are not
/// among the assets: their assemblies are loaded.
pub(super) fn scene_closure(
    sources: &Sources,
    scene: &str,
    includes: &[String],
) -> Result<SceneClosure> {
    static GUID: std::sync::LazyLock<regex::bytes::Regex> =
        std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"guid: ([0-9a-fA-F]{32})").unwrap());
    struct Walk<'a> {
        sources: &'a Sources,
        seen: BTreeSet<String>,
        order: Vec<String>,
        scripts: BTreeSet<String>,
        importers: BTreeSet<String>,
    }
    impl Walk<'_> {
        fn visit(&mut self, path: &str) -> Result<()> {
            if !self.seen.insert(path.to_string()) {
                return Ok(());
            }
            let Some(physical) = self.sources.physical(path) else {
                bail!(Unsupported(format!(
                    "a scene that refers to {path}, which lies in a package outside the project"
                )));
            };
            if stat(&physical)?.is_dir() {
                return Ok(());
            }
            if let Ok(meta) = read_file(&format!("{physical}.meta"))
                && let Some(importer) = scripted_importer(&meta)
            {
                self.importers.insert(importer);
            }
            // A graph reads its sub-graphs and the files it includes.
            if matches!(lower_ext(path).as_str(), ".shadergraph" | ".shadersubgraph") {
                for source in graph_sources(self.sources, path)? {
                    self.visit(&source)?;
                }
            }
            let data = read_file(&physical)?;
            // A document names its style sheets and templates, and a style
            // sheet the sheets it imports and its fonts and images, by path
            // as often as by GUID. The importer resolves them among the
            // imports made before, so they go first.
            if matches!(lower_ext(path).as_str(), ".uxml" | ".uss" | ".tss") {
                for reference in ui_references(path, &data) {
                    let target = match reference {
                        UiReference::Guid(guid) => self.sources.path_of(&guid),
                        UiReference::Path(path) => self
                            .sources
                            .guid_of(&path)
                            .and_then(|guid| self.sources.path_of(guid)),
                    };
                    if let Some(target) = target.map(str::to_string) {
                        self.visit(&target)?;
                    }
                }
            }
            if data.starts_with(b"%YAML") {
                for capture in GUID.captures_iter(&data) {
                    let guid = String::from_utf8_lossy(&capture[1]).to_ascii_lowercase();
                    let Some(target) = self.sources.path_of(&guid) else {
                        continue;
                    };
                    if lower_ext(target) == ".cs" {
                        self.scripts.insert(guid);
                        continue;
                    }
                    let target = target.to_string();
                    self.visit(&target)?;
                }
            }
            self.order.push(path.to_string());
            Ok(())
        }
    }
    let mut walk = Walk {
        sources,
        seen: BTreeSet::new(),
        order: Vec::new(),
        scripts: BTreeSet::new(),
        importers: BTreeSet::new(),
    };
    // What a build holds beside the scene goes before it.
    for include in includes {
        walk.visit(include)?;
    }
    let order = std::mem::take(&mut walk.order);
    walk.visit(scene)?;
    let mut order_all = order;
    order_all.append(&mut walk.order);
    walk.order = order_all;
    // As the Editor imports them: a model looks its textures up by path and
    // its remapped materials by GUID, and an import of what uses a model
    // refuses one made before them.
    let phase = |path: &String| match lower_ext(path).as_str() {
        ".mat" => 1,
        ".fbx" | ".obj" | ".blend" | ".dae" | ".3ds" | ".max" | ".ma" | ".mb" => 2,
        ".unity"
        | ".prefab"
        | ".asset"
        | ".controller"
        | ".overridecontroller"
        | ".anim"
        | ".mask"
        | ".playable"
        | ".mixer"
        | ".lighting"
        | ".spriteatlasv2"
        | ".terrainlayer"
        | ".physicmaterial"
        | ".physicsmaterial2d"
        | ".rendertexture"
        | ".cubemap"
        | ".flare"
        | ".preset" => 3,
        _ => 0,
    };
    let mut assets = walk.order;
    assets.sort_by_key(phase);
    Ok(SceneClosure {
        assets,
        scripts: walk.scripts,
        importers: walk.importers,
    })
}

/// A source a UI document or style sheet names.
#[derive(Debug, PartialEq, Eq)]
enum UiReference {
    Guid(String),
    /// A project path.
    Path(String),
}

/// What a UXML document (`<Style src>`, `<Template src>`) or a USS or TSS
/// style sheet (`@import`, `url()`) at `path` refers to. A reference is a
/// path relative to the file, a project path (`/Assets/...`) or a
/// `project://database/` URL, which carries the GUID. The engine's own
/// themes (`unity-theme://`) and `resource()` lookups name no source.
fn ui_references(path: &str, data: &[u8]) -> Vec<UiReference> {
    static DOCUMENT: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(
            r#"<(?:[\w.]+:)?(?:Style|Template)\b[^>]*?\s(?:src|path)\s*=\s*"([^"]*)""#,
        )
        .unwrap()
    });
    static SHEET: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(
            r#"(?:\burl\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s]*))\s*\))|(?:@import\s+(?:"([^"]*)"|'([^']*)'))"#,
        )
        .unwrap()
    });
    static GUID: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[?&;]guid=([0-9a-fA-F]{32})").unwrap());
    let pattern = if lower_ext(path) == ".uxml" {
        &*DOCUMENT
    } else {
        &*SHEET
    };
    let mut found = Vec::new();
    for capture in pattern.captures_iter(data) {
        let Some(value) = capture.iter().skip(1).flatten().next() else {
            continue;
        };
        let value = String::from_utf8_lossy(value.as_bytes()).trim().to_string();
        let reference = if let Some(guid) = GUID.captures(&value) {
            UiReference::Guid(guid[1].to_ascii_lowercase())
        } else {
            let value = value.split(['?', '#']).next().unwrap_or_default();
            let value = match value.strip_prefix("project://database/") {
                Some(rest) => rest.replace("%20", " "),
                None if value.contains("://") || value.is_empty() => continue,
                None => value.to_string(),
            };
            let logical = match value.strip_prefix('/') {
                Some(rooted) => sysutil::clean(rooted),
                None if value.starts_with("Assets/") || value.starts_with("Packages/") => {
                    sysutil::clean(&value)
                }
                None => sysutil::clean(&format!("{}/{value}", sysutil::dir(path))),
            };
            if !(logical.starts_with("Assets/") || logical.starts_with("Packages/")) {
                continue;
            }
            UiReference::Path(logical)
        };
        if !found.contains(&reference) {
            found.push(reference);
        }
    }
    found
}

/// What a scene is made of.
pub(super) struct SceneClosure {
    /// The assets in the order a worker imports them, the scene last.
    pub assets: Vec<String>,
    /// The scripts of their objects, by GUID.
    pub scripts: BTreeSet<String>,
    /// The scripted importers of those a script imports, by GUID.
    pub importers: BTreeSet<String>,
}

impl SceneClosure {
    /// The environment the scene is written in.
    pub fn environment(&self, project: &str) -> Result<super::scripted::ScriptEnvironment> {
        super::scripted::scene_environment(project, &self.scripts, &self.importers)
    }
}

/// A scene the Editor's build code wrote, as the Player loads it.
pub struct WrittenScene {
    /// The scene's file.
    pub scene: Vec<u8>,
    /// What it refers to, when written beside it.
    pub dependencies: Option<Vec<u8>>,
    /// The files of that file's streamed data (textures, sounds), by the
    /// name it gives them.
    pub resources: Vec<(String, Vec<u8>)>,
    pub report: Report,
    pub imports: usize,
    pub seconds: f64,
}

/// Has the Editor's build code write a scene as a build writes it, in a
/// standalone worker that imports the scene and everything it refers to,
/// with every assembly of the project's Editor compilation loaded. With
/// `with_dependencies`, what the scene refers to (but the engine's own
/// resources) is written into a second file, which the scene refers to.
pub fn write_scene_objects(
    ctx: &Context,
    build: &Build,
    with_dependencies: bool,
) -> Result<WrittenScene> {
    let started = Instant::now();
    let sources = open_sources(&build.project)?;
    mkdir_all(&build.output, 0o700)?;
    let settings = build_settings(&sources, &[])?;
    let mut batch = Vec::new();
    let closure = scene_closure(&sources, &build.target, &build.users)?;
    for path in settings.batch.iter().chain(&closure.assets).cloned() {
        if !batch.contains(&path) {
            batch.push(path);
        }
    }
    let scenes = vec![build.target.clone()];
    let request = if with_dependencies {
        Request {
            scenes_with_dependencies: scenes,
            ..Default::default()
        }
    } else {
        Request {
            scenes,
            ..Default::default()
        }
    }
    .encode()?;
    let identity = sha256(format!("{}\0{request}", importer_identity()).as_bytes());
    let files = BuildFiles {
        generated: BTreeMap::from([(
            REQUEST_PATH.to_string(),
            (
                request.into_bytes(),
                format!("fileFormatVersion: 2\nguid: {}\n", &identity[..32]).into_bytes(),
            ),
        )]),
        files: Vec::new(),
    };
    batch.push(REQUEST_PATH.to_string());
    let environment = closure.environment(&build.project)?;
    let output = sysutil::join(&[&build.output, "build-0"]);
    let run = run_in(ctx, build, &output, &batch, &environment, &files, &[])?;
    let file = |name: &str| {
        run.files
            .iter()
            .find(|(file, _)| file == name)
            .map(|(_, data)| data.clone())
    };
    let Some(primary) = file("") else {
        bail!("the worker's build left no report");
    };
    let report = read_report(&primary)?;
    let Some(scene) = file(PLAYER_FILE) else {
        bail!("the worker's build left no scene");
    };
    let dependencies = file(DEPENDENCIES_FILE);
    if with_dependencies != dependencies.is_some() {
        bail!("the worker's build left no file of what the scene refers to");
    }
    Ok(WrittenScene {
        scene,
        dependencies,
        resources: Vec::new(),
        report,
        imports: run.imports,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The sources of the shaders a text names itself.
fn linked_shaders_direct(sources: &Sources, _from: &str, text: &[u8]) -> Vec<String> {
    super::baked::linked_shader_names(text)
        .iter()
        .filter_map(|name| match &sources.shaders_named(name).ok()?[..] {
            [one] => Some(one.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_request_in_the_order_the_importer_needs() {
        let request = Request {
            write: vec!["Assets/A.shader".into()],
            users: vec!["Assets/M.mat".into()],
            needs: vec!["Assets/Settings/Pipeline.asset".into()],
            embed: vec!["Packages/p/F.shader".into()],
            only: "UnityEngine.Shader".into(),
            save: Vec::new(),
            settings: vec!["Assets/Settings/Global.asset".into()],
            scenes: vec!["Assets/S.unity".into()],
            scenes_with_dependencies: Vec::new(),
            includes: Vec::new(),
        };
        assert_eq!(
            request.encode().unwrap(),
            "only UnityEngine.Shader\nneed Assets/Settings/Pipeline.asset\nuse Assets/M.mat\nwrite Assets/A.shader\nembed Packages/p/F.shader\nsettings Assets/Settings/Global.asset\nscene Assets/S.unity\n"
        );
        let broken = Request {
            write: vec!["Assets/A\n.shader".into()],
            ..Default::default()
        };
        assert!(broken.encode().is_err());
    }

    /// The primary artifact of a scene request as a 6000.0.71f1 worker
    /// imported it for `Samples/RawProject`: a SerializedFile of format 22
    /// with the importer's object and the report as a TextAsset.
    #[test]
    fn reads_the_report_of_a_6000_0_worker() {
        let artifact = include_bytes!("../../testdata/unity6000_0/scene-request-artifact.bin");
        assert_eq!(artifact[8..12], 22u32.to_be_bytes());
        let report = read_report(artifact).unwrap();
        // The scene's objects and what it refers to: the material, the
        // texture, the built-in shader and the ScriptableObject.
        assert_eq!(report.objects.len(), 17);
        let dependencies: Vec<_> = report
            .dependencies
            .iter()
            .map(|object| (object.guid.as_str(), object.local_id))
            .collect();
        assert!(dependencies.contains(&("a01ac09a24c8493b862564ce94c80304", 2100000)));
        assert!(dependencies.contains(&("a01ac09a24c8493b862564ce94c80303", 11400000)));
    }

    /// An object read from a file of format 22 is written back in that
    /// format, as a 6000.0 Player reads it, and in format 23 as before.
    #[test]
    fn writes_an_object_of_a_6000_0_file_in_its_format() {
        let artifact = include_bytes!("../../testdata/unity6000_0/scene-request-artifact.bin");
        let read = playerbake::read_serialized_of_class(artifact, 49).unwrap();
        let [report] = &read[..] else {
            panic!("one TextAsset")
        };
        let objects = [playerbake::Object {
            local_id: report.local_id,
            tree: &report.tree,
            data: report.data.clone(),
            script: None,
        }];
        let format = crate::engine::supported("6000.0.71f1")
            .unwrap()
            .serialized_format;
        let file = playerbake::write_in(format, &objects, &[]).unwrap();
        assert_eq!(file[8..12], 22u32.to_be_bytes());
        assert_eq!(&file[48..60], b"6000.0.71f1\0");
        let again = playerbake::read_serialized_of_class(&file, 49).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(
            (again[0].local_id, &again[0].data),
            (report.local_id, &report.data)
        );
        assert_eq!(again[0].tree.entry, report.tree.entry);
        // The text the worker wrote reads the same out of the copy.
        let text = |object: &playerbake::Read| object.tree.decode(&object.data).unwrap();
        assert_eq!(text(&again[0]), text(report));
        // The verified release's format is the writer's default.
        let verified = playerbake::write_in(playerbake::VERIFIED_FORMAT, &objects, &[]).unwrap();
        assert_eq!(verified, playerbake::write(&objects, &[]).unwrap());
        assert_eq!(verified[8..12], 23u32.to_be_bytes());
    }

    #[test]
    fn compiles_the_importer_with_the_sdk() {
        let Some(editor) = std::env::var("UME_LIFECYCLE_SDK")
            .ok()
            .filter(|path| !path.is_empty())
        else {
            eprintln!("skipped: set UME_LIFECYCLE_SDK; only the installed .NET SDK is used");
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().to_str().unwrap();
        // The context names the release the importer is compiled for.
        let version = crate::release::installed_version(&editor).unwrap();
        std::fs::create_dir(directory.path().join("ProjectSettings")).unwrap();
        std::fs::write(
            directory.path().join("ProjectSettings/ProjectVersion.txt"),
            format!("m_EditorVersion: {version}\n"),
        )
        .unwrap();
        stage_importer(&Context::background(), &editor, project).unwrap();
        let assembly = std::fs::read(sysutil::join(&[
            project,
            "Library/ScriptAssemblies",
            &format!("{IMPORTER_ASSEMBLY}.dll"),
        ]))
        .unwrap();
        // The class, and the extension the worker picks it by.
        for recorded in [&b"PlayerObjectImporter\0"[..], b"umeplayer"] {
            assert!(
                assembly
                    .windows(recorded.len())
                    .any(|window| window == recorded)
            );
        }
    }

    /// Zeroes every reference to another object: where a reference leads
    /// depends on the file an object is written into.
    fn without_pointers(value: &mut Val) {
        match value {
            Val::Map(fields) => {
                let pointer =
                    fields.len() == 2 && fields[0].0 == "m_FileID" && fields[1].0 == "m_PathID";
                for (_, field) in fields.iter_mut() {
                    if pointer {
                        *field = Val::Zero;
                    } else {
                        without_pointers(field);
                    }
                }
            }
            Val::Seq(items) => items.iter_mut().for_each(without_pointers),
            _ => {}
        }
    }

    /// Compares what the worker's build writes with objects of an Editor
    /// build: `UME_PLAYER_BUILD_PROJECT` is a project with a scriptable
    /// render pipeline that an Editor has compiled, `UME_PLAYER_BUILD_CASES`
    /// a list of `<asset path>=<file of the object an Editor build wrote>`
    /// separated by commas. The materials are those of the project that name
    /// the asset. Each object has to equal its reference in every byte but
    /// the references to other files.
    #[test]
    fn builds_player_objects_like_the_editor() {
        let variable = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let (Some(editor), Some(project), Some(cases)) = (
            variable("UME_LIFECYCLE_SDK"),
            variable("UME_PLAYER_BUILD_PROJECT"),
            variable("UME_PLAYER_BUILD_CASES"),
        ) else {
            eprintln!(
                "skipped: set UME_LIFECYCLE_SDK, UME_PLAYER_BUILD_PROJECT and UME_PLAYER_BUILD_CASES; starts import workers of the installed Editor"
            );
            return;
        };
        let sources = open_sources(&project).unwrap();
        let root = sysutil::join(&[
            &project,
            "Library/UnityMicroEditor/player-build-test",
            &crate::util::random_id(),
        ]);
        let mut failures = Vec::new();
        for (index, case) in cases.split(',').enumerate() {
            let (target, references) = case.split_once('=').expect("case is <asset>=<file>");
            let mut references = references.split('+');
            let reference = references.next().unwrap();
            let guid = sources
                .guid_of(target)
                .expect("target has a .meta")
                .to_string();
            let mut users = Vec::new();
            fn materials(
                directory: &std::path::Path,
                guid: &str,
                users: &mut Vec<String>,
                root: &str,
            ) {
                let mut entries: Vec<_> = std::fs::read_dir(directory).unwrap().flatten().collect();
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let path = entry.path();
                    if path.is_dir() {
                        materials(&path, guid, users, root);
                    } else if path.extension().is_some_and(|extension| extension == "mat")
                        && std::fs::read(&path)
                            .unwrap()
                            .windows(guid.len())
                            .any(|window| window == guid.as_bytes())
                    {
                        users.push(path.to_str().unwrap()[root.len() + 1..].to_string());
                    }
                }
            }
            materials(
                std::path::Path::new(&sysutil::join(&[&project, "Assets"])),
                &guid,
                &mut users,
                &project,
            );
            let built = write_player_objects(
                &Context::background(),
                &Build {
                    project: project.clone(),
                    editor: editor.clone(),
                    output: sysutil::join(&[&root, &index.to_string()]),
                    target: target.to_string(),
                    users: users.clone(),
                    timeout: Duration::from_secs(3000),
                    reload_list: sysutil::join(&[&root, "reloaded.json"]),
                    shader_cache: variable("UME_PLAYER_BUILD_SHADER_CACHE").unwrap_or_default(),
                },
            );
            let built = match built {
                Ok(built) => built,
                Err(error) => {
                    failures.push(format!("{target}: {error:#}"));
                    continue;
                }
            };
            let expected = std::fs::read(reference).unwrap();
            let [object] = &built.objects_of(&guid)[..] else {
                failures.push(format!("{target}: {} objects", built.objects.len()));
                continue;
            };
            let mask = |data: &[u8]| {
                let mut value = object.tree.decode(data).unwrap();
                without_pointers(&mut value);
                object.tree.encode(&value).unwrap()
            };
            let equal = object.data == expected;
            let alike = equal
                || (object.data.len() == expected.len() && mask(&object.data) == mask(&expected));
            eprintln!(
                "{target}: {} bytes, {} materials, {} imports, {:.1} s ({:.2} s in the ArtifactDB), {} references, {}",
                object.data.len(),
                users.len(),
                built.imports,
                built.seconds,
                built.database_seconds,
                built.externals.len(),
                if equal {
                    "identical"
                } else if alike {
                    "identical but for references"
                } else {
                    "DIFFERENT"
                }
            );
            if object.tree.class_id == 48 {
                eprintln!(
                    "  programs: {}, objects in the file: {}",
                    playerbake::verify_shader_programs(&object.tree, &object.data).unwrap(),
                    built.objects.len()
                );
            }
            // The shaders of packages the file holds beside it: each has to
            // be one of the further reference objects of the case.
            let others: Vec<Vec<u8>> = references
                .map(|file| std::fs::read(file).unwrap())
                .collect();
            for other in built
                .objects
                .iter()
                .filter(|other| other.local_id != object.local_id)
            {
                let mask = |data: &[u8]| {
                    let mut value = other.tree.decode(data).unwrap();
                    without_pointers(&mut value);
                    other.tree.encode(&value).unwrap()
                };
                let found = others.iter().any(|expected| {
                    expected.len() == other.data.len() && mask(expected) == mask(&other.data)
                });
                eprintln!(
                    "  held object {}: {} bytes, {}",
                    other.local_id,
                    other.data.len(),
                    if found {
                        "identical but for references"
                    } else {
                        "NOT AMONG THE REFERENCES"
                    }
                );
                if !found {
                    failures.push(format!("{target}: held object {} differs", other.local_id));
                }
            }
            if !alike {
                std::fs::write(
                    sysutil::join(&[&root, &format!("{index}.bin")]),
                    &object.data,
                )
                .unwrap();
                failures.push(format!("{target}: differs from {reference}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    fn source(root: &std::path::Path, path: &str, data: &str, guid: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, data).unwrap();
        std::fs::write(
            format!("{}.meta", file.display()),
            format!("fileFormatVersion: 2\nguid: {guid}\n"),
        )
        .unwrap();
    }

    #[test]
    fn ui_documents_and_style_sheets_name_their_sources_by_path() {
        let document = br#"<ui:UXML xmlns:ui="UnityEngine.UIElements">
    <Style src="Terminal.uss" />
    <Style src="../../UI/Shared.uss" />
    <ui:Template src="/Assets/UI/Row.uxml" name="row" />
    <Style src="project://database/Assets/UI/Theme%20A.uss?fileID=7433441132597879392&amp;guid=ABCDEFABCDEFABCDEFABCDEFABCDEFAB&amp;type=3#Theme" />
    <ui:Label text="src=&quot;No.uss&quot;" />
</ui:UXML>"#;
        assert_eq!(
            ui_references("Assets/Game/Net/Hud.uxml", document),
            vec![
                UiReference::Path("Assets/Game/Net/Terminal.uss".into()),
                UiReference::Path("Assets/UI/Shared.uss".into()),
                UiReference::Path("Assets/UI/Row.uxml".into()),
                UiReference::Guid("abcdefabcdefabcdefabcdefabcdefab".into()),
            ]
        );
        let sheet = br#"@import url("unity-theme://default");
@import "Base.uss";
.a { -unity-font-definition: url("Fonts/VT323.ttf"); background-image: url('/Assets/Art/Icon.png'); }
.b { background-image: resource("Icons/Gear"); -unity-font: url(Fonts/VT323.ttf); }
"#;
        assert_eq!(
            ui_references("Assets/UI/Terminal.uss", sheet),
            vec![
                UiReference::Path("Assets/UI/Base.uss".into()),
                UiReference::Path("Assets/UI/Fonts/VT323.ttf".into()),
                UiReference::Path("Assets/Art/Icon.png".into()),
            ]
        );
    }

    #[test]
    fn a_scene_takes_the_style_sheets_its_documents_name_before_them() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let guid = |index: u8| format!("{index:02x}").repeat(16);
        source(root, "Assets/UI/Fonts/Mono.ttf", "", &guid(1));
        source(
            root,
            "Assets/UI/Terminal.uss",
            ".t { -unity-font-definition: url(\"Fonts/Mono.ttf\"); }\n",
            &guid(2),
        );
        source(
            root,
            "Assets/UI/Hud.uxml",
            "<ui:UXML xmlns:ui=\"UnityEngine.UIElements\">\n  <Style src=\"Terminal.uss\" />\n</ui:UXML>\n",
            &guid(3),
        );
        source(
            root,
            "Assets/UI/Theme.tss",
            "@import url(\"unity-theme://default\");\n",
            &guid(4),
        );
        let scene = format!(
            "%YAML 1.1\n--- !u!114 &1\nMonoBehaviour:\n  sourceAsset: {{fileID: 9197481963319205126, guid: {}, type: 3}}\n  themeUss: {{fileID: -4733365628477956816, guid: {}, type: 3}}\n",
            guid(3),
            guid(4)
        );
        source(root, "Assets/Scenes/Boot.unity", &scene, &guid(5));
        std::fs::create_dir_all(root.join("Packages")).unwrap();
        std::fs::write(root.join("Packages/manifest.json"), "{}").unwrap();
        let project = root.to_str().unwrap();
        let sources = Sources::open(project, project, &BTreeMap::new()).unwrap();
        let closure = scene_closure(&sources, "Assets/Scenes/Boot.unity", &[]).unwrap();
        assert_eq!(
            closure.assets,
            [
                "Assets/UI/Fonts/Mono.ttf",
                "Assets/UI/Terminal.uss",
                "Assets/UI/Hud.uxml",
                "Assets/UI/Theme.tss",
                "Assets/Scenes/Boot.unity",
            ]
        );
    }

    #[test]
    fn plans_a_build_from_what_the_shader_includes_names_and_is_used_by() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let guid = |index: u8| format!("{index:02x}").repeat(16);
        let package = "Library/PackageCache/com.unity.render-pipelines.universal@1";
        source(
            root,
            &format!("{package}/ShaderLibrary/Core.hlsl"),
            "#include \"Packages/com.unity.render-pipelines.universal/ShaderLibrary/Input.hlsl\"\n",
            &guid(1),
        );
        source(
            root,
            &format!("{package}/ShaderLibrary/Input.hlsl"),
            "float4 a;\n",
            &guid(2),
        );
        source(
            root,
            &format!("{package}/Shaders/FallbackError.shader"),
            "Shader \"Hidden/Universal Render Pipeline/FallbackError\" { }\n",
            &guid(3),
        );
        source(root, "Assets/S/Tint.hlsl", "float4 b;\n", &guid(4));
        source(
            root,
            "Assets/S/Other.shader",
            "Shader \"Game/Other\" { }\n",
            &guid(5),
        );
        let shader = "Shader \"Game/Lit\" {\n  SubShader { UsePass \"Game/Other/MAIN\" Pass { HLSLPROGRAM\n#include \"Packages/com.unity.render-pipelines.universal/ShaderLibrary/Core.hlsl\"\n#include \"Tint.hlsl\"\n// #include \"Gone.hlsl\"\n#include \"UnityCG.cginc\"\nENDHLSL } }\n  Fallback \"Hidden/Universal Render Pipeline/FallbackError\"\n}\n";
        source(root, "Assets/S/Lit.shader", shader, &guid(6));
        source(
            root,
            "Assets/M/Base.mat",
            "%YAML 1.1\nMaterial:\n  m_Parent: {fileID: 0}\n",
            &guid(7),
        );
        let variant = format!(
            "%YAML 1.1\nMaterial:\n  m_Parent: {{fileID: 2100000, guid: {}, type: 2}}\n",
            guid(7)
        );
        source(root, "Assets/M/Variant.mat", &variant, &guid(8));
        std::fs::create_dir_all(root.join("Packages")).unwrap();
        std::fs::write(root.join("Packages/manifest.json"), "{}").unwrap();
        let project = root.to_str().unwrap();
        let packages = BTreeMap::from([(
            "Packages/com.unity.render-pipelines.universal".to_string(),
            package.to_string(),
        )]);
        let sources = Sources::open(project, project, &packages).unwrap();
        let settings = build_settings(&sources, &[]).unwrap();
        let users = vec!["Assets/M/Variant.mat".to_string()];
        let built = plan(
            &sources,
            project,
            &settings,
            "Assets/S/Lit.shader",
            &users,
            &[("Assets/S/Lit.shader", shader.as_bytes())],
            &[],
            &|_| Ok(false),
        )
        .unwrap();
        // Included files first, then the shaders it names, itself, the
        // parent of the variant and the variant, and the request last.
        assert_eq!(
            built.batch,
            [
                "Assets/S/Tint.hlsl",
                "Packages/com.unity.render-pipelines.universal/ShaderLibrary/Core.hlsl",
                "Packages/com.unity.render-pipelines.universal/ShaderLibrary/Input.hlsl",
                "Assets/S/Other.shader",
                "Packages/com.unity.render-pipelines.universal/Shaders/FallbackError.shader",
                "Assets/S/Lit.shader",
                "Assets/M/Base.mat",
                "Assets/M/Variant.mat",
                REQUEST_PATH,
            ]
        );
        // The shader of the package goes into the file; the one of the
        // project is published on its own.
        assert_eq!(
            built.request.embed,
            ["Packages/com.unity.render-pipelines.universal/Shaders/FallbackError.shader"]
        );
        assert_eq!(built.request.needs, ["Assets/M/Base.mat"]);
        assert_eq!(built.request.only, "");
        // Only what can change is guarded: the files of the project.
        let guarded: Vec<&str> = built.ingredients.keys().map(String::as_str).collect();
        assert_eq!(
            guarded,
            [
                "Assets/M/Base.mat",
                "Assets/M/Base.mat.meta",
                "Assets/M/Variant.mat",
                "Assets/M/Variant.mat.meta",
                "Assets/S/Other.shader",
                "Assets/S/Other.shader.meta",
                "Assets/S/Tint.hlsl",
                "Assets/S/Tint.hlsl.meta",
                "Packages/manifest.json",
            ]
        );

        // A name no source declares stands for a shader built into the
        // engine, which is not delivered.
        let builtin = "Shader \"Game/B\" { Fallback \"VertexLit\" }";
        let refused = plan(
            &sources,
            project,
            &settings,
            "Assets/S/Lit.shader",
            &[],
            &[("Assets/S/Lit.shader", builtin.as_bytes())],
            &[],
            &|name| Ok(name == "VertexLit"),
        )
        .unwrap_err();
        assert!(refused.is::<Unsupported>());
        assert!(format!("{refused:#}").contains("VertexLit"));

        // A built-in shader whose passes alone are used is left to the
        // worker; one that is not built in is refused.
        let passes = "Shader \"Game/B\" { SubShader { UsePass \"Hidden/Nature/Terrain/Utilities/PICKING\" } }";
        let texts = [("Assets/S/Lit.shader", passes.as_bytes())];
        let used = |known: &'static str| {
            plan(
                &sources,
                project,
                &settings,
                "Assets/S/Lit.shader",
                &[],
                &texts,
                &[],
                &move |name| Ok(name == known),
            )
        };
        assert!(
            used("Hidden/Nature/Terrain/Utilities")
                .unwrap()
                .linked
                .is_empty()
        );
        assert!(used("VertexLit").unwrap_err().is::<Unsupported>());
    }

    #[test]
    fn takes_the_asset_a_pipeline_object_loads_by_path_from_the_engines_message() {
        let stopped = anyhow!(
            "native importer diagnostic: Exception: Cannot load. Incorrect path: Packages/com.unity.render-pipelines.universal/Shaders/Debug/DebugReplacement.shader\n  at UnityEngine.Rendering.ResourceReloader.TryReloadAllNullIn (System.Object container, System.String basePath)"
        );
        assert_eq!(
            reloaded_path(&stopped).as_deref(),
            Some(
                "Packages/com.unity.render-pipelines.universal/Shaders/Debug/DebugReplacement.shader"
            )
        );
        assert_eq!(
            reloaded_path(&anyhow!("native importer error 3: failed")),
            None
        );
        // A path outside the sources is not one to stage.
        assert_eq!(
            reloaded_path(&anyhow!("Cannot load. Incorrect path: /etc/passwd")),
            None
        );
    }

    #[test]
    fn reads_a_report_and_the_reason_of_a_refusal() {
        let report = parse_report(
            "ume-player-objects 1\nwritten 2cbf 1 Assets/A.shader\nquality 0 none\nquality 1 Assets/Settings/PC.asset\nbuild-pipeline Assets/Settings/PC.asset\npipeline Assets/Settings/PC.asset\ncallback-unconstructed UnityEditor.Rendering.Universal.UpdateShaderPrefilteringDataBeforeBuild InvalidOperationException: restricted\nshader-message Warning OpenGLCore Assets/A.shader:3 unused\nshader-preprocessor UnityEditor.Rendering.ShaderVariantStripper, Unity.RenderPipelines.Core.Editor\nvolume-profile 0123 Assets/Settings/Profile.asset\nobject 4800000 UnityEngine.Shader 2cbfcd0349c944ee788f6e2919b9d66c 4800000\nexternal ume-external/0000000000000000f000000000000000/0/10 10 UnityEngine.Texture2D Resources/unity_builtin_extra\nexternal ume-external/8d2a7b3e19e2414b96a1705d15c573b2/3/4800000 4800000 UnityEngine.Shader Assets/B.shader\nfile objects.player 1 objects.player\ndone\n",
        )
        .unwrap();
        assert_eq!(report.objects.len(), 1);
        assert_eq!(report.objects[0].index, 4_800_000);
        assert_eq!(report.objects[0].class_name, "UnityEngine.Shader");
        assert_eq!(report.objects[0].guid, "2cbfcd0349c944ee788f6e2919b9d66c");
        assert_eq!(report.quality_pipelines, ["", "Assets/Settings/PC.asset"]);
        assert_eq!(report.build_pipelines, ["Assets/Settings/PC.asset"]);
        assert_eq!(report.unconstructed.len(), 1);
        assert_eq!(report.messages.len(), 1);
        assert_eq!(report.volume_profiles, ["Assets/Settings/Profile.asset"]);
        assert_eq!(report.externals.len(), 2);
        assert_eq!(report.externals[0].file_type, 0);
        assert_eq!(report.externals[1].guid, "8d2a7b3e19e2414b96a1705d15c573b2");
        assert_eq!(report.externals[1].index, 4_800_000);
        assert_eq!(report.shader_preprocessors.len(), 1);

        let refused = parse_report(
            "ume-player-objects 1\nerror InvalidOperationException: the worker cannot load Assets/A.shader\n",
        )
        .unwrap_err();
        assert!(format!("{refused:#}").contains("the worker cannot load Assets/A.shader"));
        assert!(parse_report("ume-player-objects 1\nobject 1 UnityEngine.Shader a 1\n").is_err());
        // What a run that only saves reports: the Editor's text of each asset.
        let saved = parse_report(
            "ume-player-objects 1\nsaved-source 22 1 Packages/p/A B.mat\nsaved JVlBTUwgMS4x Packages/p/A B.mat\ndone\n",
        )
        .unwrap();
        assert_eq!(
            saved.saved,
            [("Packages/p/A B.mat".to_string(), b"%YAML 1.1".to_vec())]
        );
        assert!(parse_report("ume-player-objects 1\nsaved %%% A.mat\ndone\n").is_err());
        assert_eq!(
            Request {
                save: vec!["Packages/p/A.mat".into()],
                ..Default::default()
            }
            .encode()
            .unwrap(),
            "save Packages/p/A.mat\n"
        );
        assert!(parse_report("something else\n").is_err());
    }
}
