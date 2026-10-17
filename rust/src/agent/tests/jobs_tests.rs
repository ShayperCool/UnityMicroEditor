use super::*;
use crate::{
    agentcontrol::{self, Controller},
    authoring::AuthoringError,
    cancel::{Canceled, Flag, Source, select},
    compiler::{CompileManifest, NilMap},
};
use jobs::Run;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub(super) fn run(
    action: impl Fn(&Context, &str, &[u8]) -> Result<HandlerValue> + Send + Sync + 'static,
) -> Run {
    Arc::new(action)
}
fn map(value: Value) -> Result<HandlerValue> {
    map_reply(any_map(value))
}
fn pending(value: &HandlerValue) -> bool {
    is_map(value) && decoded(value)["status"] == "result_pending"
}
pub(super) fn wait_job(
    service: &Service,
    action: &str,
    raw: &[u8],
    run: &Run,
) -> Result<HandlerValue> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let result = service.enqueue_action(&ctx(), action, raw, run.clone())?;
        if !is_map(&result) {
            return Ok(result);
        }
        assert!(
            Instant::now() < deadline,
            "job receipt did not become confirmed"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn job_outlives_request_and_persists_confirmed_receipt() {
    let (_guard, project) = review_project();
    let service = plain_service(&project);
    let raw = br#"{"requestId":"detached-test","scene":"Assets/Main.unity"}"#;
    let (started, release) = (Flag::new(), Flag::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let action = {
        let (started, release, executions) = (started.clone(), release.clone(), executions.clone());
        run(move |ctx, _, _| {
            executions.fetch_add(1, Ordering::SeqCst);
            started.set();
            match select(&[Source::Flag(&release), Source::Context(ctx)], None) {
                Some(0) => map(json!({"baseline": "confirmed", "requestId": "detached-test"})),
                _ => Err(ctx.wait().into()),
            }
        })
    };
    let request = ctx().with_timeout(Duration::from_millis(50));
    let before = Instant::now();
    let first = service
        .enqueue_action(&request, "project_prepare", raw, action.clone())
        .unwrap();
    assert!(
        before.elapsed() < Duration::from_secs(1) && pending(&first),
        "enqueue blocked request"
    );
    assert!(started.wait(Some(Duration::from_secs(5))));
    request.cancel();
    let still = service
        .enqueue_action(&ctx(), "project_prepare", raw, action.clone())
        .unwrap();
    assert!(
        pending(&still),
        "caller cancellation stopped detached action"
    );
    release.set();
    let completed = wait_job(&service, "project_prepare", raw, &action).unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        decoded(&completed)["baseline"],
        "confirmed",
        "receipt lost confirmed result"
    );
    let restarted = plain_service(&project);
    let completed = restarted
        .enqueue_action(
            &ctx(),
            "project_prepare",
            raw,
            run(|_, _, _| panic!("confirmed job executed after restart")),
        )
        .unwrap();
    assert!(matches!(completed, HandlerValue::Json(_)));
    assert_eq!(
        decoded(&completed)["baseline"],
        "confirmed",
        "restart receipt changed"
    );
    assert_eq!(
        code(&restarted.enqueue_action(
            &ctx(),
            "project_prepare",
            br#"{"requestId":"detached-test","scene":"Assets/Changed.unity"}"#,
            action
        )),
        "request_id_conflict",
        "changed arguments accepted"
    );
    service.close();
    restarted.close();
}

#[test]
fn job_known_failure_and_unknown_outcome_are_never_reexecuted() {
    for failure in ["compile_rejected", "result_unknown"] {
        let (_guard, project) = review_project();
        let service = plain_service(&project);
        let raw = bytes(&json!({"requestId": "failed-test", "scene": "Assets/Main.unity"}));
        let executions = Arc::new(AtomicUsize::new(0));
        let action = {
            let executions = executions.clone();
            run(move |_, _, _| {
                executions.fetch_add(1, Ordering::SeqCst);
                Err(AuthoringError {
                    code: failure.into(),
                    message: "observed backend failure".into(),
                    details: Some(json!({"requestId": "phase-proof"})),
                }
                .into())
            })
        };
        let outcome = wait_job(&service, "project_prepare", &raw, &action);
        assert_eq!(code(&outcome), failure, "failure receipt lost");
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        let restarted = plain_service(&project);
        let outcome = restarted.enqueue_action(&ctx(), "project_prepare", &raw, action);
        assert_eq!(code(&outcome), failure);
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "restart reran unknown/failure"
        );
        let error = outcome.unwrap_err();
        assert_eq!(
            typed(&error).unwrap().details.as_ref().unwrap()["requestId"],
            "phase-proof"
        );
        service.close();
        restarted.close();
    }
}

