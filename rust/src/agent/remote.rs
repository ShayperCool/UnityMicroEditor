//! The `remote_session` tool: one browser-facing session per service.
use super::{Service, fail};
use crate::{mcp::HandlerValue, remote};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Input {
    action: String,
    listen: String,
    transport: String,
    webrtc_address: String,
    webrtc_port: u16,
    fps: u32,
    max_width: u32,
    bitrate_kbps: u32,
    publish: String,
    publish_pattern: String,
}

impl Service {
    pub(crate) fn remote_session(&self, raw: &[u8]) -> Result<HandlerValue> {
        let input: Input = serde_json::from_slice(raw)?;
        let mut current = self.0.remote.lock().unwrap();
        let reply = match input.action.as_str() {
            "start" => {
                let mut requested = remote::Options {
                    listen: input.listen,
                    transport: input.transport,
                    webrtc_address: input.webrtc_address,
                    webrtc_port: input.webrtc_port,
                    fps: input.fps,
                    max_width: input.max_width,
                    bitrate_kbps: input.bitrate_kbps,
                    publish: input.publish,
                    publish_pattern: input.publish_pattern,
                };
                let unspecified = requested == remote::Options::default();
                // The server's own publish command applies unless the call names one or says "off".
                match requested.publish.as_str() {
                    "" => requested.publish.clone_from(&self.0.remote_publish),
                    "off" => requested.publish.clear(),
                    _ => {}
                }
                let options = requested
                    .normalized()
                    .map_err(|error| fail("invalid_request", error))?;
                match current.as_ref() {
                    // Repeating a start returns the session that already runs.
                    Some(session) if unspecified || *session.options() == options => {
                        session.status()
                    }
                    Some(_) => {
                        return Err(fail(
                            "remote_session_running",
                            "a remote session already runs with other settings; stop it first",
                        ));
                    }
                    None => {
                        let Some(controller) = self.0.controller.clone() else {
                            return Err(fail(
                                "runtime_unavailable",
                                "runtime controller is not configured",
                            ));
                        };
                        let session = remote::Session::start(
                            Arc::new(controller),
                            options,
                            remote::Encoders::hardware(),
                        )
                        .map_err(|error| fail("remote_session_failed", error))?;
                        let status = session.status();
                        *current = Some(session);
                        status
                    }
                }
            }
            "stop" => json!({"running": false, "stopped": current.take().is_some()}),
            "status" => current
                .as_ref()
                .map_or_else(|| json!({"running": false}), remote::Session::status),
            _ => {
                return Err(fail(
                    "unsupported_operation",
                    "unknown remote session action",
                ));
            }
        };
        Ok(HandlerValue::Map(serde_json::to_vec(&reply)?))
    }
    pub(crate) fn close_remote_session(&self) {
        self.0.remote.lock().unwrap().take();
    }
}
