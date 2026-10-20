//! Tests of the transport-independent server, called with the raw arguments a client
//! would send. The stdio and HTTP tests live in `rust/tests/mcp.rs`.
use super::{
    reply::{Data, reply, resource_reply},
    *,
};
use crate::cancel::Context;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

fn project_fixture() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    for part in ["Assets/Scripts", "ProjectSettings"] {
        fs::create_dir_all(root.join(part)).unwrap();
    }
    fs::write(
        root.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 6000.6.3f1\n",
    )
    .unwrap();
    (directory, root.to_string_lossy().into_owned())
}
fn handler(
    action: impl Fn(&str, &[u8]) -> Result<HandlerValue, HandlerError> + Send + Sync + 'static,
) -> Option<Arc<dyn RuntimeHandler>> {
    Some(Arc::new(move |_: &Context, name: &str, raw: &[u8]| {
        action(name, raw)
    }))
}
fn value(value: Value) -> Result<HandlerValue, HandlerError> {
    Ok(HandlerValue::from_value(&value))
}
fn server(project: &str, configure: impl FnOnce(&mut Options)) -> Server {
    let mut options = Options {
        project: project.into(),
        ..Options::default()
    };
    configure(&mut options);
    Server::new(&options).unwrap()
}
// The result and its decoded reply envelope.
fn call_raw(server: &Server, name: &str, arguments: &str) -> (ToolResult, Value) {
    let result = server
        .call_tool(&Context::background(), name, Some(arguments.as_bytes()))
        .unwrap();
    let reply = serde_json::from_str(&result.text).unwrap();
    assert_eq!(result.structured.as_deref(), Some(result.text.as_str()));
    (result, reply)
}
fn call(server: &Server, name: &str, arguments: Value) -> (ToolResult, Value) {
    call_raw(server, name, &arguments.to_string())
}

#[test]
fn tool_discovery_resources_and_durable_script_operations() {
    let (_guard, project) = project_fixture();
    let server = server(&project, |_| {});
    let tools = server.tools();
    assert_eq!(tools.len(), 11);
    for tool in &tools {
        assert!(serde_json::from_str::<Value>(&tool.input_schema).unwrap()["type"] == "object");
    }
    assert_eq!(server.resources().len(), 2);
    let resource = server
        .read_resource(&Context::background(), PROJECT_URI)
        .unwrap();
    assert!(resource.text.contains("6000.6.3f1"));
    let create = json!({"path": "Assets/Scripts/Agent.cs", "expectedRevision": "absent", "operationId": "mcp-create", "content": "public class Agent {}\n"});
    let (result, reply) = call(&server, "script_create", create.clone());
    assert!(!result.is_error && reply["success"] == true, "{reply}");
    let data = reply["data"].clone();
    assert_eq!(data["guid"].as_str().unwrap().len(), 32);
    assert!(!data["revision"].as_str().unwrap().is_empty());
    let (_, replay) = call(&server, "script_create", create);
    assert!(replay["success"] == true && replay["data"]["replayed"] == true);
    let (_, source) = call(
        &server,
        "script_read",
        json!({"path": "Assets/Scripts/Agent.cs"}),
    );
    let metadata = source["data"]["metadata"].clone();
    let mut edit = json!({"path": "Assets/Scripts/Agent.cs", "expectedRevision": data["revision"], "operationId": "mcp-edit", "content": "public class Agent { public int value = 3; }\n"});
    let (_, edited) = call(&server, "script_edit", edit.clone());
    assert!(edited["success"] == true, "{edited}");
    let (_, after) = call(
        &server,
        "script_read",
        json!({"path": "Assets/Scripts/Agent.cs"}),
    );
    assert_eq!(after["data"]["metadata"], metadata);
    edit["operationId"] = "mcp-stale".into();
    let (result, stale) = call(&server, "script_edit", edit);
    assert!(result.is_error && stale["error"]["code"] == "revision_conflict");
    let (result, invalid) = call(
        &server,
        "asset_read",
        json!({"path": "Assets/../ProjectSettings/ProjectVersion.txt"}),
    );
    assert!(result.is_error && invalid["error"]["code"] == "invalid_path");
}

