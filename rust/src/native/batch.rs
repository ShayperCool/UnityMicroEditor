//! Cold native imports: batches of verified profiles run through one
//! standalone worker and adopted into the session cache.
use super::{
    NativeOptions, NativeProjectImporter, ProbeOptions, ProbeReport, ProjectImportedAsset,
    ProjectImportedFile,
    environment::read_worker_environment,
    import::run_project_import,
    mesh_cpu::retain_native_model_mesh_cpu,
    resolve_project_import_source,
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
        ".wav" => Some("wav"),
        _ if texture_source(logical) => Some("texture"),
        _ => None,
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

pub(crate) fn cold_input(
    opts: &NativeOptions,
    environment_sha256: &str,
    logical: &str,
) -> Result<NativeColdInput> {
    if !cold_native_profile(logical) {
        bail!("unsupported native cold profile: {logical}");
    }
    let source = resolve_project_import_source(&opts.project, logical)?;
    let sha = file_hash(&source)?;
    let meta = read_file(&format!("{source}.meta"))?;
    let guid = meta_guid(&meta)?;
    let meta_sha = sha256(&meta);
    let signature = sha256(
        format!("native-runtime-v3\0{environment_sha256}\0{logical}\0{sha}\0{meta_sha}").as_bytes(),
    );
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
        let unique: BTreeSet<String> = self
            .pending_native_paths
            .iter()
            .cloned()
            .chain(discovered)
            .collect();
        let paths: Vec<String> = unique.into_iter().collect();
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
        self.pending_native_paths = cold.clone();
        let mut paths = &cold[..];
        while !paths.is_empty() {
            let count = NATIVE_IMPORT_BATCH_SIZE.min(paths.len());
            self.import_cold_batch(ctx, &paths[..count])?;
            paths = &paths[count..];
            self.pending_native_paths = paths.to_vec();
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
        for asset in &content.assets {
            let input = match inputs.remove(&asset.path) {
                Some(input) if asset.guid == input.guid => input,
                _ => bail!("native cold batch result differs from its source path/GUID"),
            };
            let filename = sysutil::join(&[root, "runtime", &format!("{}.bin", asset.guid)]);
            replacements.push(self.adapt_cold_asset(asset, &input, &filename)?);
        }
        if !inputs.is_empty() {
            bail!("native cold batch omitted an input asset");
        }
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

    fn adapt_cold_asset(
        &self,
        asset: &ProjectImportedAsset,
        input: &NativeColdInput,
        filename: &str,
    ) -> Result<NativeReplacement> {
        adapt_cold_asset(&self.opts, &self.environment_sha256, asset, input, filename)
    }
}

/// Projects one imported asset to its runtime artifact and descriptor.
pub(crate) fn adapt_cold_asset(
    opts: &NativeOptions,
    environment_sha256: &str,
    asset: &ProjectImportedAsset,
    input: &NativeColdInput,
    filename: &str,
) -> Result<NativeReplacement> {
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
    let data = match read_file(&primary.path) {
        Ok(data) if sha256(&data) == primary.sha256 => data,
        _ => bail!("native primary artifact changed"),
    };
    let extension = lower_ext(&input.path);
    let adapted = match cold_runtime_profile(&input.path) {
        Some("texture") => {
            runtimeartifact::adapt_texture(&data, extension.strip_prefix('.').unwrap_or(&extension))
        }
        Some(profile) => runtimeartifact::adapt(&data, profile),
        None => bail!("unsupported native cold profile: {}", input.path),
    };
    let profile = extension.strip_prefix('.').unwrap_or(&extension);
    let (output, metadata) =
        adapted.map_err(|e| wrap(&format!("adapt native {}", input.path), e))?;
    if profile == "fbx" && asset.files.len() != 1 {
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
    let graph: Graph = crate::project::value::decode(&encoded)?;
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
