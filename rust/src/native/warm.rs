//! An explicitly limited cache path: only the exact source/meta revision
//! recorded in both SourceAssetDB and the current default import's ArtifactDB
//! dependency tuple is accepted. Newly authored or changed files must use an
//! importer; stale Library contents never become a runtime fallback.
use super::{
    ArtifactExtraction, NativeProjectImporter, UdsArtifactStore,
    batch::GLOBAL_GUIDS,
    environment::read_worker_environment,
    resolve_project_import_source,
    support::{
        Event, bytes_contain, check, lower_ext, mkdir_all, read_file, rune_lower, store_entry_text,
        wrap, write_file,
    },
    texture_profile::{
        texture_source, validate_native_configuration, verify_warm_bc7_dependency,
        warm_texture_configuration,
    },
};
use crate::{
    cancel::Context,
    hex, lmdb,
    project::{
        NativeContentDependency, NativeContentFile, NativeContentObject, NativeContentResource,
        NativeReplacement,
    },
    protocol::{ArtifactFile, artifact_metadata, blob_array, guid_wire},
    registry::{meta_guid, native_hash, project_registry_path},
    runtimeartifact::{self, Metadata},
    sha256, sysutil,
    util::{atomic_json, canonical_destination, file_hash, random_id, within_directory},
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    time::Instant,
};

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmNativeArtifact {
    pub source_path: String,
    pub guid: String,
    pub source_sha256: String,
    pub meta_sha256: String,
    #[serde(rename = "sourceNativeHash")]
    pub source_native: String,
    #[serde(rename = "metaNativeHash")]
    pub meta_native: String,
    #[serde(rename = "importResultId")]
    pub import_result: String,
    pub environment_sha256: String,
    pub settings_source_sha256: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub config_source_sha256: BTreeMap<String, String>,
    pub primary: ArtifactExtraction,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<NativeContentResource>,
    #[serde(rename = "runtimeMetadata")]
    pub metadata: Metadata,
}

const MAXIMUM_WARM_SOURCE_BYTES: usize = 64 << 20;

/// The databases of an Editor's imports in the project's `Library`.
const WARM_DATABASES: [&str; 3] = ["SourceAssetDB", "ArtifactDB", "DataStore/uds.db"];

/// Whether the project's `Library` holds the databases an Editor's imports
/// leave, which the warm route takes its UI imports from. A project no
/// Editor opened has none; its UI sources are imported by the worker.
pub(crate) fn warm_database_available(project: &str) -> bool {
    WARM_DATABASES.iter().all(|name| {
        sysutil::stat(&sysutil::join(&[project, "Library", name]))
            .is_ok_and(|info| info.is_regular())
    })
}

