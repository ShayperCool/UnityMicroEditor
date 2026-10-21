//! Session state, source resolution, the GUID index and revision publication.
use super::{
    lexpath::{self, rune_lower},
    types::{Document, NativeReplacement, PrefabOverrideDiagnostic, nul},
    value::{self, Map, Value, array, get, number, object, str},
};
use crate::cache::{CacheStats, Identity, SourceCache};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub project: String,
    pub manifest: String,
    pub native_catalog: String,
    /// Selects authored YAML while reusing the validated native host/catalog.
    pub scene: String,
    /// Runtime-addressed prefabs absent from YAML references.
    pub extra_assets: Vec<String>,
    /// Types discovered by a new C# compilation.
    pub script_types: BTreeMap<String, String>,
}

/// Prevents publishing a scene against stale native content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedsImportError {
    pub path: String,
    /// Other sources the same pass found it needs imported, so that an
    /// importer can take them together.
    pub also: Vec<String>,
}
impl std::fmt::Display for NeedsImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "native content changed; refresh importer catalog: {}",
            self.path
        )
    }
}
impl std::error::Error for NeedsImportError {}
/// A runtime-addressed asset a preparation failed in, and why.
#[derive(Debug)]
pub struct RefusedPrefab {
    pub path: String,
    cause: anyhow::Error,
}
impl RefusedPrefab {
    pub(crate) fn new(path: &str, cause: anyhow::Error) -> Self {
        Self {
            path: path.into(),
            cause,
        }
    }
}
impl std::fmt::Display for RefusedPrefab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime asset {}", self.path)
    }
}
impl std::error::Error for RefusedPrefab {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(crate) fn needs_import(path: &str) -> anyhow::Error {
    NeedsImportError {
        path: path.into(),
        also: Vec::new(),
    }
    .into()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SourceRoot {
    #[serde(deserialize_with = "nul")]
    pub path: String,
    #[serde(deserialize_with = "nul")]
    pub source: String,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Dependency {
    #[serde(deserialize_with = "nul")]
    pub path: String,
    #[serde(deserialize_with = "nul")]
    pub sha256: String,
    #[serde(deserialize_with = "nul")]
    pub bytes: i64,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub source_path: String,
}
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelPrefab {
    #[serde(deserialize_with = "nul")]
    pub guid: String,
    #[serde(deserialize_with = "nul")]
    pub path: String,
    #[serde(deserialize_with = "nul")]
    pub root_file_id: String,
    #[serde(deserialize_with = "nul")]
    pub objects: Vec<Document>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NativeDiscoveryPack {
    #[serde(deserialize_with = "nul")]
    pub bundle_path: String,
    #[serde(deserialize_with = "nul")]
    pub sha256: String,
    #[serde(deserialize_with = "nul")]
    pub bytes: i64,
    #[serde(deserialize_with = "nul")]
    pub registry_asset: String,
}
fn maps<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Map>, D::Error> {
    let items = Option::<Vec<Option<Map>>>::deserialize(deserializer)?.unwrap_or_default();
    Ok(items.into_iter().map(Option::unwrap_or_default).collect())
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Catalog {
    #[serde(deserialize_with = "nul")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nul")]
    pub engine_version: String,
    #[serde(deserialize_with = "nul")]
    pub project: String,
    #[serde(deserialize_with = "nul")]
    pub backend: String,
    #[serde(deserialize_with = "nul")]
    pub success: bool,
    #[serde(deserialize_with = "nul")]
    pub contains_scenes: bool,
    #[serde(deserialize_with = "nul")]
    pub scene_loader: String,
    #[serde(deserialize_with = "nul")]
    pub cooked_registry_verified: bool,
    #[serde(deserialize_with = "nul")]
    pub source_roots: Vec<SourceRoot>,
    #[serde(deserialize_with = "nul")]
    pub model_prefabs: Vec<ModelPrefab>,
    #[serde(deserialize_with = "maps")]
    pub assets: Vec<Map>,
    #[serde(deserialize_with = "nul")]
    pub packs: Vec<NativeDiscoveryPack>,
    #[serde(deserialize_with = "maps")]
    pub excluded_runtime_assets: Vec<Map>,
    #[serde(deserialize_with = "maps")]
    pub model_animation_clips: Vec<Map>,
    #[serde(deserialize_with = "maps")]
    pub prefab_observations: Vec<Map>,
    #[serde(deserialize_with = "maps")]
    pub ignored_prefab_overrides: Vec<Map>,
    #[serde(deserialize_with = "maps")]
    pub deferred_bindings: Vec<Map>,
    #[serde(deserialize_with = "nul")]
    pub source_dependencies: Vec<Dependency>,
}

pub struct YamlRecord {
    pub digest: String,
    pub documents: Arc<Vec<Document>>,
}

/// The result of one published revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prepared {
    pub manifest: String,
    pub revision: String,
    pub project: String,
    pub scene: String,
    pub objects: usize,
    pub assets: usize,
    pub elapsed_seconds: f64,
    pub cache: CacheStats,
}
impl Prepared {
    /// The published JSON text, with the shortest round-trip float layout.
    pub fn to_json_text(&self) -> Result<String> {
        let cache = value::encode_json(&self.cache)?;
        let fields = value::map([
            ("manifest", self.manifest.as_str().into()),
            ("revision", self.revision.as_str().into()),
            ("project", self.project.as_str().into()),
            ("scene", self.scene.as_str().into()),
        ]);
        let mut out = String::from("{");
        for key in ["manifest", "revision", "project", "scene"] {
            value::encode_string(key, &mut out);
            out.push(':');
            fields[key].encode(&mut out)?;
            out.push(',');
        }
        out.push_str(&format!(
            "\"objectCount\":{},\"assetCount\":{},\"prepareSeconds\":{},\"cache\":{cache}}}",
            self.objects,
            self.assets,
            value::json_float(self.elapsed_seconds)?
        ));
        Ok(out)
    }
}
impl Serialize for Prepared {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeStruct};
        let seconds: serde_json::Number =
            serde_json::from_str(&value::json_float(self.elapsed_seconds).map_err(Error::custom)?)
                .map_err(Error::custom)?;
        let mut state = serializer.serialize_struct("Prepared", 8)?;
        state.serialize_field("manifest", &self.manifest)?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("project", &self.project)?;
        state.serialize_field("scene", &self.scene)?;
        state.serialize_field("objectCount", &self.objects)?;
        state.serialize_field("assetCount", &self.assets)?;
        state.serialize_field("prepareSeconds", &seconds)?;
        state.serialize_field("cache", &self.cache)?;
        state.end()
    }
}

/// Owns caches for one project. Native content and managed assembly bindings
/// are inherited from a validated host preparation, never rebuilt here.
pub struct Session {
    inner: Mutex<Inner>,
}

pub struct Inner {
    /// The revision this session published last and the text of its
    /// manifest: a preparation whose manifest would equal it but for the
    /// revision id is that revision again, and nothing is written.
    pub published: Option<(String, Prepared)>,
    pub project: String,
    pub baseline: Map,
    pub catalog: Catalog,
    pub has_catalog: bool,
    pub cache: SourceCache,
    pub documents: HashMap<String, YamlRecord>,
    pub by_guid: HashMap<String, String>,
    pub by_path: HashMap<String, String>,
    pub models: HashMap<String, Arc<Vec<Document>>>,
    pub assets: BTreeMap<String, Map>,
    pub script_types: HashMap<String, String>,
    pub dependencies: BTreeMap<String, Dependency>,
    pub critical: HashMap<String, String>,
    pub prefab_observations: HashMap<String, Map>,
    pub ignored_overrides: HashSet<String>,
    pub index_dirty: bool,
    pub explicit_sources: HashMap<String, String>,
    pub native_overlays: BTreeMap<String, NativeReplacement>,
    /// Content derived from part of a source, such as the baked clips of a
    /// model, published under a GUID that no source owns.
    pub derived_native: BTreeMap<String, super::NativeContentFile>,
    /// The scene the worker wrote last, for the sources it was made of.
    pub native_scene: Option<super::NativeScene>,
    /// Catalog ScriptableObject sources edited since the catalog was built,
    /// each with whether it is loaded from its authored text instead.
    pub authored_scriptables: HashMap<String, bool>,
    pub extra_assets: Vec<String>,
    pub ignored_diagnostics: Vec<PrefabOverrideDiagnostic>,
}

pub(crate) fn validate_authored_path(path: &str, extension: &str) -> Result<()> {
    if !path.starts_with("Assets/")
        || lexpath::clean(path) != path
        || lexpath::ext(path) != extension
    {
        bail!("authored path must be a canonical Assets/{extension} source: {path}");
    }
    Ok(())
}
/// Whether the Editor leaves a file or folder of this name out of its asset
/// database: a name that starts with `.` or ends with `~`, `cvs`, or a
/// `.tmp` file.
pub(crate) fn unity_hidden_name(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || name.eq_ignore_ascii_case("cvs")
        || name.to_ascii_lowercase().ends_with(".tmp")
}

pub(crate) fn asset_key(guid: &str, id: &str) -> String {
    format!("{guid}/{id}")
}
pub(crate) fn raw_kind(kind: &str) -> bool {
    matches!(
        kind,
        "prefab"
            | "script"
            | "scriptableObject"
            | "mesh"
            | "authoredMaterial"
            | "texture"
            | "cubemap"
    )
}
pub(crate) fn override_key(
    path: &str,
    instance: &str,
    guid: &str,
    id: &str,
    property: &str,
) -> String {
    [path, instance, guid, id, property].join("\0")
}
/// True when the path does not exist.
fn missing(path: &str) -> bool {
    matches!(fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}
pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    let mut id = vec![0u8; bytes];
    fs::File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(crate::hex(&id))
}
pub(crate) fn write_new_file(path: &str, payload: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)?;
    let written = file.write_all(payload);
    let synced = file.sync_all();
    written?;
    synced?;
    Ok(())
}

/// A shallow manifest overlay: changed top-level values shadow the baseline.
pub(crate) struct Manifest<'a> {
    pub base: &'a Map,
    pub over: Map,
}
impl Manifest<'_> {
    pub fn encode(&self) -> Result<String> {
        let mut merged: BTreeMap<&str, &Value> =
            self.base.iter().map(|(k, v)| (k.as_str(), v)).collect();
        merged.extend(self.over.iter().map(|(k, v)| (k.as_str(), v)));
        let mut out = String::from("{");
        for (index, (key, value)) in merged.into_iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            value::encode_string(key, &mut out);
            out.push(':');
            value.encode(&mut out)?;
        }
        out.push('}');
        Ok(out)
    }
}

