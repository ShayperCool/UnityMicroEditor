use serde_json::json;
use std::{
    io::{self, Read, Write},
    os::unix::fs::symlink,
};
use unity_micro_editor::{
    cache::SourceCache,
    lmdb::{self, Image, Record, Tables},
    paths, protocol as p, registry, yaml,
};

#[test]
fn yaml_preserves_64_bit_ids_and_unity_scalars() {
    let docs = yaml::parse_unity_yaml(b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &9223372036854775807\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ABCDEF0123456789ABCDEF0123456789, type: 3}\n  value: 12934340311651418136\n  negative: -0.25\n  trueString: true\n  when: 2001-01-01\n  _typelessdata: 0000000001000000\n  m_IndexBuffer: 000010002000\n").unwrap();
    assert_eq!(docs[0].file_id, "9223372036854775807");
    assert_eq!(docs[0].data["m_Script"]["fileID"], "11500000");
    assert_eq!(
        docs[0].data["m_Script"]["guid"],
        "abcdef0123456789abcdef0123456789"
    );
    assert_eq!(docs[0].data["value"], json!(12934340311651418136u64));
    assert_eq!(docs[0].data["trueString"], "true");
    assert_eq!(docs[0].data["when"], "2001-01-01");
    assert_eq!(docs[0].data["_typelessdata"], "0000000001000000");
    assert_eq!(docs[0].data["m_IndexBuffer"], "000010002000");
}
#[test]
fn yaml_rejects_ambiguous_or_unbounded_content() {
    for body in [
        "  a: 1\n  a: 2\n",
        "  a: &shared [1]\n  b: *shared\n",
        "  a: .nan\n",
        "  a: !custom 1\n",
        "  m_Script: {fileID: 9223372036854775808}\n",
    ] {
        assert!(
            yaml::parse_unity_yaml(format!("--- !u!1 &1\nGameObject:\n{body}").as_bytes()).is_err(),
            "accepted {body}"
        );
    }
    assert!(
        yaml::parse_unity_yaml(b"--- !u!1 &1\nGameObject: {}\n--- !u!4 &1\nTransform: {}\n")
            .is_err()
    );
    assert!(yaml::parse_unity_yaml(&vec![b'x'; yaml::MAX_TEXT_BYTES + 1]).is_err());
    assert!(yaml::parse_unity_yaml(b"--- !u!1 &1\nGameObject: {x: \xff}\n").is_err());
    let deep = format!(
        "--- !u!1 &1\nGameObject: {{x: {}0{}}}\n",
        "[".repeat(129),
        "]".repeat(129)
    );
    assert!(yaml::parse_unity_yaml(deep.as_bytes()).is_err());
    let very_deep = format!(
        "--- !u!1 &1\nGameObject: {{x: {}0{}}}\n",
        "[".repeat(100000),
        "]".repeat(100000)
    );
    assert!(yaml::parse_unity_yaml(very_deep.as_bytes()).is_err());
}
#[test]
fn yaml_handles_bom_crlf_stripped_and_explicit_tags() {
    let docs = yaml::parse_unity_yaml("\u{feff}--- !u!4 &-123 stripped\r\nTransform:\r\n  x: !!str 123\r\n  y: !!bool true\r\n  z: 0xFF\r\n  octal: 077\r\n".as_bytes()).unwrap();
    assert!(docs[0].stripped);
    assert_eq!(docs[0].file_id, "-123");
    assert_eq!(docs[0].data["x"], "123");
    assert_eq!(docs[0].data["y"], "true");
    assert_eq!(docs[0].data["z"], 255);
    assert_eq!(docs[0].data["octal"], 63);
}
#[test]
fn cache_reuses_unchanged_content_and_detects_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scene.unity");
    std::fs::write(&path, b"--- !u!1 &1\nGameObject: {m_Name: Before}\n").unwrap();
    let mut cache = SourceCache::default();
    cache.begin();
    cache.parse(&path).unwrap();
    assert_eq!(cache.stats.file_reads, 1);
    assert_eq!(cache.stats.yaml_parses, 1);
    cache.begin();
    cache.parse(&path).unwrap();
    assert_eq!(cache.stats.file_reads, 0);
    assert_eq!(cache.stats.yaml_parses, 0);
    assert_eq!(cache.stats.yaml_hits, 1);
    std::fs::write(&path, b"--- !u!1 &1\nGameObject: {m_Name: After}\n").unwrap();
    assert!(cache.validate_observed().is_err());
    cache.begin();
    assert_eq!(cache.parse(&path).unwrap()[0].data["m_Name"], "After");
    assert_eq!(cache.stats.yaml_parses, 1);
}
struct Chunks(Vec<u8>);
impl Write for Chunks {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(2);
        self.0.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct ShortReader<'a>(&'a [u8]);
