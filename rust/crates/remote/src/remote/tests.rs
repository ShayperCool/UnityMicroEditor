use super::*;
use std::{
    io::{BufRead, BufReader},
    sync::mpsc,
};

// An access unit as the fake encoder writes it: a sequence parameter set
// (High profile, level 4.0) in front of keyframes, then a slice.
fn access_unit(keyframe: bool, marker: u8) -> Vec<u8> {
    let mut data = Vec::new();
    if keyframe {
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac]);
    }
    data.extend_from_slice(&[0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }, marker]);
    data
}
struct FakeEncoder(u8);
impl Encoder for FakeEncoder {
    fn encode(&mut self, pixels: &[u8], force_keyframe: bool, _: u64) -> Result<Encoded, String> {
        self.0 += 1;
        let keyframe = force_keyframe || self.0 == 1;
        Ok(Encoded {
            data: access_unit(keyframe, pixels[0]),
            keyframe,
        })
    }
    fn name(&self) -> &'static str {
        "fake"
    }
}
// Writes the first sample of each packet, scaled to a byte.
struct FakeAudio;
impl AudioEncoder for FakeAudio {
    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String> {
        Ok(vec![(pcm[0] * 100.0) as u8])
    }
    fn name(&self) -> &'static str {
        "fake-opus"
    }
}
fn fake_encoders() -> Encoders {
    Encoders {
        video: Arc::new(|_: &Parameters| Ok(Box::new(FakeEncoder(0)) as Box<dyn Encoder>)),
        audio: Arc::new(|| Ok(Box::new(FakeAudio) as Box<dyn AudioEncoder>)),
    }
}

