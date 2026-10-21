use super::*;
use crate::{
    editorbridge::{self, Client},
    util::{agent_immutable_json, agent_read_json, file_hash},
};
use host::{host_content, prepare_agent_host, retarget_stage_graph, snapshot_host_artifacts};
use native::seed_native_library;
use serde_json::json;
use staging::phase_id;
use std::time::Duration;

fn editor_service(project: &str, editor: &str) -> Service {
    Service::new(Options {
        project: project.into(),
        editor: editor.into(),
        lifetime: Some(ctx()),
        ..Default::default()
    })
}

#[test]
fn host_stage_contains_addressables_generated_source_changes() {
    let (_guard, project) = review_project();
    let addressables = join(&[&project, "Assets/AddressableAssetsData"]);
    for name in ["link.xml", "link.xml.meta"] {
        write(
            &join(&[&addressables, name]),
            format!("original Addressables {name}"),
        );
    }
    let before = snapshot_source(&ctx(), &project).unwrap();
    let (stage, snapshot) = clone_staging(
        &ctx(),
        &project,
        &join(&[
            &project,
            "Library/UnityMicroEditor/preparations/host-test/host-stage",
        ]),
    )
    .unwrap();
    for name in ["link.xml", "link.xml.meta"] {
        fs::remove_file(join(&[&stage, "Assets/AddressableAssetsData", name])).unwrap();
    }
    let generated = join(&[&stage, "Assets/AddressableAssetsData/Linux"]);
    for name in [
        "addressables_content_state.bin",
        "addressables_content_state.bin.meta",
    ] {
        write(&join(&[&generated, name]), "generated build output");
    }
    verify_snapshot(&ctx(), &snapshot).expect("stage build mutations reached authored roots");
    let after = snapshot_source(&ctx(), &project).unwrap();
    assert_eq!(
        before.files, after.files,
        "host-stage preparation changed Assets, Packages or ProjectSettings"
    );
    assert!(
        absent(&join(&[&addressables, "Linux"])),
        "generated Linux source escaped its owned clone"
    );
}

#[test]
fn snapshot_records_projects_above_four_thousand_sources() {
    let (_guard, project) = review_project();
    let folder = join(&[&project, "Assets/Many"]);
    for index in 0..5000 {
        write(&join(&[&folder, &format!("{index}.txt")]), "text");
        write(
            &join(&[&folder, &format!("{index}.txt.meta")]),
            format!("fileFormatVersion: 2\nguid: {:032x}\n", index + 1),
        );
    }
    let snapshot = snapshot_source(&ctx(), &project).unwrap();
    assert!(snapshot.files.len() > 10000);
}

#[test]
fn host_stage_normalizes_relative_packages_without_changing_source_manifest() {
    // The registered package is a sibling of the project.
    let (_guard, parent) = temp_dir();
    let project = join(&[&parent, "project"]);
    for (path, content) in [
        (
            "ProjectSettings/ProjectVersion.txt",
            format!("m_EditorVersion: {UNITY_VERSION}\n"),
        ),
        ("Assets/Sample.cs", "public class Sample {}\n".to_string()),
        (
            "Assets/Sample.cs.meta",
            "fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n".to_string(),
        ),
    ] {
        write(&join(&[&project, path]), content);
    }
    let package_root = join(&[&parent, "registered-package"]);
    write(
        &join(&[&package_root, "package.json"]),
        r#"{"name":"com.example.local"}"#,
    );
    let manifest = join(&[&project, "Packages/manifest.json"]);
    let original = "{\n  \"dependencies\": {\"com.example.local\": \"file:../../registered-package\"},\n  \"customValue\": 7\n}\n";
    write(&manifest, original);
    let (stage, snapshot) = clone_staging(
        &ctx(),
        &project,
        &join(&[
            &project,
            "Library/UnityMicroEditor/preparations/host-packages/host-stage",
        ]),
    )
    .unwrap();
    let staged: Value =
        agent_read_json(&join(&[&stage, "Packages/manifest.json"]), 1 << 20).unwrap();
    assert_eq!(
        staged["dependencies"]["com.example.local"],
        format!("file:{package_root}").as_str()
    );
    assert_eq!(staged["customValue"], 7);
    assert_eq!(
        fs::read_to_string(&manifest).unwrap(),
        original,
        "normalization rewrote authored package bytes"
    );
    verify_snapshot(&ctx(), &snapshot).unwrap();
}

