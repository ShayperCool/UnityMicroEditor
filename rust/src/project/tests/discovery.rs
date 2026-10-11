//! Tests of bounded native discovery.
use super::{
    FIXTURE_SCENE, Fixture, fixture_session, json, message,
    sources::{AUTHORED_TEXTURE_META, png},
    write_fixture_file,
};
use crate::project::{
    NativeReplacement, NeedsImportError,
    discovery::{
        LineReader, MAX_COOKED_NATIVE_DISCOVERY_BYTES, NATIVE_REFERENCE_LINE_BYTES,
        collect_native_guids, scan_native_reference_guids,
    },
    parse_unity_yaml,
    session::{Dependency, Inner, NativeDiscoveryPack},
    value::{Map, Value},
};
use crate::yaml::MAX_TEXT_BYTES;
use std::{
    collections::BTreeSet,
    fs,
    io::{Seek, SeekFrom, Write},
    path::Path,
};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// `len(s.files[path].content) != 0 || len(s.documents) != 0`.
fn retained(s: &Inner, path: &str) -> bool {
    let content = s
        .cache
        .files
        .get(Path::new(path))
        .and_then(|record| record.content.as_ref())
        .is_some_and(|content| !content.is_empty());
    content || !s.documents.is_empty()
}

fn write_at(path: &str, offset: u64, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

fn truncate(path: &str, size: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(size)
        .unwrap();
}

#[test]
fn native_discovery_traverses_authored_closure_and_skips_unrelated_assets() {
    let fixture = fixture_session();
    let guids = [
        ("Chain.prefab", "22222222222222222222222222222222"),
        ("Data.asset", "33333333333333333333333333333333"),
        ("Mesh.fbx", "44444444444444444444444444444444"),
        ("Sound.wav", "55555555555555555555555555555555"),
        ("Other.fbx", "66666666666666666666666666666666"),
        ("Extra.prefab", "77777777777777777777777777777777"),
        ("Extra.fbx", "88888888888888888888888888888888"),
    ];
    let guid = |name: &str| guids.iter().find(|(file, _)| *file == name).unwrap().1;
    let assets = format!("{}/Assets", fixture.project);
    for (filename, guid) in guids {
        let path = format!("{assets}/{filename}");
        write_fixture_file(&path, format!("native source {filename}"));
        write_fixture_file(format!("{path}.meta"), format!("guid: {guid}\n"));
    }
    write_fixture_file(
        &fixture.scene,
        format!(
            "{FIXTURE_SCENE}--- !u!1001 &90\nPrefabInstance:\n  m_SourcePrefab: {{fileID: 100100000, guid: {}, type: 3}}\n",
            guid("Chain.prefab")
        ),
    );
    write_fixture_file(
        format!("{assets}/Chain.prefab"),
        format!(
            "--- !u!114 &91\nMonoBehaviour:\n  data: {{fileID: 11400000, guid: {}, type: 2}}\n",
            guid("Data.asset")
        ),
    );
    // A cyclic data reference is bounded by visited source identity; it does not
    // trigger model expansion or import unrelated metadata-indexed FBX files.
    write_fixture_file(
        format!("{assets}/Data.asset"),
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  prefab: {{fileID: 100100000, guid: {}, type: 3}}\n  mesh: {{fileID: 4300000, guid: {}, type: 3}}\n  sound: {{fileID: 8300000, guid: {}, type: 3}}\n",
            guid("Chain.prefab"),
            guid("Mesh.fbx"),
            guid("Sound.wav")
        ),
    );
    write_fixture_file(
        format!("{assets}/Extra.prefab"),
        format!(
            "--- !u!33 &92\nMeshFilter:\n  m_Mesh: {{fileID: 4300000, guid: {}, type: 3}}\n",
            guid("Extra.fbx")
        ),
    );
    fixture.session.lock().extra_assets = strings(&["Assets/Extra.prefab"]);
    let expected = strings(&["Assets/Extra.fbx", "Assets/Mesh.fbx", "Assets/Sound.wav"]);
    let actual = fixture.session.pending_native_import_sources();
    assert!(
        actual.as_ref().is_ok_and(|actual| *actual == expected),
        "closure native discovery={actual:?}; expected {expected:?}"
    );
    {
        let mut session = fixture.session.lock();
        for filename in &expected {
            let physical = format!("{}/{filename}", fixture.project);
            let (_, source_sha) = session.read(&physical, false).unwrap();
            let (_, meta_sha) = session.read(&format!("{physical}.meta"), true).unwrap();
            let guid = session.by_path[filename].clone();
            session.native_overlays.insert(
                guid,
                NativeReplacement {
                    source_sha256: source_sha,
                    meta_sha256: meta_sha,
                    ..Default::default()
                },
            );
        }
    }
    let actual = fixture.session.pending_native_import_sources();
    assert!(
        actual.as_ref().is_ok_and(Vec::is_empty),
        "current native overlays were rediscovered: {actual:?}"
    );
    write_fixture_file(format!("{assets}/Mesh.fbx"), "changed native model");
    write_fixture_file(
        format!("{assets}/Sound.wav.meta"),
        format!("guid: {}\nchanged: 1\n", guid("Sound.wav")),
    );
    let actual = fixture.session.pending_native_import_sources();
    assert!(
        actual
            .as_ref()
            .is_ok_and(|actual| *actual == strings(&["Assets/Mesh.fbx", "Assets/Sound.wav"])),
        "changed source or importer metadata was not rediscovered: {actual:?}"
    );
}

