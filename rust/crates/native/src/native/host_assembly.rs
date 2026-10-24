//! A host Player assembled by the tool from the installation, with no
//! Editor build and so no Unity licence. A Player build of the Linux Mono
//! development target is the installation's template (the launcher,
//! `UnityPlayer.so`, Mono, the engine's modules and default resources) plus
//! a data directory written for the project. The tool writes:
//!
//! - `globalgamemanagers` from the release's manager pack and the project's
//!   `ProjectSettings` (`playerbake::write_game_managers`);
//! - `Resources/unity_builtin_extra`: the built-in shaders the build needs,
//!   baked from the Editor's own `unity_builtin_extra` with the tool's baker;
//! - `globalgamemanagers.assets` with what the Player preloads, an empty
//!   `resources.assets` and an empty bootstrap scene (`level0`);
//! - the registrations of the engine's assemblies and of the tool's bridge,
//!   compiled against them, the class library a build copies, the project's
//!   native plugins for Linux and its streaming assets.
//!
//! A preparation completes the Player with the project's scripts and
//! resources once it has compiled them (`complete_player`).
use super::shader_compiler::ShaderCompiler;
use crate::{
    cancel::Context,
    playerbake::{
        self, DerivedManagers, ManagerPack, ShaderDependency, ShaderPointer, ShaderProject,
        ShaderRelease, TypeTree, Unsupported, Val,
    },
    sha256, sysutil,
};
use anyhow::{Context as _, Result, bail};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};

/// The executable of an assembled host, beside its `Project_Data`.
pub const HOST_EXECUTABLE: &str = "Project.x86_64";
const DATA: &str = "Project_Data";
/// The record of how a host was assembled, in its data directory.
pub const ASSEMBLY_RECORD: &str = "UnityMicroEditorAssembledHost.json";
/// The build proof `agent::host::proof_graph` reads: a host of no package
/// assemblies of its own, whose packages a preparation builds.
const BUILD_PROOF: &str = "UnityMicroEditorSourceProfile.json";

/// The class library a Mono build of 6000.0 copies whatever its code uses,
/// from `MonoBleedingEdge/lib/mono/unityjit-linux` and its facades.
const CLASS_LIBRARY: [&str; 20] = [
    "Mono.Security",
    "mscorlib",
    "System.ComponentModel.Composition",
    "System.Configuration",
    "System.Core",
    "System.Data.DataSetExtensions",
    "System.Data",
    "System",
    "System.Drawing",
    "System.EnterpriseServices",
    "System.IO.Compression",
    "System.IO.Compression.FileSystem",
    "System.Net.Http",
    "System.Numerics",
    "System.Runtime.Serialization",
    "System.Security",
    "System.ServiceModel.Internals",
    "System.Transactions",
    "System.Xml",
    "System.Xml.Linq",
];
/// The facades of 6000.0's and 6000.6's builds; an assembly a build does
/// not need is never loaded.
const CLASS_LIBRARY_FACADES: [&str; 6] = [
    "netstandard",
    "System.Buffers",
    "System.Memory",
    "System.Runtime",
    "System.Text.Encoding.CodePages",
    "System.Threading.Tasks.Extensions",
];

/// The built-in shaders a 6000.0 build adds to those the project always
/// includes, as an Editor build of Bouncemasters lists them in
/// `GraphicsSettings.m_AlwaysIncludedShaders` after the authored ones (path
/// IDs of `unity_builtin_extra`): the engine's own (stencil, blits, the
/// internal error shaders), UI Toolkit's and the video decoders'.
const ENGINE_ALWAYS_INCLUDED: [i64; 28] = [
    19, 65, 66, 67, 109, 110, 107, 111, 9000, 9001, 9002, 9003, 9004, 9007, 9100, 9102, 9101, 68,
    10757, 15309, 15308, 19011, 15304, 15312, 113, 16000, 16001, 17000,
];

/// The same in 6000.6, as an Editor build of Dungeon lists them.
const ENGINE_ALWAYS_INCLUDED_6000_6: [i64; 32] = [
    19, 65, 66, 67, 109, 110, 107, 111, 9000, 9001, 9002, 9003, 9004, 9007, 9100, 9101, 68, 10757,
    20500, 20501, 20502, 20506, 20507, 15309, 15308, 19011, 15304, 15312, 113, 16000, 16001, 17000,
];

fn engine_always_included(version: &str) -> &'static [i64] {
    match version {
        "6000.6.3f1" => &ENGINE_ALWAYS_INCLUDED_6000_6,
        _ => &ENGINE_ALWAYS_INCLUDED,
    }
}

/// The type hash of `PreloadData` in a 6000.0 build (and 6000.6's).
const PRELOAD_DATA_HASH: &str = "857195b5935cf8fb7c0b98cba9717eac";

/// Built-in shaders the baker refused, by path ID, with the reason.
type Refused = Vec<(i64, String)>;

/// How a host was assembled.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AssembledHost {
    pub player: String,
    pub unity_version: String,
    /// The built-in shaders baked into `unity_builtin_extra`, by path ID.
    pub builtin_shaders: Vec<i64>,
    /// Built-in shaders the baker refused, with the reason.
    pub refused_shaders: Vec<(i64, String)>,
    pub plugins: Vec<String>,
    pub seconds: f64,
}

fn copy(source: &str, destination: &str) -> Result<()> {
    std::fs::copy(source, destination)
        .with_context(|| format!("copy {source} to {destination}"))?;
    Ok(())
}

fn copy_tree(source: &str, destination: &str, skip: &dyn Fn(&str) -> bool) -> Result<()> {
    sysutil::mkdir_all(destination)?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {source}"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip(&name) {
            continue;
        }
        let from = sysutil::join(&[source, &name]);
        let to = sysutil::join(&[destination, &name]);
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to, skip)?;
        } else if entry.file_type()?.is_file() {
            copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The installation's data directory of an Editor executable.
fn editor_data(editor: &str) -> String {
    sysutil::join(&[&sysutil::dir(editor), "Data"])
}

/// Assembles the host of `project` into `output` (which must not exist)
/// and returns where its Player is. `packages` are the roots of the
/// project's resolved packages, whose native plugins it takes.
pub fn assemble_host(
    ctx: &Context,
    project: &str,
    editor: &str,
    output: &str,
    packages: &[String],
) -> Result<AssembledHost> {
    let started = std::time::Instant::now();
    let version = crate::release::project_release(project)?;
    let Some(pack) = ManagerPack::of_release(version)? else {
        bail!(Unsupported(format!(
            "assembling a host of Unity {version}, whose managers the tool has no pack of"
        )));
    };
    let data_root = editor_data(editor);
    let variations = sysutil::join(&[
        &data_root,
        "PlaybackEngines/LinuxStandaloneSupport/Variations",
    ]);
    let template = sysutil::join(&[&variations, "linux64_player_development_mono"]);
    if !sysutil::lstat(&sysutil::join(&[&template, "UnityPlayer.so"])).is_ok_and(|i| i.is_regular())
    {
        bail!("the installation has no Linux Mono development Player: {template}");
    }
    let partial = format!("{output}.partial-{}", crate::util::random_id());
    let data = sysutil::join(&[&partial, DATA]);
    let outcome = (|| -> Result<AssembledHost> {
        for directory in ["Managed", "Resources", "Plugins", "StreamingAssets"] {
            sysutil::mkdir_all(&sysutil::join(&[&data, directory]))?;
        }
        // The engine.
        let player = sysutil::join(&[&partial, HOST_EXECUTABLE]);
        copy(&sysutil::join(&[&template, "LinuxPlayer"]), &player)?;
        for name in ["UnityPlayer.so", "libdecor-0.so.0", "libdecor-cairo.so"] {
            copy(
                &sysutil::join(&[&template, name]),
                &sysutil::join(&[&partial, name]),
            )?;
        }
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o755))?;
        }
        let template_data = sysutil::join(&[&template, "Data"]);
        copy_tree(
            &sysutil::join(&[&template_data, "MonoBleedingEdge"]),
            &sysutil::join(&[&data, "MonoBleedingEdge"]),
            &|_| false,
        )?;
        copy(
            &sysutil::join(&[&template_data, "Resources/unity default resources"]),
            &sysutil::join(&[&data, "Resources/unity default resources"]),
        )?;
        let managed = sysutil::join(&[&data, "Managed"]);
        // 6000.6 keeps the engine's assemblies beside the variations, in
        // `Variations/mono/Managed`, shared by every Mono Player.
        let mut engine_managed = sysutil::join(&[&template_data, "Managed"]);
        if sysutil::lstat(&engine_managed).is_err() {
            engine_managed = sysutil::join(&[&sysutil::dir(&template), "mono/Managed"]);
        }
        copy_tree(&engine_managed, &managed, &|_| false)?;
        copy_class_library(&data_root, &managed)?;
        let bridge = crate::compiler::compile_bridge(
            ctx,
            &sysutil::join(&[project, "Library/UnityMicroEditor/bridge"]),
            editor,
            &managed,
        )?;
        copy(&bridge, &sysutil::join(&[&managed, "UnityMicroEditor.dll"]))?;
        let (plugins, assemblies) = copy_plugins(
            project,
            packages,
            &sysutil::join(&[&data, "Plugins"]),
            &managed,
        )?;
        write_registrations(&data, &managed, &assemblies)?;
        write_identity(project, &data)?;
        copy_streaming_assets(project, &sysutil::join(&[&data, "StreamingAssets"]))?;
        // The built-in shaders, then the managers that name them.
        let (builtin_shaders, refused_shaders) =
            write_builtin_extra(ctx, project, editor, &data, packages)?;
        let default_scripts = default_resource_scripts(&data)?;
        write_managers(
            project,
            &pack,
            &data,
            &builtin_shaders,
            &default_scripts,
            &Completed::default(),
        )?;
        write_global_assets(version, &data, &default_scripts)?;
        std::fs::write(
            sysutil::join(&[&data, "resources.assets"]),
            playerbake::plain_file(version, &[], &[], &[])?,
        )?;
        write_bootstrap_scene(version, &data, build_scenes(project).len())?;
        write_proof(project, editor, version, &data, packages)?;
        let assembled = AssembledHost {
            player: sysutil::join(&[output, HOST_EXECUTABLE]),
            unity_version: version.to_string(),
            builtin_shaders,
            refused_shaders,
            plugins,
            seconds: started.elapsed().as_secs_f64(),
        };
        std::fs::write(
            sysutil::join(&[&data, ASSEMBLY_RECORD]),
            serde_json::to_vec_pretty(&assembled)?,
        )?;
        Ok(assembled)
    })();
    match outcome {
        Ok(assembled) => {
            std::fs::rename(&partial, output)
                .with_context(|| format!("move the assembled host to {output}"))?;
            Ok(assembled)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&partial);
            Err(error)
        }
    }
}

