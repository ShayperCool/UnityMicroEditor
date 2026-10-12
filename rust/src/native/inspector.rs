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
//! A standalone worker has no artifact database, so it finds an asset only
//! where a source file holds the object itself. The context therefore holds
//! the materials of each chain, the shader as the worker's own shader
//! importer wrote it (imported in a first run and stored as a native asset
//! under the shader's GUID), and a one-pixel texture under the GUID of each
//! texture a chain names; the references are retyped to match. An inspector
//! that asks the asset database about an asset gets no answer there and
//! would silently decide otherwise than in the Editor, so only inspectors
//! compared with the Editor are accepted, and no Shader Graph shader: its
//! inspectors read the graph's metadata through the asset database.
use super::{
    ProbeOptions,
    context::ContextExtras,
    import::{ImportExtras, run_project_import, run_project_import_with},
    resolve_project_import_source,
    scripted::{ScriptSelection, selected_environment},
    support::{lower_ext, mkdir_all, read_file, wrap, write_file},
};
use crate::{cancel::Context, compiler, sha256, sysutil};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
    time::Duration,
};

/// The inspectors whose results in a standalone worker were compared with
/// Editor builds, object for object (`docs/NATIVE_IMPORT.md`).
pub const VERIFIED_INSPECTORS: [&str; 6] = [
    "UnityEditor.Rendering.Universal.ShaderGUI.BakedLitShader",
    "UnityEditor.Rendering.Universal.ShaderGUI.LitShader",
    "UnityEditor.Rendering.Universal.ShaderGUI.ParticlesLitShader",
    "UnityEditor.Rendering.Universal.ShaderGUI.ParticlesUnlitShader",
    "UnityEditor.Rendering.Universal.ShaderGUI.SimpleLitShader",
    "UnityEditor.Rendering.Universal.ShaderGUI.UnlitShader",
];

/// The package those inspectors are compiled from, as the package cache
/// names the version they were compared in (17.6.0). Another version may
/// ask the asset database where this one does not.
pub const VERIFIED_PACKAGE: &str = "com.unity.render-pipelines.universal@0ffb3d4f1461";

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
    /// The variant built over parents as their sources give them.
    pub over_authored: Vec<u8>,
    /// The variant built after the variants among its parents were built.
    pub over_inspected: Vec<u8>,
}

/// What became of one requested variant: its documents, or why it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialInspection {
    pub path: String,
    pub guid: String,
    pub result: Result<InspectedMaterial, String>,
}

pub struct InspectionOptions<'a> {
    /// The Unity executable.
    pub editor: String,
    pub project: String,
    /// A new directory under the project's `Library/UnityMicroEditor` or
    /// `Temp`.
    pub output: String,
    /// For each of the two worker runs.
    pub timeout: Duration,
    /// The project path of the asset with a GUID.
    pub locate: &'a dyn Fn(&str) -> Option<String>,
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
}

