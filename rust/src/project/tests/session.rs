//! Tests of the session, scene selection, native expansion, prefab override proofs, YAML,
//! cache paths, netcode data and package sources.
use super::{
    FIXTURE_GUID, FIXTURE_SCENE, Fixture, data_json, fixture_session, message, write_fixture_file,
};
use crate::project::{
    NeedsImportError, Options, Session,
    expand::{Expanded, target_key},
    is_owned_cache_path, manifest_json,
    native::valid_native_hex,
    native_model_graph_sha256,
    netcode::{derived_network_data, xx_hash32},
    parse_unity_yaml,
    session::{Dependency, Inner, SourceRoot},
    types::{
        Document, List, NativeContentFile, NativeContentObject, NativeModelGraphObject,
        NativeModelGraphReference, NativeModelGraphTarget, NativeReplacement,
    },
    value::{self, Map, Value, array, file_id, number, str},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    process::Command,
    sync::Arc,
};

const NATIVE_GUID: &str = "22222222222222222222222222222222";
const SCENE_ROOTS: &str =
    "--- !u!1660057539 &9223372036854775807\nSceneRoots:\n  m_Roots:\n  - {fileID: 11}\n";

fn baseline_path(fixture: &Fixture) -> String {
    format!("{}/baseline.json", fixture.project)
}
fn options(fixture: &Fixture) -> Options {
    Options {
        project: fixture.project.clone(),
        manifest: baseline_path(fixture),
        ..Options::default()
    }
}
fn file_hash(path: &str) -> String {
    crate::sha256(&fs::read(path).unwrap())
}
fn objects_of(manifest: &Map) -> &[Value] {
    array(value::get(manifest, "objects"))
}
fn local(id: &str) -> Value {
    Value::Object(value::map([("fileID", id.into())]))
}

#[test]
fn session_refreshes_scene_and_adds_game_object_without_modifying_published_revision() {
    let f = fixture_session();
    let first = f.session.prepare(&f.output).unwrap();
    let old_bytes = fs::read(&first.manifest).unwrap();
    let changed = FIXTURE_SCENE
        .replacen("m_Name: Original", "m_Name: Modified", 1)
        .replacen(
            "  - {fileID: 11}\n",
            "  - {fileID: 11}\n  - {fileID: 21}\n",
            1,
        )
        + "--- !u!1 &20
GameObject:
  m_Component:
  - component: {fileID: 21}
  m_Name: Added
  m_IsActive: 1
--- !u!4 &21
Transform:
  m_GameObject: {fileID: 20}
  m_LocalPosition: {x: 3, y: 2, z: 1}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
";
    write_fixture_file(&f.scene, changed);
    let second = f.session.prepare(&f.output).unwrap();
    assert!(
        second.objects == 5 && second.cache.yaml_parses == 1,
        "addition was not refreshed: {second:?}"
    );
    let new_manifest = manifest_json(&second.manifest).unwrap();
    let mut found = false;
    for o in objects_of(&new_manifest) {
        if str(o.field("fileId")) == "10" {
            found = data_json(o).get("m_Name") == Some(&Value::String("Modified".into()));
        }
    }
    assert!(found, "authored name did not update");
    assert_eq!(
        old_bytes,
        fs::read(&first.manifest).unwrap(),
        "previous revision changed"
    );
    let warm = f.session.prepare(&f.output).unwrap();
    assert!(
        warm.cache.yaml_parses == 0 && warm.cache.file_reads == 0,
        "warm preparation reread source: {:?}",
        warm.cache
    );
}

/// A preparation whose manifest would equal the one published before but
/// for its id publishes nothing: a patched C# edit wrote a scene manifest
/// and an fsync per edit (caches6-3). A change of what the manifest holds
/// publishes a new revision, and the one before stays as it was.
#[test]
fn an_unchanged_preparation_is_the_revision_before_it() {
    let f = fixture_session();
    let first = f.session.prepare(&f.output).unwrap();
    let revisions = || {
        fs::read_dir(format!("{}/revisions", f.output))
            .unwrap()
            .count()
    };
    let again = f.session.prepare(&f.output).unwrap();
    assert_eq!(
        (
            again.revision.as_str(),
            again.manifest.as_str(),
            revisions()
        ),
        (first.revision.as_str(), first.manifest.as_str(), 1)
    );
    // A revision that is gone is published again.
    fs::remove_dir_all(crate::sysutil::dir(&first.manifest)).unwrap();
    let republished = f.session.prepare(&f.output).unwrap();
    assert!(republished.revision != first.revision && revisions() == 1);
    let old_bytes = fs::read(&republished.manifest).unwrap();
    write_fixture_file(
        &f.scene,
        FIXTURE_SCENE.replacen("m_Name: Original", "m_Name: Modified", 1),
    );
    let changed = f.session.prepare(&f.output).unwrap();
    assert!(changed.revision != republished.revision && revisions() == 2);
    assert_eq!(old_bytes, fs::read(&republished.manifest).unwrap());
    let unchanged = f.session.prepare(&f.output).unwrap();
    assert_eq!(unchanged.revision, changed.revision);
    assert_eq!(revisions(), 2);
}

/// A prefab instance's override of a script's field that the prefab's
/// document lacks (a field added after the prefab was saved, by a patch for
/// one) is the value Unity takes; the parents of a nested path are made. On
/// an engine component an absent key stays refused (mcp6-2).
#[test]
fn an_override_of_a_script_field_the_prefab_lacks_is_applied() {
    use crate::project::expand::apply_override;
    let reference = value::map([("fileID", Value::Int(0))]);
    let mut data = value::map([("level", Value::Int(1))]);
    apply_override(
        &mut data,
        "power",
        &Value::String("7".into()),
        &reference,
        true,
    )
    .unwrap();
    apply_override(
        &mut data,
        "tuning.armor",
        &Value::String("2".into()),
        &reference,
        true,
    )
    .unwrap();
    assert_eq!(number(data.get("power").unwrap()), 7);
    assert_eq!(number(data.get("tuning").unwrap().field("armor")), 2);
    let mut transform = value::map([("m_LocalPosition", Value::Null)]);
    let error = apply_override(
        &mut transform,
        "m_Missing",
        &Value::String("1".into()),
        &reference,
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("field m_Missing does not exist"),
        "{error}"
    );
}

