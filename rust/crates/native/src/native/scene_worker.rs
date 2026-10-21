//! The worker that writes a session's scenes in the Player format, kept
//! alive between edits. Its first run imports the scene and everything it
//! refers to into a context of its own; an edit that changes only the
//! contents of those files copies them into the context, imports them
//! again and has the scene written again. A worker that is not asked for a
//! while stops (`UME_SCENE_WORKER_IDLE_SECONDS`, two minutes by default),
//! since it holds more than a gigabyte.
use super::{
    ProbeOptions,
    build_settings::build_settings,
    context::BuildFiles,
    import::{BuildImport, prepare_build_run},
    player_objects::{
        Build, REQUEST_PATH, Request, WrittenScene, importer_identity, job_workers, open_sources,
        parse_report, scene_closure,
    },
    probe::{NativeImportJob, WarmWorker},
    support::{Event, read_file},
};
use crate::{cancel::Context, sha256, sysutil};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

const DEFAULT_IDLE: Duration = Duration::from_secs(120);

fn idle_limit() -> Duration {
    std::env::var("UME_SCENE_WORKER_IDLE_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(DEFAULT_IDLE, Duration::from_secs)
}

struct Warm {
    worker: WarmWorker,
    /// The project whose library the worker's imports go back to.
    project: String,
    /// The assets of the context, in import order, the request last.
    batch: Vec<String>,
    /// The hash of each of those sources and of its `.meta`.
    sources: BTreeMap<String, (String, String)>,
    environment: String,
    assemblies: String,
    code: Code,
    request: String,
    request_guid: String,
    revision: u64,
    last_used: Instant,
}

impl Drop for Warm {
    fn drop(&mut self) {
        self.worker.stop();
        let library = sysutil::join(&[self.worker.project(), "Library"]);
        if let Err(error) = keep_library(
            self.worker.engine().artifacts.library(),
            &self.project,
            &library,
            &self.environment,
            &self.assemblies,
            &self.code,
            &self.sources,
        ) {
            Event::new("scene-library-not-kept")
                .with("reason", &format!("{error:#}"))
                .emit();
        }
    }
}

/// The assemblies and the scripts registered with them, which an import of
/// a script's object depends on.
fn environment_key(environment: &super::scripted::ScriptEnvironment) -> String {
    sha256(format!("{}\0{}", environment.sha256, environment.scripts.join("\0")).as_bytes())
}

/// The assemblies loaded, by name: their asset postprocessors take part in
/// the import of a model, image or sound, which an edit of a script's body
/// leaves as it is.
fn assemblies_key(environment: &super::scripted::ScriptEnvironment) -> String {
    sha256(environment.assemblies.join("\0").as_bytes())
}

/// The sources whose imports a library holds, with the hashes of each and
/// of its `.meta` when it was imported.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryState {
    schema_version: u32,
    /// The assemblies and scripts the imports were made with.
    #[serde(default)]
    environment: String,
    /// The assemblies loaded when they were made.
    #[serde(default)]
    assemblies: String,
    sources: BTreeMap<String, (String, String)>,
    /// The code the imports were made with. A library kept before it was
    /// recorded takes an import that needs a script's class only with the
    /// same scripts and assemblies.
    #[serde(default)]
    code: Code,
}

/// The code an import that needs a script's class depends on: the hash of
/// each loaded assembly (by name, a precompiled one by its place) and the
/// assembly each script is compiled into.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Code {
    assembly_sha256: BTreeMap<String, String>,
    script_assemblies: BTreeMap<String, String>,
}

impl Code {
    fn of(environment: &super::scripted::ScriptEnvironment) -> Self {
        Self {
            assembly_sha256: environment.assembly_sha256.clone(),
            script_assemblies: environment.script_assemblies.clone(),
        }
    }