pub fn extract_warm_native_ui(
    ctx: &Context,
    project: &str,
    logical: &str,
    output: &str,
) -> Result<WarmNativeArtifact> {
    check(ctx)?;
    let extension = lower_ext(logical);
    let profile = extension.strip_prefix('.').unwrap_or(&extension);
    let texture = texture_source(logical);
    if !matches!(profile, "uxml" | "uss" | "ttf" | "otf" | "shader") && !texture {
        bail!("warm native UI profile is unsupported");
    }
    let source = resolve_project_import_source(project, logical)?;
    resolve_project_import_source(project, &format!("{logical}.meta"))?;
    let mut snapshots = Vec::new();
    for name in WARM_DATABASES {
        let path = sysutil::join(&[project, "Library", name]);
        match sysutil::stat(&path) {
            Ok(info) if info.is_regular() => snapshots.push((path, info)),
            _ => bail!("warm UI database snapshot is unavailable"),
        }
    }
    let data = match read_file(&source) {
        Ok(data) if data.len() <= MAXIMUM_WARM_SOURCE_BYTES => data,
        _ => bail!("warm UI source exceeds bounds"),
    };
    let meta = match read_file(&format!("{source}.meta")) {
        Ok(meta) if meta.len() <= 1 << 20 => meta,
        _ => bail!("warm UI metadata exceeds bounds"),
    };
    let guid = meta_guid(&meta)?;
    let wire = guid_wire(&guid).unwrap_or_default();
    let (source_hash, meta_hash) = (native_hash(&data), native_hash(&meta));
    let mut result = WarmNativeArtifact {
        source_path: logical.into(),
        guid: guid.clone(),
        source_sha256: sha256(&data),
        meta_sha256: sha256(&meta),
        source_native: hex(&source_hash),
        meta_native: hex(&meta_hash),
        ..Default::default()
    };
    let source_database = sysutil::join(&[project, "Library/SourceAssetDB"]);
    let paths = lmdb::read_table(&source_database, "PathToGuid")?;
    let key = format!("{}\0", rune_lower(logical)).into_bytes();
    let mut verified_source = false;
    for record in &paths {
        if record.key != key {
            continue;
        }
        let value = &record.value;
        if value.len() != 48
            || value[..16] != wire
            || value[16..32] != meta_hash
            || value[32..48] != source_hash
        {
            bail!(
                "warm UI SourceAssetDB revision differs from current source/meta: cached={}, current source={} meta={}",
                hex(value),
                hex(&source_hash),
                hex(&meta_hash)
            );
        }
        verified_source = true;
    }
    if !verified_source {
        bail!("warm UI source has no exact SourceAssetDB identity");
    }
    let artifact_path = sysutil::join(&[project, "Library/ArtifactDB"]);
    let revisions = lmdb::read_table(&artifact_path, "CurrentRevisions")?;
    let mut current: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = None;
    for record in &revisions {
        let value = &record.value;
        if value.len() != 56
            || value[..16] != wire
            || value[16..20] != [0xff; 4]
            || value[24..40] != [0; 16]
        {
            continue;
        }
        if current.is_some() {
            bail!("warm UI has ambiguous current default imports");
        }
        current = Some((
            value[..40].to_vec(),
            value[40..56].to_vec(),
            record.key.clone(),
        ));
    }
    let Some((address, import_result, revision_key)) = current else {
        bail!("warm UI has no current default importer result");
    };
    result.import_result = hex(&import_result);
    let stats = lmdb::read_table(&artifact_path, "ImportResultIDToImportStats")?;
    if !stats.iter().any(|record| {
        record.key == import_result && bytes_contain(&record.value, b"6000.6.3f1 (45d8eee7de74)\0")
    }) {
        bail!("warm UI import was not produced by the exact pinned Unity installation");
    }
    let indexes = lmdb::read_table(&artifact_path, "ImportAddressToImportResultIDs")?;
    let mut verified_index = false;
    for record in &indexes {
        if record.key != revision_key {
            continue;
        }
        if record.value.len() < 48 || record.value[..40] != address[..] {
            bail!("warm UI import address index does not match current revision");
        }
        let (ids, _) = blob_array(&record.value, 40, 16)?;
        if ids
            .as_chunks::<16>()
            .0
            .iter()
            .any(|id| id[..] == import_result[..])
        {
            verified_index = true;
        }
    }
    if !verified_index {
        bail!("warm UI current import result is absent from its import address index");
    }
    let dependencies = lmdb::read_table(&artifact_path, "ImportResultIDToArtifactDependencies")?;
    let tuple = [&wire[..], &source_hash[..], &meta_hash[..]].concat();
    let mut dependency_proof: Option<&[u8]> = None;
    for record in &dependencies {
        if record.key == import_result {
            if !bytes_contain(&record.value, &tuple) {
                bail!("warm UI importer dependencies do not prove current source/meta hashes");
            }
            dependency_proof = Some(&record.value);
        }
    }
    let Some(dependency_proof) = dependency_proof else {
        bail!("warm UI import has no source dependency proof");
    };
    let metadata = lmdb::read_table(&artifact_path, "ImportResultIDToArtifactMetaInfo")?;
    let (mut primary, mut resource_file): (Option<ArtifactFile>, Option<ArtifactFile>) =
        (None, None);
    for record in &metadata {
        if record.key != import_result {
            continue;
        }
        let decoded = artifact_metadata(&record.value)?;
        if decoded.address[..40] != address[..] {
            bail!("warm UI artifact metadata address does not match current import");
        }
        for file in decoded.files {
            if texture && file.name == ".resource" {
                if resource_file.is_some() || file.kind != 0 || !file.inline_data.is_empty() {
                    bail!("warm texture has an ambiguous resource artifact");
                }
                resource_file = Some(file);
                continue;
            }
            if !file.name.is_empty() {
                bail!("warm UI streamed artifacts are unsupported");
            }
            if primary.is_some() || file.kind != 0 || !file.inline_data.is_empty() {
                bail!("warm UI has an ambiguous or unsupported primary artifact");
            }
            primary = Some(file);
        }
    }
    let Some(primary) = primary else {
        bail!("warm UI has no current serialized primary artifact");
    };
    let store = UdsArtifactStore::open(&sysutil::join(&[project, "Library/DataStore"]))?;
    let (proof, input) = store.read(&primary.hash);
    let input = input?;
    result.primary = proof;
    let adapted = if profile == "shader" {
        runtimeartifact::adapt_shader(&input)
    } else if texture {
        runtimeartifact::adapt_texture(&input, profile)
    } else {
        runtimeartifact::adapt_ui(&input, profile)
    };
    let (runtime, runtime_metadata) =
        adapted.map_err(|e| wrap(&format!("adapt exact-source warm UI {logical}"), e))?;
    result.metadata = runtime_metadata;
    if result
        .metadata
        .texture
        .as_ref()
        .is_some_and(|texture| texture.format == 25)
    {
        verify_warm_bc7_dependency(project, dependency_proof)?;
    }
    let mut resource_bytes = Vec::new();
    if !result.metadata.resources.is_empty() {
        let (resource, resource_file) = match (&result.metadata.resources[..], &resource_file) {
            ([resource], Some(file)) => (resource, file),
            _ => bail!("warm texture streamed payload has no exact native artifact"),
        };
        if resource.virtual_path != format!("VirtualArtifacts/Primary/{guid}.resource") {
            bail!("warm texture resource alias does not match the source GUID");
        }
        let (proof, content) = store.read(&resource_file.hash);
        let content = content?;
        if resource.offset > proof.bytes || resource.size > proof.bytes - resource.offset {
            bail!("warm texture stream exceeds resource artifact bounds");
        }
        resource_bytes = content;
        result.resources = vec![NativeContentResource {
            path: format!("{output}.resource"),
            virtual_path: resource.virtual_path.clone(),
            sha256: proof.sha256,
        }];
    } else if resource_file.is_some() {
        bail!("warm texture has an unused resource artifact");
    }
    // Validate source snapshots again after every database/artifact read.
    if !file_hash(&source).is_ok_and(|sha| sha == result.source_sha256) {
        bail!("warm UI source changed during artifact extraction");
    }
    if !file_hash(&format!("{source}.meta")).is_ok_and(|sha| sha == result.meta_sha256) {
        bail!("warm UI metadata changed during artifact extraction");
    }
    check(ctx)?;
    for (path, before) in &snapshots {
        if !sysutil::stat(path).is_ok_and(|after| {
            before.same_file(&after)
                && before.size == after.size
                && before.modified == after.modified
        }) {
            bail!("warm UI database changed during artifact validation");
        }
    }
    mkdir_all(&sysutil::dir(output), 0o700)?;
    let absent =
        |path: &str| sysutil::lstat(path).is_err_and(|e| e.raw_os_error() == Some(libc::ENOENT));
    if !absent(output) {
        bail!("warm native UI output must be new");
    }
    // The tool's own directory in Library is no part of the Editor's database;
    // a session writes its output there.
    let library = sysutil::join(&[project, "Library"]);
    if !canonical_destination(output).is_ok_and(|canonical| canonical == output)
        || within_directory(output, &library)
            && !within_directory(output, &sysutil::join(&[&library, "UnityMicroEditor"]))
    {
        bail!("warm UI output must be independent of Library without symlink ancestors");
    }
    write_file(output, &runtime, 0o600)?;
    if !resource_bytes.is_empty() {
        let resource = format!("{output}.resource");
        if !absent(&resource) {
            bail!("warm texture resource output must be new");
        }
        write_file(&resource, &resource_bytes, 0o600)?;
    }
    Ok(result)
}