#[test]
fn native_discovery_keeps_raw_png_fast_and_selects_native_npot_resize() {
    let fixture = fixture_session();
    let texture = format!("{}/Assets/Texture.png", fixture.project);
    write_fixture_file(format!("{texture}.meta"), AUTHORED_TEXTURE_META);
    write_fixture_file(
        &fixture.scene,
        format!(
            "{FIXTURE_SCENE}--- !u!114 &99\nMonoBehaviour:\n  image: {{fileID: 2800000, guid: 22222222222222222222222222222222, type: 3}}\n"
        ),
    );
    let write_png = |width: u32| write_fixture_file(&texture, png(width, 4));
    write_png(4);
    let pending = fixture.session.pending_native_import_sources();
    assert!(
        pending.as_ref().is_ok_and(Vec::is_empty),
        "raw uncompressed texture needlessly requires native import: {pending:?}"
    );
    write_png(3);
    let pending = fixture.session.pending_native_import_sources();
    assert!(
        pending
            .as_ref()
            .is_ok_and(|pending| *pending == strings(&["Assets/Texture.png"])),
        "NPOT resampling did not select actual TextureImporter: {pending:?}"
    );
    write_png(4);
    write_fixture_file(
        format!("{texture}.meta"),
        AUTHORED_TEXTURE_META.replacen("overridden: 0", "overridden: 1", 1),
    );
    let pending = fixture.session.pending_native_import_sources();
    assert!(
        pending
            .as_ref()
            .is_ok_and(|pending| *pending == strings(&["Assets/Texture.png"])),
        "platform compression did not select native importer: {pending:?}"
    );
}

#[test]
fn native_discovery_selects_every_texture_source_format() {
    for extension in [
        "tga", "tif", "tiff", "bmp", "psd", "gif", "exr", "hdr", "TGA",
    ] {
        let fixture = fixture_session();
        let logical = format!("Assets/Texture.{extension}");
        let texture = format!("{}/{logical}", fixture.project);
        write_fixture_file(&texture, "texture source bytes");
        write_fixture_file(format!("{texture}.meta"), AUTHORED_TEXTURE_META);
        write_fixture_file(
            &fixture.scene,
            format!(
                "{FIXTURE_SCENE}--- !u!114 &99\nMonoBehaviour:\n  image: {{fileID: 2800000, guid: 22222222222222222222222222222222, type: 3}}\n"
            ),
        );
        let pending = fixture.session.pending_native_import_sources();
        assert!(
            pending
                .as_ref()
                .is_ok_and(|pending| *pending == strings(&[&logical])),
            "{extension} was not selected for TextureImporter: {pending:?}"
        );
    }
}