    /// Whether a class the dependency names comes from other code than in
    /// `before`: its script is compiled into another assembly, or that
    /// assembly changed.
    fn differs(&self, before: &Code, dependency: &Dependency) -> bool {
        let assembly = |code: &Code| -> Option<(String, Option<String>)> {
            let name = match dependency {
                Dependency::Script(path) => code.script_assemblies.get(path)?.clone(),
                Dependency::Assembly(name) => name.clone(),
            };
            let hash = code.assembly_sha256.get(&name).cloned();
            Some((name, hash))
        };
        assembly(self) != assembly(before)
    }
}

/// Where the class of an object of an asset is looked up.
#[derive(Debug, PartialEq, Eq)]
enum Dependency {
    /// A script or a precompiled assembly, by logical path.
    Script(String),
    /// An assembly a serialized reference names.
    Assembly(String),
}

/// The scripts and assemblies whose classes the import of an asset needs:
/// those its objects and serialized references name, and its scripted
/// importer.
fn code_dependencies(
    sources: &super::build_settings::Sources,
    data: &[u8],
    meta: &[u8],
) -> Vec<Dependency> {
    static GUID: std::sync::LazyLock<regex::bytes::Regex> =
        std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"guid: ([0-9a-fA-F]{32})").unwrap());
    static ASSEMBLY: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r"\basm: ([A-Za-z0-9_.\-]+)").unwrap()
    });
    let script = |guid: &str| {
        sources
            .path_of(guid)
            .filter(|path| matches!(super::support::lower_ext(path).as_str(), ".cs" | ".dll"))
            .map(|path| Dependency::Script(path.to_string()))
    };
    let mut found = Vec::new();
    if data.starts_with(b"%YAML") {
        for capture in GUID.captures_iter(data) {
            found.extend(script(
                &String::from_utf8_lossy(&capture[1]).to_ascii_lowercase(),
            ));
        }
        for capture in ASSEMBLY.captures_iter(data) {
            found.push(Dependency::Assembly(
                String::from_utf8_lossy(&capture[1]).into_owned(),
            ));
        }
    }
    if let Some(importer) = super::scripted::scripted_importer(meta) {
        found.extend(script(&importer));
    }
    found
}

/// The assets of a batch whose import is out of date: those whose source or
/// `.meta` differs from what the library holds, those `stale` names for
/// what else they depend on, and every text asset that refers to one of
/// them by GUID, transitively.
fn changed_assets(
    sources: &super::build_settings::Sources,
    batch: &[String],
    hashes: &BTreeMap<String, (String, String)>,
    kept: &BTreeMap<String, (String, String)>,
    stale: impl Fn(&str, &[u8], &[u8]) -> bool,
) -> std::collections::BTreeSet<String> {
    static GUID: std::sync::LazyLock<regex::bytes::Regex> =
        std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"guid: ([0-9a-fA-F]{32})").unwrap());
    let mut changed: std::collections::BTreeSet<String> = batch
        .iter()
        .filter(|path| kept.get(*path) != hashes.get(*path))
        .cloned()
        .collect();
    let mut references: BTreeMap<&String, Vec<String>> = BTreeMap::new();
    for path in batch {
        let Some(physical) = sources.physical(path) else {
            continue;
        };
        let Ok(data) = read_file(&physical) else {
            continue;
        };
        let meta = read_file(&format!("{physical}.meta")).unwrap_or_default();
        if !changed.contains(path) && stale(path, &data, &meta) {
            changed.insert(path.clone());
        }
        if !data.starts_with(b"%YAML") {
            continue;
        }
        let targets = GUID
            .captures_iter(&data)
            .filter_map(|capture| {
                let guid = String::from_utf8_lossy(&capture[1]).to_ascii_lowercase();
                sources.path_of(&guid).map(str::to_string)
            })
            .collect();
        references.insert(path, targets);
    }
    loop {
        let more: Vec<String> = references
            .iter()
            .filter(|(path, targets)| {
                !changed.contains(**path) && targets.iter().any(|target| changed.contains(target))
            })
            .map(|(path, _)| (*path).clone())
            .collect();
        if more.is_empty() {
            return changed;
        }
        changed.extend(more);
    }
}

