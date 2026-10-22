//! The Unity releases the tool works with, and the private protocols each of
//! them speaks.
//!
//! The import worker, where it keeps the artifacts it imports (the DataStore
//! service of 6000.6, the files of `Library/Artifacts` of 6000.0) and the
//! asset databases it reads are private to Unity, and a release may change
//! any of them. Every release the tool supports has an [`Engine`] here with
//! its own implementation of each, checked against that release. A project's
//! `ProjectVersion.txt` selects the engine, and an installation runs a worker
//! only when `Unity -version` reports that same version. A release without an
//! entry is refused, never driven with another release's protocol.
//!
//! Adding a release means a module beside [`unity6000_6`] and [`unity6000_0`]
//! whose protocols were checked against that release, and its entry in
//! `ENGINES`. The rest of the
//! tool (Player serialized files, shader programs, the source graph, package
//! resolution) still serves the verified release alone and checks for it.
use crate::{
    native::{ArtifactExtraction, DataStoreLogEvidence, WorkerEnvironment},
    protocol::{
        ArtifactFile, ArtifactMetadata, Frame, ImportAddress, ImportRecord, ImportResponse,
    },
    registry,
};
use anyhow::{Context as _, Result, anyhow, bail};
use std::{
    collections::BTreeMap,
    io::Read,
    process::{Command, Stdio},
    sync::{Mutex, mpsc},
    time::Duration,
};

pub mod unity6000_0;
pub mod unity6000_6;

/// One supported Unity release.
pub struct Engine {
    /// As `ProjectVersion.txt` and `Unity -version` name it.
    pub version: &'static str,
    pub revision: &'static str,
    /// Names what the worker's import environment is made of; part of every
    /// cached import's key.
    pub environment_revision: &'static str,
    pub worker: &'static dyn WorkerProtocol,
    /// The service a worker keeps its artifacts in, which runs beside it;
    /// `None` when the worker writes them into its context's `Library`.
    pub data_store: Option<&'static dyn DataStoreProtocol>,
    pub artifacts: &'static dyn ArtifactStorage,
    pub asset_database: &'static dyn AssetDatabase,
    /// The worker loads a script assembly of `Library/ScriptAssemblies` only
    /// when the scripts and assembly definitions registered in its
    /// `SourceAssetDB` make up that assembly, as the Editor's script
    /// compilation computes its target assemblies (6000.0). Otherwise the
    /// tool's own importer assembly is loaded without a definition (6000.6).
    pub assemblies_from_definitions: bool,
    /// The format of the Player's serialized files, which the worker's
    /// build writes and the tool writes a built object into.
    pub serialized_format: crate::playerbake::FileFormat,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unity {}", self.version)
    }
}

/// Every supported release; the first is the verified profile that the rest
/// of the tool serves.
static ENGINES: &[&Engine] = &[&unity6000_6::ENGINE, &unity6000_0::ENGINE];

/// The verified release.
pub fn verified() -> &'static Engine {
    ENGINES[0]
}

/// The versions the tool supports, comma-separated.
pub fn supported_versions() -> String {
    ENGINES
        .iter()
        .map(|engine| engine.version)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The engine of a release.
pub fn supported(version: &str) -> Result<&'static Engine> {
    ENGINES
        .iter()
        .copied()
        .find(|engine| engine.version == version)
        .ok_or_else(|| {
            anyhow!(
                "Unity {version} is not supported (supported: {})",
                supported_versions()
            )
        })
}

/// The release named by the `m_EditorVersion` line of a `ProjectVersion.txt`.
pub fn project_version_of(text: &[u8]) -> Result<String> {
    String::from_utf8_lossy(text)
        .lines()
        .find_map(|line| line.trim().strip_prefix("m_EditorVersion:"))
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
        .context("ProjectVersion.txt names no m_EditorVersion")
}

/// The release a project was last opened with.
pub fn project_version(project: &str) -> Result<String> {
    let path = crate::sysutil::join(&[project, "ProjectSettings/ProjectVersion.txt"]);
    let file = std::fs::File::open(&path).with_context(|| format!("open {path}"))?;
    let mut text = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut text)
        .with_context(|| format!("read {path}"))?;
    if text.len() > 64 * 1024 {
        bail!("{path} exceeds supported bounds");
    }
    project_version_of(&text)
}

/// The engine a project selects.
pub fn of_project(project: &str) -> Result<&'static Engine> {
    supported(&project_version(project)?)
}

/// The release an installation runs, as `Unity -version` prints it. An
/// executable is asked once per process.
pub fn editor_version(editor: &str) -> Result<String> {
    static KNOWN: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
    if let Some(version) = KNOWN.lock().unwrap().get(editor) {
        return Ok(version.clone());
    }
    let version = ask_version(editor)?;
    KNOWN
        .lock()
        .unwrap()
        .insert(editor.to_string(), version.clone());
    Ok(version)
}

// A version-only invocation opens no project and returns at once; a bound
// keeps a wrong executable from holding the caller.
fn ask_version(editor: &str) -> Result<String> {
    let mut child = Command::new(editor)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run {editor} -version"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let read = stdout.take(4096).read_to_end(&mut output);
        let _ = sender.send(read.map(|_| output));
    });
    let output = receiver.recv_timeout(Duration::from_secs(15));
    let _ = child.kill();
    let _ = child.wait();
    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => bail!("read {editor} -version: {error}"),
        Err(_) => bail!("{editor} -version did not answer"),
    };
    let version = String::from_utf8_lossy(&output).trim().to_string();
    if version.is_empty() || version.contains(char::is_whitespace) {
        bail!("{editor} -version did not print a Unity version");
    }
    Ok(version)
}

