//! Play mode code: the runtime assemblies compiled as the Editor's Play mode
//! compiles them, with the Editor's symbols (`UNITY_EDITOR`) and against the
//! Editor's engine and Editor modules, and linked into a Player.
//!
//! A game often keeps what it does in the Editor under `#if UNITY_EDITOR`:
//! a consent or ads SDK answers at once instead of calling its device's
//! native library. A Player compiled as a Player build compiles takes the
//! device's path and stops there; Play mode code takes the Editor's.
//!
//! Such code names the Editor's assemblies and members of the engine only
//! the Editor's engine has. The linker (`tools/PlayModeLinker`) writes
//! stand-ins of the Editor's assemblies into the Player, with exactly the
//! types and members the code names; it calls an engine member only the
//! Editor has through a stand-in of its own; and it aligns the serialized
//! fields that only one kind of build declares with the host's build, whose
//! data the Player reads without type trees.
use super::{
    Compiler, NilMap,
    nilable::nz,
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

/// What the Editor compiles its assemblies against: an assembly for the
/// Editor platform alone against the .NET Framework 4.8 API and every engine
/// and Editor module; any other against the Player's API, the Editor's
/// builds of the modules the project enables and every Editor module. All
/// get the scripting bootstrap, the Graphs assembly and the extensions of
/// the installed platforms.
#[derive(Debug, Clone, Default)]
pub struct EditorReferences {
    pub framework: Vec<String>,
    /// The BCL extensions' targeting pack, which Bee gives every assembly
    /// (`System.Runtime.CompilerServices.Unsafe` among them).
    pub extensions: Vec<String>,
    pub editor_modules: Vec<String>,
    pub runtime_modules: Vec<String>,
    pub extras: Vec<String>,
}

/// The Editor's references for a project whose Player references the
/// engine modules `engine`.
pub fn editor_references(editor: &str, engine: &[String]) -> EditorReferences {
    let data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
    let api = sysutil::join(&[&data, "UnityReferenceAssemblies/unity-4.8-api"]);
    let mut framework: Vec<String> = FRAMEWORK
        .iter()
        .map(|name| sysutil::join(&[&api, name]))
        .collect();
    framework.extend(super::sys_glob(&sysutil::join(&[&api, "Facades/*.dll"])));
    let extensions = super::sys_glob(&sysutil::join(&[
        &data,
        "BCLExtensions/TargetingPacks/netstandard2.1/ref/*.dll",
    ]));
    framework.extend(extensions.iter().cloned());
    let modules = sysutil::join(&[&data, "Managed/UnityEngine"]);
    let all: Vec<String> = super::sys_glob(&sysutil::join(&[&modules, "UnityE*.dll"]))
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
    // The scripting bootstrap is an assembly of its own from 6000.6 on;
    // 6000.0 has none.
    let mut extras: Vec<String> = [
        "Managed/Bootstrap/Unity.Scripting.dll",
        "Managed/UnityEditor.Graphs.dll",
    ]
    .iter()
    .map(|name| sysutil::join(&[&data, name]))
    .filter(|path| std::path::Path::new(path).is_file())
    .collect();
    extras.extend(super::sys_glob(&sysutil::join(&[
        &data,
        "PlaybackEngines/*/UnityEditor.*.Extensions.dll",
    ])));
    // 6000.0's Editor also gives its assemblies the other extension
    // assemblies of the installed platform modules (the iOS `Xcode` and
    // `Common` ones, Android's Gradle types), as its Bee input lists them.
    if !std::path::Path::new(&sysutil::join(&[
        &data,
        "Managed/Bootstrap/Unity.Scripting.dll",
    ]))
    .is_file()
    {
        for pattern in [
            "PlaybackEngines/*/UnityEditor.*.Extensions.*.dll",
            "PlaybackEngines/AndroidPlayer/Unity.Android.Gradle.dll",
            "PlaybackEngines/AndroidPlayer/Unity.Android.Types.dll",
        ] {
            extras.extend(super::sys_glob(&sysutil::join(&[&data, pattern])));
        }
    }
    EditorReferences {
        framework,
        extensions,
        editor_modules: all,
        runtime_modules,
        extras,
    }
}

/// Where a prepared Player keeps the host's builds of the assemblies Play
/// mode code replaces, whose serialized layout the host's data has.
pub const LAYOUT_DIRECTORY: &str = "UnityMicroEditorLayout";

/// The host's build of an assembly of the Player `player`, if it has one.
pub fn host_layout(player: &str, name: &str) -> Option<String> {
    let path = sysutil::join(&[
        &super::player_data(player),
        LAYOUT_DIRECTORY,
        &format!("{name}.dll"),
    ]);
    std::path::Path::new(&path).is_file().then_some(path)
}

/// Keeps the host's builds of the assemblies `names` that the staged Player
/// `player` holds, before Play mode code replaces them.
pub fn keep_host_layouts<'a>(player: &str, names: impl Iterator<Item = &'a str>) -> Result<()> {
    let managed = super::player_managed(player);
    let directory = sysutil::join(&[&super::player_data(player), LAYOUT_DIRECTORY]);
    sys::mkdir_all(&directory)?;
    for name in names {
        let file = format!("{}.dll", name.strip_suffix(".dll").unwrap_or(name));
        let source = sysutil::join(&[&managed, &file]);
        let destination = sysutil::join(&[&directory, &file]);
        if std::path::Path::new(&source).is_file() && sys::absent(&destination) {
            sys::write(&destination, &sys::read(&source)?)?;
        }
    }
    Ok(())
}

