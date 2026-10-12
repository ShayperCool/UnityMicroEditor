//! What a browser reaches: the viewer page, its WebSocket and a state probe,
//! all beneath the session's secret path.
use super::Shared;
use axum::{
    Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast::error::RecvError;

const PAGE: &str = include_str!("page.html");
/// Bound on one viewer message; input batches are far smaller.
const MAXIMUM_MESSAGE: usize = 64 * 1024;

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/{token}", get(page))
        .route("/{token}/", get(page))
        .route("/{token}/ws", get(socket))
        .route("/{token}/state", get(state))
        .with_state(shared)
}

// Every route answers as if it did not exist without the session's token.
fn authorized(shared: &Shared, token: &str) -> bool {
    token.as_bytes().ct_eq(shared.token.as_bytes()).into()
}
fn missing() -> Response {
    StatusCode::NOT_FOUND.into_response()
}
fn private(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // The address carries the token: it must not travel in a Referer header.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn page(State(shared): State<Arc<Shared>>, Path(token): Path<String>) -> Response {
    if !authorized(&shared, &token) {
        return missing();
    }
    let mut response = private(axum::response::Html(PAGE).into_response());
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src *",
        ),
    );
    response
}

async fn state(State(shared): State<Arc<Shared>>, Path(token): Path<String>) -> Response {
    if !authorized(&shared, &token) {
        return missing();
    }
    private(
        (
            [(header::CONTENT_TYPE, "application/json")],
            shared.state_json(),
        )
            .into_response(),
    )
}

async fn socket(
    State(shared): State<Arc<Shared>>,
    Path(token): Path<String>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !authorized(&shared, &token) {
        return missing();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .max_message_size(MAXIMUM_MESSAGE)
        .max_frame_size(MAXIMUM_MESSAGE)
        .on_upgrade(move |socket| viewer(shared, socket))
}

struct Viewing(Arc<Shared>, u64);
impl Drop for Viewing {
    fn drop(&mut self) {
        self.0.viewer_left(self.1);
    }
}

async fn viewer(shared: Arc<Shared>, mut socket: WebSocket) {
    let mut video = shared.video.subscribe();
    let mut changed = shared.changed.subscribe();
    let viewing = Viewing(shared.clone(), shared.viewer_joined());
    // With WebRTC the video is a media track; this socket carries state, signalling and input.
    let frames = !shared.uses_webrtc();
    // A viewer starts, and restarts after falling behind, at a keyframe.
    let mut waiting = true;
    let mut keepalive = tokio::time::interval(Duration::from_secs(5));
    loop {
        let outgoing = tokio::select! {
            packet = video.recv(), if frames => match packet {
                Ok(packet) if waiting && !packet.keyframe => continue,
                Ok(packet) => {
                    waiting = false;
                    Message::Binary(packet.wire.clone())
                }
                Err(RecvError::Lagged(_)) => {
                    waiting = true;
                    shared.request_keyframe();
                    continue;
                }
                Err(RecvError::Closed) => break,
            },
            update = changed.changed() => match update {
                Ok(()) => Message::Text(shared.state_json().into()),
                Err(_) => break,
            },
            _ = keepalive.tick() => Message::Text(shared.state_json().into()),
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let message = serde_json::from_str::<serde_json::Value>(text.as_str()).ok();
                    match message.and_then(|mut message| message.get_mut("offer").map(serde_json::Value::take)) {
                        Some(offer) => Message::Text(shared.viewer_offer(viewing.1, offer).await.into()),
                        None => {
                            shared.viewer_message(text.as_str());
                            continue;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        };
        if socket.send(outgoing).await.is_err() {
            break;
        }
    }
}
