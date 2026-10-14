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
use std::{collections::VecDeque, io::Write, os::fd::AsRawFd, process::ChildStdin};

// The runner caps the lists of its events the same way and names the report
// file that holds every entry. The state is repeated in each session reply.
const LIST_MAXIMUM: usize = 16;
const TEXT_MAXIMUM: usize = 1024;
/// The `recompile` commands of a runner whose outcome is kept for a repeated
/// request id.
pub(super) const RECOMPILE_HISTORY: usize = 16;
/// The answers of a Player process whose new notes are kept, the newest.
/// The session keeps as many events.
const BROUGHT_MAXIMUM: usize = 64;
/// The notes the runner gives a scene update from what the saved data holds
/// (a value of a field the patch added, a UnityEvent or an animation event
/// that names a method it added). Neither the patch nor a compile lists
/// them, so they do not end with a list that lacks them.
const DATA_NOTES: [&str; 2] = [
    "added_method_named_by_string",
    "serialized_field_not_restored",
];

/// The notes one answer of the runner brought: what of the edit it applied
/// is not in effect. A wait that covers several answers reports those of
/// all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Brought {
    sequence: u64,
    /// At most 16 of them.
    notes: Vec<Reason>,
    count: u64,
    /// The file that lists every note of the answer.
    report: String,
}

/// What a note means for a caller that has just been told "applied".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weight {
    /// A part of the saved code does not run, or not for what exists, until
    /// the Player restarts.
    Held,
    /// All of the code runs; something else happened that no restart undoes.
    Told,
    /// Only a lookup by name or by reflection from outside the edit differs.
    /// Listed, and no reason to read anything first.
    Minor,
}

