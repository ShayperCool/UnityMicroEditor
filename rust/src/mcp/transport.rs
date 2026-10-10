//! Binds the server to MCP stdio and Streamable HTTP through `rmcp`.
//!
//! `rmcp` owns the JSON-RPC layer, so some details of the recorded wire format
//! cannot be reproduced: object members are written in `rmcp`'s order (and
//! schemas in key order), `arguments` reach the tools as a decoded object (member
//! order, duplicate keys and string escapes of the request are gone), and protocol
//! errors `rmcp` raises before a handler runs carry its own messages. The HTTP
//! front end itself answers every request that must be refused before it reaches
//! `rmcp`, with the recorded status and body.
//!
//! Not implemented: server limits (header read timeout 5 s, idle timeout
//! 90 s, header size 16 KiB). `axum::serve` keeps hyper's defaults.
use super::{
    CallError, HandlerError, Options, RuntimeHandler, Server, ToolDefinition, ToolResult,
    server::{INSTRUCTIONS, SERVER_NAME, SERVER_VERSION, encode_tree},
};
use crate::{
    cancel::{self, Context, Flag},
    flags::FlagSet,
    jsondoc::{self, Json},
    sysutil,
};
use anyhow::{Result, anyhow, bail};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::*,
    service::{
        NotificationContext, Peer, RequestContext, ServerInitializeError, SubscriptionContext,
    },
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    net::{IpAddr, SocketAddr, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;

/// Protocol versions the server supports, newest first.
const SUPPORTED_VERSIONS: [&str; 5] = [
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const NEW_PROTOCOL: &str = "2026-07-28";

pub fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut flags = FlagSet::new("mcp");
    flags.string("project", "", "explicit target Unity project root");
    flags.string("transport", "stdio", "stdio or http");
    flags.string("listen", "127.0.0.1:8090", "loopback HTTP listen address");
    flags.string(
        "token-file",
        "",
        "optional file containing an HTTP bearer token of at least 32 characters",
    );
    flags.parse(args)?;
    let options = Options {
        project: flags.get_string("project"),
        transport: flags.get_string("transport"),
        listen: flags.get_string("listen"),
        token_file: flags.get_string("token-file"),
        ..Options::default()
    };
    if !flags.args().is_empty() || options.project.is_empty() {
        bail!("use mcp --project /absolute/UnityProject [--transport stdio|http]");
    }
    if options.transport != "stdio" && options.transport != "http" {
        bail!("transport must be stdio or http");
    }
    Ok(options)
}
pub fn run(ctx: &Context, args: impl IntoIterator<Item = String>) -> Result<()> {
    run_with_runtime(ctx, args, None)
}
pub fn run_with_runtime(
    ctx: &Context,
    args: impl IntoIterator<Item = String>,
    runtime: Option<Arc<dyn RuntimeHandler>>,
) -> Result<()> {
    let mut options = parse_options(args)?;
    options.runtime = runtime;
    run_options(ctx, options)
}

// Splits `host:port` into host and port, with the error text for a bad address.
fn split_host_port(address: &str) -> Result<(String, String)> {
    let fail = |reason: &str| anyhow!("address {address}: {reason}");
    let Some(colon) = address.rfind(':') else {
        return Err(fail("missing port in address"));
    };
    let (host, rest_start, rest_end);
    if address.starts_with('[') {
        let Some(end) = address.find(']') else {
            return Err(fail("missing ']' in address"));
        };
        if end + 1 == address.len() {
            return Err(fail("missing port in address"));
        }
        if end + 1 != colon {
            return Err(fail(if address.as_bytes()[end + 1] == b':' {
                "too many colons in address"
            } else {
                "missing port in address"
            }));
        }
        host = &address[1..end];
        rest_start = 1;
        rest_end = end + 1;
    } else {
        host = &address[..colon];
        if host.contains(':') {
            return Err(fail("too many colons in address"));
        }
        rest_start = 0;
        rest_end = 0;
    }
    if address[rest_start..].contains('[') {
        return Err(fail("unexpected '[' in address"));
    }
    if address[rest_end..].contains(']') {
        return Err(fail("unexpected ']' in address"));
    }
    Ok((host.to_string(), address[colon + 1..].to_string()))
}

// The port a TCP listener resolves: a number or a service name.
fn lookup_port(port: &str) -> Result<u16> {
    if port.bytes().all(|byte| byte.is_ascii_digit()) {
        if port.is_empty() {
            return Ok(0);
        }
        return port
            .parse()
            .map_err(|_| anyhow!("listen tcp: address {port}: invalid port"));
    }
    let unknown = || anyhow!("listen tcp: lookup tcp/{port}: unknown port");
    let name = std::ffi::CString::new(port).map_err(|_| unknown())?;
    // SAFETY: both arguments are NUL-terminated; the returned entry is read
    // before any other call into the services database.
    let entry = unsafe { libc::getservbyname(name.as_ptr(), c"tcp".as_ptr()) };
    if entry.is_null() {
        return Err(unknown());
    }
    // SAFETY: a non-null entry points at a valid `servent`.
    Ok(u16::from_be(unsafe { (*entry).s_port } as u16))
}

/// Serves until the client disconnects (stdio) or `ctx` ends. Backends in `options`
/// are not closed here; the caller owns them.
pub fn run_options(ctx: &Context, mut options: Options) -> Result<()> {
    // An untyped error prints without a code.
    let server = Server::new(&options).map_err(|error| anyhow!("{}", HandlerError::from(error)))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    if options.transport.is_empty() || options.transport == "stdio" {
        let outcome = runtime.block_on(serve_stdio(ctx, server));
        // Handlers still running are abandoned, as when the process returns.
        runtime.shutdown_background();
        return outcome;
    }
    if options.transport != "http" {
        bail!("transport must be stdio or http");
    }
    if options.listen.is_empty() {
        options.listen = "127.0.0.1:8090".into();
    }
    let (host, port) = split_host_port(&options.listen)?;
    let Some(ip) = host.parse::<IpAddr>().ok().filter(IpAddr::is_loopback) else {
        bail!("HTTP MCP must listen on a literal loopback IP address");
    };
    let mut token = String::new();
    if !options.token_file.is_empty() {
        let bytes = fs::read(&options.token_file)
            .map_err(|error| sysutil::Error::os("open", &options.token_file, &error))?;
        token = String::from_utf8_lossy(&bytes).trim().to_string();
        if token.len() < 32 {
            bail!("HTTP bearer token must contain at least 32 characters");
        }
    }
    let address = SocketAddr::new(ip, lookup_port(&port)?);
    let listener = TcpListener::bind(address).map_err(|error| {
        anyhow!(
            "listen tcp {address}: bind: {}",
            sysutil::errno_text(&error)
        )
    })?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    eprintln!("Unity Micro Editor MCP: http://{address}/mcp");
    let outcome = runtime.block_on(serve_http(ctx, server, token, listener));
    runtime.shutdown_background();
    outcome
}

// Resolves when the context is done. The watcher thread ends with the
// returned guard, so a server that stops first leaves nothing behind.
struct Watch(Flag);
impl Drop for Watch {
    fn drop(&mut self) {
        self.0.set();
    }
}
fn watch(ctx: &Context) -> (Watch, tokio::sync::oneshot::Receiver<()>) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let (ctx, finished) = (ctx.clone(), Flag::new());
    let guard = Watch(finished.clone());
    std::thread::spawn(move || {
        let sources = [
            cancel::Source::Context(&ctx),
            cancel::Source::Flag(&finished),
        ];
        if cancel::select(&sources, None) == Some(0) {
            let _ = sender.send(());
        }
    });
    (guard, receiver)
}