impl Inner {
    fn empty(project: String) -> Self {
        Self {
            published: None,
            project,
            baseline: Map::new(),
            catalog: Catalog::default(),
            has_catalog: false,
            cache: SourceCache::default(),
            documents: HashMap::new(),
            by_guid: HashMap::new(),
            by_path: HashMap::new(),
            models: HashMap::new(),
            assets: BTreeMap::new(),
            script_types: HashMap::new(),
            dependencies: BTreeMap::new(),
            critical: HashMap::new(),
            prefab_observations: HashMap::new(),
            ignored_overrides: HashSet::new(),
            index_dirty: true,
            explicit_sources: HashMap::new(),
            native_overlays: BTreeMap::new(),
            derived_native: BTreeMap::new(),
            native_scene: None,
            authored_scriptables: HashMap::new(),
            extra_assets: Vec::new(),
            ignored_diagnostics: Vec::new(),
        }
    }

    pub fn guid_of(&self, path: &str) -> &str {
        self.by_path.get(path).map_or("", String::as_str)
    }
    pub fn path_of(&self, guid: &str) -> &str {
        self.by_guid.get(guid).map_or("", String::as_str)
    }
    pub fn critical_of(&self, path: &str) -> &str {
        self.critical.get(path).map_or("", String::as_str)
    }
    pub fn baseline_text(&self, key: &str) -> String {
        str(get(&self.baseline, key)).into_owned()
    }

