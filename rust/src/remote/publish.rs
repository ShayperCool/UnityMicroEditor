//! The command a session runs to make its listener reachable from elsewhere,
//! such as a tunnel client. It lives as long as the session, and the first
//! public address it prints becomes the session's public URL.
use regex::Regex;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read},
    net::SocketAddr,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

/// Output kept for `status`, so that a failing command explains itself.
const OUTPUT_TAIL: usize = 4096;

/// Splits a command line into words: whitespace separates, single and double
/// quotes group, a backslash keeps the next character. No shell is involved.
pub fn split_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut characters = command.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some('\''), _) => word.push(character),
            (_, '\\') => word.push(characters.next().ok_or("command ends with a backslash")?),
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (None, _) if character.is_whitespace() => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            _ => word.push(character),
        }
    }
    if quote.is_some() {
        return Err("command has an unclosed quote".into());
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

/// The command with the listener's address in place of `{url}`
/// (`http://host:port`), `{address}` (`host:port`), `{host}` and `{port}`.
pub fn expand(template: &str, address: SocketAddr) -> Result<Vec<String>, String> {
    let words = split_words(template)?;
    if words.is_empty() {
        return Err("publish command is empty".into());
    }
    Ok(words
        .into_iter()
        .map(|word| {
            word.replace("{url}", &format!("http://{address}"))
                .replace("{address}", &address.to_string())
                .replace("{host}", &address.ip().to_string())
                .replace("{port}", &address.port().to_string())
        })
        .collect())
}

// An address on this machine is the tunnel's target, not its public side.
fn local(url: &str) -> bool {
    let host = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = host.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    name == "localhost"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// The public address a line of the command's output announces, if any. With
/// a pattern it is the first capture group (or the whole match); without one
/// the first http(s) URL that does not point at this machine.
pub fn find_url(line: &str, pattern: Option<&Regex>) -> Option<String> {
    if let Some(pattern) = pattern {
        let captures = pattern.captures(line)?;
        let found = captures.get(1).or_else(|| captures.get(0))?.as_str();
        return (!found.is_empty()).then(|| found.to_string());
    }
    static URL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let url = URL.get_or_init(|| Regex::new(r#"https?://[^\s"'<>\x1b]+"#).expect("fixed pattern"));
    url.find_iter(line)
        .map(|found| found.as_str().trim_end_matches(['.', ',', ';', ')', ']']))
        .find(|found| !local(found))
        .map(str::to_string)
}

#[derive(Default)]
struct State {
    url: Option<String>,
    output: String,
    exit: Option<String>,
}
type Watched = Arc<(Mutex<State>, Condvar)>;

pub struct Publisher {
    command: Vec<String>,
    child: Child,
    state: Watched,
}
impl Publisher {
    pub fn start(template: &str, pattern: &str, address: SocketAddr) -> Result<Publisher, String> {
        let command = expand(template, address)?;
        let pattern = match pattern {
            "" => None,
            pattern => {
                Some(Regex::new(pattern).map_err(|error| format!("publishPattern: {error}"))?)
            }
        };
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so that helpers it starts end with it.
            .process_group(0)
            .spawn()
            .map_err(|error| format!("publish command {}: {error}", command[0]))?;
        let state = Watched::default();
        let pattern = Arc::new(pattern);
        let mut readers = Vec::new();
        let outputs: [Option<Box<dyn Read + Send>>; 2] = [
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ];
        for pipe in outputs.into_iter().flatten() {
            let (state, pattern) = (state.clone(), pattern.clone());
            readers.push(thread::spawn(move || {
                let mut lines = BufReader::new(pipe).split(b'\n');
                while let Some(Ok(line)) = lines.next() {
                    let line = String::from_utf8_lossy(&line);
                    let mut watched = state.0.lock().unwrap();
                    watched.output.push_str(line.trim_end());
                    watched.output.push('\n');
                    if watched.output.len() > OUTPUT_TAIL {
                        let mut cut = watched.output.len() - OUTPUT_TAIL;
                        while !watched.output.is_char_boundary(cut) {
                            cut += 1;
                        }
                        watched.output.drain(..cut);
                    }
                    if watched.url.is_none()
                        && let Some(url) = find_url(&line, pattern.as_ref().as_ref())
                    {
                        watched.url = Some(url);
                        state.1.notify_all();
                    }
                }
            }));
        }
        // Both pipes close when the command, and whatever inherited them, has exited.
        let watcher = state.clone();
        thread::spawn(move || {
            for reader in readers {
                let _ = reader.join();
            }
            let mut watched = watcher.0.lock().unwrap();
            watched
                .exit
                .get_or_insert_with(|| "the publish command exited".into());
            watcher.1.notify_all();
        });
        Ok(Publisher {
            command,
            child,
            state,
        })
    }

    /// Waits until the command has announced its address, has exited, or the time is up.
    pub fn wait(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut watched = self.state.0.lock().unwrap();
        while watched.url.is_none() && watched.exit.is_none() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            watched = self.state.1.wait_timeout(watched, left).unwrap().0;
        }
    }

    /// The announced address with the session's secret path.
    pub fn url(&self, token: &str) -> Option<String> {
        let watched = self.state.0.lock().unwrap();
        watched
            .url
            .as_ref()
            .map(|url| format!("{}/{token}/", url.trim_end_matches('/')))
    }

    pub fn report(&self, token: &str) -> Value {
        let url = self.url(token);
        let watched = self.state.0.lock().unwrap();
        json!({
            "command": self.command,
            "running": watched.exit.is_none(),
            "publicUrl": url,
            "output": watched.output,
        })
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        let group = -(self.child.id() as libc::pid_t);
        // SAFETY: signals the process group this publisher created; the child is not reaped yet.
        unsafe { libc::kill(group, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: as above; the group outlived its grace period.
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}