#[derive(Clone)]
struct Handler {
    server: Server,
    /// Parent of every request context; stdio requests end with the server.
    lifetime: Context,
    /// Set by the first `initialize` of this session.
    initialized: Arc<AtomicBool>,
    /// One message per change of the `project.<name>` tool set.
    changes: broadcast::Sender<()>,
}
// A cursor that was not issued is answered with "invalid params"; no list
// of this server ever has a second page.
fn first_page(request: Option<PaginatedRequestParams>) -> std::result::Result<(), ErrorData> {
    match request.and_then(|request| request.cursor) {
        Some(cursor) if !cursor.is_empty() => Err(ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            "invalid params",
            None,
        )),
        _ => Ok(()),
    }
}
// Results of the per-request (2026-07-28) protocol name the server in `_meta`.
fn server_meta(context: &RequestContext<RoleServer>) -> Option<MetaObject> {
    context.meta.protocol_version()?;
    let mut meta = MetaObject::new();
    meta.0.insert(
        "io.modelcontextprotocol/serverInfo".into(),
        json!({"name": SERVER_NAME, "version": SERVER_VERSION}),
    );
    Some(meta)
}
fn method_not_found(method: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode::METHOD_NOT_FOUND,
        format!("method not found: {}", jsondoc::quoted_text(method)),
        None,
    )
}
// A list-changed notification is delayed by 10 ms, and the delay restarts on every
// further change. False once the server is gone.
async fn next_change(changes: &mut broadcast::Receiver<()>) -> bool {
    if matches!(
        changes.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ) {
        return false;
    }
    loop {
        match tokio::time::timeout(Duration::from_millis(10), changes.recv()).await {
            Ok(Err(broadcast::error::RecvError::Closed)) => return false,
            Ok(_) => continue,
            Err(_) => return true,
        }
    }
}
fn change_channel(server: &Server) -> broadcast::Sender<()> {
    let (sender, _) = broadcast::channel(16);
    let notifier = sender.clone();
    server.on_tools_changed(move || {
        let _ = notifier.send(());
    });
    sender
}
impl Handler {
    // Runs blocking server code under a context that follows the request.
    async fn blocking<T: Send + 'static>(
        &self,
        request: &RequestContext<RoleServer>,
        action: impl FnOnce(&Server, &Context) -> T + Send + 'static,
    ) -> std::result::Result<T, ErrorData> {
        let ctx = self.lifetime.with_cancel();
        let watcher = {
            let (ctx, token) = (ctx.clone(), request.ct.clone());
            tokio::spawn(async move {
                token.cancelled().await;
                ctx.cancel();
            })
        };
        let server = self.server.clone();
        let task = {
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || action(&server, &ctx))
        };
        let outcome = task.await;
        watcher.abort();
        ctx.cancel();
        outcome.map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }
}

