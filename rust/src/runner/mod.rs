//! The `run` command. It stages the prepared host, compiles the project's
//! scripts, prepares the scene, owns one Player process and, with `--watch`,
//! turns source changes into live patches, reloads or Player restarts.
mod addressables;
mod json;
mod noop;
mod patch;
mod player;
mod prepared;
mod routes;
mod watch;

#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod tests;

pub use addressables::AddressableRoute;
pub use noop::{normalize_scene_dependencies, scene_runtime_equal};
pub use patch::{
    SCENE_ROOTS_CLASS, decode_patch_json, make_scene_patch, patch_integer, patch_reference,
    patch_value_equal,
};
pub use player::{
    Player, ReloadAck, require_hardware_graphics, require_player_report, start_player,
    wait_reload_ack,
};
pub use prepared::{
    PreparedCompiler, PreparedDependency, PreparedFile, PreparedLaunch, PreparedNativeContent,
    PreparedScene, require_prepared_file, resolve_prepared_source, run_prepared_player,
    validate_prepared_scene,
};
pub use routes::{
    ContentImporter, ContentImporters, RawContentRoutes, RawSceneRoute,
    content_routes_runtime_equal, parse_content_scene, prepare_content_routes,
    publish_content_routes, read_verified_content_routes, split_game_arguments, valid_scene_guid,
    validate_game_arguments,
};
pub use watch::{ProjectWatcher, SourceChange};

use crate::{
    cancel::{self, Context, Flag, Source},
    compiler::{self, CompileUpdate, Compiler, NilVec},
    flags::FlagSet,
    jsondoc::{self, Target},
    native::{
        JsonF64, NativeOptions, NativeProjectImporter, apply_builtin_shaders,
        stage_native_resources,
    },
    project::{self, Session, Value, lexpath, value},
    sha256, sysutil,
    util::{atomic_json, canonical_destination, default_unity_editor, lock_session, random_id},
};
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::value::RawValue;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    pub project: String,
    pub baseline: String,
    pub output: String,
    pub editor: String,
    pub scene: String,
    pub raw_routes: String,
    pub prepared: String,
    pub content_scenes: Vec<String>,
    pub content_prefabs: Vec<String>,
    /// Addressable scenes and prefabs served from sources, and the content
    /// declarations that exist only because of them.
    pub addressables: Vec<AddressableRoute>,
    pub addressable_content: Vec<String>,
    pub no_addressables: bool,
    pub builtin_shaders: Vec<String>,
    pub game_arguments: Vec<String>,
    pub watch: bool,
    pub interactive: bool,
    pub prepare_only: bool,
    pub control: bool,
    pub frames: i64,
    pub width: i64,
    pub height: i64,
    pub fps: i64,
}
impl LaunchOptions {
    /// Whether the Player gets a content route snapshot.
    pub fn has_content_routes(&self) -> bool {
        !self.content_scenes.is_empty() || !self.addressables.is_empty()
    }
    /// The part the native importer reads.
    pub fn native(&self) -> NativeOptions {
        NativeOptions {
            project: self.project.clone(),
            output: self.output.clone(),
            editor: self.editor.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BaselineNativeImport {
    pub catalog: String,
}
/// The validated preparation `current.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BaselineBuild {
    pub project: String,
    pub manifest: String,
    pub player: String,
    #[serde(rename = "nativeImport")]
    pub native_import: BaselineNativeImport,
}
impl BaselineBuild {
    /// Reads a baseline manifest file.
    pub fn read(path: &str) -> Result<Self> {
        let document = json::read_document(path)?;
        let mut baseline = Self::default();
        let root =
            json::members(&document.json, Target::VALUE, "baselineBuild").map_err(text_error)?;
        const FIELDS: [&str; 4] = ["project", "manifest", "player", "nativeImport"];
        jsondoc::decode_struct(root, &FIELDS, |index, member| {
            let at = Target {
                strukt: "baselineBuild",
                field: FIELDS[index],
            };
            match index {
                0 => jsondoc::set_string(&mut baseline.project, &member.value, at),
                1 => jsondoc::set_string(&mut baseline.manifest, &member.value, at),
                2 => jsondoc::set_string(&mut baseline.player, &member.value, at),
                _ => {
                    let fields = json::members(&member.value, at, "object")?;
                    jsondoc::decode_struct(fields, &["catalog"], |_, member| {
                        jsondoc::set_string(
                            &mut baseline.native_import.catalog,
                            &member.value,
                            Target {
                                strukt: "",
                                field: "nativeImport.catalog",
                            },
                        )
                    })
                }
            }
        })
        .map_err(text_error)?;
        Ok(baseline)
    }
}

pub(crate) fn os_error(operation: &str, path: &str, error: &std::io::Error) -> anyhow::Error {
    anyhow!(sysutil::Error::os(operation, path, error))
}
pub(crate) fn text_error(message: String) -> anyhow::Error {
    anyhow!(message)
}
/// The full error text, including its context chain.
pub(crate) fn err_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}
/// Prefixes an error as `prefix: error`, keeping its kind (such as not-exist) and cancellation.
pub(crate) fn wrap(prefix: &str, error: anyhow::Error) -> anyhow::Error {
    let message = format!("{prefix}: {}", err_text(&error));
    match error.downcast_ref::<sysutil::Error>() {
        Some(inner) => anyhow!(sysutil::Error::with_kind(inner.kind(), message)),
        None => match error.downcast::<cancel::Canceled>() {
            Ok(canceled) => anyhow::Error::new(canceled).context(prefix.to_string()),
            Err(_) => anyhow!(message),
        },
    }
}
pub(crate) fn read_file(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| os_error("open", path, &e))
}
/// Creates the directory and its parents with mode 0700.
pub(crate) fn mkdir_all(path: &str) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| os_error("mkdir", path, &e))
}
pub(crate) fn assemblies_of(assemblies: &NilVec<String>) -> Option<&[String]> {
    (!assemblies.is_nil()).then_some(&assemblies[..])
}

