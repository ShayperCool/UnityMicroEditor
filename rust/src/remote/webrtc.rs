//! The WebRTC transport: every viewer is a peer connection that receives the
//! session's H.264 stream as an RTP video track, decoded by the browser's own
//! media pipeline. Unlike WebCodecs this works on a plain-HTTP page. All peers
//! share one UDP socket; offers and answers travel over the viewer's WebSocket.
use super::Shared;
use serde_json::Value;
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc,
    change::SdpOffer,
    format::Codec,
    media::{MediaKind, MediaTime, Mid, Pt},
    net::{Protocol, Receive},
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast::error::RecvError, mpsc, oneshot},
};

pub(crate) enum Command {
    /// A viewer's session description; the reply is the answer to give back.
    Offer {
        viewer: u64,
        offer: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Leave(u64),
}

struct Peer {
    viewer: u64,
    rtc: Rtc,
    track: Option<(Mid, Pt)>,
    // A peer starts, and restarts after a loss it reports, at a keyframe.
    waiting: bool,
}

fn accept(viewer: u64, offer: Value, address: SocketAddr) -> Result<(Peer, Value), String> {
    let offer: SdpOffer =
        serde_json::from_value(offer).map_err(|error| format!("offer: {error}"))?;
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true)
        .build(Instant::now());
    let candidate = Candidate::host(address, "udp").map_err(|error| error.to_string())?;
    rtc.add_local_candidate(candidate);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|error| format!("offer: {error}"))?;
    let answer = serde_json::to_value(answer).map_err(|error| error.to_string())?;
    let peer = Peer {
        viewer,
        rtc,
        track: None,
        waiting: true,
    };
    Ok((peer, answer))
}

// The negotiated H.264 payload the encoder's stream fits best: High profile
// where the browser offers it, single-NAL-unit-free packetization otherwise.
fn payload(rtc: &mut Rtc, mid: Mid) -> Option<Pt> {
    let writer = rtc.writer(mid)?;
    writer
        .payload_params()
        .filter(|params| params.spec().codec == Codec::H264)
        .max_by_key(|params| {
            let format = &params.spec().format;
            let high = format
                .profile_level_id
                .is_some_and(|profile| profile >> 16 == 0x64);
            (format.packetization_mode == Some(1), high)
        })
        .map(|params| params.pt())
}

// Sends what the peer has ready and handles its events; returns when it next wants to be woken.
fn drive(shared: &Shared, peer: &mut Peer, socket: &UdpSocket) -> Option<Instant> {
    loop {
        if !peer.rtc.is_alive() {
            return None;
        }
        match peer.rtc.poll_output() {
            Ok(Output::Transmit(transmit)) => {
                // A full socket buffer drops the datagram, as the network may.
                let _ = socket.try_send_to(&transmit.contents, transmit.destination);
            }
            Ok(Output::Timeout(at)) => return Some(at),
            Ok(Output::Event(event)) => match event {
                Event::MediaAdded(media) if media.kind == MediaKind::Video => {
                    peer.track = payload(&mut peer.rtc, media.mid).map(|pt| (media.mid, pt));
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    peer.rtc.disconnect()
                }
                Event::Connected | Event::KeyframeRequest(_) => {
                    peer.waiting = true;
                    shared.request_keyframe();
                }
                _ => {}
            },
            Err(_) => {
                peer.rtc.disconnect();
                return None;
            }
        }
    }
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    socket: UdpSocket,
    address: SocketAddr,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut video = shared.video.subscribe();
    let mut peers: Vec<Peer> = Vec::new();
    let mut buffer = vec![0u8; 2000];
    loop {
        let mut wake = Instant::now() + Duration::from_millis(100);
        for peer in &mut peers {
            if let Some(at) = drive(&shared, peer, &socket) {
                wake = wake.min(at);
            }
        }
        peers.retain(|peer| peer.rtc.is_alive());
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Offer { viewer, offer, reply }) => {
                    peers.retain(|peer| peer.viewer != viewer);
                    let _ = reply.send(accept(viewer, offer, address).map(|(peer, answer)| {
                        peers.push(peer);
                        answer
                    }));
                }
                Some(Command::Leave(viewer)) => peers.retain(|peer| peer.viewer != viewer),
                None => return,
            },
            packet = video.recv() => match packet {
                Ok(packet) => {
                    let now = Instant::now();
                    for peer in &mut peers {
                        let Some((mid, pt)) = peer.track else { continue };
                        if !peer.rtc.is_connected() || (peer.waiting && !packet.keyframe) {
                            continue;
                        }
                        peer.waiting = false;
                        let Some(writer) = peer.rtc.writer(mid) else { continue };
                        let time = MediaTime::from_micros(packet.timestamp);
                        if writer.write(pt, now, time, packet.data().to_vec()).is_err() {
                            peer.rtc.disconnect();
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    peers.iter_mut().for_each(|peer| peer.waiting = true);
                    shared.request_keyframe();
                }
                Err(RecvError::Closed) => return,
            },
            received = socket.recv_from(&mut buffer) => {
                let Ok((length, source)) = received else { continue };
                let Ok(contents) = buffer[..length].try_into() else { continue };
                let input = Input::Receive(
                    Instant::now(),
                    Receive { proto: Protocol::Udp, source, destination: address, contents },
                );
                if let Some(peer) = peers.iter_mut().find(|peer| peer.rtc.accepts(&input))
                    && peer.rtc.handle_input(input).is_err()
                {
                    peer.rtc.disconnect();
                }
            }
            _ = tokio::time::sleep_until(wake.into()) => {
                let now = Instant::now();
                for peer in &mut peers {
                    if peer.rtc.handle_input(Input::Timeout(now)).is_err() {
                        peer.rtc.disconnect();
                    }
                }
            }
        }
    }
}
