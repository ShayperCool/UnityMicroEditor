//! The configuration recovery as a caller of the MCP server meets it.
//!
//! Every test drives `Service::runtime`, the entry point of the server, with
//! a real `Controller` and a runner process that speaks the runner's
//! protocol (`agentcontrol::testing::helper_process`, the helper of the
//! controller's own tests, run here as `controller_helper_process`). Only the
//! Unity part of a preparation is replaced, by `preparation_of`: a
//! preparation with a compiler plan that
//! names the managed inputs it was made with, which is what the recovery
//! reads to tell what a preparation was made from.
use super::jobs_tests::{preparation_fixture, preparation_of, run, wait_job};
use super::*;
use crate::{
    agentcontrol::{self, Controller},
    sysutil::testing::{helper_arguments, shell_quote, write_executable},
};
use serde_json::json;
use std::{
    thread,
    time::{Duration, Instant},
};

const HELPER: &str = "agent::tests::recovery_tests::controller_helper_process";
const SCENE: &str = "Assets/Main.unity";
const ASMDEF: &str = "Assets/Lib/Lib.asmdef";

// The runner the rigs start is this test of the same binary.
#[test]
fn controller_helper_process() {
    agentcontrol::testing::helper_process()
}

struct Rig {
    _guard: TempDir,
    project: String,
    output: String,
    controller: Controller,
    service: Service,
    lifetime: Context,
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.service.close();
        let _ = self.controller.close();
        self.lifetime.cancel();
    }
}

/// The revision the recovery computes for the sources on disk while
/// `baseline` is the selected preparation.
fn revision(project: &str, baseline: &str) -> String {
    configuration_revision(
        project,
        &[ConfigurationSelection {
            scene: SCENE.into(),
            baseline: baseline.into(),
        }],
    )
    .unwrap()
}
/// The request ids the recovery has for those sources, the first and the next.
fn identity(project: &str, baseline: &str, attempt: u32) -> String {
    Service::recovery_identity(SCENE, &revision(project, baseline), attempt)
}
/// A confirmed job receipt for `preparation`, as an earlier server leaves it.
fn confirm(project: &str, preparation: &Preparation) {
    let earlier = plain_service(project);
    let confirmed = preparation.clone();
    let arguments = marshal(&Any::Object(obj([
        ("scene", SCENE.into()),
        ("requestId", preparation.request_id.clone().into()),
        ("activate", true.into()),
    ])))
    .unwrap();
    wait_job(
        &earlier,
        "project_prepare",
        &arguments,
        &run(move |_, _, _| json_reply(&confirmed)),
    )
    .unwrap();
    earlier.close();
}

