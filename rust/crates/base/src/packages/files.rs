use super::git::commit_pattern;
use super::semver::{parse_version, valid_package_name};
use super::{Context, PackageManifest, Snapshot};
use anyhow::{Context as _, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_JSON: i64 = 16 * 1024 * 1024;

/// Names the failing operation and path while keeping the `io::Error` reachable.
pub(crate) fn path_error(
    op: &'static str,
    path: impl AsRef<Path>,
) -> impl FnOnce(io::Error) -> anyhow::Error {
    move |source| anyhow::Error::new(source).context(format!("{op} {}", path.as_ref().display()))
}
/// Whether a file operation failed because its path does not exist.
pub(crate) fn is_not_exist(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}
/// Resolves `.` and `..` lexically, without consulting the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match clean.components().next_back() {
                Some(Component::Normal(_)) => {
                    clean.pop();
                }
                Some(Component::RootDir) => {}
                _ => clean.push(".."),
            },
            other => clean.push(other),
        }
    }
    clean
}
/// Joins path components and normalizes the result lexically. Persisted package paths
/// are compared as text, so every path the resolver builds goes through here.
pub(crate) fn join(parts: &[&str]) -> String {
    let path: PathBuf = parts.iter().filter(|part| !part.is_empty()).collect();
    normalize(&path).to_string_lossy().into_owned()
}
pub(crate) fn parent(path: &str) -> String {
    let parent = Path::new(path).parent().unwrap_or(Path::new(""));
    parent.to_string_lossy().into_owned()
}
pub(crate) fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}
/// The normalized absolute form of `path`; symlinks are left alone.
pub(crate) fn absolute(path: &str) -> Result<String> {
    utf8(&normalize(&std::path::absolute(path)?))
}
/// Lexical containment (no symlink resolution); a root contains itself.
pub(crate) fn inside(root: &str, path: &str) -> bool {
    let (root, path) = (Path::new(root), Path::new(path));
    root.is_absolute() == path.is_absolute()
        && !path.as_os_str().is_empty()
        && normalize(path).starts_with(normalize(root))
}
/// Whether an archive or repository path is a plain relative path: no empty, `.` or `..`
/// component and nothing a foreign platform would read as a separator or drive.
pub(crate) fn safe_relative(path: &[u8]) -> bool {
    !path.contains(&b'\\')
        && !path.contains(&b':')
        && path
            .split(|&byte| byte == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
}
pub(crate) fn lstat(path: &str) -> Result<fs::Metadata> {
    fs::symlink_metadata(path).map_err(path_error("lstat", path))
}
pub(crate) fn stat(path: &str) -> Result<fs::Metadata> {
    fs::metadata(path).map_err(path_error("stat", path))
}
pub(crate) fn open(path: impl AsRef<Path>) -> Result<fs::File> {
    fs::File::open(path.as_ref()).map_err(path_error("open", path.as_ref()))
}
pub(crate) fn mkdir_all(path: impl AsRef<Path>) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path.as_ref())
        .map_err(path_error("mkdir", path.as_ref()))
}
pub(crate) fn utf8(path: &Path) -> Result<String> {
    match path.to_str() {
        Some(text) => Ok(text.into()),
        None => bail!("non-UTF-8 path unsupported: {}", path.to_string_lossy()),
    }
}
/// Directory entries sorted by name, with their file types.
pub(crate) fn read_dir(path: impl AsRef<Path>) -> Result<Vec<(OsString, fs::FileType)>> {
    let path = path.as_ref();
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(path_error("open", path))? {
        let entry = entry.map_err(path_error("readdirent", path))?;
        let kind = entry
            .file_type()
            .map_err(path_error("lstat", entry.path()))?;
        entries.push((entry.file_name(), kind));
    }
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(entries)
}
pub(crate) enum Walk {
    Continue,
    SkipDir,
}
pub(crate) struct WalkEntry<'a> {
    pub path: &'a Path,
    pub name: &'a OsStr,
    pub is_root: bool,
    kind: fs::FileType,
}
impl WalkEntry<'_> {
    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }
    pub fn is_symlink(&self) -> bool {
        self.kind.is_symlink()
    }
    pub fn info(&self) -> Result<fs::Metadata> {
        fs::symlink_metadata(self.path).map_err(path_error("lstat", self.path))
    }
}
/// Walks a tree in lexical pre-order without following symlinks; the root is visited
/// first and the first visitor error stops the walk.
pub(crate) fn walk_dir(
    root: impl AsRef<Path>,
    visit: &mut dyn FnMut(&WalkEntry) -> Result<Walk>,
) -> Result<()> {
    let root = root.as_ref();
    let info = fs::symlink_metadata(root).map_err(path_error("lstat", root))?;
    let name = root.file_name().unwrap_or(root.as_os_str()).to_os_string();
    walk_entry(root, &name, info.file_type(), true, visit)
}
fn walk_entry(
    path: &Path,
    name: &OsStr,
    kind: fs::FileType,
    is_root: bool,
    visit: &mut dyn FnMut(&WalkEntry) -> Result<Walk>,
) -> Result<()> {
    let entry = WalkEntry {
        path,
        name,
        is_root,
        kind,
    };
    if matches!(visit(&entry)?, Walk::SkipDir) || !kind.is_dir() {
        return Ok(());
    }
    for (child, child_kind) in read_dir(path)? {
        walk_entry(&path.join(&child), &child, child_kind, false, visit)?;
    }
    Ok(())
}

