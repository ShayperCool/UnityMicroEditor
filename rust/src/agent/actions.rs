//! The service entry points, the project action lock, the Editor client, package
//! installation and the historical Editor-backed preparation.
use super::{
    Any, AnyMap, Preparation, Service, UNITY_VERSION, authored_scene, fail, fail_with, join,
    join_errors, json_reply, map_reply, marshal, not_exist, nz, obj, operation_id, read_json, stat,
    text, unmarshal_any, unmarshal_map,
};
use crate::{
    agentcontrol::{self, Reply},
    authoring::AuthoringError,
    cancel::{Context, Source, select},
    compiler::{self, CompileManifest},
    editorbridge::{self, Client, Request, Response},
    jsondoc,
    mcp::HandlerValue,
    project::{is_owned_cache_path, lexpath, value},
    sha256, sysutil,
    util::{
        agent_canonical, agent_id, agent_immutable_json, file_hash, micro_editor_root,
        read_agent_file, valid_sha256,
    },
};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::{io::Write, os::fd::AsRawFd, sync::TryLockError, time::Duration};

/// Checks a confirmed preparation before it
/// can stop an accepted runtime. This reads descriptors and file identities
/// only; Editor/SDK work is never replayed.
pub fn validate_cached_preparation(
    project: &str,
    editor: &str,
    scene: &str,
    id: &str,
    directory: &str,
    result: &Preparation,
) -> Result<()> {
    const CODE: &str = "invalid_preparation_receipt";
    if result.schema_version != 1
        || result.project != project
        || result.scene != scene
        || result.request_id != id
        || result.baseline != join(&[directory, "current.json"])
    {
        return Err(fail(
            CODE,
            "cached preparation does not match its exact project, scene, requestId and owned baseline",
        ));
    }
    let regular_owned = |path: &str, label: &str| -> Result<()> {
        if !sysutil::is_abs(path) || !is_owned_cache_path(project, path) {
            return Err(fail(
                CODE,
                format!("cached {label} is outside owned project caches"),
            ));
        }
        agent_canonical(path)?;
        if !super::regular(path) {
            return Err(fail(
                CODE,
                format!("cached {label} is missing or is not a canonical regular file"),
            ));
        }
        Ok(())
    };
    regular_owned(&result.baseline, "baseline")?;
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Baseline {
        #[serde(deserialize_with = "nz")]
        schema_version: i64,
        #[serde(deserialize_with = "nz")]
        project: String,
        #[serde(deserialize_with = "nz")]
        manifest: String,
        #[serde(deserialize_with = "nz")]
        player: String,
        #[serde(deserialize_with = "nz")]
        compiler: CompileManifest,
    }
    let baseline: Baseline = read_json(&result.baseline, 32 << 20)?;
    if baseline.schema_version != 1
        || baseline.project != project
        || baseline.compiler.schema != 1
        || baseline.compiler.project != project
        || baseline.compiler.unity != editor
        || baseline.compiler.compiler_inputs.is_empty()
    {
        return Err(fail(
            CODE,
            "cached baseline schema/project/compiler identity is invalid",
        ));
    }
    regular_owned(&baseline.manifest, "source manifest")?;
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct Manifest {
        #[serde(deserialize_with = "nz")]
        schema_version: i64,
        #[serde(deserialize_with = "nz")]
        project_root: String,
        #[serde(deserialize_with = "nz")]
        scene_path: String,
        #[serde(deserialize_with = "nz")]
        unity_version: String,
        #[serde(deserialize_with = "nz")]
        supported: bool,
    }
    let manifest: Manifest = read_json(&baseline.manifest, 32 << 20)?;
    if manifest.schema_version != 1
        || !manifest.supported
        || manifest.project_root != project
        || manifest.scene_path != scene
        || manifest.unity_version != UNITY_VERSION
    {
        return Err(fail(
            CODE,
            "cached source manifest does not match the supported project/scene/engine profile",
        ));
    }
    regular_owned(&baseline.player, "Player")?;
    if !result.artifact_receipt.is_empty() {
        return super::validate_preparation_artifacts(
            project,
            &result.baseline,
            &baseline.player,
            &result.artifact_receipt,
        );
    }
    if baseline.compiler.backend == "authored_source" {
        return Err(fail(
            CODE,
            "source preparation lacks its frozen generated artifact inventory",
        ));
    }
    for assembly in &baseline.compiler.assemblies {
        match file_hash(&assembly.path) {
            Ok(actual) if valid_sha256(&assembly.sha256) && actual == assembly.sha256 => {}
            Ok(_) => return Err(anyhow!("cached compiled assembly changed")),
            Err(error) => {
                return Err(join_errors(
                    "cached compiled assembly changed",
                    Some(&error),
                ));
            }
        }
    }
    Ok(())
}

