use super::*;
use crate::{
    authoring::Store,
    compiler::{
        AssemblyConfig, CompileAssembly, CompileManifest, NilMap, NilVec, canonical_configuration,
        compiler_manifest, read_assembly_configuration,
    },
    util::{atomic_json, file_hash},
};
use catalog::read_source_catalog;
use defaults::immutable_file;
use native::may_need_native;
use serde_json::json;
use source_scene::{inspect_source_scene, validate_source_scene_proof};

#[test]
fn native_preparation_includes_fresh_texture_and_audio_inputs() {
    for extension in [
        ".png",
        ".jpg",
        ".tga",
        ".tiff",
        ".fbx",
        ".flac",
        ".mat",
        ".inputactions",
    ] {
        let (_guard, project) = review_project();
        write(
            &join(&[&project, &format!("Assets/NativeInput{extension}")]),
            "native source",
        );
        assert!(
            may_need_native(&project).unwrap(),
            "native input {extension} skipped public preparation"
        );
    }
    let (_guard, project) = review_project();
    assert!(
        !may_need_native(&project).unwrap(),
        "plain source-only fixture unexpectedly needs heavy content"
    );
}

#[test]
fn native_preparation_rejects_malformed_settings_and_linked_inputs() {
    let (_guard, project) = review_project();
    write(
        &join(&[&project, "ProjectSettings/GraphicsSettings.asset"]),
        "not: [valid YAML",
    );
    assert!(
        may_need_native(&project).is_err(),
        "malformed native project settings silently accepted"
    );
    let (_guard, project) = review_project();
    let (_other_guard, other) = temp_dir();
    std::os::unix::fs::symlink(&other, join(&[&project, "Assets/linked-assets"])).unwrap();
    assert!(
        may_need_native(&project).is_err(),
        "linked native input silently accepted"
    );
}

#[test]
fn prefab_source_never_starts_editor_and_replays_exact_numbers() {
    let (_guard, project) = review_project();
    let path = "Assets/Main.unity";
    let content = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_Component:
  - component: {fileID: 101}
  m_Name: Root
  m_IsActive: 1
  exactCounter: 0 # preserve
--- !u!4 &101
Transform:
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
  m_Father: {fileID: 0}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 101}
";
    write(&join(&[&project, path]), content);
    write(
        &format!("{}.meta", join(&[&project, path])),
        "guid: 33333333333333333333333333333333\n",
    );
    let store = Store::new(Path::new(&project)).unwrap();
    let new_service = || {
        Service::new(Options {
            project: project.clone(),
            editor: "/definitely-no-editor".into(),
            lifetime: Some(ctx()),
            ..Default::default()
        })
    };
    let service = new_service();
    let call = |mut args: Value| {
        args["path"] = json!(path);
        args["expectedRevision"] = json!(store.read(path).unwrap().revision);
        let result = service
            .handle_sync(&ctx(), "prefab_edit", &bytes(&args))
            .unwrap();
        assert!(is_map(&result));
        let value = decoded(&result);
        assert_eq!(value["backend"], "raw_source");
        assert_eq!(value["editorStarted"], false);
        value
    };
    let created = call(
        json!({"action": "create_prefab", "fileId": "100", "outputPath": "Assets/Base.prefab", "operationId": "own-create"}),
    );
    assert_eq!(created["generatedPath"], "Assets/Base.prefab");
    call(
        json!({"action": "instantiate", "prefabPath": "Assets/Base.prefab", "name": "Linked", "operationId": "own-instantiate"}),
    );
    let source = store.read(path).unwrap();
    let arguments = |value: &str| {
        format!(
            r#"{{"action":"set_override","expectedRevision":{},"operationId":"own-exact","path":"{path}","property":"exactCounter","targetPath":"Linked","value":{value}}}"#,
            serde_json::to_string(&source.revision).unwrap()
        )
    };
    let raw = arguments("9223372036854775807");
    for iteration in 0..2 {
        let restarted = new_service();
        let result = restarted.handle_sync(&ctx(), "prefab_edit", raw.as_bytes());
        restarted.close();
        let replayed = decoded(&result.unwrap())["publication"]["replayed"] == true;
        assert_eq!(replayed, iteration == 1, "wrong replay");
    }
    let updated = store.read(path).unwrap();
    assert!(
        updated.content.contains("9223372036854775807")
            && updated.content.contains("exactCounter: 0 # preserve")
            && updated.metadata == source.metadata,
        "exact override or unrelated bytes were changed"
    );
    assert_eq!(
        code(&service.handle_sync(
            &ctx(),
            "prefab_edit",
            arguments("9223372036854775806").as_bytes()
        )),
        "idempotency_conflict",
        "adjacent int64 reused operation"
    );
    assert!(
        absent(&join(&[
            &project,
            "Library/UnityMicroEditor/authoring-stages"
        ])),
        "source authoring created an Editor stage"
    );
    service.close();
}