#[test]
fn runtime_boundary_allows_only_honest_read_operations() {
    let (_guard, project) = project_fixture();
    let received: Arc<Mutex<(String, Value)>> = Arc::default();
    let seen = received.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |name, raw| {
            let input: Value = serde_json::from_slice(raw).unwrap();
            let operation = input["operation"].clone();
            *seen.lock().unwrap() = (name.to_string(), input);
            value(json!({"operation": operation}))
        });
    });
    let (_, good) = call(
        &server,
        "runtime_query",
        json!({"operation": "inspect", "arguments": {"name": "9223372036854775807"}, "expectedPid": 123, "expectedRevision": "runtime-revision"}),
    );
    {
        let (name, input) = &*received.lock().unwrap();
        assert!(good["success"] == true && name == "runtime_query");
        assert_eq!(input["expectedPid"], 123);
        assert_eq!(input["arguments"]["name"], "9223372036854775807");
    }
    received.lock().unwrap().0.clear();
    let (result, bad) = call(&server, "runtime_query", json!({"operation": "pause"}));
    assert!(result.is_error && bad["error"]["code"] == "unsupported_operation");
    assert!(
        received.lock().unwrap().0.is_empty(),
        "query permitted mutation"
    );
    let (_, good) = call(
        &server,
        "runtime_command",
        json!({"operation": "pause", "requestId": "command-id"}),
    );
    let (name, input) = &*received.lock().unwrap();
    assert!(good["success"] == true && name == "runtime_command");
    assert_eq!(input["requestId"], "command-id");
}

#[test]
fn remote_session_forwards_its_settings_and_rejects_unknown_actions() {
    let (_guard, project) = project_fixture();
    let received: Arc<Mutex<(String, Value)>> = Arc::default();
    let seen = received.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |name, raw| {
            *seen.lock().unwrap() = (name.to_string(), serde_json::from_slice(raw).unwrap());
            value(json!({"running": true, "url": "http://127.0.0.1:1/secret/"}))
        });
    });
    assert!(
        server
            .tool("remote_session")
            .is_some_and(|tool| !tool.read_only)
    );
    let (result, started) = call(
        &server,
        "remote_session",
        json!({"action": "start", "listen": "127.0.0.1:0", "fps": 30, "bitrateKbps": 8000}),
    );
    assert!(
        !result.is_error && started["data"]["running"] == true,
        "{started}"
    );
    assert_eq!(
        *received.lock().unwrap(),
        (
            "remote_session".to_string(),
            json!({"action": "start", "listen": "127.0.0.1:0", "fps": 30, "bitrateKbps": 8000})
        )
    );
    received.lock().unwrap().0.clear();
    for arguments in [
        json!({"action": "restart"}),
        json!({"action": "start", "fps": "fast"}),
        json!({"action": "start", "quality": 5}),
    ] {
        let (result, _) = call(&server, "remote_session", arguments.clone());
        assert!(result.is_error, "{arguments}");
    }
    assert!(
        received.lock().unwrap().0.is_empty(),
        "forwarded a rejected call"
    );
}

