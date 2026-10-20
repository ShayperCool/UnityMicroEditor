//! The validated native cache of one runner session: replay of the persisted
//! index, on-demand imports and the published runtime resources.
use super::{
    batch::{NativeColdInput, cold_input, cold_native_profile, host_binding_current},
    environment::native_environment_fingerprint,
    mesh_cpu::retain_native_model_mesh_cpu,
    resolve_project_import_source,
    shader_cache::verify_compiled_shader_cache,
    support::{
        Event, is_not_exist, lower_ext, mkdir_all, read_file, read_json, read_json_verbatim,
        store_entry_text, text, wrap, write_file,
    },
    texture_profile::{native_texture_needs_warm, validate_native_configuration},
};
use crate::{
    cancel::Context,
    playerbake::ScriptLayout,
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
    pub(crate) entries: BTreeMap<String, NativeReplacement>,
    pub environment_sha256: String,
    pub(crate) pending_native_paths: Vec<String>,
    /// The assembly that defines each script, by script GUID. A baked clip
    /// that refers to a script needs it to name the class for the Player.
    pub script_assemblies: BTreeMap<String, String>,
    /// The serialized fields of the state machine behaviour scripts, by
    /// script GUID, as the current script assemblies declare them.
    pub(crate) behaviour_layouts: BTreeMap<String, ScriptLayout>,
    /// Runs the constructors and serialization callbacks of those scripts,
    /// which a build runs before it writes a behaviour.
    pub behaviour_code: crate::compiler::BehaviourCode,
    /// The baked clips of models, by model GUID.
    pub(crate) model_clips: BTreeMap<String, super::baked::ModelClipSet>,
    /// The GUID each known clip takes its additive reference pose from, by
    /// the clip's path; empty for a clip with none.
    pub(crate) additive_references: BTreeMap<String, String>,
    /// The import batch of every sprite atlas and of every texture one
    /// packs, by path, as of the last walk.
    pub(crate) atlas_batch: BTreeMap<String, Vec<String>>,
    /// What the import of such a source depends on beyond the source itself.
    pub(crate) atlas_identity: BTreeMap<String, String>,
    /// The index of the project's sources, read once for a pass.
    pub(crate) sources: std::sync::Mutex<Option<Arc<super::build_settings::Sources>>>,
    /// Whether a pass has run. Only the first takes a remembered revision:
    /// a later one follows edits that only the walk of the sources sees.
    pub(crate) prepared_once: bool,
    /// The worker that writes scenes in the Player format, started when
    /// one is first written.
    pub(crate) scene_worker: std::sync::OnceLock<Arc<super::scene_worker::SceneWorker>>,
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

struct Replay<'a> {
    current: &'a BTreeMap<String, NativeReplacement>,
    visiting: HashSet<String>,
    mesh_cpu_upgraded: bool,
    ui_bindings_upgraded: bool,
}