fn library_directory(project: &str) -> String {
    sysutil::join(&[
        project,
        "Library/UnityMicroEditor/native-store/scene-library",
    ])
}

/// Moves the library of the project into a new context's `Library`; a
/// library another worker took, or never kept, gives nothing. `parts` are
/// what a worker of the release leaves in its context's `Library` that a
/// later worker starts from: the import results and the store of their
/// artifacts.
fn take_library(parts: &[&str], project: &str, context_library: &str) -> LibraryState {
    let directory = library_directory(project);
    let state_path = sysutil::join(&[&directory, "state.json"]);
    let Ok(state) = crate::util::read_json_file::<LibraryState>(&state_path) else {
        return LibraryState::default();
    };
    // Whoever takes the record takes the library.
    if state.schema_version != 1 || std::fs::remove_file(&state_path).is_err() {
        return LibraryState::default();
    }
    for part in parts {
        let from = sysutil::join(&[&directory, part]);
        let to = sysutil::join(&[context_library, part]);
        if std::fs::symlink_metadata(&from).is_err() {
            continue;
        }
        let _ = std::fs::remove_dir_all(&to).or_else(|_| std::fs::remove_file(&to));
        if std::fs::rename(&from, &to).is_err() {
            return LibraryState::default();
        }
    }
    state
}

/// Moves a stopped worker's imports back into the project's library.
fn keep_library(
    parts: &[&str],
    project: &str,
    context_library: &str,
    environment: &str,
    assemblies: &str,
    code: &Code,
    sources: &BTreeMap<String, (String, String)>,
) -> Result<()> {
    let directory = library_directory(project);
    super::support::mkdir_all(&directory, 0o700)?;
    let state_path = sysutil::join(&[&directory, "state.json"]);
    let _ = std::fs::remove_file(&state_path);
    for part in parts {
        let from = sysutil::join(&[context_library, part]);
        let to = sysutil::join(&[&directory, part]);
        if std::fs::symlink_metadata(&from).is_err() {
            continue;
        }
        let _ = std::fs::remove_dir_all(&to).or_else(|_| std::fs::remove_file(&to));
        std::fs::rename(&from, &to).map_err(|error| anyhow!("rename {from}: {error}"))?;
    }
    crate::util::atomic_json(
        &state_path,
        &LibraryState {
            schema_version: 1,
            environment: environment.to_string(),
            assemblies: assemblies.to_string(),
            sources: sources.clone(),
            code: code.clone(),
        },
    )
}

/// Whether a source's import depends on nothing a script or another asset
/// changes, so that an import of the same source and `.meta` is taken as
/// it is while the same assemblies are loaded: models, images, sounds,
/// videos and fonts. Any other import is taken when the classes it needs
/// come from the same code (`Code::differs`) and nothing it refers to
/// changed.
fn kept_as_imported(path: &str) -> bool {
    kept_kind(path)
}

/// Whether a kept import of the source is never taken by another worker: an
/// image imported with sprites. Its import depends on the sprite
/// postprocessors (`postprocessor/textureSprite`), and a later worker takes
/// it as out of date without saying so, even with the same scripts and
/// assemblies; the image is then missing from what that worker writes.
fn imported_each_time(path: &str, meta: &[u8]) -> bool {
    image(path) && sprite_texture(meta)
}

fn image(path: &str) -> bool {
    matches!(
        super::support::lower_ext(path).as_str(),
        ".png"
            | ".jpg"
            | ".jpeg"
            | ".tga"
            | ".psd"
            | ".tif"
            | ".tiff"
            | ".exr"
            | ".hdr"
            | ".bmp"
            | ".gif"
    )
}

/// Whether a texture's `.meta` imports it as a sprite or a set of sprites.
fn sprite_texture(meta: &[u8]) -> bool {
    String::from_utf8_lossy(meta).lines().any(|line| {
        matches!(
            line.trim_end(),
            "  textureType: 8" | "  spriteMode: 1" | "  spriteMode: 2"
        )
    })
}