    /// Reads through the stat-validated cache; text sources keep their bytes.
    pub fn read(&mut self, path: &str, text: bool) -> Result<(Option<Arc<[u8]>>, String)> {
        self.cache.read(Path::new(path), text)
    }
    pub fn read_text(&mut self, path: &str) -> Result<(Arc<[u8]>, String)> {
        let (content, digest) = self.read(path, true)?;
        Ok((content.unwrap_or_else(|| Arc::from(Vec::new())), digest))
    }
    pub fn parse(&mut self, path: &str) -> Result<Arc<Vec<Document>>> {
        let (content, digest) = self.read_text(path)?;
        if let Some(old) = self.documents.get(path)
            && old.digest == digest
        {
            self.cache.stats.yaml_hits += 1;
            return Ok(old.documents.clone());
        }
        let documents =
            super::parse_unity_yaml(&content).map_err(|e| e.context(path.to_string()))?;
        let documents = Arc::new(documents);
        self.documents.insert(
            path.into(),
            YamlRecord {
                digest,
                documents: documents.clone(),
            },
        );
        self.cache.stats.yaml_parses += 1;
        Ok(documents)
    }

    pub fn source(&self, logical: &str) -> Result<String> {
        if lexpath::is_abs(logical) || logical.starts_with("../") || logical.contains("/../") {
            bail!("invalid logical source path {logical:?}");
        }
        let path = lexpath::join(&[&self.project, logical]);
        if fs::metadata(&path).is_ok() {
            return self.checked_path(&path, &self.project);
        }
        if let Some(physical) = self.explicit_sources.get(logical)
            && !physical.is_empty()
        {
            for root in &self.catalog.source_roots {
                if lexpath::rel(&root.source, physical).is_ok_and(|r| lexpath::inside(&r)) {
                    return self.checked_path(physical, &root.source);
                }
            }
        }
        for root in &self.catalog.source_roots {
            let prefix = format!("{}/", root.path);
            if logical == root.path || logical.starts_with(&prefix) {
                let rest = logical.strip_prefix(&prefix).unwrap_or(logical);
                return self.checked_path(&lexpath::join(&[&root.source, rest]), &root.source);
            }
        }
        bail!("source not found: {logical}")
    }

