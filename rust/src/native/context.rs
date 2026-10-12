//! Owned import contexts: selected exact source/meta bytes copied into a new
//! directory. No Editor runs and no SourceAssetDB/ArtifactDB, assemblies,
//! imported Library contents or asset bundles are copied.
use super::scripted::ScriptEnvironment;
use super::support::{
    abs, check, eval_symlinks, mkdir, mkdir_all, os_error, read_file, wrap, write_file,
};
use crate::{
    ENGINE_VERSION,
    cancel::Context,
    hex, jsondoc,
    registry::{meta_guid, project_registry_path},
    sysutil,
    util::{canonical_destination, within_directory},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::{Read, Write},
};

const SOURCE_LIMIT: i64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectImportSource {
    #[serde(deserialize_with = "super::support::nul")]
    pub path: String,
    #[serde(deserialize_with = "super::support::nul")]
    pub guid: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectImportContext {
    #[serde(deserialize_with = "super::support::nul")]
    pub schema_version: i64,
    #[serde(deserialize_with = "super::support::nul")]
    pub source_project: String,
    #[serde(deserialize_with = "super::support::nul")]
    pub project: String,
    #[serde(deserialize_with = "super::support::nul")]
    pub unity_version: String,
    /// `None` is an absent list (`null`).
    pub assets: Option<Vec<ProjectImportSource>>,
    /// `None` is an absent map (`null`).
    pub source_hashes: Option<BTreeMap<String, String>>,
    #[serde(deserialize_with = "super::support::nul")]
    pub source_bytes: i64,
    /// The selected assets the worker finds but is not asked to import.
    #[serde(skip)]
    pub registered: BTreeSet<String>,
}
impl ProjectImportContext {
    /// The selected assets the worker imports, in request order.
    pub fn imported(&self) -> impl Iterator<Item = &ProjectImportSource> {
        self.assets()
            .iter()
            .filter(|asset| !self.registered.contains(&asset.path))
    }
    pub fn assets(&self) -> &[ProjectImportSource] {
        self.assets.as_deref().unwrap_or(&[])
    }
    pub fn source_hash(&self, filename: &str) -> &str {
        self.source_hashes
            .as_ref()
            .and_then(|hashes| hashes.get(filename))
            .map_or("", String::as_str)
    }
    pub fn source_hash_count(&self) -> usize {
        self.source_hashes.as_ref().map_or(0, BTreeMap::len)
    }
}

/// Matches one path element against a glob pattern.
pub(crate) fn glob_match(pattern: &[u8], name: &[u8]) -> Result<bool> {
    let bad = || anyhow!("syntax error in pattern");
    let (mut pattern, mut name) = (pattern, name);
    'pattern: while !pattern.is_empty() {
        // Take the next chunk: leading stars, then everything up to the next star outside a class.
        let mut star = false;
        while pattern.first() == Some(&b'*') {
            pattern = &pattern[1..];
            star = true;
        }
        let mut in_range = false;
        let mut end = 0;
        while end < pattern.len() {
            match pattern[end] {
                b'\\' if end + 1 < pattern.len() => end += 1,
                b'[' => in_range = true,
                b']' => in_range = false,
                b'*' if !in_range => break,
                _ => {}
            }
            end += 1;
        }
        let (chunk, rest) = pattern.split_at(end);
        pattern = rest;
        if star && chunk.is_empty() {
            // A trailing star matches the rest of the element.
            return Ok(!name.contains(&b'/'));
        }
        if let Some(tail) = match_chunk(chunk, name)?
            && (tail.is_empty() || !pattern.is_empty())
        {
            name = tail;
            continue;
        }
        if star {
            for skip in 0..name.len() {
                if name[skip] == b'/' {
                    break;
                }
                if let Some(tail) = match_chunk(chunk, &name[skip + 1..])? {
                    if pattern.is_empty() && !tail.is_empty() {
                        continue;
                    }
                    name = tail;
                    continue 'pattern;
                }
            }
        }
        // Report a malformed remainder even when the match already failed.
        while !pattern.is_empty() {
            while pattern.first() == Some(&b'*') {
                pattern = &pattern[1..];
            }
            let mut in_range = false;
            let mut end = 0;
            while end < pattern.len() {
                match pattern[end] {
                    b'\\' if end + 1 < pattern.len() => end += 1,
                    b'[' => in_range = true,
                    b']' => in_range = false,
                    b'*' if !in_range => break,
                    _ => {}
                }
                end += 1;
            }
            let (chunk, rest) = pattern.split_at(end);
            pattern = rest;
            match_chunk(chunk, b"").map_err(|_| bad())?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}
/// The unmatched tail of `name`, or `None` when the chunk fails.
fn match_chunk<'a>(chunk: &[u8], name: &'a [u8]) -> Result<Option<&'a [u8]>> {
    let bad = || anyhow!("syntax error in pattern");
    let rune = |bytes: &[u8]| -> (char, usize) {
        let width = match bytes[0] {
            0..0x80 => 1,
            0xc0..0xe0 => 2,
            0xe0..0xf0 => 3,
            0xf0..0xf8 => 4,
            _ => 1,
        };
        match bytes.get(..width).and_then(|b| std::str::from_utf8(b).ok()) {
            Some(text) => (text.chars().next().unwrap(), width),
            None => ('\u{fffd}', 1),
        }
    };
    let (mut chunk, mut name) = (chunk, name);
    // Once the name is exhausted the chunk is still parsed for syntax errors.
    let mut failed = false;
    while !chunk.is_empty() {
        if !failed && name.is_empty() {
            failed = true;
        }
        match chunk[0] {
            b'[' => {
                let mut current = '\0';
                if !failed {
                    let (value, width) = rune(name);
                    current = value;
                    name = &name[width..];
                }
                chunk = &chunk[1..];
                let negated = chunk.first() == Some(&b'^');
                if negated {
                    chunk = &chunk[1..];
                }
                let (mut matched, mut ranges) = (false, 0);
                loop {
                    if chunk.first() == Some(&b']') && ranges > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let escape = |chunk: &mut &[u8]| -> Result<char> {
                        if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
                            return Err(bad());
                        }
                        if chunk[0] == b'\\' {
                            *chunk = &chunk[1..];
                            if chunk.is_empty() {
                                return Err(bad());
                            }
                        }
                        let (value, width) = rune(chunk);
                        if value == '\u{fffd}' && width == 1 {
                            return Err(bad());
                        }
                        *chunk = &chunk[width..];
                        if chunk.is_empty() {
                            return Err(bad());
                        }
                        Ok(value)
                    };
                    let low = escape(&mut chunk)?;
                    let mut high = low;
                    if chunk[0] == b'-' {
                        chunk = &chunk[1..];
                        high = escape(&mut chunk)?;
                    }
                    if low <= current && current <= high {
                        matched = true;
                    }
                    ranges += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            b'?' => {
                if !failed {
                    if name[0] == b'/' {
                        failed = true;
                    }
                    let (_, width) = rune(name);
                    name = &name[width..];
                }
                chunk = &chunk[1..];
            }
            first => {
                let mut literal = first;
                if first == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return Err(bad());
                    }
                    literal = chunk[0];
                }
                if !failed {
                    if literal != name[0] {
                        failed = true;
                    }
                    name = &name[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    Ok((!failed).then_some(name))
}
fn has_meta(path: &str) -> bool {
    path.contains(['*', '?', '[', '\\'])
}
/// Paths matching a glob pattern: every directory element of the pattern is a pattern too.
fn glob(pattern: &str, depth: usize) -> Result<Vec<String>> {
    if depth > 10000 {
        bail!("syntax error in pattern");
    }
    glob_match(pattern.as_bytes(), b"")?;
    if !has_meta(pattern) {
        return Ok(sysutil::lstat(pattern)
            .map(|_| vec![pattern.to_string()])
            .unwrap_or_default());
    }
    let split = pattern.rfind('/').map_or(0, |index| index + 1);
    let (directory, file) = pattern.split_at(split);
    let directory = match directory {
        "" => ".",
        "/" => "/",
        other => &other[..other.len() - 1],
    };
    let directories = if has_meta(directory) {
        if directory == pattern {
            bail!("syntax error in pattern");
        }
        glob(directory, depth + 1)?
    } else {
        vec![directory.to_string()]
    };
    let mut matches = Vec::new();
    for directory in directories {
        if !sysutil::stat(&directory).is_ok_and(|info| info.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            if glob_match(file.as_bytes(), name.as_bytes())? {
                matches.push(sysutil::join(&[&directory, &name]));
            }
        }
    }
    Ok(matches)
}

/// Accepts physical source assets and resolved UPM source packages.
/// PackageCache content is input, never a seeded import cache.
pub fn resolve_project_import_source(project: &str, logical: &str) -> Result<String> {
    if let Err(error) = project_registry_path(logical)
        && !logical.starts_with("ProjectSettings/")
    {
        return Err(error);
    }
    let mut filename = sysutil::join(&[project, logical]);
    if sysutil::lstat(&filename).is_err_and(|e| e.raw_os_error() == Some(libc::ENOENT))
        && logical.starts_with("Packages/")
    {
        let parts: Vec<&str> = logical.split('/').collect();
        if parts.len() >= 3 {
            let pattern = sysutil::join(&[
                project,
                "Library",
                "PackageCache",
                &format!("{}@*", parts[1]),
            ]);
            match glob(&pattern, 0) {
                Ok(candidates) if candidates.len() == 1 => {
                    filename = sysutil::join(&[&candidates[0], &parts[2..].join("/")]);
                }
                _ => bail!("package source {} is unresolved or ambiguous", parts[1]),
            }
        }
    }
    let canonical =
        eval_symlinks(&filename).map_err(|e| wrap(&format!("resolve source {logical}"), e))?;
    if !within_directory(&canonical, project) {
        bail!("source file resolves outside source project: {logical}");
    }
    if !sysutil::stat(&canonical).is_ok_and(|info| info.is_regular() && info.size <= SOURCE_LIMIT) {
        bail!("source file is not a bounded regular file: {logical}");
    }
    Ok(canonical)
}

fn copy_project_import_source(
    ctx: &Context,
    source: &str,
    destination: &str,
) -> Result<(String, i64)> {
    check(ctx)?;
    let mut input = File::open(source).map_err(|e| os_error("open", source, &e))?;
    let before = match sysutil::fstat(&input) {
        Ok(before) if before.is_regular() && before.size <= SOURCE_LIMIT => before,
        _ => bail!("project source file is outside import bounds"),
    };
    mkdir_all(&sysutil::dir(destination), 0o700)?;
    let mut output = super::support::create_new(destination, 0o600)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    let mut copied = 0i64;
    loop {
        check(ctx)?;
        let count = match input.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(os_error("read", source, &error)),
        };
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|e| os_error("write", destination, &e))?;
        hasher.update(&buffer[..count]);
        copied += count as i64;
        if copied > SOURCE_LIMIT {
            bail!("source file grew beyond import bounds");
        }
    }
    let unchanged = match (sysutil::fstat(&input), sysutil::stat(source)) {
        (Ok(after), Ok(current)) => {
            before.same_file(&current)
                && before.size == copied
                && before.size == after.size
                && before.modified == after.modified
                && before.modified == current.modified
        }
        _ => false,
    };
    if !unchanged {
        bail!("project source changed while copying");
    }
    drop(output);
    let stamp = libc::timespec {
        tv_sec: before.modified.0,
        tv_nsec: before.modified.1,
    };
    let name = std::ffi::CString::new(destination)
        .map_err(|_| anyhow!("chtimes {destination}: invalid argument"))?;
    // SAFETY: `name` is NUL-terminated and the two timestamps outlive the call.
    if unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), [stamp, stamp].as_ptr(), 0) } != 0 {
        return Err(os_error(
            "chtimes",
            destination,
            &std::io::Error::last_os_error(),
        ));
    }
    Ok((hex(&hasher.finalize()), copied))
}

