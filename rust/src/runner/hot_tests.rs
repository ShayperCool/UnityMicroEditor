//! Tests of the watch loop's decisions with scripted steps in place of the
//! compiler, the importer and the Player, of the events hot reload adds, of
//! the flags of `run` and of the command reader.
use super::{
    commands::{Command, Commands, parse_command, read_commands, rejected_event},
    window::{
        Cause, Compiled, LIST_MAXIMUM, Loop, Restarted, Scene, Steps, Window, patched_event,
        refusal_event, rejected_event as update_rejected, restart_event,
    },
    *,
};
use crate::{
    compiler::NilVec,
    hotreload::{Failure, Fallback, Outcome, Patched, Reason, Refusal, Unchanged},
};
use anyhow::anyhow;
use serde_json::{Value as Json, json};
use std::{collections::VecDeque, fs, io::Write, sync::Arc, time::Duration};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}
fn event_json(event: &Event) -> Json {
    serde_json::from_str(&event.line()).unwrap()
}
fn window(paths: &[&str]) -> Window {
    Window {
        paths: strings(paths),
        changed_at: Instant::now(),
        changed_wall: SystemTime::now(),
        recompile: None,
    }
}
fn recompile(paths: &[&str], id: &str) -> Window {
    Window {
        recompile: Some(id.into()),
        ..window(paths)
    }
}
fn refusal(codes: &[&str]) -> Refusal {
    Refusal {
        reasons: codes
            .iter()
            .map(|code| {
                Reason::new(
                    code,
                    &format!("Game.{code}"),
                    &format!("{code} is not supported."),
                )
            })
            .collect(),
        report: "/out/runs/r/hot-reload/1/report.json".into(),
    }
}
fn patch(generation: u64) -> Patched {
    Patched {
        generation,
        assemblies: strings(&["Assembly-CSharp"]),
        patched_methods: 1,
        members: strings(&["Game.A::M()"]),
        report: format!("/out/runs/r/hot-reload/{generation}/report.json"),
        ..Patched::default()
    }
}

// Flags

#[test]
fn hot_reload_flags() {
    let parse = |args: &[&str]| launch_options(&strings(args));
    let options = |args: &[&str]| parse(args).unwrap();
    // Hot reload belongs to a watched session and is on by default there.
    let plain = options(&[]);
    assert!(!plain.hot_reload && !plain.commands_stdin);
    assert_eq!(plain.hot_reload_fallback, Fallback::Restart);
    assert!(!options(&["--interactive"]).hot_reload);
    let watched = options(&["--watch"]);
    assert!(watched.hot_reload && watched.interactive);
    assert_eq!(watched.hot_reload_fallback, Fallback::Restart);
    assert!(!options(&["--watch", "--no-hot-reload"]).hot_reload);
    assert!(!options(&["--watch", "--prepare-only"]).hot_reload);
    let report = options(&[
        "--watch",
        "--hot-reload-fallback",
        "report",
        "--commands-stdin",
    ]);
    assert!(report.hot_reload && report.commands_stdin);
    assert_eq!(report.hot_reload_fallback, Fallback::Report);
    assert_eq!(
        options(&["--watch", "-hot-reload-fallback=restart"]).hot_reload_fallback,
        Fallback::Restart
    );
    for (args, expected) in [
        (
            &["--watch", "--hot-reload-fallback", "keep"][..],
            "--hot-reload-fallback must be restart or report",
        ),
        (
            &["--commands-stdin"],
            "--commands-stdin and --hot-reload-fallback require --watch",
        ),
        (
            &["--interactive", "--hot-reload-fallback", "report"],
            "--commands-stdin and --hot-reload-fallback require --watch",
        ),
        (
            &[
                "--watch",
                "--no-hot-reload",
                "--hot-reload-fallback",
                "restart",
            ],
            "--hot-reload-fallback has no effect with --no-hot-reload",
        ),
        // A reported edit waits for a command that has no way in.
        (
            &["--watch", "--hot-reload-fallback", "report"],
            "--hot-reload-fallback report requires --commands-stdin",
        ),
        (
            &["--watch", "--hot-reload"],
            "flag provided but not defined: -hot-reload",
        ),
    ] {
        assert_eq!(parse(args).unwrap_err().to_string(), expected, "{args:?}");
    }
    // The two command lines of the MCP controller.
    let controller = options(&[
        "--project",
        "/p",
        "--output",
        "/p/Temp/o",
        "--watch",
        "--control",
        "--fps",
        "30",
        "--baseline",
        "/p/b.json",
        "--editor",
        "/u/Unity",
        "--scene",
        "Assets/A.unity",
        "--content-scene",
        "0123456789abcdef0123456789abcdef=Assets/B.unity",
        "--content-prefab",
        "Assets/P.prefab",
        "--no-addressables",
        "--hot-reload-fallback",
        "report",
        "--commands-stdin",
        "--",
        "-game",
        "--watch",
    ]);
    assert!(controller.hot_reload && controller.commands_stdin && controller.control);
    assert_eq!(controller.hot_reload_fallback, Fallback::Report);
    assert_eq!(controller.game_arguments, ["-game", "--watch"]);
    let disabled = options(&[
        "--watch",
        "--control",
        "--no-hot-reload",
        "--commands-stdin",
    ]);
    assert!(!disabled.hot_reload && disabled.commands_stdin);
    // The usage text a flag error prints names the three flags.
    let usage = run_flags().usage();
    for flag in [
        "-no-hot-reload",
        "-hot-reload-fallback string",
        "-commands-stdin",
    ] {
        assert!(usage.contains(&format!("  {flag}\n")), "{flag}: {usage}");
    }
    assert!(usage.contains("(default \"restart\")"), "{usage}");
}

// Events

#[test]
fn code_patched_event_shape() {
    let mut applied = patch(3);
    applied.added_methods = 2;
    applied.added_fields = 1;
    applied.restored_methods = 4;
    applied.members = (0..20).map(|index| format!("Game.A::M{index}()")).collect();
    applied.notes = (0..18)
        .map(|index| {
            Reason::new(
                "existing_instances_keep_default",
                &format!("Game.A::f{index}"),
                "m",
            )
        })
        .collect();
    // The edit of this patch is about the two notes that sort last: with
    // the notes in the builder's order alone the limit would cut them.
    applied.notes.push(Reason {
        revision: "0011223344556677".into(),
        ..Reason::new("running_coroutines_keep_old_code", "Game.Z::Loop()", "old")
    });
    let fresh = applied.notes[17..].to_vec();
    let mut changed = window(&["/p/Assets/A.cs"]);
    changed.changed_at = Instant::now() - Duration::from_millis(1500);
    let event = event_json(&patched_event(
        &applied,
        &fresh,
        &changed,
        4242,
        "0123456789abcdef",
        1.25,
        0.5,
    ));
    let object = event.as_object().unwrap();
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "addedFields",
            "addedMethods",
            "assemblies",
            "codeGeneration",
            "compileSeconds",
            "event",
            "members",
            "newNotes",
            "newNotesCount",
            "notes",
            "notesCount",
            "patchSeconds",
            "patchedMethods",
            "paths",
            "pid",
            "report",
            "restoredMethods",
            "revision",
            "sourceToAckSeconds",
        ]
    );
    assert_eq!(event["event"], "code-patched");
    // Exact integers: the controller refuses `1.0` and `"1"`.
    let line = patched_event(
        &applied,
        &fresh,
        &changed,
        4242,
        "0123456789abcdef",
        1.25,
        0.5,
    )
    .line();
    for exact in [
        "\"codeGeneration\":3,",
        "\"pid\":4242,",
        "\"notesCount\":19,",
        "\"newNotesCount\":2,",
        "\"patchedMethods\":1,",
    ] {
        assert!(line.contains(exact), "{exact}: {line}");
    }
    assert_eq!(event["revision"], "0123456789abcdef");
    assert_eq!(event["paths"], json!(["/p/Assets/A.cs"]));
    assert_eq!(event["assemblies"], json!(["Assembly-CSharp"]));
    assert_eq!(
        (
            &event["addedMethods"],
            &event["addedFields"],
            &event["restoredMethods"]
        ),
        (&json!(2), &json!(1), &json!(4))
    );
    // Lists are cut at 16; the count and the report give the rest. The
    // notes this patch brought are listed on their own and lead the list of
    // all notes, so the cut takes what stood before; an event carries no
    // revision.
    assert_eq!(event["members"].as_array().unwrap().len(), LIST_MAXIMUM);
    assert_eq!(event["notes"].as_array().unwrap().len(), LIST_MAXIMUM);
    let brought = json!([
        {"code":"existing_instances_keep_default","member":"Game.A::f17","message":"m"},
        {"code":"running_coroutines_keep_old_code","member":"Game.Z::Loop()","message":"old"},
    ]);
    assert_eq!(event["newNotes"], brought);
    assert_eq!(
        (&event["notes"][0], &event["notes"][1]),
        (&brought[0], &brought[1])
    );
    assert_eq!(
        event["notes"][2],
        json!({"code":"existing_instances_keep_default","member":"Game.A::f0","message":"m"})
    );
    assert_eq!(event["report"], "/out/runs/r/hot-reload/3/report.json");
    assert_eq!(
        (&event["compileSeconds"], &event["patchSeconds"]),
        (&json!(1.25), &json!(0.5))
    );
    let total = event["sourceToAckSeconds"].as_f64().unwrap();
    assert!((1.5..10.0).contains(&total), "{total}");
}

#[test]
fn rejection_event_shapes() {
    let refused = refusal(&(0..20).map(|_| "type_added").collect::<Vec<_>>());
    let line = refusal_event(&refused, &strings(&["/p/Assets/A.cs"])).line();
    let event: Json = serde_json::from_str(&line).unwrap();
    assert_eq!(event["event"], "update-rejected");
    // Exactly the boolean, which is what makes it a refusal for a reader.
    assert!(line.contains("\"fullRecompileRequired\":true,"), "{line}");
    assert_eq!(event["unsupported"].as_array().unwrap().len(), LIST_MAXIMUM);
    assert_eq!(event["unsupportedCount"], 20);
    assert_eq!(event["report"], refused.report);
    assert_eq!(event["paths"], json!(["/p/Assets/A.cs"]));
    assert_eq!(
        event["error"],
        "hot reload cannot apply the C# edit to the running Player (20 reasons); first: [type_added] Game.type_added: type_added is not supported"
    );
    assert!(event.get("preparationRequired").is_none() && event.get("requestId").is_none());
    assert!(!compiler::requires_new_graph(
        event["error"].as_str().unwrap()
    ));
    // The output of a failed builder stays out of the event line.
    let long = Refusal {
        reasons: vec![Reason::new("patch_builder_failed", "", &"é".repeat(4000))],
        report: String::new(),
    };
    let event = event_json(&refusal_event(&long, &[]));
    assert_eq!(
        event["unsupported"][0]["message"].as_str().unwrap().len(),
        1024
    );
    assert!(refusal_event(&long, &[]).line().len() < 2048);

    // A step that failed: the compiler's text, and what a controller acts on.
    let plain = event_json(&update_rejected(
        &anyhow!("verify lifecycle A: error CS1026"),
        &[],
        None,
    ));
    assert_eq!(
        plain,
        json!({"event":"update-rejected","error":"verify lifecycle A: error CS1026","paths":[]})
    );
    let graph = event_json(&update_rejected(
        &anyhow!(
            "managed assembly configuration/GUID changed; regenerate source graph: /p/a.asmdef"
        ),
        &strings(&["/p/a.asmdef"]),
        None,
    ));
    assert_eq!(graph["preparationRequired"], true);
    // The answer to a recompile whose check failed names the command and
    // never is a refusal.
    let answer = event_json(&update_rejected(&anyhow!("error CS1002"), &[], Some("r-7")));
    assert_eq!(answer["requestId"], "r-7");
    assert!(answer.get("fullRecompileRequired").is_none());
}

