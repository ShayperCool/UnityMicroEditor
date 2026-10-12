//! Material inspectors, run by the engine in a standalone worker.
//!
//! A build writes a material variant after the engine has built its
//! properties, and building them runs the drawers of the shader's properties
//! and the `ValidateMaterial` of the shader's inspector (`CustomEditor`),
//! which is C# of a package or the project: it sets keywords, the render
//! queue, tags, disabled passes and derived properties. Nothing of that is
//! restated here. A worker loads the project's own compiled inspector, the
//! engine calls it as it does in the Editor, and a small importer of this
//! tool (`tools/MaterialInspector`) saves each variant the way the engine
//! leaves it. `playerbake::bake_inspected_variant` merges that document as a
//! build does.
//!
//! The worker imports the shaders of the chains itself, in the same run and
//! before the materials are read: a `.shader` after the files it includes, a
//! shader graph after the sources it refers to and the files its shader
//! includes, with the templates of the graph targets beside them. Every
//! accepted import is recorded in the context's `Library/ArtifactDB`
//! (`artifactdb.rs`), which is what makes the shader loadable through a
//! material's reference and lets an inspector ask the asset database about
//! it: the inspectors of Shader Graph shaders read the graph's metadata
//! that way. A texture a chain names is a one-pixel texture stored as a
//! native asset under the texture's GUID, with the references retyped to
//! match. Only inspectors compared with the Editor are accepted, each for
//! the package version it was compared in.
use super::{
    ProbeOptions,
    build_settings::{Sources, include_closure},
    context::{BuildFiles, ContextExtras},
    import::{BuildImport, run_build_import},
    player_objects::{MISSING, artifact_texts, authored, graph_sources},
    scripted::{
        ScriptEnvironment, ScriptSelection, inspection_environment, script_environment_identity,
        scripted_importer,
    },
    support::{is_not_exist, lower_ext, mkdir_all, read_file, stat, wrap, write_file},
};
use crate::{cancel::Context, compiler, sha256, sysutil};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
    time::{Duration, Instant},
};

/// The package the inspectors of hand-written shaders and of the graph
/// targets were compared in, as the package cache names the version
/// (17.6.0).
const URP: &str = "com.unity.render-pipelines.universal@0ffb3d4f1461";

/// The inspectors whose results in a standalone worker were compared with
/// Editor builds, object for object (`docs/NATIVE_IMPORT.md`), each with
/// the package version it was compared in. Another version may read what
/// this one does not.
pub const VERIFIED_INSPECTORS: [(&str, &str); 8] = [
    (
        "UnityEditor.Rendering.Universal.ShaderGUI.BakedLitShader",
        URP,
    ),
    ("UnityEditor.Rendering.Universal.ShaderGUI.LitShader", URP),
    (
        "UnityEditor.Rendering.Universal.ShaderGUI.ParticlesLitShader",
        URP,
    ),
    (
        "UnityEditor.Rendering.Universal.ShaderGUI.ParticlesUnlitShader",
        URP,
    ),
    (
        "UnityEditor.Rendering.Universal.ShaderGUI.SimpleLitShader",
        URP,
    ),
    ("UnityEditor.Rendering.Universal.ShaderGUI.UnlitShader", URP),
    ("UnityEditor.ShaderGraphLitGUI", URP),
    ("UnityEditor.ShaderGraphUnlitGUI", URP),
];

/// The package whose importer wrote the graphs of that comparison
/// (17.6.0). An inspector of a graph shader reads the metadata it leaves.
pub const VERIFIED_GRAPH_PACKAGE: &str = "com.unity.shadergraph@48089545eeac";

/// The files that state which packages a project has.
const PACKAGE_FILES: [&str; 2] = ["Packages/manifest.json", "Packages/packages-lock.json"];
/// How many variants one worker run builds.
const VARIANTS_A_RUN: usize = 1500;
/// How often a run is repeated because the shader of a graph turned out to
/// include files the context did not hold.
const GRAPH_ROUNDS: usize = 5;

const HELPER_SOURCE: &str = include_str!("../../../tools/MaterialInspector/MaterialInspector.cs");
const HELPER_ASSEMBLY: &str = "UnityMicroEditor.MaterialInspector";
const STAND_INS: &str = "Assets/UnityMicroEditorInspect";
const LIST_GUID: &str = "5c1d3e0a9b7f4e62a1d08c3b6f2e4a17";
const DEEPEST_PARENT: usize = 32;
const TEXTURE_2D: &str = "2800000";

/// A variant as the engine leaves it once its inspector has run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedMaterial {
    /// The class the engine ran.
    pub inspector: String,
    /// The package directory that class is compiled from.
    pub package: String,
    /// The GUID of the shader at the top of the chain.
    pub shader: String,
    /// The variant built over parents as their sources give them.
    pub over_authored: Vec<u8>,
    /// The variant built after the variants among its parents were built.
    pub over_inspected: Vec<u8>,
    /// The hash of every file of the project the result was made from, by
    /// logical path: the materials of the chain, the shader with what it
    /// includes or refers to, the render pipeline asset, the settings and
    /// the package files. Files of an immutable package are left out.
    pub ingredients: BTreeMap<String, String>,
}

/// What became of one requested variant: its documents, or why it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialInspection {
    pub path: String,
    pub guid: String,
    pub result: Result<InspectedMaterial, String>,
}

/// The results of one call.
#[derive(Clone, Debug, Default)]
pub struct Inspection {
    /// One entry for each requested variant, in the order of the request.
    pub materials: Vec<MaterialInspection>,
    /// The shader of each inspected chain as the worker imported it, by
    /// GUID: the primary artifact, which holds the `Shader` object.
    pub shaders: BTreeMap<String, Vec<u8>>,
    /// How many variants were answered from kept results.
    pub kept: usize,
    /// How many workers ran.
    pub worker_runs: usize,
    pub seconds: f64,
}

pub struct InspectionOptions<'a> {
    /// The Unity executable.
    pub editor: String,
    pub project: String,
    /// The sources of the project and its packages.
    pub sources: &'a Sources,
    /// A directory under the project's `Library/UnityMicroEditor` or
    /// `Temp`; each worker run gets a new context beneath it.
    pub output: String,
    /// For each worker run.
    pub timeout: Duration,
    /// A directory that keeps results from one call to the next, each
    /// under a name that identifies everything it was made from; none when
    /// empty.
    pub cache: String,
}

/// An object reference as a source spells it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Reference {
    file_id: String,
    guid: String,
    kind: String,
}

static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{fileID: (-?[0-9]+), guid: ([0-9a-f]{32}), type: ([0-9])\}").unwrap()
});

fn references(text: &str) -> Vec<Reference> {
    REFERENCE
        .captures_iter(text)
        .map(|found| Reference {
            file_id: found[1].to_string(),
            guid: found[2].to_string(),
            kind: found[3].to_string(),
        })
        .collect()
}

/// The text with every reference to one of `guids` given the type `kind`
/// returns for it.
fn retyped(text: &str, kind: &dyn Fn(&str, &str) -> Option<String>) -> String {
    REFERENCE
        .replace_all(text, |found: &regex::Captures<'_>| {
            let kind = kind(&found[1], &found[2]).unwrap_or_else(|| found[3].to_string());
            format!(
                "{{fileID: {}, guid: {}, type: {kind}}}",
                &found[1], &found[2]
            )
        })
        .into_owned()
}

/// One material of a chain: where it lies and what it names.
#[derive(Clone, Debug)]
struct Member {
    text: String,
    anchor: String,
    parent: Option<String>,
}

static MATERIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^--- !u!21 &(-?[0-9]+)\s*$").unwrap());
static PARENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^  m_Parent: \{fileID: (-?[0-9]+)(?:, guid: ([0-9a-f]{32}), type: [0-9])?\}")
        .unwrap()
});
static SHADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^  m_Shader: \{fileID: (-?[0-9]+), guid: ([0-9a-f]{32}), type: ([0-9])\}")
        .unwrap()
});

fn member(path: &str, data: &[u8]) -> Result<Member, String> {
    let text = String::from_utf8(data.to_vec()).map_err(|_| format!("{path} is not text"))?;
    let mut anchors = MATERIAL.captures_iter(&text);
    let (Some(anchor), None) = (anchors.next(), anchors.next()) else {
        return Err(format!("{path} does not hold one material"));
    };
    let anchor = anchor[1].to_string();
    let parent = match PARENT.captures(&text) {
        Some(found) if &found[1] == "0" => None,
        Some(found) => match found.get(2) {
            Some(guid) => Some(guid.as_str().to_string()),
            None => return Err(format!("{path} names a parent inside its own source")),
        },
        None => return Err(format!("{path} states no parent")),
    };
    Ok(Member {
        text,
        anchor,
        parent,
    })
}

/// The part of a source that is its material: other documents, such as the
/// version record a render pipeline keeps, are not the engine's to resolve.
fn material_text(text: &str) -> &str {
    let Some(start) = MATERIAL.find(text) else {
        return "";
    };
    let rest = &text[start.end()..];
    let end = rest.find("\n--- ").map_or(rest.len(), |end| end + 1);
    &rest[..end]
}

/// The classes a shader source names as its inspector, for any pipeline.
fn inspector_classes(source: &str) -> BTreeSet<String> {
    static EDITOR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^\s*CustomEditor(?:ForRenderPipeline)?\s+"([^"]+)""#).unwrap()
    });
    EDITOR
        .captures_iter(source)
        .map(|found| found[1].to_string())
        .collect()
}

/// The GUID of the render pipeline asset the project draws with, when every
/// quality level that names one names the same.
fn pipeline_guid(graphics: &str, quality: &str) -> Result<Option<String>> {
    static DEFAULT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^  m_CustomRenderPipeline: \{fileID: (-?[0-9]+)(?:, guid: ([0-9a-f]{32}))?",
        )
        .unwrap()
    });
    static LEVEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s+customRenderPipeline: \{fileID: (-?[0-9]+)(?:, guid: ([0-9a-f]{32}))?")
            .unwrap()
    });
    let named = |found: regex::Captures<'_>| found.get(2).map(|guid| guid.as_str().to_string());
    let mut pipeline = DEFAULT.captures(graphics).and_then(named);
    for level in LEVEL.captures_iter(quality) {
        let Some(guid) = named(level) else { continue };
        match &pipeline {
            Some(other) if *other != guid => bail!(
                "the quality levels of the project draw with different render pipeline assets, and which one a build inspects materials under is not known"
            ),
            _ => pipeline = Some(guid),
        }
    }
    Ok(pipeline)
}

/// Whether the project settings state the linear color space.
fn color_space(settings: &str) -> Option<bool> {
    let value = settings
        .lines()
        .find_map(|line| line.trim().strip_prefix("m_ActiveColorSpace:"))?;
    match value.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn native_meta(guid: &str, main: &str) -> Vec<u8> {
    format!(
        "fileFormatVersion: 2\nguid: {guid}\nNativeFormatImporter:\n  externalObjects: {{}}\n  mainObjectFileID: {main}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
    )
    .into_bytes()
}

/// A texture of one white pixel, which stands for a texture a chain names:
/// the inspectors accepted here ask only whether a texture is set.
fn texture_stand_in(name: &str) -> Vec<u8> {
    format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!28 &{TEXTURE_2D}\nTexture2D:\n  m_Name: {name}\n  serializedVersion: 4\n  m_Width: 1\n  m_Height: 1\n  m_CompleteImageSize: 4\n  m_MipsStripped: 0\n  m_TextureFormat: 4\n  m_MipCount: 1\n  m_IsReadable: 1\n  m_ImageCount: 1\n  m_TextureDimension: 2\n  m_ColorSpace: 1\n  image data: 4\n  _typelessdata: ffffffff\n"
    )
    .into_bytes()
}

/// The worker's document with the anchor and the reference types of the
/// authored sources, which the context had changed.
fn restored(document: &str, anchor: &str, authored: &BTreeMap<(String, String), String>) -> String {
    let text = retyped(document, &|file_id, guid| {
        authored
            .get(&(file_id.to_string(), guid.to_string()))
            .cloned()
    });
    MATERIAL
        .replace(&text, format!("--- !u!21 &{anchor}"))
        .into_owned()
}

/// The compiled importer: kept from an earlier call, or compiled now.
fn helper(ctx: &Context, options: &InspectionOptions<'_>) -> Result<Vec<u8>> {
    let identity = identity_of(&[
        "helper",
        crate::ENGINE_VERSION,
        &sha256(HELPER_SOURCE.as_bytes()),
    ]);
    let kept = sysutil::join(&[&options.cache, &format!("helper-{identity}.dll")]);
    if !options.cache.is_empty()
        && let Ok(data) = read_file(&kept)
        && !data.is_empty()
    {
        return Ok(data);
    }
    let data = compile_helper(
        ctx,
        &options.editor,
        &sysutil::join(&[
            &options.output,
            &format!("helper-{}", crate::util::random_id()),
        ]),
    )?;
    if !options.cache.is_empty() {
        keep(&kept, &data)?;
    }
    Ok(data)
}

/// Compiles the importer that saves the variants, with the compiler and the
/// engine assemblies of the Unity installation.
fn compile_helper(ctx: &Context, editor: &str, directory: &str) -> Result<Vec<u8>> {
    let data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
    let mut compilers = compiler::glob_lossy(&sysutil::join(&[
        &data,
        "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll",
    ]));
    compilers.sort();
    let Some(csc) = compilers.pop() else {
        bail!("the Unity installation at {data} has no C# compiler");
    };
    mkdir_all(directory, 0o700)?;
    let source = sysutil::join(&[directory, "MaterialInspector.cs"]);
    let output = sysutil::join(&[directory, &format!("{HELPER_ASSEMBLY}.dll")]);
    write_file(&source, HELPER_SOURCE.as_bytes(), 0o600)?;
    let mut arguments = vec![
        "-nologo".to_string(),
        "-nostdlib+".to_string(),
        "-target:library".to_string(),
        "-langversion:9".to_string(),
        "-deterministic+".to_string(),
        "-warnaserror+".to_string(),
        format!("-out:{output}"),
    ];
    let api = sysutil::join(&[&data, "UnityReferenceAssemblies/unity-4.8-api"]);
    let mut libraries = compiler::glob_lossy(&sysutil::join(&[&data, "Managed/UnityEngine/*.dll"]));
    for name in [
        "mscorlib.dll",
        "System.dll",
        "System.Core.dll",
        "Facades/netstandard.dll",
    ] {
        libraries.push(sysutil::join(&[&api, name]));
    }
    libraries.sort();
    arguments.extend(
        libraries
            .iter()
            .map(|library| format!("-reference:{library}")),
    );
    arguments.push(source);
    let response = sysutil::join(&[directory, "compile.rsp"]);
    let mut text = String::new();
    for argument in &arguments {
        text.push_str(&format!("\"{}\"\n", argument.replace('"', "\\\"")));
    }
    write_file(&response, text.as_bytes(), 0o600)?;
    compiler::compiler_command(
        ctx,
        directory,
        &sysutil::join(&[&data, "DotNetSdk/dotnet"]),
        &[&csc, &format!("@{response}")],
    )
    .map_err(|error| anyhow!("compile the material inspection importer: {error}"))?;
    read_file(&output)
}