impl Read for ShortReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let n = output.len().min(self.0.len()).min(3);
        output[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}
#[test]
fn worker_frames_handle_partial_io_and_native_handshake_quirk() {
    let mut writer = Chunks(Vec::new());
    p::write_frame(&mut writer, &p::handshake()).unwrap();
    assert_eq!(&writer.0[..9], b"\0\x05\0\0\0\0\0\0\0");
    assert_eq!(writer.0.len(), 16);
    assert_eq!(
        p::read_frame(&mut ShortReader(&writer.0)).unwrap(),
        p::handshake()
    );
    let frame = p::Frame {
        kind: 1,
        payload: p::initialize(24),
    };
    let mut writer = Chunks(Vec::new());
    p::write_frame(&mut writer, &frame).unwrap();
    assert_eq!(p::read_frame(&mut ShortReader(&writer.0)).unwrap(), frame);
    assert!(p::read_frame(&mut &b"\x01\0\0\0\0\0\0\0\0"[..]).is_err());
    let mut oversized = vec![1];
    oversized.extend_from_slice(&((p::MAX_FRAME_PAYLOAD + 1) as u64).to_le_bytes());
    assert!(p::read_frame(&mut &oversized[..]).is_err());
    assert!(p::blob_array(&[0xff; 8], 0, 48).is_err());
    assert!(p::blob_string(&[0xff; 4], 0).is_err());
}
#[test]
fn worker_addresses_match_unity_guid_nibble_order() {
    let wire = p::guid_wire("0123456789abcdef0123456789abcdef").unwrap();
    assert_eq!(
        &wire[..8],
        &[0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe]
    );
    assert_eq!(p::guid_text(&wire), "0123456789abcdef0123456789abcdef");
    assert!(p::guid_wire("xyz").is_err());
    let address = p::import_address("0123456789abcdef0123456789abcdef", -1, [0; 16]).unwrap();
    assert_eq!(p::create_default_asset(&address).unwrap()[0], 19);
    assert!(p::shutdown("bad\0reason").is_err());
    let complete = p::import_complete(
        &address,
        0,
        [0; 16],
        &["path/a".into(), "path/b".into()],
        true,
    )
    .unwrap();
    let (values, count) = p::blob_array(&complete, 80, 4).unwrap();
    assert_eq!(count, 2);
    assert_eq!(values.len(), 8);
    assert_eq!(p::blob_string(&complete, 96).unwrap(), "path/a");
}

fn bytes_from_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn captured_worker_messages_match_the_pinned_native_profile() {
    let wire = bytes_from_hex(
        "011f0000000000000000000000040000004173736574496d706f7274576f726b657250726f62650001010000000000000017",
    );
    let mut input = &wire[..];
    let hello = p::read_frame(&mut input).unwrap();
    assert_eq!(
        p::worker_hello(&hello.payload).unwrap(),
        "AssetImportWorkerProbe"
    );
    let prepared = p::read_frame(&mut input).unwrap();
    assert_eq!(prepared.payload, [23]);
    let data = bytes_from_hex(
        "f2ea8044e695c3e1ad7a114fc05bb18f1b1d4525ba076b6a969e8b3e0c9b21151104000000000000000000000000000000000000000000001104000000000000000000000000000000000000000000000100000001000000180000000100000034000000010000006000000001000000000000001c0000005b56b146b94ebabb3511413bb5772f5e000000000000000000",
    );
    let metadata = p::artifact_metadata(&data).unwrap();
    assert_eq!(metadata.output_id, "f2ea8044e695c3e1ad7a114fc05bb18f");
    assert_eq!(
        metadata.address,
        p::import_address("b1d15452ab70b6a669e9b8e3c0b91251", 1041, [0; 16]).unwrap()
    );
    assert_eq!(metadata.files.len(), 1);
    assert_eq!(metadata.files[0].kind, 0);
    assert_eq!(metadata.files[0].name, "");
    assert_eq!(
        unity_micro_editor::hex(&metadata.files[0].hash),
        "5b56b146b94ebabb3511413bb5772f5e"
    );
    assert!(p::artifact_metadata(&data[..143]).is_err());
    let mut bad = data;
    p::put_u32(&mut bad, 116, 1000);
    assert!(p::artifact_metadata(&bad).is_err());
}