/// The class library a build copies, and what those assemblies and the
/// engine's refer to in turn.
fn copy_class_library(data_root: &str, managed: &str) -> Result<()> {
    let library = sysutil::join(&[data_root, "MonoBleedingEdge/lib/mono/unityjit-linux"]);
    let facades = sysutil::join(&[&library, "Facades"]);
    let source_of = |name: &str| -> Option<String> {
        [&library, &facades]
            .iter()
            .map(|directory| sysutil::join(&[directory, &format!("{name}.dll")]))
            .find(|path| sysutil::lstat(path).is_ok_and(|info| info.is_regular()))
    };
    let mut pending: Vec<String> = CLASS_LIBRARY
        .iter()
        .chain(&CLASS_LIBRARY_FACADES)
        .map(|name| name.to_string())
        .collect();
    for entry in std::fs::read_dir(managed)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".dll") {
            pending.extend(references_of(&sysutil::join(&[managed, &name])));
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let destination = sysutil::join(&[managed, &format!("{name}.dll")]);
        if sysutil::lstat(&destination).is_ok() {
            continue;
        }
        let Some(source) = source_of(&name) else {
            continue;
        };
        copy(&source, &destination)?;
        pending.extend(references_of(&destination));
    }
    Ok(())
}

fn references_of(path: &str) -> Vec<String> {
    std::fs::read(path)
        .ok()
        .and_then(|image| crate::compiler::assembly_references(&image))
        .unwrap_or_default()
}

/// `ScriptingAssemblies.json`: the engine's assemblies (type 2), the main
/// one first and the modules by name, then the bridge (16); and no
/// initializers, which a preparation adds with what it compiles.
fn write_registrations(data: &str, managed: &str, plugins: &[String]) -> Result<()> {
    let mut modules: Vec<String> = std::fs::read_dir(managed)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("UnityEngine.") && name.ends_with(".dll"))
        .collect();
    let key = |name: &str| {
        let stem = name.strip_suffix(".dll").unwrap_or(name);
        stem.strip_suffix("Module").unwrap_or(stem).to_string()
    };
    modules.sort_by_key(|name| key(name));
    let mut names = vec!["UnityEngine.dll".to_string()];
    names.extend(modules.into_iter().filter(|name| name != "UnityEngine.dll"));
    let mut types = vec![2; names.len()];
    names.push("UnityMicroEditor.dll".into());
    types.push(16);
    for plugin in plugins {
        names.push(plugin.clone());
        types.push(16);
    }
    std::fs::write(
        sysutil::join(&[data, "ScriptingAssemblies.json"]),
        serde_json::to_vec(&serde_json::json!({"names": names, "types": types}))?,
    )?;
    std::fs::write(
        sysutil::join(&[data, "RuntimeInitializeOnLoads.json"]),
        br#"{"root":[]}"#,
    )?;
    Ok(())
}

