//! One debounce window of a watched session: what is compiled, whether the
//! running Player is patched, reloaded or restarted, and which events say
//! so, in which order. The work itself is behind [`Steps`], so the decisions
//! are tested without a compiler or a Player.
use super::{Event, err_text};
use crate::{
    cancel::Context,
    compiler::{self, CompileUpdate},
    hotreload::{
        CODE_FIELD_NOT_RESTORED, Failure, Fallback, Outcome, Patched, Reason, Refusal, Unchanged,
        bounded,
    },
};
use anyhow::Result;
use std::{
    collections::BTreeSet,
    time::{Instant, SystemTime},
};

/// Lists of an event hold this many entries; the report file has all.
pub(crate) const LIST_MAXIMUM: usize = 16;
/// The texts of a listed reason are cut to this many bytes in an event.
pub(crate) const TEXT_MAXIMUM: usize = 1024;

/// The source changes of one window and the command that came with them.
pub(crate) struct Window {
    pub paths: Vec<String>,
    /// The first change of the window, or the arrival of its command.
    pub changed_at: Instant,
    pub changed_wall: SystemTime,
    /// The request id of a `recompile` command.
    pub recompile: Option<String>,
}

/// What the compiler made of a window.
pub(crate) enum Compiled {
    /// No changed path can affect managed code; the compiler was not run.
    Unaffected,
    /// The sources were checked and nothing had to be rebuilt.
    Unchanged,
    /// A new compile, which no Player process has loaded yet.
    Updated,
}

/// A new Player process that became ready.
pub(crate) struct Restarted {
    /// `content-updated` with the members every restart has.
    pub event: Event,
    /// The compile the new process loaded, when the old one had not.
    pub compile: Option<CompileUpdate>,
}

/// What the scene part of a window did to the running Player.
pub(crate) enum Scene {
    /// Nothing the runtime reads changed.
    Unchanged(Event),
    Updated(Event),
    /// `fatal`: the Player's revision is uncertain, the run ends.
    Rejected {
        event: Event,
        fatal: Option<anyhow::Error>,
    },
}

/// The steps of a window that touch the compiler, the importer and the Player.
pub(crate) trait Steps {
    fn emit(&mut self, event: Event);
    /// Drops what the session cached of the changed paths.
    fn invalidate(&mut self, paths: &[String]) -> Result<()>;
    /// `None` checks the whole graph. A new compile becomes the pending one.
    fn compile(&mut self, ctx: &Context, paths: Option<&[String]>) -> Result<Compiled>;
    /// Whether a compile exists that no Player process has loaded.
    fn compile_pending(&self) -> bool;
    /// Tries to bring the running Player to the newest compile without a restart.
    fn patch_code(&mut self, ctx: &Context, window: &Window) -> Result<Outcome>;
    fn pid(&self) -> i32;
    /// The scene revision the Player confirmed last.
    fn revision(&self) -> String;
    /// Script types, behaviour layouts and bindings of the pending compile.
    /// The session then describes assemblies only a new process loads.
    fn bind_newest(&mut self, ctx: &Context) -> Result<()>;
    /// Publishes the next scene revision and content routes, for the newest
    /// compile's Player or for the running one. Returns whether the routes
    /// equal those of the running Player. `again`: the second call of a window.
    fn prepare(
        &mut self,
        ctx: &Context,
        window: &Window,
        newest: bool,
        again: bool,
    ) -> Result<bool>;
    /// Stops the Player and starts one on what `prepare` published last.
    fn restart(&mut self, ctx: &Context, window: &Window) -> Result<Restarted>;
    /// A `recompile` command that arrived while the Player restarted, when
    /// no source changed since: the new process answers it.
    fn merged_recompile(&mut self) -> Option<String>;
    /// Brings the running Player to the revision `prepare` published.
    fn scene(&mut self, ctx: &Context, window: &Window, started: Instant) -> Result<Scene>;
    /// The GUID of the script that declares the type, as the running
    /// Player's script types have it; `None` for a type no script names.
    fn script_guid(&self, _type_name: &str) -> Option<String> {
        None
    }
    /// Called after the events of a window were printed, also when it failed.
    fn window_done(&mut self, _window: &Window) {}
}

