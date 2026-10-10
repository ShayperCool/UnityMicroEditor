use super::*;
use crate::{
    agentcontrol::{self, Controller},
    authoring::{PublicationInput, PublicationOutput, StagedPublicationRequest, Store},
    compiler::{CompileManifest, NilMap},
    sysutil::testing::write_executable,
    util::{agent_immutable_json, agent_read_json},
};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

fn controller(project: &str, runner: &str, lifetime: &Context) -> Controller {
    Controller::new(
        lifetime,
        agentcontrol::Options {
            project: project.into(),
            runner: runner.into(),
            ..Default::default()
        },
    )
    .unwrap()
}
fn waiting_runner() -> (TempDir, String) {
    let (guard, directory) = temp_dir();
    let runner = join(&[&directory, "waiting-runner"]);
    write_executable(&runner, "#!/bin/sh\nexec sleep 30\n");
    (guard, runner)
}

#[test]
fn immutable_receipt_rejects_changed_arguments_and_symlink() {
    let (_guard, directory) = temp_dir();
    let path = join(&[&directory, "requests/id.json"]);
    let original = any(json!({"requestId": "original", "arguments": {"amount": 1}}));
    agent_immutable_json(&path, &original).unwrap();
    let before = fs::read(&path).unwrap();
    agent_immutable_json(&path, &original).expect("exact request replay failed");
    let changed = any(json!({"requestId": "original", "arguments": {"amount": 2}}));
    assert_eq!(
        code(&agent_immutable_json(&path, &changed)),
        "request_id_conflict"
    );
    assert_eq!(
        before,
        fs::read(&path).unwrap(),
        "conflict overwrote original receipt"
    );
    let (_outside_guard, outside_directory) = temp_dir();
    let outside = join(&[&outside_directory, "outside.json"]);
    std::os::unix::fs::symlink(&outside, join(&[&directory, "alias"])).unwrap();
    assert!(
        agent_immutable_json(&join(&[&directory, "alias"]), &original).is_err(),
        "symlink destination accepted"
    );
    assert!(absent(&outside), "rejected receipt wrote outside");
}

#[test]
fn package_install_preserves_other_entries_and_detects_conflict() {
    let (_guard, project) = review_project();
    install_agent_package(&project).unwrap();
    let path = join(&[&project, "Packages/manifest.json"]);
    let mut manifest: Value = agent_read_json(&path, 1 << 20).unwrap();
    assert_eq!(manifest["dependencies"]["com.example.test"], "1.2.3");
    assert_eq!(manifest["projectMarker"]["value"], 7);
    assert_eq!(manifest["scopedRegistries"].as_array().unwrap().len(), 1);
    let installed = fs::read(&path).unwrap();
    install_agent_package(&project).unwrap();
    assert_eq!(
        installed,
        fs::read(&path).unwrap(),
        "idempotent install rewrote manifest"
    );
    let backups = crate::compiler::glob(&join(&[
        &project,
        "Library/UnityMicroEditor/package-install/*.json",
    ]))
    .unwrap();
    assert_eq!(backups.len(), 1, "durable package backup absent");
    manifest["dependencies"]["com.unitymicroeditor.runtime"] = json!("file:/different/package");
    let changed = bytes(&manifest);
    fs::write(&path, &changed).unwrap();
    assert_eq!(code(&install_agent_package(&project)), "package_conflict");
    assert_eq!(
        fs::read(&path).unwrap(),
        changed,
        "conflict changed target manifest"
    );
}