/// `app.info` (company and product) and `boot.config` as a development
/// build writes them, without the Editor connection of a build's.
fn write_identity(project: &str, data: &str) -> Result<()> {
    let settings = std::fs::read_to_string(sysutil::join(&[
        project,
        "ProjectSettings/ProjectSettings.asset",
    ]))
    .unwrap_or_default();
    let field = |name: &str| {
        settings
            .lines()
            .find_map(|line| line.strip_prefix(&format!("  {name}: ")))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    std::fs::write(
        sysutil::join(&[data, "app.info"]),
        format!("{}\n{}", field("companyName"), field("productName")),
    )?;
    let guid = &sha256(format!("{project}\0assembled-host").as_bytes())[..32];
    std::fs::write(
        sysutil::join(&[data, "boot.config"]),
        format!(
            "gfx-threading-mode=4\nwait-for-native-debugger=0\nhdr-display-enabled=0\nbuild-guid={guid}\n"
        ),
    )?;
    Ok(())
}

/// The native libraries of the project and its packages a Linux build
/// copies into `Plugins`.
fn copy_plugins(
    project: &str,
    packages: &[String],
    plugins: &str,
    managed: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut roots = vec![sysutil::join(&[project, "Assets"])];
    roots.extend(packages.iter().cloned());
    let mut copied = Vec::new();
    let mut assemblies = Vec::new();
    for root in roots {
        let mut found = Vec::new();
        let _ = crate::compiler::walk_dir(&root, &mut |entry| {
            let name = sysutil::base(entry.path);
            if entry.is_dir() && (name.starts_with('.') || name.ends_with('~')) {
                return Ok(crate::compiler::Walk::SkipDir);
            }
            if !entry.is_dir() && (name.ends_with(".so") || name.ends_with(".dll")) {
                found.push(entry.path.to_string());
            }
            Ok(crate::compiler::Walk::Continue)
        });
        for path in found {
            // The rules of managed plugins hold for native ones.
            let plugin = crate::sourcegraph::read_source_plugin(
                &path,
                &[],
                crate::sourcegraph::LINUX_PLAYER,
            )?;
            if !plugin.enabled {
                continue;
            }
            let name = sysutil::base(&path);
            // A managed plugin the Player takes is an assembly of the build
            // (Odin's `NoEditor` builds beside the Editor's), which the
            // preparation then keeps.
            if name.ends_with(".dll") {
                if !crate::compiler::managed_plugin(&path) {
                    continue;
                }
                let destination = sysutil::join(&[managed, &name]);
                if sysutil::lstat(&destination).is_ok() {
                    continue;
                }
                copy(&path, &destination)?;
                assemblies.push(name);
                continue;
            }
            let destination = sysutil::join(&[plugins, &name]);
            if sysutil::lstat(&destination).is_ok() {
                continue;
            }
            copy(&path, &destination)?;
            copied.push(name);
        }
    }
    copied.sort();
    assemblies.sort();
    Ok((copied, assemblies))
}

/// `Assets/StreamingAssets` as it is, without `.meta` files, and an
/// Addressables runtime with an empty catalog where the project uses
/// Addressables: the session serves every addressable asset from its
/// sources, and the package only has to initialize.
fn copy_streaming_assets(project: &str, streaming: &str) -> Result<()> {
    let source = sysutil::join(&[project, "Assets/StreamingAssets"]);
    if sysutil::lstat(&source).is_ok_and(|info| info.is_dir()) {
        copy_tree(&source, streaming, &|name| name.ends_with(".meta"))?;
    }
    let manifest = std::fs::read_to_string(sysutil::join(&[project, "Packages/manifest.json"]))
        .unwrap_or_default();
    let Some(version) = serde_json::from_str::<serde_json::Value>(&manifest)
        .ok()
        .and_then(|manifest| {
            manifest["dependencies"]["com.unity.addressables"]
                .as_str()
                .map(str::to_string)
        })
    else {
        return Ok(());
    };
    let aa = sysutil::join(&[streaming, "aa"]);
    sysutil::mkdir_all(&aa)?;
    // Addressables checks the data against the platform its code is
    // compiled for, a device's where the project compiles for one.
    let target = match crate::sourcegraph::compile_for_group(project)?.as_deref() {
        Some("Android") => "Android",
        Some("iPhone") => "iOS",
        Some("WebGL") => "WebGL",
        _ => "StandaloneLinux64",
    };
    let settings = serde_json::json!({
        "m_buildTarget": target,
        "m_SettingsHash": "",
        "m_CatalogLocations": [{
            "m_Keys": ["AddressablesMainContentCatalog"],
            "m_InternalId": "{UnityEngine.AddressableAssets.Addressables.RuntimePath}/catalog.bin",
            "m_Provider": "UnityEngine.AddressableAssets.ResourceProviders.ContentCatalogProvider",
            "m_Dependencies": [],
            "m_ResourceType": {
                "m_AssemblyName": "Unity.Addressables, Version=0.0.0.0, Culture=neutral, PublicKeyToken=null",
                "m_ClassName": "UnityEngine.AddressableAssets.ResourceLocators.ContentCatalogData"
            },
            "SerializedData": []
        }],
        "m_LogResourceManagerExceptions": true,
        "m_ExtraInitializationData": [],
        "m_DisableCatalogUpdateOnStart": true,
        "m_IsLocalCatalogInBundle": false,
        "m_CertificateHandlerType": {"m_AssemblyName": "", "m_ClassName": ""},
        "m_AddressablesVersion": version,
        "m_maxConcurrentWebRequests": 3,
        "m_CatalogRequestsTimeout": 0
    });
    std::fs::write(
        sysutil::join(&[&aa, "settings.json"]),
        serde_json::to_vec(&settings)?,
    )?;
    std::fs::write(sysutil::join(&[&aa, "catalog.bin"]), empty_catalog())?;
    Ok(())
}

/// A content catalog of no keys in the binary form Addressables 2 reads
/// (`ContentCatalogData` over a `BinaryStorageBuffer`): its header (the
/// Mono hash of the class name, version 2), an empty key table, the
/// locator's name, the instance and scene providers and no resource
/// providers. Offsets point past a string's or array's size.
fn empty_catalog() -> Vec<u8> {
    const RESOURCE_MANAGER: &str =
        "Unity.ResourceManager, Version=0.0.0.0, Culture=neutral, PublicKeyToken=null";
    let mut out = vec![0u8; 32];
    // The keys, reserved after the header.
    out.extend_from_slice(&0u32.to_le_bytes());
    let keys = out.len() as u32;
    let string = |out: &mut Vec<u8>, text: &str| -> u32 {
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        let at = out.len() as u32;
        out.extend_from_slice(text.as_bytes());
        at
    };
    let id = string(&mut out, "AddressablesMainContentCatalog");
    let provider = |out: &mut Vec<u8>, class: &str| -> u32 {
        let name = string(out, class);
        let assembly = string(out, RESOURCE_MANAGER);
        let type_at = out.len() as u32;
        out.extend_from_slice(&assembly.to_le_bytes());
        out.extend_from_slice(&name.to_le_bytes());
        let at = out.len() as u32;
        out.extend_from_slice(&name.to_le_bytes());
        out.extend_from_slice(&type_at.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        at
    };
    let instance = provider(
        &mut out,
        "UnityEngine.ResourceManagement.ResourceProviders.InstanceProvider",
    );
    let scene = provider(
        &mut out,
        "UnityEngine.ResourceManagement.ResourceProviders.SceneProvider",
    );
    out.extend_from_slice(&0u32.to_le_bytes());
    let providers = out.len() as u32;
    let build = string(&mut out, "");
    let header = [
        mono_string_hash("ContentCatalogData") as u32,
        2,
        keys,
        id,
        instance,
        scene,
        providers,
        build,
    ];
    for (index, word) in header.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// `String.GetHashCode` of the Player's Mono (the 64-bit form of the .NET
/// Framework's), which Addressables stamps its catalogs with.
fn mono_string_hash(text: &str) -> i32 {
    let chars: Vec<i32> = text.encode_utf16().map(i32::from).collect();
    let (mut first, mut second) = (5381i32, 5381i32);
    let mut at = 0;
    while at < chars.len() {
        first = (first.wrapping_shl(5).wrapping_add(first)) ^ chars[at];
        if at + 1 >= chars.len() {
            break;
        }
        second = (second.wrapping_shl(5).wrapping_add(second)) ^ chars[at + 1];
        at += 2;
    }
    first.wrapping_add(second.wrapping_mul(1_566_083_941))
}

/// The `MonoScript`s of the Player's default resources, by path ID, which
/// a build preloads and registers first.
fn default_resource_scripts(data: &str) -> Result<Vec<i64>> {
    let file = std::fs::read(sysutil::join(&[data, "Resources/unity default resources"]))?;
    let plain = playerbake::PlainFile::parse(&file)?;
    let mut scripts: Vec<i64> = plain
        .objects
        .iter()
        .filter(|object| object.class_id == playerbake::MONO_SCRIPT)
        .map(|object| object.local_id)
        .collect();
    scripts.sort_unstable();
    Ok(scripts)
}

/// Where an authored reference of a settings file leads in a host: an
/// object of the engine's own files.
fn engine_pointer(source: &serde_json::Value) -> Option<(i64, i64)> {
    let file_id: i64 = source.get("fileID")?.as_str()?.parse().ok()?;
    match source.get("guid")?.as_str()? {
        "0000000000000000f000000000000000" => Some((playerbake::BUILTIN_EXTRA_FILE, file_id)),
        "0000000000000000e000000000000000" => Some((playerbake::DEFAULT_RESOURCES_FILE, file_id)),
        _ => None,
    }
}

fn shader_settings(project: &str) -> Option<playerbake::ShaderSettings> {
    let read =
        |name: &str| std::fs::read_to_string(sysutil::join(&[project, name])).unwrap_or_default();
    playerbake::ShaderSettings::read(
        &read("ProjectSettings/ProjectSettings.asset"),
        &read("ProjectSettings/GraphicsSettings.asset"),
        &read("ProjectSettings/QualitySettings.asset"),
        &read("Packages/manifest.json"),
    )
    .ok()
}

/// What a preparation gives a host's managers once it has compiled the
/// project: the scripts and resources it wrote, and the shaders among them.
#[derive(Default)]
struct Completed {
    /// Where the project's assets the settings name lie, by GUID and file
    /// ID (the render pipeline, its global settings, preloaded assets).
    settings_assets: HashMap<(String, i64), (i64, i64)>,
    scripts: Vec<(i64, i64)>,
    resources: Vec<(String, (i64, i64))>,
    shaders: Vec<((i64, i64), String)>,
}

fn write_managers(
    project: &str,
    pack: &ManagerPack,
    data: &str,
    builtin_shaders: &[i64],
    default_scripts: &[i64],
    completed: &Completed,
) -> Result<()> {
    let settings = shader_settings(project);
    let mut scripts: Vec<(i64, i64)> = default_scripts
        .iter()
        .map(|&id| (playerbake::DEFAULT_RESOURCES_FILE, id))
        .collect();
    scripts.extend(&completed.scripts);
    let derived = DerivedManagers {
        scenes: build_scenes(project),
        graphics_apis: vec![17],
        scripts,
        resources: completed.resources.clone(),
        shaders: completed.shaders.clone(),
        builtin_shaders: builtin_shaders.to_vec(),
        shader_settings: settings.as_ref(),
        pointers: &|source: &serde_json::Value| {
            engine_pointer(source).or_else(|| {
                let file_id: i64 = source.get("fileID")?.as_str()?.parse().ok()?;
                let guid = source.get("guid")?.as_str()?.to_ascii_lowercase();
                completed.settings_assets.get(&(guid, file_id)).copied()
            })
        },
    };
    let (file, _) = playerbake::write_game_managers(Path::new(project), pack, &derived)?;
    let path = sysutil::join(&[data, "globalgamemanagers"]);
    // A staged Player's files may be read-only copies.
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, file)?;
    Ok(())
}

/// What a preparation wrote into a Player it completed.
#[derive(Debug, Default, serde::Serialize)]
pub struct CompletedPlayer {
    pub scripts: usize,
    pub resources: usize,
    /// Sources a scripted importer imported.
    pub imported: usize,
    /// Scripts not laid out and resources not written, with the reason.
    pub refused: Vec<(String, String)>,
    pub seconds: f64,
}

fn replace(path: &str, data: &[u8]) -> Result<()> {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, data).with_context(|| format!("write {path}"))
}

/// Whether a source lies in a folder `Resources.Load` reads, and its name
/// there: the path below the last `Resources` folder, without extension,
/// in lower case, as a build lists it. Sources in an `Editor` folder are
/// the Editor's.
fn resource_name(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.iter().any(|part| part == "Editor") {
        return None;
    }
    let at = parts.iter().rposition(|part| part == "Resources")?;
    let rest = &parts[at + 1..];
    let (last, folders) = rest.split_last()?;
    let stem = match last.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => last.as_str(),
    };
    let mut name: Vec<&str> = folders.iter().map(String::as_str).collect();
    name.push(stem);
    Some(name.join("/").to_lowercase())
}