#[test]
fn warm_library_seed_excludes_recursive_tool_stages_and_checks_provenance() {
    let (_guard, project) = review_project();
    // The source Editor is prepared with the runtime package before its stopped
    // Library can be seeded. The clone must not need its own package install.
    install_agent_package(&project).unwrap();
    let directory = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/warm-host/host-stage",
    ]);
    let (stage, snapshot) = clone_staging(&ctx(), &project, &directory).unwrap();
    write(
        &join(&[&project, "Library/ArtifactDB"]),
        "completed owned importer database",
    );
    seed_native_library(&ctx(), &project, &stage, true).unwrap();
    assert_eq!(
        fs::read_to_string(join(&[&stage, "Library/ArtifactDB"])).unwrap(),
        "completed owned importer database",
        "completed Library seed absent"
    );
    assert!(
        absent(&join(&[&stage, "Library/UnityMicroEditor"])),
        "host/native tool stages or journals recursively entered Library seed"
    );
    let marker = join(&[&stage, ".micro-editor-library-seed.json"]);
    let evidence: Value = agent_read_json(&marker, 65536).unwrap();
    assert_eq!(evidence["sourceProject"], project.as_str());
    assert_eq!(evidence["stageProject"], stage.as_str());
    assert_eq!(evidence["sourceEditorOwnedAndStopped"], true);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        format!(
            "{{\"copyMode\":\"reflink-auto\",\"excluded\":\"Library/UnityMicroEditor\",\"excludedTransientCaches\":[\"PackageManager\",\"ScriptAssemblies\",\"Bee\",\"*.pid\",\"*.lock\",\"*-lock\"],\"schemaVersion\":1,\"seedPolicy\":2,\"sourceEditorOwnedAndStopped\":true,\"sourceProject\":\"{project}\",\"stageProject\":\"{stage}\"}}\n"
        )
    );
    seed_native_library(&ctx(), &project, &stage, true).expect("exact seed replay failed");
    let (_other_guard, other) = review_project();
    assert!(
        seed_native_library(&ctx(), &other, &stage, true).is_err(),
        "completed seed reused for another authored source"
    );
    verify_snapshot(&ctx(), &snapshot).expect("warm seed changed authored source");
}

#[test]
fn library_seed_retains_imported_assets_without_source_process_state() {
    let (_guard, project) = review_project();
    install_agent_package(&project).unwrap();
    let (stage, snapshot) = clone_staging(
        &ctx(),
        &project,
        &join(&[
            &project,
            "Library/UnityMicroEditor/preparations/seed-policy/host-stage",
        ]),
    )
    .unwrap();
    let retained = [
        "ArtifactDB",
        "SourceAssetDB",
        "ShaderCache/compiled",
        "PackageCache/com.example@pinned/Runtime.cs",
    ];
    let excluded = [
        "PackageManager/ProjectCache",
        "ScriptAssemblies/Game.dll",
        "Bee/Backend.log",
        "burst.pid",
        "ilpp.pid",
        "ArtifactDB-lock",
        "SourceAssetDB-lock",
        "resolver.lock",
    ];
    for path in retained.iter().chain(&excluded) {
        write(
            &join(&[&project, "Library", path]),
            format!("{project}:{path}"),
        );
    }
    seed_native_library(&ctx(), &project, &stage, true).unwrap();
    for path in retained {
        assert_eq!(
            fs::read_to_string(join(&[&stage, "Library", path])).ok(),
            Some(format!("{project}:{path}")),
            "imported asset cache {path} was not retained"
        );
    }
    for path in excluded {
        assert!(
            absent(&join(&[&stage, "Library", path])),
            "copied source process/compiler state {path}"
        );
        assert!(
            !absent(&join(&[&project, "Library", path])),
            "changed original source cache {path}"
        );
    }
    verify_snapshot(&ctx(), &snapshot).expect("seed changed authored source");
    seed_native_library(&ctx(), &project, &stage, true).expect("same-policy seed replay failed");
}

