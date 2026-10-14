//! The `run` command. It stages the prepared host, compiles the project's
//! scripts, prepares the scene, owns one Player process and, with `--watch`,
//! turns source changes into live patches, reloads or Player restarts.
mod addressables;
mod commands;
mod json;
mod noop;
mod patch;
mod player;
mod prepared;
mod routes;
mod watch;
mod window;

#[cfg(test)]
mod hot_tests;
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
    Player, READY_BUDGET, ReadyBudget, ReloadAck, process_cpu, require_hardware_graphics,
    require_player_report, start_player, wait_reload_ack,
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
    hotreload::{self, Fallback, Outcome, PlayerCode},
    jsondoc::{self, Target},
    native::{
        JsonF64, NativeOptions, NativeProjectImporter, apply_builtin_shaders,
        stage_native_resources,
    },
    project::{self, Prepared, Session, Value, lexpath, value},
    sha256, sysutil,
    util::{atomic_json, canonical_destination, default_unity_editor, lock_session, random_id},
};
use anyhow::{Result, anyhow, bail};
use commands::Commands;
use serde::Serialize;
use serde_json::value::RawValue;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use window::{Compiled, Loop, Restarted, Scene, Steps, Window};

/// The watch loop for the tests of the controller, which reads what it prints.
#[cfg(test)]
pub(crate) mod testing {
    pub(crate) use super::commands::Commands;
    pub(crate) use super::window::{
        Compiled, Loop, Restarted, Scene, Steps, Window, ready_event, scene_rejected_event,
        unchanged_event, updated_event,
    };
    pub(crate) use super::{Event, watch};
}

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
    /// C# edits are patched into the running Player; otherwise, and for an
    /// edit that cannot be patched, the Player restarts.
    pub hot_reload: bool,
    pub hot_reload_fallback: Fallback,
    /// Commands are read from standard input.
    pub commands_stdin: bool,
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
    /// The part the native importer reads, with the host Player the content
    /// is imported for.
    pub fn native(&self, player: &str) -> NativeOptions {
        NativeOptions {
            project: self.project.clone(),
            output: self.output.clone(),
            editor: self.editor.clone(),
            player: player.to_string(),
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
    /// The line `emit` prints, without its newline.
    #[cfg(test)]
    pub fn line(&self) -> String {
        jsondoc::marshal(&self.0)
            .ok()
            .and_then(|data| String::from_utf8(data).ok())
            .unwrap_or_default()
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

/// The flags of `run`. Their usage text is printed for a flag error.
pub(crate) fn run_flags() -> FlagSet {
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
    flags.bool(
        "no-hot-reload",
        false,
        "restart the Player for every C# change instead of patching the running one",
    );
    flags.string(
        "hot-reload-fallback",
        "restart",
        "what a C# change that cannot be patched does: restart the Player, or report it and keep the Player until a recompile command (report requires -commands-stdin)",
    );
    flags.bool(
        "commands-stdin",
        false,
        "read JSON command lines from standard input: {\"command\":\"recompile\",\"requestId\":\"<id>\"} restarts the Player on the newest compiled code",
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
    flags
}

/// The options of a `run` command line, with the checks that need no file.
pub(crate) fn launch_options(args: &[String]) -> Result<LaunchOptions> {
    let mut flags = run_flags();
    let (runner_args, game_args) = split_game_arguments(args);
    validate_game_arguments(&game_args)?;
    flags.parse(runner_args)?;
    let Some(fallback) = Fallback::parse(&flags.get_string("hot-reload-fallback")) else {
        bail!("--hot-reload-fallback must be restart or report");
    };
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
        hot_reload: false,
        hot_reload_fallback: fallback,
        commands_stdin: flags.get_bool("commands-stdin"),
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
    let no_hot_reload = flags.get_bool("no-hot-reload");
    if !opts.watch && (opts.commands_stdin || flags.is_set("hot-reload-fallback")) {
        bail!("--commands-stdin and --hot-reload-fallback require --watch");
    }
    if no_hot_reload && flags.is_set("hot-reload-fallback") {
        bail!("--hot-reload-fallback has no effect with --no-hot-reload");
    }
    // A reported edit waits for a `recompile` command, which only arrives
    // on standard input.
    if fallback == Fallback::Report && !opts.commands_stdin {
        bail!("--hot-reload-fallback report requires --commands-stdin");
    }
    // Code is patched into a Player that is watched; nothing else starts one
    // that outlives an edit.
    opts.hot_reload = opts.watch && !no_hot_reload && !opts.prepare_only;
    Ok(opts)
}

/// The `run` command. The caller prints the error and exits with
/// status 1.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut opts = launch_options(args)?;
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

/// What `prepare` published for the decision of a window.
struct Next {
    prepared: Prepared,
    routes: String,
    /// The Player the revision and the routes are bound to.
    player: String,
}

/// Hot reload of a session: its two programs and the code of the running
/// Player process.
struct Hot {
    tools: hotreload::Tools,
    code: PlayerCode,
}

/// A watched session between two windows: what the steps of a window work on.
struct Live<'a> {
    wake: &'a Arc<Wake>,
    opts: &'a mut LaunchOptions,
    baseline: &'a BaselineBuild,
    watcher: &'a ProjectWatcher,
    commands: &'a Commands,
    /// Always the newest accepted graph.
    compiler: Compiler,
    session: Arc<Session>,
    native_importer: NativeProjectImporter,
    content_importers: ContentImporters,
    failed_scene: String,
    initializers: Value,
    /// The script types of the bindings the session holds.
    script_types: compiler::NilMap<String>,
    addressable_sources: Vec<String>,
    /// The revision the running Player confirmed last.
    prepared: Prepared,
    next: Option<Next>,
    owned: StopOnDrop,
    run_directory: String,
    request: String,
    /// The newest compile, which no Player process has loaded.
    pending_compile: Option<CompileUpdate>,
    /// The compile the running Player was started with.
    active_compile: Option<CompileUpdate>,
    hot: Option<Hot>,
    /// Whether a line of timings is printed per window (`UME_TIMINGS=1`).
    timings: bool,
    times: WindowTimes,
}
impl Live<'_> {
    fn player(&mut self) -> &mut Player {
        self.owned.0.as_mut().expect("owned Player")
    }
}
impl Steps for Live<'_> {
    fn emit(&mut self, event: Event) {
        event.emit();
    }

    fn invalidate(&mut self, paths: &[String]) -> Result<()> {
        self.times.start = Some(Instant::now());
        self.session.invalidate(paths);
        apply_builtin_shaders(&self.session, &self.opts.builtin_shaders)
    }

    fn compile(&mut self, ctx: &Context, paths: Option<&[String]>) -> Result<Compiled> {
        let affected = paths.is_none_or(|paths| self.compiler.compiler_changes(paths));
        let started = Instant::now();
        let compiled = self.compiler.compile(ctx, paths);
        self.times.compile = Some(started.elapsed());
        Ok(match compiled? {
            Some(update) => {
                self.pending_compile = Some(update);
                Compiled::Updated
            }
            None if affected => Compiled::Unchanged,
            None => Compiled::Unaffected,
        })
    }

    fn compile_pending(&self) -> bool {
        self.pending_compile.is_some()
    }

    fn patch_code(&mut self, ctx: &Context, window: &Window) -> Result<Outcome> {
        let Some(Hot { tools, code }) = &mut self.hot else {
            bail!("hot reload is not set up for this session");
        };
        let current = match hotreload::newest_assemblies(&self.compiler) {
            Ok(current) => current,
            Err(error) => {
                return Ok(Outcome::Unsupported(hotreload::Refusal {
                    reasons: vec![hotreload::Reason::new(
                        hotreload::CODE_BUILDER_FAILED,
                        "",
                        &err_text(&error),
                    )],
                    report: String::new(),
                }));
            }
        };
        let player = self.owned.0.as_ref().expect("owned Player");
        let exited = player.exit_probe();
        let started = Instant::now();
        let outcome = code.attempt(
            ctx,
            &hotreload::Attempt {
                current: &current,
                build: &|ctx, request| tools.build(ctx, request),
                exited: &exited,
                pid: player.pid(),
                source_changed: window.changed_wall,
                timeout: hotreload::ACK_TIMEOUT,
            },
        );
        self.times.patch = Some(started.elapsed());
        outcome
    }

    fn pid(&self) -> i32 {
        self.owned.0.as_ref().expect("owned Player").pid()
    }

    fn script_guid(&self, type_name: &str) -> Option<String> {
        self.script_types
            .iter()
            .find(|(_, name)| name.as_str() == type_name)
            .map(|(guid, _)| guid.clone())
    }

    fn revision(&self) -> String {
        self.prepared.revision.clone()
    }

    fn bind_newest(&mut self, ctx: &Context) -> Result<()> {
        let Some(pending) = &self.pending_compile else {
            return Ok(());
        };
        self.script_types = self.compiler.script_type_names(ctx)?;
        // A baked controller lays its behaviours out as the new
        // assemblies declare them.
        self.native_importer.script_assemblies = self.compiler.script_assemblies();
        let layouts = self.compiler.behaviour_layouts(ctx, &self.script_types)?;
        self.native_importer.behaviour_code = self.compiler.behaviour_code.clone();
        self.native_importer.set_behaviour_layouts(ctx, layouts)?;
        apply_compiled_bindings(&self.session, pending, &self.script_types);
        Ok(())
    }

    fn prepare(
        &mut self,
        ctx: &Context,
        window: &Window,
        newest: bool,
        again: bool,
    ) -> Result<bool> {
        let prepared = self.native_importer.prepare(ctx)?;
        let running = self.player().path.clone();
        let player = match (&self.pending_compile, newest) {
            (Some(pending), true) => pending.player.clone(),
            _ => running,
        };
        if !again
            && window.paths.iter().any(|path| {
                self.addressable_sources
                    .iter()
                    .any(|source| path.ends_with(source))
            })
        {
            // The served keys are fixed when the session starts.
            Event::new("addressables-changed")
                .with("paths", &window.paths)
                .with(
                    "message",
                    "Addressables groups changed; restart the session to serve the new entries",
                )
                .emit();
        }
        let mut routes = self.opts.raw_routes.clone();
        if self.opts.has_content_routes() {
            let mut next_baseline = self.baseline.clone();
            next_baseline.player = player.clone();
            // The routes of the running Player keep the bindings it was
            // started with: it checks the assemblies a manifest names
            // against those it loaded.
            let compiled = match newest {
                true => self
                    .pending_compile
                    .as_ref()
                    .or(self.active_compile.as_ref()),
                false => self.active_compile.as_ref(),
            };
            routes = prepare_content_routes(
                ctx,
                self.opts,
                &next_baseline,
                compiled,
                &self.initializers,
                &self.script_types,
                Some(&self.native_importer),
                &mut self.content_importers,
                &window.paths,
                &mut self.failed_scene,
            )?;
        }
        let equal = content_routes_runtime_equal(&self.opts.raw_routes, &routes)?;
        self.next = Some(Next {
            prepared,
            routes,
            player,
        });
        Ok(equal)
    }

    fn restart(&mut self, ctx: &Context, window: &Window) -> Result<Restarted> {
        let next = self.next.take().expect("prepared revision");
        // Additive content routes are also fixed for one Player lifetime.
        // Publish a new immutable revision after stopping our instance.
        self.opts.raw_routes = next.routes.clone();
        self.player().stop();
        self.run_directory = sysutil::join(&[&self.opts.output, "runs", &random_id()]);
        mkdir_all(&self.run_directory)?;
        self.request = sysutil::join(&[&self.run_directory, "reload-request.json"]);
        // One Player process loads one applier. A new process takes the
        // current one; the one before stays usable when that cannot be built.
        let applier = self.hot.as_ref().map(|hot| {
            hot.tools
                .applier(ctx)
                .unwrap_or_else(|_| hot.code.applier().clone())
        });
        self.owned.0 = Some(start_player(
            self.wake,
            self.opts,
            &next.player,
            &next.prepared.manifest,
            &self.run_directory,
            &self.request,
        )?);
        self.player().wait_ready(ctx, READY_BUDGET)?;
        let (pid, report) = (self.player().pid(), self.player().report.clone());
        let event =
            window::ready_event(window, &next.prepared.manifest, &next.routes, pid, &report);
        let compile = self.pending_compile.take();
        if let Some(pending) = &compile {
            self.active_compile = Some(pending.clone());
        }
        if let (Some(hot), Some(applier)) = (&mut self.hot, applier) {
            // A process whose assemblies cannot be read is patched with
            // nothing: every edit then takes the fallback.
            let loaded =
                hotreload::loaded_assemblies(&self.compiler, &next.player).unwrap_or_default();
            hot.code = PlayerCode::new(&self.run_directory, applier, loaded);
        }
        self.prepared = next.prepared;
        Ok(Restarted { event, compile })
    }

    fn merged_recompile(&mut self) -> Option<String> {
        // A change the watcher holds is newer than what the new process
        // loaded; its window answers the command.
        if self.watcher.pending() {
            return None;
        }
        self.commands.try_recv().map(|command| command.request_id)
    }

    fn scene(&mut self, ctx: &Context, window: &Window, started: Instant) -> Result<Scene> {
        let next = self.next.take().expect("prepared revision").prepared;
        let pid = self.pid();
        if scene_runtime_equal(&self.prepared.manifest, &next.manifest)? {
            return Ok(Scene::Unchanged(window::unchanged_event(
                window,
                next.elapsed_seconds,
                pid,
                &self.prepared.revision,
            )));
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
                Value::Int(unix_milliseconds(window.changed_wall)),
            ),
        ]);
        if let Some((patch, patch_hash)) =
            make_scene_patch(&self.prepared.manifest, &next.manifest)?
        {
            reload.insert("patch".into(), patch.into());
            reload.insert("patchSha256".into(), patch_hash.into());
        }
        stage_native_resources(&next.manifest, &self.run_directory)?;
        atomic_json(
            &self.request,
            &RawValue::from_string(value::compact_map(&reload)?)?,
        )?;
        let reply = wait_reload_ack(
            ctx,
            &format!("{}.ack.json", self.request),
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
            let event = window::scene_rejected_event(&err_text(&error), ack_json);
            let kept = field("restartRequired").is_false()
                && field("loadedRevision").as_str() == Some(self.prepared.revision.as_str());
            let fatal = (!kept).then(|| {
                wrap(
                    "live Player revision is uncertain or requires a restart",
                    error,
                )
            });
            return Ok(Scene::Rejected { event, fatal });
        }
        if field("loadedRevision").as_str() != Some(next.revision.as_str())
            || !field("restartRequired").is_false()
            || *field("pid") != Value::Float(f64::from(pid))
        {
            bail!("Player acknowledged an incompatible live revision or process");
        }
        let event =
            window::updated_event(window, next.elapsed_seconds, started, ack_json, &next.cache);
        self.prepared = next;
        Ok(Scene::Updated(event))
    }

    fn window_done(&mut self, window: &Window) {
        let times = std::mem::take(&mut self.times);
        let empty = compiler::Phases::default();
        let compile = match times.compile {
            Some(_) => &self.compiler.last_phases,
            None => &empty,
        };
        let patch = match (&self.hot, times.patch) {
            (Some(hot), Some(_)) => hot.code.last_phases(),
            _ => &empty,
        };
        if let Some(line) = timings_report(self.timings, window, &times, compile, patch) {
            eprintln!("{line}");
        }
    }
}