#[test]
fn session_rejects_dangling_and_cyclic_scene() {
    let f = fixture_session();
    for changed in [
        FIXTURE_SCENE.replacen(
            "m_GameObject: {fileID: 10}",
            "m_GameObject: {fileID: 99}",
            1,
        ),
        FIXTURE_SCENE.replacen("m_Father: {fileID: 0}", "m_Father: {fileID: 11}", 1),
    ] {
        write_fixture_file(&f.scene, changed);
        assert!(
            f.session.prepare(&f.output).is_err(),
            "published invalid graph"
        );
    }
}

#[test]
fn session_rejects_stale_native_source_and_keeps_previous_revision() {
    let f = fixture_session();
    let native = format!("{}/Assets/Audio.wav", f.project);
    write_fixture_file(&native, "original");
    {
        let mut s = f.session.lock();
        let (_, expected) = s.read(&native, false).unwrap();
        s.critical
            .insert("Assets/Audio.wav".into(), expected.clone());
        s.baseline.insert(
            "sourceDependencies".into(),
            Value::Array(vec![Value::Object(value::map([
                ("path", "Assets/Audio.wav".into()),
                ("sha256", expected.into()),
                ("bytes", Value::Int(8)),
            ]))]),
        );
    }
    let good = f.session.prepare(&f.output).unwrap();
    write_fixture_file(&native, "modified");
    let error = f.session.prepare(&f.output).unwrap_err();
    assert!(
        error.downcast_ref::<NeedsImportError>().is_some(),
        "expected importer diagnostic, got {}",
        message(&error)
    );
    assert!(fs::metadata(&good.manifest).is_ok(), "valid revision lost");
}

#[test]
fn prefab_instance_overrides_and_stripped_references() {
    let f = fixture_session();
    let prefab = format!("{}/Assets/Base.prefab", f.project);
    write_fixture_file(&prefab, FIXTURE_SCENE.replacen(SCENE_ROOTS, "", 1));
    let guid = "22222222222222222222222222222222";
    write_fixture_file(format!("{prefab}.meta"), format!("guid: {guid}\n"));
    let scene_text = "--- !u!1001 &100
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications:
    - target: {fileID: 11, guid: 22222222222222222222222222222222, type: 3}
      propertyPath: m_LocalPosition.x
      value: 42
      objectReference: {fileID: 0}
    - target: {fileID: 10, guid: 22222222222222222222222222222222, type: 3}
      propertyPath: m_Name
      value: Authored Instance
      objectReference: {fileID: 0}
--- !u!4 &200 stripped
Transform:
  m_CorrespondingSourceObject: {fileID: 11, guid: 22222222222222222222222222222222, type: 3}
  m_PrefabInstance: {fileID: 100}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 200}
";
    write_fixture_file(&f.scene, scene_text);
    f.session.invalidate(&[format!("{prefab}.meta")]);
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let (mut found_name, mut found_position) = (false, false);
    for o in objects_of(&manifest) {
        let data = data_json(o);
        if str(o.field("fileId")) == "100:10" {
            found_name = data.get("m_Name") == Some(&Value::String("Authored Instance".into()));
        }
        if str(o.field("fileId")) == "100:11" {
            found_position = number(value::get(&data, "m_LocalPosition").field("x")) == 42;
        }
    }
    assert!(
        found_name && found_position,
        "instance overrides did not apply"
    );
}

/// JSON numbers compare by exact rational value.
fn normalize_numbers(value: &Value) -> Value {
    match value {
        Value::Number(text) => match value::decimal(text) {
            Some(exact) => Value::String(format!("number:{exact:?}")),
            None => value.clone(),
        },
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, item)| (k.clone(), normalize_numbers(item)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalize_numbers).collect()),
        other => other.clone(),
    }
}

fn compare_manifest_objects(want: &[Value], got: &[Value]) {
    assert_eq!(got.len(), want.len(), "object count");
    for (index, (old, new)) in want.iter().zip(got).enumerate() {
        assert_eq!(
            str(new.field("fileId")),
            str(old.field("fileId")),
            "object ordering differs at {index}"
        );
    }
    let by_id: BTreeMap<String, &Value> = want
        .iter()
        .map(|o| (str(o.field("fileId")).into_owned(), o))
        .collect();
    for o in got {
        let id = str(o.field("fileId"));
        let old = by_id
            .get(id.as_ref())
            .unwrap_or_else(|| panic!("new identity {id}"));
        for field in [
            "classId",
            "gameObjectId",
            "sourcePath",
            "guid",
            "typeName",
            "skipMissingScript",
        ] {
            assert_eq!(
                str(old.field(field)),
                str(o.field(field)),
                "object {id} field {field} changed"
            );
        }
        for field in ["dataJson", "scalarDataJson"] {
            let a = value::parse_use_number(&str(old.field(field))).unwrap();
            let b = value::parse_use_number(&str(o.field(field))).unwrap();
            assert_eq!(
                normalize_numbers(&a),
                normalize_numbers(&b),
                "object {id} {field} differs"
            );
        }
    }
}