/// Has every preparation complete the Players of hosts the tool assembled.
pub fn register() {
    crate::compiler::set_player_completion(|ctx, project, editor, player| {
        if let Some(completed) = complete_player(ctx, project, editor, player)? {
            super::support::Event::new("assembled-host-completed")
                .with("scripts", &completed.scripts)
                .with("resources", &completed.resources)
                .with("refused", &completed.refused.len())
                .seconds("elapsedSeconds", completed.seconds)
                .emit();
        }
        Ok(())
    });
}

/// Completes a host the tool assembled, once a preparation has compiled the
/// project into its staged copy `player`: every script class of the
/// Player's assemblies gets its `MonoScript` in `globalgamemanagers.assets`
/// (the Player loads them at its start, on its main thread, and so knows
/// their assemblies before any content loads), the project's `Resources`
/// are written into `resources.assets`, and the managers name them. A
/// Player of a host the Editor built is left as it is.
pub fn complete_player(
    ctx: &Context,
    project: &str,
    editor: &str,
    player: &str,
) -> Result<Option<CompletedPlayer>> {
    let started = std::time::Instant::now();
    let data = crate::compiler::player_data(player);
    let Ok(record) = std::fs::read(sysutil::join(&[&data, ASSEMBLY_RECORD])) else {
        return Ok(None);
    };
    let record: AssembledHost = serde_json::from_slice(&record)?;
    let version = record.unity_version.as_str();
    let pack = ManagerPack::of_release(version)?
        .with_context(|| format!("no manager pack of Unity {version}"))?;
    let mut library = playerbake::Library::of_release(version)
        .with_context(|| format!("no engine pack of Unity {version}"))??;
    let templates = playerbake::Templates::of_release(version)
        .with_context(|| format!("no script templates of Unity {version}"))?;
    let layouts = crate::compiler::player_script_layouts(ctx, project, editor, player)?;
    let layouts: playerbake::Layouts = serde_json::from_slice(&layouts)?;
    let layout = playerbake::Layout::new(&layouts, &templates);
    let root = Path::new(project);
    let mut completed = CompletedPlayer {
        refused: library.add_scripts(&layout, &playerbake::script_execution_orders(root))?,
        ..Default::default()
    };
    // The scripts, after what the file preloads.
    let script_kind = library
        .class(playerbake::MONO_SCRIPT)
        .context("the engine pack has no MonoScript")?
        .clone();
    let default_scripts = default_resource_scripts(&data)?;
    let preloaded: Vec<(i64, i64)> = default_scripts.iter().map(|&id| (1, id)).collect();
    let mut objects = vec![(1, 0, preload_data(&preloaded))];
    let mut written = Completed::default();
    // The sources of scripted importers, imported by the game's Editor code
    // that the Player runs.
    let sources = playerbake::Sources::of_project(root)?;
    let (imported, failed) = super::scripted_imports::import_scripted(
        ctx,
        project,
        player,
        &sources,
        &sysutil::join(&[&sysutil::dir(&sysutil::dir(player)), "scripted-imports"]),
    )?;
    completed.imported = imported.len();
    completed.refused.extend(failed);
    let sources = sources.with_imported(imported);
    // A script has a `MonoScript` where Unity imports one: the class a `.cs`
    // source of its name declares, and the classes of a plugin's assembly.
    let mut script_files: HashMap<String, Vec<&str>> = HashMap::new();
    let mut plugins: BTreeSet<String> = BTreeSet::new();
    for (guid, path) in sources.all() {
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("cs") => script_files.entry(stem).or_default().push(guid),
            Some("dll") => {
                plugins.insert(stem);
            }
            _ => {}
        }
    }
    let imported = |full_name: &str, script: &Val| -> bool {
        let assembly = match script.field("m_AssemblyName") {
            Some(Val::Str(name)) => name.strip_suffix(".dll").unwrap_or(name).to_string(),
            _ => String::new(),
        };
        if plugins.contains(&assembly) {
            return true;
        }
        let short = full_name.rsplit('.').next().unwrap_or(full_name);
        script_files.get(short).is_some_and(|guids| {
            guids
                .iter()
                .any(|guid| sources.script_class(guid).as_deref() == Some(full_name))
        })
    };
    let scripts: Vec<(&str, &Val)> = library
        .script_objects()
        .into_iter()
        .filter(|(name, script)| imported(name, script))
        .collect();
    // The other files of the build take their scripts from this one.
    let mut held_scripts: HashMap<String, i64> = HashMap::new();
    for (index, (name, script)) in scripts.into_iter().enumerate() {
        let local_id = index as i64 + 2;
        held_scripts.insert(name.to_string(), local_id);
        objects.push((local_id, 1, script_kind.tree.encode(script)?));
        written
            .scripts
            .push((playerbake::GLOBAL_ASSETS_FILE, local_id));
    }
    completed.scripts = written.scripts.len();
    let script_hash: [u8; 16] = script_kind.tree.entry[7..23].try_into()?;
    replace(
        &sysutil::join(&[&data, "globalgamemanagers.assets"]),
        &playerbake::plain_file(
            version,
            &[
                (150, hex16(PRELOAD_DATA_HASH)),
                (playerbake::MONO_SCRIPT, script_hash),
            ],
            &objects,
            &GLOBAL_ASSET_EXTERNALS.map(|(path, guid)| (path.to_string(), guid.to_string())),
        )?,
    )?;
    // The resources.
    let mut named: Vec<(String, String)> = Vec::new();
    for (guid, path) in sources.all() {
        let relative = path.strip_prefix(root).unwrap_or(path);
        if path.is_dir()
            || matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("cs" | "dll")
            )
        {
            continue;
        }
        if let Some(name) = resource_name(relative) {
            named.push((guid.to_string(), name));
        }
    }
    // The project's assets the settings name, which a build holds beside
    // the resources.
    let settings_assets = settings_references(root, &sources);
    if !named.is_empty() || !settings_assets.is_empty() {
        let mut roots: Vec<&str> = named.iter().map(|(guid, _)| guid.as_str()).collect();
        for (guid, _) in &settings_assets {
            if !roots.contains(&guid.as_str()) {
                roots.push(guid);
            }
        }
        let builtin_extra = sysutil::join(&[&editor_data(editor), "Resources/unity_builtin_extra"]);
        playerbake::texcomp::use_editor(Path::new(&sysutil::dir(editor)));
        playerbake::audio::use_editor(Path::new(&sysutil::dir(editor)));
        // The shaders the resources use lie in the file, as a build holds
        // them.
        let logs = sysutil::join(&[&data, "..", "resource-shader-logs"]);
        let bakery = super::project_shaders::ProjectShaderBakery::new(ctx, project, editor, &logs)?;
        let bake = |guid: &str| bakery.bake(guid);
        let assembled = playerbake::write_closure_baking(
            &library,
            &playerbake::Tags::of_project(root)?,
            &sources,
            &roots,
            None,
            playerbake::FileFormat::of_release(version)
                .with_context(|| format!("the files of Unity {version}"))?,
            "resources.assets.resS",
            Some(Path::new(&builtin_extra)),
            Some(&bake),
            Some(playerbake::ScriptsElsewhere {
                path: "globalgamemanagers.assets",
                scripts: &held_scripts,
            }),
        )?;
        let _ = std::fs::remove_dir_all(&logs);
        for (local_id, class, guid, _) in &assembled.objects {
            if class == "UnityEngine.Shader"
                && let Some(name) = bakery.name(guid)
            {
                written
                    .shaders
                    .push(((playerbake::RESOURCES_FILE, *local_id), name));
            }
        }
        replace(
            &sysutil::join(&[&data, "resources.assets"]),
            &assembled.file,
        )?;
        if !assembled.stream.is_empty() {
            replace(
                &sysutil::join(&[&data, "resources.assets.resS"]),
                &assembled.stream,
            )?;
        }
        if !assembled.resource.is_empty() {
            replace(
                &sysutil::join(&[&data, "resources.assets.resource"]),
                &assembled.resource,
            )?;
        }
        for (local_id, _, guid, file_id) in &assembled.objects {
            if settings_assets.contains(&(guid.clone(), *file_id)) {
                written.settings_assets.insert(
                    (guid.clone(), *file_id),
                    (playerbake::RESOURCES_FILE, *local_id),
                );
            }
        }
        let names: HashMap<&str, &str> = named
            .iter()
            .map(|(guid, name)| (guid.as_str(), name.as_str()))
            .collect();
        for (local_id, class, guid, file_id) in &assembled.objects {
            let Some(name) = names.get(guid.as_str()) else {
                continue;
            };
            if class == "UnityEditor.MonoScript" {
                continue;
            }
            // A prefab is found by its root; another asset by each object.
            if let Some(root_id) = assembled.roots.get(guid)
                && root_id != file_id
            {
                continue;
            }
            let at = (playerbake::RESOURCES_FILE, *local_id);
            written.resources.push((name.to_string(), at));
        }
        completed.resources = written.resources.len();
        completed.refused.extend(assembled.refused);
    }
    write_managers(
        project,
        &pack,
        &data,
        &record.builtin_shaders,
        &default_scripts,
        &written,
    )?;
    completed.seconds = started.elapsed().as_secs_f64();
    std::fs::write(
        sysutil::join(&[&data, "UnityMicroEditorCompletedPlayer.json"]),
        serde_json::to_vec_pretty(&completed)?,
    )?;
    Ok(Some(completed))
}

