//! Unity 6000.6.3f1 on Linux x64: the AssetImportWorker messages, the
//! `Unity.DataStore` service and the asset databases, as observed for this
//! release. Not a general Unity API.
use super::{
    ArtifactStorage, ArtifactStore, AssetDatabase, DataStoreProtocol, Engine, Message, Reply,
    WorkerLaunch, WorkerProtocol,
};
use crate::{
    hex,
    native::{DataStoreLogEvidence, UdsArtifactStore, WorkerEnvironment},
    protocol::{
        ArtifactFile, ArtifactMetadata, Frame, ImportAddress, ImportRecord, ImportResponse,
        MAX_FRAME_PAYLOAD, blob_array, blob_string, guid_wire, put_u32, relative_start, u32_at,
    },
    registry,
};
use anyhow::{Context as _, Result, ensure};

pub const VERSION: &str = crate::ENGINE_VERSION;

pub static ENGINE: Engine = Engine {
    version: VERSION,
    revision: "45d8eee7de74",
    environment_revision: "6000.6.3f1-linux-native-environment-v3",
    worker: &Unity6000_6,
    data_store: Some(&Unity6000_6),
    artifacts: &Unity6000_6,
    asset_database: &Unity6000_6,
};

struct Unity6000_6;

// Message kinds, the first byte of a payload.
const HELLO: u8 = 0;
const SHUTDOWN: u8 = 1;
const LOG: u8 = 2;
const CONSOLE_LOG: u8 = 3;
const INITIALIZE: u8 = 16;
const PREPARE: u8 = 17;
const IMPORT_REQUEST: u8 = 18;
const CREATE_DEFAULT_ASSET: u8 = 19;
const IMPORT_RESPONSE: u8 = 20;
const SCHEDULE_IMPORT: u8 = 21;
const IMPORT_COMPLETE: u8 = 22;
const WORKER_PREPARED: u8 = 23;
/// `BuildTarget.StandaloneLinux64`.
pub const LINUX64: i32 = 24;

/// `NormalMapEncoding::DXT5nm`, the Linux standalone default.
const NORMAL_MAP_DXT5NM: u32 = 1;
/// `LightmapEncodingQuality::High` and `HDRCubemapEncodingQuality::High`,
/// the Linux standalone defaults.
const ENCODING_QUALITY_HIGH: u32 = 2;

impl WorkerProtocol for Unity6000_6 {
    fn arguments(&self, launch: &WorkerLaunch<'_>) -> Vec<String> {
        let mut arguments: Vec<String> = [
            "-adb2",
            "-batchMode",
            "-noUpm",
            "-name",
            launch.name,
            "-projectPath",
            launch.project,
            "-logFile",
            launch.log,
            "-srvPort",
            &launch.port.to_string(),
            "-parentPid",
            &launch.parent.to_string(),
            "-job-worker-count",
            &launch.job_workers.max(2).to_string(),
            "-background-job-worker-count",
            "2",
            "-nographics",
            "-noDiagnosticSwitchPersistentStore",
        ]
        .map(String::from)
        .to_vec();
        arguments.extend(launch.data_store.iter().cloned());
        arguments
    }
    fn handshake(&self) -> Frame {
        handshake()
    }
    fn hello(&self, payload: &[u8]) -> Result<String> {
        worker_hello(payload)
    }
    fn message(&self, payload: &[u8]) -> Result<Message> {
        Ok(match payload.first() {
            Some(&WORKER_PREPARED) if payload.len() == 1 => Message::Reply(Reply::Prepared),
            Some(&IMPORT_RESPONSE) => Message::Reply(Reply::Imported),
            Some(&(LOG | CONSOLE_LOG)) => Message::Log(worker_log(payload)?),
            Some(&kind) => Message::Other(kind),
            None => Message::Other(0),
        })
    }
    fn initialize(&self, environment: Option<&WorkerEnvironment>) -> Vec<u8> {
        match environment {
            Some(environment) => environment_message(environment, INITIALIZE),
            None => initialize(LINUX64),
        }
    }
    fn prepare(&self, environment: Option<&WorkerEnvironment>) -> Vec<u8> {
        match environment {
            Some(environment) => environment_message(environment, PREPARE),
            None => prepare(LINUX64),
        }
    }
    fn import_address(&self, guid: &str, importer: i32) -> Result<ImportAddress> {
        import_address(guid, importer, [0; 16])
    }
    fn import_request(&self, address: &ImportAddress) -> Vec<u8> {
        import_request(address, 0)
    }
    fn import_response(&self, payload: &[u8]) -> Result<ImportResponse> {
        import_response(payload)
    }
    fn shutdown(&self, reason: &str) -> Result<Vec<u8>> {
        shutdown(reason)
    }
    fn logged_version(&self, log: &[u8]) -> Option<String> {
        logged_version(log)
    }
}