/// The engine of a worker over `project` run by `editor`: the project's
/// release, which the installation has to run.
pub fn for_worker(project: &str, editor: &str) -> Result<&'static Engine> {
    let engine = of_project(project)?;
    let installed = editor_version(editor)?;
    if installed != engine.version {
        bail!(
            "the project is Unity {}, but {editor} is Unity {installed}",
            engine.version
        );
    }
    Ok(engine)
}

/// How a worker process is started.
pub struct WorkerLaunch<'a> {
    /// The name the worker reports.
    pub name: &'a str,
    pub project: &'a str,
    pub log: &'a str,
    /// The coordinator's local TCP port.
    pub port: u16,
    pub parent: u32,
    pub job_workers: usize,
    /// What the DataStore service asks of its clients; empty without one.
    pub data_store: Vec<String>,
}

/// A worker message the coordinator awaits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The worker is ready for the next import.
    Prepared,
    /// An import's result.
    Imported,
}

/// What one worker message is.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Reply(Reply),
    /// A line of the worker's log.
    Log(String),
    /// A message the coordinator does not handle; its first byte.
    Other(u8),
}

/// The coordinator's side of one release's AssetImportWorker. The transport
/// (`protocol::read_frame`, `write_frame`) is shared; what is said over it
/// is the release's.
pub trait WorkerProtocol: Sync {
    /// The command line after the executable.
    fn arguments(&self, launch: &WorkerLaunch<'_>) -> Vec<String>;
    /// The handshake each side sends first.
    fn handshake(&self) -> Frame;
    /// The worker's name from its first message.
    fn hello(&self, payload: &[u8]) -> Result<String>;
    fn message(&self, payload: &[u8]) -> Result<Message>;
    /// The message that sets up the worker, sent once; the build target's
    /// defaults without a project environment.
    fn initialize(&self, environment: Option<&WorkerEnvironment>) -> Vec<u8>;
    /// The message sent before each import.
    fn prepare(&self, environment: Option<&WorkerEnvironment>) -> Vec<u8>;
    /// The address of the import of `guid` by `importer` (-1: the importer
    /// the extension names).
    fn import_address(&self, guid: &str, importer: i32) -> Result<ImportAddress>;
    fn import_request(&self, address: &ImportAddress) -> Vec<u8>;
    fn import_response(&self, payload: &[u8]) -> Result<ImportResponse>;
    fn shutdown(&self, reason: &str) -> Result<Vec<u8>>;
    /// The release a worker log names, from its first lines.
    fn logged_version(&self, log: &[u8]) -> Option<String>;
    /// Refuses a project with settings the release's messages cannot carry.
    fn check_project(&self, _project: &str) -> Result<()> {
        Ok(())
    }
    /// Whether the release's worker is sent the project's ETC compressors
    /// (`WorkerEnvironment::etc_compressors`).
    fn sends_etc_compressors(&self) -> bool {
        false
    }
    /// Whether the release's worker is sent the lightmap encoding quality.
    fn sends_lightmap_encoding(&self) -> bool {
        true
    }
}

/// The release's storage service that holds import artifacts.
pub trait DataStoreProtocol: Sync {
    /// The service executable of the installation in `installation`.
    fn executable(&self, installation: &str) -> String;
    /// The service's command line, for a store under `Library/DataStore` of
    /// its working directory.
    fn arguments(&self, namespace: &str, log: &str) -> Vec<String>;
    /// What a worker needs to reach the service.
    fn worker_arguments(&self, namespace: &str) -> Vec<String>;
    /// What the service's log says about its state.
    fn log_evidence(&self, log: &[u8]) -> DataStoreLogEvidence;
    /// Whether a file of `/dev/shm` is shared memory of the service in
    /// `namespace`, which a service stopped by a signal leaves behind.
    fn owns_shared_memory(&self, name: &str, namespace: &str) -> bool;
}

/// Where the release keeps the results of a worker's imports, and how the
/// coordinator reads them.
pub trait ArtifactStorage: Sync {
    /// Makes what the worker of the context `project` writes its artifacts
    /// into, before the worker starts.
    fn prepare(&self, project: &str) -> Result<()>;
    /// The metadata of an import the worker answered: the record its
    /// response carries, or the one the worker committed to the ArtifactDB
    /// of the context `project` before it answered.
    fn metadata(&self, project: &str, response: &ImportResponse) -> Result<ArtifactMetadata>;
    /// The stored files of the context `project`, once its worker and any
    /// service beside it stopped.
    fn open(&self, project: &str) -> Result<Box<dyn ArtifactStore>>;
    /// The entries of a context's `Library` that hold the results of its
    /// imports: what a scene worker keeps as the project's library.
    fn library(&self) -> &'static [&'static str];
}

/// The files of imports a context holds.
pub trait ArtifactStore {
    /// Copies the content of `file` (its stored file, or the data inline in
    /// the metadata) to `output`, checking it against its content key.
    fn extract(&self, file: &ArtifactFile, output: &str) -> (ArtifactExtraction, Result<()>);
}

/// The release's asset databases a worker reads: the sources it imports and
/// the results of earlier imports.
pub trait AssetDatabase: Sync {
    /// Writes the source database of a new context.
    fn write_sources(&self, input: registry::Input) -> Result<registry::Report>;
    /// Records a changed source of a context, so that its import runs again.
    fn update_source(&self, project: &str, logical: &str) -> Result<()>;
    /// Records an accepted import as the current result of its address.
    fn record_import(&self, project: &str, response: &ImportResponse) -> Result<()>;
    /// Records the results of a stopped worker, for a worker started after
    /// it to read.
    fn commit_imports(&self, project: &str, records: &[ImportRecord]) -> Result<()>;
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_6000_0;
