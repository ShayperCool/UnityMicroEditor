//! The native cache of one runner session: the scene the import worker
//! writes in the Player format, the shaders the session publishes beside
//! it, the replay of their persisted index and the published resources.
use super::{
    environment::native_environment_fingerprint,
    resolve_project_import_source,
    support::{
        Event, is_not_exist, lower_ext, mkdir_all, read_file, read_json, read_json_verbatim, text,
        wrap, write_file,
    },
};
use crate::{
    cancel::Context,
    project::{NativeContentFile, NativeReplacement, NeedsImportError, Prepared, Session},
    sha256, sysutil,
    util::{atomic_json, canonical_destination, file_hash, random_id},
};
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

/// The part of the runner's launch options the native importer reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeOptions {
    pub project: String,
    pub output: String,
    pub editor: String,
    /// The host Player the content is imported for, or empty when the caller
    /// has none. A generated material takes its built-in shader from the
    /// files of that Player.
    pub player: String,
    /// Where imports, bakes and their caches are written: the store the
    /// sessions of a project share (see [`project_native_store`]), or, when
    /// empty, the session's own `native` directory. The index of what the
    /// session holds stays with the session either way.
    pub store: String,
    /// The index of another session of the same scene, replayed when this
    /// output has none of its own yet: the preparation a run starts from.
    pub seed: String,
}

impl NativeOptions {
    /// The directory of the imports, bakes and caches of this importer.
    pub fn native_dir(&self) -> String {
        match self.store.is_empty() {
            true => sysutil::join(&[&self.output, "native"]),
            false => self.store.clone(),
        }
    }

    /// The index of the content this importer's session holds.
    pub fn index_path(&self) -> String {
        native_index(&self.output)
    }
}

/// The store of native content that every session of a project shares. Its
/// entries are named by what they were made from, so that a preparation and
/// the runs after it import a source once.
pub fn project_native_store(project: &str) -> String {
    sysutil::join(&[project, "Library/UnityMicroEditor/native-store"])
}

/// The index of the native content of a session output.
pub fn native_index(output: &str) -> String {
    sysutil::join(&[output, "native/index.json"])
}

pub struct NativeProjectImporter {
    pub opts: NativeOptions,
    pub session: Arc<Session>,
    /// The shaders the session publishes, by GUID.
    pub(crate) entries: BTreeMap<String, NativeReplacement>,
    pub environment_sha256: String,
    /// The index of the project's sources, read once for a pass.
    pub(crate) sources: std::sync::Mutex<Option<Arc<super::build_settings::Sources>>>,
    /// Whether a pass has run. Only the first takes a remembered revision:
    /// a later one follows edits that only the walk of the sources sees.
    pub(crate) prepared_once: bool,
    /// The worker that writes scenes in the Player format, started when
    /// one is first written.
    pub(crate) scene_worker: std::sync::OnceLock<Arc<super::scene_worker::SceneWorker>>,
    /// The worker that writes assets on their own
    /// (`write_native_assets`), so that the scene worker keeps its scenes.
    pub(crate) asset_worker: std::sync::OnceLock<super::scene_worker::SceneWorker>,
    /// The engine's classes and the scripts' layouts the tool's own writer
    /// writes with, by what the scripts' assemblies hold.
    pub(crate) own_library: std::sync::Mutex<Option<(String, Arc<crate::playerbake::Library>)>>,
}

/// What a published revision was made of; see `unchanged_revision`.
#[derive(Debug, Default, serde::Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RevisionMemo {
    schema_version: i64,
    tool: String,
    configuration: String,
    guid_index: String,
    /// Every file the preparation read, with its metadata then.
    files: BTreeMap<String, [i64; 8]>,
    manifest: String,
    manifest_sha256: String,
    revision: String,
    project: String,
    scene: String,
    objects: usize,
    assets: usize,
}

/// Whether a source is a shader the session builds and publishes.
fn shader_source(logical: &str) -> bool {
    matches!(
        lower_ext(logical).as_str(),
        ".shader" | ".shadergraph" | ".compute"
    )
}