struct Cached {
    project: String,
    id: &'static str,
    directory: String,
    player: String,
    snapshot: SourceSnapshot,
    _guard: TempDir,
}
#[test]
fn shared_stage_follows_the_source_and_keeps_its_caches() {
    let (_guard, project) = review_project();
    let modified = |path: &str| fs::metadata(path).unwrap().modified().unwrap();
    for (name, contents) in [("Kept", "kept"), ("Gone", "gone"), ("Edited", "before")] {
        write(
            &join(&[&project, "Assets", &format!("{name}.txt")]),
            contents,
        );
    }
    let old = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::open(join(&[&project, "Assets/Kept.txt"]))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let (stage, _) = sync_staging(&ctx(), &project).unwrap();
    assert_eq!(stage, staging::shared_stage(&project));
    assert_eq!(modified(&join(&[&stage, "Assets/Kept.txt"])), old);
    // What a stage Editor leaves behind: caches, a build and authored files.
    let cache = join(&[&stage, "Library/Bee/cache"]);
    let host = join(&[
        &stage,
        "Builds/UnityMicroEditor/earlier-host/Project.x86_64",
    ]);
    write(&cache, "warm");
    write(&host, "earlier host");
    write(&join(&[&stage, "Assets/Generated/link.xml"]), "generated");
    write(&join(&[&stage, "Assets/Kept.txt.meta"]), "generated");

    fs::remove_file(join(&[&project, "Assets/Gone.txt"])).unwrap();
    write(&join(&[&project, "Assets/Edited.txt"]), "after");
    write(&join(&[&project, "Assets/New/Added.txt"]), "added");
    let (again, snapshot) = sync_staging(&ctx(), &project).unwrap();
    assert_eq!(again, stage);
    assert_eq!(
        snapshot.files,
        snapshot_source(&ctx(), &project).unwrap().files
    );
    let staged = |logical: &str| fs::read_to_string(join(&[&stage, logical])).unwrap();
    assert_eq!(staged("Assets/Edited.txt"), "after");
    assert_eq!(staged("Assets/New/Added.txt"), "added");
    assert_eq!(staged("Library/Bee/cache"), "warm");
    assert_eq!(
        staged("Builds/UnityMicroEditor/earlier-host/Project.x86_64"),
        "earlier host"
    );
    for removed in [
        "Assets/Gone.txt",
        "Assets/Generated",
        "Assets/Kept.txt.meta",
    ] {
        assert!(absent(&join(&[&stage, removed])), "{removed} survived");
    }
    assert_eq!(
        modified(&join(&[&stage, "Assets/Kept.txt"])),
        old,
        "an unchanged file was rewritten"
    );
    for (logical, hash) in snapshot.files.iter() {
        if logical != "Packages/manifest.json" {
            assert_eq!(
                &file_hash(&join(&[&stage, logical])).unwrap(),
                hash,
                "{logical}"
            );
        }
    }

    // A stage copied along with another project's Library is not reused.
    let marker = join(&[&stage, "ProjectSettings/UnityMicroEditorStaging.json"]);
    fs::remove_file(&marker).unwrap();
    write(
        &marker,
        r#"{"schemaVersion":1,"sourceProject":"/another/project"}"#,
    );
    sync_staging(&ctx(), &project).unwrap();
    assert!(absent(&cache), "a foreign stage kept its Library");
    assert_eq!(staged("Assets/Edited.txt"), "after");
}

const REVIEW_EDITOR: &str = "/missing-review-editor";
fn cached_fixture() -> Cached {
    let (guard, project) = review_project();
    let id = "host-review-cached";
    let directory = join(&[
        &project,
        "Library/UnityMicroEditor/preparations",
        id,
        "host-stage",
    ]);
    let (stage, snapshot) = clone_staging(&ctx(), &project, &directory).unwrap();
    let snapshot_path = join(&[&directory, "source-snapshot.json"]);
    agent_immutable_json(&snapshot_path, &snapshot).unwrap();
    let phase = phase_id(id, "host");
    let output = join(&[&stage, "Builds/UnityMicroEditor", &phase]);
    let player = join(&[&output, "Project.x86_64"]);
    for (path, contents) in [
        (player.clone(), "native host executable"),
        (
            join(&[&output, "Project_Data/Managed/UnityMicroEditor.dll"]),
            "pinned public runtime bridge",
        ),
        (
            join(&[&output, "Project_Data/ScriptingAssemblies.json"]),
            r#"{"names":["UnityMicroEditor.dll","Assembly-CSharp.dll"]}"#,
        ),
    ] {
        write_mode(&path, contents, 0o700);
    }
    let artifacts = snapshot_host_artifacts(&ctx(), &project, &stage, &phase, &player).unwrap();
    let record = json!({
        "schemaVersion": 1, "project": project, "editor": REVIEW_EDITOR, "requestId": id,
        "stageProject": stage, "player": player, "sourceSnapshotSha256": file_hash(&snapshot_path).unwrap(),
        "artifacts": artifacts,
        "evidence": {"backend": "unity_editor", "sourceSnapshotVerified": true, "sourceProject": project, "stageProject": stage},
    });
    // The record is written in struct order; a decoded map is read identically.
    write(&join(&[&directory, "result.json"]), bytes(&record));
    Cached {
        project,
        id,
        directory,
        player,
        snapshot,
        _guard: guard,
    }
}