impl DataStoreProtocol for Unity6000_6 {
    fn executable(&self, installation: &str) -> String {
        crate::sysutil::join(&[installation, "Data", "Tools", "Unity.DataStore"])
    }
    fn arguments(&self, namespace: &str, log: &str) -> Vec<String> {
        [
            "--memory-size",
            "1073741824",
            "--shared-postfix",
            namespace,
            "--working-directory",
            "Library/DataStore",
            "--silent-console",
            "--shutdown-grace",
            "0",
            "--client-process-check",
            "0",
            "--uds-log-file",
            log,
            "--debug-log",
            "2",
        ]
        .map(String::from)
        .to_vec()
    }
    fn worker_arguments(&self, namespace: &str) -> Vec<String> {
        vec!["-shared-postfix".into(), namespace.into()]
    }
    fn log_evidence(&self, log: &[u8]) -> DataStoreLogEvidence {
        let has = |needle: &str| {
            log.windows(needle.len())
                .any(|window| window == needle.as_bytes())
        };
        let mut evidence = DataStoreLogEvidence {
            initialized: has("Initialising UDS system complete:"),
            service: has("  - System type: Service"),
            application_started: has("Starting Unity Data Store Application"),
            shutdown_logged: has("Unity Data Store Application Complete")
                || has("Shut down UDS service system complete"),
            startup_error_logged: has("[Error]"),
            ready_logged: false,
        };
        evidence.ready_logged = evidence.initialized
            && evidence.service
            && evidence.application_started
            && !evidence.shutdown_logged
            && !evidence.startup_error_logged;
        evidence
    }
    fn owns_shared_memory(&self, name: &str, namespace: &str) -> bool {
        name.strip_prefix("sem.")
            .unwrap_or(name)
            .strip_prefix("udsMem")
            .is_some_and(|rest| rest.starts_with(namespace))
    }
}

/// The response carries the records of an import; the files are in the
/// DataStore of the context.
impl ArtifactStorage for Unity6000_6 {
    /// The DataStore service makes its directory itself.
    fn prepare(&self, _project: &str) -> Result<()> {
        Ok(())
    }
    fn metadata(&self, _project: &str, response: &ImportResponse) -> Result<ArtifactMetadata> {
        artifact_metadata(
            response
                .artifact_blobs
                .get(1)
                .context("import response lacks its artifact metadata")?,
        )
    }
    fn open(&self, project: &str) -> Result<Box<dyn ArtifactStore>> {
        Ok(Box::new(UdsArtifactStore::open(&crate::sysutil::join(&[
            project,
            "Library",
            "DataStore",
        ]))?))
    }
    fn library(&self) -> &'static [&'static str] {
        &["ArtifactDB", "ArtifactDB-lock", "DataStore"]
    }
}

impl AssetDatabase for Unity6000_6 {
    fn write_sources(&self, input: registry::Input) -> Result<registry::Report> {
        registry::write(input)
    }
    fn update_source(&self, project: &str, logical: &str) -> Result<()> {
        registry::update_source(project, logical)
    }
    fn record_import(&self, project: &str, response: &ImportResponse) -> Result<()> {
        crate::native::artifactdb::publish(project, response).map(|_| ())
    }
    fn commit_imports(&self, project: &str, records: &[ImportRecord]) -> Result<()> {
        crate::native::artifactdb::commit(project, records)
    }
}