// Plays the Player's side of the link: connects when asked, reports its state
// and answers every line it receives with one 4x2 frame and 20 ms of sound.
struct FakePlayer {
    lines: mpsc::Sender<Value>,
    requests: Mutex<Vec<Value>>,
}
impl Player for FakePlayer {
    fn ready(&self) -> Option<(String, i64)> {
        Some(("session".into(), 4242))
    }
    fn start_stream(&self, arguments: &Value) -> Result<(), String> {
        self.requests.lock().unwrap().push(arguments.clone());
        let port = arguments["port"].as_u64().unwrap() as u16;
        let token = arguments["token"].as_str().unwrap().to_string();
        let lines = self.lines.clone();
        thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream
                .write_all(format!("UME-STREAM 1 {token} 4242\n").as_bytes())
                .unwrap();
            let state = br#"{"cursorLocked":true,"width":8,"height":4}"#;
            stream.write_all(b"S").unwrap();
            stream
                .write_all(&(state.len() as u32).to_le_bytes())
                .unwrap();
            stream.write_all(state).unwrap();
            let mut marker = 0u8;
            for line in BufReader::new(stream.try_clone().unwrap()).lines() {
                let Ok(line) = line else { return };
                let _ = lines.send(serde_json::from_str(&line).unwrap());
                marker += 1;
                let mut frame = vec![b'F'];
                frame.extend_from_slice(&(8u32 + 4 * 2 * 4).to_le_bytes());
                for side in [8u16, 4, 4, 2] {
                    frame.extend_from_slice(&side.to_le_bytes());
                }
                frame.extend_from_slice(&[marker; 4 * 2 * 4]);
                frame.push(b'A');
                frame.extend_from_slice(&(6u32 + 960 * 2 * 4).to_le_bytes());
                frame.extend_from_slice(&48_000u32.to_le_bytes());
                frame.extend_from_slice(&2u16.to_le_bytes());
                for _ in 0..960 * 2 {
                    frame.extend_from_slice(&0.25f32.to_le_bytes());
                }
                if stream.write_all(&frame).is_err() {
                    return;
                }
            }
        });
        Ok(())
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}
fn get(address: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

#[test]
fn browser_key_codes_name_input_system_keys() {
    for (code, key) in [
        ("KeyW", "W"),
        ("Digit7", "Digit7"),
        ("Numpad3", "Numpad3"),
        ("F12", "F12"),
        ("ArrowLeft", "LeftArrow"),
        ("ControlRight", "RightCtrl"),
        ("Equal", "Equals"),
        ("NumpadAdd", "NumpadPlus"),
        ("Space", "Space"),
    ] {
        assert_eq!(key_name(code).as_deref(), Some(key), "{code}");
    }
    for code in [
        "", "Key", "Keyw", "KeyWW", "F0", "F13", "F01", "Digit", "Power", "None",
    ] {
        assert_eq!(key_name(code), None, "{code}");
    }
}

#[test]
fn viewer_events_become_bounded_player_input() {
    let events = json!([
        {"t": "k", "c": "KeyA", "down": true},
        {"t": "k", "c": "Unknown", "down": true},
        {"t": "m", "x": 0.25, "y": 2.0},
        {"t": "d", "dx": 0.5, "dy": -9.0},
        {"t": "b", "b": 2, "down": false, "x": 0.5, "y": 0.5},
        {"t": "b", "b": 9, "down": true},
        {"t": "w", "dx": 0, "dy": 100},
        {"t": "m", "x": "nan", "y": 0},
        {"t": "x"},
        "junk",
    ]);
    assert_eq!(
        json!(translate(events.as_array().unwrap())),
        json!([
            {"action": "key", "key": "A", "pressed": true},
            {"action": "mouse", "normalized": {"x": 0.25, "y": 1.0}},
            {"action": "mouse", "delta": {"x": 0.5, "y": -4.0}},
            {"action": "mouse", "button": "Right", "pressed": false, "normalized": {"x": 0.5, "y": 0.5}},
            {"action": "mouse", "scroll": {"x": 0.0, "y": -120.0}},
        ])
    );
    let flood = vec![json!({"t": "k", "c": "KeyA", "down": true}); 1000];
    assert_eq!(translate(&flood).len(), input::MAXIMUM_EVENTS);
}

#[test]
fn codec_string_reads_the_sequence_parameter_set() {
    assert_eq!(
        codec_string(&access_unit(true, 0)).as_deref(),
        Some("avc1.640028")
    );
    assert_eq!(codec_string(&access_unit(false, 0)), None);
    assert_eq!(codec_string(&[0, 0, 1, 0x67]), None);
}

#[test]
fn options_fill_defaults_and_reject_values_out_of_range() {
    let options = Options::default().normalized().unwrap();
    assert_eq!(
        (
            options.listen.as_str(),
            options.fps,
            options.max_width,
            options.bitrate_kbps
        ),
        ("127.0.0.1:0", 60, 1920, 12000)
    );
    for options in [
        Options {
            fps: 61,
            ..Options::default()
        },
        Options {
            max_width: 32,
            ..Options::default()
        },
        Options {
            bitrate_kbps: 100,
            ..Options::default()
        },
    ] {
        assert!(options.normalized().is_err());
    }
    let player = Arc::new(FakePlayer {
        lines: mpsc::channel().0,
        requests: Mutex::default(),
    });
    let named = Options {
        listen: "localhost:0".into(),
        ..Options::default()
    };
    assert!(
        Session::start(player, named, fake_encoders()).is_err(),
        "accepted a host name"
    );
}

#[test]
fn session_links_the_player_encodes_for_viewers_and_returns_their_input() {
    let (lines, received) = mpsc::channel();
    let player = Arc::new(FakePlayer {
        lines,
        requests: Mutex::default(),
    });
    let session = Session::start(player.clone(), Options::default(), fake_encoders()).unwrap();
    let shared = session.shared.clone();
    let other = Session::start(player.clone(), Options::default(), fake_encoders()).unwrap();
    assert_ne!(session.url(), other.url(), "sessions share an address");
    drop(other);

    wait_for("the Player link", || {
        session.status()["playerConnected"] == true
    });
    let request = player.requests.lock().unwrap()[0].clone();
    assert_eq!(
        (&request["action"], &request["fps"], &request["maxWidth"]),
        (&json!("start"), &json!(60), &json!(1920))
    );
    assert_eq!(received.recv().unwrap(), json!({"viewers": 0}));
    wait_for("the Player state", || {
        session.status()["viewport"] == json!({"width": 8, "height": 4})
    });
    // Nobody watches yet: the frame that answered the line above was not encoded.
    assert_eq!(session.status()["video"]["frames"], 0);

    let mut video = shared.video.subscribe();
    let mut audio = shared.audio.subscribe();
    let viewer = shared.viewer_joined();
    assert_eq!(received.recv().unwrap(), json!({"viewers": 1}));
    // The first sample of the stream is interpolated from silence.
    let sound = audio.blocking_recv().unwrap();
    assert_eq!((sound.wire[0], sound.data()), (2, &[0u8][..]));
    let first_sound = sound.timestamp;
    let first = video.blocking_recv().unwrap();
    assert!(first.keyframe);
    // flags, epoch 1, timestamp, 4x2, "avc1.640028", then the access unit.
    assert_eq!(
        (first.wire[0], &first.wire[1..5], &first.wire[13..17]),
        (1, &[1u8, 0, 0, 0][..], &[4u8, 0, 2, 0][..])
    );
    assert_eq!(&first.wire[17..29], b"\x0bavc1.640028");
    assert_eq!(first.wire[29..], access_unit(true, 2));

    shared.viewer_message(r#"{"e":[{"t":"k","c":"KeyW","down":true},{"t":"m","x":0.5,"y":0.5}]}"#);
    assert_eq!(
        received.recv().unwrap(),
        json!({"input": [{"action": "key", "key": "W", "pressed": true}, {"action": "mouse", "normalized": {"x": 0.5, "y": 0.5}}]})
    );
    assert!(!video.blocking_recv().unwrap().keyframe);
    let sound = audio.blocking_recv().unwrap();
    assert_eq!(sound.data(), &[25u8]);
    assert!(
        sound.timestamp - first_sound <= 25_000,
        "the second packet follows the first by its 20 ms"
    );
    shared.viewer_message(r#"{"k":1,"r":1}"#);
    assert_eq!(received.recv().unwrap(), json!({"release": true}));
    assert!(
        video.blocking_recv().unwrap().keyframe,
        "a requested keyframe was not forced"
    );
    shared.viewer_message("not json");
    shared.viewer_message(r#"{"e":[{"t":"k","c":"Bogus","down":true}]}"#);
    shared.viewer_left(viewer);
    assert_eq!(received.recv().unwrap(), json!({"viewers": 0}));

    let status = session.status();
    assert_eq!(status["video"]["encoder"], "fake");
    assert_eq!(status["video"]["codec"], "avc1.640028");
    assert_eq!(status["audio"]["encoder"], "fake-opus");
    assert_eq!(
        (&status["audio"]["sampleRate"], &status["audio"]["channels"]),
        (&json!(48000), &json!(2))
    );
    assert_eq!(
        (&status["video"]["width"], &status["video"]["height"]),
        (&json!(4), &json!(2))
    );
    assert_eq!(status["playerPid"], 4242);

    let url = session.url();
    let (address, path) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    assert!(
        address.starts_with("127.0.0.1:") && path.len() == 33,
        "{url}"
    );
    let page = get(address, &format!("/{path}"));
    assert!(
        page.starts_with("HTTP/1.1 200") && page.contains("VideoDecoder"),
        "{page}"
    );
    assert!(page.contains("referrer-policy: no-referrer"), "{page}");
    let state = get(address, &format!("/{path}state"));
    assert!(
        state.contains(r#""playerConnected":true"#) && state.contains(r#""cursorLocked":true"#),
        "{state}"
    );
    for path in [
        "/",
        "/wrong/",
        "/wrong/ws",
        "/wrong/state",
        &format!("/{}x/", path.trim_end_matches('/')),
    ] {
        assert!(get(address, path).starts_with("HTTP/1.1 404"), "{path}");
    }

    let link_port = shared.link_port;
    drop(session);
    wait_for("the HTTP listener to close", || {
        TcpStream::connect(address).is_err()
    });
    wait_for("the Player listener to close", || {
        TcpStream::connect((Ipv4Addr::LOCALHOST, link_port)).is_err()
    });
}

#[test]
fn link_refuses_a_connection_without_the_session_token() {
    let player = Arc::new(FakePlayer {
        lines: mpsc::channel().0,
        requests: Mutex::default(),
    });
    // No Player is ready here: only the stranger below connects.
    struct Absent;
    impl Player for Absent {
        fn ready(&self) -> Option<(String, i64)> {
            None
        }
        fn start_stream(&self, _: &Value) -> Result<(), String> {
            unreachable!("no Player is ready")
        }
    }
    drop(player);
    let session = Session::start(Arc::new(Absent), Options::default(), fake_encoders()).unwrap();
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, session.shared.link_port)).unwrap();
    stream
        .write_all(b"UME-STREAM 1 00000000000000000000000000000000 1\n")
        .unwrap();
    let mut rest = Vec::new();
    assert_eq!(
        stream.read_to_end(&mut rest).unwrap(),
        0,
        "the link stayed open"
    );
    assert_eq!(session.status()["playerConnected"], false);
}

// Runs where the NVIDIA driver is present and reports that it was skipped elsewhere.
#[test]
fn nvenc_encodes_complete_access_units() {
    let parameters = Parameters {
        width: 320,
        height: 240,
        fps: 60,
        bitrate: 4_000_000,
    };
    let mut encoder = match hardware(&parameters) {
        Ok(encoder) => encoder,
        Err(error) => {
            eprintln!("skipped: NVENC is unavailable here: {error}");
            return;
        }
    };
    let frame: Vec<u8> = (0..320 * 240 * 4)
        .map(|index| (index % 251) as u8)
        .collect();
    let first = encoder.encode(&frame, false, 0).unwrap();
    assert!(first.keyframe && first.data.starts_with(&[0, 0, 0, 1]));
    assert!(
        codec_string(&first.data).unwrap().starts_with("avc1.64"),
        "{:?}",
        codec_string(&first.data)
    );
    let second = encoder.encode(&frame, false, 16_666).unwrap();
    assert!(!second.keyframe && !second.data.is_empty());
    let forced = encoder.encode(&frame, true, 33_333).unwrap();
    assert!(forced.keyframe && codec_string(&forced.data).is_some());
    assert!(encoder.encode(&frame[1..], false, 0).is_err());
}

#[test]
fn publish_commands_split_expand_and_find_the_public_address() {
    assert_eq!(
        split_words(r#"zrok share  public "a b" 'c "d"' e\ f """#).unwrap(),
        ["zrok", "share", "public", "a b", "c \"d\"", "e f", ""]
    );
    assert!(split_words("tunnel 'open").is_err() && split_words("tunnel \\").is_err());
    let address: SocketAddr = "127.0.0.1:4100".parse().unwrap();
    assert_eq!(
        expand("ngrok http {port} --target={url} {address} {host}", address).unwrap(),
        [
            "ngrok",
            "http",
            "4100",
            "--target=http://127.0.0.1:4100",
            "127.0.0.1:4100",
            "127.0.0.1"
        ]
    );
    assert!(expand("  ", address).is_err());

    let found = |line: &str| find_url(line, None);
    assert_eq!(
        found("forwarding http://127.0.0.1:4100 -> https://abc.share.zrok.io."),
        Some("https://abc.share.zrok.io".into())
    );
    assert_eq!(
        found(
            r#"lvl=info msg="started tunnel" addr=http://localhost:4100 url=https://x.ngrok-free.app"#
        ),
        Some("https://x.ngrok-free.app".into())
    );
    assert_eq!(
        found("listening on http://0.0.0.0:80 and http://[::1]:80/"),
        None
    );
    let pattern = regex::Regex::new(r"public=(\S+)").unwrap();
    assert_eq!(
        find_url(
            "see https://docs.example public=https://mine.example",
            Some(&pattern)
        ),
        Some("https://mine.example".into())
    );
    assert_eq!(find_url("see https://docs.example", Some(&pattern)), None);
}

#[test]
fn published_session_reports_the_public_url_and_stops_its_command() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let player = Arc::new(FakePlayer {
        lines: mpsc::channel().0,
        requests: Mutex::default(),
    });
    let options = Options {
        publish: format!(
            "sh -c 'echo $$ > {}; echo tunnel for {{url}} is https://tunnel.example/; exec sleep 60'",
            pid_file.display()
        ),
        ..Options::default()
    };
    let session = Session::start(player.clone(), options, fake_encoders()).unwrap();
    let status = session.status();
    let local = session.url();
    let secret = local.trim_end_matches('/').rsplit('/').next().unwrap();
    assert_eq!(
        status["publicUrl"],
        format!("https://tunnel.example/{secret}/")
    );
    assert_eq!(status["publish"]["running"], true);
    let listener = local
        .rsplit_once('/')
        .unwrap()
        .0
        .rsplit_once('/')
        .unwrap()
        .0;
    assert!(
        status["publish"]["output"]
            .as_str()
            .unwrap()
            .contains(&format!("tunnel for {listener} is")),
        "{status}"
    );
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    drop(session);
    wait_for("the publish command to stop", || {
        !std::path::Path::new(&format!("/proc/{pid}")).exists()
    });

    let missing = Options {
        publish: "/nonexistent/tunnel {url}".into(),
        ..Options::default()
    };
    assert!(Session::start(player.clone(), missing, fake_encoders()).is_err());
    // A command that ends without an address leaves the session usable locally.
    let silent = Options {
        publish: "true".into(),
        ..Options::default()
    };
    let session = Session::start(player, silent, fake_encoders()).unwrap();
    assert_eq!(session.status()["publicUrl"], Value::Null);
    wait_for("the command's exit", || {
        session.status()["publish"]["running"] == false
    });
}

#[test]
fn webrtc_session_binds_a_media_port_and_answers_only_valid_offers() {
    let player = Arc::new(FakePlayer {
        lines: mpsc::channel().0,
        requests: Mutex::default(),
    });
    let unknown = Options {
        transport: "quic".into(),
        ..Options::default()
    };
    assert!(Session::start(player.clone(), unknown, fake_encoders()).is_err());
    let options = Options {
        transport: "webrtc".into(),
        ..Options::default()
    };
    let session = Session::start(player, options, fake_encoders()).unwrap();
    let status = session.status();
    assert_eq!(status["transport"], "webrtc");
    let media: SocketAddr = status["webrtcAddress"].as_str().unwrap().parse().unwrap();
    assert!(media.ip().is_loopback() && media.port() != 0, "{media}");
    assert!(
        session
            .shared
            .state_json()
            .contains(r#""transport":"webrtc""#)
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let reply = runtime.block_on(
        session
            .shared
            .viewer_offer(1, json!({"type": "offer", "sdp": "not a description"})),
    );
    assert!(reply.contains("rtcError"), "{reply}");

    // A viewer that asks for video and sound, as the page does, gets both.
    let mut viewer = str0m::Rtc::builder().build(Instant::now());
    let mut changes = viewer.sdp_api();
    for kind in [
        str0m::media::MediaKind::Video,
        str0m::media::MediaKind::Audio,
    ] {
        changes.add_media(kind, str0m::media::Direction::RecvOnly, None, None, None);
    }
    let (offer, _) = changes.apply().unwrap();
    let reply = runtime.block_on(
        session
            .shared
            .viewer_offer(2, serde_json::to_value(offer).unwrap()),
    );
    let answer: Value = serde_json::from_str(&reply).unwrap();
    let sdp = answer["answer"]["sdp"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"));
    assert!(
        sdp.contains("m=audio") && sdp.contains("opus/48000/2"),
        "{sdp}"
    );
    assert!(
        sdp.contains("m=video") && sdp.contains("H264/90000"),
        "{sdp}"
    );
}

#[test]
fn mixdown_brings_any_rate_and_layout_to_48_khz_stereo_packets() {
    let mut mixdown = Mixdown::default();
    // A second at 44.1 kHz is a second at 48 kHz: fifty packets of 20 ms.
    let ramp: Vec<f32> = (0..44_100 * 2)
        .map(|index| (index / 2) as f32 / 44_100.0)
        .collect();
    let packets = mixdown.push(44_100, 2, &ramp);
    assert_eq!(packets.len(), 50);
    assert!(packets.iter().all(|packet| packet.len() == 960 * 2));
    // Linear interpolation keeps a ramp a ramp, one input sample late.
    let last = packets[49][960 * 2 - 2];
    assert!((last - 44_098.08 / 44_100.0).abs() < 1e-4, "{last}");
    // A change of rate or layout starts over.
    assert!(mixdown.push(48_000, 1, &[0.5; 959]).is_empty());
    let packets = mixdown.push(48_000, 1, &[0.5; 1]);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0][2..4], [0.5, 0.5], "mono goes to both sides");
    // 5.1: front pair, centre and surround pair at -3 dB, no LFE.
    let mut surround = Mixdown::default();
    let frame = [0.1, 0.2, 0.3, 0.4, 0.05, 0.06];
    let packets = surround.push(48_000, 6, &frame.repeat(961));
    let (left, right) = (packets[0][2], packets[0][3]);
    assert!((left - (0.1 + 0.707 * (0.3 + 0.05))).abs() < 1e-5, "{left}");
    assert!(
        (right - (0.2 + 0.707 * (0.3 + 0.06))).abs() < 1e-5,
        "{right}"
    );
}

#[test]
fn opus_encodes_a_packet_when_libopus_is_installed() {
    let Ok(mut encoder) = Opus::open() else {
        eprintln!("libopus.so.0 is not installed; skipped");
        return;
    };
    let tone: Vec<f32> = (0..960)
        .flat_map(|index| {
            let value = (index as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            [value, value]
        })
        .collect();
    let packet = encoder.encode(&tone).unwrap();
    assert!(
        !packet.is_empty() && packet.len() < 1000,
        "{}",
        packet.len()
    );
    assert!(encoder.encode(&tone[..10]).is_err());
}

#[test]
fn nvenc_spends_what_the_picture_needs_under_the_ceiling() {
    let (width, height) = (1280u32, 720u32);
    // Two seconds at 60 fps of a smooth picture, moving one pixel a frame
    // or standing still; the second second is measured.
    let rate = |speed: u32| -> Option<f64> {
        let parameters = Parameters {
            width,
            height,
            fps: 60,
            bitrate: 12_000_000,
        };
        let mut encoder = match hardware(&parameters) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("skipped: NVENC is unavailable here: {error}");
                return None;
            }
        };
        let mut bytes = 0;
        for frame in 0..120u32 {
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (index as u32 % width + frame * speed, index as u32 / width);
                let value = ((x / 64 + y / 64) % 2) as u8 * 120 + ((x + y) / 20 % 100) as u8;
                *pixel = [value, value / 2, 255 - value, 255];
            }
            let encoded = encoder
                .encode(&pixels, frame == 0, frame as u64 * 16_666)
                .unwrap();
            if frame >= 60 {
                bytes += encoded.data.len();
            }
        }
        Some(bytes as f64 * 8.0 / 1e6)
    };
    let Some(moving) = rate(1) else { return };
    let still = rate(0).unwrap();
    // The rate control alone spent the whole 12 Mbit/s on both.
    assert!(
        moving < 4.0,
        "a slowly moving picture took {moving:.2} Mbit/s"
    );
    assert!(still < 0.5, "a still picture took {still:.2} Mbit/s");
}