pub(crate) fn hash_bytes(data: &[u8]) -> String {
    crate::sha256(data)
}
/// The physical path: absolute, normalized and free of symlinks.
pub(crate) fn canonical(path: &str) -> Result<String> {
    let absolute = absolute(path)?;
    let physical = fs::canonicalize(&absolute).map_err(path_error("lstat", &absolute))?;
    utf8(&physical)
}
pub(crate) fn bounded_read(path: &str, max: i64) -> Result<Vec<u8>> {
    let info = lstat(path)?;
    if !info.file_type().is_file() || info.len() as i64 > max {
        bail!("not a bounded regular file: {path}");
    }
    let mut data = Vec::new();
    open(path)?
        .take(max as u64 + 1)
        .read_to_end(&mut data)
        .map_err(path_error("read", path))?;
    if data.len() as i64 > max {
        bail!("file grew beyond read bound");
    }
    Ok(data)
}
/// Decodes a bounded JSON file and returns it with the SHA-256 of its raw bytes.
pub(crate) fn read_json<T: DeserializeOwned>(path: &str) -> Result<(T, String)> {
    let data = bounded_read(path, MAX_JSON)?;
    let value = serde_json::from_slice(&data).with_context(|| path.to_string())?;
    Ok((value, hash_bytes(&data)))
}
pub(crate) fn tree_hash(
    ctx: &Context,
    root: &str,
    max_bytes: i64,
    max_files: i64,
) -> Result<String> {
    package_tree_hash(ctx, root, max_bytes, max_files, false)
}
/// UPM materializes builtin packages in PackageCache, adding `_fingerprint` only to
/// package.json. Canonicalizing that one JSON property lets us verify all source/resource
/// bytes against the selected engine without trusting folders.
pub(crate) fn engine_comparable_hash(
    ctx: &Context,
    root: &str,
    max_bytes: i64,
    max_files: i64,
) -> Result<String> {
    package_tree_hash(ctx, root, max_bytes, max_files, true)
}
fn package_tree_hash(
    ctx: &Context,
    root: &str,
    max_bytes: i64,
    max_files: i64,
    normalize_manifest: bool,
) -> Result<String> {
    let mut names: Vec<(Vec<u8>, PathBuf)> = Vec::new();
    let mut total: i64 = 0;
    let root_path = Path::new(root);
    walk_dir(root_path, &mut |entry| {
        ctx.err()?;
        if entry.is_root {
            return Ok(Walk::Continue);
        }
        if entry.name == OsStr::new(".git") && entry.is_dir() {
            return Ok(Walk::SkipDir);
        }
        if entry.is_symlink() {
            bail!("package symlink unsupported: {}", entry.path.display());
        }
        if entry.is_dir() {
            return Ok(Walk::Continue);
        }
        let info = entry.info()?;
        if !info.file_type().is_file() {
            bail!("nonregular package entry: {}", entry.path.display());
        }
        total += info.len() as i64;
        if total > max_bytes {
            bail!("package tree exceeds byte bound");
        }
        let relative = entry.path.strip_prefix(root_path).unwrap_or(entry.path);
        names.push((relative.as_os_str().as_bytes().to_vec(), entry.path.into()));
        if names.len() as i64 > max_files {
            bail!("package tree exceeds file bound");
        }
        Ok(Walk::Continue)
    })?;
    names.sort();
    let mut hash = Sha256::new();
    let mut actual_total: i64 = 0;
    for (relative, path) in &names {
        ctx.err()?;
        if normalize_manifest && relative == b"package.json" {
            let data = bounded_read(&utf8(path)?, MAX_JSON)?;
            let mut manifest: BTreeMap<String, serde_json::Value> =
                serde_json::from_slice(&data).with_context(|| path.display().to_string())?;
            manifest.remove("_fingerprint");
            let data = serde_json::to_vec(&manifest)?;
            hash.update(relative);
            hash.update(b"\0");
            hash.update(hash_bytes(&data).as_bytes());
            hash.update(b"\n");
            continue;
        }
        let mut file = open(path)?.take((max_bytes - actual_total + 1) as u64);
        let mut file_hash = Sha256::new();
        let copied = io::copy(&mut file, &mut file_hash).map_err(path_error("read", path))?;
        actual_total += copied as i64;
        if actual_total > max_bytes {
            bail!("package file grew beyond byte bound");
        }
        hash.update(relative);
        hash.update(b"\0");
        hash.update(crate::hex(&file_hash.finalize()).as_bytes());
        hash.update(b"\n");
    }
    Ok(crate::hex(&hash.finalize()))
}
pub(crate) fn manifest_at(root: &str) -> Result<(PackageManifest, String)> {
    let (manifest, hash): (PackageManifest, _) = read_json(&join(&[root, "package.json"]))?;
    if !valid_package_name(&manifest.name) {
        bail!("invalid package name {:?}", manifest.name);
    }
    parse_version(&manifest.version)?;
    for (name, value) in &manifest.dependencies {
        if !valid_package_name(name) {
            bail!("invalid dependency {name:?}");
        }
        if value.is_empty() {
            bail!("empty package dependency {name}");
        }
    }
    Ok((manifest, hash))
}
pub(crate) fn ensure_cache(project: &str, requested: &str) -> Result<String> {
    let owned = join(&[project, "Library", "UnityMicroEditor", "packages"]);
    let mut path = owned.clone();
    if !requested.is_empty() {
        path = absolute(requested)?;
        if !inside(&owned, &path) {
            bail!("package cache must be inside owned Library/UnityMicroEditor/packages");
        }
    }
    // Refuse symlinks in existing ancestors before creating directories.
    let mut current = path.clone();
    while inside(project, &current) && current != project {
        match lstat(&current) {
            Ok(info) if info.file_type().is_symlink() => {
                bail!("cache ancestor is a symlink: {current}")
            }
            Ok(_) => {}
            Err(error) if is_not_exist(&error) => {}
            Err(error) => return Err(error),
        }
        current = parent(&current);
    }
    mkdir_all(&path)?;
    Ok(path)
}
/// The on-disk form of the resolver's own JSON files: indented, with a final newline.
pub(crate) fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    Ok(data)
}
pub(crate) fn atomic_json<T: Serialize>(path: &str, value: &T) -> Result<()> {
    atomic_bytes(path, &json_bytes(value)?)
}
pub(crate) fn atomic_bytes(path: &str, data: &[u8]) -> Result<()> {
    let parent = parent(path);
    mkdir_all(&parent)?;
    let mut file = tempfile::Builder::new()
        .prefix(".publish-")
        .rand_bytes(10)
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(&parent)
        .map_err(path_error("open", &parent))?;
    file.write_all(data)
        .map_err(path_error("write", file.path()))?;
    file.as_file()
        .sync_all()
        .map_err(path_error("sync", file.path()))?;
    file.persist(path)
        .map_err(|error| path_error("rename", path)(error.error))?;
    Ok(())
}
fn hash_input(path: &str, expected: &str) -> Result<()> {
    const LIMIT: u64 = 2 << 30;
    let info = lstat(path)?;
    if !info.file_type().is_file() || info.len() > LIMIT {
        bail!("package input is not bounded regular file: {path}");
    }
    let mut hash = Sha256::new();
    let copied =
        io::copy(&mut open(path)?.take(LIMIT + 1), &mut hash).map_err(path_error("read", path))?;
    if copied > LIMIT {
        bail!("package input grew beyond bounds");
    }
    if crate::hex(&hash.finalize()) != expected {
        bail!("package input changed: {path}");
    }
    Ok(())
}
pub(crate) fn validate_snapshot(ctx: &Context, s: &Snapshot) -> Result<()> {
    if s.schema_version != 1 || !is_absolute(&s.project) {
        bail!("invalid package snapshot schema/project");
    }
    // Embedded directories can be added without changing the project manifest. Every
    // embedded package is a root dependency, so an old snapshot must reject an added,
    // removed, or retargeted root rather than omit it from compilation.
    let mut expected_embedded: BTreeMap<&str, &str> = BTreeMap::new();
    for p in s.packages.iter() {
        if !p.commit_path.is_empty() {
            if p.source != "git" || !commit_pattern().is_match(&p.revision) {
                bail!("invalid materialized Git commit proof");
            }
            if tree_hash(ctx, &p.commit_path, 2 << 30, 200000)? != p.commit_content_hash {
                bail!("verified Git commit content changed: {}", p.name);
            }
        }
        if !p.profile_path.is_empty() {
            if canonical(&p.profile_path).ok().as_deref() != Some(p.profile_path.as_str()) {
                bail!("builtin profile path changed: {}", p.name);
            }
            if tree_hash(ctx, &p.profile_path, 2 << 30, 200000)? != p.profile_content_hash {
                bail!("builtin engine profile content changed: {}", p.name);
            }
        }
        if p.source == "embedded" {
            expected_embedded.insert(&p.name, &p.path);
        }
    }
    let packages = join(&[&s.project, "Packages"]);
    for (name, kind) in read_dir(&packages)? {
        if !kind.is_dir() {
            continue;
        }
        let root = utf8(&Path::new(&packages).join(&name))?;
        let manifest = match manifest_at(&root) {
            Ok((manifest, _)) => manifest,
            Err(error) if is_not_exist(&error) => continue,
            Err(error) => return Err(error),
        };
        let physical = canonical(&root)?;
        if expected_embedded
            .get(manifest.name.as_str())
            .copied()
            .unwrap_or("")
            != physical
        {
            bail!("embedded package inventory changed: {}", manifest.name);
        }
        expected_embedded.remove(manifest.name.as_str());
    }
    if !expected_embedded.is_empty() {
        bail!("embedded package removed");
    }
    for (path, expected) in s.inputs.iter() {
        ctx.err()?;
        if expected == "absent" {
            match lstat(path) {
                Err(error) if is_not_exist(&error) => continue,
                _ => bail!("previously absent package input changed: {path}"),
            }
        }
        hash_input(path, expected)?;
    }
    for p in s.packages.iter() {
        if p.source == "local" && p.proof != "verified_local_archive" {
            let Some(root) = p.requested.strip_prefix("file:") else {
                bail!("local package request is not an explicit file path");
            };
            let root = if is_absolute(root) {
                root.to_string()
            } else {
                join(&[&s.project, "Packages", root])
            };
            if canonical(&root)? != p.path {
                bail!("local package path changed: {}", p.name);
            }
        }
        let (manifest, manifest_hash) = manifest_at(&p.path)?;
        if manifest.name != p.name
            || manifest.version != p.version
            || manifest_hash != p.manifest_hash
            || p.logical_root != format!("Packages/{}", p.name)
        {
            bail!("resolved package identity changed: {}", p.name);
        }
        if tree_hash(ctx, &p.path, 2 << 30, 200000)? != p.content_hash {
            bail!("resolved package content changed: {}", p.name);
        }
    }
    Ok(())
}
pub(crate) fn cache_size_within(ctx: &Context, cache: &str, limit: i64) -> Result<()> {
    let mut total: i64 = 0;
    walk_dir(cache, &mut |entry| {
        ctx.err()?;
        if entry.is_symlink() {
            bail!("package cache symlink unsupported");
        }
        if !entry.is_dir() {
            total += entry.info()?.len() as i64;
            if total > limit {
                bail!("package cache exceeds resource budget");
            }
        }
        Ok(Walk::Continue)
    })
}
