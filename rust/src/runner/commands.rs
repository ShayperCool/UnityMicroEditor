//! Commands for a running session: one JSON object per line on the runner's
//! standard input (`--commands-stdin`). The one command is
//! `{"command":"recompile","requestId":"<id>"}`.
use super::{Event, Wake};
use crate::sysutil::valid_id;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::Read,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime},
};

/// A line is far shorter; a longer one is not a command.
const LINE_MAXIMUM: usize = 64 * 1024;

/// "Restart the Player on the newest compiled code": the answer is the
/// readiness event of that restart, or the rejection of its compile check,
/// with this request id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recompile {
    pub request_id: String,
    pub at: Instant,
    pub wall: SystemTime,
}

/// The request id of a `recompile` command line, or why the line is none.
pub(crate) fn parse_command(line: &[u8]) -> Result<String, String> {
    let value: Value =
        serde_json::from_slice(line).map_err(|error| format!("not a JSON object: {error}"))?;
    let Some(object) = value.as_object() else {
        return Err("not a JSON object".into());
    };
    match object.get("command").and_then(Value::as_str) {
        Some("recompile") => {}
        Some(other) => return Err(format!("unknown command {other:?}")),
        None => return Err("no command".into()),
    }
    match object.get("requestId").and_then(Value::as_str) {
        Some(id) if valid_id(id) => Ok(id.to_string()),
        _ => {
            Err("requestId must be ASCII letters, digits or hyphens, at most 64 characters".into())
        }
    }
}

/// Splits `input` into lines until it ends. A line that is no command is
/// handed to `reject` with the reason; neither it nor the end of the input
/// ends anything else.
pub(crate) fn read_commands(
    mut input: impl Read,
    mut accept: impl FnMut(String),
    mut reject: impl FnMut(&[u8], String),
) {
    let mut line: Vec<u8> = Vec::new();
    // The rest of a line that outgrew the limit is skipped.
    let mut oversized = false;
    let mut finish = |line: &mut Vec<u8>, oversized: &mut bool| {
        if std::mem::take(oversized) {
            reject(
                &line[..],
                format!("command line exceeds {LINE_MAXIMUM} bytes"),
            );
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            match parse_command(line) {
                Ok(id) => accept(id),
                Err(error) => reject(&line[..], error),
            }
        }
        line.clear();
    };
    let mut chunk = [0u8; 4096];
    loop {
        let count = match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &byte in &chunk[..count] {
            if byte == b'\n' {
                finish(&mut line, &mut oversized);
            } else if line.len() < LINE_MAXIMUM {
                line.push(byte);
            } else {
                oversized = true;
            }
        }
    }
    // A last line without its newline is still a whole write of its sender.
    finish(&mut line, &mut oversized);
}

/// The commands read so far. The reader thread keeps draining its input, so
/// a sender never blocks on a full pipe while the loop is busy.
pub(crate) struct Commands {
    queue: Arc<Mutex<VecDeque<Recompile>>>,
}
impl Commands {
    /// Reads `input` on a thread of its own and wakes the loop for every command.
    pub fn read(input: impl Read + Send + 'static, wake: &Arc<Wake>) -> Self {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let (sink, wake) = (queue.clone(), wake.clone());
        std::thread::spawn(move || {
            read_commands(
                input,
                |request_id| {
                    sink.lock().unwrap().push_back(Recompile {
                        request_id,
                        at: Instant::now(),
                        wall: SystemTime::now(),
                    });
                    wake.notify();
                },
                |line, error| rejected_event(line, &error).emit(),
            );
        });
        Self { queue }
    }
    /// A source of no commands, for a session without `--commands-stdin`.
    pub fn none() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    /// A receive that does not block.
    pub fn try_recv(&self) -> Option<Recompile> {
        self.queue.lock().unwrap().pop_front()
    }
}

/// The event for a line that is no command. A controller stores it and
/// changes nothing.
pub(crate) fn rejected_event(line: &[u8], error: &str) -> Event {
    let text = String::from_utf8_lossy(&line[..line.len().min(256)]).into_owned();
    Event::new("command-rejected")
        .with("error", error)
        .with("line", &text)
}