    pub fn checked_path(&self, path: &str, root: &str) -> Result<String> {
        let canonical =
            lexpath::abs(&lexpath::eval_symlinks(path).map_err(|e| path_error(path, e))?)?;
        let base = lexpath::abs(&lexpath::eval_symlinks(root).map_err(|e| path_error(root, e))?)?;
        if !lexpath::rel(&base, &canonical).is_ok_and(|r| lexpath::inside(&r)) {
            bail!("source escapes declared root: {path}");
        }
        let library = lexpath::join(&[&self.project, "Library"]);
        if canonical == library || canonical.starts_with(&format!("{library}/")) {
            let mut package_source = false;
            for declared in &self.catalog.source_roots {
                if !declared.path.starts_with("Packages/")
                    || lexpath::eval_symlinks(&declared.source).ok().as_deref() != Some(&base)
                {
                    continue;
                }
                for permitted in [
                    lexpath::join(&[&library, "PackageCache"]),
                    lexpath::join(&[&library, "UnityMicroEditor/package-sources"]),
                ] {
                    if lexpath::rel(&permitted, &base)
                        .is_ok_and(|r| r != "." && lexpath::inside(&r))
                    {
                        package_source = true;
                    }
                }
            }
            if !package_source {
                bail!("project Library is not a declared package source: {path}");
            }
        }
        Ok(canonical)
    }

    pub fn index_guids(&mut self) -> Result<()> {
        if !self.index_dirty {
            return Ok(());
        }
        let (mut by_guid, mut by_path) = (HashMap::new(), HashMap::new());
        let mut roots = vec![
            SourceRoot {
                path: "Assets".into(),
                source: lexpath::join(&[&self.project, "Assets"]),
            },
            SourceRoot {
                path: "Packages".into(),
                source: lexpath::join(&[&self.project, "Packages"]),
            },
        ];
        for root in &self.catalog.source_roots {
            if missing(&lexpath::join(&[&self.project, &root.path])) {
                roots.push(root.clone());
            }
        }
        for root in roots {
            if missing(&root.source) {
                continue;
            }
            let info = fs::symlink_metadata(&root.source)
                .map_err(|e| lexpath::os_error("lstat", &root.source, e))?;
            self.index_entry(
                &root,
                &root.source,
                info.file_type(),
                &mut by_guid,
                &mut by_path,
            )?;
        }
        self.by_guid = by_guid;
        self.by_path = by_path;
        self.index_dirty = false;
        Ok(())
    }

