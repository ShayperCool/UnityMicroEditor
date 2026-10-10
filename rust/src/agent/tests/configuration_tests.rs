use super::*;
use crate::{
    agentcontrol::{Reply, State},
    cancel::Canceled,
    compiler::{CompileManifest, CompilerGraphExport, CompilerSourceRoot, NilMap, NilVec},
    util::{agent_immutable_json, agent_read_json},
};
use actions::{RuntimeOutcome, RuntimeValue};
use configuration::requires_new_graph;
use serde_json::json;
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const WAIT: &[u8] = br#"{"action":"wait_update"}"#;

fn revision(project: &str, selections: &[ConfigurationSelection]) -> Result<String> {
    configuration_revision(project, selections)
}
fn rejected(state: &State, error: anyhow::Error) -> RuntimeOutcome {
    RuntimeOutcome {
        value: RuntimeValue::Controller(Reply::State(Box::new(state.clone()))),
        error: Some(error),
    }
}
fn recover(service: &Service, state: &State, error: anyhow::Error) -> Result<Value> {
    let outcome =
        service.recover_configuration(&ctx(), "session_control", WAIT, rejected(state, error));
    outcome.finish().map(|value| decoded(&value))
}

#[test]
fn revision_tracks_compiler_inputs() {
    let (_guard, project) = review_project();
    let original = revision(&project, &[]).unwrap();
    write(&join(&[&project, "Assets/Main.unity"]), "scene content");
    assert_eq!(
        revision(&project, &[]).unwrap(),
        original,
        "scene changes unexpectedly changed the managed configuration identity"
    );
    for path in [
        "Assets/Sample.cs",
        "Assets/Sample.cs.meta",
        "Assets/Gameplay.asmdef",
        "Packages/manifest.json",
        "ProjectSettings/ProjectVersion.txt",
    ] {
        let before = revision(&project, &[]).unwrap();
        write(
            &join(&[&project, path]),
            format!("changed compiler input {path}"),
        );
        assert_ne!(
            revision(&project, &[]).unwrap(),
            before,
            "configuration ignored {path}"
        );
    }
    let (_other_guard, other) = temp_dir();
    std::os::unix::fs::symlink(&other, join(&[&project, "Assets/linked-sources"])).unwrap();
    assert!(
        revision(&project, &[]).is_err(),
        "configuration silently skipped linked source directory"
    );
}

#[test]
fn recovery_queues_preparation_for_rejected_graph() {
    let (_guard, project) = review_project();
    write(&join(&[&project, "Assets/Main.unity"]), "selected scene");
    let service = Service::new(Options {
        project: project.clone(),
        scene: "Assets/Main.unity".into(),
        asynchronous: true,
        lifetime: Some(ctx()),
        ..Default::default()
    });
    // Block the coordinator while checking the externally visible pending receipt.
    let held = service.0.mu.lock().unwrap();
    let state = State {
        phase: "ready".into(),
        pid: 1234,
        revision: "old-generation".into(),
        update_error:
            "managed assembly configuration/GUID changed; regenerate source graph: Assets/Gameplay.asmdef"
                .into(),
        ..Default::default()
    };
    let pending = recover(
        &service,
        &state,
        anyhow::anyhow!(state.update_error.clone()),
    )
    .unwrap();
    assert_eq!(pending["status"], "result_pending");
    assert_eq!(pending["phase"], "preparing_configuration");
    assert_eq!(pending["currentPlayer"]["pid"], 1234);
    let job = &pending["preparation"];
    assert_eq!(
        job["status"], "result_pending",
        "recovery did not enqueue preparation"
    );
    let request: Value = agent_read_json(
        &join(&[
            &crate::sysutil::dir(job["jobStatusPath"].as_str().unwrap()),
            "request.json",
        ]),
        1 << 20,
    )
    .unwrap();
    assert_eq!(request["action"], "project_prepare");
    assert_eq!(request["arguments"]["scene"], "Assets/Main.unity");
    assert_eq!(request["arguments"]["activate"], true);
    let repeated = recover(
        &service,
        &state,
        anyhow::anyhow!(state.update_error.clone()),
    )
    .unwrap();
    assert_eq!(
        repeated["preparation"]["requestId"], job["requestId"],
        "poll changed preparation identity"
    );
    service.close_action_jobs();
    drop(held);
    service.close();
}