fn kept_kind(path: &str) -> bool {
    matches!(
        super::support::lower_ext(path).as_str(),
        ".fbx"
            | ".obj"
            | ".blend"
            | ".dae"
            | ".3ds"
            | ".max"
            | ".ma"
            | ".mb"
            | ".png"
            | ".jpg"
            | ".jpeg"
            | ".tga"
            | ".psd"
            | ".tif"
            | ".tiff"
            | ".exr"
            | ".hdr"
            | ".bmp"
            | ".gif"
            | ".wav"
            | ".mp3"
            | ".ogg"
            | ".aif"
            | ".aiff"
            | ".flac"
            | ".mp4"
            | ".webm"
            | ".mov"
            | ".ttf"
            | ".otf"
    )
}

#[derive(Default)]
struct Shared {
    warm: Mutex<(Option<Warm>, bool)>,
    wake: Condvar,
}

/// The scene worker of a project, which every session of the process that
/// writes a scene of it shares: one worker at a time holds the project's
/// library, and the scenes of a session (its own, its content scenes) are
/// written one after the other.
pub(crate) fn for_project(project: &str) -> Arc<SceneWorker> {
    static WORKERS: std::sync::LazyLock<Mutex<BTreeMap<String, std::sync::Weak<SceneWorker>>>> =
        std::sync::LazyLock::new(Default::default);
    let mut workers = WORKERS.lock().unwrap_or_else(|error| error.into_inner());
    workers.retain(|_, worker| worker.strong_count() > 0);
    if let Some(worker) = workers.get(project).and_then(std::sync::Weak::upgrade) {
        return worker;
    }
    let worker = Arc::new(SceneWorker::default());
    workers.insert(project.to_string(), Arc::downgrade(&worker));
    worker
}

/// Writes scenes with a worker that lives on between them.
pub(crate) struct SceneWorker {
    shared: Arc<Shared>,
    reaper: Option<JoinHandle<()>>,
}

impl Default for SceneWorker {
    fn default() -> Self {
        let shared = Arc::new(Shared::default());
        let idle = idle_limit();
        let reaped = shared.clone();
        let reaper = std::thread::spawn(move || {
            let mut state = reaped.warm.lock().unwrap();
            loop {
                if state.1 {
                    return;
                }
                let wait = match &state.0 {
                    Some(warm) => {
                        let since = warm.last_used.elapsed();
                        if since >= idle {
                            if let Some(mut warm) = state.0.take() {
                                warm.worker.stop();
                                Event::new("scene-worker-stopped")
                                    .with("idleSeconds", &idle.as_secs())
                                    .emit();
                            }
                            continue;
                        }
                        idle - since
                    }
                    None => Duration::from_secs(3600),
                };
                state = reaped.wake.wait_timeout(state, wait).unwrap().0;
            }
        });
        Self {
            shared,
            reaper: Some(reaper),
        }
    }
}

impl Drop for SceneWorker {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.warm.lock() {
            state.1 = true;
            if let Some(mut warm) = state.0.take() {
                warm.worker.stop();
            }
        }
        self.shared.wake.notify_all();
        if let Some(reaper) = self.reaper.take() {
            let _ = reaper.join();
        }
    }
}