impl NativeProjectImporter {
    /// Replays the persisted index of shaders: only entries of the current
    /// environment whose source, metadata and ingredients still match are
    /// loaded, each after its dependencies.
    pub fn new(opts: NativeOptions, session: Arc<Session>) -> Result<Self> {
        let environment = native_environment_fingerprint(&opts.project, &opts.editor)
            .map_err(|e| wrap("native cache environment", e))?;
        let mut importer = Self {
            opts,
            session,
            entries: BTreeMap::new(),
            environment_sha256: environment.clone(),
            sources: Default::default(),
            prepared_once: false,
            scene_worker: std::sync::OnceLock::new(),
            asset_worker: std::sync::OnceLock::new(),
            own_library: std::sync::Mutex::new(None),
        };
        let read_index = |index: &str| -> Result<Option<Option<Vec<NativeReplacement>>>> {
            match read_json_verbatim(index) {
                Ok(entries) => Ok(Some(entries)),
                Err(error) if is_not_exist(&error) => Ok(None),
                Err(error) => Err(error),
            }
        };
        // A new output starts from what the seed holds.
        let (entries, seeded) = match read_index(&importer.opts.index_path())? {
            Some(entries) => (entries, false),
            None if !importer.opts.seed.is_empty() => {
                match read_index(&importer.opts.seed).unwrap_or_default() {
                    Some(entries) => (entries, true),
                    None => (None, false),
                }
            }
            None => (None, false),
        };
        let mut current: BTreeMap<String, NativeReplacement> = BTreeMap::new();
        for entry in entries.unwrap_or_default() {
            if entry.environment_sha256 != environment || !shader_source(&entry.source_path) {
                continue;
            }
            let Ok(source) = importer.baked_source(&entry.source_path) else {
                continue;
            };
            if !file_hash(&source).is_ok_and(|sha| sha == entry.source_sha256)
                || !file_hash(&format!("{source}.meta")).is_ok_and(|sha| sha == entry.meta_sha256)
                || entry.ingredient_sha256.iter().any(|(logical, expected)| {
                    !resolve_project_import_source(&importer.opts.project, logical)
                        .and_then(|ingredient| file_hash(&ingredient))
                        .is_ok_and(|sha| sha == *expected)
                })
            {
                continue;
            }
            if current.contains_key(&entry.guid) {
                bail!("duplicate GUID in native import cache");
            }
            current.insert(entry.guid.clone(), entry);
        }
        // A shader whose fallback is not current is built again.
        loop {
            let stale: Vec<String> = current
                .values()
                .filter(|entry| {
                    entry.content.dependencies.iter().any(|dependency| {
                        !dependency.global && !current.contains_key(&dependency.guid)
                    })
                })
                .map(|entry| entry.guid.clone())
                .collect();
            if stale.is_empty() {
                break;
            }
            for guid in stale {
                current.remove(&guid);
            }
        }
        let mut visiting = HashSet::new();
        for guid in current.keys() {
            match importer.replay(&current, &mut visiting, guid) {
                Ok(()) => {}
                Err(error) if seeded => {
                    Event::new("native-cache-rejected")
                        .with("path", &current[guid].source_path)
                        .with("reason", &text(&error))
                        .emit();
                    visiting.clear();
                }
                Err(error) => return Err(error),
            }
        }
        importer.require_current_environment()?;
        if seeded {
            importer.publish_index()?;
            Event::new("native-cache-seeded")
                .with("index", &importer.opts.seed)
                .with("entryCount", &importer.entries.len())
                .emit();
        }
        Ok(importer)
    }

    fn replay(
        &mut self,
        current: &BTreeMap<String, NativeReplacement>,
        visiting: &mut HashSet<String>,
        guid: &str,
    ) -> Result<()> {
        if self.entries.contains_key(guid) {
            return Ok(());
        }
        let Some(entry) = current.get(guid) else {
            bail!("native cache dependency has no current source revision: {guid}");
        };
        if !visiting.insert(guid.to_string()) {
            bail!("native cache dependency cycle: {guid}");
        }
        for dependency in entry.content.dependencies.iter() {
            if !dependency.global {
                self.replay(current, visiting, &dependency.guid)?;
            }
        }
        self.session
            .replace_native_content(entry)
            .map_err(|e| wrap(&format!("native cache {}", entry.source_path), e))?;
        self.entries.insert(guid.to_string(), entry.clone());
        visiting.remove(guid);
        Ok(())
    }