#[test]
fn native_discovery_selects_audio_and_text_sources_and_keeps_current_catalog_assets() {
    for name in [
        "Clip.ogg",
        "Clip.mp3",
        "Clip.aif",
        "Clip.aiff",
        "Clip.flac",
        "Clip.OGG",
        "Data.txt",
        "Data.json",
        "Data.bytes",
        "Data.xml",
        "Data.csv",
        "Sky.hdr",
    ] {
        let fixture = fixture_session();
        let logical = format!("Assets/{name}");
        let source = format!("{}/{logical}", fixture.project);
        write_fixture_file(&source, "native source bytes");
        write_fixture_file(format!("{source}.meta"), AUTHORED_TEXTURE_META);
        write_fixture_file(
            &fixture.scene,
            format!(
                "{FIXTURE_SCENE}--- !u!114 &99\nMonoBehaviour:\n  target: {{fileID: 1, guid: 22222222222222222222222222222222, type: 3}}\n"
            ),
        );
        let pending = fixture.session.pending_native_import_sources();
        assert!(
            pending
                .as_ref()
                .is_ok_and(|pending| *pending == strings(&[&logical])),
            "{name} was not selected for its importer: {pending:?}"
        );
        // The catalog serves an unchanged source in whatever form it holds.
        {
            let mut s = fixture.session.lock();
            let (_, source_sha) = s.read(&source, false).unwrap();
            let (_, meta_sha) = s.read(&format!("{source}.meta"), true).unwrap();
            s.critical.insert(logical.clone(), source_sha);
            s.critical.insert(format!("{logical}.meta"), meta_sha);
            let mut asset = Map::new();
            asset.insert("guid".into(), "22222222222222222222222222222222".into());
            asset.insert("classId".into(), Value::Int(89));
            s.catalog.assets = vec![asset];
        }
        let pending = fixture.session.pending_native_import_sources();
        assert!(
            pending.as_ref().is_ok_and(Vec::is_empty),
            "{name} is current in the catalog: {pending:?}"
        );
        write_fixture_file(&source, "changed native source bytes");
        let pending = fixture.session.pending_native_import_sources();
        assert!(
            pending
                .as_ref()
                .is_ok_and(|pending| *pending == strings(&[&logical])),
            "changed {name} was not selected: {pending:?}"
        );
    }
}

const LARGE_CLIP_GUID: &str = "22222222222222222222222222222222";
const LATE_NATIVE_GUID: &str = "33333333333333333333333333333333";

fn streaming_test_hash(path: &str) -> String {
    crate::sha256(&fs::read(path).unwrap())
}