/// The directory part of a clean logical path; `.` when it has no slash.
fn logical_dir(path: &str) -> &str {
    path.rfind('/').map_or(".", |index| &path[..index])
}

/// Materializes selected exact source/meta bytes in a new owned context. The
/// partially filled context is returned next to an error raised after its
/// directory was created.
pub fn create_project_import_context(
    ctx: &Context,
    source_project: &str,
    output: &str,
    assets: &[String],
) -> (ProjectImportContext, Result<()>) {
    create_scripted_import_context(
        ctx,
        source_project,
        output,
        assets,
        None,
        &ContextExtras::default(),
    )
}

/// What a context holds beyond the selected sources of its project.
#[derive(Clone, Debug, Default)]
pub struct ContextExtras {
    /// Files written into the context, by path relative to its project. One
    /// at the path of a selected asset or its `.meta` stands for the
    /// project's file.
    pub files: BTreeMap<String, Vec<u8>>,
    /// The selected assets the worker finds but is not asked to import.
    pub registered: BTreeSet<String>,
}

/// The same with the compiled importers the selected assets need: their
/// assemblies, and the scripts those were compiled from where the Editor
/// compiled them.
pub fn create_scripted_import_context(
    ctx: &Context,
    source_project: &str,
    output: &str,
    assets: &[String],
    scripts: Option<&ScriptEnvironment>,
    extras: &ContextExtras,
) -> (ProjectImportContext, Result<()>) {
    let mut result = ProjectImportContext::default();
    let outcome = create(
        ctx,
        source_project,
        output,
        assets,
        scripts,
        extras,
        None,
        &mut result,
    );
    (result, outcome)
}