#[test]
fn pending_job_can_poll_same_backend_request_after_restart() {
    let (_guard, project) = review_project();
    let service = plain_service(&project);
    let raw = br#"{"operationId":"pending-prefab","path":"Assets/Main.unity","expectedRevision":"original"}"#;
    let invoked = Flag::new();
    let signal = invoked.clone();
    service
        .enqueue_action(
            &ctx(),
            "prefab_edit",
            raw,
            run(move |_, _, _| {
                signal.set();
                Err(fail(
                    "result_pending",
                    "accepted Editor receipt is still pending",
                ))
            }),
        )
        .unwrap();
    assert!(invoked.wait(Some(Duration::from_secs(5))));
    service.close();
    let result_path = join(&[
        &project,
        "Library/UnityMicroEditor/action-jobs",
        &crate::sha256(b"pending-prefab"),
        "result.json",
    ]);
    assert!(
        absent(&result_path),
        "unconfirmed outcome was persisted as final failure"
    );
    let restarted = plain_service(&project);
    // The request's members with sorted keys and compacted values.
    let normalized = r#"{"expectedRevision":"original","operationId":"pending-prefab","path":"Assets/Main.unity"}"#;
    let executions = Arc::new(AtomicUsize::new(0));
    let (mismatch, mismatched) = mpsc::channel();
    let action = {
        let executions = executions.clone();
        run(move |_, _, retried| {
            executions.fetch_add(1, Ordering::SeqCst);
            if retried != normalized.as_bytes() {
                mismatch
                    .send(String::from_utf8_lossy(retried).into_owned())
                    .unwrap();
            }
            map(json!({"operationId": "pending-prefab", "confirmed": true}))
        })
    };
    let completed = wait_job(&restarted, "prefab_edit", raw, &action).unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        !reply_bytes(&completed).is_empty(),
        "same request did not recover confirmed receipt"
    );
    assert!(
        mismatched.try_recv().is_err(),
        "recovery changed operation identity or guards"
    );
    restarted.close();
}

#[test]
fn close_cancels_detached_work_before_taking_service_lock() {
    let (_guard, project) = review_project();
    let service = plain_service(&project);
    let entered = Flag::new();
    let action = {
        let (service, entered) = (service.clone(), entered.clone());
        run(move |ctx, _, _| {
            service.locked(ctx, || {
                entered.set();
                Err::<HandlerValue, _>(anyhow::Error::from(ctx.wait()))
            })?
        })
    };
    service
        .enqueue_action(
            &ctx(),
            "project_build",
            br#"{"requestId":"close-test","scenes":["Assets/Main.unity"]}"#,
            action,
        )
        .unwrap();
    assert!(entered.wait(Some(Duration::from_secs(5))));
    let (done, finished) = mpsc::channel();
    {
        let service = service.clone();
        thread::spawn(move || {
            service.close();
            done.send(()).unwrap();
        });
    }
    finished
        .recv_timeout(Duration::from_secs(1))
        .expect("Close waited on the action lock before canceling its job");
    let outcome = service.enqueue_action(
        &ctx(),
        "project_build",
        br#"{"requestId":"closed","scenes":[]}"#,
        run(|_, _, _| panic!("closed service ran a job")),
    );
    assert_eq!(
        outcome.unwrap_err().downcast_ref::<Canceled>(),
        Some(&Canceled::Canceled),
        "closed service accepted job"
    );
}