/// The fixture, the physical clip and the cooked registry pack.
fn cooked_large_clip_fixture(suffix: &str) -> (Fixture, String, String) {
    let fixture = fixture_session();
    let logical = "Assets/Large.anim";
    let path = format!("{}/{logical}", fixture.project);
    let mut content =
        b"--- !u!74 &7400000\nAnimationClip:\n  m_Name: Large\n  _typelessdata: ".to_vec();
    let chunk = vec![b'0'; NATIVE_REFERENCE_LINE_BYTES];
    let mut size = 0;
    while size <= MAX_TEXT_BYTES {
        content.extend(&chunk);
        size += chunk.len();
    }
    content.push(b'\n');
    content.extend(suffix.as_bytes());
    fs::write(&path, &content).unwrap();
    let source_sha = crate::sha256(&content);
    let source_bytes = content.len() as i64;
    drop(content);
    write_fixture_file(
        format!("{path}.meta"),
        format!("fileFormatVersion: 2\nguid: {LARGE_CLIP_GUID}\n"),
    );
    write_fixture_file(
        &fixture.scene,
        format!(
            "{FIXTURE_SCENE}--- !u!114 &90\nMonoBehaviour:\n  clip: {{fileID: 7400000, guid: {LARGE_CLIP_GUID}, type: 2}}\n"
        ),
    );
    let pack_path = format!(
        "{}/Library/UnityMicroEditor/native-test/registry.bundle",
        fixture.project
    );
    write_fixture_file(
        &pack_path,
        "independently verified immutable native AnimationClip registry content",
    );
    let meta_sha = streaming_test_hash(&format!("{path}.meta"));
    {
        let mut s = fixture.session.lock();
        s.catalog.success = true;
        s.catalog.cooked_registry_verified = true;
        s.catalog.backend = "unity_editor_public_api".into();
        s.catalog.scene_loader = "raw".into();
        let Value::Object(clip) = json(serde_json::json!({
            "guid": LARGE_CLIP_GUID, "fileId": "7400000", "classId": 74, "kind": "imported",
            "path": logical, "typeName": "UnityEngine.AnimationClip",
        })) else {
            unreachable!()
        };
        // The class ID is stored as an int here, not a decoded number literal.
        let mut clip: Map = clip;
        clip.insert("classId".into(), Value::Int(74));
        s.catalog.assets = vec![clip];
        s.catalog.source_dependencies = vec![
            Dependency {
                path: logical.into(),
                sha256: source_sha.clone(),
                bytes: source_bytes,
                source_path: String::new(),
            },
            Dependency {
                path: format!("{logical}.meta"),
                sha256: meta_sha.clone(),
                bytes: 84,
                source_path: String::new(),
            },
        ];
        s.critical.insert(logical.into(), source_sha);
        s.critical.insert(format!("{logical}.meta"), meta_sha);
        let pack = NativeDiscoveryPack {
            bundle_path: pack_path.clone(),
            sha256: streaming_test_hash(&pack_path),
            bytes: fs::metadata(&pack_path).unwrap().len() as i64,
            registry_asset: "assets/native-registry.asset".into(),
        };
        let mut bound = Map::new();
        bound.insert("bundlePath".into(), pack.bundle_path.as_str().into());
        bound.insert("sha256".into(), pack.sha256.as_str().into());
        bound.insert("bytes".into(), Value::Int(pack.bytes));
        bound.insert("registryAsset".into(), pack.registry_asset.as_str().into());
        s.catalog.packs = vec![pack];
        s.baseline.insert(
            "importedAssetPacks".into(),
            Value::Array(vec![Value::Object(bound)]),
        );
        s.index_dirty = true;
        s.index_guids().unwrap();
    }
    (fixture, path, pack_path)
}

#[test]
fn cooked_large_native_discovery_streams_current_source_and_keeps_late_dependency_closure() {
    let (fixture, path, _) = cooked_large_clip_fixture(&format!(
        "  reference: {{fileID: 4300000, guid: {LATE_NATIVE_GUID}, type: 3}}\n"
    ));
    write_fixture_file(
        format!("{}/Assets/Late.fbx", fixture.project),
        "native model beyond old clip byte limit",
    );
    write_fixture_file(
        format!("{}/Assets/Late.fbx.meta", fixture.project),
        format!("guid: {LATE_NATIVE_GUID}\n"),
    );
    {
        let mut s = fixture.session.lock();
        s.index_dirty = true;
        s.index_guids().unwrap();
        let refs = s.native_reference_guids(&path);
        assert!(
            refs.as_ref()
                .is_ok_and(|refs| *refs == strings(&[LATE_NATIVE_GUID])),
            "late current cooked PPtr was lost: {refs:?}"
        );
        let digest = &s.cache.files[Path::new(&path)].digest;
        assert!(
            *digest == s.critical["Assets/Large.anim"] && !retained(&s, &path),
            "streaming discovery lost exact whole-source hash or retained large text/YAML allocations"
        );
    }
    for pass in 0..2 {
        let pending = fixture.session.pending_native_import_sources();
        assert!(
            pending
                .as_ref()
                .is_ok_and(|pending| *pending == strings(&["Assets/Late.fbx"])),
            "closure beyond old byte limit differs on pass{pass}: {pending:?}"
        );
    }
}

#[test]
fn cooked_large_native_discovery_rejects_changed_current_source_and_metadata() {
    for suffix in ["", ".meta"] {
        let (fixture, path, _) = cooked_large_clip_fixture("");
        write_at(&format!("{path}{suffix}"), 20, b"X");
        let mut s = fixture.session.lock();
        s.cache.observed.clear();
        let result = s.native_reference_guids(&path);
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<NeedsImportError>().is_some()),
            "changed cooked source or metadata accepted ({suffix:?}): {result:?}"
        );
        assert!(
            !retained(&s, &path),
            "rejected large source was parsed/cached"
        );
    }
}