#[test]
fn editor_validation_precedes_package_installation() {
    for invalid in ["profile", "executable"] {
        let (_guard, project) = review_project();
        let manifest = join(&[&project, "Packages/manifest.json"]);
        let before = fs::read(&manifest).unwrap();
        let mut editor = "/bin/true".to_string();
        if invalid == "profile" {
            write(
                &join(&[&project, "ProjectSettings/ProjectVersion.txt"]),
                "m_EditorVersion: 2022.3.0f1\n",
            );
        } else {
            editor = join(&[&project, "missing-editor"]);
        }
        let service = Service::new(Options {
            project: project.clone(),
            editor,
            lifetime: Some(ctx()),
            ..Default::default()
        });
        assert!(
            service.editor_client(&ctx()).is_err(),
            "{invalid}: invalid Editor target accepted"
        );
        service.close();
        assert_eq!(
            before,
            fs::read(&manifest).unwrap(),
            "{invalid}: rejected target changed authored package manifest"
        );
        assert!(
            absent(&join(&[
                &project,
                "Library/UnityMicroEditor/package-install"
            ])),
            "{invalid}: rejected target installed the runtime package"
        );
    }
}

#[test]
fn invalid_editor_profile_does_not_install_authored_package() {
    let (_guard, project) = review_project();
    let manifest = join(&[&project, "Packages/manifest.json"]);
    let before = fs::read(&manifest).unwrap();
    write(
        &join(&[&project, "ProjectSettings/ProjectVersion.txt"]),
        "m_EditorVersion: 6000.5.0f1\n",
    );
    let service = Service::new(Options {
        project: project.clone(),
        editor: "/bin/true".into(),
        lifetime: Some(ctx()),
        ..Default::default()
    });
    let outcome = service.handle_sync(
        &ctx(),
        "component_schema",
        br#"{"componentType":"BoxCollider","requestId":"invalid-profile-test"}"#,
    );
    service.close();
    assert!(outcome.is_err(), "unsupported project profile accepted");
    assert_eq!(
        before,
        fs::read(&manifest).unwrap(),
        "rejected Editor profile still modified authored package manifest"
    );
}

#[test]
fn snapshot_detects_metadata_changes_additions_and_symlinks() {
    for change in ["metadata", "addition", "symlink"] {
        let (_guard, project) = review_project();
        let snapshot = snapshot_source(&ctx(), &project).unwrap();
        let (_other_guard, other) = temp_dir();
        match change {
            "metadata" => write(
                &join(&[&project, "Assets/Sample.cs.meta"]),
                "guid: 33333333333333333333333333333333\n",
            ),
            "addition" => write(&join(&[&project, "Assets/new.bin"]), [1u8, 2, 3]),
            _ => std::os::unix::fs::symlink(&other, join(&[&project, "Assets/escaped"])).unwrap(),
        }
        assert!(
            verify_snapshot(&ctx(), &snapshot).is_err(),
            "{change}: changed snapshot accepted"
        );
    }
}

#[test]
fn cached_clone_refuses_substituted_project_symlink() {
    let (_guard, project) = review_project();
    let directory = join(&[&project, "Library/UnityMicroEditor/stages/symlink"]);
    let snapshot = snapshot_source(&ctx(), &project).unwrap();
    agent_immutable_json(&join(&[&directory, "clone-complete.json"]), &snapshot).unwrap();
    let (_other_guard, other) = temp_dir();
    std::os::unix::fs::symlink(&other, join(&[&directory, "project"])).unwrap();
    assert!(
        clone_staging(&ctx(), &project, &directory).is_err(),
        "cached project symlink reached launch boundary"
    );
}

#[test]
fn clone_rebinds_its_owned_marker_without_changing_source() {
    let (_guard, project) = review_project();
    let marker = join(&[&project, "ProjectSettings/UnityMicroEditorStaging.json"]);
    let (_other_guard, other) = temp_dir();
    immutable(&marker, json!({"schemaVersion": 1, "sourceProject": other}));
    let before = fs::read(&marker).unwrap();
    let (stage, _) = clone_staging(
        &ctx(),
        &project,
        &join(&[&project, "Library/UnityMicroEditor/clone-marker-test"]),
    )
    .unwrap();
    let staged: Value = agent_read_json(
        &join(&[&stage, "ProjectSettings/UnityMicroEditorStaging.json"]),
        1 << 20,
    )
    .unwrap();
    assert_eq!(
        staged,
        json!({"schemaVersion": 1, "sourceProject": project})
    );
    assert_eq!(
        before,
        fs::read(&marker).unwrap(),
        "shadow planning changed source staging identity"
    );
}