#[test]
fn restart_event_shapes() {
    let base = || {
        Event::new("content-updated")
            .with("paths", &strings(&["/p/Assets/A.cs"]))
            .with("manifest", "/out/revisions/r2/scene.json")
            .with("contentRoutes", "")
            .seconds("sourceToReadySeconds", 3.5)
            .with("pid", &7)
            .with("report", "/out/runs/n/player-report.json")
    };
    let compile = || CompileUpdate {
        compiled_assemblies: NilVec::from(["Assembly-CSharp".to_string()]),
        elapsed_seconds: 2.into(),
        ..CompileUpdate::default()
    };
    let restarted = |compile: Option<CompileUpdate>| Restarted {
        event: base(),
        compile,
    };
    // As before hot reload: content only, and a compile a new process loaded.
    let content = event_json(&restart_event(restarted(None), None, None));
    assert_eq!(content["event"], "content-updated");
    assert!(content.get("reason").is_none() && content.get("compiledAssemblies").is_none());
    let code = event_json(&restart_event(
        restarted(Some(compile())),
        Some(Cause::Plain),
        None,
    ));
    assert_eq!(code["event"], "code-updated");
    assert_eq!(code["compiledAssemblies"], json!(["Assembly-CSharp"]));
    assert_eq!(code["compileSeconds"], 2);
    assert_eq!(code["declarationChanged"], Json::Null);
    assert!(code.get("reason").is_none() && code.get("hotReloadReport").is_none());

    let refused = refusal(&(0..17).map(|_| "generic_method").collect::<Vec<_>>());
    let fallback = event_json(&restart_event(
        restarted(Some(compile())),
        Some(Cause::Unsupported(refused.clone())),
        None,
    ));
    assert_eq!(fallback["event"], "code-updated");
    assert_eq!(fallback["reason"], "hot_reload_unsupported");
    assert_eq!(
        fallback["unsupported"].as_array().unwrap().len(),
        LIST_MAXIMUM
    );
    assert_eq!(fallback["unsupportedCount"], 17);
    // `report` stays the Player's report, which the controller validates.
    assert_eq!(fallback["report"], "/out/runs/n/player-report.json");
    assert_eq!(fallback["hotReloadReport"], refused.report);
    assert!(fallback.get("requestId").is_none() && fallback.get("error").is_none());

    let failed = event_json(&restart_event(
        restarted(Some(compile())),
        Some(Cause::Failed(Failure {
            error: "the Player exited while it applied the code patch".into(),
            report: "/out/runs/r/hot-reload/2/report.json".into(),
        })),
        None,
    ));
    assert_eq!(failed["reason"], "hot_reload_failed");
    assert_eq!(
        failed["error"],
        "the Player exited while it applied the code patch"
    );
    assert_eq!(
        failed["hotReloadReport"],
        "/out/runs/r/hot-reload/2/report.json"
    );
    assert_eq!(failed["report"], "/out/runs/n/player-report.json");

    // A requested restart is `code-updated` with the request, also without a
    // newer compile and also when hot reload had its own reason.
    let requested = event_json(&restart_event(restarted(None), None, Some("r-1")));
    assert_eq!(
        (
            &requested["event"],
            &requested["reason"],
            &requested["requestId"]
        ),
        (&json!("code-updated"), &json!("requested"), &json!("r-1"))
    );
    assert!(requested.get("compiledAssemblies").is_none());
    let merged = event_json(&restart_event(
        restarted(Some(compile())),
        Some(Cause::Unsupported(refused)),
        Some("r-2"),
    ));
    assert_eq!(
        (&merged["reason"], &merged["requestId"]),
        (&json!("requested"), &json!("r-2"))
    );
    assert!(merged.get("unsupported").is_none() && merged.get("hotReloadReport").is_none());
    assert_eq!(merged["compiledAssemblies"], json!(["Assembly-CSharp"]));
}

// The loop

/// What the scripted scene step does.
enum Reload {
    Unchanged,
    Updated,
    Rejected,
    Uncertain,
}

/// Scripted steps. They record the order of the calls and keep the two
/// facts the real ones depend on: whether a compile waits for a process,
/// and which assemblies the session's bindings describe.
struct Script {
    calls: Vec<String>,
    events: Vec<Json>,
    compiles: VecDeque<Result<Compiled, &'static str>>,
    patches: VecDeque<Result<Outcome, &'static str>>,
    routes: VecDeque<bool>,
    reloads: VecDeque<Reload>,
    invalidate_error: Option<&'static str>,
    bind_error: Option<&'static str>,
    prepare_error: Option<&'static str>,
    merged: Option<String>,
    pending: bool,
    /// Source changes arrived that the window did not take.
    queued: bool,
    /// How long every compile step takes.
    compile_time: Duration,
    /// How many steps had run when each event was printed.
    printed_after: Vec<usize>,
    /// The session's bindings describe the newest compile, not what the
    /// running Player loaded.
    bound_newest: bool,
    pid: i32,
    revision: u32,
    /// The script GUID of each type, as the session's script types have it.
    scripts: std::collections::BTreeMap<String, String>,
}
impl Script {
    fn new() -> Self {
        Self {
            calls: Vec::new(),
            events: Vec::new(),
            compiles: VecDeque::new(),
            patches: VecDeque::new(),
            routes: VecDeque::new(),
            reloads: VecDeque::new(),
            invalidate_error: None,
            bind_error: None,
            prepare_error: None,
            merged: None,
            pending: false,
            queued: false,
            compile_time: Duration::ZERO,
            printed_after: Vec::new(),
            bound_newest: false,
            pid: 100,
            revision: 1,
            scripts: Default::default(),
        }
    }
    fn compiled(mut self, compiled: Compiled) -> Self {
        self.compiles.push_back(Ok(compiled));
        self
    }
    fn updated(self) -> Self {
        self.compiled(Compiled::Updated)
    }
    fn patched(mut self, outcome: Outcome) -> Self {
        self.patches.push_back(Ok(outcome));
        self
    }
    fn reload(mut self, reload: Reload) -> Self {
        self.reloads.push_back(reload);
        self
    }
    fn routes_differ(mut self) -> Self {
        self.routes.push_back(false);
        self
    }
    /// The calls and the names of the events since the last take.
    fn take(&mut self) -> (Vec<String>, Vec<String>) {
        let names = self
            .events
            .iter()
            .map(|event| event["event"].as_str().unwrap().to_string())
            .collect();
        (std::mem::take(&mut self.calls), names)
    }
    fn clear(&mut self) -> Vec<Json> {
        self.calls.clear();
        self.printed_after.clear();
        std::mem::take(&mut self.events)
    }
}
impl Steps for Script {
    fn emit(&mut self, event: Event) {
        self.printed_after.push(self.calls.len());
        self.events.push(event_json(&event));
    }
    fn invalidate(&mut self, _: &[String]) -> Result<()> {
        match self.invalidate_error.take() {
            Some(error) => Err(anyhow!(error)),
            None => Ok(()),
        }
    }
    fn compile(&mut self, _: &Context, paths: Option<&[String]>) -> Result<Compiled> {
        self.calls.push(match paths {
            Some(_) => "compile".into(),
            None => "compile-all".into(),
        });
        std::thread::sleep(self.compile_time);
        let compiled = self
            .compiles
            .pop_front()
            .unwrap_or(Ok(Compiled::Unaffected))
            .map_err(|error| anyhow!(error))?;
        self.pending |= matches!(compiled, Compiled::Updated);
        Ok(compiled)
    }
    fn compile_pending(&self) -> bool {
        self.pending
    }
    fn patch_code(&mut self, _: &Context, _: &Window) -> Result<Outcome> {
        self.calls.push("patch".into());
        assert!(
            self.pending,
            "a patch needs a compile the Player has not loaded"
        );
        assert!(
            !self.bound_newest,
            "a patch is built against the loaded assemblies"
        );
        self.patches
            .pop_front()
            .expect("an unexpected code patch")
            .map_err(|error| anyhow!(error))
    }
    fn pid(&self) -> i32 {
        self.pid
    }
    fn revision(&self) -> String {
        format!("revision{}", self.revision)
    }
    fn bind_newest(&mut self, _: &Context) -> Result<()> {
        self.calls.push("bind".into());
        assert!(self.pending, "there is no newer compile to bind");
        // Script types are replaced before the step can fail.
        self.bound_newest = true;
        match self.bind_error.take() {
            Some(error) => Err(anyhow!(error)),
            None => Ok(()),
        }
    }
    fn prepare(&mut self, _: &Context, _: &Window, newest: bool, again: bool) -> Result<bool> {
        self.calls.push(format!(
            "prepare-{}{}",
            if newest { "newest" } else { "loaded" },
            if again { "-again" } else { "" }
        ));
        // The constraint of the Player's identity check: what is published
        // for the running Player must describe the assemblies it loaded.
        assert_eq!(
            newest && self.pending,
            self.bound_newest,
            "the bindings and the Player the revision is published for disagree"
        );
        if let Some(error) = self.prepare_error.take() {
            return Err(anyhow!(error));
        }
        Ok(self.routes.pop_front().unwrap_or(true))
    }
    fn restart(&mut self, _: &Context, window: &Window) -> Result<Restarted> {
        self.calls.push("restart".into());
        self.pid += 1;
        self.revision += 1;
        let compile = std::mem::take(&mut self.pending).then(|| CompileUpdate {
            compiled_assemblies: NilVec::from(["Assembly-CSharp".to_string()]),
            ..CompileUpdate::default()
        });
        self.bound_newest = false;
        Ok(Restarted {
            event: Event::new("content-updated")
                .with("paths", &window.paths)
                .with("pid", &self.pid)
                .with("report", "/out/runs/n/player-report.json"),
            compile,
        })
    }
    fn merged_recompile(&mut self) -> Option<String> {
        self.merged.take()
    }
    fn script_guid(&self, type_name: &str) -> Option<String> {
        self.scripts.get(type_name).cloned()
    }
    fn scene(&mut self, _: &Context, window: &Window, _: Instant) -> Result<Scene> {
        self.calls.push("scene".into());
        assert!(
            !self.bound_newest,
            "a scene reload would name assemblies the running Player did not load"
        );
        let event = |name: &str| {
            Event::new(name)
                .with("paths", &window.paths)
                .with("pid", &self.pid)
        };
        Ok(
            match self.reloads.pop_front().unwrap_or(Reload::Unchanged) {
                Reload::Unchanged => Scene::Unchanged(event("unchanged")),
                Reload::Updated => {
                    self.revision += 1;
                    Scene::Updated(event("updated"))
                }
                Reload::Rejected => Scene::Rejected {
                    event: event("update-rejected").with("error", "Player rejected revision: x"),
                    fatal: None,
                },
                Reload::Uncertain => Scene::Rejected {
                    event: event("update-rejected").with("error", "timed out"),
                    fatal: Some(anyhow!(
                        "live Player revision is uncertain or requires a restart"
                    )),
                },
            },
        )
    }
}

fn run(state: &mut Loop, steps: &mut Script, window: &Window) {
    let queued = steps.queued;
    state
        .window(&Context::background(), steps, window, &|| queued)
        .unwrap();
}
fn calls(items: &[&str]) -> Vec<String> {
    strings(items)
}