    fn index_entry(
        &mut self,
        root: &SourceRoot,
        path: &str,
        kind: fs::FileType,
        by_guid: &mut HashMap<String, String>,
        by_path: &mut HashMap<String, String>,
    ) -> Result<()> {
        if kind.is_symlink() {
            bail!("symlinked raw metadata root is unsupported: {path}");
        }
        if kind.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(path)
                .map_err(|e| lexpath::os_error("open", path, e))?
                .collect::<std::io::Result<_>>()?;
            entries.sort_by_key(fs::DirEntry::file_name);
            for entry in entries {
                // Logical paths are UTF-8 text. other names are raw bytes;
                // such entries cannot be addressed here and are left unindexed.
                let Ok(name) = entry.file_name().into_string() else {
                    if entry.file_type()?.is_symlink() {
                        bail!(
                            "symlinked raw metadata root is unsupported: {}",
                            entry.path().display()
                        );
                    }
                    continue;
                };
                // The Editor imports nothing under a hidden name, such as a
                // package's `Samples~`.
                if unity_hidden_name(&name) {
                    continue;
                }
                let child = format!("{}/{name}", path.trim_end_matches('/'));
                self.index_entry(root, &child, entry.file_type()?, by_guid, by_path)?;
            }
            return Ok(());
        }
        let Some(source) = path.strip_suffix(".meta") else {
            return Ok(());
        };
        match fs::metadata(source) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(path_error(source, e)),
            Ok(info) if info.is_dir() => return Ok(()),
            Ok(_) => {}
        }
        let (content, _) = self.read_text(path)?;
        let Some(guid) = crate::yaml::metadata_guid(&content) else {
            return Ok(());
        };
        let logical = format!("{}/{}", root.path, lexpath::rel(&root.source, source)?);
        if let Some(previous) = by_guid.get(&guid)
            && *previous != logical
        {
            bail!("duplicate GUID {guid}: {previous} and {logical}");
        }
        by_guid.insert(guid.clone(), logical.clone());
        by_path.insert(logical, guid);
        Ok(())
    }

    pub fn depend(&mut self, logical: &str) -> Result<()> {
        for name in [logical.to_string(), format!("{logical}.meta")] {
            let path = match self.source(&name) {
                Ok(path) => path,
                Err(error) => {
                    if name != logical && self.guid_of(logical).is_empty() {
                        continue;
                    }
                    return Err(error);
                }
            };
            let text = matches!(
                lexpath::lower_ext(&path).as_str(),
                ".unity" | ".prefab" | ".mat" | ".asset" | ".cs" | ".meta" | ".txt"
            );
            let (_, hash) = self.read(&path, text)?;
            let bytes = fs::metadata(&path).map_err(|e| path_error(&path, e))?.len() as i64;
            let source_path = if path.starts_with(&format!("{}/", self.project)) {
                String::new()
            } else {
                path
            };
            let stale = self.authored_scriptables.get(logical) != Some(&true)
                && self
                    .critical
                    .get(&name)
                    .is_some_and(|expected| !expected.is_empty() && *expected != hash);
            self.dependencies.insert(
                name.clone(),
                Dependency {
                    path: name.clone(),
                    sha256: hash,
                    bytes,
                    source_path,
                },
            );
            if stale {
                return Err(needs_import(&name));
            }
        }
        Ok(())
    }

    pub fn validate_observed(&self, message: &str) -> Result<()> {
        for (path, expected) in &self.cache.observed {
            if !Identity::read(path).is_ok_and(|actual| actual == *expected) {
                bail!("{message}: {}", path.display());
            }
        }
        Ok(())
    }

    fn prepare(&mut self, output_root: &str) -> Result<Prepared> {
        let started = Instant::now();
        self.cache.stats = CacheStats::default();
        self.cache.observed.clear();
        self.dependencies.clear();
        self.ignored_diagnostics.clear();
        self.validate_retained_builtin_shaders()?;
        self.index_guids()?;
        // Validate the authored closure once; later attempts read only changed files.
        let closure: Vec<String> = array(get(&self.baseline, "sourceDependencies"))
            .iter()
            .map(|v| str(v.field("path")).into_owned())
            .collect();
        for path in closure {
            self.depend(&path)?;
        }
        let scene = self.baseline_text("scenePath");
        let over = self.native_scene_over(&scene)?;
        self.publish_revision(output_root, started, scene, over, 0, 0)
    }

    /// Writes a revision of the manifest with the fields of `over`, or takes
    /// the one published last when it would hold the same.
    fn publish_revision(
        &mut self,
        output_root: &str,
        started: Instant,
        scene: String,
        mut over: Map,
        object_count: usize,
        asset_count: usize,
    ) -> Result<Prepared> {
        let dependencies: Vec<&Dependency> = self.dependencies.values().collect();
        let dependency_json = value::encode_json(&dependencies)?;
        over.insert(
            "contentHash".into(),
            crate::hex(&Sha256::digest(dependency_json.as_bytes())).into(),
        );
        over.insert(
            "sourceDependencies".into(),
            Value::Raw(dependency_json.into()),
        );
        over.insert(
            "ignoredPrefabOverrideDiagnostics".into(),
            value::raw(&self.sorted_ignored_diagnostics())?,
        );
        self.validate_observed("source changed during preparation, retry")?;
        let output = lexpath::abs(output_root)?;
        over.insert("projectRoot".into(), self.project.as_str().into());
        over.insert("requiresEditor".into(), false.into());
        over.insert("requiresLibrary".into(), false.into());
        // A window whose sources do not change what the manifest holds (a
        // C# edit that was patched, for one) publishes no revision: the one
        // before is the same text but for its id. Writing it cost a file of
        // the size of the scene and an fsync per edit (caches6-3).
        if let Some((payload, prepared)) = &self.published
            && lexpath::dir(&lexpath::dir(&prepared.manifest))
                == lexpath::join(&[&output, "revisions"])
        {
            over.insert("revision".into(), prepared.revision.as_str().into());
            let again = Manifest {
                base: &self.baseline,
                over: std::mem::take(&mut over),
            };
            let same = again.encode().is_ok_and(|text| text == *payload);
            over = again.over;
            if same && fs::metadata(&prepared.manifest).is_ok_and(|info| info.is_file()) {
                return Ok(Prepared {
                    scene,
                    objects: object_count,
                    assets: asset_count,
                    elapsed_seconds: started.elapsed().as_secs_f64(),
                    cache: self.cache.stats,
                    ..prepared.clone()
                });
            }
        }
        let revision = random_hex(16)?;
        let directory = lexpath::join(&[&output, "revisions", &revision]);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(lexpath::dir(&directory))?;
        fs::DirBuilder::new().mode(0o755).create(&directory)?;
        over.insert("revision".into(), revision.as_str().into());
        let path = lexpath::join(&[&directory, "scene.json"]);
        let manifest = Manifest {
            base: &self.baseline,
            over,
        };
        let payload = manifest.encode();
        let written = payload
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))
            .and_then(|payload| write_new_file(&path, payload.as_bytes()));
        if let Err(error) = written {
            let _ = fs::remove_dir_all(&directory);
            self.published = None;
            return Err(error);
        }
        let prepared = Prepared {
            manifest: path,
            revision,
            project: self.project.clone(),
            scene,
            objects: object_count,
            assets: asset_count,
            elapsed_seconds: started.elapsed().as_secs_f64(),
            cache: self.cache.stats,
        };
        self.published = payload.ok().map(|payload| (payload, prepared.clone()));
        Ok(prepared)
    }
}

