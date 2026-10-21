//! `parity-advanced-check` verifies actual game callbacks and input in the AdvancedParity
//! Player.
use super::image::Image;
use crate::flags::FlagSet;
use serde_json::{Map, Value, json};
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// How long one MCP client process may run before it is killed.
const CLIENT_LIMIT: Duration = Duration::from_secs(50);
/// How long a receipt may stay `result_pending`.
const PENDING_LIMIT: Duration = Duration::from_secs(90);
const PROBE: &str = "AdvancedCanvas/AdvancedProbe";

struct Runner {
    /// The MCP client command; the call's own flags are appended.
    client: Vec<String>,
    url: String,
    output: String,
    prefix: String,
    revision: String,
    pid: i64,
    sequence: u32,
    checks: Vec<Value>,
}

/// The operation result inside a tool reply; null when there is none.
fn payload(reply: &Value) -> &Value {
    match &reply["data"]["result"] {
        result @ Value::Object(_) => result,
        _ => &Value::Null,
    }
}

fn success(reply: &Value) -> bool {
    reply["success"] == true
}

/// A counter of a result; zero when absent.
fn count(value: &Value, name: &str) -> i64 {
    value[name].as_f64().unwrap_or(0.0) as i64
}

fn equal_counts(a: &Value, b: &Value, fields: &[&str]) -> bool {
    fields
        .iter()
        .all(|field| count(a, field) == count(b, field))
}

/// Counts clearly green pixels of a screenshot and returns its size.
fn green_pixels(path: &str) -> Result<(u64, i64, i64), String> {
    let data = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    let image = Image::decode(&data, None).map_err(|error| format!("{path}: {error}"))?;
    let mut green = 0;
    for y in 0..image.height {
        for x in 0..image.width {
            let [r, g, b, _] = image.rgba(x, y);
            green += u64::from(g > r + 30 * 257 && g > b + 30 * 257);
        }
    }
    Ok((green, i64::from(image.width), i64::from(image.height)))
}

/// Runs a command to completion or kills it at the limit. Returns how it ended and what it
/// printed.
fn execute(command: &[String], limit: Duration) -> (Result<(), String>, String) {
    let Some((program, arguments)) = command.split_first() else {
        return (Err("no MCP client command".into()), String::new());
    };
    let mut child = match Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return (Err(format!("{program}: {error}")), String::new()),
    };
    // Drain both pipes while waiting, so a talkative client cannot block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let started = Instant::now();
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => break Err(status.to_string()),
            Ok(None) if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("killed at its time limit".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => break Err(error.to_string()),
        }
    };
    let mut printed = stdout.join().unwrap_or_default();
    printed.push_str(&stderr.join().unwrap_or_default());
    (ended, printed)
}

impl Runner {
    fn check(&mut self, name: &str, passed: bool, detail: Value) {
        self.checks
            .push(json!({"name": name, "passed": passed, "detail": detail}));
        println!("{name}: {passed}");
    }

