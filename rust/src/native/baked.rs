//! Native-format sources the tool bakes itself. No worker runs: Unity has no
//! importer output for them, only Player data derived from the authored YAML.
use super::{
    ProbeOptions,
    batch::GLOBAL_GUIDS,
    import::run_project_import,
    importer::NativeProjectImporter,
    resolve_project_import_source,
    support::{Event, is_not_exist, lower_ext, mkdir_all, read_file, write_file},
};
use crate::{
    cancel::Context,
    playerbake::{
        self, Baked, BehaviourScript, ClipFacts, ClipSources, ControllerSources, Mask,
        ObjectReference, Resolution, ScriptIdentity, ScriptLayout, Unsupported,
    },
    project::{
        NativeContentDependency, NativeContentFile, NativeContentObject, NativeDeferredBinding,
        NativeReplacement,
    },
    registry::meta_guid,
    sha256, sysutil,
    util::random_id,
};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

/// Reads the scripts, clips and masks a baked source refers to.
struct ProjectSources<'a> {
    importer: &'a NativeProjectImporter,
}

impl ProjectSources<'_> {
    fn path(&self, guid: &str, extension: &str, what: &str) -> Result<String> {
        let path = self.importer.source_path(guid)?;
        if path.is_empty() {
            return Err(
                playerbake::Unsupported(format!("{what} {guid} is not a project source")).into(),
            );
        }
        if lower_ext(&path) != extension {
            return Err(Unsupported(format!("{what} stored in {path}")).into());
        }
        Ok(path)
    }
}

impl ClipSources for ProjectSources<'_> {
    fn script(&self, guid: &str) -> Result<ScriptIdentity> {
        let type_name = self.importer.session.lock().script_type(guid).to_string();
        let assembly = self.importer.script_assemblies.get(guid);
        let (Some(assembly), false) = (assembly, type_name.is_empty()) else {
            return Err(
                Unsupported(format!("reference to script {guid} of an unknown class")).into(),
            );
        };
        let path = self.importer.source_path(guid)?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        let meta = String::from_utf8_lossy(&read_file(&format!("{source}.meta"))?).into_owned();
        let execution_order = meta
            .lines()
            .find_map(|line| line.trim().strip_prefix("executionOrder:"))
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let (namespace, class_name) = type_name.rsplit_once('.').unwrap_or(("", &type_name));
        if class_name.contains('+') {
            return Err(
                Unsupported(format!("reference to nested script class {type_name}")).into(),
            );
        }
        Ok(ScriptIdentity {
            class_name: class_name.to_string(),
            namespace: namespace.to_string(),
            assembly: assembly.clone(),
            execution_order,
            local_id: playerbake::script_local_id(guid),
        })
    }
    // The clip taking its pose from this one is baked again when this one
    // changes; see `refresh_dependents`.
    fn clip(&self, guid: &str) -> Result<Vec<u8>> {
        let path = self.path(guid, ".anim", "additive reference clip")?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        read_file(&source)
    }
}

