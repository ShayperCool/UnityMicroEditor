//! `project-import`: an owned context, one worker run and validated artifacts.
use super::{
    ArtifactExtraction, JsonF64, NativeImportJob, ProbeOptions, ProbeReport, ProjectImportContext,
    UdsArtifactStore,
    context::create_scripted_import_context,
    probe::{rss_kib, run_probe},
    resolve_project_import_source,
    scripted::{
        ScriptEnvironment, generated_material_profile, script_environment, scripted_importer,
        shader_name_registry,
    },
    support::{
        check, context_error, join, lower_ext, mkdir, nul, read_file, text, wrap, write_file,
    },
    verify_project_import_sources,
};
use crate::{cancel::Context, flags::FlagSet, jsondoc, registry, sysutil, util};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::Write,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectImportedFile {
    #[serde(deserialize_with = "nul")]
    pub name: String,
    #[serde(deserialize_with = "nul")]
    pub path: String,
    #[serde(deserialize_with = "nul")]
    pub hash: String,
    #[serde(deserialize_with = "nul")]
    pub sha256: String,
    #[serde(deserialize_with = "nul")]
    pub bytes: u64,
    #[serde(deserialize_with = "nul")]
    pub proof: ArtifactExtraction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectImportedAsset {
    #[serde(deserialize_with = "nul")]
    pub path: String,
    #[serde(deserialize_with = "nul")]
    pub guid: String,
    #[serde(deserialize_with = "nul")]
    pub result_id: String,
    #[serde(deserialize_with = "nul")]
    pub files: Vec<ProjectImportedFile>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectImportContent {
    #[serde(deserialize_with = "nul")]
    pub project: String,
    #[serde(deserialize_with = "nul")]
    pub source_unchanged: bool,
    #[serde(deserialize_with = "nul")]
    pub assets: Vec<ProjectImportedAsset>,
    pub elapsed_seconds: JsonF64,
}

/// A context that is canceled on SIGINT or SIGTERM.
pub(crate) fn signal_context() -> Result<Context> {
    use tokio::signal::unix::{SignalKind, signal};
    let ctx = Context::background().with_cancel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (mut interrupt, mut terminate) = {
        let _guard = runtime.enter();
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
        )
    };
    let cancel = ctx.clone();
    std::thread::spawn(move || {
        runtime.block_on(async {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        });
        cancel.cancel();
    });
    Ok(ctx)
}

fn asset_flag(assets: &[String], _: &str) -> Result<(), String> {
    if assets.len() >= 4096 {
        return Err("project import exceeds source asset count bound".into());
    }
    Ok(())
}

pub fn run_project_import_cli(args: &[String]) -> Result<()> {
    let mut flags = FlagSet::new("project-import");
    flags.string(
        "editor",
        &util::default_unity_editor(),
        "supported native Unity executable",
    );
    flags.string("project", ".", "source Unity project, read only");
    flags.string(
        "output",
        "",
        "new directory beneath project Library/UnityMicroEditor or Temp",
    );
    flags.list(
        "asset",
        asset_flag,
        "native source asset path; repeat for multiple imports",
    );
    flags.duration(
        "timeout",
        Duration::from_secs(45),
        "whole import deadline, at most 5m",
    );
    flags.parse(args.iter().cloned())?;
    let assets = flags.get_list("asset");
    let timeout = flags.get_duration("timeout");
    let output = flags.get_string("output");
    if !flags.args().is_empty()
        || output.is_empty()
        || assets.is_empty()
        || timeout <= 0
        || timeout > 300_000_000_000
    {
        bail!(
            "use project-import --project <source> --output <new directory> --asset Assets/path.fbx [--asset ...] --timeout 45s"
        );
    }
    let timeout = Duration::from_nanos(timeout as u64);
    let opts = ProbeOptions {
        editor: flags.get_string("editor"),
        project: flags.get_string("project"),
        output: output.clone(),
        timeout,
        ..Default::default()
    };
    let signals = signal_context()?;
    let ctx = signals.with_timeout(timeout);
    let (report, mut error) = run_project_import(&ctx, opts, &assets);
    if let Some(mut report) = report {
        let mut data = match jsondoc::marshal_indent(&report) {
            Ok(data) => data,
            Err(marshal) => return Err(join([error, Some(anyhow!(marshal))]).unwrap()),
        };
        data.push(b'\n');
        if let Err(write) = write_file(&sysutil::join(&[&output, "result.json"]), &data, 0o600) {
            error = join([error, Some(write)]);
            report.success = false;
            report.error = error.as_ref().map(text).unwrap_or_default();
            data = jsondoc::marshal_indent(&report).unwrap_or_default();
            data.push(b'\n');
        }
        std::io::stdout().lock().write_all(&data)?;
    }
    error.map_or(Ok(()), Err)
}

/// The compiled importers the assets' `.meta` files select, if any does.
fn importer_environment(project: &str, assets: &[String]) -> Result<Option<ScriptEnvironment>> {
    let mut importers = BTreeSet::new();
    for asset in assets {
        // A source that does not resolve is reported by the context.
        let Ok(source) = resolve_project_import_source(project, asset) else {
            continue;
        };
        if let Ok(meta) = read_file(&format!("{source}.meta"))
            && let Some(script) = scripted_importer(&meta)
        {
            importers.insert(script);
        }
    }
    if importers.is_empty() {
        return Ok(None);
    }
    script_environment(project, &importers).map(Some)
}

/// Creates the owned context, imports every asset in one worker run and
/// extracts the linked artifacts. The report accompanies a later failure.
pub fn run_project_import(
    ctx: &Context,
    mut opts: ProbeOptions,
    assets: &[String],
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    let started = Instant::now();
    let scripts = match importer_environment(&opts.project, assets) {
        Ok(scripts) => scripts,
        Err(error) => return (None, Some(error)),
    };
    let (prepared, created) =
        create_scripted_import_context(ctx, &opts.project, &opts.output, assets, scripts.as_ref());
    if let Err(error) = created {
        return (None, Some(error));
    }
    let mut input = registry::Input {
        project: prepared.project.clone().into(),
        unity_version: prepared.unity_version.clone(),
        assets: Vec::new(),
        roots: scripts
            .as_ref()
            .map(|environment| environment.roots.clone())
            .unwrap_or_default(),
        singletons: Default::default(),
    };
    if assets
        .iter()
        .any(|asset| generated_material_profile(&lower_ext(asset)))
    {
        match shader_name_registry(&opts.project) {
            Ok(table) => input
                .singletons
                .insert("Library/ScriptMapper".into(), table),
            Err(error) => return (None, Some(error)),
        };
    }
    opts.jobs.clear();
    for asset in prepared.assets() {
        input.assets.push(registry::Asset {
            path: asset.path.clone(),
            source: String::new(),
            meta: String::new(),
            guid: asset.guid.clone(),
            directory: false,
            script: false,
        });
        opts.jobs.push(NativeImportJob {
            guid: asset.guid.clone(),
            importer: -1,
        });
    }
    // The importers' own sources are registered, not imported.
    for script in scripts.iter().flat_map(|environment| &environment.scripts) {
        input.assets.push(registry::Asset {
            path: script.clone(),
            source: String::new(),
            meta: String::new(),
            guid: String::new(),
            directory: false,
            script: true,
        });
    }
    let registry = match registry::write(input) {
        Ok(registry) => registry,
        Err(error) => return (None, Some(error)),
    };
    opts.context = prepared.project.clone();
    opts.registry = Some(registry);
    opts.project_context = Some(prepared.clone());
    let output = opts.output.clone();
    let preparation = started.elapsed().as_secs_f64();
    let (report, error) = run_probe(ctx, opts);
    let Some(mut report) = report else {
        return (None, error);
    };
    report.experiment = "native-project-assets-own-coordinator".into();
    report.native_seconds.0 += preparation;
    report
        .stages
        .insert("projectContextAndRegistry".into(), JsonF64(preparation));
    report
        .stages
        .entry("workerReadyTotal".into())
        .or_default()
        .0 += preparation;
    let error = match error {
        Some(error) => Some(error),
        None => {
            let (content, extracted) =
                extract_project_import_content(ctx, &output, &prepared, &report);
            report.content = Some(content);
            if extracted.is_ok() {
                report.success = true;
                report.stage = "project-artifacts-validated".into();
            }
            extracted.err()
        }
    };
    report.elapsed_seconds = JsonF64(started.elapsed().as_secs_f64());
    report.coordinator_rss_kib = rss_kib(i64::from(std::process::id()));
    let error = join([error, context_error(ctx)]);
    if let Some(error) = &error {
        report.success = false;
        report.error = text(error);
    }
    (Some(report), error)
}

pub fn extract_project_import_content(
    ctx: &Context,
    output: &str,
    prepared: &ProjectImportContext,
    report: &ProbeReport,
) -> (ProjectImportContent, Result<()>) {
    let started = Instant::now();
    let mut content = ProjectImportContent {
        project: prepared.source_project.clone(),
        ..Default::default()
    };
    let result = extract(ctx, output, prepared, report, &mut content);
    content.elapsed_seconds = JsonF64(started.elapsed().as_secs_f64());
    (content, result)
}
fn extract(
    ctx: &Context,
    output: &str,
    prepared: &ProjectImportContext,
    report: &ProbeReport,
    content: &mut ProjectImportContent,
) -> Result<()> {
    if report.worker_exit != "exit 0" || report.imports.len() != prepared.assets().len() {
        bail!("project artifact extraction requires all imports and a stopped worker");
    }
    let store =
        UdsArtifactStore::open(&sysutil::join(&[&prepared.project, "Library", "DataStore"]))?;
    let artifacts = sysutil::join(&[output, "artifacts"]);
    mkdir(&artifacts, 0o700)?;
    for (index, source) in prepared.assets().iter().enumerate() {
        check(ctx)?;
        let imported = &report.imports[index];
        let files = &imported.metadata.files;
        if imported.guid != source.guid
            || imported.error_code != 0
            || files.is_empty()
            || files.len() > 1024
        {
            bail!("project import metadata has no valid linked output set");
        }
        let mut asset = super::ProjectImportedAsset {
            path: source.path.clone(),
            guid: source.guid.clone(),
            result_id: imported.result_id.clone(),
            files: Vec::new(),
        };
        for (file_index, file) in files.iter().enumerate() {
            // Metadata names may be paths chosen by native importers. They
            // remain metadata; output basenames are generated by us.
            let destination =
                sysutil::join(&[&artifacts, &format!("{}-{file_index:04}.bin", source.guid)]);
            let (proof, extracted) = store.extract(&file.hash, &destination);
            extracted
                .map_err(|e| wrap(&format!("extract {} output {file_index}", source.path), e))?;
            asset.files.push(ProjectImportedFile {
                name: file.name.clone(),
                path: proof.output.clone(),
                hash: proof.raw_hash_key.clone(),
                sha256: proof.sha256.clone(),
                bytes: proof.bytes,
                proof,
            });
        }
        content.assets.push(asset);
    }
    verify_project_import_sources(ctx, prepared)?;
    content.source_unchanged = true;
    Ok(())
}