impl SceneWorker {
    /// Writes `build.target` as a build writes it, with what it refers to
    /// in a file beside it.
    pub(crate) fn write(&self, ctx: &Context, build: &Build) -> Result<WrittenScene> {
        let started = Instant::now();
        let sources = open_sources(&build.project)?;
        let settings = build_settings(&sources, &[])?;
        let mut batch: Vec<String> = Vec::new();
        let closure = scene_closure(&sources, &build.target, &build.users)?;
        for path in settings.batch.iter().chain(&closure.assets).cloned() {
            if !batch.contains(&path) {
                batch.push(path);
            }
        }
        let mut hashes = BTreeMap::new();
        for path in &batch {
            let Some(physical) = sources.physical(path) else {
                bail!("{path} lies in a package outside the project");
            };
            hashes.insert(
                path.clone(),
                (
                    crate::util::file_hash(&physical)?,
                    sha256(&read_file(&format!("{physical}.meta"))?),
                ),
            );
        }
        let environment = closure.environment(&build.project)?;
        let request = Request {
            scenes_with_dependencies: vec![build.target.clone()],
            includes: build.users.clone(),
            ..Default::default()
        }
        .encode()?;
        let request_guid =
            sha256(format!("{}\0{request}", importer_identity()).as_bytes())[..32].to_string();
        let mut state = self.shared.warm.lock().unwrap();
        let reusable = state.0.as_ref().is_some_and(|warm| {
            warm.worker.alive()
                && warm.batch[..warm.batch.len() - 1] == batch[..]
                && warm.environment == environment_key(&environment)
                && warm.request == request
                && batch.iter().all(|path| {
                    let (now, before) = (&hashes[path], &warm.sources[path]);
                    // A changed `.meta` changes how the source is imported,
                    // and only the project's own files are copied again.
                    now.1 == before.1 && (now.0 == before.0 || path.starts_with("Assets/"))
                })
        });
        let mut reused = false;
        if reusable {
            let warm = state.0.as_mut().unwrap();
            match Self::again(ctx, warm, &sources, &hashes) {
                Ok(()) => reused = true,
                Err(error) => {
                    Event::new("scene-worker-restarted")
                        .with("reason", &format!("{error:#}"))
                        .emit();
                    state.0 = None;
                }
            }
        } else {
            // Stopped, its imports go back to the library first.
            state.0 = None;
        }
        if !reused {
            state.0 = Some(Self::start(
                ctx,
                build,
                &batch,
                &hashes,
                &sources,
                &environment,
                &request,
                &request_guid,
            )?);
        }
        let warm = state.0.as_mut().unwrap();
        warm.last_used = Instant::now();
        // The variants the worker compiled serve the next worker too.
        if !build.shader_cache.is_empty() {
            let _ = super::import::copy_missing(
                &sysutil::join(&[warm.worker.project(), "Library/ShaderCache"]),
                &build.shader_cache,
            );
        }
        let written = Self::read(warm)?;
        self.shared.wake.notify_all();
        Event::new("scene-worker-wrote")
            .with("scene", &build.target)
            .with("reused", &reused)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(WrittenScene {
            seconds: started.elapsed().as_secs_f64(),
            ..written
        })
    }

