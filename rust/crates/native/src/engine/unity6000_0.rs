//! Unity 6000.0.71f1 on Linux x64: the AssetImportWorker messages, where the
//! worker keeps its artifacts and the asset databases, as observed for this
//! release (the traffic between an Editor and its parallel import workers,
//! their databases, and the worker's message handlers in the Editor binary).
//! Not a general Unity API.
//!
//! What sets it apart from 6000.6:
//! - There is no `Unity.DataStore`. The worker writes the files of an import
//!   under `Library/Artifacts` of its project and its records
//!   (`ArtifactKeyToArtifactIDs`, `ArtifactIDToArtifactMetaInfo`, ...) into
//!   `Library/ArtifactDB` before it answers; its response carries no
//!   records. Which result is current is the Editor's to record.
//! - An import address (`ArtifactKey`) is 40 bytes, without the cached hash
//!   of 6000.6; the fixed parts of the messages differ accordingly.
//! - Sources, artifacts and database keys are hashed with SpookyHash V2
//!   rather than XXH3.
use super::{
    ArtifactStorage, ArtifactStore, AssetDatabase, Engine, Message, Reply, WorkerLaunch,
    WorkerProtocol, unity6000_6,
};
use crate::{
    hex,
    lmdb::{self, Record, Tables},
    native::{LibraryArtifactStore, WorkerEnvironment},
    protocol::{
        ArtifactFile, ArtifactMetadata, Frame, ImportAddress, ImportRecord, ImportResponse,
        blob_array, blob_string, guid_wire, put_u32, relative_start,
    },
    registry::{self, ContentHash},
    sysutil,
};
use anyhow::{Context as _, Result, ensure};

pub const VERSION: &str = "6000.0.71f1";

pub static ENGINE: Engine = Engine {
    version: VERSION,
    revision: "907bc2d768b5",
    environment_revision: "6000.0.71f1-linux-native-environment-v2",
    worker: &Unity6000_0,
    data_store: None,
    artifacts: &Unity6000_0,
    asset_database: &Unity6000_0,
    assemblies_from_definitions: true,
    serialized_format: crate::playerbake::FileFormat {
        version: 22,
        unity_version: VERSION,
    },
};

/// The release's `SourceAssetDB`: the tables and layouts of 6000.6, another
/// version record, SpookyHash, and modification times to the whole second.
pub const SOURCE_DATABASE: registry::Schema = registry::Schema {
    unity_version: VERSION,
    db_version: [0x05, 0x71, 0x0c, 0x74],
    hash: ContentHash::Spooky,
    subsecond_times: false,
};

/// The hash of source files, artifact files and database keys.
const HASH: ContentHash = ContentHash::Spooky;

struct Unity6000_0;

// Message kinds, the first byte of a payload: those of 6000.6, as the
// worker's dispatch table and the captured traffic show.
const INITIALIZE: u8 = 16;
const PREPARE: u8 = 17;
const IMPORT_REQUEST: u8 = 18;
const IMPORT_RESPONSE: u8 = 20;
const WORKER_PREPARED: u8 = 23;
const LOG: u8 = 2;
const CONSOLE_LOG: u8 = 3;
/// `BuildTarget.StandaloneLinux64`.
pub const LINUX64: i32 = 24;

/// Initialize and Prepare: `BlobAssetImportParameters` from byte 4 to 48,
/// then three values of the message, then the custom dependencies.
const PARAMETERS_SIZE: usize = 64;
/// The custom dependency array of the parameters: relative offset, count.
const DEPENDENCIES_FIELD: usize = 40;
/// `ImportRequestMessage`: the kind, then the address at 8.
const IMPORT_REQUEST_SIZE: usize = 48;
/// `ImportResponseMessage`: address 8..48, result 48..64, error code 64,
/// flag 65, error message 68, paths 72.
const IMPORT_RESPONSE_SIZE: usize = 80;
/// `ArtifactKey`: GUID, importer ID, padding, importer hash.
const ADDRESS_SIZE: usize = 40;
/// `ArtifactMetaInfo`: result 0..16, address 16..56, two values, then the
/// files (64), the properties (72) and the produced objects (80).
const META_INFO_SIZE: usize = 88;
const META_INFO_FILES: usize = 64;

/// `NormalMapEncoding::DXT5nm`, the Linux standalone default.
const NORMAL_MAP_DXT5NM: u32 = 1;
/// `HDRCubemapEncodingQuality::High`, the Linux standalone default.
const ENCODING_QUALITY_HIGH: u32 = 2;

