//! The shaders a session builds and publishes on their own: a built-in
//! pipeline's shader baked from its source, and a scriptable pipeline's
//! shader, shader graph or compute shader written by the Editor's build
//! code in a worker, with every material of the project that uses it.
use super::{
    ProbeOptions,
    build_settings::{BuildSettings, Sources},
    built_store,
    import::{IMPORT_BUDGET, run_bounded_import, run_project_import},
    importer::NativeProjectImporter,
    player_objects::{
        Build, Plan, importer_identity, plan_source, plan_source_with, write_player_objects,
    },
    resolve_project_import_source,
    scripted::{legacy_script_environment_identity, script_environment_identity},
    support::{Event, is_not_exist, lower_ext, mkdir_all, read_file},
};
use crate::{
    cancel::Context,
    playerbake::{self, Baked, Unsupported},
    project::{NativeContentDependency, NativeContentFile, NativeContentObject, NativeReplacement},
    registry::meta_guid,
    sha256, sysutil,
    util::random_id,
};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

impl NativeProjectImporter {
    /// Writes a baked file under its content hash and returns its path.
    pub(super) fn store_baked(&self, guid: &str, file_sha: &str, file: &[u8]) -> Result<String> {
        // Content-addressed, so a published revision's file is never rewritten.
        let filename = sysutil::join(&[
            &self.opts.native_dir(),
            "baked",
            &format!("{guid}-{file_sha}.bin"),
        ]);
        mkdir_all(&sysutil::dir(&filename), 0o700)?;
        super::support::write_immutable_file(&filename, file)?;
        Ok(filename)
    }

    fn source_path(&self, guid: &str) -> Result<String> {
        let mut session = self.session.lock();
        session.index_guids()?;
        Ok(session.path_of(guid).to_string())
    }