impl NativeProjectImporter {
    pub(crate) fn import_warm_ui(
        &mut self,
        ctx: &Context,
        logical: &str,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        self.require_current_environment()?;
        if visiting.contains(logical) {
            bail!("warm native UI dependency cycle: {logical}");
        }
        visiting.insert(logical.to_string());
        let result = self.import_warm_ui_visiting(ctx, logical, visiting);
        visiting.remove(logical);
        result
    }

    fn import_warm_dependency(
        &mut self,
        ctx: &Context,
        guid: &str,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        if self.entries.contains_key(guid) {
            return Ok(());
        }
        let path = warm_native_guid_path(&self.opts.project, guid)?;
        self.import_warm_ui(ctx, &path, visiting)
    }

    fn import_warm_ui_visiting(
        &mut self,
        ctx: &Context,
        logical: &str,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        let project = self.opts.project.clone();
        let source = resolve_project_import_source(&project, logical)?;
        let sha = file_hash(&source)?;
        let meta_sha = file_hash(&format!("{source}.meta"))?;
        let environment = self.environment_sha256.clone();
        let mut signature =
            sha256(format!("native-ui-v2\0{environment}\0{logical}\0{sha}\0{meta_sha}").as_bytes());
        let configuration = warm_texture_configuration(&project, logical)?;
        if !configuration.is_empty() {
            let settings = configuration
                .get("ProjectSettings/EditorSettings.asset")
                .map_or("", String::as_str);
            signature = sha256(format!("native-ui-v3\0{signature}\0{settings}").as_bytes());
        }
        let cache_path = sysutil::join(&[
            &self.opts.native_dir(),
            "cache",
            &format!("{signature}.json"),
        ]);
        if let Ok(cached) = store_entry_text(&cache_path, &self.opts.native_dir())
            .and_then(|text| Ok(serde_json::from_str::<NativeReplacement>(&text)?))
            && cached.source_path == logical
            && cached.source_sha256 == sha
            && cached.meta_sha256 == meta_sha
            && cached.environment_sha256 == environment
            && validate_native_configuration(&project, logical, &cached.config_source_sha256)
                .is_ok()
        {
            for dependency in cached.content.dependencies.iter() {
                if !dependency.global {
                    self.import_warm_dependency(ctx, &dependency.guid, visiting)?;
                }
            }
            let cached = self.bind_native_ui_references(ctx, cached)?;
            if self.session.replace_native_content(&cached).is_ok() {
                self.require_current_environment()?;
                let guid = cached.guid.clone();
                self.entries.insert(guid.clone(), cached);
                self.publish_index()?;
                Event::new("native-cache-hit")
                    .with("path", logical)
                    .with("guid", &guid)
                    .emit();
                return Ok(());
            }
        }
        let started = Instant::now();
        let root = sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]);
        let filename = sysutil::join(&[&root, "runtime.bin"]);
        let mut proof = extract_warm_native_ui(ctx, &project, logical, &filename)
            .map_err(|e| wrap(&format!("exact-source warm native UI {logical}"), e))?;
        self.require_current_environment()?;
        let settings = read_worker_environment(&project)?;
        proof.environment_sha256 = environment.clone();
        proof.settings_source_sha256 = settings.settings_sha256;
        proof.config_source_sha256 = configuration.clone();
        let proof_path = sysutil::join(&[&root, "warm-artifact-proof.json"]);
        atomic_json(&proof_path, &proof)?;
        let mut replacement = NativeReplacement {
            guid: proof.guid.clone(),
            source_path: logical.into(),
            source_sha256: sha,
            meta_sha256: meta_sha,
            environment_sha256: environment,
            config_source_sha256: configuration,
            content: NativeContentFile {
                guid: proof.guid.clone(),
                path: filename,
                sha256: proof.metadata.output_sha256.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        if !proof.resources.is_empty() {
            replacement.content.resources = proof.resources.clone().into();
        }
        for object in &proof.metadata.objects {
            replacement.content.objects.push(NativeContentObject {
                file_id: object.file_id.clone(),
                type_name: object.type_name.clone(),
            });
        }
        for dependency in &proof.metadata.external_references {
            let global = GLOBAL_GUIDS.contains(&dependency.guid.as_str());
            if !global {
                self.import_warm_dependency(ctx, &dependency.guid, visiting)?;
            }
            replacement
                .content
                .dependencies
                .push(NativeContentDependency {
                    guid: dependency.guid.clone(),
                    global,
                });
        }
        self.require_current_environment()?;
        let replacement = self.bind_native_ui_references(ctx, replacement)?;
        self.session.replace_native_content(&replacement)?;
        let guid = replacement.guid.clone();
        self.entries.insert(guid.clone(), replacement.clone());
        mkdir_all(&sysutil::dir(&cache_path), 0o700)?;
        atomic_json(&cache_path, &replacement)?;
        self.publish_index()?;
        Event::new("native-warm-artifact")
            .with("path", logical)
            .with("guid", &guid)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .with("proof", &proof_path)
            .emit();
        Ok(())
    }
}