/// The Linux Player's own engine modules, as the installation ships them.
fn engine_modules(editor: &str) -> String {
    sysutil::join(&[
        &sysutil::dir(editor),
        "Data/PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed",
    ])
}

/// The directories of the Editor's own assemblies.
fn editor_directories(editor: &str) -> Vec<String> {
    let known = editor_references(editor, &[]);
    let mut directories: Vec<String> = known
        .editor_modules
        .iter()
        .chain(known.extras.iter())
        .map(|path| sysutil::dir(path))
        .collect();
    directories.push(sysutil::join(&[&sysutil::dir(editor), "Data/Managed"]));
    directories.sort();
    directories.dedup();
    directories
}

#[derive(Serialize)]
struct Request<'a> {
    mode: &'a str,
    engine: String,
    managed: &'a str,
    input: &'a str,
    output: &'a str,
    layout: &'a str,
    #[serde(rename = "editorDirectories")]
    editor_directories: Vec<String>,
    support: &'a str,
}

/// What the linker did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkReport {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    pub shims: Vec<String>,
    #[serde(rename = "editorTypes")]
    pub editor_types: i64,
    #[serde(rename = "editorMembers")]
    pub editor_members: i64,
    #[serde(rename = "supportedMembers")]
    pub supported_members: i64,
    #[serde(rename = "engineStubs")]
    pub engine_stubs: Vec<String>,
    #[serde(rename = "movedEngineTypes")]
    pub moved_engine_types: Vec<String>,
    #[serde(rename = "notSerialized")]
    pub not_serialized: Vec<String>,
    #[serde(rename = "addedFields")]
    pub added_fields: Vec<String>,
    #[serde(rename = "layoutDifferences")]
    pub layout_differences: Vec<String>,
    pub unsupported: Vec<String>,
    pub unresolved: Vec<String>,
}

const LINKER: &str = "PlayModeLinker.dll";

impl Compiler {
    /// What an assembly for the Editor alone compiles against in Play mode
    /// code, as the Editor compiles it: the .NET Framework API and every
    /// engine and Editor module, besides the extras every assembly gets.
    pub(crate) fn editor_only_references(&self) -> (Vec<String>, Vec<String>) {
        let known = editor_references(&self.editor, &[]);
        let mut engine = known.editor_modules.clone();
        engine.extend(known.extras.iter().cloned());
        (known.framework, engine)
    }