#[test]
fn cached_replay_verifies_artifacts_without_starting_editor() {
    let cached = cached_fixture();
    let service = editor_service(&cached.project, REVIEW_EDITOR);
    for replay in 0..2 {
        let (host, evidence) = prepare_agent_host(&ctx(), &service, cached.id)
            .unwrap_or_else(|error| panic!("cached host replay {replay} failed: {error}"));
        assert_eq!(host, cached.player);
        assert_eq!(
            evidence.get("sourceSnapshotVerified"),
            Some(&Any::Bool(true))
        );
    }
    assert!(
        service.client().is_none(),
        "cached host replay acquired a source Editor client"
    );
    verify_snapshot(&ctx(), &cached.snapshot).unwrap();
    service.close();
    let changed = editor_service(&cached.project, "/another-review-editor");
    assert_eq!(
        code(&prepare_agent_host(&ctx(), &changed, cached.id)),
        "request_id_conflict",
        "changed Editor reused the stable host request"
    );
    changed.close();
}

#[test]
fn cached_replay_rejects_changed_source_and_host_output() {
    for changed in [
        "asset-meta",
        "packages",
        "project-settings",
        "player-bytes",
        "extra-output",
        "missing-bridge",
        "output-symlink",
        "foreign-provenance",
        "snapshot-proof",
    ] {
        let cached = cached_fixture();
        let service = editor_service(&cached.project, REVIEW_EDITOR);
        let stage = join(&[&cached.directory, "project"]);
        let output = crate::sysutil::dir(&cached.player);
        match changed {
            "asset-meta" => write(
                &join(&[&cached.project, "Assets/Sample.cs.meta"]),
                "changed authored source",
            ),
            "packages" => write(
                &join(&[&cached.project, "Packages/manifest.json"]),
                "changed authored source",
            ),
            "project-settings" => write(
                &join(&[&cached.project, "ProjectSettings/ProjectVersion.txt"]),
                "changed authored source",
            ),
            "player-bytes" => write_mode(&cached.player, "different native executable", 0o700),
            "extra-output" => write(
                &join(&[&output, "unexpected.dll"]),
                "new unverified artifact",
            ),
            "missing-bridge" => fs::remove_file(join(&[
                &output,
                "Project_Data/Managed/UnityMicroEditor.dll",
            ]))
            .unwrap(),
            "output-symlink" => {
                std::os::unix::fs::symlink(&cached.project, join(&[&output, "linked"])).unwrap()
            }
            _ => {
                let path = join(&[&cached.directory, "result.json"]);
                let mut record: Value = agent_read_json(&path, 8 << 20).unwrap();
                if changed == "foreign-provenance" {
                    record["stageProject"] = json!(format!("{stage}-another"));
                } else {
                    record["sourceSnapshotSha256"] = json!("0".repeat(64));
                }
                write(&path, bytes(&record));
            }
        }
        assert!(
            prepare_agent_host(&ctx(), &service, cached.id).is_err(),
            "{changed}: unverified host became reusable"
        );
        assert!(
            service.client().is_none(),
            "{changed}: invalid cached host started a source Editor"
        );
        service.close();
    }
}