/// Opt-in parity check over a real project scene and import catalog; reads only.
#[test]
fn actual_sample_scene_parity() {
    let Ok(project) = std::env::var("UNITY_PLAY_PARITY_PROJECT") else {
        return;
    };
    if project.is_empty() {
        return;
    }
    let session = Session::new(Options {
        project,
        manifest: std::env::var("UNITY_PLAY_PARITY_MANIFEST").unwrap_or_default(),
        native_catalog: std::env::var("UNITY_PLAY_PARITY_CATALOG").unwrap_or_default(),
        ..Options::default()
    })
    .unwrap();
    let output = tempfile::tempdir().unwrap();
    let prepared = session.prepare(output.path().to_str().unwrap()).unwrap();
    let actual = manifest_json(&prepared.manifest).unwrap();
    let baseline = session.lock().baseline.clone();
    compare_manifest_objects(objects_of(&baseline), objects_of(&actual));
    let old_assets = array(value::get(&baseline, "assets"));
    let new_assets = array(value::get(&actual, "assets"));
    assert_eq!(new_assets.len(), old_assets.len(), "asset count");
    for (old, new) in old_assets.iter().zip(new_assets) {
        assert!(
            str(old.field("guid")) == str(new.field("guid"))
                && str(old.field("fileId")) == str(new.field("fileId")),
            "asset identities differ"
        );
        if str(old.field("kind")) == "prefab" {
            compare_manifest_objects(array(old.field("objects")), array(new.field("objects")));
        }
    }
    let warm_output = tempfile::tempdir().unwrap();
    let warm = session
        .prepare(warm_output.path().to_str().unwrap())
        .unwrap();
    assert!(
        warm.cache.file_reads == 0 && warm.cache.yaml_parses == 0,
        "warm preparation did not reuse cache: {:?}",
        warm.cache
    );
}

#[test]
fn actual_sample_scene_iteration_in_reflink_fixture() {
    let Ok(project) = std::env::var("UNITY_PLAY_PARITY_PROJECT") else {
        return;
    };
    if project.is_empty() {
        return;
    }
    let fixture_root = format!("{project}/Temp/UnityPlay/fixture-project");
    fs::create_dir_all(&fixture_root).unwrap();
    let guard = tempfile::Builder::new()
        .prefix("sample-iteration-")
        .tempdir_in(&fixture_root)
        .unwrap();
    let fixture = guard.path().to_str().unwrap().to_string();
    let copied = Command::new("cp")
        .args(["-a", "--reflink=always"])
        .args(["Assets", "Packages", "ProjectSettings"].map(|name| format!("{project}/{name}")))
        .arg(&fixture)
        .output()
        .unwrap();
    assert!(
        copied.status.success(),
        "isolated XFS source fixture: {}",
        String::from_utf8_lossy(&copied.stderr)
    );
    let mut baseline =
        manifest_json(&std::env::var("UNITY_PLAY_PARITY_MANIFEST").unwrap_or_default()).unwrap();
    let mut native =
        manifest_json(&std::env::var("UNITY_PLAY_PARITY_CATALOG").unwrap_or_default()).unwrap();
    baseline.insert("projectRoot".into(), fixture.as_str().into());
    native.insert("project".into(), fixture.as_str().into());
    if let Some(Value::Array(dependencies)) = baseline.get_mut("sourceDependencies") {
        for dependency in dependencies {
            let path = str(dependency.field("path")).into_owned();
            if path.starts_with("Temp/")
                && let Some(fields) = dependency.as_object_mut()
            {
                fields.insert("sourcePath".into(), format!("{project}/{path}").into());
            }
        }
    }
    for (name, content) in [("baseline.json", &baseline), ("catalog.json", &native)] {
        fs::write(
            format!("{fixture}/{name}"),
            value::compact_map(content).unwrap(),
        )
        .unwrap();
    }
    let session = Session::new(Options {
        project: fixture.clone(),
        manifest: format!("{fixture}/baseline.json"),
        native_catalog: format!("{fixture}/catalog.json"),
        ..Options::default()
    })
    .unwrap();
    let output = format!("{fixture}/Temp/Output");
    let before = session.prepare(&output).unwrap();
    let logical = value::text(&baseline, "scenePath").into_owned();
    let original = fs::read_to_string(format!("{project}/{logical}")).unwrap();
    let changed = original
        .replacen("m_Name: Main Camera", "m_Name: Iteration Camera", 1)
        .replacen(
            "m_LocalPosition: {x: 0, y: 1, z: -10}",
            "m_LocalPosition: {x: 0, y: 4, z: -10}",
            1,
        )
        .replacen(
            "SceneRoots:\n  m_ObjectHideFlags: 0\n  m_Roots:\n",
            "SceneRoots:\n  m_ObjectHideFlags: 0\n  m_Roots:\n  - {fileID: 900000011}\n",
            1,
        )
        + "--- !u!1 &900000010
GameObject:
  m_Component:
  - component: {fileID: 900000011}
  m_Name: Added GameObject
  m_IsActive: 1
--- !u!4 &900000011
Transform:
  m_GameObject: {fileID: 900000010}
  m_LocalPosition: {x: 3, y: 2, z: 1}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
";
    write_fixture_file(format!("{fixture}/{logical}"), changed);
    let after = session.prepare(&output).unwrap();
    assert!(
        after.objects == before.objects + 2
            && after.assets == before.assets
            && after.cache.yaml_parses == 1,
        "full scene iteration mismatch: before {before:?} after {after:?}"
    );
    let manifest = manifest_json(&after.manifest).unwrap();
    let mut names = BTreeSet::new();
    let mut camera_moved = false;
    for o in objects_of(&manifest) {
        let data = data_json(o);
        if number(o.field("classId")) == 1 {
            names.insert(value::text(&data, "m_Name").into_owned());
        }
        if str(o.field("fileId")) == "330585546" {
            camera_moved = number(value::get(&data, "m_LocalPosition").field("y")) == 4;
        }
    }
    assert!(
        names.contains("Added GameObject") && names.contains("Iteration Camera") && camera_moved,
        "actual Scene YAML changes did not reach manifest"
    );
    assert_eq!(
        fs::read_to_string(format!("{project}/{logical}")).unwrap(),
        original,
        "original authored scene changed"
    );
}