pub(super) fn preparation_fixture(project: &str, id: &str) -> (Preparation, String, String) {
    preparation_of(project, id, &[])
}
/// A preparation whose compiler plan was made with the managed inputs
/// `inputs` as they are on disk now: its graph accepts the sources for as
/// long as those files stay what they are and `Assets` holds no other
/// managed configuration.
pub(super) fn preparation_of(
    project: &str,
    id: &str,
    inputs: &[&str],
) -> (Preparation, String, String) {
    let directory = join(&[project, "Library/UnityMicroEditor/preparations", id]);
    let preparation = Preparation {
        schema_version: 1,
        project: project.into(),
        request_id: id.into(),
        scene: "Assets/Main.unity".into(),
        baseline: join(&[&directory, "current.json"]),
        ..Default::default()
    };
    let manifest = join(&[&directory, "source/scene.json"]);
    let player = join(&[&directory, "Player/Project.x86_64"]);
    immutable(
        &manifest,
        json!({"schemaVersion": 1, "projectRoot": project, "scenePath": preparation.scene, "supported": true, "unityVersion": UNITY_VERSION}),
    );
    write_mode(&player, "native Player", 0o700);
    let mut compiler = CompileManifest {
        schema: 1,
        project: project.into(),
        compiler_inputs: NilMap::from([("public-input", "verified".to_string())]),
        ..Default::default()
    };
    // The scripts of `review_project` are sources the plan was made with.
    let scripts = ["Assets/Sample.cs", "Assets/Other.cs"]
        .into_iter()
        .flat_map(|script| [script.to_string(), format!("{script}.meta")]);
    for input in scripts.chain(inputs.iter().map(|input| input.to_string())) {
        let path = join(&[project, &input]);
        if let Ok(hash) = crate::util::file_hash(&path) {
            compiler.compiler_inputs.insert(&path, hash);
        }
    }
    immutable(
        &preparation.baseline,
        json!({"schemaVersion": 1, "project": project, "manifest": manifest, "player": player, "compiler": compiler}),
    );
    // What a preparation leaves beside its baseline: the result a later
    // server reads to find it.
    immutable(&join(&[&directory, "result.json"]), json!(preparation));
    (preparation, manifest, player)
}
fn stopped_controller(project: &str, lifetime: &Context) -> Controller {
    Controller::new(
        lifetime,
        agentcontrol::Options {
            project: project.into(),
            runner: "/bin/true".into(),
            ..Default::default()
        },
    )
    .unwrap()
}
fn restarted_service(project: &str, controller: &Controller, lifetime: &Context) -> Service {
    Service::new(Options {
        project: project.into(),
        lifetime: Some(lifetime.clone()),
        controller: Some(controller.clone()),
        asynchronous: true,
        ..Default::default()
    })
}