#[test]
fn build_failure_and_cancellation_preserve_all_authored_roots() {
    for cancelled in [false, true] {
        let (_guard, project) = review_project();
        let before = snapshot_source(&ctx(), &project).unwrap();
        let service = editor_service(&project, REVIEW_EDITOR);
        let request = ctx().with_cancel();
        if cancelled {
            request.cancel();
        }
        assert!(
            prepare_agent_host(&request, &service, "host-failed-review").is_err(),
            "cancelled={cancelled}: failed host preparation reported reusable output"
        );
        let after = snapshot_source(&ctx(), &project).unwrap();
        assert_eq!(
            before.files, after.files,
            "cancelled={cancelled}: failed/cancelled host changed authored source"
        );
        assert!(
            absent(&join(&[
                &project,
                "Library/UnityMicroEditor/preparations/host-failed-review/host-stage/result.json"
            ])),
            "cancelled={cancelled}: failed host published successful result"
        );
        service.close();
    }
}

/// Needs an explicit disposable target and a matching Editor.
#[test]
fn host_preparation_actual_editor() {
    let (Ok(project), Ok(editor)) = (
        std::env::var("UNITY_AGENT_HOST_STAGE_PROJECT"),
        std::env::var("UNITY_AGENT_HOST_STAGE_EDITOR"),
    ) else {
        return;
    };
    let id = std::env::var("UNITY_AGENT_HOST_STAGE_ID")
        .unwrap_or_else(|_| "actual-isolated-host01".into());
    let request = ctx().with_timeout(Duration::from_secs(720));
    let source = Client::new(
        &request,
        editorbridge::Options {
            project: project.clone(),
            editor: editor.clone(),
            startup_timeout: Duration::from_secs(600),
            ..Default::default()
        },
    )
    .unwrap();
    let connection = source.own_process_or_attach(&request).unwrap();
    let service = Service::new(Options {
        project,
        editor,
        lifetime: Some(request.clone()),
        ..Default::default()
    });
    service.set_client(Some(source));
    let (host, evidence) = prepare_agent_host(&request, &service, &id).unwrap();
    assert_eq!(evidence.get("backend"), Some(&Any::from("unity_editor")));
    assert_eq!(
        evidence.get("sourceSnapshotVerified"),
        Some(&Any::Bool(true))
    );
    assert_eq!(
        evidence.get("warmLibraryFromOwnedStoppedEditor"),
        Some(&Any::Bool(connection.owned))
    );
    assert!(
        !connection.owned || service.client().is_none(),
        "stopped owned source client was retained"
    );
    let (replayed, _) = prepare_agent_host(&request, &service, &id).unwrap();
    assert_eq!(replayed, host, "confirmed host replay changed output");
    service.close();
}

