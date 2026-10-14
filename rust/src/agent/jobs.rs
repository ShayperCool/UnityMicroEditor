//! Durable detached project actions. Only revision-checked project coordinators may
//! run as detached jobs. Player requests retain the controller's receipt protocol and
//! are never replayed here.
use super::{
    Any, AnyMap, Job, Preparation, Service, actions::RuntimeOutcome, actions::RuntimeValue,
    actions::session_input, context_error, error_text, fail, join, map_reply, not_exist, obj,
    operation_id, reply_bytes, text, typed,
};
use crate::{
    authoring::AuthoringError,
    cancel::{Canceled, Context},
    jsondoc::{self, Json},
    mcp::HandlerValue,
    sha256,
    util::{agent_id, agent_immutable_json, read_agent_file},
};
use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::value::RawValue;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    thread,
};

/// The action a job runs: `Service::handle_sync` in production.
pub(crate) type Run = Arc<dyn Fn(&Context, &str, &[u8]) -> Result<HandlerValue> + Send + Sync>;

pub(crate) fn async_action(action: &str) -> bool {
    matches!(
        action,
        "project_prepare" | "project_build" | "prefab_edit" | "component_add" | "component_schema"
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JobRequest<'a> {
    schema_version: i64,
    project: &'a str,
    action: &'a str,
    identity: &'a str,
    arguments: &'a RawValue,
}

/// `result` is the raw JSON of the confirmed value;
/// empty means absent.
#[derive(Debug, Clone, Default)]
pub(crate) struct JobReceipt {
    pub schema_version: i64,
    pub request_fingerprint: String,
    pub result: Vec<u8>,
    pub error: Option<AuthoringError>,
}
impl Serialize for JobReceipt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeMap};
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("schemaVersion", &self.schema_version)?;
        map.serialize_entry("requestFingerprint", &self.request_fingerprint)?;
        if !self.result.is_empty() {
            let text = String::from_utf8(jsondoc::compact(&self.result)).map_err(Error::custom)?;
            map.serialize_entry(
                "result",
                &RawValue::from_string(text).map_err(Error::custom)?,
            )?;
        }
        if let Some(error) = &self.error {
            // `details` is omitted when absent or empty.
            let empty = error
                .details
                .as_ref()
                .is_none_or(|details| details.as_object().is_none_or(serde_json::Map::is_empty));
            if empty {
                map.serialize_entry(
                    "error",
                    &BTreeMap::from([("code", &error.code), ("message", &error.message)]),
                )?;
            } else {
                map.serialize_entry("error", error)?;
            }
        }
        map.end()
    }
}
impl JobReceipt {
    /// Reads a receipt of at most 80 MiB; member names match case-insensitively.
    pub(crate) fn read(path: &str) -> Result<Self> {
        let data = read_agent_file(path, 80 << 20)?;
        let (parsed, used) = jsondoc::parse_first(&data).map_err(|e| anyhow!(e))?;
        if !jsondoc::rest_is_space(&data, used) {
            return Err(anyhow!("unexpected JSON content"));
        }
        let mut receipt = JobReceipt::default();
        let members = match &parsed {
            Json::Null => return Ok(receipt),
            Json::Object(members) => members,
            other => {
                return Err(anyhow!(
                    jsondoc::Target::VALUE.mismatch(other.kind(), "agentActionJobReceipt")
                ));
            }
        };
        let at = |field| jsondoc::Target {
            strukt: "agentActionJobReceipt",
            field,
        };
        jsondoc::decode_struct(
            members,
            &["schemaVersion", "requestFingerprint", "result", "error"],
            |field, member| match field {
                0 => jsondoc::set_int(
                    &mut receipt.schema_version,
                    &member.value,
                    at("schemaVersion"),
                ),
                1 => jsondoc::set_string(
                    &mut receipt.request_fingerprint,
                    &member.value,
                    at("requestFingerprint"),
                ),
                2 => {
                    receipt.result = member.raw(&data).to_vec();
                    Ok(())
                }
                _ => decode_error(&member.value, &mut receipt.error),
            },
        )
        .map_err(|e| anyhow!(e))?;
        Ok(receipt)
    }