struct Fixture {
    project: String,
    selection: ConfigurationSelection,
    package_root: String,
    input: String,
    _guards: (TempDir, TempDir, TempDir),
}
fn fixture() -> Fixture {
    let (guard, project) = review_project();
    let (package_guard, package_root) = temp_dir();
    let (input_guard, input_directory) = temp_dir();
    let input = join(&[&input_directory, "public-plugin.dll"]);
    for (path, contents) in [
        (join(&[&project, "Assets/Main.unity"]), "selected scene"),
        (
            join(&[&project, "Assets/Main.unity.meta"]),
            "guid: 33333333333333333333333333333333\n",
        ),
        (
            join(&[&package_root, "package.json"]),
            r#"{"name":"com.example.external"}"#,
        ),
        (
            join(&[&package_root, "External.cs"]),
            "public class External {}",
        ),
        (
            join(&[&package_root, "External.asmdef"]),
            r#"{"name":"External"}"#,
        ),
        (
            join(&[&package_root, "External.asmdef.meta"]),
            "guid: 44444444444444444444444444444444\n",
        ),
        (input.clone(), "public compiled plugin"),
    ] {
        write(&path, contents);
    }
    let baseline = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/initial/current.json",
    ]);
    let inputs = NilMap::from([
        (input.clone(), "previous".to_string()),
        (
            join(&[&package_root, "External.asmdef"]),
            "previous".to_string(),
        ),
    ]);
    let compiler = CompileManifest {
        project: project.clone(),
        compiler_inputs: inputs.clone(),
        ..Default::default()
    };
    immutable(&baseline, json!({"project": project, "compiler": compiler}));
    let graph = CompilerGraphExport {
        schema_version: 1,
        project: project.clone(),
        compiler_inputs: inputs,
        source_roots: NilVec::from(vec![CompilerSourceRoot {
            path: "Packages/com.example.external".into(),
            source: package_root.clone(),
        }]),
        ..Default::default()
    };
    agent_immutable_json(
        &join(&[&crate::sysutil::dir(&baseline), "graph.json"]),
        &graph,
    )
    .unwrap();
    Fixture {
        project,
        selection: ConfigurationSelection {
            scene: "Assets/Main.unity".into(),
            baseline,
        },
        package_root,
        input,
        _guards: (guard, package_guard, input_guard),
    }
}

#[test]
fn revision_binds_scene_and_registered_public_inputs() {
    let fixture = fixture();
    let (project, package_root) = (&fixture.project, &fixture.package_root);
    let current = || revision(project, std::slice::from_ref(&fixture.selection)).unwrap();
    let original = current();
    write(
        &join(&[
            project,
            "Library/UnityMicroEditor/compiler-cache/unregistered.dll",
        ]),
        "cache output",
    );
    write(&join(&[project, "Assets/Other.unity"]), "unselected scene");
    assert_eq!(
        current(),
        original,
        "unrelated scene or private cache changed recovery identity"
    );
    let scene = join(&[project, &fixture.selection.scene]);
    for path in [
        scene.clone(),
        format!("{scene}.meta"),
        join(&[package_root, "External.cs"]),
        join(&[package_root, "External.asmdef"]),
        join(&[package_root, "External.asmdef.meta"]),
        join(&[package_root, "new.rsp.meta"]),
        join(&[package_root, "new.cs"]),
        fixture.input.clone(),
    ] {
        let before = current();
        write(&path, format!("changed public configuration input {path}"));
        assert_ne!(current(), before, "recovery ignored public input {path}");
    }
    let before = current();
    fs::remove_file(&fixture.input).unwrap();
    assert_ne!(
        current(),
        before,
        "removing a declared compile input did not change recovery identity"
    );
    let before = current();
    fs::remove_dir_all(package_root).unwrap();
    assert_ne!(
        current(),
        before,
        "removing a registered package did not change recovery identity"
    );
}

