//! Tests of the documents, of an attempt with a scripted patch builder and a
//! thread standing in for the Player, and SDK tests that build and run the
//! real programs. SDK tests return early without `UME_LIFECYCLE_SDK`.
use super::{tools::*, *};
use crate::{
    cancel::Context,
    compiler::{compiler_command, glob},
    sha256, sysutil,
    util::{file_hash, micro_editor_root, read_json_file},
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};
use tempfile::TempDir;

const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/rust/tests/testdata/hotreload");

fn temp() -> (TempDir, String) {
    let directory = TempDir::new().unwrap();
    let path = directory
        .path()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    (directory, path)
}
fn write(path: &str, data: &[u8]) -> String {
    fs::create_dir_all(sysutil::dir(path)).unwrap();
    fs::write(path, data).unwrap();
    path.to_string()
}
fn read_json(path: &str) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|error| panic!("{path}: {error}")))
        .unwrap()
}
fn recorded(name: &str) -> Vec<u8> {
    fs::read(format!("{TESTDATA}/{name}")).unwrap()
}
fn encoded<T: serde::Serialize>(value: &T) -> Value {
    serde_json::from_slice(&crate::util::marshal(value).unwrap()).unwrap()
}

// Sources

/// The project file compiles every source beside it; one the list lacks
/// would be missing from the staged build and from the cache key.
#[test]
fn patcher_source_list_names_every_source() {
    let directory = sysutil::join(&[&micro_editor_root().unwrap(), PATCHER_DIRECTORY]);
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(&directory).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".cs") || name.ends_with(".csproj") {
            found.insert(name);
        }
    }
    let listed: BTreeSet<String> = PATCHER_SOURCES
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(
        found, listed,
        "PATCHER_SOURCES must list the sources of {directory}"
    );
    assert_eq!(
        listed.len(),
        PATCHER_SOURCES.len(),
        "a source is listed twice"
    );
}

// Documents

#[test]
fn builder_request_encoding() {
    let mut request = BuilderRequest {
        schema_version: 1,
        generation: 3,
        output: "/abs/dir/for/this/generation".into(),
        runtime_assembly: RUNTIME_ASSEMBLY.into(),
        previous_result: String::new(),
        assemblies: vec![BuilderAssembly {
            name: "Assembly-CSharp".into(),
            loaded: "/abs/loaded/Assembly-CSharp.dll".into(),
            current: "/abs/new/Assembly-CSharp.dll".into(),
            netcode: false,
        }],
    };
    // The request of the design contract, 5.1.
    let contract = json!({"schemaVersion":1,"generation":3,"output":"/abs/dir/for/this/generation",
        "runtimeAssembly":"UnityMicroEditor.HotReload",
        "assemblies":[{"name":"Assembly-CSharp","loaded":"/abs/loaded/Assembly-CSharp.dll","current":"/abs/new/Assembly-CSharp.dll","netcode":false}]});
    assert_eq!(encoded(&request), contract);
    // A2: the result of the generation the Player runs.
    request.previous_result = "/abs/2/result.json".into();
    assert_eq!(encoded(&request)["previousResult"], "/abs/2/result.json");
}

#[test]
fn builder_result_decoding() {
    // The result of the design contract, 5.1, with one verdict chosen.
    let contract = br#"{"schemaVersion":1,"generation":3,"verdict":"patch",
     "assemblies":[{"name":"Assembly-CSharp","loadedMvid":"0fcce5cc-d5b5-44b3-8918-d49b7c293987","verdict":"patch",
       "patch":{"assemblyName":"Assembly-CSharp.HotReload.3","path":"/abs/Assembly-CSharp.HotReload.3.dll","sha256":"ab","pdb":""},
       "detours":[{"member":"Game.Player::Update()","kind":"changed","originalToken":100663301,"replacementToken":100663299}],
       "addedMethods":[{"member":"Game.Player::Heal(System.Int32)","token":100663300}],
       "addedFields":[{"member":"Game.Player::speed","id":"Assembly-CSharp|Game.Player|speed","static":false}],
       "changes":[{"kind":"method_body","member":"Game.Player::Update()","action":"patch","code":"","message":""}],
       "unsupported":[],
       "notes":[{"code":"existing_instances_keep_default","member":"Game.Player::speed","message":"m"}]}],
     "summary":{"patchedMethods":2,"addedMethods":1,"addedFields":1,"unsupported":0,"notes":1},
     "timingsMs":{"read":1,"diff":2,"emit":3}}"#;
    let result = BuilderResult::decode(contract, 3).unwrap();
    assert_eq!(result.verdict, "patch");
    assert_eq!(result.summary.patched_methods, 2);
    assert_eq!(
        result.members(),
        [
            "Game.Player::Update()",
            "Game.Player::Heal(System.Int32)",
            "Game.Player::speed"
        ]
    );
    assert_eq!(
        result.notes(),
        [Reason::new(
            "existing_instances_keep_default",
            "Game.Player::speed",
            "m"
        )]
    );
    let detour = &result.assemblies[0].detours[0];
    assert_eq!(
        (detour.original_token, detour.replacement_token),
        (0x06000005, 0x06000003)
    );

    // What the tool printed for the fixtures of the SDK test.
    let patch = BuilderResult::decode(&recorded("result-patch.json"), 1).unwrap();
    assert_eq!(patch.verdict, "patch");
    assert_eq!(
        patch.members(),
        ["Game.Counter::Label()", "Game.Counter::Next()"]
    );
    assert_eq!(patch.assemblies[0].patch.sha256.len(), 64);
    let added = BuilderResult::decode(&recorded("result-added.json"), 3).unwrap();
    assert_eq!(
        (added.summary.added_methods, added.summary.added_fields),
        (2, 1)
    );
    assert_eq!(added.members().len(), 5);
    assert_eq!(added.notes()[0].code, "existing_instances_keep_default");
    let unsupported = BuilderResult::decode(&recorded("result-unsupported.json"), 2).unwrap();
    assert_eq!(unsupported.verdict, "unsupported");
    assert_eq!(
        unsupported.unsupported(),
        [Reason::new(
            "type_added",
            "Game.Added",
            "Type Game.Added was added or renamed."
        )]
    );
    // No patch file is named, whatever the assemblies say.
    assert!(unsupported.assemblies[0].patch.path.is_empty());
    let unchanged = BuilderResult::decode(&recorded("result-unchanged.json"), 4).unwrap();
    assert_eq!(unchanged.verdict, "unchanged");
    assert_eq!(unchanged.patched().count(), 0);
    assert_eq!(
        unchanged.notes()[0].code,
        "earlier_patch_delegates_keep_old_code"
    );

    // Unknown members are ignored, missing and null ones are zero values.
    let sparse = br#"{"schemaVersion":1,"generation":7,"verdict":"unchanged","assemblies":[{"name":"A","detours":null,"future":[1]}],"summary":null,"later":{}}"#;
    let sparse = BuilderResult::decode(sparse, 7).unwrap();
    assert_eq!(
        (
            sparse.assemblies[0].name.as_str(),
            sparse.summary.patched_methods
        ),
        ("A", 0)
    );
    for (text, generation, expected) in [
        ("not json", 1, "is not its JSON"),
        (
            r#"{"schemaVersion":1,"generation":2,"verdict":"patch"}"#,
            1,
            "generation 2",
        ),
        (
            r#"{"schemaVersion":2,"generation":1,"verdict":"patch"}"#,
            1,
            "schema 2",
        ),
        (
            r#"{"schemaVersion":1,"generation":1,"verdict":"maybe"}"#,
            1,
            "verdict",
        ),
        (r#"{"schemaVersion":1,"generation":1}"#, 1, "verdict"),
    ] {
        let error = BuilderResult::decode(text.as_bytes(), generation).unwrap_err();
        assert!(error.to_string().contains(expected), "{text}: {error}");
    }
}

#[test]
fn descriptor_and_request_encoding() {
    let result = BuilderResult::decode(&recorded("result-patch.json"), 1).unwrap();
    let game = ["Assembly-CSharp".to_string(), "Game.Core".to_string()];
    let descriptor = Descriptor::new(1, &game, Some(&result)).unwrap();
    // The descriptor of the design contract, 5.3.
    assert_eq!(
        encoded(&descriptor),
        json!({"schemaVersion":1,"generation":1,"gameAssemblies":["Assembly-CSharp","Game.Core"],
            "assemblies":[{"name":"Assembly-CSharp","loadedMvid":"0fcce5cc-d5b5-44b3-8918-d49b7c293987",
                "patch":"/session/runs/r1/hot-reload/1/Assembly-CSharp.HotReload.1.dll",
                "patchSha256":"e8530a7b1dab74d83141a4834a5d8da139c09fb7d5facc13d65d079fe0ac317f",
                "pdb":"/session/runs/r1/hot-reload/1/Assembly-CSharp.HotReload.1.pdb",
                "detours":[
                    {"member":"Game.Counter::Label()","kind":"changed","originalToken":100663298,"replacementToken":100663297},
                    {"member":"Game.Counter::Next()","kind":"changed","originalToken":100663297,"replacementToken":100663298}]}]})
    );
    // "Restore everything" is an empty list.
    assert_eq!(
        encoded(&Descriptor::new(5, &game, None).unwrap()),
        json!({"schemaVersion":1,"generation":5,"gameAssemblies":["Assembly-CSharp","Game.Core"],"assemblies":[]})
    );
    // An assembly that is unchanged has no entry; one without a file is an error.
    let unchanged = BuilderResult::decode(&recorded("result-unchanged.json"), 4).unwrap();
    assert!(
        Descriptor::new(4, &game, Some(&unchanged))
            .unwrap()
            .assemblies
            .is_empty()
    );
    let mut torn = result.clone();
    torn.assemblies[0].patch.path.clear();
    assert!(Descriptor::new(1, &game, Some(&torn)).is_err());

    // The members are those of a descriptor and a request the harness of
    // the applier wrote by hand and the bridge's own code accepted.
    fn keys(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }
    let (ours, theirs) = (
        encoded(&descriptor),
        read_json(&format!("{TESTDATA}/apply-harness.json")),
    );
    assert_eq!(keys(&ours), keys(&theirs));
    assert_eq!(keys(&ours["assemblies"][0]), keys(&theirs["assemblies"][0]));
    assert_eq!(
        keys(&ours["assemblies"][0]["detours"][0]),
        keys(&theirs["assemblies"][0]["detours"][0])
    );

    // The request of the design contract, 5.3.
    let request = ReloadRequest {
        schema_version: 1,
        request_id: "id-1".into(),
        generation: 3,
        applier: "/abs/UnityMicroEditor.HotReload.dll".into(),
        applier_sha256: "aa".into(),
        descriptor: "/abs/hot-reload/3/apply.json".into(),
        descriptor_sha256: "bb".into(),
        requested_utc_unix_milliseconds: 20,
        source_changed_utc_unix_milliseconds: 10,
    };
    assert_eq!(
        encoded(&request),
        json!({"schemaVersion":1,"requestId":"id-1","generation":3,
            "applier":"/abs/UnityMicroEditor.HotReload.dll","applierSha256":"aa",
            "descriptor":"/abs/hot-reload/3/apply.json","descriptorSha256":"bb",
            "requestedUtcUnixMilliseconds":20,"sourceChangedUtcUnixMilliseconds":10})
    );
    assert_eq!(
        keys(&encoded(&request)),
        keys(&read_json(&format!("{TESTDATA}/request-harness.json")))
    );
}