fn definition(tool: &ToolDefinition) -> Tool {
    let object = |text: &str| -> JsonObject {
        serde_json::from_str(text).expect("tool schemas are JSON objects")
    };
    let mut converted = Tool::new(
        tool.name.clone(),
        tool.description.clone(),
        object(&tool.input_schema),
    );
    converted.output_schema = tool
        .output_schema
        .as_deref()
        .map(|text| Arc::new(object(text)));
    converted.annotations = Some(
        serde_json::from_value(json!({
            "readOnlyHint": tool.read_only,
            "idempotentHint": tool.idempotent,
            "openWorldHint": false,
        }))
        .expect("tool annotations"),
    );
    converted
}
fn tool_result(result: ToolResult) -> CallToolResult {
    let mut converted = CallToolResult::success(vec![ContentBlock::text(result.text)]);
    converted.structured_content = result
        .structured
        .map(|text| serde_json::from_str(&text).expect("the reply envelope is JSON"));
    // `isError` is omitted unless true.
    converted.is_error = result.is_error.then_some(true);
    converted
}
fn call_error(error: CallError) -> ErrorData {
    let message = error.to_string();
    match error {
        CallError::UnknownTool(_) => ErrorData::new(ErrorCode::INVALID_PARAMS, message, None),
        CallError::ResourceNotFound(uri) => ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            message,
            Some(json!({"uri": uri})),
        ),
        // A handler's plain error is reported as an internal error.
        CallError::Failed(_) => ErrorData::new(ErrorCode::INTERNAL_ERROR, message, None),
    }
}
// `{"content":[...],"structuredContent":...,"isError":true}` for requests
// `rmcp` could not decode into its typed `tools/call` parameters.
fn tool_result_value(result: ToolResult) -> Value {
    let mut value = json!({"content": [{"type": "text", "text": result.text}]});
    if let Some(structured) = result.structured {
        value["structuredContent"] =
            serde_json::from_str(&structured).expect("the reply envelope is JSON");
    }
    if result.is_error {
        value["isError"] = true.into();
    }
    value
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        let capabilities: ServerCapabilities = serde_json::from_value(json!({
            "logging": {},
            "resources": {"listChanged": true},
            "tools": {"listChanged": true},
        }))
        .expect("server capabilities");
        ServerConfig::new(capabilities)
            .with_server_info(
                serde_json::from_value(json!({"name": SERVER_NAME, "version": SERVER_VERSION}))
                    .expect("server info"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_instructions(INSTRUCTIONS)
    }
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, ErrorData> {
        if self.initialized.swap(true, Ordering::SeqCst) {
            // A plain error: it is sent with code 0.
            return Err(ErrorData::new(
                ErrorCode(0),
                "duplicate \"initialize\" received",
                None,
            ));
        }
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }
    // `logging/setLevel` is still answered.
    #[allow(deprecated)]
    async fn set_level(
        &self,
        _: SetLevelRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        Ok(())
    }
    // Newest first.
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(&[
            ProtocolVersion::V_2026_07_28,
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2024_11_05,
        ])
    }
    async fn discover(
        &self,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<DiscoverResult, ErrorData> {
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_cache_scope(CacheScope::Public))
    }
    async fn complete(
        &self,
        _: CompleteRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CompleteResult, ErrorData> {
        Err(method_not_found("completion/complete"))
    }
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<GetPromptResponse, ErrorData> {
        Err(ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            format!("unknown prompt {}", jsondoc::quoted_text(&request.name)),
            None,
        ))
    }
    #[allow(deprecated)]
    async fn subscribe(
        &self,
        _: SubscribeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        Err(method_not_found("resources/subscribe"))
    }
    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        _: UnsubscribeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        Err(method_not_found("resources/unsubscribe"))
    }
    // The list-changed kinds allowed: those of the server's capabilities.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut builder = SubscriptionFilter::builder();
        let mut any = false;
        if requested.tools_list_changed == Some(true) {
            builder = builder.tools_list_changed();
            any = true;
        }
        if requested.resources_list_changed == Some(true) {
            builder = builder.resources_list_changed();
            any = true;
        }
        any.then(|| builder.build())
    }
    async fn listen(&self, context: SubscriptionContext) -> std::result::Result<(), ErrorData> {
        let mut changes = self.changes.subscribe();
        let tools = context.accepted().tools_list_changed == Some(true);
        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                more = next_change(&mut changes) => {
                    if !more {
                        return Ok(());
                    }
                    if tools {
                        let _ = context.sink().notify_tool_list_changed().await;
                    }
                }
            }
        }
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        first_page(request)?;
        let tools = self.server.tools().iter().map(definition).collect();
        let mut result = ListToolsResult::with_all_items(tools)
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public);
        result.meta = server_meta(&context);
        Ok(result)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let arguments = request
            .arguments
            .map(|arguments| serde_json::to_vec(&arguments).expect("JSON object"));
        let result = self
            .blocking(&context, move |server, ctx| {
                server.call_tool(ctx, &name, arguments.as_deref())
            })
            .await?
            .map_err(call_error)?;
        let mut result = tool_result(result);
        result.meta = server_meta(&context);
        Ok(result.into())
    }
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        first_page(request)?;
        let resources = self
            .server
            .resources()
            .into_iter()
            .map(|resource| {
                let mut converted = Resource::new(resource.uri, resource.name);
                converted.mime_type = Some(resource.mime_type.into());
                converted
            })
            .collect();
        let mut result = ListResourcesResult::with_all_items(resources)
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public);
        result.meta = server_meta(&context);
        Ok(result)
    }
    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        first_page(request)?;
        let mut result = ListResourceTemplatesResult::with_all_items(Vec::new())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public);
        result.meta = server_meta(&context);
        Ok(result)
    }
    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListPromptsResult, ErrorData> {
        first_page(request)?;
        let mut result = ListPromptsResult::with_all_items(Vec::new())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public);
        result.meta = server_meta(&context);
        Ok(result)
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let result = self
            .blocking(&context, move |server, ctx| server.read_resource(ctx, &uri))
            .await?
            .map_err(call_error)?;
        let contents = ResourceContents::TextResourceContents {
            uri: result.uri,
            mime_type: Some(result.mime_type),
            text: result.text,
            meta: None,
        };
        let mut result = ReadResourceResult::new(vec![contents])
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Public);
        result.meta = server_meta(&context);
        Ok(result.into())
    }
    // Requests `rmcp` could not decode into a typed request arrive here; they are
    // decoded leniently and reported with this server's own errors.
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CustomResult, ErrorData> {
        let CustomRequest { method, params, .. } = request;
        let known = matches!(
            method.as_str(),
            "tools/call"
                | "resources/read"
                | "prompts/get"
                | "logging/setLevel"
                | "completion/complete"
                | "initialize"
                | "resources/subscribe"
                | "resources/unsubscribe"
        );
        if !known {
            return Err(method_not_found(&method));
        }
        let Some(params) = params.filter(|params| !params.is_null()) else {
            return Err(ErrorData::new(
                ErrorCode::INVALID_REQUEST,
                "invalid request: missing required \"params\"",
                None,
            ));
        };
        let field = |key: &str| -> std::result::Result<String, ErrorData> {
            match params.get(key) {
                None | Some(Value::Null) => Ok(String::new()),
                Some(Value::String(text)) => Ok(text.clone()),
                Some(_) => Err(ErrorData::new(
                    ErrorCode::INVALID_PARAMS,
                    format!("handling '{method}': invalid params"),
                    None,
                )),
            }
        };
        match method.as_str() {
            "tools/call" if params.is_object() => {
                let name = field("name")?;
                let arguments = params
                    .get("arguments")
                    .map(|arguments| serde_json::to_vec(arguments).expect("JSON value"));
                let result = self
                    .blocking(&context, move |server, ctx| {
                        server.call_tool(ctx, &name, arguments.as_deref())
                    })
                    .await?
                    .map_err(call_error)?;
                Ok(CustomResult::new(tool_result_value(result)))
            }
            "resources/read" if params.is_object() => {
                Err(call_error(CallError::ResourceNotFound(field("uri")?)))
            }
            _ => Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("handling '{method}': invalid params"),
                None,
            )),
        }
    }
    async fn on_initialized(&self, _: NotificationContext<RoleServer>) {}
}

