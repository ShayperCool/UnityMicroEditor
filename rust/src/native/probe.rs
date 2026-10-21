//! The coordinator side of one standalone AssetImportWorker run.
use super::{
    DataStoreService, DataStoreStatus, JsonF64, ProjectImportContent, ProjectImportContext,
    WorkerEnvironment,
    datastore::Waiter,
    environment::read_worker_environment,
    support::{
        abs, context_error, create_new, is_not_exist, join, mkdir_all, nul, os_error, read_limited,
        text, write_file,
    },
};
use crate::{
    cancel::{self, Canceled, Context, Flag, Source},
    engine::{self, Engine, Message, Reply, WorkerLaunch, WorkerProtocol},
    protocol::{ArtifactMetadata, Frame, ImportRecord, read_frame, write_frame},
    registry, sha256, sysutil,
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProbeReport {
    #[serde(deserialize_with = "nul")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nul")]
    pub experiment: String,
    #[serde(deserialize_with = "nul")]
    pub success: bool,
    #[serde(deserialize_with = "nul")]
    pub stage: String,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub project_context: Option<ProjectImportContext>,
    #[serde(deserialize_with = "nul")]
    pub main_editor_started: bool,
    #[serde(deserialize_with = "nul", rename = "sourceDBSeeded")]
    pub source_db_seeded: bool,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub source_registry: Option<registry::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<WorkerEnvironment>,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub content: Option<ProjectImportContent>,
    #[serde(deserialize_with = "nul")]
    pub worker_pid: i64,
    #[serde(deserialize_with = "nul")]
    pub coordinator_pid: i64,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub worker_name: String,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub data_store: Option<DataStoreStatus>,
    #[serde(deserialize_with = "nul", rename = "stageSeconds")]
    pub stages: BTreeMap<String, JsonF64>,
    #[serde(deserialize_with = "nul")]
    pub frames: Vec<FrameEvidence>,
    #[serde(deserialize_with = "nul")]
    pub imports: Vec<ImportEvidence>,
    #[serde(deserialize_with = "nul", rename = "coordinatorRssKiB")]
    pub coordinator_rss_kib: i64,
    #[serde(deserialize_with = "nul", rename = "workerRssKiB")]
    pub worker_rss_kib: i64,
    #[serde(deserialize_with = "nul", rename = "dataStoreRssKiB")]
    pub data_store_rss_kib: i64,
    #[serde(deserialize_with = "nul", rename = "workerPeakRssKiB")]
    pub worker_peak_rss_kib: i64,
    pub native_seconds: JsonF64,
    pub elapsed_seconds: JsonF64,
    #[serde(deserialize_with = "nul")]
    pub worker_exit: String,
    #[serde(skip)]
    pub(crate) max_frames: usize,
    #[serde(skip)]
    pub(crate) max_frame_bytes: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameEvidence {
    #[serde(deserialize_with = "nul", rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "nul")]
    pub size: usize,
    #[serde(deserialize_with = "nul")]
    pub sha256: String,
    #[serde(deserialize_with = "nul")]
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImportEvidence {
    #[serde(deserialize_with = "nul")]
    pub guid: String,
    #[serde(deserialize_with = "nul")]
    pub importer_type: i32,
    pub elapsed_seconds: JsonF64,
    #[serde(deserialize_with = "nul")]
    pub reply_type: u8,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub result_id: String,
    #[serde(deserialize_with = "nul")]
    pub error_code: u8,
    #[serde(deserialize_with = "nul", skip_serializing_if = "String::is_empty")]
    pub error_message: String,
    /// `None` is the absent list of a response without paths (`null`).
    pub returned_paths: Option<Vec<String>>,
    #[serde(deserialize_with = "nul")]
    pub metadata: ArtifactMetadata,
}

#[derive(Debug, Default)]
pub struct ProbeOptions {
    pub editor: String,
    pub project: String,
    pub output: String,
    pub timeout: Duration,
    pub context: String,
    pub registry: Option<registry::Report>,
    pub jobs: Vec<NativeImportJob>,
    pub project_context: Option<ProjectImportContext>,
    /// Records every accepted import in the context's ArtifactDB, where the
    /// worker looks up what a later import loads.
    pub artifact_database: bool,
    /// The worker's job threads, which also bound how many shader compilers
    /// it runs; two when zero.
    pub job_workers: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeImportJob {
    pub guid: String,
    pub importer: i32,
    /// The import reads artifacts of other jobs of the run (a sprite atlas
    /// packs the sprites of its textures). It is given to a second worker,
    /// started once the results of the first are in the context's ArtifactDB.
    pub dependent: bool,
}

/// The worker connection with socket error texts such as "i/o timeout". The context
/// deadline is the socket deadline of every call, so a late worker still
/// receives the protocol shutdown; cancellation shuts the socket down, which
/// releases blocked calls.
struct Connection {
    stream: TcpStream,
    ctx: Context,
    route: String,
    /// Write deadline of the closing shutdown message.
    final_write: Option<Duration>,
}
const CLOSED: &str = "use of closed network connection";
const TIMEOUT: &str = "i/o timeout";
impl Connection {
    fn error(&self, operation: &str, reason: &str) -> io::Error {
        io::Error::other(format!("{operation} tcp {}: {reason}", self.route))
    }
    /// The time left for one call, or the error of an expired deadline.
    fn budget(&self, operation: &str) -> io::Result<Option<Duration>> {
        match self.ctx.remaining() {
            Some(left) if left.is_zero() => Err(self.error(operation, TIMEOUT)),
            left => Ok(left),
        }
    }
    fn failure(&self, operation: &str, error: &io::Error) -> io::Error {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            // The context reports its deadline no later than the socket does.
            if let Some(left) = self.ctx.remaining().filter(|_| self.final_write.is_none()) {
                std::thread::sleep(left);
            }
            return self.error(operation, TIMEOUT);
        }
        if self.ctx.error() == Some(Canceled::Canceled) {
            return self.error(operation, CLOSED);
        }
        self.error(
            operation,
            &format!("{operation}: {}", sysutil::errno_text(error)),
        )
    }
}
impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let budget = self.budget("read")?;
        let _ = self.stream.set_read_timeout(budget);
        match self.stream.read(buffer) {
            Ok(0) if !buffer.is_empty() && self.ctx.error() == Some(Canceled::Canceled) => {
                Err(self.error("read", CLOSED))
            }
            Ok(count) => Ok(count),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(error),
            Err(error) => Err(self.failure("read", &error)),
        }
    }
}
impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let budget = match self.final_write {
            Some(limit) => Some(limit),
            None => self.budget("write")?,
        };
        let _ = self.stream.set_write_timeout(budget);
        match self.stream.write(buffer) {
            Err(error) if error.kind() != io::ErrorKind::Interrupted => {
                Err(self.failure("write", &error))
            }
            other => other,
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Closes the listener and the connection when the context ends. A
/// passed deadline only releases Accept; the connection keeps its deadline.
#[derive(Default)]
struct Sockets {
    canceled: bool,
    stream: Option<TcpStream>,
}
struct Watcher {
    stop: Flag,
    sockets: Arc<Mutex<Sockets>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watcher {
    fn new(ctx: &Context, listener: &Arc<TcpListener>) -> Self {
        let (stop, sockets) = (Flag::new(), Arc::new(Mutex::new(Sockets::default())));
        let (ctx, listener, flag, shared) =
            (ctx.clone(), listener.clone(), stop.clone(), sockets.clone());
        let thread = std::thread::spawn(move || {
            let fired = cancel::select(&[Source::Flag(&flag), Source::Context(&ctx)], None);
            if fired == Some(1) {
                let mut sockets = shared.lock().unwrap();
                // SAFETY: the descriptor stays open while `listener` is alive.
                unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
                if ctx.error() == Some(Canceled::Canceled) {
                    sockets.canceled = true;
                    if let Some(stream) = &sockets.stream {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                }
            }
        });
        Self {
            stop,
            sockets,
            thread: Some(thread),
        }
    }
    fn attach(&self, stream: &TcpStream) -> io::Result<()> {
        let clone = stream.try_clone()?;
        let mut sockets = self.sockets.lock().unwrap();
        if sockets.canceled {
            let _ = clone.shutdown(Shutdown::Both);
        }
        sockets.stream = Some(clone);
        Ok(())
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What every worker of one run shares.
struct Pass<'a> {
    engine: &'static Engine,
    listener: &'a Arc<TcpListener>,
    service: &'a DataStoreService,
    initialize: &'a [u8],
    prepare: &'a [u8],
}

struct Probe<'a> {
    ctx: &'a Context,
    opts: &'a ProbeOptions,
    project: &'a str,
    output: String,
    logs: String,
    started: Instant,
}

/// Runs the DataStore service and one import worker over the prepared context.
/// A report is returned together with the error once the run got far enough to
/// leave evidence behind.
pub fn run_probe(
    ctx: &Context,
    mut opts: ProbeOptions,
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    let started = Instant::now();
    if opts.context.is_empty() {
        return (
            None,
            Some(anyhow!("native import requires a prepared project context")),
        );
    }
    let output = match abs(&opts.output) {
        Ok(output) => output,
        Err(error) => return (None, Some(error)),
    };
    let logs = sysutil::join(&[&output, "logs"]);
    if let Err(error) = mkdir_all(&logs, 0o700) {
        return (None, Some(error));
    }
    let mut report = ProbeReport {
        schema_version: 1,
        experiment: "native-import-worker-own-coordinator".into(),
        stage: "project-sources".into(),
        coordinator_pid: i64::from(std::process::id()),
        project_context: opts.project_context.take(),
        source_registry: opts.registry.take(),
        max_frames: 65536.min(256 + 8 * opts.jobs.len()),
        max_frame_bytes: (128 * 1024 * 1024).min(16 * 1024 * 1024 + 64 * 1024 * opts.jobs.len()),
        ..Default::default()
    };
    let project = opts.context.clone();
    let probe = Probe {
        ctx,
        opts: &opts,
        project: &project,
        output,
        logs,
        started,
    };
    let error = probe.with_service(&mut report).err();
    report.native_seconds = JsonF64(started.elapsed().as_secs_f64());
    report.coordinator_rss_kib = rss_kib(i64::from(std::process::id()));
    let error = join([error, context_error(ctx)]);
    if let Some(error) = &error {
        report.error = text(error);
        report.success = false;
    }
    (Some(report), error)
}

impl Probe<'_> {
    fn seconds(since: Instant) -> JsonF64 {
        JsonF64(since.elapsed().as_secs_f64())
    }

    fn with_service(&self, report: &mut ProbeReport) -> Result<()> {
        let engine = engine::for_worker(self.project, &self.opts.editor)?;
        // A project with PlayerSettings supplies exact graphics values.
        match read_worker_environment(self.project) {
            Ok(environment) => report.environment = Some(environment),
            Err(error) if is_not_exist(&error) => {}
            Err(error) => return Err(error),
        }
        let environment = report.environment.as_ref();
        let (initialize, prepare) = (
            engine.worker.initialize(environment),
            engine.worker.prepare(environment),
        );
        report
            .stages
            .insert("fixtureAndRegistry".into(), Self::seconds(self.started));
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| anyhow!("listen tcp 127.0.0.1:0: {}", sysutil::errno_text(&e)))?;
        let listener = Arc::new(listener);
        let mut service =
            DataStoreService::new(engine, &self.opts.editor, self.project, &self.logs);
        let stage = Instant::now();
        service.start(self.ctx)?;
        report
            .stages
            .insert("dataStoreReady".into(), Self::seconds(stage));
        let run = Pass {
            engine,
            listener: &listener,
            service: &service,
            initialize: &initialize,
            prepare: &prepare,
        };
        let result = self.with_workers(report, &run);
        report.data_store_rss_kib = rss_kib(i64::from(service.pid));
        let result = join([result.err(), service.stop().err()]);
        if let Ok(status) = service.status() {
            report.data_store = Some(status);
        }
        result.map_or(Ok(()), Err)
    }

    /// One worker for the jobs that stand alone, then one for those that read
    /// their artifacts.
    fn with_workers(&self, report: &mut ProbeReport, run: &Pass) -> Result<()> {
        if self.opts.jobs.is_empty() {
            bail!("native import context has no source asset jobs");
        }
        let (dependent, first): (Vec<_>, Vec<_>) =
            self.opts.jobs.iter().partition(|job| job.dependent);
        let mut records = Vec::new();
        if dependent.is_empty() {
            return self.with_worker(report, run, "worker", &first, &mut records);
        }
        if first.is_empty() {
            bail!("a dependent native import has no job to depend on");
        }
        self.with_worker(report, run, "worker", &first, &mut records)?;
        run.engine
            .asset_database
            .commit_imports(self.project, &records)?;
        self.with_worker(report, run, "worker-dependent", &dependent, &mut records)
    }

    fn with_worker(
        &self,
        report: &mut ProbeReport,
        run: &Pass,
        name: &str,
        jobs: &[&NativeImportJob],
        records: &mut Vec<ImportRecord>,
    ) -> Result<()> {
        let Pass {
            engine,
            listener,
            service,
            ..
        } = *run;
        let log = sysutil::join(&[&self.logs, &format!("{name}.log")]);
        let stdout = create_new(
            &sysutil::join(&[&self.logs, &format!("{name}-stdout.log")]),
            0o600,
        )?;
        let port = listener
            .local_addr()
            .map_err(|e| anyhow!("{}", sysutil::errno_text(&e)))?
            .port();
        let mut command = Command::new(&self.opts.editor);
        command
            .args(engine.worker.arguments(&WorkerLaunch {
                name: "AssetImportWorkerProbe",
                project: self.project,
                log: &log,
                port,
                parent: std::process::id(),
                job_workers: self.opts.job_workers,
                data_store: service.worker_arguments(),
            }))
            .current_dir(self.project)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stdout.try_clone()?)
            .process_group(0);
        let stage = Instant::now();
        let child = command.spawn().map_err(|error| {
            anyhow!(
                "fork/exec {}: {}",
                self.opts.editor,
                sysutil::errno_text(&error)
            )
        })?;
        let worker = Waiter::new(child);
        report.worker_pid = i64::from(worker.pid);
        let mut connection = None;
        let result = self.converse(report, run, &mut connection, stage, &log, jobs, records);
        let mut error = result.err();
        report.worker_rss_kib = rss_kib(i64::from(worker.pid));
        if let Some(connection) = &mut connection {
            connection.final_write = Some(Duration::from_secs(1));
            if let Ok(payload) = engine
                .worker
                .shutdown("own coordinator experiment complete")
            {
                let _ = write_frame(connection, &Frame { kind: 1, payload });
            }
        }
        // The exit text of a process that needed a signal to stop; `<nil>` for a clean exit.
        let sprint = |worker: &Waiter| worker.exit_error().unwrap_or_else(|| "<nil>".into());
        // A worker unloads its domain and stops its shader compilers before
        // it exits; on a busy machine, with other workers beside it, that
        // takes more than a few seconds.
        if worker.done.wait(Some(Duration::from_secs(15))) {
            match worker.exit_error() {
                Some(exit) => {
                    error = join([error, Some(anyhow!("worker shutdown: {exit}"))]);
                    report.worker_exit = exit;
                }
                None => report.worker_exit = "exit 0".into(),
            }
        } else {
            error = join([
                error,
                Some(anyhow!("worker did not stop after protocol shutdown")),
            ]);
            let _ = worker.signal_group(libc::SIGTERM);
            if worker.done.wait(Some(Duration::from_secs(2))) {
                report.worker_exit = sprint(&worker);
            } else {
                let _ = worker.signal_group(libc::SIGKILL);
                if worker.done.wait(Some(Duration::from_secs(2))) {
                    report.worker_exit = sprint(&worker);
                } else {
                    error = join([error, Some(anyhow!("worker exit wait exceeded deadline"))]);
                }
            }
        }
        drop(connection);
        if let Some(exit) = worker.exit() {
            report.worker_peak_rss_kib = exit.max_rss_kib;
        }
        drop(stdout);
        error.map_or(Ok(()), Err)
    }

    #[allow(clippy::too_many_arguments)]
    fn converse(
        &self,
        report: &mut ProbeReport,
        run: &Pass,
        slot: &mut Option<Connection>,
        mut stage: Instant,
        log: &str,
        jobs: &[&NativeImportJob],
        records: &mut Vec<ImportRecord>,
    ) -> Result<()> {
        let Pass {
            engine,
            listener,
            initialize,
            prepare,
            ..
        } = *run;
        let worker = engine.worker;
        // Context cancellation also releases Accept and reads before their deadline.
        let watcher = Watcher::new(self.ctx, listener);
        let local = listener
            .local_addr()
            .map_or_else(|_| "127.0.0.1:0".into(), |address| address.to_string());
        let (stream, peer) = listener.accept().map_err(|error| {
            let reason = match self.ctx.error() {
                Some(_) => CLOSED.to_string(),
                None => format!("accept4: {}", sysutil::errno_text(&error)),
            };
            anyhow!("worker connection: accept tcp {local}: {reason}")
        })?;
        watcher
            .attach(&stream)
            .map_err(|e| anyhow!("{}", sysutil::errno_text(&e)))?;
        let connection = slot.insert(Connection {
            stream,
            ctx: self.ctx.clone(),
            route: format!("{local}->{peer}"),
            final_write: None,
        });
        let frame = read_frame(connection)?;
        if frame != worker.handshake() {
            bail!("worker protocol name or version does not match");
        }
        write_frame(connection, &worker.handshake())?;
        report.stage = "handshake".into();
        report
            .stages
            .insert("workerHandshake".into(), Self::seconds(stage));
        let frame = capture_frame(connection, &self.output, report)?;
        report.worker_name = worker.hello(&frame.payload)?;
        validate_worker_version(engine, log)?;
        stage = Instant::now();
        let application = |payload: &[u8]| Frame {
            kind: 1,
            payload: payload.to_vec(),
        };
        write_frame(connection, &application(initialize))?;
        receive_message(connection, &self.output, report, worker, Reply::Prepared)?;
        report.stage = "initialized".into();
        report
            .stages
            .insert("workerInitialize".into(), Self::seconds(stage));
        report
            .stages
            .insert("workerReadyTotal".into(), Self::seconds(self.started));
        for job in jobs {
            stage = Instant::now();
            write_frame(connection, &application(prepare))?;
            receive_message(connection, &self.output, report, worker, Reply::Prepared)?;
            let address = worker.import_address(&job.guid, job.importer)?;
            write_frame(connection, &application(&worker.import_request(&address)))?;
            report.stage = "import-requested".into();
            let frame = receive_message(connection, &self.output, report, worker, Reply::Imported)?;
            let response = worker.import_response(&frame.payload)?;
            if response.address != address {
                bail!("import response address differs from request");
            }
            if response.consistency_failure_count != 0 {
                bail!("native importer reported consistency failures");
            }
            report.imports.push(ImportEvidence {
                guid: job.guid.clone(),
                importer_type: job.importer,
                elapsed_seconds: Self::seconds(stage),
                reply_type: frame.payload[0],
                result_id: response.result_id.clone(),
                error_code: response.error_code,
                error_message: response.error_message.clone(),
                returned_paths: (!response.paths.is_empty()).then(|| response.paths.clone()),
                metadata: ArtifactMetadata::default(),
            });
            if response.error_code != 0 {
                bail!(
                    "native importer error {}: {}",
                    response.error_code,
                    response.error_message
                );
            }
            if response.result_id == "0".repeat(32) {
                bail!("native importer did not return an artifact identity");
            }
            let metadata = worker.artifact_metadata(&response.artifact_blobs[1])?;
            if metadata.address != address {
                bail!("artifact metadata address differs from request");
            }
            report.imports.last_mut().unwrap().metadata = metadata;
            // The worker finds what a later import loads in the ArtifactDB.
            // A build records each result while its worker lives; what a
            // second worker reads is committed once the first has stopped.
            if self.opts.artifact_database {
                let committing = Instant::now();
                engine
                    .asset_database
                    .record_import(self.project, &response)?;
                report
                    .stages
                    .entry("artifactDatabase".into())
                    .or_default()
                    .0 += committing.elapsed().as_secs_f64();
            }
            records.push(ImportRecord {
                address,
                result: super::environment::decode_hex(&response.result_id)
                    .and_then(|id| id.try_into().ok())
                    .ok_or_else(|| anyhow!("import result identity is invalid"))?,
                blobs: response.artifact_blobs,
            });
        }
        report.stage = "native-imports-completed".into();
        drop(watcher);
        Ok(())
    }
}

/// Reads one application frame and keeps it as evidence under `output`.
pub fn capture_frame(
    reader: &mut impl Read,
    output: &str,
    report: &mut ProbeReport,
) -> Result<Frame> {
    let maximum = match report.max_frames {
        0 => 256,
        maximum => maximum,
    };
    if report.frames.len() >= maximum {
        bail!("worker exceeded frame count limit");
    }
    let frame = read_frame(reader)?;
    if frame.kind != 1 || frame.payload.is_empty() {
        bail!("expected worker application frame");
    }
    let total = frame.payload.len() + report.frames.iter().map(|f| f.size).sum::<usize>();
    let limit = match report.max_frame_bytes {
        0 => 16 * 1024 * 1024,
        limit => limit,
    };
    if total > limit {
        bail!("worker exceeded total frame bytes limit");
    }
    let path = sysutil::join(&[
        output,
        &format!(
            "frame-{:02}-type-{}.bin",
            report.frames.len(),
            frame.payload[0]
        ),
    ]);
    write_file(&path, &frame.payload, 0o600)?;
    report.frames.push(FrameEvidence {
        kind: frame.payload[0],
        size: frame.payload.len(),
        sha256: sha256(&frame.payload),
        path,
    });
    Ok(frame)
}

/// Captures frames until the expected reply arrives; worker log frames in
/// between are drained and fatal native diagnostics stop the run.
pub fn receive_message(
    reader: &mut impl Read,
    output: &str,
    report: &mut ProbeReport,
    worker: &dyn WorkerProtocol,
    expected: Reply,
) -> Result<Frame> {
    loop {
        let frame = capture_frame(reader, output, report)?;
        match worker.message(&frame.payload)? {
            Message::Reply(reply) if reply == expected => return Ok(frame),
            Message::Log(message) => {
                if message.contains("Exception:") || message.contains("Assertion failed") {
                    bail!("native importer diagnostic: {message}");
                }
            }
            Message::Reply(reply) => bail!("worker sent {reply:?} while awaiting {expected:?}"),
            Message::Other(kind) => {
                bail!("unsupported worker message type {kind} while awaiting {expected:?}")
            }
        }
    }
}

/// Whether the worker that wrote `log` runs the engine's release.
pub fn validate_worker_version(engine: &Engine, log: &str) -> Result<()> {
    let file = File::open(log).map_err(|e| os_error("open", log, &e))?;
    let data = read_limited(file, 128 * 1024, log)?;
    match engine.worker.logged_version(&data) {
        Some(version) if version == engine.version => Ok(()),
        Some(version) => bail!(
            "worker runs Unity {version}, not the project's Unity {}",
            engine.version
        ),
        None => bail!("worker did not report its Unity version"),
    }
}

/// `VmRSS` of a live process in KiB; 0 when it cannot be read.
pub fn rss_kib(pid: i64) -> i64 {
    let Ok(data) = std::fs::read(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in String::from_utf8_lossy(&data).split('\n') {
        if line.starts_with("VmRSS:") {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 2 {
                return crate::project::strconv::parse_int(fields[1], 10).unwrap_or(0);
            }
        }
    }
    0
}

/// A worker that lives on between imports over one context: it is started
/// once, imports what it is asked as often as it is asked, and stops when
/// it is stopped or dropped. Each accepted import is recorded in the
/// context's ArtifactDB at once, so a later import loads it.
pub(crate) struct WarmWorker {
    engine: &'static Engine,
    lifetime: Context,
    project: String,
    output: String,
    service: DataStoreService,
    connection: Connection,
    worker: Waiter,
    prepare: Vec<u8>,
    _watcher: Watcher,
    requests: usize,
}

impl WarmWorker {
    /// Starts the DataStore service and a worker over `opts.context`.
    pub(crate) fn start(opts: &ProbeOptions) -> Result<Self> {
        let lifetime = Context::background().with_cancel();
        let output = abs(&opts.output)?;
        let logs = sysutil::join(&[&output, "logs"]);
        mkdir_all(&logs, 0o700)?;
        let project = opts.context.clone();
        let engine = engine::for_worker(&project, &opts.editor)?;
        let environment = match read_worker_environment(&project) {
            Ok(environment) => Some(environment),
            Err(error) if is_not_exist(&error) => None,
            Err(error) => return Err(error),
        };
        let (initialize, prepare) = (
            engine.worker.initialize(environment.as_ref()),
            engine.worker.prepare(environment.as_ref()),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| anyhow!("listen tcp 127.0.0.1:0: {}", sysutil::errno_text(&e)))?;
        let listener = Arc::new(listener);
        let mut service = DataStoreService::new(engine, &opts.editor, &project, &logs);
        service.start(&lifetime)?;
        let started = Self::spawn(engine, opts, &project, &logs, &listener, &service);
        let (worker, stdout) = match started {
            Ok(started) => started,
            Err(error) => {
                let _ = service.stop();
                return Err(error);
            }
        };
        drop(stdout);
        let startup = lifetime.with_timeout(Duration::from_secs(120));
        let accepted = {
            let _watcher = Watcher::new(&startup, &listener);
            listener.accept()
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                let _ = worker.signal_group(libc::SIGKILL);
                let _ = service.stop();
                bail!("worker connection: accept: {}", sysutil::errno_text(&error));
            }
        };
        let watcher = Watcher::new(&lifetime, &listener);
        watcher
            .attach(&stream)
            .map_err(|e| anyhow!("{}", sysutil::errno_text(&e)))?;
        let mut warm = Self {
            engine,
            lifetime: lifetime.clone(),
            project,
            output: output.clone(),
            service,
            connection: Connection {
                stream,
                ctx: startup,
                route: format!("worker->{peer}"),
                final_write: None,
            },
            worker,
            prepare,
            _watcher: watcher,
            requests: 0,
        };
        let log = sysutil::join(&[&logs, "worker.log"]);
        if let Err(error) = warm.handshake(&initialize, &log) {
            warm.stop();
            return Err(error);
        }
        Ok(warm)
    }

    fn spawn(
        engine: &Engine,
        opts: &ProbeOptions,
        project: &str,
        logs: &str,
        listener: &TcpListener,
        service: &DataStoreService,
    ) -> Result<(Waiter, File)> {
        let log = sysutil::join(&[logs, "worker.log"]);
        let stdout = create_new(&sysutil::join(&[logs, "worker-stdout.log"]), 0o600)?;
        let port = listener
            .local_addr()
            .map_err(|e| anyhow!("{}", sysutil::errno_text(&e)))?
            .port();
        let mut command = Command::new(&opts.editor);
        command
            .args(engine.worker.arguments(&WorkerLaunch {
                name: "AssetImportWorkerWarm",
                project,
                log: &log,
                port,
                parent: std::process::id(),
                job_workers: opts.job_workers,
                data_store: service.worker_arguments(),
            }))
            .current_dir(project)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stdout.try_clone()?)
            .process_group(0);
        let child = command.spawn().map_err(|error| {
            anyhow!("fork/exec {}: {}", opts.editor, sysutil::errno_text(&error))
        })?;
        Ok((Waiter::new(child), stdout))
    }

    /// A report that holds the evidence of one request.
    fn evidence(&self) -> ProbeReport {
        ProbeReport {
            schema_version: 1,
            max_frames: 65536,
            max_frame_bytes: 128 * 1024 * 1024,
            ..Default::default()
        }
    }

    fn frames(&self) -> Result<String> {
        let directory = sysutil::join(&[&self.output, &format!("request-{}", self.requests)]);
        mkdir_all(&directory, 0o700)?;
        Ok(directory)
    }

    fn handshake(&mut self, initialize: &[u8], log: &str) -> Result<()> {
        let mut report = self.evidence();
        let frames = self.frames()?;
        let worker = self.engine.worker;
        let frame = read_frame(&mut self.connection)?;
        if frame != worker.handshake() {
            bail!("worker protocol name or version does not match");
        }
        write_frame(&mut self.connection, &worker.handshake())?;
        let frame = capture_frame(&mut self.connection, &frames, &mut report)?;
        worker.hello(&frame.payload)?;
        validate_worker_version(self.engine, log)?;
        write_frame(
            &mut self.connection,
            &Frame {
                kind: 1,
                payload: initialize.to_vec(),
            },
        )?;
        receive_message(
            &mut self.connection,
            &frames,
            &mut report,
            worker,
            Reply::Prepared,
        )?;
        Ok(())
    }

    /// Whether the worker still runs.
    pub(crate) fn alive(&self) -> bool {
        !self.worker.finished() && self.lifetime.error().is_none()
    }

    /// The release the worker runs.
    pub(crate) fn engine(&self) -> &'static Engine {
        self.engine
    }

    /// The context the worker imports from.
    pub(crate) fn project(&self) -> &str {
        &self.project
    }

    /// Imports `jobs` in order, within `ctx`'s deadline, and records each
    /// result in the context's ArtifactDB.
    pub(crate) fn import(&mut self, ctx: &Context, jobs: &[NativeImportJob]) -> Result<usize> {
        self.requests += 1;
        let frames = self.frames()?;
        let mut report = self.evidence();
        let deadline = ctx.remaining();
        self.connection.ctx = match deadline {
            Some(left) => self.lifetime.with_timeout(left),
            None => self.lifetime.clone(),
        };
        let application = |payload: &[u8]| Frame {
            kind: 1,
            payload: payload.to_vec(),
        };
        let worker = self.engine.worker;
        for job in jobs {
            write_frame(&mut self.connection, &application(&self.prepare))?;
            receive_message(
                &mut self.connection,
                &frames,
                &mut report,
                worker,
                Reply::Prepared,
            )?;
            let address = worker.import_address(&job.guid, job.importer)?;
            write_frame(
                &mut self.connection,
                &application(&worker.import_request(&address)),
            )?;
            let frame = receive_message(
                &mut self.connection,
                &frames,
                &mut report,
                worker,
                Reply::Imported,
            )?;
            let response = worker.import_response(&frame.payload)?;
            if response.address != address {
                bail!("import response address differs from request");
            }
            if response.consistency_failure_count != 0 {
                bail!("native importer reported consistency failures");
            }
            if response.error_code != 0 {
                bail!(
                    "native importer error {}: {}",
                    response.error_code,
                    response.error_message
                );
            }
            if response.result_id == "0".repeat(32) {
                bail!("native importer did not return an artifact identity");
            }
            self.engine
                .asset_database
                .record_import(&self.project, &response)?;
        }
        Ok(jobs.len())
    }

    /// Asks the worker to stop, then stops the DataStore service.
    pub(crate) fn stop(&mut self) {
        if self.worker.finished() && self.lifetime.error().is_some() {
            return;
        }
        self.connection.final_write = Some(Duration::from_secs(1));
        if let Ok(payload) = self.engine.worker.shutdown("warm worker idle") {
            let _ = write_frame(&mut self.connection, &Frame { kind: 1, payload });
        }
        if !self.worker.done.wait(Some(Duration::from_secs(15))) {
            let _ = self.worker.signal_group(libc::SIGTERM);
            if !self.worker.done.wait(Some(Duration::from_secs(2))) {
                let _ = self.worker.signal_group(libc::SIGKILL);
                self.worker.done.wait(Some(Duration::from_secs(2)));
            }
        }
        self.lifetime.cancel();
        let _ = self.service.stop();
    }
}

impl Drop for WarmWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