#[test]
fn artifact_preflight_rejects_corruption_and_extra_player_files() {
    let (_guard, project) = temp_dir();
    let directory = join(&[&project, "Library/UnityMicroEditor/preparations/verified"]);
    let player = join(&[&directory, "Player/Project.x86_64"]);
    let assembly = join(&[&directory, "Player/Project_Data/Managed/Game.dll"]);
    let native = join(&[&project, "Library/UnityMicroEditor/native/objects.assets"]);
    let manifest = join(&[&directory, "source/scene.json"]);
    let baseline = join(&[&directory, "current.json"]);
    for path in [&player, &assembly, &native] {
        write(
            path,
            format!("exact generated {}", crate::sysutil::base(path)),
        );
    }
    immutable(
        &manifest,
        json!({"schemaVersion": 1, "supported": true, "projectRoot": project,
            "nativeContentFiles": [{"path": native, "sha256": file_hash(&native).unwrap()}]}),
    );
    let compiler = CompileManifest {
        assemblies: NilVec::from(vec![CompileAssembly {
            name: "Game".into(),
            path: assembly.clone(),
            sha256: file_hash(&assembly).unwrap(),
            ..Default::default()
        }]),
        ..Default::default()
    };
    immutable(
        &baseline,
        json!({"player": player, "manifest": manifest, "compiler": compiler}),
    );
    let receipt = capture_preparation_artifacts(
        &ctx(),
        &project,
        &baseline,
        &join(&[&directory, "artifacts.json"]),
    )
    .unwrap();
    let validate = || validate_preparation_artifacts(&project, &baseline, &player, &receipt);
    validate().unwrap();
    for path in [&player, &assembly, &native, &manifest, &baseline] {
        let original = fs::read(path).unwrap();
        write(path, "corrupt");
        assert!(
            validate().is_err(),
            "corrupted regular artifact accepted: {path}"
        );
        write(path, original);
    }
    validate().unwrap();
    write(
        &join(&[&crate::sysutil::dir(&player), "injected.dll"]),
        "unexpected",
    );
    assert!(validate().is_err(), "unrecorded Player file accepted");
}

fn compiler_locator_fixture() -> (TempDir, String, String, String, CompileManifest) {
    let (guard, project) = temp_dir();
    let source = join(&[&project, "Assets/Known.cs"]);
    write(&source, "public class Known {}");
    let dll = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/one/.compiler-preparation/Player/Player_Data/Managed/Assembly-CSharp.dll",
    ]);
    write(&dll, "accepted unchanged DLL");
    let digest = file_hash(&dll).unwrap();
    let assembly = CompileAssembly {
        name: "Assembly-CSharp".into(),
        path: dll.clone(),
        sha256: digest.clone(),
        sources: NilVec::from(vec![source]),
        assembly_input_hash: "a".repeat(64),
        ..Default::default()
    };
    let plan = CompileManifest {
        schema: 1,
        project: project.clone(),
        unity: UNITY_VERSION.into(),
        assemblies: NilVec::from(vec![assembly.clone()]),
        compiler_inputs: NilMap::from([(dll, digest)]),
        ..Default::default()
    };
    let config = AssemblyConfig {
        name: assembly.name.clone(),
        sources: assembly.sources.clone(),
        defines: NilVec::from(vec!["UNITY_STANDALONE_LINUX".to_string()]),
        ..Default::default()
    };
    let canonical = canonical_configuration(&config).unwrap();
    let source_directory = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/one/.source-preparation",
    ]);
    let descriptor = join(&[
        &source_directory,
        "compiler-cache/baseline-assemblies",
        &assembly.assembly_input_hash,
        "compile.json",
    ]);
    immutable(
        &descriptor,
        json!({
            "assemblyConfiguration": serde_json::from_slice::<Value>(&canonical).unwrap(),
            "compilerInputs": {"assemblyConfiguration": crate::sha256(&canonical)},
            "assemblies": [assembly],
        }),
    );
    let scene = join(&[&source_directory, "source/revisions/current/scene.json"]);
    immutable(
        &scene,
        json!({"schemaVersion": 1, "supported": true, "projectRoot": project}),
    );
    let wrapper = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/one/current.json",
    ]);
    atomic_json(&wrapper, &json!({"compiler": plan, "manifest": scene})).unwrap();
    (guard, project, wrapper, scene, plan)
}
fn cached(project: &str, baseline: &str, artifacts: &str, catalog: &str) -> Preparation {
    Preparation {
        schema_version: 1,
        project: project.into(),
        scene: "Assets/Main.unity".into(),
        request_id: "one".into(),
        baseline: baseline.into(),
        artifact_receipt: artifacts.into(),
        native_catalog: catalog.into(),
        ..Default::default()
    }
}