/// A project with a session that is ready on the preparation `initial`,
/// which was made from the sources without the assembly definition.
fn rig(mode: &str) -> (Rig, Preparation) {
    let (guard, project) = review_project();
    write(&join(&[&project, SCENE]), "selected scene");
    write(&join(&[&project, "helper-mode"]), mode);
    let (initial, _, _) = preparation_fixture(&project, "initial");
    let binary = std::env::current_exe()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    let runner = join(&[&project, "runner"]);
    // The runner writes down what it was started with.
    write_executable(
        &runner,
        &format!(
            "#!/bin/sh\necho \"$*\" >> {log}\nexec env UNITY_AGENT_CONTROL_HELPER=1 {binary} {helper} \"$@\" 3>&1 >/dev/null\n",
            log = shell_quote(&join(&[&project, "launches.log"])),
            binary = shell_quote(&binary),
            helper = helper_arguments(HELPER).join(" "),
        ),
    );
    let lifetime = ctx().with_cancel();
    let output = join(&[&project, "Library/UnityMicroEditor/agent"]);
    let controller = Controller::new(
        &lifetime,
        agentcontrol::Options {
            project: project.clone(),
            runner,
            output: output.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    controller
        .configure_prepared(&initial.baseline, &initial.scene)
        .unwrap();
    let service = Service::new(Options {
        project: project.clone(),
        scene: SCENE.into(),
        baseline: initial.baseline.clone(),
        lifetime: Some(lifetime.clone()),
        controller: Some(controller.clone()),
        asynchronous: true,
        ..Default::default()
    });
    let rig = Rig {
        _guard: guard,
        project,
        output,
        controller,
        service,
        lifetime,
    };
    rig.controller.start().unwrap();
    rig.until("the first Player is ready", |state| state.phase == "ready");
    (rig, initial)
}

impl Rig {
    fn until(
        &self,
        what: &str,
        reached: impl Fn(&agentcontrol::State) -> bool,
    ) -> agentcontrol::State {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let state = self.controller.status();
            if reached(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: phase {} lastError {:?} updateError {:?}",
                state.phase,
                state.last_error,
                state.update_error
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    /// What the runner of this session prints next.
    fn emit(&self, event: Value) {
        let path = join(&[&self.output, "helper-event.json"]);
        let temporary = format!("{path}.temporary");
        fs::write(&temporary, bytes(&event)).unwrap();
        fs::rename(temporary, path).unwrap();
    }
    /// The next runner starts without an event of the one before it.
    fn forget_event(&self) {
        let _ = fs::remove_file(join(&[&self.output, "helper-event.json"]));
    }
    fn launches(&self) -> Vec<String> {
        fs::read_to_string(join(&[&self.project, "launches.log"]))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn save_asmdef(&self) {
        write(&join(&[&self.project, ASMDEF]), r#"{"name":"Lib"}"#);
    }
    fn undo_asmdef(&self) {
        fs::remove_dir_all(join(&[&self.project, "Assets/Lib"])).unwrap();
    }
    /// The runner rejects the saved sources as the real one does when they
    /// have left the graph of its preparation, and the controller has read it.
    fn reject(&self, pending: bool) -> agentcontrol::State {
        let text = format!(
            "new managed configuration requires a fresh managed graph: {}",
            join(&[&self.project, ASMDEF])
        );
        let before = self.controller.status().last_update_rejected_sequence;
        let mut event = json!({"event": "update-rejected", "error": text, "preparationRequired": true, "stamp": before});
        if pending {
            event["pending"] = json!(true);
        }
        self.emit(event);
        self.until("the rejection is read", |state| {
            state.last_update_rejected_sequence > before
        })
    }
    fn call(&self, action: &str) -> Result<Value> {
        self.call_within(&ctx(), json!({"action": action}))
    }
    fn call_within(&self, ctx: &Context, arguments: Value) -> Result<Value> {
        self.service
            .runtime(ctx, "session_control", &bytes(&arguments))
            .map(|value| decoded(&value))
    }
    /// The request ids of the preparation jobs this service was asked for.
    fn jobs(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .service
            .0
            .jobs
            .lock()
            .unwrap()
            .jobs
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }
    /// Runs the session on `preparation`, as an activation does.
    fn switch_to(&self, preparation: &Preparation) -> agentcontrol::State {
        let before = self.controller.status().session_id;
        self.forget_event();
        self.controller.stop(&ctx()).unwrap();
        self.controller
            .configure_prepared(&preparation.baseline, &preparation.scene)
            .unwrap();
        self.service
            .select(&preparation.baseline, &preparation.scene);
        self.controller.start().unwrap();
        self.until("the session is ready again", |state| {
            state.phase == "ready" && state.session_id != before
        })
    }
}
fn alive(pid: i64) -> bool {
    crate::sysutil::kill(pid, 0).is_ok()
}

/// The request id of a recovery names the sources it was computed from, and
/// its job reads them later: an edit that was taken back while the job ran
/// left a confirmed receipt, under the name of the edit, for a preparation
/// made without it. When the edit is made again, a status poll must not stop
/// the Player that runs to start the session on that preparation, which
/// cannot start. It keeps the Player and requests a preparation of the
/// sources under the next name.
#[test]
fn a_receipt_made_from_other_sources_does_not_end_the_player() {
    let (rig, initial) = rig("plain");
    let first = rig.controller.status();
    rig.save_asmdef();
    let named = identity(&rig.project, &initial.baseline, 0);
    // Made from the sources without the assembly definition.
    let (known, _, _) = preparation_fixture(&rig.project, &named);
    confirm(&rig.project, &known);
    rig.reject(false);
    // The job of the new preparation waits for this lock: no Unity here.
    let held = rig.service.0.mu.lock().unwrap();
    let next = identity(&rig.project, &initial.baseline, 1);
    for _ in 0..3 {
        let status = rig.call("status").unwrap();
        assert!(
            status["phase"] == "ready"
                && status["preparationRequired"] == true
                && status["preparation"]["requestId"] == next.as_str()
                && status["preparation"]["status"] == "result_pending"
                && status.get("preparationError").is_none(),
            "{status}"
        );
    }
    let waited = rig.call("wait_update").unwrap();
    assert!(
        waited["status"] == "result_pending"
            && waited["phase"] == "preparing_configuration"
            && waited["preparation"]["requestId"] == next.as_str()
            && waited["message"]
                .as_str()
                .is_some_and(|text| text.contains("is being prepared for the saved sources")),
        "{waited}"
    );
    let after = rig.controller.status();
    assert!(
        after.session_id == first.session_id && after.phase == "ready" && alive(first.pid),
        "the Player that ran was replaced: {after:?}"
    );
    assert_eq!(rig.launches().len(), 1);
    assert_eq!(rig.service.baseline(), initial.baseline);
    assert_eq!(rig.jobs(), vec![next]);
    rig.service.close_action_jobs();
    drop(held);
}

/// A recompile issued while the saved sources stand rejected for having
/// left the graph is answered like a wait, with the preparation that
/// answers the rejection; the Player that runs is not restarted on the old
/// graph (session6-4).
#[test]
fn a_recompile_after_a_graph_rejection_names_the_preparation() {
    let (rig, initial) = rig("plain");
    let first = rig.controller.status();
    rig.save_asmdef();
    rig.reject(false);
    // The job of the preparation waits for this lock: no Unity here.
    let held = rig.service.0.mu.lock().unwrap();
    let named = identity(&rig.project, &initial.baseline, 0);
    let recompiled = rig
        .service
        .runtime(
            &ctx(),
            "hot_reload",
            &bytes(&json!({"action": "recompile", "requestId": "rc-graph"})),
        )
        .map(|value| decoded(&value))
        .unwrap();
    assert!(
        recompiled["status"] == "result_pending"
            && recompiled["preparation"]["requestId"] == named.as_str(),
        "{recompiled}"
    );
    let after = rig.controller.status();
    assert!(
        after.session_id == first.session_id
            && after.phase == "ready"
            && after.hot_reload.pending_recompile.is_empty()
            && alive(first.pid),
        "the recompile restarted the Player: {after:?}"
    );
    assert_eq!(rig.launches().len(), 1);
    assert_eq!(rig.jobs(), vec![named]);
    rig.service.close_action_jobs();
    drop(held);
}

/// The same receipt once its preparation is the one the session runs on:
/// the job activated it while the edit was undone, and the edit is made
/// again. The runner rejects the sources, and the receipt of their name is
/// the selected preparation. That is no reason to wait for a preparation
/// that nobody makes: one is requested under the next name.
#[test]
fn a_rejection_on_the_preparation_of_its_own_name_requests_another() {
    let (rig, initial) = rig("plain");
    rig.save_asmdef();
    let named = identity(&rig.project, &initial.baseline, 0);
    let (poisoned, _, _) = preparation_fixture(&rig.project, &named);
    confirm(&rig.project, &poisoned);
    let running = rig.switch_to(&poisoned);
    // The name of the sources is the same under that preparation.
    assert_eq!(identity(&rig.project, &poisoned.baseline, 0), named);
    rig.reject(false);
    let held = rig.service.0.mu.lock().unwrap();
    let next = identity(&rig.project, &poisoned.baseline, 1);
    let status = rig.call("status").unwrap();
    assert!(
        status["preparationRequired"] == true
            && status["preparation"]["requestId"] == next.as_str()
            && status["preparation"]["status"] == "result_pending",
        "{status}"
    );
    for action in ["wait_update", "wait"] {
        let waited = rig.call(action).unwrap();
        assert!(
            waited["status"] == "result_pending"
                && waited["preparation"]["requestId"] == next.as_str()
                && waited.get("message").is_some(),
            "{action}: {waited}"
        );
    }
    assert_eq!(rig.controller.status().session_id, running.session_id);
    assert_eq!(rig.jobs(), vec![next]);
    rig.service.close_action_jobs();
    drop(held);
}

/// A preparation the project has for the saved sources is used whatever its
/// name is: the one a session started on is found again when an edit that
/// left its graph is undone, and the one of the edit when it is made again.
/// The check is the one a start of the runner makes, so what does not fit
/// is left alone.
#[test]
fn the_recovery_continues_on_any_preparation_that_fits_the_sources() {
    let (rig, initial) = rig("plain");
    let first = rig.controller.status();
    // The edit, and a preparation that was made with it, under a name the
    // recovery does not compute.
    rig.save_asmdef();
    let (with_library, _, _) = preparation_of(&rig.project, "with-library", &[ASMDEF]);
    rig.reject(false);
    rig.forget_event();
    let status = rig.call("status").unwrap();
    assert!(
        status.get("preparationRequired").is_none()
            && status["preparation"]["requestId"] == "with-library"
            && status["sessionId"] != first.session_id.as_str(),
        "{status}"
    );
    assert_eq!(rig.service.baseline(), with_library.baseline);
    let second = rig.until("the session runs on the preparation of the edit", |state| {
        state.phase == "ready" && state.session_id != first.session_id
    });
    assert!(!alive(first.pid));
    assert!(rig.launches()[1].contains(&format!("--baseline {}", with_library.baseline)));
    // Nothing is required any more, and no job was made for it.
    let plain = rig.call("status").unwrap();
    assert!(plain.get("preparationRequired").is_none() && plain.get("preparation").is_none());
    assert!(rig.jobs().is_empty());

    // The edit is undone: the preparation of the edit rejects the sources,
    // and the one the session started on fits them again.
    rig.undo_asmdef();
    rig.forget_event();
    let text = format!(
        "managed assembly configuration/GUID changed; regenerate source graph: {}",
        join(&[&rig.project, ASMDEF])
    );
    rig.emit(json!({"event": "update-rejected", "error": text, "preparationRequired": true}));
    rig.until("the rejection is read", |state| {
        state.last_update_rejected_sequence > 0
    });
    let waited = rig.call("wait_update").unwrap();
    assert!(
        waited["status"] == "result_pending"
            && waited["preparation"]["requestId"] == "initial"
            && waited["message"]
                .as_str()
                .is_some_and(|text| text.contains("restarting on it")),
        "{waited}"
    );
    assert_eq!(rig.service.baseline(), initial.baseline);
    rig.until("the session runs on its first preparation again", |state| {
        state.phase == "ready" && state.session_id != second.session_id
    });
    assert_eq!(rig.launches().len(), 3);
    assert!(rig.jobs().is_empty());
}

/// A preparation that fits and whose files are gone is no preparation: the
/// Player is left alone, the caller is told, and the next call requests a
/// new one instead of finding the same receipt again.
#[test]
fn a_preparation_whose_files_are_gone_is_replaced_by_a_new_one() {
    let (rig, initial) = rig("plain");
    let first = rig.controller.status();
    rig.save_asmdef();
    let (known, _, player) = preparation_of(&rig.project, "with-library", &[ASMDEF]);
    fs::remove_file(&player).unwrap();
    rig.reject(false);
    let status = rig.call("status").unwrap();
    let text = status["preparationError"].as_str().unwrap_or_default();
    assert!(
        status["phase"] == "ready"
            && status["preparationRequired"] == true
            && text.contains(&format!(
                "the preparation {} that fits these sources could not be activated",
                known.request_id
            )),
        "{status}"
    );
    let held = rig.service.0.mu.lock().unwrap();
    let requested = rig.call("status").unwrap();
    assert!(
        requested["preparation"]["requestId"]
            == identity(&rig.project, &initial.baseline, 0).as_str()
            && requested["preparation"]["status"] == "result_pending"
            && requested.get("preparationError").is_none(),
        "{requested}"
    );
    let after = rig.controller.status();
    assert!(after.session_id == first.session_id && alive(first.pid));
    assert_eq!(rig.launches().len(), 1);
    assert_eq!(rig.service.baseline(), initial.baseline);
    rig.service.close_action_jobs();
    drop(held);
}

/// The runner marks a rejection as not its last word: it is already working
/// on a later save, here the one that takes the edit back. No wait is
/// answered on such a state, and no preparation is queued on it either.
#[test]
fn no_preparation_is_queued_on_a_rejection_the_runner_marked_as_pending() {
    let (rig, initial) = rig("plain");
    rig.save_asmdef();
    rig.undo_asmdef();
    let state = rig.reject(true);
    assert!(state.update_pending && state.preparation_required);
    let status = rig.call("status").unwrap();
    assert!(
        status["updatePending"] == true
            && status.get("preparation").is_none()
            && status.get("preparationReason").is_none(),
        "{status}"
    );
    let waited = rig.call_within(
        &ctx(),
        json!({"action": "wait_update", "afterSequence": state.last_sequence, "timeoutSeconds": 1}),
    );
    assert_eq!(code(&waited), "wait_timeout", "{waited:?}");
    assert!(rig.jobs().is_empty());
    assert_eq!(rig.service.baseline(), initial.baseline);
}

/// The recovery restarts the session inside the call that found the
/// preparation. A caller that gives up meanwhile (a client that cancels a
/// status it expected at once) must not end the restart halfway, with the
/// old Player stopped and nothing started.
#[test]
fn a_restart_that_has_begun_is_finished_without_its_caller() {
    let (rig, _) = rig("plain");
    let first = rig.controller.status();
    rig.save_asmdef();
    let (with_library, _, _) = preparation_of(&rig.project, "with-library", &[ASMDEF]);
    rig.reject(false);
    rig.forget_event();
    // The caller is gone before the call begins.
    let gone = ctx().with_cancel();
    gone.cancel();
    let _ = rig.call_within(&gone, json!({"action": "status"}));
    let after = rig.until("the session was started on the preparation", |state| {
        state.phase == "ready" && state.session_id != first.session_id
    });
    assert!(after.pid != 0 && !alive(first.pid));
    assert_eq!(rig.service.baseline(), with_library.baseline);
    assert_eq!(rig.launches().len(), 2);
}

/// While a preparation runs, the session is started by the job when it
/// ends. A wait says so and is repeated; a start comes back with the stage
/// instead of waiting for the lock the job holds; and a wait that was held
/// while the job replaced the Player is told to ask again, not that the
/// session stopped.
#[test]
fn calls_during_a_preparation_are_told_to_ask_again() {
    let (rig, initial) = rig("sync-held");
    // The job, as far as its lock and its progress go.
    let held = rig.service.0.mu.lock().unwrap();
    rig.service
        .progress_begin("project_prepare", "config-running");
    rig.service.stage("compile_scripts", "", "");
    // A start does not queue behind the job.
    let began = Instant::now();
    let start = rig.call("start").unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(
        start["status"] == "result_pending"
            && start["preparation"]["stage"] == "compile_scripts"
            && start["message"]
                .as_str()
                .is_some_and(|text| text.contains("the session starts on it by itself")),
        "{start}"
    );
    // A wait is held by the runner, which has not answered yet, and the
    // job replaces the Player meanwhile: its stop is what the wait sees.
    let waiting = {
        let (service, raw) = (
            rig.service.clone(),
            bytes(&json!({"action": "wait_update"})),
        );
        thread::spawn(move || {
            service
                .runtime(&ctx(), "session_control", &raw)
                .map(|value| decoded(&value))
        })
    };
    thread::sleep(Duration::from_millis(200));
    assert!(!waiting.is_finished());
    rig.service
        .0
        .swapping
        .store(true, std::sync::atomic::Ordering::SeqCst);
    rig.controller.stop(&ctx()).unwrap();
    let swapped = waiting.join().unwrap().unwrap();
    assert!(
        swapped["status"] == "result_pending"
            && swapped["phase"] == "preparing_configuration"
            && swapped["message"]
                .as_str()
                .is_some_and(|text| text.contains("reissue this call")),
        "{swapped}"
    );
    // The session is stopped by the job while it goes on: a wait says who
    // starts it.
    let stopped = rig.call("wait").unwrap();
    assert!(
        stopped["status"] == "result_pending"
            && stopped["phase"] == "preparing"
            && stopped["message"]
                .as_str()
                .is_some_and(|text| text.contains("the session starts on it by itself")
                    && !text.contains("session_control(start)")),
        "{stopped}"
    );
    // A session that was stopped otherwise stays stopped: the wait names
    // the call that starts it (stage 5 recovery5-6), and so does a start.
    rig.service
        .0
        .swapping
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let stopped = rig.call("wait").unwrap();
    assert!(
        stopped["status"] == "result_pending"
            && stopped["message"]
                .as_str()
                .is_some_and(|text| text.contains("nothing starts it by itself")
                    && text.contains("session_control(start)")),
        "{stopped}"
    );
    let start = rig.call("start").unwrap();
    assert!(
        start["message"]
            .as_str()
            .is_some_and(|text| text.contains("does not start a stopped session")),
        "{start}"
    );
    rig.service.progress_end();
    assert_eq!(rig.service.baseline(), initial.baseline);
    drop(held);
}

/// What the job of a recovery asks before it replaces the Player: whether
/// its request still names the sources on disk. A request of sources that
/// were changed again answers a rejection the session no longer has.
#[test]
fn a_recovery_request_is_current_while_the_sources_are_what_it_names() {
    let (rig, initial) = rig("plain");
    rig.save_asmdef();
    let first = identity(&rig.project, &initial.baseline, 0);
    let third = identity(&rig.project, &initial.baseline, 2);
    assert!(first != third && first.starts_with("config-") && first.len() == 47);
    assert!(rig.service.recovery_current(&first, SCENE));
    assert!(rig.service.recovery_current(&third, SCENE));
    rig.undo_asmdef();
    assert!(!rig.service.recovery_current(&first, SCENE));
    assert!(!rig.service.recovery_current(&third, SCENE));
    // The same sources again: the same names.
    rig.save_asmdef();
    assert!(rig.service.recovery_current(&first, SCENE));
}

// ---------------------------------------------------------------------------
// Stage 5 (recovery5-1, -2, -3, -4).
// ---------------------------------------------------------------------------

/// A new script in an embedded package belongs to a prepared assembly that a
/// session cannot extend: a preparation made without it does not fit. A
/// sample folder (`Samples~`) and the folder of another definition are not
/// that assembly.
#[test]
fn a_new_script_in_an_embedded_package_needs_a_new_preparation() {
    let (_guard, project) = review_project();
    let package = "Packages/com.r5.core";
    write(
        &join(&[&project, package, "package.json"]),
        r#"{"name":"com.r5.core","version":"1.0.0"}"#,
    );
    write(
        &join(&[&project, package, "Runtime/R5Core.asmdef"]),
        r#"{"name":"R5Core"}"#,
    );
    write(
        &join(&[&project, package, "Runtime/CoreMarker.cs"]),
        "namespace R5Core { public static class CoreMarker {} }\n",
    );
    // Neither a sample nor a folder of another definition is compiled into R5Core.
    write(
        &join(&[&project, package, "Samples~/Demo.cs"]),
        "public class Demo {}\n",
    );
    write(
        &join(&[&project, package, "Runtime/Tests/R5Tests.asmdef"]),
        r#"{"name":"R5Tests"}"#,
    );
    write(
        &join(&[&project, package, "Runtime/Tests/CoreTest.cs"]),
        "public class CoreTest {}\n",
    );
    // A source graph records every definition of a package, the nested one too.
    let inputs = [
        format!("{package}/package.json"),
        format!("{package}/Runtime/R5Core.asmdef"),
        format!("{package}/Runtime/CoreMarker.cs"),
        format!("{package}/Runtime/Tests/R5Tests.asmdef"),
        format!("{package}/Runtime/Tests/CoreTest.cs"),
    ];
    let inputs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let (made, _, _) = preparation_of(&project, "before-the-probe", &inputs);
    crate::compiler::Compiler::graph_accepts_sources(&project, &made.baseline).unwrap();
    write(
        &join(&[&project, package, "Runtime/R5Probe.cs"]),
        "namespace R5Core { public static class R5Probe {} }\n",
    );
    write(
        &join(&[&project, package, "Runtime/R5Probe.cs.meta"]),
        "fileFormatVersion: 2\nguid: 5e5c0e00000000000000000000000005\nMonoImporter:\n  serializedVersion: 2\n",
    );
    let in_package = crate::compiler::Compiler::graph_accepts_sources(&project, &made.baseline)
        .expect_err("a preparation made without the package's new script was taken as fitting");
    let text = format!("{in_package:#}");
    assert!(
        text.contains("Runtime/R5Probe.cs") && crate::compiler::requires_new_graph(&text),
        "{text}"
    );
}

/// A new assembly definition below the root of a package's assembly, and a
/// new embedded package with one, are new Player code that a preparation
/// made before them does not hold (session6-3). One for the Editor alone is
/// no part of a Player.
#[test]
fn a_new_assembly_in_a_package_needs_a_new_preparation() {
    for case in ["nested", "package", "editor"] {
        let (_guard, project) = review_project();
        let package = "Packages/com.r5.core";
        write(
            &join(&[&project, package, "package.json"]),
            r#"{"name":"com.r5.core","version":"1.0.0"}"#,
        );
        write(
            &join(&[&project, package, "Runtime/R5Core.asmdef"]),
            r#"{"name":"R5Core"}"#,
        );
        write(
            &join(&[&project, package, "Runtime/CoreMarker.cs"]),
            "namespace R5Core { public static class CoreMarker {} }\n",
        );
        let inputs = [
            format!("{package}/package.json"),
            format!("{package}/Runtime/R5Core.asmdef"),
            format!("{package}/Runtime/CoreMarker.cs"),
        ];
        let inputs: Vec<&str> = inputs.iter().map(String::as_str).collect();
        let (made, _, _) = preparation_of(&project, &format!("before-{case}"), &inputs);
        crate::compiler::Compiler::graph_accepts_sources(&project, &made.baseline).unwrap();
        let folder = match case {
            "nested" => format!("{package}/Runtime/Gameplay"),
            "editor" => format!("{package}/Editor"),
            _ => "Packages/com.r5.extra/Runtime".to_string(),
        };
        if case == "package" {
            write(
                &join(&[&project, "Packages/com.r5.extra/package.json"]),
                r#"{"name":"com.r5.extra","version":"1.0.0"}"#,
            );
        }
        let definition = match case {
            "editor" => r#"{"name":"R5Tools","includePlatforms":["Editor"]}"#,
            _ => r#"{"name":"R5Gameplay"}"#,
        };
        write(&join(&[&project, &folder, "R5Gameplay.asmdef"]), definition);
        write(
            &join(&[&project, &folder, "Boss.cs"]),
            "namespace R5Gameplay { public class Boss : UnityEngine.MonoBehaviour {} }\n",
        );
        let result = crate::compiler::Compiler::graph_accepts_sources(&project, &made.baseline);
        match case {
            "editor" => assert!(result.is_ok(), "{case}: {result:?}"),
            _ => {
                let text = format!("{:#}", result.expect_err(case));
                assert!(
                    text.contains("R5Gameplay.asmdef")
                        && crate::compiler::requires_new_graph(&text),
                    "{case}: {text}"
                );
            }
        }
    }
}

/// A C# save or a scene save while the job of a recovery runs: the request
/// still names the sources, so the job activates its preparation and no
/// second one is queued.
#[test]
fn a_save_during_the_preparation_of_a_recovery_keeps_it() {
    let (rig, initial) = rig("plain");
    rig.save_asmdef();
    let id = identity(&rig.project, &initial.baseline, 0);
    assert!(rig.service.recovery_current(&id, SCENE));
    write(
        &join(&[&rig.project, "Assets/Sample.cs"]),
        "public class Sample { public int value = 3; }\n",
    );
    write(
        &join(&[&rig.project, SCENE]),
        "selected scene, one object moved",
    );
    assert!(
        rig.service.recovery_current(&id, SCENE),
        "a content save voided the request of the recovery"
    );
    rig.reject(false);
    let held = rig.service.0.mu.lock().unwrap();
    let first = rig.call("status").unwrap();
    assert_eq!(first["preparation"]["requestId"], id.as_str(), "{first}");
    write(
        &join(&[&rig.project, "Assets/Sample.cs"]),
        "public class Sample { public int value = 4; }\n",
    );
    rig.reject(false);
    let again = rig.call("status").unwrap();
    assert_eq!(again["preparation"]["requestId"], id.as_str(), "{again}");
    let jobs = rig.jobs();
    rig.service.close_action_jobs();
    drop(held);
    assert_eq!(
        jobs.len(),
        1,
        "a second preparation was queued for a C# save: {jobs:?}"
    );
}

/// A failed preparation of a recovery answers the sources it was made for,
/// with the call that tries again; an edit of the sources since is tried
/// anew by itself.
#[test]
fn a_failed_recovery_names_the_retry_and_an_edit_retries() {
    let (rig, initial) = rig("plain");
    rig.save_asmdef();
    let id = identity(&rig.project, &initial.baseline, 0);
    let earlier = plain_service(&rig.project);
    let arguments = marshal(&Any::Object(obj([
        ("scene", SCENE.into()),
        ("requestId", id.clone().into()),
        ("activate", true.into()),
    ])))
    .unwrap();
    let _ = wait_job(
        &earlier,
        "project_prepare",
        &arguments,
        &run(|_, _, _| {
            Err(fail(
                "project_action_failed",
                "owned Editor did not exit after bounded cleanup",
            ))
        }),
    );
    earlier.close();
    let selection = ConfigurationSelection {
        scene: SCENE.into(),
        baseline: initial.baseline.clone(),
    };
    rig.service.note_recovery_sources(
        &id,
        &configuration::sources_revision(&rig.project, &[selection]).unwrap(),
    );
    rig.reject(false);
    let status = rig.call("status").unwrap();
    let error = status["preparationError"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("owned Editor did not exit")
            && error.contains(
                "call project_prepare with this scene, a new requestId and activate true"
            ),
        "{status}"
    );
    // The fix of the sources: a new attempt under the next name.
    write(
        &join(&[&rig.project, "Assets/Sample.cs"]),
        "public class Sample { public int value = 5; }\n",
    );
    rig.reject(false);
    let held = rig.service.0.mu.lock().unwrap();
    let retried = rig.call("status").unwrap();
    rig.service.close_action_jobs();
    drop(held);
    assert_eq!(
        retried["preparation"]["requestId"],
        identity(&rig.project, &initial.baseline, 1).as_str(),
        "{retried}"
    );
}

/// Eight preparations of another scene that are newer do not hide the one
/// of this scene that fits.
#[test]
fn newer_preparations_of_another_scene_do_not_hide_one_that_fits() {
    let (rig, _) = rig("plain");
    rig.save_asmdef();
    let (fitting, _, _) = preparation_of(&rig.project, "fits-oldest", &[ASMDEF]);
    for n in 0..9 {
        thread::sleep(Duration::from_millis(20));
        let directory = join(&[
            &rig.project,
            "Library/UnityMicroEditor/preparations",
            &format!("other-scene-{n}"),
        ]);
        let other = Preparation {
            schema_version: 1,
            project: rig.project.clone(),
            request_id: format!("other-scene-{n}"),
            scene: "Assets/Other.unity".into(),
            baseline: join(&[&directory, "current.json"]),
            ..Default::default()
        };
        write(
            &join(&[&directory, "result.json"]),
            serde_json::to_vec(&other).unwrap(),
        );
    }
    rig.reject(false);
    let held = rig.service.0.mu.lock().unwrap();
    let status = rig.call("status").unwrap();
    rig.service.close_action_jobs();
    drop(held);
    assert_eq!(
        status["preparation"]["requestId"],
        fitting.request_id.as_str(),
        "a new preparation was requested: {status}"
    );
}

/// On a real prepared project (UME_FIT_PROJECT, UME_FIT_BASELINE, a
/// baseline whose plan has Assembly-CSharp): a new script under Assets goes
/// into that assembly, so the preparation still fits. Ignored unless asked
/// for; it writes and removes Assets/FitProbe.cs.
#[test]
#[ignore]
fn a_new_script_under_assets_fits_a_real_preparation() {
    let project = std::env::var("UME_FIT_PROJECT").unwrap();
    let baseline = std::env::var("UME_FIT_BASELINE").unwrap();
    crate::compiler::Compiler::graph_accepts_sources(&project, &baseline).unwrap();
    let script = join(&[&project, "Assets/FitProbe.cs"]);
    write(&script, "public static class FitProbe {}\n");
    write(
        &format!("{script}.meta"),
        "fileFormatVersion: 2\nguid: 5e5c0e00000000000000000000000007\nMonoImporter:\n  serializedVersion: 2\n",
    );
    let fit = crate::compiler::Compiler::graph_accepts_sources(&project, &baseline);
    let _ = fs::remove_file(&script);
    let _ = fs::remove_file(format!("{script}.meta"));
    assert!(
        fit.is_ok(),
        "{:?}",
        fit.map_err(|error| format!("{error:#}"))
    );
}