    /// The linker, built once per source and SDK into the project's store.
    pub(crate) fn play_mode_linker(&self, ctx: &Context) -> Result<String> {
        if let Some(linker) = self.play_mode_linker.lock().unwrap().clone() {
            return Ok(linker);
        }
        let sources = sysutil::join(&[&micro_editor_root()?, "tools/PlayModeLinker"]);
        // Unity's own Cecil, which IL2CPP ships with every installation.
        let cecil = sysutil::join(&[&sysutil::dir(&self.editor), "Data/il2cpp/build/deploy"]);
        let mut inputs: BTreeMap<String, String> = BTreeMap::new();
        for name in ["PlayModeLinker.csproj", "Program.cs"] {
            inputs.insert(name.into(), file_hash(&sysutil::join(&[&sources, name]))?);
        }
        inputs.insert(
            "Mono.Cecil.dll".into(),
            file_hash(&sysutil::join(&[&cecil, "Mono.Cecil.dll"]))?,
        );
        inputs.insert("dotnet".into(), file_hash(&self.dotnet)?);
        let key = sha256(&marshal(&inputs)?);
        let entry = sysutil::join(&[&self.stored("play-mode-linker"), &key]);
        let valid = |entry: &str| {
            read_json_file::<NilMap<String>>(&sysutil::join(&[entry, "files.json"])).is_ok_and(
                |files| {
                    !files.at(LINKER).is_empty()
                        && files.iter().all(|(name, hash)| {
                            file_hash(&sysutil::join(&[entry, name]))
                                .is_ok_and(|actual| actual == *hash)
                        })
                },
            )
        };
        if !valid(&entry) {
            sys::mkdir_all(&sysutil::dir(&entry))?;
            let staging = sys::mkdir_temp(&sysutil::dir(&entry), ".linker-build-")?;
            for name in ["PlayModeLinker.csproj", "Program.cs"] {
                let payload = sys::read(&sysutil::join(&[&sources, name]))?;
                if sha256(&payload) != inputs[name] {
                    bail!("the Play mode linker's source changed while it was built: {name}");
                }
                sys::write(&sysutil::join(&[&staging, name]), &payload)?;
            }
            sys::write(
                &sysutil::join(&[&staging, "NuGet.Config"]),
                b"<configuration><packageSources><clear /></packageSources></configuration>\n",
            )?;
            let payload = sysutil::join(&[&staging, "payload"]);
            compiler_command(
                ctx,
                &staging,
                &self.dotnet,
                &[
                    "build",
                    &sysutil::join(&[&staging, "PlayModeLinker.csproj"]),
                    "--nologo",
                    "-v:q",
                    "-o",
                    &payload,
                    "-p:NuGetAudit=false",
                    &format!("-p:CecilPath={cecil}"),
                    sys::BUILD_WITHOUT_SERVERS[0],
                    sys::BUILD_WITHOUT_SERVERS[1],
                ],
            )
            .map_err(|error| anyhow!("build the Play mode linker: {error}"))?;
            let mut files = NilMap::make();
            for name in sys::regular_files(&payload)? {
                files.insert(name.clone(), file_hash(&sysutil::join(&[&payload, &name]))?);
            }
            if files.at(LINKER).is_empty() {
                bail!("the Play mode linker's build produced no {LINKER}");
            }
            atomic_json(&sysutil::join(&[&payload, "files.json"]), &files)?;
            sys::publish(&payload, &entry, || valid(&entry))?;
            let _ = std::fs::remove_dir_all(&staging);
        }
        let linker = sysutil::join(&[&entry, LINKER]);
        *self.play_mode_linker.lock().unwrap() = Some(linker.clone());
        Ok(linker)
    }