#[test]
#[allow(clippy::type_complexity)]
fn cooked_large_native_discovery_requires_exact_current_cooked_proof() {
    let cases: [(&str, fn(&mut Inner, &str)); 10] = [
        ("uncooked", |s, _| {
            s.catalog.cooked_registry_verified = false
        }),
        ("source hash omitted", |s, _| {
            s.catalog.source_dependencies.remove(0);
        }),
        ("metadata hash omitted", |s, _| {
            s.catalog.source_dependencies.truncate(1);
        }),
        ("source size mismatches", |s, _| {
            s.catalog.source_dependencies[0].bytes += 1;
        }),
        ("native identity absent", |s, _| s.catalog.assets.clear()),
        ("wrong native type", |s, _| {
            s.catalog.assets[0].insert("typeName".into(), "UnityEngine.Mesh".into());
        }),
        ("unbound packs", |s, _| {
            s.baseline.remove("importedAssetPacks");
        }),
        ("cooked pack changed", |_, pack| {
            write_fixture_file(pack, "changed native cooked artifact");
        }),
        ("cooked pack same-size tamper", |_, pack| {
            write_at(pack, 0, b"X")
        }),
        ("baseline masks catalog revision", |s, _| {
            s.catalog.source_dependencies[0].sha256 = "0".repeat(64);
        }),
    ];
    for (name, change) in cases {
        let (fixture, path, pack) = cooked_large_clip_fixture("");
        let mut s = fixture.session.lock();
        change(&mut s, &pack);
        assert!(
            s.native_reference_guids(&path).is_err(),
            "oversized source accepted without exact current cooked proof: {name}"
        );
        assert!(
            !retained(&s, &path),
            "invalid large proof caused full text/YAML allocation: {name}"
        );
    }
}

#[test]
fn cooked_large_native_discovery_never_falls_back_to_full_yaml() {
    let (fixture, path, _) = cooked_large_clip_fixture(&format!(
        "  pointer:\n    fileID: 4300000\n    guid: {LATE_NATIVE_GUID}\n    type: 3\n"
    ));
    let mut s = fixture.session.lock();
    let result = s.native_reference_guids(&path);
    assert!(
        result
            .as_ref()
            .is_err_and(|error| message(error).contains("full YAML fallback is unsupported")),
        "noncanonical oversized source triggered an unbounded parser: {result:?}"
    );
    assert!(
        !retained(&s, &path),
        "fallback allocated or cached oversized YAML"
    );
}

#[test]
fn cooked_large_native_discovery_retains_absolute_streaming_bound() {
    let (fixture, path, _) = cooked_large_clip_fixture("");
    truncate(&path, MAX_COOKED_NATIVE_DISCOVERY_BYTES + 1);
    assert!(
        fixture
            .session
            .lock()
            .native_reference_guids(&path)
            .is_err(),
        "source larger than supported cooked streaming bound accepted"
    );
}

fn scan_native_text(text: &[u8]) -> anyhow::Result<(Vec<String>, bool)> {
    scan_native_reference_guids(&mut LineReader::new(text, NATIVE_REFERENCE_LINE_BYTES))
}