    /// The confirmed result or the recorded failure.
    fn value(&self) -> Result<HandlerValue> {
        if let Some(error) = &self.error {
            return Err(error.clone().into());
        }
        if self.result.is_empty() {
            return Err(fail(
                "invalid_job_receipt",
                "completed project job has no result",
            ));
        }
        // The stored bytes, compacted: never re-decoded into a map.
        Ok(HandlerValue::Json(jsondoc::compact(&self.result)))
    }
}
// The typed error of a receipt: details are f64-decoded like any untyped value.
fn decode_error(value: &Json, target: &mut Option<AuthoringError>) -> Result<(), String> {
    let members = match value {
        Json::Null => {
            *target = None;
            return Ok(());
        }
        Json::Object(members) => members,
        other => {
            return Err(jsondoc::Target {
                strukt: "agentActionJobReceipt",
                field: "error",
            }
            .mismatch(other.kind(), "authoring.Error"));
        }
    };
    let mut error = target.take().unwrap_or_else(|| AuthoringError {
        code: String::new(),
        message: String::new(),
        details: None,
    });
    let at = |field| jsondoc::Target {
        strukt: "agentActionJobReceipt",
        field,
    };
    let outcome = jsondoc::decode_struct(
        members,
        &["code", "message", "details"],
        |field, member| match field {
            0 => jsondoc::set_string(&mut error.code, &member.value, at("error.code")),
            1 => jsondoc::set_string(&mut error.message, &member.value, at("error.message")),
            _ => match &member.value {
                Json::Null => {
                    error.details = None;
                    Ok(())
                }
                Json::Object(members) => {
                    error.details =
                        Some(serde_json::Value::Object(jsondoc::to_map(members, true)?));
                    Ok(())
                }
                other => Err(at("error.details").mismatch(other.kind(), "object")),
            },
        },
    );
    *target = Some(error);
    outcome
}

fn action_job_pending(
    action: &str,
    id: &str,
    directory: &str,
    has_operation: bool,
    progress: Option<AnyMap>,
) -> Result<HandlerValue> {
    // An accepted job that waits for the project action lock has no stage yet.
    let progress = progress.unwrap_or_else(|| obj([("stage", "queued".into())]));
    let mut result = obj([
        ("progress", Any::Object(progress)),
        ("success", false.into()),
        ("status", "result_pending".into()),
        ("backend", "project_action_job".into()),
        ("action", action.into()),
        ("requestId", id.into()),
        ("jobStatusPath", join(&[directory, "result.json"]).into()),
    ]);
    if has_operation {
        result.insert("operationId".into(), id.into());
    }
    map_reply(result)
}

/// The receipt and whether the outcome is still pending.
pub(crate) fn make_action_job_receipt(outcome: Result<HandlerValue>) -> (JobReceipt, bool) {
    let mut receipt = JobReceipt {
        schema_version: 1,
        ..Default::default()
    };
    let value = match outcome {
        Ok(value) => value,
        Err(error) => {
            if let Some(detail) = typed(&error) {
                if detail.code == "result_pending" {
                    return (receipt, true);
                }
                receipt.error = Some(detail.clone());
            } else if context_error(&error) {
                return (receipt, true);
            } else {
                receipt.error = Some(AuthoringError {
                    code: "project_action_failed".into(),
                    message: error_text(&error),
                    details: None,
                });
            }
            return (receipt, false);
        }
    };
    let encoded = reply_bytes(&value).to_vec();
    if let Ok(Json::Object(members)) = jsondoc::parse(&encoded)
        // The last duplicate key wins.
        && let Some(status) = members.iter().rev().find(|member| member.key == "status")
        && status.value.as_str() == Some("result_pending")
    {
        return (receipt, true);
    }
    receipt.result = encoded;
    (receipt, false)
}

