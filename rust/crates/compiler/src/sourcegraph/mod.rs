//! The authored source assembly graph. The graph export, the engine profile and
//! their digests are persisted formats.
//!
//! Where a result would depend on map iteration order (which of several
//! simultaneous refusals is reported, and the order of per-platform settings
//! in `ProjectSettings.asset`), key order is used.
mod cli;
mod compile;
mod options;
mod plugin_yaml;
mod profile;
#[cfg(test)]
mod tests;

pub use cli::run_source_graph_cli;
pub use compile::{
    SourceGraphDifference, compare_source_graphs, plugin_reference_paths,
    prepare_source_compiler_graph, prepare_source_compiler_graph_with,
};
pub use options::{
    SourcePlugin, SourceVersion, apply_response_files, compare_versions, lossy_runes,
    match_constraints, match_version, parse_version, plugin_references, read_editor_plugin,
    read_source_plugin, split_arguments,
};
pub use profile::{
    SourceBuildProof, SourceBuildProofAssembly, SourceEngineProfile, SourceGraphOptions,
    SourceNativeAssembly, capture_engine_profile, load_engine_profile, logical_path,
    managed_directory, physical_path, profile_hash, read_build_proof, validate_source_output,
};

use crate::{
    cancel::Context,
    compiler::{
        AssemblyConfig, CompileScript, CompilerGraphAssembly, CompilerGraphExport,
        CompilerHostAssembly, CompilerSourceRoot, MetadataGuidIndex, NilMap, NilVec, Walk,
        editor_source, equal_strings, is_not_exist, managed_plugin, unity_version, walk_dir,
    },
    jsondoc::{self, Json, Target},
    packages::Package,
    project::{self, Value, lexpath::rune_lower},
    sha256, sysutil,
    util::{file_hash, read_agent_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;

/// A JSON `null` decodes into any field as a no-op, leaving the default value.
fn nz<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A sorted copy without repeats; an absent list stays absent.
pub fn sorted_strings(values: &NilVec<String>) -> NilVec<String> {
    if values.is_nil() {
        return NilVec::nil();
    }
    let mut items = values.to_vec();
    items.sort();
    items.dedup();
    items.into()
}
/// A copy keeping the items the predicate accepts; an absent list stays absent.
fn retain(values: &NilVec<String>, keep: impl Fn(&String) -> bool) -> NilVec<String> {
    if values.is_nil() {
        return NilVec::nil();
    }
    values
        .iter()
        .filter(|value| keep(value))
        .cloned()
        .collect::<Vec<_>>()
        .into()
}
/// `^[A-Za-z_][A-Za-z0-9_]*$`.
fn symbol(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic() || *first == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// One entry of an asmdef's `versionDefines`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionDefine {
    pub name: String,
    pub expression: String,
    pub define: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceAsmdef {
    pub name: String,
    pub root_namespace: String,
    pub references: NilVec<String>,
    pub include_platforms: NilVec<String>,
    pub exclude_platforms: NilVec<String>,
    pub allow_unsafe: bool,
    pub override_references: bool,
    pub precompiled_references: NilVec<String>,
    pub auto_referenced: Option<bool>,
    pub define_constraints: NilVec<String>,
    pub version_defines: NilVec<VersionDefine>,
    pub no_engine_references: bool,
    pub optional_unity_references: NilVec<String>,
    pub path: String,
    pub root: String,
    pub guid: String,
    pub package: String,
}

const ASMDEF: &str = "sourceAsmdef";

/// The defines the pinned Editor compiles its own assemblies with beyond
/// those of a development Player, which it leaves out (`DEVELOPMENT_BUILD`).
pub const EDITOR_DEFINES: [&str; 9] = [
    "ENABLE_BURST_AOT",
    "ENABLE_UNITY_COLLECTIONS_CHECKS",
    "UNITY_EDITOR",
    "UNITY_EDITOR_64",
    "UNITY_EDITOR_LINUX",
    "UNITY_ENABLE_CHECKS",
    "UNITY_INCLUDE_INSTRUMENTATION",
    "UNITY_INCLUDE_TESTS",
    "UNITY_TEAM_LICENSE",
];

fn set_strings(target: &mut NilVec<String>, value: &Json, field: &str) -> Result<(), String> {
    let at = Target {
        strukt: ASMDEF,
        field,
    };
    match value {
        Json::Null => *target = NilVec::nil(),
        Json::Array(items) => {
            let mut first = None;
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                let mut text = String::new();
                if let Err(error) = jsondoc::set_string(&mut text, item, at) {
                    first.get_or_insert(error);
                }
                result.push(text);
            }
            *target = result.into();
            return first.map_or(Ok(()), Err);
        }
        other => return Err(at.mismatch(other.kind(), "array")),
    }
    Ok(())
}
fn set_version_defines(target: &mut NilVec<VersionDefine>, value: &Json) -> Result<(), String> {
    let at = Target {
        strukt: ASMDEF,
        field: "versionDefines",
    };
    match value {
        Json::Null => *target = NilVec::nil(),
        Json::Array(items) => {
            let mut first = None;
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                let mut define = VersionDefine::default();
                let decoded = match item {
                    Json::Null => Ok(()),
                    Json::Object(members) => {
                        const FIELDS: [&str; 3] = ["name", "expression", "define"];
                        jsondoc::decode_struct(members, &FIELDS, |field, member| {
                            let slot = match field {
                                0 => &mut define.name,
                                1 => &mut define.expression,
                                _ => &mut define.define,
                            };
                            let path = format!("versionDefines.{}", FIELDS[field]);
                            jsondoc::set_string(
                                slot,
                                &member.value,
                                // The innermost struct is anonymous.
                                Target {
                                    strukt: "",
                                    field: &path,
                                },
                            )
                        })
                    }
                    other => Err(at.mismatch(other.kind(), "object")),
                };
                if let Err(error) = decoded {
                    first.get_or_insert(error);
                }
                result.push(define);
            }
            *target = result.into();
            return first.map_or(Ok(()), Err);
        }
        other => return Err(at.mismatch(other.kind(), "array")),
    }
    Ok(())
}

/// Decodes an asmdef with case-insensitive field matching and the decoder's error texts.
pub fn decode_asmdef(content: &[u8]) -> Result<SourceAsmdef, String> {
    let mut definition = SourceAsmdef::default();
    let members = match jsondoc::parse(content)? {
        Json::Null => return Ok(definition),
        Json::Object(members) => members,
        other => return Err(Target::VALUE.mismatch(other.kind(), "sourceAsmdef")),
    };
    const FIELDS: [&str; 13] = [
        "name",
        "rootNamespace",
        "references",
        "includePlatforms",
        "excludePlatforms",
        "allowUnsafeCode",
        "overrideReferences",
        "precompiledReferences",
        "autoReferenced",
        "defineConstraints",
        "versionDefines",
        "noEngineReferences",
        "optionalUnityReferences",
    ];
    jsondoc::decode_struct(&members, &FIELDS, |field, member| {
        let name = FIELDS[field];
        let at = Target {
            strukt: ASMDEF,
            field: name,
        };
        let value = &member.value;
        match field {
            0 => jsondoc::set_string(&mut definition.name, value, at),
            1 => jsondoc::set_string(&mut definition.root_namespace, value, at),
            2 => set_strings(&mut definition.references, value, name),
            3 => set_strings(&mut definition.include_platforms, value, name),
            4 => set_strings(&mut definition.exclude_platforms, value, name),
            5 => jsondoc::set_bool(&mut definition.allow_unsafe, value, at),
            6 => jsondoc::set_bool(&mut definition.override_references, value, at),
            7 => set_strings(&mut definition.precompiled_references, value, name),
            8 => match value {
                Json::Null => {
                    definition.auto_referenced = None;
                    Ok(())
                }
                Json::Bool(flag) => {
                    definition.auto_referenced = Some(*flag);
                    Ok(())
                }
                other => Err(at.mismatch(other.kind(), "bool")),
            },
            9 => set_strings(&mut definition.define_constraints, value, name),
            10 => set_version_defines(&mut definition.version_defines, value),
            11 => jsondoc::set_bool(&mut definition.no_engine_references, value, at),
            _ => set_strings(&mut definition.optional_unity_references, value, name),
        }
    })?;
    Ok(definition)
}

/// The Linux Standalone part of `ProjectSettings.asset`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceSettings {
    pub api_compatibility: i64,
    pub defines: NilVec<String>,
    pub input_handler: i64,
    pub allow_unsafe: bool,
    pub arguments: Vec<String>,
}