#[test]
fn select_authored_scene_with_existing_host() {
    let f = fixture_session();
    let other = "Assets/Net/Boot.unity";
    let path = format!("{}/{other}", f.project);
    write_fixture_file(&path, FIXTURE_SCENE.replace("Original", "Network boot"));
    write_fixture_file(
        format!("{path}.meta"),
        "guid: 22222222222222222222222222222222\n",
    );
    let session = Session::new(Options {
        scene: other.into(),
        ..options(&f)
    })
    .unwrap();
    let prepared = session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    assert!(
        prepared.scene == other
            && value::text(&manifest, "scenePath") == other
            && manifest.get("sceneName") == Some(&Value::String("Boot".into())),
        "selected source was not published: {:?} {prepared:?}",
        manifest.get("scenePath")
    );
    assert!(
        str(objects_of(&manifest)[0].field("dataJson")).contains("Network boot"),
        "selected scene contents were not loaded"
    );
    assert_eq!(
        array(value::get(&manifest, "assemblies"))[0],
        Value::String("existing.dll".into()),
        "prepared native/managed host binding changed"
    );
}

#[test]
fn reject_invalid_scene_selection() {
    let f = fixture_session();
    for scene in [
        "/tmp/Boot.unity",
        "Assets/../Outside.unity",
        "Assets/Scenes/Missing.unity",
        "Library/Cached.unity",
        "Assets/Scenes/Sample.prefab",
    ] {
        let created = Session::new(Options {
            scene: scene.into(),
            ..options(&f)
        });
        assert!(created.is_err(), "accepted invalid source {scene}");
    }
}

#[test]
fn runtime_addressed_prefab_includes_exact_root_and_source_closure() {
    let f = fixture_session();
    let path = "Assets/Net/Player.prefab";
    let physical = format!("{}/{path}", f.project);
    write_fixture_file(
        &physical,
        FIXTURE_SCENE.split("--- !u!1660057539").next().unwrap(),
    );
    write_fixture_file(
        format!("{physical}.meta"),
        "guid: 33333333333333333333333333333333\n",
    );
    let session = Session::new(Options {
        extra_assets: vec![path.into()],
        ..options(&f)
    })
    .unwrap();
    let prepared = session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let assets = array(value::get(&manifest, "assets"));
    assert!(
        assets.len() == 1
            && assets[0].field("kind") == &Value::String("prefab".into())
            && assets[0].field("rootFileId") == &Value::String("10".into()),
        "runtime prefab lost its graph/root: {assets:?}"
    );
    let found = array(value::get(&manifest, "sourceDependencies"))
        .iter()
        .any(|dependency| str(dependency.field("path")) == path);
    assert!(
        found,
        "runtime-addressed prefab was omitted from source identity"
    );
}

/// A baseline and catalog carrying one imported
/// model whose hierarchy is the fixture scene without its SceneRoots.
fn native_fbx_fixture() -> (Fixture, Session, NativeReplacement) {
    let f = fixture_session();
    let logical = "Assets/Native.fbx";
    let source = format!("{}/{logical}", f.project);
    write_fixture_file(&source, "original imported source");
    write_fixture_file(
        format!("{source}.meta"),
        format!("fileFormatVersion: 2\nguid: {NATIVE_GUID}\n"),
    );
    let asset = serde_json::json!({"kind":"imported","guid":NATIVE_GUID,"path":logical,"fileId":"4300000","typeName":"UnityEngine.Mesh","classId":43});
    let mut baseline = value::compact_map(&f.session.lock().baseline)
        .map(|text| serde_json::from_str::<serde_json::Value>(&text).unwrap())
        .unwrap();
    baseline["assets"] = serde_json::json!([asset]);
    baseline["sourceDependencies"] = serde_json::json!([
        {"path": logical, "sha256": file_hash(&source)},
        {"path": format!("{logical}.meta"), "sha256": file_hash(&format!("{source}.meta"))},
    ]);
    fs::write(baseline_path(&f), baseline.to_string()).unwrap();
    let model = FIXTURE_SCENE.split("--- !u!1660057539").next().unwrap();
    let mut documents = parse_unity_yaml(model.as_bytes()).unwrap();
    for document in &mut documents {
        document.type_name = format!("UnityEngine.{}", document.type_name);
    }
    let root = documents[0].data.as_mut().unwrap();
    root.insert("m_Layer".into(), Value::Int(0));
    root.insert("m_TagString".into(), "Untagged".into());
    root.insert("m_StaticEditorFlags".into(), Value::Int(0));
    let objects: serde_json::Value =
        serde_json::from_str(&value::encode_json(&documents).unwrap()).unwrap();
    let catalog = serde_json::json!({"schemaVersion":1,"engineVersion":"6000.6.3f1","project":f.project,"assets":[asset],
        "modelPrefabs":[{"guid":NATIVE_GUID,"path":logical,"objects":objects}]});
    let catalog_path = format!("{}/catalog.json", f.project);
    fs::write(&catalog_path, catalog.to_string()).unwrap();
    let session = Session::new(Options {
        native_catalog: catalog_path,
        ..options(&f)
    })
    .unwrap();
    let runtime = format!("{}/native/runtime.bin", f.output);
    write_fixture_file(&runtime, "independently adapted immutable runtime file");
    write_fixture_file(
        format!("{}/native/audio.resource", f.output),
        "immutable streamed audio payload",
    );
    let mut content_objects = vec![NativeContentObject {
        file_id: "4300000".into(),
        type_name: "UnityEngine.Mesh".into(),
    }];
    let model_graph = {
        let s = session.lock();
        for document in s.models[NATIVE_GUID].iter() {
            content_objects.push(NativeContentObject {
                file_id: document.file_id.clone(),
                type_name: document.type_name.clone(),
            });
        }
        s.native_model_graph(NATIVE_GUID).unwrap()
    };
    let replacement = NativeReplacement {
        guid: NATIVE_GUID.into(),
        source_path: logical.into(),
        source_sha256: file_hash(&source),
        meta_sha256: file_hash(&format!("{source}.meta")),
        content: NativeContentFile {
            guid: NATIVE_GUID.into(),
            sha256: file_hash(&runtime),
            path: runtime,
            objects: List::from(content_objects),
            ..NativeContentFile::default()
        },
        model_graph: List::from(model_graph),
        ..NativeReplacement::default()
    };
    (f, session, replacement)
}