/// One event line: keys sorted, values encoded as compact JSON.
pub(crate) struct Event(BTreeMap<&'static str, Box<RawValue>>);
impl Event {
    pub fn new(name: &str) -> Self {
        Self(BTreeMap::new()).with("event", name)
    }
    pub fn with<T: Serialize + ?Sized>(mut self, key: &'static str, value: &T) -> Self {
        let raw = jsondoc::marshal(value)
            .ok()
            .and_then(|data| String::from_utf8(data).ok())
            .and_then(|text| RawValue::from_string(text).ok());
        if let Some(raw) = raw {
            self.0.insert(key, raw);
        }
        self
    }
    /// A value that is already encoded JSON, such as an acknowledgement map.
    pub fn raw(mut self, key: &'static str, json: String) -> Self {
        if let Ok(raw) = RawValue::from_string(json) {
            self.0.insert(key, raw);
        }
        self
    }
    pub fn seconds(self, key: &'static str, seconds: f64) -> Self {
        self.with(key, &JsonF64(seconds))
    }
    pub fn emit(self) {
        crate::util::emit_event(&self.0);
    }
}

/// Wakes the single consumer of a run's event sources (cancellation, Player
/// exit, source changes). Producers store their value first and then bump the generation.
pub struct Wake {
    generation: Mutex<u64>,
    changed: Condvar,
}
impl Wake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: Mutex::new(0),
            changed: Condvar::new(),
        })
    }
    pub fn notify(&self) {
        *self.generation.lock().unwrap() += 1;
        self.changed.notify_all();
    }
    /// Read before inspecting the sources, then pass to `wait`.
    pub fn seen(&self) -> u64 {
        *self.generation.lock().unwrap()
    }
    /// Blocks until a producer notified after `seen` was read, or the deadline.
    pub fn wait(&self, seen: u64, deadline: Option<Instant>) {
        let mut generation = self.generation.lock().unwrap();
        while *generation == seen {
            match deadline {
                None => generation = self.changed.wait(generation).unwrap(),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return;
                    }
                    generation = self
                        .changed
                        .wait_timeout(generation, deadline - now)
                        .unwrap()
                        .0;
                }
            }
        }
    }
    /// A `Wake` that is also notified when `ctx` ends, for as long as the
    /// returned bridge lives.
    pub fn bridge(ctx: &Context) -> WakeBridge {
        let wake = Self::new();
        let stop = Flag::new();
        let thread = {
            let (ctx, wake, stop) = (ctx.clone(), wake.clone(), stop.clone());
            std::thread::spawn(move || {
                cancel::select(&[Source::Context(&ctx), Source::Flag(&stop)], None);
                wake.notify();
            })
        };
        WakeBridge {
            wake,
            stop,
            thread: Some(thread),
        }
    }
}
pub struct WakeBridge {
    pub wake: Arc<Wake>,
    stop: Flag,
    thread: Option<JoinHandle<()>>,
}
impl Drop for WakeBridge {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Stops the Player when dropped.
pub(crate) struct StopOnDrop(pub Option<Player>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(player) = &mut self.0 {
            player.stop();
        }
    }
}