/// A number as f64; `int` converts it with NaN and out-of-range values becoming i64::MIN.
fn number(value: &Value) -> f64 {
    match value {
        Value::Int(value) => *value as f64,
        Value::Float(value) => *value,
        Value::Number(text) => crate::project::strconv::parse_float(text).unwrap_or(0.0),
        _ => 0.0,
    }
}
fn int(value: f64) -> i64 {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if value.is_nan() || value >= LIMIT || value < -LIMIT {
        i64::MIN
    } else {
        value as i64
    }
}
fn linux_platform(key: &str) -> bool {
    matches!(key, "Standalone" | "LinuxStandalone64" | "1")
}

/// Where a project records the build target group whose scripting define
/// symbols its Player is compiled with.
pub const SCRIPTING_DEFINES_FILE: &str = "Library/UnityMicroEditor/scripting-defines.json";

/// The platform symbols the Editor defines for a build target group, which
/// a game written for that platform alone may need to compile. Only the
/// symbols a Linux Player can stand for; the engine stays a Linux one.
pub fn group_platform_defines(group: &str) -> &'static [&'static str] {
    match group {
        "Android" => &["PLATFORM_ANDROID", "UNITY_ANDROID", "UNITY_ANDROID_API"],
        "iPhone" => &["PLATFORM_IOS", "UNITY_IOS", "UNITY_IPHONE"],
        "WebGL" => &["PLATFORM_WEBGL", "UNITY_WEBGL"],
        _ => &[],
    }
}