#[test]
fn stage_graph_is_retargeted_to_the_project_and_resolved_packages() {
    let (_guard, project) = review_project();
    let stage = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/p/host-stage/project",
    ]);
    let resolved = crate::packages::Snapshot {
        packages: vec![crate::packages::Package {
            name: "com.unity.burst".into(),
            logical_root: "Packages/com.unity.burst".into(),
            path: "/resolved/com.unity.burst".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let cache = format!("{stage}/Library/PackageCache");
    let graph = any(json!({
        "project": stage,
        "schemaVersion": 1,
        "sourceRoots": [
            {"path": "Packages/com.unity.burst", "source": format!("{cache}/com.unity.burst@1")},
            {"path": "Packages/com.unity.other", "source": format!("{cache}/com.unity.other@2")},
            {"path": "Packages/com.tool", "source": "/tool/Packages/com.tool"},
        ],
        "assemblies": [{
            "path": format!("{stage}/Library/ScriptAssemblies/Game.dll"),
            "sources": [format!("{stage}/Assets/Sample.cs"), format!("{cache}/com.unity.burst@1/Runtime/A.cs")],
            "compiledReferences": ["/editor/Data/NetStandard/ref.dll", format!("{cache}/com.unity.other@2/Plugin.dll")],
            "defines": ["/nowarn:0169"],
        }],
        "compilerInputs": {
            format!("{stage}/Packages/manifest.json"): "staged-manifest-hash",
            format!("{stage}/Packages/packages-lock.json"): "staged-lock-hash",
            format!("{stage}/Assets/Sample.cs"): "source-hash",
        },
    }));
    let retargeted = to_serde(&retarget_stage_graph(&graph, &stage, &project, &resolved).unwrap());
    assert_eq!(retargeted["project"], project.as_str());
    assert_eq!(
        retargeted["sourceRoots"],
        json!([
            {"path": "Packages/com.unity.burst", "source": "/resolved/com.unity.burst"},
            // No resolved package: the staged copy remains the existing source.
            {"path": "Packages/com.unity.other", "source": format!("{cache}/com.unity.other@2")},
            {"path": "Packages/com.tool", "source": "/tool/Packages/com.tool"},
        ])
    );
    let assembly = &retargeted["assemblies"][0];
    assert_eq!(
        assembly["sources"],
        json!([
            format!("{project}/Assets/Sample.cs"),
            "/resolved/com.unity.burst/Runtime/A.cs"
        ])
    );
    assert_eq!(
        assembly["compiledReferences"],
        json!([
            "/editor/Data/NetStandard/ref.dll",
            format!("{cache}/com.unity.other@2/Plugin.dll")
        ])
    );
    assert_eq!(
        assembly["path"],
        format!("{stage}/Library/ScriptAssemblies/Game.dll")
    );
    assert_eq!(assembly["defines"], json!(["/nowarn:0169"]));
    // Authored package inputs carry the target's bytes, not the staged manifest's.
    let manifest = join(&[&project, "Packages/manifest.json"]);
    let inputs = retargeted["compilerInputs"].as_object().unwrap();
    assert_eq!(inputs[&manifest], file_hash(&manifest).unwrap().as_str());
    assert!(
        !inputs.contains_key(&join(&[&project, "Packages/packages-lock.json"])),
        "an absent authored lock kept the staged lock hash"
    );
    assert_eq!(
        inputs[&join(&[&project, "Assets/Sample.cs"])],
        "source-hash"
    );
    assert!(
        retarget_stage_graph(&graph, "/another/stage", &project, &resolved).is_err(),
        "a graph of another stage was accepted"
    );
}

fn built_host(project: &str, request: &str, editor: &str) -> String {
    let directory = join(&[project, "Library/UnityMicroEditor/preparations", request]);
    let host = join(&[
        &directory,
        "host-stage/project/Builds/UnityMicroEditor/phase-host/Project.x86_64",
    ]);
    write_mode(&host, format!("player {request}"), 0o700);
    write(
        &join(&[
            &directory,
            "host-stage/project/Builds/UnityMicroEditor/phase-host/Project_Data/UnityMicroEditorSourceProfile.json",
        ]),
        "{}",
    );
    immutable(
        &join(&[&directory, "request.json"]),
        json!({"schemaVersion": 2, "editor": editor, "project": project}),
    );
    immutable(
        &join(&[&directory, "result.json"]),
        json!({"schemaVersion": 1, "requestId": request, "project": project, "scene": "Assets/Main.unity",
            "baseline": "", "hostPlayer": host, "nativeCatalog": format!("{directory}/native-catalog.json"),
            "referenceGraph": format!("{directory}/graph.json"), "backends": {"preparation": "native_engine_bootstrap"}}),
    );
    host
}

#[test]
fn new_request_reuses_the_newest_existing_host_unless_a_rebuild_is_asked() {
    let (_guard, project) = review_project();
    let service = editor_service(&project, "/editor/Unity");
    service.select("", "Assets/Main.unity");
    let seed = |request: &str, args: Value| {
        service
            .source_preparation_seed(
                &any_map(args),
                &join(&[&project, "Library/UnityMicroEditor/preparations", request]),
            )
            .unwrap()
    };
    assert!(seed("no-host-yet", json!({})).host_player.is_empty());

    built_host(&project, "other-editor", "/another/Unity");
    let older = built_host(&project, "older", "/editor/Unity");
    std::thread::sleep(Duration::from_millis(20));
    let newest = built_host(&project, "newest", "/editor/Unity");
    let found = seed("second", json!({}));
    assert_eq!(
        found.host_player, newest,
        "newest valid host was not reused"
    );
    assert_eq!(found.reused_preparation, "newest");
    assert!(
        found
            .native_catalog
            .ends_with("/newest/native-catalog.json")
    );
    assert!(found.reference_graph.ends_with("/newest/graph.json"));
    // A retry keeps the frozen choice even after another host appears.
    built_host(&project, "later", "/editor/Unity");
    assert_eq!(seed("second", json!({})).host_player, newest);
    // Another scene reuses the host but not that scene's native catalog.
    let other_scene = seed("other-scene", json!({"scene": "Assets/Other.unity"}));
    assert!(
        other_scene.host_player.ends_with(
            "/later/host-stage/project/Builds/UnityMicroEditor/phase-host/Project.x86_64"
        )
    );
    assert!(other_scene.native_catalog.is_empty());

    assert!(
        seed("rebuild", json!({"rebuildHost": true}))
            .host_player
            .is_empty(),
        "rebuildHost still reused a host"
    );
    // A host whose Player is gone is skipped.
    std::fs::remove_file(join(&[
        &project,
        "Library/UnityMicroEditor/preparations/later/host-stage/project/Builds/UnityMicroEditor/phase-host/Project.x86_64",
    ]))
    .unwrap();
    assert_eq!(seed("after-removal", json!({})).host_player, newest);
    let _ = older;
}

#[test]
fn broken_metadata_stops_preparation_before_any_work() {
    let (_guard, project) = review_project();
    let service = editor_service(&project, "/editor/Unity");
    service.check_authored_identity(true).unwrap();
    // A script without metadata only matters when an Editor stage would run.
    write(&join(&[&project, "Assets/New.cs"]), "public class New {}\n");
    service.check_authored_identity(false).unwrap();
    assert_eq!(
        code(&service.check_authored_identity(true)),
        "invalid_identity"
    );

    write(
        &join(&[&project, "Assets/Sample.cs.meta"]),
        "fileFormatVersion: 2\n",
    );
    let broken = service.check_authored_identity(false);
    assert_eq!(code(&broken), "invalid_identity");
    let detail = typed(broken.as_ref().unwrap_err()).unwrap();
    assert!(
        detail.message.contains("Assets/Sample.cs.meta")
            && detail.details.as_ref().unwrap()["issueCount"] == 1,
        "the failure does not name the broken metadata: {detail:?}"
    );
    // The same check is the first step of a preparation.
    service.select("", "Assets/Main.unity");
    let outcome = service.prepare_source(&ctx(), &any_map(json!({"requestId": "broken-meta"})));
    assert_eq!(code(&outcome), "invalid_identity");
    assert!(
        absent(&join(&[&project, "Library/UnityMicroEditor/packages"])),
        "packages were resolved before the metadata check"
    );
}

#[test]
fn host_content_names_the_staged_addressables_output() {
    let (_guard, project) = review_project();
    let host = built_host(&project, "content", "/editor/Unity");
    assert!(host_content(&host).is_empty());
    let stage = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/content/host-stage/project",
    ]);
    write(
        &join(&[&stage, "ProjectSettings/UnityMicroEditorStaging.json"]),
        "{}",
    );
    write(
        &join(&[&stage, "ServerData/StandaloneLinux64/catalog.json"]),
        "{}",
    );
    write(
        &join(&[
            &crate::compiler::player_data(&host),
            "StreamingAssets/aa/settings.json",
        ]),
        "{}",
    );
    let content = host_content(&host);
    assert_eq!(
        content["addressablesRemoteBuild"],
        join(&[&stage, "ServerData/StandaloneLinux64"])
    );
    assert!(content["addressablesStreamingAssets"].ends_with("Project_Data/StreamingAssets/aa"));
}

