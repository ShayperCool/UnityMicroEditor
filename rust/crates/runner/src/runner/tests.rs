//! Tests of launching, watching, patching, content routes and prepared launches,
//! plus fake-Player tests of the process handling.
use super::*;
use crate::{
    ENGINE_VERSION,
    project::collect_shader_source_requirements,
    sysutil::testing::{temp_dir, write_executable},
    util::{atomic_json, file_hash},
};
use serde_json::{Value as Json, json};
use std::{collections::BTreeMap, fs, time::Duration};

fn write(path: &str, data: &str) {
    fs::create_dir_all(sysutil::dir(path)).unwrap();
    fs::write(path, data).unwrap();
}
fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => err_text(&error),
    }
}

// Launcher

#[test]
fn player_requires_hardware_renderer() {
    for device in [
        "NVIDIA GeForce RTX 4060/PCIe/SSE2",
        "AMD Radeon RX 6600 (radeonsi, LLVM 20.1.0, DRM 3.61)",
        "Mesa Intel(R) Xe Graphics (TGL GT2)",
    ] {
        require_hardware_graphics(device, "OpenGLCore").unwrap();
    }
    for (device, kind) in [
        ("", "OpenGLCore"),
        ("Null Device", "Null"),
        ("llvmpipe (LLVM 20.1.0, 256 bits)", "OpenGLCore"),
        ("softpipe", "OpenGLCore"),
        ("lavapipe", "Vulkan"),
        ("Google SwiftShader", "Vulkan"),
        ("GDI Generic", "OpenGLCore"),
        ("Microsoft Basic Render Driver", "Direct3D11"),
    ] {
        assert!(
            require_hardware_graphics(device, kind).is_err(),
            "missing/software renderer accepted: {device} {kind}"
        );
    }
}

// Watcher

/// These inputs must reach the compiler/preparer even when a text editor replaces
/// the file by an atomic rename rather than writing to the existing inode.
#[test]
fn watch_compiler_and_asset_inputs() {
    let (_guard, project) = temp_dir();
    let assets = sysutil::join(&[&project, "Assets"]);
    fs::create_dir(&assets).unwrap();
    let ctx = Context::background().with_cancel();
    let wake = Wake::new();
    let watcher = ProjectWatcher::new(&ctx, &wake, &project).unwrap();
    for name in [
        "Plugin.dll",
        "csc.rsp",
        "texture.jpeg",
        "hud.uxml",
        "hud.uss",
        "music.mp3",
    ] {
        let destination = sysutil::join(&[&assets, name]);
        let temporary = format!("{destination}.tmp");
        fs::write(&temporary, "changed").unwrap();
        fs::rename(&temporary, &destination).unwrap();
        let limit = Instant::now() + Duration::from_secs(3);
        loop {
            let event = watcher
                .recv_timeout(limit.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| panic!("watcher missed atomic save of {name}"));
            assert!(event.error.is_none(), "{:?}", event.error);
            if event.path == destination {
                break;
            }
        }
    }
    ctx.cancel();
}

#[test]
fn watch_follows_new_directories_and_rejects_symlinks() {
    let (_guard, project) = temp_dir();
    let assets = sysutil::join(&[&project, "Assets"]);
    fs::create_dir_all(sysutil::join(&[&assets, "Existing/Deep"])).unwrap();
    let ctx = Context::background().with_cancel();
    let wake = Wake::new();
    let watcher = ProjectWatcher::new(&ctx, &wake, &project).unwrap();
    let expect = |path: &str| {
        let limit = Instant::now() + Duration::from_secs(3);
        loop {
            let event = watcher
                .recv_timeout(limit.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| panic!("watcher missed {path}"));
            assert!(event.error.is_none(), "{:?}", event.error);
            if event.path == path {
                return;
            }
        }
    };
    let deep = sysutil::join(&[&assets, "Existing/Deep/Scene.unity"]);
    fs::write(&deep, "scene").unwrap();
    expect(&deep);
    let created = sysutil::join(&[&assets, "New"]);
    fs::create_dir(&created).unwrap();
    expect(&created);
    let script = sysutil::join(&[&created, "Script.cs"]);
    fs::write(&script, "class A {}").unwrap();
    expect(&script);
    // Irrelevant files are filtered, directory events are not.
    fs::write(sysutil::join(&[&created, "notes.md"]), "x").unwrap();
    fs::remove_file(&script).unwrap();
    expect(&script);
    drop(watcher);
    std::os::unix::fs::symlink(&created, sysutil::join(&[&assets, "Link"])).unwrap();
    let refused = failure(ProjectWatcher::new(&ctx, &wake, &project));
    assert_eq!(
        refused,
        format!("source watch rejects symlinks: {assets}/Link")
    );
}

// Runtime equality

fn write_json(root: &str, name: &str, value: &Json) -> String {
    let path = sysutil::join(&[root, name]);
    atomic_json(&path, value).unwrap();
    path
}

#[test]
fn runtime_equality_skips_repeated_source_write_but_keeps_asset_changes() {
    let (_guard, root) = temp_dir();
    let previous = write_json(
        &root,
        "old.json",
        &json!({"revision": "old", "manifest": "old.json", "contentHash": "a", "objects": [], "sourceDependencies": [{"path": "Assets/Texture.png", "sha256": "same"}]}),
    );
    let same = write_json(
        &root,
        "same.json",
        &json!({"revision": "new", "manifest": "same.json", "contentHash": "b", "objects": [], "sourceDependencies": [{"path": "Assets/Texture.png", "sha256": "same"}]}),
    );
    let changed = write_json(
        &root,
        "changed.json",
        &json!({"revision": "new", "objects": [], "sourceDependencies": [{"path": "Assets/Texture.png", "sha256": "different"}]}),
    );
    assert!(scene_runtime_equal(&previous, &same).unwrap());
    assert!(!scene_runtime_equal(&previous, &changed).unwrap());
    let settings = write_json(
        &root,
        "settings.json",
        &json!({"revision": "old", "manifest": "old.json", "contentHash": "a", "objects": [], "sourceDependencies": [{"path": "Assets/Texture.png", "sha256": "same"}], "renderPipelineGuid": "changed"}),
    );
    assert!(!scene_runtime_equal(&previous, &settings).unwrap());
}

