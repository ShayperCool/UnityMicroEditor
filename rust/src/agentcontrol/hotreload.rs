//! Hot reload as the controller sees it: what the runner reported about the
//! code of the running Player, and the `recompile` command the runner reads
//! from its standard input.
use super::{Controller, Error, Inner, State, StateError, Typed};
use crate::{
    cancel::{Context, Source, select},
    jsondoc,
    sysutil::{self, valid_id},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{io::Write, os::fd::AsRawFd, process::ChildStdin};

// The runner caps the lists of its events the same way and names the report
// file that holds every entry. The state is repeated in each session reply.
const LIST_MAXIMUM: usize = 16;
const TEXT_MAXIMUM: usize = 1024;
/// The `recompile` commands of a runner whose outcome is kept for a repeated
/// request id.
pub(super) const RECOMPILE_HISTORY: usize = 16;

/// The code state of a session. It is left out of the session state while it
/// equals the default: hot reload enabled and a Player that runs exactly the
/// assemblies it loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotReload {
    /// `false`: every C# edit restarts the Player.
    pub enabled: bool,
    /// The code patch generation this Player process runs; 0 before the first one.
    pub code_generation: u64,
    /// The newest compiled code is not in the Player and cannot be patched
    /// into it. Only a recompile, which restarts the Player, applies it.
    pub full_recompile_required: bool,
    /// Why, at most 16 edits; `unsupported_count` counts all of them and
    /// `report` names the file that lists them.
    pub unsupported: Vec<Reason>,
    pub unsupported_count: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub report: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_patch: Option<LastPatch>,
    /// What the Player does not take over of the newest compiled code although
    /// nothing had to be patched, for example a changed accessibility; at most
    /// 16, `notes_count` counts all and `notes_report` names the file with
    /// them. Only a recompile applies these too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Reason>,
    #[serde(skip_serializing_if = "no_count")]
    pub notes_count: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub notes_report: String,
    /// Why this Player process was started, when hot reload caused it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_restart: Option<LastRestart>,
    /// The request id of a `recompile` command the runner has not answered.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pending_recompile: String,
    /// Sequence of the rejection that gave the reasons of the refusal that
    /// stands; 0 when none stands or the runner has not sent them yet.
    #[serde(skip)]
    refused_sequence: u64,
    /// The runner's own sentence for that rejection. The session's update
    /// error does not keep it: later updates and rejections replace that.
    #[serde(skip)]
    refused_summary: String,
}
impl Default for HotReload {
    fn default() -> Self {
        HotReload {
            enabled: true,
            code_generation: 0,
            full_recompile_required: false,
            unsupported: Vec::new(),
            unsupported_count: 0,
            report: String::new(),
            last_patch: None,
            notes: Vec::new(),
            notes_count: 0,
            notes_report: String::new(),
            last_restart: None,
            pending_recompile: String::new(),
            refused_sequence: 0,
            refused_summary: String::new(),
        }
    }
}
/// One edit that cannot be applied live, or one caveat of an applied patch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub code: String,
    pub member: String,
    pub message: String,
}
/// The last `code-patched` event of this Player process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastPatch {
    pub sequence: u64,
    pub code_generation: u64,
    pub patched_methods: u64,
    pub added_methods: u64,
    pub added_fields: u64,
    pub restored_methods: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assemblies: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// What the patch does not cover, for example values of existing objects.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Reason>,
    pub notes_count: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub report: String,
}
/// The `reason` of the readiness event that started this Player process:
/// `requested` (a `recompile` command), `hot_reload_unsupported` (the runner
/// restarted by itself for an edit it could not patch) or `hot_reload_failed`
/// (a code patch left the Player unusable, with the `error`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastRestart {
    pub sequence: u64,
    pub reason: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<Reason>,
    pub unsupported_count: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub report: String,
}
/// What a caller is told together with a refusal that stands.
pub(super) enum Refused<'a> {
    /// Nothing but the refusal answers the caller's wait.
    Edit,
    /// An update was confirmed after this sequence, the caller's mark.
    UpdatedAfter(u64),
    /// The `recompile` command with this id restarted the Player before the
    /// refused edit compiled. It is answered, so the next one needs another id.
    Recompiled(&'a str),
}

fn no_count(count: &u64) -> bool {
    *count == 0
}
fn bounded(text: &str) -> String {
    let mut end = text.len().min(TEXT_MAXIMUM);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
fn text(event: &Map<String, Value>, key: &str) -> String {
    bounded(event.get(key).and_then(Value::as_str).unwrap_or(""))
}
/// An exact non-negative integer of a decoded event.
pub(super) fn count(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) => number.to_string().parse().ok(),
        _ => None,
    }
}
// The number an event states for a list it may have cut short.
fn total(event: &Map<String, Value>, key: &str, listed: usize) -> u64 {
    count(event.get(key)).unwrap_or(0).max(listed as u64)
}
fn list(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(LIST_MAXIMUM)
}
fn strings(value: Option<&Value>) -> Vec<String> {
    list(value).filter_map(Value::as_str).map(bounded).collect()
}
fn reasons(value: Option<&Value>) -> Vec<Reason> {
    list(value)
        .filter_map(Value::as_object)
        .map(|entry| Reason {
            code: text(entry, "code"),
            member: text(entry, "member"),
            message: text(entry, "message"),
        })
        .collect()
}