    /// Calls one MCP tool, saves its reply as numbered evidence and returns the structured
    /// content. Only an explicit pending receipt permits polling the same immutable ID.
    fn call(&mut self, name: &str, mut params: Value, label: &str) -> Result<Value, String> {
        self.sequence += 1;
        let arguments = params
            .as_object_mut()
            .ok_or("tool arguments are not an object")?;
        if arguments.get("requestId").is_none_or(Value::is_null) {
            let id = format!("{}-{:03}", self.prefix, self.sequence);
            arguments.insert("requestId".into(), id.into());
        }
        if self.pid != 0 {
            arguments.insert("expectedPid".into(), self.pid.into());
            arguments.insert("expectedRevision".into(), self.revision.clone().into());
        }
        let request = json!({"name": name, "arguments": params}).to_string();
        let path = std::path::Path::new(&self.output)
            .join(format!("{:03}-{label}.json", self.sequence))
            .to_string_lossy()
            .into_owned();
        let mut command = self.client.clone();
        command.extend(
            [
                "-url",
                &self.url,
                "-method",
                "tools/call",
                "-params",
                &request,
                "-output",
                &path,
                "-timeout",
                "45s",
            ]
            .map(String::from),
        );
        let deadline = Instant::now() + PENDING_LIMIT;
        loop {
            // A reply left by an earlier attempt is not evidence of this one.
            let _ = std::fs::remove_file(&path);
            let (ended, printed) = execute(&command, CLIENT_LIMIT);
            let Ok(body) = std::fs::read_to_string(&path) else {
                let ended = ended.err().unwrap_or_else(|| "no reply saved".into());
                return Err(format!("{label}: {ended}: {printed}"));
            };
            let envelope: Value =
                serde_json::from_str(&body).map_err(|error| format!("{label}: {error}"))?;
            let reply = &envelope["result"]["structuredContent"];
            if !reply.is_object() {
                return Err(format!("{label} has no structured receipt: {body}"));
            }
            if reply["data"]["status"] != "result_pending" {
                return Ok(reply.clone());
            }
            if Instant::now() > deadline {
                return Err(format!("{label} receipt remains pending"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn runtime(
        &mut self,
        operation: &str,
        arguments: Value,
        label: &str,
        read: bool,
    ) -> Result<Value, String> {
        let name = if read {
            "runtime_query"
        } else {
            "runtime_command"
        };
        self.call(
            name,
            json!({"operation": operation, "arguments": arguments}),
            label,
        )
    }

    fn invoke(
        &mut self,
        name: &str,
        parameters: Value,
        target: &str,
        label: &str,
    ) -> Result<Value, String> {
        let mut arguments = Map::new();
        arguments.insert("name".into(), name.into());
        arguments.insert("parameters".into(), parameters);
        if !target.is_empty() {
            arguments.insert("target".into(), json!({"path": target}));
        }
        self.runtime("invoke", arguments.into(), label, false)
    }

    fn read(&mut self, label: &str) -> Result<Value, String> {
        self.invoke("advanced.read", json!({}), "", label)
    }

    /// Takes a screenshot and checks its size, overlay flag and amount of green, which
    /// only the overlay UI draws.
    fn screenshot_check(
        &mut self,
        name: &str,
        reply: &Value,
        expected: impl Fn(&Value, u64, i64, i64) -> bool,
    ) {
        if !success(reply) {
            return self.check(name, false, reply.clone());
        }
        let image = payload(reply);
        let measured = green_pixels(image["path"].as_str().unwrap_or_default());
        let (green, width, height) = measured.clone().unwrap_or_default();
        self.check(
            name,
            measured.is_ok() && expected(image, green, width, height),
            json!({"receipt": image, "greenPixels": green, "error": measured.err()}),
        );
    }

    fn run(&mut self) -> Result<(), String> {
        let status = self.runtime("status", json!({}), "status", true)?;
        if !success(&status) {
            return Err("runtime status failed".into());
        }
        let state = payload(&status).clone();
        self.pid = count(&state, "pid");
        self.revision = state["revision"].as_str().unwrap_or_default().into();
        if self.pid <= 0 || self.revision.is_empty() {
            return Err("runtime identity is missing".into());
        }

        let paused = self.runtime("simulation_pause", json!({}), "pause", false)?;
        self.check("pause accepted", success(&paused), payload(&paused).clone());
        let reset = self.invoke("advanced.reset", json!({}), "", "reset")?;
        self.check(
            "reset checkpoint",
            success(&reset) && count(payload(&reset), "value") == 5,
            payload(&reset).clone(),
        );
        let before = self.read("before-wait")?;
        let wait = self.runtime("wait_frames", json!({"frames": 60}), "wait-60", false)?;
        let after = self.read("after-wait")?;
        self.check(
            "render frames advance while game callbacks freeze",
            success(&wait)
                && count(payload(&wait), "frame") > count(payload(&paused), "frame")
                && equal_counts(
                    payload(&before),
                    payload(&after),
                    &["updates", "fixedUpdates", "lateUpdates"],
                ),
            json!({"before": payload(&before), "after": payload(&after), "wait": payload(&wait)}),
        );

        let catalog = self.runtime("tools", json!({}), "catalog", true)?;
        let tools = payload(&catalog);
        let listed = |name: &str| {
            tools["tools"]
                .as_array()
                .is_some_and(|list| list.iter().any(|tool| tool["name"] == name))
        };
        let valid = [
            "advanced.read",
            "advanced.reset",
            "advanced.bump",
            "advanced.instance",
            "advanced.persist",
            "advanced.throw",
            "advanced.invalid_output",
        ]
        .into_iter()
        .all(listed);
        let hidden = [
            "advanced.collision",
            "advanced.unsupported",
            "advanced.private_hidden",
        ]
        .into_iter()
        .any(listed);
        self.check(
            "attributed catalog excludes invalid/private names",
            success(&catalog) && valid && !hidden,
            tools.clone(),
        );

        let invalid = [
            json!({"amount": 2}),
            json!({"amount": 0, "message": "bad"}),
            json!({"amount": 11, "message": "bad"}),
            json!({"amount": 2, "message": null}),
            json!({"amount": 2, "message": "ok", "extra": true}),
        ];
        for (index, parameters) in invalid.into_iter().enumerate() {
            let reply =
                self.invoke("advanced.bump", parameters, "", &format!("invalid-{index}"))?;
            self.check(
                &format!("invalid DTO {index} rejected"),
                !success(&reply),
                reply,
            );
        }
        let read = self.read("after-invalid")?;
        let counters = payload(&read);
        self.check(
            "validation precedes DTO construction and method",
            count(counters, "dtoConstructors") == 0
                && count(counters, "toolCalls") == 0
                && count(counters, "value") == 5,
            counters.clone(),
        );

        let first = self.call(
            "project.advanced.bump",
            json!({"parameters": {"amount": 2, "message": "static"}}),
            "dynamic-static",
        )?;
        let second = self.call(
            "project.advanced.instance",
            json!({"parameters": {"amount": 3, "message": "instance"}, "target": {"path": PROBE}}),
            "dynamic-instance",
        )?;
        let instance = payload(&second);
        self.check(
            "dynamic MCP static and instance execute on main thread",
            success(&first)
                && success(&second)
                && count(instance, "value") == 10
                && count(instance, "dtoConstructors") == 2
                && count(instance, "toolCalls") == 2
                && instance["sameMainThread"] == true,
            instance.clone(),
        );
        for name in ["advanced.throw", "advanced.invalid_output"] {
            let reply = self.invoke(name, json!({}), "", &name.replace('.', "-"))?;
            self.check(
                &format!("{name} execution failure"),
                !success(&reply),
                reply,
            );
        }
        let read = self.read("after-execution-errors")?;
        let counters = payload(&read);
        self.check(
            "execution failure preserves observed effects",
            count(counters, "toolCalls") == 4 && count(counters, "dtoConstructors") == 2,
            counters.clone(),
        );

        self.invoke("advanced.reset", json!({}), "", "reset-input")?;
        let presses = [
            json!({"action": "key", "key": "A", "pressed": true}),
            json!({"action": "mouse", "button": "Left", "pressed": true, "position": {"x": 0, "y": 0}}),
            json!({"action": "ui_click", "target": {"path": PROBE}}),
        ];
        for (index, arguments) in presses.into_iter().enumerate() {
            let reply = self.runtime("input", arguments, &format!("input-{index}"), false)?;
            self.check(&format!("input {index} accepted"), success(&reply), reply);
        }
        let step = self.runtime("simulation_step", json!({}), "single-step", false)?;
        let read = self.read("after-step")?;
        let stepped = payload(&read).clone();
        self.check(
            "one actual Update FixedUpdate LateUpdate",
            success(&step)
                && count(&stepped, "updates") == 1
                && count(&stepped, "fixedUpdates") == 1
                && count(&stepped, "lateUpdates") == 1,
            json!({"step": step, "counters": stepped}),
        );
        self.check(
            "actual game sees key mouse and UI handlers",
            [
                "keyPresses",
                "mousePresses",
                "pointerDowns",
                "pointerUps",
                "clicks",
            ]
            .into_iter()
            .all(|counter| count(&stepped, counter) == 1),
            stepped.clone(),
        );
        self.runtime(
            "wait_frames",
            json!({"frames": 60}),
            "wait-after-step",
            false,
        )?;
        let read = self.read("after-step-wait")?;
        self.check(
            "step returns to a paused game",
            equal_counts(
                &stepped,
                payload(&read),
                &[
                    "updates",
                    "fixedUpdates",
                    "lateUpdates",
                    "keyPresses",
                    "mousePresses",
                ],
            ),
            payload(&read).clone(),
        );
        let releases = [
            json!({"action": "key", "key": "A", "pressed": false}),
            json!({"action": "mouse", "button": "Left", "pressed": false}),
        ];
        for (index, arguments) in releases.into_iter().enumerate() {
            let label = format!("input-release-{index}");
            let reply = self.runtime("input", arguments, &label, false)?;
            self.check(
                &format!("input release {index} queued"),
                success(&reply),
                reply,
            );
        }

        let screen = self.runtime("screenshot", json!({}), "screen-paused", false)?;
        self.screenshot_check(
            "paused end-of-frame screen includes overlay",
            &screen,
            |image, green, width, height| {
                green > 1000
                    && width == count(image, "width")
                    && height == count(image, "height")
                    && image["includesOverlayUi"] == true
            },
        );
        let camera = self.runtime(
            "screenshot",
            json!({"mode": "camera", "width": 960, "height": 540, "target": {"path": "AdvancedCamera"}}),
            "camera-paused",
            false,
        )?;
        self.screenshot_check(
            "camera mode excludes overlay",
            &camera,
            |image, green, width, height| {
                green < 100 && width == 960 && height == 540 && image["includesOverlayUi"] == false
            },
        );
        let wrong = self.runtime(
            "screenshot",
            json!({"width": count(&state["viewport"], "width") + 1}),
            "screen-size-rejected",
            false,
        )?;
        self.check(
            "screen cannot silently resize viewport",
            !success(&wrong),
            wrong,
        );

        let persisted = self.call(
            "project.advanced.persist",
            json!({"parameters": {}, "target": {"path": PROBE}}),
            "persistent-root",
        )?;
        let catalog = self.runtime("tools", json!({}), "persistent-catalog", true)?;
        let read = self.call(
            "project.advanced.instance",
            json!({"parameters": {"amount": 1, "message": "persistent"}, "target": {"path": PROBE}}),
            "persistent-instance",
        )?;
        let inspected = self.runtime(
            "inspect",
            json!({"path": PROBE}),
            "persistent-inspect",
            true,
        )?;
        let hierarchy = self.runtime("hierarchy", json!({}), "persistent-hierarchy", true)?;
        self.check(
            "persistent receiver remains in catalog and hierarchy",
            success(&persisted)
                && payload(&persisted)["persistent"] == true
                && success(&catalog)
                && success(&read)
                && payload(&read)["persistent"] == true
                && count(payload(&read), "value") == 6
                && success(&inspected)
                && success(&hierarchy)
                && count(payload(&hierarchy), "total") == 4,
            json!({"persist": persisted, "instance": read, "inspect": inspected, "hierarchy": hierarchy}),
        );
        Ok(())
    }
}

/// Runs the assertions and writes `assertions.json`; whether every one passed.
fn assert_all(client: Vec<String>, url: &str, output: &str) -> Result<bool, String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(output)
        .map_err(|error| format!("{output}: {error}"))?;
    let nanoseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut runner = Runner {
        client,
        url: url.into(),
        output: output.into(),
        prefix: format!("advanced-{nanoseconds}"),
        revision: String::new(),
        pid: 0,
        sequence: 0,
        checks: Vec::new(),
    };
    let error = runner.run().err();
    let passed = error.is_none() && runner.checks.iter().all(|check| check["passed"] == true);
    let mut report = json!({
        "schemaVersion": 1,
        "passed": passed,
        "pid": runner.pid,
        "revision": runner.revision,
        "checks": runner.checks,
    });
    if let Some(error) = error {
        report["error"] = error.into();
    }
    let path = std::path::Path::new(output).join("assertions.json");
    super::write_private(&path.to_string_lossy(), super::pretty(&report)?.as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(passed)
}

/// Exit status 0 when every assertion passed, 1 when one failed or could not be made, 2 for
/// a usage error.
pub fn main(args: Vec<String>) -> i32 {
    let mut flags = FlagSet::new("unity-micro-editor parity-advanced-check");
    flags.string(
        "client",
        "",
        "MCP client executable taking the parity-mcp-call flags (default: this program's parity-mcp-call)",
    );
    flags.string(
        "url",
        "http://127.0.0.1:8093/mcp",
        "owned advanced MCP endpoint",
    );
    flags.string("output", "", "new evidence directory");
    if let Err(status) = super::parse(&mut flags, args) {
        return status;
    }
    let output = flags.get_string("output");
    if output.is_empty() {
        eprintln!("-output is required");
        return 2;
    }
    let client = match flags.get_string("client") {
        client if !client.is_empty() => vec![client],
        _ => match std::env::current_exe() {
            Ok(program) => vec![
                program.to_string_lossy().into_owned(),
                super::MCP_CALL_COMMAND.into(),
            ],
            Err(error) => {
                eprintln!(
                    "cannot locate this program to run {}: {error}",
                    super::MCP_CALL_COMMAND
                );
                return 1;
            }
        },
    };
    match assert_all(client, &flags.get_string("url"), &output) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("advanced assertions failed; inspect evidence directory");
            1
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A client that saves the reply named by `$REPLY` — pending the first time — to its
    /// `-output` file, as `parity-mcp-call` saves a server's.
    fn scripted_client(directory: &std::path::Path, reply: &Value) -> Vec<String> {
        let script = directory.join("client.sh");
        let marker = directory.join("polled");
        let pending = json!({"result": {"structuredContent": {"success": true, "data": {"status": "result_pending"}}}});
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = -output ] && out=$2; [ \"$1\" = -params ] && printf '%s' \"$2\" > '{marker}.params'; shift; done\nif [ -e '{marker}' ]; then printf '%s' '{reply}' > \"$out\"; else : > '{marker}'; printf '%s' '{pending}' > \"$out\"; fi\n",
                marker = marker.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        vec![script.to_string_lossy().into_owned()]
    }

    fn runner(client: Vec<String>, output: &std::path::Path) -> Runner {
        Runner {
            client,
            url: "http://127.0.0.1:1/mcp".into(),
            output: output.to_string_lossy().into_owned(),
            prefix: "advanced-test".into(),
            revision: String::new(),
            pid: 0,
            sequence: 0,
            checks: Vec::new(),
        }
    }

    #[test]
    fn call_polls_a_pending_receipt_and_identifies_the_request() {
        let directory = tempfile::tempdir().unwrap();
        let reply = json!({"result": {"structuredContent": {"success": true, "data": {"status": "completed", "result": {"value": 5}}}}});
        let mut runner = runner(scripted_client(directory.path(), &reply), directory.path());
        runner.pid = 42;
        runner.revision = "r1".into();
        let received = runner.runtime("status", json!({}), "status", true).unwrap();
        assert!(success(&received));
        assert_eq!(count(payload(&received), "value"), 5);
        assert!(directory.path().join("001-status.json").exists());
        let sent: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.path().join("polled.params")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            sent,
            json!({"name": "runtime_query", "arguments": {
                "operation": "status", "arguments": {}, "requestId": "advanced-test-001",
                "expectedPid": 42, "expectedRevision": "r1",
            }})
        );
    }

    #[test]
    fn call_without_a_saved_reply_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = runner(vec!["/bin/false".into()], directory.path());
        let error = runner.read("missing").unwrap_err();
        assert!(error.starts_with("missing: "), "{error}");
    }

    #[test]
    fn results_and_counters_default_when_absent() {
        let reply = json!({"success": true, "data": {"result": {"frame": 7.0, "name": "x"}}});
        assert_eq!(count(payload(&reply), "frame"), 7);
        assert_eq!(count(payload(&reply), "name"), 0);
        assert_eq!(payload(&json!({"data": {"result": "text"}})), &Value::Null);
        assert!(!success(&json!({"success": "true"})));
        assert!(equal_counts(
            &json!({"a": 1}),
            &json!({"a": 1, "b": 2}),
            &["a"]
        ));
        assert!(!equal_counts(&json!({"a": 1}), &json!({"a": 2}), &["a"]));
    }

    #[test]
    fn green_pixels_counts_only_clearly_green_ones() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("screen.png");
        let mut encoder = png::Encoder::new(std::fs::File::create(&path).unwrap(), 3, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[0, 255, 0, 100, 120, 100, 240, 255, 10])
            .unwrap();
        drop(writer);
        assert_eq!(green_pixels(path.to_str().unwrap()), Ok((1, 3, 1)));
        assert!(green_pixels("/nonexistent/screen.png").is_err());
    }
}