#[test]
fn ack_decoding() {
    // The acknowledgement of the design contract, 5.3.
    let contract = r#"{"schemaVersion":1,"requestId":"<id>","pid":123,"success":true,"generation":3,"appliedGeneration":3,
     "restartRequired":false,"error":"",
     "failures":[{"assembly":"A","member":"M","stage":"resolve","message":"m"}],
     "detoursApplied":2,"detoursRestored":0,"callbacksInvoked":1,"callbackErrors":0,
     "inliningDisabled":true,"elapsedSeconds":0.004,"completedUtcUnixMilliseconds":0}"#;
    let ack: Ack = serde_json::from_str(contract).unwrap();
    assert!(ack.success && !ack.restart_required);
    assert_eq!(
        (ack.pid, ack.generation, ack.applied_generation),
        (123, 3, 3)
    );
    assert_eq!(ack.failures[0].stage, "resolve");
    // As the applier wrote it on Unity's Mono (the harness of
    // tests/HotReloadRuntimeTests): more counters than the contract (A1).
    let ack: Ack = serde_json::from_slice(&recorded("ack-applied.json")).unwrap();
    assert!(ack.success && !ack.restart_required && ack.error.is_empty());
    assert_eq!((ack.request_id.as_str(), ack.pid), ("request-3", 417921));
    assert_eq!((ack.generation, ack.applied_generation), (2, 2));
    assert_eq!(
        (
            ack.detours_applied,
            ack.detours_restored,
            ack.callback_errors
        ),
        (1, 0, 3)
    );
    // As the bridge wrote it for a request it refused itself.
    let ack: Ack = serde_json::from_slice(&recorded("ack-bridge.json")).unwrap();
    assert!(!ack.success && ack.restart_required);
    assert_eq!((ack.applied_generation, ack.failures.len()), (0, 1));
    assert_eq!(ack.failures[0].stage, "applier");
    assert!(ack.error.contains("one Player process loads one applier"));
    // A duration in the exponent form a runtime may choose.
    let ack: Ack = serde_json::from_str(r#"{"requestId":"z","elapsedSeconds":1.5E-05}"#).unwrap();
    assert_eq!(ack.request_id, "z");
    let sparse: Ack = serde_json::from_str(r#"{"requestId":"y","failures":null,"new":1}"#).unwrap();
    assert_eq!((sparse.request_id.as_str(), sparse.success), ("y", false));
}

#[test]
fn refusal_sentence_names_the_first_reason_and_the_count() {
    let mut refusal = Refusal {
        reasons: vec![
            Reason::new(
                "type_added",
                "Game.Added",
                "Type Game.Added was added or renamed.",
            ),
            Reason::new("generic_method", "Game.Box`1::Get()", "m"),
        ],
        report: "/r/report.json".into(),
    };
    assert_eq!(
        refusal.summary(),
        "hot reload cannot apply the C# edit to the running Player (2 reasons); first: [type_added] Game.Added: Type Game.Added was added or renamed"
    );
    refusal.reasons.truncate(1);
    assert!(refusal.summary().contains("(1 reason);"));
    // A reason must never read as a managed graph error: the controller
    // would prepare the project again instead of recompiling.
    for phrase in [
        "regenerate source graph",
        "fresh managed graph",
        "outside the prepared managed graph",
    ] {
        refusal.reasons[0].message = format!("the helper said: {phrase}: x");
        let text = refusal.summary();
        assert!(!crate::compiler::requires_new_graph(&text), "{text}");
        assert!(text.ends_with("first: [type_added]"), "{text}");
    }
    // The output of a failed builder: its first line, cut.
    refusal.reasons[0] = Reason::new(
        CODE_BUILDER_FAILED,
        "",
        &format!(
            "dotnet: exit status 1\nUnhandled exception.\n{}",
            "é".repeat(4000)
        ),
    );
    assert!(
        refusal
            .summary()
            .ends_with("first: [patch_builder_failed] dotnet: exit status 1")
    );
    refusal.reasons[0].message = "é".repeat(4000);
    let text = refusal.summary();
    assert!(text.len() < 700 && text.ends_with('é'), "{}", text.len());
    refusal.reasons.clear();
    assert!(refusal.summary().ends_with("(0 reasons)"));
}

#[test]
fn fallback_values() {
    assert_eq!(Fallback::default(), Fallback::Restart);
    assert_eq!(Fallback::parse("restart"), Some(Fallback::Restart));
    assert_eq!(Fallback::parse("report"), Some(Fallback::Report));
    for other in ["", "Report", "keep", "restart "] {
        assert_eq!(Fallback::parse(other), None, "{other:?}");
    }
}

// A scripted patch builder and a thread standing in for the Player.

/// What the stand-in Player answers to its next request.
#[derive(Clone)]
enum Reply {
    Apply {
        restored: u64,
        callback_errors: u64,
    },
    /// Success with one redirect less than the descriptor lists.
    ApplyShort,
    /// Success with one redirect more than the descriptor lists.
    ApplyLong,
    /// `success: false`, nothing written.
    Refuse(Vec<(&'static str, &'static str, &'static str, &'static str)>),
    /// The bridge's own answer: no list of failures.
    RefuseBare(&'static str),
    /// A refusal that names another generation than the one applied here.
    RefuseAt(u64),
    Restart(&'static str),
    WrongPid,
    WrongGeneration,
    Garbage,
    /// Sees the request and never answers.
    Silent,
    /// Sees the request and "crashes".
    Exit,
}

struct FakePlayer {
    stop: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    /// Every request with the descriptor it named.
    seen: Arc<Mutex<Vec<(Value, Value)>>>,
    thread: Option<JoinHandle<()>>,
}
impl FakePlayer {
    const PID: i32 = 4242;

    fn start(run: &str, replies: Vec<Reply>) -> Self {
        let (stop, exited) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let request = PlayerCode::request_path(run);
        let thread = {
            let (stop, exited, seen) = (stop.clone(), exited.clone(), seen.clone());
            std::thread::spawn(move || {
                let (mut replies, mut last, mut applied) =
                    (replies.into_iter(), String::new(), 0u64);
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                    let Ok(data) = fs::read(&request) else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_slice::<Value>(&data) else {
                        continue;
                    };
                    let id = value["requestId"].as_str().unwrap().to_string();
                    if id == last {
                        continue;
                    }
                    last = id.clone();
                    // What the bridge and the applier check before they act.
                    assert_eq!(value["schemaVersion"], 1);
                    assert_eq!(
                        file_hash(value["applier"].as_str().unwrap()).unwrap(),
                        value["applierSha256"]
                    );
                    let descriptor = value["descriptor"].as_str().unwrap();
                    assert_eq!(file_hash(descriptor).unwrap(), value["descriptorSha256"]);
                    let descriptor = read_json(descriptor);
                    assert_eq!(descriptor["generation"], value["generation"]);
                    for assembly in descriptor["assemblies"].as_array().unwrap() {
                        assert_eq!(
                            file_hash(assembly["patch"].as_str().unwrap()).unwrap(),
                            assembly["patchSha256"]
                        );
                    }
                    // The applier counts one per redirect of the descriptor.
                    let redirects: usize = descriptor["assemblies"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|assembly| assembly["detours"].as_array().unwrap().len())
                        .sum();
                    let generation = value["generation"].as_u64().unwrap();
                    assert!(generation > applied, "a generation must be newer");
                    seen.lock().unwrap().push((value.clone(), descriptor));
                    let mut ack = json!({"schemaVersion":1,"requestId":id,"pid":Self::PID,"success":false,
                        "generation":generation,"appliedGeneration":applied,"restartRequired":false,"error":"",
                        "failures":[],"detoursApplied":0,"detoursRestored":0,"islandWrites":0,"gatedWrites":0,
                        "callbacksInvoked":0,"callbackErrors":0,"inliningDisabled":true,"elapsedSeconds":0.001,
                        "completedUtcUnixMilliseconds":0});
                    match replies.next().expect("an unexpected request") {
                        Reply::Apply {
                            restored,
                            callback_errors,
                        } => {
                            applied = generation;
                            ack["success"] = true.into();
                            ack["appliedGeneration"] = generation.into();
                            ack["detoursApplied"] = redirects.into();
                            ack["detoursRestored"] = restored.into();
                            ack["callbackErrors"] = callback_errors.into();
                        }
                        Reply::ApplyShort => {
                            ack["success"] = true.into();
                            ack["appliedGeneration"] = generation.into();
                            ack["detoursApplied"] = (redirects - 1).into();
                        }
                        Reply::ApplyLong => {
                            ack["success"] = true.into();
                            ack["appliedGeneration"] = generation.into();
                            ack["detoursApplied"] = (redirects + 1).into();
                        }
                        Reply::Refuse(failures) => {
                            ack["error"] = failures[0].3.into();
                            ack["failures"] = failures
                                .iter()
                                .map(|(assembly, member, stage, message)| json!({"assembly":assembly,"member":member,"stage":stage,"message":message}))
                                .collect();
                        }
                        Reply::RefuseBare(error) => ack["error"] = error.into(),
                        Reply::RefuseAt(other) => ack["appliedGeneration"] = other.into(),
                        Reply::Restart(error) => {
                            ack["restartRequired"] = true.into();
                            ack["error"] = error.into();
                        }
                        Reply::WrongPid => {
                            ack["success"] = true.into();
                            ack["appliedGeneration"] = generation.into();
                            ack["detoursApplied"] = redirects.into();
                            ack["pid"] = 1.into();
                        }
                        Reply::WrongGeneration => {
                            ack["success"] = true.into();
                            ack["detoursApplied"] = redirects.into();
                        }
                        Reply::Garbage => ack = json!({"requestId":id,"pid":"many"}),
                        Reply::Silent => continue,
                        Reply::Exit => {
                            exited.store(true, Ordering::SeqCst);
                            continue;
                        }
                    }
                    let path = format!("{request}.ack.json");
                    fs::write(format!("{path}.tmp"), ack.to_string()).unwrap();
                    fs::rename(format!("{path}.tmp"), &path).unwrap();
                }
            })
        };
        Self {
            stop,
            exited,
            seen,
            thread: Some(thread),
        }
    }
    fn seen(&self) -> Vec<(Value, Value)> {
        self.seen.lock().unwrap().clone()
    }
}
impl Drop for FakePlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // A failed check of the stand-in fails the test that used it.
        if let Err(panic) = self.thread.take().unwrap().join()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(panic);
        }
    }
}