/// The note codes by how much a caller must know of them, the first the
/// most. A code that is not listed (a newer builder) leads.
/// Every Held code stands before every Told one: the sentence is worded by
/// the first, and "its code runs" must not hide a limit.
const WEIGHTS: [(&str, Weight); 19] = [
    ("runtime_initializer_changed", Weight::Held),
    ("added_method_named_by_string", Weight::Held),
    ("serialized_field_not_restored", Weight::Held),
    ("running_coroutines_keep_old_code", Weight::Held),
    ("started_objects_keep_state", Weight::Held),
    ("spawned_objects_keep_state", Weight::Held),
    ("added_method_computed_name", Weight::Held),
    ("lambda_not_remapped", Weight::Held),
    ("earlier_patch_delegates_keep_old_code", Weight::Held),
    ("constructor_applies_to_new_objects", Weight::Held),
    ("existing_instances_keep_default", Weight::Held),
    ("runtime_initializer_added", Weight::Held),
    ("runtime_initializer_removed", Weight::Held),
    ("added_method_delegate_identity", Weight::Held),
    ("added_member_attributes_ignored", Weight::Held),
    ("callback_failed", Weight::Told),
    ("field_value_reset", Weight::Told),
    ("accessibility_changed", Weight::Minor),
    ("added_method_invisible_by_name", Weight::Minor),
];
fn weight(code: &str) -> (usize, Weight) {
    WEIGHTS
        .iter()
        .position(|(known, _)| *known == code)
        .map_or((0, Weight::Held), |index| (index + 1, WEIGHTS[index].1))
}

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
    /// The notes that the last change of the Player's code brought and the
    /// code before it did not have: what of that edit is not in effect. At
    /// most 16; `new_notes_count` counts all, and `new_notes_sequence` is the
    /// event that brought them. Notes that stood before are in
    /// `last_patch.notes` or `notes`. In the reply of a wait or a recompile
    /// these are the notes of every answer after the caller's mark, the most
    /// important first, and the sequence is that of the newest of them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub new_notes: Vec<Reason>,
    #[serde(skip_serializing_if = "no_count")]
    pub new_notes_count: u64,
    #[serde(skip_serializing_if = "no_count")]
    pub new_notes_sequence: u64,
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
    /// The scene or asset change that the running Player rejected while the
    /// refusal stands: the sequence of its event and its own error. A scene
    /// that only lacks the refused code fails with it, and so does one that
    /// is wrong by itself; the caller is given the text to tell them apart.
    #[serde(skip)]
    scene_rejected: (u64, String),
    /// The new notes of each answer of this Player process, oldest first.
    /// A refusal does not end them: the patch they came with is still in
    /// the Player. A note that a later answer shows to be gone is dropped.
    #[serde(skip)]
    brought: VecDeque<Brought>,
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
            new_notes: Vec::new(),
            new_notes_count: 0,
            new_notes_sequence: 0,
            last_restart: None,
            pending_recompile: String::new(),
            refused_sequence: 0,
            refused_summary: String::new(),
            scene_rejected: (0, String::new()),
            brought: VecDeque::new(),
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
    /// After the caller's mark the session confirmed an update, the newest
    /// of them by this event at this sequence: an earlier edit of the same
    /// wait that was patched in, a scene or asset change, a restart.
    After(&'a str, u64),
    /// The `recompile` command with this id restarted the Player before the
    /// refused edit compiled. It is answered, so the next one needs another id.
    Recompiled(&'a str),
}

/// What a restart for a Netcode reason means for the other Players of the
/// same network game: they still run the old code, reconnect to this one,
/// and read what it sends out of step until they are restarted on the new
/// code too. Empty for other reasons.
fn peers(reasons: &[Reason]) -> &'static str {
    match reasons
        .iter()
        .any(|reason| reason.code.starts_with("netcode_"))
    {
        true => {
            " This Player leaves its network session and starts on the new code; other Players of the same network game that still run the old code (a client of another session, the host) must be restarted on the new code too, or they read what this Player sends out of step."
        }
        false => "",
    }
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
        self.take_new_notes(sequence, event, &text(event, "report"));
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
        self.forget_ended();
    }
    /// `update-rejected` with `fullRecompileRequired`; `summary` is its text.
    pub(super) fn refused(&mut self, sequence: u64, event: &Map<String, Value>, summary: &str) {
        self.full_recompile_required = true;
        self.clear_notes();
        self.refused_sequence = sequence;
        // The rejection of a scene part repeats the reasons beside an error
        // of its own; the sentence of the refusal is the refusal's.
        if event.get("part").and_then(Value::as_str) == Some("scene") {
            self.scene_rejected = (sequence, bounded(summary));
        } else {
            self.refused_summary = bounded(summary);
        }
        self.unsupported = reasons(event.get("unsupported"));
        self.unsupported_count = total(event, "unsupportedCount", self.unsupported.len());
        self.report = text(event, "report");
    }
    /// `updated` or `unchanged`: the Player runs the scene as it is saved.
    pub(super) fn scene_applied(&mut self) {
        self.scene_rejected = (0, String::new());
    }
    /// `updated` or `unchanged`, or the rejection of a scene part. A scene
    /// update proves nothing about the code, so the runner states it: `false`
    /// when the Player runs the newest compiled code, `true` when that code
    /// cannot be patched into it. With `false` come the notes of a compile
    /// that needed no patch, for as long as they stand. An event without the
    /// statement leaves the state as it is.
    pub(super) fn confirmed(&mut self, sequence: u64, event: &Map<String, Value>) {
        match event.get("fullRecompileRequired") {
            Some(Value::Bool(false)) => {
                self.clear_refusal();
                self.notes = reasons(event.get("notes"));
                self.notes_count = total(event, "notesCount", self.notes.len());
                self.notes_report = match self.notes_count {
                    0 => String::new(),
                    _ => text(event, "hotReloadReport"),
                };
                // The compile of this window needed no patch and brought
                // notes; or the notes of such a compile have ended, and with
                // them what was new about them. Those of a patch are its own.
                let of_patch = self
                    .last_patch
                    .as_ref()
                    .is_some_and(|patch| patch.sequence == self.new_notes_sequence);
                if event.contains_key("newNotes") || self.notes_count == 0 && !of_patch {
                    let report = text(event, "hotReloadReport");
                    self.take_new_notes(sequence, event, &report);
                }
                self.forget_ended();
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
        self.new_notes.clear();
        self.new_notes_count = 0;
        self.new_notes_sequence = 0;
    }
    fn take_new_notes(&mut self, sequence: u64, event: &Map<String, Value>, report: &str) {
        self.new_notes = reasons(event.get("newNotes"));
        self.new_notes_count = total(event, "newNotesCount", self.new_notes.len());
        self.new_notes_sequence = match self.new_notes_count {
            0 => 0,
            _ => sequence,
        };
        if self.new_notes_count > 0 {
            while self.brought.len() >= BROUGHT_MAXIMUM {
                self.brought.pop_front();
            }
            self.brought.push_back(Brought {
                sequence,
                notes: self.new_notes.clone(),
                count: self.new_notes_count,
                report: report.to_string(),
            });
        }
    }
    /// Drops from the kept answers the notes that stand no more. A patch
    /// repeats every note of the code it carries, and a compile that needs
    /// no patch lists its own, so a note in neither list has ended: the
    /// edit was taken back or made again without it. Lists that were cut
    /// to 16 prove nothing, and everything is kept then.
    fn forget_ended(&mut self) {
        let whole = |listed: &[Reason], count: u64| listed.len() as u64 == count;
        let patch = self.last_patch.as_ref();
        if !whole(&self.notes, self.notes_count)
            || patch.is_some_and(|patch| !whole(&patch.notes, patch.notes_count))
        {
            return;
        }
        let standing: Vec<(&str, &str)> = self
            .notes
            .iter()
            .chain(patch.into_iter().flat_map(|patch| patch.notes.iter()))
            .map(|note| (note.code.as_str(), note.member.as_str()))
            .collect();
        // The runner's notes about the data of a scene update name a member
        // the patch added, which the patch notes under a code of its own
        // (an added method that no string names, for one). They stand for
        // as long as the patch notes that member.
        let patched: Vec<&str> = patch
            .into_iter()
            .flat_map(|patch| patch.notes.iter())
            .map(|note| note.member.as_str())
            .collect();
        let of_data = |note: &Reason| {
            DATA_NOTES.contains(&note.code.as_str()) && patched.contains(&note.member.as_str())
        };
        // What happened once is no state that could end.
        let stated = |note: &Reason| note.code == "callback_failed";
        for answer in &mut self.brought {
            let before = answer.notes.len();
            answer.notes.retain(|note| {
                stated(note)
                    || of_data(note)
                    || standing.contains(&(note.code.as_str(), note.member.as_str()))
            });
            answer.count = answer
                .count
                .saturating_sub((before - answer.notes.len()) as u64);
        }
        self.brought.retain(|answer| answer.count > 0);
    }
    fn clear_refusal(&mut self) {
        self.full_recompile_required = false;
        self.refused_sequence = 0;
        self.refused_summary.clear();
        self.scene_rejected = (0, String::new());
        self.unsupported.clear();
        self.unsupported_count = 0;
        self.report.clear();
    }

    /// Makes `new_notes` those of every answer after `after`: a wait or a
    /// recompile that covers several windows reports what each of them
    /// brought. The most important stand first, each note once.
    pub(super) fn since(&mut self, after: u64) {
        let mut notes: Vec<Reason> = Vec::new();
        let (mut count, mut sequence) = (0u64, 0u64);
        for answer in self.brought.iter().filter(|answer| answer.sequence > after) {
            let mut added = 0;
            for note in &answer.notes {
                let known = notes
                    .iter()
                    .any(|kept| kept.code == note.code && kept.member == note.member);
                if !known {
                    notes.push(note.clone());
                    added += 1;
                }
            }
            // The notes an answer did not list are counted as they are.
            count += added + answer.count.saturating_sub(answer.notes.len() as u64);
            sequence = answer.sequence;
        }
        notes.sort_by_key(|note| weight(&note.code).0);
        notes.truncate(LIST_MAXIMUM);
        self.new_notes = notes;
        self.new_notes_count = count.max(self.new_notes.len() as u64);
        self.new_notes_sequence = sequence;
    }

    /// What stands in the Player, for a caller whose mark nothing followed:
    /// how its code came to be, and the first limit of it that lasts until a
    /// restart. Another caller of the same server may have been told of the
    /// caller's own edit before it asked, and this is all it learns of it.
    pub(super) fn standing(&self) -> String {
        let held = |notes: &mut Vec<&Reason>| {
            notes.retain(|note| weight(&note.code).1 != Weight::Minor);
            notes.sort_by_key(|note| weight(&note.code).0);
            match notes.first() {
                None => String::new(),
                Some(first) => {
                    let more = match notes.len() - 1 {
                        0 => String::new(),
                        more => format!(
                            " (and {more} more in hotReload.lastPatch.notes and hotReload.notes)"
                        ),
                    };
                    format!(
                        "; this limit of it lasts until the Player restarts: {} [{}]{more}",
                        first.message.trim_end_matches('.'),
                        first.code
                    )
                }
            }
        };
        let mut notes: Vec<&Reason> = self.notes.iter().collect();
        // What a scene update brought about the patch's members stands too.
        for note in self.brought.iter().flat_map(|answer| answer.notes.iter()) {
            if DATA_NOTES.contains(&note.code.as_str())
                && !notes
                    .iter()
                    .any(|kept| kept.code == note.code && kept.member == note.member)
            {
                notes.push(note);
            }
        }
        if let Some(patch) = &self.last_patch {
            notes.extend(patch.notes.iter());
            return format!(
                "What stands: the Player runs code generation {}, patched in at sequence {}{}.",
                patch.code_generation,
                patch.sequence,
                held(&mut notes)
            );
        }
        let started = match &self.last_restart {
            Some(restart) if restart.reason == "requested" => format!(
                "the Player was restarted on the saved code by a recompile at sequence {}",
                restart.sequence
            ),
            Some(restart) => format!(
                "the runner restarted the Player by itself at sequence {} ({}), and game state was reset",
                restart.sequence, restart.reason
            ),
            None => "the Player runs the code it was started with".into(),
        };
        format!("What stands: {started}{}.", held(&mut notes))
    }

    /// What a caller reads first when its wait or recompile succeeded:
    /// what the answers after `after` brought that a caller who is told
    /// "applied" would not expect. The edit is applied either way. Empty
    /// when there is nothing of that kind.
    ///
    /// The sentence does not say more than is known. A note is a limit of
    /// the patch, and whether it matters depends on the game: a coroutine
    /// that was not running lost nothing. So it names the limit and leaves
    /// the restart to the caller, and it does not offer one for what a
    /// restart does not undo. Notes about lookups by name alone stay in
    /// the list and out of the sentence.
    pub(super) fn attention(&self, after: u64) -> String {
        let mut answered = self.clone();
        answered.since(after);
        let restarted = self
            .last_restart
            .as_ref()
            .filter(|restart| restart.sequence > after)
            .and_then(|restart| match restart.reason.as_str() {
                "hot_reload_failed" => Some(format!(
                    "The runner restarted the Player by itself and game state is reset: a code patch left the Player unusable ({}). The Player runs the saved code.",
                    restart.error.trim_end_matches('.')
                )),
                "hot_reload_unsupported" => {
                    let first = restart.unsupported.first();
                    Some(format!(
                        "The edit could not be applied to the running Player ({}), so the runner restarted it on the saved code, as onUnsupportedEdit restart asks; game state is reset.{}",
                        first.map_or("the report lists the reasons".into(), |reason| format!(
                            "{} [{}]",
                            reason.message.trim_end_matches('.'),
                            reason.code
                        )),
                        peers(&restart.unsupported)
                    ))
                }
                _ => None,
            });
        let first = answered
            .new_notes
            .iter()
            .find(|note| weight(&note.code).1 != Weight::Minor);
        let noted = first.map(|first| {
            let listed = answered.new_notes.len() as u64;
            let more = match answered.new_notes_count.saturating_sub(1) {
                0 => String::new(),
                more if answered.new_notes_count > listed => {
                    let report = self
                        .brought
                        .iter()
                        .rev()
                        .find(|answer| answer.sequence > after && !answer.report.is_empty())
                        .map_or(String::new(), |answer| format!(", {} all", answer.report));
                    format!(
                        " (and {more} more notes: hotReload.newNotes lists {listed} of them{report})"
                    )
                }
                more => format!(" (and {more} more in hotReload.newNotes)"),
            };
            let limit = format!("{} [{}]{more}", first.message.trim_end_matches('.'), first.code);
            // A restart of a Player of a network game also leaves the network session.
            let network = match first.code.as_str() {
                "spawned_objects_keep_state" => " (in a network game it also leaves the network session, and its peers see it disconnect)",
                _ => "",
            };
            match weight(&first.code).1 {
                Weight::Told => format!("The edit is applied and its code runs. Note: {limit}."),
                _ => format!(
                    "The edit is applied, with a limit that lasts until the Player restarts: {limit}. If what you are checking depends on it, hot_reload with action \"recompile\" restarts the Player on the saved code and resets game state{network}; otherwise go on."
                ),
            }
        });
        [restarted, noted]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The failure that reports the refusal that stands to a caller.
    /// `scene_after`: a scene or asset change that the Player rejected after
    /// this sequence is named with its own error.
    /// `last_sequence`: the session's, for the mark of the caller's next
    /// wait. A failed call returns no state to read it from.
    pub(super) fn refusal(
        &self,
        told: Refused,
        scene_after: u64,
        last_sequence: u64,
    ) -> (Error, Typed) {
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
            "The C# edit compiled but cannot be applied to the running Player: {reason}. The Player keeps the code it ran before this edit"
        );
        let next = format!(
            "Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.{}",
            peers(&self.unsupported)
        );
        // The Player's own words for a scene it would not load. A scene that
        // only lacks the refused code is applied by the recompile; one that
        // is wrong by itself fails the recompile with this text again.
        let scene = Some(&self.scene_rejected).filter(|(sequence, _)| *sequence > scene_after);
        let rejected = match scene {
            Some((_, error)) => format!(
                " A scene or asset change saved since was rejected by the running Player: {}. If it only needs the new code, the recompile applies it.",
                error.trim_end_matches('.')
            ),
            None => String::new(),
        };
        let message = match told {
            Refused::Edit => format!("{refused}.{rejected} {next}"),
            // What the caller's wait did bring, so that it is not taken for
            // nothing: of two edits one may be in the Player.
            Refused::After(update, sequence) => {
                let applied = match update {
                    "code-patched" => format!(
                        "; an earlier C# edit was applied at sequence {sequence} and is in the Player (hotReload.lastPatch)"
                    ),
                    "updated" => format!(
                        "; a scene or asset change confirmed at sequence {sequence} is applied"
                    ),
                    "code-updated" | "content-updated" | "ready" => format!(
                        "; the Player was restarted at sequence {sequence}, before this edit compiled"
                    ),
                    _ => String::new(),
                };
                format!("{refused}{applied}.{rejected} {next}")
            }
            Refused::Recompiled(id) => format!(
                "Recompile {id} restarted the Player, and a C# edit compiled after it cannot be applied to the running Player: {reason}.{rejected} Call hot_reload with action \"recompile\" and a new requestId to restart the Player on the new code; game state is reset."
            ),
        };
        let mut details = json!({
            "unsupported": self.unsupported,
            "unsupportedCount": self.unsupported_count,
            "report": self.report,
            "next": {"tool": "hot_reload", "arguments": {"action": "recompile"}},
            "lastSequence": last_sequence,
        });
        if let Some((sequence, error)) = scene {
            details["rejected"] = json!({"error": error, "sequence": sequence});
        }
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
        // What the next launch uses, also while hot reload is off for it.
        reply["onUnsupportedEdit"] = inner.launch.fallback().into();
        if inner.state.update_pending {
            reply["updatePending"] = true.into();
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
                    // No recompile applies a change that has left the
                    // prepared graph; the caller is told what does.
                    Some(rejection) if state.preparation_required => Err(failed(
                        state,
                        Error::new(format!(
                            "{rejection}. The change needs a new preparation of the project, which no recompile does: session_control(wait_update) or session_control(status) queues it and the session continues on it when it is done"
                        )),
                        None,
                    )),
                    Some(rejection) => Err(failed(state, Error::new(rejection), None)),
                    // The restart cleared every earlier refusal, so this one is
                    // of an edit compiled after it. Success would hide it.
                    None if state.hot_reload.full_recompile_required => {
                        let (error, typed) = state.hot_reload.refusal(
                            Refused::Recompiled(request_id),
                            sent.answered,
                            state.last_sequence,
                        );
                        Err(failed(state, error, Some(typed)))
                    }
                    None => {
                        // A C# edit that was patched in after the restart may
                        // have brought notes.
                        let mut state = state;
                        // The restart itself is what the caller asked for.
                        state.attention = state.hot_reload.attention(sent.answered);
                        state.hot_reload.since(sent.answered);
                        let mut inner = self.lock();
                        inner.told = inner.told.max(state.last_sequence);
                        Ok(state)
                    }
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

#[cfg(test)]
mod weight_tests {
    use super::*;

    /// Every note code of the patch builder has a weight here, so that a new
    /// one is ranked and worded by what it means instead of leading as an
    /// unknown code.
    #[test]
    fn every_note_of_the_patch_builder_is_weighed() {
        let codes = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/HotReloadPatcher/Codes.cs"
        ))
        .unwrap();
        let constants: std::collections::BTreeMap<&str, &str> = codes
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("public const string ")?;
                let (name, value) = rest.split_once(" = \"")?;
                Some((name, value.strip_suffix("\";")?))
            })
            .collect();
        let notes = codes.split("Notes =").nth(1).unwrap();
        let notes = notes.split("};").next().unwrap();
        let listed: Vec<&str> = notes
            .lines()
            .filter_map(|line| line.trim().strip_prefix('(')?.split(',').next())
            .collect();
        assert!(listed.len() > 10, "{listed:?}");
        for name in listed {
            let code = constants
                .get(name)
                .unwrap_or_else(|| panic!("Codes.cs has no constant {name}"));
            assert!(
                WEIGHTS.iter().any(|(known, _)| known == code),
                "the note {code} of the patch builder has no weight"
            );
        }
    }
}