#[test]
fn canceled_queued_action_does_not_execute() {
    let (_guard, project) = review_project();
    let service = Service::new(Options {
        project,
        ..Default::default()
    });
    let held = service.0.mu.lock().unwrap();
    let request = ctx().with_timeout(Duration::from_millis(50));
    let executed = std::sync::Arc::new(AtomicBool::new(false));
    let (done, finished) = mpsc::channel();
    {
        let (service, executed) = (service.clone(), executed.clone());
        thread::spawn(move || {
            let outcome = service.locked(&request, || executed.store(true, Ordering::SeqCst));
            done.send(outcome.map_err(|error| error_text(&error)))
                .unwrap();
        });
    }
    let outcome = finished
        .recv_timeout(Duration::from_millis(500))
        .expect("queued action ignored caller cancellation");
    assert_eq!(outcome.unwrap_err(), "deadline exceeded");
    assert!(
        !executed.load(Ordering::SeqCst),
        "canceled action executed work"
    );
    drop(held);
}

#[test]
fn staged_publication_replays_after_coordinator_restart() {
    let (_guard, project) = review_project();
    let store = Store::new(Path::new(&project)).unwrap();
    let first = store.read("Assets/Sample.cs").unwrap();
    let second = store.read("Assets/Other.cs").unwrap();
    let id = "staged-crash-test";
    let mut args = json!({"operationId": id, "action": "revert", "path": first.path, "expectedRevision": first.revision});
    let directory = join(&[
        &project,
        "Library/UnityMicroEditor/authoring-stages",
        &crate::sha256(id.as_bytes()),
    ]);
    immutable(
        &join(&[&directory, "request.json"]),
        json!({"schemaVersion": 1, "project": project, "arguments": args}),
    );
    let plan = StagedPublicationRequest {
        operation_id: id.into(),
        inputs: vec![
            PublicationInput {
                path: first.path.clone(),
                expected_revision: first.revision.clone(),
            },
            PublicationInput {
                path: second.path.clone(),
                expected_revision: second.revision.clone(),
            },
        ],
        outputs: vec![
            PublicationOutput {
                path: first.path.clone(),
                content: first.content.replacen("= 1", "= 3", 1),
                metadata: None,
            },
            PublicationOutput {
                path: second.path.clone(),
                content: second.content.replacen("= 2", "= 4", 1),
                metadata: None,
            },
        ],
    };
    agent_immutable_json(&join(&[&directory, "publication.json"]), &plan).unwrap();
    let raw = bytes(&args);
    let service = plain_service(&project);
    let receipt = service.handle(&ctx(), "prefab_edit", &raw).unwrap();
    assert!(is_map(&receipt));
    let publication = &decoded(&receipt)["publication"];
    assert_eq!(publication["changed"], true);
    assert!(publication.get("replayed").is_none());
    assert_eq!(publication["assets"].as_array().unwrap().len(), 2);
    let restarted = plain_service(&project);
    let receipt = restarted.handle(&ctx(), "prefab_edit", &raw).unwrap();
    assert_eq!(
        decoded(&receipt)["publication"]["replayed"],
        true,
        "restart did not recover published receipt"
    );
    for source in [&first, &second] {
        let updated = store.read(&source.path).unwrap();
        assert_eq!(
            updated.metadata, source.metadata,
            "staged publication changed metadata"
        );
    }
    args["action"] = json!("unpack");
    assert_eq!(
        code(&restarted.handle(&ctx(), "prefab_edit", &bytes(&args))),
        "request_id_conflict",
        "changed staged arguments reused receipt"
    );
}