/// Failed path operations read `<op> <path>: <reason>`.
pub(crate) fn path_error(path: &str, error: std::io::Error) -> anyhow::Error {
    anyhow::Error::new(lexpath::os_error("stat", path, error))
}

fn canonical_project(project: &str, symlinks_first: bool) -> Result<String> {
    if symlinks_first {
        let resolved = lexpath::eval_symlinks(project).map_err(|e| path_error(project, e))?;
        lexpath::abs(&resolved)
    } else {
        let absolute = lexpath::abs(project)?;
        lexpath::eval_symlinks(&absolute).map_err(|e| path_error(&absolute, e))
    }
}

impl Session {
    pub fn new(options: Options) -> Result<Session> {
        let project = canonical_project(&options.project, true)?;
        let mut s = Inner::empty(project.clone());
        s.baseline = match value::read_json::<Value>(&options.manifest)? {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => bail!("json: cannot unmarshal baseline into value of type object"),
        };
        if number(get(&s.baseline, "schemaVersion")) != 1
            || !get(&s.baseline, "supported").is_true()
        {
            bail!("baseline must be a supported schema1 scene manifest");
        }
        if s.baseline_text("projectRoot") != project {
            bail!("baseline belongs to another project");
        }
        if !s.baseline_text("scenePath").starts_with("Assets/") {
            bail!("scene must be under Assets");
        }
        if !options.scene.is_empty() {
            validate_authored_path(&options.scene, ".unity")?;
            s.source(&options.scene)?;
            s.baseline
                .insert("scenePath".into(), options.scene.as_str().into());
            let name = lexpath::base(&options.scene);
            s.baseline.insert(
                "sceneName".into(),
                name.strip_suffix(".unity").unwrap_or(name).into(),
            );
        }
        for path in &options.extra_assets {
            // A prefab, or any other asset the game asks for by address.
            let extension = lexpath::ext(path);
            if matches!(extension, "" | ".unity" | ".meta") {
                bail!("a runtime asset must be an authored asset other than a scene: {path}");
            }
            validate_authored_path(path, extension)?;
            s.extra_assets.push(path.clone());
        }
        if !options.native_catalog.is_empty() {
            s.catalog =
                value::read_json::<Option<Catalog>>(&options.native_catalog)?.unwrap_or_default();
            s.has_catalog = true;
            if s.catalog.schema_version != 1
                || s.catalog.engine_version != s.baseline_text("unityVersion")
                || s.catalog.project != project
            {
                bail!("native catalog does not match baseline project and engine");
            }
            for root in &s.catalog.source_roots {
                if !root.path.starts_with("Packages/")
                    || lexpath::clean(&root.path) != root.path
                    || !lexpath::is_abs(&root.source)
                {
                    bail!("invalid declared package root {}", root.path);
                }
            }
            for model in &s.catalog.model_prefabs {
                s.models
                    .insert(model.guid.clone(), Arc::new(model.objects.clone()));
            }
            for d in &s.catalog.source_dependencies {
                let ext = lexpath::lower_ext(&d.path);
                if ext != ".unity" && ext != ".prefab" && ext != ".cs" {
                    s.critical.insert(d.path.clone(), d.sha256.clone());
                }
            }
            for observation in &s.catalog.prefab_observations {
                s.prefab_observations.insert(
                    value::text(observation, "guid").into_owned(),
                    observation.clone(),
                );
            }
            for o in &s.catalog.ignored_prefab_overrides {
                if get(o, "propertyExists").is_false() {
                    s.ignored_overrides.insert(override_key(
                        &value::text(o, "path"),
                        &value::text(o, "instanceFileId"),
                        &value::text(o, "targetGuid"),
                        &value::text(o, "targetFileId"),
                        &value::text(o, "propertyPath"),
                    ));
                }
            }
        }
        for a in array(get(&s.baseline, "assets")) {
            let asset = object(a);
            let (guid, kind) = (value::text(asset, "guid"), value::text(asset, "kind"));
            s.assets.insert(
                asset_key(&guid, &value::text(asset, "fileId")),
                asset.clone(),
            );
            if kind == "script" {
                s.script_types.insert(
                    guid.into_owned(),
                    value::text(asset, "typeName").into_owned(),
                );
            }
            if raw_kind(&kind) {
                s.critical.remove(value::text(asset, "path").as_ref());
            }
        }
        for (guid, type_name) in &options.script_types {
            s.script_types.insert(guid.clone(), type_name.clone());
        }
        let raw_paths: HashSet<String> = s
            .assets
            .values()
            .filter(|a| raw_kind(&value::text(a, "kind")))
            .map(|a| value::text(a, "path").into_owned())
            .collect();
        let closure: Vec<(String, String, String)> = array(get(&s.baseline, "sourceDependencies"))
            .iter()
            .map(|v| {
                let d = object(v);
                (
                    value::text(d, "path").into_owned(),
                    value::text(d, "sourcePath").into_owned(),
                    value::text(d, "sha256").into_owned(),
                )
            })
            .collect();
        for (path, physical, sha256) in closure {
            if !physical.is_empty() {
                let mut allowed = false;
                for root in s.catalog.source_roots.clone() {
                    if lexpath::rel(&root.source, &physical).is_ok_and(|r| lexpath::inside(&r)) {
                        let canonical = s.checked_path(&physical, &root.source)?;
                        s.explicit_sources.insert(path.clone(), canonical);
                        allowed = true;
                        break;
                    }
                }
                if !allowed {
                    bail!("sourcePath is outside declared package roots: {physical}");
                }
            }
            let extension = lexpath::lower_ext(&path);
            // Scene and prefab data are rebuilt below. C# belongs to the compiler.
            if extension != ".unity"
                && extension != ".prefab"
                && extension != ".cs"
                && !raw_paths.contains(&path)
            {
                s.critical.insert(path.clone(), sha256);
            }
            if rune_lower(&path).ends_with(".cs.meta") {
                s.critical.remove(&path);
            }
        }
        Ok(Session {
            inner: Mutex::new(s),
        })
    }