struct Chain {
    /// The requested variant first, the material without a parent last.
    members: Vec<String>,
    /// The GUID of the shader the last member names.
    shader: String,
}

/// A shader the chains end in, and what the worker imports before it.
struct ShaderSource {
    path: String,
    /// The inspectors a `.shader` source names; a graph's are named by the
    /// shader its importer writes.
    classes: BTreeSet<String>,
    graph: bool,
    /// The sources a graph refers to, each after those it refers to itself.
    parts: Vec<String>,
    /// The text a `.shader` source includes files from.
    text: Vec<u8>,
    /// The hash of every file of the project it is made of, its own
    /// included, by logical path.
    ingredients: BTreeMap<String, String>,
    /// Identifies all of that and the files of packages beside it.
    identity: String,
}

/// What is the same for every variant of a call.
struct Setting {
    space: &'static str,
    /// The path of the render pipeline asset.
    pipeline: Option<String>,
    /// The hash of every settings file read, by logical path.
    ingredients: BTreeMap<String, String>,
    /// Identifies the engine, the compiled scripts, the importer of this
    /// tool and the settings.
    identity: String,
}

/// One kept result, `material-<key>.json` in the cache directory.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Kept {
    path: String,
    guid: String,
    /// The identity of the shader, whose imported form lies beside it.
    shader: String,
    inspector: String,
    package: String,
    over_authored: String,
    over_inspected: String,
    refused: String,
    ingredients: BTreeMap<String, String>,
}

fn kept_path(cache: &str, key: &str) -> String {
    sysutil::join(&[cache, &format!("material-{key}.json")])
}

fn shader_path(cache: &str, identity: &str) -> String {
    sysutil::join(&[cache, &format!("shader-{identity}.bin")])
}

fn includes_path(cache: &str, identity: &str) -> String {
    sysutil::join(&[cache, &format!("graph-includes-{identity}.json")])
}

/// Writes a file of the cache under its final name only when it is whole.
fn keep(path: &str, data: &[u8]) -> Result<()> {
    mkdir_all(&sysutil::dir(path), 0o700)?;
    let partial = format!("{path}.{}.partial", crate::util::random_id());
    write_file(&partial, data, 0o600)?;
    std::fs::rename(&partial, path).map_err(|error| anyhow!("rename {partial} {path}: {error}"))
}

