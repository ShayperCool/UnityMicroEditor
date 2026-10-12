//! Cold native imports: batches of verified profiles run through one
//! standalone worker and adopted into the session cache.
use super::{
    NativeOptions, NativeProjectImporter, ProbeOptions, ProbeReport, ProjectImportedAsset,
    ProjectImportedFile,
    environment::read_worker_environment,
    import::run_project_import,
    mesh_cpu::retain_native_model_mesh_cpu,
    resolve_project_import_source,
    scripted::{generated_material_profile, script_environment_identity, scripted_profile},
    support::{Event, check, join, lower_ext, mkdir_all, read_file, wrap, write_file},
    texture_profile::{native_texture_needs_warm, texture_source, warm_texture_configuration},
};
use crate::{
    cancel::Context,
    project::{
        List, NativeContentDependency, NativeContentFile, NativeContentObject,
        NativeContentResource, NativeModelGraphObject, NativeReplacement, NeedsImportError,
    },
    registry::meta_guid,
    runtimeartifact, sha256, sysutil,
    util::{atomic_json, file_hash, random_id},
};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    time::{Duration, Instant},
};

pub const NATIVE_IMPORT_BATCH_SIZE: usize = 32;

pub fn cold_native_profile(logical: &str) -> bool {
    cold_runtime_profile(logical).is_some()
}

/// The runtime adapter profile of a source a standalone worker imports.
fn cold_runtime_profile(logical: &str) -> Option<&'static str> {
    match lower_ext(logical).as_str() {
        ".fbx" => Some("fbx"),
        extension if generated_material_profile(extension) => Some("fbx"),
        _ if texture_source(logical) => Some("texture"),
        extension if runtimeartifact::audio_profile(extension.trim_start_matches('.')) => {
            Some("wav")
        }
        extension if runtimeartifact::text_profile(extension.trim_start_matches('.')) => {
            Some("text")
        }
        extension if scripted_profile(extension) => Some("scripted"),
        super::atlas::ATLAS_EXTENSION => Some("atlas"),
        // The one container the engine reads video from on Linux.
        ".webm" => Some("video"),
        _ => None,
    }
}