#[test]
fn hot_reload_is_a_runtime_tool_that_forwards_status_and_recompile() {
    let (_guard, project) = project_fixture();
    let source_only = server(&project, |_| {});
    assert!(source_only.tool("hot_reload").is_none());
    assert_eq!(source_only.instructions(), INSTRUCTIONS);
    let (_, capabilities) = call(&source_only, "project_capabilities", json!({}));
    assert!(capabilities["data"].get("hotReload").is_none());

    let received: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let seen = received.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |name, raw| {
            let input: Value = serde_json::from_slice(raw).unwrap();
            seen.lock().unwrap().push((name.to_string(), input.clone()));
            match (name, input["action"].as_str()) {
                ("hot_reload", Some("status")) => value(
                    json!({"enabled": true, "codeGeneration": 2, "fullRecompileRequired": false, "unsupported": [], "unsupportedCount": 0, "phase": "ready"}),
                ),
                ("hot_reload", _) => {
                    value(json!({"phase": "ready", "pid": 31, "revision": "rev-1"}))
                }
                _ => value(
                    json!({"success": true, "pid": 31, "revision": "rev-1", "result": {"tools": [{"name": "game.reset", "binding": "static", "readOnly": false, "inputSchema": {"type": "object"}}]}}),
                ),
            }
        });
    });
    let tool = server.tool("hot_reload").expect("hot_reload is not listed");
    assert!(!tool.read_only && !tool.idempotent && tool.output_schema.is_some());
    assert_eq!(
        serde_json::from_str::<Value>(&tool.input_schema).unwrap(),
        json!({"type": "object", "additionalProperties": false, "required": ["action"], "properties": {
            "action": {"type": "string", "description": "status,recompile"},
            "requestId": {"type": "string", "description": "recompile: identity of the command, default a new one; a repeated id returns the outcome of its command; ASCII letters, digits or hyphens, at most 64"},
        }})
    );
    // A server names only tools it has. The C# write tools describe the
    // session's part where a session exists, and stay as they were without one.
    let session_tools: Vec<String> = server
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| source_only.tool(name).is_none())
        .collect();
    assert!(session_tools.contains(&"session_control".to_string()));
    for tool in source_only.tools() {
        let foreign = session_tools
            .iter()
            .find(|name| tool.description.contains(name.as_str()));
        assert!(
            foreign.is_none() && !tool.description.contains("full_recompile_required"),
            "{} names {foreign:?}, which this server lacks: {}",
            tool.name,
            tool.description
        );
    }
    assert!(
        source_only
            .tool("script_edit")
            .unwrap()
            .description
            .ends_with("Does not run the compiler.")
    );
    // An added type restarts the Player without the error in two of the
    // three session policies, so the text does not promise the error.
    let create = server.tool("script_create").unwrap().description;
    assert!(
        create.contains("unless the session restarts by itself"),
        "{create}"
    );
    // What an agent reads first names the error and the call that answers it.
    let session = server.tool("session_control").unwrap();
    assert!(
        tool.description.contains("full_recompile_required")
            && tool.description.contains("resets game state"),
        "{}",
        tool.description
    );
    for text in [
        &session.description,
        &server.tool("script_create").unwrap().description,
        &server.instructions(),
    ] {
        assert!(
            text.contains("full_recompile_required") && text.contains("hot_reload"),
            "{text}"
        );
    }
    // The contract stands once, in session_control: the tool that saves a
    // source names the call that reports it.
    let edit = server.tool("script_edit").unwrap().description;
    assert!(edit.contains("session_control(wait_update)"), "{edit}");
    // The launch options and the arguments of a wait are described with the
    // arguments, where a caller that builds a call reads them.
    let schema = &session.input_schema;
    for text in [
        "hotReload (default true",
        "onUnsupportedEdit (report, the default, or restart",
        "status.lastSequence read before the first of the edits",
        "the error code wait_timeout",
        "the newest session events",
    ] {
        assert!(schema.contains(text), "{text}: {schema}");
    }
    // What the description promises: one wait covers what was saved before
    // it and nothing saved while it runs, and then reports what stands;
    // neither wait reports success for a Player that runs older code, a
    // compile error included, whatever the caller's mark is; a success that
    // left something out, or that has nothing to report, says so first;
    // launch options stay.
    for promise in [
        "lets the runner finish every source change saved before the call",
        "an edit made while it runs is not covered",
        "wait does the same once a Player is ready",
        "When the reply has attention, read it first",
        "hotReload.newNotes lists every such note since afterSequence",
        "or that nothing followed afterSequence",
        "Error code full_recompile_required",
        "a compile error with the compiler's text",
        "Both kinds are reported whatever afterSequence is",
        "Other failures are no rejection",
        "stay for later launches of this server; null restores a default; rebuildHost is for the first preparation",
    ] {
        assert!(
            session.description.contains(promise),
            "{promise}: {}",
            session.description
        );
    }
    // It is read by every agent on every list of tools.
    assert!(
        session.description.len() < 1500,
        "{}",
        session.description.len()
    );
    // The session part of the instructions is two sentences: the contract
    // stands once, in the description above.
    let instructions = server.instructions();
    let session_part =
        &instructions[instructions.find(" With a running Player session").unwrap()..];
    assert!(session_part.len() < 420, "{}", session_part.len());
    assert!(
        tool.description
            .contains("returns its outcome afterwards, without a second restart"),
        "{}",
        tool.description
    );
    let (_, capabilities) = call(&server, "project_capabilities", json!({}));
    let advertised = &capabilities["data"]["hotReload"];
    assert_eq!(advertised["errorCode"], "full_recompile_required");
    assert_eq!(
        advertised["recompile"],
        json!({"tool": "hot_reload", "arguments": {"action": "recompile"}})
    );
    assert!(advertised["launchArguments"]["onUnsupportedEdit"].is_string());

    let (result, status) = call(&server, "hot_reload", json!({"action": "status"}));
    assert!(
        !result.is_error && status["data"]["codeGeneration"] == 2,
        "{status}"
    );
    assert_eq!(
        *received.lock().unwrap(),
        [("hot_reload".to_string(), json!({"action": "status"}))],
        "status read the tool catalog or changed its arguments"
    );
    received.lock().unwrap().clear();
    // A recompile replaces the Player, so its tools are read again.
    let (result, recompiled) = call(
        &server,
        "hot_reload",
        json!({"action": "recompile", "requestId": "rc-7"}),
    );
    assert!(
        !result.is_error && recompiled["data"]["projectToolsRefreshed"] == true,
        "{recompiled}"
    );
    assert_eq!(
        received.lock().unwrap()[0],
        (
            "hot_reload".to_string(),
            json!({"action": "recompile", "requestId": "rc-7"})
        )
    );
    assert!(server.tool("project.game.reset").is_some());
    received.lock().unwrap().clear();
    for arguments in [
        json!({"action": "apply"}),
        json!({"action": "recompile", "force": true}),
        json!({"requestId": "rc-8"}),
        json!({"action": "recompile", "requestId": 8}),
    ] {
        let (result, reply) = call(&server, "hot_reload", arguments.clone());
        assert!(result.is_error, "{arguments}: {reply}");
    }
    assert!(
        received.lock().unwrap().is_empty(),
        "forwarded a rejected call"
    );
}