/// Why the next decision restarts the Player on the newest compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cause {
    /// A compile only a new process can load, or bindings that already
    /// describe it.
    Plain,
    /// An edit that cannot be patched, under the `restart` fallback.
    Unsupported(Refusal),
    /// A code patch that left the Player unusable.
    Failed(Failure),
}

/// The state of the watch loop that outlives a window.
pub(crate) struct Loop {
    /// Whether C# edits are patched into the running Player.
    pub hot_reload: bool,
    pub fallback: Fallback,
    /// The last compile or script fact step failed: the next compile checks
    /// the whole graph.
    managed_rejected: bool,
    /// Set until a restart happens, also across a window that failed first.
    restart: Option<Cause>,
    /// The newest compiled code is not in the Player and cannot be patched
    /// into it; only a restart applies it.
    refused: Option<Refusal>,
    /// The newest compiled code needed no patch, and the Player still does
    /// not take over all of it: the builder's notes, until the next compile
    /// or restart.
    noted: Option<Unchanged>,
    /// The notes of the code this Player process runs, as code, member and
    /// revision: those of its last patch, or of the last compile that
    /// needed none. A note of a later outcome that is not among them is
    /// about what that outcome changed.
    told: BTreeSet<(String, String, String)>,
    /// The fields the applied patch added that Unity would serialize, as the
    /// builder's notes. The Player does not fill them from a scene, so a
    /// scene update that brings a value for one is not all in the Player.
    unrestored: Vec<Reason>,
    /// The instance methods the applied patch added to engine objects, as
    /// the builder's notes. A UnityEvent or an animation event of the data
    /// looks them up by name in the loaded class and does not find them.
    unnamed: Vec<Reason>,
}

/// What a window says when its work is done.
#[derive(Default)]
struct Tail {
    /// The events of the window, in the order they are printed.
    events: Vec<Event>,
    /// `code-patched` is one of them.
    patched: bool,
    /// The refusal that stands is the last of them.
    refusal: bool,
    /// The notes that the compile of this window brought and the outcome
    /// before did not have, when it needed no patch.
    fresh: Vec<Reason>,
}

/// The note of a method that data names by a string.
const CODE_METHOD_NAMED: &str = "added_method_named_by_string";
/// The builder's notes of an added instance method of an engine object
/// that no string of the code names.
const CODES_METHOD_ADDED: [&str; 2] = [
    "added_method_invisible_by_name",
    "added_method_computed_name",
];

/// The documents of a Unity YAML text: each `--- !u!` line begins one, and
/// what stands before the first is one too.
fn documents(text: &str) -> impl Iterator<Item = &str> {
    let mut starts: Vec<usize> = vec![0];
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if offset > 0 && line.starts_with("--- !u!") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts.push(text.len());
    let pieces: Vec<&str> = starts
        .windows(2)
        .map(|pair| &text[pair[0]..pair[1]])
        .collect();
    pieces.into_iter()
}
/// The GUID of the script a document's `m_Script` names, in flow style
/// (`m_Script: {fileID: 11500000, guid: ..., type: 3}`) or in block style
/// (`guid: ...` on a line of its own below `m_Script:`).
fn document_script(document: &str) -> Option<&str> {
    let mut lines = document.lines();
    let line = lines.find(|line| line.trim_start().starts_with("m_Script:"))?;
    let indent = line.len() - line.trim_start().len();
    let rest = match line.find("guid:") {
        Some(at) => &line[at + 5..],
        None => {
            let below = lines
                .take_while(|line| line.len() - line.trim_start().len() > indent)
                .find(|line| line.trim_start().starts_with("guid:"))?;
            &below[below.find("guid:")? + 5..]
        }
    };
    let guid = rest.trim_start();
    Some(&guid[..guid.find([',', '}', ' ']).unwrap_or(guid.len())])
}
/// The key of a YAML line, also of a list item.
fn key_of(line: &str) -> Option<&str> {
    let line = line.trim_start().trim_start_matches("- ");
    let colon = line.find(':')?;
    let key = &line[..colon];
    (!key.is_empty() && !key.contains(char::is_whitespace)).then_some(key)
}
/// Whether a prefab instance's override in the document sets the field:
/// its property path has the field as one of its parts.
fn overridden(document: &str, field: &str) -> bool {
    document.lines().any(|line| {
        let line = line.trim_start().trim_start_matches("- ");
        line.strip_prefix("propertyPath:")
            .is_some_and(|path| path.trim().split('.').any(|part| part == field))
    })
}