#[test]
fn without_hot_reload_every_compile_restarts() {
    let mut state = Loop::new(false, Fallback::Restart);
    let mut steps = Script::new().updated();
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "bind", "prepare-newest", "restart"]),
            calls(&["code-updated"])
        )
    );
    let event = &steps.events[0];
    assert_eq!(event["compiledAssemblies"], json!(["Assembly-CSharp"]));
    // Nothing of hot reload is said by a session that has none.
    assert!(event.get("reason").is_none());
    steps.clear();
    for reload in [Reload::Unchanged, Reload::Updated] {
        steps.reloads.push_back(reload);
        run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    }
    let events = steps.clear();
    assert_eq!(
        (&events[0]["event"], &events[1]["event"]),
        (&json!("unchanged"), &json!("updated"))
    );
    assert!(
        events
            .iter()
            .all(|event| event.get("fullRecompileRequired").is_none())
    );
    // A compile whose bindings could not be made still restarts later.
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps.bind_error = Some("read script types");
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(
        steps.take(),
        (calls(&["compile", "bind"]), calls(&["update-rejected"]))
    );
    steps.clear();
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile-all", "bind", "prepare-newest", "restart"]),
            calls(&["code-updated"])
        )
    );
}

#[test]
fn a_patched_edit_keeps_the_player_and_its_bindings() {
    let mut state = Loop::new(true, Fallback::Restart);
    let mut steps = Script::new().updated().patched(Outcome::Patched(patch(1)));
    steps.compile_time = Duration::from_millis(60);
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    // The patch comes before the scene part, which works on the loaded
    // bindings; a confirmation that nothing else changed is not printed.
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "patch", "prepare-loaded", "scene"]),
            calls(&["code-patched"])
        )
    );
    // It is printed when the window is done with the scene part as well: a
    // reader that acts on it finds no step of this window still running.
    assert_eq!(steps.printed_after, [4]);
    let event = steps.clear().remove(0);
    assert_eq!(
        (&event["pid"], &event["revision"]),
        (&json!(100), &json!("revision1"))
    );
    assert_eq!(event["codeGeneration"], 1);
    // The time of the whole compile step as the window waited for it, not
    // what the compiler says of its own run.
    let compile = event["compileSeconds"].as_f64().unwrap();
    assert!((0.06..5.0).contains(&compile), "{compile}");
    assert_eq!(event["paths"], json!(["/p/Assets/A.cs"]));
    // The last word of its window, and nothing is queued behind it.
    assert!(event.get("pending").is_none());
    assert!(state.restart().is_none() && state.refused().is_none() && steps.pending);

    // A later scene edit: same Player, loaded bindings, and the code is current.
    steps.reloads.push_back(Reload::Updated);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "prepare-loaded", "scene"]),
            calls(&["updated"])
        )
    );
    assert_eq!(steps.clear()[0]["fullRecompileRequired"], false);
    assert_eq!(steps.pid, 100);

    // Changed content routes restart the Player. The new process loads the
    // newest compile, so everything is prepared once more for it.
    steps.routes.push_back(false);
    run(&mut state, &mut steps, &window(&["/p/Assets/B.unity"]));
    assert_eq!(
        steps.take(),
        (
            calls(&[
                "compile",
                "prepare-loaded",
                "bind",
                "prepare-newest-again",
                "restart"
            ]),
            calls(&["code-updated"])
        )
    );
    let event = steps.clear().remove(0);
    assert_eq!(event["pid"], 101);
    assert!(event.get("reason").is_none());
    assert!(!steps.pending && state.restart().is_none());
    // Routes alone, with no newer compile: today's content restart.
    steps.routes.push_back(false);
    run(&mut state, &mut steps, &window(&["/p/Assets/B.unity"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "prepare-loaded", "restart"]),
            calls(&["content-updated"])
        )
    );
}

#[test]
fn a_mixed_window_patches_first_and_then_reloads_the_scene() {
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Patched(patch(1)))
        .reload(Reload::Updated);
    run(
        &mut state,
        &mut steps,
        &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"]),
    );
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "patch", "prepare-loaded", "scene"]),
            calls(&["code-patched", "updated"])
        )
    );
    // Both are printed when both parts are in the Player, and the first one
    // says that it is not the answer for this save.
    assert_eq!(steps.printed_after, [4, 4]);
    let events = steps.clear();
    assert_eq!(events[1]["fullRecompileRequired"], false);
    assert_eq!(
        (&events[0]["pid"], &events[1]["pid"]),
        (&json!(100), &json!(100))
    );
    assert_eq!(
        (&events[0]["revision"], events[0].get("pending")),
        (&json!("revision1"), Some(&json!(true)))
    );
    assert!(events[1].get("pending").is_none());
    // A scene part that fails is the last word of its window. The code part
    // was applied, and the rejection says that no refusal stands.
    for (reload, prepare_error, steps_run) in [
        (Reload::Rejected, None, 4),
        (Reload::Unchanged, Some("import failed"), 3),
    ] {
        steps.compiles.push_back(Ok(Compiled::Updated));
        steps.patches.push_back(Ok(Outcome::Patched(patch(2))));
        steps.reloads.push_back(reload);
        steps.prepare_error = prepare_error;
        run(
            &mut state,
            &mut steps,
            &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"]),
        );
        let (_, names) = steps.take();
        assert_eq!(names, calls(&["code-patched", "update-rejected"]));
        assert_eq!(steps.printed_after, [steps_run, steps_run]);
        let events = steps.clear();
        assert_eq!(events[0]["pending"], true);
        assert_eq!(events[1]["fullRecompileRequired"], false);
        assert!(events[1].get("pending").is_none() && events[1].get("unsupported").is_none());
        steps.reloads.clear();
    }
}

#[test]
fn an_event_is_pending_while_source_changes_are_queued() {
    // A save that arrived while the window worked is handled by the next
    // one. Whatever this window says is then not the answer for it, also
    // when a part of that save was already read by this window.
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new().updated().patched(Outcome::Patched(patch(1)));
    steps.queued = true;
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    let events = steps.clear();
    assert_eq!(events.len(), 1);
    assert_eq!(
        (&events[0]["event"], &events[0]["pending"]),
        (&json!("code-patched"), &json!(true))
    );
    // Every kind of answer: a scene update, a rejection, a compile error, a
    // refusal with its scene part, a restart.
    steps.reloads.push_back(Reload::Updated);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    steps.reloads.push_back(Reload::Rejected);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    steps.compiles.push_back(Err(
        "runtime source changed while snapshotting: /p/Assets/B.cs",
    ));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps
        .patches
        .push_back(Ok(Outcome::Unsupported(refusal(&["type_added"]))));
    steps.reloads.push_back(Reload::Updated);
    run(
        &mut state,
        &mut steps,
        &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"]),
    );
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    run(&mut state, &mut steps, &recompile(&[], "r-1"));
    let events = steps.clear();
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "updated",
            "update-rejected",
            "update-rejected",
            "updated",
            "update-rejected",
            "code-updated"
        ]
    );
    assert!(events.iter().all(|event| event["pending"] == true));
    // The command is answered by its restart all the same.
    assert_eq!(events[5]["requestId"], "r-1");
    // Nothing is queued any more: the next answer is the last word.
    steps.queued = false;
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    let event = steps.clear().remove(0);
    assert_eq!(event["event"], "unchanged");
    assert!(event.get("pending").is_none());
}

#[test]
fn notes_of_a_compile_that_needs_no_patch_are_stated_while_they_stand() {
    let noted = || {
        Outcome::Unchanged(Unchanged {
            notes: (0..18)
                .map(|index| {
                    Reason::new(
                        "accessibility_changed",
                        &format!("Game.A::f{index}"),
                        "Field changed accessibility.",
                    )
                })
                .collect(),
            report: "/out/runs/r/hot-reload/1/report.json".into(),
        })
    };
    let stated = |event: &Json| {
        assert_eq!(event["fullRecompileRequired"], false, "{event}");
        assert_eq!(event["notes"].as_array().unwrap().len(), LIST_MAXIMUM);
        assert_eq!(event["notes"][0]["code"], "accessibility_changed");
        assert_eq!(event["notes"][0]["member"], "Game.A::f0");
        assert_eq!(event["notesCount"], 18);
        assert_eq!(
            event["hotReloadReport"],
            "/out/runs/r/hot-reload/1/report.json"
        );
    };
    let plain = |event: &Json| {
        assert!(
            event.get("notes").is_none()
                && event.get("notesCount").is_none()
                && event.get("hotReloadReport").is_none(),
            "{event}"
        );
    };
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new().updated().patched(noted());
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    // Nothing was patched, so the confirmation of the window is printed,
    // and it carries what the builder said.
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "patch", "prepare-loaded", "scene"]),
            calls(&["unchanged"])
        )
    );
    let brought = steps.clear().remove(0);
    stated(&brought);
    // The same event says that they are new: this compile brought them.
    assert_eq!(
        (
            brought["newNotesCount"].clone(),
            &brought["newNotes"][0]["member"]
        ),
        (json!(18), &json!("Game.A::f0"))
    );
    assert_eq!(brought["newNotes"].as_array().unwrap().len(), LIST_MAXIMUM);
    // They stand for the Player's code, so later scene answers repeat them:
    // applied, unchanged, rejected in the Player or before it. Nothing is
    // new about them there.
    for (reload, prepare_error, name) in [
        (Reload::Updated, None, "updated"),
        (Reload::Unchanged, None, "unchanged"),
        (Reload::Rejected, None, "update-rejected"),
        (Reload::Unchanged, Some("import failed"), "update-rejected"),
    ] {
        steps.reloads.push_back(reload);
        steps.prepare_error = prepare_error;
        run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
        let event = steps.clear().remove(0);
        assert_eq!(event["event"], name);
        stated(&event);
        assert!(
            event.get("newNotes").is_none() && event.get("newNotesCount").is_none(),
            "{event}"
        );
        steps.reloads.clear();
    }
    // A compile error says nothing about the code, and takes nothing back.
    steps.compiles.push_back(Err("error CS1002: ; expected"));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    let event = steps.clear().remove(0);
    plain(&event);
    assert!(event.get("fullRecompileRequired").is_none());
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    stated(&steps.clear()[0]);

    // What ends them: a patch, a compile without notes, a refusal, a restart.
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps.patches.push_back(Ok(Outcome::Patched(patch(2))));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    // The patch has notes of its own, here none.
    let event = steps.clear().remove(0);
    assert_eq!(
        (&event["event"], &event["notesCount"]),
        (&json!("code-patched"), &json!(0))
    );
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    plain(&steps.clear()[0]);

    for ending in [
        Outcome::Unchanged(Unchanged::default()),
        Outcome::Unsupported(refusal(&["type_added"])),
    ] {
        steps.compiles.push_back(Ok(Compiled::Updated));
        steps.patches.push_back(Ok(noted()));
        run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
        stated(&steps.clear()[0]);
        steps.compiles.push_back(Ok(Compiled::Updated));
        steps.patches.push_back(Ok(ending));
        run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
        steps.clear().iter().for_each(plain);
        run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
        steps.clear().iter().for_each(plain);
    }
    // The refusal of the last round stands; a recompile ends it, and the
    // new process has neither a refusal nor notes.
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    run(&mut state, &mut steps, &recompile(&[], "r-1"));
    steps.clear();
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps.patches.push_back(Ok(noted()));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    stated(&steps.clear()[0]);
    steps.routes.push_back(false);
    run(&mut state, &mut steps, &window(&["/p/Assets/B.unity"]));
    assert_eq!(steps.take().1, calls(&["code-updated"]));
    plain(&steps.clear()[0]);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    let event = steps.clear().remove(0);
    plain(&event);
    assert_eq!(event["fullRecompileRequired"], false);

    // A session without hot reload says nothing of this.
    let mut state = Loop::new(false, Fallback::Restart);
    let mut steps = Script::new().reload(Reload::Rejected);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    let event = steps.clear().remove(0);
    plain(&event);
    assert!(event.get("fullRecompileRequired").is_none());
}