    /// Starts a worker over a new context and imports everything once.
    #[allow(clippy::too_many_arguments)]
    fn start(
        ctx: &Context,
        build: &Build,
        batch: &[String],
        hashes: &BTreeMap<String, (String, String)>,
        sources: &super::build_settings::Sources,
        environment: &super::scripted::ScriptEnvironment,
        request: &str,
        request_guid: &str,
    ) -> Result<Warm> {
        let output = sysutil::join(&[&build.output, "warm"]);
        let files = BuildFiles {
            generated: BTreeMap::from([(
                REQUEST_PATH.to_string(),
                (
                    format!("{request}revision 0\n").into_bytes(),
                    format!("fileFormatVersion: 2\nguid: {request_guid}\n").into_bytes(),
                ),
            )]),
            // A graph is imported from the templates of its targets, which
            // its importer reads as plain files.
            files: if batch
                .iter()
                .any(|path| path.ends_with(".shadergraph") || path.ends_with(".shadersubgraph"))
            {
                open_sources(&build.project)?.graph_templates()
            } else {
                Vec::new()
            },
        };
        let mut assets = batch.to_vec();
        assets.push(REQUEST_PATH.to_string());
        let (opts, _) = prepare_build_run(
            ctx,
            ProbeOptions {
                project: build.project.clone(),
                editor: build.editor.clone(),
                output: output.clone(),
                timeout: build.timeout,
                job_workers: job_workers(),
                ..Default::default()
            },
            &assets,
            &BuildImport {
                environment,
                files: &files,
                shader_cache: &build.shader_cache,
                extras: None,
            },
        )?;
        // The imports a worker before it kept: a source imported from the
        // same file and `.meta` is not imported again.
        let library = sysutil::join(&[&opts.context, "Library"]);
        let parts = crate::engine::of_project(&opts.context)?
            .artifacts
            .library();
        let kept = take_library(parts, &build.project, &library);
        let environment_key = environment_key(environment);
        let assemblies = assemblies_key(environment);
        let code = Code::of(environment);
        // With the code of its imports recorded, an import is out of date
        // when a class it needs comes from changed code; without, any import
        // but a model's, image's or sound's is when any script changed.
        let recorded = !kept.code.assembly_sha256.is_empty();
        let changed = changed_assets(sources, batch, hashes, &kept.sources, |path, data, meta| {
            if recorded {
                code_dependencies(sources, data, meta)
                    .iter()
                    .any(|dependency| code.differs(&kept.code, dependency))
            } else {
                !kept_as_imported(path) && kept.environment != environment_key
            }
        });
        let paths: BTreeMap<&str, &str> = opts
            .project_context
            .iter()
            .flat_map(|context| context.assets())
            .map(|asset| (asset.guid.as_str(), asset.path.as_str()))
            .collect();
        let jobs: Vec<_> = opts
            .jobs
            .iter()
            .filter(|job| {
                // The request, and anything the batch does not hash, is
                // always imported.
                let Some(path) = paths
                    .get(job.guid.as_str())
                    .filter(|path| hashes.contains_key(**path))
                else {
                    return true;
                };
                let meta = sources
                    .physical(path)
                    .and_then(|physical| read_file(&format!("{physical}.meta")).ok())
                    .unwrap_or_default();
                imported_each_time(path, &meta)
                    || changed.contains(*path)
                    || kept.assemblies != assemblies
            })
            .cloned()
            .collect();
        Event::new("scene-library")
            .with("kept", &(opts.jobs.len() - jobs.len()))
            .with("imported", &jobs.len())
            .emit();
        // Until it is stopped, what the library held is the worker's.
        let mut sources = kept.sources;
        let mut worker = match WarmWorker::start(&opts) {
            Ok(worker) => worker,
            Err(error) => {
                let _ = keep_library(
                    parts,
                    &build.project,
                    &library,
                    &kept.environment,
                    &kept.assemblies,
                    &kept.code,
                    &sources,
                );
                return Err(error);
            }
        };
        if let Err(error) = worker.import(&ctx.with_timeout(build.timeout), &jobs) {
            // What the library held stays for the next worker.
            worker.stop();
            let _ = keep_library(
                parts,
                &build.project,
                &library,
                &kept.environment,
                &kept.assemblies,
                &kept.code,
                &sources,
            );
            return Err(error);
        }
        sources.extend(
            hashes
                .iter()
                .map(|(path, hash)| (path.clone(), hash.clone())),
        );
        Ok(Warm {
            worker,
            project: build.project.clone(),
            batch: assets,
            sources,
            environment: environment_key,
            assemblies,
            code,
            request: request.to_string(),
            request_guid: request_guid.to_string(),
            revision: 0,
            last_used: Instant::now(),
        })
    }