/// Members by name with their decoded value and raw bytes.
type RawMembers = BTreeMap<String, (Json, Vec<u8>)>;
/// Decodes an object into its members: `None` for `null`.
fn raw_members(raw: &[u8]) -> Result<Option<RawMembers>, String> {
    match jsondoc::parse(raw)? {
        Json::Null => Ok(None),
        Json::Object(members) => Ok(Some(
            members
                .into_iter()
                .map(|member| {
                    let bytes = member.raw(raw).to_vec();
                    (member.key, (member.value, bytes))
                })
                .collect(),
        )),
        other => Err(jsondoc::Target::VALUE.mismatch(other.kind(), "object")),
    }
}

/// The members of an encoded session state; number literals stay verbatim.
pub(crate) fn state_members(state: &crate::agentcontrol::State) -> Result<AnyMap> {
    let encoded = jsondoc::marshal(state).map_err(|e| anyhow!(e))?;
    match crate::project::value::parse_json(&encoded, true)? {
        (Any::Object(members), _) => Ok(members),
        _ => Err(anyhow!("session state is not an object")),
    }
}

impl Service {
    /// Starts the job once and reports its durable receipt.
    pub(crate) fn enqueue_action(
        &self,
        ctx: &Context,
        action: &str,
        raw: &[u8],
        run: Run,
    ) -> Result<HandlerValue> {
        ctx.err()?;
        if !async_action(action) || raw.len() > 1 << 20 {
            return Err(fail(
                "invalid_request",
                "unsupported or oversized detached project action",
            ));
        }
        let Ok(Some(args)) = raw_members(raw) else {
            return Err(fail(
                "invalid_request",
                "project arguments must be an object",
            ));
        };
        let id_field = if action == "prefab_edit" || action == "component_add" {
            "operationId"
        } else {
            "requestId"
        };
        let id = match args.get(id_field) {
            Some((Json::String(id), _)) if operation_id(id) => id.clone(),
            _ => {
                return Err(fail(
                    "invalid_request",
                    format!(
                        "{id_field} must be at most 64 ASCII letters/digits/hyphens for a detached project action"
                    ),
                ));
            }
        };
        // Sorted keys, compacted values.
        let mut normalized = vec![b'{'];
        for (index, (key, (_, bytes))) in args.iter().enumerate() {
            if index > 0 {
                normalized.push(b',');
            }
            normalized.extend_from_slice(jsondoc::quote(key).as_bytes());
            normalized.push(b':');
            normalized.extend_from_slice(&jsondoc::compact(bytes));
        }
        normalized.push(b'}');
        let directory = join(&[
            &self.0.project,
            "Library/UnityMicroEditor/action-jobs",
            &sha256(id.as_bytes()),
        ]);
        let arguments = RawValue::from_string(String::from_utf8(normalized.clone())?)?;
        let request = JobRequest {
            schema_version: 1,
            project: &self.0.project,
            action,
            identity: &id,
            arguments: &arguments,
        };
        let fingerprint = sha256(&super::marshal(&request)?);
        let result_path = join(&[&directory, "result.json"]);

        let mut jobs = self.0.jobs.lock().unwrap();
        if jobs.closed {
            return Err(Canceled::Canceled.into());
        }
        agent_immutable_json(&join(&[&directory, "request.json"]), &request)?;
        match JobReceipt::read(&result_path) {
            Ok(persisted) => {
                if persisted.schema_version != 1 || persisted.request_fingerprint != fingerprint {
                    return Err(fail(
                        "invalid_job_receipt",
                        "project job receipt has an unsupported schema",
                    ));
                }
                return persisted.value();
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        let has_operation = args.contains_key("operationId");
        if let Some(job) = jobs.jobs.get(&id) {
            let job = job.lock().unwrap();
            if let Some(receipt) = &job.receipt {
                return receipt.value();
            }
            if job.active {
                return action_job_pending(
                    action,
                    &id,
                    &directory,
                    has_operation,
                    self.progress(&id),
                );
            }
        }
        if jobs.jobs.len() >= 128 {
            jobs.jobs.retain(|_, job| job.lock().unwrap().active);
            if jobs.jobs.len() >= 128 {
                return Err(fail(
                    "too_many_action_jobs",
                    "at most 128 project action jobs may be active in this service",
                ));
            }
        }
        let job_context = match &jobs.context {
            Some(context) => context.clone(),
            None => {
                let context = self.lifetime().with_cancel();
                jobs.context = Some(context.clone());
                context
            }
        };
        job_context.err()?;
        let job = Arc::new(Mutex::new(Job {
            active: true,
            receipt: None,
        }));
        jobs.jobs.insert(id.clone(), job.clone());
        *self.0.job_wait.0.lock().unwrap() += 1;
        let service = self.clone();
        let action_name = action.to_string();
        let job_id = id.clone();
        thread::spawn(move || {
            struct Done(Service);
            impl Drop for Done {
                fn drop(&mut self) {
                    let (count, changed) = &self.0.0.job_wait;
                    *count.lock().unwrap() -= 1;
                    changed.notify_all();
                }
            }
            let _done = Done(service.clone());
            let outcome = run(&job_context, &action_name, &normalized);
            let (mut receipt, pending) = make_action_job_receipt(outcome);
            receipt.request_fingerprint = fingerprint.clone();
            if !pending && let Err(write_error) = agent_immutable_json(&result_path, &receipt) {
                match JobReceipt::read(&result_path) {
                    Ok(winner)
                        if winner.schema_version == 1
                            && winner.request_fingerprint == fingerprint =>
                    {
                        receipt = winner;
                    }
                    _ => {
                        receipt.error = Some(AuthoringError {
                            code: "outcome_unknown".into(),
                            message:
                                "project action completed but its durable receipt could not be published"
                                    .into(),
                            details: Some(serde_json::json!({
                                "requestId": job_id,
                                "jobStatusPath": result_path,
                                "cause": error_text(&write_error),
                            })),
                        });
                        receipt.result.clear();
                    }
                }
            }
            let _jobs = service.0.jobs.lock().unwrap();
            let mut job = job.lock().unwrap();
            job.active = false;
            if !pending {
                job.receipt = Some(receipt);
            }
        });
        action_job_pending(action, &id, &directory, has_operation, None)
    }

    pub(crate) fn close_action_jobs(&self) {
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            jobs.closed = true;
            if let Some(context) = &jobs.context {
                context.cancel();
            }
        }
        let (count, changed) = &self.0.job_wait;
        let mut running = count.lock().unwrap();
        while *running > 0 {
            running = changed.wait(running).unwrap();
        }
    }

    /// A confirmed preparation may be read after the MCP process restarts. Restore its
    /// in-memory controller selection without rerunning Editor/SDK preparation.
    pub(crate) fn restore_prepared_job_activation(
        &self,
        result: HandlerValue,
        raw: &[u8],
    ) -> Result<HandlerValue> {
        let Some(controller) = &self.0.controller else {
            return Ok(result);
        };
        let preparation = match serde_json::from_slice::<Option<Preparation>>(reply_bytes(&result))
        {
            Ok(Some(preparation))
                if preparation.schema_version == 1 && !preparation.baseline.is_empty() =>
            {
                preparation
            }
            _ => return Ok(result),
        };
        let args = raw_members(raw)
            .map_err(|e| anyhow!(e))?
            .unwrap_or_default();
        let activate = match args.get("activate") {
            None | Some((Json::Null, _)) => true,
            Some((Json::Bool(flag), _)) => *flag,
            Some((other, _)) => {
                return Err(anyhow!(
                    jsondoc::Target::VALUE.mismatch(other.kind(), "bool")
                ));
            }
        };
        if !activate || !self.baseline().is_empty() {
            return Ok(result);
        }
        let _guard = match self.0.mu.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return map_reply(obj([
                    ("success", false.into()),
                    ("status", "result_pending".into()),
                    ("requestId", preparation.request_id.into()),
                    ("backend", "project_action_job".into()),
                    (
                        "message",
                        "preparation is confirmed; waiting to activate its controller selection"
                            .into(),
                    ),
                ]));
            }
        };
        if preparation.project != self.0.project {
            return Err(fail(
                "invalid_job_receipt",
                "preparation receipt belongs to another project",
            ));
        }
        let directory = join(&[
            &self.0.project,
            "Library/UnityMicroEditor/preparations",
            &preparation.request_id,
        ]);
        super::validate_cached_preparation(
            &self.0.project,
            &self.0.editor,
            &preparation.scene,
            &preparation.request_id,
            &directory,
            &preparation,
        )?;
        controller.configure_prepared(&preparation.baseline, &preparation.scene)?;
        self.select(&preparation.baseline, &preparation.scene);
        Ok(result)
    }

    /// `status`, `wait` and `wait_update` while a preparation may be running.
    /// Before the first Player exists the controller only knows `stopped`, so a
    /// pending preparation answers with its stage instead of a stopped session.
    fn session_with_preparation(
        &self,
        ctx: &Context,
        name: &str,
        raw: &[u8],
        action: &str,
    ) -> RuntimeOutcome {
        let Some(controller) = &self.0.controller else {
            return self.controller_handle(ctx, name, raw);
        };
        // A session that has a preparation is started again by whoever
        // replaces it (the job of a preparation when it ends, the recovery
        // that continues on a known one): the caller has nothing to start
        // and repeats its call. Before the first preparation it is the start
        // itself that reports the progress.
        let swapping = || self.0.swapping.load(std::sync::atomic::Ordering::SeqCst);
        let pending = |phase: &str, progress: Option<AnyMap>| {
            // A preparation starts a session that ran when it replaces it; a
            // session that was stopped stays stopped.
            let stopped = controller.status().phase == "stopped" && !swapping();
            let message = match (self.baseline().is_empty(), progress.is_some(), stopped) {
                (true, _, _) => {
                    "preparation is still running and no Player has started; reissue the same session_control(start) until it completes, then wait"
                }
                (false, true, true) => {
                    "a preparation of the project is running, and the session is stopped: nothing starts it by itself. Call session_control(start) once the preparation has ended (until then it returns the stage)"
                }
                (false, true, false) => {
                    "a preparation of the project is running; when it activates, the session starts on it by itself, reissue this call"
                }
                (false, false, _) => {
                    "the session is being restarted on another preparation of the project, reissue this call"
                }
            };
            let mut reply = obj([
                ("success", false.into()),
                ("status", "result_pending".into()),
                ("phase", phase.into()),
                ("backend", "project_action_job".into()),
                ("message", message.into()),
            ]);
            if let Some(progress) = progress {
                reply.insert("preparation".into(), Any::Object(progress));
            }
            Ok(reply)
        };
        // The Player is replaced by a stop and a start. A wait that was held
        // through that sees the session stopped, which is no failure of
        // anything: it is told to ask again, like a wait that comes later.
        let through = |began: bool| -> RuntimeOutcome {
            let outcome = self.controller_handle(ctx, name, raw);
            let stopped = outcome.error.is_some()
                && matches!(
                    &outcome.value,
                    RuntimeValue::Controller(crate::agentcontrol::Reply::State(state))
                        if state.phase == "stopped" || state.phase == "stopping"
                );
            if action == "status" || !stopped {
                return outcome;
            }
            let progress = self.preparation_progress();
            if progress.is_none() && !began && !swapping() {
                return outcome;
            }
            match pending("preparing_configuration", progress).and_then(map_reply) {
                Ok(value) => RuntimeOutcome::handler(value),
                Err(error) => RuntimeOutcome::failed(error),
            }
        };
        let Some(progress) = self.preparation_progress() else {
            return through(swapping());
        };
        let state = controller.status_reply();
        let reply = if action == "status" {
            state_members(&state).map(|mut reply| {
                reply.insert("preparation".into(), Any::Object(progress));
                reply
            })
        } else if state.phase == "stopped" || state.phase == "failed" {
            pending("preparing", Some(progress))
        } else {
            return through(true);
        };
        match reply.and_then(map_reply) {
            Ok(value) => RuntimeOutcome {
                value: RuntimeValue::Handler(value),
                error: None,
            },
            Err(error) => RuntimeOutcome::failed(error),
        }
    }

    pub(crate) fn runtime_with_action_jobs(
        &self,
        ctx: &Context,
        name: &str,
        raw: &[u8],
    ) -> RuntimeOutcome {
        const SCENE: &str = "prepare and activate the selected scene with project_prepare before starting or resetting this runtime";
        // A recompile restarts the Player on the prepared graph. Sources that
        // have left it need a new preparation, which no recompile makes: the
        // caller is answered as a wait would be, with the preparation that
        // answers the rejection (session6-4).
        if name == "hot_reload"
            && let Some(controller) = &self.0.controller
            && matches!(
                jsondoc::parse(raw),
                Ok(Json::Object(members)) if members.iter().any(|member| member.key == "action" && member.value.as_str() == Some("recompile"))
            )
        {
            let state = controller.status();
            if state.phase == "ready"
                && !state.code_refused()
                && (state.preparation_required
                    || crate::compiler::requires_new_graph(&state.update_error))
            {
                let standing = RuntimeOutcome {
                    value: RuntimeValue::Controller(crate::agentcontrol::Reply::State(Box::new(
                        state,
                    ))),
                    error: None,
                };
                let recovered = self.recover_configuration(
                    ctx,
                    "session_control",
                    br#"{"action":"wait_update"}"#,
                    standing,
                );
                // Without a recovery to name the controller refuses the
                // recompile itself; the state of the session is no answer.
                if !matches!(recovered.value, RuntimeValue::Controller(_)) {
                    return recovered;
                }
            }
        }
        if name != "session_control" {
            return self.controller_handle(ctx, name, raw);
        }
        let input = match session_input(raw) {
            Ok(input) => input,
            Err(error) => return RuntimeOutcome::failed(error),
        };
        if input.action == "stop" {
            return self.locked_handle(ctx, name, raw);
        }
        if input.action != "start" && input.action != "reset" {
            return self.session_with_preparation(ctx, name, raw, &input.action);
        }
        // A misspelled key is refused before a preparation of minutes runs.
        if let Some(key) = input
            .arguments
            .keys()
            .find(|key| !crate::agentcontrol::LAUNCH_ARGUMENTS.contains(&key.as_str()))
        {
            return RuntimeOutcome::failed(anyhow!(
                "unknown start/reset argument {}; the arguments are {}",
                jsondoc::quoted_text(key),
                crate::agentcontrol::LAUNCH_ARGUMENTS.join(", ")
            ));
        }
        // A preparation holds the project action lock for as long as it
        // runs, minutes for a large project. A start or a reset that waited
        // for the lock would hang that long; it comes back with the stage
        // instead. A preparation that activates starts the session itself.
        if !self.baseline().is_empty()
            && let Some(progress) = self.preparation_progress()
        {
            // The job starts a session that ran when it replaces it, and
            // leaves a stopped one stopped.
            let stopped = self
                .0
                .controller
                .as_ref()
                .is_some_and(|controller| controller.status().phase == "stopped");
            let message = match stopped {
                true => {
                    "a preparation of the project is running, and it does not start a stopped session: reissue this call after the preparation has ended (preparation names its stage)"
                }
                false => {
                    "a preparation of the project is running; when it activates, the session starts on it by itself: wait with session_control(wait), and reissue this call only after the preparation has ended"
                }
            };
            let reply = map_reply(obj([
                ("success", false.into()),
                ("status", "result_pending".into()),
                ("phase", "preparing".into()),
                ("backend", "project_action_job".into()),
                ("message", message.into()),
                ("preparation", Any::Object(progress)),
            ]));
            return match reply {
                Ok(value) => RuntimeOutcome::handler(value),
                Err(error) => RuntimeOutcome::failed(error),
            };
        }
        let selected = text(&input.arguments, "scene");
        let mut scene = String::new();
        // The first preparation holds the project action lock for as long as
        // it runs. A reissued start must come back with its progress at once,
        // so it does not queue behind that lock.
        let first_preparation = self.baseline().is_empty() && self.preparation_progress().is_some();
        let configured = if first_preparation {
            scene = self.scene();
            Ok(None)
        } else {
            self.locked(ctx, || -> Option<RuntimeOutcome> {
                let baseline = self.baseline();
                scene = self.scene();
                if baseline.is_empty() {
                    return None;
                }
                if !selected.is_empty() && selected != scene {
                    return Some(RuntimeOutcome::failed(fail(
                        "scene_requires_preparation",
                        SCENE,
                    )));
                }
                // The inputs of a preparation do nothing to a session that
                // has one; a caller that gives them must not be told that the
                // Player runs with them.
                let given: Vec<&str> = ["hostPlayer", "nativeCatalog", "engineProfile"]
                    .into_iter()
                    .filter(|key| !text(&input.arguments, key).is_empty())
                    .chain(input.arguments.get("rebuildHost").is_some_and(Any::is_true).then_some("rebuildHost"))
                    .collect();
                if !given.is_empty() {
                    return Some(RuntimeOutcome::failed(fail(
                        "preparation_input_ignored",
                        format!(
                            "{} {} an input of a preparation, and this session already has one, so a start or reset would ignore it; call project_prepare with this scene, the input, a new requestId and activate true: the session restarts on that preparation when it activates",
                            given.join(", "),
                            if given.len() == 1 { "is" } else { "are" }
                        ),
                    )));
                }
                Some(self.controller_handle(ctx, name, raw))
            })
        };
        match configured {
            Err(error) => return RuntimeOutcome::failed(error),
            Ok(Some(outcome)) => return outcome,
            Ok(None) => {}
        }
        if !selected.is_empty() {
            scene = selected.clone();
        }
        let mut id = input.request_id.clone();
        if id.is_empty() {
            let mut jobs = self.0.jobs.lock().unwrap();
            if jobs.auto_preparation_id.is_empty() {
                jobs.auto_preparation_id = agent_id();
            }
            id = jobs.auto_preparation_id.clone();
        }
        let mut preparation = obj([
            ("scene", scene.into()),
            ("requestId", id.into()),
            ("activate", true.into()),
        ]);
        // Optional preparation inputs travel with the start. Without them the
        // preparation finds and reuses the newest valid host by itself.
        for key in ["hostPlayer", "nativeCatalog", "engineProfile"] {
            let value = text(&input.arguments, key);
            if !value.is_empty() {
                preparation.insert(key.into(), value.into());
            }
        }
        if input.arguments.get("rebuildHost").is_some_and(Any::is_true) {
            preparation.insert("rebuildHost".into(), true.into());
        }
        let encoded = match super::marshal(&Any::Object(preparation)) {
            Ok(encoded) => encoded,
            Err(error) => return RuntimeOutcome::failed(error),
        };
        let result = match self.handle(ctx, "project_prepare", &encoded) {
            Ok(result) => result,
            Err(error) => return RuntimeOutcome::failed(error),
        };
        if let Ok(Json::Object(members)) = jsondoc::parse(reply_bytes(&result))
            && members
                .iter()
                .rev()
                .find(|member| member.key == "status")
                .is_some_and(|status| status.value.as_str() == Some("result_pending"))
        {
            return RuntimeOutcome::handler(result);
        }
        self.locked(ctx, || {
            let (activated, selected_scene) = (!self.baseline().is_empty(), self.scene());
            if !activated {
                return RuntimeOutcome::failed(fail(
                    "preparation_not_activated",
                    "completed preparation did not activate a controller baseline",
                ));
            }
            if !selected.is_empty() && selected != selected_scene {
                return RuntimeOutcome::failed(fail("scene_requires_preparation", SCENE));
            }
            self.controller_handle(ctx, name, raw)
        })
        .unwrap_or_else(RuntimeOutcome::failed)
    }
}