#[test]
fn import_response_checks_aligned_dependency_strides_and_blob_bounds() {
    let first = p::import_address("0123456789abcdef0123456789abcdef", 1041, [0; 16]).unwrap();
    let second = p::import_address("fedcba9876543210fedcba9876543210", 1020, [0; 16]).unwrap();
    let mut data = vec![0; 128 + 2 * 48];
    data[0] = 20;
    p::put_u32(&mut data, 112, 16);
    p::put_u32(&mut data, 116, 2);
    p::put_u32(&mut data, 120, 3);
    data[128..172].copy_from_slice(&first);
    data[176..220].copy_from_slice(&second);
    let response = p::import_response(&data).unwrap();
    assert_eq!(response.dependencies, [first, second]);
    assert_eq!(response.consistency_failure_count, 3);
    assert!(p::import_response(&data[..data.len() - 1]).is_err());
    p::put_u32(&mut data, 84, 1);
    assert!(p::import_response(&data).is_err());
    p::put_u32(&mut data, 80, 1000);
    assert!(p::import_response(&data).is_err());
}
#[test]
fn lmdb_roundtrips_branches_and_overflow_with_native_limits() {
    let records: Vec<_> = (0..12000)
        .map(|i| Record {
            key: format!("asset-{i:08}").into_bytes(),
            value: if i % 500 == 0 {
                vec![i as u8; 13000]
            } else {
                vec![i as u8; 64]
            },
        })
        .collect();
    let mut tables = Tables::new();
    tables.insert("assets".into(), records.clone());
    tables.insert("empty".into(), Vec::new());
    let data = lmdb::build(&tables).unwrap();
    let image = Image::open(&data).unwrap();
    assert_eq!(image.table("assets").unwrap(), records);
    assert!(image.table("empty").unwrap().is_empty());
    let mut broken = data.clone();
    p::put_u64(&mut broken, 136, u64::MAX);
    p::put_u64(&mut broken, lmdb::PAGE_SIZE + 136, u64::MAX);
    assert!(Image::open(&broken).is_err());
    tables.insert(
        "duplicate".into(),
        vec![
            Record {
                key: b"same".to_vec(),
                value: Vec::new()
            };
            2
        ],
    );
    assert!(lmdb::build(&tables).is_err());
}
#[test]
fn output_paths_reject_traversal_and_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().canonicalize().unwrap();
    assert!(paths::is_owned_cache_path(
        &project,
        &project.join("Library/UnityMicroEditor/report.json")
    ));
    assert!(!paths::is_owned_cache_path(
        &project,
        &project.join("Library/UnityMicroEditor")
    ));
    assert!(paths::logical_path("Assets/../outside").is_err());
    assert!(paths::logical_path("Assets/./scene.unity").is_err());
    std::fs::create_dir_all(project.join("Library")).unwrap();
    symlink(temp.path(), project.join("Library/UnityMicroEditor")).unwrap();
    assert!(
        paths::checked_output(
            &project,
            &project.join("Library/UnityMicroEditor/output.json")
        )
        .is_err()
    );
}
/// The children of a folder follow the hash of their list, in the order of
/// the Editor's project window. The first sixteen bytes of a record are
/// read as that hash, so a list without one hides its first child from
/// everything that walks folders, an asset search for one.
#[test]
fn registry_lists_the_children_of_a_folder_as_the_editor_does() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Temp/context");
    std::fs::create_dir_all(project.join("Library")).unwrap();
    std::fs::create_dir_all(project.join("Assets/Settings")).unwrap();
    // In byte order `Mobile_RPAsset` precedes `Mobile_Renderer` and `b@128`
    // precedes `b@32`; the Editor orders without case and by number.
    let names = [
        "Mobile_RPAsset.asset",
        "Mobile_Renderer.asset",
        "b@128.png",
        "b@32.png",
    ];
    let mut assets = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let path = format!("Assets/Settings/{name}");
        std::fs::write(project.join(&path), "x").unwrap();
        std::fs::write(
            project.join(format!("{path}.meta")),
            format!("fileFormatVersion: 2\nguid: {:032x}\n", index + 1),
        )
        .unwrap();
        assets.push(registry::Asset {
            path,
            source: String::new(),
            meta: String::new(),
            guid: String::new(),
            directory: false,
            script: false,
        });
    }
    let report = registry::write(registry::Input {
        project: project.clone(),
        unity_version: String::new(),
        assets,
        roots: Default::default(),
        singletons: Default::default(),
    })
    .unwrap();
    let data = std::fs::read(&report.database).unwrap();
    let image = Image::open(&data).unwrap();
    let paths = image.table("GuidToPath").unwrap();
    let name_of = |wire: &[u8]| {
        let record = paths.iter().find(|record| record.key == wire).unwrap();
        String::from_utf8_lossy(&record.value[..record.value.len() - 1]).into_owned()
    };
    let folder = paths
        .iter()
        .find(|record| record.value == b"Assets/Settings\0")
        .unwrap();
    let children = image.table("GuidToChildren").unwrap();
    let list = &children
        .iter()
        .find(|record| record.key == folder.key)
        .unwrap()
        .value;
    assert_eq!(list.len(), 16 * 5);
    assert_eq!(list[..16], registry::native_hash(&list[16..]));
    let listed: Vec<String> = list[16..].chunks(16).map(name_of).collect();
    assert_eq!(
        listed,
        [
            "Assets/Settings/b@32.png",
            "Assets/Settings/b@128.png",
            "Assets/Settings/Mobile_Renderer.asset",
            "Assets/Settings/Mobile_RPAsset.asset",
        ]
    );
}