/// The project's assets the settings files refer to, by GUID and file ID:
/// what a build writes beside the managers that name them.
fn settings_references(project: &Path, sources: &playerbake::Sources) -> Vec<(String, i64)> {
    let mut found: Vec<(String, i64)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(project.join("ProjectSettings")) else {
        return found;
    };
    let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    for file in files {
        // The files of the Player's managers; Build Settings and the
        // Editor's own settings name what a build does not hold.
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !matches!(
            name.as_str(),
            "GraphicsSettings.asset"
                | "QualitySettings.asset"
                | "ProjectSettings.asset"
                | "AudioManager.asset"
                | "DynamicsManager.asset"
                | "Physics2DSettings.asset"
                | "VFXManager.asset"
        ) {
            continue;
        }
        let Ok(data) = std::fs::read(&file) else {
            continue;
        };
        let Ok(documents) = crate::yaml::parse_unity_yaml_lexical(&data) else {
            continue;
        };
        let mut pending: Vec<&serde_json::Value> = Vec::new();
        let values: Vec<serde_json::Value> = documents
            .into_iter()
            .map(|d| serde_json::Value::Object(d.data))
            .collect();
        pending.extend(values.iter());
        while let Some(value) = pending.pop() {
            match value {
                serde_json::Value::Object(map) => {
                    if let (
                        Some(serde_json::Value::String(id)),
                        Some(serde_json::Value::String(guid)),
                    ) = (map.get("fileID"), map.get("guid"))
                        && let Ok(id) = id.parse::<i64>()
                    {
                        let guid = guid.to_ascii_lowercase();
                        if id != 0
                            && !guid.starts_with("0000000000000000")
                            && sources.path(&guid).is_some()
                            && !found.contains(&(guid.clone(), id))
                        {
                            found.push((guid, id));
                        }
                        continue;
                    }
                    pending.extend(map.values());
                }
                serde_json::Value::Array(items) => pending.extend(items.iter()),
                _ => {}
            }
        }
    }
    found
}

/// The scenes of a host's build: its bootstrap scene, then for each enabled
/// scene of the project's Build Settings after the first an empty scene of
/// the same name at the same build index, which the session builds the
/// authored scene into when the game loads it by name, path or index
/// (`RawProjectContent`, which looks for the `/BuildScenes/` folder).
fn build_scenes(project: &str) -> Vec<String> {
    let mut scenes = vec![BOOTSTRAP_SCENE.to_string()];
    let path = sysutil::join(&[project, "ProjectSettings/EditorBuildSettings.asset"]);
    let documents = std::fs::read(&path)
        .ok()
        .and_then(|data| crate::yaml::parse_unity_yaml_lexical(&data).ok())
        .unwrap_or_default();
    let enabled: Vec<String> = documents
        .first()
        .and_then(|document| document.data.get("m_Scenes"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|scene| scene.get("enabled").and_then(serde_json::Value::as_str) == Some("1"))
        .filter_map(|scene| scene.get("path").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    for (index, path) in enabled.iter().enumerate().skip(1) {
        let name = Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        scenes.push(format!(
            "Assets/UnityMicroEditorGenerated/BuildScenes/{index}/{name}.unity"
        ));
    }
    scenes
}

/// The name a host's bootstrap scene has in its build settings.
pub const BOOTSTRAP_SCENE: &str = "Assets/UnityMicroEditorGenerated/Bootstrap.unity";

/// `PreloadData`: the objects the Player loads with the file, the default
/// resources' scripts.
fn preload_data(scripts: &[(i64, i64)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(scripts.len() as u32).to_le_bytes());
    for (file, path) in scripts {
        out.extend_from_slice(&(*file as i32).to_le_bytes());
        out.extend_from_slice(&path.to_le_bytes());
    }
    // No dependencies, and no explicit data layout.
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    out
}

/// The files `globalgamemanagers.assets` refers to.
const GLOBAL_ASSET_EXTERNALS: [(&str, &str); 2] = [
    (
        "Library/unity default resources",
        "0000000000000000e000000000000000",
    ),
    (
        "Resources/unity_builtin_extra",
        "0000000000000000f000000000000000",
    ),
];

fn write_global_assets(version: &str, data: &str, default_scripts: &[i64]) -> Result<()> {
    let scripts: Vec<(i64, i64)> = default_scripts.iter().map(|&id| (1, id)).collect();
    let file = playerbake::plain_file(
        version,
        &[(150, hex16(PRELOAD_DATA_HASH))],
        &[(1, 0, preload_data(&scripts))],
        &GLOBAL_ASSET_EXTERNALS.map(|(path, guid)| (path.to_string(), guid.to_string())),
    )?;
    std::fs::write(sysutil::join(&[data, "globalgamemanagers.assets"]), file)?;
    Ok(())
}

fn hex16(text: &str) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).unwrap_or(0);
    }
    bytes
}

/// `level0` and the stand-ins after it: a scene of nothing but its settings, the engine pack's samples
/// with their references cleared, which the Player loads at start before
/// the session loads its own.
fn write_bootstrap_scene(version: &str, data: &str, scenes: usize) -> Result<()> {
    let library = playerbake::Library::of_release(version)
        .with_context(|| format!("no engine pack of Unity {version}"))??;
    let mut types = Vec::new();
    let mut objects = Vec::new();
    // RenderSettings, LightmapSettings and NavMeshSettings.
    for (local_id, class_id) in [(1, 104), (2, 157), (3, 196)] {
        let Some(kind) = library.class(class_id) else {
            bail!("the engine pack of Unity {version} has no class {class_id}");
        };
        let mut value = kind.sample.clone();
        clear_pointers(&mut value);
        let hash: [u8; 16] = kind.tree.entry[7..23].try_into()?;
        types.push((kind.tree.class_id, hash));
        objects.push((local_id, types.len() - 1, kind.tree.encode(&value)?));
    }
    // The stand-ins of the Build Settings scenes are the same empty scene.
    let scene = playerbake::plain_file(version, &types, &objects, &[])?;
    for index in 0..scenes.max(1) {
        std::fs::write(sysutil::join(&[data, &format!("level{index}")]), &scene)?;
    }
    Ok(())
}

fn clear_pointers(value: &mut Val) {
    match value {
        Val::Map(fields) => {
            if fields.len() == 2 && fields[0].0 == "m_FileID" && fields[1].0 == "m_PathID" {
                fields[0].1 = Val::I(0);
                fields[1].1 = Val::I(0);
                return;
            }
            for (_, field) in fields.iter_mut() {
                clear_pointers(field);
            }
        }
        Val::Seq(items) => items.iter_mut().for_each(clear_pointers),
        _ => {}
    }
}

