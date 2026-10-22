//! Unity 6000.0.71f1, against what it was recorded doing: the traffic between
//! a batch Editor and its parallel import workers, and a worker run of this
//! tool's over `Samples/RawProject/Assets/Textures/Probe.png`.
use super::{unity6000_0 as release, *};
use crate::{
    lmdb,
    native::WorkerEnvironment,
    protocol::{ArtifactFile, guid_text, put_u32, u32_at},
    registry::{self, ContentHash},
};

const EDITOR_REQUEST: &[u8] =
    include_bytes!("../../testdata/unity6000_0/editor-import-request.bin");
const WORKER_RESPONSE: &[u8] =
    include_bytes!("../../testdata/unity6000_0/worker-import-response.bin");
/// The first 64 bytes of the Editor's Initialize message.
const EDITOR_INITIALIZE: &[u8] =
    include_bytes!("../../testdata/unity6000_0/editor-initialize-head.bin");
/// The tool's worker run: its response and the ArtifactDB the worker left.
const PROBE_RESPONSE: &[u8] =
    include_bytes!("../../testdata/unity6000_0/probe-import-response.bin");
const PROBE_ARTIFACT_DB: &[u8] = include_bytes!("../../testdata/unity6000_0/probe-ArtifactDB");
const PROBE_PNG: &[u8] =
    include_bytes!("../../../../../Samples/RawProject/Assets/Textures/Probe.png");
/// The name of the table of file hashes of a `SourceAssetDB`.
const HASH_TABLE: &str = "hash";

fn engine() -> &'static Engine {
    supported("6000.0.71f1").unwrap()
}

fn bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

#[test]
fn a_request_is_the_one_the_editor_sends() {
    let guid = guid_text(&EDITOR_REQUEST[8..24]);
    let worker = engine().worker;
    let address = worker.import_address(&guid, -1).unwrap();
    assert_eq!(worker.import_request(&address), EDITOR_REQUEST);
    assert_eq!(address[40..], [0; 4]);
}

#[test]
fn a_response_of_a_worker_to_the_editor_decodes() {
    let worker = engine().worker;
    let response = worker.import_response(WORKER_RESPONSE).unwrap();
    let guid = guid_text(&WORKER_RESPONSE[8..24]);
    assert_eq!(response.address, worker.import_address(&guid, -1).unwrap());
    assert_eq!(response.result_id, "7ab8cc10ee41544832b0597bdebdd7ee");
    assert_eq!((response.error_code, response.flag), (0, true));
    assert!(response.error_message.is_empty() && response.paths.is_empty());
    assert!(response.artifact_blobs.is_empty());
    assert_eq!(
        worker.message(WORKER_RESPONSE).unwrap(),
        Message::Reply(Reply::Imported)
    );
    assert!(worker.import_response(&WORKER_RESPONSE[..79]).is_err());
    let mut other = WORKER_RESPONSE.to_vec();
    other[0] = 18;
    assert!(worker.import_response(&other).is_err());

    // An error, its message and the paths it lacks follow the fixed part.
    let mut failed = WORKER_RESPONSE[..80].to_vec();
    failed[64] = 6;
    put_u32(&mut failed, 72, 8);
    put_u32(&mut failed, 76, 2);
    failed.resize(88, 0);
    for (field, text) in [(80, &b"Assets/A.png\0"[..]), (84, b"Assets/B.png\0")] {
        let relative = (failed.len() - field) as u32;
        put_u32(&mut failed, field, relative);
        failed.extend_from_slice(text);
    }
    let relative = (failed.len() - 68) as u32;
    put_u32(&mut failed, 68, relative);
    failed.extend_from_slice(b"missing\0");
    let response = worker.import_response(&failed).unwrap();
    assert_eq!(response.error_code, 6);
    assert_eq!(response.error_message, "missing");
    assert_eq!(response.paths, ["Assets/A.png", "Assets/B.png"]);
}

#[test]
fn worker_messages_are_those_of_the_editors_traffic() {
    let worker = engine().worker;
    assert_eq!(
        worker.message(&[23]).unwrap(),
        Message::Reply(Reply::Prepared)
    );
    assert_eq!(worker.message(&[23, 0]).unwrap(), Message::Other(23));
    // A log line as the worker sends one.
    let mut log = vec![2, 0, 0, 0, 5, 0, 0, 0, 4, 0, 0, 0];
    log.extend_from_slice(b"Start importing Assets/T1.png\0");
    assert_eq!(
        worker.message(&log).unwrap(),
        Message::Log("Start importing Assets/T1.png".into())
    );
    assert_eq!(worker.message(&[21]).unwrap(), Message::Other(21));
    let shutdown = worker.shutdown("done").unwrap();
    assert_eq!(shutdown, b"\x01\0\0\0\x04\0\0\0done\0");
}

