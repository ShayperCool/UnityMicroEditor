//! One debounce window of a watched session: what is compiled, whether the
//! running Player is patched, reloaded or restarted, and which events say
//! so, in which order. The work itself is behind [`Steps`], so the decisions
//! are tested without a compiler or a Player.
use super::{Event, err_text};
use crate::{
    cancel::Context,
    compiler::{self, CompileUpdate},
    hotreload::{Failure, Fallback, Outcome, Patched, Reason, Refusal, Unchanged, bounded},
};
use anyhow::Result;
use std::time::{Instant, SystemTime};

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
    Updated { seconds: f64 },
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
    /// Whether source changes arrived that this window did not take: the
    /// next window handles them.
    fn changes_pending(&self) -> bool;
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
        .map(|reason| Reason {
            code: cut(&reason.code),
            member: cut(&reason.member),
            message: cut(&reason.message),
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

pub(crate) fn patched_event(
    patch: &Patched,
    window: &Window,
    pid: i32,
    revision: &str,
    compile_seconds: f64,
    patch_seconds: f64,
) -> Event {
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
        .with("notes", &listed(&patch.notes))
        .with("notesCount", &patch.notes.len())
        .seconds("compileSeconds", compile_seconds)
        .seconds("patchSeconds", patch_seconds)
        .seconds(
            "sourceToAckSeconds",
            window.changed_at.elapsed().as_secs_f64(),
        )
        .with("report", &patch.report)
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
        }
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
    fn stated(&self, event: Event) -> Event {
        if !self.hot_reload {
            return event;
        }
        let event = event.with("fullRecompileRequired", &self.refused.is_some());
        match &self.noted {
            Some(noted) => event
                .with("notes", &listed(&noted.notes))
                .with("notesCount", &noted.notes.len())
                .with("hotReloadReport", &noted.report),
            None => event,
        }
    }

    /// States the code on the rejection of a scene part that left the
    /// Player running. A scene that uses a type only the refused code has
    /// fails for that reason alone, and a reader that stops at this event
    /// must be led to the recompile, with the reasons that stand. Without a
    /// refusal it says so too: a window that took one back may end here.
    fn blocked(&self, event: Event) -> Event {
        match &self.refused {
            Some(refusal) if self.hot_reload => event
                .with("fullRecompileRequired", &true)
                .with("unsupported", &listed(&refusal.reasons))
                .with("unsupportedCount", &refusal.reasons.len())
                .with("report", &refusal.report),
            _ => self.stated(event),
        }
    }

    /// Handles one window. An error ends the run; everything else is an
    /// event and leaves the session running. The events are printed when
    /// the work of the window is done, so that none is read while a part of
    /// the same save is still on its way into the Player. `pending` marks an
    /// event that is not the last word: another one of this window follows
    /// it, or source changes arrived meanwhile and the next window answers.
    pub fn window<S: Steps>(
        &mut self,
        ctx: &Context,
        steps: &mut S,
        window: &Window,
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
        let queued = steps.changes_pending();
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
        self.managed_rejected = false;
        match compiled {
            // A window that restarts anyway needs no patch.
            Compiled::Updated { seconds }
                if self.hot_reload && request.is_none() && self.restart.is_none() =>
            {
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
                        let (pid, revision) = (steps.pid(), steps.revision());
                        tail.events.push(patched_event(
                            &patch,
                            window,
                            pid,
                            &revision,
                            seconds,
                            patch_started.elapsed().as_secs_f64(),
                        ));
                    }
                    Outcome::Unchanged(unchanged) => {
                        self.refused = None;
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
            Compiled::Updated { .. } if !self.hot_reload => {
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
                    let event = self.blocked(rejected_event(&error, paths, request));
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
            Scene::Unchanged(event) | Scene::Updated(event) => {
                tail.events.push(self.stated(event));
            }
            Scene::Rejected { event, fatal } => {
                tail.events.push(self.blocked(event));
                if let Some(error) = fatal {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}
