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
    /// The assets of the context, in import order, the request last.
    batch: Vec<String>,
    /// The hash of each of those sources and of its `.meta`.
    sources: BTreeMap<String, (String, String)>,
    environment: String,
    request: String,
    request_guid: String,
    revision: u64,
    last_used: Instant,
}

#[derive(Default)]
struct Shared {
    warm: Mutex<(Option<Warm>, bool)>,
    wake: Condvar,
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
        for path in settings
            .batch
            .iter()
            .cloned()
            .chain(scene_closure(&sources, &build.target)?)
        {
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
        let environment = super::scripted::scene_environment(&build.project)?;
        let request = Request {
            scenes_with_dependencies: vec![build.target.clone()],
            ..Default::default()
        }
        .encode()?;
        let request_guid =
            sha256(format!("{}\0{request}", importer_identity()).as_bytes())[..32].to_string();
        let mut state = self.shared.warm.lock().unwrap();
        let reusable = state.0.as_ref().is_some_and(|warm| {
            warm.worker.alive()
                && warm.batch[..warm.batch.len() - 1] == batch[..]
                && warm.environment == environment.sha256
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
        } else if let Some(mut warm) = state.0.take() {
            warm.worker.stop();
        }
        if !reused {
            let warm = Self::start(ctx, build, &batch, &environment, &request, &request_guid)?;
            state.0 = Some(Warm {
                sources: hashes,
                ..warm
            });
        }
        let warm = state.0.as_mut().unwrap();
        warm.last_used = Instant::now();
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
    fn start(
        ctx: &Context,
        build: &Build,
        batch: &[String],
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
            files: Vec::new(),
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
                shader_cache: "",
                extras: None,
            },
        )?;
        let mut worker = WarmWorker::start(&opts)?;
        worker.import(&ctx.with_timeout(build.timeout), &opts.jobs)?;
        Ok(Warm {
            worker,
            batch: assets,
            sources: BTreeMap::new(),
            environment: environment.sha256.clone(),
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
            crate::registry::update_source(&project, path)?;
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
        for name in ["scene", "deps", "report.txt"] {
            let _ = std::fs::remove_file(sysutil::join(&[&directory, name]));
        }
        replace(
            &sysutil::join(&[&project, REQUEST_PATH]),
            format!("{}revision {}\n", warm.request, warm.revision).as_bytes(),
        )?;
        crate::registry::update_source(&project, REQUEST_PATH)?;
        jobs.push(NativeImportJob {
            guid: warm.request_guid.clone(),
            importer: -1,
            dependent: false,
        });
        warm.worker
            .import(&ctx.with_timeout(Duration::from_secs(600)), &jobs)?;
        for (path, hash) in hashes {
            warm.sources.insert(path.clone(), hash.clone());
        }
        Ok(())
    }

    fn read(warm: &Warm) -> Result<WrittenScene> {
        let directory = output_directory(warm.worker.project(), &warm.request_guid);
        let file = |name: &str| read_file(&sysutil::join(&[&directory, name]));
        let report = parse_report(&String::from_utf8_lossy(&file("report.txt")?))?;
        Ok(WrittenScene {
            scene: file("scene")?,
            dependencies: Some(file("deps")?),
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
