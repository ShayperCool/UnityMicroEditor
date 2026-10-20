//! The command entry points `mcp`, `auto-prepare`, `build-project`,
//! `prepare-project` and `cache-component-defaults`. Each returns the error
//! the caller prints before exiting with status 1.
use super::{
    Any, Options, Service, UNITY_VERSION, authored_scene, defaults::defaults_directory,
    handler_error, join, marshal, native::attach_native_seed, obj, reply_bytes,
};
use crate::{
    agentcontrol::{self, Controller},
    cancel::Context,
    compiler::{Compiler, CompilerPreparationResult, RuntimeInitializer},
    flags::FlagSet,
    mcp::{self, HandlerValue, RuntimeHandler},
    project::{self, BUILTIN_RESOURCE_GUID, is_owned_cache_path, lexpath, parse_unity_yaml, value},
    sha256, sysutil,
    util::{atomic_json, canonical_destination, default_unity_editor, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::{fs, io::Write, sync::Arc};

fn emit(data: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(data)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
/// Resolves symlinks, reporting the error as plain text.
fn eval_symlinks(path: &str) -> Result<String> {
    lexpath::eval_symlinks(path).map_err(|error| anyhow!("{error}"))
}
struct Closing<'a>(&'a Service);
impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The `mcp` command.
pub fn run_mcp_cli(arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new("mcp");
    flags.string("project", "", "explicit Unity project root");
    flags.string("transport", "stdio", "stdio or http");
    flags.string("listen", "127.0.0.1:8091", "loopback HTTP listener");
    flags.string("token-file", "", "optional local bearer token file");
    flags.string("baseline", "", "validated prepared project current.json");
    flags.string(
        "output",
        "",
        "owned runtime session output under project Temp or Library/UnityMicroEditor",
    );
    flags.string(
        "editor",
        &default_unity_editor(),
        "matching installed Unity SDK",
    );
    flags.string(
        "scene",
        "",
        "authored scene selected for the runtime session",
    );
    flags.string(
        "host-player",
        "",
        "optional compatible host; absent means automatic package-aware Editor host build",
    );
    flags.string(
        "engine-profile",
        "",
        "optional cached native engine/host profile for our source graph",
    );
    flags.bool(
        "no-editor",
        false,
        "forbid main Editor execution; require prepared native profiles/defaults/imports",
    );
    flags.int("fps", 30, "runtime frame cap");
    flags.string(
        "remote-publish",
        &std::env::var("UNITY_MICRO_EDITOR_REMOTE_PUBLISH").unwrap_or_default(),
        "command every remote session runs to publish its listener, with {url}, {address}, {host} or {port}",
    );
    flags.parse(arguments)?;
    if !flags.args().is_empty() || flags.get_string("project").is_empty() {
        bail!("mcp requires an explicit --project");
    }
    let project = lexpath::abs(&flags.get_string("project"))?;
    let runner = std::env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow!("executable path is not UTF-8"))?;
    let no_editor = flags.get_bool("no-editor");
    let runtime = agentcontrol::Options {
        project: project.clone(),
        baseline: flags.get_string("baseline"),
        output: flags.get_string("output"),
        editor: flags.get_string("editor"),
        runner,
        scene: flags.get_string("scene"),
        fps: flags.get_int("fps"),
    };
    let ctx = mcp::signal_context()?;
    let controller = Controller::new(&ctx, runtime.clone())?;
    struct ClosingController(Controller);
    impl Drop for ClosingController {
        fn drop(&mut self) {
            let _ = self.0.close();
        }
    }
    let _controller = ClosingController(controller.clone());
    let editor = eval_symlinks(&runtime.editor)?;
    let service = Service::new(Options {
        project: project.clone(),
        editor,
        host: flags.get_string("host-player"),
        scene: runtime.scene.clone(),
        baseline: runtime.baseline.clone(),
        engine_profile_path: flags.get_string("engine-profile"),
        no_editor,
        asynchronous: true,
        remote_publish: flags.get_string("remote-publish"),
        controller: Some(controller),
        lifetime: Some(ctx.clone()),
    });
    let _closing = Closing(&service);
    let runtime_handler: Arc<dyn RuntimeHandler> = {
        let service = service.clone();
        Arc::new(move |ctx: &Context, name: &str, raw: &[u8]| {
            service.runtime(ctx, name, raw).map_err(handler_error)
        })
    };
    let project_actions: Arc<dyn RuntimeHandler> = {
        let service = service.clone();
        Arc::new(move |ctx: &Context, name: &str, raw: &[u8]| {
            service.handle(ctx, name, raw).map_err(handler_error)
        })
    };
    mcp::run_options(
        &ctx,
        mcp::Options {
            project,
            transport: flags.get_string("transport"),
            listen: flags.get_string("listen"),
            token_file: flags.get_string("token-file"),
            runtime: Some(runtime_handler),
            project_actions: Some(project_actions),
            backends: serde_json::json!({
                "sourceAuthoring": "raw_source",
                "runtime": "unity_mono_player",
                "compiler": "dotnet_sdk",
                "hotReload": "patch_assembly_method_detour",
                "projectPreparation": "authored_source",
                "projectBuild": "unity_editor_proxy",
                "prefabApplyRevertUnpack": "raw_source",
                "componentDefaults": "versioned_component_cache",
                "mainEditorAllowed": !no_editor,
                "generalEditorExtensions": false,
                "editModeRunner": false,
            }),
        },
    )
}

/// The `auto-prepare` command.
pub fn run_auto_prepare_cli(arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new("auto-prepare");
    flags.string("project", "", "explicit Unity project");
    flags.string("scene", "", "authored Assets scene");
    flags.string("editor", &default_unity_editor(), "matching Unity Editor");
    flags.string("host-player", "", "optional compatible package-aware host");
    flags.string("request-id", "", "durable preparation identity");
    flags.string("native-catalog", "", "optional validated native catalog");
    flags.string(
        "engine-profile",
        "",
        "cached pinned engine/native-host profile",
    );
    flags.bool(
        "no-editor",
        false,
        "forbid main Editor execution, including native profile bootstrap",
    );
    flags.bool(
        "rebuild-host",
        false,
        "build a new host instead of reusing the newest one of an earlier preparation",
    );
    flags.parse(arguments)?;
    let (project, scene) = (flags.get_string("project"), flags.get_string("scene"));
    if !flags.args().is_empty() || project.is_empty() || scene.is_empty() {
        bail!("auto-prepare requires --project and --scene");
    }
    let project = eval_symlinks(&lexpath::abs(&project)?)?;
    let editor = eval_symlinks(&flags.get_string("editor"))?;
    let ctx = mcp::signal_context()?;
    let service = Service::new(Options {
        project,
        editor,
        lifetime: Some(ctx.clone()),
        no_editor: flags.get_bool("no-editor"),
        engine_profile_path: flags.get_string("engine-profile"),
        ..Default::default()
    });
    let _closing = Closing(&service);
    let mut arguments = obj([
        ("scene", scene.into()),
        ("hostPlayer", flags.get_string("host-player").into()),
        ("requestId", flags.get_string("request-id").into()),
        ("nativeCatalog", flags.get_string("native-catalog").into()),
        ("activate", false.into()),
    ]);
    if flags.get_bool("rebuild-host") {
        arguments.insert("rebuildHost".into(), true.into());
    }
    let raw = marshal(&Any::Object(arguments))?;
    let result = service.handle(&ctx, "project_prepare", &raw)?;
    emit(reply_bytes(&result))
}

/// The `build-project` command.
pub fn run_build_project_cli(arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new("build-project");
    flags.string("project", "", "explicit Unity project");
    flags.string(
        "editor",
        &default_unity_editor(),
        "real Unity Editor backend",
    );
    flags.string("request-id", "", "durable build identity");
    flags.string("scenes", "", "comma-separated saved Assets scenes");
    flags.bool("development", false, "Unity Development build");
    flags.parse(arguments)?;
    let (project, scenes) = (flags.get_string("project"), flags.get_string("scenes"));
    if !flags.args().is_empty() || project.is_empty() || scenes.is_empty() {
        bail!("build-project requires --project and --scenes");
    }
    let project = eval_symlinks(&lexpath::abs(&project)?)?;
    let editor = eval_symlinks(&flags.get_string("editor"))?;
    let ctx = mcp::signal_context()?;
    let service = Service::new(Options {
        project,
        editor,
        lifetime: Some(ctx.clone()),
        ..Default::default()
    });
    let _closing = Closing(&service);
    let raw = marshal(&Any::Object(obj([
        (
            "scenes",
            scenes.split(',').map(Any::from).collect::<Vec<_>>().into(),
        ),
        ("development", flags.get_bool("development").into()),
        ("requestId", flags.get_string("request-id").into()),
    ])))?;
    let result = service.handle(&ctx, "project_build", &raw)?;
    emit(reply_bytes(&result))?;
    if let HandlerValue::Map(bytes) = &result
        && let Ok((Any::Object(reply), _)) = value::parse_json(bytes, false)
        && reply.get("success") == Some(&Any::Bool(false))
    {
        bail!("Unity Editor build did not complete successfully; see returned receipt");
    }
    Ok(())
}

/// The `cache-component-defaults` command.
pub fn run_component_defaults_cli(arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new("cache-component-defaults");
    flags.string(
        "project",
        "",
        "explicit Unity project whose native profile defaults should be cached",
    );
    flags.string(
        "editor",
        &default_unity_editor(),
        "matching installed Unity Editor used once in isolated capture staging",
    );
    flags.parse(arguments)?;
    let project = flags.get_string("project");
    if project.is_empty() || !flags.args().is_empty() {
        bail!("cache-component-defaults requires an explicit --project");
    }
    let project = lexpath::abs(&eval_symlinks(&project)?)?;
    let editor = eval_symlinks(&flags.get_string("editor"))?;
    let ctx = mcp::signal_context()?;
    let service = Service::new(Options {
        project: project.clone(),
        editor,
        lifetime: Some(ctx.clone()),
        ..Default::default()
    });
    let _closing = Closing(&service);
    let catalog = service.locked(&ctx, || service.capture_component_defaults(&ctx))??;
    emit(&marshal(&Any::Object(obj([
        ("schemaVersion", 1.into()),
        ("backend", "versioned_component_cache".into()),
        ("profileSha256", catalog.profile_sha256.clone().into()),
        (
            "catalog",
            join(&[
                &defaults_directory(&project, &catalog.profile_sha256),
                "catalog.json",
            ])
            .into(),
        ),
        ("componentCount", (catalog.templates.len() as i64).into()),
    ])))?)
}

/// The `prepare-project` command. First source
/// preparation consumes an explicit, verified SDK/compiler baseline. It never
/// invokes historical Python or regenerates authored game assets.
pub fn run_project_prepare_cli(arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new("prepare-project");
    flags.string("project", "", "explicit Unity project root");
    flags.string(
        "editor",
        &default_unity_editor(),
        "matching installed Unity SDK",
    );
    flags.string("compiler", "", "compiler-prepare result JSON");
    flags.string("scene", "", "authored Assets/...unity scene");
    flags.string(
        "output",
        "",
        "new current.json under project Temp or Library/UnityMicroEditor",
    );
    flags.string(
        "scene-proof",
        "",
        "public Editor lighting proof bound to this authored scene",
    );
    flags.string(
        "native-catalog",
        "",
        "verified public native asset catalog for this project; reuse without Editor import",
    );
    flags.parse(arguments)?;
    let (project, compiler, scene, output) = (
        flags.get_string("project"),
        flags.get_string("compiler"),
        flags.get_string("scene"),
        flags.get_string("output"),
    );
    if !flags.args().is_empty()
        || project.is_empty()
        || compiler.is_empty()
        || scene.is_empty()
        || output.is_empty()
    {
        bail!("prepare-project requires --project, --compiler, --scene and --output");
    }
    let ctx = mcp::signal_context()?;
    prepare_agent_project_with_native(
        &ctx,
        &project,
        &flags.get_string("editor"),
        &compiler,
        &scene,
        &output,
        &flags.get_string("scene-proof"),
        &flags.get_string("native-catalog"),
    )?;
    let data = fs::read(&output).map_err(|error| sysutil::Error::os("open", &output, &error))?;
    emit(&data)
}

pub fn prepare_agent_project(
    ctx: &Context,
    project: &str,
    editor: &str,
    compiler_path: &str,
    scene: &str,
    output: &str,
    scene_proof: &str,
) -> Result<()> {
    prepare_agent_project_with_native(
        ctx,
        project,
        editor,
        compiler_path,
        scene,
        output,
        scene_proof,
        "",
    )
}

/// Compiles the prepared graph, publishes the
/// scene manifest of the authored scene and links the baseline `current.json`.
#[allow(clippy::too_many_arguments)]
pub fn prepare_agent_project_with_native(
    ctx: &Context,
    project: &str,
    editor: &str,
    compiler_path: &str,
    scene: &str,
    output: &str,
    scene_proof: &str,
    native_catalog: &str,
) -> Result<()> {
    let project = lexpath::abs(&eval_symlinks(project)?)?;
    let compiler_path = lexpath::abs(compiler_path)?;
    let output = lexpath::abs(output)?;
    if canonical_destination(&output).ok().as_deref() != Some(output.as_str())
        || !is_owned_cache_path(&project, &output)
    {
        bail!(
            "prepared current.json must be a canonical file under project Temp or Library/UnityMicroEditor"
        );
    }
    if !matches!(sysutil::lstat(&output), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        bail!("prepared output already exists");
    }
    if !authored_scene(scene) {
        bail!("scene must be an authored Assets/...unity path");
    }
    let scene_file = join(&[&project, scene]);
    let read_scene =
        || fs::read(&scene_file).map_err(|error| sysutil::Error::os("open", &scene_file, &error));
    let contents = read_scene()?;
    super::validate_scene_lighting(&contents, &project, scene, scene_proof)?;
    let verified_empty_builtin_lighting = parse_unity_yaml(&contents)
        .unwrap_or_default()
        .iter()
        .any(|document| {
            document.class_id == 157
                && value::get(document.fields(), "m_LightingDataAsset")
                    .as_object()
                    .is_some_and(|reference| {
                        // validated identity and public proof above
                        reference.get("guid") == Some(&Any::from(BUILTIN_RESOURCE_GUID))
                    })
        });
    let preparation: CompilerPreparationResult =
        read_json_file::<Option<_>>(&compiler_path)?.unwrap_or_default();
    if preparation.schema_version != 1 || preparation.compiler.project != project {
        bail!("compiler preparation belongs to another project");
    }
    let parent = sysutil::dir(&output);
    sysutil::mkdir_all(&parent)?;
    let staging_directory = tempfile::Builder::new()
        .prefix(".source-preparation-")
        .tempdir_in(&parent)
        .map_err(|error| {
            sysutil::Error::os("mkdir", &join(&[&parent, ".source-preparation-"]), &error)
        })?;
    let staging = staging_directory.path().to_string_lossy().into_owned();
    let mut compiler = Compiler::new(
        &project,
        &compiler_path,
        &preparation.player,
        editor,
        &join(&[&staging, "compiler-cache"]),
    )?;
    let mut active_player = preparation.player.clone();
    if let Some(compiled) = compiler.compile(ctx, None)? {
        active_player = compiled.player;
    }
    let types = compiler.script_type_names(ctx)?;
    let accepted_compiler = compiler.prepared_manifest(ctx)?;
    let mut assemblies = Vec::with_capacity(accepted_compiler.assemblies.len());
    let mut initializers: Vec<RuntimeInitializer> = Vec::new();
    for assembly in accepted_compiler.assemblies.iter() {
        assemblies.push(Any::from(assembly.path.clone()));
        initializers.extend(
            assembly
                .lifecycle_barrier
                .runtime_initializers
                .iter()
                .cloned(),
        );
    }
    let empty = || Any::Array(Vec::new());
    let mut seed = obj([
        ("schemaVersion", 1.into()),
        ("supported", true.into()),
        ("unityVersion", UNITY_VERSION.into()),
        ("projectRoot", project.clone().into()),
        ("scenePath", scene.into()),
        (
            "sceneName",
            sysutil::base(scene)
                .strip_suffix(".unity")
                .unwrap_or(&sysutil::base(scene))
                .to_string()
                .into(),
        ),
        ("objects", empty()),
        ("assets", empty()),
        ("assemblies", assemblies.clone().into()),
        ("sourceDependencies", empty()),
        ("lifecycleBarrierVersion", 1.into()),
        ("lifecycleBarrierAssemblies", assemblies.into()),
        ("scriptableObjectBarrierVersion", 1.into()),
        ("runtimeInitializers", value::raw(&initializers)?),
        (
            "emptyBuiltinLightingDataVerified",
            verified_empty_builtin_lighting.into(),
        ),
    ]);
    if compiler.manifest.native_only {
        seed.insert(
            "compiler".into(),
            Any::Object(obj([
                ("nativeOnly", true.into()),
                (
                    "nativeSlotsSha256",
                    compiler.manifest.native_slots_sha.clone().into(),
                ),
            ])),
        );
    }
    let seed_path = join(&[&staging, "source-seed.json"]);
    attach_native_seed(&mut seed, &project, native_catalog)?;
    atomic_json(&seed_path, &Any::Object(seed))?;
    let session = project::Session::new(project::Options {
        project: project.clone(),
        manifest: seed_path,
        script_types: types
            .iter()
            .map(|(key, name)| (key.clone(), name.clone()))
            .collect(),
        native_catalog: native_catalog.into(),
        ..Default::default()
    })?;
    let session = std::sync::Arc::new(session);
    let source_output = join(&[&staging, "source"]);
    let prepared = match session.prepare(&source_output) {
        // A source the baseline's native catalog does not hold, or holds in
        // an older form: import or bake it as a running session would. The
        // artifacts stay with the baseline.
        Err(error)
            if error.chain().any(|cause| {
                cause.is::<project::NeedsImportError>() || cause.is::<project::NeedsNativeScene>()
            }) =>
        {
            let mut importer = crate::native::NativeProjectImporter::new(
                crate::native::NativeOptions {
                    project: project.clone(),
                    output: source_output.clone(),
                    editor: editor.to_string(),
                    player: preparation.player.clone(),
                    store: crate::native::project_native_store(&project),
                    seed: earlier_native_index(&project, scene, &output),
                },
                session.clone(),
            )?;
            importer.prepare(ctx)?
        }
        other => other?,
    };
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Dependency {
        #[serde(deserialize_with = "super::nz")]
        path: String,
        #[serde(deserialize_with = "super::nz")]
        sha256: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct ManifestSources {
        #[serde(rename = "sourceDependencies", deserialize_with = "super::nz")]
        dependencies: Vec<Dependency>,
    }
    let manifest_sources: ManifestSources =
        read_json_file::<Option<_>>(&prepared.manifest)?.unwrap_or_default();
    let scene_hash = sha256(&contents);
    let matched = manifest_sources
        .dependencies
        .iter()
        .any(|dependency| dependency.path == scene && dependency.sha256 == scene_hash);
    let latest_contents = read_scene()?;
    if !matched || sha256(&latest_contents) != scene_hash {
        bail!(
            "authored scene changed after its preparation/lighting proof; export a fresh proof and retry"
        );
    }
    super::validate_scene_lighting(&latest_contents, &project, scene, scene_proof)?;
    let preparation_backend = if accepted_compiler.backend == "authored_source" {
        "source-graph-sdk-raw-source"
    } else {
        "public-editor-graph-sdk-raw-source"
    };
    let current = obj([
        ("schemaVersion", 1.into()),
        ("project", project.clone().into()),
        ("manifest", prepared.manifest.clone().into()),
        ("player", active_player.into()),
        ("compiler", value::raw(&accepted_compiler)?),
        (
            "nativeImport",
            Any::Object(obj([
                ("catalog", native_catalog.into()),
                ("index", crate::native::native_index(&source_output).into()),
            ])),
        ),
        ("preparation", preparation_backend.into()),
    ]);
    super::publish_agent_baseline(&output, &Any::Object(current))?;
    // Published: the staging directory stays as part of the baseline.
    let _ = staging_directory.keep();
    Ok(())
}

/// The index of the native content of the newest other preparation of the
/// scene, or empty when there is none. A new preparation starts from it: the
/// content it holds is checked against the sources as they are now, and
/// whatever changed is imported again.
fn earlier_native_index(project: &str, scene: &str, output: &str) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Prepared {
        scene: String,
        baseline: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct NativeImport {
        index: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Baseline {
        #[serde(rename = "nativeImport")]
        native_import: NativeImport,
    }
    let preparations = join(&[project, "Library/UnityMicroEditor/preparations"]);
    let Ok(directories) = fs::read_dir(&preparations) else {
        return String::new();
    };
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for directory in directories.flatten() {
        let result = join(&[&directory.path().to_string_lossy(), "result.json"]);
        let Ok(Some(found)) = read_json_file::<Option<Prepared>>(&result) else {
            continue;
        };
        if found.scene != scene || found.baseline == output {
            continue;
        }
        let Ok(Some(baseline)) = read_json_file::<Option<Baseline>>(&found.baseline) else {
            continue;
        };
        let index = baseline.native_import.index;
        let Ok(modified) = fs::metadata(&index).and_then(|info| info.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, index));
        }
    }
    newest.map(|(_, index)| index).unwrap_or_default()
}
