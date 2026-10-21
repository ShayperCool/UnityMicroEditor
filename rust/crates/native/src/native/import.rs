//! `project-import`: an owned context, one worker run and validated artifacts.
use super::{
    ArtifactExtraction, JsonF64, NativeImportJob, ProbeOptions, ProbeReport, ProjectImportContext,
    context::{
        BuildFiles, ContextExtras, create_build_import_context, create_scripted_import_context,
    },
    player_objects::stage_importer,
    probe::{rss_kib, run_probe},
    resolve_project_import_source,
    scripted::{
        ScriptEnvironment, ScriptSelection, generated_material_profile, scripted_importer,
        selected_environment, shader_name_registry,
    },
    support::{
        Event, check, context_error, join, lower_ext, mkdir, nul, read_file, text, wrap, write_file,
    },
    verify_project_import_sources,
};
use crate::{
    cancel::{Canceled, Context},
    flags::FlagSet,
    jsondoc, registry, sysutil, util,
};
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

/// The time one run of the import worker has in a session: its context,
/// the imports, the stop of its services, the extraction of the artifacts
/// and the check that the sources did not change meanwhile. A batch of 32
/// models takes some 7 s.
pub(crate) const IMPORT_BUDGET: Duration = Duration::from_secs(45);

/// Whether a failed run of the worker is worth repeating: it ran out of its
/// own budget while the caller's context is live, or its store had to be
/// killed and may not have written what it holds. Both happen to a sound
/// import on a machine that is busy with something else.
pub(crate) fn import_repeatable(
    caller: Option<Canceled>,
    bounded: Option<Canceled>,
    store_killed: bool,
) -> bool {
    caller.is_none() && (bounded == Some(Canceled::DeadlineExceeded) || store_killed)
}

/// Runs the worker within `budget`, and once more with twice that when the
/// first run is repeatable. `run` gets the bounded context and its budget,
/// and makes an output directory of its own for each run. An import that
/// ran out of its budget fails as `what`, with the budget named.
pub(crate) fn run_bounded_import(
    ctx: &Context,
    what: &str,
    mut budget: Duration,
    mut run: impl FnMut(&Context, Duration) -> (Option<ProbeReport>, Option<anyhow::Error>),
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    for repeated in [false, true] {
        let bounded = ctx.with_timeout(budget);
        let (report, error) = run(&bounded, budget);
        let ended = bounded.error();
        bounded.cancel();
        let Some(error) = error else {
            return (report, None);
        };
        let killed = report
            .as_ref()
            .and_then(|report| report.data_store.as_ref())
            .is_some_and(|store| store.forced_stop);
        if !repeated && import_repeatable(ctx.error(), ended, killed) {
            Event::new("native-import-repeated")
                .with("import", what)
                .with("reason", &text(&error))
                .seconds("budgetSeconds", budget.as_secs_f64())
                .seconds("nextBudgetSeconds", (budget * 2).as_secs_f64())
                .emit();
            budget *= 2;
            continue;
        }
        let expired = ctx.error().is_none() && ended == Some(Canceled::DeadlineExceeded);
        let error = match expired {
            true => wrap(
                &format!(
                    "{what}: the import ran out of its {}s budget{}",
                    budget.as_secs_f64(),
                    if repeated {
                        ", twice that of a first run that failed too"
                    } else {
                        ""
                    }
                ),
                error,
            ),
            false => error,
        };
        return (report, Some(error));
    }
    unreachable!("the second run returns")
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
fn importer_environment(
    project: &str,
    assets: &[String],
    extras: &ImportExtras,
) -> Result<Option<ScriptEnvironment>> {
    let mut selection = extras.scripts.clone();
    // As the Editor loads them: the assemblies an immutable package ships
    // compiled, which the importers' assemblies name (nunit for an
    // assembly the test framework is referenced from).
    selection.plugins = true;
    let mut elements = BTreeSet::new();
    let importers = &mut selection.scripts;
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
        // The importer of a document writes the serialized data of each
        // custom element, of a class it finds among the loaded assemblies.
        if lower_ext(asset) == ".uxml"
            && let Ok(text) = read_file(&source)
        {
            elements.extend(super::scripted::uxml_elements(&text));
        }
    }
    selection.elements.append(&mut elements);
    if selection.scripts.is_empty() && selection.files.is_empty() && selection.elements.is_empty() {
        return Ok(None);
    }
    selected_environment(project, &selection).map(Some)
}

