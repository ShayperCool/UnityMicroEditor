//! Native-format sources the tool bakes itself. No worker runs: Unity has no
//! importer output for them, only Player data derived from the authored YAML.
use super::{
    ProbeOptions,
    batch::GLOBAL_GUIDS,
    built_store,
    import::{IMPORT_BUDGET, run_bounded_import, run_project_import},
    importer::NativeProjectImporter,
    player_objects::{
        Build, Plan, importer_identity, open_sources, plan_source, write_player_objects,
    },
    resolve_project_import_source,
    scripted::script_environment_identity,
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
    ctx: &'a Context,
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

impl playerbake::NativeSources for ProjectSources<'_> {
    fn linear_color_space(&self) -> Option<bool> {
        let settings = resolve_project_import_source(
            &self.importer.opts.project,
            "ProjectSettings/ProjectSettings.asset",
        )
        .and_then(|source| read_file(&source))
        .ok()?;
        let text = String::from_utf8_lossy(&settings);
        let value = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("m_ActiveColorSpace:"))?;
        match value.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        }
    }

    // A variant is written merged with the material it names as its parent.
    fn material(&self, guid: &str) -> Option<Vec<u8>> {
        let path = self.importer.source_path(guid).ok()?;
        if lower_ext(&path) != ".mat" {
            return None;
        }
        resolve_project_import_source(&self.importer.opts.project, &path)
            .and_then(|source| read_file(&source))
            .ok()
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
    // The code of the project runs in the SDK helper, on the assemblies the
    // layouts were read from.
    fn run_object(&self, request: &serde_json::Value) -> Result<Option<serde_json::Value>> {
        self.importer.behaviour_code.run(self.ctx, request)
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
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ModelClipSet {
    path: String,
    source_sha256: String,
    meta_sha256: String,
    /// The GUID the baked clips are published under; empty if none baked.
    derived_guid: String,
    /// Clip name and loop setting by local file ID.
    clips: BTreeMap<String, (String, bool)>,
    refused: BTreeMap<String, String>,
    /// The published file, kept so another session need not import the model.
    content: Option<NativeContentFile>,
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
        // What an earlier session baked from this revision of the model.
        let signature = sha256(
            format!(
                "playerbake-model-clips\0{}\0{guid}\0{source_sha}\0{meta_sha}",
                self.environment_sha256
            )
            .as_bytes(),
        );
        let cache = sysutil::join(&[
            &self.opts.output,
            "native/cache",
            &format!("model-clips-{signature}.json"),
        ]);
        if let Ok(known) = crate::util::read_json_file::<ModelClipSet>(&cache)
            && known.path == logical
            && known.source_sha256 == source_sha
            && known.meta_sha256 == meta_sha
            && known.content.as_ref().is_none_or(|content| {
                read_file(&content.path).is_ok_and(|file| sha256(&file) == content.sha256)
            })
        {
            if let Some(content) = &known.content {
                self.session.set_derived_native_content(content)?;
            }
            self.model_clips.insert(guid.to_string(), known);
            return Ok(());
        }
        let started = Instant::now();
        let imports = sysutil::join(&[&self.opts.output, "native/imports"]);
        mkdir_all(&imports, 0o700)?;
        let (report, error) = run_bounded_import(
            ctx,
            &format!("native import of the clips of {logical}"),
            IMPORT_BUDGET,
            |bounded, budget| {
                run_project_import(
                    bounded,
                    ProbeOptions {
                        project: self.opts.project.clone(),
                        editor: self.opts.editor.clone(),
                        output: sysutil::join(&[&imports, &random_id()]),
                        timeout: budget,
                        ..Default::default()
                    },
                    &[logical.to_string()],
                )
            },
        );
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
            set.content = Some(content);
        }
        mkdir_all(&sysutil::dir(&cache), 0o700)?;
        crate::util::atomic_json(&cache, &set)?;
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
        let source = self.baked_source(logical)?;
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
        // The shaders a shader falls back on or takes passes from are baked
        // first: its own bake reads them.
        if lower_ext(logical) == ".shader" && !self.pipeline_project()? {
            for (path, linked) in self.linked_shaders(&data)? {
                if !self.shader_current(&path, &linked) {
                    self.publish_baked(ctx, &path, depth + 1)?;
                }
            }
        }
        let mut deferred = Vec::new();
        // Where the sources lie that an object built by the Editor's code
        // refers to, by GUID.
        let mut built_paths = BTreeMap::new();
        let baked = match lower_ext(logical).as_str() {
            ".anim" => playerbake::bake_clip(
                &data,
                &ProjectSources {
                    importer: self,
                    ctx,
                },
            ),
            ".controller" => playerbake::bake_controller(
                &data,
                &ProjectSources {
                    importer: self,
                    ctx,
                },
            )
            .map(|controller| {
                deferred = controller.deferred;
                controller.baked
            }),
            ".overridecontroller" => playerbake::bake_override(&data),
            ".mask" => playerbake::bake_mask(&data),
            ".physicmaterial" | ".physicsmaterial2d" | ".rendertexture" | ".mixer" => {
                playerbake::bake_native(
                    &data,
                    &ProjectSources {
                        importer: self,
                        ctx,
                    },
                )
            }
            ".shader" if !self.pipeline_project()? => {
                self.bake_shader_source(ctx, logical, &guid, &mut ingredients)
            }
            ".shader" | ".shadergraph" | ".compute" => {
                self.build_player_source(ctx, logical, &guid, &mut ingredients, &mut built_paths)
            }
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
            let mut dependency = self.source_path(&external.guid)?;
            if dependency.is_empty() {
                dependency = built_paths.get(&external.guid).cloned().unwrap_or_default();
            }
            // A mixer refers only to the mixer it plays into.
            if !matches!(
                lower_ext(&dependency).as_str(),
                ".anim" | ".controller" | ".mixer" | ".shader"
            ) || !dependency.starts_with("Assets/")
            {
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
            // The Player sets a deferred field by its path in the object; an
            // object held by managed reference has no such path.
            if binding.path.starts_with("managedReferences[") {
                return Err(anyhow::Error::from(Unsupported(format!(
                    "behaviour field {} of an object held by managed reference refers to asset {}",
                    binding.path, binding.guid
                )))
                .context(format!("bake native {logical}")));
            }
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

/// What a baked shader was made from beside its own source: the hash of every
/// such file, and how its materials use it.
type ShaderIngredients = (BTreeMap<String, String>, Vec<playerbake::ShaderMaterialUse>);

/// The settings files a build of a shader reads beside the Player settings,
/// which the import environment covers.
const SHADER_SETTINGS: [&str; 3] = [
    "ProjectSettings/GraphicsSettings.asset",
    "ProjectSettings/QualitySettings.asset",
    "Packages/manifest.json",
];

/// How a material source uses its shader: the keywords it enables, valid for
/// the shader or not, and whether it asks for instancing variants.
fn material_use(path: &str, source: &[u8]) -> Result<playerbake::ShaderMaterialUse> {
    let text = String::from_utf8_lossy(source);
    let mut usage = playerbake::ShaderMaterialUse::default();
    let mut listing = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("m_ValidKeywords:")
            .or_else(|| trimmed.strip_prefix("m_InvalidKeywords:"))
        {
            listing = rest.trim().is_empty();
            continue;
        }
        if let (true, Some(keyword)) = (listing, trimmed.strip_prefix("- ")) {
            usage.keywords.push(keyword.trim().to_string());
            continue;
        }
        listing = false;
        if let Some(value) = trimmed.strip_prefix("m_EnableInstancingVariants:") {
            usage.instancing = value.trim() != "0";
        }
        // A variant takes keywords from its parent, which a build merges.
        if let Some(parent) = trimmed.strip_prefix("m_Parent:")
            && !parent.replace(' ', "").starts_with("{fileID:0}")
        {
            return Err(Unsupported(format!(
                "a shader used by the material variant {path}, whose keywords come from its parent"
            ))
            .into());
        }
    }
    usage.keywords.sort();
    usage.keywords.dedup();
    Ok(usage)
}

/// The shaders a shader source names as its fallback or as the owner of a
/// pass it uses.
pub(super) fn linked_shader_names(source: &[u8]) -> Vec<String> {
    linked_shader_links(source)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// The same, each with whether the source names it only for passes it
/// uses, which the compiler copies in: such a shader is not needed by what
/// is built from the source.
pub(super) fn linked_shader_links(source: &[u8]) -> Vec<(String, bool)> {
    static PATTERN: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"(?i-u)\b(fallback|usepass)\s*"([^"\r\n]+)""#).unwrap()
    });
    let Ok(text) = crate::project::shader_without_comments(source) else {
        return Vec::new();
    };
    let mut names: Vec<(String, bool)> = Vec::new();
    for found in PATTERN.captures_iter(&text) {
        let name = String::from_utf8_lossy(&found[2]).into_owned();
        // A used pass is named after its shader.
        let pass = found[1].eq_ignore_ascii_case(b"usepass");
        let name = if pass {
            match name.rsplit_once('/') {
                Some((shader, _)) => shader.to_string(),
                None => continue,
            }
        } else {
            name
        };
        match names.iter_mut().find(|(known, _)| *known == name) {
            Some((_, only_passes)) => *only_passes &= pass,
            None => names.push((name, pass)),
        }
    }
    names
}

/// Finds the shaders a shader names among the baked shaders of the session.
struct PublishedShaders<'a> {
    importer: &'a NativeProjectImporter,
}

impl playerbake::ShaderSources for PublishedShaders<'_> {
    fn shader(&self, name: &str) -> Result<Option<playerbake::ShaderDependency>> {
        let found = self.importer.session.shaders_named(name)?;
        let (path, guid) = match &found[..] {
            [] => return Ok(None),
            [one] => one,
            _ => {
                return Err(Unsupported(format!(
                    "the shader {name:?}, which several sources declare"
                ))
                .into());
            }
        };
        let Some(entry) = self.importer.entries.get(guid) else {
            return Err(Unsupported(format!(
                "the shader {name:?} of {path}, which this session has not baked"
            ))
            .into());
        };
        let file = read_file(&entry.content.path)?;
        if sha256(&file) != entry.content.sha256 {
            bail!("baked shader of {path} changed on disk");
        }
        let objects = playerbake::read_serialized(&file)?;
        let Some(object) = objects
            .iter()
            .find(|object| object.local_id == playerbake::SHADER_LOCAL_ID)
        else {
            bail!("baked file of {path} holds no Shader");
        };
        Ok(Some(playerbake::ShaderDependency {
            pointer: playerbake::ShaderPointer::External { guid: guid.clone() },
            links: playerbake::read_shader_links(&object.data)?,
        }))
    }
}

impl NativeProjectImporter {
    /// The sources of the shaders a shader source names, each with its GUID.
    /// A name no source or several sources declare is left to the bake,
    /// which refuses it.
    fn linked_shaders(&self, source: &[u8]) -> Result<Vec<(String, String)>> {
        let mut linked = Vec::new();
        for name in linked_shader_names(source) {
            if let [one] = &self.session.shaders_named(&name)?[..] {
                linked.push(one.clone());
            }
        }
        Ok(linked)
    }

    /// Whether a shader is published as baked from what it is made of now.
    fn shader_current(&self, logical: &str, guid: &str) -> bool {
        let Some(entry) = self.entries.get(guid) else {
            return false;
        };
        resolve_project_import_source(&self.opts.project, logical)
            .and_then(|source| Ok((file_sha(&source)?, file_sha(&format!("{source}.meta"))?)))
            .is_ok_and(|(source, meta)| source == entry.source_sha256 && meta == entry.meta_sha256)
            && self
                .shader_ingredients(logical, guid)
                .is_ok_and(|(current, _)| current == entry.ingredient_sha256)
    }

    /// What a baked shader was made from beside its own source: the files it
    /// includes, the settings files a build reads, the materials that use
    /// it and the shaders it names with what those are made from, each with
    /// its hash.
    fn shader_ingredients(&self, logical: &str, guid: &str) -> Result<ShaderIngredients> {
        if lower_ext(logical) != ".shader" || self.pipeline_project()? {
            return Ok((self.built_plan(logical, guid)?.1.ingredients, Vec::new()));
        }
        let mut ingredients = BTreeMap::new();
        let mut visited = BTreeSet::from([logical.to_string()]);
        let mut pending = vec![logical.to_string()];
        while let Some(next) = pending.pop() {
            let Ok(source) = resolve_project_import_source(&self.opts.project, &next) else {
                continue;
            };
            let data = read_file(&source)?;
            if next != logical {
                ingredients.insert(next.clone(), sha256(&data));
                let (included, _) =
                    crate::project::collect_shader_source_requirements(&self.opts.project, &next)
                        .map_err(|error| Unsupported(format!("{error:#}")))?;
                for file in included {
                    if file.path != logical {
                        ingredients.insert(file.path, file.sha256);
                    }
                }
            }
            for (path, _) in self.linked_shaders(&data)? {
                if visited.len() < 64 && visited.insert(path.clone()) {
                    pending.push(path);
                }
            }
        }
        let (included, _) =
            crate::project::collect_shader_source_requirements(&self.opts.project, logical)
                .map_err(|error| Unsupported(format!("{error:#}")))?;
        for file in included {
            if file.path != logical {
                ingredients.insert(file.path, file.sha256);
            }
        }
        // The worker's project holds no packages, so a file of one would be
        // missing where the shader is parsed.
        if let Some(package) = ingredients
            .keys()
            .find(|path| path.starts_with("Packages/"))
        {
            return Err(
                Unsupported(format!("a shader that includes the package file {package}")).into(),
            );
        }
        for settings in SHADER_SETTINGS {
            if let Ok(source) = resolve_project_import_source(&self.opts.project, settings) {
                ingredients.insert(settings.to_string(), file_sha(&source)?);
            }
        }
        let mut usage = Vec::new();
        for material in self.session.shader_materials(guid)? {
            usage.push(material_use(&material.path, &material.source)?);
            ingredients.insert(material.path, material.sha256);
        }
        usage.sort();
        usage.dedup();
        Ok((ingredients, usage))
    }

    /// Imports a shader with the worker, which parses it, and compiles its
    /// programs with the engine's shader compiler.
    fn bake_shader_source(
        &self,
        ctx: &Context,
        logical: &str,
        guid: &str,
        ingredients: &mut BTreeMap<String, String>,
    ) -> Result<Baked> {
        let (guards, materials) = self.shader_ingredients(logical, guid)?;
        *ingredients = guards;
        let settings = |logical: &str| -> Result<String> {
            match resolve_project_import_source(&self.opts.project, logical) {
                Ok(source) => Ok(String::from_utf8_lossy(&read_file(&source)?).into_owned()),
                // A file the project lacks holds the engine's defaults.
                Err(error) if is_not_exist(&error) => Ok(String::new()),
                Err(error) => Err(error),
            }
        };
        let player = settings("ProjectSettings/ProjectSettings.asset")?;
        let project = playerbake::ShaderProject {
            platforms: playerbake::shader_platforms(&player)?,
            settings: playerbake::ShaderSettings::read(
                &player,
                &settings(SHADER_SETTINGS[0])?,
                &settings(SHADER_SETTINGS[1])?,
                &settings(SHADER_SETTINGS[2])?,
            )?,
            materials,
        };
        let imports = sysutil::join(&[&self.opts.output, "native/imports"]);
        mkdir_all(&imports, 0o700)?;
        let mut root = String::new();
        let (report, error) = run_bounded_import(
            ctx,
            &format!("native import of {logical}"),
            IMPORT_BUDGET,
            |bounded, budget| {
                root = sysutil::join(&[&imports, &random_id()]);
                run_project_import(
                    bounded,
                    ProbeOptions {
                        project: self.opts.project.clone(),
                        editor: self.opts.editor.clone(),
                        output: root.clone(),
                        timeout: budget,
                        ..Default::default()
                    },
                    &[logical.to_string()],
                )
            },
        );
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
        let compile_context = ctx.with_timeout(Duration::from_secs(300));
        let mut compiler = super::ShaderCompiler::start(
            &compile_context,
            &self.opts.editor,
            &self.opts.project,
            &sysutil::join(&[&root, "logs"]),
        )?;
        let baked = playerbake::bake_shader(
            &artifact,
            &project,
            &PublishedShaders { importer: self },
            &mut compiler,
        );
        drop(compiler);
        compile_context.cancel();
        baked
    }

    /// Bakes again the shaders whose includes or materials have changed
    /// since they were baked.
    pub(crate) fn refresh_shaders(&mut self, ctx: &Context) -> Result<()> {
        let shaders: Vec<(String, String, BTreeMap<String, String>)> = self
            .entries
            .values()
            .filter(|entry| shader_source(&entry.source_path))
            .map(|entry| {
                (
                    entry.guid.clone(),
                    entry.source_path.clone(),
                    entry.ingredient_sha256.clone(),
                )
            })
            .collect();
        for (guid, logical, baked_from) in shaders {
            match self.shader_ingredients(&logical, &guid) {
                Ok((current, _)) if current == baked_from => continue,
                _ => {}
            }
            match self.publish_baked(ctx, &logical, 0) {
                Ok(_) => {}
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
}

impl NativeProjectImporter {
    /// Where a baked source lies. A package lies where the Editor
    /// compilation of the project says, which need not be the project's own
    /// `Library`.
    pub(super) fn baked_source(&self, logical: &str) -> Result<String> {
        if !logical.starts_with("Packages/") {
            return resolve_project_import_source(&self.opts.project, logical);
        }
        open_sources(&self.opts.project)?
            .physical(logical)
            .ok_or_else(|| {
                Unsupported(format!(
                    "{logical}, which lies in a package outside the project"
                ))
                .into()
            })
    }

    /// Builds the shaders of packages that materials of the scene name, in a
    /// project with a scriptable render pipeline. The prepared content holds
    /// such a shader as a build without materials writes it, with the first
    /// combination of every `shader_feature` alone, so a material that
    /// enables a keyword would be drawn with another variant than in a
    /// build. Once built, a shader is kept current by `refresh_shaders`.
    pub(crate) fn build_package_shaders(&mut self, ctx: &Context) -> Result<()> {
        let shaders = self.session.material_package_shaders();
        if shaders.is_empty() || !self.pipeline_project()? {
            return Ok(());
        }
        for (guid, logical) in shaders {
            if guid.is_empty() || self.entries.contains_key(&guid) {
                continue;
            }
            match self.publish_baked(ctx, &logical, 0) {
                Ok(_) => {}
                // The prepared shader stays; the event says what it lacks.
                Err(error) if error.chain().any(|cause| cause.is::<Unsupported>()) => {
                    Event::new("native-bake-refused")
                        .with("path", &logical)
                        .with("reason", &format!("{error:#}"))
                        .with(
                            "kept",
                            "the shader of the prepared content, built without the project's materials",
                        )
                        .emit();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Whether a source is baked as a shader: delivered with what its materials
/// use, and baked again when one of its ingredients changes.
pub(super) fn shader_source(path: &str) -> bool {
    matches!(
        lower_ext(path).as_str(),
        ".shader" | ".shadergraph" | ".compute"
    )
}

impl NativeProjectImporter {
    /// Whether the project names a scriptable render pipeline asset. Its
    /// shaders are then built by the Editor's own code in a worker.
    fn pipeline_project(&self) -> Result<bool> {
        let settings = |logical: &str| -> Result<String> {
            match resolve_project_import_source(&self.opts.project, logical) {
                Ok(source) => Ok(String::from_utf8_lossy(&read_file(&source)?).into_owned()),
                Err(error) if is_not_exist(&error) => Ok(String::new()),
                Err(error) => Err(error),
            }
        };
        Ok(playerbake::names_render_pipeline(
            &settings(SHADER_SETTINGS[0])?,
            &settings(SHADER_SETTINGS[1])?,
        ))
    }

    /// The assets the pipeline's objects were found to load by path, kept
    /// for the project from one build to the next.
    fn reload_list(&self) -> String {
        sysutil::join(&[
            &self.opts.project,
            "Library/UnityMicroEditor/pipeline-reloads.json",
        ])
    }

    /// The materials that use a source, and the plan of its build, which
    /// holds the hash of every file of the project the build depends on.
    fn built_plan(&self, logical: &str, guid: &str) -> Result<(Vec<String>, Plan)> {
        if !self.pipeline_project()? {
            return Err(Unsupported(format!(
                "a {} of a project without a scriptable render pipeline: the Editor's build code in a worker is verified for pipeline projects only",
                match lower_ext(logical).as_str() {
                    ".compute" => "compute shader",
                    _ => "shader graph",
                }
            ))
            .into());
        }
        let users: Vec<String> = if lower_ext(logical) == ".compute" {
            Vec::new()
        } else {
            self.session
                .shader_materials(guid)?
                .into_iter()
                .map(|material| material.path)
                .collect()
        };
        let sources = open_sources(&self.opts.project)?;
        let plan = plan_source(
            &sources,
            &self.opts.project,
            &self.opts.editor,
            &self.reload_list(),
            logical,
            &users,
        )?;
        Ok((users, plan))
    }

    /// The key of a build in the project's store: everything the build is
    /// made from. A plan tells what the worker imports and is asked for.
    fn built_key(
        &self,
        logical: &str,
        guid: &str,
        users: &[String],
        plan: &Plan,
    ) -> Result<String> {
        let source = self.baked_source(logical)?;
        built_store::Inputs {
            importer: &importer_identity(),
            environment_sha256: &self.environment_sha256,
            scripts_identity: &script_environment_identity(&self.opts.project),
            target: logical,
            guid,
            source_sha256: &file_sha(&source)?,
            meta_sha256: &file_sha(&format!("{source}.meta"))?,
            users,
            request: &plan.request.encode()?,
            batch: &plan.batch,
            scripts: &plan.scripts,
            ingredients: &plan.ingredients,
        }
        .key()
    }

    /// Has the Editor's own build code write the Player object of a shader,
    /// a shader graph or a compute shader, in a standalone worker.
    fn build_player_source(
        &self,
        ctx: &Context,
        logical: &str,
        guid: &str,
        ingredients: &mut BTreeMap<String, String>,
        paths: &mut BTreeMap<String, String>,
    ) -> Result<Baked> {
        let (users, plan) = self.built_plan(logical, guid)?;
        *ingredients = plan.ingredients.clone();
        let (class, local_id, type_name) = if lower_ext(logical) == ".compute" {
            (
                playerbake::COMPUTE_SHADER_CLASS,
                playerbake::COMPUTE_SHADER_LOCAL_ID,
                "UnityEngine.ComputeShader",
            )
        } else {
            (48, playerbake::SHADER_LOCAL_ID, "UnityEngine.Shader")
        };
        // Another importer of the project may have built the same already:
        // that of a content scene builds the shaders of the scene's own.
        let key = self.built_key(logical, guid, &users, &plan)?;
        if let Some(kept) = built_store::load(&self.opts.project, &key) {
            Event::new("native-build-reused")
                .with("path", logical)
                .with("key", &key)
                .emit();
            paths.extend(kept.external_paths);
            return Ok(Baked {
                file: kept.file,
                objects: kept.objects,
                externals: kept
                    .externals
                    .into_iter()
                    .map(|guid| playerbake::External { guid })
                    .collect(),
            });
        }
        let built = write_player_objects(
            ctx,
            &Build {
                project: self.opts.project.clone(),
                editor: self.opts.editor.clone(),
                output: sysutil::join(&[&self.opts.output, "native/imports", &random_id()]),
                target: logical.to_string(),
                users,
                timeout: Duration::from_secs(1800),
                reload_list: self.reload_list(),
                // Kept for the project, not for the session: a shader of
                // a thousand variants takes minutes to compile again.
                shader_cache: sysutil::join(&[
                    &self.opts.project,
                    "Library/UnityMicroEditor/shader-cache",
                ]),
            },
        )?;
        let own = built.objects_of(guid);
        let [main] = &own[..] else {
            return Err(Unsupported(format!(
                "a source whose build writes {} objects of its own, not one",
                own.len()
            ))
            .into());
        };
        // A graph's shader has the local ID its importer gave it.
        let local_id = if lower_ext(logical) == ".shadergraph" {
            main.local_id
        } else {
            local_id
        };
        if main.tree.class_id != class || main.local_id != local_id {
            bail!("the build of {logical} wrote another object than its {type_name}");
        }
        for object in &built.objects {
            match object.tree.class_id {
                48 => {
                    playerbake::verify_shader_programs(&object.tree, &object.data)?;
                }
                playerbake::COMPUTE_SHADER_CLASS if object.local_id == main.local_id => {}
                class => {
                    return Err(Unsupported(format!(
                        "a source whose build writes an object of class {class} beside it"
                    ))
                    .into());
                }
            }
        }
        // What the objects refer to in other files: the shaders of the
        // project a shader names, each published as a file of its own.
        let mut externals = Vec::with_capacity(built.externals.len());
        let mut external_paths = BTreeMap::new();
        for external in &built.externals {
            let path = built
                .external_paths
                .get(&external.guid)
                .cloned()
                .unwrap_or_default();
            if external.file_type != 3
                || external.class_name != "UnityEngine.Shader"
                || external.local_id != playerbake::SHADER_LOCAL_ID
                || external.index != playerbake::SHADER_LOCAL_ID
                || !path.starts_with("Assets/")
                || lower_ext(&path) != ".shader"
            {
                return Err(Unsupported(format!(
                    "a reference to {} {} of {}, which is not a shader source of the project",
                    external.class_name,
                    external.local_id,
                    if path.is_empty() {
                        &external.guid
                    } else {
                        &path
                    }
                ))
                .into());
            }
            external_paths.insert(external.guid.clone(), path);
            externals.push(playerbake::External {
                guid: external.guid.clone(),
            });
        }
        // The object itself and the shaders of packages it names, under the
        // local IDs the build gave them.
        let mut objects = vec![playerbake::Object {
            local_id: main.local_id,
            tree: &main.tree,
            data: main.data.clone(),
            script: None,
        }];
        for object in &built.objects {
            if object.local_id != main.local_id {
                objects.push(playerbake::Object {
                    local_id: object.local_id,
                    tree: &object.tree,
                    data: object.data.clone(),
                    script: None,
                });
            }
        }
        objects.sort_by_key(|object| object.local_id);
        let file = playerbake::write(&objects, &externals)?;
        Event::new("native-built")
            .with("path", logical)
            .with("imports", &built.imports)
            .with("objects", &built.objects.len())
            .seconds("artifactDatabaseSeconds", built.database_seconds)
            .seconds("elapsedSeconds", built.seconds)
            .emit();
        let baked = Baked {
            file,
            objects: vec![(local_id.to_string(), type_name.into())],
            externals,
        };
        // Kept under what the build was planned from, and under what a
        // plan says now when the build itself added to the list of assets
        // the pipeline loads by path: the next plan is made with that list.
        let kept = built_store::Built {
            file: baked.file.clone(),
            objects: baked.objects.clone(),
            externals: baked
                .externals
                .iter()
                .map(|external| external.guid.clone())
                .collect(),
            external_paths: external_paths.clone(),
        };
        let mut keys = vec![key];
        if let Ok((users, plan)) = self.built_plan(logical, guid)
            && let Ok(now) = self.built_key(logical, guid, &users, &plan)
            && !keys.contains(&now)
        {
            keys.push(now);
        }
        for key in keys {
            // Only a store: without it the next importer builds again.
            if let Err(error) = built_store::store(&self.opts.project, &key, logical, &kept) {
                Event::new("native-build-not-kept")
                    .with("path", logical)
                    .with("reason", &format!("{error:#}"))
                    .emit();
            }
        }
        paths.extend(external_paths);
        Ok(baked)
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
