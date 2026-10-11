//! Tests of native overlays: models, shaders, UI, Mesh CPU retention, built-in Shader and
//! compiled Shader snapshots.
use super::{FIXTURE_SCENE, Fixture, fixture_session, message, write_fixture_file};
use crate::project::{
    BUILTIN_RESOURCE_GUID, CompiledShaderSnapshot, Document, List, NativeContentDependency,
    NativeContentFile, NativeContentObject, NativeContentResource, NativeDeferredBinding,
    NativeMeshCPURetention, NativeModelGraphObject, NativeModelGraphReference,
    NativeModelGraphTarget, NativeReplacement, NativeUIBinding, NativeUIBindingProof,
    NeedsImportError, Options, RetainedBuiltinShader, Session, collect_shader_source_requirements,
    expand::apply_override,
    manifest_json,
    native::{match_native_model_graph, native_content_class, native_scalar_equal},
    native_model_graph_sha256,
    native_ui::{validate_native_mesh_cpu_retention, validate_native_ui_bindings},
    parse_unity_yaml,
    session::asset_key,
    value::{self, Map, Value, array, file_id, number, object, str},
};
use std::{collections::BTreeMap, fs, sync::Arc};

const NATIVE_FIXTURE_GUID: &str = "22222222222222222222222222222222";
const OTHER_GUID: &str = "33333333333333333333333333333333";

fn native_fixture_hash(path: &str) -> String {
    crate::sha256(&fs::read(path).unwrap())
}
fn native_object(file_id: &str, type_name: &str) -> NativeContentObject {
    NativeContentObject {
        file_id: file_id.into(),
        type_name: type_name.into(),
    }
}
fn target(file_id: &str) -> NativeModelGraphTarget {
    NativeModelGraphTarget {
        file_id: file_id.into(),
        guid: String::new(),
    }
}
fn reference(property: &str, targets: &[&str]) -> NativeModelGraphReference {
    NativeModelGraphReference {
        property: property.into(),
        targets: targets.iter().map(|id| target(id)).collect(),
    }
}
fn items<T>(list: &mut List<T>) -> &mut Vec<T> {
    list.0.get_or_insert_with(Vec::new)
}
fn zeros() -> String {
    "0".repeat(64)
}
fn needs_import(error: &anyhow::Error) -> Option<&NeedsImportError> {
    error.downcast_ref::<NeedsImportError>()
}

struct NativeFixture {
    fixture: Fixture,
    replacement: NativeReplacement,
    baseline: String,
    catalog: String,
}
impl NativeFixture {
    fn session(&self) -> &Session {
        &self.fixture.session
    }
    fn replace(&self) -> anyhow::Result<()> {
        self.session().replace_native_content(&self.replacement)
    }
}

fn native_fixture(extension: &str) -> NativeFixture {
    let mut fixture = fixture_session();
    let project = fixture.project.clone();
    let logical = format!("Assets/Native{extension}");
    let source = format!("{project}/{logical}");
    write_fixture_file(&source, "original imported source");
    write_fixture_file(
        format!("{source}.meta"),
        format!("fileFormatVersion: 2\nguid: {NATIVE_FIXTURE_GUID}\n"),
    );
    let mut asset = serde_json::json!({"kind":"imported","guid":NATIVE_FIXTURE_GUID,"path":logical,"fileId":"8300000","typeName":"UnityEngine.AudioClip","classId":83});
    if extension == ".fbx" {
        asset["fileId"] = "4300000".into();
        asset["typeName"] = "UnityEngine.Mesh".into();
        asset["classId"] = 43.into();
    }
    let baseline_value = serde_json::json!({"schemaVersion":1,"supported":true,"unityVersion":"6000.6.3f1","projectRoot":project,"scenePath":"Assets/Scenes/Sample.unity","sceneName":"Sample","objects":[],"assets":[asset],"assemblies":["existing.dll"],
        "sourceDependencies":[
            {"path":logical,"sha256":native_fixture_hash(&source)},
            {"path":format!("{logical}.meta"),"sha256":native_fixture_hash(&format!("{source}.meta"))}]});
    let baseline = format!("{project}/baseline.json");
    fs::write(&baseline, baseline_value.to_string()).unwrap();
    let catalog = format!("{project}/catalog.json");
    let mut cat = serde_json::json!({"schemaVersion":1,"engineVersion":"6000.6.3f1","project":project,"assets":[asset]});
    if extension == ".fbx" {
        let model = FIXTURE_SCENE.split("--- !u!1660057539").next().unwrap();
        let mut docs = parse_unity_yaml(model.as_bytes()).unwrap();
        for doc in &mut docs {
            doc.type_name = format!("UnityEngine.{}", doc.type_name);
        }
        let data = docs[0].data.as_mut().unwrap();
        data.insert("m_Layer".into(), Value::Int(0));
        data.insert("m_TagString".into(), "Untagged".into());
        data.insert("m_StaticEditorFlags".into(), Value::Int(0));
        let objects: serde_json::Value =
            serde_json::from_str(&value::encode_json(&docs).unwrap()).unwrap();
        cat["modelPrefabs"] =
            serde_json::json!([{"guid":NATIVE_FIXTURE_GUID,"path":logical,"objects":objects}]);
    }
    fs::write(&catalog, cat.to_string()).unwrap();
    fixture.session = Session::new(Options {
        project: project.clone(),
        manifest: baseline.clone(),
        native_catalog: catalog.clone(),
        ..Options::default()
    })
    .unwrap();
    let runtime = format!("{}/native/runtime.bin", fixture.output);
    let resource = format!("{}/native/audio.resource", fixture.output);
    write_fixture_file(&runtime, "independently adapted immutable runtime file");
    write_fixture_file(&resource, "immutable streamed audio payload");
    let mut replacement = NativeReplacement {
        guid: NATIVE_FIXTURE_GUID.into(),
        source_path: logical.clone(),
        source_sha256: native_fixture_hash(&source),
        meta_sha256: native_fixture_hash(&format!("{source}.meta")),
        content: NativeContentFile {
            guid: NATIVE_FIXTURE_GUID.into(),
            path: runtime.clone(),
            sha256: native_fixture_hash(&runtime),
            objects: List::from(vec![native_object(
                asset["fileId"].as_str().unwrap(),
                asset["typeName"].as_str().unwrap(),
            )]),
            ..NativeContentFile::default()
        },
        ..NativeReplacement::default()
    };
    if extension == ".wav" {
        replacement.content.resources = List::from(vec![NativeContentResource {
            path: resource.clone(),
            virtual_path: format!("VirtualArtifacts/Primary/{NATIVE_FIXTURE_GUID}.resource"),
            sha256: native_fixture_hash(&resource),
        }]);
    } else {
        let s = fixture.session.lock();
        for doc in s.models[NATIVE_FIXTURE_GUID].iter() {
            replacement
                .content
                .objects
                .push(native_object(&doc.file_id, &doc.type_name));
        }
        replacement.model_graph = List::from(s.native_model_graph(NATIVE_FIXTURE_GUID).unwrap());
    }
    NativeFixture {
        fixture,
        replacement,
        baseline,
        catalog,
    }
}