/// What a build of Player objects adds to an import context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildFiles {
    /// Assets the coordinator makes itself, which no source of the project
    /// stands for: the data and the `.meta` of each, by logical path. They
    /// are listed among the assets of the batch.
    pub generated: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
    /// Files an importer reads from disk without their being assets of the
    /// batch, by logical path: the templates a shader graph is written
    /// out with.
    pub files: Vec<String>,
}

/// The same for a build of Player objects: the context also holds the
/// graphics and quality settings, whose pipeline assets a build reads, and
/// an asset of a package is taken from where the compilation found the
/// package.
pub fn create_build_import_context(
    ctx: &Context,
    source_project: &str,
    output: &str,
    assets: &[String],
    scripts: &ScriptEnvironment,
    build: &BuildFiles,
    extras: &ContextExtras,
) -> (ProjectImportContext, Result<()>) {
    let mut result = ProjectImportContext::default();
    let outcome = create(
        ctx,
        source_project,
        output,
        assets,
        Some(scripts),
        extras,
        Some(build),
        &mut result,
    );
    (result, outcome)
}

/// A file of the project an importer's assemblies were compiled in.
fn resolve_script_source(library: &str, relative: &str) -> Result<String> {
    let canonical = eval_symlinks(&sysutil::join(&[library, relative]))
        .map_err(|e| wrap(&format!("resolve importer source {relative}"), e))?;
    if !within_directory(&canonical, library) {
        bail!("importer source resolves outside its project: {relative}");
    }
    if !sysutil::stat(&canonical).is_ok_and(|info| info.is_regular() && info.size <= SOURCE_LIMIT) {
        bail!("importer source is not a bounded regular file: {relative}");
    }
    Ok(canonical)
}

