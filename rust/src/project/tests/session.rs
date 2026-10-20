//! Tests of the session, scene selection, native expansion, prefab override proofs, YAML,
//! cache paths, netcode data and package sources.
use super::{FIXTURE_SCENE, Fixture, data_json, fixture_session, write_fixture_file};
use crate::project::{
    Options, Session, is_owned_cache_path, manifest_json, parse_unity_yaml,
    session::SourceRoot,
    value::{self, Map, Value, array, file_id, number, str},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    process::Command,
};

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
fn objects_of(manifest: &Map) -> &[Value] {
    array(value::get(manifest, "objects"))
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

#[test]
fn a_scene_is_asked_for_by_its_sources_and_published_once_written() {
    use crate::project::{
        NativeContentDependency, NativeContentFile, NativeContentObject, NativeScene,
        NativeSceneFile, NeedsNativeScene,
    };
    let fixture = fixture_session();
    let needed = |fixture: &Fixture| {
        let error = fixture.session.prepare(&fixture.output).unwrap_err();
        let needed = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<NeedsNativeScene>())
            .expect("the session asks for its scene");
        (needed.scene.clone(), needed.key.clone())
    };
    let (scene, key) = needed(&fixture);
    assert_eq!(scene, "Assets/Scenes/Sample.unity");
    // Files the worker wrote, kept where the session takes native content from.
    let store = format!(
        "{}/Library/UnityMicroEditor/native-store/baked",
        fixture.project
    );
    fs::create_dir_all(&store).unwrap();
    let write = |name: &str, data: &[u8]| {
        let path = format!("{store}/{name}");
        fs::write(&path, data).unwrap();
        (path, crate::sha256(data))
    };
    let (scene_path, scene_sha) = write("scene", b"scene");
    let (deps_path, deps_sha) = write("dependencies", b"dependencies");
    let deps_guid = "44444444444444444444444444444444";
    let mut content = NativeContentFile {
        guid: deps_guid.into(),
        path: deps_path,
        sha256: deps_sha,
        ..Default::default()
    };
    content.objects.push(NativeContentObject {
        file_id: "1".into(),
        type_name: "UnityEngine.Material".into(),
    });
    let mut file = NativeSceneFile {
        path: scene_path.clone(),
        sha256: scene_sha,
        ..Default::default()
    };
    file.dependencies.push(NativeContentDependency {
        guid: deps_guid.into(),
        global: false,
    });
    fixture.session.set_native_scene(NativeScene {
        key: key.clone(),
        scene: file,
        content: vec![content],
        published: Vec::new(),
        objects: BTreeMap::new(),
        roots: BTreeMap::new(),
    });
    let prepared = fixture.session.prepare(&fixture.output).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&prepared.manifest).unwrap()).unwrap();
    assert_eq!(manifest["nativeScene"]["path"], scene_path.as_str());
    assert_eq!(
        manifest["nativeScene"]["dependencies"][0]["guid"],
        deps_guid
    );
    assert_eq!(manifest["nativeContentFiles"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["objects"].as_array().unwrap().len(), 0);
    // An edit of the scene asks for it again under another key.
    write_fixture_file(&fixture.scene, FIXTURE_SCENE.replace("Original", "Edited"));
    fixture.session.invalidate(std::slice::from_ref(&fixture.scene));
    let (_, edited) = needed(&fixture);
    assert_ne!(edited, key);
}