#[test]
fn full_recompile_required_reaches_the_client_as_a_typed_error_with_details() {
    use crate::{
        agentcontrol::{Failure, Typed},
        sysutil,
    };
    let details = json!({
        "unsupported": [{"code": "type_added", "member": "Game.Boss", "message": "a type was added"}],
        "unsupportedCount": 1,
        "report": "/owned/report.json",
        "next": {"tool": "hot_reload", "arguments": {"action": "recompile"}},
    });
    // The controller's own failure keeps its code and details.
    let failure = Failure {
        error: sysutil::Error::new("cannot be applied to the running Player"),
        reply: None,
        typed: Some(Typed {
            code: "full_recompile_required",
            details: details.clone(),
        }),
    };
    let HandlerError::Typed(typed) = HandlerError::from(failure) else {
        panic!("the code of a controller failure was dropped");
    };
    assert_eq!(
        (typed.code.as_str(), typed.message.as_str(), &typed.details),
        (
            "full_recompile_required",
            "cannot be applied to the running Player",
            &Some(details.clone())
        )
    );
    let untyped = Failure {
        error: sysutil::Error::new("CS1002: ; expected"),
        reply: None,
        typed: None,
    };
    assert!(matches!(
        HandlerError::from(untyped),
        HandlerError::Failed(message) if message == "CS1002: ; expected"
    ));

    let (_guard, project) = project_fixture();
    let rejection = typed.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |name, _| match name {
            "session_control" => Err(HandlerError::Typed(rejection.clone())),
            _ => Err(HandlerError::failed("CS1002: ; expected")),
        });
    });
    let (result, reply) = call(
        &server,
        "session_control",
        json!({"action": "wait_update", "afterSequence": 4}),
    );
    assert!(
        result.is_error && reply["success"] == false && reply.get("data").is_none(),
        "{reply}"
    );
    assert_eq!(
        reply["error"],
        json!({"code": "full_recompile_required", "message": "cannot be applied to the running Player", "details": details})
    );
    // The compile error of a recompile keeps the untyped form of a rejected update.
    let (result, reply) = call(&server, "hot_reload", json!({"action": "recompile"}));
    assert!(result.is_error, "{reply}");
    assert_eq!(
        reply["error"],
        json!({"code": "operation_failed", "message": "CS1002: ; expected"})
    );
}

#[test]
fn cli_requires_project_and_loopback_listener() {
    assert!(
        parse_options(Vec::new()).is_err(),
        "accepted inferred project"
    );
    let (_guard, project) = project_fixture();
    let error = run_options(
        &Context::background(),
        Options {
            project,
            transport: "http".into(),
            listen: "0.0.0.0:0".into(),
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("loopback"), "{error}");
}

#[test]
fn runtime_rejected_and_pending_results_do_not_claim_applied_success() {
    let (_guard, project) = project_fixture();
    let response = Arc::new(Mutex::new(
        json!({"success": false, "error": "stale object", "requestId": "receipt-id", "revision": "revision"}),
    ));
    let current = response.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |_, _| value(current.lock().unwrap().clone()));
    });
    let arguments = json!({"operation": "pause", "requestId": "receipt-id"});
    let (result, rejected) = call(&server, "runtime_command", arguments.clone());
    assert!(
        result.is_error && rejected["success"] == false,
        "{rejected}"
    );
    assert_eq!(rejected["error"]["code"], "runtime_rejected");
    assert_eq!(rejected["error"]["details"]["requestId"], "receipt-id");
    assert_eq!(rejected["error"]["details"]["revision"], "revision");
    *response.lock().unwrap() = json!({"status": "result_pending", "requestId": "receipt-id"});
    let (result, pending) = call(&server, "runtime_command", arguments);
    assert!(!result.is_error && pending["success"] == false, "{pending}");
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["data"]["requestId"], "receipt-id");
}