impl NativeProjectImporter {
    /// Imports a UI source in the standalone worker, after the sources it
    /// names by GUID that are not imported yet, so that its importer finds
    /// them as the Editor's does.
    pub(crate) fn import_cold_ui(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        let sources = self.project_sources()?;
        let batch = ui_import_order(
            logical,
            &|path| sources.read(path),
            &|path| sources.guid_of(path).map(str::to_string),
            &|guid| sources.path_of(guid).map(str::to_string),
            // The importers load what a source names, which the worker has
            // only among the imports of its own batch: an import done before
            // is done again. Without it the source names an
            // import-in-progress proxy (class 41386430).
            &|path| super::batch::cold_ui_profile(path) || super::cold_native_profile(path),
        )?;
        if batch.len() > super::NATIVE_IMPORT_BATCH_SIZE {
            bail!("the UI source {logical} names more sources than one import holds");
        }
        // What another session imported as it is now is taken; the batch is
        // imported whole otherwise, since its sources name each other.
        if self.take_cold_caches(batch.clone())?.is_empty() {
            return Ok(());
        }
        self.import_cold_batch(ctx, &batch)
    }
}

/// `logical` after the sources it names, transitively, each after the ones
/// it names: the order one batch imports them in. Only sources `importable`
/// accepts are followed; a cycle is cut where it closes.
pub(crate) fn ui_import_order(
    logical: &str,
    read: &dyn Fn(&str) -> Result<Vec<u8>>,
    guid_of: &dyn Fn(&str) -> Option<String>,
    path_of: &dyn Fn(&str) -> Option<String>,
    importable: &dyn Fn(&str) -> bool,
) -> Result<Vec<String>> {
    struct Walk<'a> {
        read: &'a dyn Fn(&str) -> Result<Vec<u8>>,
        guid_of: &'a dyn Fn(&str) -> Option<String>,
        path_of: &'a dyn Fn(&str) -> Option<String>,
        importable: &'a dyn Fn(&str) -> bool,
        order: Vec<String>,
        visiting: HashSet<String>,
    }
    impl Walk<'_> {
        fn visit(&mut self, path: &str) -> Result<()> {
            if self.order.iter().any(|done| done == path) || !self.visiting.insert(path.to_string())
            {
                return Ok(());
            }
            if self.order.len() > 4096 {
                bail!("UI sources name too many sources");
            }
            if matches!(lower_ext(path).as_str(), ".uxml" | ".uss" | ".tss") {
                for guid in ui_source_references(path, &(self.read)(path)?, self.guid_of) {
                    if let Some(named) =
                        (self.path_of)(&guid).filter(|named| (self.importable)(named))
                    {
                        self.visit(&named)?;
                    }
                }
            }
            self.visiting.remove(path);
            self.order.push(path.to_string());
            Ok(())
        }
    }
    let mut walk = Walk {
        read,
        guid_of,
        path_of,
        importable,
        order: Vec::new(),
        visiting: HashSet::new(),
    };
    walk.visit(logical)?;
    Ok(walk.order)
}

