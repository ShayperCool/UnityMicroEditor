//! Authored source snapshots, owned staging clones and the source prefab action.
use super::{
    Any, AnyMap, Service, fail, join, json_kind, lstat, map_reply, marshal, not_exist, nz, obj,
    operation_id, read_json, store_error, text, to_serde, unmarshal_map,
};
use crate::{
    authoring::{NativeDefaultsTemplate, PrefabEditRequest, StagedPublicationRequest, Store},
    cancel::Context,
    compiler::{NilMap, Walk, WalkEntry, compiler_command, walk_dir},
    jsondoc,
    mcp::HandlerValue,
    project::{is_owned_cache_path, lexpath, value},
    sha256, sysutil,
    util::{agent_canonical, agent_immutable_json, agent_read_json, file_hash, read_agent_file},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSnapshot {
    #[serde(deserialize_with = "nz")]
    pub project: String,
    pub files: NilMap<String>,
}

/// Largest stored snapshot that is read back. A snapshot holds one path and
/// hash per authored file, so this bounds a project to some 200 000 files.
pub(crate) const SNAPSHOT_MAXIMUM: u64 = 64 << 20;

pub(crate) fn phase_id(id: &str, phase: &str) -> String {
    format!("{}-{phase}", &sha256(id.as_bytes())[..32])
}

/// Every authored file with its hash.
pub fn snapshot_source(ctx: &Context, project: &str) -> Result<SourceSnapshot> {
    let mut result = SourceSnapshot {
        project: project.into(),
        files: NilMap::make(),
    };
    for folder in ["Assets", "Packages", "ProjectSettings"] {
        walk_dir(&join(&[project, folder]), &mut |entry: &WalkEntry| {
            ctx.err()?;
            if entry.kind.is_symlink() {
                return Err(anyhow!(
                    "staging refuses authored source symlinks: {}",
                    entry.path
                ));
            }
            if entry.is_dir() {
                return Ok(Walk::Continue);
            }
            if !lstat(entry.path)?.is_regular() {
                return Err(anyhow!(
                    "staging source is not a regular file: {}",
                    entry.path
                ));
            }
            let logical = sysutil::rel(project, entry.path)
                .ok_or_else(|| anyhow!("Rel: can't make {} relative to {project}", entry.path))?;
            result.files.insert(logical, file_hash(entry.path)?);
            Ok(Walk::Continue)
        })?;
    }
    Ok(result)
}

/// The authored inventory still has the snapshot's bytes.
pub fn verify_snapshot(ctx: &Context, snapshot: &SourceSnapshot) -> Result<()> {
    let current = snapshot_source(ctx, &snapshot.project)?;
    if marshal(&snapshot.files)? != marshal(&current.files)? {
        return Err(fail(
            "revision_conflict",
            "authored source inventory changed while Editor prepared staged content",
        ));
    }
    Ok(())
}

/// Relative `file:` packages of the source
/// project become absolute in its clone.
fn normalize_staged_package_paths(source: &str, stage: &str) -> Result<()> {
    let path = join(&[stage, "Packages/manifest.json"]);
    let data = read_agent_file(&path, 1 << 20)?;
    let mut manifest = unmarshal_map(&data)?.unwrap_or_default();
    let Some(Any::Object(dependencies)) = manifest.get_mut("dependencies") else {
        return Err(anyhow!("staged package manifest lacks dependencies"));
    };
    let mut changed = false;
    // Visited in key order.
    for location in dependencies.values_mut() {
        let Any::String(text) = location else {
            continue;
        };
        let Some(physical) = text.strip_prefix("file:") else {
            continue;
        };
        if sysutil::is_abs(physical) {
            continue;
        }
        let canonical = lexpath::eval_symlinks(&join(&[source, "Packages", physical]))?;
        *location = format!("file:{canonical}").into();
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let mut encoded = jsondoc::marshal_indent(&Any::Object(manifest)).map_err(|e| anyhow!(e))?;
    encoded.push(b'\n');
    fs::write(&path, encoded).map_err(|error| sysutil::Error::os("open", &path, &error))?;
    Ok(())
}

/// An owned copy of the authored roots below `directory`,
/// with the runtime package installed and its completion recorded once.
pub fn clone_staging(
    ctx: &Context,
    source: &str,
    directory: &str,
) -> Result<(String, SourceSnapshot)> {
    let stage = join(&[directory, "project"]);
    agent_canonical(&stage)?;
    let marker = join(&[directory, "clone-complete.json"]);
    match read_json::<SourceSnapshot>(&marker, SNAPSHOT_MAXIMUM) {
        Ok(snapshot) => {
            if !sysutil::lstat(&stage).is_ok_and(|info| info.is_dir()) {
                return Err(anyhow!(
                    "completed staging project is missing or not a regular directory"
                ));
            }
            if snapshot.project != source {
                return Err(anyhow!("staging cache belongs to another project"));
            }
            return Ok((stage, snapshot));
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }
    if !matches!(sysutil::lstat(&stage), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(fail(
            "staging_incomplete",
            "staging directory exists without a completed snapshot; inspect it and use a new operationId",
        ));
    }
    let snapshot = snapshot_source(ctx, source)?;
    sysutil::mkdir_all(&stage)?;
    compiler_command(
        ctx,
        "",
        "cp",
        &[
            "-a",
            "--reflink=auto",
            "--",
            &join(&[source, "Assets"]),
            &join(&[source, "Packages"]),
            &join(&[source, "ProjectSettings"]),
            &stage,
        ],
    )?;
    normalize_staged_package_paths(source, &stage)?;
    super::install_agent_package(&stage)?;
    // A copied staging-derived target can carry a prior tool marker. Replace
    // only the marker inside this newly owned clone; retain the source bytes.
    let staging_marker = join(&[&stage, "ProjectSettings/UnityMicroEditorStaging.json"]);
    if snapshot
        .files
        .contains_key("ProjectSettings/UnityMicroEditorStaging.json")
    {
        agent_canonical(&staging_marker)?;
        fs::remove_file(&staging_marker)
            .map_err(|error| sysutil::Error::os("remove", &staging_marker, &error))?;
    }
    agent_immutable_json(
        &staging_marker,
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("sourceProject", source.into()),
        ])),
    )?;
    verify_snapshot(ctx, &snapshot)?;
    agent_immutable_json(&marker, &snapshot)?;
    Ok((stage, snapshot))
}