/// The same model, unknown to baseline and catalog.
fn new_native_model_fixture() -> (Fixture, Session, NativeReplacement) {
    let (f, session, mut replacement) = native_fbx_fixture();
    {
        let mut s = session.lock();
        s.assets.clear();
        s.catalog.assets.clear();
        s.catalog.model_prefabs.clear();
        s.models.clear();
        s.baseline.insert("assets".into(), Value::Array(Vec::new()));
    }
    replacement.model_graph_sha256 = native_model_graph_sha256(&replacement.model_graph);
    (f, session, replacement)
}

#[test]
fn reimported_model_ignores_only_overrides_for_proven_removed_objects() {
    for target in ["-5824371076177060002", "4300000"] {
        let (f, session, replacement) = new_native_model_fixture();
        session.lock().dependencies.clear();
        session.replace_native_content(&replacement).unwrap();
        write_fixture_file(
            format!("{}/Assets/Scenes/Sample.unity", f.project),
            format!(
                "--- !u!1001 &100
PrefabInstance:
  m_SourcePrefab: {{fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}}
  m_Modification:
    m_TransformParent: {{fileID: 0}}
    m_Modifications:
    - target: {{fileID: {target}, guid: 22222222222222222222222222222222, type: 3}}
      propertyPath: m_Name
      value: Removed object
      objectReference: {{fileID: 0}}
    - target: {{fileID: 10, guid: 22222222222222222222222222222222, type: 3}}
      propertyPath: m_Name
      value: Current instance
      objectReference: {{fileID: 0}}
"
            ),
        );
        let prepared = session.prepare(&f.output);
        if target == "4300000" {
            assert!(
                prepared
                    .as_ref()
                    .is_err_and(|e| message(e).contains("unresolved prefab override")),
                "existing native mesh override must remain rejected: {:?}",
                prepared.as_ref().err().map(message)
            );
            continue;
        }
        if let Err(error) = &prepared {
            panic!(
                "proven stale override prevented loading current model: {}",
                message(error)
            );
        }
        let (objects, _) = session
            .lock()
            .expand("Assets/Scenes/Sample.unity", "", &[])
            .unwrap();
        assert!(
            objects.len() == 2 && str(objects[0].get("m_Name")) == "Current instance",
            "current object override was lost: {objects:?}"
        );
    }
}

#[test]
fn fresh_imported_model_keeps_signed_native_mesh_identity_outside_authored_graph() {
    let (_f, session, mut replacement) = new_native_model_fixture();
    session.lock().dependencies.clear();
    let (mesh_id, filter_id) = ("-2986133483422379211", "-5459214052098992906");
    let mut content_objects = replacement.content.objects.to_vec();
    content_objects[0].file_id = mesh_id.into();
    content_objects.push(NativeContentObject {
        file_id: filter_id.into(),
        type_name: "UnityEngine.MeshFilter".into(),
    });
    replacement.content.objects = List::from(content_objects);
    let mut graph = replacement.model_graph.to_vec();
    let mut references = graph[0].references.to_vec();
    references[0].targets.push(NativeModelGraphTarget {
        file_id: filter_id.into(),
        ..NativeModelGraphTarget::default()
    });
    graph[0].references = List::from(references);
    let reference = |property: &str, id: &str| NativeModelGraphReference {
        property: property.into(),
        targets: List::from(vec![NativeModelGraphTarget {
            file_id: id.into(),
            ..NativeModelGraphTarget::default()
        }]),
    };
    graph.push(NativeModelGraphObject {
        file_id: filter_id.into(),
        class_id: 33,
        scalars: Some(Map::new()),
        references: List::from(vec![
            reference("m_GameObject", "10"),
            reference("m_Mesh", mesh_id),
        ]),
    });
    replacement.model_graph_sha256 = native_model_graph_sha256(&graph);
    replacement.model_graph = List::from(graph);
    session.replace_native_content(&replacement).unwrap();
    let mut s = session.lock();
    let documents = s.document_source(&replacement.source_path).unwrap();
    let mut found = false;
    for document in documents.iter() {
        if document.file_id != filter_id {
            continue;
        }
        let mesh = document.get("m_Mesh");
        assert!(
            file_id(mesh) == mesh_id
                && str(mesh.field("guid")) == replacement.guid
                && str(document.get("m_GameObject").field("guid")).is_empty(),
            "fresh mesh reference/local owner remapped incorrectly: {:?}",
            document.data
        );
        found = true;
    }
    assert!(found, "verified MeshFilter record disappeared");
    // Cached source documents remain local-ID native data; GUID qualification is
    // applied only to the copy used by this authored scene/prefab expansion.
    assert!(
        str(s.models[&replacement.guid][2].get("m_Mesh").field("guid")).is_empty(),
        "model cache changed during expansion"
    );
}

const REMOVED_MODEL_RENDERER: &str = "-5824371076177060002";
const CURRENT_MODEL_ROOT: &str = "919132149155446097";
const CURRENT_MODEL_TRANSFORM: &str = "-8679921383154817045";
const STALE_VARIANT_INSTANCE: &str = "4076523666981031211";
const STALE_VARIANT_GUID: &str = "33333333333333333333333333333333";
const VARIANT: &str = "Assets/Variant.prefab";

