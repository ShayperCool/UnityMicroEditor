use super::*;
use crate::{
    editorbridge::{self, Client},
    util::{agent_immutable_json, agent_read_json, file_hash},
};
use host::{prepare_agent_host, snapshot_host_artifacts};
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
    seed_native_library(&ctx(), &project, &stage).unwrap();
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
    seed_native_library(&ctx(), &project, &stage).expect("exact seed replay failed");
    let (_other_guard, other) = review_project();
    assert!(
        seed_native_library(&ctx(), &other, &stage).is_err(),
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
    seed_native_library(&ctx(), &project, &stage).unwrap();
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
    seed_native_library(&ctx(), &project, &stage).expect("same-policy seed replay failed");
}

struct Cached {
    project: String,
    id: &'static str,
    directory: String,
    player: String,
    snapshot: SourceSnapshot,
    _guard: TempDir,
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