/// The one staging copy every Editor stage of `project` runs in. Its
/// `Library` and build caches outlive a preparation, so only the first host
/// build of a project is cold.
pub(crate) fn shared_stage(project: &str) -> String {
    join(&[project, "Library/UnityMicroEditor/stage/project"])
}

const STAGE_ROOTS: [&str; 3] = ["Assets", "Packages", "ProjectSettings"];
const STAGE_MARKER: &str = "ProjectSettings/UnityMicroEditorStaging.json";

/// Brings the shared staging copy of `source` up to date with its authored
/// roots and installs the runtime package there. Files whose bytes already
/// match keep their timestamps, so Unity reimports only what changed; whatever
/// an earlier stage Editor authored in those roots is removed. An interrupted
/// run is repaired by the next one.
pub fn sync_staging(ctx: &Context, source: &str, editor: &str) -> Result<(String, SourceSnapshot)> {
    let stage = shared_stage(source);
    if !is_owned_cache_path(source, &stage) {
        return Err(anyhow!("shared stage is outside owned project caches"));
    }
    agent_canonical(&stage)?;
    let exists = match sysutil::lstat(&stage) {
        Ok(info) if info.is_dir() => true,
        Ok(_) => return Err(anyhow!("shared staging project is not a regular directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(sysutil::Error::os("lstat", &stage, &error).into()),
    };
    if exists {
        if crate::editorbridge::project_is_open(&stage)? {
            return Err(fail(
                "staging_busy",
                "an Editor still has the shared staging copy open; wait for it or stop it before another preparation",
            ));
        }
        // A project copied together with its Library carries the stage of the
        // original, whose caches hold that path.
        let foreign = match super::read_map(&join(&[&stage, STAGE_MARKER]), 65536) {
            Ok(marker) => marker.get("sourceProject") != Some(&Any::from(source)),
            Err(error) if not_exist(&error) => false,
            Err(error) => return Err(error),
        };
        if foreign {
            let directory = sysutil::dir(&stage);
            fs::remove_dir_all(&directory)
                .map_err(|error| sysutil::Error::os("remove", &directory, &error))?;
        }
    }
    let snapshot = snapshot_source(ctx, source)?;
    sysutil::mkdir_all(&stage)?;
    for root in STAGE_ROOTS {
        prune_staged(ctx, source, &stage, root, &snapshot)?;
        walk_dir(&join(&[source, root]), &mut |entry: &WalkEntry| {
            if entry.is_dir() {
                let logical = sysutil::rel(source, entry.path).ok_or_else(|| {
                    anyhow!("Rel: can't make {} relative to {source}", entry.path)
                })?;
                sysutil::mkdir_all(&join(&[&stage, &logical]))?;
            }
            Ok(Walk::Continue)
        })?;
    }
    for (logical, hash) in snapshot.files.iter() {
        ctx.err()?;
        let (from, to) = (join(&[source, logical]), join(&[&stage, logical]));
        match sysutil::lstat(&to) {
            Ok(info) if info.is_regular() && file_hash(&to)? == *hash => continue,
            Ok(_) => {
                fs::remove_file(&to).map_err(|error| sysutil::Error::os("remove", &to, &error))?
            }
            Err(_) => {}
        }
        // Reflinked where the filesystem can; the source timestamp tells Unity
        // an unchanged file apart from an edited one.
        fs::copy(&from, &to).map_err(|error| sysutil::Error::os("copy", &to, &error))?;
        let modified = fs::metadata(&from)
            .and_then(|info| info.modified())
            .map_err(|error| sysutil::Error::os("stat", &from, &error))?;
        fs::File::open(&to)
            .and_then(|file| file.set_modified(modified))
            .map_err(|error| sysutil::Error::os("utimens", &to, &error))?;
    }
    normalize_staged_package_paths(source, &stage)?;
    super::install_agent_package(&stage)?;
    pin_staged_packages(ctx, source, editor, &stage)?;
    stage_compile_for(source, &stage)?;
    stage_device_platforms(ctx, source, editor, &stage)?;
    let staging_marker = join(&[&stage, STAGE_MARKER]);
    if snapshot.files.contains_key(STAGE_MARKER) {
        fs::remove_file(&staging_marker)
            .map_err(|error| sysutil::Error::os("remove", &staging_marker, &error))?;
    }
    agent_immutable_json(
        &staging_marker,
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("sourceProject", source.into()),
        ])),
    )?;
    verify_snapshot(ctx, &snapshot)?;
    Ok((stage, snapshot))
}