fn public_model_stale_override_fixture(target: &str) -> (Fixture, Session) {
    let (f, session, _) = native_fbx_fixture();
    {
        let mut guard = session.lock();
        let s: &mut Inner = &mut guard;
        let mut docs: Vec<Document> = s.models[NATIVE_GUID].as_ref().clone();
        docs[0].file_id = CURRENT_MODEL_ROOT.into();
        docs[1].file_id = CURRENT_MODEL_TRANSFORM.into();
        docs[0].data.as_mut().unwrap().insert(
            "m_Component".into(),
            Value::Array(vec![Value::Object(value::map([(
                "component",
                local(CURRENT_MODEL_TRANSFORM),
            )]))]),
        );
        docs[1]
            .data
            .as_mut()
            .unwrap()
            .insert("m_GameObject".into(), local(CURRENT_MODEL_ROOT));
        s.models.insert(NATIVE_GUID.into(), Arc::new(docs.clone()));
        s.catalog.model_prefabs[0].objects = docs.clone();
        s.catalog.model_prefabs[0].root_file_id = CURRENT_MODEL_ROOT.into();
        s.catalog.success = true;
        s.catalog.cooked_registry_verified = true;
        s.catalog.backend = "unity_editor_public_api".into();
        s.catalog.scene_loader = "raw".into();
        s.catalog.source_dependencies = ["Assets/Native.fbx", "Assets/Native.fbx.meta"]
            .map(|path| Dependency {
                path: path.into(),
                sha256: s.critical_of(path).into(),
                ..Dependency::default()
            })
            .to_vec();
        for doc in &docs {
            s.catalog.assets.push(value::map([
                ("guid", NATIVE_GUID.into()),
                ("path", "Assets/Native.fbx".into()),
                ("fileId", doc.file_id.as_str().into()),
                ("typeName", doc.type_name.as_str().into()),
                ("classId", Value::Int(doc.class_id)),
            ]));
        }
        s.index_dirty = true;
    }
    write_fixture_file(
        format!("{}/{VARIANT}", f.project),
        format!(
            "--- !u!1001 &{STALE_VARIANT_INSTANCE}
PrefabInstance:
  m_SourcePrefab: {{fileID: 100100000, guid: {NATIVE_GUID}, type: 3}}
  m_Modification:
    m_TransformParent: {{fileID: 0}}
    m_Modifications:
    - target: {{fileID: {target}, guid: {NATIVE_GUID}, type: 3}}
      propertyPath: m_Materials.Array.data[0]
      value:
      objectReference: {{fileID: 0}}
    - target: {{fileID: {CURRENT_MODEL_TRANSFORM}, guid: {NATIVE_GUID}, type: 3}}
      propertyPath: m_LocalPosition.x
      value: 3.5
      objectReference: {{fileID: 0}}
"
        ),
    );
    write_fixture_file(
        format!("{}/{VARIANT}.meta", f.project),
        format!("guid: {STALE_VARIANT_GUID}\n"),
    );
    write_fixture_file(
        format!("{}/Assets/Scenes/Sample.unity", f.project),
        format!(
            "--- !u!1001 &9223372036854775806
PrefabInstance:
  m_SourcePrefab: {{fileID: 100100000, guid: {STALE_VARIANT_GUID}, type: 3}}
  m_Modification:
    m_TransformParent: {{fileID: 0}}
    m_Modifications: []
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {{fileID: 9223372036854775806}}
"
        ),
    );
    (f, session)
}

fn rejects_unresolved_override(f: &Fixture, session: &Session) -> bool {
    session
        .prepare(&f.output)
        .is_err_and(|e| message(&e).contains("unresolved prefab override"))
}

#[test]
fn baseline_public_model_ignores_proven_removed_override_with_exact_provenance() {
    let (f, session) = public_model_stale_override_fixture(REMOVED_MODEL_RENDERER);
    let variant_meta = format!("{VARIANT}.meta");
    let sources = [
        "Assets/Native.fbx",
        "Assets/Native.fbx.meta",
        VARIANT,
        variant_meta.as_str(),
        "Assets/Scenes/Sample.unity",
        "Assets/Scenes/Sample.unity.meta",
    ];
    let before: Vec<Vec<u8>> = sources
        .iter()
        .map(|path| fs::read(format!("{}/{path}", f.project)).unwrap())
        .collect();
    let text = |entry: &Value, key: &str| str(entry.field(key)).into_owned();
    for _pass in 0..2 {
        let prepared = session.prepare(&f.output).unwrap_or_else(|e| {
            panic!(
                "complete public inventory should permit only the removed target: {}",
                message(&e)
            )
        });
        let manifest = manifest_json(&prepared.manifest).unwrap();
        let diagnostics = array(value::get(&manifest, "ignoredPrefabOverrideDiagnostics"));
        assert_eq!(
            diagnostics.len(),
            1,
            "diagnostic must be stable across repeat preparation: {diagnostics:?}"
        );
        let entry = &diagnostics[0];
        assert!(
            text(entry, "path") == VARIANT
                && text(entry, "instanceFileId") == STALE_VARIANT_INSTANCE
                && text(entry, "targetGuid") == NATIVE_GUID
                && text(entry, "targetFileId") == REMOVED_MODEL_RENDERER
                && text(entry, "propertyPath") == "m_Materials.Array.data[0]"
                && text(entry, "reason") == "target_absent_from_verified_native_model",
            "lost exact saved override provenance: {entry:?}"
        );
        let s = session.lock();
        assert!(
            text(entry, "proofSourcePath") == "Assets/Native.fbx"
                && text(entry, "proofSourceSha256") == s.critical_of("Assets/Native.fbx")
                && text(entry, "proofMetaSha256") == s.critical_of("Assets/Native.fbx.meta")
                && valid_native_hex(&text(entry, "proofInventorySha256"), 64),
            "ignored override lacks hash-bound current public proof: {entry:?}"
        );
    }
    let graph = session
        .authored_graph("Assets/Scenes/Sample.unity")
        .unwrap();
    assert!(
        graph.ignored_prefab_override_diagnostics.len() == 1 && graph.objects.len() == 3,
        "read graph lost the full model or its diagnostic: {graph:?}"
    );
    let (mut position_preserved, mut identity_preserved) = (false, false);
    for object in &graph.objects {
        if object.document.class_id == 4 {
            position_preserved = str(object.document.get("m_LocalPosition").field("x")) == "3.5";
            for identity in &object.identities {
                identity_preserved = identity_preserved
                    || identity.guid == NATIVE_GUID && identity.file_id == CURRENT_MODEL_TRANSFORM;
            }
        }
    }
    assert!(
        position_preserved && identity_preserved,
        "live override or signed native source identity changed: {:?}",
        graph.objects
    );
    for (path, expected) in sources.iter().zip(&before) {
        assert_eq!(
            &fs::read(format!("{}/{path}", f.project)).unwrap(),
            expected,
            "inspection/preparation modified authored source {path}"
        );
    }
}