/// What a worker run takes beyond the sources it imports.
#[derive(Clone, Debug, Default)]
pub struct ImportExtras {
    /// Compiled code the worker loads besides the importers the sources'
    /// `.meta` files select.
    pub scripts: ScriptSelection,
    pub context: ContextExtras,
}

/// Creates the owned context, imports every asset in one worker run and
/// extracts the linked artifacts. The report accompanies a later failure.
pub fn run_project_import(
    ctx: &Context,
    opts: ProbeOptions,
    assets: &[String],
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    run_project_import_with(ctx, opts, assets, &ImportExtras::default())
}

/// The same with files and compiled code of the caller's in the context.
pub fn run_project_import_with(
    ctx: &Context,
    opts: ProbeOptions,
    assets: &[String],
    extras: &ImportExtras,
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    import(ctx, opts, assets, extras, None)
}

/// What a build of Player objects imports with: the script assemblies a
/// build runs, and the assets the coordinator makes itself.
pub struct BuildImport<'a> {
    pub environment: &'a ScriptEnvironment,
    pub files: &'a BuildFiles,
    /// A directory of compiled shader variants kept from earlier builds,
    /// which the worker starts with; none when empty. The Editor keeps the
    /// same files in a project's `Library/ShaderCache`, each named after
    /// everything its variant was compiled from.
    pub shader_cache: &'a str,
    /// What the context of a run that writes no Player objects holds
    /// beyond its batch (`native::inspect_material_variants`). Such a run
    /// gets neither the tool's build importer nor the project's settings
    /// managers; its imports are recorded all the same.
    pub extras: Option<&'a ContextExtras>,
}

/// Copies the files of a directory tree that the destination lacks. A file
/// appears under its name only when it is whole.
pub(crate) fn copy_missing(from: &str, to: &str) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    std::fs::create_dir_all(to)?;
    for entry in entries {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let (source, destination) = (sysutil::join(&[from, &name]), sysutil::join(&[to, &name]));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_missing(&source, &destination)?;
        } else if kind.is_file() && !std::path::Path::new(&destination).exists() {
            // Builds run side by side, in one process: the name is the copy's own.
            let partial = format!("{destination}.{}.partial", crate::util::random_id());
            std::fs::copy(&source, &partial)?;
            std::fs::rename(&partial, &destination)?;
        }
    }
    Ok(())
}

/// The same for a build of Player objects. The worker gets the project's
/// graphics, quality and Player settings, the tool's own importer, and a
/// record of every import it has made, so that a later import of the batch
/// can load what an earlier one produced.
pub fn run_build_import(
    ctx: &Context,
    opts: ProbeOptions,
    assets: &[String],
    build: &BuildImport<'_>,
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    import(ctx, opts, assets, &ImportExtras::default(), Some(build))
}