/// The build target group (as `ProjectSettings.asset` names it, `Android`,
/// `iPhone`, ...) whose scripting define symbols the project's Player is
/// compiled with instead of the Standalone ones; `None` for Standalone.
///
/// A mobile game is often edited with its device as the active target, and
/// its code compiles only with that group's symbols, its platform symbols
/// (`group_platform_defines`) included, as in the Editor's Play mode. The
/// Player stays a Linux one: code that calls the device's native plugins
/// under those symbols fails at run time unless it also checks for the Editor.
pub fn scripting_defines_group(project: &str) -> Result<Option<String>> {
    let path = sysutil::join(&[project, SCRIPTING_DEFINES_FILE]);
    let content = match read_agent_file(&path, 64 << 10) {
        Ok(content) => content,
        Err(error) if is_not_exist(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Selection {
        schema_version: i64,
        group: String,
    }
    let selection: Selection =
        serde_json::from_slice(&content).map_err(|error| anyhow!("{path}: {error}"))?;
    if selection.schema_version != 1 || !symbol(&selection.group) {
        bail!("{path} names no valid build target group");
    }
    Ok(Some(selection.group))
}

pub fn read_source_settings(project: &str) -> Result<SourceSettings> {
    let mut settings = SourceSettings::default();
    let path = sysutil::join(&[project, "ProjectSettings/ProjectSettings.asset"]);
    let content = match read_agent_file(&path, 32 << 20) {
        Ok(content) => content,
        Err(error) if is_not_exist(&error) => return Ok(settings),
        Err(error) => return Err(error),
    };
    let documents = project::parse_unity_yaml(&content)?;
    if documents.len() != 1 || documents[0].class_id != 129 {
        bail!("project PlayerSettings requires exact class129 YAML");
    }
    let data = &documents[0];
    let platforms = |key: &str| {
        project::value::object(data.get(key))
            .iter()
            .filter(|(platform, _)| linux_platform(platform))
    };
    settings.api_compatibility = int(number(data.get("apiCompatibilityLevel")));
    for (_, value) in platforms("apiCompatibilityLevelPerPlatform") {
        settings.api_compatibility = int(number(value));
    }
    settings.input_handler = int(number(data.get("activeInputHandler")));
    settings.allow_unsafe = number(data.get("allowUnsafeCode")) != 0.0;
    let group = scripting_defines_group(project)?;
    let defines = project::value::object(data.get("scriptingDefineSymbols"));
    let selected = defines.iter().filter(|(platform, _)| match &group {
        Some(group) => platform.as_str() == group,
        None => linux_platform(platform),
    });
    if let Some(group) = &group
        && !defines.iter().any(|(platform, _)| platform == group)
    {
        bail!("the project's PlayerSettings have no scripting define symbols for {group}");
    }
    for (_, value) in selected {
        if let Value::String(text) = value {
            settings.defines.append(
                text.split([';', ','])
                    .filter(|define| !define.is_empty())
                    .map(String::from),
            );
        }
    }
    if let Some(group) = &group {
        settings.defines.append(
            group_platform_defines(group)
                .iter()
                .map(|define| define.to_string()),
        );
    }
    for (_, value) in platforms("additionalCompilerArguments") {
        for argument in project::value::array(value) {
            settings.arguments.push(match argument {
                Value::String(text) => text.clone(),
                _ => String::new(),
            });
        }
    }
    settings.defines = sorted_strings(&settings.defines);
    for define in &settings.defines {
        if !symbol(define) || define.starts_with("UNITY_EDITOR") {
            bail!("invalid authored scripting define: {define}");
        }
    }
    Ok(settings)
}

pub fn read_metadata_guid(path: &str) -> Result<String> {
    let identity = MetadataGuidIndex::new().read(path)?;
    if identity.guid.is_empty() {
        bail!("managed source metadata has no GUID: {path}");
    }
    Ok(identity.guid)
}

fn on(map: &BTreeMap<String, bool>, key: &str) -> bool {
    map.get(key).copied().unwrap_or(false)
}
fn source_count(configs: &BTreeMap<String, AssemblyConfig>, name: &str) -> usize {
    configs.get(name).map_or(0, |config| config.sources.len())
}
fn has(values: &[String], wanted: &str) -> bool {
    values.iter().any(|value| value == wanted)
}

/// Every assembly decision comes from authored sources,
/// resolved packages and the cached engine profile.
pub fn build_source_graph(
    ctx: &Context,
    options: &SourceGraphOptions,
) -> Result<CompilerGraphExport> {
    const MISMATCH: &str = "Source graph requires explicit matching engine/package profiles";
    // An empty project path means the working directory.
    let project = if options.project.is_empty() {
        project::lexpath::getwd()?
    } else {
        project::lexpath::abs(&sysutil::eval_symlinks(&options.project)?)?
    };
    let (Some(profile), Some(packages)) = (&options.profile, &options.packages) else {
        bail!(MISMATCH);
    };
    if packages.project != project || packages.editor != options.editor {
        bail!(MISMATCH);
    }
    packages.validate(ctx)?;
    if !profile_hash(profile).is_ok_and(|hash| hash == profile.profile_sha256)
        || profile.editor != options.editor
        || profile.player != options.player
        || profile.unity_version != unity_version(&project)?
        || profile.unity_revision != crate::release::of_project(&project)?.revision
        || profile.target != "LinuxStandalone64"
        || profile.scripting_backend != "Mono"
    {
        bail!("Source graph engine profile mismatch");
    }
    for (path, expected) in &profile.inputs {
        ctx.err()?;
        if !file_hash(path).is_ok_and(|actual| actual == *expected) {
            bail!("Source graph engine/native input changed: {path}");
        }
    }
    let settings = read_source_settings(&project)?;
    if settings.api_compatibility != profile.api_compatibility {
        bail!("Player API compatibility differs from cached engine/BCL profile");
    }
    let mut base = profile.base_defines.clone();
    base.append(settings.defines.iter().cloned());
    if settings.input_handler == 1 || settings.input_handler == 2 {
        base.push("ENABLE_INPUT_SYSTEM".into());
    }
    if settings.input_handler == 0 || settings.input_handler == 2 {
        base.push("ENABLE_LEGACY_INPUT_MANAGER".into());
    }
    if options.editor_domain {
        // What the Editor defines beyond a development Player of the pinned
        // engine (compared with its own `Library/Bee` record of a project).
        base = retain(&base, |define| define != "DEVELOPMENT_BUILD");
        base.append(EDITOR_DEFINES.iter().map(|define| define.to_string()));
    }
    let base = sorted_strings(&base);
    let automatic_engine_assemblies = automatic_engine_assemblies(profile);
    let base_compiler_options = base_compiler_options(profile);
    let engine_assembly_names: Vec<String> = profile
        .engine_references
        .iter()
        .map(|path| {
            let name = sysutil::base(path);
            name.strip_suffix(".dll").unwrap_or(&name).to_string()
        })
        .collect();
    let mut graph = CompilerGraphExport {
        schema_version: 1,
        project: project.clone(),
        unity: options.editor.clone(),
        backend: "authored_source".into(),
        engine_profile_sha256: profile.profile_sha256.clone(),
        package_resolution_sha256: packages.resolution_hash.clone(),
        compiler_inputs: NilMap::from(packages.inputs.clone()),
        development: true,
        base_references: profile.base_references.clone(),
        engine_references: profile.engine_references.clone(),
        ..Default::default()
    };
    let settings_path = sysutil::join(&[&project, "ProjectSettings/ProjectSettings.asset"]);
    match file_hash(&settings_path) {
        Ok(hash) => graph.compiler_inputs.insert(&settings_path, hash),
        Err(error) if is_not_exist(&error) => graph
            .compiler_inputs
            .insert(&settings_path, "absent".to_string()),
        Err(error) => return Err(error),
    }
    for (path, value) in &profile.inputs {
        graph.compiler_inputs.insert(path, value.clone());
    }
    // (logical root, physical root, package name)
    let mut trees = vec![(
        "Assets".to_string(),
        sysutil::join(&[&project, "Assets"]),
        String::new(),
    )];
    let mut versions =
        BTreeMap::from([("Unity".to_string(), unity_version(&project)?.to_string())]);
    let mut package_map: BTreeMap<&str, &Package> = BTreeMap::new();
    for package in packages.packages.iter() {
        versions.insert(package.name.clone(), package.version.clone());
        package_map.insert(&package.name, package);
        if package.path.is_empty() || package.logical_root != format!("Packages/{}", package.name) {
            bail!("resolved package has invalid source root: {}", package.name);
        }
        trees.push((
            package.logical_root.clone(),
            package.path.clone(),
            package.name.clone(),
        ));
        graph.source_roots.push(CompilerSourceRoot {
            path: package.logical_root.clone(),
            source: package.path.clone(),
        });
    }
    // A module package enables an engine module the host's own compile may
    // not have referenced; its assembly ships with every Player.
    for path in package_module_references(
        &options.editor,
        packages
            .packages
            .iter()
            .map(|package| package.name.as_str()),
    )? {
        if !graph.engine_references.contains(&path) {
            graph.compiler_inputs.insert(&path, file_hash(&path)?);
            graph.engine_references.push(path);
        }
    }
    graph.engine_references = sorted_strings(&graph.engine_references);
    let mut definitions: BTreeMap<String, SourceAsmdef> = BTreeMap::new();
    let mut by_guid: BTreeMap<String, String> = BTreeMap::new();
    let mut roots: BTreeMap<String, String> = BTreeMap::new();
    let mut asmrefs: BTreeMap<String, String> = BTreeMap::new();
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    let mut plugins: BTreeMap<String, SourcePlugin> = BTreeMap::new();
    // Every managed DLL of a package by file name, whatever platform it is
    // for: the post-processor assemblies name the Editor's.
    let mut all_plugins: BTreeMap<String, String> = BTreeMap::new();
    let mut codegen_sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut metadata_index = MetadataGuidIndex::new();
    let mut metadata_owners: BTreeMap<String, String> = BTreeMap::new();
    for (_, physical, package) in &trees {
        walk_dir(physical, &mut |entry| {
            let path = entry.path;
            ctx.err()?;
            if entry.kind.is_symlink() {
                bail!("source graph rejects symlink: {path}");
            }
            if entry.is_dir() {
                let name = sysutil::base(path);
                if path != physical && (name.starts_with('.') || name.ends_with('~')) {
                    return Ok(Walk::SkipDir);
                }
                return Ok(Walk::Continue);
            }
            let ext = rune_lower(sysutil::ext(path));
            if !matches!(
                ext.as_str(),
                ".cs" | ".asmdef" | ".asmref" | ".dll" | ".rsp"
            ) {
                return Ok(Walk::Continue);
            }
            if sources.len() + definitions.len() + asmrefs.len() > 100000 {
                bail!("Source graph exceeds supported source bound");
            }
            let mut content = Vec::new();
            if ext == ".dll" {
                graph.compiler_inputs.insert(path, file_hash(path)?);
            } else {
                content = read_agent_file(path, 32 << 20)?;
                graph.compiler_inputs.insert(path, sha256(&content));
            }
            let meta = format!("{path}.meta");
            match file_hash(&meta) {
                Ok(hash) => {
                    graph.compiler_inputs.insert(&meta, hash);
                    let identity = metadata_index.read(&meta)?;
                    if !identity.guid.is_empty() {
                        if let Some(previous) = metadata_owners
                            .get(&identity.guid)
                            .filter(|previous| !previous.is_empty() && *previous != path)
                        {
                            bail!(
                                "managed source metadata GUID collision: {} between {previous} and {path}",
                                identity.guid
                            );
                        }
                        metadata_owners.insert(identity.guid, path.to_string());
                    }
                }
                Err(error) if is_not_exist(&error) => {}
                Err(error) => return Err(error),
            }
            match ext.as_str() {
                ".cs" => {
                    sources.insert(path.to_string(), package.clone());
                }
                ".asmdef" => {
                    let mut definition = decode_asmdef(&content)
                        .map_err(|error| anyhow!("asmdef {path}: {error}"))?;
                    if definition.name.is_empty()
                        || sysutil::base(&definition.name) != definition.name
                        || definition.name.contains(['\0', '\r', '\n', ';', ':'])
                    {
                        bail!("invalid asmdef name: {path}");
                    }
                    if definitions.contains_key(&definition.name) {
                        bail!("duplicate asmdef assembly name: {}", definition.name);
                    }
                    let guid = read_metadata_guid(&meta)?;
                    if by_guid.get(&guid).is_some_and(|name| !name.is_empty()) {
                        bail!("duplicate asmdef GUID: {guid}");
                    }
                    definition.path = path.to_string();
                    definition.root = sysutil::dir(path);
                    definition.guid = guid.clone();
                    definition.package = package.clone();
                    if roots.contains_key(&definition.root) {
                        bail!(
                            "multiple assembly roots in one directory: {}",
                            definition.root
                        );
                    }
                    by_guid.insert(guid, definition.name.clone());
                    roots.insert(definition.root.clone(), definition.name.clone());
                    definitions.insert(definition.name.clone(), definition);
                }
                ".asmref" => {
                    let reference = match jsondoc::parse(&content) {
                        Ok(Json::Object(members)) => {
                            let mut reference = String::new();
                            jsondoc::decode_struct(&members, &["reference"], |_, member| {
                                jsondoc::set_string(&mut reference, &member.value, Target::VALUE)
                            })
                            .ok()
                            .map(|()| reference)
                        }
                        _ => None,
                    };
                    let Some(reference) = reference.filter(|reference| !reference.is_empty())
                    else {
                        bail!("invalid asmref: {path}");
                    };
                    let directory = sysutil::dir(path);
                    if asmrefs.contains_key(&directory) {
                        bail!("multiple asmrefs in one directory: {directory}");
                    }
                    asmrefs.insert(directory, reference);
                }
                ".dll" => {
                    if managed_plugin(path) {
                        all_plugins
                            .entry(sysutil::base(path))
                            .or_insert_with(|| path.to_string());
                    }
                    let plugin = match options.editor_domain {
                        true => read_editor_plugin(path, &base)?,
                        false => read_source_plugin(path, &base)?,
                    };
                    if plugin.enabled && managed_plugin(path) {
                        let name = sysutil::base(path);
                        if plugins.contains_key(&name) {
                            bail!("duplicate active managed plugin filename: {name}");
                        }
                        plugins.insert(name, plugin);
                    }
                }
                _ => {}
            }
            Ok(Walk::Continue)
        })?;
    }
    let native_name = |name: &str| {
        profile
            .native_assemblies
            .get(name)
            .is_some_and(|fact| !fact.name.is_empty())
    };
    let resolve =
        |definitions: &BTreeMap<String, SourceAsmdef>, reference: &str| -> Result<String, String> {
            let reference = match reference.strip_prefix("GUID:") {
                Some(guid) => by_guid.get(&rune_lower(guid)).cloned().unwrap_or_default(),
                None => reference.to_string(),
            };
            if reference.is_empty() {
                return Err("assembly reference GUID is unresolved".into());
            }
            if !definitions.contains_key(&reference) && !native_name(&reference) {
                return Err(format!("assembly reference is unresolved: {reference}"));
            }
            Ok(reference)
        };
    for (root, reference) in &asmrefs {
        if roots.contains_key(root) {
            bail!("asmdef/asmref share directory: {root}");
        }
        let name = resolve(&definitions, reference).map_err(|error| anyhow!(error))?;
        roots.insert(root.clone(), name);
    }
    let mut configs: BTreeMap<String, AssemblyConfig> = BTreeMap::new();
    let mut active: BTreeMap<String, bool> = BTreeMap::new();
    let mut owner_package: BTreeMap<String, String> = BTreeMap::new();
    // Whether each active asmdef is Editor-only and whether it is a test
    // assembly, for the references the Editor adds on its own.
    let mut implicit_kinds: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for (name, definition) in &definitions {
        let mut defines = base.clone();
        for version in &definition.version_defines {
            if !symbol(&version.define) {
                bail!("invalid asmdef version define: {}", definition.path);
            }
            if let Some(current) = versions.get(&version.name).filter(|v| !v.is_empty())
                && match_version(current, &version.expression)
                    .map_err(|error| anyhow!("{name} version define: {error}"))?
            {
                defines.push(version.define.clone());
            }
        }
        let defines = sorted_strings(&defines);
        let constraints = match_constraints(&definition.define_constraints, &defines)
            .map_err(|error| anyhow!("{name} constraints: {error}"))?;
        let included = match options.editor_domain {
            true => {
                (definition.include_platforms.is_empty()
                    || has(&definition.include_platforms, "Editor"))
                    && !has(&definition.exclude_platforms, "Editor")
            }
            false => {
                (definition.include_platforms.is_empty()
                    || has(&definition.include_platforms, "LinuxStandalone64")
                    || has(&definition.include_platforms, "Standalone"))
                    && !has(&definition.exclude_platforms, "LinuxStandalone64")
                    && !has(&definition.exclude_platforms, "Standalone")
            }
        };
        // The Editor compiles the test framework and what refers to it, but
        // not the tests of packages, which only build with UNITY_INCLUDE_TESTS
        // when the project lists the package as testable.
        let tests = match options.editor_domain {
            true => {
                !definition.package.is_empty()
                    && has(&definition.define_constraints, "UNITY_INCLUDE_TESTS")
            }
            false => test_assembly(definition),
        };
        let enabled = included && constraints && !tests;
        active.insert(name.clone(), enabled);
        let mut config = AssemblyConfig {
            name: name.clone(),
            defines,
            compiler_options: base_compiler_options.clone(),
            ..Default::default()
        };
        config.config.allow_unsafe = definition.allow_unsafe;
        config.config.no_engine_references = definition.no_engine_references;
        config.config.override_references = definition.override_references;
        config.config.precompiled = definition.precompiled_references.clone();
        for reference in
            &expanded_references(&definition.references, &definitions, options.editor_domain)
        {
            if !enabled {
                break;
            }
            let dependency = match resolve(&definitions, reference) {
                Ok(dependency) => dependency,
                Err(error) => {
                    let package = package_map.get(definition.package.as_str());
                    // The Editor leaves out a reference to an assembly the
                    // project does not have; an assembly of a package nobody
                    // edits (registry, built-in, Git) is compiled the same
                    // way, and code that needs the missing one fails to
                    // compile. Built-in packages of 6000.6 still name
                    // `Unity.Mathematics`, which is now an engine module.
                    // The tool's runtime package is the tool's, like those, and
                    // guards the code that needs an optional package with
                    // version defines.
                    if package.is_some_and(|package| {
                        matches!(package.source.as_str(), "registry" | "builtin" | "git")
                            || package.name == crate::packages::RUNTIME_PACKAGE
                    }) {
                        continue;
                    }
                    if profile.native_assemblies.get(name).is_some_and(|fact| {
                        !fact.package.is_empty()
                            && package.is_some_and(|package| {
                                fact.package == package.name
                                    && fact.package_content_hash == package.content_hash
                            })
                            && has(&fact.unresolved_references, reference)
                    }) {
                        continue;
                    }
                    bail!("{name}: {error}");
                }
            };
            if has(&engine_assembly_names, &dependency) && !definition.no_engine_references {
                continue;
            }
            if !definitions.contains_key(&dependency)
                && plugins
                    .get(&format!("{dependency}.dll"))
                    .is_some_and(|plugin| !plugin.path.is_empty())
            {
                continue;
            }
            config.dependencies.push(dependency);
        }
        if enabled {
            implicit_kinds.insert(
                name.clone(),
                (
                    definition.include_platforms.len() == 1
                        && has(&definition.include_platforms, "Editor"),
                    test_assembly(definition)
                        || has(&definition.define_constraints, "UNITY_INCLUDE_TESTS"),
                ),
            );
        }
        configs.insert(name.clone(), config);
        owner_package.insert(name.clone(), definition.package.clone());
    }
    if options.editor_domain {
        add_implicit_editor_references(&mut configs, &implicit_kinds);
    }
    for path in sources.keys() {
        let mut name = String::new();
        let mut folder = sysutil::dir(path);
        loop {
            if let Some(root) = roots.get(&folder).filter(|root| !root.is_empty()) {
                name = root.clone();
                break;
            }
            let parent = sysutil::dir(&folder);
            if parent == folder {
                break;
            }
            folder = parent;
        }
        if name.is_empty() {
            let logical = logical_path(&project, packages, path);
            let editor = editor_source(path);
            if editor && !options.editor_domain {
                continue;
            }
            name = "Assembly-CSharp".into();
            if [
                "Assets/Plugins/",
                "Assets/Standard Assets/",
                "Assets/Pro Standard Assets/",
            ]
            .iter()
            .any(|prefix| logical.starts_with(prefix))
            {
                name = "Assembly-CSharp-firstpass".into();
            }
            if editor {
                name = match name.as_str() {
                    "Assembly-CSharp-firstpass" => "Assembly-CSharp-Editor-firstpass".into(),
                    _ => "Assembly-CSharp-Editor".into(),
                };
            }
            if configs
                .get(&name)
                .is_none_or(|config| config.name.is_empty())
            {
                let mut config = AssemblyConfig {
                    name: name.clone(),
                    defines: base.clone(),
                    compiler_options: base_compiler_options.clone(),
                    ..Default::default()
                };
                config.config.allow_unsafe = settings.allow_unsafe;
                configs.insert(name.clone(), config);
                active.insert(name.clone(), true);
            }
        }
        if name.ends_with(".CodeGen")
            && definitions.get(&name).is_some_and(|definition| {
                definition.define_constraints.is_empty()
                    && package_map
                        .get(definition.package.as_str())
                        .is_some_and(|package| {
                            matches!(package.source.as_str(), "registry" | "builtin" | "git")
                        })
            })
        {
            codegen_sources
                .entry(name.clone())
                .or_default()
                .push(path.clone());
        }
        if !on(&active, &name) {
            continue;
        }
        if let Some(config) = configs.get_mut(&name) {
            config.sources.push(path.clone());
        }
    }
    for (name, mut sources) in codegen_sources {
        let definition = &definitions[&name];
        sources.sort();
        let mut defines: Vec<String> = configs
            .get(&name)
            .map(|config| config.defines.to_vec())
            .unwrap_or_else(|| base.to_vec());
        for define in ["UNITY_EDITOR", "UNITY_EDITOR_64", "UNITY_EDITOR_LINUX"] {
            if !defines.iter().any(|known| known == define) {
                defines.push(define.into());
            }
        }
        let mut assembly = crate::compiler::CodegenAssembly {
            name: name.clone(),
            sources,
            allow_unsafe: definition.allow_unsafe,
            defines,
            compiler_options: base_compiler_options.to_vec(),
            ..Default::default()
        };
        for reference in
            &expanded_references(&definition.references, &definitions, options.editor_domain)
        {
            if let Ok(reference) = resolve(&definitions, reference) {
                assembly.references.push(reference);
            }
        }
        for precompiled in &definition.precompiled_references {
            if let Some(path) = all_plugins.get(precompiled) {
                assembly.precompiled.push(path.clone());
            }
        }
        graph.codegen_assemblies.push(assembly);
    }
    let names: Vec<String> = configs.keys().cloned().collect();
    for name in &names {
        let mut config = configs[name].clone();
        if !on(&active, name) || config.sources.is_empty() {
            continue;
        }
        if !config.config.no_engine_references {
            for dependency in &automatic_engine_assemblies {
                if dependency != name
                    && on(&active, dependency)
                    && source_count(&configs, dependency) > 0
                {
                    config.dependencies.push(dependency.clone());
                }
            }
        }
        if name.starts_with("Assembly-CSharp") {
            for (dependency, definition) in &definitions {
                if on(&active, dependency)
                    && source_count(&configs, dependency) > 0
                    && definition.auto_referenced.unwrap_or(true)
                {
                    config.dependencies.push(dependency.clone());
                }
            }
            // Each predefined assembly sees the ones the Editor compiles
            // before it.
            let earlier: &[&str] = match name.as_str() {
                "Assembly-CSharp" => &["Assembly-CSharp-firstpass"],
                "Assembly-CSharp-Editor-firstpass" => &["Assembly-CSharp-firstpass"],
                "Assembly-CSharp-Editor" => &[
                    "Assembly-CSharp-firstpass",
                    "Assembly-CSharp",
                    "Assembly-CSharp-Editor-firstpass",
                ],
                _ => &[],
            };
            for dependency in earlier {
                if source_count(&configs, dependency) > 0 {
                    config.dependencies.push(dependency.to_string());
                }
            }
        }
        for dependency in &config.dependencies {
            if definitions.contains_key(dependency)
                && (!on(&active, dependency) || source_count(&configs, dependency) == 0)
            {
                bail!("active assembly {name} references excluded/empty assembly {dependency}");
            }
        }
        config.dependencies = sorted_strings(&config.dependencies);
        config.sources = sorted_strings(&config.sources);
        let sources = config.sources.clone();
        apply_response_files(
            &project,
            &sources,
            &settings.arguments,
            &mut config,
            &plugins,
            &mut graph.compiler_inputs,
        )?;
        config.compiler_references = match options.editor_domain {
            true => editor_plugin_references(&config, &plugins, &options.editor),
            false => plugin_references(&config, &plugins)?,
        };
        configs.insert(name.clone(), config);
    }
    for (name, config) in &configs {
        if !on(&active, name) || config.sources.is_empty() {
            continue;
        }
        let package = owner_package
            .get(name)
            .and_then(|owner| package_map.get(owner.as_str()));
        let fact = profile.native_assemblies.get(name);
        let fact_defines = |fact: &SourceNativeAssembly| {
            equal_strings(
                &sorted_strings(&fact.configuration.defines),
                &config.defines,
            )
        };
        let source_proof = |fact: &SourceNativeAssembly, source: &String, required: bool| {
            let logical = logical_path(&project, packages, source);
            let expected = fact.source_hashes.at(&logical);
            (!required || !expected.is_empty())
                && expected == graph.compiler_inputs.at(source)
                && fact.source_hashes.at(&format!("{logical}.meta"))
                    == graph.compiler_inputs.at(&format!("{source}.meta"))
        };
        let mut native = !options.editor_domain
            && match (package, fact) {
                (Some(package), Some(fact)) => {
                    !package.name.is_empty()
                        && !fact.name.is_empty()
                        && fact.package == package.name
                        && fact.package_content_hash == package.content_hash
                        && fact_defines(fact)
                }
                _ => false,
            };
        if native && let Some(fact) = fact {
            native = config
                .sources
                .iter()
                .all(|source| source_proof(fact, source, true));
        }
        if name == "UnityMicroEditor" && !options.editor_domain {
            // The preparation compiles the bridge from the installation's
            // sources with the SDK and puts it, with its initializers, in
            // place of the host's (`compiler::prepare`). The host only has
            // to hold an assembly of that name, which its
            // `ScriptingAssemblies.json` registers.
            native = fact.is_some_and(|fact| fact.name == *name);
            if !native {
                bail!(
                    "the host Player holds no runtime bridge (UnityMicroEditor); build a host with the tool's runtime package"
                );
            }
        }
        for source in &config.sources {
            graph.scripts.push(CompileScript {
                guid: read_metadata_guid(&format!("{source}.meta"))?,
                path: source.clone(),
                assembly: name.clone(),
                host_provided: native,
                ..Default::default()
            });
        }
        if native {
            let mut editor_defines = config.defines.clone();
            editor_defines.push("UNITY_EDITOR".into());
            editor_defines.push("UNITY_EDITOR_LINUX".into());
            graph.host_assemblies.push(CompilerHostAssembly {
                name: name.clone(),
                path: sysutil::join(&[&managed_directory(&options.player), &format!("{name}.dll")]),
                sha256: fact.map(|fact| fact.sha256.clone()).unwrap_or_default(),
                sources: config.sources.clone(),
                defines: config.defines.clone(),
                editor_defines,
                configuration: Some(Box::new(config.clone())),
                compiled_references: config.compiler_references.clone(),
                response_files: config.response_files.clone(),
            });
        } else {
            graph.assemblies.push(CompilerGraphAssembly {
                response_files: config.response_files.clone(),
                config: config.clone(),
                path: String::new(),
                sha256: String::new(),
                references: config.dependencies.clone(),
                compiled_references: config.compiler_references.clone(),
                package_build: !options.editor_domain
                    && package.is_some_and(|package| {
                        matches!(package.source.as_str(), "registry" | "builtin" | "git")
                    }),
                editor_only: options.editor_domain
                    && (editor_source(name)
                        || definitions.get(name).is_some_and(|definition| {
                            definition.include_platforms.len() == 1
                                && has(&definition.include_platforms, "Editor")
                        })),
            });
        }
    }
    if graph.assemblies.len() + graph.host_assemblies.len() > 256 && !options.editor_domain {
        bail!("Source graph exceeds256 runtime assemblies");
    }
    packages.validate(ctx)?;
    Ok(graph)
}

/// These native script assemblies
/// participate in Unity's default engine reference set. Infer the pinned table
/// from complete public build configs, rather than turning arbitrary
/// autoReferenced user asmdefs into engine refs.
/// The engine module assemblies of the module packages (`com.unity.modules.*`)
/// among `packages`, as the platform's module setup names them: a module
/// controlled by a built-in package is that package's name in lower case,
/// and its assembly is `UnityEngine.<Module>Module.dll`.
pub fn package_module_references<'a>(
    editor: &str,
    packages: impl Iterator<Item = &'a str>,
) -> Result<Vec<String>> {
    let support = sysutil::join(&[
        &sysutil::dir(editor),
        "Data/PlaybackEngines/LinuxStandaloneSupport",
    ]);
    let setup = sysutil::join(&[&support, "modules.asset"]);
    let text = match std::fs::read_to_string(&setup) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(anyhow!("read {setup}: {error}")),
    };
    let mut modules = BTreeMap::new();
    let mut current: Option<&str> = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("  - name: ") {
            current = Some(name.trim());
        } else if line.trim() == "controlledByBuiltinPackage: 1"
            && let Some(name) = current
        {
            modules.insert(name.to_ascii_lowercase(), name.to_string());
        }
    }
    let mut result = Vec::new();
    for package in packages {
        let Some(module) = package
            .strip_prefix("com.unity.modules.")
            .and_then(|key| modules.get(key))
        else {
            continue;
        };
        let path = sysutil::join(&[
            &support,
            &format!("Variations/mono/Managed/UnityEngine.{module}Module.dll"),
        ]);
        if std::path::Path::new(&path).is_file() {
            result.push(path);
        }
    }
    Ok(result)
}