#[test]
fn source_paging_retains_revision_and_utf8_boundaries() {
    let (_guard, project) = project_fixture();
    let path = "Assets/Scripts/Large.cs";
    let prefix = "public class Large {}\n// ";
    let content = format!("{prefix}{}\n", "Я".repeat(180000));
    let file = Path::new(&project).join(path);
    fs::write(&file, &content).unwrap();
    fs::write(
        format!("{}.meta", file.display()),
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    )
    .unwrap();
    let server = server(&project, |_| {});
    let (_, first) = call(&server, "script_read", json!({"path": path}));
    let data = &first["data"];
    let revision = data["revision"].as_str().unwrap().to_string();
    assert!(data["truncated"] == true && data["fullLength"] == content.len());
    let mut reconstructed = data["content"].as_str().unwrap().to_string();
    let mut offset = data["nextOffset"].as_u64().unwrap();
    loop {
        let (_, page) = call(
            &server,
            "script_read",
            json!({"path": path, "offset": offset, "expectedRevision": revision}),
        );
        assert!(page["success"] == true, "{page}");
        assert_eq!(page["data"]["revision"], revision.as_str());
        reconstructed.push_str(page["data"]["content"].as_str().unwrap());
        match page["data"]["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert!(
        reconstructed == content,
        "pages did not reproduce exact source"
    );
    let (result, invalid) = call(
        &server,
        "script_read",
        json!({"path": path, "offset": prefix.len() + 1}),
    );
    assert!(result.is_error && invalid["error"]["code"] == "invalid_request");
    fs::write(&file, format!("{content}// changed\n")).unwrap();
    let (result, stale) = call(
        &server,
        "script_read",
        json!({"path": path, "offset": 0, "expectedRevision": revision}),
    );
    assert!(result.is_error && stale["error"]["code"] == "revision_conflict");
}

#[test]
fn authored_hierarchy_paging_and_field_omission() {
    let (_guard, project) = project_fixture();
    let mut scene = String::from("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n");
    for index in 0..120 {
        let (object, transform) = (index * 2 + 1, index * 2 + 2);
        scene.push_str(&format!("--- !u!1 &{object}\nGameObject:\n  m_Name: Object{index}\n  m_IsActive: 1\n  m_Component:\n  - component: {{fileID: {transform}}}\n--- !u!4 &{transform}\nTransform:\n  m_GameObject: {{fileID: {object}}}\n  m_LocalPosition: {{x: 0, y: 0, z: 0}}\n  m_Children: []\n  m_Father: {{fileID: 0}}\n"));
    }
    let path = "Assets/Large.unity";
    let file = Path::new(&project).join(path);
    fs::write(&file, scene).unwrap();
    fs::write(
        format!("{}.meta", file.display()),
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    )
    .unwrap();
    let server = server(&project, |_| {});
    let (_, first) = call(&server, "scene_hierarchy", json!({"path": path}));
    let data = &first["data"];
    assert_eq!(data["objects"].as_array().unwrap().len(), 100);
    assert!(data["total"] == 120 && data["nextOffset"] == 100, "{data}");
    let (_, second) = call(
        &server,
        "scene_hierarchy",
        json!({"path": path, "offset": 100, "includeFields": false, "expectedRevision": data["revision"]}),
    );
    let values = &second["data"];
    assert_eq!(values["objects"].as_array().unwrap().len(), 20);
    assert!(values["nextOffset"].is_null());
    assert!(values["objects"][0]["components"][0]["fields"].is_null());
}

#[test]
fn oversized_reply_returns_structured_error() {
    let data = Data::json(&"x".repeat(5 << 20)).unwrap();
    let result = reply(data, None).unwrap();
    let response: Value = serde_json::from_str(&result.text).unwrap();
    assert!(result.is_error && response["error"]["code"] == "response_too_large");
}

#[test]
fn oversized_resource_returns_error_and_preserves_connection() {
    // NUL needs six JSON bytes. The resource bound must measure the encoded
    // response, rather than just the original string's byte length.
    let encoded = crate::jsondoc::marshal(&"\0".repeat(2 << 20)).unwrap();
    let error = resource_reply("unitymicroeditor://oversized-test", encoded).unwrap_err();
    assert!(error.message().contains("response_too_large"), "{error}");
    let (_guard, project) = project_fixture();
    let server = server(&project, |_| {});
    let (result, response) = call(&server, "project_capabilities", json!({}));
    assert!(!result.is_error && response["success"] == true);
}

#[test]
fn project_actions_expose_typed_schemas_and_preserve_null_reference_value() {
    let (_guard, project) = project_fixture();
    let received: Arc<Mutex<(String, Vec<u8>)>> = Arc::default();
    let seen = received.clone();
    let server = server(&project, |options| {
        options.backends = json!({"project_build": "unity_editor"});
        options.project_actions = handler(move |name, raw| {
            *seen.lock().unwrap() = (name.to_string(), raw.to_vec());
            value(json!({"success": true, "backend": "unity_editor"}))
        });
    });
    assert_eq!(server.tools().len(), 16);
    // The raw bytes of the forwarded "value" member, if any.
    let forwarded = || -> Option<String> {
        let raw = received.lock().unwrap().1.clone();
        let members = match crate::jsondoc::parse(&raw).unwrap() {
            crate::jsondoc::Json::Object(members) => members,
            _ => panic!("arguments"),
        };
        members
            .iter()
            .find(|member| member.key == "value")
            .map(|member| String::from_utf8(member.raw(&raw).to_vec()).unwrap())
    };
    let (_, result) = call_raw(
        &server,
        "prefab_edit",
        r#"{"action":"set_override","path":"Assets/Scene.unity","expectedRevision":"source-revision","operationId":"edit-prefab","fileId":"123","property":"linkedAsset","value":null}"#,
    );
    assert!(result["success"] == true && received.lock().unwrap().0 == "prefab_edit");
    assert_eq!(forwarded().as_deref(), Some("null"));
    for value in [
        "9223372036854775807",
        "7",
        "true",
        "\"label\"",
        r#"{"fileId":"9223372036854775807"}"#,
        "[1,2]",
    ] {
        let arguments = format!(
            r#"{{"action":"set_override","path":"Assets/Scene.unity","expectedRevision":"source-revision","operationId":"edit-prefab","property":"counter","value":{value}}}"#
        );
        let (_, result) = call_raw(&server, "prefab_edit", &arguments);
        assert!(result["success"] == true, "{result}");
        assert_eq!(forwarded().as_deref(), Some(value), "JSON value changed");
    }
    let (_, result) = call(
        &server,
        "prefab_edit",
        json!({"action": "apply", "path": "Assets/Scene.unity", "expectedRevision": "source-revision", "operationId": "edit-prefab"}),
    );
    assert!(result["success"] == true);
    assert_eq!(
        forwarded(),
        None,
        "absent optional value became an explicit null"
    );
    let (_, result) = call(&server, "project_capabilities", json!({}));
    let data = &result["data"];
    assert!(data["projectActionsConfigured"] == true);
    assert_eq!(data["backends"]["project_build"], "unity_editor");
}

#[test]
fn typed_tool_preserves_large_numbers_in_arguments_and_replies() {
    let (_guard, project) = project_fixture();
    let server = server(&project, |options| {
        options.project_actions = handler(|_, raw| {
            let text = String::from_utf8(raw.to_vec()).unwrap();
            assert!(
                text.contains(r#""fields":{"counter":9223372036854775807}"#),
                "serialized integer input rounded: {text}"
            );
            Ok(HandlerValue::Map(
                br#"{"counter":9223372036854775807}"#.to_vec(),
            ))
        });
    });
    let (result, receipt) = call_raw(
        &server,
        "component_add",
        r#"{"path":"Assets/Scene.unity","fileId":"1","componentType":"BoxCollider","expectedRevision":"revision","operationId":"counter","fields":{"counter":9223372036854775807}}"#,
    );
    assert!(receipt["success"] == true, "{receipt}");
    assert!(
        result.text.contains(r#""counter":9223372036854775807"#),
        "serialized integer reply rounded: {}",
        result.text
    );
}

#[test]
fn project_build_forwards_explicit_and_default_release_boolean() {
    let (_guard, project) = project_fixture();
    let received: Arc<Mutex<Value>> = Arc::default();
    let seen = received.clone();
    let server = server(&project, |options| {
        options.project_actions = handler(move |name, raw| {
            assert_eq!(name, "project_build");
            *seen.lock().unwrap() = serde_json::from_slice(raw).unwrap();
            value(json!({"success": true}))
        });
    });
    for development in [None, Some(false), Some(true)] {
        let mut input = json!({"scenes": ["Assets/Scene.unity"], "requestId": "release-build"});
        if let Some(development) = development {
            input["development"] = development.into();
        }
        let (_, receipt) = call(&server, "project_build", input);
        assert!(receipt["success"] == true, "{receipt}");
        assert_eq!(
            received.lock().unwrap()["development"],
            development == Some(true)
        );
    }
}

#[test]
fn source_property_long_survives_mcp_and_yaml_publication() {
    let (_guard, project) = project_fixture();
    let path = Path::new(&project).join("Assets/Counter.asset");
    fs::write(&path, "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n  m_ObjectHideFlags: 0\n  m_Name: Counter\n  counter: 0\n").unwrap();
    fs::write(
        format!("{}.meta", path.display()),
        "fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n",
    )
    .unwrap();
    let server = server(&project, |_| {});
    let (_, source) = call(
        &server,
        "asset_read",
        json!({"path": "Assets/Counter.asset"}),
    );
    let arguments = format!(
        r#"{{"path":"Assets/Counter.asset","expectedRevision":{},"operationId":"long-property","operations":[{{"action":"set_property","fileId":"11400000","property":"counter","value":9223372036854775807}}]}}"#,
        source["data"]["revision"]
    );
    let (_, result) = call_raw(&server, "scene_edit", &arguments);
    assert!(result["success"] == true, "{result}");
    let actual = fs::read_to_string(&path).unwrap();
    assert!(
        actual.contains("counter: 9223372036854775807\n"),
        "authored long property rounded: {actual}"
    );
}

#[derive(Clone)]
struct Catalog {
    name: &'static str,
    revision: &'static str,
    read_only: bool,
    pid: i64,
}

#[test]
fn dynamic_project_tools_schemas_refresh_guards_and_raw_integer_preservation() {
    let (_guard, project) = project_fixture();
    let catalog = Arc::new(Mutex::new(Catalog {
        name: "game.value",
        revision: "catalog-one",
        read_only: true,
        pid: 77,
    }));
    let invocations: Arc<Mutex<Vec<String>>> = Arc::default();
    let (current, calls) = (catalog.clone(), invocations.clone());
    let server = server(&project, |options| {
        options.runtime = handler(move |operation, raw| {
            let catalog = current.lock().unwrap().clone();
            let request: Value = serde_json::from_slice(raw).unwrap();
            if operation == "session_control" {
                return value(
                    json!({"phase": "ready", "pid": catalog.pid, "revision": catalog.revision}),
                );
            }
            match request["operation"].as_str().unwrap() {
                "tools" => value(
                    json!({"success": true, "pid": catalog.pid, "revision": catalog.revision, "result": {"tools": [{"name": catalog.name, "description": "Public method", "readOnly": catalog.read_only, "binding": "static", "inputSchema": {"type": "object", "properties": {"amount": {"type": "integer", "minimum": 1}}, "required": ["amount"], "additionalProperties": false}, "outputSchema": {"type": "integer"}}], "errors": []}}),
                ),
                "invoke" => {
                    calls
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(raw.to_vec()).unwrap());
                    value(
                        json!({"success": true, "pid": catalog.pid, "revision": catalog.revision, "result": 1}),
                    )
                }
                _ => Ok(HandlerValue::Nil),
            }
        });
    });
    let changes = Arc::new(Mutex::new(0));
    let counter = changes.clone();
    server.on_tools_changed(move || *counter.lock().unwrap() += 1);
    let (_, state) = call(&server, "session_control", json!({"action": "wait"}));
    assert!(state["data"]["projectToolsRefreshed"] == true, "{state}");
    assert!(*changes.lock().unwrap() > 0, "no list-changed signal");
    let dynamic = server
        .tool("project.game.value")
        .expect("dynamic tool is listed");
    assert!(server.tools().contains(&dynamic));
    assert!(dynamic.read_only && dynamic.output_schema.is_none());
    let schema: Value = serde_json::from_str(&dynamic.input_schema).unwrap();
    assert_eq!(
        schema["properties"]["parameters"]["properties"]["amount"]["minimum"],
        1
    );
    let (result, invalid) = call(&server, &dynamic.name, json!({"parameters": {"amount": 0}}));
    assert!(result.is_error && invalid["error"]["code"] == "invalid_request");
    assert!(
        invocations.lock().unwrap().is_empty(),
        "invalid DTO executed"
    );
    let (_, invoked) = call_raw(
        &server,
        &dynamic.name,
        r#"{"parameters":{"amount":9223372036854775807}}"#,
    );
    assert!(invoked["success"] == true, "{invoked}");
    {
        let calls = invocations.lock().unwrap();
        let last = calls.last().unwrap();
        assert!(last.contains(r#""expectedPid":77"#), "{last}");
        assert!(
            last.contains(r#""expectedRevision":"catalog-one""#),
            "{last}"
        );
        assert!(last.contains("9223372036854775807"), "{last}");
    }
    *catalog.lock().unwrap() = Catalog {
        name: "game.changed",
        revision: "catalog-two",
        read_only: false,
        pid: 88,
    };
    let (_, refreshed) = call(&server, "session_control", json!({"action": "wait_update"}));
    assert!(refreshed["success"] == true);
    assert!(
        server.tool("project.game.value").is_none(),
        "stale tool survived"
    );
    let changed = server.tool("project.game.changed").expect("new tool");
    assert!(!changed.read_only, "mutating method retains readonly hint");
    let (result, invalid) = call(
        &server,
        "project.game.changed",
        json!({"parameters": {"amount": 1}}),
    );
    assert!(result.is_error && invalid["error"]["code"] == "invalid_request");
    call(&server, "session_control", json!({"action": "stop"}));
    assert!(
        server
            .tools()
            .iter()
            .all(|tool| !tool.name.starts_with("project."))
    );
}

#[test]
fn dynamic_component_binding_rejects_ambiguous_target_and_invalid_catalog() {
    let (_guard, project) = project_fixture();
    let invocations = Arc::new(Mutex::new(0));
    let count = invocations.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |_, raw| {
            let input: Value = serde_json::from_slice(raw).unwrap();
            if input["operation"] == "tools" {
                return value(
                    json!({"success": true, "pid": 123, "revision": "rev", "result": {"tools": [
                        {"name": "component.run", "binding": "component", "readOnly": false, "inputSchema": {"type": "object", "additionalProperties": false}},
                        {"name": "bad.schema", "binding": "static", "readOnly": true, "inputSchema": {"type": "object", "$ref": "https://invalid.example/schema"}},
                    ]}}),
                );
            }
            *count.lock().unwrap() += 1;
            value(json!({"success": true}))
        });
    });
    let result = server
        .call_tool(&Context::background(), "project_tools", Some(b"{}"))
        .unwrap();
    let discovered: Value = serde_json::from_str(&result.text).unwrap();
    let data = &discovered["data"]["result"];
    assert_eq!(data["mcpRegisteredCount"], 1, "{data}");
    assert_eq!(data["mcpCatalogErrors"].as_array().unwrap().len(), 1);
    for target in [None, Some(json!({"path": "Root", "name": "Root"}))] {
        let mut arguments = json!({"parameters": {}, "requestId": "run-component"});
        if let Some(target) = target {
            arguments["target"] = target;
        }
        let (result, invalid) = call(&server, "project.component.run", arguments);
        assert!(result.is_error && invalid["error"]["code"] == "invalid_request");
    }
    assert_eq!(*invocations.lock().unwrap(), 0);
    let (_, valid) = call(
        &server,
        "project.component.run",
        json!({"parameters": {}, "requestId": "run-component", "target": {"path": "Root"}}),
    );
    assert!(valid["success"] == true && *invocations.lock().unwrap() == 1);
}

#[test]
fn catalog_discovery_failure_drops_previously_published_tools() {
    let (_guard, project) = project_fixture();
    let mode = Arc::new(Mutex::new("ready"));
    let current = mode.clone();
    let server = server(&project, |options| {
        options.runtime = handler(move |_, _| {
            let mode = *current.lock().unwrap();
            if mode == "pending" {
                return value(json!({"status": "result_pending", "success": false}));
            }
            let mut result = json!({"success": true, "pid": 23, "revision": "current", "result": {"tools": [{"name": "read", "binding": "static", "readOnly": true, "inputSchema": {"type": "object"}}]}});
            if mode == "invalid" {
                result.as_object_mut().unwrap().remove("revision");
            }
            value(result)
        });
    });
    let discover = || -> Value {
        let result = server
            .call_tool(&Context::background(), "project_tools", Some(b"{}"))
            .unwrap();
        serde_json::from_str(&result.text).unwrap()
    };
    for failure in ["pending", "invalid"] {
        *mode.lock().unwrap() = "ready";
        assert!(discover()["success"] == true, "valid catalog rejected");
        assert!(server.tool("project.read").is_some());
        *mode.lock().unwrap() = failure;
        assert!(discover()["success"] == false, "{failure} reported success");
        assert!(
            server
                .tools()
                .iter()
                .all(|tool| !tool.name.starts_with("project.")),
            "{failure} discovery retained a stale tool"
        );
    }
}

// The error text recorded for every case in the reference file.
// Cases marked "approximate" compare only the stage and whether validation fails: the
// recorded text names a pattern error in another regex engine's words and lists
// additional properties in map iteration order.
#[test]
fn jsonschema_matches_recorded_cases() {
    let cases = include_str!("../../tests/testdata/mcp/jsonschema-oracle.jsonl");
    let mut failures = Vec::new();
    for line in cases.lines().filter(|line| !line.is_empty()) {
        let case: Value = serde_json::from_str(line).unwrap();
        let expected = case["expected"].as_object().unwrap();
        let (stage, message) = expected.iter().next().unwrap();
        let message = message.as_str().unwrap();
        let actual = match jsonschema::Resolved::parse(&case["schema"]) {
            Err(error) => ("resolve", error),
            Ok(resolved) => {
                // The instance text as written: `Value` would rewrite exponents.
                #[derive(serde::Deserialize)]
                struct Raw<'a> {
                    #[serde(borrow)]
                    instance: &'a serde_json::value::RawValue,
                }
                let raw: Raw = serde_json::from_str(line).unwrap();
                let instance = crate::jsondoc::parse(raw.instance.get().as_bytes())
                    .and_then(|json| crate::jsondoc::to_value(&json, true));
                match instance {
                    Err(error) => ("instance", error),
                    Ok(instance) => (
                        "validate",
                        resolved.validate(&instance).err().unwrap_or_default(),
                    ),
                }
            }
        };
        // The recorded cases split schema failures into "unmarshal" and "resolve";
        // here both are the resolve stage.
        let stage = if stage == "unmarshal" {
            "resolve"
        } else {
            stage.as_str()
        };
        if case["approximate"] == true {
            if actual.0 != stage || actual.1.is_empty() != message.is_empty() {
                failures.push(format!("{line}\n   rust: {actual:?}"));
            }
        } else if actual != (stage, message.to_string()) {
            failures.push(format!("{line}\n   rust: {actual:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