impl WorkerProtocol for Unity6000_0 {
    fn arguments(&self, launch: &WorkerLaunch<'_>) -> Vec<String> {
        // As the Editor starts a worker; this release has no `-parentPid`
        // and no DataStore of its own.
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
            "-nographics",
            "-job-worker-count",
            &launch.job_workers.max(2).to_string(),
            "-background-job-worker-count",
            "2",
            "-gc-helper-count",
            "1",
        ]
        .map(String::from)
        .to_vec();
        arguments.extend(launch.data_store.iter().cloned());
        arguments
    }
    fn handshake(&self) -> Frame {
        unity6000_6::handshake()
    }
    fn hello(&self, payload: &[u8]) -> Result<String> {
        unity6000_6::worker_hello(payload)
    }
    fn message(&self, payload: &[u8]) -> Result<Message> {
        Ok(match payload.first() {
            Some(&WORKER_PREPARED) if payload.len() == 1 => Message::Reply(Reply::Prepared),
            Some(&IMPORT_RESPONSE) => Message::Reply(Reply::Imported),
            Some(&(LOG | CONSOLE_LOG)) => Message::Log(unity6000_6::worker_log(payload)?),
            Some(&kind) => Message::Other(kind),
            None => Message::Other(0),
        })
    }
    fn initialize(&self, environment: Option<&WorkerEnvironment>) -> Vec<u8> {
        match environment {
            Some(environment) => environment_message(environment, INITIALIZE),
            None => parameters(INITIALIZE, LINUX64),
        }
    }
    fn prepare(&self, environment: Option<&WorkerEnvironment>) -> Vec<u8> {
        match environment {
            Some(environment) => environment_message(environment, PREPARE),
            None => parameters(PREPARE, LINUX64),
        }
    }
    fn import_address(&self, guid: &str, importer: i32) -> Result<ImportAddress> {
        import_address(guid, importer, [0; 16])
    }
    fn import_request(&self, address: &ImportAddress) -> Vec<u8> {
        import_request(address)
    }
    fn import_response(&self, payload: &[u8]) -> Result<ImportResponse> {
        import_response(payload)
    }
    fn shutdown(&self, reason: &str) -> Result<Vec<u8>> {
        unity6000_6::shutdown(reason)
    }
    fn logged_version(&self, log: &[u8]) -> Option<String> {
        unity6000_6::logged_version(log)
    }
    fn sends_etc_compressors(&self) -> bool {
        true
    }
    fn sends_lightmap_encoding(&self) -> bool {
        false
    }
}

/// The worker writes the records of an import into the context's
/// ArtifactDB before it answers, and its files into `Library/Artifacts`.
impl ArtifactStorage for Unity6000_0 {
    /// The directories the Editor makes for its workers: one per first byte
    /// of a content key under `Library/Artifacts`, and where an import
    /// writes its files before they are moved there. Without them the
    /// worker dies of a division by zero writing the first file.
    fn prepare(&self, project: &str) -> Result<()> {
        let library = sysutil::join(&[project, "Library"]);
        let mut directories: Vec<String> = (0..=255u8)
            .map(|first| sysutil::join(&[&library, "Artifacts", &hex(&[first])]))
            .collect();
        for temporary in ["Primary", "Extra"] {
            directories.push(sysutil::join(&[&library, "TempArtifacts", temporary]));
        }
        for directory in directories {
            std::fs::create_dir_all(&directory).with_context(|| format!("create {directory}"))?;
        }
        Ok(())
    }
    fn metadata(&self, project: &str, response: &ImportResponse) -> Result<ArtifactMetadata> {
        let id = result_id(response)?;
        let path = sysutil::join(&[project, "Library", "ArtifactDB"]);
        let records = lmdb::read_table(&path, META_INFO)
            .with_context(|| format!("read the import results of {path}"))?;
        let record = records
            .iter()
            .find(|record| record.key[..] == id[..])
            .with_context(|| {
                format!(
                    "the worker recorded no metadata for import result {}",
                    response.result_id
                )
            })?;
        artifact_metadata(&record.value)
    }
    fn open(&self, project: &str) -> Result<Box<dyn ArtifactStore>> {
        Ok(Box::new(LibraryArtifactStore::open(project, HASH)?))
    }
    fn library(&self) -> &'static [&'static str] {
        &["ArtifactDB", "ArtifactDB-lock", "Artifacts"]
    }
}