impl HotReload {
    pub(super) fn is_default(&self) -> bool {
        *self == HotReload::default()
    }
    /// The state of a launch before the runner reported anything.
    pub(super) fn launched(enabled: bool) -> Self {
        HotReload {
            enabled,
            ..HotReload::default()
        }
    }

    /// A readiness event names a new Player process. It runs the newest
    /// compiled code without a patch, so nothing of the old process applies.
    pub(super) fn restarted(&mut self, sequence: u64, event: &Map<String, Value>) {
        let enabled = match event.get("hotReload") {
            Some(Value::Bool(enabled)) => *enabled,
            _ => self.enabled,
        };
        *self = HotReload {
            pending_recompile: std::mem::take(&mut self.pending_recompile),
            ..HotReload::launched(enabled)
        };
        let reason = text(event, "reason");
        if !reason.is_empty() {
            let unsupported = reasons(event.get("unsupported"));
            self.last_restart = Some(LastRestart {
                sequence,
                reason,
                request_id: text(event, "requestId"),
                error: text(event, "error"),
                unsupported_count: total(event, "unsupportedCount", unsupported.len()),
                unsupported,
                // `report` of a readiness event is the Player's own report.
                report: text(event, "hotReloadReport"),
            });
        }
    }
    /// `code-patched`: a patch holds every difference from the loaded
    /// assemblies, so the same Player now runs the newest code.
    pub(super) fn patched(&mut self, sequence: u64, generation: u64, event: &Map<String, Value>) {
        self.code_generation = generation;
        self.clear_refusal();
        self.clear_notes();
        let number = |key: &str| count(event.get(key)).unwrap_or(0);
        let notes = reasons(event.get("notes"));
        self.last_patch = Some(LastPatch {
            sequence,
            code_generation: generation,
            patched_methods: number("patchedMethods"),
            added_methods: number("addedMethods"),
            added_fields: number("addedFields"),
            restored_methods: number("restoredMethods"),
            assemblies: strings(event.get("assemblies")),
            members: strings(event.get("members")),
            notes_count: total(event, "notesCount", notes.len()),
            notes,
            report: text(event, "report"),
        });
    }
    /// `update-rejected` with `fullRecompileRequired`; `summary` is its text.
    pub(super) fn refused(&mut self, sequence: u64, event: &Map<String, Value>, summary: &str) {
        self.full_recompile_required = true;
        self.clear_notes();
        self.refused_sequence = sequence;
        self.refused_summary = bounded(summary);
        self.unsupported = reasons(event.get("unsupported"));
        self.unsupported_count = total(event, "unsupportedCount", self.unsupported.len());
        self.report = text(event, "report");
    }
    /// `updated` or `unchanged`, or the rejection of a scene part. A scene
    /// update proves nothing about the code, so the runner states it: `false`
    /// when the Player runs the newest compiled code, `true` when that code
    /// cannot be patched into it. With `false` come the notes of a compile
    /// that needed no patch, for as long as they stand. An event without the
    /// statement leaves the state as it is.
    pub(super) fn confirmed(&mut self, event: &Map<String, Value>) {
        match event.get("fullRecompileRequired") {
            Some(Value::Bool(false)) => {
                self.clear_refusal();
                self.notes = reasons(event.get("notes"));
                self.notes_count = total(event, "notesCount", self.notes.len());
                self.notes_report = match self.notes_count {
                    0 => String::new(),
                    _ => text(event, "hotReloadReport"),
                };
            }
            // The reasons come with the rejection of the same change, which
            // the runner may print after this event.
            Some(Value::Bool(true)) => {
                self.full_recompile_required = true;
                self.clear_notes();
            }
            _ => {}
        }
    }
    fn clear_notes(&mut self) {
        self.notes.clear();
        self.notes_count = 0;
        self.notes_report.clear();
    }
    fn clear_refusal(&mut self) {
        self.full_recompile_required = false;
        self.refused_sequence = 0;
        self.refused_summary.clear();
        self.unsupported.clear();
        self.unsupported_count = 0;
        self.report.clear();
    }