/// The file kinds whose changes reach the compiler or Prepare.
pub fn relevant_source(path: &str) -> bool {
    matches!(
        lexpath::rune_lower(sysutil::ext(path)).as_str(),
        ".unity" | ".prefab" | ".asset" | ".cs" | ".dll" | ".rsp" | ".meta" | ".asmdef"
            | ".asmref" | ".json" | ".fbx" | ".wav" | ".png" | ".mat" | ".controller"
            | ".anim" | ".overridecontroller" | ".mask" | ".physicmaterial" | ".physicsmaterial2d" | ".rendertexture" | ".mixer" | ".ogg" | ".jpg" | ".jpeg" | ".shader" | ".shadergraph" | ".hlsl"
            | ".cginc" | ".compute" | ".uxml" | ".uss" | ".inputactions"
            // Unsupported native profiles must also reach Prepare so that a changed
            // dependency is rejected explicitly rather than silently kept stale.
            | ".mp3" | ".aif" | ".aiff" | ".flac" | ".obj" | ".blend" | ".dae" | ".tga"
            | ".psd" | ".bmp" | ".tif" | ".tiff" | ".hdr" | ".exr" | ".ttf" | ".otf"
            | ".bytes" | ".txt" | ".mp4" | ".mov" | ".webm"
    )
}

pub fn apply_compiled_bindings(
    session: &Session,
    compiled: &CompileUpdate,
    script_types: &BTreeMap<String, String>,
) {
    let initializers = compiled
        .runtime_initializers
        .iter()
        .map(|initializer| {
            Value::Object(value::map([
                ("assembly", initializer.assembly.as_str().into()),
                ("typeName", initializer.type_name.as_str().into()),
                ("methodName", initializer.method_name.as_str().into()),
                ("phase", Value::Int(initializer.phase)),
            ]))
        })
        .collect();
    session.set_managed_bindings(
        assemblies_of(&compiled.assemblies),
        script_types,
        Some(Value::Array(initializers)),
    );
}

fn string_list(_: &[String], value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("empty content source".into());
    }
    Ok(())
}

fn unix_milliseconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}