#[test]
fn native_reference_scanner_distinguishes_pptrs_from_guid_text() {
    let (a, b) = (
        "22222222222222222222222222222222",
        "33333333333333333333333333333333",
    );
    let text = format!(
        "--- !u!114 &11400000\nMonoBehaviour:\n\
\x20\x20local: {{fileID: 15}}\n\
\x20\x20null: {{fileID: 0, guid: {a}, type: 3}}\n\
\x20\x20identifier: {a}\n\
\x20\x20guidOnly: {{guid: {a}, type: 3}}\n\
\x20\x20double: \"{{fileID: 1, guid: {a}, type: 3}}\"\n\
\x20\x20single: '{{fileID: 1, guid: {a}, type: 3}}'\n\
\x20\x20plain: label {{fileID:1,guid:{a},type:3}}\n\
\x20\x20quotedEscapes: 'don''t treat {{fileID: 1, guid: {a}}} as data'\n\
\x20\x20literal: |\n    {{fileID: 1, guid: {a}, type: 3}}\n\
\x20\x20folded: >-\n    {{fileID: 1, guid: {a}, type: 3}}\n\
\x20\x20real: {{fileID: -9223372036854775808, guid: {b}, type: 2}}\n\
\x20\x20duplicate: {{fileID: 4300000, guid: {b}, type: 3}} # comment\n\
\x20\x20list: [{{nested: {{fileID: 43, guid: {b}, type: 3}}}}, {{guid: {a}}}]\n\
\x20\x20ids:\n  - 123\n  - 456\n"
    );
    let scanned = scan_native_text(text.as_bytes());
    let Ok((guids, false)) = scanned else {
        panic!("canonical PPtr selection = {scanned:?}");
    };
    assert_eq!(guids, strings(&[b]), "canonical PPtr selection");
    // Compare with the existing loader traversal, including nested flow maps.
    let documents = parse_unity_yaml(text.as_bytes()).unwrap();
    let mut expected = BTreeSet::new();
    for document in &documents {
        collect_native_guids(&Value::Object(document.fields().clone()), &mut expected);
    }
    assert_eq!(
        guids,
        expected.into_iter().collect::<Vec<_>>(),
        "scanner differs from normal YAML traversal"
    );
}

#[test]
fn native_reference_scanner_fallback_keeps_block_pointers_and_schema_errors() {
    let fixture = fixture_session();
    let mut s = fixture.session.lock();
    let path = format!("{}/Assets/Custom.asset", fixture.project);
    let guid = "22222222222222222222222222222222";
    let cases = [
        (
            "block pointer",
            format!("  pointer:\n    fileID: 4300000\n    guid: {guid}\n    type: 3\n"),
        ),
        (
            "wrapped flow",
            format!("  pointer: {{fileID: 4300000,\n    guid: {guid}, type: 3}}\n"),
        ),
        (
            "quoted field",
            format!("  pointer: {{\"fileID\": 4300000, guid: {guid}, type: 3}}\n"),
        ),
    ];
    for (name, body) in cases {
        write_fixture_file(
            &path,
            format!("--- !u!114 &11400000\nMonoBehaviour:\n{body}"),
        );
        s.cache.observed.clear();
        let refs = s.native_reference_guids(&path);
        assert!(
            refs.as_ref().is_ok_and(|refs| *refs == strings(&[guid])),
            "custom PPtr lost ({name}): {refs:?}"
        );
        assert!(
            s.documents.is_empty(),
            "fallback seeded full YAML document cache: {name}"
        );
    }
    write_fixture_file(
        &path,
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  pointer:\n    fileID: 4\n    fileID: 5\n    guid: {guid}\n"
        ),
    );
    s.cache.observed.clear();
    assert!(
        s.native_reference_guids(&path).is_err(),
        "fallback accepted duplicate pointer keys"
    );
    write_fixture_file(
        &path,
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  pointer:\n    fileID: 9223372036854775808\n    guid: {guid}\n"
        ),
    );
    s.cache.observed.clear();
    assert!(
        s.native_reference_guids(&path).is_err(),
        "fallback rounded oversized signed fileID"
    );
}