/// Whether the runner prints a line of timings per window.
pub(crate) fn timings_enabled() -> bool {
    std::env::var_os("UME_TIMINGS").is_some_and(|value| value == "1")
}

/// What the steps of one window took, as the window waited for them.
#[derive(Debug, Default)]
pub(crate) struct WindowTimes {
    /// The first step of the window.
    pub start: Option<Instant>,
    pub compile: Option<Duration>,
    pub patch: Option<Duration>,
}

/// The line of timings of a window, when they are asked for: one JSON object
/// with the time from the first change to the printed events, the steps, and
/// every phase of the compile and of the patch attempt (0 for a phase that
/// did not run). Standard error is a log; nothing parses it.
pub(crate) fn timings_report(
    enabled: bool,
    window: &Window,
    times: &WindowTimes,
    compile: &compiler::Phases,
    patch: &compiler::Phases,
) -> Option<String> {
    if !enabled {
        return None;
    }
    let seconds = |phases: &compiler::Phases, names: &[&'static str]| {
        let mut map = serde_json::Map::new();
        for (name, value) in phases.listed(names) {
            map.insert(name.into(), serde_json::json!(value));
        }
        serde_json::Value::Object(map)
    };
    let optional = |time: Option<Duration>| time.map_or(0.0, |time| time.as_secs_f64());
    let line = serde_json::json!({
        "timings": "window",
        "paths": window.paths.len(),
        "recompile": window.recompile.is_some(),
        "totalSeconds": window.changed_at.elapsed().as_secs_f64(),
        "waitSeconds": times
            .start
            .map_or(0.0, |start| start.saturating_duration_since(window.changed_at).as_secs_f64()),
        "compileSeconds": optional(times.compile),
        "patchSeconds": optional(times.patch),
        "compile": seconds(compile, &compiler::COMPILE_PHASES),
        "patch": seconds(patch, &compiler::PATCH_PHASES),
    });
    Some(line.to_string())
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
    // Read from the start, so that a sender never waits on a full pipe.
    let commands = match opts.commands_stdin {
        true => Commands::read(std::io::stdin(), wake),
        false => Commands::none(),
    };
    let staged_player = compiler::stage_host(ctx, &opts.output, &opts.editor, &baseline.player)?;
    baseline.player = staged_player.clone();
    // The patch builder and the applier are built beside the startup compile
    // and the Player start, which take longer than both.
    let setup = opts.hot_reload.then(|| {
        hotreload::Setup::start(
            ctx,
            &opts.editor,
            &compiler::player_managed(&staged_player),
            &output,
        )
    });
    let mut compiler = Compiler::new(
        &project,
        &opts.baseline,
        &staged_player,
        &opts.editor,
        &sysutil::join(&[&output, "compiler"]),
    )?;
    // A watched session compiles again and again; its compiles keep their
    // processes and caches. Preparations and runs without --watch do not.
    if opts.watch {
        compiler.serve(&sysutil::join(&[&output, "compiler"]));
    }
    let initial_compile = compiler.compile(ctx, None)?;
    if let Some(compiled) = &initial_compile {
        baseline.player = compiled.player.clone();
    }
    let script_types = compiler.script_type_names(ctx)?;
    let addressable_sources = adopt_addressables(opts);
    let assemblies = compiler::player_assembly_paths(&baseline.player, &baseline.manifest)?;
    let initializers = json::read_runtime_initializers(&baseline.manifest)?;
    let (session, native_importer, prepared) = loop {
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
        let mut native_importer =
            NativeProjectImporter::new(opts.native(&baseline.player), session.clone())?;
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
    let run_directory = sysutil::join(&[&output, "runs", &random_id()]);
    mkdir_all(&run_directory)?;
    let request = sysutil::join(&[&run_directory, "reload-request.json"]);
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
    player!().wait_ready(ctx, READY_BUDGET)?;
    // A session whose patch programs cannot be built is not a failed one:
    // its C# edits restart the Player, as they do without hot reload.
    let hot = match setup.map(hotreload::Setup::finish) {
        Some(Ok((tools, applier))) => {
            // A watched session patches edit after edit: its builder stays.
            let tools = match opts.watch {
                true => tools.served(),
                false => tools,
            };
            let loaded =
                hotreload::loaded_assemblies(&compiler, &baseline.player).unwrap_or_default();
            Some(Hot {
                tools,
                code: PlayerCode::new(&run_directory, applier, loaded),
            })
        }
        Some(Err(error)) => {
            if let Some(canceled) = ctx.error() {
                return Err(canceled.into());
            }
            opts.hot_reload = false;
            Event::new("hot-reload-unavailable")
                .with("error", &err_text(&error))
                .with("message", "C# changes restart the Player in this session")
                .emit();
            None
        }
        None => None,
    };
    let mut ready = Event::new("ready")
        .with("pid", &player!().pid())
        .with("manifest", &prepared.manifest)
        .seconds("prepareSeconds", prepared.elapsed_seconds)
        .seconds("startupSeconds", started.elapsed().as_secs_f64())
        .with("report", &player!().report);
    if opts.watch {
        ready = ready.with("hotReload", &hot.is_some());
    }
    ready.emit();
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
    let mut state = Loop::new(hot.is_some(), opts.hot_reload_fallback);
    let mut live = Live {
        wake,
        opts,
        baseline,
        watcher: &watcher,
        commands: &commands,
        compiler,
        session,
        native_importer,
        content_importers,
        failed_scene,
        initializers,
        script_types,
        addressable_sources,
        prepared,
        next: None,
        owned,
        run_directory,
        request,
        pending_compile: None,
        active_compile: initial_compile,
        hot,
        timings: timings_enabled(),
        times: WindowTimes::default(),
    };
    watch(
        ctx,
        wake,
        &watcher,
        &commands,
        &mut state,
        &mut live,
        |live| {
            let result = live.player().take_done()?;
            live.player().exited = true;
            Some(match result {
                Some(error) => Err(anyhow!(error)),
                None => require_player_report(&live.player().report),
            })
        },
    )
}

/// The loop of a watched session: one window after another until the run
/// ends, the Player exits (`exited` then gives the result) or a window
/// fails. When several sources are ready at once the order is cancellation,
/// Player exit, source change, command.
///
/// A `sync` command is answered between two windows, by the event `synced`,
/// once every source change that the watcher had been told of when the
/// loop saw the command has gone through a window. Whoever reads `synced`
/// has therefore read the events of everything saved before the command.
/// The number of those changes comes from a flush that read the kernel's
/// queue to its end. While the watcher's own queue is full no flush can,
/// and the command waits without a number for the window that makes room.
pub(crate) fn watch<S: Steps>(
    ctx: &Context,
    wake: &Wake,
    watcher: &ProjectWatcher,
    commands: &Commands,
    state: &mut Loop,
    steps: &mut S,
    mut exited: impl FnMut(&mut S) -> Option<Result<()>>,
) -> Result<()> {
    // Request ids, each with the number of changes to hand out first; none
    // until a flush has counted them.
    let mut syncs: std::collections::VecDeque<(String, Option<u64>)> = Default::default();
    loop {
        let seen = wake.seen();
        if ctx.error().is_some() {
            return Ok(());
        }
        if let Some(result) = exited(steps) {
            return result;
        }
        for id in commands.take_syncs() {
            syncs.push_back((id, None));
        }
        // One flush counts for every command that has no number yet: each
        // of them arrived before it.
        if syncs.iter().any(|(_, changes)| changes.is_none())
            && let Some(count) = watcher.flush()
        {
            for (_, changes) in syncs.iter_mut().filter(|(_, changes)| changes.is_none()) {
                *changes = Some(count);
            }
        }
        while syncs
            .front()
            .is_some_and(|(_, changes)| changes.is_some_and(|count| watcher.taken() >= count))
        {
            let (id, _) = syncs.pop_front().expect("a waiting sync");
            steps.emit(Event::new("synced").with("requestId", &id));
        }
        let Some(mut window) = open_window(watcher, commands)? else {
            wake.wait(seen, None);
            continue;
        };
        if !fill_window(ctx, wake, watcher, commands, &mut window)? {
            return Ok(());
        }
        let result = state.window(ctx, steps, &window, &|| watcher.pending());
        steps.window_done(&window);
        result?;
    }
}

/// How long a window waits for more changes of the same save.
const DEBOUNCE: Duration = Duration::from_millis(35);

/// The window that the next source change opens, or the next command when
/// no source changed. `None` when there is neither.
pub(crate) fn open_window(watcher: &ProjectWatcher, commands: &Commands) -> Result<Option<Window>> {
    if let Some(change) = watcher.try_recv() {
        if let Some(error) = change.error {
            bail!(error);
        }
        return Ok(Some(Window {
            paths: vec![change.path],
            changed_at: change.at,
            changed_wall: change.wall,
            recompile: None,
        }));
    }
    Ok(commands.try_recv().map(|command| Window {
        paths: Vec::new(),
        changed_at: command.at,
        changed_wall: command.wall,
        recompile: Some(command.request_id),
    }))
}

/// Adds what arrives until the debounce is over: every source change, and
/// a command when the window has none. False when the run ended meanwhile.
pub(crate) fn fill_window(
    ctx: &Context,
    wake: &Wake,
    watcher: &ProjectWatcher,
    commands: &Commands,
    window: &mut Window,
) -> Result<bool> {
    let debounce = Instant::now() + DEBOUNCE;
    loop {
        let seen = wake.seen();
        if let Some(next) = watcher.try_recv() {
            if let Some(error) = next.error {
                bail!(error);
            }
            window.paths.push(next.path);
            continue;
        }
        // One command a window: the next one gets a restart and an answer
        // of its own.
        if window.recompile.is_none()
            && let Some(command) = commands.try_recv()
        {
            window.recompile = Some(command.request_id);
            continue;
        }
        if Instant::now() >= debounce {
            return Ok(true);
        }
        if ctx.error().is_some() {
            return Ok(false);
        }
        wake.wait(seen, Some(debounce));
    }
}