// A session on the shared channel (stdio) is told about every change.
fn notify_tool_changes(mut changes: broadcast::Receiver<()>, peer: Peer<RoleServer>) {
    tokio::spawn(async move {
        while next_change(&mut changes).await {
            let _ = peer.notify_tool_list_changed().await;
        }
    });
}

async fn serve_stdio(ctx: &Context, server: Server) -> Result<()> {
    let changes = change_channel(&server);
    let receiver = changes.subscribe();
    let handler = Handler {
        server,
        lifetime: ctx.clone(),
        initialized: Arc::default(),
        changes,
    };
    let (_guard, mut cancelled) = watch(ctx);
    // An interrupted server returns the context error.
    let interrupted = || {
        anyhow!(
            "{}",
            ctx.error()
                .map(|error| error.to_string())
                .unwrap_or_default()
        )
    };
    let service = tokio::select! {
        service = handler.serve(stdio()) => match service {
            Ok(service) => service,
            // The client left before its first request: a clean end.
            Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        },
        Ok(()) = &mut cancelled => return Err(interrupted()),
    };
    notify_tool_changes(receiver, service.peer().clone());
    let cancellation = service.cancellation_token();
    tokio::select! {
        outcome = service.waiting() => {
            outcome?;
            Ok(())
        }
        Ok(()) = &mut cancelled => {
            cancellation.cancel();
            Err(interrupted())
        }
    }
}