/// The `run` command. The caller prints the error and exits with
/// status 1.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut flags = FlagSet::new("run");
    flags.string("project", ".", "source project");
    flags.string(
        "baseline",
        "",
        "validated preparation current.json; required unless --prepared",
    );
    flags.string(
        "output",
        "",
        "owned session cache beneath project Library/UnityMicroEditor or Temp",
    );
    flags.string(
        "editor",
        &default_unity_editor(),
        "matching Unity SDK, used for native import and C# compiler",
    );
    flags.string(
        "scene",
        "",
        "authored Assets/...unity scene, reusing the prepared host",
    );
    flags.string(
        "prepared",
        "",
        "immutable launch descriptor produced from prepared source/content manifests",
    );
    flags.list(
        "content-scene",
        string_list,
        "runtime scene GUID=Assets/...unity; repeat for additional scenes",
    );
    flags.list(
        "content-prefab",
        string_list,
        "runtime-addressed Assets/...prefab; repeat for additional prefabs",
    );
    flags.bool(
        "no-addressables",
        false,
        "leave the project's Addressables keys to the host's built content",
    );
    flags.list(
        "builtin-shader",
        string_list,
        "verified retained native built-in Shader snapshot JSON",
    );
    flags.bool(
        "watch",
        false,
        "watch sources and update the running Player",
    );
    flags.bool("interactive", false, "keep the Player window open");
    flags.bool(
        "prepare-only",
        false,
        "prepare immutable source/content/managed manifests without starting a Player",
    );
    flags.bool(
        "control",
        false,
        "enable revision-checked agent commands in the interactive Player",
    );
    flags.int("frames", 120, "frames in finite verification mode");
    flags.int("width", 960, "window width");
    flags.int("height", 540, "window height");
    flags.int("fps", 60, "frame cap");
    let (runner_args, game_args) = split_game_arguments(args);
    validate_game_arguments(&game_args)?;
    flags.parse(runner_args)?;
    let mut opts = LaunchOptions {
        project: flags.get_string("project"),
        baseline: flags.get_string("baseline"),
        output: flags.get_string("output"),
        editor: flags.get_string("editor"),
        scene: flags.get_string("scene"),
        raw_routes: String::new(),
        prepared: flags.get_string("prepared"),
        content_scenes: flags.get_list("content-scene"),
        content_prefabs: flags.get_list("content-prefab"),
        addressables: Vec::new(),
        addressable_content: Vec::new(),
        no_addressables: flags.get_bool("no-addressables"),
        builtin_shaders: flags.get_list("builtin-shader"),
        game_arguments: game_args,
        watch: flags.get_bool("watch"),
        interactive: flags.get_bool("interactive"),
        prepare_only: flags.get_bool("prepare-only"),
        control: flags.get_bool("control"),
        frames: flags.get_int("frames"),
        width: flags.get_int("width"),
        height: flags.get_int("height"),
        fps: flags.get_int("fps"),
    };
    if !flags.args().is_empty()
        || opts.frames <= 0
        || opts.fps <= 0
        || opts.width < 1
        || opts.height < 1
    {
        bail!("invalid run arguments");
    }
    if opts.watch {
        opts.interactive = true;
    }
    if opts.control && (!opts.interactive || opts.prepare_only) {
        bail!("--control requires an interactive Player run");
    }
    let project = lexpath::abs(&opts.project)?;
    let project = sysutil::eval_symlinks(&project)?;
    opts.project = project.clone();
    if opts.baseline.is_empty() && opts.prepared.is_empty() {
        bail!("run requires --baseline, the current.json reported by auto-prepare");
    }
    if opts.output.is_empty() {
        opts.output = sysutil::join(&[&project, "Library/UnityMicroEditor/Sessions/default"]);
    }
    opts.output = lexpath::abs(&opts.output)?;
    let output = canonical_destination(&opts.output)?;
    if !project::is_owned_cache_path(&project, &output) {
        bail!("Session output must stay under project Temp or Library/UnityMicroEditor");
    }
    opts.output = output.clone();
    if !opts.prepared.is_empty() {
        if opts.watch
            || opts.prepare_only
            || !opts.scene.is_empty()
            || !opts.content_scenes.is_empty()
            || !opts.content_prefabs.is_empty()
            || !opts.builtin_shaders.is_empty()
        {
            bail!(
                "--prepared is a launch mode; it cannot be combined with source preparation or --watch"
            );
        }
        return run_prepared_player(&mut opts);
    }
    let mut baseline = BaselineBuild::read(&opts.baseline).map_err(|e| wrap("read baseline", e))?;
    if baseline.project != project || baseline.manifest.is_empty() || baseline.player.is_empty() {
        bail!("baseline is not a validated preparation for this project");
    }
    mkdir_all(&output)?;
    let _lock = lock_session(&output)?;
    let signals = crate::packages::signal_context()?;
    let ctx = signals.with_cancel();
    let result = run_session(&ctx, &mut opts, &mut baseline);
    ctx.cancel();
    result
}