/// The environment of the scratch project the Editor's traffic was
/// recorded with: gamma, OpenGL Core and Vulkan, default settings.
fn environment() -> WorkerEnvironment {
    WorkerEnvironment {
        unity_version: "6000.0.71f1".into(),
        build_target: 24,
        subtarget: 2,
        color_space: 0,
        graphics_api_mask: (1 << 17) | (1 << 21),
        shader_precision_model: 0,
        bc7_compressor: 2,
        ..Default::default()
    }
}

/// The custom dependencies of an Initialize or Prepare message.
fn keys(message: &[u8]) -> Vec<(String, Vec<u8>)> {
    let start = 40 + u32_at(message, 40).unwrap() as usize;
    (0..u32_at(message, 44).unwrap() as usize)
        .map(|index| {
            let entry = start + index * 24;
            let text = entry + u32_at(message, entry).unwrap() as usize;
            let end = text + message[text..].iter().position(|b| *b == 0).unwrap();
            (
                String::from_utf8(message[text..end].to_vec()).unwrap(),
                message[entry + 8..entry + 24].to_vec(),
            )
        })
        .collect()
}

#[test]
fn initialize_and_prepare_carry_the_environment_as_the_editor_does() {
    let worker = engine().worker;
    let initialize = worker.initialize(Some(&environment()));
    // The fixed part of the Editor's message but for the number of custom
    // dependencies: the Editor sends 305, the tool those importers ask for.
    assert_eq!(initialize[..44], EDITOR_INITIALIZE[..44]);
    assert_eq!(initialize[48..64], EDITOR_INITIALIZE[48..64]);
    assert_eq!(u32_at(&initialize, 44).unwrap(), 11);
    let prepare = worker.prepare(Some(&environment()));
    assert_eq!((prepare[0], &prepare[1..]), (17, &initialize[1..]));
    // The values the Editor sent for the same settings.
    let sent = keys(&initialize);
    for (key, value) in [
        ("graphics/shader-precision-model", "01"),
        ("graphics/metal-needs-renderpass-fbfetch-fallback", "01"),
        ("graphics/texture-import-overrides", "01"),
        ("graphics/default-texture-compression-format", "01"),
        ("graphics/texture-platform-settings", "01"),
        ("graphics/active-color-space", "02"),
        ("graphics/normal-map-encoding", "02"),
        ("graphics/hdr-cubemap-encoding", "03"),
        ("graphics/bc7-compressor", "0102"),
        ("graphics/etc-compressor", "01010204"),
        ("2d/spriteatlas-import-mode", "01"),
    ] {
        let mut expected = bytes(value);
        expected.resize(16, 0);
        let found = sent.iter().find(|(name, _)| name == key);
        assert_eq!(found.map(|(_, value)| value), Some(&expected), "{key}");
    }
    assert!(
        !sent
            .iter()
            .any(|(name, _)| name == "graphics/lightmap-encoding")
    );
    // Without a project environment: the build target alone.
    let bare = worker.initialize(None);
    assert_eq!(bare.len(), 64);
    assert_eq!((bare[0], u32_at(&bare, 4).unwrap()), (16, 24));
    assert_eq!(u32_at(&bare, 44).unwrap(), 0);
}

#[test]
fn the_worker_is_started_as_the_editor_starts_it() {
    let arguments = engine().worker.arguments(&WorkerLaunch {
        name: "W",
        project: "/p",
        log: "/l",
        port: 7,
        parent: 9,
        job_workers: 0,
        data_store: Vec::new(),
    });
    assert_eq!(
        arguments.join(" "),
        "-adb2 -batchMode -noUpm -name W -projectPath /p -logFile /l -srvPort 7 -nographics \
         -job-worker-count 2 -background-job-worker-count 2 -gc-helper-count 1"
    );
    assert!(engine().data_store.is_none());
    let log = b"Unity Editor version:    6000.0.71f1 (907bc2d768b5)\nBranch: x\n";
    assert_eq!(
        engine().worker.logged_version(log).as_deref(),
        Some("6000.0.71f1")
    );
}

/// A context whose `Library/ArtifactDB` is the one the worker left.
fn context() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let library = directory.path().join("Library");
    std::fs::create_dir(&library).unwrap();
    std::fs::write(library.join("ArtifactDB"), PROBE_ARTIFACT_DB).unwrap();
    let project = directory.path().to_string_lossy().into_owned();
    (directory, project)
}

fn unknown_result() -> crate::protocol::ImportResponse {
    let mut unknown = PROBE_RESPONSE.to_vec();
    unknown[48] ^= 1;
    engine().worker.import_response(&unknown).unwrap()
}