    /// The failure that reports the refusal that stands to a caller.
    pub(super) fn refusal(&self, told: Refused) -> (Error, Typed) {
        let first = self.unsupported.first().cloned().unwrap_or_default();
        let code = match first.code.as_str() {
            "" => String::new(),
            code => format!("[{code}]"),
        };
        let mut reason = [first.member.as_str(), first.message.trim_end_matches('.')]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(": ");
        reason = [reason, code]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        // The runner's sentence stands in when its event listed no reason.
        if reason.is_empty() {
            reason = self.refused_summary.clone();
        } else if self.unsupported_count > 1 {
            reason.push_str(&format!(" (and {} more)", self.unsupported_count - 1));
        }
        let mut reason = reason.trim_end_matches('.');
        // Only the runner's statement on a scene update has arrived so far.
        if reason.is_empty() {
            reason =
                "the reasons are not reported yet, hot_reload with action \"status\" lists them";
        }
        let refused = format!(
            "The C# edit compiled but cannot be applied to the running Player: {reason}. The Player still runs the previous code"
        );
        const NEXT: &str = "Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.";
        let message = match told {
            Refused::Edit => format!("{refused}. {NEXT}"),
            Refused::UpdatedAfter(sequence) => format!(
                "{refused}; the other source changes confirmed after sequence {sequence} are applied. {NEXT}"
            ),
            Refused::Recompiled(id) => format!(
                "Recompile {id} restarted the Player, and a C# edit compiled after it cannot be applied to the running Player: {reason}. Call hot_reload with action \"recompile\" and a new requestId to restart the Player on the new code; game state is reset."
            ),
        };
        let details = json!({
            "unsupported": self.unsupported,
            "unsupportedCount": self.unsupported_count,
            "report": self.report,
            "next": {"tool": "hot_reload", "arguments": {"action": "recompile"}},
        });
        (
            Error::new(message),
            Typed {
                code: "full_recompile_required",
                details,
            },
        )
    }
}

/// A `recompile` command written to the runner and, once the runner answered
/// it, its outcome.
#[derive(Clone)]
pub(super) struct Recompile {
    id: String,
    /// Sequence of the event that answered it; 0 until the runner has.
    answered: u64,
    /// The text of the `update-rejected` that answered it; none for a restart.
    rejection: Option<String>,
}

// A runner that stopped reading must fail a command, not block the controller
// in a full pipe while it holds the session lock.
pub(super) fn nonblocking(commands: &ChildStdin) {
    // SAFETY: the descriptor is open and owned by `commands`; both calls only
    // read and set its status flags.
    unsafe {
        let flags = libc::fcntl(commands.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(
                commands.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            );
        }
    }
}

impl Inner {
    /// Matches a readiness event (`rejection` absent) or a plain
    /// `update-rejected` against the pending command, which the runner names
    /// in its answer. A rejection without that id is the compile error or the
    /// failed reload of a source edit, and a restart with another cause
    /// belongs to an edit handled first: the runner executes the command
    /// after either.
    pub(super) fn answer_recompile(
        &mut self,
        sequence: u64,
        event: &Map<String, Value>,
        rejection: Option<&str>,
    ) {
        // Only the newest command can be unanswered: the next one is written
        // after the answer.
        let Some(recompile) = self.recompiles.back_mut().filter(|sent| sent.answered == 0) else {
            return;
        };
        let member = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or("");
        let named = member("requestId");
        // Nothing but a command restarts the Player with this reason.
        let requested = rejection.is_none() && named.is_empty() && member("reason") == "requested";
        if named == recompile.id || requested {
            recompile.answered = sequence;
            recompile.rejection = rejection.map(str::to_string);
            self.state.hot_reload.pending_recompile.clear();
        }
    }
    /// The runner is gone, and with it the Player its code state described
    /// and the commands it was given.
    pub(super) fn forget_code(&mut self) {
        self.commands = None;
        self.recompiles.clear();
        self.state.update_pending = false;
        self.state.hot_reload = HotReload::launched(self.state.hot_reload.enabled);
    }
}

impl State {
    /// Whether the update this session last rejected was refused by hot
    /// reload. A recompile answers that, never a new preparation.
    pub fn code_refused(&self) -> bool {
        let refused = self.hot_reload.refused_sequence;
        refused != 0 && refused == self.last_update_rejected_sequence
    }
}

impl Controller {
    /// The `hot_reload(status)` reply: the code state of the session, or what
    /// the next launch uses while no runner is owned, with the session phase
    /// and the policy for edits that cannot be applied live.
    pub fn hot_reload(&self) -> Value {
        let inner = self.lock();
        let state = match inner.command {
            Some(_) => inner.state.hot_reload.clone(),
            None => HotReload::launched(inner.launch.hot_reload != Some(false)),
        };
        let mut reply = serde_json::to_value(&state).expect("hot reload state serializes");
        reply["phase"] = inner.state.phase.as_str().into();
        if state.enabled {
            reply["onUnsupportedEdit"] = inner.launch.fallback().into();
        }
        reply
    }