#[test]
fn stage_takes_the_selected_groups_scripting_defines() {
    let settings = "PlayerSettings:\n  scriptingDefineSymbols:\n    Android: A;B\n    Standalone: C\n    iPhone: D\n  additionalCompilerArguments: {}\n";
    assert_eq!(
        crate::agent::staging::with_standalone_defines(settings, "Android").unwrap(),
        "PlayerSettings:\n  scriptingDefineSymbols:\n    Android: A;B\n    Standalone: A;B;PLATFORM_ANDROID;UNITY_ANDROID;UNITY_ANDROID_API;UNITY_MICRO_EDITOR_HOST_BUILD\n    iPhone: D\n  additionalCompilerArguments: {}\n"
    );
    // A project without Standalone symbols gets an entry in key order, and a
    // folded value is copied whole.
    let folded = "  scriptingDefineSymbols:\n    Android: A;\n      B\n    iPhone: D\n  x: 1\n";
    assert_eq!(
        crate::agent::staging::with_standalone_defines(folded, "Android").unwrap(),
        "  scriptingDefineSymbols:\n    Android: A;\n      B\n    Standalone: A;\n      B;PLATFORM_ANDROID;UNITY_ANDROID;UNITY_ANDROID_API;UNITY_MICRO_EDITOR_HOST_BUILD\n    iPhone: D\n  x: 1\n"
    );
    assert!(crate::agent::staging::with_standalone_defines(settings, "WebGL").is_err());
}