pub fn automatic_engine_assemblies(profile: &SourceEngineProfile) -> Vec<String> {
    let mut result = Vec::new();
    for (name, candidate) in &profile.native_assemblies {
        if !name.starts_with("UnityEngine.")
            || candidate.package.is_empty()
            || candidate.source_hashes.is_empty()
        {
            continue;
        }
        let mut others = profile.native_assemblies.iter().filter(|(other, fact)| {
            *other != name
                && !fact.configuration.config.no_engine_references
                && !fact.source_hashes.is_empty()
        });
        let mut count = 0;
        let shared = others.all(|(_, fact)| {
            count += 1;
            has(&fact.configuration.dependencies, name)
        });
        if shared && count > 0 {
            result.push(name.clone());
        }
    }
    result
}

/// The `nowarn` defaults every source-built
/// native assembly was compiled with.
pub fn base_compiler_options(profile: &SourceEngineProfile) -> NilVec<String> {
    let mut result = NilVec::nil();
    let mut first = true;
    for fact in profile.native_assemblies.values() {
        if fact.source_hashes.is_empty() {
            continue;
        }
        if first {
            result = fact.configuration.compiler_options.clone();
            first = false;
            continue;
        }
        result = retain(&result, |option| {
            fact.configuration.compiler_options.contains(option)
        });
    }
    // Only defaults whose behavior can be preserved exactly are harvested.
    // Arbitrary authored compiler options are parsed separately from source.
    retain(&result, |option| {
        option.starts_with("/nowarn:") || option.starts_with("-nowarn:")
    })
}