/// Runs the inspectors of the given variants (project paths under `Assets`)
/// and returns what the engine leaves of each, or why a variant has no
/// result. An error is a failure of the run as a whole.
pub fn inspect_material_variants(
    ctx: &Context,
    options: &InspectionOptions<'_>,
    variants: &[String],
) -> Result<Vec<MaterialInspection>> {
    let project = &options.project;
    let read = |path: &str| -> Result<Vec<u8>, String> {
        if !path.starts_with("Assets/") {
            return Err(format!("{path} is not under Assets"));
        }
        resolve_project_import_source(project, path)
            .and_then(|source| read_file(&source))
            .map_err(|error| format!("{path}: {error:#}"))
    };
    let guid_of = |path: &str| -> Result<String, String> {
        resolve_project_import_source(project, &format!("{path}.meta"))
            .and_then(|meta| read_file(&meta))
            .and_then(|meta| crate::registry::meta_guid(&meta))
            .map_err(|error| format!("{path}.meta: {error:#}"))
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
                    members.insert(path.clone(), member(&path, &read(&path)?)?);
                    guids.insert(path.clone(), guid_of(&path)?);
                }
                chain.push(path.clone());
                match &members[&path].parent {
                    None => break,
                    Some(guid) => match (options.locate)(guid) {
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
            })
        })();
        let guid = guids.get(variant).cloned().unwrap_or_default();
        outcomes.push((variant.clone(), guid, walked));
    }
    // What the chains name: shaders to import, textures to stand in for.
    let mut shaders: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut textures: BTreeMap<String, String> = BTreeMap::new();
    let mut authored: BTreeMap<(String, String), String> = BTreeMap::new();
    for (_, _, outcome) in &mut outcomes {
        let Ok(chain) = outcome else { continue };
        let mut named = Vec::new();
        let checked = (|| -> Result<(), String> {
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
                    let Some(target) = (options.locate)(&reference.guid) else {
                        continue;
                    };
                    if Some(&reference.guid) == shader.as_ref() && reference.kind == "3" {
                        match lower_ext(&target).as_str() {
                            ".shader" => {}
                            ".shadergraph" => {
                                return Err(format!(
                                    "{path} uses the Shader Graph shader {target}, whose inspector reads the graph's metadata through the asset database, which a standalone worker does not have"
                                ));
                            }
                            _ => {
                                return Err(format!(
                                    "{path} uses the shader {target}, which is not a shader source"
                                ));
                            }
                        }
                        named.push((reference, Some(target)));
                    } else if reference.file_id == TEXTURE_2D && reference.kind == "3" {
                        named.push((reference, None));
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
            let shader = SHADER
                .captures(material_text(&members[top].text))
                .and_then(|found| (options.locate)(&found[2]));
            if shader.is_none() {
                return Err(format!(
                    "the shader of {top} is not a source of the project"
                ));
            }
            Ok(())
        })();
        if let Err(reason) = checked {
            *outcome = Err(reason);
            continue;
        }
        for (reference, shader) in named {
            match shader {
                Some(target) => {
                    shaders.insert(reference.guid.clone(), (target, reference.file_id.clone()));
                }
                None => {
                    let target = (options.locate)(&reference.guid).unwrap_or_default();
                    let base = sysutil::base(&target);
                    let name = base
                        .rsplit_once('.')
                        .map_or(base.as_str(), |(name, _)| name);
                    textures.insert(reference.guid.clone(), name.to_string());
                }
            }
            authored.insert((reference.file_id, reference.guid), reference.kind);
        }
    }
    let finish = |outcomes: Vec<(String, String, Result<Chain, String>)>,
                  results: &dyn Fn(&str) -> Result<InspectedMaterial, String>| {
        outcomes
            .into_iter()
            .map(|(path, guid, outcome)| MaterialInspection {
                result: outcome.and_then(|_| results(&path)),
                path,
                guid,
            })
            .collect::<Vec<_>>()
    };
    if !outcomes.iter().any(|(_, _, outcome)| outcome.is_ok()) {
        return Ok(finish(outcomes, &|_| Err(String::new())));
    }

    // The shaders as the worker's own importer writes them.
    let output = &options.output;
    let shader_paths: Vec<String> = shaders.values().map(|(path, _)| path.clone()).collect();
    let mut classes = BTreeSet::new();
    for path in &shader_paths {
        let source =
            resolve_project_import_source(project, path).and_then(|source| read_file(&source))?;
        classes.extend(inspector_classes(&String::from_utf8_lossy(&source)));
    }
    if classes.is_empty() {
        // Such a variant is baked from its source alone.
        return Ok(finish(outcomes, &|path| {
            Err(format!("the shader of {path} names no inspector"))
        }));
    }
    let (report, error) = run_project_import(
        ctx,
        ProbeOptions {
            editor: options.editor.clone(),
            project: project.clone(),
            output: sysutil::join(&[output, "shaders"]),
            timeout: options.timeout,
            ..Default::default()
        },
        &shader_paths,
    );
    if let Some(error) = error {
        return Err(wrap("import the shaders of the material variants", error));
    }
    let imported = report
        .and_then(|report| report.content)
        .ok_or_else(|| anyhow!("the shader import left no content"))?;
    let scripts = ScriptSelection {
        // A class is compiled from the script of its name.
        files: classes
            .iter()
            .map(|class| format!("{}.cs", class.rsplit('.').next().unwrap_or(class)))
            .collect(),
        plugins: true,
        ..Default::default()
    };
    let compiled = selected_environment(project, &scripts)
        .map_err(|error| wrap("the inspectors of the material variants", error))?;
    if !compiled
        .roots
        .values()
        .any(|directory| sysutil::base(directory) == VERIFIED_PACKAGE)
    {
        bail!(
            "the inspectors of the project are not those of {VERIFIED_PACKAGE}, the version whose results in a standalone worker were compared with the Editor"
        );
    }
    let mut extras = ImportExtras {
        scripts,
        context: ContextExtras::default(),
    };
    let mut assets = Vec::new();
    for (guid, (path, file_id)) in &shaders {
        let artifact = imported
            .assets
            .iter()
            .find(|asset| asset.path == *path)
            .and_then(|asset| asset.files.first())
            .ok_or_else(|| anyhow!("the shader import left no artifact of {path}"))?;
        let stand_in = format!("{STAND_INS}/shader-{guid}.asset");
        let files = &mut extras.context.files;
        files.insert(stand_in.clone(), read_file(&artifact.path)?);
        files.insert(format!("{stand_in}.meta"), native_meta(guid, file_id));
        assets.push(stand_in);
    }
    for (guid, name) in &textures {
        let stand_in = format!("{STAND_INS}/texture-{guid}.asset");
        let files = &mut extras.context.files;
        files.insert(stand_in.clone(), texture_stand_in(name));
        files.insert(format!("{stand_in}.meta"), native_meta(guid, TEXTURE_2D));
        assets.push(stand_in);
    }
    let stand_ins: BTreeSet<&String> = shaders.keys().chain(textures.keys()).collect();
    let settings = |name: &str| -> String {
        resolve_project_import_source(project, &format!("ProjectSettings/{name}"))
            .and_then(|source| read_file(&source))
            .map(|data| String::from_utf8_lossy(&data).into_owned())
            .unwrap_or_default()
    };
    // A standalone worker starts in gamma space whatever the project says.
    let space = match color_space(&settings("ProjectSettings.asset")) {
        Some(true) => "linear",
        Some(false) => "gamma",
        None => bail!("the color space of the project is not known"),
    };
    let mut list = format!("colorspace {space}\n");
    if let Some(guid) = pipeline_guid(
        &settings("GraphicsSettings.asset"),
        &settings("QualitySettings.asset"),
    )? {
        let path = (options.locate)(&guid)
            .filter(|path| path.starts_with("Assets/"))
            .ok_or_else(|| {
                anyhow!(
                    "the render pipeline asset {guid} of the project is not a source under Assets"
                )
            })?;
        list.push_str(&format!("pipeline {path}\n"));
        assets.push(path);
    }
    let mut listed = Vec::new();
    for (path, member) in &members {
        if !outcomes.iter().any(|(_, _, outcome)| {
            outcome
                .as_ref()
                .is_ok_and(|chain| chain.members.contains(path))
        }) {
            continue;
        }
        let text = retyped(&member.text, &|_, guid| {
            stand_ins
                .contains(&guid.to_string())
                .then(|| "2".to_string())
        });
        extras.context.files.insert(path.clone(), text.into_bytes());
        assets.push(path.clone());
    }
    for (path, _, outcome) in &outcomes {
        if outcome.is_ok() && !listed.contains(path) {
            list.push_str(&format!("material {path}\n"));
            listed.push(path.clone());
        }
    }
    extras.context.registered = assets.iter().cloned().collect();
    let list_path = format!("{STAND_INS}/variants.umeinspect");
    let files = &mut extras.context.files;
    files.insert(list_path.clone(), list.into_bytes());
    files.insert(
        format!("{list_path}.meta"),
        format!("fileFormatVersion: 2\nguid: {LIST_GUID}\n").into_bytes(),
    );
    files.insert(
        format!("Library/ScriptAssemblies/{HELPER_ASSEMBLY}.dll"),
        compile_helper(ctx, &options.editor, &sysutil::join(&[output, "helper"]))?,
    );
    assets.push(list_path);
    let context = sysutil::join(&[output, "inspect"]);
    let (_, error) = run_project_import_with(
        ctx,
        ProbeOptions {
            editor: options.editor.clone(),
            project: project.clone(),
            output: context.clone(),
            timeout: options.timeout,
            ..Default::default()
        },
        &assets,
        &extras,
    );
    if let Some(error) = error {
        return Err(wrap("inspect the material variants", error));
    }
    let written = sysutil::join(&[&context, "project/Temp/UnityMicroEditorInspect"]);
    let report = String::from_utf8(read_file(&sysutil::join(&[&written, "report.txt"]))?)?;
    if report.lines().last() != Some("done") {
        bail!("the material inspection did not finish");
    }
    if !report
        .lines()
        .any(|line| line == format!("colorspace\t{space}"))
    {
        bail!("the worker did not take the color space of the project");
    }
    let mut inspectors = BTreeMap::new();
    for line in report.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if let ["material", index, inspector, _] = fields[..] {
            inspectors.insert(index.parse::<usize>()?, inspector.to_string());
        }
    }
    let result = |path: &str| -> Result<InspectedMaterial, String> {
        let index = listed.iter().position(|listed| listed == path).unwrap();
        let Some(inspector) = inspectors.get(&index) else {
            return Err(format!("the worker found no material in {path}"));
        };
        if inspector.is_empty() {
            return Err(format!(
                "the shader of {path} names no inspector for the project's render pipeline"
            ));
        }
        if !VERIFIED_INSPECTORS.contains(&inspector.as_str()) {
            return Err(format!(
                "{path} is inspected by {inspector}, whose results in a standalone worker have not been compared with the Editor"
            ));
        }
        let document = |phase: &str| -> Result<Vec<u8>, String> {
            let file = sysutil::join(&[&written, phase, &format!("{index}.mat")]);
            let text = read_file(&file)
                .map_err(|error| format!("{path}: {error:#}"))
                .and_then(|data| {
                    String::from_utf8(data).map_err(|_| format!("{path}: not text"))
                })?;
            Ok(restored(&text, &members[path].anchor, &authored).into_bytes())
        };
        Ok(InspectedMaterial {
            inspector: inspector.clone(),
            over_authored: document("a")?,
            over_inspected: document("b")?,
        })
    };
    Ok(finish(outcomes, &result))
}