    /// Copies the changed sources into the worker's context, imports them
    /// again and has the scene written again.
    fn again(
        ctx: &Context,
        warm: &mut Warm,
        sources: &super::build_settings::Sources,
        hashes: &BTreeMap<String, (String, String)>,
    ) -> Result<()> {
        let project = warm.worker.project().to_string();
        let mut jobs = Vec::new();
        for path in &warm.batch[..warm.batch.len() - 1] {
            if hashes[path].0 == warm.sources[path].0 {
                continue;
            }
            let physical = sources
                .physical(path)
                .ok_or_else(|| anyhow!("{path} has no source"))?;
            let data = read_file(&physical)?;
            if sha256(&data) != hashes[path].0 {
                bail!("{path} changed while it was read");
            }
            replace(&sysutil::join(&[&project, path]), &data)?;
            warm.worker
                .engine()
                .asset_database
                .update_source(&project, path)?;
            let guid = sources
                .guid_of(path)
                .ok_or_else(|| anyhow!("{path} has no GUID"))?;
            jobs.push(NativeImportJob {
                guid: guid.to_string(),
                importer: -1,
                dependent: false,
            });
        }
        warm.revision += 1;
        let directory = output_directory(&project, &warm.request_guid);
        for name in ["scene", "deps", "deps.resS", "deps.resource", "report.txt"] {
            let _ = std::fs::remove_file(sysutil::join(&[&directory, name]));
        }
        replace(
            &sysutil::join(&[&project, REQUEST_PATH]),
            format!("{}revision {}\n", warm.request, warm.revision).as_bytes(),
        )?;
        warm.worker
            .engine()
            .asset_database
            .update_source(&project, REQUEST_PATH)?;
        jobs.push(NativeImportJob {
            guid: warm.request_guid.clone(),
            importer: -1,
            dependent: false,
        });
        warm.worker
            .import(&ctx.with_timeout(Duration::from_secs(1800)), &jobs)?;
        for (path, hash) in hashes {
            warm.sources.insert(path.clone(), hash.clone());
        }
        Ok(())
    }

    fn read(warm: &Warm) -> Result<WrittenScene> {
        let directory = output_directory(warm.worker.project(), &warm.request_guid);
        let file = |name: &str| read_file(&sysutil::join(&[&directory, name]));
        let report = parse_report(&String::from_utf8_lossy(&file("report.txt")?))?;
        let mut resources = Vec::new();
        for extension in ["resS", "resource"] {
            let path = sysutil::join(&[&directory, &format!("deps.{extension}")]);
            if std::fs::metadata(&path).is_ok() {
                resources.push((format!("ume-deps.{extension}"), read_file(&path)?));
            }
        }
        Ok(WrittenScene {
            scene: file("scene")?,
            dependencies: Some(file("deps")?),
            resources,
            report,
            imports: warm.batch.len(),
            seconds: 0.0,
        })
    }
}

/// Where the worker's importer leaves what it wrote for a request.
fn output_directory(project: &str, request_guid: &str) -> String {
    sysutil::join(&[project, "Temp", &format!("ume-player-{request_guid}")])
}