#[test]
fn runtime_equality_whitespace_only_scene_and_prefab_writes_are_no_op() {
    let (_guard, root) = temp_dir();
    let make = |scene_source: &str, prefab_source: &str| {
        json!({
            "revision": "original",
            "objects": [{"fileId": "10", "classId": 1, "dataJson": r#"{"m_Name":"Scene object","m_IsActive":1}"#}],
            "assets": [{"guid": "11111111111111111111111111111111", "kind": "prefab", "objects": [{"fileId": "20", "dataJson": r#"{"m_Name":"Prefab object"}"#}]}],
            "sourceDependencies": [
                {"path": "Assets/Scenes/Sample.unity", "sha256": sha256(scene_source.as_bytes()), "bytes": scene_source.len()},
                {"path": "Assets/Prefab.prefab", "sha256": sha256(prefab_source.as_bytes()), "bytes": prefab_source.len()},
                {"path": "Assets/Texture.png", "sha256": "unchanged texture"},
            ],
        })
    };
    let source = "--- !u!1 &10\nGameObject:\n  m_Name: Scene object\n";
    let prefab = "--- !u!1 &20\nGameObject:\n  m_Name: Prefab object\n";
    let previous = write_json(&root, "previous.json", &make(source, prefab));
    for kind in ["scene", "prefab", "both"] {
        let (mut scene_next, mut prefab_next) = (source.to_string(), prefab.to_string());
        if kind != "prefab" {
            scene_next += "\n\n";
        }
        if kind != "scene" {
            prefab_next += "\n\n";
        }
        let mut next = make(&scene_next, &prefab_next);
        next["revision"] = json!("whitespace changed");
        let path = write_json(&root, &format!("{kind}.json"), &next);
        assert!(
            scene_runtime_equal(&previous, &path).unwrap(),
            "whitespace-only {kind} write reset runtime graph"
        );
    }
    // Ignoring scene provenance may not hide either changed scene fields or raw
    // runtime prefab fields. Both require a new runtime revision.
    let mut changed_scene = make(&format!("{source}\n"), prefab);
    changed_scene["objects"][0]["dataJson"] =
        json!(r#"{"m_Name":"Edited scene object","m_IsActive":1}"#);
    let mut changed_prefab = make(source, &format!("{prefab}\n"));
    changed_prefab["assets"][0]["objects"][0]["dataJson"] =
        json!(r#"{"m_Name":"Edited prefab object"}"#);
    for (name, doc) in [
        ("scene runtime", changed_scene),
        ("prefab runtime", changed_prefab),
    ] {
        let path = write_json(&root, &format!("{name}.json"), &doc);
        assert!(
            !scene_runtime_equal(&previous, &path).unwrap(),
            "changed {name} fields were treated as no-op"
        );
    }
}

// Content routes

const SHIP_GUID: &str = "a4c2d5e87cc894d488ffbfd8c52ed16a";

#[test]
fn scene_route_requires_exact_authored_path_and_guid() {
    let path = "Assets/Scenes/Demo.unity";
    assert_eq!(
        parse_content_scene(&format!("{SHIP_GUID}={path}")).unwrap(),
        (SHIP_GUID.to_string(), path.to_string())
    );
    for invalid in [
        path.to_string(),
        format!("GUID={path}"),
        format!("{SHIP_GUID}=/tmp/scene.unity"),
        format!("{SHIP_GUID}=Assets/../scene.unity"),
        format!("{SHIP_GUID}=Assets/scene.prefab"),
    ] {
        assert_eq!(
            failure(parse_content_scene(&invalid)),
            format!("content scene must be lowercase GUID=Assets/...unity: {invalid}")
        );
    }
}

fn route_manifest(root: &str, revision: &str, extra: Json) -> String {
    let mut manifest = json!({"projectRoot": root, "sourceScenePath": "Assets/Ship.unity", "revision": revision,
        "objects": [{"fileId": "1", "classId": 1, "dataJson": r#"{"m_Name":"Ship","m_IsActive":1}"#}],
        "sourceDependencies": [{"path": "Assets/Ship.unity", "sha256": revision, "bytes": revision.len()}]});
    if let Json::Object(extra) = extra {
        for (key, value) in extra {
            manifest[key] = value;
        }
    }
    let path = sysutil::join(&[root, revision, "scene.json"]);
    fs::create_dir_all(sysutil::dir(&path)).unwrap();
    atomic_json(&path, &manifest).unwrap();
    path
}
fn route_snapshot(root: &str, manifests: &[&str]) -> String {
    let guids = [SHIP_GUID, "b4c2d5e87cc894d488ffbfd8c52ed16a"];
    let scenes = manifests
        .iter()
        .zip(guids)
        .map(|(manifest, guid)| RawSceneRoute {
            guid: guid.into(),
            manifest: manifest.to_string(),
            sha256: file_hash(manifest).unwrap(),
        })
        .collect();
    publish_content_routes(
        root,
        &RawContentRoutes {
            schema_version: 1,
            project: root.into(),
            scenes: Some(scenes),
            addressables: Vec::new(),
            build_scenes: Vec::new(),
        },
    )
    .unwrap()
}

#[test]
fn content_routes_carry_the_build_scenes_the_host_stands_in_for() {
    let (_guard, root) = temp_dir();
    let main = route_manifest(&root, "main", Json::Null);
    let guid = "c".repeat(32);
    let routes = RawContentRoutes {
        schema_version: 1,
        project: root.clone(),
        scenes: Some(vec![RawSceneRoute {
            guid: guid.clone(),
            manifest: main.clone(),
            sha256: file_hash(&main).unwrap(),
        }]),
        addressables: Vec::new(),
        build_scenes: vec![super::addressables::BuildSceneRoute {
            index: 1,
            guid: guid.clone(),
            path: String::new(),
        }],
    };
    let path = publish_content_routes(&root, &routes).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(&format!(r#""buildScenes":[{{"index":1,"guid":"{guid}"}}]"#)),
        "{text}"
    );
    assert_eq!(RawContentRoutes::read(&path).unwrap(), routes);
}

#[test]
fn content_route_watch_detects_ship_changes_with_unchanged_boot() {
    let (_guard, root) = temp_dir();
    let before = route_manifest(&root, "before", Json::Null);
    let boot = route_manifest(&root, "boot", Json::Null);
    let old_routes = route_snapshot(&root, &[&before]);
    for (name, extra, equal) in [
        ("yaml-formatting", Json::Null, true),
        (
            "added-game-object",
            json!({"objects": [{"fileId": "1"}, {"fileId": "2"}]}),
            false,
        ),
        (
            "prefab-component-edit",
            json!({"objects": [{"fileId": "1", "dataJson": r#"{"m_Name":"New ship"}"#}]}),
            false,
        ),
        (
            "native-asset-edit",
            json!({"nativeContentFiles": [{"sha256": "changed-model"}]}),
            false,
        ),
        (
            "material-edit",
            json!({"assets": [{"kind": "material", "color": "new-color"}]}),
            false,
        ),
    ] {
        let next = route_manifest(&root, name, extra);
        let new_routes = route_snapshot(&root, &[&next]);
        assert!(scene_runtime_equal(&boot, &boot).unwrap());
        assert_eq!(
            content_routes_runtime_equal(&old_routes, &new_routes).unwrap(),
            equal,
            "{name}"
        );
    }
}

#[test]
fn content_routes_are_immutable_and_validate_manifest_hashes() {
    let (_guard, root) = temp_dir();
    let old_manifest = route_manifest(&root, "old", Json::Null);
    let new_manifest = route_manifest(&root, "new", Json::Null);
    let old_path = route_snapshot(&root, &[&old_manifest]);
    let old_bytes = fs::read(&old_path).unwrap();
    let new_path = route_snapshot(&root, &[&new_manifest]);
    assert_ne!(old_path, new_path);
    assert_eq!(fs::read(&old_path).unwrap(), old_bytes);
    assert_eq!(route_snapshot(&root, &[&old_manifest]), old_path);
    fs::write(&new_manifest, r#"{"objects":[]}"#).unwrap();
    assert_eq!(
        failure(content_routes_runtime_equal(&old_path, &new_path)),
        format!("published content manifest changed: {new_manifest}")
    );
    let routes = RawContentRoutes::read(&old_path).unwrap();
    fs::write(&old_path, "{}").unwrap();
    assert_eq!(
        failure(publish_content_routes(&root, &routes)),
        "published content routes were modified"
    );
}

#[test]
fn content_route_comparison_uses_guid_not_declaration_order() {
    let (_guard, root) = temp_dir();
    let a = route_manifest(&root, "a", Json::Null);
    let b = route_manifest(&root, "b", Json::Null);
    let first = route_snapshot(&root, &[&a, &b]);
    let mut routes = RawContentRoutes::read(&first).unwrap();
    routes.scenes.as_mut().unwrap().swap(0, 1);
    let reordered = publish_content_routes(&root, &routes).unwrap();
    assert!(content_routes_runtime_equal(&first, &reordered).unwrap());
    routes.scenes.as_mut().unwrap()[0].guid = "c4c2d5e87cc894d488ffbfd8c52ed16a".into();
    let remapped = publish_content_routes(&root, &routes).unwrap();
    assert!(!content_routes_runtime_equal(&first, &remapped).unwrap());
}

#[test]
fn game_arguments_keep_values_and_do_not_consume_runner_flags() {
    let args: Vec<String> = [
        "--scene",
        "Assets/Boot.unity",
        "--",
        "-host",
        "-name",
        "Captain 1",
        "-relay",
        "ws://127.0.0.1:8800",
        "-autostart",
        "2",
    ]
    .map(String::from)
    .to_vec();
    let (runner, game) = split_game_arguments(&args);
    assert_eq!(runner, args[..2]);
    assert_eq!(game, args[3..]);
    validate_game_arguments(&game).unwrap();
    assert!(validate_game_arguments(&["--unity-cli-frames".into(), "1".into()]).is_err());
    assert!(validate_game_arguments(&["a\0b".into()]).is_err());
}

// Prepared launches

#[test]
fn prepared_scene_requires_matching_source_and_assembly_content() {
    let (_guard, project) = temp_dir();
    let assembly = sysutil::join(&[&project, "Temp/Game.dll"]);
    let source = sysutil::join(&[&project, "Assets/Boot.unity"]);
    write(&assembly, "game assembly");
    write(&source, "authored source");
    let manifest = sysutil::join(&[&project, "Temp/manifest.json"]);
    fs::write(
        &manifest,
        json!({"schemaVersion": 1, "supported": true, "projectRoot": project, "unityVersion": "6000.6.3f1", "assemblies": [assembly], "sourceDependencies": [{"path": "Assets/Boot.unity", "sha256": file_hash(&source).unwrap()}]}).to_string(),
    )
    .unwrap();
    let manifest_hash = file_hash(&manifest).unwrap();
    let launch = PreparedLaunch {
        project: project.clone(),
        assembly_hashes: BTreeMap::from([(assembly.clone(), file_hash(&assembly).unwrap())]),
        ..Default::default()
    };
    let validate =
        || validate_prepared_scene(&launch, &manifest, &manifest_hash, &mut BTreeMap::new());
    validate().unwrap();
    fs::write(&source, "new authored scene").unwrap();
    assert!(validate().is_err(), "launched a stale source snapshot");
    fs::write(&source, "authored source").unwrap();
    fs::write(&assembly, "new game assembly").unwrap();
    assert!(validate().is_err(), "launched an unverified managed DLL");
}

#[test]
fn prepared_artifact_rejects_conflicting_hashes() {
    let (_guard, root) = temp_dir();
    let file = sysutil::join(&[&root, "artifact"]);
    fs::write(&file, "immutable").unwrap();
    let mut observed = BTreeMap::new();
    require_prepared_file(&file, &file_hash(&file).unwrap(), &mut observed).unwrap();
    assert_eq!(
        failure(require_prepared_file(
            &file,
            &sha256(b"another version"),
            &mut observed
        )),
        format!("prepared artifacts disagree on content: {file}")
    );
}

#[test]
fn prepared_native_only_scene_requires_verified_host_registrations() {
    let (_guard, project) = temp_dir();
    let player = sysutil::join(&[&project, "Library/UnityMicroEditor/Host/Game.x86_64"]);
    let registrations =
        sysutil::join(&[&sysutil::dir(&player), "Game_Data/ScriptingAssemblies.json"]);
    let contents = r#"{"names":["UnityMicroEditor.dll","UnityEngine.CoreModule.dll"]}"#;
    write(&registrations, contents);
    let manifest = sysutil::join(&[&project, "manifest.json"]);
    let mut value = json!({"schemaVersion": 1, "supported": true, "projectRoot": project,
        "unityVersion": ENGINE_VERSION, "assemblies": [],
        "compiler": {"nativeOnly": true, "nativeSlotsSha256": sha256(contents.as_bytes())}});
    let validate = |value: &Json| {
        let data = value.to_string();
        fs::write(&manifest, &data).unwrap();
        let launch = PreparedLaunch {
            project: project.clone(),
            player: player.clone(),
            ..Default::default()
        };
        validate_prepared_scene(
            &launch,
            &manifest,
            &sha256(data.as_bytes()),
            &mut BTreeMap::new(),
        )
    };
    validate(&value).unwrap();
    value.as_object_mut().unwrap().remove("compiler");
    assert!(
        validate(&value).is_err(),
        "unproved empty game assembly list was accepted"
    );
    value["compiler"] = json!({"nativeOnly": true});
    assert!(
        validate(&value).is_err(),
        "native-only scene without registration digest was accepted"
    );
    value["compiler"] =
        json!({"nativeOnly": true, "nativeSlotsSha256": sha256(contents.as_bytes())});
    fs::write(&registrations, r#"{"names":["Other.dll"]}"#).unwrap();
    assert!(
        validate(&value).is_err(),
        "changed native host registrations were accepted"
    );
    let missing_bridge = r#"{"names":["UnityEngine.CoreModule.dll"]}"#;
    fs::write(&registrations, missing_bridge).unwrap();
    value["compiler"] =
        json!({"nativeOnly": true, "nativeSlotsSha256": sha256(missing_bridge.as_bytes())});
    assert_eq!(
        failure(validate(&value)),
        "native-only prepared host has no registered runtime bridge"
    );
    value["assemblies"] = json!(["Game.dll"]);
    assert!(
        validate(&value).is_err(),
        "native-only proof accepted additional game assemblies"
    );
}

#[test]
fn prepared_package_source_resolves_cache_and_rejects_ambiguity() {
    let (_guard, project) = temp_dir();
    let logical = "Packages/com.unity.test/Shaders/Unlit.shader";
    let physical = sysutil::join(&[
        &project,
        "Library/PackageCache/com.unity.test@abc/Shaders/Unlit.shader",
    ]);
    write(&physical, "current shader");
    let resolved = resolve_prepared_source(&project, logical).unwrap();
    assert_eq!(resolved, physical);
    let sha = file_hash(&physical).unwrap();
    require_prepared_file(&resolved, &sha, &mut BTreeMap::new()).unwrap();
    fs::write(&physical, "edited shader").unwrap();
    assert!(
        require_prepared_file(&resolved, &sha, &mut BTreeMap::new()).is_err(),
        "UPM cache edit was not rejected"
    );
    fs::create_dir_all(sysutil::join(&[
        &project,
        "Library/PackageCache/com.unity.test@another",
    ]))
    .unwrap();
    assert!(
        resolve_prepared_source(&project, logical).is_err(),
        "ambiguous UPM cache selection accepted"
    );
    for invalid in ["", "a/../b", "../x", "a//b"] {
        assert_eq!(
            failure(resolve_prepared_source(&project, invalid)),
            format!("prepared source path is invalid: {invalid}")
        );
    }
}

#[test]
fn prepared_scene_rechecks_shader_include_closure_before_launching() {
    let (_guard, project) = temp_dir();
    let assembly = sysutil::join(&[&project, "Temp/Game.dll"]);
    write(&assembly, "game assembly");
    write(
        &sysutil::join(&[&project, "Assets/Screen.shader"]),
        "Shader \"Test/Screen\" {\n#if OPTIONAL\n#include \"Optional.hlsl\"\n#endif\n}\n",
    );
    let (requirements, missing) =
        collect_shader_source_requirements(&project, "Assets/Screen.shader").unwrap();
    let manifest = sysutil::join(&[&project, "Temp/manifest.json"]);
    let snapshot = crate::project::CompiledShaderSnapshot {
        schema_version: 1,
        source_path: "Assets/Screen.shader".into(),
        source_requirements: requirements.into(),
        missing_includes: missing.into(),
        ..Default::default()
    };
    fs::write(
        &manifest,
        json!({"schemaVersion": 1, "supported": true, "projectRoot": project, "unityVersion": "6000.6.3f1", "assemblies": [assembly],
            "compiledShaderSnapshots": [snapshot]}).to_string(),
    )
    .unwrap();
    let manifest_hash = file_hash(&manifest).unwrap();
    let launch = PreparedLaunch {
        project: project.clone(),
        assembly_hashes: BTreeMap::from([(assembly.clone(), file_hash(&assembly).unwrap())]),
        ..Default::default()
    };
    let validate =
        || validate_prepared_scene(&launch, &manifest, &manifest_hash, &mut BTreeMap::new());
    validate().unwrap();
    write(
        &sysutil::join(&[&project, "Assets/Optional.hlsl"]),
        "new variant input",
    );
    assert!(
        validate().is_err(),
        "prepared launch ignored a new optional Shader include"
    );
}

// Fake Players: a shell script standing in for the Mono Player.

/// Writes the ready report the launcher polls for and then behaves as `body` says.
fn fake_player(root: &str, ready: &str, body: &str) -> String {
    let path = sysutil::join(&[root, "Host.x86_64"]);
    write_executable(
        &path,
        &format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --unity-cli-report) report=\"$2\"; shift;; -logFile) log=\"$2\"; shift;; esac; shift; done\n{}\n{body}\n",
            ready.replace("READY", "\"$report.ready.json\"")
        ),
    );
    path
}
const READY: &str = r#"printf '{"schemaVersion":1,"pid":%d,"errors":0,"Exceptions":0,"graphicsDeviceName":"NVIDIA GeForce RTX 4060","graphicsDeviceType":"OpenGLCore","scene":{"isLoaded":true}}' $$ > READY.tmp && mv READY.tmp READY"#;

fn launch(root: &str, player: &str, wake: &Arc<Wake>) -> Player {
    let manifest = sysutil::join(&[root, "manifest.json"]);
    fs::write(&manifest, "{}").unwrap();
    let run = sysutil::join(&[root, &format!("runs/{}", random_id())]);
    fs::create_dir_all(&run).unwrap();
    let opts = LaunchOptions {
        frames: 120,
        width: 960,
        height: 540,
        fps: 60,
        game_arguments: vec!["-host".into()],
        ..Default::default()
    };
    start_player(wake, &opts, player, &manifest, &run, "").unwrap()
}
fn alive(pid: i32) -> bool {
    sysutil::kill(i64::from(pid), 0).is_ok()
}

#[test]
fn fake_player_becomes_ready_reports_and_stops_on_interrupt() {
    let (_guard, root) = temp_dir();
    let player = fake_player(
        &root,
        READY,
        "printf '%s\\n' \"$PWD\" > \"$report.cwd\"\ntrap 'exit 0' INT\nwhile :; do sleep 0.05; done",
    );
    let wake = Wake::new();
    let ctx = Context::background();
    let mut player = launch(&root, &player, &wake);
    player
        .wait_ready(&ctx, ReadyBudget::fixed(Duration::from_secs(10)))
        .unwrap();
    assert!(alive(player.pid()));
    // The Player leads its own process group and runs in its run directory.
    assert_eq!(unsafe { libc::getpgid(player.pid()) }, player.pid());
    assert_eq!(
        fs::read_to_string(format!("{}.cwd", player.report))
            .unwrap()
            .trim(),
        sysutil::dir(&player.report)
    );
    let started = Instant::now();
    player.stop();
    assert!(player.exited && started.elapsed() < Duration::from_secs(2));
    assert!(!alive(player.pid()));
    player.stop();
}

#[test]
fn fake_player_stop_escalates_to_terminate() {
    let (_guard, root) = temp_dir();
    let player = fake_player(
        &root,
        READY,
        "trap '' INT\ntrap 'exit 0' TERM\nwhile :; do sleep 0.05; done",
    );
    let wake = Wake::new();
    let mut player = launch(&root, &player, &wake);
    player
        .wait_ready(
            &Context::background(),
            ReadyBudget::fixed(Duration::from_secs(10)),
        )
        .unwrap();
    let started = Instant::now();
    player.stop();
    let elapsed = started.elapsed();
    assert!(player.exited && !alive(player.pid()));
    assert!(
        elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
}

#[test]
fn fake_player_readiness_failures() {
    let (_guard, root) = temp_dir();
    let wake = Wake::new();
    let ctx = Context::background();
    // Exit before the report exists.
    let mut player = launch(&root, &fake_player(&root, "", "exit 3"), &wake);
    assert_eq!(
        failure(player.wait_ready(&ctx, ReadyBudget::fixed(Duration::from_secs(10)))),
        "Player exited before ready: exit status 3"
    );
    let mut player = launch(&root, &fake_player(&root, "", "exit 0"), &wake);
    assert_eq!(
        failure(player.wait_ready(&ctx, ReadyBudget::fixed(Duration::from_secs(10)))),
        "Player exited before ready: <nil>"
    );
    // A report of another process, with errors, or from a software renderer.
    let keep = "while :; do sleep 0.05; done";
    for (ready, expected) in [
        (
            READY.replace("$$", "1"),
            "invalid Player ready report: pid=1 errors=0 exceptions=0 loaded=true".to_string(),
        ),
        (
            READY.replace("\"errors\":0", "\"errors\":2"),
            "errors=2 exceptions=0 loaded=true".to_string(),
        ),
        (
            READY.replace("\"isLoaded\":true", "\"isLoaded\":false"),
            "loaded=false".to_string(),
        ),
        (
            READY.replace(
                "NVIDIA GeForce RTX 4060",
                "llvmpipe (LLVM 20.1.0, 256 bits)",
            ),
            "Player uses a software renderer: llvmpipe (LLVM 20.1.0, 256 bits)".to_string(),
        ),
    ] {
        let mut player = launch(&root, &fake_player(&root, &ready, keep), &wake);
        let error = failure(player.wait_ready(&ctx, ReadyBudget::fixed(Duration::from_secs(10))));
        assert!(error.contains(&expected), "{error}");
        player.stop();
    }
    // Errors logged before ready: the first is quoted with the log (mcp6-5).
    let logged = format!(
        "printf 'Loaded\\nUnityEngine.Debug:Log (object)\\n\\nUnsupported enum type '\\''RgLong'\\'' used for field '\\''longKind'\\''\\nUnityEngine.Debug:ExtractStackTraceNoAlloc (byte*,int,string)\\n' > \"$log\"\n{}",
        READY.replace("\"errors\":0", "\"errors\":9")
    );
    let mut player = launch(&root, &fake_player(&root, &logged, keep), &wake);
    let error = failure(player.wait_ready(&ctx, ReadyBudget::fixed(Duration::from_secs(10))));
    let log = sysutil::join(&[&sysutil::dir(&player.report), "player.log"]);
    assert!(
        error.contains("errors=9")
            && error.ends_with(&format!(
                "the first: \"Unsupported enum type 'RgLong' used for field 'longKind'\"; see {log}"
            )),
        "{error}"
    );
    player.stop();
    // Timeout and cancellation.
    let mut player = launch(&root, &fake_player(&root, "", keep), &wake);
    let error = failure(player.wait_ready(&ctx, ReadyBudget::fixed(Duration::from_secs(1))));
    assert!(
        error.starts_with("Player start: not ready within its 1s budget (it had 0.")
            && error.ends_with(&format!("s of processor time); see {}", player.report)),
        "{error}"
    );
    let canceled = Context::background().with_cancel();
    canceled.cancel();
    assert_eq!(
        failure(player.wait_ready(&canceled, ReadyBudget::fixed(Duration::from_secs(10)))),
        "canceled"
    );
    player.stop();
}

#[test]
fn readiness_budget_extends_only_for_a_player_that_hardly_ran() {
    let seconds = Duration::from_secs;
    let budget = ReadyBudget {
        budget: seconds(45),
        limit: seconds(300),
        ran: seconds(30),
    };
    // Within the budget nothing is decided, whatever the Player used.
    assert_eq!(budget.verdict(seconds(44), Some(seconds(400))), None);
    // A Player that ran has its budget and no more.
    assert_eq!(
        budget.verdict(seconds(45), Some(seconds(30))).unwrap(),
        "Player start: not ready within its 45s budget (it had 30.0s of processor time)"
    );
    // So has one whose processor time cannot be read.
    assert_eq!(
        budget.verdict(seconds(45), None).unwrap(),
        "Player start: not ready within its 45s budget (it had an unknown amount of processor time)"
    );
    // One that hardly ran is waited for, until it has run or the limit.
    assert_eq!(
        budget.verdict(seconds(45), Some(Duration::from_millis(400))),
        None
    );
    assert_eq!(budget.verdict(seconds(299), Some(seconds(29))), None);
    assert!(budget.verdict(seconds(120), Some(seconds(31))).is_some());
    assert_eq!(
        budget
            .verdict(seconds(300), Some(Duration::from_millis(400)))
            .unwrap(),
        "Player start: not ready within 300s, the limit for a Player that hardly gets to run (budget 45s; it had 0.4s of processor time)"
    );
    // A fixed budget never extends.
    let fixed = ReadyBudget::fixed(seconds(10));
    assert_eq!(fixed.verdict(seconds(9), Some(Duration::ZERO)), None);
    assert_eq!(
        fixed.verdict(seconds(10), Some(Duration::ZERO)).unwrap(),
        "Player start: not ready within its 10s budget (it had 0.0s of processor time)"
    );
}

#[test]
fn fake_player_that_hardly_runs_is_waited_for_beyond_its_budget() {
    let (_guard, root) = temp_dir();
    let wake = Wake::new();
    let ctx = Context::background();
    let budget = ReadyBudget {
        budget: Duration::from_millis(200),
        limit: Duration::from_secs(20),
        ran: Duration::from_secs(5),
    };
    // Asleep for longer than the budget, as a Player a busy machine does
    // not run, then ready.
    let script = fake_player(&root, &format!("sleep 1.5\n{READY}"), "exec sleep 30");
    let mut player = launch(&root, &script, &wake);
    let started = Instant::now();
    player.wait_ready(&ctx, budget).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1400));
    player.stop();
    // One that never is ready is given up at the limit, which is named.
    let limited = ReadyBudget {
        limit: Duration::from_millis(1500),
        ..budget
    };
    let mut player = launch(&root, &fake_player(&root, "", "exec sleep 30"), &wake);
    let started = Instant::now();
    let error = failure(player.wait_ready(&ctx, limited));
    assert!(started.elapsed() >= Duration::from_millis(1500));
    assert!(
        error.starts_with(
            "Player start: not ready within 1s, the limit for a Player that hardly gets to run (budget 0s; it had 0.0s of processor time); see "
        ),
        "{error}"
    );
    player.stop();
}

/// A shell that was started with SIGINT ignored cannot trap it, and a tool
/// started in the background of a script has it ignored.
#[test]
fn fake_player_is_interrupted_when_the_tool_ignores_interrupts() {
    crate::sysutil::testing::pass_ignoring_interrupts(
        "runner::tests::fake_player_becomes_ready_reports_and_stops_on_interrupt",
    );
}

#[test]
fn player_reports_and_reload_acknowledgements() {
    let (_guard, root) = temp_dir();
    let report = sysutil::join(&[&root, "report.json"]);
    assert!(failure(require_player_report(&report)).ends_with("no such file or directory"));
    fs::write(
        &report,
        r#"{"errors":0,"EXCEPTIONS":0,"fatalError":"","exitCode":0}"#,
    )
    .unwrap();
    require_player_report(&report).unwrap();
    fs::write(
        &report,
        r#"{"errors":1,"exceptions":2,"fatalError":"boom","exitCode":3}"#,
    )
    .unwrap();
    assert_eq!(
        failure(require_player_report(&report)),
        "Player errors=1 exceptions=2 exit=3: boom"
    );
    fs::write(&report, r#"{"errors":"1"}"#).unwrap();
    assert_eq!(
        failure(require_player_report(&report)),
        "json: cannot unmarshal string into field .Errors of type int"
    );
    let ctx = Context::background();
    let ack = sysutil::join(&[&root, "ack.json"]);
    let timeout = wait_reload_ack(&ctx, &ack, "id", Duration::from_millis(50));
    assert!(timeout.ack.is_none());
    assert_eq!(
        failure::<()>(Err(timeout.error.unwrap())),
        "Player reload acknowledgement timed out"
    );
    fs::write(&ack, r#"{"requestId":"other","success":true}"#).unwrap();
    assert!(
        wait_reload_ack(&ctx, &ack, "id", Duration::from_millis(50))
            .ack
            .is_none()
    );
    fs::write(
        &ack,
        r#"{"requestId":"id","success":false,"fatalError":"bad scene","pid":7}"#,
    )
    .unwrap();
    let rejected = wait_reload_ack(&ctx, &ack, "id", Duration::from_secs(5));
    assert_eq!(
        value::compact_map(&rejected.ack.unwrap()).unwrap(),
        r#"{"fatalError":"bad scene","pid":7,"requestId":"id","success":false}"#
    );
    assert_eq!(
        failure::<()>(Err(rejected.error.unwrap())),
        "Player rejected revision: bad scene"
    );
    fs::write(&ack, r#"{"requestId":"id","success":false}"#).unwrap();
    let rejected = wait_reload_ack(&ctx, &ack, "id", Duration::from_secs(5));
    assert_eq!(
        failure::<()>(Err(rejected.error.unwrap())),
        "Player rejected revision: <nil>"
    );
    fs::write(
        &ack,
        r#"{"requestId":"id","success":true,"loadedRevision":"r"}"#,
    )
    .unwrap();
    let accepted = wait_reload_ack(&ctx, &ack, "id", Duration::from_secs(5));
    assert!(accepted.error.is_none() && accepted.ack.is_some());
}

#[test]
fn relevant_sources() {
    for path in [
        "A.unity",
        "a/b.PREFAB",
        "x.cs",
        "x.meta",
        "x.mp4",
        "x.Bytes",
        "x.inputactions",
        "x.mask",
        "x.overrideController",
    ] {
        assert!(relevant_source(path), "{path}");
    }
    for path in ["x", "x.md", "x.cs~", ".unity/x", "x.csproj", "x.tmp"] {
        assert!(!relevant_source(path), "{path}");
    }
}

#[test]
fn baseline_and_launch_descriptors_decode() {
    let (_guard, root) = temp_dir();
    let path = sysutil::join(&[&root, "current.json"]);
    fs::write(&path, r#"{"Project":"/p","manifest":"/m","player":null,"nativeImport":{"CATALOG":"/c","index":"/i"},"extra":[1]}"#).unwrap();
    assert_eq!(
        BaselineBuild::read(&path).unwrap(),
        BaselineBuild {
            project: "/p".into(),
            manifest: "/m".into(),
            player: String::new(),
            native_import: BaselineNativeImport {
                catalog: "/c".into(),
                index: "/i".into(),
            },
        }
    );
    fs::write(&path, r#"{"project":1}"#).unwrap();
    assert_eq!(
        failure(BaselineBuild::read(&path)),
        "json: cannot unmarshal number into field baselineBuild.project of type string"
    );
    fs::write(&path, "[]").unwrap();
    assert_eq!(
        failure(BaselineBuild::read(&path)),
        "json: cannot unmarshal array into value of type baselineBuild"
    );
    fs::write(&path, "{").unwrap();
    assert_eq!(
        failure(BaselineBuild::read(&path)),
        "unexpected end of JSON input"
    );
    fs::write(
        &path,
        r#"{"runtimeInitializers":[{"assembly":"A","phase":1,"n":1e21},null]}"#,
    )
    .unwrap();
    assert_eq!(
        json::read_runtime_initializers(&path)
            .unwrap()
            .to_json()
            .unwrap(),
        r#"[{"assembly":"A","n":1e+21,"phase":1},null]"#
    );
    fs::write(&path, "{}").unwrap();
    assert_eq!(json::read_runtime_initializers(&path).unwrap(), Value::Null);
}

// Hot reload at the Player's launch

/// A stand-in Player that records its arguments, the Mono options of its
/// environment and what its standard input is, then becomes ready.
fn recording_player(root: &str) -> String {
    let path = sysutil::join(&[root, "Host.x86_64"]);
    write_executable(
        &path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' \"${{MONO_ARGUMENTS-<unset>}}\" > \"$0.mono\"\nreadlink /proc/$$/fd/0 > \"$0.stdin\"\nwhile [ $# -gt 0 ]; do case \"$1\" in --unity-cli-report) report=\"$2\"; shift;; esac; shift; done\n{}\ntrap 'exit 0' INT\nwhile :; do sleep 0.05; done\n",
            READY.replace("READY", "\"$report.ready.json\"")
        ),
    );
    path
}

/// Starts the recording Player and returns its arguments and Mono options.
fn launch_recording(opts: &LaunchOptions) -> (Vec<String>, String, String) {
    let (_guard, root) = temp_dir();
    let player = recording_player(&root);
    let manifest = sysutil::join(&[&root, "manifest.json"]);
    fs::write(&manifest, "{}").unwrap();
    let run = sysutil::join(&[&root, "runs/r1"]);
    fs::create_dir_all(&run).unwrap();
    // Left by nobody in a real run directory; a request of another process
    // must never be answered by this one.
    let code_request = crate::hotreload::PlayerCode::request_path(&run);
    for stale in [code_request.clone(), format!("{code_request}.ack.json")] {
        fs::write(stale, "{}").unwrap();
    }
    let wake = Wake::new();
    let request = sysutil::join(&[&run, "reload-request.json"]);
    let mut player = start_player(&wake, opts, &player, &manifest, &run, &request).unwrap();
    player
        .wait_ready(
            &Context::background(),
            ReadyBudget::fixed(Duration::from_secs(10)),
        )
        .unwrap();
    let exited = player.exit_probe();
    assert!(!exited());
    let read = |suffix: &str| fs::read_to_string(format!("{}.{suffix}", player.path)).unwrap();
    let args: Vec<String> = read("args").lines().map(str::to_string).collect();
    // No child of a session may take the lines meant for the command reader.
    assert_eq!(read("stdin").trim(), "/dev/null");
    let stale = fs::exists(&code_request).unwrap()
        || fs::exists(format!("{code_request}.ack.json")).unwrap();
    assert_eq!(stale, !(opts.interactive && opts.hot_reload));
    let mono = read("mono");
    player.stop();
    // The probe keeps telling it after the loop has taken the exit result.
    assert!(player.exited && exited());
    (args, mono, run)
}

#[test]
fn player_launch_for_hot_reload() {
    let options = |hot_reload: bool| LaunchOptions {
        frames: 120,
        width: 960,
        height: 540,
        fps: 60,
        watch: true,
        interactive: true,
        hot_reload,
        ..Default::default()
    };
    // What the developer set for the Player's Mono is never replaced.
    let inherited = std::env::var("MONO_ARGUMENTS").ok();
    let (args, mono, run) = launch_recording(&options(true));
    let position = |name: &str| args.iter().position(|argument| argument == name);
    let code = position("--unity-cli-code-reload").expect("the code reload argument");
    assert_eq!(
        args[code + 1],
        sysutil::join(&[&run, "code-reload-request.json"])
    );
    // The bridge takes it only in an interactive run with a raw manifest,
    // and beside a scene reload request of another name.
    assert!(position("--unity-cli-interactive").unwrap() < code);
    assert!(position("--unity-cli-raw-manifest").is_some());
    let scene = position("--unity-cli-raw-reload").unwrap();
    assert_eq!(
        args[scene + 1],
        sysutil::join(&[&run, "reload-request.json"])
    );
    assert_eq!(
        mono,
        inherited.clone().unwrap_or("-O=-float32,-inline".into())
    );

    let (args, mono, _) = launch_recording(&options(false));
    assert!(
        !args
            .iter()
            .any(|argument| argument == "--unity-cli-code-reload")
    );
    assert_eq!(mono, inherited.clone().unwrap_or("<unset>".into()));
    // A finite run starts no Player that could be patched.
    let finite = LaunchOptions {
        watch: false,
        interactive: false,
        screenshot: "/tmp/last.png".into(),
        ..options(true)
    };
    let (args, mono, _) = launch_recording(&finite);
    assert!(
        !args
            .iter()
            .any(|argument| argument == "--unity-cli-code-reload")
    );
    // A screenshot needs a window to capture.
    assert!(!args.iter().any(|argument| argument == "-batchmode"));
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--unity-cli-screenshot" && pair[1] == "/tmp/last.png")
    );
    // The development Player's console of errors stays closed unless asked for.
    assert!(
        !args
            .iter()
            .any(|argument| argument == "--unity-cli-developer-console")
    );
    let (args, _, _) = launch_recording(&LaunchOptions {
        developer_console: true,
        ..finite.clone()
    });
    assert!(
        args.iter()
            .any(|argument| argument == "--unity-cli-developer-console")
    );
    let (args, _, _) = launch_recording(&LaunchOptions {
        screenshot: String::new(),
        ..finite
    });
    assert!(args.iter().any(|argument| argument == "-batchmode"));
    assert_eq!(mono, inherited.unwrap_or("<unset>".into()));
}

/// Runs in a child process of the test below, which sets the variable.
#[test]
fn helper_player_reports_mono_arguments() {
    let Ok(expected) = std::env::var("UME_TEST_EXPECT_MONO_ARGUMENTS") else {
        return;
    };
    let opts = LaunchOptions {
        frames: 120,
        width: 960,
        height: 540,
        fps: 60,
        watch: true,
        interactive: true,
        hot_reload: true,
        ..Default::default()
    };
    assert_eq!(launch_recording(&opts).1, expected);
}

#[test]
fn player_keeps_the_mono_arguments_of_the_caller() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(crate::sysutil::testing::helper_arguments(
            "runner::tests::helper_player_reports_mono_arguments",
        ))
        .env("MONO_ARGUMENTS", "--debug -O=-inline")
        .env("UME_TEST_EXPECT_MONO_ARGUMENTS", "--debug -O=-inline")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && text.contains("1 passed"),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sdk_tools_do_not_inherit_standard_input() {
    let output = crate::compiler::compiler_command(
        &Context::background(),
        "",
        "readlink",
        &["/proc/self/fd/0"],
    )
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&output).trim(), "/dev/null");
}

#[test]
fn unique_symbols_of_a_plugin_become_weak() {
    // An ELF64 image with one .dynsym section of two symbols: a unique
    // object and a global function.
    let mut image = vec![0u8; 176];
    image[..6].copy_from_slice(b"\x7fELF\x02\x01");
    image[0x28] = 64; // e_shoff
    image[0x3a] = 64; // e_shentsize
    image[0x3c] = 1; // e_shnum
    image[64 + 4] = 11; // SHT_DYNSYM
    image[64 + 0x18] = 128; // sh_offset
    image[64 + 0x20] = 48; // sh_size
    image[64 + 0x38] = 24; // sh_entsize
    image[128 + 4] = (10 << 4) | 1;
    image[152 + 4] = (1 << 4) | 2;
    assert_eq!(super::player::weaken_unique_symbols(&mut image), 1);
    assert_eq!(image[128 + 4], (2 << 4) | 1);
    assert_eq!(image[152 + 4], (1 << 4) | 2);
    assert_eq!(super::player::weaken_unique_symbols(&mut image), 0);
    assert_eq!(
        super::player::weaken_unique_symbols(&mut b"not an ELF".to_vec()),
        0
    );
}

#[test]
fn assets_play_mode_code_asked_for_are_remembered_while_their_sources_exist() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().to_str().unwrap();
    fs::create_dir_all(root.path().join("Assets/Stub")).unwrap();
    fs::create_dir_all(root.path().join("Library/UnityMicroEditor")).unwrap();
    fs::write(root.path().join("Assets/Stub/Banner.prefab"), "").unwrap();
    fs::write(root.path().join("Assets/Stub/Gone.prefab"), "").unwrap();
    for path in [
        "Assets/Stub/Gone.prefab",
        "Assets/Stub/Banner.prefab",
        "Assets/Stub/Banner.prefab",
    ] {
        assets::remember(project, path).unwrap();
    }
    assert_eq!(
        assets::remembered(project),
        ["Assets/Stub/Banner.prefab", "Assets/Stub/Gone.prefab"]
    );
    // A source that is gone is not written with the next scene; a package
    // the project has no compilation record of is not found.
    fs::remove_file(root.path().join("Assets/Stub/Gone.prefab")).unwrap();
    assets::remember(
        project,
        "Packages/com.applovin.mediation.ads/Prefabs/BannerTop.prefab",
    )
    .unwrap();
    assert_eq!(assets::remembered(project), ["Assets/Stub/Banner.prefab"]);
}