#[test]
fn registry_preserves_metadata_and_refuses_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Temp/context");
    std::fs::create_dir_all(project.join("Library")).unwrap();
    std::fs::create_dir_all(project.join("Assets/Scripts")).unwrap();
    let meta = "fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\nMonoImporter:\n  executionOrder: -12\n  userData: 'hello'\n  assetBundleName: bundle\n";
    std::fs::write(project.join("Assets/Scripts/A.cs"), "class A {}\n").unwrap();
    std::fs::write(project.join("Assets/Scripts/A.cs.meta"), meta).unwrap();
    let make_input = || registry::Input {
        project: project.clone(),
        unity_version: String::new(),
        assets: vec![registry::Asset {
            path: "Assets/Scripts/A.cs".into(),
            source: String::new(),
            meta: String::new(),
            guid: String::new(),
            directory: false,
            script: false,
        }],
        roots: Default::default(),
        singletons: Default::default(),
    };
    let report = registry::write(make_input()).unwrap();
    assert_eq!(report.guid_count, 3);
    assert_eq!(
        std::fs::read_to_string(project.join("Assets/Scripts/A.cs.meta")).unwrap(),
        meta
    );
    assert!(!project.join("Assets/Scripts.meta").exists());
    let data = std::fs::read(&report.database).unwrap();
    let image = Image::open(&data).unwrap();
    assert_eq!(image.table("GuidToPath").unwrap().len(), 3);
    let props = image.table("GuidPropertyIDToProperty").unwrap();
    assert!(
        props
            .iter()
            .any(|r| r.key.starts_with(b"MonoImporter.executionOrder")
                && r.value == (-12i32).to_le_bytes())
    );
    // Nothing of an importer's own code is recorded unless it is asked for.
    assert!(
        !props
            .iter()
            .any(|r| r.key.starts_with(b"scriptCompilationAssetPath"))
    );
    assert!(image.table("Misc").unwrap().is_empty());
    assert!(image.table("SerializedSingletons").unwrap().is_empty());
    assert!(registry::write(make_input()).is_err());
    assert_eq!(std::fs::read(report.database).unwrap(), data);
}
#[test]
fn registry_records_a_changed_source_of_a_live_context_in_place() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Temp/context");
    std::fs::create_dir_all(project.join("Library")).unwrap();
    std::fs::create_dir_all(project.join("Assets/Data")).unwrap();
    std::fs::write(project.join("Assets/Data/A.asset"), "one\n").unwrap();
    std::fs::write(
        project.join("Assets/Data/A.asset.meta"),
        "fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();
    let report = registry::write(registry::Input {
        project: project.clone(),
        unity_version: String::new(),
        assets: vec![registry::Asset {
            path: "Assets/Data/A.asset".into(),
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
    let record = |data: &[u8]| {
        let image = Image::open(data).unwrap();
        image
            .table("hash")
            .unwrap()
            .iter()
            .find(|record| record.key == b"assets/data/a.asset\0")
            .unwrap()
            .value
            .clone()
    };
    let before = record(&std::fs::read(&report.database).unwrap());
    std::fs::write(project.join("Assets/Data/A.asset"), "two, longer\n").unwrap();
    registry::update_source(project.to_str().unwrap(), "Assets/Data/A.asset").unwrap();
    let after = record(&std::fs::read(&report.database).unwrap());
    assert_ne!(before[..16], after[..16]);
    assert_eq!(u64::from_le_bytes(after[24..32].try_into().unwrap()), 12);
    assert!(registry::update_source(project.to_str().unwrap(), "Assets/Data/B.asset").is_err());
}
#[test]
fn registry_records_importer_scripts_package_roots_and_singletons() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Temp/context");
    let package = "Library/PackageCache/com.example.importer@0123456789ab";
    std::fs::create_dir_all(project.join(package).join("Editor")).unwrap();
    std::fs::create_dir_all(project.join("Assets")).unwrap();
    let meta = |guid: &str| format!("fileFormatVersion: 2\nguid: {guid}\n");
    for (name, guid) in [
        ("Editor/Importer.cs", "0123456789abcdef0123456789abcde1"),
        ("Editor/Example.asmdef", "0123456789abcdef0123456789abcde2"),
    ] {
        std::fs::write(project.join(package).join(name), "x\n").unwrap();
        std::fs::write(
            project.join(package).join(format!("{name}.meta")),
            meta(guid),
        )
        .unwrap();
    }
    std::fs::write(project.join("Assets/a.example"), "a\n").unwrap();
    std::fs::write(
        project.join("Assets/a.example.meta"),
        meta("0123456789abcdef0123456789abcde3"),
    )
    .unwrap();
    let asset = |path: &str, script: bool| registry::Asset {
        path: path.into(),
        source: String::new(),
        meta: String::new(),
        guid: String::new(),
        directory: false,
        script,
    };
    let root = "Packages/com.example.importer";
    let input = |directory: &str| registry::Input {
        project: project.clone(),
        unity_version: String::new(),
        assets: vec![
            asset("Assets/a.example", false),
            asset(&format!("{root}/Editor/Importer.cs"), true),
            asset(&format!("{root}/Editor/Example.asmdef"), true),
        ],
        roots: [(root.to_string(), directory.to_string())].into(),
        singletons: [("Library/ScriptMapper".to_string(), b"table".to_vec())].into(),
    };
    // A package may only be placed where the Editor keeps immutable ones.
    assert!(registry::write(input("Assets/Elsewhere")).is_err());
    let report = registry::write(input(package)).unwrap();
    let data = std::fs::read(&report.database).unwrap();
    let image = Image::open(&data).unwrap();
    let props = image.table("GuidPropertyIDToProperty").unwrap();
    let scripts: Vec<&[u8]> = props
        .iter()
        .filter(|r| r.key.starts_with(b"scriptCompilationAssetPath"))
        .map(|r| r.value.as_slice())
        .collect();
    assert_eq!(scripts.len(), 2);
    assert!(scripts.contains(&&b"Packages/com.example.importer/Editor/Importer.cs\0"[..]));
    // The root folder names the directory the package lies in, and the
    // refresh version makes the worker read it.
    let roots = image.table("RootFolders").unwrap();
    let physical = project.join(package);
    let physical = physical.as_os_str().as_encoded_bytes();
    assert!(roots.iter().any(|r| {
        r.key == format!("{root}\0").as_bytes()
            && r.value
                .windows(physical.len())
                .any(|window| window == physical)
    }));
    let misc = image.table("Misc").unwrap();
    assert_eq!(misc.len(), 1);
    assert_eq!(misc[0].key, b"refreshVersion\0");
    assert_eq!(misc[0].value, 1u32.to_le_bytes());
    let singletons = image.table("SerializedSingletons").unwrap();
    assert_eq!(singletons.len(), 1);
    assert_eq!(singletons[0].key, b"mem://Library/ScriptMapper\0");
    assert_eq!(singletons[0].value, b"table");
}