/// The arguments of the Editor's `build_project`.
pub fn project_build_arguments(args: &AnyMap) -> Result<Vec<u8>> {
    let development = match args.get("development") {
        None => false,
        Some(Any::Bool(flag)) => *flag,
        Some(_) => return Err(fail("invalid_request", "development must be a boolean")),
    };
    marshal(&Any::Object(obj([
        ("scenes", args.get("scenes").cloned().unwrap_or(Any::Null)),
        ("development", development.into()),
    ])))
}

/// Whether the project itself declares or embeds the runtime package. The
/// target's manifest is authored source: it is read here and never written.
pub(crate) fn require_declared_agent_package(project: &str) -> Result<()> {
    const NAME: &str = crate::packages::RUNTIME_PACKAGE;
    let embedded = join(&[project, "Packages", NAME, "package.json"]);
    if super::regular(&embedded) {
        return Ok(());
    }
    let manifest = read_agent_file(&join(&[project, "Packages/manifest.json"]), 1 << 20)?;
    let declared = unmarshal_map(&manifest)?
        .unwrap_or_default()
        .get("dependencies")
        .and_then(Any::as_object)
        .is_some_and(|dependencies| dependencies.contains_key(NAME));
    if declared {
        return Ok(());
    }
    let package = micro_editor_root().map(|tool| join(&[&tool, "Packages", NAME]))?;
    Err(fail_with(
        "runtime_package_missing",
        "this action runs in the target project's own Editor, which needs the runtime package; the tool does not edit Packages/manifest.json, so declare the package there yourself",
        obj([
            ("package", NAME.into()),
            ("dependency", format!("file:{package}").into()),
            ("manifest", "Packages/manifest.json".into()),
        ]),
    ))
}