#[test]
fn baseline_removed_override_requires_complete_current_public_model() {
    type Change = fn(&mut Inner);
    let cases: [(&str, Change); 13] = [
        ("failed export", |s| s.catalog.success = false),
        ("unverified registry", |s| {
            s.catalog.cooked_registry_verified = false
        }),
        ("native scene", |s| s.catalog.contains_scenes = true),
        ("foreign backend", |s| s.catalog.backend = "unknown".into()),
        ("missing meta proof", |s| {
            s.critical.remove("Assets/Native.fbx.meta");
        }),
        ("catalog source proof missing", |s| {
            s.catalog.source_dependencies.clear()
        }),
        ("baseline masks changed catalog source", |s| {
            s.catalog.source_dependencies[0].sha256 = "0".repeat(64)
        }),
        ("missing root", |s| {
            s.catalog.model_prefabs[0].root_file_id = "0".into()
        }),
        ("incomplete graph", |s| {
            s.catalog.model_prefabs[0].objects.truncate(1)
        }),
        ("incomplete registry", |s| s.catalog.assets.truncate(1)),
        ("existing omitted component", |s| {
            s.catalog.assets.push(value::map([
                ("guid", NATIVE_GUID.into()),
                ("path", "Assets/Native.fbx".into()),
                ("fileId", "7795428923366279233".into()),
                ("typeName", "UnityEngine.MeshRenderer".into()),
                ("classId", Value::Int(23)),
            ]))
        }),
        ("excluded target exists", |s| {
            s.catalog.excluded_runtime_assets = vec![value::map([
                ("guid", NATIVE_GUID.into()),
                ("fileId", REMOVED_MODEL_RENDERER.into()),
            ])]
        }),
        ("known animation target exists", |s| {
            s.catalog.model_animation_clips = vec![value::map([
                ("guid", NATIVE_GUID.into()),
                ("fileId", REMOVED_MODEL_RENDERER.into()),
            ])]
        }),
    ];
    for (name, change) in cases {
        let (f, session) = public_model_stale_override_fixture(REMOVED_MODEL_RENDERER);
        change(&mut session.lock());
        assert!(
            rejects_unresolved_override(&f, &session),
            "unproven stale override was accepted: {name}"
        );
    }
}

#[test]
fn baseline_existing_native_resource_override_remains_rejected() {
    let (f, session) = public_model_stale_override_fixture("4300000");
    assert!(
        rejects_unresolved_override(&f, &session),
        "existing native mesh outside hierarchy was silently treated as removed"
    );
}

#[test]
fn baseline_malformed_native_target_cannot_be_called_removed() {
    for target in ["0", "9223372036854775808", "01", "abc"] {
        let (f, session) = public_model_stale_override_fixture(target);
        assert!(
            session.prepare(&f.output).is_err(),
            "noncanonical signed 64-bit target accepted: {target}"
        );
    }
}

#[test]
fn baseline_stale_override_cannot_bypass_changed_native_source_proof() {
    for path in ["Assets/Native.fbx", "Assets/Native.fbx.meta"] {
        let (f, session) = public_model_stale_override_fixture(REMOVED_MODEL_RENDERER);
        session.prepare(&f.output).unwrap();
        let physical = format!("{}/{path}", f.project);
        let mut bytes = fs::read(&physical).unwrap();
        bytes.extend_from_slice(b"\nchanged importer/source\n");
        write_fixture_file(&physical, bytes);
        let result = session.prepare(&f.output);
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.downcast_ref::<NeedsImportError>().is_some()),
            "stale override bypassed current source hash validation: {path}"
        );
    }
}

#[test]
fn unity_yaml_preserves_identities_and_numbers() {
    let input = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &9223372036854775807\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ABCDEF0123456789ABCDEF0123456789, type: 3}\n  value: 12934340311651418136\n  negative: -0.25\n  trueString: true\n";
    let docs = parse_unity_yaml(input.as_bytes()).unwrap();
    assert!(
        docs[0].file_id == "9223372036854775807" && file_id(docs[0].get("m_Script")) == "11500000",
        "fileID lost precision"
    );
    assert_eq!(
        str(docs[0].get("value")),
        "12934340311651418136",
        "uint64 lost precision"
    );
    assert_eq!(
        docs[0].get("m_Script").field("guid"),
        &Value::String("abcdef0123456789abcdef0123456789".into()),
        "GUID not normalized"
    );
    assert_eq!(
        docs[0].get("trueString"),
        &Value::String("true".into()),
        "implicit boolean changed authored scalar"
    );
}

#[test]
fn unity_yaml_keeps_hash_marks_of_plain_values() {
    let input = "--- !u!1 &1\nGameObject:\n  m_Name: Enemy #1\n  m_Tag: it's #2  \n  list:\n  - a #b\n  - name: c #d\n    other: \"e #f\"\n  # a comment\n  quoted: \"g\n    #h\"\n  flow: {x: 1} # i\n  lead: #j\n  count: 5 # k\n";
    let docs = parse_unity_yaml(input.as_bytes()).unwrap();
    let document = &docs[0];
    assert_eq!(str(document.get("m_Name")), "Enemy #1");
    assert_eq!(str(document.get("m_Tag")), "it's #2");
    assert_eq!(
        serde_json::to_string(document.get("list")).unwrap(),
        r##"["a #b",{"name":"c #d","other":"e #f"}]"##
    );
    assert_eq!(str(document.get("quoted")), "g #h");
    assert_eq!(str(document.get("lead")), "#j");
    assert_eq!(
        serde_json::to_string(&(document.get("flow"), document.get("count"))).unwrap(),
        r#"[{"x":1},5]"#
    );
    assert!(
        parse_unity_yaml(b"--- !u!1 &1\nGameObject:\n  m_Name: a\n    b #c\n").is_err(),
        "accepted a `#` in a folded plain value"
    );
}