    /// The project's sources, `Assets` and the packages, as an index: read
    /// once for a pass of [`Self::prepare`].
    pub(crate) fn project_sources(&self) -> Result<Arc<super::build_settings::Sources>> {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sources) = &*sources {
            return Ok(sources.clone());
        }
        let opened = Arc::new(super::player_objects::open_sources(&self.opts.project)?);
        *sources = Some(opened.clone());
        Ok(opened)
    }

    pub fn entries(&self) -> &BTreeMap<String, NativeReplacement> {
        &self.entries
    }

    /// Publishes a revision, importing native content the Session reports as
    /// changed until it resolves.
    pub fn prepare(&mut self, ctx: &Context) -> Result<Prepared> {
        // A pass sees the sources as they are when it begins.
        *self.sources.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.require_current_environment()?;
        // What the session holds before it looks at the sources again; with
        // native content waiting to be imported nothing is taken. A start
        // replayed its index against the sources as they are; a later pass
        // of a watched session has not.
        let first = !std::mem::replace(&mut self.prepared_once, true);
        let configuration = match first {
            true => Some(self.session.configuration_sha256()?),
            false => None,
        };
        if let Some(prepared) = configuration
            .as_deref()
            .and_then(|configuration| self.unchanged_revision(configuration))
        {
            Event::new("native-revision-unchanged")
                .with("revision", &prepared.revision)
                .emit();
            return Ok(prepared);
        }
        let prepared = self.prepare_sources(ctx)?;
        if let Some(configuration) = &configuration {
            self.remember_revision(configuration, &prepared);
        }
        Ok(prepared)
    }

    /// The revision published last, when nothing it was made of has changed
    /// since: the session's configuration and native content, the GUIDs of
    /// the project, every file the preparation read (by its metadata), the
    /// published manifest and this program. A start then neither walks nor
    /// prepares the scene, as Unity enters Play Mode with an up-to-date
    /// asset database.
    fn unchanged_revision(&self, configuration: &str) -> Option<Prepared> {
        let memo: RevisionMemo = read_json(&self.revision_memo()).ok()?;
        let current = memo.schema_version == 1
            && Some(memo.tool.as_str()) == crate::util::tool_identity()
            && memo.configuration == configuration
            && file_hash(&memo.manifest).ok()? == memo.manifest_sha256
            && memo.files.iter().all(|(path, fields)| {
                crate::cache::Identity::read(std::path::Path::new(path))
                    .is_ok_and(|identity| identity.fields() == *fields)
            })
            && self.session.guid_index_sha256().ok()? == memo.guid_index;
        current.then(|| Prepared {
            manifest: memo.manifest,
            revision: memo.revision,
            project: memo.project,
            scene: memo.scene,
            objects: memo.objects,
            assets: memo.assets,
            ..Default::default()
        })
    }

    /// Remembers what a published revision was made of. A record that cannot
    /// be written only costs the next start a preparation.
    fn remember_revision(&self, configuration: &str, prepared: &Prepared) {
        let (Some(tool), Ok(guid_index), Ok(manifest_sha256)) = (
            crate::util::tool_identity(),
            self.session.guid_index_sha256(),
            file_hash(&prepared.manifest),
        ) else {
            return;
        };
        let memo = RevisionMemo {
            schema_version: 1,
            tool: tool.into(),
            configuration: configuration.into(),
            guid_index,
            files: self.session.read_identities(),
            manifest: prepared.manifest.clone(),
            manifest_sha256,
            revision: prepared.revision.clone(),
            project: prepared.project.clone(),
            scene: prepared.scene.clone(),
            objects: prepared.objects,
            assets: prepared.assets,
        };
        let path = self.revision_memo();
        if mkdir_all(&sysutil::dir(&path), 0o700).is_ok() {
            let _ = atomic_json(&path, &memo);
        }
    }

    fn revision_memo(&self) -> String {
        sysutil::join(&[&self.opts.output, "native/revision-memo.json"])
    }

    /// Publishes a revision from the sources: the scene the worker writes,
    /// and the shaders it needs, which are built and published on their own.
    fn prepare_sources(&mut self, ctx: &Context) -> Result<Prepared> {
        let mut attempted = HashSet::new();
        loop {
            let error = match self.session.prepare(&self.opts.output) {
                Ok(prepared) => return Ok(prepared),
                Err(error) => error,
            };
            if let Some(scene) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<crate::project::NeedsNativeScene>())
            {
                if !attempted.insert(format!("scene:{}", scene.key)) {
                    return Err(wrap(
                        "the scene in the Player format did not resolve",
                        error,
                    ));
                }
                self.build_native_scene(ctx, scene)?;
                continue;
            }
            let Some(needed) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<NeedsImportError>())
            else {
                return Err(error);
            };
            let source = needed
                .path
                .strip_suffix(".meta")
                .unwrap_or(&needed.path)
                .to_string();
            if attempted.contains(&source) || attempted.len() >= 1024 {
                return Err(wrap(
                    &format!("native content import did not resolve {source}"),
                    error,
                ));
            }
            attempted.insert(source.clone());
            self.import_asset(ctx, &source)?;
        }
    }

    pub(crate) fn import_asset(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        self.require_current_environment()?;
        if !shader_source(logical) {
            bail!("the session builds only shaders, not {logical}");
        }
        self.import_baked(ctx, logical)
    }

    pub(crate) fn require_current_environment(&self) -> Result<()> {
        let current = native_environment_fingerprint(&self.opts.project, &self.opts.editor)?;
        if current != self.environment_sha256 {
            bail!(
                "native import environment changed; restart the runner to rebuild dependent content"
            );
        }
        Ok(())
    }

    pub(crate) fn publish_index(&self) -> Result<()> {
        let entries: Vec<&NativeReplacement> = self.entries.values().collect();
        let index = self.opts.index_path();
        mkdir_all(&sysutil::dir(&index), 0o700)?;
        atomic_json(&index, &entries)
    }
}