impl NativeProjectImporter {
    /// Replays the persisted index: only entries of the current environment
    /// whose source, metadata and configuration still match are loaded, each
    /// after its dependencies. Entries are visited in GUID order, which only decides
    /// which of several faults is reported.
    pub fn new(opts: NativeOptions, session: Arc<Session>) -> Result<Self> {
        let environment = native_environment_fingerprint(&opts.project, &opts.editor)
            .map_err(|e| wrap("native cache environment", e))?;
        let mut importer = Self {
            opts,
            session,
            entries: BTreeMap::new(),
            environment_sha256: environment.clone(),
            pending_native_paths: Vec::new(),
            script_assemblies: BTreeMap::new(),
            behaviour_layouts: BTreeMap::new(),
            behaviour_code: Default::default(),
            model_clips: BTreeMap::new(),
            additive_references: BTreeMap::new(),
            atlas_batch: BTreeMap::new(),
            atlas_identity: BTreeMap::new(),
            sources: Default::default(),
            prepared_once: false,
            scene_worker: std::sync::OnceLock::new(),
        };
        let read_index = |index: &str| -> Result<Option<Option<Vec<NativeReplacement>>>> {
            match read_json_verbatim(index) {
                Ok(entries) => Ok(Some(entries)),
                Err(error) if is_not_exist(&error) => Ok(None),
                Err(error) => Err(error),
            }
        };
        // A new output starts from what the seed holds. The seed's entries
        // are checked as this output's own would be; one that no longer
        // replays is left to be imported again rather than failing the start.
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
            if entry.environment_sha256 != environment {
                if cold_native_profile(&entry.source_path)
                    && entry.compiled_shader_snapshot.is_none()
                    && importer.cold_input(&entry.source_path).is_ok_and(|input| {
                        input.guid == entry.guid
                            && input.source_sha == entry.source_sha256
                            && input.meta_sha == entry.meta_sha256
                    })
                {
                    importer.pending_native_paths.push(entry.source_path);
                }
                continue;
            }
            // Only a current, independently validated cache entry may replace a baseline.
            let Ok(source) = importer.baked_source(&entry.source_path) else {
                continue;
            };
            if !file_hash(&source).is_ok_and(|sha| sha == entry.source_sha256)
                || !file_hash(&format!("{source}.meta")).is_ok_and(|sha| sha == entry.meta_sha256)
            {
                continue;
            }
            // A generated material is bound to the shaders of one host Player.
            if !host_binding_current(&importer.opts, &entry) {
                continue;
            }
            // A controller's behaviours are laid out from the scripts that
            // were compiled when it was baked. Another session bakes it from
            // its own instead of replaying it.
            if lower_ext(&entry.source_path) == ".controller"
                && read_file(&source).is_ok_and(|data| super::baked::holds_behaviours(&data))
            {
                continue;
            }
            // So does one whose layers hold another revision of a mask.
            if entry.ingredient_sha256.iter().any(|(logical, expected)| {
                !resolve_project_import_source(&importer.opts.project, logical)
                    .and_then(|ingredient| file_hash(&ingredient))
                    .is_ok_and(|sha| sha == *expected)
            }) {
                continue;
            }
            if let Err(error) = verify_compiled_shader_cache(&entry) {
                Event::new("native-cache-rejected")
                    .with("path", &entry.source_path)
                    .with("reason", &text(&error))
                    .emit();
                continue;
            }
            if validate_native_configuration(
                &importer.opts.project,
                &entry.source_path,
                &entry.config_source_sha256,
            )
            .is_err()
            {
                continue;
            }
            if current.contains_key(&entry.guid) {
                bail!("duplicate GUID in native import cache");
            }
            current.insert(entry.guid.clone(), entry);
        }
        // A baked controller whose clip has changed, or a baked shader whose
        // fallback has, is baked again on demand rather than replayed against
        // a source that is no longer current.
        loop {
            let stale: Vec<String> = current
                .values()
                .filter(|entry| {
                    // So is a texture whose atlas is not current.
                    (matches!(
                        lower_ext(&entry.source_path).as_str(),
                        ".controller" | ".overridecontroller" | ".shader" | ".shadergraph"
                    ) || super::texture_profile::texture_source(&entry.source_path))
                        && entry.content.dependencies.iter().any(|dependency| {
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
        let mut replay = Replay {
            current: &current,
            visiting: HashSet::new(),
            mesh_cpu_upgraded: false,
            ui_bindings_upgraded: false,
        };
        for guid in current.keys() {
            match importer.replay(&mut replay, guid) {
                Ok(()) => {}
                Err(error) if seeded => {
                    Event::new("native-cache-rejected")
                        .with("path", &current[guid].source_path)
                        .with("reason", &text(&error))
                        .emit();
                    replay.visiting.clear();
                }
                Err(error) => return Err(error),
            }
        }
        let (mesh_cpu_upgraded, ui_bindings_upgraded) =
            (replay.mesh_cpu_upgraded, replay.ui_bindings_upgraded);
        importer.require_current_environment()?;
        importer.pending_native_paths.sort();
        if seeded {
            importer.publish_index()?;
            Event::new("native-cache-seeded")
                .with("index", &importer.opts.seed)
                .with("entryCount", &importer.entries.len())
                .emit();
        } else if mesh_cpu_upgraded || ui_bindings_upgraded {
            importer.publish_index()?;
            if mesh_cpu_upgraded {
                Event::new("native-mesh-cpu-prepared")
                    .with("entryCount", &importer.entries.len())
                    .with("workerStarted", &false)
                    .emit();
            }
            if ui_bindings_upgraded {
                Event::new("native-ui-bindings-prepared")
                    .with("entryCount", &importer.entries.len())
                    .with("workerStarted", &false)
                    .emit();
            }
        }
        Ok(importer)
    }

    fn replay(&mut self, replay: &mut Replay<'_>, guid: &str) -> Result<()> {
        if self.entries.contains_key(guid) {
            return Ok(());
        }
        let current = replay.current;
        let Some(entry) = current.get(guid) else {
            bail!("native cache dependency has no current source revision: {guid}");
        };
        if replay.visiting.contains(guid) {
            bail!("native cache dependency cycle: {guid}");
        }
        replay.visiting.insert(guid.to_string());
        for dependency in entry.content.dependencies.iter() {
            if !dependency.global {
                self.replay(replay, &dependency.guid)?;
            }
        }
        let previous_content_path = entry.content.path.clone();
        let entry = retain_native_model_mesh_cpu(&self.opts, entry.clone())?;
        replay.mesh_cpu_upgraded |= entry.content.path != previous_content_path;
        let previous_ui_bindings = entry.content.ui_bindings.len();
        let entry = self.bind_native_ui_references(&Context::background(), entry)?;
        replay.ui_bindings_upgraded |= entry.content.ui_bindings.len() != previous_ui_bindings;
        self.session
            .replace_native_content(&entry)
            .map_err(|e| wrap(&format!("native cache {}", entry.source_path), e))?;
        self.entries.insert(guid.to_string(), entry);
        replay.visiting.remove(guid);
        Ok(())
    }

    /// The project's sources, `Assets` and the packages, as an index: read
    /// once for a pass of [`Self::prepare`] (and for the replay of the index)
    /// instead of by every source of a package that is looked up.
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
    pub fn pending_native_paths(&self) -> &[String] {
        &self.pending_native_paths
    }
    pub(crate) fn cold_input(&self, logical: &str) -> Result<NativeColdInput> {
        cold_input(
            &self.opts,
            &self.environment_sha256,
            logical,
            self.atlas_identity.get(logical).map_or("", String::as_str),
        )
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
        let configuration = match first && self.pending_native_paths.is_empty() {
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

    /// Publishes a revision from the sources, importing what they need.
    fn prepare_sources(&mut self, ctx: &Context) -> Result<Prepared> {
        // A scene the worker writes holds what it refers to: nothing of it
        // is imported or baked beside it.
        if !self.session.native_scene_mode() {
            self.refresh_pending_native(ctx)?;
        }
        let mut attempted = HashSet::new();
        loop {
            let error = match self.session.prepare(&self.opts.output) {
                Ok(prepared) => return Ok(prepared),
                Err(error) => error,
            };
            // Native components the loader has no adapter for are built
            // from templates, which the worker writes once.
            if let Some(templates) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<crate::project::NeedsNativeTemplates>())
            {
                let templates = templates.0.clone();
                let key = templates
                    .iter()
                    .map(|t| t.key.clone())
                    .collect::<Vec<_>>()
                    .join(",");
                if !attempted.insert(format!("templates:{key}")) || attempted.len() >= 1024 {
                    return Err(wrap("native component templates did not resolve", error));
                }
                self.build_native_templates(ctx, &templates)?;
                continue;
            }
            // A scene the engine loads itself is written by the worker.
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
            let mut lacking = vec![source];
            for other in &needed.also {
                let other = other.strip_suffix(".meta").unwrap_or(other).to_string();
                if !lacking.contains(&other) && attempted.insert(other.clone()) {
                    lacking.push(other);
                }
            }
            self.import_needed(ctx, &lacking)?;
        }
    }

    pub(crate) fn import_asset(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        self.require_current_environment()?;
        let extension = lower_ext(logical);
        let profile = extension.strip_prefix('.').unwrap_or(&extension);
        if matches!(profile, "uxml" | "uss" | "tss" | "ttf" | "otf") {
            // The Editor's import is taken while it is current; a source
            // changed since, or one the Editor never imported, is imported
            // by the worker.
            if super::warm::warm_database_available(&self.opts.project) {
                match self.import_warm_ui(ctx, logical, &mut HashSet::new()) {
                    Ok(()) => return Ok(()),
                    Err(error) => Event::new("native-warm-ui-unavailable")
                        .with("path", logical)
                        .with("reason", &format!("{error:#}"))
                        .emit(),
                }
            }
            return self.import_cold_ui(ctx, logical);
        }
        if native_texture_needs_warm(&self.opts.project, logical)? {
            return self.import_warm_ui(ctx, logical, &mut HashSet::new());
        }
        // A material is loaded from its authored fields; the session asks
        // only for a variant whose shader names an inspector.
        if profile == "mat" {
            return self.import_material_variant(ctx, logical);
        }
        // A Mesh is loaded from its authored fields; the session asks only
        // for one of an older serialization.
        if profile == "mesh" {
            return self.upgrade_mesh(ctx, logical);
        }
        if matches!(
            profile,
            "anim"
                | "controller"
                | "overridecontroller"
                | "mask"
                | "physicmaterial"
                | "physicsmaterial2d"
                | "rendertexture"
                | "mixer"
                | "shader"
                | "shadergraph"
                | "compute"
        ) {
            return self.import_baked(ctx, logical);
        }
        // A pipeline's global settings are loaded from their authored fields;
        // the session asks which entries a Player build keeps.
        if profile == "asset"
            && let Ok(source) = resolve_project_import_source(&self.opts.project, logical)
            && super::graphics_settings::holds_graphics_settings(&read_file(&source)?)
        {
            return self.import_player_graphics_settings(ctx, logical);
        }
        if !cold_native_profile(logical) {
            bail!(
                "native update for {logical} requires an unsupported importer runtime profile ({profile})"
            );
        }
        // An atlas and the textures it packs are imported together.
        if let Some(batch) = self.atlas_batch.get(logical).cloned() {
            return self.import_cold_batch(ctx, &batch);
        }
        if self.take_cold_cache(logical)? {
            return Ok(());
        }
        self.import_cold_batch(ctx, &[logical.to_string()])
    }

    /// Publishes a source's import from the cache when the cache holds one
    /// made from the source and environment as they are now.
    pub(crate) fn take_cold_cache(&mut self, logical: &str) -> Result<bool> {
        Ok(match self.cached_cold(logical)? {
            Cached::Taken => true,
            Cached::Absent => false,
            Cached::Refused(error) => {
                cache_rejected(logical, &error);
                false
            }
        })
    }

    /// The same for several sources: returns those the cache does not give.
    /// An entry can depend on another of them (a clip on the model whose
    /// avatar it copies), so they are taken in passes until one takes none.
    pub(crate) fn take_cold_caches(&mut self, logicals: Vec<String>) -> Result<Vec<String>> {
        let mut left: Vec<(String, Option<anyhow::Error>)> = logicals
            .into_iter()
            .map(|logical| (logical, None))
            .collect();
        loop {
            let before = left.len();
            let mut next = Vec::new();
            for (logical, _) in left {
                match self.cached_cold(&logical)? {
                    Cached::Taken => {}
                    Cached::Absent => next.push((logical, None)),
                    Cached::Refused(error) => next.push((logical, Some(error))),
                }
            }
            left = next;
            if left.len() == before || left.iter().all(|(_, refused)| refused.is_none()) {
                break;
            }
        }
        Ok(left
            .into_iter()
            .map(|(logical, refused)| {
                if let Some(error) = refused {
                    cache_rejected(&logical, &error);
                }
                logical
            })
            .collect())
    }

    fn cached_cold(&mut self, logical: &str) -> Result<Cached> {
        let input = self.cold_input(logical)?;
        if let Ok(cached) = store_entry_text(&input.cache_path, &self.opts.native_dir())
            .and_then(|text| crate::project::value::decode_use_number::<NativeReplacement>(&text))
            && cached.source_path == logical
            && cached.source_sha256 == input.source_sha
            && cached.meta_sha256 == input.meta_sha
            && cached.environment_sha256 == self.environment_sha256
            && verify_compiled_shader_cache(&cached).is_ok()
            && host_binding_current(&self.opts, &cached)
        {
            let cached = retain_native_model_mesh_cpu(&self.opts, cached)?;
            if let Err(error) = self.session.replace_native_content(&cached) {
                return Ok(Cached::Refused(error));
            }
            let guid = cached.guid.clone();
            self.entries.insert(guid.clone(), cached);
            self.publish_index()?;
            Event::new("native-cache-hit")
                .with("path", logical)
                .with("guid", &guid)
                .emit();
            return Ok(Cached::Taken);
        }
        Ok(Cached::Absent)
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

/// What the cache gave for a source.
enum Cached {
    Taken,
    Absent,
    /// An entry the session would not take as things are now.
    Refused(anyhow::Error),
}

fn cache_rejected(logical: &str, error: &anyhow::Error) {
    Event::new("native-cache-rejected")
        .with("path", logical)
        .with("reason", &text(error))
        .emit();
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