/// What the scripted builder answers to its next request.
enum Build {
    /// A patch of the named assemblies with these members.
    Patch(Vec<(&'static str, Vec<&'static str>)>),
    PatchWithNote(&'static str),
    /// The verdict `patch` without an assembly that carries one.
    PatchOfNothing,
    Unchanged,
    /// Nothing to patch, and something the Player does not take over.
    UnchangedWithNotes(Vec<Reason>),
    Unsupported(Vec<Reason>),
    Fail(&'static str),
    Garbage,
    /// A result whose patch file is not what its hash says.
    Torn,
}

struct Rig {
    _guard: TempDir,
    root: String,
    run: String,
    code: PlayerCode,
    current: Vec<Assembly>,
    script: RefCell<Vec<Build>>,
    /// Every request the builder was given.
    requests: RefCell<Vec<Value>>,
}
impl Rig {
    /// A Player that loaded `Assembly-CSharp` and `Game.Core`, with the
    /// newest compile equal to what it loaded.
    fn new() -> Self {
        let (guard, root) = temp();
        let run = sysutil::join(&[&root, "runs/r1"]);
        fs::create_dir_all(&run).unwrap();
        let applier = write(
            &sysutil::join(&[&root, "applier/UnityMicroEditor.HotReload.dll"]),
            b"applier",
        );
        let loaded: Vec<Assembly> = ["Assembly-CSharp", "Game.Core"]
            .iter()
            .map(|name| {
                let path = write(
                    &sysutil::join(&[&root, "player/Managed", &format!("{name}.dll")]),
                    format!("{name} loaded").as_bytes(),
                );
                Assembly {
                    name: name.to_string(),
                    sha256: file_hash(&path).unwrap(),
                    path,
                    netcode: false,
                }
            })
            .collect();
        let code = PlayerCode::new(
            &run,
            Applier {
                sha256: file_hash(&applier).unwrap(),
                path: applier,
            },
            loaded.clone(),
        );
        Self {
            _guard: guard,
            root,
            run,
            code,
            current: loaded,
            script: RefCell::new(Vec::new()),
            requests: RefCell::new(Vec::new()),
        }
    }
    /// A new compile of `name` with other bytes.
    fn compile(&mut self, name: &str, revision: u32) {
        let path = write(
            &sysutil::join(&[
                &self.root,
                &format!("compile/{revision}/Managed/{name}.dll"),
            ]),
            format!("{name} revision {revision}").as_bytes(),
        );
        let assembly = self.current.iter_mut().find(|a| a.name == name).unwrap();
        assembly.sha256 = file_hash(&path).unwrap();
        assembly.path = path;
    }
    /// The newest compile is what the Player loaded again.
    fn revert(&mut self) {
        self.current = self.code.loaded().to_vec();
    }
    fn builds(&self, script: Vec<Build>) {
        *self.script.borrow_mut() = script;
    }
    fn build(&self, request: &str) -> Result<Vec<u8>> {
        let request = read_json(request);
        self.requests.borrow_mut().push(request.clone());
        let generation = request["generation"].as_u64().unwrap();
        let output = request["output"].as_str().unwrap();
        let listed: Vec<&str> = request["assemblies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|assembly| assembly["name"].as_str().unwrap())
            .collect();
        let step = self.script.borrow_mut().remove(0);
        let assembly = |name: &str, verdict: &str, members: &[&str], good: bool| {
            let mut patch = json!({"assemblyName":"","path":"","sha256":"","pdb":""});
            if verdict == "patch" {
                let path = write(
                    &sysutil::join(&[output, &format!("{name}.HotReload.{generation}.dll")]),
                    format!("patch {name} {generation}").as_bytes(),
                );
                let hash = match good {
                    true => file_hash(&path).unwrap(),
                    false => sha256(b"something else"),
                };
                patch = json!({"assemblyName":format!("{name}.HotReload.{generation}"),"path":path,"sha256":hash,"pdb":""});
            }
            let detours: Vec<Value> = members
                .iter()
                .enumerate()
                .map(|(index, member)| json!({"member":member,"kind":"changed","originalToken":0x06000001usize + index,"replacementToken":0x06000010usize + index}))
                .collect();
            json!({"name":name,"loadedMvid":"0fcce5cc-d5b5-44b3-8918-d49b7c293987","verdict":verdict,"patch":patch,
                "detours":detours,"addedMethods":[],"addedFields":[],"patchMembers":[],"changes":[],"unsupported":[],"notes":[]})
        };
        let result = |verdict: &str, assemblies: Vec<Value>, patched: usize| {
            json!({"schemaVersion":1,"generation":generation,"verdict":verdict,"assemblies":assemblies,
                "summary":{"patchedMethods":patched,"addedMethods":0,"addedFields":0,"unsupported":0,"notes":0},
                "timingsMs":{"read":1,"diff":1,"emit":1}})
        };
        let document = match step {
            Build::Patch(patches) => {
                let count: usize = patches.iter().map(|(_, members)| members.len()).sum();
                let assemblies = listed
                    .iter()
                    .map(
                        |name| match patches.iter().find(|(patched, _)| patched == name) {
                            Some((_, members)) => assembly(name, "patch", members, true),
                            None => assembly(name, "unchanged", &[], true),
                        },
                    )
                    .collect();
                result("patch", assemblies, count)
            }
            Build::PatchWithNote(code) => {
                let mut patched = assembly(listed[0], "patch", &["Game.A::M()"], true);
                patched["notes"] = json!([{"code":code,"member":"Game.A::f","message":"note"}]);
                result("patch", vec![patched], 1)
            }
            Build::PatchOfNothing => result(
                "patch",
                vec![assembly(listed[0], "unchanged", &[], true)],
                1,
            ),
            Build::Unchanged => result(
                "unchanged",
                listed
                    .iter()
                    .map(|name| assembly(name, "unchanged", &[], true))
                    .collect(),
                0,
            ),
            Build::UnchangedWithNotes(notes) => {
                let mut noted = assembly(listed[0], "unchanged", &[], true);
                noted["notes"] = serde_json::to_value(&notes).unwrap();
                result("unchanged", vec![noted], 0)
            }
            Build::Unsupported(reasons) => {
                let mut refused = assembly(listed[0], "unsupported", &[], true);
                refused["unsupported"] = serde_json::to_value(&reasons).unwrap();
                result("unsupported", vec![refused], 0)
            }
            Build::Torn => result(
                "patch",
                vec![assembly(listed[0], "patch", &["Game.A::M()"], false)],
                1,
            ),
            Build::Fail(message) => return Err(anyhow!("dotnet: exit status 1\n{message}\n")),
            Build::Garbage => return Ok(b"warning: something\n{".to_vec()),
        };
        Ok(document.to_string().into_bytes())
    }
    fn attempt_with(
        &mut self,
        player: &FakePlayer,
        ctx: &Context,
        timeout: Duration,
    ) -> Result<Outcome> {
        let (current, exited) = (self.current.clone(), player.exited.clone());
        let this: &Self = self;
        let build = |_: &Context, request: &str| this.build(request);
        let exited = move || exited.load(Ordering::SeqCst);
        let attempt = Attempt {
            current: &current,
            build: &build,
            exited: &exited,
            pid: FakePlayer::PID,
            source_changed: SystemTime::UNIX_EPOCH + Duration::from_millis(1500),
            timeout,
        };
        // The scripted builder borrows the rig, the state is changed by the attempt.
        let mut code = self.code.clone();
        let outcome = code.attempt(ctx, &attempt);
        self.code = code;
        outcome
    }
    fn attempt(&mut self, player: &FakePlayer) -> Outcome {
        self.attempt_with(player, &Context::background(), Duration::from_secs(10))
            .unwrap()
    }
    fn file(&self, generation: u64, name: &str) -> String {
        sysutil::join(&[&self.run, "hot-reload", &generation.to_string(), name])
    }
    /// What the builder run of `generation` gives when it finds nothing to patch.
    fn unchanged(&self, generation: u64, notes: Vec<Reason>) -> Outcome {
        Outcome::Unchanged(Unchanged {
            notes,
            report: self.file(generation, "report.json"),
        })
    }
}

fn patched(outcome: Outcome) -> Patched {
    match outcome {
        Outcome::Patched(patch) => patch,
        other => panic!("expected a patch, got {other:?}"),
    }
}

#[test]
fn a_patch_is_applied_recorded_and_followed_by_the_next_generation() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(
        &rig.run,
        vec![
            Reply::Apply {
                restored: 0,
                callback_errors: 0,
            },
            Reply::Apply {
                restored: 1,
                callback_errors: 0,
            },
        ],
    );
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::Patch(vec![(
        "Assembly-CSharp",
        vec!["Game.A::M()", "Game.A::N()"],
    )])]);
    let patch = patched(rig.attempt(&player));
    assert_eq!(
        patch,
        Patched {
            generation: 1,
            assemblies: vec!["Assembly-CSharp".into()],
            patched_methods: 2,
            members: vec!["Game.A::M()".into(), "Game.A::N()".into()],
            report: rig.file(1, "report.json"),
            ..Patched::default()
        }
    );
    // Only the assembly whose bytes differ is listed, with the DLL the Player loaded.
    let request = rig.requests.borrow()[0].clone();
    assert_eq!(
        request,
        json!({"schemaVersion":1,"generation":1,"output":sysutil::join(&[&rig.run, "hot-reload/1"]),
            "runtimeAssembly":"UnityMicroEditor.HotReload",
            "assemblies":[{"name":"Assembly-CSharp","loaded":rig.code.loaded()[0].path,"current":rig.current[0].path,"netcode":false}]})
    );
    assert_eq!(read_json(&rig.file(1, "request.json")), request);
    // What the Player was sent.
    let (sent, descriptor) = player.seen()[0].clone();
    assert_eq!(sent["generation"], 1);
    assert_eq!(sent["applier"], rig.code.applier().path);
    assert_eq!(sent["descriptor"], rig.file(1, "apply.json"));
    assert_eq!(sent["sourceChangedUtcUnixMilliseconds"], 1500);
    assert_eq!(
        descriptor["gameAssemblies"],
        json!(["Assembly-CSharp", "Game.Core"])
    );
    assert_eq!(
        descriptor["assemblies"][0]["detours"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // The report holds the builder's result and the acknowledgement in full.
    let report = read_json(&rig.file(1, "report.json"));
    assert_eq!(report["outcome"], "patched");
    assert_eq!(report["result"], read_json(&rig.file(1, "result.json")));
    assert_eq!(report["ack"], read_json(&rig.file(1, "ack.json")));
    assert_eq!(report["ack"]["appliedGeneration"], 1);
    assert_eq!(
        (
            rig.code.applied_generation(),
            rig.code.redirected(),
            rig.code.refused()
        ),
        (1, true, false)
    );
    assert_eq!(rig.code.applied_result(), rig.file(1, "result.json"));

    // The next edit: generation 2, cumulative, with the applied result as A2 asks.
    rig.compile("Assembly-CSharp", 2);
    rig.compile("Game.Core", 2);
    rig.builds(vec![Build::Patch(vec![("Game.Core", vec!["Core.B::M()"])])]);
    let patch = patched(rig.attempt(&player));
    assert_eq!((patch.generation, patch.restored_methods), (2, 1));
    assert_eq!(patch.assemblies, ["Game.Core"]);
    let request = rig.requests.borrow()[1].clone();
    assert_eq!(request["previousResult"], rig.file(1, "result.json"));
    assert_eq!(request["assemblies"].as_array().unwrap().len(), 2);
    assert_eq!(rig.code.applied_result(), rig.file(2, "result.json"));
    assert_eq!(
        player.seen()[1].1["assemblies"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn equal_bytes_need_neither_builder_nor_player() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, Vec::new());
    assert_eq!(
        rig.attempt(&player),
        Outcome::Unchanged(Unchanged::default())
    );
    assert!(rig.requests.borrow().is_empty());
    assert!(!fs::exists(sysutil::join(&[&rig.run, "hot-reload"])).unwrap());
    // A difference without meaning, such as a comment: nothing is sent, and
    // the generation of the builder run is not used again.
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::Unchanged, Build::Unchanged]);
    assert_eq!(rig.attempt(&player), rig.unchanged(1, Vec::new()));
    assert_eq!(
        read_json(&rig.file(1, "report.json"))["outcome"],
        "unchanged"
    );
    assert_eq!(rig.attempt(&player), rig.unchanged(2, Vec::new()));
    assert_eq!(rig.requests.borrow()[1]["generation"], 2);
    assert!(player.seen().is_empty());
    assert!(!fs::exists(PlayerCode::request_path(&rig.run)).unwrap());
    assert_eq!(
        (rig.code.applied_generation(), rig.code.refused()),
        (0, false)
    );
}

#[test]
fn the_notes_of_a_change_that_needs_no_patch_reach_the_caller() {
    // A field made public: no body differs, so there is nothing to send.
    // What the builder says about it is all the caller ever learns.
    let notes = vec![
        Reason::new(
            "accessibility_changed",
            "Game.A::speed",
            "Field Game.A::speed changed accessibility; Unity serialization and reflection keep the loaded accessibility until a full recompile.",
        ),
        Reason::new("runtime_initializer_removed", "Game.A::Boot()", "removed"),
    ];
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, Vec::new());
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::UnchangedWithNotes(notes.clone())]);
    let outcome = rig.attempt(&player);
    assert_eq!(outcome, rig.unchanged(1, notes.clone()));
    // The report the outcome names holds the builder's result in full.
    let report = read_json(&rig.file(1, "report.json"));
    assert_eq!(report["outcome"], "unchanged");
    assert_eq!(
        report["result"]["assemblies"][0]["notes"],
        serde_json::to_value(&notes).unwrap()
    );
    assert!(player.seen().is_empty());
    assert_eq!(
        (rig.code.applied_generation(), rig.code.refused()),
        (0, false)
    );
}

