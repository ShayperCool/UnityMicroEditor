//! Remote session: the Player's picture as hardware-encoded H.264 in a browser,
//! and the browser's keyboard and mouse back in the Player.
//!
//! A session owns two listeners. The Player connects to the loopback one and
//! sends raw frames it read back from its viewport; they are encoded here and
//! fanned out over WebSockets to the viewers of the HTTP listener, whose input
//! travels the other way. Each session binds its own ports, so several
//! editors serve their Players side by side. Making the HTTP address reachable
//! from elsewhere is left to whoever starts the session.
mod encoder;
mod http;
mod input;
#[cfg(test)]
mod tests;

pub use encoder::{Encoded, Encoder, Factory, Parameters, codec_string, hardware};
pub use input::{key_name, translate};

use crate::{agentcontrol::Controller, cancel::Context, sysutil};
use axum::body::Bytes;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, watch};

/// The largest frame side the encoder accepts.
const MAXIMUM_SIDE: usize = 4096;
/// Viewers may fall this many frames behind before they resume at a keyframe.
const VIEWER_BACKLOG: usize = 120;

/// The Player a session shows, as far as the session needs it.
pub trait Player: Send + Sync {
    /// The session id and process id of the Player once it is ready.
    fn ready(&self) -> Option<(String, i64)>;
    /// Asks the ready Player to connect its stream to this session.
    fn start_stream(&self, arguments: &Value) -> Result<(), String>;
}
impl Player for Controller {
    fn ready(&self) -> Option<(String, i64)> {
        let state = self.status();
        (state.phase == "ready" && state.pid > 0).then_some((state.session_id, state.pid))
    }
    fn start_stream(&self, arguments: &Value) -> Result<(), String> {
        let ctx = Context::background().with_timeout(Duration::from_secs(5));
        let mut request = BTreeMap::new();
        request.insert("arguments".to_string(), arguments.to_string().into_bytes());
        let reply = self.request(&ctx, "stream", &request);
        ctx.cancel();
        let reply = reply.map_err(|error| error.message().to_string())?;
        if reply.get("success") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let rejection = reply
            .pointer("/error/message")
            .or_else(|| reply.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("the Player did not confirm the stream");
        Err(rejection.to_string())
    }
}

/// Zero or empty members select the defaults: `127.0.0.1:0` (a free loopback
/// port), 60 frames per second, at most 1920 pixels wide, 12000 kbit/s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub listen: String,
    pub fps: u32,
    pub max_width: u32,
    pub bitrate_kbps: u32,
}
impl Options {
    /// The options with every default filled in, or what is out of range.
    pub fn normalized(mut self) -> Result<Options, String> {
        if self.listen.is_empty() {
            self.listen = "127.0.0.1:0".into();
        }
        let defaults = [
            (&mut self.fps, 60),
            (&mut self.max_width, 1920),
            (&mut self.bitrate_kbps, 12000),
        ];
        for (value, default) in defaults {
            if *value == 0 {
                *value = default;
            }
        }
        if !(1..=60).contains(&self.fps) {
            return Err("fps must be between 1 and 60".into());
        }
        if !(64..=MAXIMUM_SIDE as u32).contains(&self.max_width) {
            return Err("maxWidth must be between 64 and 4096".into());
        }
        if !(250..=100_000).contains(&self.bitrate_kbps) {
            return Err("bitrateKbps must be between 250 and 100000".into());
        }
        Ok(self)
    }
}

/// One encoded frame in the form viewers receive it.
pub(crate) struct Packet {
    pub keyframe: bool,
    pub wire: Bytes,
}
// flags, encoder epoch, microseconds since the session started, picture size
// and the codec string a decoder is configured with, then the access unit.
fn wire(
    keyframe: bool,
    epoch: u32,
    timestamp: u64,
    width: u16,
    height: u16,
    codec: &str,
    data: &[u8],
) -> Bytes {
    let mut out = Vec::with_capacity(18 + codec.len() + data.len());
    out.push(keyframe as u8);
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push(codec.len() as u8);
    out.extend_from_slice(codec.as_bytes());
    out.extend_from_slice(data);
    out.into()
}

#[derive(Default)]
struct Status {
    player_connected: bool,
    player_pid: i64,
    cursor_locked: bool,
    viewport: (u64, u64),
    encoded: (u16, u16),
    encoder: &'static str,
    codec: String,
    frames: u64,
    bytes: u64,
    error: String,
    attach_error: String,
}

