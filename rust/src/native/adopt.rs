//! `native-adopt` reuses a stopped standalone worker report. Every source and
//! UDS output is revalidated; neither a worker nor an Editor is started.
use super::{
    MAX_ARTIFACT_BYTES, NativeOptions, NativeProjectImporter, ProbeReport, ProjectImportContent,
    ProjectImportContext, UdsArtifactStore,
    batch::{NativeColdInput, cold_input},
    import::signal_context,
    resolve_project_import_source,
    support::{Event, abs, check, eval_symlinks, read_file, read_json},
    verify_project_import_sources,
};
use crate::{
    ENGINE_VERSION,
    cancel::Context,
    flags::FlagSet,
    hex,
    project::{Options, Session},
    sysutil,
    util::{self, canonical_destination, file_hash, random_id, valid_sha256, within_directory},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, sync::Arc, time::Instant};

/// The fields of the runner's baseline `current.json` this command reads.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Baseline {
    project: Option<String>,
    manifest: Option<String>,
    #[serde(rename = "nativeImport")]
    native_import: Option<BaselineNativeImport>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct BaselineNativeImport {
    catalog: Option<String>,
}

pub fn run_native_adopt_cli(args: &[String]) -> Result<()> {
    let mut flags = FlagSet::new("native-adopt");
    flags.string("project", ".", "source Unity project");
    flags.string("baseline", "", "validated host current.json");
    flags.string(
        "output",
        "",
        "owned session cache beneath project Temp or Library/UnityMicroEditor",
    );
    flags.string(
        "editor",
        &util::default_unity_editor(),
        "matching pinned native executable",
    );
    flags.string("report", "", "successful project-import result.json");
    flags.parse(args.iter().cloned())?;
    let (output, report_path) = (flags.get_string("output"), flags.get_string("report"));
    if !flags.args().is_empty() || output.is_empty() || report_path.is_empty() {
        bail!(
            "use native-adopt --project <source> --output <session Temp cache> --report <project-import result.json> [--baseline current.json]"
        );
    }
    let project = eval_symlinks(&abs(&flags.get_string("project"))?)?;
    let baseline_path = flags.get_string("baseline");
    if baseline_path.is_empty() {
        bail!("native-adopt requires --baseline");
    }
    let output = abs(&output)?;
    if !canonical_destination(&output).is_ok_and(|canonical| {
        canonical == output && sysutil::is_owned_cache_path(&project, &canonical)
    }) {
        bail!(
            "native adoption output must be owned beneath project Temp or Library/UnityMicroEditor without symlink ancestors"
        );
    }
    let report_path = abs(&report_path)?;
    let ctx = signal_context()?;
    let started = Instant::now();
    let (report, prepared, content) = read_cold_adoption_report(&ctx, &project, &report_path)?;
    let baseline: Baseline = read_json(&baseline_path)?;
    let manifest = baseline.manifest.unwrap_or_default();
    if baseline.project.as_deref() != Some(project.as_str()) || manifest.is_empty() {
        bail!("native adoption baseline belongs to another project");
    }
    let session = Session::new(Options {
        project: project.clone(),
        manifest,
        native_catalog: baseline
            .native_import
            .and_then(|native| native.catalog)
            .unwrap_or_default(),
        ..Default::default()
    })?;
    let opts = NativeOptions {
        project,
        output: output.clone(),
        editor: flags.get_string("editor"),
        ..Default::default()
    };
    let mut importer = NativeProjectImporter::new(opts, Arc::new(session))?;
    let inputs = adoption_inputs(&importer.opts, &importer.environment_sha256, &prepared)?;
    let logicals: Vec<String> = content.assets.iter().map(|a| a.path.clone()).collect();
    let root = sysutil::join(&[&output, "native/adopt", &random_id()]);
    importer.adopt_cold_report(&ctx, Some(&report), &root, &report_path, inputs, &logicals)?;
    Event::new("native-adopted")
        .with("assetCount", &logicals.len())
        .with("environmentSha256", &importer.environment_sha256)
        .with("report", &report_path)
        .with("index", &sysutil::join(&[&output, "native/index.json"]))
        .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
        .with("workerStarted", &false)
        .with("mainEditorStarted", &false)
        .emit();
    Ok(())
}

pub(crate) fn adoption_inputs(
    opts: &NativeOptions,
    environment_sha256: &str,
    prepared: &ProjectImportContext,
) -> Result<BTreeMap<String, NativeColdInput>> {
    let mut inputs = BTreeMap::new();
    let mut guids = std::collections::HashSet::new();
    for asset in prepared.assets() {
        let input = cold_input(opts, environment_sha256, &asset.path, "")?;
        match resolve_project_import_source(&opts.project, &asset.path) {
            Ok(physical)
                if input.guid == asset.guid
                    && prepared.source_hash(&physical) == input.source_sha
                    && prepared.source_hash(&format!("{physical}.meta")) == input.meta_sha => {}
            _ => bail!(
                "native adoption source/meta hashes differ from the imported revision: {}",
                asset.path
            ),
        }
        if inputs.contains_key(&asset.path) || !guids.insert(asset.guid.clone()) {
            bail!("native adoption report contains a duplicate source path or GUID");
        }
        inputs.insert(asset.path.clone(), input);
    }
    Ok(inputs)
}