/// The release a worker log names on its `Unity Editor version:` line.
pub fn logged_version(log: &[u8]) -> Option<String> {
    log.split(|&byte| byte == b'\n').find_map(|line| {
        let rest = line.strip_prefix(b"Unity Editor version:")?;
        Some(
            String::from_utf8_lossy(rest)
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string(),
        )
    })
}
pub fn handshake() -> Frame {
    Frame {
        kind: 0,
        payload: b"AIW\x01\0\x01\0".to_vec(),
    }
}
pub fn worker_hello(data: &[u8]) -> Result<String> {
    ensure!(
        data.len() >= 8 && data[0] == HELLO,
        "expected WorkerHello message"
    );
    blob_string(data, 4)
}
pub fn worker_log(data: &[u8]) -> Result<String> {
    ensure!(
        data.len() >= 12 && matches!(data[0], LOG | CONSOLE_LOG),
        "expected worker log message"
    );
    ensure!(
        data[0] != CONSOLE_LOG || data.len() >= 80,
        "console log fixed part is incomplete"
    );
    blob_string(data, 8)
}
pub fn shutdown(reason: &str) -> Result<Vec<u8>> {
    ensure!(!reason.contains('\0'), "shutdown reason is invalid");
    let mut data = vec![0; 8];
    data[0] = SHUTDOWN;
    put_u32(&mut data, 4, 4);
    data.extend_from_slice(reason.as_bytes());
    data.push(0);
    Ok(data)
}
pub fn initialize(target: i32) -> Vec<u8> {
    let mut data = vec![0; 96];
    data[0] = INITIALIZE;
    put_u32(&mut data, 4, target as u32);
    data
}
pub fn prepare(target: i32) -> Vec<u8> {
    let mut data = initialize(target);
    data[0] = PREPARE;
    data
}

/// The Initialize (16) or Prepare (17) message carrying a project's
/// environment.
pub fn environment_message(environment: &WorkerEnvironment, kind: u8) -> Vec<u8> {
    let mut payload = initialize(environment.build_target as i32);
    payload[0] = kind;
    put_u32(&mut payload, 8, environment.subtarget);
    payload[24] = 1; // This worker enables texture import compression.
    put_u32(&mut payload, 28, environment.color_space);
    put_u32(&mut payload, 32, environment.graphics_api_mask);
    put_u32(&mut payload, 36, 1); // Unity 6 latest scripting runtime.
    // Each value is what the Editor registers for the key
    // (`RefreshInternalV2`): a setting plus one in the first byte of a
    // Hash128, so that zero stays "not registered". An importer that asks
    // for a key the message lacks aborts the worker.
    let one = |value: u32| [1u8.wrapping_add(value as u8), 0];
    let keys: [(&str, [u8; 2]); 11] = [
        (
            "graphics/shader-precision-model",
            one(environment.shader_precision_model),
        ),
        // Linux cannot require the Metal renderpass fallback.
        ("graphics/metal-needs-renderpass-fbfetch-fallback", one(0)),
        // Valid marker, optional log2(maxSize)+1 and compression override.
        // This coordinator uses no overrides.
        ("graphics/texture-import-overrides", one(0)),
        (
            "graphics/default-texture-compression-format",
            one(environment.default_texture_compression_format),
        ),
        ("graphics/texture-platform-settings", one(0)),
        (
            "graphics/active-color-space",
            one(1 + environment.color_space),
        ),
        ("graphics/normal-map-encoding", one(NORMAL_MAP_DXT5NM)),
        ("graphics/lightmap-encoding", one(ENCODING_QUALITY_HIGH)),
        ("graphics/hdr-cubemap-encoding", one(ENCODING_QUALITY_HIGH)),
        // Valid marker, then the effective compressor.
        (
            "graphics/bc7-compressor",
            [1, environment.bc7_compressor as u8],
        ),
        (
            "2d/spriteatlas-import-mode",
            one(environment.sprite_atlas_v2),
        ),
    ];
    put_u32(&mut payload, 44, 96 - 44);
    put_u32(&mut payload, 48, keys.len() as u32);
    payload.resize(96 + keys.len() * 24, 0);
    for (index, (key, value)) in keys.iter().enumerate() {
        let start = 96 + index * 24;
        let relative = (payload.len() - start) as u32;
        put_u32(&mut payload, start, relative);
        payload[start + 8..start + 10].copy_from_slice(value);
        payload.extend_from_slice(key.as_bytes());
        payload.push(0);
    }
    payload
}

