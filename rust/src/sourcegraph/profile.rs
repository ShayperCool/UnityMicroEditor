//! The pinned engine profile. The public reference
//! is used once to record pinned engine compilation facts. Target source
//! graphs are always derived from authored files and packages.
use super::{SourceAsmdef, decode_asmdef, nz, read_metadata_guid, read_source_settings, retain};
use crate::{
    cancel::Context,
    compiler::{
        AssemblyConfig, CompilerGraphExport, NilMap, NilVec, UNITY_VERSION, Walk,
        canonical_configuration, glob_lossy, is_not_exist, walk_dir,
    },
    packages::Snapshot,
    project::lexpath::rune_lower,
    sha256, sysutil,
    util::{
        agent_canonical, agent_immutable_json, agent_read_json, file_hash, marshal,
        read_agent_file, read_json_file, valid_sha256, within_directory,
    },
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const UNITY_REVISION: &str = "45d8eee7de74";

fn is_empty(value: &str) -> bool {
    value.is_empty()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceEngineProfile {
    #[serde(rename = "apiCompatibility", deserialize_with = "nz")]
    pub api_compatibility: i64,
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub backend: String,
    #[serde(rename = "unityVersion", deserialize_with = "nz")]
    pub unity_version: String,
    #[serde(rename = "unityRevision", deserialize_with = "nz")]
    pub unity_revision: String,
    #[serde(deserialize_with = "nz")]
    pub editor: String,
    #[serde(deserialize_with = "nz")]
    pub target: String,
    #[serde(rename = "scriptingBackend", deserialize_with = "nz")]
    pub scripting_backend: String,
    #[serde(deserialize_with = "nz")]
    pub development: bool,
    #[serde(deserialize_with = "nz")]
    pub player: String,
    #[serde(rename = "playerSHA256", deserialize_with = "nz")]
    pub player_sha256: String,
    #[serde(rename = "scriptingAssembliesSHA256", deserialize_with = "nz")]
    pub scripting_assemblies_sha256: String,
    #[serde(rename = "runtimeInitializersSHA256", deserialize_with = "nz")]
    pub runtime_initializers_sha256: String,
    #[serde(rename = "baseDefines")]
    pub base_defines: NilVec<String>,
    #[serde(rename = "baseReferences")]
    pub base_references: NilVec<String>,
    #[serde(rename = "engineReferences")]
    pub engine_references: NilVec<String>,
    pub inputs: NilMap<String>,
    #[serde(rename = "nativeAssemblies")]
    pub native_assemblies: NilMap<SourceNativeAssembly>,
    #[serde(rename = "profileSHA256", deserialize_with = "nz")]
    pub profile_sha256: String,
    #[serde(rename = "sourceBuildProofSHA256", deserialize_with = "nz")]
    pub source_build_proof_sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceNativeAssembly {
    #[serde(rename = "unresolvedReferences", skip_serializing_if = "NilVec::skip")]
    pub unresolved_references: NilVec<String>,
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub package: String,
    #[serde(
        rename = "packageContentHash",
        deserialize_with = "nz",
        skip_serializing_if = "is_empty"
    )]
    pub package_content_hash: String,
    #[serde(deserialize_with = "nz")]
    pub sha256: String,
    #[serde(deserialize_with = "nz")]
    pub configuration: AssemblyConfig,
    #[serde(rename = "sourceHashes")]
    pub source_hashes: NilMap<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SourceGraphOptions {
    pub project: String,
    pub editor: String,
    pub player: String,
    pub profile: Option<SourceEngineProfile>,
    pub packages: Option<Snapshot>,
    /// The graph the Editor compiles for itself (`Library/ScriptAssemblies`):
    /// Editor platform and defines, Editor folders, nothing from the host.
    pub editor_domain: bool,
}

/// The digest of the profile without its own digest.
pub fn profile_hash(profile: &SourceEngineProfile) -> Result<String> {
    let mut profile = profile.clone();
    profile.profile_sha256.clear();
    Ok(sha256(&marshal(&profile)?))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SourceBuildProof {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub backend: String,
    #[serde(rename = "unityVersion", deserialize_with = "nz")]
    pub unity_version: String,
    #[serde(deserialize_with = "nz")]
    pub target: String,
    #[serde(rename = "scriptingBackend", deserialize_with = "nz")]
    pub scripting_backend: String,
    #[serde(deserialize_with = "nz")]
    pub development: bool,
    pub assemblies: NilMap<SourceBuildProofAssembly>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SourceBuildProofAssembly {
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(rename = "dllSha256", deserialize_with = "nz")]
    pub dll_sha256: String,
    #[serde(deserialize_with = "nz")]
    pub configuration: AssemblyConfig,
    pub sources: NilMap<String>,
    pub references: NilMap<String>,
}

pub fn capture_engine_profile(
    ctx: &Context,
    project: &str,
    editor: &str,
    player: &str,
    reference_graph: &str,
    output: &str,
    snapshot: Option<&Snapshot>,
) -> Result<SourceEngineProfile> {
    validate_source_output(project, output)?;
    let Some(snapshot) =
        snapshot.filter(|snapshot| snapshot.project == project && snapshot.editor == editor)
    else {
        bail!("engine profile requires an exact resolved package snapshot");
    };
    snapshot.validate(ctx)?;
    let graph: CompilerGraphExport =
        read_json_file::<Option<_>>(reference_graph)?.unwrap_or_default();
    if graph.project != project || graph.unity != editor || graph.schema_version != 1 {
        bail!("public engine profile reference belongs to another project/Editor");
    }
    if graph.backend != "unity_editor"
        || !graph.development
        || graph.unity_version != UNITY_VERSION
        || graph.target != "StandaloneLinux64"
        || graph.scripting_backend != "Mono2x"
    {
        bail!("engine profile requires an explicit public Development Linux Mono graph");
    }
    let managed = managed_directory(player);
    let data = sysutil::dir(&managed);
    let build_proof_path = sysutil::join(&[&data, "UnityMicroEditorSourceProfile.json"]);
    let build_proof = read_build_proof(&build_proof_path)?;
    let settings = read_source_settings(project)?;
    let mut profile = SourceEngineProfile {
        schema_version: 1,
        backend: "unity_public_engine_profile".into(),
        unity_version: UNITY_VERSION.into(),
        unity_revision: UNITY_REVISION.into(),
        editor: editor.into(),
        target: "LinuxStandalone64".into(),
        scripting_backend: "Mono".into(),
        development: true,
        player: player.into(),
        inputs: NilMap::make(),
        native_assemblies: NilMap::make(),
        ..Default::default()
    };
    profile.api_compatibility = settings.api_compatibility;
    profile.source_build_proof_sha256 = file_hash(&build_proof_path)?;
    profile
        .inputs
        .insert(&build_proof_path, profile.source_build_proof_sha256.clone());
    profile.player_sha256 = file_hash(player)?;
    let scripting_assemblies = sysutil::join(&[&data, "ScriptingAssemblies.json"]);
    profile.scripting_assemblies_sha256 = file_hash(&scripting_assemblies)?;
    let initializers = sysutil::join(&[&data, "RuntimeInitializeOnLoads.json"]);
    match file_hash(&initializers) {
        Ok(hash) => {
            profile.runtime_initializers_sha256 = hash.clone();
            profile.inputs.insert(&initializers, hash);
        }
        Err(error) if is_not_exist(&error) => {}
        Err(error) => return Err(error),
    }
    let mut all_defines: Vec<&NilVec<String>> = Vec::new();
    for assembly in &graph.assemblies {
        all_defines.push(&assembly.config.defines);
        capture_profile_references(&mut profile, editor, &assembly.compiled_references);
    }
    for assembly in &graph.host_assemblies {
        all_defines.push(&assembly.defines);
        capture_profile_references(&mut profile, editor, &assembly.compiled_references);
    }
    let Some((first, rest)) = all_defines.split_first() else {
        bail!("public profile has no Player define evidence");
    };
    let mut defines = (*first).clone();
    for other in rest {
        defines = retain(&defines, |define| other.contains(define));
    }
    defines = retain(&defines, |define| {
        !(settings.defines.contains(define)
            || define == "ENABLE_INPUT_SYSTEM"
            || define == "ENABLE_LEGACY_INPUT_MANAGER"
            || define.starts_with("UNITY_EDITOR"))
    });
    profile.base_defines = super::sorted_strings(&defines);
    profile.base_references = super::sorted_strings(&profile.base_references);
    profile.engine_references = super::sorted_strings(&profile.engine_references);
    if profile.base_references.is_empty() || profile.engine_references.is_empty() {
        bail!("public profile lacks exact BCL/engine reference evidence");
    }
    for path in profile
        .base_references
        .iter()
        .chain(&profile.engine_references)
        .cloned()
        .collect::<Vec<_>>()
    {
        let hash = file_hash(&path)?;
        profile.inputs.insert(path, hash);
    }
    if !["UNITY_STANDALONE_LINUX", "ENABLE_MONO"]
        .iter()
        .all(|required| profile.base_defines.iter().any(|define| define == required))
    {
        bail!("public profile lacks pinned Linux Mono compilation symbols");
    }
    // A package assembly the host does not prove for this project (absent,
    // built from other sources, references or configuration) is compiled
    // from source like one the host lacks. Only the bridge must match.
    let engine_managed = sysutil::join(&[
        &sysutil::dir(editor),
        "Data/PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed",
    ]);
    'host: for exported in &graph.host_assemblies {
        macro_rules! unproven {
            ($($reason:tt)*) => {{
                if exported.name == "UnityMicroEditor" {
                    bail!($($reason)*);
                }
                eprintln!(
                    "[unity-micro-editor] host assembly {} is compiled from source: {}",
                    exported.name,
                    format!($($reason)*)
                );
                continue 'host;
            }};
        }
        let path = sysutil::join(&[&managed, &format!("{}.dll", exported.name)]);
        let hash = match file_hash(&path) {
            Ok(hash) => hash,
            Err(error) => unproven!("host engine profile lacks {}: {error}", exported.name),
        };
        // A preparation compiles the bridge again from the installation's
        // sources and puts it in place of the host's (`compiler::prepare`):
        // the host only has to hold it, whatever sources it was built from.
        if exported.name == "UnityMicroEditor" {
            profile.native_assemblies.insert(
                &exported.name,
                SourceNativeAssembly {
                    name: exported.name.clone(),
                    sha256: hash,
                    ..Default::default()
                },
            );
            continue;
        }
        let mut fact = SourceNativeAssembly {
            name: exported.name.clone(),
            sha256: hash.clone(),
            configuration: AssemblyConfig {
                name: exported.name.clone(),
                sources: exported.sources.clone(),
                defines: exported.defines.clone(),
                ..Default::default()
            },
            source_hashes: NilMap::make(),
            ..Default::default()
        };
        if let Some(configuration) = &exported.configuration {
            fact.configuration = (**configuration).clone();
        }
        let Some(built) = build_proof.assemblies.get(&exported.name).filter(|built| {
            built.name == exported.name && built.dll_sha256 == hash && !built.references.is_empty()
        }) else {
            unproven!(
                "native host lacks exact compiled-source proof: {}",
                exported.name
            );
        };
        for index in 0..fact.configuration.sources.len() {
            let source = fact.configuration.sources[index].clone();
            let mut logical = logical_path(project, snapshot, &source);
            if logical.is_empty() {
                for root in &graph.source_roots {
                    if within_directory(&source, &root.source) {
                        let relative = sysutil::rel(&root.source, &source).unwrap_or_default();
                        logical = format!("{}/{relative}", root.path);
                        break;
                    }
                }
            }
            if logical.is_empty() {
                unproven!("public native package source is outside resolved roots: {source}");
            }
            fact.configuration.sources[index] = logical.clone();
            for package in snapshot.packages.iter() {
                let prefix = format!("{}/", package.logical_root);
                if let Some(relative) = logical.strip_prefix(&prefix) {
                    if fact.package.is_empty() {
                        fact.package = package.name.clone();
                        fact.package_content_hash = package.content_hash.clone();
                    }
                    let current = sysutil::join(&[&package.path, relative]);
                    match (file_hash(&source), file_hash(&current)) {
                        (Ok(old), Ok(current)) if old == current => {}
                        _ => unproven!(
                            "public native source differs from resolved package content: {logical}"
                        ),
                    }
                    break;
                }
            }
            for input in [source.clone(), format!("{source}.meta")] {
                match file_hash(&input) {
                    Ok(hash) => {
                        let mut key = logical.clone();
                        if input.ends_with(".meta") {
                            key.push_str(".meta");
                        }
                        if built.sources.at(&key) != hash {
                            unproven!(
                                "native host source-body proof differs from current public source: {key}"
                            );
                        }
                        fact.source_hashes.insert(key, hash);
                    }
                    Err(error) if is_not_exist(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        // The first failure in key order is reported.
        for (logical, expected) in &built.sources {
            let path = physical_path(project, snapshot, logical);
            if path.is_empty() {
                unproven!(
                    "native source-build proof path is outside explicit source roots: {logical}"
                );
            }
            if !file_hash(&path).is_ok_and(|actual| actual == *expected) {
                unproven!("native host source/config proof differs: {logical}");
            }
            fact.source_hashes.insert(logical, expected.clone());
        }
        for (logical, expected) in &built.references {
            let mut path = physical_path(project, snapshot, logical);
            if path.is_empty()
                && sysutil::is_abs(logical)
                && within_directory(logical, &sysutil::join(&[&sysutil::dir(editor), "Data"]))
            {
                path = logical.clone();
            }
            if !file_hash(&path).is_ok_and(|actual| actual == *expected) {
                unproven!("native host compilation reference differs: {logical}");
            }
        }
        for path in &exported.compiled_references {
            let mut logical = logical_path(project, snapshot, path);
            if logical.is_empty() {
                logical = path.clone();
            }
            // The bridge is compiled again for every session against the
            // staged Player, with the references of this project. An engine
            // module the project enables after the host was built is a
            // reference the host's assembly compiled without, so did not need.
            let added_module = built.references.at(&logical).is_empty()
                && within_directory(path, &sysutil::join(&[&engine_managed, ""]))
                && sysutil::base(path).starts_with("UnityEngine.")
                && sysutil::base(path).ends_with("Module.dll");
            if !file_hash(path).is_ok_and(|actual| built.references.at(&logical) == actual)
                && exported.name != "UnityMicroEditor"
                && !added_module
            {
                unproven!(
                    "public native reference is missing/different in source-build proof: {path}"
                );
            }
        }
        for index in 0..fact.configuration.response_files.len() {
            let logical =
                logical_path(project, snapshot, &fact.configuration.response_files[index]);
            if logical.is_empty() {
                unproven!("native response path outside declared package/source roots");
            }
            fact.configuration.response_files[index] = logical;
        }
        if canonical_configuration(&fact.configuration)?
            != canonical_configuration(&built.configuration)?
        {
            unproven!(
                "native host compiled configuration differs from public graph: {}",
                exported.name
            );
        }
        profile.native_assemblies.insert(&exported.name, fact);
        if exported.name != "UnityMicroEditor" {
            profile.inputs.insert(path, hash);
        }
    }
    capture_optional_native_references(ctx, snapshot, &mut profile)?;
    for path in glob_lossy(&sysutil::join(&[&managed, "*.dll"])) {
        let base = sysutil::base(&path);
        let name = base.strip_suffix(".dll").unwrap_or(&base);
        if profile
            .native_assemblies
            .get(name)
            .is_some_and(|fact| !fact.name.is_empty())
        {
            continue;
        }
        let hash = file_hash(&path)?;
        profile.native_assemblies.insert(
            name,
            SourceNativeAssembly {
                name: name.to_string(),
                sha256: hash.clone(),
                ..Default::default()
            },
        );
        if name != "UnityMicroEditor" {
            profile.inputs.insert(path, hash);
        }
    }
    let editor_data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
    for pattern in ["Managed/UnityEngine/*.dll", "NetStandard/ref/2.1.0/*.dll"] {
        for path in glob_lossy(&sysutil::join(&[&editor_data, pattern])) {
            let hash = file_hash(&path)?;
            profile.inputs.insert(path, hash);
        }
    }
    profile.inputs.insert(
        scripting_assemblies,
        profile.scripting_assemblies_sha256.clone(),
    );
    profile.profile_sha256 = profile_hash(&profile)?;
    agent_immutable_json(output, &profile)?;
    Ok(profile)
}

pub fn read_build_proof(path: &str) -> Result<SourceBuildProof> {
    agent_canonical(path)?;
    let proof: SourceBuildProof = agent_read_json::<Option<_>>(path, 32 << 20)
        .map_err(|error| anyhow!("native host requires fresh public source-build proof: {error}"))?
        .unwrap_or_default();
    if proof.schema_version != 1
        || proof.backend != "unity_editor_build_sources"
        || proof.unity_version != UNITY_VERSION
        || proof.target != "StandaloneLinux64"
        || proof.scripting_backend != "Mono2x"
        || !proof.development
        || proof.assemblies.is_empty()
    {
        bail!("native source-build proof lacks pinned Development Linux Mono provenance");
    }
    Ok(proof)
}

/// Empty when the logical path has no canonical file
/// location under the explicit source roots.
pub fn physical_path(project: &str, snapshot: &Snapshot, logical: &str) -> String {
    if sysutil::clean(logical) != logical || sysutil::is_abs(logical) {
        return String::new();
    }
    let mut path = String::new();
    if logical.starts_with("Assets/") {
        path = sysutil::join(&[project, logical]);
    }
    for package in snapshot.packages.iter() {
        if let Some(relative) = logical.strip_prefix(&format!("{}/", package.logical_root)) {
            path = sysutil::join(&[&package.path, relative]);
            break;
        }
    }
    if path.is_empty()
        || logical_path(project, snapshot, &path) != logical
        || agent_canonical(&path).is_err()
    {
        return String::new();
    }
    path
}

pub fn validate_source_output(project: &str, output: &str) -> Result<()> {
    if !sysutil::is_abs(output)
        || sysutil::clean(output) != output
        || !sysutil::is_owned_cache_path(project, output)
    {
        bail!("Source output must be a strict child of owned project cache");
    }
    agent_canonical(output)?;
    if !matches!(std::fs::symlink_metadata(output), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        bail!("Source output already exists; choose a new output");
    }
    Ok(())
}

fn capture_profile_references(profile: &mut SourceEngineProfile, editor: &str, paths: &[String]) {
    let data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
    for path in paths {
        // The BCL extensions (`System.Runtime.CompilerServices.Unsafe` and the
        // like) are given to every assembly the Editor compiles.
        if within_directory(path, &sysutil::join(&[&data, "NetStandard"]))
            || within_directory(path, &sysutil::join(&[&data, "MonoBleedingEdge/lib/mono"]))
            || within_directory(
                path,
                &sysutil::join(&[&data, "BCLExtensions/TargetingPacks/netstandard2.1/ref"]),
            )
        {
            profile.base_references.push(path.clone());
        } else if within_directory(
            path,
            &sysutil::join(&[
                &data,
                "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed",
            ]),
        ) {
            profile.engine_references.push(path.clone());
        }
    }
}

/// Unity's built-in packages can
/// carry GUID references to optional, uninstalled packages. The already built
/// public Player proves this exact unchanged native package assembly compiled
/// without that reference. Record those exceptions explicitly; a new authored
/// or modified package assembly cannot inherit them.
fn capture_optional_native_references(
    ctx: &Context,
    snapshot: &Snapshot,
    profile: &mut SourceEngineProfile,
) -> Result<()> {
    let mut definitions: BTreeMap<String, SourceAsmdef> = BTreeMap::new();
    let mut guids = BTreeSet::new();
    for package in snapshot.packages.iter() {
        walk_dir(&package.path, &mut |entry| {
            let path = entry.path;
            ctx.err()?;
            if entry.is_dir() {
                let name = sysutil::base(path);
                if path != package.path && (name.starts_with('.') || name.ends_with('~')) {
                    return Ok(Walk::SkipDir);
                }
                return Ok(Walk::Continue);
            }
            if sysutil::ext(path) != ".asmdef" {
                return Ok(Walk::Continue);
            }
            let content = read_agent_file(path, 1 << 20)?;
            let mut definition = decode_asmdef(&content).map_err(|error| anyhow!(error))?;
            guids.insert(read_metadata_guid(&format!("{path}.meta"))?);
            definition.package = package.name.clone();
            definitions.insert(definition.name.clone(), definition);
            Ok(Walk::Continue)
        })?;
    }
    let names: Vec<String> = profile.native_assemblies.keys().cloned().collect();
    for name in names {
        let fact = profile.native_assemblies.value(&name);
        let (package, references) = definitions
            .get(&name)
            .map(|definition| (definition.package.as_str(), &definition.references[..]))
            .unwrap_or_default();
        if fact.package.is_empty() || fact.package != package || fact.source_hashes.is_empty() {
            continue;
        }
        let mut fact = fact;
        for reference in references {
            let missing = match reference.strip_prefix("GUID:") {
                Some(guid) => !guids.contains(&rune_lower(guid)),
                None => {
                    !definitions.contains_key(reference)
                        && profile
                            .native_assemblies
                            .get(reference)
                            .is_none_or(|fact| fact.name.is_empty())
                }
            };
            if missing {
                fact.unresolved_references.push(reference.clone());
            }
        }
        fact.unresolved_references = super::sorted_strings(&fact.unresolved_references);
        profile.native_assemblies.insert(name, fact);
    }
    Ok(())
}

pub fn load_engine_profile(
    ctx: &Context,
    filename: &str,
    editor: &str,
    player: &str,
) -> Result<SourceEngineProfile> {
    let profile: SourceEngineProfile =
        agent_read_json::<Option<_>>(filename, 16 << 20)?.unwrap_or_default();
    if profile.schema_version != 1
        || profile.backend != "unity_public_engine_profile"
        || profile.unity_version != UNITY_VERSION
        || profile.unity_revision != UNITY_REVISION
        || profile.editor != editor
        || profile.player != player
        || profile.target != "LinuxStandalone64"
        || profile.scripting_backend != "Mono"
        || !profile.development
        || profile.base_defines.is_empty()
        || profile.inputs.is_empty()
        || profile.base_references.is_empty()
        || profile.engine_references.is_empty()
        || !valid_sha256(&profile.source_build_proof_sha256)
    {
        bail!("engine profile does not match pinned Linux Mono host");
    }
    if !profile_hash(&profile).is_ok_and(|hash| hash == profile.profile_sha256) {
        bail!("engine profile fingerprint differs");
    }
    // Inputs are checked in key order.
    for (path, expected) in &profile.inputs {
        ctx.err()?;
        if !file_hash(path).is_ok_and(|actual| actual == *expected) {
            bail!("engine/native profile input changed: {path}");
        }
    }
    if !file_hash(player).is_ok_and(|actual| actual == profile.player_sha256) {
        bail!("engine profile Player executable changed");
    }
    Ok(profile)
}

/// `<player dir>/<player stem>_Data/Managed`.
pub fn managed_directory(player: &str) -> String {
    let base = sysutil::base(player);
    let stem = base.strip_suffix(sysutil::ext(player)).unwrap_or(&base);
    sysutil::join(&[&sysutil::dir(player), &format!("{stem}_Data"), "Managed"])
}

/// `Assets/...` or `Packages/<name>/...`, empty when
/// the file is outside the explicit source roots.
pub fn logical_path(project: &str, snapshot: &Snapshot, path: &str) -> String {
    if within_directory(path, &sysutil::join(&[project, "Assets"])) {
        return sysutil::rel(project, path).unwrap_or_default();
    }
    for package in snapshot.packages.iter() {
        if within_directory(path, &package.path) {
            let relative = sysutil::rel(&package.path, path).unwrap_or_default();
            return format!("{}/{relative}", package.logical_root);
        }
    }
    String::new()
}