#[test]
fn native_reference_discovery_does_not_cache_large_buffers_and_keeps_current_hash() {
    let fixture = fixture_session();
    let mut s = fixture.session.lock();
    let path = format!("{}/Assets/Large.asset", fixture.project);
    let guid = "33333333333333333333333333333333";
    let content = format!(
        "--- !u!43 &4300000\nMesh:\n  _typelessdata: {}\n  glyphs:\n{}  reference: {{fileID: 4300000, guid: {guid}, type: 3}}\n",
        "00".repeat(2 << 20),
        "  - index: 123\n    value: {x: 1, y: 2}\n".repeat(10000)
    );
    write_fixture_file(&path, &content);
    let refs = s.native_reference_guids(&path);
    assert!(
        refs.as_ref().is_ok_and(|refs| *refs == strings(&[guid])),
        "bounded discovery failed: {refs:?}"
    );
    assert_eq!(
        s.cache.files[Path::new(&path)].digest,
        crate::sha256(content.as_bytes()),
        "discovery did not hash the complete large scalar"
    );
    assert!(!retained(&s, &path), "discovery cached mesh/glyph content");
    write_fixture_file(
        &path,
        content.replacen(guid, "44444444444444444444444444444444", 1),
    );
    assert!(
        s.native_reference_guids(&path).is_err(),
        "changed source during one scan pass accepted"
    );
    s.cache.observed.clear();
    let refs = s.native_reference_guids(&path);
    assert!(
        refs.as_ref()
            .is_ok_and(|refs| *refs == strings(&["44444444444444444444444444444444"])),
        "source revision not refreshed: {refs:?}"
    );
}

#[test]
fn native_discovery_ignores_missing_null_and_unrelated_guids() {
    let fixture = fixture_session();
    let native = format!("{}/Assets/Unrelated.fbx", fixture.project);
    let guid = "22222222222222222222222222222222";
    write_fixture_file(&native, "native model");
    write_fixture_file(format!("{native}.meta"), format!("guid: {guid}\n"));
    write_fixture_file(
        &fixture.scene,
        format!(
            "{FIXTURE_SCENE}--- !u!114 &90\nMonoBehaviour:\n  missing: {{fileID: 4300000, guid: 33333333333333333333333333333333, type: 3}}\n  null: {{fileID: 0, guid: {guid}, type: 3}}\n  guidOnly: {{guid: {guid}, type: 3}}\n  text: \"{{fileID: 4300000, guid: {guid}, type: 3}}\"\n"
        ),
    );
    let refs = fixture.session.pending_native_import_sources();
    assert!(
        refs.as_ref().is_ok_and(Vec::is_empty),
        "unrelated/missing GUID selected for import: {refs:?}"
    );
    assert!(
        fixture.session.lock().documents.is_empty(),
        "discovery seeded parsed scene cache"
    );
}

#[test]
fn native_reference_scanner_binary_and_unterminated_last_line() {
    for source in [
        "\x00binary.asset",
        "--- !u!43 &4300000\nMesh:\n  _typelessdata: 00",
    ] {
        let scanned = scan_native_text(source.as_bytes());
        assert!(
            scanned
                .as_ref()
                .is_ok_and(|(refs, fallback)| refs.is_empty() && !fallback),
            "empty/binary EOF: {scanned:?}"
        );
    }
}

#[test]
fn native_discovery_keeps_normal_prepare_schema_validation() {
    let fixture = fixture_session();
    write_fixture_file(
        &fixture.scene,
        FIXTURE_SCENE.replacen(
            "  m_Name: Original",
            "  m_Name: Original\n  m_Name: Duplicate",
            1,
        ),
    );
    if let Err(error) = fixture.session.pending_native_import_sources() {
        panic!("discovery should only collect references: {error:#}");
    }
    let prepared = fixture.session.prepare(&fixture.output);
    assert!(
        prepared
            .as_ref()
            .is_err_and(|error| message(error).contains("duplicate YAML key")),
        "normal preparation lost schema validation: {:?}",
        prepared.err().map(|error| message(&error))
    );
}

#[test]
fn native_reference_source_bounds_reject_fifo_and_oversize_before_reading() {
    let fixture = fixture_session();
    let mut s = fixture.session.lock();
    let pipe = format!("{}/source.fifo", fixture.project);
    let made = std::process::Command::new("mkfifo")
        .args(["-m", "600", &pipe])
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo failed");
    assert!(
        s.native_reference_guids(&pipe).is_err(),
        "nonregular source accepted"
    );
    let large = format!("{}/oversize.asset", fixture.project);
    write_fixture_file(&large, "--- !u!43 &4300000\nMesh:\n");
    truncate(&large, MAX_TEXT_BYTES as u64 + 1);
    assert!(
        s.native_reference_guids(&large).is_err(),
        "oversize source accepted"
    );
}