/// The registry whose packages any Editor can fetch.
const UNITY_REGISTRY: &str = "https://packages.unity.com";

/// Pins the stage's third-party packages to the copies the resolver verified.
///
/// A scoped registry or a Git repository may need credentials that only the
/// target's own Editor has; the resolver then takes the copy that Editor's
/// Package Manager left in the project. The stage's Editor gets the same
/// packages as `file:` dependencies, so it resolves nothing over the network
/// and builds with exactly what the tool compiles. Packages of the Unity
/// registry, built-in, embedded and local packages stay as authored.
fn pin_staged_packages(ctx: &Context, source: &str, editor: &str, stage: &str) -> Result<()> {
    // Only a scoped registry or a Git dependency brings a third-party package.
    let authored = read_agent_file(&join(&[source, "Packages/manifest.json"]), 1 << 20)?;
    let authored = unmarshal_map(&authored)?.unwrap_or_default();
    let scoped = matches!(authored.get("scopedRegistries"), Some(Any::Array(registries)) if !registries.is_empty());
    let git = matches!(authored.get("dependencies"), Some(Any::Object(dependencies))
        if dependencies.values().any(|request| matches!(request, Any::String(text) if crate::packages::is_git_request(text))));
    if !scoped && !git {
        return Ok(());
    }
    let options = || crate::packages::Options::default().with_runtime_package(source);
    // Without a resolution the stage's Editor resolves the packages itself,
    // with whatever credentials it has.
    let resolved = match crate::packages::load_current(ctx, source, editor, options()?) {
        Ok(snapshot) => snapshot,
        Err(_) => match crate::packages::resolve(ctx, source, editor, options()?) {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(()),
        },
    };
    let path = join(&[stage, "Packages/manifest.json"]);
    let data = read_agent_file(&path, 1 << 20)?;
    let mut manifest = unmarshal_map(&data)?.unwrap_or_default();
    let Some(Any::Object(dependencies)) = manifest.get_mut("dependencies") else {
        return Err(anyhow!("staged package manifest lacks dependencies"));
    };
    let mut changed = false;
    for package in &resolved.packages {
        let third_party = match package.source.as_str() {
            "git" => true,
            "registry" => package.registry.trim_end_matches('/') != UNITY_REGISTRY,
            _ => false,
        };
        if !third_party {
            continue;
        }
        let pinned = Any::String(format!("file:{}", package.path));
        if dependencies.get(&package.name) != Some(&pinned) {
            dependencies.insert(package.name.clone(), pinned);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let mut encoded = jsondoc::marshal_indent(&Any::Object(manifest)).map_err(|e| anyhow!(e))?;
    encoded.push(b'\n');
    fs::write(&path, encoded).map_err(|error| sysutil::Error::os("open", &path, &error))?;
    Ok(())
}

/// Gives the stage's Linux Standalone group the scripting define symbols of
/// the group the project's Player is compiled with
/// (`sourcegraph::compile_for_group`), so that its Editor builds the
/// host with the symbols the tool compiles with. The entry of that group is
/// copied as written; the copy keeps the source's timestamp, so an unchanged
/// selection does not make Unity reimport the settings.
fn stage_compile_for(source: &str, stage: &str) -> Result<()> {
    let Some(group) = crate::sourcegraph::compile_for_group(source)? else {
        return Ok(());
    };
    let from = join(&[source, "ProjectSettings/ProjectSettings.asset"]);
    let path = join(&[stage, "ProjectSettings/ProjectSettings.asset"]);
    let text = String::from_utf8(read_agent_file(&from, 32 << 20)?)
        .map_err(|_| anyhow!("{from} is not UTF-8"))?;
    let staged = with_standalone_defines(&text, &group)?;
    fs::write(&path, staged).map_err(|error| sysutil::Error::os("open", &path, &error))?;
    let modified = fs::metadata(&from)
        .and_then(|info| info.modified())
        .map_err(|error| sysutil::Error::os("stat", &from, &error))?;
    fs::File::open(&path)
        .and_then(|file| file.set_modified(modified))
        .map_err(|error| sysutil::Error::os("utimens", &path, &error))?;
    Ok(())
}

/// For a project compiled for a device group, makes the stage's Linux build
/// take the assemblies that group's build takes: every asmdef of the stage's
/// `Assets`, and of the packages it holds, is a Linux one exactly when it is
/// one of the device (`device_platforms`). The host is then a Linux build of
/// the device's code, with the device's symbols (`stage_compile_for`), and
/// the objects its Resources hold are serialized with their scripts. A
/// package with such an asmdef is embedded in the stage as a copy whose
/// asmdefs are rewritten. The copies keep the sources' timestamps.
fn stage_device_platforms(ctx: &Context, source: &str, editor: &str, stage: &str) -> Result<()> {
    let platform = crate::sourcegraph::code_platform(source)?;
    if platform == crate::sourcegraph::LINUX_PLAYER {
        return Ok(());
    }
    let rewrite = |from: &str, to: &str| -> Result<bool> {
        let data = read_agent_file(from, 1 << 20)?;
        let Some(text) = device_platforms(&String::from_utf8_lossy(&data), platform.asmdef)? else {
            return Ok(false);
        };
        fs::write(to, text).map_err(|error| sysutil::Error::os("open", to, &error))?;
        let modified = fs::metadata(from)
            .and_then(|info| info.modified())
            .map_err(|error| sysutil::Error::os("stat", from, &error))?;
        fs::File::open(to)
            .and_then(|file| file.set_modified(modified))
            .map_err(|error| sysutil::Error::os("utimens", to, &error))?;
        Ok(true)
    };
    let mut definitions = Vec::new();
    walk_dir(&join(&[stage, "Assets"]), &mut |entry: &WalkEntry| {
        ctx.err()?;
        if !entry.is_dir() && entry.path.ends_with(".asmdef") {
            definitions.push(entry.path.to_string());
        }
        Ok(Walk::Continue)
    })?;
    for path in definitions {
        let logical = sysutil::rel(stage, &path)
            .ok_or_else(|| anyhow!("Rel: can't make {path} relative to {stage}"))?;
        rewrite(&join(&[source, &logical]), &path)?;
    }
    // The packages, as the resolver took them.
    let options = || crate::packages::Options::default().with_runtime_package(source);
    let resolved = match crate::packages::load_current(ctx, source, editor, options()?) {
        Ok(snapshot) => snapshot,
        Err(_) => crate::packages::resolve(ctx, source, editor, options()?)?,
    };
    let path = join(&[stage, "Packages/manifest.json"]);
    let mut manifest = unmarshal_map(&read_agent_file(&path, 1 << 20)?)?.unwrap_or_default();
    let mut embedded = Vec::new();
    for package in &resolved.packages {
        if matches!(package.source.as_str(), "embedded" | "builtin")
            || package.name == crate::packages::RUNTIME_PACKAGE
        {
            continue;
        }
        let mut needed = Vec::new();
        walk_dir(&package.path, &mut |entry: &WalkEntry| {
            if entry.is_dir() && sysutil::base(entry.path).ends_with('~') {
                return Ok(Walk::SkipDir);
            }
            if !entry.is_dir() && entry.path.ends_with(".asmdef") {
                let text =
                    String::from_utf8_lossy(&read_agent_file(entry.path, 1 << 20)?).into_owned();
                if device_platforms(&text, platform.asmdef)?.is_some() {
                    needed.push(entry.path.to_string());
                }
            }
            Ok(Walk::Continue)
        })?;
        if needed.is_empty() {
            continue;
        }
        let copy = join(&[stage, "Packages", &package.name]);
        if fs::symlink_metadata(&copy).is_ok() {
            fs::remove_dir_all(&copy)
                .map_err(|error| sysutil::Error::os("remove", &copy, &error))?;
        }
        compiler_command(
            ctx,
            "",
            "cp",
            &["-a", "--reflink=auto", "--", &package.path, &copy],
        )?;
        for definition in needed {
            let relative = sysutil::rel(&package.path, &definition).ok_or_else(|| {
                anyhow!("Rel: can't make {definition} relative to {}", package.path)
            })?;
            rewrite(&definition, &join(&[&copy, &relative]))?;
        }
        embedded.push(package.name.clone());
    }
    if !embedded.is_empty() {
        if let Some(Any::Object(dependencies)) = manifest.get_mut("dependencies") {
            for name in &embedded {
                dependencies.remove(name);
            }
        }
        let mut encoded =
            jsondoc::marshal_indent(&Any::Object(manifest)).map_err(|e| anyhow!(e))?;
        encoded.push(b'\n');
        fs::write(&path, encoded).map_err(|error| sysutil::Error::os("open", &path, &error))?;
    }
    Ok(())
}

/// An asmdef whose membership of the Linux Standalone platform is made that
/// of `device` (`includePlatforms`/`excludePlatforms` as Unity names them),
/// or `None` when it already is.
pub(crate) fn device_platforms(text: &str, device: &str) -> Result<Option<String>> {
    const LINUX: &str = "LinuxStandalone64";
    let mut definition: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|error| anyhow!("asmdef: {error}"))?;
    let list =
        |definition: &serde_json::Map<String, serde_json::Value>, key: &str| -> Vec<String> {
            definition
                .get(key)
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
    let (mut include, mut exclude) = (
        list(&definition, "includePlatforms"),
        list(&definition, "excludePlatforms"),
    );
    let member = |include: &[String], exclude: &[String], platform: &str| {
        (include.is_empty() || include.iter().any(|item| item == platform))
            && !exclude.iter().any(|item| item == platform)
    };
    let wanted = member(&include, &exclude, device);
    if member(&include, &exclude, LINUX) == wanted {
        return Ok(None);
    }
    if wanted {
        exclude.retain(|item| item != LINUX);
        if !include.is_empty() {
            include.push(LINUX.into());
        }
    } else if include.is_empty() {
        exclude.push(LINUX.into());
    } else {
        include.retain(|item| item != LINUX);
        if include.is_empty() {
            // Only Linux was listed; another platform alone keeps it out.
            include.push(device.into());
        }
    }
    definition.insert("includePlatforms".into(), include.into());
    definition.insert("excludePlatforms".into(), exclude.into());
    let mut encoded = serde_json::to_string_pretty(&definition)?;
    encoded.push('\n');
    Ok(Some(encoded))
}

/// `ProjectSettings.asset` with the `Standalone` entry of
/// `scriptingDefineSymbols` replaced by a copy of `group`'s entry.
pub(crate) fn with_standalone_defines(text: &str, group: &str) -> Result<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = lines
        .iter()
        .position(|line| line.trim_end() == "  scriptingDefineSymbols:")
        .ok_or_else(|| anyhow!("PlayerSettings have no scripting define symbols"))?;
    // The map's entries are indented by four spaces; a folded value goes on
    // with deeper lines.
    let mut entries: Vec<(String, Vec<&str>)> = Vec::new();
    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end];
        if let Some(entry) = line.strip_prefix("    ")
            && !entry.starts_with(' ')
        {
            let key = entry.split(':').next().unwrap_or_default().to_string();
            entries.push((key, vec![line]));
        } else if line.starts_with("      ") && !entries.is_empty() {
            entries.last_mut().unwrap().1.push(line);
        } else {
            break;
        }
        end += 1;
    }
    let Some((_, chosen)) = entries.iter().find(|(key, _)| key == group) else {
        return Err(anyhow!(
            "the project's PlayerSettings have no scripting define symbols for {group}"
        ));
    };
    let mut copy: Vec<String> = chosen.iter().map(|line| line.to_string()).collect();
    copy[0] = format!("    Standalone{}", &copy[0][4 + group.len()..]);
    let platform = crate::sourcegraph::group_platform_defines(group);
    if !platform.is_empty() {
        let last = copy.last_mut().unwrap();
        let ending = &last[last.trim_end_matches(['\r', '\n']).len()..];
        let ending = ending.to_string();
        let value = last.trim_end_matches(['\r', '\n']).to_string();
        let separator = if value.trim_end().ends_with(':') {
            " "
        } else {
            ";"
        };
        *last = format!("{value}{separator}{}{ending}", platform.join(";"));
    }
    let mut result: String = lines[..=start].concat();
    let mut placed = false;
    for (key, entry) in &entries {
        if key == "Standalone" {
            result.extend(copy.iter().map(String::as_str));
            placed = true;
        } else {
            if !placed && key.as_str() > "Standalone" {
                result.extend(copy.iter().map(String::as_str));
                placed = true;
            }
            result.extend(entry.iter().copied());
        }
    }
    if !placed {
        result.extend(copy.iter().map(String::as_str));
    }
    result.push_str(&lines[end..].concat());
    Ok(result)
}