#[test]
fn revision_rejects_source_aliases_and_foreign_graph() {
    for invalid in [
        "package-symlink",
        "input-symlink",
        "private-root",
        "foreign-graph",
        "baseline-symlink",
    ] {
        let fixture = fixture();
        let (_other_guard, other) = temp_dir();
        let mut selection = fixture.selection.clone();
        match invalid {
            "package-symlink" => {
                std::os::unix::fs::symlink(&other, join(&[&fixture.package_root, "alias"])).unwrap()
            }
            "input-symlink" => {
                fs::remove_file(&fixture.input).unwrap();
                std::os::unix::fs::symlink(
                    join(&[&fixture.package_root, "External.cs"]),
                    &fixture.input,
                )
                .unwrap();
            }
            "private-root" | "foreign-graph" => {
                let graph_path = join(&[&crate::sysutil::dir(&selection.baseline), "graph.json"]);
                let mut graph: Value = agent_read_json(&graph_path, 1 << 20).unwrap();
                if invalid == "private-root" {
                    graph["sourceRoots"][0]["source"] = json!(join(&[
                        &fixture.project,
                        "Library/UnityMicroEditor/private"
                    ]));
                } else {
                    graph["project"] = json!(other);
                }
                write(&graph_path, bytes(&graph));
            }
            _ => {
                let alias = join(&[&crate::sysutil::dir(&selection.baseline), "alias.json"]);
                std::os::unix::fs::symlink(&selection.baseline, &alias).unwrap();
                selection.baseline = alias;
            }
        }
        assert!(
            revision(&fixture.project, &[selection]).is_err(),
            "{invalid}: unsafe recovery input accepted"
        );
    }
}

#[test]
fn recovery_leaves_failed_job_when_scene_or_external_package_changes() {
    for changed in ["scene", "external-package"] {
        let fixture = fixture();
        let (project, selection) = (&fixture.project, &fixture.selection);
        let service = Service::new(Options {
            project: project.clone(),
            scene: selection.scene.clone(),
            baseline: selection.baseline.clone(),
            asynchronous: true,
            lifetime: Some(ctx()),
            ..Default::default()
        });
        let current = revision(project, std::slice::from_ref(selection)).unwrap();
        let id = format!(
            "config-{}",
            &crate::sha256(format!("{}\n{current}", selection.scene).as_bytes())[..40]
        );
        let raw = bytes(&json!({"scene": selection.scene, "requestId": id, "activate": true}));
        let run: jobs::Run = Arc::new(|_, _, _| {
            Err(fail(
                "revision_conflict",
                "source changed during the previous preparation",
            ))
        });
        service
            .enqueue_action(&ctx(), "project_prepare", &raw, run.clone())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while service
            .enqueue_action(&ctx(), "project_prepare", &raw, run.clone())
            .is_ok()
        {
            assert!(
                Instant::now() < deadline,
                "failed preparation was not persisted"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let state = State {
            phase: "ready".into(),
            pid: 1234,
            revision: "old-player".into(),
            update_error: "managed configuration requires a fresh managed graph".into(),
            ..Default::default()
        };
        assert!(
            recover(
                &service,
                &state,
                anyhow::anyhow!(state.update_error.clone())
            )
            .is_err(),
            "{changed}: durable failed job was unexpectedly rerun"
        );
        let path = if changed == "external-package" {
            join(&[&fixture.package_root, "External.asmdef"])
        } else {
            join(&[project, &selection.scene])
        };
        write(&path, "repaired configuration");
        let held = service.0.mu.lock().unwrap();
        let pending = recover(
            &service,
            &state,
            anyhow::anyhow!(state.update_error.clone()),
        )
        .expect("changed configuration reused failed durable preparation");
        let job = &pending["preparation"];
        assert_ne!(job["requestId"], id.as_str());
        assert_eq!(job["status"], "result_pending");
        assert_eq!(pending["currentPlayer"]["pid"], 1234);
        service.close_action_jobs();
        drop(held);
        service.close();
    }
}

#[test]
fn recovery_preserves_ordinary_compile_and_cancellation_errors() {
    let service = Service::new(Options::default());
    for (failure, message) in [
        (
            anyhow::anyhow!("CS1002: semicolon expected"),
            "CS1002: semicolon expected",
        ),
        (Canceled::Canceled.into(), "canceled"),
        (Canceled::DeadlineExceeded.into(), "deadline exceeded"),
    ] {
        let cancelled = context_error(&failure);
        let state = State {
            phase: "ready".into(),
            pid: 1234,
            update_error: if cancelled {
                "configuration requires a fresh managed graph".into()
            } else {
                message.into()
            },
            ..Default::default()
        };
        let outcome = service.recover_configuration(
            &ctx(),
            "session_control",
            WAIT,
            rejected(&state, failure),
        );
        assert_eq!(error_text(outcome.error.as_ref().unwrap()), message);
        assert!(
            matches!(&outcome.value, RuntimeValue::Controller(Reply::State(kept)) if kept.pid == state.pid),
            "ordinary failure or canceled request was replaced by automatic preparation"
        );
    }
    assert!(
        !requires_new_graph("CS0246: type could not be found"),
        "ordinary C# error was classified as managed configuration change"
    );
}