pub fn test_assembly(definition: &SourceAsmdef) -> bool {
    if definition.name == "UnityEngine.TestRunner" || definition.name == "UnityEditor.TestRunner" {
        return true;
    }
    [
        &definition.optional_unity_references,
        &definition.references,
        &definition.precompiled_references,
        &definition.define_constraints,
    ]
    .iter()
    .any(|entries| {
        entries.iter().any(|value| {
            matches!(
                value.as_str(),
                "TestAssemblies"
                    | "UnityEngine.TestRunner"
                    | "UnityEditor.TestRunner"
                    | "nunit.framework.dll"
                    | "UNITY_INCLUDE_TESTS"
            )
        })
    })
}

/// The precompiled assemblies an Editor-domain assembly compiles against:
/// as for a Player, except that an assembly named explicitly may be one the
/// Editor ships (IL2CPP's, Cecil's), and one found nowhere is left out, as
/// the Editor leaves it out.
fn editor_plugin_references(
    config: &AssemblyConfig,
    plugins: &BTreeMap<String, SourcePlugin>,
    editor: &str,
) -> NilVec<String> {
    let mut result = config.compiler_references.clone();
    if config.config.override_references {
        let data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
        for name in &config.config.precompiled {
            if let Some(plugin) = plugins.get(name) {
                result.push(plugin.path.clone());
                continue;
            }
            let file = match name.ends_with(".dll") {
                true => name.clone(),
                false => format!("{name}.dll"),
            };
            for directory in [
                "il2cpp/build/deploy",
                "Tools/BuildPipeline/BuiltinILPPs",
                "Managed",
            ] {
                let path = sysutil::join(&[&data, directory, &file]);
                if std::path::Path::new(&path).is_file() {
                    result.push(path);
                    break;
                }
            }
        }
        // One that refers to the test runner gets NUnit besides those it
        // names (`com.unity.ide.visualstudio`'s, which names only
        // Newtonsoft.Json; the Editor's compilation record, 6000.6.3f1).
        if config
            .dependencies
            .iter()
            .any(|name| name == "UnityEngine.TestRunner" || name == "UnityEditor.TestRunner")
            && let Some(plugin) = plugins.get("nunit.framework.dll")
        {
            result.push(plugin.path.clone());
        }
    } else {
        for plugin in plugins.values() {
            if !plugin.explicit {
                result.push(plugin.path.clone());
            }
        }
    }
    sorted_strings(&result)
}