#[test]
fn staged_publication_preserves_exact_request_numbers_across_restart() {
    let (_guard, project) = review_project();
    let store = Store::new(Path::new(&project)).unwrap();
    let source = store.read("Assets/Sample.cs").unwrap();
    let id = "staged-exact-int64";
    let arguments = |value: &str| {
        format!(
            r#"{{"action":"set_override","expectedRevision":{},"fields":{{"enabled":true,"ids":[9007199254740993,-9223372036854775808]}},"operationId":"{id}","path":"{}","value":{value}}}"#,
            serde_json::to_string(&source.revision).unwrap(),
            source.path
        )
    };
    let raw = arguments("9223372036854775807");
    let directory = join(&[
        &project,
        "Library/UnityMicroEditor/authoring-stages",
        &crate::sha256(id.as_bytes()),
    ]);
    let args = value::parse_json(raw.as_bytes(), true).unwrap().0;
    agent_immutable_json(
        &join(&[&directory, "request.json"]),
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("project", project.clone().into()),
            ("arguments", args),
        ])),
    )
    .unwrap();
    let content = "public class Sample { public long value = 9223372036854775807; }\n";
    let plan = StagedPublicationRequest {
        operation_id: id.into(),
        inputs: vec![PublicationInput {
            path: source.path.clone(),
            expected_revision: source.revision.clone(),
        }],
        outputs: vec![PublicationOutput {
            path: source.path.clone(),
            content: content.into(),
            metadata: None,
        }],
    };
    agent_immutable_json(&join(&[&directory, "publication.json"]), &plan).unwrap();
    for iteration in 0..2 {
        let service = plain_service(&project);
        let result = service.handle_sync(&ctx(), "prefab_edit", raw.as_bytes());
        service.close();
        let result = result
            .unwrap_or_else(|error| panic!("exact staged request iteration {iteration}: {error}"));
        let replayed = decoded(&result)["publication"]["replayed"] == true;
        assert_eq!(replayed, iteration == 1, "durable replay was not retained");
    }
    let updated = store.read(&source.path).unwrap();
    assert_eq!(updated.content, content);
    assert_eq!(updated.metadata, source.metadata);
    let service = plain_service(&project);
    assert_eq!(
        code(&service.handle_sync(
            &ctx(),
            "prefab_edit",
            arguments("9223372036854775806").as_bytes()
        )),
        "request_id_conflict",
        "neighboring 64-bit integer reused durable receipt"
    );
    for invalid in ["{} {}", "[]", "null"] {
        assert_eq!(
            code(&service.handle_sync(&ctx(), "prefab_edit", invalid.as_bytes())),
            "invalid_request",
            "invalid JSON object accepted: {invalid}"
        );
    }
    service.close();
}

#[test]
fn configure_prepared_rejects_escapes_and_keeps_last_accepted_baseline() {
    let (_guard, project) = review_project();
    let lifetime = ctx().with_cancel();
    let controller = controller(&project, "/bin/true", &lifetime);
    let baseline = join(&[
        &project,
        "Library/UnityMicroEditor/Preparation/current.json",
    ]);
    write(&baseline, "{}\n");
    controller
        .configure_prepared(&baseline, "Assets/Main.unity")
        .unwrap();
    let (_other_guard, other) = temp_dir();
    for (invalid_baseline, scene) in [
        (join(&[&other, "current.json"]), "Assets/Main.unity"),
        (baseline.clone(), "Assets/../Packages/Main.unity"),
        (baseline.clone(), "Assets/Main.prefab"),
    ] {
        assert!(
            controller
                .configure_prepared(&invalid_baseline, scene)
                .is_err(),
            "invalid preparation accepted {invalid_baseline} {scene}"
        );
    }
    let alias = join(&[&project, "Library/UnityMicroEditor/Preparation/alias.json"]);
    std::os::unix::fs::symlink(&baseline, &alias).unwrap();
    assert!(
        controller
            .configure_prepared(&alias, "Assets/Main.unity")
            .is_err(),
        "prepared symlink accepted"
    );
    let _ = controller.close();
    lifetime.cancel();
}