fn capped<T: Clone>(list: &[T]) -> Vec<T> {
    list.iter().take(LIST_MAXIMUM).cloned().collect()
}
/// The first reasons of a list, small enough for an event line.
fn listed(reasons: &[Reason]) -> Vec<Reason> {
    let cut = |text: &str| bounded(text, TEXT_MAXIMUM).to_string();
    reasons
        .iter()
        .take(LIST_MAXIMUM)
        .map(|reason| {
            Reason::new(
                &cut(&reason.code),
                &cut(&reason.member),
                &cut(&reason.message),
            )
        })
        .collect()
}

/// `update-rejected` of a step that failed before it reached the Player.
pub(crate) fn rejected_event(
    error: &anyhow::Error,
    paths: &[String],
    request: Option<&str>,
) -> Event {
    let text = err_text(error);
    let mut event = Event::new("update-rejected")
        .with("error", &text)
        .with("paths", paths);
    // The Player keeps its old code until the project is prepared again.
    if compiler::requires_new_graph(&text) {
        event = event.with("preparationRequired", &true);
    }
    // The answer to the command whose compile check or restart this was.
    if let Some(id) = request {
        event = event.with("requestId", id);
    }
    event
}

/// `update-rejected` of a C# edit that compiled and cannot be patched.
pub(crate) fn refusal_event(refusal: &Refusal, paths: &[String]) -> Event {
    Event::new("update-rejected")
        .with("error", &refusal.summary())
        .with("paths", paths)
        .with("fullRecompileRequired", &true)
        .with("unsupported", &listed(&refusal.reasons))
        .with("unsupportedCount", &refusal.reasons.len())
        .with("report", &refusal.report)
}

/// `fresh`: the notes of the patch that the code before it did not have.
/// They lead `notes`, so that the limit of a list cuts what stood before.
pub(crate) fn patched_event(
    patch: &Patched,
    fresh: &[Reason],
    window: &Window,
    pid: i32,
    revision: &str,
    compile_seconds: f64,
    patch_seconds: f64,
) -> Event {
    let mut notes = fresh.to_vec();
    notes.extend(
        patch
            .notes
            .iter()
            .filter(|note| !fresh.contains(note))
            .cloned(),
    );
    Event::new("code-patched")
        .with("paths", &window.paths)
        .with("pid", &pid)
        .with("revision", revision)
        .with("codeGeneration", &patch.generation)
        .with("assemblies", &capped(&patch.assemblies))
        .with("patchedMethods", &patch.patched_methods)
        .with("addedMethods", &patch.added_methods)
        .with("addedFields", &patch.added_fields)
        .with("restoredMethods", &patch.restored_methods)
        .with("members", &capped(&patch.members))
        .with("notes", &listed(&notes))
        .with("notesCount", &patch.notes.len())
        .with("newNotes", &listed(fresh))
        .with("newNotesCount", &fresh.len())
        .seconds("compileSeconds", compile_seconds)
        .seconds("patchSeconds", patch_seconds)
        .seconds(
            "sourceToAckSeconds",
            window.changed_at.elapsed().as_secs_f64(),
        )
        .with("report", &patch.report)
}

/// `unchanged`: nothing the runtime reads changed with this window. It names
/// the Player and the scene revision that stand.
pub(crate) fn unchanged_event(
    window: &Window,
    prepare_seconds: f64,
    pid: i32,
    revision: &str,
) -> Event {
    Event::new("unchanged")
        .with("paths", &window.paths)
        .seconds("prepareSeconds", prepare_seconds)
        .with("pid", &pid)
        .with("revision", revision)
}