struct HttpState {
    token: String,
}
fn plain(status: StatusCode, message: &str) -> Response {
    // Plain-text body with a trailing newline and `nosniff`.
    let mut response = (status, format!("{message}\n")).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}
// Whether a `Host` header names the loopback interface.
fn loopback_host(host: &str) -> bool {
    let name = match split_host_port(host) {
        Ok((name, _)) => name,
        Err(_) => host.trim_matches(['[', ']']).to_string(),
    };
    name == "localhost" || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
// The host (with port and without userinfo) of an `Origin` URL.
fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    Some(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host),
    )
}
// Cross-origin protection for unsafe methods, without bypasses or trusted origins.
fn cross_origin(method: &Method, headers: &HeaderMap, host: &str) -> Option<&'static str> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    match header_text(headers, "sec-fetch-site") {
        "" => {}
        "same-origin" | "none" => return None,
        _ => return Some("cross-origin request detected from Sec-Fetch-Site header"),
    }
    let origin = header_text(headers, "origin");
    if origin.is_empty() || origin_host(origin) == Some(host) {
        return None;
    }
    Some(
        "cross-origin request detected, and/or browser is out of date: Sec-Fetch-Site is missing, and Origin does not match Host",
    )
}
// The media type of a header value, lower-cased and without parameters.
fn base_media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}
fn accepts(headers: &HeaderMap) -> (bool, bool) {
    let (mut json_ok, mut stream_ok) = (false, false);
    for value in headers.get_all(header::ACCEPT) {
        for token in value.to_str().unwrap_or_default().split(',') {
            match base_media_type(token).as_str() {
                "application/json" | "application/*" => json_ok = true,
                "text/event-stream" | "text/*" => stream_ok = true,
                "*/*" => (json_ok, stream_ok) = (true, true),
                _ => {}
            }
        }
    }
    (json_ok, stream_ok)
}