impl ControllerSources for ProjectSources<'_> {
    fn clip(&self, guid: &str, file_id: &str) -> Result<ClipFacts> {
        if let Some(model) = self.importer.model_clips.get(guid) {
            return model.facts(file_id);
        }
        let path = self.path(guid, ".anim", "animation clip")?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        playerbake::read_clip_facts(&read_file(&source)?, &path)
    }
    fn mask(&self, guid: &str) -> Result<Mask> {
        let path = self.path(guid, ".mask", "avatar mask")?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        playerbake::read_mask(&read_file(&source)?)
    }
    fn behaviour(&self, guid: &str) -> Result<Option<BehaviourScript>> {
        // The engine drops a behaviour whose script the project lacks. A
        // script this session does not know may still exist for the engine,
        // so it is refused here rather than dropped.
        let Some(layout) = self.importer.behaviour_layouts.get(guid) else {
            return Err(Unsupported(format!(
                "state machine behaviour of script {guid}, whose class is unknown or not a behaviour"
            ))
            .into());
        };
        let identity = self.script(guid)?;
        if (
            &identity.class_name,
            &identity.namespace,
            &identity.assembly,
        ) != (&layout.class_name, &layout.namespace, &layout.assembly)
        {
            return Err(Unsupported(format!(
                "state machine behaviour of script {guid}, whose class changed since its fields were read"
            ))
            .into());
        }
        Ok(Some(BehaviourScript {
            identity,
            layout: layout.clone(),
        }))
    }
    // A clip is published in baked form, so the behaviour's file can point at
    // it. Any other object reaches the Player some other way; the session
    // binds it once the scene's assets are loaded.
    fn reference(&self, reference: &ObjectReference<'_>) -> Result<Resolution> {
        let refused = |what: &str| {
            Unsupported(format!(
                "behaviour field {} refers to {what} {}",
                reference.path, reference.guid
            ))
        };
        if GLOBAL_GUIDS.contains(&reference.guid) {
            return Err(refused("a resource built into the engine,").into());
        }
        let path = self.importer.source_path(reference.guid)?;
        if path.is_empty() {
            // The engine nulls a reference to nothing, but a source this
            // session does not know may still exist for the engine.
            return Err(refused("an asset that is not a project source,").into());
        }
        let file_id = reference.file_id.to_string();
        let clip = match lower_ext(&path).as_str() {
            ".anim" => true,
            ".fbx" => self
                .importer
                .model_clips
                .get(reference.guid)
                .is_some_and(|model| model.clips.contains_key(&file_id)),
            _ => false,
        };
        if !clip {
            return Ok(Resolution::Deferred);
        }
        // The engine nulls a reference to an object of another class than
        // the field declares.
        let declared = reference.namespace == "UnityEngine"
            && matches!(reference.class_name, "AnimationClip" | "Motion" | "Object");
        if !declared {
            return Ok(Resolution::Null);
        }
        ControllerSources::clip(self, reference.guid, &file_id)?;
        Ok(Resolution::External {
            guid: reference.guid.to_string(),
            local_id: reference.file_id,
        })
    }
}

/// Whether a controller source holds state machine behaviours.
pub(super) fn holds_behaviours(source: &[u8]) -> bool {
    contains(source, b"--- !u!114 ")
}

/// The baked clips of one imported model, published as derived content.
#[derive(Clone, Debug, Default)]
pub(crate) struct ModelClipSet {
    path: String,
    source_sha256: String,
    meta_sha256: String,
    /// The GUID the baked clips are published under; empty if none baked.
    derived_guid: String,
    /// Clip name and loop setting by local file ID.
    clips: BTreeMap<String, (String, bool)>,
    refused: BTreeMap<String, String>,
}

impl ModelClipSet {
    fn facts(&self, file_id: &str) -> Result<ClipFacts> {
        if let Some((name, looping)) = self.clips.get(file_id) {
            return Ok(ClipFacts {
                looping: *looping,
                path: self.path.clone(),
                name: name.clone(),
            });
        }
        let reason = self
            .refused
            .get(file_id)
            .map_or("it holds no such clip", String::as_str);
        Err(Unsupported(format!("clip {file_id} of model {}: {reason}", self.path)).into())
    }
}

/// The GUID a model's baked clips are published under. No source owns it.
fn model_clips_guid(guid: &str) -> String {
    sha256(format!("playerbake-model-clips\0{guid}").as_bytes())[..32].to_string()
}

/// The object references of a source that name another asset.
fn references(data: &[u8]) -> Vec<(String, String)> {
    static PATTERN: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r"fileID:\s*(-?[0-9]+),\s*guid:\s*([0-9a-fA-F]{32})").unwrap()
    });
    PATTERN
        .captures_iter(data)
        .map(|found| {
            (
                String::from_utf8_lossy(&found[2]).to_ascii_lowercase(),
                String::from_utf8_lossy(&found[1]).into_owned(),
            )
        })
        .collect()
}