/// `updated`: the Player acknowledged the scene revision of this window.
/// `ack` is its acknowledgement, as JSON.
pub(crate) fn updated_event<C: serde::Serialize>(
    window: &Window,
    prepare_seconds: f64,
    started: Instant,
    ack: String,
    cache: &C,
) -> Event {
    Event::new("updated")
        .with("paths", &window.paths)
        .seconds("prepareSeconds", prepare_seconds)
        .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
        .seconds(
            "sourceToAckSeconds",
            window.changed_at.elapsed().as_secs_f64(),
        )
        .raw("ack", ack)
        .with("cache", cache)
}

/// `update-rejected` of a scene revision the Player refused or did not
/// acknowledge; `ack` is what it wrote, or `null`.
pub(crate) fn scene_rejected_event(error: &str, ack: String) -> Event {
    Event::new("update-rejected")
        .with("error", error)
        .raw("ack", ack)
}

/// `content-updated`: a new Player process became ready. `restart_event`
/// makes the event of the window of it.
pub(crate) fn ready_event(
    window: &Window,
    manifest: &str,
    routes: &str,
    pid: i32,
    report: &str,
) -> Event {
    Event::new("content-updated")
        .with("paths", &window.paths)
        .with("manifest", manifest)
        .with("contentRoutes", routes)
        .seconds(
            "sourceToReadySeconds",
            window.changed_at.elapsed().as_secs_f64(),
        )
        .with("pid", &pid)
        .with("report", report)
}

/// `content-updated`, or `code-updated` when the new process loaded a newer
/// compile or a command asked for it, with the reason hot reload gave.
pub(crate) fn restart_event(
    restarted: Restarted,
    cause: Option<Cause>,
    request: Option<&str>,
) -> Event {
    let mut event = restarted.event;
    if let Some(compile) = &restarted.compile {
        event = event
            .with("event", "code-updated")
            .with("sourceHashes", &compile.source_hashes)
            .with("compiledAssemblies", &compile.compiled_assemblies)
            .with("declarationChanged", &compile.declaration_changed)
            .with("compileSeconds", &compile.elapsed_seconds);
    }
    match (request, cause) {
        // A requested restart is answered as one, whatever else caused it.
        (Some(id), _) => event
            .with("event", "code-updated")
            .with("reason", "requested")
            .with("requestId", id),
        // `report` is the Player's own report on a readiness event.
        (None, Some(Cause::Unsupported(refusal))) => event
            .with("reason", "hot_reload_unsupported")
            .with("unsupported", &listed(&refusal.reasons))
            .with("unsupportedCount", &refusal.reasons.len())
            .with("hotReloadReport", &refusal.report),
        (None, Some(Cause::Failed(failure))) => event
            .with("reason", "hot_reload_failed")
            .with("error", &failure.error)
            .with("hotReloadReport", &failure.report),
        (None, Some(Cause::Plain) | None) => event,
    }
}

impl Loop {
    pub fn new(hot_reload: bool, fallback: Fallback) -> Self {
        Self {
            hot_reload,
            fallback,
            managed_rejected: false,
            restart: None,
            refused: None,
            noted: None,
            told: BTreeSet::new(),
            unrestored: Vec::new(),
            unnamed: Vec::new(),
        }
    }

