//! Project-bound authoring and optional Player control over MCP stdio or Streamable HTTP.
//!
//! `server` holds the tools, resources and reply rules and takes raw JSON
//! arguments; `transport` binds it to the `rmcp` SDK. Wire details owned by
//! `rmcp` (member order of JSON objects, SDK-generated protocol errors, typed
//! `arguments` decoding) differ from the recorded wire format; `transport` documents each.
mod backend;
pub mod jsonschema;
mod reply;
mod schema;
mod server;
#[cfg(test)]
mod tests;
mod transport;

pub use backend::{HandlerError, HandlerValue, RuntimeHandler};
pub use reply::{MAX_REPLY_BYTES, ResourceResult, ToolResult};
pub use server::{
    CAPABILITIES_URI, CallError, INSTRUCTIONS, PROJECT_URI, ProjectToolsInput, ReadInput,
    ResourceDefinition, RuntimeInput, SERVER_NAME, SERVER_VERSION, Server, SessionInput,
    SourcePage, ToolDefinition, read_page,
};
pub use transport::{
    http_router, parse_options, run, run_options, run_with_runtime, signal_context,
};

use serde_json::Value;
use std::sync::Arc;

/// An empty `transport` means stdio and an empty `listen` means `127.0.0.1:8090`.
/// `runtime` and `project_actions` add their tools only when set; `backends` is
/// reported by `project_capabilities`.
#[derive(Clone, Default)]
pub struct Options {
    pub project: String,
    pub transport: String,
    pub listen: String,
    pub token_file: String,
    pub runtime: Option<Arc<dyn RuntimeHandler>>,
    pub project_actions: Option<Arc<dyn RuntimeHandler>>,
    pub backends: Value,
}