/// The project path of every asset under `Assets` and of every shader
/// source of an installed package, by GUID.
pub fn project_asset_paths(project: &str) -> Result<BTreeMap<String, String>> {
    fn walk(
        directory: &std::path::Path,
        logical: &str,
        shaders_only: bool,
        paths: &mut BTreeMap<String, String>,
    ) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = entry.path();
            if path.is_dir() {
                walk(&path, &format!("{logical}/{name}"), shaders_only, paths);
                continue;
            }
            let Some(asset) = name.strip_suffix(".meta") else {
                continue;
            };
            if shaders_only && !matches!(lower_ext(asset).as_str(), ".shader" | ".shadergraph") {
                continue;
            }
            if let Ok(guid) = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|meta| crate::registry::meta_guid(&meta))
            {
                paths.insert(guid, format!("{logical}/{asset}"));
            }
        }
    }
    let mut paths = BTreeMap::new();
    let root = std::path::Path::new(project);
    if let Ok(packages) = std::fs::read_dir(root.join("Library/PackageCache")) {
        for package in packages.flatten() {
            let Ok(name) = package.file_name().into_string() else {
                continue;
            };
            let name = name.split('@').next().unwrap_or(&name).to_string();
            walk(
                &package.path(),
                &format!("Packages/{name}"),
                true,
                &mut paths,
            );
        }
    }
    walk(&root.join("Assets"), "Assets", false, &mut paths);
    Ok(paths)
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
    let paths = project_asset_paths(&project)?;
    let signals = super::import::signal_context()?;
    let inspections = inspect_material_variants(
        &signals,
        &InspectionOptions {
            editor: flags.get_string("editor"),
            project,
            output: output.clone(),
            timeout: Duration::from_nanos(timeout as u64),
            locate: &|guid| paths.get(guid).cloned(),
        },
        &variants,
    )?;
    let directory = sysutil::join(&[&output, "inspected"]);
    mkdir_all(&directory, 0o700)?;
    let mut entries = Vec::new();
    for inspection in &inspections {
        let mut entry = serde_json::json!({ "path": inspection.path, "guid": inspection.guid });
        match &inspection.result {
            Ok(material) => {
                for (phase, data) in [
                    ("a", &material.over_authored),
                    ("b", &material.over_inspected),
                ] {
                    let file =
                        sysutil::join(&[&directory, &format!("{}.{phase}.mat", inspection.guid)]);
                    write_file(&file, data, 0o600)?;
                }
                entry["inspector"] = material.inspector.clone().into();
                entry["sha256"] = sha256(&material.over_authored).into();
            }
            Err(reason) => entry["refused"] = reason.clone().into(),
        }
        entries.push(entry);
    }
    let mut data = serde_json::to_vec_pretty(&serde_json::json!({ "materials": entries }))?;
    data.push(b'\n');
    write_file(&sysutil::join(&[&output, "result.json"]), &data, 0o600)?;
    use std::io::Write;
    std::io::stdout().lock().write_all(&data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