#[test]
fn runtime_start_cannot_interleave_preparation_activation() {
    for asynchronous in [false, true] {
        let (_guard, project) = review_project();
        let (_runner_guard, runner) = waiting_runner();
        let controller = controller(&project, &runner, &ctx());
        let old_baseline = join(&[
            &project,
            "Library/UnityMicroEditor/preparations/old/current.json",
        ]);
        let new_baseline = join(&[
            &project,
            "Library/UnityMicroEditor/preparations/new/current.json",
        ]);
        for baseline in [&old_baseline, &new_baseline] {
            immutable(baseline, json!({"project": project}));
        }
        controller
            .configure_prepared(&old_baseline, "Assets/Main.unity")
            .unwrap();
        controller.start().unwrap();
        let service = Service::new(Options {
            project: project.clone(),
            scene: "Assets/Main.unity".into(),
            baseline: old_baseline.clone(),
            controller: Some(controller.clone()),
            asynchronous,
            lifetime: Some(ctx()),
            ..Default::default()
        });
        let (done, finished) = mpsc::channel();
        service
            .locked(&ctx(), || {
                controller
                    .stop(&ctx())
                    .map_err(|failure| failure.error)
                    .unwrap();
                let (started, entered) = mpsc::channel();
                let runtime = service.clone();
                thread::spawn(move || {
                    started.send(()).unwrap();
                    let outcome =
                        runtime.runtime(&ctx(), "session_control", br#"{"action":"start"}"#);
                    done.send(outcome.map(drop).map_err(|error| error_text(&error)))
                        .unwrap();
                });
                entered.recv().unwrap();
                assert!(
                    finished.recv_timeout(Duration::from_millis(100)).is_err(),
                    "async={asynchronous}: runtime start entered Stop/Configure gap"
                );
                controller
                    .configure_prepared(&new_baseline, "Assets/Main.unity")
                    .unwrap();
                service.select(&new_baseline, "Assets/Main.unity");
                controller.start().unwrap();
            })
            .unwrap();
        finished
            .recv_timeout(Duration::from_secs(1))
            .expect("runtime start remained blocked after activation completed")
            .unwrap();
        service.close();
        let _ = controller.close();
    }
}

#[test]
fn cached_preparation_validates_identity_and_baseline_before_activation() {
    for changed in [
        "valid",
        "project",
        "scene",
        "requestId",
        "foreign-baseline",
        "baseline-symlink",
        "baseline-directory",
        "malformed-baseline",
        "baseline-project",
        "manifest-foreign",
        "manifest-missing",
        "manifest-unsupported",
        "manifest-scene",
        "player-missing",
        "player-symlink",
    ] {
        let (_guard, project) = review_project();
        let (_other_guard, other) = temp_dir();
        let id = "cached-preparation";
        let directory = join(&[&project, "Library/UnityMicroEditor/preparations", id]);
        let baseline = join(&[&directory, "current.json"]);
        let mut result = Preparation {
            schema_version: 1,
            project: project.clone(),
            scene: "Assets/Main.unity".into(),
            request_id: id.into(),
            baseline: baseline.clone(),
            ..Default::default()
        };
        let manifest = join(&[&directory, "source/scene.json"]);
        let player = join(&[&directory, "Player/Project.x86_64"]);
        let mut manifest_value = json!({"schemaVersion": 1, "projectRoot": project, "scenePath": result.scene, "unityVersion": UNITY_VERSION, "supported": true});
        immutable(&manifest, manifest_value.clone());
        write_mode(&player, "prepared native Player", 0o700);
        let compiler = CompileManifest {
            schema: 1,
            project: project.clone(),
            unity: "/missing-editor".into(),
            compiler_inputs: NilMap::from([("public-input", "verified".to_string())]),
            ..Default::default()
        };
        let mut baseline_value = json!({"schemaVersion": 1, "project": project, "manifest": manifest, "player": player, "compiler": compiler});
        immutable(&baseline, baseline_value.clone());
        match changed {
            "project" => result.project = other.clone(),
            "scene" => result.scene = "Assets/Other.unity".into(),
            "requestId" => result.request_id = "another-preparation".into(),
            "foreign-baseline" => result.baseline = join(&[&other, "current.json"]),
            "baseline-symlink" => {
                let outside = join(&[&other, "current.json"]);
                write(&outside, "{}");
                fs::remove_file(&baseline).unwrap();
                std::os::unix::fs::symlink(&outside, &baseline).unwrap();
            }
            "baseline-directory" => {
                fs::remove_file(&baseline).unwrap();
                fs::create_dir(&baseline).unwrap();
            }
            "malformed-baseline" => write(&baseline, "{broken"),
            "baseline-project" | "manifest-foreign" => {
                if changed == "baseline-project" {
                    baseline_value["project"] = json!(other);
                } else {
                    baseline_value["manifest"] = json!(join(&[&other, "scene.json"]));
                }
                write(&baseline, bytes(&baseline_value));
            }
            "manifest-missing" => fs::remove_file(&manifest).unwrap(),
            "manifest-unsupported" | "manifest-scene" => {
                if changed == "manifest-unsupported" {
                    manifest_value["supported"] = json!(false);
                } else {
                    manifest_value["scenePath"] = json!("Assets/Other.unity");
                }
                write(&manifest, bytes(&manifest_value));
            }
            "player-missing" | "player-symlink" => {
                fs::remove_file(&player).unwrap();
                if changed == "player-symlink" {
                    std::os::unix::fs::symlink("/bin/true", &player).unwrap();
                }
            }
            _ => {}
        }
        agent_immutable_json(&join(&[&directory, "result.json"]), &result).unwrap();
        let (_runner_guard, runner) = waiting_runner();
        let controller = controller(&project, &runner, &ctx());
        let old_baseline = join(&[&project, "Library/UnityMicroEditor/accepted/current.json"]);
        immutable(&old_baseline, json!({"project": project}));
        controller
            .configure_prepared(&old_baseline, "Assets/Main.unity")
            .unwrap();
        controller.start().unwrap();
        let service = Service::new(Options {
            project: project.clone(),
            editor: "/missing-editor".into(),
            baseline: old_baseline.clone(),
            controller: Some(controller.clone()),
            lifetime: Some(ctx()),
            ..Default::default()
        });
        let before = controller.status();
        let raw = bytes(&json!({"scene": "Assets/Main.unity", "requestId": id}));
        let outcome = service.handle_sync(&ctx(), "project_prepare", &raw);
        if changed == "valid" {
            outcome.expect("valid cached preparation required Editor rebuild or failed activation");
            assert_eq!(service.baseline(), baseline);
        } else {
            assert!(
                outcome.is_err(),
                "{changed}: invalid cached preparation reached activation"
            );
            let after = controller.status();
            assert!(
                service.baseline() == old_baseline
                    && after.session_id == before.session_id
                    && after.phase == before.phase
                    && after.last_sequence == before.last_sequence,
                "{changed}: invalid cached preparation mutated controller selection"
            );
        }
        service.close();
        let _ = controller.close();
    }
}

#[test]
fn project_build_backend_arguments_default_to_release() {
    for (raw, want) in [
        (r#"{"scenes":["Assets/Main.unity"]}"#, false),
        (
            r#"{"scenes":["Assets/Main.unity"],"development":false}"#,
            false,
        ),
        (
            r#"{"scenes":["Assets/Main.unity"],"development":true}"#,
            true,
        ),
    ] {
        let input = any_map(serde_json::from_str(raw).unwrap());
        let encoded = project_build_arguments(&input).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            format!(r#"{{"development":{want},"scenes":["Assets/Main.unity"]}}"#)
        );
    }
    for value in [json!(null), json!("false"), json!(0.0)] {
        assert!(
            project_build_arguments(&any_map(json!({"development": value}))).is_err(),
            "invalid build boolean accepted: {value}"
        );
    }
}