/// The GUIDs of the sources a UI source names: by GUID, as the Editor writes
/// a reference (`project://database/...?guid=`, a UXML `src` with its GUID),
/// or by path in a `url()`, relative to the source, to the project (`/`), or
/// as a `project://database/` URL. A theme (`unity-theme://`) and other
/// schemes name no source. `guid_of` finds the GUID of a logical path.
pub(crate) fn ui_source_references(
    logical: &str,
    text: &[u8],
    guid_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    static GUID: std::sync::LazyLock<regex::bytes::Regex> =
        std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"guid=([0-9a-fA-F]{32})").unwrap());
    static URL: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"url\(\s*["']?([^"')]+?)["']?\s*\)"#).unwrap()
    });
    // A document's `<Style src="…">` and `<Template src="…">` name a file
    // by its path the way a sheet's `url()` does.
    static SOURCE: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap()
    });
    let mut found = Vec::new();
    for capture in GUID.captures_iter(text) {
        found.push(String::from_utf8_lossy(&capture[1]).to_ascii_lowercase());
    }
    let sources = lower_ext(logical) == ".uxml";
    let paths = URL
        .captures_iter(text)
        .filter_map(|capture| capture.get(1))
        .chain(
            SOURCE
                .captures_iter(text)
                .filter(|_| sources)
                .filter_map(|capture| capture.get(1).or_else(|| capture.get(2))),
        );
    for capture in paths {
        let url = String::from_utf8_lossy(capture.as_bytes());
        if url.contains("guid=") {
            continue;
        }
        let url = url.split(['?', '#']).next().unwrap_or_default();
        let url = percent_decoded(url);
        let joined = if let Some(rest) = url.strip_prefix("project://database/") {
            rest.to_string()
        } else if url.contains("://") {
            continue;
        } else if let Some(rest) = url.strip_prefix('/') {
            rest.to_string()
        } else {
            format!("{}/{url}", sysutil::dir(logical))
        };
        let Some(path) = normalized(&joined) else {
            continue;
        };
        if let Some(guid) = guid_of(&path) {
            found.push(guid);
        }
    }
    found.sort();
    found.dedup();
    found
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some(value) = text
                .get(at + 1..at + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(value);
            at += 3;
            continue;
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A logical path with its `.` and `..` parts resolved; `None` above the root.
fn normalized(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn warm_native_guid_path(project: &str, guid: &str) -> Result<String> {
    let wire = guid_wire(guid)?;
    let records = lmdb::read_table(
        &sysutil::join(&[project, "Library/SourceAssetDB"]),
        "GuidToPath",
    )?;
    for record in &records {
        if record.key == wire {
            let logical = match record.value.split_last() {
                Some((0, path)) if !path.contains(&0) => path,
                _ => bail!("warm native GUID path is malformed"),
            };
            let logical = String::from_utf8_lossy(logical).into_owned();
            project_registry_path(&logical)?;
            return Ok(logical);
        }
    }
    bail!("warm native dependency has no project source identity: {guid}")
}