    fn run_linker(&self, ctx: &Context, folder: &str, request: &Request) -> Result<LinkReport> {
        let linker = self.play_mode_linker(ctx)?;
        sys::mkdir_all(folder)?;
        let file = sysutil::join(&[folder, &format!("play-mode-{}.json", request.mode)]);
        atomic_json(&file, request)?;
        let output = compiler_command(ctx, folder, &self.dotnet, &[&linker, &file])
            .map_err(|error| anyhow!("Play mode linker ({}): {error}", request.mode))?;
        let report: LinkReport = serde_json::from_slice(&output)
            .map_err(|error| anyhow!("the Play mode linker's report: {error}"))?;
        if report.schema_version != 1 {
            bail!("the Play mode linker returned no report");
        }
        sys::write(
            &sysutil::join(&[folder, &format!("play-mode-{}-report.json", request.mode)]),
            &output,
        )?;
        Ok(report)
    }

    /// Links one compiled Play mode assembly `input` into `output` for the
    /// Player `self.player`, aligned with the host's build of it.
    pub(crate) fn link_play_mode_assembly(
        &self,
        ctx: &Context,
        folder: &str,
        name: &str,
        input: &str,
        output: &str,
    ) -> Result<LinkReport> {
        let layout = host_layout(&self.player, name).unwrap_or_default();
        let report = self.run_linker(
            ctx,
            folder,
            &Request {
                mode: "link",
                engine: engine_modules(&self.editor),
                managed: &super::player_managed(&self.player),
                input,
                output,
                layout: &layout,
                editor_directories: editor_directories(&self.editor),
                support: "",
            },
        )?;
        for difference in &report.layout_differences {
            eprintln!("[unity-micro-editor] Play mode layout of {name}: {difference}");
        }
        for unsupported in &report.unsupported {
            eprintln!("[unity-micro-editor] Play mode code of {name} keeps {unsupported}");
        }
        Ok(report)
    }

    /// The identity of the link of an assembly: the linker and the host's
    /// build it is aligned with.
    pub(crate) fn play_mode_link_identity(&self, ctx: &Context, name: &str) -> Result<String> {
        let linker = self.play_mode_linker(ctx)?;
        let layout = match host_layout(&self.player, name) {
            Some(path) => file_hash(&path)?,
            None => String::new(),
        };
        Ok(sha256(
            format!("{}\0{layout}", file_hash(&linker)?).as_bytes(),
        ))
    }

    /// Writes the stand-ins of the assemblies the code of the Player whose
    /// managed directory is `managed` names and the Player lacks.
    pub fn write_play_mode_stand_ins(
        &self,
        ctx: &Context,
        folder: &str,
        managed: &str,
    ) -> Result<LinkReport> {
        let report = self.run_linker(
            ctx,
            folder,
            &Request {
                mode: "stand-ins",
                engine: engine_modules(&self.editor),
                managed,
                input: "",
                output: "",
                layout: "",
                editor_directories: editor_directories(&self.editor),
                support: "UnityMicroEditor",
            },
        )?;
        eprintln!(
            "[unity-micro-editor] Play mode stand-ins {}: {} types, {} members ({} answered as in Play mode)",
            report.shims.join(", "),
            report.editor_types,
            report.editor_members,
            report.supported_members
        );
        for unresolved in &report.unresolved {
            eprintln!(
                "[unity-micro-editor] Play mode code names what no stand-in has: {unresolved}"
            );
        }
        Ok(report)
    }

    /// The stand-ins of the newest compile that the Player at `player` does
    /// not hold as they are: the code of that compile names types or members
    /// of the Editor the running Player's stand-ins lack, so no patch of it
    /// can be applied there.
    pub fn changed_play_mode_stand_ins(&self, player: &str) -> Vec<String> {
        let managed = super::player_managed(player);
        self.play_mode_stand_ins
            .iter()
            .filter(|(name, hash)| {
                let path = sysutil::join(&[&managed, &format!("{name}.dll")]);
                file_hash(&path).map_or(true, |loaded| loaded != **hash)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }
}
