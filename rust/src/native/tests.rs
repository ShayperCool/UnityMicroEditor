//! Tests of the native import coordinator.
use super::{
    adopt::adoption_inputs,
    mesh_cpu::{
        read_owned_native_mesh_file, retain_native_model_mesh_cpu, write_immutable_native_mesh_file,
    },
    probe::{ImportRecord, commit_import_results},
    support::{is_canceled, text},
    texture_profile::{recorded_bc7_dependency, verify_warm_bc7_dependency},
    *,
};
use crate::{
    ENGINE_VERSION,
    cancel::Context,
    lmdb::{self, Record, Tables},
    project::{
        NativeContentDependency, NativeContentFile, NativeContentObject, NativeContentResource,
        NativeReplacement, Options, RetainedBuiltinShader, Session,
    },
    protocol::{
        ArtifactFile, ArtifactMetadata, Frame, blob_array, blob_string, guid_wire, import_address,
        write_frame,
    },
    registry::{self, native_hash},
    sha256,
    util::{atomic_json, within_directory},
};
use std::{
    collections::BTreeMap,
    fs,
    io::Cursor,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    sync::Arc,
};

const MODEL_SOURCE: &str = "Assets/Models/Floor.fbx";
const TEST_GUID: &str = "71705265666978617564696f70726f62";
const WORKER_SETTINGS: &str = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!129 &1\nPlayerSettings:\n  m_ActiveColorSpace: 1\n  shaderPrecisionModel: 0\n  m_BuildTargetGraphicsAPIs: []\n  m_BuildTargetDefaultTextureCompressionFormat: []\n";

struct Temp(#[allow(dead_code)] tempfile::TempDir, String);
impl Temp {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        Self(directory, root)
    }
    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.1)
    }
}
fn write(path: &str, data: impl AsRef<[u8]>) {
    fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}