#[test]
fn the_metadata_of_an_import_is_the_record_the_worker_wrote() {
    let (_directory, project) = context();
    let engine = engine();
    let response = engine.worker.import_response(PROBE_RESPONSE).unwrap();
    assert_eq!(response.result_id, "42c02560456fbe777597d0aa61145e3a");
    let metadata = engine.artifacts.metadata(&project, &response).unwrap();
    assert_eq!(metadata.address, response.address);
    assert_eq!(metadata.files.len(), 1);
    let file = &metadata.files[0];
    // The Editor's own artifact of the same source has this key.
    assert_eq!(crate::hex(&file.hash), "59472f189582011f98a3c9f3f5fce86c");
    assert_eq!((file.kind, file.inline_data.len()), (0, 0));

    let error = engine
        .artifacts
        .metadata(&project, &unknown_result())
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("recorded no metadata"),
        "{error:#}"
    );
    assert!(release::artifact_metadata(&[0; 87]).is_err());
}

#[test]
fn an_accepted_import_becomes_the_current_revision_of_its_address() {
    let (_directory, project) = context();
    let engine = engine();
    let response = engine.worker.import_response(PROBE_RESPONSE).unwrap();
    let database = format!("{project}/Library/ArtifactDB");
    let current = || lmdb::read_table(&database, "CurrentRevisions").unwrap();
    // The worker leaves that to the Editor.
    assert!(current().is_empty());
    engine
        .asset_database
        .record_import(&project, &response)
        .unwrap();
    engine
        .asset_database
        .record_import(&project, &response)
        .unwrap();
    let records = current();
    assert_eq!(records.len(), 1);
    // `ArtifactDBWriteTxn::SetCurrentRevision`: SpookyHash of the GUID, the
    // importer ID and the importer hash; the address and the result.
    let address = response.address;
    assert_eq!(
        records[0].key,
        ContentHash::Spooky.of(&[&address[..20], &address[24..40]].concat())
    );
    assert_eq!(records[0].key, release::address_key(&address));
    assert_eq!(records[0].value[..40], address[..40]);
    assert_eq!(crate::hex(&records[0].value[40..]), response.result_id);
    // The worker's own records stay.
    let ids = lmdb::read_table(&database, "ArtifactKeyToArtifactIDs").unwrap();
    assert_eq!(ids.len(), 1);
    // A result the worker did not record is refused.
    assert!(
        engine
            .asset_database
            .record_import(&project, &unknown_result())
            .is_err()
    );
    // What a second worker reads: the results of a stopped one.
    let record = crate::protocol::ImportRecord {
        address,
        result: bytes(&response.result_id).try_into().unwrap(),
        blobs: Vec::new(),
    };
    engine
        .asset_database
        .commit_imports(&project, &[record])
        .unwrap();
    assert_eq!(current(), records);
}

#[test]
fn stored_and_inline_artifacts_are_read_by_their_content_key() {
    let (_directory, project) = context();
    let engine = engine();
    engine.artifacts.prepare(&project).unwrap();
    for directory in ["Artifacts/00", "Artifacts/ff", "TempArtifacts/Primary"] {
        assert!(std::path::Path::new(&format!("{project}/Library/{directory}")).is_dir());
    }
    let content = b"stored artifact".to_vec();
    let hash = ContentHash::Spooky.of(&content);
    let name = crate::hex(&hash);
    std::fs::write(
        format!("{project}/Library/Artifacts/{}/{name}", &name[..2]),
        &content,
    )
    .unwrap();
    let store = engine.artifacts.open(&project).unwrap();
    let output = tempfile::tempdir().unwrap();
    let destination = |name: &str| output.path().join(name).to_string_lossy().into_owned();
    let stored = ArtifactFile {
        kind: 0,
        hash,
        ..Default::default()
    };
    let (proof, result) = store.extract(&stored, &destination("a.bin"));
    result.unwrap();
    assert!(proof.content_key_verified && proof.bytes == content.len() as u64);
    assert_eq!(std::fs::read(destination("a.bin")).unwrap(), content);

    let inline = ArtifactFile {
        kind: 1,
        hash: ContentHash::Spooky.of(b"inline"),
        inline_data: b"inline".to_vec(),
        ..Default::default()
    };
    store.extract(&inline, &destination("b.bin")).1.unwrap();
    assert_eq!(std::fs::read(destination("b.bin")).unwrap(), b"inline");

    // Content that does not match its key, a missing file and an unknown
    // kind are refused, and nothing is written into the store.
    let wrong = ArtifactFile {
        inline_data: b"other".to_vec(),
        ..inline.clone()
    };
    assert!(store.extract(&wrong, &destination("c.bin")).1.is_err());
    let missing = ArtifactFile {
        hash: [7; 16],
        ..stored.clone()
    };
    assert!(store.extract(&missing, &destination("c.bin")).1.is_err());
    let other = ArtifactFile {
        kind: 2,
        ..stored.clone()
    };
    assert!(store.extract(&other, &destination("c.bin")).1.is_err());
    let inside = format!("{project}/Library/Artifacts/copy.bin");
    assert!(store.extract(&stored, &inside).1.is_err());
    assert_eq!(
        engine.artifacts.library(),
        ["ArtifactDB", "ArtifactDB-lock", "Artifacts"]
    );
}