    /// The notes for a scene update the Player took over: each field that
    /// the applied patch added and that one of the changed files holds a
    /// value for, and each method it added that the data names. The Player
    /// rebuilt its objects from that file: the field has what its
    /// initializer gave it, and a UnityEvent or an animation event that
    /// names the method finds nothing. A caller that is told `updated` and
    /// nothing else takes the saved value or call for applied. The files are
    /// Unity's YAML. A value of the field is a key of a document of the
    /// declaring type's script (any document when no script declares the
    /// type, a [Serializable] class held by another), or the property path
    /// of a prefab instance's override (`propertyPath: field`, also as a part
    /// of a longer path such as `tuning.field` or `field.Array.data[0]`).
    fn unfilled(&self, paths: &[String], script: &dyn Fn(&str) -> Option<String>) -> Vec<Reason> {
        if self.unrestored.is_empty() && self.unnamed.is_empty() {
            return Vec::new();
        }
        let read = |extensions: &[&str]| -> Vec<(String, String)> {
            let mut files: Vec<&String> = paths
                .iter()
                .filter(|path| extensions.iter().any(|extension| path.ends_with(extension)))
                .collect();
            files.sort();
            files.dedup();
            files
                .into_iter()
                .filter_map(|path| {
                    Some((
                        path.clone(),
                        String::from_utf8(std::fs::read(path).ok()?).ok()?,
                    ))
                })
                .collect()
        };
        let mut notes = Vec::new();
        if !self.unrestored.is_empty() {
            let texts = read(&[".unity", ".prefab", ".asset", ".controller"]);
            for note in &self.unrestored {
                let Some((declaring, field)) = note.member.rsplit_once("::") else {
                    continue;
                };
                let guid = script(declaring);
                let holder = texts.iter().find(|(_, text)| {
                    !field.is_empty()
                        && documents(text).any(|document| {
                            let own = match (&guid, document_script(document)) {
                                (Some(guid), Some(script)) => guid == script,
                                (Some(_), None) => false,
                                (None, _) => true,
                            };
                            (own && document.lines().any(|line| key_of(line) == Some(field)))
                                || overridden(document, field)
                        })
                });
                if let Some((path, _)) = holder {
                    notes.push(Reason::new(
                        CODE_FIELD_NOT_RESTORED,
                        &note.member,
                        &format!(
                            "{path} holds a value for the added field {}, which the Player does not read from scenes, prefabs or assets until a restart; its objects keep what the field's initializer gave them.",
                            note.member
                        ),
                    ));
                }
            }
        }
        if !self.unnamed.is_empty() {
            let texts = read(&[
                ".unity",
                ".prefab",
                ".asset",
                ".controller",
                ".anim",
                ".meta",
            ]);
            for note in &self.unnamed {
                let Some(name) = note
                    .member
                    .rsplit_once("::")
                    .and_then(|(_, method)| method.split_once('(').map(|(name, _)| name))
                else {
                    continue;
                };
                let named = texts.iter().find_map(|(path, text)| {
                    text.lines().find_map(|line| {
                        let line = line.trim_start().trim_start_matches("- ");
                        ["m_MethodName", "functionName"]
                            .into_iter()
                            .find(|key| {
                                line.strip_prefix(key)
                                    .and_then(|rest| rest.strip_prefix(':'))
                                    .is_some_and(|value| value.trim() == name)
                            })
                            .map(|key| (path, key))
                    })
                });
                if let Some((path, key)) = named {
                    notes.push(Reason::new(
                        CODE_METHOD_NAMED,
                        &note.member,
                        &format!(
                            "{path} names the added method {} ({key}); a UnityEvent or an animation event looks it up by name in the loaded class and does not find it until a full recompile, so that call does nothing.",
                            note.member
                        ),
                    ));
                }
            }
        }
        notes
    }

    /// Takes the notes of an outcome that is in the Player now and returns
    /// those the outcome before it did not have.
    fn fresh(&mut self, notes: &[Reason]) -> Vec<Reason> {
        let key = |note: &Reason| {
            (
                note.code.clone(),
                note.member.clone(),
                note.revision.clone(),
            )
        };
        let fresh = notes
            .iter()
            .filter(|note| !self.told.contains(&key(note)))
            .cloned()
            .collect();
        self.told = notes.iter().map(key).collect();
        fresh
    }

    /// Why the newest compiled code waits for a full recompile, when it does.
    #[cfg(test)]
    pub fn refused(&self) -> Option<&Refusal> {
        self.refused.as_ref()
    }
    /// Why the next window that gets that far restarts the Player.
    #[cfg(test)]
    pub fn restart(&self) -> Option<&Cause> {
        self.restart.as_ref()
    }

    /// States the Player's code on a scene confirmation. A scene update
    /// proves nothing about it, and a reader must not take one for the
    /// arrival of a refused edit. The notes of a compile that needed no
    /// patch are repeated for as long as they stand.
    /// `fresh`: those of the notes that the compile of this window brought.
    fn stated(&self, event: Event, fresh: &[Reason]) -> Event {
        if !self.hot_reload {
            return event;
        }
        let mut event = event.with("fullRecompileRequired", &self.refused.is_some());
        if let Some(noted) = &self.noted {
            event = event
                .with("notes", &listed(&noted.notes))
                .with("notesCount", &noted.notes.len())
                .with("hotReloadReport", &noted.report);
        }
        match fresh.is_empty() {
            true => event,
            false => event
                .with("newNotes", &listed(fresh))
                .with("newNotesCount", &fresh.len()),
        }
    }

