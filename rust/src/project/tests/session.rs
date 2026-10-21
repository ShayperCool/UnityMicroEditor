//! Tests of the session, scene selection, native expansion, prefab override proofs, YAML,
//! cache paths, netcode data and package sources.
use super::{FIXTURE_SCENE, Fixture, fixture_session, write_fixture_file};
use crate::project::{
    Options, Session, is_owned_cache_path, parse_unity_yaml,
    session::SourceRoot,
    value::{self, Value, file_id, number, str},
};
use std::{collections::BTreeMap, fs};

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
        assets: BTreeMap::new(),
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
    fixture
        .session
        .invalidate(std::slice::from_ref(&fixture.scene));
    let (_, edited) = needed(&fixture);
    assert_ne!(edited, key);
}

#[test]
fn an_addressed_asset_is_named_by_the_objects_the_worker_wrote_main_first() {
    use crate::project::{
        NativeContentFile, NativeScene, NativeSceneFile, NeedsNativeScene, Options, RefusedPrefab,
    };
    let mut fixture = fixture_session();
    let material = "Assets/Art/Glow.mat";
    let guid = "55555555555555555555555555555555";
    write_fixture_file(
        format!("{}/{material}", fixture.project),
        "%YAML 1.1\n--- !u!21 &2100000\nMaterial:\n  m_Name: Glow\n",
    );
    write_fixture_file(
        format!("{}/{material}.meta", fixture.project),
        format!("guid: {guid}\n"),
    );
    fixture.session = Session::new(Options {
        project: fixture.project.clone(),
        manifest: format!("{}/baseline.json", fixture.project),
        extra_assets: vec![material.into()],
        ..Options::default()
    })
    .unwrap();
    let error = fixture.session.prepare(&fixture.output).unwrap_err();
    let key = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<NeedsNativeScene>())
        .expect("the session asks for its scene")
        .key
        .clone();
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
    let content = NativeContentFile {
        guid: "44444444444444444444444444444444".into(),
        path: deps_path,
        sha256: deps_sha,
        ..Default::default()
    };
    let written = |assets: Vec<&str>| NativeScene {
        key: key.clone(),
        scene: NativeSceneFile {
            path: scene_path.clone(),
            sha256: scene_sha.clone(),
            ..Default::default()
        },
        content: vec![content.clone()],
        published: Vec::new(),
        // The material and a sub-object of it the file holds; a third the
        // worker reported but left out of the file is not named.
        objects: BTreeMap::from([
            (format!("{guid}/2100000"), "7".to_string()),
            (format!("{guid}/-301"), "8".to_string()),
        ]),
        roots: BTreeMap::new(),
        assets: BTreeMap::from([(
            guid.to_string(),
            assets.into_iter().map(String::from).collect(),
        )]),
    };
    fixture
        .session
        .set_native_scene(written(vec!["2100000", "999", "-301"]));
    let prepared = fixture.session.prepare(&fixture.output).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&prepared.manifest).unwrap()).unwrap();
    let aliases: Vec<(String, String)> = manifest["nativeScene"]["aliases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|alias| alias["guid"] == guid)
        .map(|alias| {
            (
                alias["fileId"].as_str().unwrap().to_string(),
                alias["localId"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        aliases,
        [
            ("2100000".to_string(), "7".to_string()),
            ("-301".to_string(), "8".to_string())
        ]
    );
    // An asset of which the file holds nothing is refused by its path, so
    // that the runner gives up that one asset.
    fixture.session.set_native_scene(written(vec!["999"]));
    let error = fixture.session.prepare(&fixture.output).unwrap_err();
    let refused = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RefusedPrefab>())
        .expect("the asset is refused");
    assert_eq!(refused.path, material);
}