fn exists(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn background() -> Context {
    Context::background()
}

// ---- validation ----

fn worker_log_fixture(kind: u8, message: &str) -> Vec<u8> {
    let size = if kind == 3 { 80 } else { 12 };
    let mut payload = vec![0u8; size];
    payload[0] = kind;
    payload[8..12].copy_from_slice(&((size - 8) as u32).to_le_bytes());
    payload.extend_from_slice(message.as_bytes());
    payload.push(0);
    payload
}
fn worker_wire_fixture(payloads: &[Vec<u8>]) -> Cursor<Vec<u8>> {
    let mut wire = Vec::new();
    for payload in payloads {
        write_frame(
            &mut wire,
            &Frame {
                kind: 1,
                payload: payload.clone(),
            },
        )
        .unwrap();
    }
    Cursor::new(wire)
}
fn remaining(reader: &Cursor<Vec<u8>>) -> usize {
    reader.get_ref().len() - reader.position() as usize
}

#[test]
fn receive_message_drains_both_native_log_kinds() {
    let mut reader = worker_wire_fixture(&[
        worker_log_fixture(2, "Refreshing native importer environment"),
        worker_log_fixture(3, "Worker is ready"),
        vec![23],
    ]);
    let output = Temp::new();
    let mut report = ProbeReport::default();
    let frame = receive_message(&mut reader, &output.1, &mut report, 23).unwrap();
    assert!(frame.kind == 1 && frame.payload == [23]);
    assert!(remaining(&reader) == 0 && report.frames.len() == 3);
    for evidence in &report.frames {
        let data = fs::read(&evidence.path).unwrap();
        assert!(data.len() == evidence.size && data[0] == evidence.kind);
    }
}

#[test]
fn receive_message_rejects_native_fatal_diagnostics() {
    for diagnostic in [
        "InvalidOperationException: importer settings failed",
        "Assertion failed on expression: 'IsPersistent()'",
    ] {
        let mut reader = worker_wire_fixture(&[worker_log_fixture(3, diagnostic), vec![23]]);
        let output = Temp::new();
        let mut report = ProbeReport::default();
        let error = receive_message(&mut reader, &output.1, &mut report, 23).unwrap_err();
        assert!(text(&error).contains(diagnostic), "{error:#}");
        assert!(remaining(&reader) != 0 && report.frames.len() == 1);
    }
}

#[test]
fn worker_version_must_match_supported_installation() {
    for (log, valid) in [
        ("Unity Editor version:    6000.6.3f1 (45d8eee7de74)\n", true),
        ("Unity Editor version:    6000.6.4f1 (different)\n", false),
        ("Worker has connected\n", false),
    ] {
        let root = Temp::new();
        let path = root.path("worker.log");
        write(&path, log);
        assert_eq!(validate_worker_version(&path).is_ok(), valid, "{log}");
    }
}

// ---- datastore ----

#[test]
fn data_store_readiness_rejects_client_shutdown_and_error_logs() {
    let initialized = "Initialising UDS system complete:\n  - System type: Service\n";
    let started = "Starting Unity Data Store Application\n";
    for (name, log, ready) in [
        ("service", format!("{initialized}{started}"), true),
        ("missing application", initialized.to_string(), false),
        (
            "client",
            format!("Initialising UDS system complete:\n  - System type: Client\n{started}"),
            false,
        ),
        (
            "shutdown",
            format!("{initialized}{started}Shut down UDS service system complete\n"),
            false,
        ),
        (
            "startup error",
            format!("{initialized}{started}[Error] shared memory allocation failed\n"),
            false,
        ),
    ] {
        let root = Temp::new();
        let path = root.path("service.log");
        write(&path, log);
        let evidence = read_data_store_log(&path).unwrap();
        assert_eq!(evidence.ready_logged, ready, "{name}: {evidence:?}");
    }
}

#[test]
fn data_store_missing_log_is_pending() {
    let root = Temp::new();
    let evidence = read_data_store_log(&root.path("missing.log")).unwrap();
    assert!(!evidence.ready_logged);
}

#[test]
fn data_store_canceled_start_creates_nothing() {
    let root = Temp::new();
    let mut service = DataStoreService::new(
        &root.path("Unity"),
        &root.path("context"),
        &root.path("logs"),
    );
    let ctx = background().with_cancel();
    ctx.cancel();
    let error = service.start(&ctx).unwrap_err();
    assert!(is_canceled(&error), "{error:#}");
    assert_eq!(fs::read_dir(&root.1).unwrap().count(), 0);
    service.stop().unwrap();
}

#[test]
fn data_store_missing_executable_does_not_create_context() {
    let root = Temp::new();
    let installation = root.path("Editor");
    fs::create_dir(&installation).unwrap();
    let mut service =
        DataStoreService::new(&installation, &root.path("context"), &root.path("logs"));
    assert!(service.start(&background()).is_err());
    assert!(!exists(&service.context));
}

#[test]
fn data_store_ready_log_requires_running_process() {
    let root = Temp::new();
    let service = DataStoreService::new("", "", &root.1);
    write(
        &service.service_log(),
        "Initialising UDS system complete:\n  - System type: Service\nStarting Unity Data Store Application\n",
    );
    let status = service.status().unwrap();
    assert!(!status.ready && !status.running && status.log_evidence.ready_logged);
}

// ---- worker environment ----

#[test]
fn worker_environment_preserves_source_graphics_settings_and_native_dependencies() {
    let environment = parse_worker_environment(WORKER_SETTINGS.as_bytes()).unwrap();
    assert!(
        environment.color_space == 1
            && environment.graphics_api_mask == 1 << 17
            && environment.shader_precision_model == 0
    );
    let (initialize, prepare) = (environment.message(16), environment.message(17));
    assert!(initialize[0] == 16 && prepare[0] == 17 && initialize[1..] == prepare[1..]);
    let word = |at: usize| u32::from_le_bytes(initialize[at..at + 4].try_into().unwrap());
    assert!(
        word(4) == 24
            && word(8) == 2
            && initialize[24] == 1
            && word(28) == 1
            && word(32) == 1 << 17
            && word(36) == 1
    );
    let (dependencies, _) = blob_array(&initialize, 44, 24).unwrap();
    // Every key with the value the pinned Editor registers for these
    // settings (read back from its ArtifactDB).
    let keys: [(&str, [u8; 2]); 11] = [
        ("graphics/shader-precision-model", [1, 0]),
        ("graphics/metal-needs-renderpass-fbfetch-fallback", [1, 0]),
        ("graphics/texture-import-overrides", [1, 0]),
        ("graphics/default-texture-compression-format", [1, 0]),
        ("graphics/texture-platform-settings", [1, 0]),
        ("graphics/active-color-space", [3, 0]),
        ("graphics/normal-map-encoding", [2, 0]),
        ("graphics/lightmap-encoding", [3, 0]),
        ("graphics/hdr-cubemap-encoding", [3, 0]),
        ("graphics/bc7-compressor", [1, 2]),
        ("2d/spriteatlas-import-mode", [1, 0]),
    ];
    assert_eq!(dependencies.len(), 24 * keys.len());
    for (index, (key, value)) in keys.iter().enumerate() {
        assert_eq!(blob_string(&initialize, 96 + 24 * index).unwrap(), *key);
        let mut expected = [0u8; 16];
        expected[..2].copy_from_slice(value);
        assert_eq!(
            dependencies[index * 24 + 8..index * 24 + 24],
            expected,
            "{key}"
        );
    }
    let custom = WORKER_SETTINGS
        .replacen("shaderPrecisionModel: 0", "shaderPrecisionModel: 1", 1)
        .replacen(
            "m_BuildTargetGraphicsAPIs: []",
            "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 15000000i\n    m_Automatic: 0",
            1,
        );
    let environment = parse_worker_environment(custom.as_bytes()).unwrap();
    assert!(
        environment.graphics_api_mask == 1 << 21
            && environment.shader_precision_model == 1
            && environment.message(16)[104] == 2
    );
}

#[test]
fn worker_environment_rejects_ambiguous_or_unproven_settings() {
    let apis = "m_BuildTargetGraphicsAPIs: []";
    let formats = "m_BuildTargetDefaultTextureCompressionFormat: []";
    for source in [
        WORKER_SETTINGS.replacen("m_ActiveColorSpace: 1", "m_ActiveColorSpace: 2", 1),
        WORKER_SETTINGS.replacen("shaderPrecisionModel: 0", "shaderPrecisionModel: 2", 1),
        WORKER_SETTINGS.replacen("shaderPrecisionModel: 0\n", "", 1),
        WORKER_SETTINGS.replacen(
            "shaderPrecisionModel: 0",
            "shaderPrecisionModel: 0\n  shaderPrecisionModel: 1",
            1,
        ),
        WORKER_SETTINGS.replacen("!u!129", "!u!114", 1),
        WORKER_SETTINGS.replacen(
            apis,
            "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 02000000i\n    m_Automatic: 0",
            1,
        ),
        WORKER_SETTINGS.replacen(
            apis,
            "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 1500000015000000i\n    m_Automatic: 0",
            1,
        ),
        WORKER_SETTINGS.replacen(
            formats,
            "m_BuildTargetDefaultTextureCompressionFormat:\n  - m_BuildTarget: Standalone\n    m_Formats: 03000000i",
            1,
        ),
        WORKER_SETTINGS.replacen(
            formats,
            "m_BuildTargetDefaultTextureCompressionFormat:\n  - m_BuildTarget: Standalone\n    m_Formats: 00000000i\n  - m_BuildTarget: Standalone\n    m_Formats: 00000000i",
            1,
        ),
    ] {
        assert!(
            parse_worker_environment(source.as_bytes()).is_err(),
            "{source}"
        );
    }
    let project = Temp::new();
    write(
        &project.path("ProjectSettings/ProjectSettings.asset"),
        WORKER_SETTINGS,
    );
    let version = project.path("ProjectSettings/ProjectVersion.txt");
    write(&version, "m_EditorVersion: 6000.6.3f10\n");
    assert!(read_worker_environment(&project.1).is_err());
    write(&version, format!("m_EditorVersion: {ENGINE_VERSION}\n"));
    let environment = read_worker_environment(&project.1).unwrap();
    // The hash is of the settings as a context holds them.
    assert_eq!(
        environment.settings_sha256,
        sha256(&super::environment::context_player_settings(
            WORKER_SETTINGS.as_bytes()
        ))
    );
}

// ---- artifact ----

fn uds_storage_record(hash: &[u8], kind: u32, file: u64, offset: u64, size: u64) -> Record {
    let mut value = vec![0u8; 32];
    value[..4].copy_from_slice(&kind.to_le_bytes());
    value[8..16].copy_from_slice(&file.to_le_bytes());
    value[16..24].copy_from_slice(&offset.to_le_bytes());
    value[24..32].copy_from_slice(&size.to_le_bytes());
    Record {
        key: hash.to_vec(),
        value,
    }
}

#[test]
fn uds_reads_exact_inline_and_concat_content() {
    let directory = Temp::new();
    let (inline, concat) = (
        b"inline exact bytes\0".to_vec(),
        b"file exact bytes\x01".to_vec(),
    );
    let (inline_hash, concat_hash) = (native_hash(&inline), native_hash(&concat));
    let mut data = [b"prefix".to_vec(), concat.clone(), b"suffix".to_vec()].concat();
    write(&directory.path("UDSData_7.bin"), &data);
    let mut store = UdsArtifactStore::from_records(
        &directory.1,
        &[
            uds_storage_record(&inline_hash, 3, 0, 0, inline.len() as u64),
            uds_storage_record(&concat_hash, 2, 7, 6, concat.len() as u64),
        ],
        &[Record {
            key: inline_hash.to_vec(),
            value: inline.clone(),
        }],
    )
    .unwrap();
    for (hash, expected) in [(inline_hash, &inline), (concat_hash, &concat)] {
        let (proof, content) = store.read(&hash);
        assert_eq!(&content.unwrap(), expected);
        assert!(proof.sha256 == sha256(expected) && proof.content_key_verified);
    }
    let other = Temp::new();
    let output = other.path("asset.bin");
    store.extract(&concat_hash, &output).1.unwrap();
    assert!(store.extract(&concat_hash, &output).1.is_err());
    assert!(
        store
            .extract(&concat_hash, &directory.path("must-not-write"))
            .1
            .is_err()
    );
    store.content.get_mut(&inline_hash[..]).unwrap()[0] ^= 1;
    assert!(store.read(&inline_hash).1.is_err());
    data[6] ^= 1;
    write(&directory.path("UDSData_7.bin"), &data);
    assert!(store.read(&concat_hash).1.is_err());
}

#[test]
fn uds_rejects_truncated_and_unsupported_storage() {
    let directory = Temp::new();
    let hash = [3u8; 16];
    write(&directory.path("UDSData_0.bin"), "short");
    for record in [
        uds_storage_record(&hash, 2, 0, 4, 2),
        uds_storage_record(&hash, 2, 0, u64::MAX, 2),
        uds_storage_record(&hash, 3, 0, 0, 2),
        uds_storage_record(&hash, 1, 0, 0, 2),
        uds_storage_record(&hash, 2, 0, 0, MAX_ARTIFACT_BYTES + 1),
    ] {
        let store =
            UdsArtifactStore::from_records(&directory.1, std::slice::from_ref(&record), &[])
                .unwrap();
        assert!(store.read(&hash).1.is_err(), "{record:?}");
    }
    let truncated = Record {
        key: hash.to_vec(),
        value: vec![1],
    };
    assert!(UdsArtifactStore::from_records(&directory.1, &[truncated], &[]).is_err());
}

// ---- project context ----

fn fixture_source() -> Temp {
    let project = Temp::new();
    let source = project.path(MODEL_SOURCE);
    write(&source, b"Kaydara FBX Binary  \x00fixture");
    write(
        &format!("{source}.meta"),
        "fileFormatVersion: 2\nguid: b1d15452ab70b6a669e9b8e3c0b91251\nModelImporter:\n  meshes:\n    globalScale: 1\n",
    );
    project
}
fn project_context_source() -> Temp {
    let project = fixture_source();
    write(
        &project.path("ProjectSettings/ProjectVersion.txt"),
        format!("m_EditorVersion: {ENGINE_VERSION}\r\n"),
    );
    project
}
fn registry_test_asset(project: &str, name: &str, guid: &str, importer: &str) {
    let filename = format!("{project}/{name}");
    write(&filename, format!("content of {name}"));
    write(
        &format!("{filename}.meta"),
        format!(
            "fileFormatVersion: 2\nguid: {guid}\n{importer}:\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
        ),
    );
}
fn create_context(
    project: &str,
    output: &str,
    assets: &[&str],
) -> anyhow::Result<ProjectImportContext> {
    let assets: Vec<String> = assets.iter().map(|a| a.to_string()).collect();
    let (prepared, result) = create_project_import_context(&background(), project, output, &assets);
    result.map(|()| prepared)
}

#[test]
fn project_context_owned_library_survives_editor_temp_cleanup() {
    let project = project_context_source();
    let prepared = create_context(
        &project.1,
        &project.path("Library/UnityMicroEditor/imports/model"),
        &[MODEL_SOURCE],
    )
    .unwrap();
    let _ = fs::remove_dir_all(project.path("Temp"));
    verify_project_import_sources(&background(), &prepared).unwrap();
    assert!(exists(&format!("{}/{MODEL_SOURCE}", prepared.project)));
    for path in [
        "Library/ScriptAssemblies/new",
        "Library/UnityMicroEditorOther/new",
        "Library/UnityMicroEditor",
    ] {
        assert!(
            create_context(&project.1, &project.path(path), &[MODEL_SOURCE]).is_err(),
            "{path}"
        );
    }
}

#[test]
fn project_context_copies_source_identity_without_seeding_library() {
    let project = project_context_source();
    write(
        &project.path("Library/SourceAssetDB"),
        "must not seed worker",
    );
    write(
        &project.path("Assets/Models.meta"),
        "guid: c1d15452ab70b6a669e9b8e3c0b91251\nfolderAsset: yes\n",
    );
    write(
        &project.path("ProjectSettings/ProjectSettings.asset"),
        "PlayerSettings:\n  m_ColorSpace: 1\n",
    );
    let output = project.path("Temp/project-import/new");
    let prepared = create_context(&project.1, &output, &[MODEL_SOURCE]).unwrap();
    assert!(
        prepared.assets().len() == 1
            && prepared.assets()[0].guid == "b1d15452ab70b6a669e9b8e3c0b91251"
    );
    let meta = format!("{MODEL_SOURCE}.meta");
    for logical in [
        MODEL_SOURCE,
        meta.as_str(),
        "Assets/Models.meta",
        "ProjectSettings/ProjectVersion.txt",
        "ProjectSettings/ProjectSettings.asset",
    ] {
        assert_eq!(
            fs::read(project.path(logical)).unwrap(),
            fs::read(format!("{}/{logical}", prepared.project)).unwrap(),
            "{logical}"
        );
    }
    assert_eq!(
        fs::read_dir(format!("{}/Library", prepared.project))
            .unwrap()
            .count(),
        0
    );
    let report = registry::write(registry::Input {
        project: prepared.project.clone().into(),
        unity_version: String::new(),
        assets: vec![registry::Asset {
            path: MODEL_SOURCE.into(),
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
        report
            .guid_paths
            .get("c1d15452ab70b6a669e9b8e3c0b91251")
            .map(String::as_str),
        Some("Assets/Models")
    );
    verify_project_import_sources(&background(), &prepared).unwrap();
    write(&project.path(MODEL_SOURCE), "changed during import");
    assert!(verify_project_import_sources(&background(), &prepared).is_err());
    let manifest = format!("{output}/project-context.json");
    let before = fs::read(&manifest).unwrap();
    assert!(create_context(&project.1, &output, &[MODEL_SOURCE]).is_err());
    assert_eq!(before, fs::read(&manifest).unwrap());
}

#[test]
fn project_context_package_source_is_materialized_with_logical_path() {
    let project = project_context_source();
    registry_test_asset(
        &project.1,
        "Library/PackageCache/com.test.models@abc/Runtime/Texture.png",
        "b2d15452ab70b6a669e9b8e3c0b91251",
        "TextureImporter",
    );
    let logical = "Packages/com.test.models/Runtime/Texture.png";
    let prepared = create_context(
        &project.1,
        &project.path("Temp/package-context"),
        &[logical],
    )
    .unwrap();
    assert!(exists(&format!("{}/{logical}", prepared.project)));
    assert!(!exists(&format!(
        "{}/Library/PackageCache",
        prepared.project
    )));
}

#[test]
fn project_context_rejects_escapes_bad_versions_and_cancelled_copies() {
    for scenario in [
        "outputEscape",
        "sourceEscape",
        "badVersion",
        "duplicate",
        "cancelled",
    ] {
        let project = project_context_source();
        let outside = Temp::new();
        let mut output = project.path("Temp/rejected");
        let mut assets = vec![MODEL_SOURCE.to_string()];
        let ctx = background().with_cancel();
        match scenario {
            "outputEscape" => output = outside.path("rejected"),
            "sourceEscape" => {
                let filename = project.path(MODEL_SOURCE);
                let target = outside.path("outside.fbx");
                write(&target, "unrelated source");
                fs::remove_file(&filename).unwrap();
                symlink(&target, &filename).unwrap();
            }
            "badVersion" => write(
                &project.path("ProjectSettings/ProjectVersion.txt"),
                "m_EditorVersion: 6000.5.0f1\n",
            ),
            "duplicate" => assets.push(MODEL_SOURCE.into()),
            _ => ctx.cancel(),
        }
        let (_, result) = create_project_import_context(&ctx, &project.1, &output, &assets);
        assert!(result.is_err(), "{scenario}");
        assert!(!exists(&output), "{scenario}");
    }
}

// ---- project import ----

#[test]
fn project_import_extraction_links_native_hashes_and_ignores_metadata_paths() {
    let project = project_context_source();
    let output = project.path("Temp/extraction-context");
    let prepared = create_context(&project.1, &output, &[MODEL_SOURCE]).unwrap();
    let store = format!("{}/Library/DataStore", prepared.project);
    fs::create_dir(&store).unwrap();
    let content = b"arbitrary native artifact fixture\0".to_vec();
    let hash = native_hash(&content);
    let mut storage = vec![0u8; 32];
    storage[..4].copy_from_slice(&3u32.to_le_bytes());
    storage[24..32].copy_from_slice(&(content.len() as u64).to_le_bytes());
    let record = |value: &[u8]| {
        vec![Record {
            key: hash.to_vec(),
            value: value.to_vec(),
        }]
    };
    let database = lmdb::build(&Tables::from([
        ("StorageInfoTable".to_string(), record(&storage)),
        ("ContentDataTable".to_string(), record(&content)),
    ]))
    .unwrap();
    write(&format!("{store}/uds.db"), database);
    let report = ProbeReport {
        worker_exit: "exit 0".into(),
        imports: vec![ImportEvidence {
            guid: prepared.assets()[0].guid.clone(),
            result_id: "a".repeat(32),
            metadata: ArtifactMetadata {
                files: vec![ArtifactFile {
                    name: "../../escape".into(),
                    hash,
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let (extracted, result) =
        extract_project_import_content(&background(), &output, &prepared, &report);
    result.unwrap();
    let file = &extracted.assets[0].files[0];
    assert!(
        extracted.source_unchanged
            && file.name == "../../escape"
            && within_directory(&file.path, &format!("{output}/artifacts"))
            && file.proof.content_key_verified
            && file.sha256 == sha256(&content)
            && file.bytes == content.len() as u64,
        "{extracted:?}"
    );
    assert!(!exists(&project.path("escape")));
}

#[test]
fn project_import_extraction_rejects_incomplete_shutdown() {
    let prepared = ProjectImportContext {
        assets: Some(vec![ProjectImportSource {
            path: MODEL_SOURCE.into(),
            guid: String::new(),
        }]),
        ..Default::default()
    };
    for report in [
        ProbeReport {
            worker_exit: "signal: killed".into(),
            imports: vec![ImportEvidence {
                guid: "guid".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ProbeReport {
            worker_exit: "exit 0".into(),
            ..Default::default()
        },
    ] {
        let output = Temp::new();
        let (_, result) =
            extract_project_import_content(&background(), &output.1, &prepared, &report);
        assert!(result.is_err());
    }
}

// ---- native import ----

fn write_native_manifest(root: &str, files: &[NativeContentFile]) -> String {
    let filename = format!("{root}/scene.json");
    let data = crate::util::marshal(&BTreeMap::from([("nativeContentFiles", files)])).unwrap();
    write(&filename, data);
    filename
}
fn write_native_resource(root: &str, name: &str, contents: &[u8]) -> NativeContentResource {
    let filename = format!("{root}/{name}");
    write(&filename, contents);
    NativeContentResource {
        path: filename,
        virtual_path: format!("VirtualArtifacts/Primary/{TEST_GUID}.resource"),
        sha256: sha256(contents),
    }
}
fn resource_file(guid: &str, resources: Vec<NativeContentResource>) -> NativeContentFile {
    NativeContentFile {
        guid: guid.into(),
        resources: resources.into(),
        ..Default::default()
    }
}

#[test]
fn native_resource_mount_uses_exact_runtime_alias_and_replaces_own_file() {
    let root = Temp::new();
    let run = root.path("run");
    let first = [0u8, 1, 0, 2, 0xff, 0x7f];
    let resource = write_native_resource(&root.1, "native.resource", &first);
    let manifest =
        write_native_manifest(&root.1, &[resource_file(TEST_GUID, vec![resource.clone()])]);
    stage_native_resources(&manifest, &run).unwrap();
    let destination = format!("{run}/{}", resource.virtual_path);
    assert_eq!(fs::read(&destination).unwrap(), first);
    assert_eq!(fs::read(&resource.path).unwrap(), first);
    let info = fs::metadata(&destination).unwrap();
    assert!(info.is_file() && info.permissions().mode() & 0o077 == 0);
    let second = [0x52u8, 0x49, 0x46, 0x46, 0, 0, 0, 0];
    let resource = write_native_resource(&root.1, "native-next.resource", &second);
    let manifest = write_native_manifest(&root.1, &[resource_file(TEST_GUID, vec![resource])]);
    stage_native_resources(&manifest, &run).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), second);
    assert_eq!(fs::read(root.path("native.resource")).unwrap(), first);
    let entries: Vec<_> = fs::read_dir(Path::new(&destination).parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries, [format!("{TEST_GUID}.resource")]);
}

#[test]
fn native_resource_hash_mismatch_keeps_previous_mount() {
    let root = Temp::new();
    let run = root.path("run");
    let original = b"validated immutable streaming audio bytes";
    let resource = write_native_resource(&root.1, "native.resource", original);
    let manifest =
        write_native_manifest(&root.1, &[resource_file(TEST_GUID, vec![resource.clone()])]);
    stage_native_resources(&manifest, &run).unwrap();
    write(&resource.path, "artifact corrupted after validation");
    assert!(stage_native_resources(&manifest, &run).is_err());
    assert_eq!(
        fs::read(format!("{run}/{}", resource.virtual_path)).unwrap(),
        original
    );
}

#[test]
fn native_resource_rejects_alias_outside_guid() {
    for alias in [
        "other.resource".to_string(),
        "VirtualArtifacts/Primary/other.resource".to_string(),
        "../escaped.resource".to_string(),
        format!("VirtualArtifacts/Primary/{TEST_GUID}.resource/../escaped.resource"),
    ] {
        let root = Temp::new();
        let mut resource = write_native_resource(&root.1, "native.resource", b"native resource");
        resource.virtual_path = alias.clone();
        let manifest = write_native_manifest(&root.1, &[resource_file(TEST_GUID, vec![resource])]);
        let run = root.path("run");
        assert!(stage_native_resources(&manifest, &run).is_err(), "{alias}");
        assert!(!exists(&run), "{alias}");
    }
}

#[test]
fn native_resource_rejects_guid_traversal() {
    let root = Temp::new();
    let run = root.path("run");
    let mut resource = write_native_resource(&root.1, "native.resource", b"native resource");
    let guid = "../../../escaped";
    resource.virtual_path = format!("VirtualArtifacts/Primary/{guid}.resource");
    let manifest = write_native_manifest(&root.1, &[resource_file(guid, vec![resource])]);
    assert!(stage_native_resources(&manifest, &run).is_err());
    assert!(!exists(&root.path("escaped.resource")));
}

#[test]
fn native_resource_rejects_symlink_mount_parents() {
    for parent in ["run", "VirtualArtifacts", "Primary"] {
        let root = Temp::new();
        let run = root.path("run");
        let outside = root.path("outside");
        fs::create_dir(&outside).unwrap();
        let link = match parent {
            "VirtualArtifacts" => format!("{run}/{parent}"),
            "Primary" => format!("{run}/VirtualArtifacts/{parent}"),
            _ => run.clone(),
        };
        fs::create_dir_all(Path::new(&link).parent().unwrap()).unwrap();
        symlink(&outside, &link).unwrap();
        let resource = write_native_resource(&root.1, "native.resource", b"native resource");
        let manifest = write_native_manifest(&root.1, &[resource_file(TEST_GUID, vec![resource])]);
        assert!(stage_native_resources(&manifest, &run).is_err(), "{parent}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "{parent}");
    }
}

#[test]
fn native_resource_rejects_conflicting_shared_alias_before_mounting() {
    let root = Temp::new();
    let run = root.path("run");
    let first = write_native_resource(&root.1, "first.resource", b"first native descriptor bytes");
    let second = write_native_resource(
        &root.1,
        "second.resource",
        b"conflicting native descriptor bytes",
    );
    let manifest = write_native_manifest(
        &root.1,
        &[
            resource_file(TEST_GUID, vec![first.clone()]),
            resource_file(TEST_GUID, vec![second]),
        ],
    );
    assert!(stage_native_resources(&manifest, &run).is_err());
    assert!(!exists(&format!("{run}/{}", first.virtual_path)));
}

/// Cache replay through the real Session validator. Its runtime files are
/// immutable byte fixtures; no native importer or Player is invoked.
struct Replay {
    _root: Temp,
    opts: NativeOptions,
    session: Arc<Session>,
    entries: Vec<NativeReplacement>,
}
fn native_replay_fixture() -> Replay {
    let root = Temp::new();
    let project = root.1.clone();
    let output = root.path("Temp/native-replay");
    fs::create_dir_all(format!("{output}/native")).unwrap();
    write(
        &root.path("ProjectSettings/ProjectSettings.asset"),
        WORKER_SETTINGS,
    );
    write(
        &root.path("ProjectSettings/ProjectVersion.txt"),
        format!("m_EditorVersion: {ENGINE_VERSION}\n"),
    );
    let environment = native_environment_fingerprint(&project, "").unwrap();
    let scene = "--- !u!1 &10\nGameObject:\n  m_Component:\n  - component: {fileID: 11}\n  m_Name: Cache proof\n  m_IsActive: 1\n--- !u!4 &11\nTransform:\n  m_GameObject: {fileID: 10}\n  m_LocalPosition: {x: 0, y: 0, z: 0}\n  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}\n  m_LocalScale: {x: 1, y: 1, z: 1}\n  m_Father: {fileID: 0}\n  m_Children: []\n";
    write(&root.path("Assets/Scene.unity"), scene);
    write(
        &root.path("Assets/Scene.unity.meta"),
        "guid: 99999999999999999999999999999999\n",
    );
    let (mut assets, mut dependencies, mut entries) = (Vec::new(), Vec::new(), Vec::new());
    for (index, guid) in [
        "11111111111111111111111111111111",
        "22222222222222222222222222222222",
        "33333333333333333333333333333333",
    ]
    .iter()
    .enumerate()
    {
        let logical = format!("Assets/Audio{}.wav", char::from(b'A' + index as u8));
        let source = format!("source fixture {guid}");
        let meta = format!("guid: {guid}\n");
        write(&root.path(&logical), &source);
        write(&root.path(&format!("{logical}.meta")), &meta);
        let runtime = format!("{output}/native/{guid}.runtime.bin");
        let contents = format!("adapted fixture {guid}");
        write(&runtime, &contents);
        assets.push(serde_json::json!({"kind": "imported", "guid": guid, "fileId": "8300000", "typeName": "UnityEngine.AudioClip", "path": logical, "classId": 83}));
        dependencies.push(serde_json::json!({"path": logical, "sha256": sha256(source.as_bytes()), "bytes": source.len()}));
        dependencies.push(serde_json::json!({"path": format!("{logical}.meta"), "sha256": sha256(meta.as_bytes()), "bytes": meta.len()}));
        entries.push(NativeReplacement {
            guid: guid.to_string(),
            source_path: logical,
            source_sha256: sha256(source.as_bytes()),
            meta_sha256: sha256(meta.as_bytes()),
            environment_sha256: environment.clone(),
            content: NativeContentFile {
                guid: guid.to_string(),
                path: runtime,
                sha256: sha256(contents.as_bytes()),
                objects: vec![NativeContentObject {
                    file_id: "8300000".into(),
                    type_name: "UnityEngine.AudioClip".into(),
                }]
                .into(),
                ..Default::default()
            },
            ..Default::default()
        });
    }
    let baseline = format!("{output}/baseline.json");
    atomic_json(
        &baseline,
        &serde_json::json!({"schemaVersion": 1, "supported": true, "unityVersion": "6000.6.3f1", "projectRoot": project, "scenePath": "Assets/Scene.unity", "objects": [], "assets": assets, "sourceDependencies": dependencies}),
    )
    .unwrap();
    let session = Session::new(Options {
        project: project.clone(),
        manifest: baseline,
        ..Default::default()
    })
    .unwrap();
    Replay {
        _root: root,
        opts: NativeOptions {
            project,
            output,
            editor: String::new(),
            player: String::new(),
        },
        session: Arc::new(session),
        entries,
    }
}
fn dependency(guid: &str) -> NativeContentDependency {
    NativeContentDependency {
        guid: guid.into(),
        global: false,
    }
}
fn index_path(opts: &NativeOptions) -> String {
    format!("{}/native/index.json", opts.output)
}

#[test]
fn native_index_replay_loads_dependencies_before_guid_sorted_dependent() {
    let Replay {
        _root,
        opts,
        session,
        mut entries,
    } = native_replay_fixture();
    // Persistent index uses lexical GUID order, which places A before its B/C
    // dependencies. A also lists a native built-in that needs no cached source.
    entries[0].content.dependencies = vec![
        dependency(&entries[1].guid),
        dependency(&entries[2].guid),
        NativeContentDependency {
            guid: "0000000000000000f000000000000000".into(),
            global: true,
        },
    ]
    .into();
    entries[1].content.dependencies = vec![dependency(&entries[2].guid)].into();
    let index = index_path(&opts);
    atomic_json(&index, &entries).unwrap();
    let original = fs::read(&index).unwrap();
    let importer = NativeProjectImporter::new(opts.clone(), session.clone()).unwrap();
    assert_eq!(importer.entries.len(), 3);
    let prepared = session.prepare(&opts.output).unwrap();
    #[derive(serde::Deserialize)]
    struct Published {
        #[serde(rename = "nativeContentFiles")]
        files: Vec<NativeContentFile>,
    }
    let published: Published =
        serde_json::from_slice(&fs::read(&prepared.manifest).unwrap()).unwrap();
    assert_eq!(published.files.len(), 3);
    for (file, entry) in published.files.iter().zip(&entries) {
        assert!(file.guid == entry.guid && file.sha256 == entry.content.sha256);
    }
    assert_eq!(
        published.files[0].dependencies,
        entries[0].content.dependencies
    );
    assert_eq!(original, fs::read(&index).unwrap());
}

#[test]
fn native_index_replay_rejects_dependency_cycles_and_missing_source_revision() {
    for kind in ["cycle", "self cycle", "missing", "stale dependency"] {
        let Replay {
            _root,
            opts,
            session,
            mut entries,
        } = native_replay_fixture();
        let want = match kind {
            "cycle" => {
                entries.truncate(2);
                entries[0].content.dependencies = vec![dependency(&entries[1].guid)].into();
                entries[1].content.dependencies = vec![dependency(&entries[0].guid)].into();
                "dependency cycle"
            }
            "self cycle" => {
                entries.truncate(1);
                entries[0].content.dependencies = vec![dependency(&entries[0].guid)].into();
                "dependency cycle"
            }
            "missing" => {
                entries.truncate(1);
                entries[0].content.dependencies =
                    vec![dependency("77777777777777777777777777777777")].into();
                "no current source revision"
            }
            _ => {
                entries.truncate(2);
                entries[0].content.dependencies = vec![dependency(&entries[1].guid)].into();
                write(
                    &format!("{}/{}", opts.project, entries[1].source_path),
                    "source changed since its cached import",
                );
                "no current source revision"
            }
        };
        atomic_json(&index_path(&opts), &entries).unwrap();
        let error = NativeProjectImporter::new(opts, session)
            .err()
            .unwrap_or_else(|| panic!("{kind} accepted"));
        assert!(text(&error).contains(want), "{kind}: {error:#}");
    }
}

#[test]
fn native_index_replay_invalidates_legacy_and_changed_environment() {
    for kind in [
        "legacy",
        "color space",
        "shader precision",
        "graphics API",
        "different engine path",
    ] {
        let Replay {
            _root,
            mut opts,
            session,
            mut entries,
        } = native_replay_fixture();
        let mut source = WORKER_SETTINGS.to_string();
        match kind {
            "legacy" => entries
                .iter_mut()
                .for_each(|entry| entry.environment_sha256.clear()),
            "color space" => {
                source = source.replacen("m_ActiveColorSpace: 1", "m_ActiveColorSpace: 0", 1)
            }
            "shader precision" => {
                source = source.replacen("shaderPrecisionModel: 0", "shaderPrecisionModel: 1", 1)
            }
            "graphics API" => {
                source = source.replacen(
                    "m_BuildTargetGraphicsAPIs: []",
                    "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 15000000i\n    m_Automatic: 0",
                    1,
                )
            }
            _ => opts.editor = "/a/different/pinned/Unity".into(),
        }
        write(
            &format!("{}/ProjectSettings/ProjectSettings.asset", opts.project),
            source,
        );
        let index = index_path(&opts);
        atomic_json(&index, &entries).unwrap();
        let original = fs::read(&index).unwrap();
        let importer = NativeProjectImporter::new(opts, session).unwrap();
        assert!(importer.entries.is_empty(), "{kind}");
        assert_eq!(original, fs::read(&index).unwrap(), "{kind}");
    }
}

#[test]
fn native_prepare_rejects_environment_mutation_during_live_session() {
    let Replay {
        _root,
        opts,
        session,
        entries,
    } = native_replay_fixture();
    atomic_json(&index_path(&opts), &entries).unwrap();
    let mut importer = NativeProjectImporter::new(opts.clone(), session).unwrap();
    importer.prepare(&background()).unwrap();
    write(
        &format!("{}/ProjectSettings/ProjectSettings.asset", opts.project),
        WORKER_SETTINGS.replacen("m_ActiveColorSpace: 1", "m_ActiveColorSpace: 0", 1),
    );
    let error = importer.prepare(&background()).unwrap_err();
    assert!(text(&error).contains("environment changed"), "{error:#}");
}

// ---- native adoption ----

#[test]
fn native_adoption_requires_imported_source_metadata_and_guid_revision() {
    let Replay {
        _root,
        opts,
        entries,
        ..
    } = native_replay_fixture();
    let environment = entries[0].environment_sha256.clone();
    let mut assets = Vec::new();
    let mut hashes = BTreeMap::new();
    for replacement in &entries {
        let physical =
            resolve_project_import_source(&opts.project, &replacement.source_path).unwrap();
        assets.push(ProjectImportSource {
            path: replacement.source_path.clone(),
            guid: replacement.guid.clone(),
        });
        hashes.insert(format!("{physical}.meta"), replacement.meta_sha256.clone());
        hashes.insert(physical, replacement.source_sha256.clone());
    }
    let mut prepared = ProjectImportContext {
        assets: Some(assets),
        source_hashes: Some(hashes),
        ..Default::default()
    };
    let inputs = adoption_inputs(&opts, &environment, &prepared).unwrap();
    assert_eq!(inputs.len(), entries.len());
    let physical = format!("{}/{}", opts.project, entries[0].source_path);
    for filename in [physical.clone(), format!("{physical}.meta")] {
        let hashes = prepared.source_hashes.as_mut().unwrap();
        let original = hashes.insert(filename.clone(), "0".repeat(64)).unwrap();
        assert!(
            adoption_inputs(&opts, &environment, &prepared).is_err(),
            "{filename}"
        );
        prepared
            .source_hashes
            .as_mut()
            .unwrap()
            .insert(filename, original);
    }
    let assets = prepared.assets.as_mut().unwrap();
    assets[0].guid = "f".repeat(32);
    assert!(adoption_inputs(&opts, &environment, &prepared).is_err());
    let assets = prepared.assets.as_mut().unwrap();
    assets[0].guid = entries[0].guid.clone();
    let repeated = assets[0].clone();
    assets.push(repeated);
    assert!(adoption_inputs(&opts, &environment, &prepared).is_err());
}

#[test]
fn native_adoption_rejects_wrong_worker_environment_before_publishing() {
    let Replay {
        _root,
        opts,
        session,
        entries,
    } = native_replay_fixture();
    let mut importer = NativeProjectImporter {
        opts: opts.clone(),
        session,
        entries: BTreeMap::new(),
        environment_sha256: entries[0].environment_sha256.clone(),
        pending_native_paths: Vec::new(),
        script_assemblies: BTreeMap::new(),
        behaviour_layouts: BTreeMap::new(),
        behaviour_code: Default::default(),
        model_clips: BTreeMap::new(),
        additive_references: BTreeMap::new(),
        atlas_batch: BTreeMap::new(),
        atlas_identity: BTreeMap::new(),
    };
    let environment = read_worker_environment(&opts.project).unwrap();
    for name in [
        "precision",
        "textureOverrides",
        "workerFailure",
        "editorStarted",
    ] {
        let mut copy = environment.clone();
        let mut report = ProbeReport {
            worker_exit: "exit 0".into(),
            success: true,
            ..Default::default()
        };
        match name {
            "precision" => copy.shader_precision_model ^= 1,
            "textureOverrides" => copy.texture_max_size_override = 1024,
            "workerFailure" => report.worker_exit = "exit 1".into(),
            _ => report.main_editor_started = true,
        }
        report.environment = Some(copy);
        let error = importer
            .adopt_cold_report(
                &background(),
                Some(&report),
                &opts.output,
                "result.json",
                BTreeMap::new(),
                &[],
            )
            .unwrap_err();
        assert!(
            text(&error).contains("current standalone worker environment")
                && importer.entries.is_empty(),
            "{name}: {error:#}"
        );
    }
}

#[test]
fn native_adoption_rejects_legacy_report_environment_and_symlink_artifacts() {
    let project = Temp::new();
    let root = project.path("Temp/import");
    fs::create_dir_all(&root).unwrap();
    let report_path = format!("{root}/result.json");
    let keys = [
        "textureMaxSizeOverride",
        "textureCompressionOverride",
        "defaultTextureCompressionFormat",
    ];
    for omitted in keys {
        let environment: BTreeMap<&str, u32> = keys
            .iter()
            .filter(|key| **key != omitted)
            .map(|key| (*key, 0))
            .collect();
        write(
            &report_path,
            serde_json::to_vec(&BTreeMap::from([("environment", environment)])).unwrap(),
        );
        let error = read_cold_adoption_report(&background(), &project.1, &report_path).unwrap_err();
        assert!(text(&error).contains("complete v2"), "{omitted}: {error:#}");
    }
    let alias = format!("{root}/alias.json");
    symlink(&report_path, &alias).unwrap();
    assert!(validate_adoption_file(&alias, &root, MAX_ARTIFACT_BYTES as i64).is_err());
    assert!(
        validate_adoption_file(
            &report_path,
            &format!("{root}/artifacts"),
            MAX_ARTIFACT_BYTES as i64
        )
        .is_err()
    );
}

/// Replays actual stopped-worker UDS content; no native process is started.
#[test]
fn native_adoption_actual_stopped_batch_proves_every_artifact() {
    let (Ok(project), Ok(report_path)) = (
        std::env::var("UNITY_WORKER_UI_PROJECT"),
        std::env::var("UNITY_WORKER_BATCH_IMPORT_REPORT"),
    ) else {
        eprintln!(
            "skipped: set UNITY_WORKER_UI_PROJECT and UNITY_WORKER_BATCH_IMPORT_REPORT for actual batch artifact evidence"
        );
        return;
    };
    if project.is_empty() || report_path.is_empty() {
        return;
    }
    let (report, prepared, content) =
        read_cold_adoption_report(&background(), &project, &report_path).unwrap();
    assert!(
        content.assets.len() == 176
            && prepared.assets().len() == 176
            && !report.main_editor_started
            && report.worker_exit == "exit 0",
        "unexpected actual batch report: assets={} workerExit={}",
        content.assets.len(),
        report.worker_exit
    );
}

// ---- native batch ----

#[test]
fn native_batch_dependencies_publish_fresh_replacement_before_dependent() {
    let mut first = NativeReplacement {
        guid: "11111111111111111111111111111111".into(),
        source_sha256: "new first".into(),
        ..Default::default()
    };
    let mut second = NativeReplacement {
        guid: "22222222222222222222222222222222".into(),
        source_sha256: "new second".into(),
        ..Default::default()
    };
    first.content.dependencies = vec![
        dependency(&second.guid),
        NativeContentDependency {
            guid: "0000000000000000f000000000000000".into(),
            global: true,
        },
    ]
    .into();
    let loaded = BTreeMap::from([(
        second.guid.clone(),
        NativeReplacement {
            guid: second.guid.clone(),
            source_sha256: "old second".into(),
            ..Default::default()
        },
    )]);
    let ordered = order_native_batch(vec![first.clone(), second.clone()], &loaded).unwrap();
    assert!(
        ordered.len() == 2
            && ordered[0].guid == second.guid
            && ordered[0].source_sha256 == "new second"
            && ordered[1].guid == first.guid
    );
    first.content.dependencies = vec![dependency("33333333333333333333333333333333")].into();
    let error = order_native_batch(vec![first.clone()], &BTreeMap::new()).unwrap_err();
    assert!(text(&error).contains("no imported runtime descriptor"));
    first.content.dependencies = vec![dependency(&second.guid)].into();
    second.content.dependencies = vec![dependency(&first.guid)].into();
    let error = order_native_batch(vec![first, second], &BTreeMap::new()).unwrap_err();
    assert!(text(&error).contains("cycle"));
}

#[test]
fn native_legacy_refresh_candidates_require_exact_source_guid_and_metadata() {
    let Replay {
        _root,
        opts,
        session,
        mut entries,
    } = native_replay_fixture();
    entries
        .iter_mut()
        .for_each(|entry| entry.environment_sha256.clear());
    entries[1].source_sha256 = "0".repeat(64);
    entries[2].meta_sha256 = "0".repeat(64);
    atomic_json(&index_path(&opts), &entries).unwrap();
    let importer = NativeProjectImporter::new(opts, session).unwrap();
    assert_eq!(
        importer.pending_native_paths,
        [entries[0].source_path.clone()]
    );
    assert!(importer.entries.is_empty());
    assert!(
        cold_native_profile("Assets/Model.fbx")
            && cold_native_profile("Assets/Poster.png")
            && cold_native_profile("Assets/Audio.wav")
            && !cold_native_profile("Assets/UI.uxml")
            && !cold_native_profile("Assets/Uncompiled.shader")
            && cold_native_profile("Assets/Photo.JPG")
            && cold_native_profile("Assets/Scan.tiff")
            && !cold_native_profile("Assets/Unverified.dds")
    );
}

// ---- native Mesh CPU ----

#[test]
fn native_mesh_cpu_cache_upgrade_actual_index() {
    let (Ok(index), Ok(project)) = (
        std::env::var("UNITY_NATIVE_MESH_CPU_INDEX"),
        std::env::var("UNITY_MICRO_EDITOR_TEST_PROJECT"),
    ) else {
        eprintln!(
            "skipped: set UNITY_NATIVE_MESH_CPU_INDEX and UNITY_MICRO_EDITOR_TEST_PROJECT for the real native model cache"
        );
        return;
    };
    if index.is_empty() || project.is_empty() {
        return;
    }
    let project = super::support::abs(&project).unwrap();
    let entries: Vec<NativeReplacement> =
        serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    let output = format!(
        "{project}/Temp/UnityPlay/mesh-cpu-proof/{}",
        crate::util::random_id()
    );
    let opts = NativeOptions {
        project,
        output: output.clone(),
        editor: String::new(),
        player: String::new(),
    };
    let (mut models, mut meshes) = (0, 0);
    let mut first: Option<NativeReplacement> = None;
    for entry in entries {
        if !crate::sysutil::ext(&entry.source_path).eq_ignore_ascii_case(".fbx") {
            continue;
        }
        let next = retain_native_model_mesh_cpu(&opts, entry.clone())
            .unwrap_or_else(|error| panic!("{}: {error:#}", entry.source_path));
        assert!(
            next.guid == entry.guid
                && next.source_sha256 == entry.source_sha256
                && next.meta_sha256 == entry.meta_sha256
                && next.environment_sha256 == entry.environment_sha256
                && next.model_graph_sha256 == entry.model_graph_sha256,
            "{}",
            entry.source_path
        );
        let verified = retain_native_model_mesh_cpu(&opts, next.clone()).unwrap();
        assert!(
            verified.content.path == next.content.path
                && verified.content.sha256 == next.content.sha256
        );
        models += 1;
        meshes += next.content.mesh_cpu_retention.len();
        if first.is_none() && next.mesh_cpu_projection.is_some() {
            first = Some(next);
        }
    }
    assert!(models != 0 && meshes != 0);
    // A valid SHA over a modified output must still fail against the original
    // imported bytes; claimed descriptor hashes alone do not prove fidelity.
    if let Some(mut first) = first {
        let mut data = fs::read(&first.content.path).unwrap();
        *data.last_mut().unwrap() ^= 1;
        let tampered = format!("{output}/tampered-runtime.bin");
        write(&tampered, &data);
        first.content.path = tampered;
        first.content.sha256 = sha256(&data);
        assert!(retain_native_model_mesh_cpu(&opts, first).is_err());
    }
    let result = serde_json::json!({"models": models, "meshes": meshes, "mainEditorStarted": false, "workerStarted": false, "output": output});
    write(&format!("{output}/result.json"), result.to_string());
}

#[test]
fn native_mesh_cpu_files_reject_unowned_symlink_and_immutable_conflicts() {
    let project = Temp::new();
    let directory = project.path("Temp/derived");
    fs::create_dir_all(&directory).unwrap();
    let filename = format!("{directory}/mesh.bin");
    write_immutable_native_mesh_file(&filename, b"immutable payload").unwrap();
    write_immutable_native_mesh_file(&filename, b"immutable payload").unwrap();
    assert!(write_immutable_native_mesh_file(&filename, b"changed geometry").is_err());
    read_owned_native_mesh_file(&project.1, &filename).unwrap();
    assert!(read_owned_native_mesh_file(&project.1, &project.path("Assets/mesh.bin")).is_err());
    let link = format!("{directory}/linked.bin");
    symlink(&filename, &link).unwrap();
    assert!(read_owned_native_mesh_file(&project.1, &link).is_err());
    assert!(write_immutable_native_mesh_file(&link, b"immutable payload").is_err());
}

// ---- native UI runtime ----

#[test]
fn native_ui_runtime_sdk() {
    let Some(editor) = std::env::var("UME_LIFECYCLE_SDK")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        eprintln!("skipped: set UME_LIFECYCLE_SDK; only the installed .NET SDK is used");
        return;
    };
    let tool_root = crate::util::micro_editor_root().unwrap();
    let root = Temp::new();
    for name in [
        "NativeUIRuntime.cs",
        "RawSceneManifest.cs",
        "RawJson.cs",
        "RawOwnedObjects.cs",
        "ImportedAssetRuntimePaths.cs",
    ] {
        let source = fs::read(format!(
            "{tool_root}/Packages/com.unitymicroeditor.runtime/Runtime/Raw/{name}"
        ))
        .unwrap();
        write(&root.path(name), source);
    }
    let fixture = fs::read(format!(
        "{}/rust/tests/testdata/cs/NativeUIRuntime.cs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    write(&root.path("Program.cs"), fixture);
    write(
        &root.path("Tests.csproj"),
        r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework><UseAppHost>false</UseAppHost><TreatWarningsAsErrors>true</TreatWarningsAsErrors></PropertyGroup></Project>"#,
    );
    write(
        &root.path("NuGet.Config"),
        r#"<configuration><packageSources><clear /></packageSources></configuration>"#,
    );
    let dotnet = format!("{}/Data/DotNetSdk/dotnet", crate::sysutil::dir(&editor));
    let run = |arguments: &[&str]| {
        let output = std::process::Command::new(&dotnet)
            .args(arguments)
            .current_dir(&root.1)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    // Without build servers: one would outlive the test.
    run(&[
        "build",
        "Tests.csproj",
        "--nologo",
        "-v:q",
        "-p:NuGetAudit=false",
        crate::compiler::BUILD_WITHOUT_SERVERS[0],
        crate::compiler::BUILD_WITHOUT_SERVERS[1],
    ]);
    run(&[&root.path("bin/Debug/net8.0/Tests.dll")]);
}

// ---- native UI ----

const NATIVE_UI_DUMP: &str = "External References\npath(1): \"Library/unity default resources\" GUID: 0000000000000000e000000000000000 Type: 0\npath(2): \"\" GUID: cd5ff7a50bd83450bbe76880cf52d119 Type: 3\n\nID: 1068529831357515114 (ClassID: 114) MonoBehaviour\n\tm_Script  (PPtr<MonoScript>)\n\t\tm_FileID 1 (int)\n\t\tm_PathID 11998 (SInt64)\nID: 9197481963319205126 (ClassID: 114) MonoBehaviour\n\tinlineSheet  (PPtr<$StyleSheet>)\n\t\tm_FileID 0 (int)\n\t\tm_PathID 1068529831357515114 (SInt64)\n\treferences  (ManagedReferencesRegistry)\n\t\tversion 2 (int)\n\t\trid -2 (class: , ns: , asm: )\n\t\trid 1000 (class: VisualElementAsset, ns: UnityEngine.UIElements, asm: UnityEngine.UIElementsModule)\n\t\t\tVisualElementAsset\n\t\t\t\tm_VisualTreeAsset  (PPtr<$VisualTreeAsset>)\n\t\t\t\t\tm_FileID 0 (int)\n\t\t\t\t\tm_PathID 9197481963319205126 (SInt64)\n\t\t\t\tm_Stylesheets  (vector)\n\t\t\t\t\tsize 2 (int)\n\t\t\t\t\tdata  (PPtr<$StyleSheet>)\n\t\t\t\t\t\tm_FileID 2 (int)\n\t\t\t\t\t\tm_PathID 7433441132597879392 (SInt64)\n\t\t\t\t\tdata  (PPtr<$StyleSheet>)\n\t\t\t\t\t\tm_FileID 2 (int)\n\t\t\t\t\t\tm_PathID 7433441132597879392 (SInt64)\n";

fn native_ui_fixture_file() -> NativeContentFile {
    NativeContentFile {
        guid: "87785bfd71a0a425fb905aaf7743f114".into(),
        objects: vec![
            NativeContentObject {
                file_id: "1068529831357515114".into(),
                type_name: "UnityEngine.UIElements.StyleSheet".into(),
            },
            NativeContentObject {
                file_id: "9197481963319205126".into(),
                type_name: "UnityEngine.UIElements.VisualTreeAsset".into(),
            },
        ]
        .into(),
        dependencies: vec![
            NativeContentDependency {
                guid: "0000000000000000e000000000000000".into(),
                global: true,
            },
            dependency("cd5ff7a50bd83450bbe76880cf52d119"),
        ]
        .into(),
        ..Default::default()
    }
}

#[test]
fn native_ui_exact_managed_pptr_slots_and_local_inline_sheet() {
    let file = native_ui_fixture_file();
    let bindings = parse_native_ui_bindings(NATIVE_UI_DUMP.as_bytes(), &file).unwrap();
    assert!(
        bindings.len() == 3
            && bindings[0].property_path == "inlineSheet"
            && bindings[0].target_guid == file.guid
            && bindings[1].property_path == "references.RefIds[0].data.m_Stylesheets[0]"
            && bindings[2].property_path == "references.RefIds[0].data.m_Stylesheets[1]"
            && bindings[1].managed_reference_id == "1000"
            && bindings[1].target_file_id == "7433441132597879392",
        "{bindings:?}"
    );
    let mut bad = native_ui_fixture_file();
    let mut dependencies = bad.dependencies.to_vec();
    dependencies[1].guid = "b54a76024e0db49348ae426002a8a519".into();
    bad.dependencies = dependencies.into();
    assert!(parse_native_ui_bindings(NATIVE_UI_DUMP.as_bytes(), &bad).is_err());
    let mut bad = native_ui_fixture_file();
    let mut objects = bad.objects.to_vec();
    objects[1].file_id = "19".into();
    bad.objects = objects.into();
    assert!(parse_native_ui_bindings(NATIVE_UI_DUMP.as_bytes(), &bad).is_err());
}

// ---- warm native ----

#[test]
fn warm_native_ui_requires_exact_source_metadata_and_pinned_import() {
    for variant in [
        "stale source",
        "stale metadata",
        "wrong GUID",
        "missing current import",
    ] {
        let project = Temp::new();
        let logical = "Assets/UI.uss";
        let source = b".root {color: red;}";
        let meta = b"guid: 11111111111111111111111111111111\n";
        fs::create_dir_all(project.path("Library/DataStore")).unwrap();
        write(&project.path(logical), source);
        write(&project.path(&format!("{logical}.meta")), meta);
        let wire = guid_wire("11111111111111111111111111111111").unwrap();
        let mut mapping = [&wire[..], &native_hash(meta), &native_hash(source)].concat();
        match variant {
            "stale source" => mapping[32] ^= 1,
            "stale metadata" => mapping[16] ^= 1,
            "wrong GUID" => mapping[0] ^= 1,
            _ => {}
        }
        let database = lmdb::build(&Tables::from([(
            "PathToGuid".to_string(),
            vec![Record {
                key: format!("{}\0", logical.to_lowercase()).into_bytes(),
                value: mapping,
            }],
        )]))
        .unwrap();
        write(&project.path("Library/SourceAssetDB"), database);
        let database = lmdb::build(&Tables::from([(
            "CurrentRevisions".to_string(),
            Vec::new(),
        )]))
        .unwrap();
        write(&project.path("Library/ArtifactDB"), &database);
        write(&project.path("Library/DataStore/uds.db"), &database);
        let output = project.path("Temp/runtime.bin");
        assert!(
            extract_warm_native_ui(&background(), &project.1, logical, &output).is_err(),
            "{variant}"
        );
        assert!(!exists(&output), "{variant}");
    }
}

// ---- texture profile ----

const NATIVE_TEXTURE_PROFILE: &str = "guid: 22222222222222222222222222222222\nTextureImporter:\n  textureType: 0\n  textureShape: 1\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    textureCompression: 0\n    textureFormat: -1\n  - buildTarget: Standalone\n    overridden: 0\n    textureCompression: 2\n    textureFormat: -1\n";

#[test]
fn native_texture_profile_preserves_effective_platform_settings_and_compressor_guard() {
    let project = Temp::new();
    let meta = project.path("Assets/Texture.png.meta");
    write(&meta, NATIVE_TEXTURE_PROFILE);
    assert!(!native_texture_needs_warm(&project.1, "Assets/Texture.png").unwrap());
    // A standalone override is followed: high quality compression is
    // imported by a worker, a format of another platform family is not.
    let overridden = NATIVE_TEXTURE_PROFILE.replacen("overridden: 0", "overridden: 1", 1);
    write(&meta, &overridden);
    assert!(!native_texture_needs_warm(&project.1, "Assets/Texture.png").unwrap());
    write(
        &meta,
        overridden.replacen(
            "textureCompression: 2\n    textureFormat: -1",
            "textureCompression: 2\n    textureFormat: 48",
            1,
        ),
    );
    assert!(native_texture_needs_warm(&project.1, "Assets/Texture.png").unwrap());
    let settings = project.path("ProjectSettings/EditorSettings.asset");
    let data = "--- !u!159 &1\nEditorSettings:\n  m_Bc7TextureCompressor: 0\n";
    write(&settings, data);
    let configuration = warm_texture_configuration(&project.1, "Assets/Texture.png").unwrap();
    assert_eq!(
        configuration.get("ProjectSettings/EditorSettings.asset"),
        Some(&sha256(data.as_bytes()))
    );
    validate_native_configuration(&project.1, "Assets/Texture.png", &configuration).unwrap();
    assert!(
        validate_native_configuration(&project.1, "Assets/Texture.png", &BTreeMap::new()).is_err()
    );
    write(&settings, data.replacen(": 0", ": 1", 1));
    assert!(
        validate_native_configuration(&project.1, "Assets/Texture.png", &configuration).is_err()
    );
}

#[test]
fn native_texture_profile_covers_every_texture_source_and_keeps_unverified_settings_warm() {
    let project = Temp::new();
    let profile = |compression: &str, format: &str, crunched: &str| {
        format!(
            "guid: 22222222222222222222222222222222\nTextureImporter:\n  textureType: 0\n  textureShape: 1\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    textureCompression: {compression}\n    textureFormat: {format}\n    crunchedCompression: {crunched}\n"
        )
    };
    for name in [
        "a.png", "a.jpg", "a.JPEG", "a.tga", "a.tif", "a.tiff", "a.bmp", "a.psd", "a.gif", "a.exr",
        "a.hdr",
    ] {
        let logical = format!("Assets/{name}");
        let meta = project.path(&format!("{logical}.meta"));
        // Every compression quality in the automatic format is imported by
        // a standalone worker, crunched or not; high quality selects BC7.
        for (compression, crunched) in [("0", "0"), ("1", "0"), ("2", "0"), ("3", "0"), ("1", "1")]
        {
            write(&meta, profile(compression, "-1", crunched));
            assert!(
                !native_texture_needs_warm(&project.1, &logical).unwrap(),
                "{name} {compression} {crunched}"
            );
        }
        // So is a compared explicit format; ASTC, ETC2 and an unknown
        // quality are not.
        for (compression, format, warm) in [
            ("1", "25", false),
            ("0", "4", false),
            ("1", "74", false),
            ("1", "48", true),
            ("1", "47", true),
            ("4", "-1", true),
        ] {
            write(&meta, profile(compression, format, "0"));
            assert_eq!(
                native_texture_needs_warm(&project.1, &logical).unwrap(),
                warm,
                "{name} {compression} {format}"
            );
        }
        // Sprites, normal maps, cubemaps, arrays and volumes are imported;
        // a type or shape that was not compared is not.
        for (from, to, warm) in [
            ("textureType: 0", "textureType: 8", false),
            ("textureType: 0", "textureType: 1", false),
            ("textureShape: 1", "textureShape: 2", false),
            ("textureShape: 1", "textureShape: 8", false),
            ("textureType: 0", "textureType: 9", true),
            ("textureShape: 1", "textureShape: 16", true),
        ] {
            write(&meta, profile("1", "-1", "0").replacen(from, to, 1));
            assert_eq!(
                native_texture_needs_warm(&project.1, &logical).unwrap(),
                warm,
                "{name} {to}"
            );
        }
        // Without importer settings the engine makes a cubemap of the file.
        write(
            &meta,
            "fileFormatVersion: 2\nguid: 22222222222222222222222222222222\n",
        );
        let error = native_texture_needs_warm(&project.1, &logical).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("holds no TextureImporter settings")
        );
    }
    // Other sources are not asked for texture metadata.
    for name in ["a.dds", "a.wav", "a.fbx", "a.uxml"] {
        assert!(!native_texture_needs_warm(&project.1, &format!("Assets/{name}")).unwrap());
    }
}

#[test]
fn warm_bc7_historical_dependency_prevents_restamping_old_compressed_artifact() {
    let project = Temp::new();
    let settings = project.path("ProjectSettings/EditorSettings.asset");
    let write_settings = |value: &str| {
        write(
            &settings,
            format!("--- !u!159 &1\nEditorSettings:\n  m_Bc7TextureCompressor: {value}\n"),
        )
    };
    write_settings("0");
    let mut data = vec![0u8; 224];
    let key = b"graphics/bc7-compressor\0";
    data[176..176 + key.len()].copy_from_slice(key);
    data[128..132].copy_from_slice(&(176u32 - 128).to_le_bytes());
    (data[136], data[137]) = (1, 2);
    verify_warm_bc7_dependency(&project.1, &data).unwrap();
    write_settings("1");
    assert!(verify_warm_bc7_dependency(&project.1, &data).is_err());
    write_settings("0");
    data[128..132].copy_from_slice(&0u32.to_le_bytes());
    assert!(recorded_bc7_dependency(&data).is_err());
    data[128..132].copy_from_slice(&(176u32 - 128).to_le_bytes());
    data[152..156].copy_from_slice(&(176u32 - 152).to_le_bytes());
    (data[160], data[161]) = (1, 2);
    assert!(recorded_bc7_dependency(&data).is_err());
}

// ---- built-in Shader ----

#[test]
fn builtin_shader_snapshot_rejects_native_payload_substitution() {
    let Some(path) = std::env::var("UNITY_PLAY_BUILTIN_SHADER_SNAPSHOT")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        eprintln!("skipped: set a real built-in Shader snapshot; SDK/native files are only read");
        return;
    };
    let good: RetainedBuiltinShader = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    verify_builtin_shader_snapshot(&good).unwrap();
    let corruptions: [fn(&mut RetainedBuiltinShader); 4] = [
        |v| v.archive_file_id = "1".into(),
        |v| v.payload_sha256 = "0".repeat(64),
        |v| v.member_name = "other-member".into(),
        |v| v.builtin_source_sha256 = "0".repeat(64),
    ];
    for corrupt in corruptions {
        let mut value = good.clone();
        corrupt(&mut value);
        assert!(verify_builtin_shader_snapshot(&value).is_err());
    }
    let mut bytes = fs::read(&good.content.path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    let root = Temp::new();
    let changed = root.path("changed.bin");
    write(&changed, &bytes);
    let mut value = good.clone();
    value.content.path = changed;
    value.content.sha256 = sha256(&bytes);
    assert!(verify_builtin_shader_snapshot(&value).is_err());
}

// ---- dependent imports ----

/// The key and the records are those the pinned Editor wrote for a sprite
/// texture of the comparison project.
#[test]
fn import_results_become_the_current_revisions_of_the_context() {
    let project = Temp::new();
    let database = project.path("Library/ArtifactDB");
    let names = [
        "CurrentRevisions",
        "DBVersion",
        "ImportAddressToImportResultIDs",
        "ImportResultIDPropertyIDToProperty",
        "ImportResultIDToArtifactDependencies",
        "ImportResultIDToArtifactMetaInfo",
        "ImportResultIDToImportStats",
    ];
    let mut tables: Tables = names
        .iter()
        .map(|name| (name.to_string(), Vec::new()))
        .collect();
    tables.get_mut("DBVersion").unwrap().push(Record {
        key: vec![0; 8],
        value: vec![9, 0, 0, 0],
    });
    write(&database, lmdb::build(&tables).unwrap());
    let address = import_address("4c6c282c6d34326599b20a88ae7486cf", -1, [0; 16]).unwrap();
    let record = |result: u8| ImportRecord {
        address,
        result: [result; 16],
        blobs: vec![vec![1, result], vec![2, result], vec![3, result]],
    };
    commit_import_results(&project.1, &[record(7)]).unwrap();
    // A later result of the same address replaces the revision.
    commit_import_results(&project.1, &[record(8)]).unwrap();
    let data = fs::read(&database).unwrap();
    let tables = lmdb::Image::open(&data).unwrap().tables().unwrap();
    assert_eq!(tables.keys().map(String::as_str).collect::<Vec<_>>(), names);
    assert_eq!(tables["DBVersion"][0].value, [9, 0, 0, 0]);
    let key = [
        0xad, 0x6f, 0xa5, 0xdb, 0xef, 0x8e, 0x71, 0x7e, 0xd1, 0xf4, 0x3a, 0x85, 0x19, 0xaf, 0x25,
        0xd2,
    ];
    let current = &tables["CurrentRevisions"];
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].key, key);
    assert_eq!(current[0].value.len(), 56);
    assert_eq!(current[0].value[..40], address[..40]);
    assert_eq!(current[0].value[40..], [8; 16]);
    let history = &tables["ImportAddressToImportResultIDs"];
    assert_eq!((history.len(), &history[0].key[..]), (1, &key[..]));
    assert_eq!(history[0].value[40..48], [8, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(history[0].value[48..], [8; 16]);
    for (name, tag) in [
        ("ImportResultIDToArtifactDependencies", 1),
        ("ImportResultIDToArtifactMetaInfo", 2),
        ("ImportResultIDToImportStats", 3),
    ] {
        let records = &tables[name];
        assert_eq!(records.len(), 2, "{name}");
        assert!(
            records
                .iter()
                .any(|record| record.key == [8; 16] && record.value == [tag, 8]),
            "{name}"
        );
    }
    // A database of another layout is not written to.
    let mut other = tables.clone();
    other.remove("ImportResultIDToImportStats");
    write(&database, lmdb::build(&other).unwrap());
    assert!(commit_import_results(&project.1, &[record(9)]).is_err());
}

#[test]
fn sprite_atlas_sources_list_their_textures_folders_included() {
    let project = Temp::new();
    let meta = |guid: &str| format!("fileFormatVersion: 2\nguid: {guid}\n");
    let guids = [
        ("Assets/Art/a.png", "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"),
        ("Assets/Art/Folder", "f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0"),
        (
            "Assets/Art/Folder/b.png",
            "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
        ),
        (
            "Assets/Art/Folder/Deep/c.tga",
            "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
        ),
        (
            "Assets/Art/Folder/notes.txt",
            "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4",
        ),
        (
            "Assets/Art/unpacked.png",
            "e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5",
        ),
        (
            "Assets/Main.spriteatlasv2",
            "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a",
        ),
        (
            "Assets/Variant.spriteatlasv2",
            "2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b",
        ),
    ];
    for (path, guid) in guids {
        if !path.ends_with("Folder") {
            write(&project.path(path), path);
        }
        write(&project.path(&format!("{path}.meta")), meta(guid));
    }
    // A hidden file and one the Editor ignores are no members.
    write(&project.path("Assets/Art/Folder/.hidden.png"), "x");
    write(&project.path("Assets/Art/Folder/old.png~"), "x");
    let atlas = |packables: &str, variant: u8| {
        format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!612988286 &1\nSpriteAtlasAsset:\n  serializedVersion: 2\n  m_MasterAtlas: {{fileID: 0}}\n  m_ImporterData:\n    packables:\n{packables}  m_IsVariant: {variant}\n  m_ScriptablePacker: {{fileID: 0}}\n"
        )
    };
    // A texture, a sprite of the same texture, a folder and a missing asset.
    write(
        &project.path("Assets/Main.spriteatlasv2"),
        atlas(
            "    - {fileID: 2800000, guid: a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, type: 3}\n    - {fileID: 21300000, guid: a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1, type: 3}\n    - {fileID: 102900000, guid: f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0, type: 3}\n    - {fileID: 2800000, guid: 99999999999999999999999999999999, type: 3}\n",
            0,
        ),
    );
    write(
        &project.path("Assets/Variant.spriteatlasv2"),
        atlas(
            "    - {fileID: 2800000, guid: e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5, type: 3}\n",
            1,
        ),
    );
    let mut path_of = |guid: &str| {
        guids
            .iter()
            .find(|(_, known)| *known == guid)
            .map_or(String::new(), |(path, _)| path.to_string())
    };
    let atlases = super::atlas::read_atlas_sources(&project.1, &mut path_of).unwrap();
    assert_eq!(atlases.len(), 2);
    let main = &atlases[0];
    assert_eq!(main.revision.path, "Assets/Main.spriteatlasv2");
    assert_eq!(main.revision.guid, "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a");
    assert_eq!(main.refusal, "");
    assert_eq!(
        main.members
            .iter()
            .map(|member| member.path.as_str())
            .collect::<Vec<_>>(),
        [
            "Assets/Art/Folder/Deep/c.tga",
            "Assets/Art/Folder/b.png",
            "Assets/Art/a.png"
        ]
    );
    assert_eq!(
        main.batch().last().map(String::as_str),
        Some("Assets/Main.spriteatlasv2")
    );
    let ingredients = main.ingredients();
    assert_eq!(ingredients.len(), 6);
    assert_eq!(ingredients["Assets/Art/a.png"], sha256(b"Assets/Art/a.png"));
    assert_eq!(
        ingredients["Assets/Art/a.png.meta"],
        sha256(meta("a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1").as_bytes())
    );
    assert_eq!(atlases[1].refusal, "a variant atlas is not supported");
    // An atlas whose sprites a build leaves for the game to bind.
    write(
        &project.path("Assets/Main.spriteatlasv2.meta"),
        format!(
            "{}SpriteAtlasImporter:\n  bindAsDefault: 0\n",
            meta("1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a")
        ),
    );
    let atlases = super::atlas::read_atlas_sources(&project.1, &mut path_of).unwrap();
    assert_eq!(
        atlases[0].refusal,
        "an atlas that is not included in the build is not supported"
    );
}

/// One import response as a worker sends it, with the three artifact
/// records in their blob arrays.
fn import_response_fixture(address: &[u8; 44], id: &[u8; 16], meta: &[u8]) -> Vec<u8> {
    let mut dependencies = [0; 72];
    dependencies[40..56].copy_from_slice(id);
    let stats = [7; 24];
    let mut data = vec![0; 128];
    data[0] = 20;
    data[8..52].copy_from_slice(address);
    data[56..72].copy_from_slice(id);
    for (field, blob) in [(88, &dependencies[..]), (96, meta), (104, &stats[..])] {
        let start = data.len();
        crate::protocol::put_u32(&mut data, field, (start - field) as u32);
        crate::protocol::put_u32(&mut data, field + 4, blob.len() as u32);
        data.extend_from_slice(blob);
    }
    data
}

#[test]
fn artifact_database_records_follow_the_editor_layout() {
    // A record of an Editor-written ArtifactDB: the key of this address.
    let mut address = [0u8; 44];
    address[..16]
        .copy_from_slice(&environment::decode_hex("23b5a128e3538d44878e69968f80ce9b").unwrap());
    address[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        crate::hex(&artifactdb::address_key(&address)),
        "0004e6b046dc126463243521ce412f38"
    );

    // An ArtifactMetaInfo with one property, "Kind" = [1, 2, 3].
    let mut meta = vec![0; 112];
    crate::protocol::put_u32(&mut meta, 96, 16);
    crate::protocol::put_u32(&mut meta, 100, 1);
    meta.extend_from_slice(&[0; 12]);
    let entry = 112;
    let name = meta.len();
    crate::protocol::put_u32(&mut meta, entry, (name - entry) as u32);
    meta.extend_from_slice(b"Kind\0");
    let value = meta.len();
    crate::protocol::put_u32(&mut meta, entry + 4, (value - entry - 4) as u32);
    crate::protocol::put_u32(&mut meta, entry + 8, 3);
    meta.extend_from_slice(&[1, 2, 3]);

    let mut tables = Tables::new();
    for id in [[0x11u8; 16], [0x22; 16], [0x11; 16]] {
        let frame = import_response_fixture(&address, &id, &meta);
        let response = crate::protocol::import_response(&frame).unwrap();
        artifactdb::record(&mut tables, &response).unwrap();
    }
    let key = artifactdb::address_key(&address).to_vec();
    let only = |table: &str| {
        let records = &tables[table];
        assert_eq!(
            records.iter().filter(|r| r.key == key).count(),
            1,
            "{table}"
        );
        records.iter().find(|r| r.key == key).unwrap().value.clone()
    };
    // The current revision is the last result; the list leads with it and
    // keeps the other one known for the address.
    let revision = only("CurrentRevisions");
    assert_eq!(revision.len(), 56);
    assert_eq!(revision[..40], address[..40]);
    assert_eq!(revision[40..], [0x11; 16]);
    let list = only("ImportAddressToImportResultIDs");
    assert_eq!(list[..40], address[..40]);
    assert_eq!(
        blob_array(&list, 40, 16).unwrap(),
        (&[[0x11u8; 16], [0x22; 16]].concat()[..], 2)
    );
    for table in [
        "ImportResultIDToArtifactDependencies",
        "ImportResultIDToArtifactMetaInfo",
        "ImportResultIDToImportStats",
    ] {
        assert_eq!(tables[table].len(), 2, "{table}");
    }
    assert_eq!(
        tables["ImportResultIDToArtifactMetaInfo"]
            .iter()
            .find(|r| r.key == [0x22; 16])
            .unwrap()
            .value,
        meta
    );
    let property = [&b"Kind"[..], &[0x11; 16]].concat();
    assert!(
        tables["ImportResultIDPropertyIDToProperty"]
            .iter()
            .any(|r| r.key == property && r.value == [1, 2, 3])
    );
}

#[test]
fn live_commit_publishes_revisions_without_touching_the_current_one() {
    let temp = Temp::new();
    let path = temp.path("ArtifactDB");
    let table = |count: usize| -> Vec<Record> {
        (0..count)
            .map(|i| Record {
                key: format!("key-{i:05}").into_bytes(),
                value: vec![i as u8; 300 + (i % 7) * 900],
            })
            .collect()
    };
    let initial = Tables::from([
        ("Table".to_string(), table(1)),
        ("Empty".to_string(), vec![]),
    ]);
    write(&path, lmdb::build(&initial).unwrap());
    // A lock file as native LMDB leaves it: one idle reader slot.
    let lock = format!("{path}-lock");
    let mut header = vec![0u8; 8192];
    header[..4].copy_from_slice(&0xbeefc0deu32.to_le_bytes());
    header[48..56].copy_from_slice(&1u64.to_le_bytes());
    header[56..60].copy_from_slice(&1u32.to_le_bytes());
    header[128..136].copy_from_slice(&u64::MAX.to_le_bytes());
    header[136..140].copy_from_slice(&4242u32.to_le_bytes());
    write(&lock, &header);

    let mut previous = fs::read(&path).unwrap();
    let mut largest = 0;
    for count in 2..40 {
        let transaction = lmdb::commit(&path, |tables| {
            tables.insert("Table".into(), table(count));
            Ok(())
        })
        .unwrap();
        let data = fs::read(&path).unwrap();
        let image = lmdb::Image::open(&data).unwrap();
        assert_eq!(image.table("Table").unwrap().len(), count);
        assert!(image.table("Empty").unwrap().is_empty());
        // The lock file names the new transaction, whose metadata page is
        // the one of its parity.
        let lock_data = fs::read(&lock).unwrap();
        assert_eq!(lock_data[48..56], transaction.to_le_bytes());
        let meta = (transaction & 1) as usize * lmdb::PAGE_SIZE;
        assert_eq!(data[meta + 144..meta + 152], transaction.to_le_bytes());
        // What the previous revision reads is still there.
        let before = lmdb::Image::open(&previous).unwrap().tables().unwrap();
        let mut old = data.clone();
        old[..2 * lmdb::PAGE_SIZE].copy_from_slice(&previous[..2 * lmdb::PAGE_SIZE]);
        assert_eq!(lmdb::Image::open(&old).unwrap().tables().unwrap(), before);
        largest = largest.max(data.len());
        previous = data;
    }
    // Older revisions are reused: the file stays near the database size.
    let database = lmdb::build(&lmdb::Image::open(&previous).unwrap().tables().unwrap())
        .unwrap()
        .len();
    assert!(largest <= 4 * database, "{largest} > 4 * {database}");

    // A reader inside a transaction pins every older page.
    let mut busy = fs::read(&lock).unwrap();
    busy[128..136].copy_from_slice(&3u64.to_le_bytes());
    write(&lock, &busy);
    let size = fs::read(&path).unwrap().len();
    lmdb::commit(&path, |_| Ok(())).unwrap();
    let data = fs::read(&path).unwrap();
    let image = lmdb::Image::open(&data).unwrap();
    assert!(data.len() > size);
    assert_eq!(image.table("Table").unwrap().len(), 39);
    assert_eq!(
        data[..size][2 * lmdb::PAGE_SIZE..],
        previous[2 * lmdb::PAGE_SIZE..]
    );
}

#[test]
fn context_settings_give_an_automatic_linux_list_the_api_of_the_player() {
    use super::environment::context_player_settings;
    let held =
        |source: &str| String::from_utf8(context_player_settings(source.as_bytes())).unwrap();
    let entry =
        "  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 11000000i\n    m_Automatic: 0\n";
    // No list, a list of other targets, and an automatic Linux entry.
    assert_eq!(
        held("PlayerSettings:\n  m_BuildTargetGraphicsAPIs: []\n  next: 1\n"),
        format!("PlayerSettings:\n  m_BuildTargetGraphicsAPIs:\n{entry}  next: 1\n")
    );
    let others = "  m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: WebGLSupport\n    m_APIs: 1c000000i\n    m_Automatic: 0\n";
    assert_eq!(
        held(&format!("PlayerSettings:\n{others}  next: 1\n")),
        format!("PlayerSettings:\n{others}{entry}  next: 1\n")
    );
    assert_eq!(
        held(&format!(
            "PlayerSettings:\n{others}  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 1500000011000000i\n    m_Automatic: 1\n  next: 1\n"
        )),
        format!("PlayerSettings:\n{others}{entry}  next: 1\n")
    );
    // A list of the project's own, and a file without the key, stay.
    for kept in [
        format!(
            "PlayerSettings:\n{others}  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 15000000i\n    m_Automatic: 0\n  next: 1\n"
        ),
        "PlayerSettings:\n  next: 1\n".to_string(),
    ] {
        assert_eq!(held(&kept), kept);
    }
    // What the context holds is read as OpenGLCore alone.
    let environment = parse_worker_environment(held(WORKER_SETTINGS).as_bytes()).unwrap();
    assert_eq!(environment.graphics_api_mask, 1 << 17);
}

#[test]
fn script_register_needs_the_editor_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let editor = temp.path().join("Unity").to_string_lossy().into_owned();
    assert!(super::scripted::script_register(&editor).is_err());
}

#[test]
fn script_register_lists_the_ui_scripts_of_the_installed_editor() {
    // Optional: the Editor at UNITY_EDITOR.
    let Some(editor) = std::env::var("UNITY_EDITOR").ok().filter(|v| !v.is_empty()) else {
        return;
    };
    let register = super::scripted::script_register(&editor).unwrap();
    let text = std::str::from_utf8(&register).unwrap();
    assert!(
        text.starts_with(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!116 &1\nMonoManager:\n  m_ObjectHideFlags: 0\n  serializedVersion: 3\n"
        )
    );
    // VisualTreeAsset and StyleSheet.
    for id in [11995, 11998] {
        assert!(text.contains(&format!(
            "  - {{fileID: {id}, guid: 0000000000000000e000000000000000, type: 0}}\n"
        )));
    }
}

#[test]
fn ui_source_references_follow_guids_and_relative_urls() {
    let known = |path: &str| match path {
        "Packages/p/UI/Window.uss" => Some("aa".repeat(16)),
        "Packages/p/Common.uss" => Some("bb".repeat(16)),
        "Assets/Art/My Icon.png" => Some("cc".repeat(16)),
        _ => None,
    };
    let theme = br#"@import url("unity-theme://default");
@import url("Window.uss");
@import url('../Common.uss');
.a { background-image: url("project://database/Assets/Art/My%20Icon.png?fileID=2800000&type=3#My Icon"); }
.b { background-image: url("/Assets/Art/My Icon.png"); }
.c { -unity-font: url("project://database/Assets/F.ttf?fileID=1&guid=DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD&type=3#F"); }
.d { background-image: url("Missing.png"); }"#;
    let mut expected = vec![
        "aa".repeat(16),
        "bb".repeat(16),
        "cc".repeat(16),
        "dd".repeat(16),
    ];
    expected.sort();
    assert_eq!(
        super::warm::ui_source_references("Packages/p/UI/Theme.tss", theme, &known),
        expected
    );
    // Nothing above the project root.
    assert!(
        super::warm::ui_source_references("A.uss", br#"url("../../x.uss")"#, &known).is_empty()
    );
}

#[test]
fn ui_import_order_puts_each_source_after_what_it_names() {
    // The theme names both sheets; Window.uss names Common.uss, which names
    // Window.uss back.
    let files = |path: &str| -> anyhow::Result<Vec<u8>> {
        Ok(match path {
            "P/Theme.tss" => b"@import url(\"Common.uss\"); @import url(\"Window.uss\");".to_vec(),
            "P/Window.uss" => {
                b"@import url(\"Common.uss\"); .a { background-image: url(\"Icon.png\"); }".to_vec()
            }
            "P/Common.uss" => b"@import url(\"Window.uss\");".to_vec(),
            _ => Vec::new(),
        })
    };
    let guid_of = |path: &str| Some(format!("{:0>32}", path.len() + path.as_bytes()[2] as usize));
    let paths = ["P/Theme.tss", "P/Window.uss", "P/Common.uss", "P/Icon.png"];
    let path_of = |guid: &str| {
        paths
            .iter()
            .find(|path| guid_of(path).as_deref() == Some(guid))
            .map(|path| path.to_string())
    };
    let order =
        super::warm::ui_import_order("P/Theme.tss", &files, &guid_of, &path_of, &|_| true).unwrap();
    let at = |path: &str| order.iter().position(|item| item == path).unwrap();
    assert_eq!(order.len(), 4);
    assert_eq!(at("P/Theme.tss"), 3);
    assert!(at("P/Icon.png") < at("P/Window.uss"));
    assert!(at("P/Window.uss") < at("P/Theme.tss") && at("P/Common.uss") < at("P/Theme.tss"));
    // Not importable: not followed.
    let order = super::warm::ui_import_order("P/Theme.tss", &files, &guid_of, &path_of, &|path| {
        !path.ends_with(".png")
    })
    .unwrap();
    assert!(!order.iter().any(|path| path.ends_with(".png")));
}

#[test]
fn a_real_theme_projects_and_binds_its_imports() {
    // Optional: UNITY_WORKER_UI_FIXTURES holds the worker's artifact of SRP
    // Core's RuntimeDebugWindow.tss, named by its GUID plus `.bin`, and
    // UNITY_EDITOR names the Editor whose binary2text reads it.
    let (Some(root), Some(editor)) = (
        std::env::var("UNITY_WORKER_UI_FIXTURES")
            .ok()
            .filter(|v| !v.is_empty()),
        std::env::var("UNITY_EDITOR").ok().filter(|v| !v.is_empty()),
    ) else {
        return;
    };
    const THEME: &str = "76404632120e0ae4885fc7be8197097c";
    let Ok(input) = fs::read(format!("{root}/{THEME}.bin")) else {
        return;
    };
    let (output, metadata) = crate::runtimeartifact::adapt_ui(&input, "tss").unwrap();
    let count = |name: &str| {
        metadata
            .objects
            .iter()
            .filter(|object| object.type_name == name)
            .count()
    };
    assert_eq!(count("UnityEngine.UIElements.ThemeStyleSheet"), 2);
    assert_eq!(count("UnityEngine.Texture2D") + 2, metadata.objects.len());
    let temp = tempfile::tempdir().unwrap();
    let (file, dump) = (temp.path().join("theme.bin"), temp.path().join("theme.txt"));
    fs::write(&file, &output).unwrap();
    let reader = format!("{}/Data/Tools/binary2text", crate::sysutil::dir(&editor));
    assert!(
        std::process::Command::new(reader)
            .arg(&file)
            .arg(&dump)
            .status()
            .unwrap()
            .success()
    );
    let content = NativeContentFile {
        guid: THEME.into(),
        objects: metadata
            .objects
            .iter()
            .map(|object| NativeContentObject {
                file_id: object.file_id.clone(),
                type_name: object.type_name.clone(),
            })
            .collect::<Vec<_>>()
            .into(),
        dependencies: metadata
            .external_references
            .iter()
            .map(|reference| NativeContentDependency {
                guid: reference.guid.clone(),
                global: reference.guid == "0000000000000000e000000000000000",
            })
            .collect::<Vec<_>>()
            .into(),
        ..Default::default()
    };
    let bindings = parse_native_ui_bindings(&fs::read(&dump).unwrap(), &content).unwrap();
    // The theme imports the default theme, which it holds, and the
    // package's style sheet.
    let main = bindings
        .iter()
        .filter(|binding| binding.owner_file_id == "-4733365628477956816")
        .map(|binding| {
            (
                binding.property_path.as_str(),
                binding.target_guid.as_str(),
                binding.target_file_id.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert!(main.contains(&("imports[0].styleSheet", THEME, "4413694071827464959")));
    assert!(main.contains(&(
        "imports[1].styleSheet",
        "3a8aa4d508da46d4dace18b280f064aa",
        "7433441132597879392"
    )));
}