fn address_hash(data: &mut ImportAddress) -> Result<()> {
    let mut hash = xxhash_rust::xxh32::xxh32(&data[..16], u32_at(data, 16)?);
    hash ^= u32_at(data, 24)?
        .wrapping_add(hash << 6)
        .wrapping_add(hash >> 2)
        .wrapping_add(0x9e3779b9);
    put_u32(data, 40, hash);
    Ok(())
}
pub fn import_address(guid: &str, importer: i32, importer_hash: [u8; 16]) -> Result<ImportAddress> {
    let mut data = [0; 44];
    data[..16].copy_from_slice(&guid_wire(guid)?);
    put_u32(&mut data, 16, importer as u32);
    data[24..40].copy_from_slice(&importer_hash);
    address_hash(&mut data)?;
    Ok(data)
}
pub fn import_request(address: &ImportAddress, flags: u32) -> Vec<u8> {
    let mut data = vec![0; 64];
    data[0] = IMPORT_REQUEST;
    data[8..52].copy_from_slice(address);
    put_u32(&mut data, 56, flags);
    data
}
pub fn create_default_asset(address: &ImportAddress) -> Result<Vec<u8>> {
    ensure!(
        u32_at(address, 16)? as i32 == -1,
        "DefaultAsset requires importer ID -1"
    );
    let mut data = vec![0; 56];
    data[0] = CREATE_DEFAULT_ASSET;
    data[8..52].copy_from_slice(address);
    Ok(data)
}
pub fn schedule_import(data: &[u8]) -> Result<ImportAddress> {
    ensure!(
        data.len() == 56 && data[0] == SCHEDULE_IMPORT,
        "expected ScheduleImport message"
    );
    Ok(data[8..52].try_into()?)
}
pub fn import_complete(
    address: &ImportAddress,
    state: u32,
    result_id: [u8; 16],
    paths: &[String],
    flag: bool,
) -> Result<Vec<u8>> {
    ensure!(
        paths.len() <= (MAX_FRAME_PAYLOAD - 96) / 4,
        "too many dependency artifact paths"
    );
    let mut data = vec![0; 96 + paths.len() * 4];
    data[0] = IMPORT_COMPLETE;
    data[8..52].copy_from_slice(address);
    put_u32(&mut data, 56, state);
    data[64..80].copy_from_slice(&result_id);
    data[88] = u8::from(flag);
    if !paths.is_empty() {
        put_u32(&mut data, 80, 16);
        put_u32(&mut data, 84, paths.len() as u32);
    }
    for (i, path) in paths.iter().enumerate() {
        ensure!(!path.contains('\0'), "dependency artifact path is invalid");
        ensure!(
            data.len() + path.len() < MAX_FRAME_PAYLOAD,
            "dependency artifact paths exceed frame limit"
        );
        let field = 96 + i * 4;
        let relative = data.len() - field;
        put_u32(&mut data, field, relative as u32);
        data.extend_from_slice(path.as_bytes());
        data.push(0);
    }
    Ok(data)
}
pub fn import_response(data: &[u8]) -> Result<ImportResponse> {
    ensure!(
        data.len() >= 128 && data[0] == IMPORT_RESPONSE,
        "expected complete ImportResponse fixed part"
    );
    let error_message = blob_string(data, 76)?;
    let (_, count) = blob_array(data, 80, 4)?;
    let mut paths = Vec::with_capacity(count);
    if count > 0 {
        let start = relative_start(data, 80)?;
        for i in 0..count {
            paths.push(blob_string(data, start + i * 4)?);
        }
    }
    let mut artifact_blobs = Vec::new();
    for field in [88, 96, 104] {
        artifact_blobs.push(blob_array(data, field, 1)?.0.to_vec());
    }
    let (deps, _) = blob_array(data, 112, 48)?;
    Ok(ImportResponse {
        address: data[8..52].try_into()?,
        result_id: hex(&data[56..72]),
        error_code: data[72],
        flag: data[73] != 0,
        error_message,
        paths,
        artifact_blobs,
        dependencies: deps
            .as_chunks::<48>()
            .0
            .iter()
            .map(|b| b[..44].try_into().unwrap())
            .collect(),
        consistency_failure_count: u32_at(data, 120)? as i32,
    })
}
pub fn artifact_metadata(data: &[u8]) -> Result<ArtifactMetadata> {
    ensure!(
        data.len() >= 112,
        "ArtifactMetaInfo fixed part is incomplete"
    );
    let mut address = [0; 44];
    address[..20].copy_from_slice(&data[16..36]);
    address[24..40].copy_from_slice(&data[40..56]);
    address_hash(&mut address)?;
    let (_, count) = blob_array(data, 88, 32)?;
    let mut files = Vec::with_capacity(count);
    if count > 0 {
        let start = relative_start(data, 88)?;
        for i in 0..count {
            let f = start + i * 32;
            files.push(ArtifactFile {
                kind: u32_at(data, f)?,
                name: blob_string(data, f + 4)?,
                hash: data[f + 8..f + 24].try_into()?,
                inline_data: blob_array(data, f + 24, 1)?.0.to_vec(),
            });
        }
    }
    Ok(ArtifactMetadata {
        output_id: hex(&data[..16]),
        address,
        files,
    })
}