#[test]
fn a_reverted_edit_restores_the_loaded_code_with_an_empty_descriptor() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(
        &rig.run,
        vec![
            Reply::Apply {
                restored: 0,
                callback_errors: 0,
            },
            Reply::Apply {
                restored: 2,
                callback_errors: 0,
            },
        ],
    );
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::Patch(vec![(
        "Assembly-CSharp",
        vec!["Game.A::M()", "Game.A::N()"],
    )])]);
    patched(rig.attempt(&player));
    // The sources are what the Player loaded again: no byte differs, but two
    // of its methods still jump into generation 1.
    rig.revert();
    rig.builds(vec![Build::Unchanged]);
    let restored = patched(rig.attempt(&player));
    assert_eq!(
        restored,
        Patched {
            generation: 2,
            restored_methods: 2,
            report: rig.file(2, "report.json"),
            ..Patched::default()
        }
    );
    // The builder compares the loaded assembly with what generation 1 defined.
    let request = rig.requests.borrow()[1].clone();
    let loaded = &rig.code.loaded()[0].path;
    assert_eq!(
        request["assemblies"],
        json!([{"name":"Assembly-CSharp","loaded":loaded,"current":loaded,"netcode":false}])
    );
    assert_eq!(request["previousResult"], rig.file(1, "result.json"));
    assert_eq!(player.seen()[1].1["assemblies"], json!([]));
    assert_eq!(
        (
            rig.code.applied_generation(),
            rig.code.redirected(),
            rig.code.applied_result()
        ),
        (2, false, "")
    );
    // Nothing is applied any more, so equal bytes are simply unchanged.
    assert_eq!(
        rig.attempt(&player),
        Outcome::Unchanged(Unchanged::default())
    );
    assert_eq!(rig.requests.borrow().len(), 2);
}

#[test]
fn an_unsupported_edit_is_refused_without_touching_the_player() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(
        &rig.run,
        vec![Reply::Apply {
            restored: 0,
            callback_errors: 0,
        }],
    );
    rig.compile("Assembly-CSharp", 1);
    let reasons = vec![
        Reason::new(
            "type_added",
            "Game.Added",
            "Type Game.Added was added or renamed.",
        ),
        Reason::new("generic_method", "Game.Box`1::Get()", "m"),
    ];
    rig.builds(vec![
        Build::Unsupported(reasons.clone()),
        Build::Unsupported(Vec::new()),
        Build::Patch(vec![("Assembly-CSharp", vec!["Game.A::M()"])]),
    ]);
    assert_eq!(
        rig.attempt(&player),
        Outcome::Unsupported(Refusal {
            reasons,
            report: rig.file(1, "report.json"),
        })
    );
    assert!(rig.code.refused());
    let report = read_json(&rig.file(1, "report.json"));
    assert_eq!(
        (&report["outcome"], &report["ack"]),
        (&json!("unsupported"), &Value::Null)
    );
    assert_eq!(
        report["result"]["assemblies"][0]["unsupported"][1]["code"],
        "generic_method"
    );
    assert!(player.seen().is_empty());
    // A refusal without a reason still is one.
    match rig.attempt(&player) {
        Outcome::Unsupported(refusal) => assert_eq!(refusal.reasons[0].code, CODE_BUILDER_FAILED),
        other => panic!("{other:?}"),
    }
    // The edit is fixed: the next generation applies and the refusal is over.
    rig.compile("Assembly-CSharp", 2);
    assert_eq!(patched(rig.attempt(&player)).generation, 3);
    assert!(!rig.code.refused());
    assert_eq!(player.seen()[0].0["generation"], 3);
}

#[test]
fn a_failing_builder_is_a_refusal_with_its_text() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, Vec::new());
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![
        Build::Fail("The assembly /x.dll does not exist."),
        Build::Garbage,
        Build::Torn,
        Build::PatchOfNothing,
    ]);
    for (generation, expected) in [
        (
            1,
            "dotnet: exit status 1\nThe assembly /x.dll does not exist.",
        ),
        (2, "patch builder result is not its JSON"),
        (3, "patch assembly differs from the hash of its result"),
        // An empty descriptor would restore the loaded code and be
        // acknowledged; it is not what a patch verdict means.
        (
            4,
            "patch builder result has the verdict patch and no patched assembly",
        ),
    ] {
        let Outcome::Unsupported(refusal) = rig.attempt(&player) else {
            panic!("generation {generation} must be refused");
        };
        assert_eq!(refusal.reasons.len(), 1);
        assert_eq!(refusal.reasons[0].code, CODE_BUILDER_FAILED);
        assert!(refusal.reasons[0].message.contains(expected), "{refusal:?}");
        assert_eq!(refusal.report, rig.file(generation, "report.json"));
        let report = read_json(&refusal.report);
        assert_eq!(report["outcome"], "unsupported");
        assert!(report["error"].as_str().unwrap().contains(expected));
    }
    assert!(player.seen().is_empty() && rig.code.refused());
}

#[test]
fn a_refused_apply_keeps_the_generation_and_gives_its_stages() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(
        &rig.run,
        vec![
            Reply::Apply {
                restored: 0,
                callback_errors: 0,
            },
            Reply::Refuse(vec![
                (
                    "Assembly-CSharp",
                    "Game.A::M()",
                    "compile",
                    "The JIT refused the replacement.",
                ),
                ("", "", "inlining", "The JIT inlines."),
            ]),
            Reply::RefuseBare("Code reload applier is missing or exceeds the size limit."),
            Reply::RefuseAt(7),
        ],
    );
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![
        Build::Patch(vec![("Assembly-CSharp", vec!["Game.A::M()"])]),
        Build::Patch(vec![("Assembly-CSharp", vec!["Game.A::M()"])]),
        Build::Patch(vec![("Assembly-CSharp", vec!["Game.A::M()"])]),
        Build::Patch(vec![("Assembly-CSharp", vec!["Game.A::M()"])]),
    ]);
    patched(rig.attempt(&player));
    rig.compile("Assembly-CSharp", 2);
    assert_eq!(
        rig.attempt(&player),
        Outcome::ApplyFailed(Refusal {
            reasons: vec![
                Reason::new(
                    CODE_APPLY_FAILED,
                    "Assembly-CSharp: Game.A::M()",
                    "compile: The JIT refused the replacement."
                ),
                Reason::new(CODE_APPLY_FAILED, "", "inlining: The JIT inlines."),
            ],
            report: rig.file(2, "report.json"),
        })
    );
    // Nothing was written: the Player runs generation 1, whose result stays
    // the base of the next request.
    assert_eq!(
        (
            rig.code.applied_generation(),
            rig.code.refused(),
            rig.code.applied_result()
        ),
        (1, true, rig.file(1, "result.json").as_str())
    );
    assert_eq!(
        read_json(&rig.file(2, "report.json"))["ack"]["success"],
        false
    );
    // The bridge answered for an applier that did not run.
    match rig.attempt(&player) {
        Outcome::ApplyFailed(refusal) => assert_eq!(
            refusal.reasons,
            [Reason::new(
                CODE_APPLY_FAILED,
                "",
                "Code reload applier is missing or exceeds the size limit."
            )]
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        rig.requests.borrow()[2]["previousResult"],
        rig.file(1, "result.json")
    );
    // A Player that names another generation than this side applied is not known.
    match rig.attempt(&player) {
        Outcome::RestartRequired(failure) => {
            assert!(
                failure
                    .error
                    .contains("generation 7 where generation 1 was applied"),
                "{failure:?}"
            );
            assert_eq!(read_json(&failure.report)["outcome"], "restart_required");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_uncertain_player_needs_a_restart() {
    for (reply, expected) in [
        (
            Reply::Restart("An exception interrupted the write."),
            "An exception interrupted the write.",
        ),
        (Reply::Restart(""), "must be restarted"),
        (
            Reply::WrongPid,
            "process 1 acknowledged the code patch of Player 4242",
        ),
        (
            Reply::WrongGeneration,
            "acknowledged code generation 0 for the request of generation 1",
        ),
        (Reply::Garbage, "acknowledgement is unreadable"),
        // A Player that says it applied a patch it did not apply in full.
        (
            Reply::ApplyShort,
            "the Player acknowledged 1 of the 2 redirects of code generation 1",
        ),
        // Or more than it was sent: neither number is the patch.
        (
            Reply::ApplyLong,
            "the Player acknowledged 3 of the 2 redirects of code generation 1",
        ),
    ] {
        let mut rig = Rig::new();
        let player = FakePlayer::start(&rig.run, vec![reply]);
        rig.compile("Assembly-CSharp", 1);
        rig.builds(vec![Build::Patch(vec![(
            "Assembly-CSharp",
            vec!["Game.A::M()", "Game.A::N()"],
        )])]);
        match rig.attempt(&player) {
            Outcome::RestartRequired(failure) => {
                assert!(failure.error.contains(expected), "{failure:?}");
                let report = read_json(&failure.report);
                assert_eq!(report["outcome"], "restart_required");
                assert_eq!(report["error"], failure.error);
                assert!(fs::exists(rig.file(1, "ack.json")).unwrap());
            }
            other => panic!("{expected}: {other:?}"),
        }
        assert_eq!(
            (rig.code.applied_generation(), rig.code.refused()),
            (0, false)
        );
    }
}

#[test]
fn a_player_that_exits_or_stays_silent_ends_the_wait() {
    // A crash during the patch is seen at once, not after the timeout.
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, vec![Reply::Exit]);
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::Patch(vec![(
        "Assembly-CSharp",
        vec!["Game.A::M()"],
    )])]);
    let started = Instant::now();
    match rig.attempt(&player) {
        Outcome::PlayerExited(failure) => {
            assert_eq!(read_json(&failure.report)["outcome"], "player_exited");
            assert_eq!(read_json(&failure.report)["ack"], Value::Null);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // A Player that is gone before the attempt is not asked at all.
    rig.builds(Vec::new());
    assert!(matches!(rig.attempt(&player), Outcome::PlayerExited(_)));
    assert_eq!(rig.requests.borrow().len(), 1);

    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, vec![Reply::Silent]);
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::Patch(vec![(
        "Assembly-CSharp",
        vec!["Game.A::M()"],
    )])]);
    let started = Instant::now();
    let outcome = rig
        .attempt_with(&player, &Context::background(), Duration::from_millis(300))
        .unwrap();
    let elapsed = started.elapsed();
    match outcome {
        Outcome::TimedOut(failure) => {
            assert!(failure.error.contains("did not acknowledge"), "{failure:?}");
            assert_eq!(read_json(&failure.report)["outcome"], "timed_out");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(300) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
    assert_eq!(player.seen().len(), 1);

    // The end of the run ends the wait; nothing is concluded.
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, vec![Reply::Silent]);
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![Build::Patch(vec![(
        "Assembly-CSharp",
        vec!["Game.A::M()"],
    )])]);
    let ctx = Context::background().with_timeout(Duration::from_millis(200));
    let started = Instant::now();
    assert!(
        rig.attempt_with(&player, &ctx, Duration::from_secs(30))
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn generations_start_at_one_and_are_never_reused() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(
        &rig.run,
        vec![
            Reply::Refuse(vec![("A", "M", "resolve", "m")]),
            Reply::Apply {
                restored: 0,
                callback_errors: 2,
            },
        ],
    );
    rig.compile("Assembly-CSharp", 1);
    rig.builds(vec![
        Build::Unsupported(vec![Reason::new("type_added", "T", "m")]),
        Build::Fail("broken"),
        Build::Patch(vec![("Assembly-CSharp", vec!["Game.A::M()"])]),
        Build::PatchWithNote("existing_instances_keep_default"),
    ]);
    assert!(matches!(rig.attempt(&player), Outcome::Unsupported(_)));
    assert!(matches!(rig.attempt(&player), Outcome::Unsupported(_)));
    assert!(matches!(rig.attempt(&player), Outcome::ApplyFailed(_)));
    let patch = patched(rig.attempt(&player));
    assert_eq!(patch.generation, 4);
    // Callbacks of the game that failed do not fail the patch; the caller is told.
    assert_eq!(patch.notes.len(), 2);
    assert_eq!(patch.notes[0].code, "existing_instances_keep_default");
    assert_eq!(patch.notes[1].code, CODE_CALLBACK_FAILED);
    let generations: Vec<u64> = rig
        .requests
        .borrow()
        .iter()
        .map(|request| request["generation"].as_u64().unwrap())
        .collect();
    assert_eq!(generations, [1, 2, 3, 4]);
    let sent: Vec<u64> = player
        .seen()
        .iter()
        .map(|(request, _)| request["generation"].as_u64().unwrap())
        .collect();
    assert_eq!(sent, [3, 4]);
    // The first applied patch has no earlier result to compare with.
    assert!(rig.requests.borrow()[3].get("previousResult").is_none());
    // A new process starts at 1 again.
    let fresh = PlayerCode::new(
        &rig.run,
        rig.code.applier().clone(),
        rig.code.loaded().to_vec(),
    );
    assert_eq!(
        (
            fresh.applied_generation(),
            fresh.redirected(),
            fresh.refused()
        ),
        (0, false, false)
    );
}