/// What a source's import context holds beyond the sources: nothing, the
/// Editor's shader name table, or compiled importers.
fn context_kind(logical: &str) -> u8 {
    let extension = lower_ext(logical);
    if scripted_profile(&extension) {
        2
    } else if generated_material_profile(&extension) {
        1
    } else {
        0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NativeColdInput {
    pub path: String,
    pub guid: String,
    pub source_sha: String,
    pub meta_sha: String,
    pub cache_path: String,
}

/// `binding` is what the import depends on beyond its source and metadata:
/// for a texture the atlas that packs it, for an atlas its textures.
pub(crate) fn cold_input(
    opts: &NativeOptions,
    environment_sha256: &str,
    logical: &str,
    binding: &str,
) -> Result<NativeColdInput> {
    if !cold_native_profile(logical) {
        bail!("unsupported native cold profile: {logical}");
    }
    let source = resolve_project_import_source(&opts.project, logical)?;
    let sha = file_hash(&source)?;
    let meta = read_file(&format!("{source}.meta"))?;
    let guid = meta_guid(&meta)?;
    let meta_sha = sha256(&meta);
    let mut identity =
        format!("native-runtime-v3\0{environment_sha256}\0{logical}\0{sha}\0{meta_sha}");
    if cold_runtime_profile(logical) == Some("scripted") {
        // The importer is compiled code of the project: another compilation
        // may import the same source differently.
        identity.push('\0');
        identity.push_str(&script_environment_identity(&opts.project));
    }
    if !binding.is_empty() {
        identity.push('\0');
        identity.push_str(binding);
    }
    let signature = sha256(identity.as_bytes());
    Ok(NativeColdInput {
        path: logical.into(),
        guid,
        source_sha: sha,
        meta_sha,
        cache_path: sysutil::join(&[&opts.output, "native/cache", &format!("{signature}.json")]),
    })
}

pub(crate) const GLOBAL_GUIDS: [&str; 2] = [
    "0000000000000000f000000000000000",
    "0000000000000000e000000000000000",
];

impl NativeProjectImporter {
    pub(crate) fn refresh_pending_native(&mut self, ctx: &Context) -> Result<()> {
        let started = Instant::now();
        // The walk stops at a changed source too large to read as a whole. A
        // clip is baked from it, after which the walk goes on.
        let mut baked = HashSet::new();
        let discovered = loop {
            let error = match self.session.pending_native_import_sources() {
                Ok(discovered) => break discovered,
                Err(error) => error,
            };
            let source = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<NeedsImportError>())
                .map(|needed| needed.path.strip_suffix(".meta").unwrap_or(&needed.path))
                .filter(|source| lower_ext(source) == ".anim")
                .map(str::to_string);
            match source {
                Some(source) if baked.insert(source.clone()) => self.import_asset(ctx, &source)?,
                _ => return Err(error),
            }
        };
        // A model that only controllers refer to: bake those controllers
        // again, which bakes its clips; one that cannot be baked is reported.
        for (guid, path) in self.session.changed_clip_models() {
            let current = self.model_clip_source_current(&guid, &path);
            if !current {
                self.refresh_dependents(ctx, &guid, &path, 0)?;
            }
        }
        for mask in self.session.controller_masks()? {
            self.refresh_mask_dependents(ctx, &mask)?;
        }
        self.refresh_shaders(ctx)?;
        self.build_package_shaders(ctx)?;
        let unique: BTreeSet<String> = self
            .pending_native_paths
            .iter()
            .cloned()
            .chain(discovered)
            .collect();
        let (paths, atlas_batches) = self.atlas_batches(unique.into_iter().collect())?;
        self.pending_native_paths = paths.clone();
        Event::new("native-discovery")
            .with("assetCount", &paths.len())
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        let mut cold = Vec::new();
        for logical in paths {
            if native_texture_needs_warm(&self.opts.project, &logical)? {
                self.import_warm_ui(ctx, &logical, &mut HashSet::new())?;
            } else {
                cold.push(logical);
            }
        }
        // Imports that need more of the Editor in their context run apart
        // from the others, so that what an asset is imported with does not
        // depend on what changed along with it.
        cold.sort_by_key(|logical| context_kind(logical));
        self.pending_native_paths = cold.clone();
        let mut paths = &cold[..];
        while !paths.is_empty() {
            let kind = context_kind(&paths[0]);
            let count = paths
                .iter()
                .take(NATIVE_IMPORT_BATCH_SIZE)
                .take_while(|logical| context_kind(logical) == kind)
                .count();
            self.import_cold_batch(ctx, &paths[..count])?;
            paths = &paths[count..];
            self.pending_native_paths = paths.to_vec();
        }
        for batch in atlas_batches {
            self.import_cold_batch(ctx, &batch)?;
        }
        Ok(())
    }

    pub(crate) fn import_cold_batch(&mut self, ctx: &Context, logicals: &[String]) -> Result<()> {
        self.require_current_environment()?;
        if logicals.is_empty() || logicals.len() > NATIVE_IMPORT_BATCH_SIZE {
            bail!("native cold import batch exceeds supported asset count");
        }
        let mut inputs = BTreeMap::new();
        let mut guids = HashSet::new();
        for logical in logicals {
            let input = self.cold_input(logical)?;
            if inputs.contains_key(logical) || !guids.insert(input.guid.clone()) {
                bail!("native cold import batch has a duplicate path or GUID");
            }
            inputs.insert(logical.clone(), input);
        }
        let root = sysutil::join(&[&self.opts.output, "native/imports", &random_id()]);
        mkdir_all(&sysutil::dir(&root), 0o700)?;
        let import_context = ctx.with_timeout(Duration::from_secs(45));
        let (report, error) = run_project_import(
            &import_context,
            ProbeOptions {
                project: self.opts.project.clone(),
                editor: self.opts.editor.clone(),
                output: root.clone(),
                timeout: Duration::from_secs(45),
                ..Default::default()
            },
            logicals,
        );
        import_context.cancel();
        let report_path = sysutil::join(&[&root, "result.json"]);
        if let Some(report) = &report
            && let Err(write) = atomic_json(&report_path, report)
        {
            return Err(join([error, Some(write)]).unwrap());
        }
        if let Some(error) = error {
            return Err(error);
        }
        self.adopt_cold_report(ctx, report.as_ref(), &root, &report_path, inputs, logicals)
    }

    pub(crate) fn adopt_cold_report(
        &mut self,
        ctx: &Context,
        report: Option<&ProbeReport>,
        root: &str,
        report_path: &str,
        mut inputs: BTreeMap<String, NativeColdInput>,
        logicals: &[String],
    ) -> Result<()> {
        check(ctx)?;
        let environment = read_worker_environment(&self.opts.project);
        let report = match (report, environment) {
            (Some(report), Ok(environment))
                if report.environment.as_ref() == Some(&environment)
                    && !report.main_editor_started
                    && report.worker_exit == "exit 0" =>
            {
                report
            }
            _ => bail!(
                "native batch report does not prove the current standalone worker environment"
            ),
        };
        let content = match &report.content {
            Some(content)
                if report.success
                    && content.source_unchanged
                    && content.assets.len() == inputs.len() =>
            {
                content
            }
            _ => bail!("native worker produced no complete validated project batch"),
        };
        let mut replacements = Vec::new();
        // The atlas of the batch first: the textures it packs are bound to it.
        let mut atlas: Option<(NativeReplacement, runtimeartifact::PackedAtlas)> = None;
        for asset in &content.assets {
            if lower_ext(&asset.path) != super::atlas::ATLAS_EXTENSION {
                continue;
            }
            let Some(input) = inputs
                .get(&asset.path)
                .filter(|input| asset.guid == input.guid)
            else {
                bail!("native cold batch result differs from its source path/GUID");
            };
            if atlas.is_some() {
                bail!("native cold batch holds more than one sprite atlas");
            }
            let filename = sysutil::join(&[root, "runtime", &format!("{}.bin", asset.guid)]);
            let mut ingredients = BTreeMap::new();
            for member in inputs.values().filter(|member| member.path != input.path) {
                ingredients.insert(member.path.clone(), member.source_sha.clone());
                ingredients.insert(format!("{}.meta", member.path), member.meta_sha.clone());
            }
            atlas = Some(adapt_cold_atlas(
                &self.environment_sha256,
                asset,
                input,
                &filename,
                ingredients,
            )?);
        }
        for asset in &content.assets {
            let input = match inputs.remove(&asset.path) {
                Some(input) if asset.guid == input.guid => input,
                _ => bail!("native cold batch result differs from its source path/GUID"),
            };
            if lower_ext(&asset.path) == super::atlas::ATLAS_EXTENSION {
                continue;
            }
            if atlas.is_none() && self.atlas_identity.contains_key(&asset.path) {
                bail!(
                    "{} is packed by a sprite atlas and has to be imported with it",
                    asset.path
                );
            }
            let filename = sysutil::join(&[root, "runtime", &format!("{}.bin", asset.guid)]);
            let binding = atlas.as_ref().map(|(atlas, packed)| (atlas, packed));
            replacements.push(adapt_cold_asset(
                &self.opts,
                &self.environment_sha256,
                asset,
                &input,
                &filename,
                binding,
            )?);
        }
        if !inputs.is_empty() {
            bail!("native cold batch omitted an input asset");
        }
        replacements.extend(atlas.map(|(atlas, _)| atlas));
        self.require_current_environment()?;
        let ordered = order_native_batch(replacements, &self.entries)?;
        let count = ordered.len();
        for replacement in ordered {
            self.session.replace_native_content(&replacement)?;
            self.entries
                .insert(replacement.guid.clone(), replacement.clone());
            let input = match self.cold_input(&replacement.source_path) {
                Ok(input)
                    if input.source_sha == replacement.source_sha256
                        && input.meta_sha == replacement.meta_sha256 =>
                {
                    input
                }
                _ => bail!("native cold batch source changed before cache publication"),
            };
            mkdir_all(&sysutil::dir(&input.cache_path), 0o700)?;
            atomic_json(&input.cache_path, &replacement)?;
        }
        self.publish_index()?;
        Event::new("native-imported")
            .with("assetCount", &count)
            .with("paths", logicals)
            .with("elapsedSeconds", &report.elapsed_seconds)
            .with("workerPeakRssKiB", &report.worker_peak_rss_kib)
            .with("report", report_path)
            .emit();
        Ok(())
    }
}

/// The one serialized output of an imported asset, checked against the hash
/// its import recorded.
fn primary_artifact(asset: &ProjectImportedAsset) -> Result<Vec<u8>> {
    let mut primary: Option<&ProjectImportedFile> = None;
    for file in &asset.files {
        if file.name.is_empty() {
            if primary.is_some() {
                bail!("native asset has multiple serialized outputs");
            }
            primary = Some(file);
        }
    }
    let Some(primary) = primary else {
        bail!("native asset has no serialized primary output");
    };
    match read_file(&primary.path) {
        Ok(data) if sha256(&data) == primary.sha256 => Ok(data),
        _ => bail!("native primary artifact changed"),
    }
}

/// Projects an imported sprite atlas to the atlas a Player loads. Its
/// ingredients are the textures it was packed from.
fn adapt_cold_atlas(
    environment_sha256: &str,
    asset: &ProjectImportedAsset,
    input: &NativeColdInput,
    filename: &str,
    ingredients: BTreeMap<String, String>,
) -> Result<(NativeReplacement, runtimeartifact::PackedAtlas)> {
    if asset.files.len() != 1 {
        bail!("a sprite atlas with streamed resources is not supported");
    }
    let (output, metadata, packed) = runtimeartifact::adapt_sprite_atlas(&primary_artifact(asset)?)
        .map_err(|e| wrap(&format!("adapt native {}", input.path), e))?;
    mkdir_all(&sysutil::dir(filename), 0o700)?;
    write_file(filename, &output, 0o600)?;
    let mut result = NativeReplacement {
        guid: asset.guid.clone(),
        source_path: input.path.clone(),
        source_sha256: input.source_sha.clone(),
        meta_sha256: input.meta_sha.clone(),
        environment_sha256: environment_sha256.to_string(),
        ingredient_sha256: ingredients,
        content: NativeContentFile {
            guid: asset.guid.clone(),
            path: filename.into(),
            sha256: metadata.output_sha256.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    for object in &metadata.objects {
        result.content.objects.push(NativeContentObject {
            file_id: object.file_id.clone(),
            type_name: object.type_name.clone(),
        });
    }
    Ok((result, packed))
}

/// The host binding of a model that names objects built into the engine and
/// has no generated material to bind.
const NO_HOST_BINDING: &str = "none";

fn names_builtin_objects<'a>(mut guids: impl Iterator<Item = &'a str>) -> bool {
    guids.any(|guid| guid == runtimeartifact::BUILTIN_EXTRA_GUID)
}

/// Whether a model may be replayed for the host Player this importer feeds:
/// its generated materials were bound to the shaders of that Player. A model
/// that names built-in objects and records no binding was adapted before
/// materials were bound, and its material leads nowhere in a Player.
pub(crate) fn host_binding_current(opts: &NativeOptions, entry: &NativeReplacement) -> bool {
    match entry.host_binding.as_str() {
        "" => {
            cold_runtime_profile(&entry.source_path) != Some("fbx")
                || !names_builtin_objects(
                    entry
                        .content
                        .dependencies
                        .iter()
                        .map(|dependency| dependency.guid.as_str()),
                )
        }
        NO_HOST_BINDING => true,
        identity => super::host_shaders::host_shaders(&opts.player)
            .is_ok_and(|shaders| shaders.identity == identity),
    }
}

/// Gives the materials a model importer generated the shaders of the host
/// Player, and returns the binding to record: what stands for those shaders,
/// `none` for a model that names built-in objects without such a material,
/// nothing for any other asset. A build writes a generated material with the
/// shader the Editor gave it; where the session cannot show the same, the
/// model is refused.
fn bind_generated_materials(
    opts: &NativeOptions,
    logical: &str,
    output: &mut Vec<u8>,
    metadata: &mut runtimeartifact::Metadata,
) -> Result<String> {
    if cold_runtime_profile(logical) != Some("fbx")
        || !names_builtin_objects(
            metadata
                .external_references
                .iter()
                .map(|external| external.guid.as_str()),
        )
    {
        return Ok(String::new());
    }
    // With the legacy material mode the Editor names the built-in shader in
    // every project; otherwise a render pipeline's importer script makes the
    // material, and a standalone worker does not run it.
    let pipeline = super::host_shaders::names_render_pipeline(&opts.project)
        && !resolve_project_import_source(&opts.project, logical)
            .and_then(|source| read_file(&format!("{source}.meta")))
            .is_ok_and(|meta| String::from_utf8_lossy(&meta).contains("materialImportMode: 1"));
    let mut shaders = None;
    let bound = runtimeartifact::bind_builtin_shaders(output, metadata, &mut |used| {
        if pipeline {
            bail!(
                "the project has a scriptable render pipeline, whose importer script makes the material {} of a model in the Editor (the Universal pipeline gives it its Lit shader); a standalone worker does not run that script and names a built-in shader instead. Assign the model's materials from the project or import it without materials",
                used.material
            );
        }
        if shaders.is_none() {
            shaders = Some(super::host_shaders::host_shaders(&opts.player)?);
        }
        shaders.as_ref().unwrap().resolve(used)
    })?;
    if let Some(bound) = bound {
        *output = bound;
    }
    Ok(shaders.map_or_else(
        || NO_HOST_BINDING.to_string(),
        |shaders| shaders.identity.clone(),
    ))
}

/// Projects one imported asset to its runtime artifact and descriptor.
/// `atlas` is the sprite atlas imported with it, if any: a texture whose
/// sprites the atlas packs is bound to it.
pub(crate) fn adapt_cold_asset(
    opts: &NativeOptions,
    environment_sha256: &str,
    asset: &ProjectImportedAsset,
    input: &NativeColdInput,
    filename: &str,
    atlas: Option<(&NativeReplacement, &runtimeartifact::PackedAtlas)>,
) -> Result<NativeReplacement> {
    let data = primary_artifact(asset)?;
    let extension = lower_ext(&input.path);
    let mut source_resource = false;
    let adapted = match cold_runtime_profile(&input.path) {
        Some("video") => {
            runtimeartifact::adapt_video(&data, &asset.guid).map(|(output, metadata, source)| {
                source_resource = source;
                (output, metadata)
            })
        }
        Some("texture") => {
            runtimeartifact::adapt_texture(&data, extension.strip_prefix('.').unwrap_or(&extension))
        }
        Some("scripted") => runtimeartifact::adapt_scripted(&data),
        Some(profile) => runtimeartifact::adapt(&data, profile),
        None => bail!("unsupported native cold profile: {}", input.path),
    };
    let (mut output, mut metadata) =
        adapted.map_err(|e| wrap(&format!("adapt native {}", input.path), e))?;
    let host_binding = bind_generated_materials(opts, &input.path, &mut output, &mut metadata)
        .map_err(|e| wrap(&format!("generated material of {}", input.path), e))?;
    let packed = atlas.filter(|(_, packed)| {
        cold_runtime_profile(&input.path) == Some("texture")
            && packed.sprites.iter().any(|(guid, _)| *guid == asset.guid)
    });
    if let Some((atlas, packed)) = packed {
        output = runtimeartifact::bind_packed_sprites(
            &output,
            &mut metadata,
            &asset.guid,
            &atlas.guid,
            packed,
        )
        .map_err(|e| wrap(&format!("bind {} to its sprite atlas", input.path), e))?;
    }
    if cold_runtime_profile(&input.path) == Some("fbx") && asset.files.len() != 1 {
        bail!("streamed FBX resources are not supported by the runtime adapter");
    }
    mkdir_all(&sysutil::dir(filename), 0o700)?;
    write_file(filename, &output, 0o600)?;
    let mut result = NativeReplacement {
        guid: asset.guid.clone(),
        source_path: input.path.clone(),
        source_sha256: input.source_sha.clone(),
        meta_sha256: input.meta_sha.clone(),
        environment_sha256: environment_sha256.to_string(),
        content: NativeContentFile {
            guid: asset.guid.clone(),
            path: filename.into(),
            sha256: metadata.output_sha256.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    result.config_source_sha256 = warm_texture_configuration(&opts.project, &input.path)?;
    result.host_binding = host_binding;
    if let Some((atlas, _)) = packed {
        // Bound to this revision of the atlas.
        result
            .ingredient_sha256
            .insert(atlas.source_path.clone(), atlas.source_sha256.clone());
        result.ingredient_sha256.insert(
            format!("{}.meta", atlas.source_path),
            atlas.meta_sha256.clone(),
        );
    }
    for object in &metadata.objects {
        result.content.objects.push(NativeContentObject {
            file_id: object.file_id.clone(),
            type_name: object.type_name.clone(),
        });
    }
    for dependency in &metadata.external_references {
        result.content.dependencies.push(NativeContentDependency {
            guid: dependency.guid.clone(),
            global: GLOBAL_GUIDS.contains(&dependency.guid.as_str()),
        });
    }
    for resource in &metadata.resources {
        if resource.virtual_path != format!("VirtualArtifacts/Primary/{}.resource", asset.guid) {
            bail!("native resource alias differs from source GUID");
        }
        if source_resource {
            // The clip's frames are the source file: keep the revision the
            // clip was imported from next to the clip.
            let source = resolve_project_import_source(&opts.project, &input.path)?;
            let contents = read_file(&source)?;
            if sha256(&contents) != input.source_sha
                || resource.offset != 0
                || resource.size != contents.len() as u64
            {
                bail!("video source differs from the imported clip");
            }
            let path = format!("{}.source.resource", filename.trim_end_matches(".bin"));
            write_file(&path, &contents, 0o600)?;
            result.content.resources.push(NativeContentResource {
                path,
                virtual_path: resource.virtual_path.clone(),
                sha256: input.source_sha.clone(),
            });
            continue;
        }
        let mut found: Option<&ProjectImportedFile> = None;
        for file in &asset.files {
            if file.name == ".resource" {
                if found.is_some() {
                    bail!("multiple native resource outputs");
                }
                found = Some(file);
            }
        }
        let found = match found {
            Some(found)
                if resource.offset <= found.bytes
                    && resource.size <= found.bytes - resource.offset =>
            {
                found
            }
            _ => bail!("native resource file is missing or has invalid bounds"),
        };
        match read_file(&found.path) {
            Ok(data) if data.len() as u64 == found.bytes && sha256(&data) == found.sha256 => {}
            _ => bail!("native resource content changed"),
        }
        result.content.resources.push(NativeContentResource {
            path: found.path.clone(),
            virtual_path: resource.virtual_path.clone(),
            sha256: found.sha256.clone(),
        });
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Graph {
        #[serde(rename = "modelGraph")]
        model_graph: List<NativeModelGraphObject>,
    }
    let encoded = serde_json::to_string(&metadata)?;
    // The graph's hash covers its numbers as written: `1.0` must not come
    // back as `1`.
    let graph: Graph = crate::project::value::decode_use_number(&encoded)?;
    result.model_graph = graph.model_graph;
    result.model_graph_sha256 = metadata.model_topology_sha256.clone();
    retain_native_model_mesh_cpu(opts, result)
}

/// Dependencies first; within the batch entries are visited in GUID order.
pub fn order_native_batch(
    entries: Vec<NativeReplacement>,
    loaded: &BTreeMap<String, NativeReplacement>,
) -> Result<Vec<NativeReplacement>> {
    let mut by_guid: BTreeMap<String, NativeReplacement> = BTreeMap::new();
    for entry in entries {
        if by_guid.contains_key(&entry.guid) {
            bail!("duplicate GUID in native adapted batch");
        }
        by_guid.insert(entry.guid.clone(), entry);
    }
    struct Walk<'a> {
        by_guid: &'a BTreeMap<String, NativeReplacement>,
        loaded: &'a BTreeMap<String, NativeReplacement>,
        visiting: HashSet<String>,
        visited: HashSet<String>,
        ordered: Vec<NativeReplacement>,
    }
    fn visit(walk: &mut Walk<'_>, guid: &str) -> Result<()> {
        if walk.visited.contains(guid) {
            return Ok(());
        }
        let by_guid = walk.by_guid;
        let Some(entry) = by_guid.get(guid) else {
            if walk.loaded.contains_key(guid) {
                return Ok(());
            }
            bail!("native batch dependency has no imported runtime descriptor: {guid}");
        };
        if walk.visiting.contains(guid) {
            bail!("native batch dependency cycle: {guid}");
        }
        walk.visiting.insert(guid.to_string());
        for dependency in entry.content.dependencies.iter() {
            if !dependency.global {
                visit(walk, &dependency.guid)?;
            }
        }
        walk.visiting.remove(guid);
        walk.visited.insert(guid.to_string());
        walk.ordered.push(entry.clone());
        Ok(())
    }
    let mut walk = Walk {
        by_guid: &by_guid,
        loaded,
        visiting: HashSet::new(),
        visited: HashSet::new(),
        ordered: Vec::new(),
    };
    for guid in by_guid.keys() {
        visit(&mut walk, guid)?;
    }
    Ok(walk.ordered)
}