#[test]
fn the_source_database_hashes_with_spooky_and_whole_seconds() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Temp/context");
    std::fs::create_dir_all(project.join("Library")).unwrap();
    std::fs::create_dir_all(project.join("Assets/Textures")).unwrap();
    std::fs::write(project.join("Assets/Textures/Probe.png"), PROBE_PNG).unwrap();
    std::fs::write(
        project.join("Assets/Textures/Probe.png.meta"),
        "fileFormatVersion: 2\nguid: a01ac09a24c8493b862564ce94c80305\n",
    )
    .unwrap();
    let report = engine()
        .asset_database
        .write_sources(registry::Input {
            project: project.clone(),
            unity_version: "6000.0.71f1".into(),
            assets: vec![registry::Asset {
                path: "Assets/Textures/Probe.png".into(),
                source: String::new(),
                meta: String::new(),
                guid: String::new(),
                directory: false,
                script: false,
            }],
            roots: Default::default(),
            singletons: Default::default(),
        })
        .unwrap();
    assert_eq!(
        report.source_hash_algorithm,
        "SpookyV2-128 low64/high64 little endian"
    );
    let record = |database: &std::path::Path| {
        let data = std::fs::read(database).unwrap();
        let image = lmdb::Image::open(&data).unwrap();
        assert_eq!(
            image.table("DBVersion").unwrap()[0].value,
            [0x05, 0x71, 0x0c, 0x74]
        );
        for record in image.table("GuidToChildren").unwrap() {
            assert_eq!(
                record.value[..16],
                ContentHash::Spooky.of(&record.value[16..])
            );
        }
        image
            .table(HASH_TABLE)
            .unwrap()
            .into_iter()
            .find(|record| record.key == b"assets/textures/probe.png\0")
            .unwrap()
            .value
    };
    let value = record(&report.database);
    // The 6000.0 Editor's own record of the same file has this hash.
    assert_eq!(crate::hex(&value[..16]), "cb25a0e914961df12dc114525ac29f05");
    let ticks = u64::from_le_bytes(value[16..24].try_into().unwrap());
    assert_eq!(ticks % 10_000_000, 0);
    // A changed source is recorded the same way.
    std::fs::write(project.join("Assets/Textures/Probe.png"), b"other").unwrap();
    engine()
        .asset_database
        .update_source(project.to_str().unwrap(), "Assets/Textures/Probe.png")
        .unwrap();
    let value = record(&report.database);
    assert_eq!(value[..16], ContentHash::Spooky.of(b"other"));
    let ticks = u64::from_le_bytes(value[16..24].try_into().unwrap());
    assert_eq!(ticks % 10_000_000, 0);
}

#[test]
fn the_worker_is_sent_the_projects_etc_compressors() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("ProjectSettings")).unwrap();
    let path = directory.path().to_string_lossy().into_owned();
    let settings = directory
        .path()
        .join("ProjectSettings/EditorSettings.asset");
    std::fs::write(
        &settings,
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!159 &1\nEditorSettings:\n  m_EtcTextureFastCompressor: 2\n  m_EtcTextureNormalCompressor: 2\n  m_EtcTextureBestCompressor: 5\n",
    )
    .unwrap();
    let read = crate::native::read_editor_settings(&path).unwrap();
    assert_eq!(read.etc_compressors, [2, 2, 5]);
    let mut environment = WorkerEnvironment {
        etc_compressors: read.etc_compressors.to_vec(),
        ..Default::default()
    };
    let message = release::environment_message(&environment, 16);
    let key = b"graphics/etc-compressor\0";
    let at = message
        .windows(key.len())
        .position(|window| window == key)
        .expect("the key is sent");
    assert!(message.len() > at);
    // Without project values the Editor's defaults are sent.
    environment.etc_compressors.clear();
    assert_ne!(release::environment_message(&environment, 16), message);
    assert!(engine().worker.sends_etc_compressors());
    assert!(!verified().worker.sends_etc_compressors());
}