#[test]
fn native_wav_overlay_refreshes_hash_and_preserves_baseline_and_revisions() {
    let mut f = native_fixture(".wav");
    let output = f.fixture.output.clone();
    let original_baseline = fs::read(&f.baseline).unwrap();
    let original_catalog = fs::read(&f.catalog).unwrap();
    let before = f.session().prepare(&output).unwrap();
    let old_revision = fs::read(&before.manifest).unwrap();
    let source = format!("{}/{}", f.fixture.project, f.replacement.source_path);
    write_fixture_file(&source, "changed source audio");
    f.replacement.source_sha256 = native_fixture_hash(&source);
    assert!(
        f.session().prepare(&output).is_err(),
        "published changed WAV without importer"
    );
    f.replace().unwrap();
    // Ownership of all nested replacement fields belongs to the session.
    items(&mut f.replacement.content.objects)[0].type_name = "modified by caller".into();
    let prepared = f.session().prepare(&output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let content = object(&array(value::get(&manifest, "nativeContentFiles"))[0]);
    assert!(
        value::text(content, "guid") == NATIVE_FIXTURE_GUID
            && str(array(value::get(content, "objects"))[0].field("typeName"))
                == "UnityEngine.AudioClip",
        "wrong native overlay: {content:?}"
    );
    let mut found = false;
    for v in array(value::get(&manifest, "sourceDependencies")) {
        let d = object(v);
        if value::text(d, "path") == f.replacement.source_path {
            found = value::text(d, "sha256") == f.replacement.source_sha256;
        }
    }
    assert!(found, "source dependency retained stale hash");
    assert!(
        original_baseline == fs::read(&f.baseline).unwrap()
            && original_catalog == fs::read(&f.catalog).unwrap()
            && old_revision == fs::read(&before.manifest).unwrap(),
        "an immutable baseline, catalog, or published revision changed"
    );
    let warm = f.session().prepare(&output).unwrap();
    assert_eq!(
        warm.cache.file_reads, 0,
        "warm overlay verification reread immutable files: {:?}",
        warm.cache
    );
    write_fixture_file(&source, "changed a second time");
    let error = f.session().prepare(&output).unwrap_err();
    assert!(
        needs_import(&error).is_some(),
        "subsequent source changes must require an import: {}",
        message(&error)
    );
}

#[test]
fn native_replacement_fails_closed_before_changing_overlay() {
    type Change = fn(&mut NativeReplacement);
    let cases: [(&str, Change); 13] = [
        ("source hash", |r| r.source_sha256 = zeros()),
        ("metadata hash", |r| r.meta_sha256 = zeros()),
        ("unknown GUID", |r| {
            r.guid = OTHER_GUID.into();
            r.content.guid = OTHER_GUID.into();
        }),
        ("wrong source", |r| {
            r.source_path = "Assets/Other.wav".into()
        }),
        ("missing object", |r| r.content.objects = List::nil()),
        ("duplicate object", |r| {
            let first = r.content.objects[0].clone();
            r.content.objects.push(first);
        }),
        ("changed type", |r| {
            items(&mut r.content.objects)[0].type_name = "UnityEngine.Mesh".into()
        }),
        ("changed ID", |r| {
            items(&mut r.content.objects)[0].file_id = "8300001".into()
        }),
        ("runtime hash", |r| r.content.sha256 = zeros()),
        ("resource hash", |r| {
            items(&mut r.content.resources)[0].sha256 = zeros()
        }),
        ("resource alias", |r| {
            items(&mut r.content.resources)[0].virtual_path = "../../other".into()
        }),
        ("unknown dependency", |r| {
            r.content.dependencies = List::from(vec![NativeContentDependency {
                guid: OTHER_GUID.into(),
                global: false,
            }])
        }),
        ("fake global", |r| {
            r.content.dependencies = List::from(vec![NativeContentDependency {
                guid: OTHER_GUID.into(),
                global: true,
            }])
        }),
    ];
    for (name, change) in cases {
        let mut f = native_fixture(".wav");
        let before = f.session().lock().critical.clone();
        change(&mut f.replacement);
        assert!(
            f.replace().is_err(),
            "{name}: accepted invalid native replacement"
        );
        {
            let s = f.session().lock();
            assert!(
                s.native_overlays.is_empty() && before == s.critical,
                "{name}: failed replacement changed session expectations"
            );
        }
        if let Err(error) = f.session().prepare(&f.fixture.output) {
            panic!(
                "{name}: failed replacement invalidated original source {}",
                message(&error)
            );
        }
    }
}

#[test]
fn native_overlay_rejects_artifacts_changed_after_acceptance() {
    for file in ["runtime", "resource"] {
        let f = native_fixture(".wav");
        f.replace().unwrap();
        let first = f.session().prepare(&f.fixture.output).unwrap();
        let path = if file == "resource" {
            f.replacement.content.resources[0].path.clone()
        } else {
            f.replacement.content.path.clone()
        };
        write_fixture_file(&path, "changed after acceptance");
        assert!(
            f.session().prepare(&f.fixture.output).is_err(),
            "{file}: published changed native artifact"
        );
        let revision = std::path::Path::new(&first.manifest).parent().unwrap();
        assert_eq!(
            fs::read_dir(revision).unwrap().count(),
            1,
            "{file}: previous manifest disappeared"
        );
        assert_eq!(
            fs::read_dir(revision.parent().unwrap()).unwrap().count(),
            1,
            "{file}: failed verification published a new revision"
        );
    }
}

#[test]
fn native_fbx_replacement_requires_identical_references_and_authored_fields() {
    type Change = fn(&mut NativeReplacement);
    let cases: [(&str, Change); 7] = [
        ("unchanged", |_| {}),
        ("missing graph", |r| r.model_graph = List::nil()),
        ("changed owner", |r| {
            let transform = &mut items(&mut r.model_graph)[1];
            items(&mut items(&mut transform.references)[0].targets)[0].file_id = "20".into();
        }),
        ("changed name", |r| {
            let scalars = items(&mut r.model_graph)[0].scalars.as_mut().unwrap();
            scalars.insert("m_Name".into(), "different".into());
        }),
        ("changed transform", |r| {
            let scalars = items(&mut r.model_graph)[1].scalars.as_mut().unwrap();
            let position = scalars.get_mut("m_LocalPosition").unwrap();
            position
                .as_object_mut()
                .unwrap()
                .insert("x".into(), Value::Int(1));
        }),
        ("unknown scalar", |r| {
            let scalars = items(&mut r.model_graph)[0].scalars.as_mut().unwrap();
            scalars.insert("unexpected".into(), Value::Int(1));
        }),
        ("missing ID", |r| items(&mut r.content.objects).truncate(2)),
    ];
    for (name, change) in cases {
        let mut f = native_fixture(".fbx");
        change(&mut f.replacement);
        let result = f.replace();
        if name != "unchanged" {
            assert!(result.is_err(), "{name}: accepted changed model graph");
            continue;
        }
        result.unwrap();
        f.session().prepare(&f.fixture.output).unwrap();
    }
    // Unity exports binary32 values through decimal JSON; those decimal spellings
    // need to compare at the original native precision.
    let a = Value::Object(value::map([("x", Value::Number("0.1".into()))]));
    let b = Value::Object(value::map([("x", Value::Float(0.1f32 as f64))]));
    assert!(
        native_scalar_equal(&a, &b, true) && !native_scalar_equal(&a, &b, false),
        "model precision normalizer is incorrect"
    );
}

#[test]
fn native_replacement_rejects_cross_overlay_dependency_cycle() {
    let f = native_fixture(".wav");
    let mut first = f.replacement.clone();
    let session = f.session();
    session.replace_native_content(&first).unwrap();
    let guid = OTHER_GUID;
    let path = "Assets/Second.wav";
    let source = format!("{}/{path}", f.fixture.project);
    write_fixture_file(&source, "second source");
    write_fixture_file(format!("{source}.meta"), format!("guid: {guid}\n"));
    session.lock().assets.insert(
        asset_key(guid, "8300000"),
        value::map([
            ("kind", "imported".into()),
            ("guid", guid.into()),
            ("path", path.into()),
            ("fileId", "8300000".into()),
            ("typeName", "UnityEngine.AudioClip".into()),
        ]),
    );
    let mut second = NativeReplacement {
        guid: guid.into(),
        source_path: path.into(),
        source_sha256: native_fixture_hash(&source),
        meta_sha256: native_fixture_hash(&format!("{source}.meta")),
        content: first.content.clone(),
        ..NativeReplacement::default()
    };
    second.content.guid = guid.into();
    second.content.resources = List::nil();
    second.content.dependencies = List::from(vec![NativeContentDependency {
        guid: first.guid.clone(),
        global: false,
    }]);
    session.replace_native_content(&second).unwrap();
    first.content.dependencies = List::from(vec![NativeContentDependency {
        guid: guid.into(),
        global: false,
    }]);
    assert!(
        session.replace_native_content(&first).is_err(),
        "accepted a cycle between previously valid overlays"
    );
    assert!(
        session.lock().native_overlays[&first.guid]
            .content
            .dependencies
            .is_empty(),
        "failed cycle validation replaced valid overlay"
    );
}

fn new_native_texture_fixture() -> (Fixture, NativeReplacement) {
    let fixture = fixture_session();
    let logical = "Assets/NewTexture.png";
    let source = format!("{}/{logical}", fixture.project);
    write_fixture_file(&source, "native texture source bytes");
    write_fixture_file(
        format!("{source}.meta"),
        format!(
            "fileFormatVersion: 2\nguid: {NATIVE_FIXTURE_GUID}\nTextureImporter:\n  textureType: 0\n"
        ),
    );
    let runtime = format!("{}/native/texture.bin", fixture.output);
    write_fixture_file(&runtime, "independently validated native texture artifact");
    let r = NativeReplacement {
        guid: NATIVE_FIXTURE_GUID.into(),
        source_path: logical.into(),
        source_sha256: native_fixture_hash(&source),
        meta_sha256: native_fixture_hash(&format!("{source}.meta")),
        content: NativeContentFile {
            guid: NATIVE_FIXTURE_GUID.into(),
            path: runtime.clone(),
            sha256: native_fixture_hash(&runtime),
            objects: List::from(vec![native_object("2800000", "UnityEngine.Texture2D")]),
            ..NativeContentFile::default()
        },
        ..NativeReplacement::default()
    };
    (fixture, r)
}

#[test]
fn new_native_project_texture_publishes_without_mutating_baseline() {
    let (f, mut r) = new_native_texture_fixture();
    let baseline = f.session.lock().baseline.clone();
    let before = f.session.prepare(&f.output).unwrap();
    let old_revision = fs::read(&before.manifest).unwrap();
    f.session.replace_native_content(&r).unwrap();
    items(&mut r.content.objects)[0].type_name = "caller mutated descriptor".into();
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let mut texture: Option<&Map> = None;
    for value in array(value::get(&manifest, "assets")) {
        let asset = object(value);
        if value::text(asset, "guid") == NATIVE_FIXTURE_GUID {
            texture = Some(asset);
        }
    }
    assert!(
        texture.is_some_and(|texture| value::text(texture, "kind") == "imported"
            && value::text(texture, "fileId") == "2800000"
            && value::text(texture, "typeName") == "UnityEngine.Texture2D"
            && value::text(texture, "path") == r.source_path),
        "new source did not publish verified native identity: {texture:?}"
    );
    assert!(
        array(value::get(&manifest, "nativeContentFiles")).len() == 1
            && baseline == f.session.lock().baseline,
        "native append changed baseline or omitted runtime descriptor"
    );
    assert!(
        fs::read(&before.manifest).unwrap() == old_revision,
        "native append changed existing immutable revision"
    );
    write_fixture_file(
        format!("{}/{}", f.project, r.source_path),
        "changed texture pixels",
    );
    let error = f.session.prepare(&f.output).unwrap_err();
    assert!(
        needs_import(&error).is_some(),
        "accepted texture change without an importer: {}",
        message(&error)
    );
}

/// A baked controller's behaviour holds no pointer out of its file: the
/// session publishes the binding and brings the target into the closure.
#[test]
fn baked_controller_publishes_the_bindings_its_file_leaves_null() {
    let fixture = fixture_session();
    let logical = "Assets/New.controller";
    let source = format!("{}/{logical}", fixture.project);
    write_fixture_file(&source, "authored controller");
    write_fixture_file(
        format!("{source}.meta"),
        format!("fileFormatVersion: 2\nguid: {NATIVE_FIXTURE_GUID}\n"),
    );
    let target = format!("{}/Assets/Target.prefab", fixture.project);
    write_fixture_file(
        &target,
        "%YAML 1.1\n--- !u!1 &100\nGameObject:\n  m_Component:\n  - component: {fileID: 101}\n  m_Name: Target\n  m_IsActive: 1\n--- !u!4 &101\nTransform:\n  m_GameObject: {fileID: 100}\n  m_LocalPosition: {x: 0, y: 0, z: 0}\n  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}\n  m_LocalScale: {x: 1, y: 1, z: 1}\n  m_Father: {fileID: 0}\n  m_Children: []\n",
    );
    write_fixture_file(format!("{target}.meta"), format!("guid: {OTHER_GUID}\n"));
    let runtime = format!("{}/native/controller.bin", fixture.output);
    write_fixture_file(&runtime, "baked controller");
    let binding = |owner: &str, property: &str| NativeDeferredBinding {
        owner_file_id: owner.into(),
        property_path: property.into(),
        target_guid: OTHER_GUID.into(),
        target_file_id: "100".into(),
    };
    let mut r = NativeReplacement {
        guid: NATIVE_FIXTURE_GUID.into(),
        source_path: logical.into(),
        source_sha256: native_fixture_hash(&source),
        meta_sha256: native_fixture_hash(&format!("{source}.meta")),
        content: NativeContentFile {
            guid: NATIVE_FIXTURE_GUID.into(),
            path: runtime.clone(),
            sha256: native_fixture_hash(&runtime),
            objects: List::from(vec![
                native_object("9100000", "UnityEngine.RuntimeAnimatorController"),
                native_object("500", "Game.Behaviour"),
            ]),
            ..NativeContentFile::default()
        },
        ..NativeReplacement::default()
    };
    for (owner, property) in [
        ("9100000", "prefab"),
        ("500", "prefab[x]"),
        ("501", "prefab"),
    ] {
        r.deferred_bindings = List::from(vec![binding(owner, property)]);
        let error = fixture.session.replace_native_content(&r).unwrap_err();
        assert!(
            message(&error).contains("native deferred binding is invalid"),
            "accepted binding {owner}.{property}: {}",
            message(&error)
        );
    }
    r.deferred_bindings = List::from(vec![binding("500", "items[1].prefab")]);
    fixture.session.replace_native_content(&r).unwrap();
    let prepared = fixture.session.prepare(&fixture.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let bindings = array(value::get(&manifest, "importedAssetBindings"));
    assert!(
        bindings.len() == 1
            && value::text(object(&bindings[0]), "ownerGuid") == NATIVE_FIXTURE_GUID
            && value::text(object(&bindings[0]), "ownerFileId") == "500"
            && value::text(object(&bindings[0]), "propertyPath") == "items[1].prefab"
            && value::text(object(&bindings[0]), "targetGuid") == OTHER_GUID
            && value::text(object(&bindings[0]), "targetFileId") == "100"
            && value::text(object(&bindings[0]), "targetPath") == "Assets/Target.prefab",
        "binding was not published: {bindings:?}"
    );
    assert!(
        array(value::get(&manifest, "assets")).iter().any(|asset| {
            value::text(object(asset), "guid") == OTHER_GUID
                && value::text(object(asset), "kind") == "prefab"
        }),
        "binding target is outside the published closure"
    );
}

#[test]
fn new_native_project_source_rejects_invalid_ownership_and_runtime_identity() {
    for name in [
        "wrong GUID",
        "duplicate GUID",
        "wrong type",
        "wrong ID",
        "extra object",
        "graph",
        "stale source",
    ] {
        let (f, mut r) = new_native_texture_fixture();
        let before = f.session.lock().assets.clone();
        match name {
            "wrong GUID" => {
                r.guid = OTHER_GUID.into();
                r.content.guid = r.guid.clone();
            }
            "duplicate GUID" => {
                write_fixture_file(
                    format!("{}/Assets/Duplicate.png", f.project),
                    "duplicate source",
                );
                write_fixture_file(
                    format!("{}/Assets/Duplicate.png.meta", f.project),
                    format!("guid: {}\n", r.guid),
                );
            }
            "wrong type" => {
                items(&mut r.content.objects)[0].type_name = "UnityEngine.AudioClip".into()
            }
            "wrong ID" => items(&mut r.content.objects)[0].file_id = "2800001".into(),
            "extra object" => r
                .content
                .objects
                .push(native_object("21300000", "UnityEngine.Sprite")),
            "graph" => {
                r.model_graph = List::from(vec![NativeModelGraphObject {
                    file_id: "1".into(),
                    class_id: 1,
                    ..NativeModelGraphObject::default()
                }])
            }
            _ => write_fixture_file(
                format!("{}/{}", f.project, r.source_path),
                "pixels changed after native import",
            ),
        }
        assert!(
            f.session.replace_native_content(&r).is_err(),
            "{name}: accepted invalid new native source"
        );
        let s = f.session.lock();
        assert!(
            s.native_overlays.is_empty() && before == s.assets,
            "{name}: failed new source registration changed published native identities"
        );
    }
}

#[test]
fn new_native_ui_and_font_sources_publish_exact_objects() {
    let profiles = [
        (
            ".uss",
            vec![native_object(
                "7433441132597879392",
                "UnityEngine.UIElements.StyleSheet",
            )],
        ),
        (
            ".uxml",
            vec![
                native_object(
                    "9197481963319205126",
                    "UnityEngine.UIElements.VisualTreeAsset",
                ),
                native_object("1068529831357515114", "UnityEngine.UIElements.StyleSheet"),
            ],
        ),
        (
            ".ttf",
            vec![
                native_object("12800000", "UnityEngine.Font"),
                native_object("2100000", "UnityEngine.Material"),
                native_object("2800000", "UnityEngine.Texture2D"),
            ],
        ),
    ];
    for (extension, identities) in profiles {
        let (f, mut r) = new_native_texture_fixture();
        let old = format!("{}/{}", f.project, r.source_path);
        r.source_path = format!("Assets/NewNativeUI{extension}");
        let source = format!("{}/{}", f.project, r.source_path);
        fs::rename(&old, &source).unwrap();
        fs::rename(format!("{old}.meta"), format!("{source}.meta")).unwrap();
        r.content.objects = List::from(identities.clone());
        f.session.replace_native_content(&r).unwrap();
        let prepared = f.session.prepare(&f.output).unwrap();
        let manifest = manifest_json(&prepared.manifest).unwrap();
        let mut seen = 0;
        for value in array(value::get(&manifest, "assets")) {
            let asset = object(value);
            if value::text(asset, "guid") == r.guid {
                seen += 1;
                assert!(
                    value::text(asset, "kind") == "imported"
                        && native_content_class(&value::text(asset, "typeName")) != 0,
                    "{extension}: native UI asset publication omitted exact type"
                );
            }
        }
        assert_eq!(
            seen,
            identities.len(),
            "{extension}: native UI subasset identities were lost"
        );
        let mut bad = r.clone();
        items(&mut bad.content.objects)[0].type_name = "UnityEditor.StyleSheetImporter".into();
        assert!(
            f.session.replace_native_content(&bad).is_err(),
            "{extension}: Editor importer identity became a runtime object"
        );
    }
}

#[test]
fn native_texture_takes_over_existing_raw_texture_settings() {
    let (f, r) = new_native_texture_fixture();
    let key = asset_key(&r.guid, "2800000");
    f.session.lock().assets.insert(
        key.clone(),
        value::map([
            ("kind", "texture".into()),
            ("guid", r.guid.as_str().into()),
            ("fileId", "2800000".into()),
            ("path", r.source_path.as_str().into()),
            ("classId", Value::Int(28)),
            ("typeName", "UnityEngine.Texture2D".into()),
        ]),
    );
    let baseline = f.session.lock().baseline.clone();
    f.session.replace_native_content(&r).unwrap();
    assert!(
        value::text(&f.session.lock().assets[&key], "kind") == "imported",
        "verified native importer output did not replace the raw texture path"
    );
    f.session.prepare(&f.output).unwrap();
    assert!(
        baseline == f.session.lock().baseline,
        "native texture transition mutated the immutable baseline"
    );
}

fn new_native_model_fixture() -> NativeFixture {
    let mut f = native_fixture(".fbx");
    {
        let mut s = f.fixture.session.lock();
        s.assets.clear();
        s.catalog.assets.clear();
        s.catalog.model_prefabs.clear();
        s.models.clear();
        s.baseline.insert("assets".into(), Value::Array(Vec::new()));
    }
    f.replacement.model_graph_sha256 = native_model_graph_sha256(&f.replacement.model_graph);
    f
}

#[test]
fn new_static_native_model_publishes_verified_hierarchy_and_replays_as_prefab() {
    let mut f = new_native_model_fixture();
    let guid = f.replacement.guid.clone();
    let path = f.replacement.source_path.clone();
    {
        let mut s = f.session().lock();
        s.dependencies.clear();
        let error = s.document_source(&path).unwrap_err();
        assert!(
            needs_import(&error).is_some_and(|needs| needs.path == path),
            "missing model did not request importer: {}",
            message(&error)
        );
    }
    let before = native_model_graph_sha256(&f.replacement.model_graph);
    f.replace().unwrap();
    let graph = {
        let mut s = f.session().lock();
        assert_eq!(
            s.models[&guid].len(),
            2,
            "new source model hierarchy was not registered"
        );
        let documents = s.document_source(&path).unwrap();
        assert_eq!(documents.len(), 2, "new native model cannot expand");
        assert!(
            str(documents[0].get("m_Name")) == "Original"
                && file_id(array(documents[0].get("m_Component"))[0].field("component")) == "11",
            "native hierarchy contents were changed"
        );
        let graph = s.native_model_graph(&guid).unwrap();
        if let Err(error) = match_native_model_graph(&f.replacement.model_graph, &graph) {
            panic!(
                "projection did not round-trip native fields: {}",
                message(&error)
            );
        }
        graph
    };
    items(&mut f.replacement.model_graph)[0]
        .scalars
        .as_mut()
        .unwrap()
        .insert("m_Name".into(), "caller mutation".into());
    assert!(
        str(f.session().lock().models[&guid][0].get("m_Name")) == "Original",
        "caller retained mutable ownership of accepted hierarchy"
    );
    // The scene consumes an imported source model using its native local IDs.
    write_fixture_file(
        &f.fixture.scene,
        "--- !u!1001 &100
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications: []
--- !u!4 &200 stripped
Transform:
  m_CorrespondingSourceObject: {fileID: 11, guid: 22222222222222222222222222222222, type: 3}
  m_PrefabInstance: {fileID: 100}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 200}
",
    );
    let prepared = match f.session().prepare(&f.fixture.output) {
        Ok(prepared) => prepared,
        Err(error) => panic!("new native FBX scene failed: {}", message(&error)),
    };
    assert_eq!(prepared.objects, 3, "new native FBX scene failed");
    assert!(
        before == native_model_graph_sha256(&graph),
        "accepted model topology/scalars changed"
    );
}

#[test]
fn new_native_model_rejects_tampering_and_invalid_hierarchy_before_publication() {
    type Change = fn(&mut NativeReplacement);
    fn rehash(r: &mut NativeReplacement) {
        r.model_graph_sha256 = native_model_graph_sha256(&r.model_graph);
    }
    fn graph_target(
        r: &mut NativeReplacement,
        object: usize,
        reference: usize,
    ) -> &mut Vec<NativeModelGraphTarget> {
        items(&mut items(&mut items(&mut r.model_graph)[object].references)[reference].targets)
    }
    fn scalars(r: &mut NativeReplacement, object: usize) -> &mut Map {
        items(&mut r.model_graph)[object].scalars.as_mut().unwrap()
    }
    let cases: [(&str, Change); 8] = [
        ("graph digest", |r| {
            scalars(r, 0).insert("m_Name".into(), "tampered".into());
        }),
        ("local owner", |r| {
            graph_target(r, 1, 0)[0].file_id = "999".into();
            rehash(r);
        }),
        ("duplicate component", |r| {
            let first = graph_target(r, 0, 0)[0].clone();
            graph_target(r, 0, 0).push(first);
            rehash(r);
        }),
        ("cyclic parent", |r| {
            graph_target(r, 1, 2)[0].file_id = "11".into();
            rehash(r);
        }),
        ("unknown scalar", |r| {
            scalars(r, 0).insert("invented".into(), Value::Int(0));
            rehash(r);
        }),
        ("unknown object", |r| {
            r.content
                .objects
                .push(native_object("90", "UnityEngine.Avatar"));
        }),
        ("unresolved external", |r| {
            graph_target(r, 1, 0)[0].guid = OTHER_GUID.into();
            rehash(r);
        }),
        ("invalid layer", |r| {
            scalars(r, 0).insert("m_Layer".into(), Value::Int(32));
            rehash(r);
        }),
    ];
    for (name, mutate) in cases {
        let mut f = new_native_model_fixture();
        let critical = f.session().lock().critical.clone();
        mutate(&mut f.replacement);
        assert!(
            f.replace().is_err(),
            "{name}: invalid new model was accepted"
        );
        let s = f.session().lock();
        assert!(
            s.models.is_empty()
                && s.native_overlays.is_empty()
                && s.assets.is_empty()
                && critical == s.critical,
            "{name}: rejected hierarchy changed session state"
        );
    }
}

#[test]
fn new_native_model_preserves_child_order_and_rejects_asymmetric_links() {
    let mut f = new_native_model_fixture();
    let mut root_transform = f.replacement.model_graph[1].clone();
    let mut child_object = f.replacement.model_graph[0].clone();
    child_object.file_id = "20".into();
    child_object
        .scalars
        .as_mut()
        .unwrap()
        .insert("m_Name".into(), "Child".into());
    child_object.references = List::from(vec![reference("m_Component", &["21"])]);
    let mut child = root_transform.clone();
    child.file_id = "21".into();
    child.references = List::from(vec![
        reference("m_GameObject", &["20"]),
        reference("m_Children", &[]),
        reference("m_Father", &["11"]),
    ]);
    root_transform.references = List::from(vec![
        reference("m_GameObject", &["10"]),
        reference("m_Children", &["21"]),
        reference("m_Father", &["0"]),
    ]);
    let replacement = &mut f.replacement;
    items(&mut replacement.model_graph)[1] = root_transform;
    replacement.model_graph.push(child_object);
    replacement.model_graph.push(child);
    replacement
        .content
        .objects
        .push(native_object("20", "UnityEngine.GameObject"));
    replacement
        .content
        .objects
        .push(native_object("21", "UnityEngine.Transform"));
    replacement.model_graph_sha256 = native_model_graph_sha256(&replacement.model_graph);
    let s = f.fixture.session.lock();
    match s.static_native_model_documents(&f.replacement) {
        Ok(docs) => assert!(
            file_id(&array(docs[1].get("m_Children"))[0]) == "21",
            "authored child order changed"
        ),
        Err(error) => panic!("authored child order changed: {}", message(&error)),
    }
    items(&mut items(&mut f.replacement.model_graph)[1].references)[1].targets = List::nil();
    match s.static_native_model_documents(&f.replacement) {
        Err(error) if message(&error).contains("parent omits") => {}
        Err(error) => panic!("asymmetric links accepted: {}", message(&error)),
        Ok(_) => panic!("asymmetric links accepted"),
    }
}

#[test]
fn native_model_editor_euler_hint_does_not_change_runtime_rotation_or_fingerprint() {
    let f = new_native_model_fixture();
    let mut s = f.session().lock();
    let mut documents: Vec<Document> = s.static_native_model_documents(&f.replacement).unwrap();
    let before = documents[1].get("m_LocalRotation").clone();
    apply_override(
        documents[1].data.as_mut().unwrap(),
        "m_LocalEulerAnglesHint.x",
        &Value::String("180".into()),
        &value::map([("fileID", Value::Int(0))]),
    )
    .unwrap();
    assert!(
        before == *documents[1].get("m_LocalRotation")
            && number(documents[1].get("m_LocalEulerAnglesHint").field("x")) == 180,
        "Inspector hint changed authoritative native rotation"
    );
    s.models
        .insert(f.replacement.guid.clone(), Arc::new(documents));
    let graph = s.native_model_graph(&f.replacement.guid).unwrap();
    if let Err(error) = match_native_model_graph(&f.replacement.model_graph, &graph) {
        panic!(
            "Inspector hint contaminated native graph fingerprint: {}",
            message(&error)
        );
    }
}

fn new_native_shader_fixture() -> (Fixture, NativeReplacement) {
    let (f, mut replacement) = new_native_texture_fixture();
    let old = format!("{}/{}", f.project, replacement.source_path);
    replacement.source_path = "Assets/Authored.shader".into();
    let source = format!("{}/{}", f.project, replacement.source_path);
    fs::rename(&old, &source).unwrap();
    fs::rename(format!("{old}.meta"), format!("{source}.meta")).unwrap();
    replacement.content.objects = List::from(vec![native_object("4800000", "UnityEngine.Shader")]);
    (f, replacement)
}

#[test]
fn native_shader_publishes_exact_standalone_object() {
    let (f, replacement) = new_native_shader_fixture();
    let baseline = f.session.lock().baseline.clone();
    f.session.replace_native_content(&replacement).unwrap();
    f.session.prepare(&f.output).unwrap();
    let s = f.session.lock();
    let asset = &s.assets[&asset_key(&replacement.guid, "4800000")];
    assert!(
        value::text(asset, "kind") == "imported"
            && value::text(asset, "typeName") == "UnityEngine.Shader"
            && *value::get(asset, "classId") == Value::Int32(48)
            && baseline == s.baseline,
        "native Shader identity changed: {asset:?}"
    );
}

#[test]
fn native_shader_rejects_additional_graph_resources_and_dependencies() {
    for name in [
        "wrong ID",
        "wrong type",
        "extra object",
        "model graph",
        "graph hash",
        "resource",
        "dependency",
        "source changed",
    ] {
        let (f, mut replacement) = new_native_shader_fixture();
        let before = f.session.lock().assets.clone();
        match name {
            "wrong ID" => items(&mut replacement.content.objects)[0].file_id = "4800001".into(),
            "wrong type" => {
                items(&mut replacement.content.objects)[0].type_name = "UnityEngine.Material".into()
            }
            "extra object" => replacement
                .content
                .objects
                .push(native_object("1", "UnityEngine.GameObject")),
            "model graph" => {
                replacement.model_graph = List::from(vec![NativeModelGraphObject {
                    file_id: "1".into(),
                    class_id: 1,
                    ..NativeModelGraphObject::default()
                }])
            }
            "graph hash" => replacement.model_graph_sha256 = replacement.content.sha256.clone(),
            "resource" => {
                replacement.content.resources = List::from(vec![NativeContentResource::default()])
            }
            "dependency" => {
                replacement.content.dependencies = List::from(vec![NativeContentDependency {
                    guid: "0000000000000000f000000000000000".into(),
                    global: true,
                }])
            }
            _ => write_fixture_file(
                format!("{}/{}", f.project, replacement.source_path),
                "new shader source",
            ),
        }
        assert!(
            f.session.replace_native_content(&replacement).is_err(),
            "{name}: invalid native Shader accepted"
        );
        let s = f.session.lock();
        assert!(
            s.native_overlays.is_empty() && before == s.assets,
            "{name}: rejected Shader changed session content"
        );
    }
}

fn native_ui_proof_for_test(file: &mut NativeContentFile) {
    let data = value::encode_json(&file.ui_bindings).unwrap();
    file.ui_binding_proof = Some(NativeUIBindingProof {
        schema_version: 1,
        mode: "native-ui-pptr-bindings-v1".into(),
        native_sha256: file.sha256.clone(),
        reader_sha256: "a".repeat(64),
        dump_sha256: "b".repeat(64),
        bindings_sha256: crate::sha256(data.as_bytes()),
    });
}

#[test]
fn native_ui_binding_validation_exact_owner_dependencies_and_registry_guard() {
    const OWNER: &str = "87785bfd71a0a425fb905aaf7743f114";
    const STYLE: &str = "cd5ff7a50bd83450bbe76880cf52d119";
    let mut file = NativeContentFile {
        guid: OWNER.into(),
        sha256: "c".repeat(64),
        objects: List::from(vec![
            native_object("19", "UnityEngine.UIElements.VisualTreeAsset"),
            native_object("10", "UnityEngine.UIElements.StyleSheet"),
        ]),
        dependencies: List::from(vec![NativeContentDependency {
            guid: STYLE.into(),
            global: false,
        }]),
        ui_bindings: List::from(vec![
            NativeUIBinding {
                owner_file_id: "19".into(),
                property_path: "inlineSheet".into(),
                target_guid: OWNER.into(),
                target_file_id: "10".into(),
                ..NativeUIBinding::default()
            },
            NativeUIBinding {
                owner_file_id: "19".into(),
                property_path: "references.RefIds[0].data.m_Stylesheets[0]".into(),
                managed_reference_id: "1000".into(),
                target_guid: STYLE.into(),
                target_file_id: "7433".into(),
            },
        ]),
        ..NativeContentFile::default()
    };
    native_ui_proof_for_test(&mut file);
    let files: BTreeMap<String, NativeContentFile> = BTreeMap::from([
        (OWNER.to_string(), file.clone()),
        (
            STYLE.to_string(),
            NativeContentFile {
                guid: STYLE.into(),
                objects: List::from(vec![native_object(
                    "7433",
                    "UnityEngine.UIElements.StyleSheet",
                )]),
                ..NativeContentFile::default()
            },
        ),
    ]);
    validate_native_ui_bindings(&file, &files).unwrap();
    type Mutation = fn(&mut Vec<NativeUIBinding>);
    let mutations: [Mutation; 7] = [
        |b| b[1].managed_reference_id = String::new(),
        |b| b[1].target_file_id = "999".into(),
        |b| b[1].target_guid = "a54a76024e0db49348ae426002a8a519".into(),
        |b| b[1].property_path = "references.RefIds[-1].data.m_Stylesheets[0]".into(),
        |b| b[1].owner_file_id = "999".into(),
        |b| {
            let repeated = b[1].clone();
            b.push(repeated);
        },
        |b| b[0].target_file_id = "19".into(),
    ];
    for mutate in mutations {
        let mut bad = file.clone();
        mutate(items(&mut bad.ui_bindings));
        native_ui_proof_for_test(&mut bad);
        assert!(
            validate_native_ui_bindings(&bad, &files).is_err(),
            "bad native UI descriptor accepted: {bad:?}"
        );
    }
    let mut bad = file.clone();
    bad.ui_binding_proof = None;
    assert!(
        validate_native_ui_bindings(&bad, &files).is_err(),
        "missing extraction proof accepted"
    );
    let mut bad = file.clone();
    bad.sha256 = "d".repeat(64);
    assert!(
        validate_native_ui_bindings(&bad, &files).is_err(),
        "proof from different native payload accepted"
    );
}

#[test]
fn native_mesh_cpu_retention_names_only_imported_unreadable_meshes() {
    let retention = |file_id: &str, original_is_readable: bool| NativeMeshCPURetention {
        file_id: file_id.into(),
        original_is_readable,
    };
    let mut file = NativeContentFile {
        objects: List::from(vec![
            native_object("-4300", "UnityEngine.Mesh"),
            native_object("21", "UnityEngine.Material"),
        ]),
        mesh_cpu_retention: List::from(vec![retention("-4300", false)]),
        ..NativeContentFile::default()
    };
    validate_native_mesh_cpu_retention(&file).unwrap();
    for values in [
        vec![retention("-4300", true)],
        vec![retention("-4300", false), retention("-4300", false)],
        vec![retention("21", false)],
        vec![retention("missing", false)],
        vec![retention("0", false)],
    ] {
        file.mesh_cpu_retention = List::from(values);
        assert!(
            validate_native_mesh_cpu_retention(&file).is_err(),
            "foreign/duplicate/non-Mesh/readable retention declaration passed: {:?}",
            file.mesh_cpu_retention
        );
    }
}

fn builtin_snapshot_fixture(root: &str) -> RetainedBuiltinShader {
    let path = format!("{root}/compiled-builtin.bin");
    write_fixture_file(&path, "immutable compiled native Shader fixture");
    let hash = crate::sha256(b"immutable compiled native Shader fixture");
    let sha = "a".repeat(64);
    RetainedBuiltinShader {
        schema_version: 1,
        unity_version: "6000.6.3f1".into(),
        guid: BUILTIN_RESOURCE_GUID.into(),
        file_id: "108".into(),
        shader_name: "Skybox/Panoramic".into(),
        source_guid: "f57c2bc4f17ba48d9b9b6758dcfe812d".into(),
        source_file_id: "4800000".into(),
        archive_file_id: "2756170372350126523".into(),
        bundle_path: format!("{root}/source.bundle"),
        bundle_sha256: sha.clone(),
        member_path: format!("{root}/member"),
        member_name: "CAB-builtin".into(),
        member_sha256: sha.clone(),
        payload_sha256: sha.clone(),
        type_tree_sha256: sha.clone(),
        builtin_source_path: format!("{root}/engine-resources"),
        builtin_source_sha256: sha,
        compiled_source_revision_verified: false,
        content: NativeContentFile {
            guid: BUILTIN_RESOURCE_GUID.into(),
            path,
            sha256: hash,
            objects: List::from(vec![native_object("108", "UnityEngine.Shader")]),
            ..NativeContentFile::default()
        },
    }
}

#[test]
fn retain_builtin_shader_preserves_authored_graph_and_pins_runtime_file() {
    let f = fixture_session();
    let before = fs::read(&f.scene).unwrap();
    let snapshot = builtin_snapshot_fixture(&f.project);
    f.session.retain_builtin_shader(&snapshot).unwrap();
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    assert!(
        array(value::get(&manifest, "builtinShaderSnapshots")).len() == 1
            && array(value::get(&manifest, "nativeContentFiles")).len() == 1,
        "retained built-in Shader/provenance missing from source manifest"
    );
    let file = object(&array(value::get(&manifest, "nativeContentFiles"))[0]);
    assert!(
        value::text(file, "guid") == BUILTIN_RESOURCE_GUID
            && str(array(value::get(file, "objects"))[0].field("fileId")) == "108",
        "authored built-in Shader identity was remapped incorrectly"
    );
    assert!(
        before == fs::read(&f.scene).unwrap(),
        "retaining a native engine Shader rewrote the authored scene"
    );
    write_fixture_file(&snapshot.content.path, "changed native Shader bytes");
    assert!(
        f.session.prepare(&f.output).is_err(),
        "published a changed retained Shader artifact"
    );
}

#[test]
fn retained_builtin_shader_rejects_substituted_identity_or_dependencies() {
    let directory = tempfile::tempdir().unwrap();
    let good = builtin_snapshot_fixture(directory.path().to_str().unwrap());
    type Corrupt = fn(&mut RetainedBuiltinShader);
    let corruptions: [Corrupt; 7] = [
        |v| v.file_id = "101".into(),
        |v| v.shader_name = "Skybox/Cubemap".into(),
        |v| v.source_guid = "1".repeat(32),
        |v| v.content.guid = v.source_guid.clone(),
        |v| v.content.objects = List::from(vec![native_object("4800000", "UnityEngine.Shader")]),
        |v| {
            v.content.dependencies = List::from(vec![NativeContentDependency {
                guid: BUILTIN_RESOURCE_GUID.into(),
                global: true,
            }])
        },
        |v| v.compiled_source_revision_verified = true,
    ];
    for corrupt in corruptions {
        let mut value = good.clone();
        corrupt(&mut value);
        assert!(
            value.validate("6000.6.3f1").is_err(),
            "substituted native engine Shader accepted: {value:?}"
        );
    }
}

fn new_compiled_shader_snapshot_fixture() -> (Fixture, NativeReplacement) {
    let (f, mut r) = new_native_shader_fixture();
    let source = format!("{}/{}", f.project, r.source_path);
    write_fixture_file(
        &source,
        "Shader \"Test/Authored\" {\n#include \"Nested/A.hlsl\"\n}\n",
    );
    write_fixture_file(
        format!("{}/Assets/Nested/A.hlsl", f.project),
        "#include \"Packages/test.pipeline/Runtime/B.hlsl\"\n#if OPTIONAL\n#include \"Absent.hlsl\"\n#endif\n",
    );
    write_fixture_file(
        format!(
            "{}/Library/PackageCache/test.pipeline@abc/Runtime/B.hlsl",
            f.project
        ),
        "// #include \"Commented.hlsl\"\n#include \"Assets/Nested/A.hlsl\"\nfloat value = 1;\n",
    );
    r.source_sha256 = native_fixture_hash(&source);
    let archive = format!("{}/compiled/origin.bundle", f.output);
    let member = format!("{}/compiled/origin.sharedAssets", f.output);
    write_fixture_file(&archive, "verified compiled archive fixture");
    write_fixture_file(&member, "verified shader member fixture");
    let (requirements, missing) =
        collect_shader_source_requirements(&f.project, &r.source_path).unwrap();
    r.compiled_shader_snapshot = Some(CompiledShaderSnapshot {
        schema_version: 1,
        source_path: r.source_path.clone(),
        shader_name: "Test/Authored".into(),
        archive_file_id: "5597".into(),
        bundle_sha256: native_fixture_hash(&archive),
        bundle_path: archive,
        member_sha256: native_fixture_hash(&member),
        member_path: member,
        member_name: "origin.sharedAssets".into(),
        payload_sha256: r.content.sha256.clone(),
        type_tree_sha256: r.content.sha256.clone(),
        source_requirements: List::from(requirements),
        missing_includes: List::from(missing),
        ..CompiledShaderSnapshot::default()
    });
    (f, r)
}
fn snapshot(r: &mut NativeReplacement) -> &mut CompiledShaderSnapshot {
    r.compiled_shader_snapshot.as_mut().unwrap()
}

#[test]
fn compiled_shader_snapshot_publishes_complete_guard_and_owns_descriptor() {
    let (f, mut r) = new_compiled_shader_snapshot_fixture();
    assert!(
        snapshot(&mut r).source_requirements.len() == 3
            && snapshot(&mut r).missing_includes.to_vec()
                == ["Assets/Nested/Absent.hlsl".to_string()],
        "nested, package, cyclic or conditional include closure was incorrect"
    );
    f.session.replace_native_content(&r).unwrap();
    items(&mut snapshot(&mut r).source_requirements)[0].sha256 = zeros();
    items(&mut snapshot(&mut r).missing_includes)[0] = "caller mutation".into();
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let proofs = array(value::get(&manifest, "compiledShaderSnapshots"));
    assert!(
        proofs.len() == 1
            && proofs[0].field("compiledSourceRevisionVerified").is_false()
            && str(proofs[0].field("guid")) == r.guid,
        "explicit snapshot provenance was lost"
    );
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    for entry in array(value::get(&manifest, "sourceDependencies")) {
        let d = object(entry);
        paths.insert(
            value::text(d, "path").into_owned(),
            value::text(d, "sourcePath").into_owned(),
        );
    }
    let path_of = |path: &str| paths.get(path).map_or("", String::as_str);
    let s = f.session.lock();
    let accepted = s.native_overlays[&r.guid]
        .compiled_shader_snapshot
        .as_ref()
        .unwrap();
    for requirement in accepted.source_requirements.iter() {
        assert!(
            path_of(&requirement.path) == requirement.source_path,
            "missing current source guard {}",
            requirement.path
        );
    }
    for origin in [&accepted.bundle_path, &accepted.member_path] {
        assert!(
            path_of(origin) == origin,
            "compiled origin was not pinned for prepared launch"
        );
    }
}

#[test]
fn compiled_shader_snapshot_rejects_tamper_and_incomplete_closure() {
    for kind in [
        "include mutation",
        "missing include appears",
        "omitted nested guard",
        "omitted missing guard",
        "unverified provenance claim",
        "archive mutation",
        "member mutation",
        "wrong source path",
        "duplicate guard",
        "foreign source",
    ] {
        let (f, mut r) = new_compiled_shader_snapshot_fixture();
        let before = f.session.lock().assets.clone();
        match kind {
            "include mutation" => write_fixture_file(
                format!("{}/Assets/Nested/A.hlsl", f.project),
                "new include body",
            ),
            "missing include appears" => write_fixture_file(
                format!("{}/Assets/Nested/Absent.hlsl", f.project),
                "new optional include",
            ),
            "omitted nested guard" => items(&mut snapshot(&mut r).source_requirements).truncate(2),
            "omitted missing guard" => snapshot(&mut r).missing_includes = List::nil(),
            "unverified provenance claim" => {
                snapshot(&mut r).compiled_source_revision_verified = true
            }
            "archive mutation" => write_fixture_file(&snapshot(&mut r).bundle_path, "new archive"),
            "member mutation" => write_fixture_file(&snapshot(&mut r).member_path, "new member"),
            "wrong source path" => snapshot(&mut r).source_path = "Assets/Other.shader".into(),
            "duplicate guard" => {
                let first = snapshot(&mut r).source_requirements[0].clone();
                snapshot(&mut r).source_requirements.push(first);
            }
            _ => {
                let foreign = r.content.path.clone();
                items(&mut snapshot(&mut r).source_requirements)[0].source_path = foreign;
            }
        }
        assert!(
            f.session.replace_native_content(&r).is_err(),
            "{kind}: invalid compiled Shader snapshot accepted"
        );
        let s = f.session.lock();
        assert!(
            s.native_overlays.is_empty() && before == s.assets,
            "{kind}: rejected snapshot changed session content"
        );
    }
}

#[test]
fn compiled_shader_snapshot_rechecks_nested_includes_before_prepare_and_replay() {
    for kind in ["include body", "absent include"] {
        let (f, r) = new_compiled_shader_snapshot_fixture();
        f.session.replace_native_content(&r).unwrap();
        let first = f.session.prepare(&f.output).unwrap();
        let original = fs::read(&first.manifest).unwrap();
        if kind == "include body" {
            write_fixture_file(
                format!(
                    "{}/Library/PackageCache/test.pipeline@abc/Runtime/B.hlsl",
                    f.project
                ),
                "float value = 2;\n",
            );
        } else {
            write_fixture_file(
                format!("{}/Assets/Nested/Absent.hlsl", f.project),
                "appeared\n",
            );
        }
        assert!(
            f.session.prepare(&f.output).is_err(),
            "{kind}: accepted stale GPU shader after an include change"
        );
        assert!(
            f.session.replace_native_content(&r).is_err(),
            "{kind}: cache replay accepted stale source/include guard"
        );
        assert!(
            fs::read(&first.manifest).is_ok_and(|current| current == original),
            "{kind}: failed preparation changed immutable revision"
        );
    }
}

#[test]
fn shader_snapshot_rejects_dynamic_includes_ambiguous_packages_and_escapes() {
    for kind in ["macro", "multiple package versions", "escape"] {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().to_str().unwrap();
        let mut body = "#include PLATFORM_INCLUDE\n";
        if kind == "multiple package versions" {
            body = "#include \"Packages/test.pipeline/B.hlsl\"\n";
            for version in ["a", "b"] {
                write_fixture_file(
                    format!("{project}/Library/PackageCache/test.pipeline@{version}/B.hlsl"),
                    "body",
                );
            }
        }
        if kind == "escape" {
            body = "#include \"../../Outside.hlsl\"\n";
        }
        write_fixture_file(format!("{project}/Assets/Test.shader"), body);
        assert!(
            collect_shader_source_requirements(project, "Assets/Test.shader").is_err(),
            "{kind}: unsupported include resolution was accepted"
        );
    }
}