impl NativeProjectImporter {
    /// Imports a model with the worker, bakes its clips and publishes them as
    /// derived content. The clips of an unchanged model are kept.
    fn prepare_model_clips(&mut self, ctx: &Context, guid: &str, logical: &str) -> Result<()> {
        let source = resolve_project_import_source(&self.opts.project, logical)?;
        let source_sha = sha256(&read_file(&source)?);
        let meta_sha = sha256(&read_file(&format!("{source}.meta"))?);
        if self
            .model_clips
            .get(guid)
            .is_some_and(|known| known.source_sha256 == source_sha && known.meta_sha256 == meta_sha)
        {
            return Ok(());
        }
        let started = Instant::now();
        let root = sysutil::join(&[&self.opts.output, "native/imports", &random_id()]);
        mkdir_all(&sysutil::dir(&root), 0o700)?;
        let import_context = ctx.with_timeout(Duration::from_secs(45));
        let (report, error) = run_project_import(
            &import_context,
            ProbeOptions {
                project: self.opts.project.clone(),
                editor: self.opts.editor.clone(),
                output: root,
                timeout: Duration::from_secs(45),
                ..Default::default()
            },
            &[logical.to_string()],
        );
        import_context.cancel();
        if let Some(error) = error {
            return Err(error);
        }
        let primary = report
            .as_ref()
            .and_then(|report| report.content.as_ref())
            .filter(|content| content.source_unchanged && content.assets.len() == 1)
            .and_then(|content| {
                content.assets[0]
                    .files
                    .iter()
                    .find(|file| file.name.is_empty())
            });
        let Some(primary) = primary else {
            bail!("native worker produced no artifact for {logical}");
        };
        let artifact = match read_file(&primary.path) {
            Ok(data) if sha256(&data) == primary.sha256 => data,
            _ => bail!("native artifact of {logical} changed"),
        };
        let baked = playerbake::bake_model_clips(&artifact)
            .map_err(|error| error.context(format!("bake clips of {logical}")))?;
        let mut set = ModelClipSet {
            path: logical.to_string(),
            source_sha256: source_sha,
            meta_sha256: meta_sha,
            refused: baked.refused.into_iter().collect(),
            ..Default::default()
        };
        for (file_id, name, looping) in baked.facts {
            set.clips.insert(file_id, (name, looping));
        }
        if let Some(baked) = baked.baked {
            set.derived_guid = model_clips_guid(guid);
            let file_sha = sha256(&baked.file);
            let filename = self.store_baked(&set.derived_guid, &file_sha, &baked.file)?;
            let mut content = NativeContentFile {
                guid: set.derived_guid.clone(),
                path: filename,
                sha256: file_sha,
                ..Default::default()
            };
            for (file_id, type_name) in baked.objects {
                content
                    .objects
                    .push(NativeContentObject { file_id, type_name });
            }
            self.session.set_derived_native_content(&content)?;
        }
        Event::new("native-model-clips-baked")
            .with("path", logical)
            .with("clipCount", &set.clips.len())
            .with("refusedCount", &set.refused.len())
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        self.model_clips.insert(guid.to_string(), set);
        Ok(())
    }

    /// Whether the baked clips of a model were made from its current source.
    pub(crate) fn model_clip_source_current(&self, guid: &str, logical: &str) -> bool {
        let Some(known) = self.model_clips.get(guid) else {
            return false;
        };
        resolve_project_import_source(&self.opts.project, logical)
            .and_then(|source| {
                Ok((
                    sha256(&read_file(&source)?),
                    sha256(&read_file(&format!("{source}.meta"))?),
                ))
            })
            .is_ok_and(|(source, meta)| source == known.source_sha256 && meta == known.meta_sha256)
    }

    /// Writes a baked file under its content hash and returns its path.
    fn store_baked(&self, guid: &str, file_sha: &str, file: &[u8]) -> Result<String> {
        // Content-addressed, so a published revision's file is never rewritten.
        let filename = sysutil::join(&[
            &self.opts.output,
            "native/baked",
            &format!("{guid}-{file_sha}.bin"),
        ]);
        match read_file(&filename) {
            Ok(existing) if existing == file => {}
            Ok(_) => bail!("baked native artifact changed on disk: {filename}"),
            Err(error) if is_not_exist(&error) => {
                mkdir_all(&sysutil::dir(&filename), 0o700)?;
                write_file(&filename, file, 0o600)?;
            }
            Err(error) => return Err(error),
        }
        Ok(filename)
    }

    fn source_path(&self, guid: &str) -> Result<String> {
        let mut session = self.session.lock();
        session.index_guids()?;
        Ok(session.path_of(guid).to_string())
    }