#[test]
fn fallback_restart_restarts_with_the_reasons() {
    for outcome in [
        Outcome::Unsupported(refusal(&["type_added", "generic_method"])),
        Outcome::ApplyFailed(refusal(&["apply_failed"])),
    ] {
        let count = match &outcome {
            Outcome::Unsupported(refused) | Outcome::ApplyFailed(refused) => refused.reasons.len(),
            _ => unreachable!(),
        };
        let mut state = Loop::new(true, Fallback::Restart);
        let mut steps = Script::new().updated().patched(outcome);
        run(
            &mut state,
            &mut steps,
            &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"]),
        );
        // One restart delivers code and scene; nothing is reloaded in between.
        assert_eq!(
            steps.take(),
            (
                calls(&["compile", "patch", "bind", "prepare-newest", "restart"]),
                calls(&["code-updated"])
            )
        );
        let event = steps.clear().remove(0);
        assert_eq!(event["reason"], "hot_reload_unsupported");
        assert_eq!(event["unsupportedCount"], count);
        assert_eq!(event["unsupported"].as_array().unwrap().len(), count);
        assert_eq!(
            event["hotReloadReport"],
            "/out/runs/r/hot-reload/1/report.json"
        );
        assert_eq!(event["pid"], 101);
        assert!(state.restart().is_none() && state.refused().is_none());
        // The new process runs the newest code.
        run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
        assert_eq!(steps.clear()[0]["fullRecompileRequired"], false);
    }
}

#[test]
fn fallback_restart_survives_a_window_that_fails_first() {
    let mut state = Loop::new(true, Fallback::Restart);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Unsupported(refusal(&["type_added"])));
    steps.prepare_error = Some("native import failed");
    run(
        &mut state,
        &mut steps,
        &window(&["/p/Assets/A.cs", "/p/Assets/M.fbx"]),
    );
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "patch", "bind", "prepare-newest"]),
            calls(&["update-rejected"])
        )
    );
    // A plain rejection: the Player keeps its code and nothing asks the
    // caller to recompile, because the next window does it.
    assert!(steps.clear()[0].get("fullRecompileRequired").is_none());
    assert!(matches!(state.restart(), Some(Cause::Unsupported(_))));
    // The session describes the newest assemblies now. Whatever comes next,
    // also another C# edit, restarts instead of patching or reloading.
    steps.compiles.push_back(Ok(Compiled::Updated));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "bind", "prepare-newest", "restart"]),
            calls(&["code-updated"])
        )
    );
    assert_eq!(steps.clear()[0]["reason"], "hot_reload_unsupported");
    assert!(state.restart().is_none());
}

#[test]
fn fallback_report_keeps_the_player_and_refuses_last() {
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Unsupported(refusal(&[
            "type_added",
            "enum_changed",
        ])));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    // One terminal event for a C# edit: the refusal.
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "patch", "prepare-loaded", "scene"]),
            calls(&["update-rejected"])
        )
    );
    let event = steps.clear().remove(0);
    assert_eq!(event["fullRecompileRequired"], true);
    assert_eq!(event["unsupportedCount"], 2);
    assert_eq!(event["unsupported"][0]["code"], "type_added");
    assert_eq!(event["report"], "/out/runs/r/hot-reload/1/report.json");
    assert!(
        event["error"]
            .as_str()
            .unwrap()
            .contains("(2 reasons); first: [type_added]")
    );
    assert!(state.refused().is_some() && state.restart().is_none() && steps.pending);
    assert_eq!(steps.pid, 100);

    // A later scene edit works on the same Player and does not clear the
    // refusal: its confirmation says that the code is still not there.
    steps.reloads.push_back(Reload::Updated);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "prepare-loaded", "scene"]),
            calls(&["updated"])
        )
    );
    assert_eq!(steps.clear()[0]["fullRecompileRequired"], true);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    let events = steps.clear();
    assert_eq!(
        (&events[0]["event"], &events[0]["fullRecompileRequired"]),
        (&json!("unchanged"), &json!(true))
    );
    assert!(state.refused().is_some());

    // A later scene edit that fails, before the Player or in it. The scene
    // may use a type only the refused code has, so its rejection is not
    // plain: it states the refusal with the reasons that stand, in the one
    // event a reader may stop at.
    for (reload, prepare_error, error) in [
        (Reload::Rejected, None, "Player rejected revision: x"),
        (
            Reload::Unchanged,
            Some("unresolved managed component type 0123 in Assets/S.unity"),
            "unresolved managed component type 0123 in Assets/S.unity",
        ),
    ] {
        steps.reloads.push_back(reload);
        steps.prepare_error = prepare_error;
        run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
        let events = steps.clear();
        assert_eq!(events.len(), 1, "{error}");
        let event = &events[0];
        assert_eq!(
            (&event["event"], &event["error"]),
            (&json!("update-rejected"), &json!(error))
        );
        assert_eq!(event["fullRecompileRequired"], true, "{error}");
        assert_eq!(event["unsupportedCount"], 2, "{error}");
        assert_eq!(event["unsupported"][0]["code"], "type_added", "{error}");
        assert_eq!(event["report"], "/out/runs/r/hot-reload/1/report.json");
        assert!(state.refused().is_some() && state.restart().is_none());
        assert_eq!(steps.pid, 100);
    }
    steps.reloads.clear();

    // A second edit that still cannot be patched: the newest reasons replace
    // the first ones.
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps
        .patches
        .push_back(Ok(Outcome::ApplyFailed(refusal(&["apply_failed"]))));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    let events = steps.clear();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["unsupported"][0]["code"], "apply_failed");
    assert_eq!(events[0]["unsupportedCount"], 1);

    // The edit is fixed or reverted: the next compile is patched against the
    // loaded assemblies and the refusal is over.
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps.patches.push_back(Ok(Outcome::Patched(patch(3))));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(steps.take().1, calls(&["code-patched"]));
    assert!(state.refused().is_none());
    steps.clear();
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    assert_eq!(steps.clear()[0]["fullRecompileRequired"], false);
    // Without a refusal a rejected scene part says so and has no reasons.
    steps.reloads.push_back(Reload::Rejected);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    let event = steps.clear().remove(0);
    assert_eq!(
        (&event["event"], &event["error"]),
        (
            &json!("update-rejected"),
            &json!("Player rejected revision: x")
        )
    );
    assert_eq!(event["fullRecompileRequired"], false);
    assert!(event.get("unsupported").is_none() && event.get("report").is_none());
    steps.prepare_error = Some("import failed");
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    // `part` tells the rejection of a scene part from a compile error and
    // from the refusal itself.
    assert_eq!(
        steps.clear().remove(0),
        json!({"event":"update-rejected","error":"import failed","paths":["/p/Assets/S.unity"],"fullRecompileRequired":false,"part":"scene"})
    );
}

#[test]
fn a_refusal_that_is_taken_back_is_stated_also_when_the_scene_part_fails() {
    // The refused edit is reverted in a save whose scene part fails. No
    // confirmation is printed in that window, so its rejection must say that
    // the Player runs the newest code again: nothing else would, and a
    // caller told to recompile would restart the Player for nothing.
    for (reload, prepare_error) in [
        (Reload::Rejected, None),
        (
            Reload::Unchanged,
            Some("prepare prefab: unsupported component"),
        ),
    ] {
        let mut state = Loop::new(true, Fallback::Report);
        let mut steps = Script::new()
            .updated()
            .patched(Outcome::Unsupported(refusal(&["type_added"])));
        run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
        assert_eq!(steps.clear()[0]["fullRecompileRequired"], true);
        steps.compiles.push_back(Ok(Compiled::Updated));
        steps
            .patches
            .push_back(Ok(Outcome::Unchanged(Unchanged::default())));
        steps.reloads.push_back(reload);
        steps.prepare_error = prepare_error;
        run(
            &mut state,
            &mut steps,
            &window(&["/p/Assets/A.cs", "/p/Assets/P.prefab"]),
        );
        let events = steps.clear();
        assert_eq!(events.len(), 1);
        assert_eq!(
            (&events[0]["event"], &events[0]["fullRecompileRequired"]),
            (&json!("update-rejected"), &json!(false))
        );
        assert!(events[0].get("unsupported").is_none());
        assert!(state.refused().is_none());
    }
}

#[test]
fn a_mixed_window_in_report_mode_ends_with_the_refusal() {
    // The scene part is applied, rejected by the Player, or fails before it.
    for (reload, prepare_error, expected) in [
        (Reload::Updated, None, "updated"),
        (Reload::Rejected, None, "update-rejected"),
        (Reload::Unchanged, Some("import failed"), "update-rejected"),
    ] {
        let mut state = Loop::new(true, Fallback::Report);
        let mut steps = Script::new()
            .updated()
            .patched(Outcome::Unsupported(refusal(&["type_added"])))
            .reload(reload);
        steps.prepare_error = prepare_error;
        run(
            &mut state,
            &mut steps,
            &window(&["/p/Assets/A.cs", "/p/Assets/S.unity"]),
        );
        let events = steps.clear();
        assert_eq!(events.len(), 2, "{expected}");
        assert_eq!(events[0]["event"], expected);
        // A waiter that wakes on the scene part already sees the refusal,
        // and with the rejection of a scene part its reasons.
        assert_eq!(events[0]["fullRecompileRequired"], true, "{expected}");
        if expected == "update-rejected" {
            assert_eq!(events[0]["unsupported"][0]["code"], "type_added");
            assert_eq!(events[0]["unsupportedCount"], 1);
            assert_eq!(events[0]["report"], events[1]["report"]);
            assert_ne!(events[0]["error"], events[1]["error"]);
        }
        assert_eq!(
            (&events[1]["event"], &events[1]["fullRecompileRequired"]),
            (&json!("update-rejected"), &json!(true))
        );
        // The refusal is the answer; what comes before it says so.
        assert_eq!(events[0]["pending"], true, "{expected}");
        assert!(events[1].get("pending").is_none());
        assert_eq!(
            events[1]["paths"],
            json!(["/p/Assets/A.cs", "/p/Assets/S.unity"])
        );
        assert!(state.refused().is_some());
        assert_eq!(steps.pid, 100);
    }
    // Changed content routes restart the Player on the refused code: the
    // refusal is over and is not printed.
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Unsupported(refusal(&["type_added"])))
        .routes_differ();
    run(
        &mut state,
        &mut steps,
        &window(&["/p/Assets/A.cs", "/p/Assets/B.unity"]),
    );
    assert_eq!(
        steps.take(),
        (
            calls(&[
                "compile",
                "patch",
                "prepare-loaded",
                "bind",
                "prepare-newest-again",
                "restart"
            ]),
            calls(&["code-updated"])
        )
    );
    assert!(steps.clear()[0].get("reason").is_none());
    assert!(state.refused().is_none());
}

#[test]
fn a_failed_patch_restarts_in_both_modes() {
    let failure = |error: &str| Failure {
        error: error.into(),
        report: "/out/runs/r/hot-reload/2/report.json".into(),
    };
    for fallback in [Fallback::Restart, Fallback::Report] {
        for outcome in [
            Outcome::RestartRequired(failure("An exception interrupted the write.")),
            Outcome::PlayerExited(failure("the Player exited while it applied the code patch")),
            Outcome::TimedOut(failure(
                "the Player did not acknowledge the code patch within 20s",
            )),
        ] {
            let error = match &outcome {
                Outcome::RestartRequired(failure)
                | Outcome::PlayerExited(failure)
                | Outcome::TimedOut(failure) => failure.error.clone(),
                _ => unreachable!(),
            };
            let mut state = Loop::new(true, fallback);
            let mut steps = Script::new().updated().patched(outcome);
            run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
            assert_eq!(
                steps.take(),
                (
                    calls(&["compile", "patch", "bind", "prepare-newest", "restart"]),
                    calls(&["code-updated"])
                )
            );
            let event = steps.clear().remove(0);
            assert_eq!(
                (&event["reason"], &event["error"]),
                (&json!("hot_reload_failed"), &json!(error))
            );
            assert_eq!(
                event["hotReloadReport"],
                "/out/runs/r/hot-reload/2/report.json"
            );
            assert!(state.refused().is_none() && state.restart().is_none());
        }
    }
}