    /// Builds a shader the scene uses and publishes it as native content.
    pub(crate) fn import_baked(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        self.publish_baked(ctx, logical, 0).map(|_| ())
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
        let mut ingredients = BTreeMap::new();
        // The shaders a shader falls back on or takes passes from are baked
        // first: its own bake reads them.
        if lower_ext(logical) == ".shader" && !self.pipeline_project()? {
            for (path, linked) in self.linked_shaders(&data)? {
                if !self.shader_current(&path, &linked) {
                    self.publish_baked(ctx, &path, depth + 1)?;
                }
            }
        }
        // Where the sources lie that an object built by the Editor's code
        // refers to, by GUID.
        let mut built_paths = BTreeMap::new();
        let baked = match lower_ext(logical).as_str() {
            ".shader" if !self.pipeline_project()? => {
                self.bake_shader_source(ctx, logical, &guid, &mut ingredients)
            }
            ".shader" | ".shadergraph" | ".compute" => {
                self.prebuild_pipeline_shaders(ctx);
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
            let mut dependency = self.source_path(&external.guid)?;
            if dependency.is_empty() {
                dependency = built_paths.get(&external.guid).cloned().unwrap_or_default();
            }
            if lower_ext(&dependency) != ".shader" || !dependency.starts_with("Assets/") {
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
        let mut programless = Vec::new();
        if matches!(lower_ext(logical).as_str(), ".shader" | ".shadergraph") {
            for object in playerbake::read_serialized(&baked.file)? {
                if object.tree.class_id == 48
                    && playerbake::verify_shader_programs(&object.tree, &object.data)? == 0
                {
                    programless.push(object.local_id.to_string());
                }
            }
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
                programless_shaders: programless.into(),
                ..Default::default()
            },
            ..Default::default()
        };
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
        self.shader_ingredients_in(None, logical, guid)
    }

    /// The same, taking a build's sources and settings from `shared` when
    /// given: one reading of them serves every shader of a pass.
    fn shader_ingredients_in(
        &self,
        shared: Option<&(std::sync::Arc<Sources>, BuildSettings)>,
        logical: &str,
        guid: &str,
    ) -> Result<ShaderIngredients> {
        if lower_ext(logical) != ".shader" || self.pipeline_project()? {
            let plan = self.built_plan_in(shared, logical, guid)?.1;
            return Ok((plan.ingredients, Vec::new()));
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
        let imports = sysutil::join(&[&self.opts.native_dir(), "imports"]);
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
}

impl NativeProjectImporter {
    /// Where a baked source lies. A package lies where the Editor
    /// compilation of the project says, which need not be the project's own
    /// `Library`.
    pub(super) fn baked_source(&self, logical: &str) -> Result<String> {
        if !logical.starts_with("Packages/") {
            return resolve_project_import_source(&self.opts.project, logical);
        }
        self.project_sources()?.physical(logical).ok_or_else(|| {
            Unsupported(format!(
                "{logical}, which lies in a package outside the project"
            ))
            .into()
        })
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
    pub(super) fn reload_list(&self) -> String {
        sysutil::join(&[
            &self.opts.project,
            "Library/UnityMicroEditor/pipeline-reloads.json",
        ])
    }

    /// The materials that use a source, and the plan of its build, which
    /// holds the hash of every file of the project the build depends on.
    fn built_plan(&self, logical: &str, guid: &str) -> Result<(Vec<String>, Plan)> {
        self.built_plan_in(None, logical, guid)
    }

    fn built_plan_in(
        &self,
        shared: Option<&(std::sync::Arc<Sources>, BuildSettings)>,
        logical: &str,
        guid: &str,
    ) -> Result<(Vec<String>, Plan)> {
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
        let plan = match shared {
            Some((sources, settings)) => plan_source_with(
                sources,
                settings,
                &self.opts.project,
                &self.opts.editor,
                logical,
                &users,
            )?,
            None => plan_source(
                &*self.project_sources()?,
                &self.opts.project,
                &self.opts.editor,
                &self.reload_list(),
                logical,
                &users,
            )?,
        };
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
        let identity = script_environment_identity(&self.opts.project);
        self.built_key_with(logical, guid, users, plan, &identity)
    }

    /// The build kept for a plan: under its key, or under the key an earlier
    /// version of the tool gave it, which then keeps it under the key too.
    /// Returns the key.
    fn kept_build(
        &self,
        logical: &str,
        guid: &str,
        users: &[String],
        plan: &Plan,
    ) -> Result<(String, Option<built_store::Built>)> {
        let project = &self.opts.project;
        let key = self.built_key(logical, guid, users, plan)?;
        if let Some(kept) = built_store::load(project, &key) {
            return Ok((key, Some(kept)));
        }
        let legacy = legacy_script_environment_identity(project);
        let earlier = self.built_key_with(logical, guid, users, plan, &legacy)?;
        let kept = (earlier != key)
            .then(|| built_store::load(project, &earlier))
            .flatten();
        if let Some(kept) = &kept {
            let _ = built_store::store(project, &key, logical, kept);
        }
        Ok((key, kept))
    }

    fn built_key_with(
        &self,
        logical: &str,
        guid: &str,
        users: &[String],
        plan: &Plan,
        scripts_identity: &str,
    ) -> Result<String> {
        let source = self.baked_source(logical)?;
        built_store::Inputs {
            importer: &importer_identity(),
            environment_sha256: &self.environment_sha256,
            scripts_identity,
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
        // Another importer of the project may have built the same already:
        // that of a content scene builds the shaders of the scene's own.
        let (key, kept) = self.kept_build(logical, guid, &users, &plan)?;
        if let Some(kept) = kept {
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
        let kept = build_source(ctx, &self.build_of(logical, users), guid)?;
        // Kept under what the build was planned from, and under what a
        // plan says now when the build itself added to the list of assets
        // the pipeline loads by path: the next plan is made with that list.
        self.keep_built(logical, guid, key, &kept);
        paths.extend(kept.external_paths.clone());
        Ok(Baked {
            file: kept.file,
            objects: kept.objects,
            externals: kept
                .externals
                .into_iter()
                .map(|guid| playerbake::External { guid })
                .collect(),
        })
    }

    /// The build of a source for the materials `users`.
    pub(super) fn build_of(&self, logical: &str, users: Vec<String>) -> Build {
        Build {
            project: self.opts.project.clone(),
            editor: self.opts.editor.clone(),
            output: sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]),
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
        }
    }

    /// Stores a build under `key` and under the key a plan gives now.
    fn keep_built(&self, logical: &str, guid: &str, key: String, kept: &built_store::Built) {
        let mut keys = vec![key];
        if let Ok((users, plan)) = self.built_plan(logical, guid)
            && let Ok(now) = self.built_key(logical, guid, &users, &plan)
            && !keys.contains(&now)
        {
            keys.push(now);
        }
        for key in keys {
            // Only a store: without it the next importer builds again.
            if let Err(error) = built_store::store(&self.opts.project, &key, logical, kept) {
                Event::new("native-build-not-kept")
                    .with("path", logical)
                    .with("reason", &format!("{error:#}"))
                    .emit();
            }
        }
    }
}

/// Has the Editor's own build code write the Player object of a shader, a
/// shader graph or a compute shader in a standalone worker, and checks what
/// it wrote.
fn build_source(ctx: &Context, build: &Build, guid: &str) -> Result<built_store::Built> {
    let logical = build.target.as_str();
    let (class, local_id, type_name) = if lower_ext(logical) == ".compute" {
        (
            playerbake::COMPUTE_SHADER_CLASS,
            playerbake::COMPUTE_SHADER_LOCAL_ID,
            "UnityEngine.ComputeShader",
        )
    } else {
        (48, playerbake::SHADER_LOCAL_ID, "UnityEngine.Shader")
    };
    let built = write_player_objects(ctx, build)?;
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
    Ok(built_store::Built {
        file,
        objects: vec![(local_id.to_string(), type_name.into())],
        externals: externals
            .into_iter()
            .map(|external| external.guid)
            .collect(),
        external_paths,
        sources: Vec::new(),
        resources: Vec::new(),
        roots: BTreeMap::new(),
    })
}

fn file_sha(path: &str) -> Result<String> {
    Ok(sha256(&read_file(path)?))
}

impl NativeProjectImporter {
    /// The shader sources the project's render pipeline names through its
    /// settings: the shaders, compute shaders and graphs the graphics and
    /// quality settings reach over settings assets (`.asset`), as the
    /// pipeline's global settings and resources hold them.
    pub(super) fn pipeline_shader_sources(&self) -> Result<Vec<(String, String)>> {
        static GUID: std::sync::LazyLock<regex::bytes::Regex> =
            std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"guid: ([0-9a-f]{32})").unwrap());
        let sources = self.project_sources()?;
        let mut pending: Vec<String> = Vec::new();
        for settings in &SHADER_SETTINGS[..2] {
            match resolve_project_import_source(&self.opts.project, settings) {
                Ok(source) => pending.push(source),
                Err(error) if is_not_exist(&error) => {}
                Err(error) => return Err(error),
            }
        }
        let mut seen = BTreeSet::new();
        let mut found = BTreeMap::new();
        let mut read = 0;
        while let Some(file) = pending.pop() {
            read += 1;
            if read > 512 {
                break;
            }
            let data = read_file(&file)?;
            if data.len() > 16 << 20 {
                continue;
            }
            for capture in GUID.captures_iter(&data) {
                let guid = String::from_utf8_lossy(&capture[1]).into_owned();
                if !seen.insert(guid.clone()) {
                    continue;
                }
                let Some(path) = sources.path_of(&guid) else {
                    continue;
                };
                // An Editor folder holds the Editor's own resources, which no
                // Player build holds.
                if path.contains("/Editor/") {
                    continue;
                }
                if shader_source(path) {
                    found.insert(path.to_string(), guid);
                } else if lower_ext(path) == ".asset"
                    && let Some(physical) = sources.physical(path)
                {
                    pending.push(physical);
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    /// Builds side by side, once per project and import environment in a
    /// process, the shader sources of the pipeline that are neither
    /// published nor kept in the project's store, and keeps them there. The
    /// importer asks for them one by one as it meets them; each then finds
    /// its build in the store. A build is found only under the hash of all
    /// its inputs, so one made for nothing costs time, not correctness.
    fn prebuild_pipeline_shaders(&self, ctx: &Context) {
        static DONE: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
        let once = format!("{}\0{}", self.opts.project, self.environment_sha256);
        if !DONE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(once)
        {
            return;
        }
        let started = std::time::Instant::now();
        let sources = match self.pipeline_shader_sources() {
            Ok(sources) => sources,
            Err(error) => {
                Event::new("native-prebuild-skipped")
                    .with("reason", &format!("{error:#}"))
                    .emit();
                return;
            }
        };
        let mut jobs = Vec::new();
        for (logical, guid) in sources {
            if self.entries.contains_key(&guid) {
                continue;
            }
            let Ok((users, plan)) = self.built_plan(&logical, &guid) else {
                continue;
            };
            let Ok((key, kept)) = self.kept_build(&logical, &guid, &users, &plan) else {
                continue;
            };
            if kept.is_some() {
                continue;
            }
            jobs.push((guid, key, self.build_of(&logical, users)));
        }
        if jobs.is_empty() {
            return;
        }
        // A worker run starts a whole Editor process and keeps few cores
        // busy for most of it; with its shader compilers it holds up to
        // about 2 GiB.
        let workers = std::thread::available_parallelism()
            .map_or(2, |count| count.get() / 3)
            .min(available_memory().map_or(usize::MAX, |bytes| (bytes / (2 << 30)) as usize))
            .clamp(1, 6)
            .min(jobs.len());
        Event::new("native-prebuild")
            .with("sources", &jobs.len())
            .with("workers", &workers)
            .emit();
        let queue = std::sync::Mutex::new(jobs.into_iter());
        let results = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let next = queue
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .next();
                        let Some((guid, key, build)) = next else {
                            break;
                        };
                        if ctx.err().is_err() {
                            break;
                        }
                        let result = build_source(ctx, &build, &guid);
                        results
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((guid, key, build.target, result));
                    }
                });
            }
        });
        let results = results
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut failed = 0;
        for (guid, key, logical, result) in results {
            match result {
                Ok(kept) => self.keep_built(&logical, &guid, key, &kept),
                // The importer builds it again when it asks for it, and
                // reports what fails then.
                Err(error) => {
                    failed += 1;
                    Event::new("native-prebuild-failed")
                        .with("path", &logical)
                        .with("reason", &format!("{error:#}"))
                        .emit();
                }
            }
        }
        Event::new("native-prebuilt")
            .with("failed", &failed)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
    }
}

/// The memory the system can give without swapping (`MemAvailable`).
fn available_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}