pub(crate) struct Shared {
    options: Options,
    pub(crate) token: String,
    link_token: String,
    address: SocketAddr,
    link_port: u16,
    started: Instant,
    encoders: Arc<Factory>,
    pub(crate) video: broadcast::Sender<Arc<Packet>>,
    pub(crate) changed: watch::Sender<u64>,
    status: Mutex<Status>,
    // The current Player connection and its generation.
    link: Mutex<Option<(u64, TcpStream)>>,
    generations: AtomicU64,
    epochs: AtomicU32,
    viewers: AtomicUsize,
    keyframe: AtomicBool,
    closed: AtomicBool,
}
impl Shared {
    fn notify(&self) {
        self.changed.send_modify(|version| *version += 1);
    }
    fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
    pub(crate) fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }
    // One JSON line to the Player; a link that cannot take it is closed.
    fn send(&self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        let mut link = self.link.lock().unwrap();
        if let Some((_, stream)) = link.as_mut()
            && stream.write_all(line.as_bytes()).is_err()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
    fn send_viewers(&self) {
        self.send(&json!({"viewers": self.viewers.load(Ordering::Relaxed)}));
    }
    pub(crate) fn viewer_joined(&self) {
        self.viewers.fetch_add(1, Ordering::Relaxed);
        self.request_keyframe();
        self.send_viewers();
        self.notify();
    }
    pub(crate) fn viewer_left(&self) {
        self.viewers.fetch_sub(1, Ordering::Relaxed);
        self.send_viewers();
        self.notify();
    }
    /// A text message of a viewer: input events, a keyframe request after a
    /// decoder error, or the release of everything it holds down.
    pub(crate) fn viewer_message(&self, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if message.get("k").is_some() {
            self.request_keyframe();
        }
        if message.get("r").is_some() {
            self.send(&json!({"release": true}));
        }
        if let Some(events) = message.get("e").and_then(Value::as_array) {
            let events = translate(events);
            if !events.is_empty() {
                self.send(&json!({"input": events}));
            }
        }
    }
    /// What viewers are told whenever it changes.
    pub(crate) fn state_json(&self) -> String {
        let status = self.status.lock().unwrap();
        json!({
            "playerConnected": status.player_connected,
            "cursorLocked": status.cursor_locked,
            "viewport": {"width": status.viewport.0, "height": status.viewport.1},
            "viewers": self.viewers.load(Ordering::Relaxed),
            "encoder": status.encoder,
            "error": if status.error.is_empty() { &status.attach_error } else { &status.error },
        })
        .to_string()
    }
    fn url(&self) -> String {
        let mut address = self.address;
        if address.ip().is_unspecified() {
            address.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        format!("http://{address}/{}/", self.token)
    }
    fn report(&self) -> Value {
        let status = self.status.lock().unwrap();
        json!({
            "running": true,
            "url": self.url(),
            "listen": self.address.to_string(),
            "playerConnected": status.player_connected,
            "playerPid": status.player_pid,
            "viewers": self.viewers.load(Ordering::Relaxed),
            "video": {
                "encoder": status.encoder,
                "codec": status.codec,
                "width": status.encoded.0,
                "height": status.encoded.1,
                "frames": status.frames,
                "bytes": status.bytes,
            },
            "viewport": {"width": status.viewport.0, "height": status.viewport.1},
            "fps": self.options.fps,
            "maxWidth": self.options.max_width,
            "bitrateKbps": self.options.bitrate_kbps,
            "lastError": if status.error.is_empty() { &status.attach_error } else { &status.error },
        })
    }
}

/// A running remote session. Dropping it closes both listeners and every connection.
pub struct Session {
    shared: Arc<Shared>,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Session {
    /// Binds the listeners and starts serving. The Player is asked to connect
    /// as soon as it is ready, and again whenever it is replaced.
    pub fn start(
        player: Arc<dyn Player>,
        options: Options,
        encoders: Arc<Factory>,
    ) -> Result<Session, String> {
        let options = options.normalized()?;
        let address: SocketAddr = options.listen.parse().map_err(|_| {
            "listen must be a literal IP address and port, such as 127.0.0.1:0".to_string()
        })?;
        let bind = |address: SocketAddr| {
            TcpListener::bind(address).map_err(|error| {
                format!(
                    "listen tcp {address}: bind: {}",
                    sysutil::errno_text(&error)
                )
            })
        };
        let listener = bind(address)?;
        let link = bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
        let local =
            |listener: &TcpListener| listener.local_addr().map_err(|error| error.to_string());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("remote-session")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let shared = Arc::new(Shared {
            options,
            token: sysutil::new_id(),
            link_token: sysutil::new_id(),
            address: local(&listener)?,
            link_port: local(&link)?.port(),
            started: Instant::now(),
            encoders,
            video: broadcast::channel(VIEWER_BACKLOG).0,
            changed: watch::channel(0).0,
            status: Mutex::default(),
            link: Mutex::default(),
            generations: AtomicU64::new(0),
            epochs: AtomicU32::new(0),
            viewers: AtomicUsize::new(0),
            keyframe: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        {
            let _context = runtime.enter();
            let listener =
                tokio::net::TcpListener::from_std(listener).map_err(|error| error.to_string())?;
            let router = http::router(shared.clone());
            runtime.spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
        }
        {
            let shared = shared.clone();
            thread::spawn(move || accept(&shared, link));
        }
        {
            let shared = shared.clone();
            thread::spawn(move || attach(&shared, player.as_ref()));
        }
        Ok(Session {
            shared,
            runtime: Some(runtime),
        })
    }
    pub fn options(&self) -> &Options {
        &self.shared.options
    }
    /// The address to open in a browser. It carries the session's secret.
    pub fn url(&self) -> String {
        self.shared.url()
    }
    /// The reply of `remote_session`.
    pub fn status(&self) -> Value {
        self.shared.report()
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        if let Some((_, stream)) = self.shared.link.lock().unwrap().take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        // Wakes the blocked accept so that its thread sees the session end.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.shared.link_port));
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

// Keeps the Player of the moment connected: a Player that became ready, or
// replaced the previous one, is asked once to open its stream. A request that
// did not lead to a connection is repeated a few times, further apart.
fn attach(shared: &Shared, player: &dyn Player) {
    let mut target = None;
    let mut attempts = 0u32;
    let mut next = Instant::now();
    while !shared.closed() {
        thread::sleep(Duration::from_millis(250));
        if shared.status.lock().unwrap().player_connected {
            attempts = 0;
            next = Instant::now() + Duration::from_secs(1);
            continue;
        }
        let ready = player.ready();
        if ready != target {
            target = ready;
            attempts = 0;
            next = Instant::now();
        }
        if target.is_none() || attempts >= 6 || Instant::now() < next || shared.closed() {
            continue;
        }
        attempts += 1;
        next = Instant::now() + Duration::from_secs(2 * attempts as u64);
        let outcome = player.start_stream(&json!({
            "action": "start",
            "port": shared.link_port,
            "token": shared.link_token,
            "fps": shared.options.fps,
            "maxWidth": shared.options.max_width,
        }));
        let message = outcome.err().unwrap_or_default();
        let mut status = shared.status.lock().unwrap();
        if status.attach_error != message {
            status.attach_error = message;
            drop(status);
            shared.notify();
        }
    }
}

fn accept(shared: &Arc<Shared>, listener: TcpListener) {
    for stream in listener.incoming() {
        if shared.closed() {
            return;
        }
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        thread::spawn(move || serve(&shared, stream));
    }
}

// `UME-STREAM 1 <token> <pid>`: only the Player this session asked knows the token.
fn handshake(shared: &Shared, stream: &mut TcpStream) -> Option<i64> {
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut line = Vec::new();
    let mut byte = [0u8];
    loop {
        stream.read_exact(&mut byte).ok()?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > 256 {
            return None;
        }
    }
    let line = String::from_utf8(line).ok()?;
    let mut parts = line.split(' ');
    let known = (parts.next()?, parts.next()?) == ("UME-STREAM", "1");
    let token: bool = parts
        .next()?
        .as_bytes()
        .ct_eq(shared.link_token.as_bytes())
        .into();
    let pid = parts.next()?.parse::<i64>().ok()?;
    stream.set_read_timeout(None).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    stream.set_nodelay(true).ok()?;
    (known && token && parts.next().is_none()).then_some(pid)
}

fn serve(shared: &Shared, mut stream: TcpStream) {
    let Some(pid) = handshake(shared, &mut stream) else {
        return;
    };
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let generation = shared.generations.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some((_, previous)) = shared.link.lock().unwrap().replace((generation, writer)) {
        let _ = previous.shutdown(Shutdown::Both);
    }
    {
        let mut status = shared.status.lock().unwrap();
        status.player_connected = true;
        status.player_pid = pid;
        status.error.clear();
        status.attach_error.clear();
    }
    shared.send_viewers();
    shared.notify();
    let outcome = pump(shared, &mut stream);
    let _ = stream.shutdown(Shutdown::Both);
    let mut link = shared.link.lock().unwrap();
    // A newer connection has already taken this one's place.
    if link
        .as_ref()
        .is_some_and(|(current, _)| *current == generation)
    {
        *link = None;
        let mut status = shared.status.lock().unwrap();
        status.player_connected = false;
        status.cursor_locked = false;
        if let Err(error) = outcome
            && !shared.closed()
        {
            status.error = error;
        }
        drop(status);
        drop(link);
        shared.notify();
    }
}

struct Active {
    width: u16,
    height: u16,
    epoch: u32,
    codec: String,
    encoder: Box<dyn Encoder>,
}

// Reads the Player's messages until the connection ends: 'S' is its state as
// JSON, 'F' a frame (viewport and picture size as four u16, then RGBA rows).
// A Player that goes away ends the stream without an error; a message that
// breaks the protocol is one.
fn pump(shared: &Shared, stream: &mut TcpStream) -> Result<(), String> {
    let mut header = [0u8; 5];
    let mut frame = Vec::new();
    let mut active: Option<Active> = None;
    let mut retry = Instant::now();
    loop {
        if stream.read_exact(&mut header).is_err() {
            return Ok(());
        }
        let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        match header[0] {
            b'S' if length <= 4096 => {
                let mut text = vec![0u8; length];
                if stream.read_exact(&mut text).is_err() {
                    return Ok(());
                }
                let Ok(state) = serde_json::from_slice::<Value>(&text) else {
                    continue;
                };
                let mut status = shared.status.lock().unwrap();
                status.cursor_locked = state["cursorLocked"] == true;
                status.viewport = (
                    state["width"].as_u64().unwrap_or(0),
                    state["height"].as_u64().unwrap_or(0),
                );
                drop(status);
                shared.notify();
            }
            b'F' if (8..=8 + MAXIMUM_SIDE * MAXIMUM_SIDE * 4).contains(&length) => {
                let mut sizes = [0u8; 8];
                if stream.read_exact(&mut sizes).is_err() {
                    return Ok(());
                }
                let side = |index: usize| u16::from_le_bytes([sizes[index], sizes[index + 1]]);
                let (width, height) = (side(4), side(6));
                frame.resize(length - 8, 0);
                if stream.read_exact(&mut frame).is_err() {
                    return Ok(());
                }
                let even = |side: u16| {
                    side >= 2 && side.is_multiple_of(2) && side as usize <= MAXIMUM_SIDE
                };
                if !even(width)
                    || !even(height)
                    || frame.len() != width as usize * height as usize * 4
                {
                    return Err("the Player sent a malformed frame".into());
                }
                if shared.viewers.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                if active
                    .as_ref()
                    .is_none_or(|active| (active.width, active.height) != (width, height))
                {
                    active = None;
                    if Instant::now() < retry {
                        continue;
                    }
                    let parameters = Parameters {
                        width: width.into(),
                        height: height.into(),
                        fps: shared.options.fps,
                        bitrate: shared.options.bitrate_kbps * 1000,
                    };
                    match (shared.encoders)(&parameters) {
                        Ok(encoder) => {
                            let mut status = shared.status.lock().unwrap();
                            status.encoder = encoder.name();
                            status.encoded = (width, height);
                            status.error.clear();
                            active = Some(Active {
                                width,
                                height,
                                epoch: shared.epochs.fetch_add(1, Ordering::Relaxed) + 1,
                                codec: String::new(),
                                encoder,
                            });
                        }
                        Err(error) => {
                            retry = Instant::now() + Duration::from_secs(5);
                            shared.status.lock().unwrap().error =
                                format!("video encoder unavailable: {error}");
                            shared.notify();
                            continue;
                        }
                    }
                }
                let current = active.as_mut().expect("encoder was just ensured");
                // A stream's first frame carries the parameter sets the codec string is read from.
                let force =
                    shared.keyframe.swap(false, Ordering::Relaxed) || current.codec.is_empty();
                let timestamp = shared.started.elapsed().as_micros() as u64;
                match current.encoder.encode(&frame, force, timestamp) {
                    Ok(encoded) => {
                        if encoded.keyframe
                            && let Some(codec) = codec_string(&encoded.data)
                        {
                            current.codec = codec;
                        }
                        if current.codec.is_empty() {
                            continue;
                        }
                        let packet = Packet {
                            keyframe: encoded.keyframe,
                            wire: wire(
                                encoded.keyframe,
                                current.epoch,
                                timestamp,
                                width,
                                height,
                                &current.codec,
                                &encoded.data,
                            ),
                        };
                        let mut status = shared.status.lock().unwrap();
                        status.frames += 1;
                        status.bytes += encoded.data.len() as u64;
                        if status.codec != current.codec {
                            status.codec.clone_from(&current.codec);
                        }
                        drop(status);
                        // No receiver only means the last viewer has just left.
                        let _ = shared.video.send(Arc::new(packet));
                    }
                    Err(error) => {
                        active = None;
                        retry = Instant::now() + Duration::from_secs(2);
                        shared.status.lock().unwrap().error =
                            format!("video encoding failed: {error}");
                        shared.notify();
                    }
                }
            }
            _ => return Err("the Player sent an unknown stream message".into()),
        }
    }
}