impl AssetDatabase for Unity6000_0 {
    fn write_sources(&self, input: registry::Input) -> Result<registry::Report> {
        registry::write_with(&SOURCE_DATABASE, input)
    }
    fn update_source(&self, project: &str, logical: &str) -> Result<()> {
        registry::update_source_with(&SOURCE_DATABASE, project, logical)
    }
    fn record_import(&self, project: &str, response: &ImportResponse) -> Result<()> {
        let id = result_id(response)?;
        set_current_revisions(project, &[(response.address, id)])
    }
    fn commit_imports(&self, project: &str, records: &[ImportRecord]) -> Result<()> {
        let revisions: Vec<_> = records
            .iter()
            .map(|record| (record.address, record.result))
            .collect();
        set_current_revisions(project, &revisions)
    }
}

const CURRENT_REVISIONS: &str = "CurrentRevisions";
const RESULT_IDS: &str = "ArtifactKeyToArtifactIDs";
const META_INFO: &str = "ArtifactIDToArtifactMetaInfo";

/// The key of an import address in `CurrentRevisions` and
/// `ArtifactKeyToArtifactIDs` (`ArtifactDBWriteTxn::SetCurrentRevision`):
/// SpookyHash V2 over the GUID, the importer ID and the importer hash.
pub fn address_key(address: &ImportAddress) -> [u8; 16] {
    HASH.of(&[&address[..20], &address[24..40]].concat())
}

fn result_id(response: &ImportResponse) -> Result<[u8; 16]> {
    let text = response.result_id.as_bytes();
    let mut id = [0; 16];
    ensure!(
        text.len() == 32 && text.is_ascii(),
        "import result identity is invalid"
    );
    for (index, byte) in id.iter_mut().enumerate() {
        let digits = std::str::from_utf8(&text[index * 2..index * 2 + 2])?;
        *byte = u8::from_str_radix(digits, 16).context("import result identity is invalid")?;
    }
    Ok(id)
}

/// Makes each result the current revision of its address, as the Editor
/// does when it accepts a worker's answer. The worker added the result to
/// the results of the address itself, with its records.
fn set_current_revisions(project: &str, revisions: &[(ImportAddress, [u8; 16])]) -> Result<()> {
    let path = sysutil::join(&[project, "Library", "ArtifactDB"]);
    lmdb::commit(&path, |tables| {
        for (address, id) in revisions {
            current_revision(tables, address, id)?;
        }
        Ok(())
    })
    .map(|_| ())
    .with_context(|| format!("commit import results to {path}"))
}

fn current_revision(tables: &mut Tables, address: &ImportAddress, id: &[u8; 16]) -> Result<()> {
    let key = address_key(address).to_vec();
    let known = tables
        .get(RESULT_IDS)
        .and_then(|records| records.iter().find(|record| record.key == key))
        .context("the worker recorded no result for the import address")?;
    let (ids, _) = blob_array(&known.value, ADDRESS_SIZE, 16)?;
    ensure!(
        ids.as_chunks::<16>().0.iter().any(|known| known == id),
        "the worker did not record import result {} for its address",
        hex(id)
    );
    let mut revision = address[..ADDRESS_SIZE].to_vec();
    revision.extend_from_slice(id);
    let records = tables.entry(CURRENT_REVISIONS.into()).or_default();
    records.retain(|record| record.key != key);
    records.push(Record {
        key,
        value: revision,
    });
    Ok(())
}

/// Initialize (16) or Prepare (17) for `target` with nothing else set.
pub fn parameters(kind: u8, target: i32) -> Vec<u8> {
    let mut data = vec![0; PARAMETERS_SIZE];
    data[0] = kind;
    put_u32(&mut data, 4, target as u32);
    data
}