/// The same of a 6000.6 Player, from an Editor build of Dungeon's bridge
/// less its version's, API level's and input handler's symbols.
const PLAYER_DEFINES_6000_6: [&str; 85] = [
    "CSHARP_7_3_OR_NEWER",
    "CSHARP_7_OR_LATER",
    "DEBUG",
    "DEVELOPMENT_BUILD",
    "EDITOR_ONLY_NAVMESH_BUILDER_DEPRECATED",
    "ENABLE_ACCELERATOR_CLIENT_DEBUGGING",
    "ENABLE_AUDIO",
    "ENABLE_AUDIO_SCRIPTABLE_PIPELINE",
    "ENABLE_BURST",
    "ENABLE_CACHING",
    "ENABLE_CLOTH",
    "ENABLE_CLOUD_LICENSE",
    "ENABLE_CLOUD_SERVICES",
    "ENABLE_CLOUD_SERVICES_ADS",
    "ENABLE_CLOUD_SERVICES_ANALYTICS",
    "ENABLE_CLOUD_SERVICES_BUILD",
    "ENABLE_CLOUD_SERVICES_CRASH_REPORTING",
    "ENABLE_CLOUD_SERVICES_PURCHASING",
    "ENABLE_CLOUD_SERVICES_USE_WEBREQUEST",
    "ENABLE_CLUSTERINPUT",
    "ENABLE_CLUSTER_SYNC",
    "ENABLE_CRUNCH_TEXTURE_COMPRESSION",
    "ENABLE_CUSTOM_RENDER_TEXTURE",
    "ENABLE_DIRECTOR",
    "ENABLE_DIRECTOR_AUDIO",
    "ENABLE_DIRECTOR_TEXTURE",
    "ENABLE_EDITOR_GAME_SERVICES",
    "ENABLE_EDITOR_HUB_LICENSE",
    "ENABLE_GENERATE_NATIVE_PLUGINS_FOR_ASSEMBLIES_API",
    "ENABLE_LOCALIZATION",
    "ENABLE_LZMA",
    "ENABLE_MANAGED_ANIMATION_JOBS",
    "ENABLE_MANAGED_AUDIO_JOBS",
    "ENABLE_MANAGED_JOBS",
    "ENABLE_MANAGED_TRANSFORM_JOBS",
    "ENABLE_MANAGED_UNITYTLS",
    "ENABLE_MARSHALLING_TESTS",
    "ENABLE_MICROPHONE",
    "ENABLE_MODULAR_UNITYENGINE_ASSEMBLIES",
    "ENABLE_MONO",
    "ENABLE_MOVIES",
    "ENABLE_MULTIPLE_DISPLAYS",
    "ENABLE_NAVIGATION_OFFMESHLINK_TO_NAVMESHLINK",
    "ENABLE_NETWORK",
    "ENABLE_PHYSICS",
    "ENABLE_PROFILER",
    "ENABLE_RUNTIME_GI",
    "ENABLE_SCRIPTING_GC_WBARRIERS",
    "ENABLE_SPATIALTRACKING",
    "ENABLE_SPRITES",
    "ENABLE_TERRAIN",
    "ENABLE_TEXTURE_STREAMING",
    "ENABLE_TILEMAP",
    "ENABLE_TIMELINE",
    "ENABLE_UNITYEVENTS",
    "ENABLE_UNITYWEBREQUEST",
    "ENABLE_UNITY_CLOUD_IDENTIFIERS",
    "ENABLE_UNITY_CONSENT",
    "ENABLE_UNITY_GAME_SERVICES_ANALYTICS_SUPPORT",
    "ENABLE_VIDEO",
    "ENABLE_VIRTUALTEXTURING",
    "ENABLE_VR",
    "ENABLE_WEBCAM",
    "ENABLE_WEBSOCKET_CLIENT",
    "ENABLE_WWW",
    "INCLUDE_DYNAMIC_GI",
    "INCLUDE_UNIFIED_RAYTRACING_COMPUTE_BACKEND",
    "INCLUDE_UNIFIED_RAYTRACING_HARDWARE_BACKEND",
    "PLATFORM_ARCH_64",
    "PLATFORM_STANDALONE",
    "PLATFORM_STANDALONE_LINUX",
    "PLATFORM_SUPPORTS_DISPLAYINFO_API",
    "PLATFORM_SUPPORTS_MONO",
    "PLATFORM_SUPPORTS_SPLIT_GRAPHICS_JOBS",
    "PLATFORM_USES_EXPLICIT_MEMORY_MANAGER_INITIALIZER",
    "RENDER_SOFTWARE_CURSOR",
    "TEXTCORE_1_0_OR_NEWER",
    "TEXTCORE_FONT_ENGINE_1_5_OR_NEWER",
    "TEXTCORE_FONT_ENGINE_1_6_OR_NEWER",
    "TEXTCORE_TEXT_ENGINE_1_5_OR_NEWER",
    "TRACE",
    "UNITY_ASSERTIONS",
    "UNITY_STANDALONE",
    "UNITY_STANDALONE_LINUX",
    "UNITY_STANDALONE_LINUX_API",
];

/// The symbols a 6000.0 development Player of Linux Mono is compiled with
/// whatever the project: an Editor build of Bouncemasters' bridge less the
/// project's own symbols, those of the release's version
/// (`release::version_defines`), of the API level and of the input handler.
const PLAYER_DEFINES_6000_0: [&str; 80] = [
    "CSHARP_7_3_OR_NEWER",
    "CSHARP_7_OR_LATER",
    "DEBUG",
    "DEVELOPMENT_BUILD",
    "EDITOR_ONLY_NAVMESH_BUILDER_DEPRECATED",
    "ENABLE_ACCELERATOR_CLIENT_DEBUGGING",
    "ENABLE_AUDIO",
    "ENABLE_CACHING",
    "ENABLE_CLOTH",
    "ENABLE_CLOUD_LICENSE",
    "ENABLE_CLOUD_SERVICES",
    "ENABLE_CLOUD_SERVICES_ADS",
    "ENABLE_CLOUD_SERVICES_ANALYTICS",
    "ENABLE_CLOUD_SERVICES_BUILD",
    "ENABLE_CLOUD_SERVICES_CRASH_REPORTING",
    "ENABLE_CLOUD_SERVICES_PURCHASING",
    "ENABLE_CLOUD_SERVICES_USE_WEBREQUEST",
    "ENABLE_CLUSTERINPUT",
    "ENABLE_CLUSTER_SYNC",
    "ENABLE_CRUNCH_TEXTURE_COMPRESSION",
    "ENABLE_CUSTOM_RENDER_TEXTURE",
    "ENABLE_DIRECTOR",
    "ENABLE_DIRECTOR_AUDIO",
    "ENABLE_DIRECTOR_TEXTURE",
    "ENABLE_EDITOR_GAME_SERVICES",
    "ENABLE_EDITOR_HUB_LICENSE",
    "ENABLE_GENERATE_NATIVE_PLUGINS_FOR_ASSEMBLIES_API",
    "ENABLE_LOCALIZATION",
    "ENABLE_LZMA",
    "ENABLE_MANAGED_ANIMATION_JOBS",
    "ENABLE_MANAGED_AUDIO_JOBS",
    "ENABLE_MANAGED_JOBS",
    "ENABLE_MANAGED_TRANSFORM_JOBS",
    "ENABLE_MANAGED_UNITYTLS",
    "ENABLE_MARSHALLING_TESTS",
    "ENABLE_MICROPHONE",
    "ENABLE_MODULAR_UNITYENGINE_ASSEMBLIES",
    "ENABLE_MONO",
    "ENABLE_MOVIES",
    "ENABLE_MULTIPLE_DISPLAYS",
    "ENABLE_NAVIGATION_OFFMESHLINK_TO_NAVMESHLINK",
    "ENABLE_NETWORK",
    "ENABLE_PHYSICS",
    "ENABLE_PROFILER",
    "ENABLE_RUNTIME_GI",
    "ENABLE_SCRIPTING_GC_WBARRIERS",
    "ENABLE_SPATIALTRACKING",
    "ENABLE_SPRITES",
    "ENABLE_TERRAIN",
    "ENABLE_TEXTURE_STREAMING",
    "ENABLE_TILEMAP",
    "ENABLE_TIMELINE",
    "ENABLE_UNITYEVENTS",
    "ENABLE_UNITYWEBREQUEST",
    "ENABLE_UNITY_CLOUD_IDENTIFIERS",
    "ENABLE_UNITY_CONSENT",
    "ENABLE_UNITY_GAME_SERVICES_ANALYTICS_SUPPORT",
    "ENABLE_VIDEO",
    "ENABLE_VIRTUALTEXTURING",
    "ENABLE_VR",
    "ENABLE_WEBCAM",
    "ENABLE_WEBSOCKET_CLIENT",
    "ENABLE_WWW",
    "INCLUDE_DYNAMIC_GI",
    "PLATFORM_ARCH_64",
    "PLATFORM_STANDALONE",
    "PLATFORM_STANDALONE_LINUX",
    "PLATFORM_SUPPORTS_MONO",
    "PLATFORM_SUPPORTS_SPLIT_GRAPHICS_JOBS",
    "PLATFORM_USES_EXPLICIT_MEMORY_MANAGER_INITIALIZER",
    "RENDER_SOFTWARE_CURSOR",
    "TEXTCORE_1_0_OR_NEWER",
    "TEXTCORE_FONT_ENGINE_1_5_OR_NEWER",
    "TEXTCORE_TEXT_ENGINE_1_5_OR_NEWER",
    "TRACE",
    "UNITY_64",
    "UNITY_ASSERTIONS",
    "UNITY_STANDALONE",
    "UNITY_STANDALONE_LINUX",
    "UNITY_STANDALONE_LINUX_API",
];