/// Removes what the staged `relative` directory holds beyond the snapshot.
fn prune_staged(
    ctx: &Context,
    source: &str,
    stage: &str,
    relative: &str,
    snapshot: &SourceSnapshot,
) -> Result<()> {
    ctx.err()?;
    let directory = join(&[stage, relative]);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(sysutil::Error::os("open", &directory, &error).into()),
    };
    for entry in entries {
        let entry = entry.map_err(|error| sysutil::Error::os("readdirent", &directory, &error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let (logical, path) = (join(&[relative, &name]), join(&[&directory, &name]));
        let kind = entry
            .file_type()
            .map_err(|error| sysutil::Error::os("lstat", &path, &error))?;
        if !kind.is_dir() {
            if logical != STAGE_MARKER && !snapshot.files.contains_key(&logical) {
                fs::remove_file(&path)
                    .map_err(|error| sysutil::Error::os("remove", &path, &error))?;
            }
        } else if sysutil::lstat(&join(&[source, &logical])).is_ok_and(|info| info.is_dir()) {
            prune_staged(ctx, source, stage, &logical, snapshot)?;
        } else {
            fs::remove_dir_all(&path)
                .map_err(|error| sysutil::Error::os("remove", &path, &error))?;
        }
    }
    Ok(())
}

/// Decodes the arguments into a `PrefabEditRequest`:
/// members are applied in key order, unknown members are ignored and names
/// match case-insensitively after an exact match.
pub(crate) fn decode_prefab_request(args: &AnyMap) -> Result<PrefabEditRequest, String> {
    const FIELDS: [&str; 17] = [
        "action",
        "path",
        "expectedRevision",
        "operationId",
        "fileId",
        "targetPath",
        "prefabPath",
        "outputPath",
        "parentFileId",
        "parentPath",
        "name",
        "completely",
        "componentType",
        "property",
        "value",
        "fields",
        "defaultsTemplate",
    ];
    let fold = |name: &str| {
        name.to_lowercase()
            .replace('\u{17f}', "s")
            .replace('\u{212a}', "k")
    };
    let mut request = PrefabEditRequest::default();
    let mut first: Option<String> = None;
    for (key, member) in args {
        let Some(field) = FIELDS
            .iter()
            .position(|field| *field == key)
            .or_else(|| FIELDS.iter().position(|field| fold(field) == fold(key)))
        else {
            continue;
        };
        let at = jsondoc::Target {
            strukt: "PrefabEditRequest",
            field: FIELDS[field],
        };
        let string = |target: &mut String| match member {
            Any::Null => Ok(()),
            Any::String(text) => {
                *target = text.clone();
                Ok(())
            }
            other => Err(at.mismatch(json_kind(other), "string")),
        };
        let outcome = match field {
            0 => string(&mut request.action),
            1 => string(&mut request.path),
            2 => string(&mut request.expected_revision),
            3 => string(&mut request.operation_id),
            4 => string(&mut request.file_id),
            5 => string(&mut request.target_path),
            6 => string(&mut request.prefab_path),
            7 => string(&mut request.output_path),
            8 => string(&mut request.parent_file_id),
            9 => string(&mut request.parent_path),
            10 => string(&mut request.name),
            11 => match member {
                Any::Null => Ok(()),
                Any::Bool(flag) => {
                    request.completely = *flag;
                    Ok(())
                }
                other => Err(at.mismatch(json_kind(other), "bool")),
            },
            12 => string(&mut request.component_type),
            13 => string(&mut request.property),
            14 => {
                request.value = (!member.is_null()).then(|| to_serde(member));
                Ok(())
            }
            15 => match member {
                Any::Null => {
                    request.fields = BTreeMap::new();
                    Ok(())
                }
                Any::Object(fields) => {
                    for (name, field) in fields {
                        request.fields.insert(name.clone(), to_serde(field));
                    }
                    Ok(())
                }
                other => Err(at.mismatch(json_kind(other), "object")),
            },
            _ => match member {
                Any::Null => {
                    request.defaults_template = None;
                    Ok(())
                }
                Any::Object(_) => {
                    serde_json::from_value::<NativeDefaultsTemplate>(to_serde(member))
                        .map(|template| request.defaults_template = Some(template))
                        .map_err(|error| error.to_string())
                }
                other => Err(at.mismatch(json_kind(other), "authoring.NativeDefaultsTemplate")),
            },
        };
        if let Err(error) = outcome {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(request), Err)
}

impl Service {
    /// A revision-checked source prefab action. No Editor runs.
    pub(crate) fn prefab_edit(&self, ctx: &Context, args: &AnyMap) -> Result<HandlerValue> {
        let project = self.0.project.as_str();
        let id = text(args, "operationId");
        if !operation_id(&id) {
            return Err(fail(
                "invalid_request",
                "source prefab authoring requires operationId of at most 64 ASCII letters/digits/hyphens",
            ));
        }
        let mut request = decode_prefab_request(args).map_err(|error| {
            fail(
                "invalid_request",
                format!("invalid source prefab arguments: {error}"),
            )
        })?;
        let mut profile_sha = String::new();
        let mut guard_profile = None;
        if request.action == "add_component" {
            let name = super::defaults::component_defaults_name(&request.component_type)?;
            let (profile, hash) = super::component_defaults_profile(ctx, project, &self.0.editor)?;
            let catalog = super::defaults::read_defaults_catalog(project, &profile, &hash)?;
            let template = catalog.templates.get(&name).cloned().unwrap_or_default();
            request.component_type = name;
            request.defaults_template = Some(template);
            profile_sha = hash.clone();
            guard_profile = Some(hash);
        } else if request.defaults_template.is_some() {
            return Err(fail(
                "invalid_request",
                "defaultsTemplate is supplied by the versioned component cache, not the caller",
            ));
        }
        let store = Store::new(Path::new(project)).map_err(store_error)?;
        // Previously accepted Editor-backed plans remain replayable through the
        // same source journal. This migration path never creates a legacy plan or
        // starts an Editor; it requires the existing immutable request receipt.
        let directory = join(&[
            project,
            "Library/UnityMicroEditor/authoring-stages",
            &sha256(id.as_bytes()),
        ]);
        match agent_read_json::<StagedPublicationRequest>(
            &join(&[&directory, "publication.json"]),
            80 << 20,
        ) {
            Ok(legacy) => {
                if legacy.operation_id != id {
                    return Err(fail(
                        "invalid_plan",
                        "legacy publication belongs to another operation",
                    ));
                }
                let request_path = join(&[&directory, "request.json"]);
                if sysutil::lstat(&request_path).is_err() {
                    return Err(fail(
                        "invalid_plan",
                        "legacy publication has no immutable request receipt",
                    ));
                }
                agent_immutable_json(
                    &request_path,
                    &Any::Object(obj([
                        ("schemaVersion", 1.into()),
                        ("project", project.into()),
                        ("arguments", Any::Object(args.clone())),
                    ])),
                )?;
                let result = store.publish_staged(&legacy).map_err(store_error)?;
                return map_reply(obj([
                    ("backend", "raw_source".into()),
                    ("planOrigin", "unity_editor_staged_legacy".into()),
                    ("operationId", id.into()),
                    ("publication", value::raw(&result)?),
                    ("editorStarted", false.into()),
                ]));
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        let guard = guard_profile.map(|hash| self.component_defaults_guard(ctx, hash));
        let result = match &guard {
            Some(guard) => store.prefab_edit(&request, Some(guard)),
            None => store.prefab_edit(&request, None),
        }
        .map_err(store_error)?;
        let created = if result.created_ids_nil {
            Any::Null
        } else {
            value::raw(&result.created_ids)?
        };
        let mut response = obj([
            ("backend", result.backend.clone().into()),
            ("operationId", result.operation_id.clone().into()),
            ("action", result.action.clone().into()),
            ("publication", value::raw(&result.publication)?),
            ("createdIds", created),
            ("generatedPath", result.generated_path.clone().into()),
            ("editorStarted", false.into()),
        ]);
        if !profile_sha.is_empty() {
            response.insert("defaultsBackend".into(), "versioned_component_cache".into());
            response.insert("profileSha256".into(), profile_sha.into());
        }
        map_reply(response)
    }
}

/// An Editor working in a clone may neither
/// change existing authored metadata nor leave metadata for unplanned assets.
pub fn validate_staged_metadata(
    snapshot: &SourceSnapshot,
    stage: &str,
    new_assets: &std::collections::BTreeSet<String>,
) -> Result<()> {
    for (logical, expected) in &snapshot.files {
        if !logical.starts_with("Assets/") || !logical.ends_with(".meta") {
            continue;
        }
        let path = join(&[stage, logical]);
        agent_canonical(&path)?;
        if !file_hash(&path).is_ok_and(|actual| actual == *expected) {
            return Err(fail(
                "invalid_identity",
                format!("Editor changed or removed existing authored metadata: {logical}"),
            ));
        }
    }
    walk_dir(&join(&[stage, "Assets"]), &mut |entry: &WalkEntry| {
        if entry.is_dir() || !entry.path.ends_with(".meta") {
            return Ok(Walk::Continue);
        }
        let logical = sysutil::rel(stage, entry.path)
            .ok_or_else(|| anyhow!("Rel: can't make {} relative to {stage}", entry.path))?;
        if snapshot.files.contains_key(&logical) {
            return Ok(Walk::Continue);
        }
        if !new_assets.contains(logical.strip_suffix(".meta").unwrap_or(&logical)) {
            return Err(fail(
                "invalid_identity",
                format!("Editor generated orphan or unplanned metadata: {logical}"),
            ));
        }
        Ok(Walk::Continue)
    })
}