#[test]
fn artifact_preflight_requires_exact_sibling_assembly_configuration() {
    for damage in ["missing", "corrupt"] {
        let (_guard, project, baseline, manifest, compiler) = compiler_locator_fixture();
        let up = |path: &str, levels: usize| {
            (0..levels).fold(path.to_string(), |path, _| crate::sysutil::dir(&path))
        };
        let player = join(&[&up(&compiler.assemblies[0].path, 3), "Player.x86_64"]);
        write_mode(&player, "verified native player", 0o700);
        atomic_json(
            &manifest,
            &json!({"schemaVersion": 1, "supported": true, "projectRoot": project, "scenePath": "Assets/Main.unity", "unityVersion": UNITY_VERSION}),
        )
        .unwrap();
        atomic_json(
            &baseline,
            &json!({"schemaVersion": 1, "project": project, "compiler": compiler, "manifest": manifest, "player": player}),
        )
        .unwrap();
        let directory = crate::sysutil::dir(&baseline);
        let artifacts = capture_preparation_artifacts(
            &ctx(),
            &project,
            &baseline,
            &join(&[&directory, "artifacts.json"]),
        )
        .unwrap();
        let preparation = cached(&project, &baseline, &artifacts, "");
        let validate = || {
            validate_cached_preparation(
                &project,
                UNITY_VERSION,
                &preparation.scene,
                "one",
                &directory,
                &preparation,
            )
        };
        validate().unwrap();
        let descriptor = join(&[
            &up(&manifest, 4),
            "compiler-cache/baseline-assemblies",
            &compiler.assemblies[0].assembly_input_hash,
            "compile.json",
        ]);
        assert!(!absent(&descriptor));
        if damage == "missing" {
            fs::remove_file(&descriptor).unwrap();
        } else {
            write(&descriptor, r#"{"assemblyConfiguration":{}}"#);
        }
        let (accepted, locator) = compiler_manifest(&baseline).unwrap();
        assert!(
            read_assembly_configuration(&locator, &accepted.assemblies[0], Some(&accepted))
                .is_err(),
            "{damage}: damaged exact configuration did not prevent compiler startup"
        );
        assert!(
            validate().is_err(),
            "{damage}: preflight accepted damaged compiler configuration before healthy Player stop"
        );
    }
}

#[test]
fn artifact_preflight_freezes_referenced_native_catalog() {
    for damage in ["missing", "corrupt"] {
        let (_guard, project) = temp_dir();
        let directory = join(&[&project, "Library/UnityMicroEditor/preparations/one"]);
        let baseline = join(&[&directory, "current.json"]);
        let manifest = join(&[&directory, "source/scene.json"]);
        let player = join(&[&directory, "Player/Player.x86_64"]);
        let catalog = join(&[
            &project,
            "Library/UnityMicroEditor/native/native-catalog.json",
        ]);
        write_mode(&player, "native host bytes", 0o700);
        immutable(
            &manifest,
            json!({"schemaVersion": 1, "supported": true, "projectRoot": project, "scenePath": "Assets/Main.unity", "unityVersion": UNITY_VERSION}),
        );
        immutable(
            &catalog,
            json!({"schemaVersion": 1, "project": project, "assets": [], "packs": []}),
        );
        let compiler = CompileManifest {
            schema: 1,
            project: project.clone(),
            unity: UNITY_VERSION.into(),
            native_only: true,
            compiler_inputs: NilMap::from([("generated-proof", "accepted".to_string())]),
            ..Default::default()
        };
        immutable(
            &baseline,
            json!({"schemaVersion": 1, "project": project, "manifest": manifest, "player": player, "compiler": compiler, "nativeImport": {"catalog": catalog}}),
        );
        let artifacts = capture_preparation_artifacts(
            &ctx(),
            &project,
            &baseline,
            &join(&[&directory, "artifacts.json"]),
        )
        .unwrap();
        let preparation = cached(&project, &baseline, &artifacts, &catalog);
        let validate = || {
            validate_cached_preparation(
                &project,
                UNITY_VERSION,
                &preparation.scene,
                "one",
                &directory,
                &preparation,
            )
        };
        validate().unwrap();
        if damage == "missing" {
            fs::remove_file(&catalog).unwrap();
        } else {
            write(&catalog, "broken generated catalog");
        }
        assert!(
            validate().is_err(),
            "{damage}: preflight accepted missing/corrupt native catalog before healthy Player stop"
        );
    }
}

const LIT_SCENE: &str = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!157 &3\nLightmapSettings:\n  m_LightingDataAsset: {fileID: 20201, guid: 0000000000000000f000000000000000, type: 0}\n  m_LightingSettings: {fileID: 0}\n";

#[test]
fn scene_lighting_requires_exact_public_proof() {
    let contents = LIT_SCENE.as_bytes();
    let (_guard, project) = temp_dir();
    let scene = "Assets/Scenes/Test.unity";
    assert!(
        validate_scene_lighting(contents, &project, scene, "").is_err(),
        "builtin GUID accepted without public evidence"
    );
    let valid = json!({"schemaVersion": 1, "projectRoot": project, "unityVersion": UNITY_VERSION, "scenePath": scene, "sceneSHA256": crate::sha256(contents), "lightmapCount": 0, "lightProbeCount": 0, "activeSceneCount": 1, "isPlaying": false, "sceneDirty": false, "lightingDataIdentityKnown": true, "lightingDataGuid": "0000000000000000f000000000000000", "lightingDataFileId": 20201});
    for scenario in [
        "valid",
        "wrong-hash",
        "baked-map",
        "baked-probe",
        "another-project",
        "another-engine",
        "dirty",
        "playing",
        "multiple-scenes",
        "missing-count",
        "wrong-identity",
        "missing-dirty",
    ] {
        let mut proof = valid.clone();
        match scenario {
            "wrong-hash" => proof["sceneSHA256"] = json!("incorrect"),
            "baked-map" => proof["lightmapCount"] = json!(1),
            "baked-probe" => proof["lightProbeCount"] = json!(1),
            "another-project" => proof["projectRoot"] = json!(format!("{project}-other")),
            "another-engine" => proof["unityVersion"] = json!("6000.0.1f1"),
            "dirty" => proof["sceneDirty"] = json!(true),
            "playing" => proof["isPlaying"] = json!(true),
            "multiple-scenes" => proof["activeSceneCount"] = json!(2),
            "missing-count" => {
                proof.as_object_mut().unwrap().remove("lightmapCount");
            }
            "wrong-identity" => {
                proof["lightingDataGuid"] = json!("00000000000000000000000000000001")
            }
            "missing-dirty" => {
                proof.as_object_mut().unwrap().remove("sceneDirty");
            }
            _ => {}
        }
        let (_proof_guard, proof_directory) = temp_dir();
        let path = join(&[&proof_directory, "proof.json"]);
        write(&path, bytes(&proof));
        let outcome = validate_scene_lighting(contents, &project, scene, &path);
        assert_eq!(
            outcome.is_ok(),
            scenario == "valid",
            "{scenario}: unexpected proof acceptance: {outcome:?}"
        );
    }
    let custom = LIT_SCENE.replacen(
        "0000000000000000f000000000000000",
        "00000000000000000000000000000001",
        1,
    );
    assert!(
        validate_scene_lighting(custom.as_bytes(), &project, scene, "").is_err(),
        "custom baked asset accepted"
    );
    let unlit = LIT_SCENE.replacen(
        "fileID: 20201, guid: 0000000000000000f000000000000000, type: 0",
        "fileID: 0",
        1,
    );
    validate_scene_lighting(unlit.as_bytes(), &project, scene, "").unwrap();
}

#[test]
fn baseline_publication_does_not_overwrite_accepted_output() {
    let (_guard, directory) = temp_dir();
    let path = join(&[&directory, "current.json"]);
    publish_agent_baseline(&path, &any(json!({"accepted": 1}))).unwrap();
    assert!(
        publish_agent_baseline(&path, &any(json!({"accepted": 2}))).is_err(),
        "existing baseline replaced"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"accepted\":1}\n");
    let leftovers: Vec<_> = fs::read_dir(&directory).unwrap().collect();
    assert_eq!(
        leftovers.len(),
        1,
        "temporary baseline files were left behind"
    );
}

const CATALOG: &str = "{\n  \"project\":\"unchanged-project\", \"backend\":\"original-native-backend\",\n  \"assets\":[{\"fileId\":-5824371076177060002,\"positive\":9223372036854775807,\"path\":\"Assets/<raw>&asset.prefab\"}]\n}\n";

struct Catalog {
    project: String,
    directory: String,
    input: String,
    _guards: (TempDir, TempDir),
}
fn catalog_fixture() -> Catalog {
    let (guard, project) = temp_dir();
    let (input_guard, input_directory) = temp_dir();
    let input = join(&[&input_directory, "external-catalog.json"]);
    write(&input, CATALOG);
    Catalog {
        directory: join(&[
            &project,
            "Library/UnityMicroEditor/preparations/catalog-test",
        ]),
        project,
        input,
        _guards: (guard, input_guard),
    }
}
fn freeze(fixture: &Catalog, input: &str) -> Result<String> {
    freeze_source_catalog(&ctx(), &fixture.project, &fixture.directory, input)
}

#[test]
fn freeze_source_catalog_preserves_identity_and_replays() {
    let fixture = catalog_fixture();
    let snapshot = freeze(&fixture, &fixture.input).unwrap();
    assert_eq!(
        snapshot,
        join(&[&fixture.directory, "native-input-catalog.json"])
    );
    assert_eq!(
        fs::read_to_string(&snapshot).unwrap(),
        CATALOG,
        "catalog bytes/long numeric identities were altered"
    );
    let receipt_path = join(&[&fixture.directory, "native-input-receipt.json"]);
    let hash = crate::sha256(CATALOG.as_bytes());
    assert_eq!(
        fs::read_to_string(&receipt_path).unwrap(),
        format!(
            "{{\"schemaVersion\":1,\"project\":\"{}\",\"sourcePath\":\"{}\",\"sourceSHA256\":\"{hash}\",\"snapshotPath\":\"{snapshot}\",\"snapshotSHA256\":\"{hash}\"}}\n",
            fixture.project, fixture.input
        ),
        "snapshot lacks exact source/hash provenance"
    );
    let before = crate::sysutil::stat(&snapshot).unwrap();
    assert_eq!(
        freeze(&fixture, &fixture.input).unwrap(),
        snapshot,
        "stable replay failed"
    );
    let after = crate::sysutil::stat(&snapshot).unwrap();
    assert!(
        before.same_file(&after) && before.modified == after.modified,
        "stable replay rewrote its immutable snapshot"
    );
    assert_eq!(
        fs::read_to_string(&fixture.input).unwrap(),
        CATALOG,
        "external authored input was modified"
    );
    let unused = join(&[&fixture.project, "Library/UnityMicroEditor/unused"]);
    assert_eq!(
        freeze_source_catalog(&ctx(), &fixture.project, &unused, &snapshot).unwrap(),
        snapshot,
        "owned catalog did not pass through"
    );
    assert!(
        absent(&unused),
        "owned pass-through created an unnecessary snapshot"
    );
}

#[test]
fn freeze_source_catalog_rejects_changed_retry_and_snapshot() {
    for damage in ["source", "source-path", "snapshot", "receipt"] {
        let fixture = catalog_fixture();
        let (_other_guard, other) = temp_dir();
        let snapshot = freeze(&fixture, &fixture.input).unwrap();
        let mut input = fixture.input.clone();
        let (mut path, mut replacement) = (input.clone(), r#"{"assets":[],"changed":true}"#);
        match damage {
            "source-path" => {
                input = join(&[&other, "same-bytes-new-input.json"]);
                (path, replacement) = (input.clone(), CATALOG);
            }
            "snapshot" => path = snapshot.clone(),
            "receipt" => {
                (path, replacement) = (
                    join(&[&fixture.directory, "native-input-receipt.json"]),
                    "malformed receipt",
                );
            }
            _ => {}
        }
        write(&path, replacement);
        let outcome = freeze(&fixture, &input);
        assert!(
            outcome.is_err(),
            "{damage}: changed frozen source/snapshot identity was silently accepted"
        );
        if damage != "receipt" {
            assert_eq!(
                code(&outcome),
                "request_id_conflict",
                "{damage}: changed retry was not reported as a conflict"
            );
        }
        if damage == "source" || damage == "source-path" {
            assert_eq!(
                fs::read_to_string(&snapshot).unwrap(),
                CATALOG,
                "{damage}: failed retry replaced the accepted snapshot"
            );
        }
    }
}

#[test]
fn freeze_source_catalog_completes_matching_interrupted_snapshot() {
    let fixture = catalog_fixture();
    let snapshot = join(&[&fixture.directory, "native-input-catalog.json"]);
    immutable_file(&snapshot, CATALOG.as_bytes()).unwrap();
    assert_eq!(
        freeze(&fixture, &fixture.input).unwrap(),
        snapshot,
        "matching interrupted snapshot could not finish its receipt"
    );
    assert!(!absent(&join(&[
        &fixture.directory,
        "native-input-receipt.json"
    ])));
}

#[test]
fn freeze_source_catalog_rejects_malformed_or_unsafe_inputs() {
    for damage in [
        "malformed",
        "trailing",
        "null",
        "array",
        "invalid-utf8",
        "oversize",
        "directory",
        "symlink",
        "parent-symlink",
        "fifo",
        "relative",
    ] {
        let fixture = catalog_fixture();
        let (_other_guard, other) = temp_dir();
        let mut input = fixture.input.clone();
        let content: Option<&[u8]> = match damage {
            "malformed" => Some(br#"{"unfinished":"#),
            "trailing" => Some(b"{} {}"),
            "null" => Some(b"null"),
            "array" => Some(b"[]"),
            "invalid-utf8" => Some(b"{\"x\":\"\xff\"}"),
            _ => None,
        };
        match damage {
            "oversize" => fs::OpenOptions::new()
                .write(true)
                .open(&input)
                .unwrap()
                .set_len((32 << 20) + 1)
                .unwrap(),
            "directory" | "symlink" | "fifo" => {
                fs::remove_file(&input).unwrap();
                if damage == "directory" {
                    fs::create_dir(&input).unwrap();
                } else if damage == "symlink" {
                    std::os::unix::fs::symlink(
                        join(&[&fixture.project, "linked-target.json"]),
                        &input,
                    )
                    .unwrap();
                } else {
                    let path = std::ffi::CString::new(input.clone()).unwrap();
                    // SAFETY: `path` is a valid NUL-terminated string.
                    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                }
            }
            "parent-symlink" => {
                let linked = join(&[&other, "linked-parent"]);
                std::os::unix::fs::symlink(crate::sysutil::dir(&input), &linked).unwrap();
                input = join(&[&linked, &crate::sysutil::base(&input)]);
            }
            "relative" => input = "catalog.json".into(),
            _ => {}
        }
        if let Some(content) = content {
            write(&input, content);
        }
        assert!(
            freeze(&fixture, &input).is_err(),
            "{damage}: malformed or unsafe catalog input accepted"
        );
        assert!(
            absent(&join(&[&fixture.directory, "native-input-catalog.json"])),
            "{damage}: unsafe input published a snapshot"
        );
    }
}

#[test]
fn freeze_source_catalog_rejects_foreign_or_linked_outputs() {
    for damage in [
        "foreign",
        "linked-parent",
        "linked-snapshot",
        "linked-receipt",
        "canceled",
    ] {
        let fixture = catalog_fixture();
        let (_other_guard, other) = temp_dir();
        let mut directory = fixture.directory.clone();
        let request = ctx().with_cancel();
        match damage {
            "foreign" => directory = other.clone(),
            "canceled" => request.cancel(),
            "linked-parent" => {
                fs::create_dir_all(crate::sysutil::dir(&directory)).unwrap();
                std::os::unix::fs::symlink(&other, &directory).unwrap();
            }
            _ => {
                fs::create_dir_all(&directory).unwrap();
                let name = if damage == "linked-receipt" {
                    "native-input-receipt.json"
                } else {
                    "native-input-catalog.json"
                };
                std::os::unix::fs::symlink(&fixture.input, join(&[&directory, name])).unwrap();
            }
        }
        assert!(
            freeze_source_catalog(&request, &fixture.project, &directory, &fixture.input).is_err(),
            "{damage}: unsafe snapshot output was accepted"
        );
    }
    // The bounded reader itself refuses a missing file as "not exist".
    let fixture = catalog_fixture();
    let missing = read_source_catalog(&ctx(), &join(&[&fixture.project, "missing.json"]));
    assert!(not_exist(&missing.unwrap_err()));
}

#[test]
fn source_scene_proof_preserves_metadata_and_rejects_changed_identity() {
    let (_guard, project) = review_project();
    let scene = "Assets/SourceProof.unity";
    let contents = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!157 &3\nLightmapSettings:\n  m_LightingDataAsset: {fileID: 0}\n  m_LightingSettings: {fileID: 0}\n  m_Lightmaps: []\n";
    let meta = "fileFormatVersion: 2\nguid: 77777777777777777777777777777777\nDefaultImporter:\n  externalObjects: {}\n";
    let (scene_file, meta_file) = (
        join(&[&project, scene]),
        format!("{}.meta", join(&[&project, scene])),
    );
    write(&scene_file, contents);
    write(&meta_file, meta);
    let output = join(&[&project, "Library/UnityMicroEditor/source-scene/proof.json"]);
    let mut proof = build_source_scene_proof(
        &ctx(),
        &project,
        "/explicit/unused/SDK/Unity",
        scene,
        &output,
    )
    .unwrap();
    assert_eq!(proof.schema_version, 2);
    assert_eq!(proof.backend, "source_native_resource");
    assert!(proof.builtin_lighting.is_none());
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        format!(
            "{{\"schemaVersion\":2,\"backend\":\"source_native_resource\",\"projectRoot\":\"{project}\",\"unityVersion\":\"{UNITY_VERSION}\",\"scenePath\":\"{scene}\",\"sceneSHA256\":\"{}\",\"sceneMetaSha256\":\"{}\"}}\n",
            crate::sha256(contents.as_bytes()),
            crate::sha256(meta.as_bytes())
        )
    );
    let validate =
        |contents: &[u8]| validate_source_scene_proof(contents, &project, scene, &output);
    validate(contents.as_bytes()).unwrap();
    assert_eq!(fs::read_to_string(&scene_file).unwrap(), contents);
    assert_eq!(fs::read_to_string(&meta_file).unwrap(), meta);
    let changed = format!("{contents}# new source revision\n");
    assert!(
        validate(changed.as_bytes()).is_err(),
        "proof accepted changed scene content"
    );
    write(&meta_file, format!("{meta}#"));
    assert!(
        validate(contents.as_bytes()).is_err(),
        "proof accepted changed metadata"
    );
    write(&meta_file, meta);
    proof.backend = "unity_editor".into();
    write(&output, marshal(&proof).unwrap());
    assert!(
        validate(contents.as_bytes()).is_err(),
        "source proof was accepted after relabeling it as Editor evidence"
    );
}

#[test]
fn source_scene_rejects_baked_and_malformed_references() {
    for field in [
        "m_LightingDataAsset: {fileID: 20201, guid: 77777777777777777777777777777777}",
        "m_LightingDataAsset: invalid",
        "m_LightingSettings: {fileID: 11400000, guid: 77777777777777777777777777777777}",
        "m_OcclusionCullingData: {fileID: 12}",
        "m_NavMeshData: {fileID: 12}",
        "m_Lightmaps: [{texture: {fileID: 12}}]",
        "m_Lightmaps: invalid",
    ] {
        let content = format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!157 &3\nLightmapSettings:\n  {field}\n"
        );
        assert!(
            inspect_source_scene(content.as_bytes()).is_err(),
            "unsupported baked state was accepted: {field}"
        );
    }
}