/// Adds the project's addressable scenes and prefabs to the content the
/// session prepares. Returns the settings and group assets they came from.
fn adopt_addressables(opts: &mut LaunchOptions) -> Vec<String> {
    if opts.no_addressables {
        return Vec::new();
    }
    let found = match addressables::discover(&opts.project) {
        Ok(found) => found,
        Err(error) => {
            Event::new("addressables-skipped")
                .with("error", &err_text(&error))
                .emit();
            return Vec::new();
        }
    };
    let mut scenes: BTreeSet<String> = opts
        .content_scenes
        .iter()
        .filter_map(|declaration| parse_content_scene(declaration).ok())
        .map(|(guid, _)| guid)
        .collect();
    for entry in &found.entries {
        if entry.scene {
            if scenes.insert(entry.guid.clone()) {
                opts.content_scenes
                    .push(format!("{}={}", entry.guid, entry.path));
                opts.addressable_content.push(entry.guid.clone());
            }
        } else if !opts.content_prefabs.contains(&entry.path) {
            opts.content_prefabs.push(entry.path.clone());
            opts.addressable_content.push(entry.path.clone());
        }
    }
    opts.addressables = found.entries;
    if !found.unsupported.is_empty() {
        Event::new("addressables-skipped")
            .with("paths", &found.unsupported)
            .with(
                "error",
                "only scenes and prefabs are served from sources; these stay with the host's built content",
            )
            .emit();
    }
    found.sources
}

/// Gives up the prefabs that were added for Addressables after they failed to
/// prepare. False when there is none, so the error is the caller's.
fn drop_addressable_prefabs(opts: &mut LaunchOptions, error: &anyhow::Error) -> bool {
    let dropped: Vec<String> = opts
        .addressable_content
        .iter()
        .filter(|path| opts.content_prefabs.contains(path))
        .cloned()
        .collect();
    if dropped.is_empty() {
        return false;
    }
    opts.content_prefabs.retain(|path| !dropped.contains(path));
    opts.addressable_content
        .retain(|path| !dropped.contains(path));
    opts.addressables
        .retain(|entry| entry.scene || !dropped.contains(&entry.path));
    Event::new("addressables-skipped")
        .with("paths", &dropped)
        .with("error", &err_text(error))
        .emit();
    true
}

/// Gives up one scene that was added for Addressables after it failed to prepare.
fn drop_addressable_scene(opts: &mut LaunchOptions, guid: &str, error: &anyhow::Error) -> bool {
    let Some(index) = opts.addressable_content.iter().position(|g| g == guid) else {
        return false;
    };
    opts.addressable_content.remove(index);
    opts.content_scenes
        .retain(|declaration| declaration.split('=').next() != Some(guid));
    let mut paths = Vec::new();
    opts.addressables.retain(|entry| {
        if entry.guid != guid {
            return true;
        }
        paths.push(entry.path.clone());
        false
    });
    paths.dedup();
    Event::new("addressables-skipped")
        .with("paths", &paths)
        .with("error", &err_text(error))
        .emit();
    true
}