/// Creates the import context of a run and the options its worker is
/// started with: the context, its source registry and the import jobs.
fn prepare_run(
    ctx: &Context,
    mut opts: ProbeOptions,
    assets: &[String],
    extras: &ImportExtras,
    build: Option<&BuildImport<'_>>,
) -> Result<(ProbeOptions, ProjectImportContext)> {
    // Before anything is read from the installation: it has to run the
    // project's release, whose protocol the worker is driven with.
    crate::engine::for_worker(&opts.project, &opts.editor)?;
    let scripts = match build {
        Some(build) => Some(build.environment.clone()),
        None => importer_environment(&opts.project, assets, extras)?,
    };
    let (prepared, created) = match build {
        Some(build) => create_build_import_context(
            ctx,
            &opts.project,
            &opts.output,
            assets,
            build.environment,
            build.files,
            build.extras.unwrap_or(&ContextExtras::default()),
        ),
        None => create_scripted_import_context(
            ctx,
            &opts.project,
            &opts.output,
            assets,
            scripts.as_ref(),
            &extras.context,
        ),
    };
    created?;
    // A build of Player objects, as opposed to a run that only loads what
    // it imports.
    let player = build.is_some_and(|build| build.extras.is_none());
    if player {
        stage_importer(ctx, &opts.editor, &prepared.project)?;
    }
    // UI Toolkit's importers load the style sheets a source imports, which
    // the worker finds only among the imports recorded before.
    if build.is_some()
        || assets
            .iter()
            .any(|asset| matches!(lower_ext(asset).as_str(), ".uss" | ".tss" | ".uxml"))
    {
        opts.artifact_database = true;
    }
    if let Some(cache) = build.map(|build| build.shader_cache)
        && !cache.is_empty()
    {
        // Only a cache: a build without it compiles everything again.
        let _ = copy_missing(
            cache,
            &sysutil::join(&[&prepared.project, "Library/ShaderCache"]),
        );
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
    if assets.iter().any(|asset| lower_ext(asset) == ".blend")
        && let Err(error) = super::scripted::require_blender()
    {
        return Err(error);
    }
    if assets
        .iter()
        .any(|asset| generated_material_profile(&lower_ext(asset)))
    {
        let table = shader_name_registry(&opts.project)?;
        input
            .singletons
            .insert("Library/ScriptMapper".into(), table);
    }
    // As the main Editor does, so that an importer may add objects of the
    // engine's script classes.
    let register = super::scripted::script_register(&opts.editor)?;
    input
        .singletons
        .insert("Library/MonoManager.asset".into(), register.to_vec());
    if player {
        // The worker reads these managers from the database, as text. With
        // them it has the project's colour space, its quality levels and
        // the render pipeline asset of each.
        for name in [
            "GraphicsSettings.asset",
            "QualitySettings.asset",
            "ProjectSettings.asset",
        ] {
            match read_file(&sysutil::join(&[
                &prepared.project,
                "ProjectSettings",
                name,
            ])) {
                Ok(data) => {
                    input
                        .singletons
                        .insert(format!("ProjectSettings/{name}"), data);
                }
                Err(error) if super::is_not_exist(&error) => {}
                Err(error) => return Err(error),
            }
        }
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
        if prepared.registered.contains(&asset.path) {
            continue;
        }
        opts.jobs.push(NativeImportJob {
            guid: asset.guid.clone(),
            importer: -1,
            dependent: lower_ext(&asset.path) == ".spriteatlasv2",
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
    let registry = crate::engine::supported(&prepared.unity_version)?
        .asset_database
        .write_sources(input)?;
    opts.context = prepared.project.clone();
    opts.registry = Some(registry);
    opts.project_context = Some(prepared.clone());
    Ok((opts, prepared))
}

/// Prepares the context of a build of Player objects and the options of a
/// worker that imports it, without starting the worker.
pub(crate) fn prepare_build_run(
    ctx: &Context,
    opts: ProbeOptions,
    assets: &[String],
    build: &BuildImport<'_>,
) -> Result<(ProbeOptions, ProjectImportContext)> {
    prepare_run(ctx, opts, assets, &ImportExtras::default(), Some(build))
}

fn import(
    ctx: &Context,
    opts: ProbeOptions,
    assets: &[String],
    extras: &ImportExtras,
    build: Option<&BuildImport<'_>>,
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    let started = Instant::now();
    let (opts, prepared) = match prepare_run(ctx, opts, assets, extras, build) {
        Ok(prepared) => prepared,
        Err(error) => return (None, Some(error)),
    };
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
            // A store that had to be killed may not have written the index
            // of what it holds.
            let killed = report
                .data_store
                .as_ref()
                .is_some_and(|store| store.forced_stop);
            // An extraction that ran out of time says so itself.
            let killed = killed && ctx.error().is_none();
            extracted.err().map(|error| {
                if killed {
                    wrap(
                        "Unity.DataStore did not stop by itself and was killed; the import has to be repeated",
                        error,
                    )
                } else {
                    error
                }
            })
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
    if report.worker_exit != "exit 0" || report.imports.len() != prepared.imported().count() {
        bail!("project artifact extraction requires all imports and a stopped worker");
    }
    let store = crate::engine::supported(&prepared.unity_version)?
        .data_store
        .artifacts(&sysutil::join(&[&prepared.project, "Library", "DataStore"]))?;
    let artifacts = sysutil::join(&[output, "artifacts"]);
    mkdir(&artifacts, 0o700)?;
    for (index, source) in prepared.imported().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::DataStoreStatus;

    fn report(store_killed: bool) -> Option<ProbeReport> {
        Some(ProbeReport {
            data_store: Some(DataStoreStatus {
                forced_stop: store_killed,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    #[test]
    fn only_a_run_out_budget_or_a_killed_store_is_repeated() {
        use Canceled::{Canceled as Stopped, DeadlineExceeded as Late};
        assert!(import_repeatable(None, Some(Late), false));
        assert!(import_repeatable(None, None, true));
        // An import that failed for a reason of its own fails again.
        assert!(!import_repeatable(None, None, false));
        // The caller's own end is not the import's.
        assert!(!import_repeatable(Some(Stopped), Some(Stopped), true));
        assert!(!import_repeatable(Some(Late), Some(Late), false));
    }

    #[test]
    fn an_import_that_runs_out_of_its_budget_is_repeated_once_with_twice_as_much() {
        let ctx = Context::background();
        let budget = Duration::from_millis(60);
        // The first run is late, the second is in time.
        let mut budgets = Vec::new();
        let (_, error) =
            run_bounded_import(&ctx, "native import of A.fbx", budget, |bounded, budget| {
                budgets.push(budget);
                if budgets.len() == 1 {
                    assert_eq!(bounded.wait(), Canceled::DeadlineExceeded);
                    return (report(false), context_error(bounded));
                }
                assert!(bounded.error().is_none());
                (report(false), None)
            });
        assert!(error.is_none());
        assert_eq!(budgets, [budget, budget * 2]);
        // A store that had to be killed is a reason too, with time left.
        let mut runs = 0;
        let (_, error) = run_bounded_import(&ctx, "native import of A.fbx", budget, |_, _| {
            runs += 1;
            match runs {
                1 => (report(true), Some(anyhow!("no index"))),
                _ => (report(false), None),
            }
        });
        assert!(error.is_none() && runs == 2);
        // Late twice: the step and the budget it ran out of are named.
        let mut runs = 0;
        let (_, error) =
            run_bounded_import(&ctx, "native import of A.fbx", budget, |bounded, _| {
                runs += 1;
                bounded.wait();
                (report(false), context_error(bounded))
            });
        assert_eq!(runs, 2);
        assert_eq!(
            text(&error.unwrap()),
            "native import of A.fbx: the import ran out of its 0.12s budget, twice that of a first run that failed too: deadline exceeded"
        );
    }

    #[test]
    fn an_import_that_fails_by_itself_or_is_cancelled_is_not_repeated() {
        let ctx = Context::background().with_cancel();
        let mut runs = 0;
        let (_, error) = run_bounded_import(&ctx, "import", Duration::from_secs(5), |_, _| {
            runs += 1;
            (report(false), Some(anyhow!("native importer error 3")))
        });
        assert_eq!(
            (runs, text(&error.unwrap()).as_str()),
            (1, "native importer error 3")
        );
        let mut runs = 0;
        let (_, error) =
            run_bounded_import(&ctx, "import", Duration::from_secs(5), |bounded, _| {
                runs += 1;
                ctx.cancel();
                (report(true), context_error(bounded))
            });
        assert_eq!((runs, text(&error.unwrap()).as_str()), (1, "canceled"));
    }
}