#[test]
fn a_compile_error_keeps_the_code_state_and_a_later_compile_heals() {
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Unsupported(refusal(&["type_added"])));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    steps.clear();
    // The compiler's text, as before hot reload; no patch, no second event.
    steps.compiles.push_back(Err(
        "verify lifecycle Assembly-CSharp: error CS1026: ) expected",
    ));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(
        steps.take(),
        (calls(&["compile"]), calls(&["update-rejected"]))
    );
    let event = steps.clear().remove(0);
    assert_eq!(
        event["error"],
        "verify lifecycle Assembly-CSharp: error CS1026: ) expected"
    );
    assert!(event.get("fullRecompileRequired").is_none());
    assert!(state.refused().is_some());
    // The error is repaired to what was refused before: the whole graph is
    // checked, nothing is rebuilt, and the refusal is said again.
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile-all", "prepare-loaded", "scene"]),
            calls(&["update-rejected"])
        )
    );
    assert_eq!(steps.clear()[0]["fullRecompileRequired"], true);
    // A compile without a meaning for the Player (the edit was reverted to
    // the loaded code): nothing is applied, and the flag is taken back.
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps
        .patches
        .push_back(Ok(Outcome::Unchanged(Unchanged::default())));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile", "patch", "prepare-loaded", "scene"]),
            calls(&["unchanged"])
        )
    );
    assert_eq!(steps.clear()[0]["fullRecompileRequired"], false);
    assert!(state.refused().is_none());
    // A failed step before the compiler is a plain rejection too.
    steps.invalidate_error = Some("built-in shader snapshot differs");
    run(&mut state, &mut steps, &window(&["/p/Assets/S.shader"]));
    assert_eq!(steps.take(), (calls(&[]), calls(&["update-rejected"])));
}

#[test]
fn recompile_restarts_on_the_newest_code_and_answers_with_its_request() {
    // After a refusal: the pending compile is bound and loaded by a new process.
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Unsupported(refusal(&["type_added"])));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    steps.clear();
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    run(&mut state, &mut steps, &recompile(&[], "r-1"));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile-all", "bind", "prepare-newest", "restart"]),
            calls(&["code-updated"])
        )
    );
    let event = steps.clear().remove(0);
    assert_eq!(
        (&event["reason"], &event["requestId"]),
        (&json!("requested"), &json!("r-1"))
    );
    assert_eq!(event["compiledAssemblies"], json!(["Assembly-CSharp"]));
    assert_eq!((&event["pid"], &event["paths"]), (&json!(101), &json!([])));
    assert!(event.get("unsupported").is_none());
    assert!(state.refused().is_none() && !steps.pending);

    // With nothing to load: still a restart, the caller asked for a clean one.
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    run(&mut state, &mut steps, &recompile(&[], "r-2"));
    assert_eq!(
        steps.take(),
        (
            calls(&["compile-all", "prepare-newest", "restart"]),
            calls(&["code-updated"])
        )
    );
    let event = steps.clear().remove(0);
    assert_eq!(
        (&event["reason"], &event["requestId"]),
        (&json!("requested"), &json!("r-2"))
    );
    assert!(event.get("compiledAssemblies").is_none());

    // An edit of the same window is compiled and loaded by the restart; no
    // patch is tried for a Player that is about to be replaced.
    steps.compiles.push_back(Ok(Compiled::Updated));
    run(
        &mut state,
        &mut steps,
        &recompile(&["/p/Assets/A.cs"], "r-3"),
    );
    assert_eq!(
        steps.take(),
        (
            calls(&["compile-all", "bind", "prepare-newest", "restart"]),
            calls(&["code-updated"])
        )
    );
    assert_eq!(steps.clear()[0]["requestId"], "r-3");

    // The compile check fails: the Player is kept and the command answered
    // by a plain rejection that names it.
    steps.compiles.push_back(Err("error CS1002: ; expected"));
    run(&mut state, &mut steps, &recompile(&[], "r-4"));
    assert_eq!(
        steps.take(),
        (calls(&["compile-all"]), calls(&["update-rejected"]))
    );
    let event = steps.clear().remove(0);
    assert_eq!(
        event,
        json!({"event":"update-rejected","error":"error CS1002: ; expected","paths":[],"requestId":"r-4"})
    );
    assert_eq!(steps.pid, 103);
    // So does a later step that fails.
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    steps.prepare_error = Some("import failed");
    run(&mut state, &mut steps, &recompile(&[], "r-5"));
    assert_eq!(steps.take().1, calls(&["update-rejected"]));
    assert_eq!(steps.clear()[0]["requestId"], "r-5");
    // Nothing was bound, so the answered command leaves no restart behind.
    assert!(state.restart().is_none());
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    assert_eq!(steps.take().1, calls(&["unchanged"]));

    // The answer to a command is plain also while a refusal stands: an event
    // that states a refusal answers no command.
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps
        .patches
        .push_back(Ok(Outcome::Unsupported(refusal(&["type_added"]))));
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    steps.clear();
    steps.compiles.push_back(Ok(Compiled::Unchanged));
    steps.prepare_error = Some("import failed");
    run(&mut state, &mut steps, &recompile(&[], "r-6"));
    let events = steps.clear();
    // The refusal that stands is said again after it, as after every check
    // of the sources that finds nothing new.
    assert_eq!(
        events[0],
        json!({"event":"update-rejected","error":"import failed","paths":[],"requestId":"r-6","pending":true})
    );
    assert_eq!(
        (&events[1]["event"], &events[1]["fullRecompileRequired"]),
        (&json!("update-rejected"), &json!(true))
    );
    assert!(events[1].get("requestId").is_none() && events[1].get("pending").is_none());
    assert!(state.refused().is_some());
}

#[test]
fn a_command_that_arrives_during_a_restart_is_answered_by_it() {
    let mut state = Loop::new(true, Fallback::Restart);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Unsupported(refusal(&["type_added"])));
    steps.merged = Some("r-9".into());
    run(&mut state, &mut steps, &window(&["/p/Assets/A.cs"]));
    let event = steps.clear().remove(0);
    assert_eq!(
        (&event["reason"], &event["requestId"]),
        (&json!("requested"), &json!("r-9"))
    );
    assert!(event.get("unsupported").is_none());
    // A window with a command of its own does not take a second one.
    steps.merged = Some("r-10".into());
    run(&mut state, &mut steps, &recompile(&[], "r-11"));
    assert_eq!(steps.clear()[0]["requestId"], "r-11");
    assert_eq!(steps.merged.as_deref(), Some("r-10"));
}

#[test]
fn the_end_of_the_run_and_an_uncertain_player_end_a_window() {
    // A patch that was cut off by the end of the run concludes nothing.
    let ctx = Context::background().with_cancel();
    let mut state = Loop::new(true, Fallback::Restart);
    let mut steps = Script::new().updated();
    steps.patches.push_back(Err("context canceled"));
    ctx.cancel();
    state
        .window(&ctx, &mut steps, &window(&["/p/Assets/A.cs"]), &|| false)
        .unwrap();
    assert_eq!(steps.take(), (calls(&["compile", "patch"]), calls(&[])));
    assert!(state.restart().is_none() && state.refused().is_none());
    // An acknowledgement that leaves the scene revision uncertain ends the
    // run after its event, as before.
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new().reload(Reload::Uncertain);
    let error = state
        .window(
            &Context::background(),
            &mut steps,
            &window(&["/p/Assets/S.unity"]),
            &|| false,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "live Player revision is uncertain or requires a restart"
    );
    assert_eq!(steps.take().1, calls(&["update-rejected"]));
}