fn file_ingredient(
    sources: &Sources,
    project: &str,
    logical: &str,
    ingredients: &mut BTreeMap<String, String>,
) -> Result<()> {
    if !authored(sources, project, logical) {
        return Ok(());
    }
    let Some(physical) = sources.physical(logical) else {
        return Ok(());
    };
    ingredients.insert(logical.to_string(), crate::util::file_hash(&physical)?);
    match crate::util::file_hash(&format!("{physical}.meta")) {
        Ok(hash) => {
            ingredients.insert(format!("{logical}.meta"), hash);
        }
        Err(error) if is_not_exist(&error) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn identity_of(parts: &[&str]) -> String {
    let mut text = String::new();
    for part in parts {
        text.push_str(part);
        text.push('\0');
    }
    sha256(text.as_bytes())
}

fn map_text(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect()
}

fn shader_source(sources: &Sources, project: &str, path: &str) -> Result<ShaderSource, String> {
    let read = || -> Result<ShaderSource> {
        let graph = lower_ext(path) == ".shadergraph";
        let text = sources.read(path)?;
        let (parts, classes, included) = if graph {
            (graph_sources(sources, path)?, BTreeSet::new(), Vec::new())
        } else {
            (
                Vec::new(),
                inspector_classes(&String::from_utf8_lossy(&text)),
                include_closure(sources, &[(path, &text[..])])?,
            )
        };
        let mut ingredients = BTreeMap::new();
        let mut packaged = String::new();
        for file in std::iter::once(&path.to_string())
            .chain(&parts)
            .chain(&included)
        {
            file_ingredient(sources, project, file, &mut ingredients)?;
            if !ingredients.contains_key(file) {
                // A file of a cached package: its directory names its content.
                packaged.push_str(&sources.physical(file).unwrap_or_default());
                packaged.push('\n');
            }
        }
        let identity = identity_of(&["shader", path, &map_text(&ingredients), &packaged]);
        Ok(ShaderSource {
            path: path.to_string(),
            classes,
            graph,
            parts,
            text: if graph { Vec::new() } else { text },
            ingredients,
            identity,
        })
    };
    read().map_err(|error| format!("{path}: {error:#}"))
}

/// The assets a run imports before the materials are inspected, in order:
/// what the shaders include, what the graphs refer to, the shaders.
fn shader_batch(
    sources: &Sources,
    shaders: &[&ShaderSource],
    known: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<String>> {
    let mut listings = Vec::new();
    for shader in shaders {
        // The files a graph's shader was found to include, as a text that
        // includes them, to follow what they include in turn.
        let listing: Vec<u8> = match shader.graph {
            true => known
                .get(&shader.path)
                .into_iter()
                .flatten()
                .map(|path| format!("#include \"{path}\"\n"))
                .collect::<String>()
                .into_bytes(),
            false => shader.text.clone(),
        };
        listings.push((shader.path.as_str(), listing));
    }
    let texts: Vec<(&str, &[u8])> = listings
        .iter()
        .map(|(path, text)| (*path, text.as_slice()))
        .collect();
    let mut batch = include_closure(sources, &texts)?;
    for path in shaders
        .iter()
        .flat_map(|shader| &shader.parts)
        .chain(shaders.iter().map(|shader| &shader.path))
    {
        if !batch.contains(path) {
            batch.push(path.clone());
        }
    }
    Ok(batch)
}

/// What the worker left of one run.
struct Round {
    report: String,
    written: String,
    /// The primary artifact of each imported shader, by path.
    artifacts: BTreeMap<String, Vec<u8>>,
}

/// Runs the inspectors of the given variants (project paths under `Assets`)
/// and returns what the engine leaves of each, or why a variant has no
/// result. An error is a failure of the run as a whole.
pub fn inspect_material_variants(
    ctx: &Context,
    options: &InspectionOptions<'_>,
    variants: &[String],
) -> Result<Inspection> {
    let started = Instant::now();
    let project = &options.project;
    let sources = options.sources;
    let locate = |guid: &str| sources.path_of(guid).map(String::from);
    let read = |path: &str| -> Result<Vec<u8>, String> {
        if !path.starts_with("Assets/") {
            return Err(format!("{path} is not under Assets"));
        }
        sources
            .read(path)
            .map_err(|error| format!("{path}: {error:#}"))
    };
    // Every material of every chain, and which of them are requested.
    let mut members: BTreeMap<String, Member> = BTreeMap::new();
    let mut guids: BTreeMap<String, String> = BTreeMap::new();
    let mut outcomes: Vec<(String, String, Result<Chain, String>)> = Vec::new();
    for variant in variants {
        let mut chain = Vec::new();
        let walked = (|| -> Result<Chain, String> {
            let mut path = variant.clone();
            loop {
                if chain.contains(&path) {
                    return Err(format!("the parents of {variant} form a cycle"));
                }
                if chain.len() > DEEPEST_PARENT {
                    return Err(format!("{variant} has more than {DEEPEST_PARENT} parents"));
                }
                if lower_ext(&path) != ".mat" {
                    return Err(format!(
                        "{variant} has the parent {path}, which is not a material source"
                    ));
                }
                if !members.contains_key(&path) {
                    let guid = sources
                        .guid_of(&path)
                        .ok_or_else(|| format!("{path} has no .meta"))?;
                    members.insert(path.clone(), member(&path, &read(&path)?)?);
                    guids.insert(path.clone(), guid.to_string());
                }
                chain.push(path.clone());
                match &members[&path].parent {
                    None => break,
                    Some(guid) => match locate(guid) {
                        Some(parent) => path = parent,
                        None => return Err(format!("the parent {guid} of {path} is not known")),
                    },
                }
            }
            if chain.len() < 2 {
                return Err(format!("{variant} is not a variant"));
            }
            Ok(Chain {
                members: chain.clone(),
                shader: String::new(),
            })
        })();
        let guid = guids.get(variant).cloned().unwrap_or_default();
        outcomes.push((variant.clone(), guid, walked));
    }
    // What the chains name: shaders to import, textures to stand in for.
    let mut shaders: BTreeMap<String, ShaderSource> = BTreeMap::new();
    let mut refused_shaders: BTreeMap<String, String> = BTreeMap::new();
    let mut textures: BTreeMap<String, String> = BTreeMap::new();
    let mut authored_kinds: BTreeMap<(String, String), String> = BTreeMap::new();
    for (_, _, outcome) in &mut outcomes {
        let Ok(chain) = outcome else { continue };
        let mut named = Vec::new();
        let checked = (|| -> Result<String, String> {
            for path in &chain.members {
                let member = &members[path];
                let text = material_text(&member.text);
                let shader = SHADER.captures(text).map(|found| found[2].to_string());
                for reference in references(text) {
                    // The parent is a material of the chain.
                    if Some(&reference.guid) == member.parent.as_ref() {
                        continue;
                    }
                    // An asset the project lacks stays missing, as in the Editor.
                    let Some(target) = locate(&reference.guid) else {
                        continue;
                    };
                    if Some(&reference.guid) == shader.as_ref() && reference.kind == "3" {
                        if !matches!(lower_ext(&target).as_str(), ".shader" | ".shadergraph") {
                            return Err(format!(
                                "{path} uses the shader {target}, which is not a shader source"
                            ));
                        }
                    } else if reference.file_id == TEXTURE_2D && reference.kind == "3" {
                        named.push(reference);
                    } else {
                        return Err(format!(
                            "{path} refers to object {} of {target}, for which the inspection has no stand-in",
                            reference.file_id
                        ));
                    }
                }
            }
            // The shader of the chain is the one its last material names.
            let top = chain.members.last().unwrap();
            let named = SHADER
                .captures(material_text(&members[top].text))
                .map(|found| (found[2].to_string(), found[3].to_string()));
            let shader = named
                .as_ref()
                .filter(|(_, kind)| kind == "3")
                .and_then(|(guid, _)| Some((guid.clone(), locate(guid)?)))
                .filter(|(_, path)| matches!(lower_ext(path).as_str(), ".shader" | ".shadergraph"));
            let Some((guid, path)) = shader else {
                return Err(format!(
                    "the shader of {top} is not a source of the project"
                ));
            };
            if let Some(reason) = refused_shaders.get(&guid) {
                return Err(reason.clone());
            }
            if !shaders.contains_key(&guid) {
                match shader_source(sources, project, &path) {
                    Ok(source) => {
                        shaders.insert(guid.clone(), source);
                    }
                    Err(reason) => {
                        refused_shaders.insert(guid.clone(), reason.clone());
                        return Err(reason);
                    }
                }
            }
            let source = &shaders[&guid];
            if !source.graph && source.classes.is_empty() {
                // Such a variant is baked from its source alone.
                return Err(format!(
                    "the shader {path} of {} names no inspector",
                    chain.members[0]
                ));
            }
            Ok(guid)
        })();
        match checked {
            Ok(shader) => chain.shader = shader,
            Err(reason) => {
                *outcome = Err(reason);
                continue;
            }
        }
        for reference in named {
            let target = locate(&reference.guid).unwrap_or_default();
            let base = sysutil::base(&target);
            let name = base
                .rsplit_once('.')
                .map_or(base.as_str(), |(name, _)| name);
            textures.insert(reference.guid.clone(), name.to_string());
            authored_kinds.insert((reference.file_id, reference.guid), reference.kind);
        }
    }
    let mut inspection = Inspection::default();
    if !outcomes.iter().any(|(_, _, outcome)| outcome.is_ok()) {
        inspection.materials = outcomes
            .into_iter()
            .map(|(path, guid, outcome)| MaterialInspection {
                result: Err(outcome.err().unwrap_or_default()),
                path,
                guid,
            })
            .collect();
        inspection.seconds = started.elapsed().as_secs_f64();
        return Ok(inspection);
    }
    let setting = setting(sources, project)?;

    // What each variant is made of, and the results kept of those that
    // are made of the same as before.
    let mut subjects: BTreeMap<String, (String, BTreeMap<String, String>)> = BTreeMap::new();
    let mut results: BTreeMap<String, Result<InspectedMaterial, String>> = BTreeMap::new();
    for (path, _, outcome) in &outcomes {
        let Ok(chain) = outcome else { continue };
        if subjects.contains_key(path) {
            continue;
        }
        let shader = &shaders[&chain.shader];
        let mut ingredients = setting.ingredients.clone();
        ingredients.extend(shader.ingredients.clone());
        // Whether an asset a chain names exists, and under which name,
        // decides what stands for it.
        let mut named = String::new();
        for member in &chain.members {
            file_ingredient(sources, project, member, &mut ingredients)?;
            for reference in references(material_text(&members[member].text)) {
                named.push_str(&format!(
                    "{} {} {}\n",
                    reference.file_id,
                    reference.guid,
                    locate(&reference.guid).unwrap_or_default()
                ));
            }
        }
        let key = identity_of(&[
            "material",
            &setting.identity,
            &shader.identity,
            path,
            &chain.members.join("\n"),
            &map_text(&ingredients),
            &named,
        ]);
        if !options.cache.is_empty()
            && let Ok(kept) = crate::util::read_json_file::<Kept>(&kept_path(&options.cache, &key))
            && kept.path == *path
            && kept.shader == shader.identity
            && kept.ingredients == ingredients
        {
            let artifact = match kept.refused.is_empty() {
                true => read_file(&shader_path(&options.cache, &shader.identity)).ok(),
                false => Some(Vec::new()),
            };
            if let Some(artifact) = artifact {
                results.insert(
                    path.clone(),
                    match kept.refused.is_empty() {
                        true => {
                            inspection.shaders.insert(chain.shader.clone(), artifact);
                            Ok(InspectedMaterial {
                                inspector: kept.inspector,
                                package: kept.package,
                                shader: chain.shader.clone(),
                                over_authored: kept.over_authored.into_bytes(),
                                over_inspected: kept.over_inspected.into_bytes(),
                                ingredients: ingredients.clone(),
                            })
                        }
                        false => Err(kept.refused),
                    },
                );
                inspection.kept += 1;
            }
        }
        subjects.insert(path.clone(), (key, ingredients));
    }

    let pending: Vec<&String> = subjects
        .keys()
        .filter(|path| !results.contains_key(*path))
        .collect();
    for group in pending.chunks(VARIANTS_A_RUN) {
        let mut group: Vec<String> = group.iter().map(|path| (*path).clone()).collect();
        let mut chains: Vec<&Chain> = outcomes
            .iter()
            .filter(|(path, _, _)| group.contains(path))
            .filter_map(|(_, _, outcome)| outcome.as_ref().ok())
            .collect();
        let mut used: Vec<&ShaderSource> = shaders
            .iter()
            .filter(|(guid, _)| chains.iter().any(|chain| chain.shader == **guid))
            .map(|(_, shader)| shader)
            .collect();
        // The includes of a graph's shader are known once it is imported.
        let mut known: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for shader in used.iter().filter(|shader| shader.graph) {
            let kept = (!options.cache.is_empty())
                .then(|| read_file(&includes_path(&options.cache, &shader.identity)).ok())
                .flatten()
                .and_then(|data| serde_json::from_slice::<Vec<String>>(&data).ok())
                .unwrap_or_default();
            known.insert(shader.path.clone(), kept);
        }
        // What the compiled code of the project does not allow holds for
        // every variant of the run; each is refused for it.
        let environment = match round_environment(sources, project, &used) {
            Ok(environment) => environment,
            Err(error) => {
                for path in &group {
                    results.insert(path.clone(), Err(format!("{path}: {error:#}")));
                }
                continue;
            }
        };
        let mut unsettled: BTreeMap<String, String> = BTreeMap::new();
        let mut graph_rounds = 0;
        let round = loop {
            if group.is_empty() {
                break None;
            }
            let batch = shader_batch(sources, &used, &known)?;
            let output = sysutil::join(&[
                &options.output,
                &format!("inspect-{}", inspection.worker_runs),
            ]);
            let outcome = run(
                ctx,
                options,
                &setting,
                &environment,
                &output,
                &batch,
                &group,
                &chains,
                &members,
                &textures,
            )?;
            inspection.worker_runs += 1;
            let run = match outcome {
                Ok(run) => run,
                // A shader the worker does not import, or a part of one:
                // its variants are refused and the others inspected.
                Err((asset, message)) => {
                    let mut dropped = Vec::new();
                    for shader in &used {
                        if shader_batch(sources, &[*shader], &known)?.contains(&asset) {
                            dropped.push(shader.path.clone());
                        }
                    }
                    if dropped.is_empty() {
                        bail!("inspect the material variants: {asset}: {message}");
                    }
                    group.retain(|path| {
                        let shader = chains
                            .iter()
                            .find(|chain| chain.members[0] == *path)
                            .map(|chain| &shaders[&chain.shader].path);
                        match shader.filter(|shader| dropped.contains(shader)) {
                            Some(shader) => {
                                results.insert(
                                    path.clone(),
                                    Err(format!(
                                        "the worker does not import {asset}, which the shader {shader} of {path} is made of: {message}"
                                    )),
                                );
                                false
                            }
                            None => true,
                        }
                    });
                    chains.retain(|chain| group.contains(&chain.members[0]));
                    used.retain(|shader| !dropped.contains(&shader.path));
                    continue;
                }
            };
            let mut added = false;
            unsettled.clear();
            for shader in used.iter().filter(|shader| shader.graph) {
                let Some(artifact) = run.artifacts.get(&shader.path) else {
                    bail!("the worker left no artifact of {}", shader.path);
                };
                let text = artifact_texts(artifact)?;
                let list = known.get_mut(&shader.path).unwrap();
                for found in MISSING.captures_iter(&text) {
                    let path = String::from_utf8_lossy(&found[1]).into_owned();
                    if list.contains(&path) {
                        unsettled.insert(
                            shader.path.clone(),
                            format!(
                                "the importer of the shader graph {} leaves the include {path} out although its file is an asset of the worker",
                                shader.path
                            ),
                        );
                    } else if sources
                        .physical(&path)
                        .is_some_and(|physical| stat(&physical).is_ok_and(|info| info.is_regular()))
                    {
                        list.push(path);
                        added = true;
                    } else {
                        unsettled.insert(
                            shader.path.clone(),
                            format!(
                                "the shader of the shader graph {} includes {path}, which no source of the project or its packages holds",
                                shader.path
                            ),
                        );
                    }
                }
            }
            if !added {
                break Some(run);
            }
            graph_rounds += 1;
            if graph_rounds >= GRAPH_ROUNDS {
                bail!("the includes of the shader graphs did not settle in {GRAPH_ROUNDS} imports");
            }
        };
        let Some(round) = round else {
            continue;
        };
        if !options.cache.is_empty() {
            for shader in used.iter().filter(|shader| shader.graph) {
                keep(
                    &includes_path(&options.cache, &shader.identity),
                    &serde_json::to_vec(&known[&shader.path])?,
                )?;
            }
        }
        if round.report.lines().last() != Some("done") {
            bail!("the material inspection did not finish");
        }
        if !round
            .report
            .lines()
            .any(|line| line == format!("colorspace\t{}", setting.space))
        {
            bail!("the worker did not take the color space of the project");
        }
        // The class the engine ran on each variant and the assembly that
        // defines it.
        let mut inspectors = BTreeMap::new();
        for line in round.report.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            if let ["material", index, inspector, assembly, _] = fields[..] {
                inspectors.insert(
                    index.parse::<usize>()?,
                    (inspector.to_string(), assembly.to_string()),
                );
            }
        }
        for (index, path) in group.iter().enumerate() {
            let chain = chains
                .iter()
                .find(|chain| chain.members[0] == *path)
                .unwrap();
            let shader = &shaders[&chain.shader];
            let (key, ingredients) = &subjects[path];
            let result = (|| -> Result<InspectedMaterial, String> {
                if let Some(reason) = unsettled.get(&shader.path) {
                    return Err(reason.clone());
                }
                let Some((inspector, assembly)) = inspectors.get(&index) else {
                    return Err(format!("the worker found no material in {path}"));
                };
                let package = accepted(path, shader, inspector, assembly, &environment)?;
                let document = |phase: &str| -> Result<Vec<u8>, String> {
                    let file = sysutil::join(&[&round.written, phase, &format!("{index}.mat")]);
                    let text = read_file(&file)
                        .map_err(|error| format!("{path}: {error:#}"))
                        .and_then(|data| {
                            String::from_utf8(data).map_err(|_| format!("{path}: not text"))
                        })?;
                    Ok(restored(&text, &members[path].anchor, &authored_kinds).into_bytes())
                };
                Ok(InspectedMaterial {
                    inspector: inspector.clone(),
                    package,
                    shader: chain.shader.clone(),
                    over_authored: document("a")?,
                    over_inspected: document("b")?,
                    ingredients: ingredients.clone(),
                })
            })();
            if result.is_ok() && !inspection.shaders.contains_key(&chain.shader) {
                let Some(artifact) = round.artifacts.get(&shader.path) else {
                    bail!("the worker left no artifact of {}", shader.path);
                };
                if !options.cache.is_empty() {
                    keep(&shader_path(&options.cache, &shader.identity), artifact)?;
                }
                inspection
                    .shaders
                    .insert(chain.shader.clone(), artifact.clone());
            }
            if !options.cache.is_empty() {
                let text = |data: &[u8]| String::from_utf8_lossy(data).into_owned();
                let kept = Kept {
                    path: path.clone(),
                    guid: guids[path].clone(),
                    shader: shader.identity.clone(),
                    inspector: result
                        .as_ref()
                        .map_or_else(|_| String::new(), |found| found.inspector.clone()),
                    package: result
                        .as_ref()
                        .map_or_else(|_| String::new(), |found| found.package.clone()),
                    over_authored: result
                        .as_ref()
                        .map_or_else(|_| String::new(), |found| text(&found.over_authored)),
                    over_inspected: result
                        .as_ref()
                        .map_or_else(|_| String::new(), |found| text(&found.over_inspected)),
                    refused: result.as_ref().err().cloned().unwrap_or_default(),
                    ingredients: ingredients.clone(),
                };
                keep(&kept_path(&options.cache, key), &serde_json::to_vec(&kept)?)?;
            }
            results.insert(path.clone(), result);
        }
    }
    inspection.materials = outcomes
        .into_iter()
        .map(|(path, guid, outcome)| MaterialInspection {
            result: outcome.and_then(|_| results[&path].clone()),
            path,
            guid,
        })
        .collect();
    inspection.seconds = started.elapsed().as_secs_f64();
    Ok(inspection)
}

/// The package directory the inspector of a variant is compiled from, when
/// its results in a standalone worker were compared with the Editor for
/// that version of the package; why the variant is refused otherwise.
/// `inspector` is the class the engine chose for the shader and `assembly`
/// the loaded assembly that defines it, none when no loaded assembly does:
/// the engine then ran nothing.
fn accepted(
    path: &str,
    shader: &ShaderSource,
    inspector: &str,
    assembly: &str,
    environment: &ScriptEnvironment,
) -> Result<String, String> {
    let package_of = |root: &str| -> String {
        environment
            .roots
            .get(root)
            .map(|directory| sysutil::base(directory))
            .unwrap_or_default()
    };
    if inspector.is_empty() {
        return Err(format!(
            "the shader {} of {path} names no inspector for the project's render pipeline",
            shader.path
        ));
    }
    if assembly.is_empty() {
        return Err(format!(
            "{path} is inspected by {inspector}, a class that no assembly compiled from an immutable package of the project defines"
        ));
    }
    let package = environment
        .assembly_roots
        .get(assembly)
        .map(|root| package_of(root))
        .unwrap_or_default();
    if !VERIFIED_INSPECTORS.contains(&(inspector, package.as_str())) {
        return Err(format!(
            "{path} is inspected by {inspector} of {}, whose results in a standalone worker have not been compared with the Editor",
            if package.is_empty() {
                assembly
            } else {
                &package
            }
        ));
    }
    // A graph's inspector reads what the graph's importer left.
    let importer = package_of("Packages/com.unity.shadergraph");
    if shader.graph && importer != VERIFIED_GRAPH_PACKAGE {
        return Err(format!(
            "{path} uses the shader graph {}, imported by {importer}; the inspection of graph materials was compared with the Editor for {VERIFIED_GRAPH_PACKAGE}",
            shader.path
        ));
    }
    Ok(package)
}

/// The color space, the render pipeline asset and what identifies the
/// engine and the compiled scripts.
fn setting(sources: &Sources, project: &str) -> Result<Setting> {
    let mut ingredients = BTreeMap::new();
    let mut settings = |name: &str| -> Result<String> {
        let logical = format!("ProjectSettings/{name}");
        match read_file(&sysutil::join(&[project, &logical])) {
            Ok(data) => {
                ingredients.insert(logical, sha256(&data));
                Ok(String::from_utf8_lossy(&data).into_owned())
            }
            Err(error) if is_not_exist(&error) => Ok(String::new()),
            Err(error) => Err(error),
        }
    };
    // A standalone worker starts in gamma space whatever the project says.
    let space = match color_space(&settings("ProjectSettings.asset")?) {
        Some(true) => "linear",
        Some(false) => "gamma",
        None => bail!("the color space of the project is not known"),
    };
    let pipeline = match pipeline_guid(
        &settings("GraphicsSettings.asset")?,
        &settings("QualitySettings.asset")?,
    )? {
        Some(guid) => Some(
            sources
                .path_of(&guid)
                .filter(|path| path.starts_with("Assets/"))
                .map(String::from)
                .ok_or_else(|| {
                    anyhow!(
                        "the render pipeline asset {guid} of the project is not a source under Assets"
                    )
                })?,
        ),
        None => None,
    };
    if let Some(path) = &pipeline {
        file_ingredient(sources, project, path, &mut ingredients)?;
    }
    for path in PACKAGE_FILES {
        match read_file(&sysutil::join(&[project, path])) {
            Ok(data) => {
                ingredients.insert(path.to_string(), sha256(&data));
            }
            Err(error) if is_not_exist(&error) => {}
            Err(error) => return Err(error),
        }
    }
    let identity = identity_of(&[
        "material-inspection-v1",
        crate::ENGINE_VERSION,
        &sha256(HELPER_SOURCE.as_bytes()),
        &script_environment_identity(project),
        space,
        pipeline.as_deref().unwrap_or_default(),
        &map_text(&ingredients),
    ]);
    Ok(Setting {
        space,
        pipeline,
        ingredients,
        identity,
    })
}

/// The compiled code a run over the given shaders loads.
fn round_environment(
    sources: &Sources,
    project: &str,
    shaders: &[&ShaderSource],
) -> Result<ScriptEnvironment> {
    let mut selection = ScriptSelection {
        plugins: true,
        ..Default::default()
    };
    for shader in shaders {
        // A class is compiled from the script of its name.
        for class in &shader.classes {
            selection
                .files
                .insert(format!("{}.cs", class.rsplit('.').next().unwrap_or(class)));
        }
        for path in shader.parts.iter().chain(std::iter::once(&shader.path)) {
            let Some(physical) = sources.physical(path) else {
                continue;
            };
            if let Ok(meta) = read_file(&format!("{physical}.meta"))
                && let Some(importer) = scripted_importer(&meta)
            {
                selection.scripts.insert(importer);
            }
        }
    }
    inspection_environment(project, &selection)
        .map_err(|error| wrap("the inspectors of the material variants", error))
}

/// One worker run: imports the shaders and what they are made of, each
/// recorded in the context's ArtifactDB so that the next import and the
/// inspectors can load it, then has the tool's importer build and save the
/// variants.
#[allow(clippy::too_many_arguments)]
fn run(
    ctx: &Context,
    options: &InspectionOptions<'_>,
    setting: &Setting,
    environment: &ScriptEnvironment,
    output: &str,
    imported: &[String],
    variants: &[String],
    chains: &[&Chain],
    members: &BTreeMap<String, Member>,
    textures: &BTreeMap<String, String>,
) -> Result<Result<Round, (String, String)>> {
    let sources = options.sources;
    let used: Vec<String> = chains.iter().map(|chain| chain.shader.clone()).collect();
    let shaders: Vec<&String> = imported
        .iter()
        .filter(|path| matches!(lower_ext(path).as_str(), ".shader" | ".shadergraph"))
        .filter(|path| {
            sources
                .guid_of(path)
                .is_some_and(|guid| used.iter().any(|used| used == guid))
        })
        .collect();
    let mut extras = ContextExtras::default();
    let mut assets: Vec<String> = imported.to_vec();
    let mut registered = Vec::new();
    // The textures the chains of this run name.
    let mut stand_ins = BTreeSet::new();
    let mut listed = Vec::new();
    for chain in chains {
        for path in &chain.members {
            if listed.contains(path) {
                continue;
            }
            listed.push(path.clone());
            for reference in references(material_text(&members[path].text)) {
                // A texture a graph refers to is imported with the graph.
                if reference.file_id == TEXTURE_2D
                    && reference.kind == "3"
                    && textures.contains_key(&reference.guid)
                    && !sources
                        .path_of(&reference.guid)
                        .is_some_and(|path| imported.iter().any(|known| known == path))
                {
                    stand_ins.insert(reference.guid);
                }
            }
        }
    }
    for guid in &stand_ins {
        let stand_in = format!("{STAND_INS}/texture-{guid}.asset");
        extras
            .files
            .insert(stand_in.clone(), texture_stand_in(&textures[guid]));
        extras
            .files
            .insert(format!("{stand_in}.meta"), native_meta(guid, TEXTURE_2D));
        registered.push(stand_in);
    }
    let mut list = format!("colorspace {}\n", setting.space);
    if let Some(path) = &setting.pipeline {
        list.push_str(&format!("pipeline {path}\n"));
        registered.push(path.clone());
    }
    listed.sort();
    for path in &listed {
        let text = retyped(&members[path].text, &|file_id, guid| {
            (file_id == TEXTURE_2D && stand_ins.contains(guid)).then(|| "2".to_string())
        });
        extras.files.insert(path.clone(), text.into_bytes());
        registered.push(path.clone());
    }
    for path in variants {
        list.push_str(&format!("material {path}\n"));
    }
    let list_path = format!("{STAND_INS}/variants.umeinspect");
    extras.files.insert(list_path.clone(), list.into_bytes());
    extras.files.insert(
        format!("{list_path}.meta"),
        format!("fileFormatVersion: 2\nguid: {LIST_GUID}\n").into_bytes(),
    );
    extras.files.insert(
        format!("Library/ScriptAssemblies/{HELPER_ASSEMBLY}.dll"),
        helper(ctx, options)?,
    );
    extras.registered = registered.iter().cloned().collect();
    assets.extend(registered);
    assets.push(list_path);
    if assets.len() > 4000 {
        bail!(
            "an inspection of {} assets, more than one import context holds",
            assets.len()
        );
    }
    // A graph is written out from the templates of its targets, which its
    // importer reads as plain files.
    let files = BuildFiles {
        generated: BTreeMap::new(),
        files: match imported
            .iter()
            .any(|path| matches!(lower_ext(path).as_str(), ".shadergraph" | ".shadersubgraph"))
        {
            true => sources.graph_templates(),
            false => Vec::new(),
        },
    };
    let import_context = ctx.with_timeout(options.timeout);
    let (report, error) = run_build_import(
        &import_context,
        ProbeOptions {
            editor: options.editor.clone(),
            project: options.project.clone(),
            output: output.to_string(),
            timeout: options.timeout,
            ..Default::default()
        },
        &assets,
        &BuildImport {
            environment,
            files: &files,
            shader_cache: "",
            extras: Some(&extras),
        },
    );
    import_context.cancel();
    if let Some(error) = error {
        // The import the worker was at when it stopped: the one after
        // those it answered, or the last when that answer was an error.
        let at = report.as_ref().map(|report| {
            let answered = report.imports.len();
            match report.imports.last() {
                Some(last) if last.error_code != 0 => answered - 1,
                _ => answered,
            }
        });
        return match at.and_then(|at| imported.get(at)) {
            Some(asset) if super::support::context_error(ctx).is_none() => {
                Ok(Err((asset.clone(), format!("{error:#}"))))
            }
            _ => Err(wrap("inspect the material variants", error)),
        };
    }
    let content = report
        .and_then(|report| report.content)
        .filter(|content| content.source_unchanged)
        .ok_or_else(|| anyhow!("the inspection left no content"))?;
    let mut artifacts = BTreeMap::new();
    for path in shaders {
        let file = content
            .assets
            .iter()
            .find(|asset| asset.path == *path)
            .and_then(|asset| asset.files.iter().find(|file| file.name.is_empty()))
            .ok_or_else(|| anyhow!("the worker left no artifact of {path}"))?;
        let data = read_file(&file.path)?;
        if sha256(&data) != file.sha256 {
            bail!("native artifact of {path} changed");
        }
        artifacts.insert(path.clone(), data);
    }
    let written = sysutil::join(&[output, "project/Temp/UnityMicroEditorInspect"]);
    let report = String::from_utf8(read_file(&sysutil::join(&[&written, "report.txt"]))?)?;
    Ok(Ok(Round {
        report,
        written,
        artifacts,
    }))
}

/// `material-inspect`: writes the two documents of each variant next to a
/// `result.json`.
pub fn run_material_inspect_cli(args: &[String]) -> Result<()> {
    let mut flags = crate::flags::FlagSet::new("material-inspect");
    flags.string(
        "editor",
        &crate::util::default_unity_editor(),
        "supported native Unity executable",
    );
    flags.string("project", ".", "source Unity project, read only");
    flags.string(
        "output",
        "",
        "new directory beneath project Library/UnityMicroEditor or Temp",
    );
    flags.list(
        "material",
        |_, _| Ok(()),
        "material variant under Assets; repeat",
    );
    flags.string("list", "", "file naming one material variant a line");
    flags.string(
        "cache",
        "",
        "directory of kept results; Library/UnityMicroEditor/material-inspections of the project when empty, none for `-`",
    );
    flags.duration(
        "timeout",
        Duration::from_secs(120),
        "deadline of each worker run, at most 5m",
    );
    flags.parse(args.iter().cloned())?;
    let mut variants = flags.get_list("material");
    let list = flags.get_string("list");
    if !list.is_empty() {
        let text = String::from_utf8(read_file(&list)?)?;
        variants.extend(
            text.lines()
                .filter(|line| !line.is_empty())
                .map(String::from),
        );
    }
    let output = flags.get_string("output");
    let timeout = flags.get_duration("timeout");
    if !flags.args().is_empty() || output.is_empty() || variants.is_empty() || timeout <= 0 {
        bail!(
            "use material-inspect --project <source> --output <new directory> --material Assets/variant.mat [--material ...]"
        );
    }
    let project =
        super::support::eval_symlinks(&super::support::abs(&flags.get_string("project"))?)?;
    let cache = match flags.get_string("cache").as_str() {
        "" => inspection_cache(&project),
        "-" => String::new(),
        cache => super::support::abs(cache)?,
    };
    let sources = super::player_objects::open_sources(&project)?;
    let signals = super::import::signal_context()?;
    let inspection = inspect_material_variants(
        &signals,
        &InspectionOptions {
            editor: flags.get_string("editor"),
            project,
            sources: &sources,
            output: output.clone(),
            timeout: Duration::from_nanos(timeout as u64),
            cache,
        },
        &variants,
    )?;
    let directory = sysutil::join(&[&output, "inspected"]);
    mkdir_all(&directory, 0o700)?;
    let mut entries = Vec::new();
    for inspected in &inspection.materials {
        let mut entry = serde_json::json!({ "path": inspected.path, "guid": inspected.guid });
        match &inspected.result {
            Ok(material) => {
                for (phase, data) in [
                    ("a", &material.over_authored),
                    ("b", &material.over_inspected),
                ] {
                    let file =
                        sysutil::join(&[&directory, &format!("{}.{phase}.mat", inspected.guid)]);
                    write_file(&file, data, 0o600)?;
                }
                entry["inspector"] = material.inspector.clone().into();
                entry["package"] = material.package.clone().into();
                entry["shader"] = material.shader.clone().into();
                entry["sha256"] = sha256(&material.over_authored).into();
            }
            Err(reason) => entry["refused"] = reason.clone().into(),
        }
        entries.push(entry);
    }
    // The shaders as the worker imported them, for a caller that reads
    // what a material takes from its shader.
    for (guid, artifact) in &inspection.shaders {
        write_file(
            &sysutil::join(&[&directory, &format!("{guid}.shader.bin")]),
            artifact,
            0o600,
        )?;
    }
    let mut data = serde_json::to_vec_pretty(&serde_json::json!({
        "materials": entries,
        "kept": inspection.kept,
        "workerRuns": inspection.worker_runs,
        "elapsedSeconds": inspection.seconds,
    }))?;
    data.push(b'\n');
    write_file(&sysutil::join(&[&output, "result.json"]), &data, 0o600)?;
    use std::io::Write;
    std::io::stdout().lock().write_all(&data)?;
    Ok(())
}

/// Where the results of a project are kept from one call to the next.
pub fn inspection_cache(project: &str) -> String {
    sysutil::join(&[project, "Library/UnityMicroEditor/material-inspections"])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader(path: &str) -> ShaderSource {
        ShaderSource {
            path: path.to_string(),
            classes: BTreeSet::new(),
            graph: lower_ext(path) == ".shadergraph",
            parts: Vec::new(),
            text: Vec::new(),
            ingredients: BTreeMap::new(),
            identity: String::new(),
        }
    }

    fn compiled(universal: &str, graph: &str) -> ScriptEnvironment {
        let cached = |package: &str| format!("Library/PackageCache/{package}");
        ScriptEnvironment {
            roots: BTreeMap::from([
                (
                    "Packages/com.unity.render-pipelines.universal".to_string(),
                    cached(universal),
                ),
                ("Packages/com.unity.shadergraph".to_string(), cached(graph)),
            ]),
            assembly_roots: BTreeMap::from([(
                "Unity.RenderPipelines.Universal.Editor".to_string(),
                "Packages/com.unity.render-pipelines.universal".to_string(),
            )]),
            ..Default::default()
        }
    }

    /// An inspector is accepted for the package version it was compared
    /// in. Another version, an unknown class and a class nothing loaded
    /// defines refuse the one material, each with its name.
    #[test]
    fn accepts_an_inspector_only_for_the_version_it_was_compared_in() {
        const LIT: &str = "UnityEditor.Rendering.Universal.ShaderGUI.LitShader";
        const EDITOR: &str = "Unity.RenderPipelines.Universal.Editor";
        let verified = compiled(URP, VERIFIED_GRAPH_PACKAGE);
        let hand = shader("Assets/A.shader");
        let graph = shader("Assets/A.shadergraph");
        let ask = |shader: &ShaderSource, inspector: &str, assembly: &str, environment| {
            accepted("Assets/V.mat", shader, inspector, assembly, environment)
        };
        assert_eq!(ask(&hand, LIT, EDITOR, &verified).as_deref(), Ok(URP));
        assert_eq!(
            ask(&graph, "UnityEditor.ShaderGraphLitGUI", EDITOR, &verified).as_deref(),
            Ok(URP)
        );
        let other = compiled(
            "com.unity.render-pipelines.universal@aaaaaaaaaaaa",
            VERIFIED_GRAPH_PACKAGE,
        );
        let reason = ask(&hand, LIT, EDITOR, &other).unwrap_err();
        assert!(
            reason.starts_with("Assets/V.mat is inspected by ")
                && reason
                    .contains("LitShader of com.unity.render-pipelines.universal@aaaaaaaaaaaa")
                && reason.contains("have not been compared"),
            "{reason}"
        );
        let reason = ask(&hand, "UnityEditor.Other.Gui", EDITOR, &verified).unwrap_err();
        assert!(
            reason.contains("UnityEditor.Other.Gui of ") && reason.contains("not been compared"),
            "{reason}"
        );
        let reason = ask(&hand, "My.Gui", "", &verified).unwrap_err();
        assert!(
            reason.contains("My.Gui, a class that no assembly"),
            "{reason}"
        );
        let reason = ask(&hand, "", "", &verified).unwrap_err();
        assert!(reason.contains("names no inspector"), "{reason}");
        // A class of an assembly no package of the environment owns.
        let reason = ask(&hand, LIT, "Assembly-CSharp-Editor", &verified).unwrap_err();
        assert!(
            reason.contains("LitShader of Assembly-CSharp-Editor"),
            "{reason}"
        );
        // A graph is also bound to the version of its importer.
        let importer = compiled(URP, "com.unity.shadergraph@bbbbbbbbbbbb");
        assert_eq!(ask(&hand, LIT, EDITOR, &importer).as_deref(), Ok(URP));
        let reason = ask(&graph, "UnityEditor.ShaderGraphLitGUI", EDITOR, &importer).unwrap_err();
        assert!(
            reason.contains("imported by com.unity.shadergraph@bbbbbbbbbbbb"),
            "{reason}"
        );
    }

    #[test]
    fn identifies_a_result_by_everything_it_is_made_of() {
        let one = BTreeMap::from([("Assets/P.mat".to_string(), "1".to_string())]);
        let two = BTreeMap::from([("Assets/P.mat".to_string(), "2".to_string())]);
        assert_ne!(
            identity_of(&["material", &map_text(&one)]),
            identity_of(&["material", &map_text(&two)])
        );
        // The parts are kept apart.
        assert_ne!(identity_of(&["ab", "c"]), identity_of(&["a", "bc"]));
        assert_eq!(
            kept_path("/c", "k"),
            "/c/material-k.json",
            "a kept result lies under its key"
        );
    }

    const VARIANT: &str = "%YAML 1.1\n--- !u!21 &2100000\nMaterial:\n  m_Name: V\n  m_Shader: {fileID: 4800000, guid: 933532a4fcc9baf4fa0491de14d08ed7, type: 3}\n  m_Parent: {fileID: 2100000, guid: 94eacbebc3fbf262eac8661204a19aa6, type: 2}\n  m_SavedProperties:\n    m_TexEnvs:\n    - _BumpMap:\n        m_Texture: {fileID: 2800000, guid: 4459db655324dc581bdb480d2b420ea4, type: 3}\n";

    #[test]
    fn reads_what_a_variant_names() {
        let read = member("Assets/V.mat", VARIANT.as_bytes()).unwrap();
        assert_eq!(read.anchor, "2100000");
        assert_eq!(
            read.parent.as_deref(),
            Some("94eacbebc3fbf262eac8661204a19aa6")
        );
        let named = references(VARIANT);
        assert_eq!(named.len(), 3);
        assert_eq!(
            (named[2].file_id.as_str(), named[2].kind.as_str()),
            ("2800000", "3")
        );
        let plain = VARIANT.replace(
            "m_Parent: {fileID: 2100000, guid: 94eacbebc3fbf262eac8661204a19aa6, type: 2}",
            "m_Parent: {fileID: 0}",
        );
        assert_eq!(
            member("Assets/P.mat", plain.as_bytes()).unwrap().parent,
            None
        );
        assert!(member("Assets/P.mat", b"--- !u!21 &1\n--- !u!21 &2\n").is_err());
    }

    /// The context names a stand-in as a native asset; the document the
    /// worker saves gets the authored types and anchor back.
    #[test]
    fn retypes_stand_ins_and_restores_them() {
        let context = retyped(VARIANT, &|_, guid| {
            (guid != "94eacbebc3fbf262eac8661204a19aa6").then(|| "2".to_string())
        });
        assert!(!context.contains("type: 3"));
        let saved = context.replace("--- !u!21 &2100000", "--- !u!21 &1");
        let authored = references(VARIANT)
            .into_iter()
            .map(|reference| ((reference.file_id, reference.guid), reference.kind))
            .collect();
        assert_eq!(restored(&saved, "2100000", &authored), VARIANT);
    }

    #[test]
    fn finds_the_inspectors_and_the_pipeline() {
        let shader = "Shader \"X\" {\n  CustomEditorForRenderPipeline \"A.B\" \"P\"\n    CustomEditor \"UnityEditor.Rendering.Universal.ShaderGUI.LitShader\"\n}";
        assert_eq!(
            inspector_classes(shader).into_iter().collect::<Vec<_>>(),
            ["A.B", "UnityEditor.Rendering.Universal.ShaderGUI.LitShader"]
        );
        let graphics = "GraphicsSettings:\n  m_CustomRenderPipeline: {fileID: 11400000, guid: 21ccace7345df5470a0d2efe4c3b2d51, type: 2}\n";
        let none = "  - name: Low\n    customRenderPipeline: {fileID: 0}\n";
        assert_eq!(
            pipeline_guid(graphics, none).unwrap().as_deref(),
            Some("21ccace7345df5470a0d2efe4c3b2d51")
        );
        let other = "    customRenderPipeline: {fileID: 11400000, guid: 31ccace7345df5470a0d2efe4c3b2d51, type: 2}\n";
        assert!(pipeline_guid(graphics, other).is_err());
        assert_eq!(
            pipeline_guid("  m_CustomRenderPipeline: {fileID: 0}\n", none).unwrap(),
            None
        );
    }
}