/// The class library a Player of the .NET Framework API level compiles
/// against, beside every facade of `unity-4.8-api`.
const FRAMEWORK_REFERENCES: [&str; 17] = [
    "Microsoft.CSharp",
    "System.ComponentModel.Composition",
    "System.Core",
    "System.Data.DataSetExtensions",
    "System.Data",
    "System.Drawing",
    "System.IO.Compression.FileSystem",
    "System.IO.Compression",
    "System.Net.Http",
    "System.Numerics.Vectors",
    "System.Numerics",
    "System.Runtime.Serialization",
    "System.Transactions",
    "System.Xml.Linq",
    "System.Xml",
    "System",
    "mscorlib",
];

/// The engine's module assemblies a Player compiles against: those no
/// built-in package controls, and those of the module packages the project
/// resolves.
fn engine_module_references(editor: &str, packages: &[String]) -> Result<Vec<String>> {
    let support = sysutil::join(&[
        &editor_data(editor),
        "PlaybackEngines/LinuxStandaloneSupport",
    ]);
    let setup =
        std::fs::read_to_string(sysutil::join(&[&support, "modules.asset"])).unwrap_or_default();
    let mut controlled = BTreeSet::new();
    let mut current = None;
    for line in setup.lines() {
        if let Some(name) = line.strip_prefix("  - name: ") {
            current = Some(name.trim().to_string());
        } else if line.trim() == "controlledByBuiltinPackage: 1"
            && let Some(name) = &current
        {
            controlled.insert(format!("UnityEngine.{name}Module.dll"));
        }
    }
    let managed = sysutil::join(&[&support, "Variations/mono/Managed"]);
    let mut references: Vec<String> = std::fs::read_dir(&managed)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".dll") && !controlled.contains(name))
        .map(|name| sysutil::join(&[&managed, &name]))
        .collect();
    let names: Vec<String> = packages
        .iter()
        .map(|path| sysutil::base(path))
        .map(|base| base.split('@').next().unwrap_or("").to_string())
        .collect();
    references.extend(crate::sourcegraph::package_module_references(
        editor,
        names.iter().map(String::as_str),
    )?);
    references.sort();
    references.dedup();
    Ok(references)
}

/// The build proof of a host that holds no assembly of a package, which the
/// preparation builds from source: only the bridge, whose symbols and
/// references are those every Player assembly of the project compiles with
/// and which the engine profile is read from.
fn write_proof(
    project: &str,
    editor: &str,
    version: &str,
    data: &str,
    packages: &[String],
) -> Result<()> {
    let engine: &[&str] = match version {
        "6000.0.71f1" => &PLAYER_DEFINES_6000_0,
        "6000.6.3f1" => &PLAYER_DEFINES_6000_6,
        _ => bail!(Unsupported(format!(
            "the compile symbols of a Unity {version} Player, which the tool has no table of"
        ))),
    };
    let settings = std::fs::read_to_string(sysutil::join(&[
        project,
        "ProjectSettings/ProjectSettings.asset",
    ]))
    .unwrap_or_default();
    let number = |name: &str| {
        settings
            .lines()
            .find_map(|line| line.trim().strip_prefix(&format!("{name}: ")))
            .and_then(|value| value.trim().parse::<i64>().ok())
    };
    let mut defines: Vec<String> = engine.iter().map(|d| d.to_string()).collect();
    defines.extend(crate::release::version_defines(version)?);
    let level = number("apiCompatibilityLevel").unwrap_or(6);
    match level {
        3 => defines.extend(["NET_4_6", "NET_UNITY_4_8"].map(String::from)),
        6 => defines.extend(
            [
                "NETSTANDARD",
                "NETSTANDARD2_1",
                "NET_STANDARD",
                "NET_STANDARD_2_0",
                "NET_STANDARD_2_1",
            ]
            .map(String::from),
        ),
        level => bail!(Unsupported(format!(
            "a Player of API compatibility level {level}, whose class library the tool has no table of"
        ))),
    }
    match number("activeInputHandler").unwrap_or(0) {
        0 => defines.push("ENABLE_LEGACY_INPUT_MANAGER".into()),
        1 => defines.push("ENABLE_INPUT_SYSTEM".into()),
        _ => {
            defines.extend(["ENABLE_INPUT_SYSTEM", "ENABLE_LEGACY_INPUT_MANAGER"].map(String::from))
        }
    }
    defines.sort();
    defines.dedup();
    let data_root = editor_data(editor);
    let dlls_of = |directory: &str| -> Result<Vec<String>> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Ok(found);
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.ends_with(".dll") {
                found.push(sysutil::join(&[directory, &name]));
            }
        }
        found.sort();
        Ok(found)
    };
    let mut references: Vec<String> = Vec::new();
    if level == 3 {
        let api = sysutil::join(&[&data_root, "UnityReferenceAssemblies/unity-4.8-api"]);
        references.extend(
            FRAMEWORK_REFERENCES
                .iter()
                .map(|name| sysutil::join(&[&api, &format!("{name}.dll")])),
        );
        references.extend(dlls_of(&sysutil::join(&[&api, "Facades"]))?);
    } else {
        // .NET Standard 2.1: the reference assembly, its shims of both
        // kinds, its extensions and the release's targeting pack.
        let standard = sysutil::join(&[&data_root, "NetStandard"]);
        references.push(sysutil::join(&[&standard, "ref/2.1.0/netstandard.dll"]));
        for directory in [
            "compat/2.1.0/shims/netstandard",
            "Extensions/2.0.0",
            "compat/2.1.0/shims/netfx",
        ] {
            references.extend(dlls_of(&sysutil::join(&[&standard, directory]))?);
        }
        references.extend(dlls_of(&sysutil::join(&[
            &data_root,
            "BCLExtensions/TargetingPacks/netstandard2.1/ref",
        ]))?);
    }
    references.extend(engine_module_references(editor, packages)?);
    let mut hashed = serde_json::Map::new();
    for path in references {
        hashed.insert(path.clone(), crate::util::file_hash(&path)?.into());
    }
    let bridge = sysutil::join(&[data, "Managed/UnityMicroEditor.dll"]);
    std::fs::write(
        sysutil::join(&[data, BUILD_PROOF]),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "backend": "unity_editor_build_sources",
            "unityVersion": version,
            "target": "StandaloneLinux64",
            "scriptingBackend": "Mono2x",
            "development": true,
            "assembledBy": "unity-micro-editor",
            "assemblies": {"UnityMicroEditor": {
                "name": "UnityMicroEditor",
                "configuration": {
                    "name": "UnityMicroEditor",
                    "sources": [],
                    "dependencies": [],
                    "defines": defines,
                    "nativeBaseAssemblies": [],
                    "compilerOptions": [],
                    "responseFiles": [],
                },
                "sources": {},
                "references": hashed,
                "dllSha256": crate::util::file_hash(&bridge)?,
            }},
        }))?,
    )?;
    Ok(())
}

/// The Editor's built-in shaders by path ID and name.
struct EditorBuiltins {
    tree: Arc<TypeTree>,
    objects: HashMap<i64, Vec<u8>>,
    names: HashMap<String, i64>,
}

fn editor_builtins(editor: &str) -> Result<EditorBuiltins> {
    let path = sysutil::join(&[&editor_data(editor), "Resources/unity_builtin_extra"]);
    let file = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let shaders = playerbake::read_serialized_of_class(&file, 48)?;
    let Some(first) = shaders.first() else {
        bail!("{path} holds no shader");
    };
    let tree = first.tree.clone();
    let mut objects = HashMap::new();
    let mut names = HashMap::new();
    let parsed = tree
        .children(0)
        .iter()
        .position(|&child| tree.nodes[child].name == "m_ParsedForm")
        .context("the Editor's Shader has no parsed form")?;
    for shader in shaders {
        let value = shader.tree.decode_leading(&shader.data, parsed + 1)?;
        if let Some(Val::Str(name)) = value
            .field("m_ParsedForm")
            .and_then(|parsed| parsed.field("m_Name"))
        {
            names.insert(name.clone(), shader.local_id);
        }
        objects.insert(shader.local_id, shader.data);
    }
    Ok(EditorBuiltins {
        tree,
        objects,
        names,
    })
}

/// Bakes built-in shaders on demand, a shader's fallback and the owners of
/// its passes first.
struct BuiltinShaders<'a> {
    ctx: &'a Context,
    editor: &'a str,
    project_dir: &'a str,
    logs: &'a str,
    builtins: &'a EditorBuiltins,
    project: &'a ShaderProject,
    materials: &'a HashMap<i64, Vec<playerbake::ShaderMaterialUse>>,
    baked: RefCell<BTreeMap<i64, std::result::Result<Vec<u8>, String>>>,
    depth: std::cell::Cell<usize>,
}