#[allow(clippy::too_many_arguments)]
fn create(
    ctx: &Context,
    source_project: &str,
    output: &str,
    assets: &[String],
    scripts: Option<&ScriptEnvironment>,
    extras: &ContextExtras,
    build: Option<&BuildFiles>,
    result: &mut ProjectImportContext,
) -> Result<()> {
    // Where the context holds a logical path, relative to its project.
    let place = |logical: &str| scripts.map_or(logical.to_string(), |env| env.physical(logical));
    if assets.is_empty() || assets.len() > 4096 {
        bail!("project import requires a bounded nonempty asset set");
    }
    let source_project = eval_symlinks(&abs(source_project)?)?;
    let output = abs(output)?;
    let canonical = canonical_destination(&output)?;
    if output != canonical || !sysutil::is_owned_cache_path(&source_project, &output) {
        bail!(
            "project import output must be a new directory inside source project Temp or Library/UnityMicroEditor without symlink ancestors"
        );
    }
    if !sysutil::lstat(&output).is_err_and(|e| e.raw_os_error() == Some(libc::ENOENT)) {
        bail!("project import output already exists");
    }
    // Validate source identities before creating output or native processes.
    // A build takes an asset of a package from where the compilation found
    // the package; anything else comes from the project.
    let resolve = |logical: &str| -> Result<String> {
        match scripts.filter(|_| build.is_some()) {
            Some(environment) if environment.physical(logical) != logical => {
                resolve_script_source(&environment.library, &environment.physical(logical))
            }
            _ => resolve_project_import_source(&source_project, logical),
        }
    };
    let mut selected = Vec::new();
    let mut source_files: BTreeMap<String, String> = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut guid_paths: HashMap<String, String> = HashMap::new();
    for asset in assets {
        let root = match project_registry_path(asset) {
            Ok(root) if *asset != root && !asset.ends_with(".meta") => root,
            _ => bail!("invalid source asset {asset}"),
        };
        if !seen.insert(super::support::rune_lower(asset)) {
            bail!("source asset repeated or has a case collision: {asset}");
        }
        if let Some((_, meta)) = build.and_then(|build| build.generated.get(asset)) {
            let guid = meta_guid(meta).map_err(|e| wrap(asset, e))?;
            if let Some(previous) = guid_paths.get(&guid) {
                bail!("source GUID is shared by {previous} and {asset}");
            }
            guid_paths.insert(guid.clone(), asset.clone());
            selected.push(ProjectImportSource {
                path: asset.clone(),
                guid,
            });
            continue;
        }
        let meta_path = format!("{asset}.meta");
        if !extras.files.contains_key(&place(asset)) {
            let filename = resolve(asset)?;
            source_files.insert(place(asset), filename);
        }
        let meta = match extras.files.get(&place(&meta_path)) {
            Some(meta) => meta.clone(),
            None => {
                let meta_name = resolve(&meta_path)?;
                if !sysutil::stat(&meta_name).is_ok_and(|info| info.size <= 16 * 1024 * 1024) {
                    bail!("source .meta exceeds import bounds");
                }
                let meta = read_file(&meta_name)?;
                source_files.insert(place(&meta_path), meta_name);
                meta
            }
        };
        let guid = meta_guid(&meta).map_err(|e| wrap(asset, e))?;
        if let Some(previous) = guid_paths.get(&guid) {
            bail!("source GUID is shared by {previous} and {asset}");
        }
        guid_paths.insert(guid.clone(), asset.clone());
        selected.push(ProjectImportSource {
            path: asset.clone(),
            guid,
        });
        let mut folder = logical_dir(asset);
        while folder != "Assets" && folder != "Packages" && folder != "." {
            match resolve(&format!("{folder}.meta")) {
                Ok(folder_meta) => {
                    source_files.insert(place(&format!("{folder}.meta")), folder_meta);
                }
                Err(error) if super::is_not_exist(&error) => {}
                Err(error) => return Err(error),
            }
            if folder == root {
                break;
            }
            folder = logical_dir(folder);
        }
    }
    // The importer reads the files a shader includes: `#include_with_pragmas`
    // declares keywords, and the hash of the includes is part of the result.
    for asset in assets {
        // A build lists the included files among its assets.
        if build.is_some()
            || !asset.starts_with("Assets/")
            || !asset.to_ascii_lowercase().ends_with(".shader")
        {
            continue;
        }
        let (included, _) =
            crate::project::collect_shader_source_requirements(&source_project, asset)?;
        for file in included {
            if !file.path.starts_with("Assets/") || source_files.contains_key(&file.path) {
                continue;
            }
            let meta = format!("{}.meta", file.path);
            if let Ok(meta_name) = resolve_project_import_source(&source_project, &meta) {
                source_files.insert(meta, meta_name);
            }
            source_files.insert(file.path, file.source_path);
        }
    }
    for file in build.iter().flat_map(|build| &build.files) {
        if project_registry_path(file).is_err() || file.ends_with(".meta") {
            bail!("invalid build file {file}");
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = source_files.entry(place(file)) {
            entry.insert(resolve(file)?);
        }
    }
    if let Some(environment) = scripts {
        for script in &environment.scripts {
            if project_registry_path(script).is_err() || script.ends_with(".meta") {
                bail!("invalid importer source {script}");
            }
            let physical = environment.physical(script);
            for name in [physical.clone(), format!("{physical}.meta")] {
                let filename = resolve_script_source(&environment.library, &name)?;
                source_files.entry(name).or_insert(filename);
            }
            let root = project_registry_path(script)?;
            let mut folder = logical_dir(script);
            while folder != root && folder != "." {
                let name = environment.physical(&format!("{folder}.meta"));
                if source_files.contains_key(&name) {
                    break;
                }
                match resolve_script_source(&environment.library, &name) {
                    Ok(filename) => {
                        source_files.insert(name, filename);
                    }
                    Err(error) if super::is_not_exist(&error) => {}
                    Err(error) => return Err(error),
                }
                folder = logical_dir(folder);
            }
        }
        for plugin in &environment.plugins {
            let filename = resolve_script_source(&environment.library, plugin)?;
            source_files.insert(
                format!("Library/ScriptAssemblies/{}", sysutil::base(plugin)),
                filename,
            );
        }
        for assembly in &environment.assemblies {
            let name = format!("Library/ScriptAssemblies/{assembly}.dll");
            let filename = resolve_script_source(&environment.library, &name)?;
            source_files.insert(name, filename);
        }
    }
    let version_path =
        resolve_project_import_source(&source_project, "ProjectSettings/ProjectVersion.txt")?;
    if !sysutil::stat(&version_path).is_ok_and(|info| info.size <= 64 * 1024) {
        bail!("source ProjectVersion.txt exceeds supported bounds");
    }
    let pinned = format!("m_EditorVersion: {ENGINE_VERSION}");
    let matched = read_file(&version_path).is_ok_and(|version| {
        // Each line is compared with surrounding white space trimmed.
        String::from_utf8_lossy(&version)
            .split('\n')
            .any(|line| line.trim() == pinned)
    });
    if !matched {
        bail!("source project Unity version differs from supported worker installation");
    }
    source_files.insert("ProjectSettings/ProjectVersion.txt".into(), version_path);
    // These native settings files contain source configuration, never
    // pre-imported binary state. Importer settings remain in exact .meta.
    for settings in [
        "AudioManager.asset",
        "EditorSettings.asset",
        "ProjectSettings.asset",
        "Physics2DSettings.asset",
        "DynamicsManager.asset",
        "TagManager.asset",
        "TimeManager.asset",
        "GraphicsSettings.asset",
        "QualitySettings.asset",
    ] {
        // Only a build reads the pipeline assets these two name.
        if (settings.starts_with("Graphics") || settings.starts_with("Quality")) && build.is_none()
        {
            continue;
        }
        let logical = format!("ProjectSettings/{settings}");
        match resolve_project_import_source(&source_project, &logical) {
            Ok(filename) => {
                source_files.insert(logical, filename);
            }
            Err(error) if super::is_not_exist(&error) => {}
            Err(error) => return Err(error),
        }
    }
    check(ctx)?;
    mkdir_all(&sysutil::dir(&output), 0o700)?;
    mkdir(&output, 0o700)?;
    let project = sysutil::join(&[&output, "project"]);
    for directory in [
        "Assets",
        "Packages",
        "ProjectSettings",
        "Library",
        "Temp",
        "Logs",
    ] {
        mkdir_all(&sysutil::join(&[&project, directory]), 0o700)?;
    }
    *result = ProjectImportContext {
        schema_version: 1,
        source_project,
        project: project.clone(),
        unity_version: ENGINE_VERSION.into(),
        assets: Some(selected),
        source_hashes: Some(BTreeMap::new()),
        source_bytes: 0,
        registered: extras.registered.clone(),
    };
    for (logical, data) in &extras.files {
        if project_registry_path(logical).is_err()
            && !logical.starts_with("Library/ScriptAssemblies/")
        {
            bail!("invalid context file {logical}");
        }
        let destination = sysutil::join(&[&project, logical]);
        mkdir_all(&sysutil::dir(&destination), 0o700)?;
        write_file(&destination, data, 0o600)?;
    }
    for (logical, filename) in &source_files {
        if extras.files.contains_key(logical) {
            continue;
        }
        let destination = sysutil::join(&[&project, logical]);
        let (hash, size) = copy_project_import_source(ctx, filename, &destination)?;
        if logical == "ProjectSettings/ProjectSettings.asset" {
            // The hash stays that of the source; the context holds the
            // settings with the graphics API of the Player.
            let copied = read_file(&destination)?;
            let held = super::environment::context_player_settings(&copied);
            if held != copied {
                write_file(&destination, &held, 0o600)?;
            }
        }
        result
            .source_hashes
            .as_mut()
            .unwrap()
            .insert(filename.clone(), hash);
        result.source_bytes += size;
    }
    for (logical, (data, meta)) in build.iter().flat_map(|build| &build.generated) {
        if project_registry_path(logical).is_err() || logical.ends_with(".meta") {
            bail!("invalid generated asset {logical}");
        }
        let filename = sysutil::join(&[&project, logical]);
        mkdir_all(&sysutil::dir(&filename), 0o700)?;
        write_file(&filename, data, 0o600)?;
        write_file(&format!("{filename}.meta"), meta, 0o600)?;
    }
    // The database names every package root, and the worker looks each up.
    for directory in scripts
        .iter()
        .flat_map(|environment| environment.roots.values())
    {
        mkdir_all(&sysutil::join(&[&project, directory]), 0o700)?;
    }
    write_file(
        &sysutil::join(&[&project, "Packages", "manifest.json"]),
        b"{\"dependencies\":{}}\n",
        0o600,
    )?;
    let mut encoded = jsondoc::marshal_indent(result).map_err(|e| anyhow!(e))?;
    encoded.push(b'\n');
    write_file(
        &sysutil::join(&[&output, "project-context.json"]),
        &encoded,
        0o600,
    )?;
    Ok(())
}

/// Files are checked in path order.
pub fn verify_project_import_sources(ctx: &Context, prepared: &ProjectImportContext) -> Result<()> {
    let mut buffer = vec![0u8; 128 * 1024];
    for (filename, expected) in prepared.source_hashes.iter().flatten() {
        check(ctx)?;
        let mut file = File::open(filename)
            .map_err(|e| os_error("open", filename, &e))?
            .take(SOURCE_LIMIT as u64 + 1);
        let mut hasher = Sha256::new();
        let read = loop {
            match file.read(&mut buffer) {
                Ok(0) => break None,
                Ok(count) => hasher.update(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => break Some(os_error("read", filename, &error)),
            }
        };
        if let Some(error) = read {
            return Err(error);
        }
        if hex(&hasher.finalize()) != *expected {
            bail!("source project file changed during import: {filename}");
        }
    }
    check(ctx)
}