    /// Tells the runner to restart the Player on the newest compiled code and
    /// waits for its answer: that restart, or the `update-rejected` of a
    /// compile error, which keeps the Player. A request id names one command
    /// of the runner. Repeating it waits for that command while it is pending
    /// and returns its recorded outcome afterwards; it never restarts the
    /// Player a second time.
    pub fn recompile(
        &self,
        ctx: &Context,
        request_id: &str,
    ) -> std::result::Result<State, StateError> {
        let failed = |state: State, error: Error, typed: Option<Typed>| StateError {
            state: Box::new(state),
            error,
            typed,
        };
        if !valid_id(request_id) {
            return Err(failed(
                self.status(),
                Error::new(
                    "requestId must be ASCII letters, digits or hyphens, at most 64 characters",
                ),
                None,
            ));
        }
        // Whether the runner had answered the command before this call.
        let repeated = {
            let mut inner = self.lock();
            let known = inner
                .recompiles
                .iter()
                .find(|sent| sent.id == request_id)
                .map(|sent| sent.answered != 0);
            if known.is_none() {
                let pending = inner
                    .recompiles
                    .back()
                    .filter(|sent| sent.answered == 0)
                    .map(|sent| sent.id.clone());
                if let Some(pending) = pending {
                    return Err(failed(
                        inner.state.clone(),
                        Error::new(format!(
                            "recompile {pending} is still pending; wait for it with session_control(wait_update) or repeat hot_reload(recompile) with that requestId. session_control(reset) replaces a runner that does not answer"
                        )),
                        Some(Typed {
                            code: "recompile_pending",
                            details: json!({"requestId": pending}),
                        }),
                    ));
                }
                let line = format!(
                    "{{\"command\":\"recompile\",\"requestId\":{}}}\n",
                    jsondoc::quote(request_id)
                );
                let ready = inner.state.phase == "ready";
                let Some(commands) = inner.commands.as_mut().filter(|_| ready) else {
                    return Err(failed(
                        inner.state.clone(),
                        Error::new(
                            "no ready Player session to recompile; use session_control(start) and session_control(wait)",
                        ),
                        Some(Typed {
                            code: "session_not_ready",
                            details: json!({"phase": inner.state.phase}),
                        }),
                    ));
                };
                // The runner closed its input or stopped reading it. The line
                // is shorter than PIPE_BUF, so none of it was written, and
                // the session stays ready without a way to command it.
                if let Err(error) = commands.write_all(line.as_bytes()) {
                    return Err(failed(
                        inner.state.clone(),
                        Error::new(format!(
                            "the runner of this session takes no commands (write the recompile command: {}); use session_control(reset) to replace it, which restarts the Player on the newest code",
                            sysutil::errno_text(&error)
                        )),
                        None,
                    ));
                }
                while inner.recompiles.len() >= RECOMPILE_HISTORY {
                    inner.recompiles.pop_front();
                }
                inner.recompiles.push_back(Recompile {
                    id: request_id.into(),
                    answered: 0,
                    rejection: None,
                });
                inner.state.hot_reload.pending_recompile = request_id.into();
                inner.notify();
            }
            known == Some(true)
        };
        loop {
            let (state, changed, sent) = {
                let inner = self.lock();
                let sent = inner.recompiles.iter().find(|sent| sent.id == request_id);
                (inner.state.clone(), inner.changed.clone(), sent.cloned())
            };
            let ended = match state.phase.as_str() {
                "failed" => Some(state.last_error.clone()),
                "stopped" => Some("runtime session stopped before the requested recompile".into()),
                // The runner that took the command ended and another started.
                _ if sent.is_none() => {
                    Some("runtime session was restarted before the requested recompile".into())
                }
                _ => None,
            };
            if let Some(message) = ended {
                return Err(failed(state, Error::new(message), None));
            }
            if let Some(sent) = sent.filter(|sent| sent.answered != 0) {
                return match sent.rejection {
                    Some(rejection) if repeated => Err(failed(
                        state,
                        Error::new(format!(
                            "recompile {request_id} was already rejected at sequence {} (send a new requestId to recompile again): {rejection}",
                            sent.answered
                        )),
                        None,
                    )),
                    Some(rejection) => Err(failed(state, Error::new(rejection), None)),
                    // The restart cleared every earlier refusal, so this one is
                    // of an edit compiled after it. Success would hide it.
                    None if state.hot_reload.full_recompile_required => {
                        let (error, typed) =
                            state.hot_reload.refusal(Refused::Recompiled(request_id));
                        Err(failed(state, error, Some(typed)))
                    }
                    None => Ok(state),
                };
            }
            if select(&[Source::Context(ctx), Source::Flag(&changed)], None) == Some(0) {
                return Err(failed(
                    self.status(),
                    ctx.error().expect("selected context is done").into(),
                    None,
                ));
            }
        }
    }
}