/// The Initialize (16) or Prepare (17) message carrying a project's
/// environment, laid out as the Editor sends it.
pub fn environment_message(environment: &WorkerEnvironment, kind: u8) -> Vec<u8> {
    let mut payload = parameters(kind, environment.build_target as i32);
    put_u32(&mut payload, 8, environment.subtarget);
    payload[20] = 1; // This worker enables texture import compression.
    put_u32(&mut payload, 24, environment.color_space);
    put_u32(&mut payload, 28, environment.graphics_api_mask);
    put_u32(&mut payload, 32, 1); // Unity 6 latest scripting runtime.
    // The three values of the message after the parameters, as the Editor
    // sends them; what they select was not established.
    put_u32(&mut payload, 48, 3);
    put_u32(&mut payload, 52, 3);
    // Each value is what the Editor registers for the key: a setting plus
    // one in the first byte of a Hash128, so that zero stays "not
    // registered", as in 6000.6. This release registers no lightmap
    // encoding and an ETC compressor setting.
    let one = |value: u32| [1u8.wrapping_add(value as u8), 0, 0, 0];
    let keys: [(&str, [u8; 4]); 11] = [
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
        ("graphics/hdr-cubemap-encoding", one(ENCODING_QUALITY_HIGH)),
        // Valid marker, then the effective compressor.
        (
            "graphics/bc7-compressor",
            [1, environment.bc7_compressor as u8, 0, 0],
        ),
        // Valid marker, then the fast, normal and best compressors.
        ("graphics/etc-compressor", {
            let etc = match environment.etc_compressors.as_slice() {
                [fast, normal, best] => [*fast, *normal, *best],
                _ => crate::native::DEFAULT_ETC_COMPRESSORS,
            };
            [1, etc[0] as u8, etc[1] as u8, etc[2] as u8]
        }),
        (
            "2d/spriteatlas-import-mode",
            one(environment.sprite_atlas_v2),
        ),
    ];
    put_u32(
        &mut payload,
        DEPENDENCIES_FIELD,
        (PARAMETERS_SIZE - DEPENDENCIES_FIELD) as u32,
    );
    put_u32(&mut payload, DEPENDENCIES_FIELD + 4, keys.len() as u32);
    payload.resize(PARAMETERS_SIZE + keys.len() * 24, 0);
    for (index, (key, value)) in keys.iter().enumerate() {
        let start = PARAMETERS_SIZE + index * 24;
        let relative = (payload.len() - start) as u32;
        put_u32(&mut payload, start, relative);
        payload[start + 8..start + 12].copy_from_slice(value);
        payload.extend_from_slice(key.as_bytes());
        payload.push(0);
    }
    payload
}

/// An address as the coordinator handles one: the 40 bytes of the
/// release's `ArtifactKey`, then four zero bytes where 6000.6 keeps its
/// cached hash.
pub fn import_address(guid: &str, importer: i32, importer_hash: [u8; 16]) -> Result<ImportAddress> {
    let mut data = [0; 44];
    data[..16].copy_from_slice(&guid_wire(guid)?);
    put_u32(&mut data, 16, importer as u32);
    data[24..40].copy_from_slice(&importer_hash);
    Ok(data)
}

fn address_of(data: &[u8]) -> Result<ImportAddress> {
    let mut address = [0; 44];
    address[..ADDRESS_SIZE].copy_from_slice(data.get(..ADDRESS_SIZE).context("short address")?);
    Ok(address)
}

pub fn import_request(address: &ImportAddress) -> Vec<u8> {
    let mut data = vec![0; IMPORT_REQUEST_SIZE];
    data[0] = IMPORT_REQUEST;
    data[8..8 + ADDRESS_SIZE].copy_from_slice(&address[..ADDRESS_SIZE]);
    data
}

pub fn import_response(data: &[u8]) -> Result<ImportResponse> {
    ensure!(
        data.len() >= IMPORT_RESPONSE_SIZE && data[0] == IMPORT_RESPONSE,
        "expected complete ImportResponse fixed part"
    );
    let error_message = blob_string(data, 68)?;
    let (_, count) = blob_array(data, 72, 4)?;
    let mut paths = Vec::with_capacity(count);
    if count > 0 {
        let start = relative_start(data, 72)?;
        for i in 0..count {
            paths.push(blob_string(data, start + i * 4)?);
        }
    }
    Ok(ImportResponse {
        address: address_of(&data[8..])?,
        result_id: hex(&data[48..64]),
        error_code: data[64],
        flag: data[65] != 0,
        error_message,
        paths,
        // The records of the result are in the ArtifactDB.
        artifact_blobs: Vec::new(),
        dependencies: Vec::new(),
        consistency_failure_count: 0,
    })
}

pub fn artifact_metadata(data: &[u8]) -> Result<ArtifactMetadata> {
    ensure!(
        data.len() >= META_INFO_SIZE,
        "ArtifactMetaInfo fixed part is incomplete"
    );
    let address = address_of(&data[16..])?;
    let (_, count) = blob_array(data, META_INFO_FILES, 32)?;
    let mut files = Vec::with_capacity(count);
    if count > 0 {
        let start = relative_start(data, META_INFO_FILES)?;
        for i in 0..count {
            let f = start + i * 32;
            files.push(ArtifactFile {
                kind: crate::protocol::u32_at(data, f)?,
                name: blob_string(data, f + 4)?,
                hash: data
                    .get(f + 8..f + 24)
                    .context("artifact file lies outside its metadata")?
                    .try_into()?,
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