/// A raw JSON member: `None` only when the key is absent.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawReport {
    environment: Option<BTreeMap<String, Box<RawValue>>>,
    #[serde(rename = "projectContext", deserialize_with = "present")]
    project_context: Option<Box<RawValue>>,
    #[serde(deserialize_with = "present")]
    content: Option<Box<RawValue>>,
}
/// Decodes a raw member; an absent or `null` member gives the default.
fn decode_member<T: Default + serde::de::DeserializeOwned>(
    raw: &Option<Box<RawValue>>,
) -> Result<T> {
    match raw.as_deref().map(RawValue::get) {
        None => bail!("unexpected end of JSON input"),
        Some("null") => Ok(T::default()),
        Some(text) => serde_json::from_str(text).map_err(|e| anyhow!("{e}")),
    }
}

/// Reads and revalidates a successful stopped-worker `project-import` report.
pub fn read_cold_adoption_report(
    ctx: &Context,
    project: &str,
    report_path: &str,
) -> Result<(ProbeReport, ProjectImportContext, ProjectImportContent)> {
    if !sysutil::is_owned_cache_path(project, report_path) {
        bail!("native adoption report must be in the project's owned cache");
    }
    validate_adoption_file(report_path, project, 32 << 20)?;
    let data = read_file(report_path)?;
    let mut report: ProbeReport = serde_json::from_slice(&data).map_err(|e| anyhow!("{e}"))?;
    let raw: RawReport = serde_json::from_slice(&data).map_err(|e| anyhow!("{e}"))?;
    // Missing explicit keys identify pre-v2 reports even if zero-valued fields
    // happen to equal the current environment after JSON decoding.
    for key in [
        "textureMaxSizeOverride",
        "textureCompressionOverride",
        "defaultTextureCompressionFormat",
    ] {
        let value = raw.environment.as_ref().and_then(|map| map.get(key));
        if value.is_none_or(|value| value.get() == "null") {
            bail!("native adoption requires the complete v2 worker environment");
        }
    }
    let prepared: ProjectImportContext = decode_member(&raw.project_context)?;
    let content: ProjectImportContent = decode_member(&raw.content)?;
    let assets = prepared.assets();
    if report.schema_version != 1
        || report.experiment != "native-project-assets-own-coordinator"
        || !report.success
        || report.stage != "project-artifacts-validated"
        || report.main_editor_started
        || report.worker_exit != "exit 0"
        || prepared.schema_version != 1
        || prepared.unity_version != ENGINE_VERSION
        || prepared.source_project != project
        || content.project != project
        || !content.source_unchanged
        || assets.is_empty()
        || assets.len() > 4096
        || assets.len() != content.assets.len()
        || assets.len() != report.imports.len()
    {
        bail!("native adoption requires a complete successful stopped-worker source report");
    }
    let root = sysutil::dir(report_path);
    if prepared.project != sysutil::join(&[&root, "project"])
        || prepared.source_hash_count() == 0
        || prepared.source_hash_count() > 32768
    {
        bail!("native adoption report context is outside its owned output or source bounds");
    }
    for (filename, sha) in prepared.source_hashes.iter().flatten() {
        if !valid_sha256(sha) {
            bail!("native adoption context has an invalid source hash");
        }
        validate_adoption_file(filename, project, 1 << 30)?;
    }
    verify_project_import_sources(ctx, &prepared)?;
    validate_adoption_file(
        &sysutil::join(&[&prepared.project, "Library/DataStore/uds.db"]),
        &root,
        1 << 30,
    )?;
    let store = UdsArtifactStore::open(&sysutil::join(&[&prepared.project, "Library/DataStore"]))?;
    let artifacts = sysutil::join(&[&root, "artifacts"]);
    for (index, asset) in content.assets.iter().enumerate() {
        check(ctx)?;
        let (source, imported) = (&assets[index], &report.imports[index]);
        let files = &imported.metadata.files;
        if asset.path != source.path
            || asset.guid != source.guid
            || imported.guid != source.guid
            || imported.error_code != 0
            || imported.result_id != asset.result_id
            || asset.files.is_empty()
            || asset.files.len() > 1024
            || asset.files.len() != files.len()
        {
            bail!("native adoption content differs from its source/import metadata");
        }
        for (file, metadata) in asset.files.iter().zip(files) {
            if file.name != metadata.name || file.hash != hex(&metadata.hash) {
                bail!("native adoption artifact key differs from native metadata");
            }
            validate_adoption_file(&file.path, &artifacts, MAX_ARTIFACT_BYTES as i64)?;
            let (proof, read) = store.read(&metadata.hash);
            if read.is_err()
                || !proof.content_key_verified
                || proof.sha256 != file.sha256
                || proof.bytes != file.bytes
            {
                bail!(
                    "native adoption artifact differs from its actual UDS content: {}",
                    file.path
                );
            }
            if !file_hash(&file.path).is_ok_and(|sha| sha == proof.sha256) {
                bail!("native adoption extracted artifact changed: {}", file.path);
            }
        }
    }
    report.project_context = Some(prepared.clone());
    report.content = Some(content.clone());
    Ok((report, prepared, content))
}

pub fn validate_adoption_file(filename: &str, root: &str, maximum: i64) -> Result<()> {
    if !sysutil::is_abs(filename) || !within_directory(filename, root) {
        bail!("native adoption input is outside its source/output ownership");
    }
    if !eval_symlinks(filename).is_ok_and(|canonical| canonical == filename) {
        bail!("native adoption input has symlink ancestors: {filename}");
    }
    if !sysutil::stat(filename)
        .is_ok_and(|info| info.is_regular() && info.size >= 0 && info.size <= maximum)
    {
        bail!("native adoption input is not a bounded regular file: {filename}");
    }
    Ok(())
}