/// Installs the runtime package into an owned staging clone. It preserves
/// unrelated JSON entries and refuses a different installed package. The
/// target project's manifest is never passed here.
pub fn install_agent_package(project: &str) -> Result<()> {
    const NAME: &str = crate::packages::RUNTIME_PACKAGE;
    let tool = micro_editor_root()?;
    let package_root = lexpath::eval_symlinks(&join(&[&tool, "Packages", NAME]))?;
    let path = join(&[project, "Packages/manifest.json"]);
    let before = read_agent_file(&path, 1 << 20)?;
    let mut manifest = unmarshal_map(&before)?.unwrap_or_default();
    let Some(Any::Object(dependencies)) = manifest.get_mut("dependencies") else {
        return Err(anyhow!("package manifest dependencies must be an object"));
    };
    let expected = format!("file:{package_root}");
    let mut changed = false;
    if let Some(installed) = dependencies.get(NAME) {
        let same = *installed == Any::String(expected.clone())
            || matches!(installed, Any::String(location)
                if location.strip_prefix("file:").is_some_and(|physical| {
                    let physical = if sysutil::is_abs(physical) {
                        physical.to_string()
                    } else {
                        join(&[project, "Packages", physical])
                    };
                    lexpath::eval_symlinks(&physical).is_ok_and(|canonical| canonical == package_root)
                }));
        if !same {
            return Err(fail(
                "package_conflict",
                "the target already requests another UnityMicroEditor package; resolve it explicitly before preparation",
            ));
        }
    } else {
        dependencies.insert(NAME.into(), expected.into());
        changed = true;
    }
    // The packages the runtime package needs from the project's release line,
    // unless the project declares them itself.
    for (name, version) in crate::engine::of_project(project)?.runtime_dependencies {
        if !dependencies.contains_key(*name) {
            dependencies.insert((*name).into(), (*version).into());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let mut after = jsondoc::marshal_indent(&Any::Object(manifest)).map_err(|e| anyhow!(e))?;
    after.push(b'\n');
    let backup = join(&[
        project,
        "Library/UnityMicroEditor/package-install",
        &format!("{}.json", sha256(&before)),
    ]);
    agent_immutable_json(
        &backup,
        &Any::Object(obj([
            ("schemaVersion", 1.into()),
            ("project", project.into()),
            ("path", "Packages/manifest.json".into()),
            // Invalid UTF-8 becomes U+FFFD when the string is encoded.
            (
                "before",
                String::from_utf8_lossy(&before).into_owned().into(),
            ),
            ("beforeSha256", sha256(&before).into()),
            ("afterSha256", sha256(&after).into()),
        ])),
    )?;
    if read_agent_file(&path, 1 << 20)? != before {
        return Err(fail(
            "revision_conflict",
            "package manifest changed during preparation",
        ));
    }
    let directory = sysutil::dir(&path);
    let mut temporary = tempfile::Builder::new()
        .prefix(".micro-editor-manifest-")
        .tempfile_in(&directory)
        .map_err(|error| {
            sysutil::Error::os(
                "open",
                &join(&[&directory, ".micro-editor-manifest-"]),
                &error,
            )
        })?;
    temporary.write_all(&after)?;
    temporary.as_file().sync_all()?;
    let name = temporary.path().to_string_lossy().into_owned();
    temporary.persist(&path).map_err(|error| {
        anyhow!(
            "rename {name} {path}: {}",
            sysutil::errno_text(&error.error)
        )
    })?;
    Ok(())
}

/// The result of a completed, successful Editor call.
pub(crate) fn require_response(response: &Response) -> Result<Vec<u8>> {
    let details = || {
        obj([
            ("receipt", response.receipt.clone().into()),
            ("requestId", response.request_id.clone().into()),
            ("backend", response.backend.clone().into()),
        ])
    };
    if response.status != "completed" {
        return Err(fail_with(
            &response.status,
            "Editor result is not confirmed; retry the same requestId",
            details(),
        ));
    }
    if !response.success {
        let (code, message) = match &response.error {
            Some(error) => (error.code.as_str(), error.message.as_str()),
            None => ("editor_rejected", "Editor rejected operation"),
        };
        return Err(fail_with(code, message, details()));
    }
    Ok(response.result.clone())
}
/// The Editor call followed by `require_response`. The response is the default
/// value when the call itself failed.
pub(crate) fn request(
    client: &Client,
    ctx: &Context,
    id: &str,
    action: &str,
    arguments: &[u8],
) -> (Response, Result<Vec<u8>>) {
    match client.do_request(
        ctx,
        Request {
            id: id.into(),
            action: action.into(),
            arguments: arguments.to_vec(),
            expected_pid: 0,
        },
    ) {
        Ok(response) => {
            let result = require_response(&response);
            (response, result)
        }
        Err(error) => (Response::default(), Err(error.into())),
    }
}

/// The value and error of a `Controller::handle` call.
pub(crate) struct RuntimeOutcome {
    pub value: RuntimeValue,
    pub error: Option<anyhow::Error>,
}
pub(crate) enum RuntimeValue {
    Nil,
    Controller(Reply),
    Handler(HandlerValue),
}
impl RuntimeOutcome {
    pub fn failed(error: impl Into<anyhow::Error>) -> Self {
        Self {
            value: RuntimeValue::Nil,
            error: Some(error.into()),
        }
    }
    pub fn handler(value: HandlerValue) -> Self {
        Self {
            value: RuntimeValue::Handler(value),
            error: None,
        }
    }
    fn controller(outcome: std::result::Result<Reply, agentcontrol::Failure>) -> Self {
        match outcome {
            Ok(reply) => Self {
                value: RuntimeValue::Controller(reply),
                error: None,
            },
            Err(failure) => Self {
                value: failure
                    .reply
                    .map_or(RuntimeValue::Nil, RuntimeValue::Controller),
                // A failure with a code, such as `full_recompile_required`,
                // reaches the agent as a typed error with its details.
                error: Some(match failure.typed {
                    Some(typed) => AuthoringError {
                        code: typed.code.into(),
                        message: failure.error.message().into(),
                        details: Some(typed.details),
                    }
                    .into(),
                    None => failure.error.into(),
                }),
            },
        }
    }
    /// What the MCP server receives: the value, or the error alone.
    pub fn finish(self) -> Result<HandlerValue> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(match self.value {
            RuntimeValue::Nil => HandlerValue::Nil,
            RuntimeValue::Handler(value) => value,
            RuntimeValue::Controller(reply) => {
                let bytes = jsondoc::marshal(&reply).map_err(|e| anyhow!(e))?;
                match reply {
                    // `Controller.Request` returns the decoded receipt object.
                    Reply::Value(serde_json::Value::Object(_)) => HandlerValue::Map(bytes),
                    Reply::Value(serde_json::Value::Null) => HandlerValue::Nil,
                    _ => HandlerValue::Json(bytes),
                }
            }
        })
    }
}

/// The `session_control` input both runtime paths decode.
#[derive(Default)]
pub(crate) struct SessionInput {
    pub action: String,
    pub request_id: String,
    pub arguments: AnyMap,
}
pub(crate) fn session_input(raw: &[u8]) -> Result<SessionInput> {
    let mut input = SessionInput::default();
    let members = match unmarshal_any(raw)? {
        Any::Null => return Ok(input),
        Any::Object(members) => members,
        other => {
            return Err(anyhow!(
                "json: cannot unmarshal {} into value of type object",
                super::json_kind(&other)
            ));
        }
    };
    let mismatch = |field: &str, value: &Any, kind: &str| {
        anyhow!(jsondoc::Target { strukt: "", field }.mismatch(super::json_kind(value), kind))
    };
    // Member names match case-insensitively; the server sends exact names.
    let mut first = None;
    for (key, member) in members {
        let outcome = match key.to_ascii_lowercase().as_str() {
            "action" | "requestid" => {
                let target = if key.eq_ignore_ascii_case("action") {
                    &mut input.action
                } else {
                    &mut input.request_id
                };
                match member {
                    Any::Null => Ok(()),
                    Any::String(text) => {
                        *target = text;
                        Ok(())
                    }
                    other => Err(mismatch(&key, &other, "string")),
                }
            }
            "arguments" => match member {
                Any::Null => Ok(()),
                Any::Object(map) => {
                    input.arguments = map;
                    Ok(())
                }
                other => Err(mismatch(&key, &other, "object")),
            },
            _ => Ok(()),
        };
        if let Err(error) = outcome {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(input), Err)
}

impl Service {
    /// Cancels detached jobs, then stops the owned Editor.
    pub fn close(&self) {
        self.close_remote_session();
        self.close_action_jobs();
        let _guard = self
            .0
            .mu
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = self.client() {
            let _ = client.close();
        }
    }

    /// All coordinators sharing a project serialize preparation and
    /// publication. Unity's own project lock is a second boundary, not our
    /// transaction lock.
    pub(crate) fn locked<T>(&self, ctx: &Context, work: impl FnOnce() -> T) -> Result<T> {
        let wait = |ctx: &Context| -> Result<()> {
            if select(&[Source::Context(ctx)], Some(Duration::from_millis(50))).is_some() {
                return Err(ctx.wait().into());
            }
            Ok(())
        };
        let _guard = loop {
            match self.0.mu.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => wait(ctx)?,
            }
        };
        ctx.err()?;
        let directory = join(&[&self.0.project, "Library/UnityMicroEditor"]);
        agent_canonical(&directory)?;
        sysutil::mkdir_all(&directory)?;
        let root = sysutil::Root::open(&directory)?;
        let lock = root.open_file(
            "project-actions.lock",
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
            0o600,
        )?;
        loop {
            // SAFETY: `lock` is an open descriptor for the duration of the call.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(anyhow!(sysutil::errno_text(&error)));
            }
            wait(ctx)?;
        }
        struct Unlock<'a>(&'a std::fs::File);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                // SAFETY: the descriptor is still open; unlocking cannot fail usefully.
                unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
            }
        }
        let _unlock = Unlock(&lock);
        ctx.err()?;
        Ok(work())
    }

    /// The Editor of the target project, started or attached.
    pub(crate) fn editor_client(&self, ctx: &Context) -> Result<Client> {
        if self.0.no_editor {
            return Err(fail(
                "editor_disabled",
                "main Editor execution is disabled; provide a compatible cached engine/host/native profile",
            ));
        }
        let client = match self.client() {
            Some(client) => client,
            None => {
                let client = Client::new(
                    &self.lifetime(),
                    editorbridge::Options {
                        project: self.0.project.clone(),
                        editor: self.0.editor.clone(),
                        startup_timeout: Duration::from_secs(600),
                        ..Default::default()
                    },
                )?;
                self.set_client(Some(client.clone()));
                client
            }
        };
        // The target's own Editor reaches the bridge only through a package the
        // project already declares. Preparation never writes that declaration.
        require_declared_agent_package(&self.0.project)?;
        client.own_process_or_attach(ctx)?;
        Ok(client)
    }

    /// The project-actions backend of the MCP server.
    pub fn handle(&self, ctx: &Context, action: &str, raw: &[u8]) -> Result<HandlerValue> {
        if self.0.asynchronous && super::jobs::async_action(action) {
            let service = self.clone();
            let run: super::jobs::Run =
                std::sync::Arc::new(move |ctx, action, raw| service.handle_sync(ctx, action, raw));
            let result = self.enqueue_action(ctx, action, raw, run)?;
            if action == "project_prepare" {
                return self.restore_prepared_job_activation(result, raw);
            }
            return Ok(result);
        }
        self.handle_sync(ctx, action, raw)
    }

    pub fn handle_sync(&self, ctx: &Context, action: &str, raw: &[u8]) -> Result<HandlerValue> {
        if raw.len() > 1 << 20 {
            return Err(fail("invalid_request", "project action exceeds 1 MiB"));
        }
        let Ok((Any::Object(args), used)) = value::parse_json(raw, true) else {
            return Err(fail(
                "invalid_request",
                "project arguments must be an object",
            ));
        };
        if !jsondoc::rest_is_space(raw, used) {
            return Err(fail(
                "invalid_request",
                "project arguments must contain exactly one object",
            ));
        }
        self.locked(ctx, || -> Result<HandlerValue> {
            let mut id = text(&args, "requestId");
            if id.is_empty() {
                id = text(&args, "operationId");
            }
            self.progress_begin(action, &id);
            struct End<'a>(&'a Service);
            impl Drop for End<'_> {
                fn drop(&mut self) {
                    self.0.progress_end();
                }
            }
            let _end = End(self);
            match action {
                "project_prepare" => {
                    // A request of the configuration recovery names the
                    // sources it was made for. When they have changed since
                    // (the edit was taken back while the job ran), the job
                    // answers a rejection the session no longer has: a
                    // failure is then no word about the sources of its name,
                    // and a preparation is published and replaces no Player.
                    let recovery = id.starts_with("config-");
                    let scene = text(&args, "scene");
                    let result = match self.prepare(ctx, &args) {
                        Ok(result) => result,
                        Err(error)
                            if recovery
                                && !super::context_error(&error)
                                && !self.recovery_current(&id, &scene) =>
                        {
                            return Err(fail(
                                super::configuration::SUPERSEDED,
                                format!(
                                    "the sources changed while they were prepared: {}",
                                    super::error_text(&error)
                                ),
                            ));
                        }
                        Err(error) => return Err(error),
                    };
                    let activate = !matches!(args.get("activate"), Some(Any::Bool(false)))
                        && (!recovery || self.recovery_current(&id, &result.scene));
                    if activate && let Some(controller) = &self.0.controller {
                        let running = controller.status().phase != "stopped";
                        self.0
                            .swapping
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        let swapped = (|| -> Result<()> {
                            if running {
                                controller.stop(ctx).map_err(|failure| failure.error)?;
                            }
                            controller.configure_prepared(&result.baseline, &result.scene)?;
                            self.select(&result.baseline, &result.scene);
                            if running {
                                controller.start()?;
                            }
                            Ok(())
                        })();
                        self.0
                            .swapping
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        swapped?;
                    }
                    json_reply(&result)
                }
                "project_build" => {
                    let encoded = project_build_arguments(&args)?;
                    let client = self.editor_client(ctx)?;
                    let mut id = text(&args, "requestId");
                    if id.is_empty() {
                        id = agent_id();
                    }
                    if !operation_id(&id) {
                        return Err(fail(
                            "invalid_request",
                            "requestId must be at most 64 ASCII letters/digits/hyphens",
                        ));
                    }
                    let response = client.do_request(
                        ctx,
                        Request {
                            id,
                            action: "build_project".into(),
                            arguments: encoded,
                            expected_pid: 0,
                        },
                    )?;
                    // Keep backend and request receipt visible even for build failure.
                    let encoded = marshal(&response)?;
                    map_reply(unmarshal_map(&encoded)?.unwrap_or_default())
                }
                "prefab_edit" => self.prefab_edit(ctx, &args),
                "component_schema" => self.component_schema(ctx, &args),
                "component_add" => self.component_add(ctx, &args),
                _ => Err(fail(
                    "unsupported_operation",
                    format!("unknown project action {action}"),
                )),
            }
        })?
    }

    /// Retained for historical Editor-prepared receipts. The
    /// active coordinator uses `prepare_engine_bootstrap` followed by our
    /// source graph and SDK compiler.
    pub fn prepare_editor(&self, ctx: &Context, args: &AnyMap) -> Result<Preparation> {
        let project = self.0.project.as_str();
        let mut scene = text(args, "scene");
        if scene.is_empty() {
            scene = self.scene();
        }
        if !authored_scene(&scene) {
            return Err(fail(
                "invalid_request",
                "preparation requires an explicit authored Assets/...unity scene",
            ));
        }
        let mut id = text(args, "requestId");
        if id.is_empty() {
            id = agent_id();
        }
        if !operation_id(&id) {
            return Err(fail("invalid_request", "invalid preparation requestId"));
        }
        let mut host = text(args, "hostPlayer");
        if host.is_empty() {
            host = self.0.host.clone();
        }
        let directory = join(&[project, "Library/UnityMicroEditor/preparations", &id]);
        agent_immutable_json(
            &join(&[&directory, "request.json"]),
            &Any::Object(obj([
                ("schemaVersion", 1.into()),
                ("project", project.into()),
                ("scene", scene.clone().into()),
                ("hostPlayer", host.clone().into()),
                ("editor", self.0.editor.clone().into()),
                ("nativeCatalog", text(args, "nativeCatalog").into()),
            ])),
        )?;
        let result_path = join(&[&directory, "result.json"]);
        match read_json::<Preparation>(&result_path, 1 << 20) {
            Ok(result) => {
                validate_cached_preparation(
                    project,
                    &self.0.editor,
                    &scene,
                    &id,
                    &directory,
                    &result,
                )?;
                return Ok(result);
            }
            Err(error) if !not_exist(&error) => return Err(error),
            Err(_) => {}
        }
        let mut client = self.editor_client(ctx)?;
        let phase = |name: &str| super::staging::phase_id(&id, name);
        // An attached Editor may still hold the previously compiled local package.
        // Import current source before asking it to build the reusable host.
        request(&client, ctx, &phase("initial-refresh"), "refresh", b"{}").1?;
        let mut host_evidence_path = String::new();
        if host.is_empty() {
            let (player, evidence) = super::host::prepare_agent_host(ctx, self, &id)?;
            host = player;
            host_evidence_path = join(&[&directory, "host-evidence.json"]);
            agent_immutable_json(&host_evidence_path, &Any::Object(evidence))?;
            client = self.editor_client(ctx)?;
        }
        // Export after the build restores public Player settings. Each phase gets
        // its own stable ID so a retry cannot rebuild or export different bytes.
        request(&client, ctx, &phase("refresh"), "refresh", b"{}").1?;
        let graph = request(&client, ctx, &phase("graph"), "export_graph", b"{}").1?;
        let graph_path = join(&[&directory, "graph.json"]);
        agent_immutable_json(&graph_path, &unmarshal_any(&graph)?)?;
        let arguments = marshal(&Any::Object(obj([("scene", scene.clone().into())])))?;
        let proof = request(&client, ctx, &phase("proof"), "scene_proof", &arguments).1?;
        let proof_path = join(&[&directory, "scene-proof.json"]);
        agent_immutable_json(&proof_path, &unmarshal_any(&proof)?)?;
        let compiler_path = join(&[&directory, "compiler.json"]);
        if let Err(error) = stat(&compiler_path) {
            if !not_exist(&error) {
                return Err(error);
            }
            compiler::prepare_compiler_graph(
                ctx,
                project,
                &self.0.editor,
                &host,
                &graph_path,
                &compiler_path,
            )?;
        }
        let mut native_catalog = text(args, "nativeCatalog");
        if native_catalog.is_empty() {
            let candidate = join(&[&directory, "native-catalog.json"]);
            match stat(&candidate) {
                Ok(_) => native_catalog = candidate,
                Err(error) if !not_exist(&error) => return Err(error),
                Err(_) => {
                    if super::native::may_need_native(project)? {
                        native_catalog = self.prepare_native(ctx, &scene, &id, &directory)?;
                    }
                }
            }
        }
        let baseline = join(&[&directory, "current.json"]);
        if native_catalog.is_empty() {
            native_catalog = super::native::identity_catalog(project, &graph_path, &directory)?;
        }
        if let Err(error) = stat(&baseline) {
            if !not_exist(&error) {
                return Err(error);
            }
            super::prepare_agent_project_with_native(
                ctx,
                project,
                &self.0.editor,
                &compiler_path,
                &scene,
                &baseline,
                &proof_path,
                &native_catalog,
            )?;
        }
        let mut result = Preparation {
            schema_version: 1,
            request_id: id,
            project: project.into(),
            scene,
            baseline,
            host_player: host,
            native_catalog,
            host_evidence: host_evidence_path.clone(),
            backends: [
                ("sceneLoader", "raw_source"),
                ("compiler", "dotnet_sdk"),
                ("preparation", "unity_editor_public_api"),
                ("build", "unity_editor_proxy"),
            ]
            .map(|(key, value)| (key, value.to_string()))
            .into(),
            ..Default::default()
        };
        if !host_evidence_path.is_empty() {
            result
                .backends
                .insert("host", "isolated_unity_editor_proxy".to_string());
        }
        agent_immutable_json(&result_path, &result)?;
        Ok(result)
    }

    /// The runtime backend of the MCP server.
    pub fn runtime(&self, ctx: &Context, name: &str, raw: &[u8]) -> Result<HandlerValue> {
        self.runtime_outcome(ctx, name, raw).finish()
    }
    pub(crate) fn controller_handle(
        &self,
        ctx: &Context,
        name: &str,
        raw: &[u8],
    ) -> RuntimeOutcome {
        match &self.0.controller {
            Some(controller) => RuntimeOutcome::controller(controller.handle(ctx, name, raw)),
            None => RuntimeOutcome::failed(anyhow!("runtime controller is not configured")),
        }
    }
    /// `controller_handle` while holding the project action lock.
    pub(crate) fn locked_handle(&self, ctx: &Context, name: &str, raw: &[u8]) -> RuntimeOutcome {
        self.locked(ctx, || self.controller_handle(ctx, name, raw))
            .unwrap_or_else(RuntimeOutcome::failed)
    }
    pub(crate) fn runtime_outcome(&self, ctx: &Context, name: &str, raw: &[u8]) -> RuntimeOutcome {
        if name == "remote_session" {
            return match self.remote_session(raw) {
                Ok(value) => RuntimeOutcome::handler(value),
                Err(error) => RuntimeOutcome::failed(error),
            };
        }
        if self.0.asynchronous {
            let outcome = self.runtime_with_action_jobs(ctx, name, raw);
            return self.recover_configuration(ctx, name, raw, outcome);
        }
        if name == "session_control" {
            let input = match session_input(raw) {
                Ok(input) => input,
                Err(error) => return RuntimeOutcome::failed(error),
            };
            let starting = input.action == "start" || input.action == "reset";
            if starting {
                let prepared = self.locked(ctx, || -> Result<()> {
                    if !self.baseline().is_empty() {
                        return Ok(());
                    }
                    let mut scene = text(&input.arguments, "scene");
                    if scene.is_empty() {
                        scene = self.scene();
                    }
                    let result = self.prepare(ctx, &obj([("scene", scene.into())]))?;
                    let Some(controller) = &self.0.controller else {
                        return Err(anyhow!("runtime controller is not configured"));
                    };
                    controller.configure_prepared(&result.baseline, &result.scene)?;
                    self.select(&result.baseline, &result.scene);
                    Ok(())
                });
                if let Err(error) = prepared.and_then(|inner| inner) {
                    return RuntimeOutcome::failed(error);
                }
            }
            if starting || input.action == "stop" {
                return self.locked_handle(ctx, name, raw);
            }
        }
        self.controller_handle(ctx, name, raw)
    }
}