/// (is a notification, may omit params) for the methods an MCP server receives.
fn method_info(method: &str) -> Option<(bool, bool)> {
    Some(match method {
        "completion/complete"
        | "prompts/get"
        | "tools/call"
        | "resources/read"
        | "logging/setLevel"
        | "resources/subscribe"
        | "resources/unsubscribe"
        | "subscriptions/listen"
        | "initialize" => (false, false),
        "server/discover"
        | "ping"
        | "prompts/list"
        | "tools/list"
        | "resources/list"
        | "resources/templates/list" => (false, true),
        "notifications/cancelled"
        | "notifications/initialized"
        | "notifications/roots/list_changed" => (true, true),
        "notifications/progress" => (true, false),
        _ => return None,
    })
}
/// A legacy-protocol POST body that is dispatched to the server.
struct Payload {
    /// The JSON text of each request or notification, in order.
    requests: Vec<Vec<u8>>,
    calls: usize,
    batch: bool,
}
// The offset of the byte a JSON syntax error names: the shortest prefix that
// is no longer just incomplete.
fn incomplete(error: &str) -> bool {
    error == jsondoc::END || error == "unexpected EOF"
}
fn syntax_error_offset(body: &[u8]) -> usize {
    (0..body.len())
        .find(|&end| jsondoc::parse_first(&body[..=end]).is_err_and(|error| !incomplete(&error)))
        .unwrap_or(body.len())
}
// The decoding error text for a value of the wrong type.
fn wrong_type(raw: &[u8], target: &str) -> String {
    format!(
        "unmarshaling jsonrpc message: json: cannot unmarshal {} into {target}",
        jsondoc::quoted_text(&String::from_utf8_lossy(raw))
    )
}
// The batch or single JSON-RPC message a POST body holds, or the message the
// POST is refused with.
fn inspect_payload(body: &[u8], version: &str) -> std::result::Result<Payload, String> {
    let malformed = |reason: String| format!("malformed payload: {reason}");
    // The decoder reads one value and ignores what follows it.
    let (json, end) = jsondoc::parse_first(body).map_err(|error| {
        malformed(if incomplete(&error) {
            "unmarshaling jsonrpc message: unexpected EOF".into()
        } else {
            let rest = &body[syntax_error_offset(body).min(body.len())..];
            format!(
                "unmarshaling jsonrpc message: json: {error}: {}",
                String::from_utf8_lossy(rest)
            )
        })
    })?;
    let start = body
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(0);
    let text = |json: &Json| {
        let mut out = Vec::new();
        encode_tree(json, &mut out);
        out
    };
    let (messages, batch) = match json {
        Json::Null => return Err(malformed("empty batch".into())),
        Json::Array(items) if items.is_empty() => return Err(malformed("empty batch".into())),
        Json::Array(items) => (
            items
                .into_iter()
                .map(|item| {
                    let raw = text(&item);
                    (item, raw)
                })
                .collect::<Vec<_>>(),
            true,
        ),
        other => (vec![(other, body[start..end].to_vec())], false),
    };
    let mut payload = Payload {
        requests: Vec::new(),
        calls: 0,
        batch,
    };
    let mut checks = Vec::new();
    for (message, raw) in messages {
        let Json::Object(members) = &message else {
            return Err(malformed(wrong_type(
                &raw,
                "value of type jsonrpc2.wireDecode",
            )));
        };
        let member = |key: &str| members.iter().rev().find(|member| member.key == key);
        let tag = match member("jsonrpc").map(|member| (&member.value, member)) {
            None | Some((Json::Null, _)) => "",
            Some((Json::String(tag), _)) => tag.as_str(),
            Some((_, member)) => {
                let tail = if batch {
                    &raw[..]
                } else {
                    &body[member.start..end]
                };
                return Err(malformed(wrong_type(
                    tail,
                    "struct field jsonrpc2.wireDecode.jsonrpc of type string",
                )));
            }
        };
        if tag != "2.0" {
            return Err(malformed(format!(
                "invalid message version tag {}; expected \"2.0\"",
                jsondoc::quoted_text(tag)
            )));
        }
        let call = match member("id").map(|member| &member.value) {
            None | Some(Json::Null) => false,
            Some(Json::String(_) | Json::Number(_)) => true,
            Some(other) => {
                let kind = match other {
                    Json::Bool(_) => "bool",
                    Json::Array(_) => "array",
                    _ => "object",
                };
                return Err(malformed(format!("parse error: invalid ID type {kind}")));
            }
        };
        let method = match member("method").map(|member| &member.value) {
            // A response: accepted and ignored by a stateless session.
            None | Some(Json::Null) => {
                if !call {
                    return Err(malformed("invalid request".into()));
                }
                continue;
            }
            Some(Json::String(method)) => method.clone(),
            Some(other) => {
                return Err(malformed(wrong_type(&text(other), "value of type string")));
            }
        };
        checks.push((method, call, member("params").is_some()));
        payload.calls += usize::from(call);
        payload.requests.push(raw);
    }
    if batch && version >= "2025-06-18" {
        return Err(format!(
            "JSON-RPC batching is not supported in 2025-06-18 and later (request version: {version})"
        ));
    }
    for (method, call, params) in checks {
        let quoted = jsondoc::quoted_text(&method);
        let Some((notification, optional_params)) = method_info(&method) else {
            return Err(format!("JSON RPC not handled: {quoted} unsupported"));
        };
        if notification && call {
            return Err(format!("invalid request: unexpected id for {quoted}"));
        }
        if !notification && !call {
            return Err(format!("invalid request: missing id for {quoted}"));
        }
        if !optional_params && !params {
            return Err("invalid request: missing required \"params\"".into());
        }
    }
    Ok(payload)
}