/// Mounts the published native resources under their runtime aliases. The
/// entire set is validated before a live Player's aliases are touched.
pub fn stage_native_resources(manifest: &str, run_directory: &str) -> Result<()> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Manifest {
        #[serde(rename = "nativeContentFiles")]
        files: Option<Vec<NativeContentFile>>,
        #[serde(rename = "projectRoot")]
        project: Option<String>,
    }
    let doc: Manifest = read_json(manifest)?;
    let project = doc.project.unwrap_or_default();
    let mut aliases = HashSet::new();
    let mut mounts: Vec<(String, Vec<u8>)> = Vec::new();
    for file in doc.files.iter().flatten() {
        if !valid_native_guid(&file.guid) {
            bail!("native resource GUID is invalid");
        }
        for resource in file.resources.iter() {
            if resource.virtual_path != format!("VirtualArtifacts/Primary/{}.resource", file.guid) {
                bail!("native resource path is outside source GUID");
            }
            if !aliases.insert(resource.virtual_path.clone()) {
                bail!("duplicate native resource alias");
            }
            let destination = sysutil::join(&[run_directory, &resource.virtual_path]);
            if !canonical_destination(&destination).is_ok_and(|c| c == destination) {
                bail!("native resource mount has symlink ancestors");
            }
            let contents = read_file(&resource.path)?;
            if sha256(&contents) != resource.sha256 {
                bail!("native resource changed before mounting");
            }
            mounts.push((destination.clone(), contents.clone()));
            if !file.path.is_empty() {
                if !sysutil::is_abs(&file.path)
                    || project.is_empty()
                    || !sysutil::is_owned_cache_path(&project, &file.path)
                {
                    bail!(
                        "native resource content file must be owned beneath project Temp or Library/UnityMicroEditor"
                    );
                }
                let local = sysutil::join(&[&sysutil::dir(&file.path), &resource.virtual_path]);
                if !canonical_destination(&local).is_ok_and(|c| c == local) {
                    bail!("native content resource mirror has symlink ancestors");
                }
                if local != destination {
                    mounts.push((local, contents));
                }
            }
        }
    }
    for (destination, contents) in mounts {
        mkdir_all(&sysutil::dir(&destination), 0o700)?;
        let temporary = format!("{destination}.{}.tmp", random_id());
        write_file(&temporary, &contents, 0o600)?;
        if let Err(error) = std::fs::rename(&temporary, &destination) {
            let _ = std::fs::remove_file(&temporary);
            return Err(anyhow!(
                "rename {temporary} {destination}: {}",
                sysutil::errno_text(&error)
            ));
        }
    }
    Ok(())
}

pub fn valid_native_guid(guid: &str) -> bool {
    guid.len() == 32
        && guid != "0".repeat(32)
        && guid
            .bytes()
            .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
}