/// Writes a file of the context whole, in place of the one before.
fn replace(path: &str, data: &[u8]) -> Result<()> {
    let partial = format!("{path}.{}.tmp", crate::util::random_id());
    super::support::write_file(&partial, data, 0o600)?;
    std::fs::rename(&partial, path).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        anyhow!("rename {partial}: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "0123456789abcdef0123456789abcdef";
    const PLUGIN: &str = "fedcba9876543210fedcba9876543210";
    const IMPORTER: &str = "00112233445566778899aabbccddeeff";

    fn project() -> (tempfile::TempDir, super::super::build_settings::Sources) {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().to_str().unwrap().to_string();
        for (path, guid) in [
            ("Assets/Game/Enemy.cs", SCRIPT),
            ("Assets/Plugins/Tool.dll", PLUGIN),
            ("Assets/Editor/Import.cs", IMPORTER),
        ] {
            let file = sysutil::join(&[&root, path]);
            std::fs::create_dir_all(sysutil::dir(&file)).unwrap();
            std::fs::write(&file, "").unwrap();
            std::fs::write(
                format!("{file}.meta"),
                format!("fileFormatVersion: 2\nguid: {guid}\n"),
            )
            .unwrap();
        }
        let sources =
            super::super::build_settings::Sources::open(&root, &root, &BTreeMap::new()).unwrap();
        (directory, sources)
    }

    fn code(game: &str, scripts: &[(&str, &str)]) -> Code {
        Code {
            assembly_sha256: BTreeMap::from([
                ("Game".to_string(), game.to_string()),
                ("Tools".to_string(), "t".to_string()),
                ("Assets/Plugins/Tool.dll".to_string(), "p".to_string()),
            ]),
            script_assemblies: scripts
                .iter()
                .map(|(path, assembly)| (path.to_string(), assembly.to_string()))
                .collect(),
        }
    }

    #[test]
    fn an_import_needs_the_classes_its_objects_references_and_importer_name() {
        let (_directory, sources) = project();
        let prefab = format!(
            "%YAML 1.1\n--- !u!114 &1\nMonoBehaviour:\n  m_Script: {{fileID: 11500000, guid: {SCRIPT}, type: 3}}\n  m_Tool: {{fileID: 7, guid: {PLUGIN}, type: 3}}\n  references:\n    - type: {{class: Loot, ns: Game, asm: Game.Data}}\n"
        );
        let meta = format!(
            "fileFormatVersion: 2\nScriptedImporter:\n  script: {{fileID: 11500000, guid: {IMPORTER}, type: 3}}\n"
        );
        assert_eq!(
            code_dependencies(&sources, prefab.as_bytes(), meta.as_bytes()),
            [
                Dependency::Script("Assets/Game/Enemy.cs".into()),
                Dependency::Script("Assets/Plugins/Tool.dll".into()),
                Dependency::Assembly("Game.Data".into()),
                Dependency::Script("Assets/Editor/Import.cs".into()),
            ]
        );
        // A material names a shader, no class.
        let material = "%YAML 1.1\n--- !u!21 &2100000\nMaterial:\n  m_Shader: {fileID: 4800000, guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, type: 3}\n";
        assert!(code_dependencies(&sources, material.as_bytes(), b"").is_empty());
    }

    #[test]
    fn an_import_is_out_of_date_only_when_code_it_needs_changed() {
        let scripts = [
            ("Assets/Game/Enemy.cs", "Game"),
            ("Assets/Editor/Import.cs", "Tools"),
        ];
        let before = code("g1", &scripts);
        let enemy = Dependency::Script("Assets/Game/Enemy.cs".into());
        let importer = Dependency::Script("Assets/Editor/Import.cs".into());
        let plugin = Dependency::Script("Assets/Plugins/Tool.dll".into());
        // Same code.
        assert!(!code("g1", &scripts).differs(&before, &enemy));
        // Game changed: its scripts' imports are out of date, the others not.
        let after = code("g2", &scripts);
        assert!(after.differs(&before, &enemy));
        assert!(after.differs(&before, &Dependency::Assembly("Game".into())));
        assert!(!after.differs(&before, &importer));
        assert!(!after.differs(&before, &plugin));
        // A script that moved to another assembly, or one that was not
        // compiled before, is out of date as well.
        let moved = code("g1", &[("Assets/Game/Enemy.cs", "Tools"), scripts[1]]);
        assert!(moved.differs(&before, &enemy));
        let new = Dependency::Script("Assets/Game/Boss.cs".into());
        let added = code(
            "g1",
            &[scripts[0], scripts[1], ("Assets/Game/Boss.cs", "Game")],
        );
        assert!(added.differs(&before, &new));
    }

    #[test]
    fn a_texture_with_sprites_is_imported_by_every_worker() {
        let texture = b"TextureImporter:\n  textureType: 0\n  spriteMode: 0\n";
        let sprite = b"TextureImporter:\n  textureType: 8\n  spriteMode: 1\n";
        let sheet = b"TextureImporter:\n  textureType: 0\n  spriteMode: 2\n";
        assert!(!imported_each_time("Assets/Art/Wall.png", texture));
        assert!(imported_each_time("Assets/Art/Icon.png", sprite));
        assert!(imported_each_time("Assets/Art/Sheet.PNG", sheet));
        // Only images are told apart by their sprites.
        assert!(!imported_each_time("Assets/Audio/Hit.wav", sprite));
    }
}