// The lexically cleaned request path, as it is before route matching.
fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    let rooted = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let mut cleaned = sysutil::clean(&rooted);
    if rooted.ends_with('/') && cleaned != "/" {
        cleaned.push('/');
    }
    cleaned
}

// Everything decided before a JSON-RPC message is dispatched: authorization,
// Host and Origin rules, media types and body validation.
async fn front(State(state): State<Arc<HttpState>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    if !state.token.is_empty() {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .map(HeaderValue::as_bytes)
            .unwrap_or_default();
        let supplied = authorization.strip_prefix(b"Bearer ");
        if !supplied.is_some_and(|supplied| bool::from(supplied.ct_eq(state.token.as_bytes()))) {
            let mut response = plain(StatusCode::UNAUTHORIZED, "unauthorized");
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"unity-micro-editor\""),
            );
            return response;
        }
    }
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let cleaned = clean_path(&path);
    if cleaned != path && method != Method::CONNECT {
        let mut location = cleaned.clone();
        if let Some(query) = request.uri().query() {
            location.push('?');
            location.push_str(query);
        }
        let body = if method == Method::GET {
            format!("<a href=\"{location}\">Temporary Redirect</a>.\n\n")
        } else {
            String::new()
        };
        let mut response = (StatusCode::TEMPORARY_REDIRECT, body).into_response();
        response.headers_mut().remove(header::CONTENT_TYPE);
        if let Ok(value) = HeaderValue::from_str(&location) {
            response.headers_mut().insert(header::LOCATION, value);
        }
        if method == Method::GET || method == Method::HEAD {
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
        }
        return response;
    }
    if path == "/health" {
        if method != Method::GET && method != Method::HEAD {
            let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return response;
        }
        let mut response =
            r#"{"status":"ready","service":"unity-micro-editor-mcp"}"#.into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return response;
    }
    if path != "/mcp" {
        return plain(StatusCode::NOT_FOUND, "404 page not found");
    }
    let host = header_text(headers, "host").to_string();
    if !loopback_host(&host) {
        return plain(
            StatusCode::FORBIDDEN,
            &format!(
                "Forbidden: invalid Host header {}",
                jsondoc::quoted_text(&host)
            ),
        );
    }
    if let Some(reason) = cross_origin(&method, headers, &host) {
        return plain(StatusCode::FORBIDDEN, reason);
    }
    let version = header_text(headers, "mcp-protocol-version").to_string();
    if !version.is_empty()
        && !SUPPORTED_VERSIONS.contains(&version.as_str())
        && version.as_str() < NEW_PROTOCOL
    {
        return plain(
            StatusCode::BAD_REQUEST,
            &format!(
                "Bad Request: Unsupported protocol version (supported versions: {})",
                SUPPORTED_VERSIONS.join(",")
            ),
        );
    }
    if method != Method::POST {
        let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    if base_media_type(header_text(headers, "content-type")) != "application/json" {
        return plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be 'application/json'",
        );
    }
    if accepts(headers) != (true, true) {
        return plain(
            StatusCode::BAD_REQUEST,
            "Accept must contain both 'application/json' and 'text/event-stream'",
        );
    }
    if headers.contains_key("last-event-id") {
        return plain(
            StatusCode::BAD_REQUEST,
            "can't send Last-Event-ID for POST request",
        );
    }
    let (parts, body) = request.into_parts();
    let body: Bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(_) => return plain(StatusCode::BAD_REQUEST, "failed to read body"),
    };
    if body.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "POST requires a non-empty body");
    }
    let effective = if version.is_empty() {
        "2025-03-26"
    } else {
        version.as_str()
    };
    let mut parts = parts;
    // `rmcp` wants both media types spelled out, where clients may send wildcards
    // and parameters.
    parts.headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    let forward = |body: Vec<u8>| {
        let mut request = Request::new(Body::from(body));
        *request.method_mut() = parts.method.clone();
        *request.uri_mut() = parts.uri.clone();
        *request.version_mut() = parts.version;
        *request.headers_mut() = parts.headers.clone();
        next.clone().run(request)
    };
    // Requests of the 2026-07-28 protocol are validated by `rmcp`.
    if effective >= NEW_PROTOCOL {
        return forward(body.to_vec()).await;
    }
    let payload = match inspect_payload(&body, effective) {
        Ok(payload) => payload,
        Err(message) => return plain(StatusCode::BAD_REQUEST, &message),
    };
    let answered = |mut response: Response| {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        response
    };
    if !payload.batch {
        let Some(request) = payload.requests.into_iter().next() else {
            return StatusCode::ACCEPTED.into_response();
        };
        let response = forward(request).await;
        return if payload.calls > 0 && response.status() == StatusCode::OK {
            answered(response)
        } else {
            response
        };
    }
    // `rmcp` has no batches: dispatch the messages one by one and answer with
    // the array of their responses.
    let mut responses = Vec::new();
    for request in payload.requests {
        let response = forward(request).await;
        if response.status() != StatusCode::OK {
            continue;
        }
        if let Ok(bytes) = axum::body::to_bytes(response.into_body(), usize::MAX).await
            && !bytes.is_empty()
        {
            responses.push(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    if payload.calls == 0 {
        return StatusCode::ACCEPTED.into_response();
    }
    let mut response = format!("[{}]", responses.join(",")).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    answered(response)
}

/// `/mcp` (stateless Streamable HTTP with JSON responses),
/// `GET /health`, and optional bearer authentication over both.
pub fn http_router(server: Server, token: &str) -> Router {
    http_service(server, token, Context::background()).0
}
// The router and a closure that ends the `rmcp` HTTP service.
fn http_service(server: Server, token: &str, lifetime: Context) -> (Router, impl FnOnce()) {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(usize::MAX)
        // `front` applies the Host and Origin rules.
        .disable_allowed_hosts()
        .disable_allowed_origins();
    config.sse_keep_alive = None;
    let cancellation = config.cancellation_token.clone();
    let changes = change_channel(&server);
    let service = StreamableHttpService::new(
        // Every stateless request is its own session.
        move || {
            Ok(Handler {
                server: server.clone(),
                lifetime: lifetime.clone(),
                initialized: Arc::default(),
                changes: changes.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let state = Arc::new(HttpState {
        token: token.to_string(),
    });
    let router = Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(state, front));
    (router, move || cancellation.cancel())
}

async fn serve_http(
    ctx: &Context,
    server: Server,
    token: String,
    listener: TcpListener,
) -> Result<()> {
    // HTTP request contexts do not derive from `ctx`.
    let (router, cancellation) = http_service(server, &token, Context::background());
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let stop = Arc::new(tokio::sync::Notify::new());
    let serving = {
        let stop = stop.clone();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { stop.notified().await })
                .await
        })
    };
    let (_guard, cancelled) = watch(ctx);
    let _ = cancelled.await;
    stop.notify_one();
    // Graceful shutdown under a five second deadline.
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
    cancellation();
    Ok(())
}

static SIGNAL_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
extern "C" fn on_signal(_: libc::c_int) {
    let descriptor = SIGNAL_PIPE.load(std::sync::atomic::Ordering::Relaxed);
    if descriptor >= 0 {
        // SAFETY: write(2) is async-signal-safe; the pipe stays open for the process lifetime.
        unsafe { libc::write(descriptor, [1u8].as_ptr().cast(), 1) };
    }
}
/// A context cancelled by the first SIGINT or SIGTERM. Later signals stay caught.
pub fn signal_context() -> Result<Context> {
    let mut descriptors = [0 as libc::c_int; 2];
    // SAFETY: `descriptors` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    SIGNAL_PIPE.store(descriptors[1], std::sync::atomic::Ordering::Relaxed);
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only performs an async-signal-safe write.
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    let ctx = Context::background().with_cancel();
    let cancelled = ctx.clone();
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        loop {
            // SAFETY: reads one byte into a live buffer from the pipe created above.
            let count = unsafe { libc::read(descriptors[0], byte.as_mut_ptr().cast(), 1) };
            if count == 1 {
                cancelled.cancel();
            } else if count == 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return;
            }
        }
    });
    Ok(ctx)
}