/// An asmdef's references with the names the Editor resolves to other
/// assemblies: `Unity.ugui`, which no assembly is named, stands for uGUI's
/// `UnityEngine.UI` and, in the Editor, `UnityEditor.UI` (the Editor's own
/// compilation record of a project whose Input System names it, 6000.6.3f1).
fn expanded_references<T>(
    references: &[String],
    definitions: &BTreeMap<String, T>,
    editor_domain: bool,
) -> Vec<String> {
    let mut expanded = Vec::with_capacity(references.len());
    for reference in references {
        if reference == "Unity.ugui" && !definitions.contains_key(reference) {
            for name in ["UnityEngine.UI", "UnityEditor.UI"] {
                if (editor_domain || name == "UnityEngine.UI")
                    && !expanded.iter().any(|known| known == name)
                {
                    expanded.push(name.to_string());
                }
            }
            continue;
        }
        expanded.push(reference.clone());
    }
    expanded
}

/// The references the Editor adds to asmdefs on its own (6000.6.3f1, from
/// its compilation record of a project): uGUI's `UnityEngine.UI` and
/// `UnityEditor.UI` to every assembly that refers to the engine, and the test
/// runner's `UnityEngine.TestRunner` and `UnityEditor.TestRunner` to every
/// Editor-only or test assembly. None is added where it would make a cycle
/// or where the target is not part of the compilation.
fn add_implicit_editor_references(
    configs: &mut BTreeMap<String, AssemblyConfig>,
    kinds: &BTreeMap<String, (bool, bool)>,
) {
    fn reaches(configs: &BTreeMap<String, AssemblyConfig>, from: &str, to: &str) -> bool {
        let mut pending = vec![from.to_string()];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(name) = pending.pop() {
            if name == to {
                return true;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(config) = configs.get(&name) {
                pending.extend(config.dependencies.iter().cloned());
            }
        }
        false
    }
    const UI: [&str; 2] = ["UnityEngine.UI", "UnityEditor.UI"];
    const TESTS: [&str; 2] = ["UnityEngine.TestRunner", "UnityEditor.TestRunner"];
    let names: Vec<String> = kinds.keys().cloned().collect();
    for name in names {
        let (editor_only, test) = kinds[&name];
        let Some(config) = configs.get(&name) else {
            continue;
        };
        let mut wanted: Vec<&str> = Vec::new();
        if !config.config.no_engine_references {
            wanted.extend(UI);
        }
        if editor_only || test {
            wanted.extend(TESTS);
        }
        for target in wanted {
            if target == name
                || !kinds.contains_key(target)
                || configs[&name]
                    .dependencies
                    .iter()
                    .any(|known| known == target)
                || reaches(configs, target, &name)
            {
                continue;
            }
            if let Some(config) = configs.get_mut(&name) {
                config.dependencies.push(target.to_string());
            }
        }
    }
}