    /// Takes the field layouts of the current script assemblies. A published
    /// controller whose behaviours were laid out differently is baked again.
    pub fn set_behaviour_layouts(
        &mut self,
        ctx: &Context,
        layouts: BTreeMap<String, ScriptLayout>,
    ) -> Result<()> {
        let changed: Vec<String> = layouts
            .iter()
            .filter(|(guid, layout)| self.behaviour_layouts.get(*guid) != Some(layout))
            .map(|(guid, _)| guid.clone())
            .chain(
                self.behaviour_layouts
                    .keys()
                    .filter(|guid| !layouts.contains_key(*guid))
                    .cloned(),
            )
            .collect();
        // The first layouts of a session are the ones the catalog was built
        // with; later ones also outdate the catalog's controllers.
        let first = self.behaviour_layouts.is_empty();
        self.behaviour_layouts = layouts;
        let mut controllers: BTreeSet<String> = self
            .entries
            .values()
            .filter(|entry| lower_ext(&entry.source_path) == ".controller")
            .map(|entry| entry.source_path.clone())
            .collect();
        if !first {
            controllers.extend(self.session.lock().native_asset_paths(".controller"));
        }
        for logical in controllers {
            let Ok(source) = resolve_project_import_source(&self.opts.project, &logical) else {
                continue;
            };
            if !read_file(&source).is_ok_and(|data| {
                holds_behaviours(&data)
                    && changed.iter().any(|guid| contains(&data, guid.as_bytes()))
            }) {
                continue;
            }
            match self.publish_baked(ctx, &logical, 0) {
                Ok(guid) => self.refresh_dependents(ctx, &guid, &logical, 1)?,
                Err(error) if error.chain().any(|cause| cause.is::<Unsupported>()) => {
                    Event::new("native-bake-refused")
                        .with("path", &logical)
                        .with("reason", &format!("{error:#}"))
                        .emit();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Bakes an authored animation source and publishes it as native content,
    /// then refreshes the baked sources that refer to it. A source using a
    /// feature the baker refuses is reported, not approximated.
    pub(crate) fn import_baked(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        let guid = self.publish_baked(ctx, logical, 0)?;
        self.refresh_dependents(ctx, &guid, logical, 0)
    }

    /// A controller holds facts of its clips and, once baked, refers to their
    /// baked form; one built into the catalog never sees a baked clip. The
    /// same holds for an override controller and its controller.
    pub(crate) fn refresh_dependents(
        &mut self,
        ctx: &Context,
        guid: &str,
        logical: &str,
        depth: usize,
    ) -> Result<()> {
        if lower_ext(logical) == ".overridecontroller" || depth > 2 {
            return Ok(());
        }
        let mut dependents = BTreeSet::new();
        if lower_ext(logical) == ".anim" {
            dependents.extend(self.additive_dependents(guid)?);
        }
        {
            let session = self.session.lock();
            for extension in [".controller", ".overrideController"] {
                dependents.extend(session.native_asset_paths(extension));
            }
        }
        for entry in self.entries.values() {
            if matches!(
                lower_ext(&entry.source_path).as_str(),
                ".controller" | ".overridecontroller"
            ) {
                dependents.insert(entry.source_path.clone());
            }
        }
        for dependent in dependents {
            if dependent == logical {
                continue;
            }
            let Ok(source) = resolve_project_import_source(&self.opts.project, &dependent) else {
                continue;
            };
            if !read_file(&source).is_ok_and(|data| contains(&data, guid.as_bytes())) {
                continue;
            }
            match self.publish_baked(ctx, &dependent, 0) {
                Ok(published) => self.refresh_dependents(ctx, &published, &dependent, depth + 1)?,
                Err(error) if error.chain().any(|cause| cause.is::<Unsupported>()) => {
                    Event::new("native-bake-refused")
                        .with("path", &dependent)
                        .with("reason", &format!("{error:#}"))
                        .emit();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Bakes again the controllers whose layers were baked with another
    /// revision of a mask than the current one: those of the catalog if the
    /// mask has changed since the catalog was built, and those baked since.
    pub(crate) fn refresh_mask_dependents(
        &mut self,
        ctx: &Context,
        mask: &crate::project::ControllerMask,
    ) -> Result<()> {
        let mut controllers: BTreeSet<String> =
            self.session.lock().native_asset_paths(".controller");
        controllers.extend(
            self.entries
                .values()
                .filter(|entry| lower_ext(&entry.source_path) == ".controller")
                .map(|entry| entry.source_path.clone()),
        );
        let meta_path = format!("{}.meta", mask.path);
        for controller in controllers {
            let Ok(source) = resolve_project_import_source(&self.opts.project, &controller) else {
                continue;
            };
            if !read_file(&source).is_ok_and(|data| contains(&data, mask.guid.as_bytes())) {
                continue;
            }
            let baked = self
                .entries
                .values()
                .find(|entry| entry.source_path == controller);
            let current = match baked {
                Some(entry) => {
                    entry.ingredient_sha256.get(&mask.path) == Some(&mask.source_sha256)
                        && entry.ingredient_sha256.get(&meta_path) == Some(&mask.meta_sha256)
                }
                None => mask.in_catalog,
            };
            if current {
                continue;
            }
            match self.publish_baked(ctx, &controller, 0) {
                Ok(published) => self.refresh_dependents(ctx, &published, &controller, 1)?,
                Err(error) if error.chain().any(|cause| cause.is::<Unsupported>()) => {
                    Event::new("native-bake-refused")
                        .with("path", &controller)
                        .with("reason", &format!("{error:#}"))
                        .emit();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// The clips whose additive reference pose is taken from a clip.
    fn additive_dependents(&mut self, guid: &str) -> Result<Vec<String>> {
        let mut clips: BTreeSet<String> = self.session.lock().native_asset_paths(".anim");
        clips.extend(
            self.entries
                .values()
                .filter(|entry| lower_ext(&entry.source_path) == ".anim")
                .map(|entry| entry.source_path.clone()),
        );
        for logical in clips {
            if self.additive_references.contains_key(&logical) {
                continue;
            }
            let reference = resolve_project_import_source(&self.opts.project, &logical)
                .and_then(|source| read_file(&source))
                .map(|data| additive_reference(&data))
                .unwrap_or_default();
            self.additive_references.insert(logical, reference);
        }
        Ok(self
            .additive_references
            .iter()
            .filter(|(_, reference)| reference.as_str() == guid)
            .map(|(logical, _)| logical.clone())
            .collect())
    }

    /// Bakes one source and the baked sources it refers to; returns its GUID.
    fn publish_baked(&mut self, ctx: &Context, logical: &str, depth: usize) -> Result<String> {
        if depth > 4 {
            bail!("baked sources refer to each other too deeply: {logical}");
        }
        let started = Instant::now();
        let source = resolve_project_import_source(&self.opts.project, logical)?;
        let data = read_file(&source)?;
        let meta = read_file(&format!("{source}.meta"))?;
        let guid = meta_guid(&meta)?;
        let source_sha = sha256(&data);
        let meta_sha = sha256(&meta);
        if lower_ext(logical) == ".anim" {
            self.additive_references
                .insert(logical.to_string(), additive_reference(&data));
        }
        // The clips a controller takes from models are baked first: its own
        // bake reads their facts.
        let mut model_references = Vec::new();
        if lower_ext(logical) != ".anim" {
            for (reference, file_id) in references(&data) {
                let path = self.source_path(&reference)?;
                if lower_ext(&path) == ".fbx" {
                    self.prepare_model_clips(ctx, &reference, &path)?;
                    model_references.push((reference, file_id));
                }
            }
        }
        // An override controller is baked without reading clip facts. A
        // controller reads those of its motions itself, and its behaviours
        // may refer to other objects of a model.
        if lower_ext(logical) == ".overridecontroller" {
            for (reference, file_id) in &model_references {
                self.model_clips[reference]
                    .facts(file_id)
                    .map_err(|error| error.context(format!("bake native {logical}")))?;
            }
        }
        // The masks a controller's layers are baked with.
        let mut ingredients = BTreeMap::new();
        if lower_ext(logical) == ".controller" {
            for (reference, _) in references(&data) {
                let path = self.source_path(&reference)?;
                if lower_ext(&path) != ".mask" {
                    continue;
                }
                let mask = resolve_project_import_source(&self.opts.project, &path)?;
                ingredients.insert(format!("{path}.meta"), file_sha(&format!("{mask}.meta"))?);
                ingredients.insert(path, file_sha(&mask)?);
            }
        }
        let mut deferred = Vec::new();
        let baked = match lower_ext(logical).as_str() {
            ".anim" => playerbake::bake_clip(&data, &ProjectSources { importer: self }),
            ".controller" => playerbake::bake_controller(&data, &ProjectSources { importer: self })
                .map(|controller| {
                    deferred = controller.deferred;
                    controller.baked
                }),
            ".overridecontroller" => playerbake::bake_override(&data),
            ".mask" => playerbake::bake_mask(&data),
            _ => bail!("no baker for {logical}"),
        };
        // Keeps the cause, so a refusal can be told from a failure.
        let baked: Baked =
            baked.map_err(|error| error.context(format!("bake native {logical}")))?;
        // Every source this one refers to has to be published in baked form.
        let mut dependencies = Vec::new();
        for external in &baked.externals {
            if let Some(model) = self.model_clips.get(&external.guid) {
                dependencies.push(model.derived_guid.clone());
                continue;
            }
            let dependency = self.source_path(&external.guid)?;
            if !matches!(lower_ext(&dependency).as_str(), ".anim" | ".controller") {
                return Err(anyhow::Error::from(Unsupported(format!(
                    "reference to {} {}, which is not a baked source",
                    external.guid, dependency
                )))
                .context(format!("bake native {logical}")));
            }
            let current = resolve_project_import_source(&self.opts.project, &dependency)
                .and_then(|source| read_file(&source))
                .map(|data| sha256(&data))?;
            let published = self
                .entries
                .get(&external.guid)
                .is_some_and(|entry| entry.source_sha256 == current);
            if !published {
                self.publish_baked(ctx, &dependency, depth + 1)?;
            }
            dependencies.push(external.guid.clone());
        }
        let file_sha = sha256(&baked.file);
        let filename = self.store_baked(&guid, &file_sha, &baked.file)?;
        let mut replacement = NativeReplacement {
            guid: guid.clone(),
            source_path: logical.into(),
            source_sha256: source_sha,
            meta_sha256: meta_sha,
            environment_sha256: self.environment_sha256.clone(),
            ingredient_sha256: ingredients,
            content: NativeContentFile {
                guid: guid.clone(),
                path: filename,
                sha256: file_sha,
                ..Default::default()
            },
            ..Default::default()
        };
        for binding in deferred {
            replacement.deferred_bindings.push(NativeDeferredBinding {
                owner_file_id: binding.owner,
                property_path: binding.path,
                target_guid: binding.guid,
                target_file_id: binding.file_id.to_string(),
            });
        }
        for (file_id, type_name) in baked.objects {
            replacement
                .content
                .objects
                .push(NativeContentObject { file_id, type_name });
        }
        for dependency in dependencies {
            replacement
                .content
                .dependencies
                .push(NativeContentDependency {
                    guid: dependency,
                    global: false,
                });
        }
        self.session.replace_native_content(&replacement)?;
        self.entries.insert(guid.clone(), replacement);
        self.publish_index()?;
        Event::new("native-baked")
            .with("path", logical)
            .with("guid", &guid)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(guid)
    }
}

/// The GUID of the clip an authored clip takes its additive reference pose
/// from; empty if it names none.
fn additive_reference(data: &[u8]) -> String {
    static PATTERN: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(
            r"m_AdditiveReferencePoseClip:\s*\{fileID:\s*-?[0-9]+,\s*guid:\s*([0-9a-fA-F]{32})",
        )
        .unwrap()
    });
    PATTERN.captures(data).map_or_else(String::new, |found| {
        String::from_utf8_lossy(&found[1]).to_ascii_lowercase()
    })
}

fn file_sha(path: &str) -> Result<String> {
    Ok(sha256(&read_file(path)?))
}

fn contains(data: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && data.windows(needle.len()).any(|window| window == needle)
}