#[test]
fn an_assembly_the_player_did_not_load_is_refused() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, Vec::new());
    rig.current.push(Assembly {
        name: "Game.New".into(),
        path: "/x/Game.New.dll".into(),
        sha256: "00".into(),
        netcode: false,
    });
    match rig.attempt(&player) {
        Outcome::Unsupported(refusal) => {
            assert_eq!(refusal.reasons[0].code, "assembly_not_loaded");
            assert_eq!(refusal.reasons[0].member, "Game.New");
        }
        other => panic!("{other:?}"),
    }
    assert!(rig.code.refused() && rig.requests.borrow().is_empty());
}

#[test]
fn netcode_assemblies_are_named_to_the_builder() {
    let mut rig = Rig::new();
    let player = FakePlayer::start(&rig.run, Vec::new());
    rig.compile("Game.Core", 1);
    rig.current[1].netcode = true;
    rig.builds(vec![Build::Unchanged]);
    assert_eq!(rig.attempt(&player), rig.unchanged(1, Vec::new()));
    assert_eq!(
        rig.requests.borrow()[0]["assemblies"],
        json!([{"name":"Game.Core","loaded":rig.code.loaded()[1].path,"current":rig.current[1].path,"netcode":true}])
    );
}

#[test]
fn the_wait_takes_only_the_acknowledgement_of_its_request() {
    let (_guard, root) = temp();
    let path = sysutil::join(&[&root, "request.json.ack.json"]);
    let ctx = Context::background();
    let never = || false;
    let wait =
        |id: &str| wait_code_ack(&ctx, &path, id, Duration::from_millis(60), &never).unwrap();
    assert!(matches!(wait("a"), Answer::TimedOut));
    // The acknowledgement of the request before is still there.
    fs::write(
        &path,
        r#"{"requestId":"a","success":true,"appliedGeneration":1}"#,
    )
    .unwrap();
    assert!(matches!(wait("b"), Answer::TimedOut));
    match wait("a") {
        Answer::Ack(ack, data) => {
            assert!(ack.success && ack.applied_generation == 1);
            assert_eq!(data, fs::read(&path).unwrap());
        }
        other => panic!("{other:?}"),
    }
    // A file that is being replaced reads as nothing, not as an error.
    fs::write(&path, r#"{"requestId":"a","succ"#).unwrap();
    assert!(matches!(wait("a"), Answer::TimedOut));
    assert!(matches!(
        wait_code_ack(&ctx, &path, "a", Duration::from_secs(5), &|| true).unwrap(),
        Answer::Exited
    ));
    let ended = Context::background().with_cancel();
    ended.cancel();
    assert!(wait_code_ack(&ended, &path, "a", Duration::from_secs(5), &never).is_err());
}

/// A session that cannot build its patch programs learns it from the setup
/// and goes on without hot reload; one that ends first leaves no build behind.
#[test]
fn setup_reports_a_missing_sdk_and_ends_with_its_session() {
    let (_guard, root) = temp();
    let ctx = Context::background();
    let setup = Setup::start(
        &ctx,
        "/nonexistent/Editor/Unity",
        "/nonexistent/Managed",
        &root,
    );
    let error = setup.finish().unwrap_err();
    assert_eq!(error.to_string(), "installed Unity Roslyn SDK is missing");
    let started = Instant::now();
    drop(Setup::start(
        &ctx,
        "/nonexistent/Editor/Unity",
        "/nonexistent/Managed",
        &root,
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(ctx.error().is_none(), "the setup must not end its session");
}

/// A patch builder that does not end is stopped at its deadline and is a
/// failed build like any other; the session's own end stays what it is.
#[test]
fn a_builder_that_does_not_end_is_stopped() {
    use std::os::unix::fs::PermissionsExt;
    let (_guard, root) = temp();
    let stuck = write(
        &sysutil::join(&[&root, "dotnet"]),
        b"#!/bin/sh\necho started\nexec sleep 30\n",
    );
    fs::set_permissions(&stuck, fs::Permissions::from_mode(0o700)).unwrap();
    let tools = Tools {
        sdk: Sdk {
            dotnet: stuck,
            csc: String::new(),
        },
        patcher: sysutil::join(&[&root, "HotReloadPatcher.dll"]),
        ..Tools::default()
    };
    let request = write(&sysutil::join(&[&root, "1/request.json"]), b"{}");
    let started = Instant::now();
    let error = tools
        .build_within(&Context::background(), &request, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the patch builder did not finish within 1s and was stopped"
    );
    let waited = started.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(20)).contains(&waited),
        "{waited:?}"
    );
    // The call the runner makes applies the limit of the session's tools:
    // without one this builder would be waited for until it ends by itself.
    let limited = Tools {
        limit: Some(Duration::from_secs(1)),
        sdk: tools.sdk.clone(),
        patcher: tools.patcher.clone(),
        ..Tools::default()
    };
    let started = Instant::now();
    let error = limited.build(&Context::background(), &request).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the patch builder did not finish within 1s and was stopped"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    // The end of the session is not a deadline of the builder.
    let ctx = Context::background().with_cancel();
    ctx.cancel();
    let error = tools
        .build_within(&ctx, &request, Duration::from_secs(600))
        .unwrap_err();
    assert!(!error.to_string().contains("did not finish"), "{error}");
    // A run that ends in time is not touched by the limit.
    let quick = write(
        &sysutil::join(&[&root, "quick"]),
        b"#!/bin/sh\nprintf '{\"verdict\":\"unchanged\"}'\n",
    );
    fs::set_permissions(&quick, fs::Permissions::from_mode(0o700)).unwrap();
    let tools = Tools {
        sdk: Sdk {
            dotnet: quick,
            csc: String::new(),
        },
        ..tools
    };
    assert_eq!(
        tools.build(&Context::background(), &request).unwrap(),
        b"{\"verdict\":\"unchanged\"}"
    );
    // The documented limit of a session.
    assert_eq!(BUILD_TIMEOUT, Duration::from_secs(60));
    assert!(tools.limit.is_none());
}

// SDK tests

fn sdk(reason: &str) -> Option<String> {
    let value = std::env::var("UME_LIFECYCLE_SDK")
        .ok()
        .filter(|value| !value.is_empty());
    if value.is_none() {
        eprintln!("skipped: {reason}");
    }
    value
}

/// Compiles one fixture as game assemblies are compiled.
fn compile_fixture(ctx: &Context, sdk: &Sdk, editor: &str, source: &str, output: &str) -> String {
    let bcl = sysutil::join(&[
        &sysutil::dir(editor),
        "Data/MonoBleedingEdge/lib/mono/unityaot-linux/mscorlib.dll",
    ]);
    let dll = sysutil::join(&[output, "Assembly-CSharp.dll"]);
    fs::create_dir_all(output).unwrap();
    compiler_command(
        ctx,
        output,
        &sdk.dotnet,
        &[
            &sdk.csc,
            "-nologo",
            "-target:library",
            "-nostdlib+",
            "-langversion:9.0",
            "-deterministic+",
            "-debug:portable",
            "-optimize-",
            &format!("-reference:{bcl}"),
            &format!("-out:{dll}"),
            &format!("{TESTDATA}/{source}"),
        ],
    )
    .unwrap();
    dll
}

/// The patch builder is built from the tool's sources through the session's
/// own build function and run on two compiled revisions of a fixture; then a
/// whole attempt runs with it against the stand-in Player.
#[test]
fn hot_reload_patch_builder_sdk() {
    let Some(editor) = sdk("the hot reload patch builder needs the Unity SDK") else {
        return;
    };
    let (_guard, root) = temp();
    let ctx = Context::background();
    let sdk = Sdk::locate(&editor).unwrap();
    let cache = sysutil::join(&[&root, "hot-reload/patcher"]);
    let started = Instant::now();
    let patcher = prepare_patcher(&ctx, &sdk, &cache).unwrap();
    eprintln!(
        "patch builder built in {:.2}s: {patcher}",
        started.elapsed().as_secs_f64()
    );
    assert_eq!(sysutil::base(&patcher), "HotReloadPatcher.dll");
    // The entry is complete and verified, and the staging copy is gone.
    let entry = sysutil::dir(&patcher);
    let files: crate::compiler::NilMap<String> =
        read_json_file(&sysutil::join(&[&entry, "files.json"])).unwrap();
    assert!(crate::compiler::verify_cache_files(&entry, &files));
    assert_eq!(
        glob(&sysutil::join(&[&cache, ".helper-build-*"])).unwrap(),
        Vec::<String>::new()
    );
    // A second session reuses it: nothing is built again.
    let written = fs::metadata(&patcher).unwrap().modified().unwrap();
    let started = Instant::now();
    assert_eq!(prepare_patcher(&ctx, &sdk, &cache).unwrap(), patcher);
    assert_eq!(fs::metadata(&patcher).unwrap().modified().unwrap(), written);
    eprintln!(
        "patch builder reused in {:.3}s",
        started.elapsed().as_secs_f64()
    );

    let v1 = compile_fixture(
        &ctx,
        &sdk,
        &editor,
        "Counter.v1.cs",
        &sysutil::join(&[&root, "v1"]),
    );
    let v2 = compile_fixture(
        &ctx,
        &sdk,
        &editor,
        "Counter.v2.cs",
        &sysutil::join(&[&root, "v2"]),
    );
    let v3 = compile_fixture(
        &ctx,
        &sdk,
        &editor,
        "Counter.v3.cs",
        &sysutil::join(&[&root, "v3"]),
    );
    let tools = Tools {
        sdk: sdk.clone(),
        editor: editor.clone(),
        managed: String::new(),
        output: root.clone(),
        patcher,
        limit: None,
    };
    // The command line of the contract, on a request written by hand.
    let output = sysutil::join(&[&root, "direct"]);
    let request = sysutil::join(&[&root, "direct-request.json"]);
    crate::util::atomic_json(
        &request,
        &BuilderRequest {
            schema_version: 1,
            generation: 1,
            output: output.clone(),
            runtime_assembly: RUNTIME_ASSEMBLY.into(),
            previous_result: String::new(),
            assemblies: vec![BuilderAssembly {
                name: "Assembly-CSharp".into(),
                loaded: v1.clone(),
                current: v2.clone(),
                netcode: false,
            }],
        },
    )
    .unwrap();
    let started = Instant::now();
    let printed = tools.build(&ctx, &request).unwrap();
    eprintln!(
        "patch builder ran in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    let result = BuilderResult::decode(&printed, 1).unwrap();
    assert_eq!(
        result.verdict,
        "patch",
        "{}",
        String::from_utf8_lossy(&printed)
    );
    assert_eq!(
        result.members(),
        ["Game.Counter::Label()", "Game.Counter::Next()"]
    );
    assert_eq!(result.summary.patched_methods, 2);
    let patch = &result.assemblies[0].patch;
    assert_eq!(patch.assembly_name, "Assembly-CSharp.HotReload.1");
    assert_eq!(sysutil::dir(&patch.path), output);
    assert_eq!(file_hash(&patch.path).unwrap(), patch.sha256);
    // The recorded result the unit tests decode is what the tool prints.
    let recorded = BuilderResult::decode(&recorded("result-patch.json"), 1).unwrap();
    assert_eq!(result.members(), recorded.members());
    assert_eq!(result.assemblies[0].detours, recorded.assemblies[0].detours);

    // A whole attempt: patch, refusal, and the way back.
    let run = sysutil::join(&[&root, "runs/r1"]);
    fs::create_dir_all(&run).unwrap();
    let applier = write(
        &sysutil::join(&[&root, "applier/UnityMicroEditor.HotReload.dll"]),
        b"applier",
    );
    let assembly = |path: &str| Assembly {
        name: "Assembly-CSharp".into(),
        path: path.into(),
        sha256: file_hash(path).unwrap(),
        netcode: false,
    };
    let mut code = PlayerCode::new(
        &run,
        Applier {
            sha256: file_hash(&applier).unwrap(),
            path: applier,
        },
        vec![assembly(&v1)],
    );
    let player = FakePlayer::start(
        &run,
        vec![
            Reply::Apply {
                restored: 0,
                callback_errors: 0,
            },
            Reply::Apply {
                restored: 2,
                callback_errors: 0,
            },
        ],
    );
    let exited = || false;
    let build = |ctx: &Context, request: &str| tools.build(ctx, request);
    let attempt = |code: &mut PlayerCode, current: &str| {
        code.attempt(
            &ctx,
            &Attempt {
                current: &[assembly(current)],
                build: &build,
                exited: &exited,
                pid: FakePlayer::PID,
                source_changed: SystemTime::now(),
                timeout: Duration::from_secs(20),
            },
        )
        .unwrap()
    };
    let patch = patched(attempt(&mut code, &v2));
    assert_eq!((patch.generation, patch.patched_methods), (1, 2));
    assert_eq!(patch.assemblies, ["Assembly-CSharp"]);
    let (_, descriptor) = player.seen()[0].clone();
    assert_eq!(
        descriptor["assemblies"][0]["detours"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    match attempt(&mut code, &v3) {
        Outcome::Unsupported(refusal) => {
            assert_eq!(refusal.reasons.len(), 1);
            assert_eq!(
                (
                    refusal.reasons[0].code.as_str(),
                    refusal.reasons[0].member.as_str()
                ),
                ("type_added", "Game.Added")
            );
            assert_eq!(
                read_json(&refusal.report)["result"]["verdict"],
                "unsupported"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!((code.applied_generation(), code.refused()), (1, true));
    // The revert: the real builder is given the loaded DLL on both sides and
    // the result of generation 1, and the Player restores its two methods.
    let restored = patched(attempt(&mut code, &v1));
    assert_eq!((restored.generation, restored.restored_methods), (3, 2));
    assert!(restored.assemblies.is_empty() && !code.redirected());
    assert_eq!(player.seen()[1].1["assemblies"], json!([]));
    assert_eq!(player.seen().len(), 2);
}

/// The newest file named `name` below `root`: the lifecycle helper's copy of
/// a source for the last compile.
fn newest_copy(root: &str, name: &str) -> String {
    let mut found: Option<(SystemTime, String)> = None;
    let mut pending = vec![root.to_string()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap().flatten() {
            let path = entry.path().to_string_lossy().into_owned();
            let info = entry.metadata().unwrap();
            if info.is_dir() {
                pending.push(path);
            } else if sysutil::base(&path) == name && path.contains("/Lifecycle/") {
                let modified = info.modified().unwrap();
                if found.as_ref().is_none_or(|(newest, _)| modified > *newest) {
                    found = Some((modified, path));
                }
            }
        }
    }
    found.expect("a rewritten copy of the source").1
}

/// The whole way of an edit without a Player: the real compiler pipeline
/// (lifecycle helper, Roslyn, a Player directory per revision) on a
/// synthetic project, the assemblies as the runner reads them from it, the
/// real patch builder, and the stand-in for the Player.
#[test]
fn hot_reload_compiled_revisions_sdk() {
    use crate::compiler::{
        AssemblyConfig, CompileScript, Compiler, CompilerGraphAssembly, CompilerGraphExport,
        NilVec, UNITY_VERSION, deduplicate_references, prepare_compiler_graph,
    };
    let Some(editor) = sdk("hot reload of compiled revisions needs the Unity SDK") else {
        return;
    };
    let (_guard, project) = temp();
    let ctx = Context::background();
    let file =
        |relative: &str, text: &str| write(&sysutil::join(&[&project, relative]), text.as_bytes());
    file(
        "ProjectSettings/ProjectVersion.txt",
        &format!("m_EditorVersion: {UNITY_VERSION}\n"),
    );
    file("Packages/manifest.json", r#"{"dependencies":{}}"#);
    let mut paths = Compiler {
        editor: editor.clone(),
        ..Default::default()
    };
    paths.initialize_sdk().unwrap();
    let compile = |dll: &str, source: &str, extra: &[&str]| {
        let filename = file(&format!("Temp/initial/{}.cs", sysutil::base(dll)), source);
        fs::create_dir_all(sysutil::dir(dll)).unwrap();
        let mut references: Vec<String> = paths.base_references.to_vec();
        references.extend(paths.engine_references.iter().cloned());
        references.extend(extra.iter().map(|path| path.to_string()));
        let mut arguments: Vec<String> = [
            paths.csc.as_str(),
            "-nologo",
            "-target:library",
            "-nostdlib+",
            "-debug:portable",
            "-unsafe+",
        ]
        .map(String::from)
        .to_vec();
        arguments.push(format!("-out:{dll}"));
        for reference in &deduplicate_references(&references) {
            arguments.push(format!("-reference:{reference}"));
        }
        arguments.push(filename);
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        if let Err(error) = compiler_command(&ctx, &project, &paths.dotnet, &arguments) {
            panic!("compile {dll}: {error}");
        }
    };
    // A host that is never started, with the part of the bridge that
    // instrumented game code is compiled against.
    let player = file(
        "Temp/native-host/Host.x86_64",
        "SDK fixture executable, not launched",
    );
    fs::set_permissions(&player, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let managed = sysutil::join(&[&sysutil::dir(&player), "Host_Data/Managed"]);
    let bridge = sysutil::join(&[&managed, "UnityMicroEditor.dll"]);
    compile(
        &bridge,
        r#"using System; using UnityEngine; namespace UnityMicroEditor.Raw {
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawLifecycleInstrumentedAttribute : Attribute { public RawLifecycleInstrumentedAttribute(int version) {} }
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawScriptableObjectInstrumentedAttribute : Attribute { public RawScriptableObjectInstrumentedAttribute(int version) {} }
public static class RawLifecycle {
 public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
 public static bool ShouldCollect(MonoBehaviour target, Phase phase) => false;
 public static bool ShouldCollect(ScriptableObject target, Phase phase) => false;
 public static void Enqueue(MonoBehaviour target, Phase phase, Action callback, int order) {}
 public static void Enqueue(ScriptableObject target, Phase phase, Action callback, int order) {}
}}"#,
        &[],
    );
    file(
        "Temp/native-host/Host_Data/ScriptingAssemblies.json",
        r#"{"names":["UnityMicroEditor.dll"],"types":[16]}"#,
    );
    // `Awake` has no access modifier and follows a directive, as scripts
    // with regions have it: the helper made of it must not take the
    // directive along. The classes below it have no `Awake`, so the rewrite
    // appends one, in front of a closing brace that follows a directive;
    // one of their callbacks needs `unsafe` for its body.
    const TAILS: &str = "public class ProbeRegions : MonoBehaviour\n{\n    #region Fields\n    public int level;\n    #endregion\n    #region Methods\n    unsafe void OnEnable() { int value = 2; int* pointer = &value; level = *pointer; }\n    void Update() { level++; }\n    #endregion\n}\npublic class ProbeEditor : MonoBehaviour\n{\n    public int checks;\n#if UNITY_EDITOR\n    void OnValidate() { checks++; }\n#endif\n}\npublic class ProbeEmpty : MonoBehaviour\n{\n    #region Nothing yet\n    #endregion\n}\n";
    let probe = |awake: u32, update: u32, tail: &str| {
        format!(
            "using UnityEngine;\npublic class Probe : MonoBehaviour\n{{\n    public int seed = 3;\n    private int frames;\n    #region Unity messages\n    void Awake() {{ seed += {awake}; }}\n    #endregion\n    private void Update() {{ frames += {update}; }}\n    public int Frames() {{ return frames; }}\n}}\n{TAILS}{tail}"
        )
    };
    let source = file("Assets/Probe.cs", &probe(1, 1, ""));
    file(
        "Assets/Probe.cs.meta",
        "guid: 10000000000000000000000000000001\nMonoImporter:\n  executionOrder: 0\n",
    );
    let exported = sysutil::join(&[&project, "Library/ScriptAssemblies/Assembly-CSharp.dll"]);
    compile(&exported, &probe(1, 1, ""), &[&bridge]);
    let graph = CompilerGraphExport {
        schema_version: 1,
        project: project.clone(),
        unity: editor.clone(),
        assemblies: NilVec::from([CompilerGraphAssembly {
            config: AssemblyConfig {
                name: "Assembly-CSharp".into(),
                sources: NilVec::from([source.clone()]),
                defines: NilVec::from(["UNITY_6000_6".to_string()]),
                config: crate::compiler::AssemblyOptions {
                    allow_unsafe: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            path: exported.clone(),
            references: NilVec::from(["UnityMicroEditor".to_string()]),
            ..Default::default()
        }]),
        scripts: NilVec::from([CompileScript {
            guid: "10000000000000000000000000000001".into(),
            path: source.clone(),
            assembly: "Assembly-CSharp".into(),
            type_name: "Probe".into(),
            host_provided: false,
        }]),
        compiler_inputs: crate::compiler::NilMap::make(),
        ..Default::default()
    };
    let graph_file = sysutil::join(&[&project, "Temp/export.json"]);
    crate::util::atomic_json(&graph_file, &graph).unwrap();
    let prepared = prepare_compiler_graph(
        &ctx,
        &project,
        &editor,
        &player,
        &graph_file,
        &sysutil::join(&[&project, "Temp/prepared/compile.json"]),
    )
    .unwrap();
    let baseline = sysutil::join(&[&project, "Temp/scene-prepared/current.json"]);
    fs::create_dir_all(sysutil::dir(&baseline)).unwrap();
    crate::util::atomic_json(&baseline, &prepared).unwrap();
    let mut compiler = Compiler::new(
        &project,
        &baseline,
        &prepared.player,
        &editor,
        &sysutil::join(&[&project, "Temp/session/compiler"]),
    )
    .unwrap();
    assert!(matches!(compiler.compile(&ctx, None), Ok(None)));

    // The session's two programs, and a Player that "starts" on the
    // baseline assemblies.
    let sdk = Sdk::locate(&editor).unwrap();
    let output = sysutil::join(&[&project, "Temp/session"]);
    let tools = Tools {
        patcher: prepare_patcher(&ctx, &sdk, &sysutil::join(&[&output, "hot-reload/patcher"]))
            .unwrap(),
        sdk,
        editor: editor.clone(),
        managed: managed.clone(),
        output: output.clone(),
        limit: None,
    };
    let run = sysutil::join(&[&output, "runs/r1"]);
    fs::create_dir_all(&run).unwrap();
    let applier = write(
        &sysutil::join(&[&output, "applier/UnityMicroEditor.HotReload.dll"]),
        b"applier",
    );
    let loaded = loaded_assemblies(&compiler, &compiler.player).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].path, compiler.current_dlls.at("Assembly-CSharp"));
    assert_eq!(loaded, newest_assemblies(&compiler).unwrap());
    let mut code = PlayerCode::new(
        &run,
        Applier {
            sha256: file_hash(&applier).unwrap(),
            path: applier,
        },
        loaded.clone(),
    );
    let player = FakePlayer::start(
        &run,
        vec![
            Reply::Apply {
                restored: 0,
                callback_errors: 0,
            },
            Reply::Apply {
                restored: 0,
                callback_errors: 0,
            },
            Reply::Apply {
                restored: 2,
                callback_errors: 0,
            },
        ],
    );
    let exited = || false;
    let build = |ctx: &Context, request: &str| tools.build(ctx, request);
    let edit = |compiler: &mut Compiler, code: &mut PlayerCode, text: String| {
        fs::write(&source, text).unwrap();
        let update = compiler
            .compile(&ctx, Some(std::slice::from_ref(&source)))
            .unwrap()
            .expect("a changed source compiles to a new revision");
        let newest = newest_assemblies(compiler).unwrap();
        // The newest compile is a Player directory of its own; the one the
        // "running" Player loaded stays as it was.
        assert_eq!(newest[0].path, update.assemblies[0]);
        assert_ne!(newest[0].path, loaded[0].path);
        assert_eq!(file_hash(&loaded[0].path).unwrap(), loaded[0].sha256);
        assert_eq!(file_hash(&newest[0].path).unwrap(), newest[0].sha256);
        code.attempt(
            &ctx,
            &Attempt {
                current: &newest,
                build: &build,
                exited: &exited,
                pid: FakePlayer::PID,
                source_changed: SystemTime::now(),
                timeout: Duration::from_secs(20),
            },
        )
        .unwrap()
    };
    // Nothing compiled yet: the Player runs the newest code.
    assert_eq!(
        code.attempt(
            &ctx,
            &Attempt {
                current: &loaded,
                build: &build,
                exited: &exited,
                pid: FakePlayer::PID,
                source_changed: SystemTime::now(),
                timeout: Duration::from_secs(20),
            },
        )
        .unwrap(),
        Outcome::Unchanged(Unchanged::default())
    );

    // Two bodies change. `Awake` is the lifecycle helper's wrapper; what the
    // author wrote lives in the method the helper made of it.
    let patch = patched(edit(&mut compiler, &mut code, probe(2, 5, "")));
    assert_eq!(
        (patch.generation, patch.patched_methods),
        (1, 2),
        "{patch:?}"
    );
    assert_eq!(patch.assemblies, ["Assembly-CSharp"]);
    assert_eq!(
        patch.members,
        ["Probe::Update()", "Probe::__UnityPlay_Awake()"]
    );
    // The compiler read a copy of the source in which the lifecycle helper
    // names the lines of the source: the copy starts with the file, and the
    // body of the callback, which moved behind a wrapper, is on its line 7.
    let copy = newest_copy(&sysutil::join(&[&output, "compiler"]), "Probe.cs");
    let rewritten = fs::read_to_string(&copy).unwrap();
    assert!(
        rewritten.starts_with(&format!("#line 1 \"{source}\"\n")),
        "{copy}: {}",
        &rewritten[..rewritten.len().min(200)]
    );
    let resumed = format!("\n#line 7 \"{source}\"\n{{ seed += 2; }}");
    assert!(rewritten.contains(&resumed), "{copy}:\n{rewritten}");
    let (request, descriptor) = player.seen()[0].clone();
    assert_eq!(request["generation"], 1);
    assert_eq!(descriptor["gameAssemblies"], json!(["Assembly-CSharp"]));
    assert_eq!(descriptor["assemblies"][0]["name"], "Assembly-CSharp");
    assert_eq!(
        read_json(&patch.report)["result"]["assemblies"][0]["detours"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // A comment on top: other bytes, the same meaning. The patch is
    // cumulative, so the two methods are sent again as generation 2.
    let patch = patched(edit(
        &mut compiler,
        &mut code,
        probe(2, 5, "// a comment\n"),
    ));
    assert_eq!((patch.generation, patch.patched_methods), (2, 2));
    assert_eq!(player.seen()[1].0["generation"], 2);

    // An added type is refused by name; the Player keeps generation 2.
    match edit(
        &mut compiler,
        &mut code,
        probe(2, 5, "public class ProbeAdded { }\n"),
    ) {
        Outcome::Unsupported(refusal) => {
            assert_eq!(refusal.reasons.len(), 1, "{refusal:?}");
            assert_eq!(
                (
                    refusal.reasons[0].code.as_str(),
                    refusal.reasons[0].member.as_str()
                ),
                ("type_added", "ProbeAdded")
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!((code.applied_generation(), code.refused()), (2, true));
    assert_eq!(player.seen().len(), 2);

    // The edit is taken back to the source the Player loaded: its two
    // methods are restored and nothing is patched any more.
    let restored = patched(edit(&mut compiler, &mut code, probe(1, 1, "")));
    assert_eq!(
        (
            restored.generation,
            restored.patched_methods,
            restored.restored_methods
        ),
        (4, 0, 2)
    );
    assert!(restored.assemblies.is_empty() && !code.redirected() && !code.refused());
    assert_eq!(player.seen()[2].1["assemblies"], json!([]));

    // A comment on the loaded code changes the bytes and nothing else: the
    // builder says so and the Player is not asked.
    match edit(
        &mut compiler,
        &mut code,
        probe(1, 1, "// another comment\n"),
    ) {
        Outcome::Unchanged(unchanged) => {
            assert!(unchanged.notes.is_empty(), "{unchanged:?}");
            assert_eq!(read_json(&unchanged.report)["outcome"], "unchanged");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(player.seen().len(), 3);
    assert_eq!(code.applied_generation(), 4);

    // A method made public. No body differs and nothing is sent, but the
    // Player keeps the old accessibility for reflection: the builder's note
    // must come back with the outcome.
    let public = probe(1, 1, "").replace("private void Update()", "public void Update()");
    assert!(public.contains("public void Update()"));
    match edit(&mut compiler, &mut code, public) {
        Outcome::Unchanged(unchanged) => {
            assert_eq!(unchanged.notes.len(), 1, "{unchanged:?}");
            let note = &unchanged.notes[0];
            assert_eq!(
                (note.code.as_str(), note.member.as_str()),
                ("accessibility_changed", "Probe::Update()")
            );
            assert!(note.message.contains("until a full recompile"), "{note:?}");
            let report = read_json(&unchanged.report);
            assert_eq!(report["result"]["verdict"], "unchanged");
            assert_eq!(report["result"]["summary"]["notes"], 1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(player.seen().len(), 3);
    assert!(!code.refused() && !code.redirected());

    // A field made public, as one does to have Unity serialize it. No body
    // differs, and nothing of the edit is in the Player before it restarts:
    // a scene value for the field is not read. That is a refusal, as
    // `[SerializeField]` on the field is one.
    let exposed = probe(1, 1, "").replace("private int frames;", "public int frames;");
    assert!(exposed.contains("public int frames;"));
    match edit(&mut compiler, &mut code, exposed) {
        Outcome::Unsupported(refusal) => {
            assert_eq!(refusal.reasons.len(), 1, "{refusal:?}");
            assert_eq!(
                (
                    refusal.reasons[0].code.as_str(),
                    refusal.reasons[0].member.as_str()
                ),
                ("serialization_changed", "Probe::frames")
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(player.seen().len(), 3);
    assert!(code.refused() && !code.redirected());
}

/// A `Managed` directory as a staged host has it, as far as the applier
/// needs: the engine modules of the installation and a bridge made of the
/// real attribute file.
fn synthetic_managed(ctx: &Context, sdk: &Sdk, editor: &str, root: &str) -> String {
    let data = sysutil::join(&[&sysutil::dir(editor), "Data"]);
    let managed = sysutil::join(&[root, "Host_Data/Managed"]);
    fs::create_dir_all(&managed).unwrap();
    let engine = sysutil::join(&[
        &data,
        "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed",
    ]);
    let mut arguments: Vec<String> = [
        "-nologo",
        "-target:library",
        "-nostdlib+",
        "-langversion:9",
        "-warnaserror+",
    ]
    .map(String::from)
    .to_vec();
    for file in glob(&sysutil::join(&[&engine, "UnityEngine*.dll"])).unwrap() {
        let link = sysutil::join(&[&managed, &sysutil::base(&file)]);
        std::os::unix::fs::symlink(&file, &link).unwrap();
        arguments.push(format!("-reference:{link}"));
    }
    let bcl = sysutil::join(&[&data, "MonoBleedingEdge/lib/mono/unityaot-linux"]);
    let attributes = sysutil::join(&[
        &micro_editor_root().unwrap(),
        "Packages/com.unitymicroeditor.runtime/Runtime/HotReloadAttributes.cs",
    ]);
    // A copy, as every build of tool sources works on one.
    let source = write(
        &sysutil::join(&[root, "bridge/HotReloadAttributes.cs"]),
        &fs::read(attributes).unwrap(),
    );
    // The engine modules are built against the facades of the class library.
    for pattern in ["*.dll", "Facades/*.dll"] {
        for file in glob(&sysutil::join(&[&bcl, pattern])).unwrap() {
            arguments.push(format!("-reference:{file}"));
        }
    }
    arguments.push(format!("-out:{managed}/UnityMicroEditor.dll"));
    arguments.push(source);
    let mut command = vec![sdk.csc.as_str()];
    command.extend(arguments.iter().map(String::as_str));
    if let Err(error) = compiler_command(ctx, root, &sdk.dotnet, &command) {
        panic!("compile the stand-in bridge: {error}");
    }
    managed
}

fn check_applier(ctx: &Context, sdk: &Sdk, editor: &str, managed: &str, cache: &str) {
    let started = Instant::now();
    let applier = stage_applier(ctx, sdk, editor, managed, cache).unwrap();
    eprintln!(
        "applier compiled in {:.2}s: {}",
        started.elapsed().as_secs_f64(),
        applier.path
    );
    // The bridge loads the file from bytes; the name is what patches bind to.
    assert_eq!(
        sysutil::base(&applier.path),
        "UnityMicroEditor.HotReload.dll"
    );
    assert_eq!(file_hash(&applier.path).unwrap(), applier.sha256);
    assert!(
        !sysutil::within(managed, &applier.path),
        "the applier must stay outside the Player"
    );
    assert!(!fs::exists(sysutil::join(&[managed, "UnityMicroEditor.HotReload.dll"])).unwrap());
    let image = fs::read(&applier.path).unwrap();
    for name in [
        "FieldStore",
        "Entry",
        "Handle",
        "UnityMicroEditor.HotReload",
    ] {
        assert!(
            image
                .windows(name.len())
                .any(|window| window == name.as_bytes()),
            "the applier lacks {name}"
        );
    }
    // Every source of the directory was compiled from a copy.
    let sources = glob(&sysutil::join(&[
        &micro_editor_root().unwrap(),
        APPLIER_DIRECTORY,
        "*.cs",
    ]))
    .unwrap();
    let copies = glob(&sysutil::join(&[
        &sysutil::dir(&applier.path),
        "Sources/*.cs",
    ]))
    .unwrap();
    assert!(!sources.is_empty());
    assert_eq!(sources.len(), copies.len());
    // The next Player start of the session takes the same file.
    let written = fs::metadata(&applier.path).unwrap().modified().unwrap();
    assert_eq!(
        stage_applier(ctx, sdk, editor, managed, cache).unwrap(),
        applier
    );
    assert_eq!(
        fs::metadata(&applier.path).unwrap().modified().unwrap(),
        written
    );
}

#[test]
fn hot_reload_applier_compiles_sdk() {
    let Some(editor) = sdk("the hot reload applier needs the Unity SDK") else {
        return;
    };
    let (_guard, root) = temp();
    let ctx = Context::background();
    let sdk = Sdk::locate(&editor).unwrap();
    let managed = synthetic_managed(&ctx, &sdk, &editor, &root);
    // A game may name its own types as it likes. Types without a namespace
    // that are called like those the applier uses of the class library must
    // not reach its compilation.
    let game = write(
        &sysutil::join(&[&root, "game/Game.cs"]),
        b"public class Console { }\npublic class Stopwatch { }\npublic class Process { }\npublic class Volatile { }\npublic class File { }\npublic class Math { }\n",
    );
    let bcl = sysutil::join(&[
        &sysutil::dir(&editor),
        "Data/MonoBleedingEdge/lib/mono/unityaot-linux/mscorlib.dll",
    ]);
    compiler_command(
        &ctx,
        &root,
        &sdk.dotnet,
        &[
            &sdk.csc,
            "-nologo",
            "-target:library",
            "-nostdlib+",
            &format!("-reference:{bcl}"),
            &format!("-out:{managed}/Assembly-CSharp.dll"),
            &game,
        ],
    )
    .unwrap();
    check_applier(
        &ctx,
        &sdk,
        &editor,
        &managed,
        &sysutil::join(&[&root, "hot-reload/applier"]),
    );
    // Only the engine's core module and the bridge are named of `Managed`.
    let response = glob(&sysutil::join(&[&root, "hot-reload/applier/*/compile.rsp"])).unwrap();
    let response = fs::read_to_string(&response[0]).unwrap();
    let named: Vec<&str> = response
        .lines()
        .filter(|line| line.contains(&managed))
        .map(|line| line.trim_matches('"').rsplit('/').next().unwrap())
        .collect();
    assert_eq!(named, APPLIER_REFERENCES);
    // Without a bridge there is nothing to compile against.
    fs::remove_file(sysutil::join(&[&managed, "UnityMicroEditor.dll"])).unwrap();
    let error = stage_applier(
        &ctx,
        &sdk,
        &editor,
        &managed,
        &sysutil::join(&[&root, "other"]),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("lacks the runtime bridge"),
        "{error}"
    );
}

/// The setup of a session: both programs from one call on a thread of its
/// own, and no build left behind by a session that ends before it finished.
#[test]
fn hot_reload_setup_sdk() {
    let Some(editor) = sdk("the hot reload setup needs the Unity SDK") else {
        return;
    };
    let (_guard, root) = temp();
    let ctx = Context::background();
    let located = Sdk::locate(&editor).unwrap();
    let managed = synthetic_managed(&ctx, &located, &editor, &root);
    let output = sysutil::join(&[&root, "session"]);
    let (tools, applier) = Setup::start(&ctx, &editor, &managed, &output)
        .finish()
        .unwrap();
    assert_eq!(tools.sdk, located);
    assert!(fs::exists(&tools.patcher).unwrap() && fs::exists(&applier.path).unwrap());
    assert_eq!(tools.applier(&ctx).unwrap(), applier);

    // A session that fails while the first build runs: the build is ended
    // with it, and its staging directory with the build.
    let other = sysutil::join(&[&root, "ended"]);
    let setup = Setup::start(&ctx, &editor, &managed, &other);
    std::thread::sleep(Duration::from_millis(150));
    let started = Instant::now();
    drop(setup);
    let waited = started.elapsed();
    assert!(waited < Duration::from_secs(10), "{waited:?}");
    assert!(ctx.error().is_none());
    let mut running = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let line = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        if String::from_utf8_lossy(&line).contains(&other) {
            running.push(entry.file_name());
        }
    }
    assert!(
        running.is_empty(),
        "a build outlived its session: {running:?}"
    );
    assert_eq!(
        glob(&sysutil::join(&[
            &other,
            "hot-reload/patcher/.helper-build-*"
        ]))
        .unwrap(),
        Vec::<String>::new()
    );
    // What it left is not taken for a finished build.
    let (again, _) = Tools::prepare(&ctx, &editor, &managed, &other).unwrap();
    assert!(fs::exists(&again.patcher).unwrap());
}

/// The session's own path on a prepared host: `stage_host` compiles the
/// bridge of this tree into a copy of the host, and the applier is compiled
/// against that copy. Needs `UME_HOT_RELOAD_HOST`, the Player executable of
/// a prepared host; no Player is started.
#[test]
fn hot_reload_applier_on_a_staged_host_sdk() {
    let Some(editor) = sdk("the hot reload applier needs the Unity SDK") else {
        return;
    };
    let Some(host) = std::env::var("UME_HOT_RELOAD_HOST")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: UME_HOT_RELOAD_HOST names no prepared host Player");
        return;
    };
    let (_guard, root) = temp();
    let ctx = Context::background();
    let staged = crate::compiler::stage_host(&ctx, &root, &editor, &host).unwrap();
    let managed = crate::compiler::player_managed(&staged);
    let (tools, applier) = Tools::prepare(&ctx, &editor, &managed, &root).unwrap();
    assert!(
        applier
            .path
            .starts_with(&sysutil::join(&[&root, "hot-reload/applier"]))
    );
    assert!(
        tools
            .patcher
            .starts_with(&sysutil::join(&[&root, "hot-reload/patcher"]))
    );
    check_applier(
        &ctx,
        &tools.sdk,
        &editor,
        &managed,
        &sysutil::join(&[&root, "hot-reload/applier"]),
    );
    assert_eq!(tools.applier(&ctx).unwrap(), applier);
}

/// Copies what a C# harness reads into a staging directory, so that its
/// build output and a source that changes meanwhile stay apart.
fn stage_tree(root: &str, relative: &[&str]) -> String {
    let source = micro_editor_root().unwrap();
    for directory in relative {
        let from = sysutil::join(&[&source, directory]);
        let to = sysutil::join(&[root, directory]);
        fs::create_dir_all(sysutil::dir(&to)).unwrap();
        let status = std::process::Command::new("cp")
            .args(["-a", "--", &from, &to])
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "copy {from}");
        for build in ["bin", "obj"] {
            let _ = fs::remove_dir_all(sysutil::join(&[&to, build]));
        }
    }
    root.to_string()
}

/// `tests/HotReloadPatcherTests` as it is: classification, behaviour on
/// Unity's Mono and generations of the real patch builder.
#[test]
fn hot_reload_patcher_harness_sdk() {
    let Some(editor) = sdk("the patch builder harness needs the Unity SDK") else {
        return;
    };
    let (_guard, root) = temp();
    let stage = stage_tree(
        &root,
        &["tools/HotReloadPatcher", "tests/HotReloadPatcherTests"],
    );
    let data = sysutil::join(&[&sysutil::dir(&editor), "Data"]);
    let project = sysutil::join(&[
        &stage,
        "tests/HotReloadPatcherTests/HotReloadPatcherTests.csproj",
    ]);
    let started = Instant::now();
    let output = std::process::Command::new(sysutil::join(&[&data, "DotNetSdk/dotnet"]))
        .args(["run", "--project", &project])
        .env("UNITY_DATA", &data)
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .current_dir(&stage)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tail: Vec<&str> = text.lines().rev().take(12).collect();
    eprintln!(
        "patch builder harness: {:?} in {:.0}s\n{}",
        output.status,
        started.elapsed().as_secs_f64(),
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    );
    assert!(output.status.success(), "{text}");
}

/// `tests/HotReloadRuntimeTests/run.sh` as it is: the applier and the
/// bridge's request handling on the standalone Mono of the installation.
#[test]
fn hot_reload_runtime_harness_sdk() {
    let Some(editor) = sdk("the applier harness needs the Unity SDK") else {
        return;
    };
    let (_guard, root) = temp();
    let stage = stage_tree(
        &root,
        &[
            "tools/HotReloadRuntime",
            "tests/HotReloadRuntimeTests",
            "Packages/com.unitymicroeditor.runtime/Runtime",
        ],
    );
    let started = Instant::now();
    let output = std::process::Command::new("bash")
        .arg(sysutil::join(&[
            &stage,
            "tests/HotReloadRuntimeTests/run.sh",
        ]))
        .env("UNITY_EDITOR_DIR", sysutil::dir(&editor))
        .env("HOTRELOAD_TEST_OUT", sysutil::join(&[&root, "out"]))
        .current_dir(&stage)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.starts_with("===") || line.starts_with("RESULT") || line.contains("FAILED")
        })
        .collect();
    eprintln!(
        "applier harness: {:?} in {:.0}s\n{}",
        output.status,
        started.elapsed().as_secs_f64(),
        summary.join("\n")
    );
    assert!(output.status.success(), "{text}");
    assert!(text.contains("all runs passed"), "{text}");
}
