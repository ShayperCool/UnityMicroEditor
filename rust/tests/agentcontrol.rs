//! The controller behind the service and the MCP tools the `mcp` command
//! builds. The fixtures' helper process is the test of this binary at the path
//! they name, `agentcontrol::tests::controller_helper_process`.
pub mod tests {
    use serde_json::{Value, json};
    use std::{fs, sync::Arc, thread};
    use unity_micro_editor::{
        agent, agentcontrol::testing::*, cancel::Context, jsondoc, mcp, sysutil,
    };

    #[test]
    fn controller_helper_process() {
        helper_process()
    }

    // The reply an agent gets through the server the `mcp` command builds: the
    // service in front of the controller, and the MCP tools in front of the service.
    #[test]
    fn mcp_reply_names_the_full_recompile_and_the_call_that_applies_it() {
        let c = controller_fixture("");
        let project = c.project();
        for directory in ["Assets", "ProjectSettings"] {
            fs::create_dir_all(sysutil::join(&[&project, directory])).unwrap();
        }
        for (path, content) in [
            (
                "ProjectSettings/ProjectVersion.txt",
                "m_EditorVersion: 6000.6.3f1\n",
            ),
            ("Assets/Main.unity", "selected scene"),
        ] {
            fs::write(sysutil::join(&[&project, path]), content).unwrap();
        }
        let service = agent::Service::new(agent::Options {
            project: project.clone(),
            scene: "Assets/Main.unity".into(),
            asynchronous: true,
            controller: Some(c.controller.clone()),
            lifetime: Some(Context::background()),
            ..Default::default()
        });
        let runtime: Arc<dyn mcp::RuntimeHandler> = {
            let service = service.clone();
            Arc::new(move |ctx: &Context, name: &str, raw: &[u8]| {
                service
                    .runtime(ctx, name, raw)
                    .map_err(agent::handler_error)
            })
        };
        let server = mcp::Server::new(&mcp::Options {
            project,
            runtime: Some(runtime),
            ..Default::default()
        })
        .unwrap();
        let call = |name: &str, arguments: Value| -> (bool, Value) {
            let result = server
                .call_tool(&seconds(3), name, Some(&jsondoc::encode(&arguments)))
                .unwrap();
            (result.is_error, serde_json::from_str(&result.text).unwrap())
        };
        let ready = start_ready(&c);
        // A refusal is answered by a recompile even when its text reads like a
        // graph rejection, which would queue a preparation and replace the Player.
        let mut event = refusal("/owned/report.json");
        event["error"] =
            json!("Game.Boss is outside the prepared managed graph of the running Player");
        emit_fixture_event(&c, event);
        let (is_error, reply) = call(
            "session_control",
            json!({"action": "wait_update", "afterSequence": ready.last_sequence}),
        );
        let error = &reply["error"];
        assert!(is_error && reply["success"] == false, "{reply}");
        assert_eq!(error["code"], "full_recompile_required", "{reply}");
        assert_eq!(
            error["details"]["next"],
            json!({"tool": "hot_reload", "arguments": {"action": "recompile"}})
        );
        assert_eq!(error["details"]["unsupported"][0]["code"], "type_added");
        assert_eq!(error["details"]["unsupportedCount"], 3);
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("Game.Boss: a type was added [type_added] (and 2 more)"),
            "{reply}"
        );
        // A status poll shows the same state and starts no preparation for it.
        let (is_error, status) = call("session_control", json!({"action": "status"}));
        assert!(
            !is_error
                && status["data"]["hotReload"]["fullRecompileRequired"] == true
                && status["data"].get("preparation").is_none()
                && status["data"].get("preparationRequired").is_none(),
            "{status}"
        );
        let refused = status["data"]["lastSequence"].as_u64().unwrap();
        let (_, status) = call("hot_reload", json!({"action": "status"}));
        assert!(
            status["success"] == true
                && status["data"]["fullRecompileRequired"] == true
                && status["data"]["onUnsupportedEdit"] == "report"
                && status["data"]["phase"] == "ready",
            "{status}"
        );
        // A scene edit confirmed after the refusal does not make the answer a
        // success: not with the mark of before the C# edit, not without a mark,
        // not with the mark of the status that showed the refusal, and not for
        // an agent that waits for readiness.
        accept(
            &c,
            json!({"event": "unchanged", "pid": ready.pid, "revision": ready.revision, "paths": ["Assets/Main.unity"]}),
        );
        for arguments in [
            json!({"action": "wait_update", "afterSequence": ready.last_sequence}),
            json!({"action": "wait_update"}),
            json!({"action": "wait_update", "afterSequence": refused}),
            json!({"action": "wait", "afterSequence": refused}),
            json!({"action": "wait"}),
        ] {
            let (is_error, reply) = call("session_control", arguments.clone());
            assert!(
                is_error
                    && reply["success"] == false
                    && reply["error"]["code"] == "full_recompile_required"
                    && reply["error"]["details"]["unsupportedCount"] == 3
                    && reply["error"]["details"]["next"]["tool"] == "hot_reload",
                "{arguments}: {reply}"
            );
        }
        let worker = {
            let server = server.clone();
            thread::spawn(move || {
                server.call_tool(
                    &seconds(3),
                    "hot_reload",
                    Some(br#"{"action":"recompile","requestId":"rc-mcp"}"#),
                )
            })
        };
        wait_for("the recompile command", || !commands(&c).is_empty());
        assert_eq!(
            commands(&c),
            [r#"{"command":"recompile","requestId":"rc-mcp"}"#]
        );
        emit_fixture_event(
            &c,
            restarted(
                &c,
                ready.pid,
                json!({"reason": "requested", "requestId": "rc-mcp"}),
            ),
        );
        let result = worker.join().unwrap().unwrap();
        let reply: Value = serde_json::from_str(&result.text).unwrap();
        let data = &reply["data"];
        assert!(
            !result.is_error
                && reply["success"] == true
                && data["phase"] == "ready"
                && data["hotReload"]["fullRecompileRequired"] == false
                && data["hotReload"]["lastRestart"]["requestId"] == "rc-mcp"
                // The fixture Player publishes no tool catalog; the refresh was attempted.
                && data["projectToolsRefreshError"].is_string(),
            "{reply}"
        );
        service.close();
    }
}