/// Sources that were prepared before under the identity the recovery gives
/// them (an edit that was undone and made again, a branch switched back) have
/// a confirmed receipt and no job, so no job activates their preparation. The
/// recovery does, and restarts the session on it.
#[test]
fn recovery_continues_on_the_preparation_the_sources_already_have() {
    let (_guard, project) = review_project();
    write(&join(&[&project, "Assets/Main.unity"]), "selected scene");
    let (active, _, _) = preparation_fixture(&project, "active-test");
    let revision = configuration_revision(
        &project,
        &[ConfigurationSelection {
            scene: active.scene.clone(),
            baseline: active.baseline.clone(),
        }],
    )
    .unwrap();
    let identity = format!(
        "config-{}",
        &crate::sha256(format!("{}\n{revision}", active.scene).as_bytes())[..40]
    );
    let (known, _, _) = preparation_fixture(&project, &identity);
    let raw = marshal(&Any::Object(obj([
        ("scene", active.scene.clone().into()),
        ("requestId", identity.clone().into()),
        ("activate", true.into()),
    ])))
    .unwrap();
    // An earlier server prepared these sources.
    let earlier = plain_service(&project);
    let confirmed = known.clone();
    let outcome = wait_job(
        &earlier,
        "project_prepare",
        &raw,
        &run(move |_, _, _| json_reply(&confirmed)),
    );
    earlier.close();
    outcome.unwrap();
    // A runner that stays, and writes down what it was started with.
    let (_runner_guard, directory) = temp_dir();
    let runner = join(&[&directory, "runner"]);
    write_mode(
        &runner,
        "#!/bin/sh\necho \"$@\" >> \"$0.arguments\"\nexec sleep 60\n",
        0o700,
    );
    let started = || -> Vec<String> {
        fs::read_to_string(format!("{runner}.arguments"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let launches = |count: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while started().len() < count {
            assert!(
                Instant::now() < deadline,
                "the runner was started {:?}",
                started()
            );
            thread::sleep(Duration::from_millis(10));
        }
        started()
    };
    let lifetime = ctx().with_cancel();
    let controller = Controller::new(
        &lifetime,
        agentcontrol::Options {
            project: project.clone(),
            runner: runner.clone(),
            output: join(&[&project, "Library/UnityMicroEditor/agent"]),
            ..Default::default()
        },
    )
    .unwrap();
    controller
        .configure_prepared(&active.baseline, &active.scene)
        .unwrap();
    let service = Service::new(Options {
        project: project.clone(),
        scene: active.scene.clone(),
        baseline: active.baseline.clone(),
        lifetime: Some(lifetime.clone()),
        controller: Some(controller.clone()),
        asynchronous: true,
        ..Default::default()
    });
    let before = controller.start().unwrap();
    assert!(launches(1)[0].contains(&format!("--baseline {}", active.baseline)));
    // What the session answers after the edit that left the prepared graph.
    let state = agentcontrol::State {
        phase: "ready".into(),
        pid: 1234,
        session_id: before.session_id.clone(),
        update_error:
            "new managed configuration requires a fresh managed graph: Assets/Lib/Lib.asmdef".into(),
        ..Default::default()
    };
    let recover = |action: &str| {
        let outcome = service.recover_configuration(
            &ctx(),
            "session_control",
            &bytes(&json!({"action": action})),
            actions::RuntimeOutcome {
                value: actions::RuntimeValue::Controller(agentcontrol::Reply::State(Box::new(
                    state.clone(),
                ))),
                error: None,
            },
        );
        decoded(&outcome.finish().unwrap())
    };
    // Not behind a project action that runs: the receipt is shown and stays.
    let held = service.0.mu.lock().unwrap();
    let busy = recover("status");
    assert!(
        busy["preparationRequired"] == true
            && busy["preparation"]["requestId"] == identity.as_str()
            && service.baseline() == active.baseline
            && started().len() == 1,
        "{busy}"
    );
    drop(held);
    let status = recover("status");
    assert_eq!(
        (service.baseline(), service.scene()),
        (known.baseline.clone(), active.scene.clone()),
        "the recovery left the session on the preparation of other sources: {status}"
    );
    let after = controller.status();
    assert!(
        after.session_id != before.session_id && after.phase == "starting",
        "the session was not restarted: {} {}",
        after.session_id,
        after.phase
    );
    assert!(
        launches(2)[1].contains(&format!("--baseline {}", known.baseline)),
        "{:?}",
        started()
    );
    // The reply is the session as it continues: nothing is required any more.
    assert!(
        status.get("preparationRequired").is_none()
            && status["sessionId"] == after.session_id.as_str()
            && status["preparation"]["requestId"] == identity.as_str(),
        "{status}"
    );
    // The same rejection on the preparation the recovery chose says that
    // the choice did not fit after all. Nothing waits on a preparation that
    // nobody makes: the session goes back to the one it had, which fits as
    // well, and after that a new preparation is requested under a name of
    // its own. No preparation is tried twice for the same sources.
    let back = recover("wait_update");
    assert!(
        back["status"] == "result_pending"
            && back["message"]
                .as_str()
                .is_some_and(|text| text.contains("restarting on it"))
            && back["preparation"]["requestId"] == "active-test",
        "{back}"
    );
    assert_eq!(service.baseline(), active.baseline);
    assert_eq!(launches(3).len(), 3);
    let held = service.0.mu.lock().unwrap();
    let requested = recover("status");
    let next = requested["preparation"]["requestId"].as_str().unwrap();
    assert!(
        requested["preparationRequired"] == true
            && requested["preparation"]["status"] == "result_pending"
            && next.starts_with("config-")
            && next != identity
            && service.baseline() == active.baseline,
        "{requested}"
    );
    // A poll names the same request, and none restarts anything.
    assert_eq!(recover("status")["preparation"]["requestId"], next);
    assert_eq!(started().len(), 3);
    service.close_action_jobs();
    drop(held);
    service.close();
    let _ = controller.close();
    lifetime.cancel();
}

#[test]
fn confirmed_preparation_restores_activation_after_restart() {
    let (_guard, project) = review_project();
    let (preparation, _, _) = preparation_fixture(&project, "restore-test");
    let original = plain_service(&project);
    let raw = br#"{"requestId":"restore-test","scene":"Assets/Main.unity","activate":true}"#;
    let confirmed = preparation.clone();
    let outcome = wait_job(
        &original,
        "project_prepare",
        raw,
        &run(move |_, _, _| json_reply(&confirmed)),
    );
    original.close();
    outcome.unwrap();
    let lifetime = ctx().with_cancel();
    let controller = stopped_controller(&project, &lifetime);
    let restarted = restarted_service(&project, &controller, &lifetime);
    restarted.handle(&ctx(), "project_prepare", raw).unwrap();
    assert_eq!(
        (restarted.baseline(), restarted.scene()),
        (
            preparation.baseline.clone(),
            "Assets/Main.unity".to_string()
        ),
        "confirmed preparation did not restore controller selection"
    );
    restarted.close();
    let _ = controller.close();
    lifetime.cancel();
}

#[test]
fn persisted_preparation_rejects_corrupt_files_before_restore() {
    for corrupt in ["baseline-json", "manifest-missing", "player-symlink"] {
        let (_guard, project) = review_project();
        let (preparation, manifest, player) = preparation_fixture(&project, "restore-corrupt");
        let raw = br#"{"requestId":"restore-corrupt","scene":"Assets/Main.unity","activate":true}"#;
        let original = plain_service(&project);
        let executions = Arc::new(AtomicUsize::new(0));
        let action = {
            let (executions, confirmed) = (executions.clone(), preparation.clone());
            run(move |_, _, _| {
                executions.fetch_add(1, Ordering::SeqCst);
                json_reply(&confirmed)
            })
        };
        let outcome = wait_job(&original, "project_prepare", raw, &action);
        original.close();
        outcome.unwrap();
        match corrupt {
            "baseline-json" => write(&preparation.baseline, "{bad"),
            "manifest-missing" => fs::remove_file(&manifest).unwrap(),
            _ => {
                fs::remove_file(&player).unwrap();
                std::os::unix::fs::symlink("/bin/true", &player).unwrap();
            }
        }
        let controller = stopped_controller(&project, &ctx());
        let restarted = restarted_service(&project, &controller, &ctx());
        assert!(
            restarted.handle(&ctx(), "project_prepare", raw).is_err(),
            "{corrupt}: corrupt confirmed preparation was activated"
        );
        assert!(
            restarted.baseline().is_empty()
                && controller.status().phase == "stopped"
                && executions.load(Ordering::SeqCst) == 1,
            "{corrupt}: restore mutated controller or reran confirmed work"
        );
        restarted.close();
        let _ = controller.close();
    }
}

#[test]
fn start_does_not_ignore_different_explicit_scene() {
    let (_guard, project) = review_project();
    let service = Service::new(Options {
        project,
        baseline: "/prepared/current.json".into(),
        scene: "Assets/Main.unity".into(),
        ..Default::default()
    });
    for action in ["start", "reset"] {
        let raw = bytes(&json!({"action": action, "arguments": {"scene": "Assets/Other.unity"}}));
        let outcome = service.runtime_with_action_jobs(&ctx(), "session_control", &raw);
        let detail = outcome
            .error
            .as_ref()
            .and_then(typed)
            .map(|detail| detail.code.clone());
        assert_eq!(
            detail.as_deref(),
            Some("scene_requires_preparation"),
            "{action} silently ignored the selected scene"
        );
    }
}

/// A start or reset of a session that has a preparation cannot take the
/// inputs of one: it names the call that does, instead of returning the
/// running session as if they were used. A misspelled key is refused.
#[test]
fn start_does_not_ignore_preparation_inputs_or_unknown_keys() {
    let (_guard, project) = review_project();
    let service = Service::new(Options {
        project,
        baseline: "/prepared/current.json".into(),
        scene: "Assets/Main.unity".into(),
        ..Default::default()
    });
    for action in ["start", "reset"] {
        for arguments in [
            json!({"rebuildHost": true}),
            json!({"hostPlayer": "/elsewhere/Player"}),
            json!({"engineProfile": "/p.json", "nativeCatalog": "/c"}),
        ] {
            let raw = bytes(&json!({"action": action, "arguments": arguments}));
            let outcome = service.runtime_with_action_jobs(&ctx(), "session_control", &raw);
            let detail = outcome.error.as_ref().and_then(typed);
            assert_eq!(
                detail.map(|detail| detail.code.as_str()),
                Some("preparation_input_ignored"),
                "{action} {arguments} was taken for a launch with it"
            );
            assert!(
                outcome
                    .error
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("call project_prepare with this scene"),
                "{:?}",
                outcome.error
            );
        }
        let raw = bytes(&json!({"action": action, "arguments": {"hotreload": false}}));
        let outcome = service.runtime_with_action_jobs(&ctx(), "session_control", &raw);
        let text = format!("{:#}", outcome.error.expect("a misspelled key is refused"));
        assert!(
            text.contains("unknown start/reset argument \"hotreload\""),
            "{text}"
        );
    }
}

/// The start that began a preparation with an input is reissued, as the
/// documentation says, with the same input; once that preparation is the
/// session's, the input is what the session runs and the start goes on
/// (session6-5). Another input, or one of the wrong type, is refused
/// (mcp6-3).
#[test]
fn a_start_reissued_with_the_inputs_of_its_preparation_goes_on() {
    let (_guard, project) = review_project();
    let id = "auto-prepared";
    let record = join(&[
        &project,
        "Library/UnityMicroEditor/action-jobs",
        &crate::sha256(id.as_bytes()),
        "request.json",
    ]);
    fs::create_dir_all(crate::sysutil::dir(&record)).unwrap();
    write(
        &record,
        format!(
            r#"{{"schemaVersion":1,"project":"{project}","action":"project_prepare","identity":"{id}","arguments":{{"activate":true,"hostPlayer":"/host/Player","rebuildHost":true,"requestId":"{id}","scene":"Assets/Main.unity"}}}}"#
        ),
    );
    let service = Service::new(Options {
        project: project.clone(),
        baseline: join(&[
            &project,
            "Library/UnityMicroEditor/preparations",
            id,
            "current.json",
        ]),
        scene: "Assets/Main.unity".into(),
        ..Default::default()
    });
    let code = |arguments: Value| {
        let raw = bytes(&json!({"action": "start", "arguments": arguments}));
        let outcome = service.runtime_with_action_jobs(&ctx(), "session_control", &raw);
        let error = outcome.error.expect("no controller in this test");
        typed(&error).map_or(format!("{error:#}"), |detail| detail.code.clone())
    };
    // Taken to the controller, which this test does not have.
    for arguments in [
        json!({"hostPlayer": "/host/Player"}),
        json!({"hostPlayer": "/host/Player", "rebuildHost": true}),
        json!({"rebuildHost": false, "nativeCatalog": null}),
    ] {
        assert_eq!(
            code(arguments.clone()),
            "runtime controller is not configured",
            "{arguments}"
        );
    }
    for arguments in [
        json!({"hostPlayer": "/elsewhere/Player"}),
        json!({"hostPlayer": "/host/Player", "nativeCatalog": "/c"}),
    ] {
        assert_eq!(
            code(arguments.clone()),
            "preparation_input_ignored",
            "{arguments}"
        );
    }
    for arguments in [
        json!({"nativeCatalog": true}),
        json!({"hostPlayer": 7}),
        json!({"rebuildHost": "true"}),
    ] {
        assert_eq!(code(arguments.clone()), "invalid_request", "{arguments}");
    }
}

/// The receipt bytes written for the same outcomes, which the persisted format fixes.
#[test]
fn receipts_have_fixed_layout() {
    let (mut receipt, pending) = jobs::make_action_job_receipt(map(json!({"b": 1, "a": "<"})));
    assert!(!pending);
    receipt.request_fingerprint = "f".into();
    assert_eq!(
        String::from_utf8(marshal(&receipt).unwrap()).unwrap(),
        r#"{"schemaVersion":1,"requestFingerprint":"f","result":{"a":"@","b":1}}"#
            .replace('@', &format!("{}u003c", '\\'))
    );
    let (mut receipt, pending) =
        jobs::make_action_job_receipt(Err(fail("compile_rejected", "no & yes")));
    assert!(!pending);
    receipt.request_fingerprint = "f".into();
    assert_eq!(
        String::from_utf8(marshal(&receipt).unwrap()).unwrap(),
        r#"{"schemaVersion":1,"requestFingerprint":"f","error":{"code":"compile_rejected","message":"no @ yes"}}"#.replace('@', &format!("{}u0026", '\\'))
    );
    let (receipt, pending) = jobs::make_action_job_receipt(Err(anyhow::anyhow!("plain failure")));
    assert!(!pending);
    assert_eq!(receipt.error.unwrap().code, "project_action_failed");
    for outcome in [
        Err(fail("result_pending", "later")),
        Err(Canceled::Canceled.into()),
        Err(Canceled::DeadlineExceeded.into()),
        map(json!({"status": "result_pending"})),
    ] {
        assert!(jobs::make_action_job_receipt(outcome).1);
    }
}

#[test]
fn pending_preparation_reports_its_stage_to_start_status_and_wait() {
    let (_guard, project) = review_project();
    let lifetime = ctx().with_cancel();
    let controller = stopped_controller(&project, &lifetime);
    let service = Service::new(Options {
        project: project.clone(),
        lifetime: Some(lifetime.clone()),
        controller: Some(controller),
        asynchronous: true,
        scene: "Assets/Main.unity".into(),
        ..Default::default()
    });
    // The arguments a start derives for its automatic preparation.
    let raw = br#"{"activate":true,"requestId":"staged","scene":"Assets/Main.unity"}"#;
    let (started, release) = (Flag::new(), Flag::new());
    let action = {
        let (started, release, service) = (started.clone(), release.clone(), service.clone());
        run(move |ctx, action, _| {
            // What `handle_sync` and the preparation do around their work,
            // including the project action lock held for the whole run.
            service.locked(ctx, || {
                service.progress_begin(action, "staged");
                service.stage("build_host", "building the host", "/stage/editor.log");
                started.set();
                select(&[Source::Flag(&release), Source::Context(ctx)], None);
                service.progress_end();
            })?;
            map(json!({"baseline": "confirmed", "requestId": "staged"}))
        })
    };
    let first = service
        .enqueue_action(&ctx(), "project_prepare", raw, action.clone())
        .unwrap();
    assert_eq!(decoded(&first)["progress"]["stage"], "queued");
    assert!(started.wait(Some(Duration::from_secs(3))));
    let polled = decoded(
        &service
            .enqueue_action(&ctx(), "project_prepare", raw, action.clone())
            .unwrap(),
    );
    assert_eq!(polled["status"], "result_pending");
    let progress = &polled["progress"];
    assert!(
        progress["stage"] == "build_host"
            && progress["stageIndex"] == 3
            && progress["stageCount"] == 12
            && progress["logPath"] == "/stage/editor.log"
            && progress["elapsedSeconds"].is_number(),
        "pending reply lacks the running stage and its log: {polled}"
    );
    // A reissued start reports the same progress at once; it does not queue
    // behind the lock the preparation holds.
    let brief = ctx().with_timeout(Duration::from_secs(2));
    let reissued = service
        .runtime_with_action_jobs(
            &brief,
            "session_control",
            br#"{"action":"start","requestId":"staged"}"#,
        )
        .finish()
        .expect("reissued start waited for the running preparation");
    assert!(
        pending(&reissued) && decoded(&reissued)["progress"]["stage"] == "build_host",
        "reissued start lacks the running stage: {}",
        decoded(&reissued)
    );
    // Before the first Player exists the controller only knows `stopped`.
    for wait in ["wait", "wait_update"] {
        let outcome = service.runtime_with_action_jobs(
            &ctx(),
            "session_control",
            &bytes(&json!({"action": wait})),
        );
        let reply = decoded(&outcome.finish().expect("wait failed during preparation"));
        assert!(
            reply["status"] == "result_pending" && reply["preparation"]["stage"] == "build_host",
            "{wait} reported a stopped session instead of the preparation: {reply}"
        );
    }
    let status = decoded(
        &service
            .runtime_with_action_jobs(&ctx(), "session_control", br#"{"action":"status"}"#)
            .finish()
            .unwrap(),
    );
    assert!(
        status["phase"] == "stopped" && status["preparation"]["logPath"] == "/stage/editor.log",
        "status hides the running preparation: {status}"
    );
    release.set();
    wait_job(&service, "project_prepare", raw, &action).unwrap();
    let status = decoded(
        &service
            .runtime_with_action_jobs(&ctx(), "session_control", br#"{"action":"status"}"#)
            .finish()
            .unwrap(),
    );
    assert!(
        status.get("preparation").is_none(),
        "finished preparation still reported"
    );
    lifetime.cancel();
    service.close();
}