/// A patch holds every difference from the loaded code, so its notes are
/// those of every edit since the Player started. What an edit brought is
/// listed on its own and first: a note that only still stands must neither
/// pass for news nor push the news off the list.
#[test]
fn the_notes_an_edit_brought_are_told_apart_from_those_that_stand() {
    let note = |code: &str, member: &str, revision: &str| Reason {
        revision: revision.into(),
        ..Reason::new(code, member, "A note of the patch builder.")
    };
    let field = |index: usize| {
        note(
            "existing_instances_keep_default",
            &format!("Game.A::f{index:02}"),
            "",
        )
    };
    let brought = |event: &Json| -> Vec<String> {
        event["newNotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["member"].as_str().unwrap().to_string())
            .collect()
    };
    let patched = |generation: u64, notes: Vec<Reason>| {
        Outcome::Patched(Patched {
            notes,
            ..patch(generation)
        })
    };
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new();
    let edit = |state: &mut Loop, steps: &mut Script, outcome: Outcome| -> Json {
        steps.compiles.push_back(Ok(Compiled::Updated));
        steps.patches.push_back(Ok(outcome));
        run(state, steps, &window(&["/p/Assets/A.cs"]));
        steps.clear().remove(0)
    };
    // The first patch of a Player process: all of its notes are new.
    let fields: Vec<Reason> = (0..18).map(field).collect();
    let first = edit(&mut state, &mut steps, patched(1, fields.clone()));
    assert_eq!(
        (&first["notesCount"], &first["newNotesCount"]),
        (&json!(18), &json!(18))
    );
    assert_eq!(first["newNotes"].as_array().unwrap().len(), LIST_MAXIMUM);

    // The next edit changes a coroutine whose name sorts behind eighteen
    // notes that stand. Its note is the one that says what does not run.
    let mut notes = fields.clone();
    notes.push(note(
        "running_coroutines_keep_old_code",
        "Game.Z::Loop()",
        "aa",
    ));
    let second = edit(&mut state, &mut steps, patched(2, notes.clone()));
    assert_eq!(brought(&second), ["Game.Z::Loop()"]);
    assert_eq!(
        (&second["notesCount"], &second["newNotesCount"]),
        (&json!(19), &json!(1))
    );
    assert_eq!(second["notes"][0]["member"], "Game.Z::Loop()");
    assert_eq!(second["notes"].as_array().unwrap().len(), LIST_MAXIMUM);
    // An edit elsewhere: the note stands and is no news.
    let third = edit(&mut state, &mut steps, patched(3, notes.clone()));
    assert_eq!(
        (brought(&third).len(), &third["newNotesCount"]),
        (0, &json!(0))
    );
    assert_eq!(third["notes"][0]["member"], "Game.A::f00");
    // The coroutine is edited again: the builder says so with another
    // revision, and the note is about this edit again.
    let last = notes.len() - 1;
    notes[last].revision = "bb".into();
    let fourth = edit(&mut state, &mut steps, patched(4, notes.clone()));
    assert_eq!(brought(&fourth), ["Game.Z::Loop()"]);
    // A refused edit leaves the Player on the code of the last patch, so
    // its notes still stand when the next patch comes.
    let refused = edit(
        &mut state,
        &mut steps,
        Outcome::Unsupported(refusal(&["type_added"])),
    );
    assert_eq!(refused["fullRecompileRequired"], true);
    let fifth = edit(&mut state, &mut steps, patched(6, notes.clone()));
    assert_eq!(fifth["newNotesCount"], 0);
    // A new process runs nothing of the old patches: what the first patch
    // of it notes is new, whatever the process before was told.
    steps.routes.push_back(false);
    run(&mut state, &mut steps, &window(&["/p/Assets/S.unity"]));
    assert_eq!(steps.clear()[0]["event"], "code-updated");
    let again = edit(&mut state, &mut steps, patched(1, notes));
    assert_eq!(again["newNotesCount"], 19);
}

// Commands

#[test]
fn command_lines() {
    assert_eq!(
        parse_command(br#"{"command":"recompile","requestId":"a-1"}"#).unwrap(),
        Command::Recompile("a-1".into())
    );
    // Members a later controller adds are ignored.
    assert_eq!(
        parse_command(br#" {"requestId":"B2","command":"recompile","reason":"x"} "#).unwrap(),
        Command::Recompile("B2".into())
    );
    assert_eq!(
        parse_command(br#"{"command":"sync","requestId":"s-7"}"#).unwrap(),
        Command::Sync("s-7".into())
    );
    let long = "x".repeat(65);
    for (line, expected) in [
        ("recompile", "not a JSON object"),
        ("[1]", "not a JSON object"),
        ("{}", "no command"),
        (r#"{"command":1}"#, "no command"),
        (
            r#"{"command":"restart","requestId":"a"}"#,
            "unknown command \"restart\"",
        ),
        (r#"{"command":"recompile"}"#, "requestId must be"),
        (r#"{"command":"sync"}"#, "requestId must be"),
        (
            r#"{"command":"recompile","requestId":""}"#,
            "requestId must be",
        ),
        (
            r#"{"command":"recompile","requestId":"a b"}"#,
            "requestId must be",
        ),
        (
            &format!(r#"{{"command":"recompile","requestId":"{long}"}}"#),
            "requestId must be",
        ),
    ] {
        let error = parse_command(line.as_bytes()).unwrap_err();
        assert!(error.starts_with(expected), "{line}: {error}");
    }
}

#[test]
fn command_reader_survives_bad_lines_and_the_end_of_its_input() {
    let oversized = format!(
        "{{\"command\":\"recompile\",\"requestId\":\"{}\"}}",
        "y".repeat(70_000)
    );
    let input = format!(
        "{}\n\n   \n{}\nnot json\n{}\n{}\n{}",
        r#"{"command":"recompile","requestId":"first"}"#,
        r#"{"command":"stop"}"#,
        oversized,
        r#"{"command":"recompile","requestId":"second"}"#,
        // The last line of a sender that closed without a newline.
        r#"{"command":"recompile","requestId":"third"}"#,
    );
    let (mut accepted, mut rejected) = (Vec::new(), Vec::new());
    read_commands(
        input.as_bytes(),
        |command| accepted.push(command),
        |line, error| rejected.push((line.len(), error)),
    );
    assert_eq!(
        accepted,
        ["first", "second", "third"].map(|id| Command::Recompile(id.into()))
    );
    assert_eq!(rejected.len(), 3, "{rejected:?}");
    assert_eq!(rejected[0].1, "unknown command \"stop\"");
    assert!(rejected[1].1.starts_with("not a JSON object"));
    assert_eq!(
        rejected[2],
        (64 * 1024, "command line exceeds 65536 bytes".to_string())
    );
    // Nothing but the end of the input.
    read_commands(&b""[..], |_| panic!("no command"), |_, _| panic!("no line"));

    // The event of a rejected line is one a controller stores and ignores.
    let event = event_json(&rejected_event(&[b'x'; 300], "not a JSON object"));
    assert_eq!(event["event"], "command-rejected");
    assert_eq!(event["error"], "not a JSON object");
    assert_eq!(event["line"].as_str().unwrap().len(), 256);
}

#[test]
fn commands_arrive_through_a_pipe_and_wake_the_loop() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let wake = Wake::new();
    let commands = Commands::read(reader, &wake);
    assert!(commands.try_recv().is_none());
    let seen = wake.seen();
    writer
        .write_all(
            b"{\"command\":\"recompile\",\"requestId\":\"one\"}\n{\"command\":\"recompile\",",
        )
        .unwrap();
    wake.wait(seen, Some(Instant::now() + Duration::from_secs(10)));
    let first = commands.try_recv().expect("the first command");
    assert_eq!(first.request_id, "one");
    assert!(first.at.elapsed() < Duration::from_secs(10));
    // Half a line is no command yet.
    assert!(commands.try_recv().is_none());
    let seen = wake.seen();
    writer.write_all(b"\"requestId\":\"two\"}\n").unwrap();
    wake.wait(seen, Some(Instant::now() + Duration::from_secs(10)));
    assert_eq!(commands.try_recv().unwrap().request_id, "two");
    // The controller is gone: the reader ends, the session goes on.
    drop(writer);
    std::thread::sleep(Duration::from_millis(50));
    assert!(commands.try_recv().is_none());
    assert!(Commands::none().try_recv().is_none());
}

// The windows of the main loop

/// A watched project directory, a command pipe and what wakes the loop.
struct Inputs {
    _guard: tempfile::TempDir,
    assets: String,
    ctx: Context,
    wake: Arc<Wake>,
    watcher: ProjectWatcher,
    commands: Commands,
    writer: std::io::PipeWriter,
}
impl Inputs {
    fn new() -> Self {
        let (guard, project) = crate::sysutil::testing::temp_dir();
        let assets = sysutil::join(&[&project, "Assets"]);
        fs::create_dir(&assets).unwrap();
        let ctx = Context::background().with_cancel();
        let wake = Wake::new();
        let watcher = ProjectWatcher::new(&ctx, &wake, &project).unwrap();
        let (reader, writer) = std::io::pipe().unwrap();
        let commands = Commands::read(reader, &wake);
        Self {
            _guard: guard,
            assets,
            ctx,
            wake,
            watcher,
            commands,
            writer,
        }
    }
    /// Saves a source file in place and returns once the watcher holds
    /// both of its events, the creation and the end of the write.
    fn save(&self, name: &str) -> String {
        let path = sysutil::join(&[&self.assets, name]);
        let held = self.watcher.queued();
        fs::write(&path, "changed").unwrap();
        self.until("the source change", || self.watcher.queued() >= held + 2);
        path
    }
    /// Sends a `recompile` command; the reader thread takes it in its own time.
    fn command(&mut self, id: &str) {
        writeln!(
            self.writer,
            "{{\"command\":\"recompile\",\"requestId\":\"{id}\"}}"
        )
        .unwrap();
    }
    fn until(&self, what: &str, reached: impl Fn() -> bool) {
        let limit = Instant::now() + Duration::from_secs(10);
        while !reached() {
            assert!(Instant::now() < limit, "{what} did not arrive");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    /// The next window as the main loop makes it, once something opens one,
    /// with each path once.
    fn next(&self) -> Window {
        let limit = Instant::now() + Duration::from_secs(10);
        let mut window = loop {
            let seen = self.wake.seen();
            if let Some(window) = open_window(&self.watcher, &self.commands).unwrap() {
                break window;
            }
            assert!(Instant::now() < limit, "nothing opened a window");
            self.wake
                .wait(seen, Some(Instant::now() + Duration::from_millis(50)));
        };
        assert!(
            fill_window(
                &self.ctx,
                &self.wake,
                &self.watcher,
                &self.commands,
                &mut window
            )
            .unwrap()
        );
        window.paths.dedup();
        window
    }
}

#[test]
fn a_window_takes_the_changes_of_one_save_and_at_most_one_command() {
    let mut inputs = Inputs::new();
    assert!(
        open_window(&inputs.watcher, &inputs.commands)
            .unwrap()
            .is_none()
    );
    // A command alone opens a window without paths.
    inputs.command("only");
    let window = inputs.next();
    assert_eq!(
        (window.paths.len(), window.recompile.as_deref()),
        (0, Some("only"))
    );
    // Two files of one save are one window, and no command comes with it.
    let (first, second) = (inputs.save("A.cs"), inputs.save("B.cs"));
    let window = inputs.next();
    assert_eq!(window.paths, [first.clone(), second]);
    assert!(window.recompile.is_none());
    assert!(window.changed_at.elapsed() < Duration::from_secs(10));
    // A source change and commands that are ready together: the change
    // opens the window, which takes one command; the second command gets a
    // window, a restart and an answer of its own.
    inputs.command("one");
    inputs.command("two");
    // Both lines are with the reader before the window is opened.
    std::thread::sleep(Duration::from_millis(100));
    let third = inputs.save("C.cs");
    let window = inputs.next();
    assert_eq!(
        (window.paths.clone(), window.recompile.as_deref()),
        (vec![third], Some("one"))
    );
    let window = inputs.next();
    assert_eq!(
        (window.paths.len(), window.recompile.as_deref()),
        (0, Some("two"))
    );
    // A command that arrives within the debounce of a change joins it.
    let fourth = inputs.save("D.cs");
    let mut window = open_window(&inputs.watcher, &inputs.commands)
        .unwrap()
        .expect("the change opens a window");
    inputs.command("late");
    std::thread::sleep(Duration::from_millis(5));
    assert!(
        fill_window(
            &inputs.ctx,
            &inputs.wake,
            &inputs.watcher,
            &inputs.commands,
            &mut window
        )
        .unwrap()
    );
    window.paths.dedup();
    assert_eq!(window.paths, [fourth]);
    // The reader thread may be slower than the debounce; then the command
    // opens the next window.
    if window.recompile.is_none() {
        window = inputs.next();
    }
    assert_eq!(window.recompile.as_deref(), Some("late"));
    // What the watch loop asks at the end of a window.
    assert!(!inputs.watcher.pending());
    inputs.save("E.cs");
    assert!(inputs.watcher.pending());
    // The end of the run ends the wait for more.
    let mut window = inputs.next();
    inputs.ctx.cancel();
    assert!(
        !fill_window(
            &inputs.ctx,
            &inputs.wake,
            &inputs.watcher,
            &inputs.commands,
            &mut window
        )
        .unwrap()
    );
}

// The whole loop

/// Steps for the loop as a session runs it: every window with a source
/// compiles for a while and is patched in; what is printed is shared with
/// the test.
struct Timed {
    printed: Arc<std::sync::Mutex<Vec<Json>>>,
    /// How long a compile takes, and how many have begun.
    compile: Duration,
    compiles: Arc<std::sync::atomic::AtomicUsize>,
    generation: u64,
}
impl Steps for Timed {
    fn emit(&mut self, event: Event) {
        self.printed.lock().unwrap().push(event_json(&event));
    }
    fn invalidate(&mut self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn compile(&mut self, _: &Context, _: Option<&[String]>) -> Result<Compiled> {
        self.compiles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(self.compile);
        Ok(Compiled::Updated)
    }
    fn compile_pending(&self) -> bool {
        true
    }
    fn patch_code(&mut self, _: &Context, _: &Window) -> Result<Outcome> {
        self.generation += 1;
        Ok(Outcome::Patched(patch(self.generation)))
    }
    fn pid(&self) -> i32 {
        100
    }
    fn revision(&self) -> String {
        "revision1".into()
    }
    fn bind_newest(&mut self, _: &Context) -> Result<()> {
        Ok(())
    }
    fn prepare(&mut self, _: &Context, _: &Window, _: bool, _: bool) -> Result<bool> {
        Ok(true)
    }
    fn restart(&mut self, _: &Context, _: &Window) -> Result<Restarted> {
        Err(anyhow!("no restart in this test"))
    }
    fn merged_recompile(&mut self) -> Option<String> {
        None
    }
    fn scene(&mut self, _: &Context, window: &Window, _: Instant) -> Result<Scene> {
        Ok(Scene::Unchanged(
            Event::new("unchanged").with("paths", &window.paths),
        ))
    }
}

/// The loop of a session on its own thread: the real watcher over a
/// project directory, the real command reader on a pipe, timed steps.
struct Session {
    _guard: tempfile::TempDir,
    assets: String,
    ctx: Context,
    wake: Arc<Wake>,
    writer: std::io::PipeWriter,
    printed: Arc<std::sync::Mutex<Vec<Json>>>,
    compiles: Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}
impl Session {
    fn start(compile: Duration) -> Self {
        let (guard, project) = crate::sysutil::testing::temp_dir();
        let assets = sysutil::join(&[&project, "Assets"]);
        fs::create_dir(&assets).unwrap();
        let ctx = Context::background().with_cancel();
        let wake = Wake::new();
        let watcher = ProjectWatcher::new(&ctx, &wake, &project).unwrap();
        let (reader, writer) = std::io::pipe().unwrap();
        let commands = Commands::read(reader, &wake);
        let printed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let compiles = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut steps = Timed {
            printed: printed.clone(),
            compile,
            compiles: compiles.clone(),
            generation: 0,
        };
        let thread = {
            let (ctx, wake) = (ctx.clone(), wake.clone());
            std::thread::spawn(move || {
                let mut state = Loop::new(true, Fallback::Report);
                watch(
                    &ctx,
                    &wake,
                    &watcher,
                    &commands,
                    &mut state,
                    &mut steps,
                    |_| None,
                )
            })
        };
        Self {
            _guard: guard,
            assets,
            ctx,
            wake,
            writer,
            printed,
            compiles,
            thread: Some(thread),
        }
    }
    fn save(&self, name: &str) -> String {
        let path = sysutil::join(&[&self.assets, name]);
        fs::write(&path, "changed").unwrap();
        path
    }
    fn sync(&mut self, id: &str) {
        writeln!(
            self.writer,
            "{{\"command\":\"sync\",\"requestId\":\"{id}\"}}"
        )
        .unwrap();
    }
    fn printed(&self) -> Vec<Json> {
        self.printed.lock().unwrap().clone()
    }
    /// Waits until a compile has begun after `began` of them.
    fn compiling(&self, began: usize) {
        let limit = Instant::now() + Duration::from_secs(20);
        while self.compiles.load(std::sync::atomic::Ordering::SeqCst) == began {
            assert!(Instant::now() < limit, "the compile did not begin");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    /// Waits until an event that fits was printed and returns its position.
    fn until(&self, what: &str, fits: impl Fn(&Json) -> bool) -> usize {
        let limit = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(index) = self.printed().iter().position(&fits) {
                return index;
            }
            assert!(
                Instant::now() < limit,
                "{what} was not printed: {:?}",
                self.printed()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn finish(mut self) -> Vec<Json> {
        self.ctx.cancel();
        self.wake.notify();
        self.thread.take().unwrap().join().unwrap().unwrap();
        self.printed()
    }
}
fn patched_of(path: &str) -> impl Fn(&Json) -> bool + '_ {
    move |event| {
        event["event"] == "code-patched"
            && event["paths"]
                .as_array()
                .is_some_and(|paths| paths.iter().any(|item| item == path))
    }
}
fn synced(id: &str) -> impl Fn(&Json) -> bool + '_ {
    move |event| event["event"] == "synced" && event["requestId"] == id
}

/// What a caller builds on: the answer to a `sync` command comes after the
/// events of everything that was saved before the command, also when the
/// loop had already answered an earlier save and printed nothing about the
/// later one yet.
#[test]
fn a_sync_is_answered_after_the_events_of_what_was_saved_before_it() {
    let mut session = Session::start(Duration::from_millis(250));
    // Nothing is saved: the command is answered right away, and no window
    // is made for it.
    session.sync("idle");
    session.until("the answer to a sync without changes", synced("idle"));
    assert_eq!(session.printed().len(), 1);

    // The first save is answered, and its answer is the last word.
    let first = session.save("A.cs");
    let answered = session.until("the first save", patched_of(&first));
    assert!(session.printed()[answered].get("pending").is_none());
    // The second save, and the command at once: the loop has not begun to
    // look at the save, and nothing it printed so far speaks of it.
    let second = session.save("B.cs");
    session.sync("after-b");
    let barrier = session.until("the answer to the sync", synced("after-b"));
    let events = session.printed();
    let position = events
        .iter()
        .position(patched_of(&second))
        .expect("the second save was answered before the sync");
    assert!(position < barrier, "{events:?}");
    assert_eq!(
        events[barrier],
        json!({"event": "synced", "requestId": "after-b"})
    );

    // A command that arrives while a save is being compiled waits for the
    // end of that window, and commands are answered in their order.
    let began = session.compiles.load(std::sync::atomic::Ordering::SeqCst);
    let third = session.save("C.cs");
    session.compiling(began);
    session.sync("during-c");
    session.sync("again");
    let later = session.until("the second answer", synced("again"));
    let events = session.finish();
    let compiled = events.iter().position(patched_of(&third)).unwrap();
    let during = events.iter().position(synced("during-c")).unwrap();
    assert!(compiled < during && during < later, "{events:?}");
}

/// The loop itself asks the watcher whether changes wait when it prints
/// the events of a window: an answer that a later save follows is marked,
/// and the answer of that save is not.
#[test]
fn the_loop_marks_an_answer_that_a_queued_save_follows() {
    let session = Session::start(Duration::from_millis(300));
    let began = session.compiles.load(std::sync::atomic::Ordering::SeqCst);
    let first = session.save("A.cs");
    session.compiling(began);
    // Saved while the first save is compiled: the next window's.
    let second = session.save("B.cs");
    session.until("the second save", patched_of(&second));
    let events = session.finish();
    let (earlier, later) = (
        &events[events.iter().position(patched_of(&first)).unwrap()],
        &events[events.iter().position(patched_of(&second)).unwrap()],
    );
    assert_eq!(earlier["pending"], true, "{events:?}");
    assert!(later.get("pending").is_none(), "{events:?}");
    // The time of the compile step is what the window waited for it.
    assert!(
        earlier["compileSeconds"].as_f64().unwrap() >= 0.3,
        "{earlier}"
    );
}

/// The barrier under the `sync` command: every change that is complete
/// when `flush` is called has been handed to the queue when it returns.
#[test]
fn a_flush_reads_every_change_that_was_made_before_it() {
    let inputs = Inputs::new();
    assert_eq!(
        (inputs.watcher.flush(), inputs.watcher.taken()),
        (Some(0), 0)
    );
    let mut expected = 0;
    for round in 0..40 {
        // No wait between the saves and the flush: nothing but the flush
        // itself gives the reader time.
        for index in 0..5 {
            fs::write(
                sysutil::join(&[&inputs.assets, &format!("F{round}_{index}.cs")]),
                "changed",
            )
            .unwrap();
            // A new file: its creation and the end of the write.
            expected += 2;
        }
        assert_eq!(inputs.watcher.flush(), Some(expected), "round {round}");
        assert_eq!(
            inputs.watcher.queued() as u64 + inputs.watcher.taken(),
            expected
        );
        // The loop takes some of them; the count of all stays.
        assert!(inputs.watcher.try_recv().is_some());
    }
    assert_eq!(inputs.watcher.taken(), 40);
}

/// The queue of the watcher holds 8192 changes, and the reader waits when
/// it is full. A flush then has not read to the end: the count it could
/// give leaves out changes that were complete as well, and a `synced` on
/// that count would precede their window. It says so instead, and counts
/// all of them once the loop has made room.
#[test]
fn a_flush_that_cannot_read_to_the_end_says_so() {
    let inputs = Inputs::new();
    const FILES: u64 = 4200;
    for index in 0..FILES {
        fs::write(
            sysutil::join(&[&inputs.assets, &format!("Burst{index}.cs")]),
            "changed",
        )
        .unwrap();
    }
    // 8400 changes, nobody takes one: the reader is held at the capacity.
    let started = Instant::now();
    assert_eq!(inputs.watcher.flush(), None);
    // It does not wait for what only the caller can end.
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(inputs.watcher.queued(), 8192);
    // The loop takes what is queued and asks again.
    let limit = Instant::now() + Duration::from_secs(20);
    let counted = loop {
        while inputs.watcher.try_recv().is_some() {}
        if let Some(count) = inputs.watcher.flush() {
            break count;
        }
        assert!(Instant::now() < limit, "the flush never read to the end");
    };
    assert_eq!(counted, 2 * FILES);
    while inputs.watcher.try_recv().is_some() {}
    assert_eq!(inputs.watcher.taken(), 2 * FILES);
}

/// A save of an existing file is one change, the end of its write. A sync
/// that counts one change too few is answered before the window of that
/// save, which is the wait that reports the save before.
#[test]
fn a_sync_waits_for_the_window_of_one_queued_change() {
    let mut session = Session::start(Duration::from_millis(250));
    let existing = session.save("E.cs");
    session.until("the first save of E", patched_of(&existing));
    session.sync("settled");
    let settled = session.until("the loop at rest", synced("settled"));
    // While another save is compiled, E is saved again: one change in the
    // queue, and then the command.
    let began = session.compiles.load(std::sync::atomic::Ordering::SeqCst);
    let other = session.save("A.cs");
    session.compiling(began);
    fs::write(&existing, "changed again").unwrap();
    session.sync("after-e");
    session.until("the answer to the sync", synced("after-e"));
    let events = session.finish();
    let barrier = events.iter().position(synced("after-e")).unwrap();
    let first = events.iter().position(patched_of(&other)).unwrap();
    let again = settled
        + events[settled..]
            .iter()
            .position(patched_of(&existing))
            .expect("the second save of E was answered");
    assert!(first < again && again < barrier, "{events:?}");
    // The window of the second save held that one change.
    assert_eq!(events[again]["paths"], json!([existing]), "{events:?}");
}

/// More changes than the watcher's queue holds wait behind a compile when
/// the command arrives, and a save after them. No flush can count them
/// then; the command is numbered when the loop has made room, and its
/// answer follows the window of the last save.
#[test]
fn a_sync_behind_a_full_queue_is_answered_after_all_of_it() {
    let mut session = Session::start(Duration::from_millis(400));
    let began = session.compiles.load(std::sync::atomic::Ordering::SeqCst);
    session.save("A.cs");
    session.compiling(began);
    for index in 0..4200 {
        fs::write(
            sysutil::join(&[&session.assets, &format!("Burst{index}.cs")]),
            "changed",
        )
        .unwrap();
    }
    let last = session.save("Last.cs");
    session.sync("after-last");
    session.until("the answer to the sync", synced("after-last"));
    let events = session.finish();
    let barrier = events.iter().position(synced("after-last")).unwrap();
    let answered = events
        .iter()
        .position(patched_of(&last))
        .expect("the last save was answered");
    assert!(answered < barrier, "{:?}", &events[..barrier.min(4)]);
    // Every change went through a window before the answer.
    let taken: usize = events[..barrier]
        .iter()
        .filter_map(|event| event["paths"].as_array())
        .map(Vec::len)
        .sum();
    assert_eq!(taken, 2 + 2 * 4200 + 2);
}

/// A tool that makes a temporary directory and removes it at once is
/// faster than the reader: the directory is gone when the reader gets to
/// its creation. That is no error of the watch, and the session goes on.
#[test]
fn a_directory_that_is_gone_again_does_not_end_the_watch() {
    let inputs = Inputs::new();
    for round in 0..400 {
        let directory = sysutil::join(&[&inputs.assets, &format!("T{round}")]);
        fs::create_dir(&directory).unwrap();
        // A file inside makes the walk of the reader longer.
        fs::write(sysutil::join(&[&directory, "x.cs"]), "x").unwrap();
        fs::remove_dir_all(&directory).unwrap();
    }
    let saved = inputs.save("After.cs");
    let count = inputs.watcher.flush().expect("the queue has room");
    let mut paths = Vec::new();
    while let Some(change) = inputs.watcher.try_recv() {
        assert_eq!(change.error, None, "the watch ended");
        paths.push(change.path);
    }
    assert_eq!(paths.len() as u64, count);
    assert!(paths.contains(&saved));
    // The directories themselves were reported, made and removed.
    let first = sysutil::join(&[&inputs.assets, "T0"]);
    assert!(paths.iter().filter(|path| **path == first).count() >= 2);
}

/// A patch adds a field that Unity would serialize; the Player does not
/// fill it from scenes. A scene update that brings a value for that field
/// is confirmed by the Player and is not all in it: its event says so, as
/// a note the save brought. A scene that does not name the field, and any
/// scene once the Player has restarted, is confirmed without one.
#[test]
fn a_scene_update_names_the_added_fields_it_holds_values_for() {
    let (_guard, root) = crate::sysutil::testing::temp_dir();
    let scene = sysutil::join(&[&root, "Lamp.unity"]);
    let other = sysutil::join(&[&root, "Other.unity"]);
    let asset = sysutil::join(&[&root, "Tuning.asset"]);
    fs::write(
        &scene,
        "MonoBehaviour:\n  m_Script: {fileID: 11500000}\n  dimRate: 0.5\n  glowCut: 0.9\n",
    )
    .unwrap();
    // The name inside another key or a value is no value for the field.
    fs::write(
        &other,
        "MonoBehaviour:\n  glowCutoff: 1\n  note: glowCut: no\n  m_Name: glowCut\n",
    )
    .unwrap();
    fs::write(&asset, "MonoBehaviour:\n  items:\n  - glowCut: 3\n").unwrap();
    let mut added = patch(1);
    added.added_fields = 2;
    added.notes = vec![
        Reason::new(
            "existing_instances_keep_default",
            "Game.Lamp::glowCut",
            "Objects that already exist read the default value.",
        ),
        Reason::new(
            "serialized_field_not_restored",
            "Game.Lamp::glowCut",
            "The added field Game.Lamp::glowCut is not restored from scenes.",
        ),
        Reason::new(
            "serialized_field_not_restored",
            "Game.Lamp::shield",
            "The added field Game.Lamp::shield is not restored from scenes.",
        ),
    ];
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new()
        .updated()
        .patched(Outcome::Patched(added.clone()));
    run(&mut state, &mut steps, &window(&["/p/Assets/Lamp.cs"]));
    assert_eq!(steps.take().1, calls(&["code-patched"]));
    steps.clear();

    // The scene edit that sets the field.
    let updated = |state: &mut Loop, steps: &mut Script, paths: &[&str]| {
        steps.compiles.push_back(Ok(Compiled::Unaffected));
        steps.reloads.push_back(Reload::Updated);
        run(state, steps, &window(paths));
        let mut events = steps.clear();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["event"], "updated");
        assert_eq!(events[0]["fullRecompileRequired"], false);
        events.remove(0)
    };
    let event = updated(&mut state, &mut steps, &[scene.as_str(), scene.as_str()]);
    assert_eq!(event["newNotesCount"], 1, "{event}");
    assert_eq!(
        (
            &event["newNotes"][0]["code"],
            &event["newNotes"][0]["member"]
        ),
        (
            &json!("serialized_field_not_restored"),
            &json!("Game.Lamp::glowCut")
        )
    );
    let message = event["newNotes"][0]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "{scene} holds a value for the added field Game.Lamp::glowCut"
        )) && message.contains("until a restart"),
        "{message}"
    );
    // A list item of an asset names it too; a scene that only has the name
    // elsewhere does not, nor does a file that is no scene, prefab or asset.
    let event = updated(&mut state, &mut steps, &[asset.as_str()]);
    assert_eq!(event["newNotesCount"], 1, "{event}");
    for paths in [vec![other.as_str()], vec!["/p/Assets/Lamp.png"]] {
        let event = updated(&mut state, &mut steps, &paths);
        assert!(event.get("newNotes").is_none(), "{event}");
    }
    // A patch without the field ends it: the next patch is all there is.
    added.notes.truncate(1);
    steps.compiles.push_back(Ok(Compiled::Updated));
    steps.patches.push_back(Ok(Outcome::Patched(added)));
    run(&mut state, &mut steps, &window(&["/p/Assets/Lamp.cs"]));
    steps.clear();
    let event = updated(&mut state, &mut steps, &[scene.as_str()]);
    assert!(event.get("newNotes").is_none(), "{event}");
}

/// The forms in which a scene, a prefab variant or a controller holds a value
/// for a field the applied patch added, and data that names a method it
/// added; a field of the same name of another script is none of them
/// (stage 5 runner5-1, waits5-5, waits5-3).
#[test]
fn a_scene_update_finds_overrides_and_named_methods_and_skips_other_scripts() {
    let (_guard, root) = crate::sysutil::testing::temp_dir();
    let lamp = "a01ac09a24c8493b862564ce94c80306";
    let file = |name: &str, text: &str| {
        let path = sysutil::join(&[&root, name]);
        fs::write(&path, text).unwrap();
        path
    };
    // A scene sets the field of a prefab instance, as Unity writes it.
    let override_scene = file(
        "Override.unity",
        "%YAML 1.1\n--- !u!1001 &500\nPrefabInstance:\n  m_Modification:\n    m_Modifications:\n    - target: {fileID: 105, guid: 9f0c, type: 3}\n      propertyPath: boost\n      value: 3\n      objectReference: {fileID: 0}\n",
    );
    // A field of a [Serializable] class the component holds.
    let nested = file(
        "Nested.prefab",
        "%YAML 1.1\n--- !u!1001 &501\nPrefabInstance:\n  m_Modification:\n    m_Modifications:\n    - target: {fileID: 105, guid: 9f0c, type: 3}\n      propertyPath: tuning.armor\n      value: 7\n",
    );
    // A StateMachineBehaviour of the controller.
    let controller = file(
        "Lamp.controller",
        &format!(
            "%YAML 1.1\n--- !u!114 &7\nMonoBehaviour:\n  m_Script: {{fileID: 11500000, guid: {lamp}, type: 3}}\n  boost: 0.4\n"
        ),
    );
    // The component in a prefab, with m_Script in block style as a hand edit
    // or another tool may write it.
    let prefab = file(
        "Cube.prefab",
        &format!(
            "%YAML 1.1\n--- !u!114 &105\nMonoBehaviour:\n  m_GameObject:\n    fileID: 100\n  m_Script:\n    fileID: 11500000\n    guid: {lamp}\n    type: 3\n  dim: 0.35\n  boost: 2\n"
        ),
    );
    // Another script has a field of the same name, and the camera changed.
    let other = file(
        "Main.unity",
        "%YAML 1.1\n--- !u!114 &500\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb, type: 3}\n  boost: 5\n--- !u!20 &600\nCamera:\n  m_BackGroundColor: {r: 0.1, g: 0.2, b: 0.3, a: 1}\n",
    );
    // A button's UnityEvent and an animation event name the added methods.
    let button = file(
        "Menu.unity",
        "%YAML 1.1\n--- !u!114 &9\nMonoBehaviour:\n  m_OnClick:\n    m_PersistentCalls:\n      m_Calls:\n      - m_Target: {fileID: 7}\n        m_MethodName: OnPinged\n",
    );
    let clip = file(
        "Blink.anim",
        "%YAML 1.1\n--- !u!74 &1\nAnimationClip:\n  m_Events:\n  - time: 0.5\n    functionName: Flash\n",
    );
    let mut added = patch(1);
    added.notes = vec![
        Reason::new(
            "serialized_field_not_restored",
            "Game.Lamp::boost",
            "The added field Game.Lamp::boost is not restored.",
        ),
        Reason::new(
            "serialized_field_not_restored",
            "Game.Tuning::armor",
            "The added field Game.Tuning::armor is not restored.",
        ),
        Reason::new(
            "added_method_invisible_by_name",
            "Game.Lamp::OnPinged()",
            "The added method Game.Lamp::OnPinged() belongs to an engine object.",
        ),
        Reason::new(
            "added_method_computed_name",
            "Game.Lamp::Flash()",
            "The game calls methods of its objects by a name it computes.",
        ),
    ];
    let mut state = Loop::new(true, Fallback::Report);
    let mut steps = Script::new().updated().patched(Outcome::Patched(added));
    steps.scripts.insert("Game.Lamp".into(), lamp.into());
    run(&mut state, &mut steps, &window(&["/p/Assets/Lamp.cs"]));
    steps.clear();
    let updated = |state: &mut Loop, steps: &mut Script, path: &str| -> Json {
        steps.compiles.push_back(Ok(Compiled::Unaffected));
        steps.reloads.push_back(Reload::Updated);
        run(state, steps, &window(&[path]));
        let mut events = steps.clear();
        assert_eq!(events.len(), 1, "{events:?}");
        events.remove(0)
    };
    let noted = |event: &Json| -> Vec<(String, String)> {
        event["newNotes"]
            .as_array()
            .map(|notes| {
                notes
                    .iter()
                    .map(|note| {
                        (
                            note["code"].as_str().unwrap().to_string(),
                            note["member"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let field = |member: &str| {
        (
            "serialized_field_not_restored".to_string(),
            member.to_string(),
        )
    };
    let method = |member: &str| {
        (
            "added_method_named_by_string".to_string(),
            member.to_string(),
        )
    };
    assert_eq!(
        noted(&updated(&mut state, &mut steps, &override_scene)),
        [field("Game.Lamp::boost")]
    );
    assert_eq!(
        noted(&updated(&mut state, &mut steps, &nested)),
        [field("Game.Tuning::armor")]
    );
    assert_eq!(
        noted(&updated(&mut state, &mut steps, &controller)),
        [field("Game.Lamp::boost")]
    );
    assert_eq!(
        noted(&updated(&mut state, &mut steps, &prefab)),
        [field("Game.Lamp::boost")]
    );
    let event = updated(&mut state, &mut steps, &other);
    assert!(noted(&event).is_empty(), "{event}");
    let event = updated(&mut state, &mut steps, &button);
    assert_eq!(noted(&event), [method("Game.Lamp::OnPinged()")]);
    assert!(
        event["newNotes"][0]["message"]
            .as_str()
            .unwrap()
            .contains("(m_MethodName)"),
        "{event}"
    );
    assert_eq!(
        noted(&updated(&mut state, &mut steps, &clip)),
        [method("Game.Lamp::Flash()")]
    );
}

#[test]
fn timings_line_names_every_phase_and_only_when_asked() {
    let window = window(&["/project/Assets/Scripts/A.cs"]);
    let mut compile = compiler::Phases::default();
    compile.add("csc", Duration::from_millis(250));
    compile.add("csc", Duration::from_millis(250));
    compile.add("an_extra_phase", Duration::from_millis(1));
    let mut patch = compiler::Phases::default();
    patch.add("builder", Duration::from_millis(30));
    let times = WindowTimes {
        start: Some(Instant::now()),
        compile: Some(Duration::from_millis(900)),
        patch: Some(Duration::from_millis(40)),
    };
    assert_eq!(
        timings_report(false, &window, &times, &compile, &patch),
        None,
        "a line of timings was made without UME_TIMINGS=1"
    );
    let line = timings_report(true, &window, &times, &compile, &patch).unwrap();
    assert!(!line.contains('\n'), "one line per window: {line}");
    let value: Json = serde_json::from_str(&line).unwrap();
    assert_eq!(value["timings"], "window");
    assert_eq!(value["paths"], 1);
    for key in [
        "totalSeconds",
        "waitSeconds",
        "compileSeconds",
        "patchSeconds",
    ] {
        assert!(value[key].is_f64(), "{key} is missing: {line}");
    }
    assert_eq!(value["compileSeconds"].as_f64(), Some(0.9));
    for name in compiler::COMPILE_PHASES {
        assert!(value["compile"][name].is_f64(), "{name} is missing: {line}");
    }
    for name in compiler::PATCH_PHASES {
        assert!(value["patch"][name].is_f64(), "{name} is missing: {line}");
    }
    assert_eq!(value["compile"]["csc"].as_f64(), Some(0.5));
    assert_eq!(value["compile"]["an_extra_phase"].as_f64(), Some(0.001));
    assert_eq!(value["compile"]["lifecycle_rewrite"].as_f64(), Some(0.0));
    assert_eq!(value["patch"]["builder"].as_f64(), Some(0.03));
}