impl BuiltinShaders<'_> {
    fn bake(&self, id: i64) -> std::result::Result<Vec<u8>, String> {
        if let Some(known) = self.baked.borrow().get(&id) {
            return known.clone();
        }
        let outcome = (|| -> Result<Vec<u8>> {
            let Some(data) = self.builtins.objects.get(&id) else {
                bail!("the Editor's built-in resources have no shader {id}");
            };
            let artifact = playerbake::write_in(
                self.project.release.format(),
                &[playerbake::Object {
                    local_id: playerbake::SHADER_LOCAL_ID,
                    tree: &self.builtins.tree,
                    data: data.clone(),
                    script: None,
                    script_file: 0,
                }],
                &[],
            )?;
            let mut project = self.project.clone();
            project.materials = self.materials.get(&id).cloned().unwrap_or_default();
            let mut compiler = ShaderCompiler::start_with_folders(
                self.ctx,
                self.editor,
                self.project_dir,
                self.logs,
                &[],
            )?;
            if self.depth.get() > 4 {
                bail!("built-in shaders refer to each other too deeply at {id}");
            }
            self.depth.set(self.depth.get() + 1);
            let baked = playerbake::bake_shader_object(&artifact, &project, self, &mut compiler);
            self.depth.set(self.depth.get() - 1);
            let (object, externals) = baked?;
            if !externals.is_empty() {
                bail!("a built-in shader that names a shader of another file");
            }
            Ok(object)
        })()
        .map_err(|error| format!("{error:#}"));
        self.baked.borrow_mut().insert(id, outcome.clone());
        outcome
    }
}

impl playerbake::ShaderSources for BuiltinShaders<'_> {
    fn shader(&self, name: &str) -> Result<Option<ShaderDependency>> {
        let Some(&id) = self.builtins.names.get(name) else {
            return Ok(None);
        };
        let object = self.bake(id).map_err(|error| anyhow::anyhow!(error))?;
        Ok(Some(ShaderDependency {
            pointer: ShaderPointer::Raw {
                file_id: 0,
                path_id: id,
            },
            links: playerbake::read_shader_links_with(self.project.release, &object, &[])?,
        }))
    }
}

/// The built-in shaders the project's materials name, with how each
/// material uses its shader.
fn builtin_shader_users(
    project: &str,
    packages: &[String],
    builtins: &EditorBuiltins,
) -> HashMap<i64, Vec<playerbake::ShaderMaterialUse>> {
    let mut users: HashMap<i64, Vec<playerbake::ShaderMaterialUse>> = HashMap::new();
    let mut roots = vec![sysutil::join(&[project, "Assets"])];
    roots.extend(packages.iter().cloned());
    for root in roots {
        let mut materials = Vec::new();
        let _ = crate::compiler::walk_dir(&root, &mut |entry| {
            let name = sysutil::base(entry.path);
            if entry.is_dir() && (name.starts_with('.') || name.ends_with('~')) {
                return Ok(crate::compiler::Walk::SkipDir);
            }
            if !entry.is_dir() && name.ends_with(".mat") {
                materials.push(entry.path.to_string());
            }
            Ok(crate::compiler::Walk::Continue)
        });
        for path in materials {
            let Ok(source) = std::fs::read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&source);
            let Some(shader) = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("m_Shader:"))
            else {
                continue;
            };
            if !shader.contains("guid: 0000000000000000f000000000000000") {
                continue;
            }
            let Some(id) = shader
                .split("fileID:")
                .nth(1)
                .and_then(|rest| rest.split(',').next())
                .and_then(|id| id.trim().parse::<i64>().ok())
            else {
                continue;
            };
            if !builtins.objects.contains_key(&id) {
                continue;
            }
            if let Ok(usage) = super::baked::material_use(&path, &source) {
                users.entry(id).or_default().push(usage);
            }
        }
    }
    users
}

/// The project's always-included built-in shaders, as its graphics settings
/// list them.
fn authored_always_included(project: &str) -> Vec<i64> {
    let graphics = std::fs::read_to_string(sysutil::join(&[
        project,
        "ProjectSettings/GraphicsSettings.asset",
    ]))
    .unwrap_or_default();
    let mut ids = Vec::new();
    let mut listing = false;
    for line in graphics.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("m_AlwaysIncludedShaders:") {
            listing = true;
            continue;
        }
        if listing {
            let Some(item) = trimmed.strip_prefix("- ") else {
                break;
            };
            if item.contains("guid: 0000000000000000f000000000000000")
                && let Some(id) = item
                    .split("fileID:")
                    .nth(1)
                    .and_then(|rest| rest.split(',').next())
                    .and_then(|id| id.trim().parse::<i64>().ok())
            {
                ids.push(id);
            }
        }
    }
    ids
}

/// `Resources/unity_builtin_extra`: the built-in shaders the build always
/// includes, those the project's materials use, and what they take passes
/// of or fall back on, each at its path ID in the Editor's file. Returns
/// the path IDs written and those the baker refused.
fn write_builtin_extra(
    ctx: &Context,
    project: &str,
    editor: &str,
    data: &str,
    packages: &[String],
) -> Result<(Vec<i64>, Refused)> {
    let version = crate::release::project_release(project)?;
    let release = ShaderRelease::of(version)
        .ok_or_else(|| Unsupported(format!("baking the shaders of Unity {version}")))?;
    let builtins = editor_builtins(editor)?;
    let read =
        |name: &str| std::fs::read_to_string(sysutil::join(&[project, name])).unwrap_or_default();
    let player = read("ProjectSettings/ProjectSettings.asset");
    let shader_project = ShaderProject {
        release,
        // A host draws with OpenGL Core alone.
        platforms: vec![15],
        settings: playerbake::ShaderSettings::read(
            &player,
            &read("ProjectSettings/GraphicsSettings.asset"),
            &read("ProjectSettings/QualitySettings.asset"),
            &read("Packages/manifest.json"),
        )?,
        materials: Vec::new(),
    };
    let materials = builtin_shader_users(project, packages, &builtins);
    let mut wanted: Vec<i64> = authored_always_included(project);
    wanted.extend(engine_always_included(version));
    wanted.extend(materials.keys().copied());
    let logs = sysutil::join(&[data, "..", "builtin-shader-logs"]);
    let bakery = BuiltinShaders {
        ctx,
        editor,
        project_dir: project,
        logs: &logs,
        builtins: &builtins,
        project: &shader_project,
        materials: &materials,
        baked: RefCell::new(BTreeMap::new()),
        depth: std::cell::Cell::new(0),
    };
    for id in &wanted {
        if builtins.objects.contains_key(id) {
            let _ = bakery.bake(*id);
        }
    }
    let _ = std::fs::remove_dir_all(&logs);
    let baked = bakery.baked.into_inner();
    let tree = release.tree()?;
    let hash: [u8; 16] = tree.entry[7..23].try_into()?;
    let mut objects = Vec::new();
    let mut written = Vec::new();
    let mut refused = Vec::new();
    for (id, outcome) in baked {
        match outcome {
            Ok(object) => {
                objects.push((id, 0, object));
                written.push(id);
            }
            Err(error) => refused.push((id, error)),
        }
    }
    std::fs::write(
        sysutil::join(&[data, "Resources/unity_builtin_extra"]),
        playerbake::plain_file(version, &[(48, hash)], &objects, &[])?,
    )?;
    Ok((written, refused))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_is_named_below_its_last_resources_folder_in_lower_case() {
        let name = |path: &str| resource_name(Path::new(path));
        assert_eq!(
            name("Assets/Resources/UI/Main Menu.prefab").as_deref(),
            Some("ui/main menu")
        );
        assert_eq!(
            name("Assets/A/Resources/B/Resources/x.y.asset").as_deref(),
            Some("x.y")
        );
        assert_eq!(name("Assets/Editor/Resources/x.png"), None);
        assert_eq!(name("Assets/Textures/x.png"), None);
    }

    #[test]
    fn a_catalog_is_stamped_with_the_hash_mono_gives_its_class_name() {
        // As an Addressables 2.7 build of Bouncemasters stamps catalog.bin.
        assert_eq!(mono_string_hash("ContentCatalogData"), 233015618);
        let catalog = empty_catalog();
        assert_eq!(&catalog[..8], &[0x42, 0x89, 0xe3, 0x0d, 2, 0, 0, 0]);
        assert_eq!(&catalog[8..12], &36u32.to_le_bytes());
    }

    #[test]
    fn preload_data_lists_its_objects_and_no_dependencies() {
        let data = preload_data(&[(1, 7)]);
        assert_eq!(data.len(), 4 + 4 + 12 + 4 + 1);
        assert_eq!(&data[8..12], &1i32.to_le_bytes());
        assert_eq!(&data[12..20], &7i64.to_le_bytes());
    }
}