#[test]
fn unity_yaml_rejects_ambiguous_graphs() {
    for body in [
        "  a: 1\n  a: 2\n",
        "  a: &shared [1]\n  b: *shared\n",
        "  a: .nan\n",
        "  a: !custom 1\n",
    ] {
        let text = format!("--- !u!1 &1\nGameObject:\n{body}");
        assert!(
            parse_unity_yaml(text.as_bytes()).is_err(),
            "accepted unsupported body {body:?}"
        );
    }
    assert!(
        parse_unity_yaml(b"--- !u!1 &1\nGameObject: {}\n--- !u!4 &1\nTransform: {}\n").is_err(),
        "accepted duplicate identity"
    );
    assert!(
        parse_unity_yaml("x".repeat(crate::yaml::MAX_TEXT_BYTES + 1).as_bytes()).is_err(),
        "accepted oversized text"
    );
}

#[test]
fn cache_ownership_keeps_persistent_state_out_of_unity_library_files() {
    let guard = tempfile::tempdir().unwrap();
    let project = guard.path().to_str().unwrap();
    let join = |relative: &str| crate::project::lexpath::join(&[project, relative]);
    for relative in [
        "Temp/Session/current.json",
        "Library/UnityMicroEditor/baselines/current.json",
    ] {
        assert!(
            is_owned_cache_path(project, &join(relative)),
            "owned cache rejected: {relative}"
        );
    }
    for relative in [
        "Temp",
        "Library/UnityMicroEditor",
        "Library/ScriptAssemblies/Game.dll",
        "Assets/Scene.unity",
        "Library/UnityMicroEditorOther/current.json",
        "../other/Temp/output",
    ] {
        assert!(
            !is_owned_cache_path(project, &join(relative)),
            "foreign/cache-root path accepted: {relative}"
        );
    }
}

#[test]
fn xx_hash32_matches_known_vectors() {
    for (text, hash) in [
        ("", 0x02cc5d05u32),
        ("a", 0x550d7456),
        ("abc", 0x32d153ff),
        // Independently evaluated by the installed NGO package's XXHash.cs,
        // compiled with the Unity SDK and executed by its standalone Mono.
        (
            "GlobalObjectId_V1-3-11111111111111111111111111111111-55-0",
            0x89706b6f,
        ),
        (
            "GlobalObjectId_V1-3-d3492bcf1044c4bbc8cb697713855d96-1979069788112063146-0",
            0x637d7bc2,
        ),
    ] {
        let actual = xx_hash32(text.as_bytes());
        assert_eq!(
            actual, hash,
            "xxhash32 {text:?}: got {actual:08x} expected {hash:08x}"
        );
    }
}

#[test]
fn network_prefab_derived_identity_is_stable_and_leaves_authored_data_untouched() {
    const NETWORK_OBJECT: &str = "Unity.Netcode.NetworkObject";
    let source = value::map([
        ("GlobalObjectIdHash", Value::Int(0)),
        ("SomeSetting", Value::Bool(true)),
    ]);
    let mut record = Expanded {
        file_id: "41:55".into(),
        source_path: "Assets/Net/Player.prefab".into(),
        guid: FIXTURE_GUID.into(),
        targets: BTreeSet::from([target_key(FIXTURE_GUID, "55")]),
        ..Expanded::default()
    };
    // `None` stands for the unchanged `source`.
    let derive = |record: &Expanded, source: &Map| {
        derived_network_data(record, NETWORK_OBJECT, source)
            .map(|derived| derived.unwrap_or_else(|| source.clone()))
    };
    let first = derive(&record, &source).unwrap();
    assert!(
        number(value::get(&first, "GlobalObjectIdHash")) != 0,
        "identity unavailable {first:?}"
    );
    record.file_id = "999:55".into();
    let second = derive(&record, &source).unwrap();
    assert_eq!(
        first, second,
        "same source prefab acquired instance-dependent identity"
    );
    assert!(
        source["GlobalObjectIdHash"] == Value::Int(0) && first["SomeSetting"] == Value::Bool(true),
        "authored source or unrelated settings changed"
    );
    let existing = value::map([("GlobalObjectIdHash", Value::Int(123))]);
    let retained = derive(&record, &existing).unwrap();
    assert_eq!(
        retained, existing,
        "replaced an already validated source hash"
    );
    record.source_path = "Assets/Net/Boot.unity".into();
    assert!(
        derive(&record, &source).is_err(),
        "silently invented an in-scene identity"
    );
}

#[test]
fn only_declared_package_sources_may_read_library() {
    let guard = tempfile::tempdir().unwrap();
    let project = guard.path().canonicalize().unwrap();
    let project = project.to_str().unwrap();
    let package_root = format!("{project}/Library/PackageCache/test.runtime@abc");
    let source = format!("{package_root}/Script.cs");
    write_fixture_file(&source, "source");
    let session = Session::authored_only(project).unwrap();
    let mut s = session.lock();
    assert!(
        s.checked_path(&source, &package_root).is_err(),
        "undeclared Library source accepted"
    );
    s.catalog.source_roots = vec![SourceRoot {
        path: "Packages/test.runtime".into(),
        source: package_root.clone(),
    }];
    s.checked_path(&source, &package_root).unwrap();
    let artifact_root = format!("{project}/Library/Artifacts/fake");
    let artifact = format!("{artifact_root}/Script.cs");
    write_fixture_file(&artifact, "not package source");
    s.catalog.source_roots.push(SourceRoot {
        path: "Packages/fake".into(),
        source: artifact_root.clone(),
    });
    assert!(
        s.checked_path(&artifact, &artifact_root).is_err(),
        "Library artifact accepted as package source"
    );
    let outside = format!("{project}/Library/ScriptAssemblies/Unsafe.cs");
    write_fixture_file(&outside, "outside");
    assert!(
        s.checked_path(&outside, &package_root).is_err(),
        "package root escape accepted"
    );
}