    /// States the code on the rejection of a scene part that left the
    /// Player running. A scene that uses a type only the refused code has
    /// fails for that reason alone, and a reader that stops at this event
    /// must be led to the recompile, with the reasons that stand. Without a
    /// refusal it says so too: a window that took one back may end here.
    /// `part` tells this rejection from the refusal itself: its `error` is
    /// the scene's own.
    fn blocked(&self, event: Event, fresh: &[Reason]) -> Event {
        let event = event.with("part", "scene");
        match &self.refused {
            Some(refusal) if self.hot_reload => event
                .with("fullRecompileRequired", &true)
                .with("unsupported", &listed(&refusal.reasons))
                .with("unsupportedCount", &refusal.reasons.len())
                .with("report", &refusal.report),
            _ => self.stated(event, fresh),
        }
    }

    /// Handles one window. An error ends the run; everything else is an
    /// event and leaves the session running. The events are printed when
    /// the work of the window is done, so that none is read while a part of
    /// the same save is still on its way into the Player. `pending` marks an
    /// event that is not the last word: another one of this window follows
    /// it, or source changes arrived meanwhile and the next window answers.
    /// `queued` says whether such changes wait when the events are printed.
    pub fn window<S: Steps>(
        &mut self,
        ctx: &Context,
        steps: &mut S,
        window: &Window,
        queued: &dyn Fn() -> bool,
    ) -> Result<()> {
        let mut tail = Tail::default();
        let outcome = self.apply(ctx, steps, window, &mut tail);
        // The refusal is the last terminal event of its window, also after a
        // scene part that was applied or rejected.
        if outcome.is_ok()
            && tail.refusal
            && let Some(refusal) = &self.refused
        {
            tail.events.push(refusal_event(refusal, &window.paths));
        }
        let queued = queued();
        let last = tail.events.len().saturating_sub(1);
        for (index, event) in tail.events.into_iter().enumerate() {
            steps.emit(match index < last || queued {
                true => event.with("pending", &true),
                false => event,
            });
        }
        outcome
    }

