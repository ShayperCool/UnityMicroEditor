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
fn fake_encoders() -> Arc<Factory> {
    Arc::new(|_: &Parameters| Ok(Box::new(FakeEncoder(0)) as Box<dyn Encoder>))
}

// Plays the Player's side of the link: connects when asked, reports its state
// and answers every line it receives with one 4x2 frame.
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
    shared.viewer_joined();
    assert_eq!(received.recv().unwrap(), json!({"viewers": 1}));
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
    shared.viewer_message(r#"{"k":1,"r":1}"#);
    assert_eq!(received.recv().unwrap(), json!({"release": true}));
    assert!(
        video.blocking_recv().unwrap().keyframe,
        "a requested keyframe was not forced"
    );
    shared.viewer_message("not json");
    shared.viewer_message(r#"{"e":[{"t":"k","c":"Bogus","down":true}]}"#);
    shared.viewer_left();
    assert_eq!(received.recv().unwrap(), json!({"viewers": 0}));

    let status = session.status();
    assert_eq!(status["video"]["encoder"], "fake");
    assert_eq!(status["video"]["codec"], "avc1.640028");
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
