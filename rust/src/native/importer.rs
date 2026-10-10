//! The validated native cache of one runner session: replay of the persisted
//! index, on-demand imports and the published runtime resources.
use super::{
    batch::{NativeColdInput, cold_input, cold_native_profile},
    environment::native_environment_fingerprint,
    mesh_cpu::retain_native_model_mesh_cpu,
    resolve_project_import_source,
    shader_cache::verify_compiled_shader_cache,
    support::{
        Event, is_not_exist, lower_ext, mkdir_all, read_file, read_json, text, wrap, write_file,
    },
    texture_profile::{native_png_needs_warm, validate_native_configuration},
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
}

pub struct NativeProjectImporter {
    pub opts: NativeOptions,
    pub session: Arc<Session>,
    pub(crate) entries: BTreeMap<String, NativeReplacement>,
    pub environment_sha256: String,
    pub(crate) pending_native_paths: Vec<String>,
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
        };
        let index = sysutil::join(&[&importer.opts.output, "native/index.json"]);
        let entries: Option<Vec<NativeReplacement>> = match read_json(&index) {
            Ok(entries) => entries,
            Err(error) if is_not_exist(&error) => None,
            Err(error) => return Err(error),
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
            let Ok(source) =
                resolve_project_import_source(&importer.opts.project, &entry.source_path)
            else {
                continue;
            };
            if !file_hash(&source).is_ok_and(|sha| sha == entry.source_sha256)
                || !file_hash(&format!("{source}.meta")).is_ok_and(|sha| sha == entry.meta_sha256)
            {
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
        let mut replay = Replay {
            current: &current,
            visiting: HashSet::new(),
            mesh_cpu_upgraded: false,
            ui_bindings_upgraded: false,
        };
        for guid in current.keys() {
            importer.replay(&mut replay, guid)?;
        }
        let (mesh_cpu_upgraded, ui_bindings_upgraded) =
            (replay.mesh_cpu_upgraded, replay.ui_bindings_upgraded);
        importer.require_current_environment()?;
        importer.pending_native_paths.sort();
        if mesh_cpu_upgraded || ui_bindings_upgraded {
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

    pub fn entries(&self) -> &BTreeMap<String, NativeReplacement> {
        &self.entries
    }
    pub fn pending_native_paths(&self) -> &[String] {
        &self.pending_native_paths
    }
    pub(crate) fn cold_input(&self, logical: &str) -> Result<NativeColdInput> {
        cold_input(&self.opts, &self.environment_sha256, logical)
    }

    /// Publishes a revision, importing native content the Session reports as
    /// changed until it resolves.
    pub fn prepare(&mut self, ctx: &Context) -> Result<Prepared> {
        self.require_current_environment()?;
        self.refresh_pending_native(ctx)?;
        let mut attempted = HashSet::new();
        loop {
            let error = match self.session.prepare(&self.opts.output) {
                Ok(prepared) => return Ok(prepared),
                Err(error) => error,
            };
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
        let extension = lower_ext(logical);
        let profile = extension.strip_prefix('.').unwrap_or(&extension);
        if matches!(profile, "uxml" | "uss" | "ttf" | "otf" | "jpg" | "jpeg") {
            return self.import_warm_ui(ctx, logical, &mut HashSet::new());
        }
        if profile == "png" && native_png_needs_warm(&self.opts.project, logical)? {
            return self.import_warm_ui(ctx, logical, &mut HashSet::new());
        }
        if !cold_native_profile(logical) {
            bail!(
                "native update for {logical} requires an unsupported importer runtime profile ({profile})"
            );
        }
        let input = self.cold_input(logical)?;
        if let Ok(cached) = read_json::<NativeReplacement>(&input.cache_path)
            && cached.source_path == logical
            && cached.source_sha256 == input.source_sha
            && cached.meta_sha256 == input.meta_sha
            && cached.environment_sha256 == self.environment_sha256
            && verify_compiled_shader_cache(&cached).is_ok()
        {
            let cached = retain_native_model_mesh_cpu(&self.opts, cached)?;
            if self.session.replace_native_content(&cached).is_ok() {
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
        self.import_cold_batch(ctx, &[logical.to_string()])
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
        atomic_json(
            &sysutil::join(&[&self.opts.output, "native/index.json"]),
            &entries,
        )
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