    fn apply<S: Steps>(
        &mut self,
        ctx: &Context,
        steps: &mut S,
        window: &Window,
        tail: &mut Tail,
    ) -> Result<()> {
        let paths = &window.paths[..];
        let request = window.recompile.as_deref();
        let reject = |tail: &mut Tail, error: &anyhow::Error| {
            tail.events.push(rejected_event(error, paths, request));
        };
        if let Err(error) = steps.invalidate(paths) {
            reject(tail, &error);
            return Ok(());
        }
        let started = Instant::now();
        // A recompile checks every source, as the compile after a rejection does.
        let compile_paths = (!self.managed_rejected && request.is_none()).then_some(paths);
        let compiled = match steps.compile(ctx, compile_paths) {
            Ok(compiled) => compiled,
            Err(error) => {
                self.managed_rejected = true;
                reject(tail, &error);
                return Ok(());
            }
        };
        // The whole step as the window waited for it: the compiler's own
        // time leaves out the source and revision hashes around it.
        let seconds = started.elapsed().as_secs_f64();
        self.managed_rejected = false;
        match compiled {
            // A window that restarts anyway needs no patch.
            Compiled::Updated if self.hot_reload && request.is_none() && self.restart.is_none() => {
                let patch_started = Instant::now();
                let outcome = match steps.patch_code(ctx, window) {
                    Ok(outcome) => outcome,
                    // The run is ending; the caller sees the context.
                    Err(_) if ctx.error().is_some() => return Ok(()),
                    Err(error) => return Err(error),
                };
                match outcome {
                    Outcome::Patched(patch) => {
                        (self.refused, self.noted) = (None, None);
                        tail.patched = true;
                        self.unrestored = patch
                            .notes
                            .iter()
                            .filter(|note| note.code == CODE_FIELD_NOT_RESTORED)
                            .cloned()
                            .collect();
                        self.unnamed = patch
                            .notes
                            .iter()
                            .filter(|note| CODES_METHOD_ADDED.contains(&note.code.as_str()))
                            .cloned()
                            .collect();
                        let fresh = self.fresh(&patch.notes);
                        let (pid, revision) = (steps.pid(), steps.revision());
                        tail.events.push(patched_event(
                            &patch,
                            &fresh,
                            window,
                            pid,
                            &revision,
                            seconds,
                            patch_started.elapsed().as_secs_f64(),
                        ));
                    }
                    Outcome::Unchanged(unchanged) => {
                        self.refused = None;
                        // No redirect is live: the Player runs what it loaded.
                        self.unrestored.clear();
                        self.unnamed.clear();
                        tail.fresh = self.fresh(&unchanged.notes);
                        self.noted = (!unchanged.notes.is_empty()).then_some(unchanged);
                    }
                    Outcome::Unsupported(refusal) | Outcome::ApplyFailed(refusal) => {
                        // Nothing of the newest compile is in the Player.
                        self.noted = None;
                        match self.fallback {
                            Fallback::Restart => self.restart = Some(Cause::Unsupported(refusal)),
                            Fallback::Report => {
                                self.refused = Some(refusal);
                                tail.refusal = true;
                            }
                        }
                    }
                    Outcome::RestartRequired(failure)
                    | Outcome::PlayerExited(failure)
                    | Outcome::TimedOut(failure) => self.restart = Some(Cause::Failed(failure)),
                }
            }
            // Without hot reload only a new process runs new code.
            Compiled::Updated if !self.hot_reload => {
                self.restart.get_or_insert(Cause::Plain);
            }
            // The sources were compiled again and still are what was refused.
            Compiled::Unchanged if self.refused.is_some() => tail.refusal = true,
            _ => {}
        }
        // A scene manifest and the content routes name the assemblies of one
        // Player process: those the running Player loaded until this window
        // is known to restart it, the newest ones from then on.
        let mut newest = self.restart.is_some() || request.is_some();
        let mut again = false;
        let routes_equal = loop {
            if newest && steps.compile_pending() {
                // From here on only a restart makes Player and session agree,
                // also when a later step of this window fails.
                self.restart.get_or_insert(Cause::Plain);
                if let Err(error) = steps.bind_newest(ctx) {
                    self.managed_rejected = true;
                    reject(tail, &error);
                    return Ok(());
                }
            }
            let equal = match steps.prepare(ctx, window, newest, again) {
                Ok(equal) => equal,
                // What was prepared for the running Player is a scene part.
                // A command prepares for a new process, so its answer stays
                // plain: an event that states a refusal answers none.
                Err(error) if !newest => {
                    let event = self.blocked(rejected_event(&error, paths, request), &tail.fresh);
                    tail.events.push(event);
                    return Ok(());
                }
                Err(error) => {
                    reject(tail, &error);
                    return Ok(());
                }
            };
            // Content routes are fixed for one Player lifetime. The process
            // that replaces this one loads the newest compile.
            if !equal && !newest && steps.compile_pending() {
                (newest, again) = (true, true);
                continue;
            }
            break equal;
        };
        if self.restart.is_some() || request.is_some() || !routes_equal {
            // Mono cannot unload or replace an already loaded assembly identity.
            let restarted = steps.restart(ctx, window)?;
            let cause = self.restart.take();
            (self.refused, self.noted) = (None, None);
            self.told.clear();
            self.unrestored.clear();
            self.unnamed.clear();
            tail.refusal = false;
            let merged = match request {
                Some(_) => None,
                None => steps.merged_recompile(),
            };
            tail.events.push(restart_event(
                restarted,
                cause,
                request.or(merged.as_deref()),
            ));
            return Ok(());
        }
        match steps.scene(ctx, window, started)? {
            // The code event of this window already is its confirmation.
            Scene::Unchanged(_) if tail.patched || tail.refusal => {}
            Scene::Unchanged(event) => tail.events.push(self.stated(event, &tail.fresh)),
            Scene::Updated(event) => {
                let mut fresh = tail.fresh.clone();
                fresh.extend(self.unfilled(paths, &|name| steps.script_guid(name)));
                tail.events.push(self.stated(event, &fresh));
            }
            Scene::Rejected { event, fatal } => {
                tail.events.push(self.blocked(event, &tail.fresh));
                if let Some(error) = fatal {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}