fn run_session(
    ctx: &Context,
    opts: &mut LaunchOptions,
    baseline: &mut BaselineBuild,
) -> Result<()> {
    let (project, output) = (opts.project.clone(), opts.output.clone());
    let bridge = Wake::bridge(ctx);
    let wake = &bridge.wake;
    let started = Instant::now();
    let watcher = match opts.watch {
        true => Some(ProjectWatcher::new(ctx, wake, &project)?),
        false => None,
    };
    let staged_player = compiler::stage_host(ctx, &opts.output, &opts.editor, &baseline.player)?;
    baseline.player = staged_player.clone();
    let mut compiler = Compiler::new(
        &project,
        &opts.baseline,
        &staged_player,
        &opts.editor,
        &sysutil::join(&[&output, "compiler"]),
    )?;
    let initial_compile = compiler.compile(ctx, None)?;
    if let Some(compiled) = &initial_compile {
        baseline.player = compiled.player.clone();
    }
    let mut script_types = compiler.script_type_names(ctx)?;
    let addressable_sources = adopt_addressables(opts);
    let assemblies = compiler::player_assembly_paths(&baseline.player, &baseline.manifest)?;
    let initializers = json::read_runtime_initializers(&baseline.manifest)?;
    let (session, mut native_importer, mut prepared) = loop {
        let session = Arc::new(Session::new(project::Options {
            project: project.clone(),
            manifest: baseline.manifest.clone(),
            native_catalog: baseline.native_import.catalog.clone(),
            scene: opts.scene.clone(),
            extra_assets: opts.content_prefabs.clone(),
            script_types: (*script_types).clone(),
        })?);
        session.set_managed_bindings(
            assemblies_of(&assemblies),
            &BTreeMap::new(),
            Some(initializers.clone()),
        );
        if let Some(compiled) = &initial_compile {
            apply_compiled_bindings(&session, compiled, &script_types);
        }
        apply_builtin_shaders(&session, &opts.builtin_shaders)?;
        let mut native_importer = NativeProjectImporter::new(opts.native(), session.clone())?;
        native_importer.script_assemblies = compiler.script_assemblies();
        let layouts = compiler.behaviour_layouts(ctx, &script_types)?;
        native_importer.behaviour_code = compiler.behaviour_code.clone();
        native_importer.set_behaviour_layouts(ctx, layouts)?;
        match native_importer.prepare(ctx) {
            Ok(prepared) => break (session, native_importer, prepared),
            // An addressable prefab the raw loader cannot prepare must not
            // stop a session that runs without it.
            Err(error) if ctx.error().is_none() && drop_addressable_prefabs(opts, &error) => {}
            Err(error) => return Err(error),
        }
    };
    let mut content_importers = ContentImporters::new();
    let mut failed_scene = String::new();
    while opts.has_content_routes() {
        match prepare_content_routes(
            ctx,
            opts,
            baseline,
            initial_compile.as_ref(),
            &initializers,
            &script_types,
            Some(&native_importer),
            &mut content_importers,
            &[],
            &mut failed_scene,
        ) {
            Ok(routes) => {
                opts.raw_routes = routes;
                break;
            }
            Err(error)
                if ctx.error().is_none() && drop_addressable_scene(opts, &failed_scene, &error) => {
            }
            Err(error) => return Err(error),
        }
    }
    if !opts.addressables.is_empty() {
        Event::new("addressables")
            .with(
                "scenes",
                &opts.addressables.iter().filter(|e| e.scene).count(),
            )
            .with(
                "prefabs",
                &opts.addressables.iter().filter(|e| !e.scene).count(),
            )
            .emit();
    }
    if opts.prepare_only {
        Event::new("prepared")
            .with("player", &baseline.player)
            .with("manifest", &prepared.manifest)
            .with("contentRoutes", &opts.raw_routes)
            .seconds("prepareSeconds", prepared.elapsed_seconds)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        return Ok(());
    }
    let mut run_directory = sysutil::join(&[&output, "runs", &random_id()]);
    mkdir_all(&run_directory)?;
    let mut request = sysutil::join(&[&run_directory, "reload-request.json"]);
    let mut owned = StopOnDrop(Some(start_player(
        wake,
        opts,
        &baseline.player,
        &prepared.manifest,
        &run_directory,
        &request,
    )?));
    macro_rules! player {
        () => {
            owned.0.as_mut().expect("owned Player")
        };
    }
    player!().wait_ready(ctx, Duration::from_secs(45))?;
    Event::new("ready")
        .with("pid", &player!().pid())
        .with("manifest", &prepared.manifest)
        .seconds("prepareSeconds", prepared.elapsed_seconds)
        .seconds("startupSeconds", started.elapsed().as_secs_f64())
        .with("report", &player!().report)
        .emit();
    let Some(watcher) = watcher else {
        loop {
            let seen = wake.seen();
            if let Some(result) = player!().take_done() {
                player!().exited = true;
                if let Some(error) = result {
                    bail!(error);
                }
                return require_player_report(&player!().report);
            }
            if let Some(error) = ctx.error() {
                return Err(error.into());
            }
            wake.wait(seen, None);
        }
    };
    let mut pending_compile: Option<CompileUpdate> = None;
    let mut active_compile = initial_compile;
    let mut managed_rejected = false;
    // When several sources are ready at once the order is cancellation,
    // Player exit, source change.
    loop {
        let seen = wake.seen();
        if ctx.error().is_some() {
            return Ok(());
        }
        if let Some(result) = player!().take_done() {
            player!().exited = true;
            if let Some(error) = result {
                bail!(error);
            }
            return require_player_report(&player!().report);
        }
        let Some(change) = watcher.try_recv() else {
            wake.wait(seen, None);
            continue;
        };
        if let Some(error) = change.error {
            bail!(error);
        }
        let mut paths = vec![change.path];
        let (changed_at, changed_wall) = (change.at, change.wall);
        let debounce = Instant::now() + Duration::from_millis(35);
        loop {
            let seen = wake.seen();
            if let Some(next) = watcher.try_recv() {
                if let Some(error) = next.error {
                    bail!(error);
                }
                paths.push(next.path);
                continue;
            }
            if Instant::now() >= debounce {
                break;
            }
            if ctx.error().is_some() {
                return Ok(());
            }
            wake.wait(seen, Some(debounce));
        }
        let rejected = |error: &anyhow::Error| {
            let text = err_text(error);
            let mut event = Event::new("update-rejected")
                .with("error", &text)
                .with("paths", &paths);
            // The Player keeps its old code until the project is prepared again.
            if compiler::requires_new_graph(&text) {
                event = event.with("preparationRequired", &true);
            }
            event.emit();
        };
        session.invalidate(&paths);
        if let Err(error) = apply_builtin_shaders(&session, &opts.builtin_shaders) {
            rejected(&error);
            continue;
        }
        let update_started = Instant::now();
        let compile_paths = (!managed_rejected).then_some(&paths[..]);
        let compiled = match compiler.compile(ctx, compile_paths) {
            Ok(compiled) => compiled,
            Err(error) => {
                managed_rejected = true;
                rejected(&error);
                continue;
            }
        };
        managed_rejected = false;
        if compiled.is_some() {
            pending_compile = compiled;
        }
        if let Some(pending) = &pending_compile {
            match compiler.script_type_names(ctx) {
                Ok(next) => script_types = next,
                Err(error) => {
                    managed_rejected = true;
                    rejected(&error);
                    continue;
                }
            }
            // A baked controller lays its behaviours out as the new
            // assemblies declare them.
            native_importer.script_assemblies = compiler.script_assemblies();
            if let Err(error) = compiler
                .behaviour_layouts(ctx, &script_types)
                .and_then(|layouts| {
                    native_importer.behaviour_code = compiler.behaviour_code.clone();
                    native_importer.set_behaviour_layouts(ctx, layouts)
                })
            {
                managed_rejected = true;
                rejected(&error);
                continue;
            }
            apply_compiled_bindings(&session, pending, &script_types);
        }
        let next = match native_importer.prepare(ctx) {
            Ok(next) => next,
            Err(error) => {
                rejected(&error);
                continue;
            }
        };
        let next_player = match &pending_compile {
            Some(pending) => pending.player.clone(),
            None => player!().path.clone(),
        };
        if paths.iter().any(|path| {
            addressable_sources
                .iter()
                .any(|source| path.ends_with(source))
        }) {
            // The served keys are fixed when the session starts.
            Event::new("addressables-changed")
                .with("paths", &paths)
                .with(
                    "message",
                    "Addressables groups changed; restart the session to serve the new entries",
                )
                .emit();
        }
        let mut next_routes = opts.raw_routes.clone();
        if opts.has_content_routes() {
            let mut next_baseline = baseline.clone();
            next_baseline.player = next_player.clone();
            next_routes = match prepare_content_routes(
                ctx,
                opts,
                &next_baseline,
                pending_compile.as_ref().or(active_compile.as_ref()),
                &initializers,
                &script_types,
                Some(&native_importer),
                &mut content_importers,
                &paths,
                &mut failed_scene,
            ) {
                Ok(routes) => routes,
                Err(error) => {
                    rejected(&error);
                    continue;
                }
            };
        }
        let routes_equal = match content_routes_runtime_equal(&opts.raw_routes, &next_routes) {
            Ok(equal) => equal,
            Err(error) => {
                rejected(&error);
                continue;
            }
        };
        if pending_compile.is_some() || !routes_equal {
            // Mono cannot unload or replace an already loaded assembly identity.
            // Additive content routes are also fixed for one Player lifetime.
            // Publish a new immutable revision after stopping our instance.
            opts.raw_routes = next_routes.clone();
            player!().stop();
            run_directory = sysutil::join(&[&output, "runs", &random_id()]);
            mkdir_all(&run_directory)?;
            request = sysutil::join(&[&run_directory, "reload-request.json"]);
            owned.0 = Some(start_player(
                wake,
                opts,
                &next_player,
                &next.manifest,
                &run_directory,
                &request,
            )?);
            player!().wait_ready(ctx, Duration::from_secs(45))?;
            let mut event = Event::new("content-updated")
                .with("paths", &paths)
                .with("manifest", &next.manifest)
                .with("contentRoutes", &next_routes)
                .seconds("sourceToReadySeconds", changed_at.elapsed().as_secs_f64())
                .with("pid", &player!().pid())
                .with("report", &player!().report);
            if let Some(pending) = pending_compile.take() {
                event = event
                    .with("event", "code-updated")
                    .with("sourceHashes", &pending.source_hashes)
                    .with("compiledAssemblies", &pending.compiled_assemblies)
                    .with("declarationChanged", &pending.declaration_changed)
                    .with("compileSeconds", &pending.elapsed_seconds);
                active_compile = Some(pending);
            }
            event.emit();
            prepared = next;
            continue;
        }
        if scene_runtime_equal(&prepared.manifest, &next.manifest)? {
            Event::new("unchanged")
                .with("paths", &paths)
                .seconds("prepareSeconds", next.elapsed_seconds)
                .with("pid", &player!().pid())
                .with("revision", &prepared.revision)
                .emit();
            continue;
        }
        let data = read_file(&next.manifest)?;
        let id = random_id();
        let mut reload = value::map([
            ("schemaVersion", Value::Int(1)),
            ("requestId", id.as_str().into()),
            ("manifest", next.manifest.as_str().into()),
            ("manifestSha256", sha256(&data).into()),
            ("revision", next.revision.as_str().into()),
            (
                "requestedUtcUnixMilliseconds",
                Value::Int(unix_milliseconds(SystemTime::now())),
            ),
            (
                "sourceChangedUtcUnixMilliseconds",
                Value::Int(unix_milliseconds(changed_wall)),
            ),
        ]);
        if let Some((patch, patch_hash)) = make_scene_patch(&prepared.manifest, &next.manifest)? {
            reload.insert("patch".into(), patch.into());
            reload.insert("patchSha256".into(), patch_hash.into());
        }
        stage_native_resources(&next.manifest, &run_directory)?;
        atomic_json(
            &request,
            &RawValue::from_string(value::compact_map(&reload)?)?,
        )?;
        let reply = wait_reload_ack(
            ctx,
            &format!("{request}.ack.json"),
            &id,
            Duration::from_secs(20),
        );
        let ack_json = match &reply.ack {
            Some(ack) => value::compact_map(ack)?,
            None => "null".into(),
        };
        let ack = reply.ack.unwrap_or_default();
        let field = |key: &str| ack.get(key).unwrap_or(&Value::Null);
        if let Some(error) = reply.error {
            Event::new("update-rejected")
                .with("error", &err_text(&error))
                .raw("ack", ack_json)
                .emit();
            if field("restartRequired").is_false()
                && field("loadedRevision").as_str() == Some(prepared.revision.as_str())
            {
                continue;
            }
            return Err(wrap(
                "live Player revision is uncertain or requires a restart",
                error,
            ));
        }
        if field("loadedRevision").as_str() != Some(next.revision.as_str())
            || !field("restartRequired").is_false()
            || *field("pid") != Value::Float(f64::from(player!().pid()))
        {
            bail!("Player acknowledged an incompatible live revision or process");
        }
        Event::new("updated")
            .with("paths", &paths)
            .seconds("prepareSeconds", next.elapsed_seconds)
            .seconds("elapsedSeconds", update_started.elapsed().as_secs_f64())
            .seconds("sourceToAckSeconds", changed_at.elapsed().as_secs_f64())
            .raw("ack", ack_json)
            .with("cache", &next.cache)
            .emit();
        prepared = next;
    }
}