    /// A catalog-less session over authored YAML only, as `ResolveAuthoredGraph` builds.
    pub(crate) fn authored_only(project: &str) -> Result<Session> {
        Ok(Session {
            inner: Mutex::new(Inner::empty(canonical_project(project, false)?)),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publishes `revisions/<id>/scene.json` under `output_root`.
    pub fn prepare(&self, output_root: &str) -> Result<Prepared> {
        self.lock().prepare(output_root)
    }

    /// Every source the last preparation read, with the hash of what it
    /// read, by absolute path.
    pub fn read_sources(&self) -> BTreeMap<String, String> {
        let s = self.lock();
        s.cache
            .observed
            .iter()
            .filter_map(|(path, identity)| {
                let record = s.cache.files.get(path)?;
                (record.identity == *identity)
                    .then(|| (path.to_string_lossy().into_owned(), record.digest.clone()))
            })
            .collect()
    }

    /// A digest of which source each GUID of the project names.
    pub fn guid_index_sha256(&self) -> Result<String> {
        let mut s = self.lock();
        s.index_guids()?;
        let mut pairs: Vec<(&String, &String)> = s.by_guid.iter().collect();
        pairs.sort();
        let mut hasher = Sha256::new();
        for (guid, path) in pairs {
            hasher.update(guid.as_bytes());
            hasher.update([0]);
            hasher.update(path.as_bytes());
            hasher.update([0]);
        }
        Ok(crate::hex(&hasher.finalize()))
    }

    /// What a preparation is made of beside the files it reads: the baseline
    /// as configured (scene, assemblies, script types, initializers), the
    /// runtime-addressed prefabs and the native content the session holds.
    /// Two sessions with the same configuration and the same files publish
    /// the same scene.
    pub fn configuration_sha256(&self) -> Result<String> {
        let s = self.lock();
        let mut hasher = Sha256::new();
        let mut put = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        // The catalog comes with the baseline: a preparation names both.
        put(value::compact_map(&s.baseline)?.as_bytes());
        put(&[s.has_catalog as u8]);
        for path in &s.extra_assets {
            put(path.as_bytes());
        }
        let mut sorted: Vec<(&String, &String)> = s.script_types.iter().collect();
        sorted.sort();
        for (guid, name) in sorted {
            put(guid.as_bytes());
            put(name.as_bytes());
        }
        let mut sorted: Vec<(&String, &String)> = s.critical.iter().collect();
        sorted.sort();
        for (path, sha) in sorted {
            put(path.as_bytes());
            put(sha.as_bytes());
        }
        for (guid, overlay) in &s.native_overlays {
            put(guid.as_bytes());
            put(overlay.content.sha256.as_bytes());
            put(overlay.source_sha256.as_bytes());
            put(overlay.meta_sha256.as_bytes());
        }
        for (guid, file) in &s.derived_native {
            put(guid.as_bytes());
            put(file.sha256.as_bytes());
        }
        Ok(crate::hex(&hasher.finalize()))
    }

    /// The files the last preparation read, each with its metadata then.
    pub fn read_identities(&self) -> BTreeMap<String, [i64; 8]> {
        self.lock()
            .cache
            .observed
            .iter()
            .map(|(path, identity)| (path.to_string_lossy().into_owned(), identity.fields()))
            .collect()
    }

    /// Stops serving runtime-addressed prefabs.
    pub fn remove_extra_assets(&self, paths: &[String]) {
        self.lock()
            .extra_assets
            .retain(|path| !paths.contains(path));
    }

    /// Accepts watcher events. New, deleted or changed metadata rebuilds the
    /// GUID index. Existing scene and prefab files are checked by stat on every call.
    pub fn invalidate(&self, paths: &[String]) {
        let mut s = self.lock();
        for path in paths {
            if rune_lower(path).ends_with(".meta") || lexpath::ext(path).is_empty() {
                s.index_dirty = true;
            }
        }
    }

    /// Updates the baseline after a caller has verified a new C# compilation.
    /// `assemblies: None` is an absent list (`null`); `initializers: None`
    /// leaves the baseline initializers unchanged.
    pub fn set_managed_bindings(
        &self,
        assemblies: Option<&[String]>,
        script_types: &BTreeMap<String, String>,
        initializers: Option<Value>,
    ) {
        let mut s = self.lock();
        let list = assemblies.map_or(Value::Null, |items| {
            Value::Array(items.iter().map(|a| a.as_str().into()).collect())
        });
        s.baseline.insert("assemblies".into(), list);
        if let Some(initializers) = initializers {
            s.baseline
                .insert("runtimeInitializers".into(), initializers);
        }
        for (guid, type_name) in script_types {
            s.script_types.insert(guid.clone(), type_name.clone());
        }
    }

    /// The canonical project root this session was opened on.
    pub fn project(&self) -> String {
        self.lock().project.clone()
    }
    /// Counters of the most recent preparation.
    pub fn cache_stats(&self) -> CacheStats {
        self.lock().cache.stats
    }
}

/// Useful for callers replacing managed assembly bindings before launch.
/// Unknown fields survive the preparation unchanged.
pub fn manifest_json(path: &str) -> Result<Map> {
    match value::read_json::<Value>(path)? {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(Map::new()),
        _ => {
            bail!("json: cannot unmarshal manifest into value of type object")
        }
    }
}
