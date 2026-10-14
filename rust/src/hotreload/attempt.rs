//! The code of one Player process and one attempt to patch it: which
//! assemblies differ from what the process loaded, the patch builder, the
//! descriptor, the request to the Player and its acknowledgement.
use super::{
    tools::Applier,
    wire::{
        Ack, BuilderAssembly, BuilderRequest, BuilderResult, Descriptor, RUNTIME_ASSEMBLY, Reason,
        ReloadRequest, VERDICT_PATCH, VERDICT_UNSUPPORTED,
    },
};
use crate::{
    cancel::{Context, Source, select},
    compiler::Compiler,
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, random_id},
};
use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::value::RawValue;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How long the Player gets to answer a code reload request. The call is
/// synchronous on its main thread and takes milliseconds, plus the game's
/// own callbacks; this is the limit of a scene reload.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(20);

/// The reason code of a patch builder that failed or answered unusably.
pub const CODE_BUILDER_FAILED: &str = "patch_builder_failed";
/// The reason code of a patch the Player refused before writing anything.
pub const CODE_APPLY_FAILED: &str = "apply_failed";
/// The note code of game callbacks that threw or could not be called.
pub const CODE_CALLBACK_FAILED: &str = "callback_failed";
/// The builder's note code of an added field that Unity would serialize:
/// the Player does not fill it from scenes, prefabs or assets.
pub const CODE_FIELD_NOT_RESTORED: &str = "serialized_field_not_restored";

/// One game assembly, as a Player loaded it or as the newest compile has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assembly {
    pub name: String,
    pub path: String,
    pub sha256: String,
}

/// The game assemblies of the compiler graph as a Player started from
/// `player` loads them.
pub fn loaded_assemblies(compiler: &Compiler, player: &str) -> Result<Vec<Assembly>> {
    let managed = crate::compiler::player_managed(player);
    let mut loaded = Vec::new();
    for assembly in &compiler.manifest.assemblies {
        let path = sysutil::join(&[&managed, &format!("{}.dll", assembly.name)]);
        loaded.push(Assembly {
            name: assembly.name.clone(),
            sha256: file_hash(&path)?,
            path,
        });
    }
    Ok(loaded)
}

/// The game assemblies of the newest compile the compiler accepted.
pub fn newest_assemblies(compiler: &Compiler) -> Result<Vec<Assembly>> {
    let mut newest = Vec::new();
    for assembly in &compiler.manifest.assemblies {
        let path = compiler.current_dlls.at(&assembly.name).to_string();
        let sha256 = match compiler.accepted_dll_hashes.at(&assembly.name) {
            "" => file_hash(&path)?,
            hash => hash.to_string(),
        };
        newest.push(Assembly {
            name: assembly.name.clone(),
            path,
            sha256,
        });
    }
    Ok(newest)
}

/// `text` cut to at most `limit` bytes, at a character boundary.
pub fn bounded(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Why the newest compiled code is not in the Player: the builder's reasons
/// or the Player's refusal, and the file that holds all of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refusal {
    pub reasons: Vec<Reason>,
    pub report: String,
}
impl Refusal {
    /// One sentence: the count and the first reason. It never reads as a
    /// managed graph error, which would ask for a new preparation.
    pub fn summary(&self) -> String {
        let count = self.reasons.len();
        let plural = if count == 1 { "" } else { "s" };
        let head = format!(
            "hot reload cannot apply the C# edit to the running Player ({count} reason{plural})"
        );
        let Some(first) = self.reasons.first() else {
            return head;
        };
        // The output of a failed builder has many lines; the report has all.
        let message = bounded(first.message.lines().next().unwrap_or(""), 512);
        let detail = [first.member.as_str(), message.trim_end_matches('.')]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(": ");
        let text = format!("{head}; first: [{}] {detail}", first.code);
        match crate::compiler::requires_new_graph(&text) {
            true => format!("{head}; first: [{}]", first.code),
            false => text,
        }
    }
}

/// An applied code patch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patched {
    pub generation: u64,
    /// The assemblies that carry a patch; empty when the loaded code was restored.
    pub assemblies: Vec<String>,
    pub patched_methods: u64,
    pub added_methods: u64,
    pub added_fields: u64,
    pub restored_methods: u64,
    pub members: Vec<String>,
    pub notes: Vec<Reason>,
    pub report: String,
}

/// A compile the Player needs no patch for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unchanged {
    /// What the Player still does not take over until it restarts, in the
    /// builder's words: a changed accessibility, a removed start-up method.
    pub notes: Vec<Reason>,
    /// Empty when no byte differed and the builder did not run.
    pub report: String,
}

/// A patch that left the Player unusable, or a Player that did not answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Failure {
    pub error: String,
    pub report: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The Player runs the newest compiled code.
    Patched(Patched),
    /// The newest compile needs no patch in the Player, which runs it but
    /// for the notes; nothing was sent to it.
    Unchanged(Unchanged),
    /// The edit cannot be patched. Nothing was sent to the Player.
    Unsupported(Refusal),
    /// The Player refused the patch before it wrote anything and keeps
    /// running the generation it had.
    ApplyFailed(Refusal),
    /// The acknowledgement says the Player's code is in an unknown state.
    RestartRequired(Failure),
    PlayerExited(Failure),
    /// No acknowledgement in time: the Player's state is unknown.
    TimedOut(Failure),
}

/// How one attempt reaches the patch builder and the Player.
pub struct Attempt<'a> {
    /// The game assemblies of the newest compile, all of them.
    pub current: &'a [Assembly],
    /// Runs the patch builder on a request file and returns its output.
    pub build: &'a dyn Fn(&Context, &str) -> Result<Vec<u8>>,
    /// Whether the Player process has exited.
    pub exited: &'a dyn Fn() -> bool,
    pub pid: i32,
    /// The wall clock time of the source change that led to this attempt.
    pub source_changed: SystemTime,
    pub timeout: Duration,
}

/// The code of one Player process: what it loaded at its start and what was
/// patched into it since. A new process gets a new value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCode {
    run_directory: String,
    applier: Applier,
    /// The game assemblies inside the Player directory the process started
    /// from. They stay on disk for as long as it runs: nothing in a session
    /// removes a Player directory or a compiled revision.
    loaded: Vec<Assembly>,
    /// Never reused in one process, also not after a refused or failed attempt.
    next_generation: u64,
    /// The generation the Player runs; 0 is the code it loaded.
    applied_generation: u64,
    /// The assemblies the applied generation patches; empty when no redirect is live.
    patched: Vec<String>,
    /// The builder result of the applied generation.
    applied_result: String,
    /// The newest compile cannot be patched into this process.
    refused: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: i64,
    generation: u64,
    outcome: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    error: &'a str,
    /// The patch builder's result, as it printed it.
    result: Option<&'a RawValue>,
    /// The Player's acknowledgement, as it wrote it.
    ack: Option<&'a RawValue>,
}

fn unix_milliseconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}
fn raw(data: &[u8]) -> Option<Box<RawValue>> {
    RawValue::from_string(String::from_utf8(data.to_vec()).ok()?.trim().to_string()).ok()
}
fn write(path: &str, data: &[u8]) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(data))
        .map_err(|error| anyhow!(sysutil::Error::os("write", path, &error)))
}

/// What ended the wait for an acknowledgement.
#[derive(Debug)]
pub enum Answer {
    /// The acknowledgement of the request, decoded and as written.
    Ack(Box<Ack>, Vec<u8>),
    /// A file with the request's id that is not an acknowledgement.
    Unreadable(String, Vec<u8>),
    Exited,
    TimedOut,
}

/// Polls every 10 ms for the acknowledgement of `id`. When several outcomes
/// are due at once the order is cancellation, Player exit, acknowledgement,
/// timeout.
pub fn wait_code_ack(
    ctx: &Context,
    path: &str,
    id: &str,
    timeout: Duration,
    exited: &dyn Fn() -> bool,
) -> Result<Answer> {
    const TICK: Duration = Duration::from_millis(10);
    let limit = Instant::now() + timeout;
    loop {
        ctx.err()?;
        // A Player that crashes on a bad redirect never answers.
        if exited() {
            return Ok(Answer::Exited);
        }
        if let Ok(data) = fs::read(path)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data)
            && value.get("requestId").and_then(serde_json::Value::as_str) == Some(id)
        {
            return Ok(match serde_json::from_value::<Ack>(value) {
                Ok(ack) => Answer::Ack(Box::new(ack), data),
                Err(error) => Answer::Unreadable(error.to_string(), data),
            });
        }
        let now = Instant::now();
        if now >= limit {
            return Ok(Answer::TimedOut);
        }
        select(&[Source::Context(ctx)], Some(TICK.min(limit - now)));
    }
}

impl PlayerCode {
    pub const REQUEST: &'static str = "code-reload-request.json";

    /// The path of the request file a Player started in `run_directory` polls.
    pub fn request_path(run_directory: &str) -> String {
        sysutil::join(&[run_directory, Self::REQUEST])
    }

    /// The state of a Player process that was started in `run_directory`
    /// from the assemblies `loaded`, with the applier it is patched with for
    /// as long as it lives.
    pub fn new(run_directory: &str, applier: Applier, loaded: Vec<Assembly>) -> Self {
        Self {
            run_directory: run_directory.into(),
            applier,
            loaded,
            next_generation: 1,
            applied_generation: 0,
            patched: Vec::new(),
            applied_result: String::new(),
            refused: false,
        }
    }

    pub fn applier(&self) -> &Applier {
        &self.applier
    }
    pub fn loaded(&self) -> &[Assembly] {
        &self.loaded
    }
    /// The generation the Player runs; 0 before the first applied patch.
    pub fn applied_generation(&self) -> u64 {
        self.applied_generation
    }
    /// Whether a method of the Player is redirected to patch code.
    pub fn redirected(&self) -> bool {
        !self.patched.is_empty()
    }
    /// Whether the newest compiled code could not be patched into the Player.
    pub fn refused(&self) -> bool {
        self.refused
    }
    /// The builder result of the generation the Player runs; empty when it
    /// runs the code it loaded.
    pub fn applied_result(&self) -> &str {
        &self.applied_result
    }
    fn directory(&self, generation: u64) -> String {
        sysutil::join(&[&self.run_directory, "hot-reload", &generation.to_string()])
    }

    /// The assemblies a builder request lists: those whose newest DLL
    /// differs in bytes from the loaded one, and those the applied patch
    /// covers, which the builder compares with what that patch defined.
    fn differing(&self, current: &[Assembly]) -> Result<Vec<BuilderAssembly>, Reason> {
        let mut list = Vec::new();
        for assembly in current {
            let Some(loaded) = self.loaded.iter().find(|l| l.name == assembly.name) else {
                return Err(Reason::new(
                    "assembly_not_loaded",
                    &assembly.name,
                    &format!(
                        "The running Player did not load the assembly {}.",
                        assembly.name
                    ),
                ));
            };
            let differs = loaded.sha256 != assembly.sha256;
            if differs || self.patched.contains(&assembly.name) {
                list.push(BuilderAssembly {
                    name: assembly.name.clone(),
                    loaded: loaded.path.clone(),
                    current: match differs {
                        true => assembly.path.clone(),
                        false => loaded.path.clone(),
                    },
                });
            }
        }
        Ok(list)
    }

    /// Brings the Player to the newest compiled code, or says why not.
    /// Every attempt that reaches the builder takes a generation of its own
    /// and leaves its files in `<run>/hot-reload/<generation>/`. The only
    /// error is the end of `ctx`.
    pub fn attempt(&mut self, ctx: &Context, attempt: &Attempt) -> Result<Outcome> {
        ctx.err()?;
        if (attempt.exited)() {
            return Ok(Outcome::PlayerExited(Failure {
                error: "the Player exited before the code patch".into(),
                report: String::new(),
            }));
        }
        let assemblies = match self.differing(attempt.current) {
            Ok(assemblies) => assemblies,
            Err(reason) => {
                self.refused = true;
                return Ok(Outcome::Unsupported(Refusal {
                    reasons: vec![reason],
                    report: String::new(),
                }));
            }
        };
        if assemblies.is_empty() {
            self.refused = false;
            return Ok(Outcome::Unchanged(Unchanged::default()));
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        let mut files = Files::new(self.directory(generation), generation);
        let outcome = self.attempt_generation(ctx, attempt, assemblies, &mut files);
        // The context ended: nothing is known and the caller stops.
        ctx.err()?;
        // A builder that failed, or files that could not be written, leave
        // the Player untouched: the edit is one that cannot be patched.
        let mut failed = String::new();
        let outcome = outcome.unwrap_or_else(|error| {
            failed = format!("{error:#}");
            Outcome::Unsupported(files.refusal(vec![Reason::new(CODE_BUILDER_FAILED, "", &failed)]))
        });
        let (name, error) = match &outcome {
            Outcome::Patched(_) => ("patched", ""),
            Outcome::Unchanged(_) => ("unchanged", ""),
            Outcome::Unsupported(_) => ("unsupported", failed.as_str()),
            Outcome::ApplyFailed(_) => ("apply_failed", ""),
            Outcome::RestartRequired(failure) => ("restart_required", failure.error.as_str()),
            Outcome::PlayerExited(failure) => ("player_exited", failure.error.as_str()),
            Outcome::TimedOut(failure) => ("timed_out", failure.error.as_str()),
        };
        files.report(name, error);
        self.refused = matches!(outcome, Outcome::Unsupported(_) | Outcome::ApplyFailed(_));
        Ok(outcome)
    }

    fn attempt_generation(
        &mut self,
        ctx: &Context,
        attempt: &Attempt,
        assemblies: Vec<BuilderAssembly>,
        files: &mut Files,
    ) -> Result<Outcome> {
        let generation = files.generation;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&files.directory)
            .map_err(|error| anyhow!(sysutil::Error::os("mkdir", &files.directory, &error)))?;
        let request = files.path("request.json");
        atomic_json(
            &request,
            &BuilderRequest {
                schema_version: 1,
                generation,
                output: files.directory.clone(),
                runtime_assembly: RUNTIME_ASSEMBLY.into(),
                previous_result: self.applied_result.clone(),
                assemblies,
            },
        )?;
        let output = (attempt.build)(ctx, &request)?;
        write(&files.path("result.json"), &output)?;
        let result = BuilderResult::decode(&output, generation)?;
        files.result = raw(&output);
        if result.verdict == VERDICT_UNSUPPORTED {
            let mut reasons = result.unsupported();
            if reasons.is_empty() {
                reasons.push(Reason::new(
                    CODE_BUILDER_FAILED,
                    "",
                    "The patch builder refused the change without a reason.",
                ));
            }
            return Ok(Outcome::Unsupported(files.refusal(reasons)));
        }
        let patch = (result.verdict == VERDICT_PATCH).then_some(&result);
        if patch.is_none() && !self.redirected() {
            // A verdict without a patch still has notes, and they are all
            // the caller learns about what the Player does not take over.
            return Ok(Outcome::Unchanged(Unchanged {
                notes: result.notes(),
                report: files.path("report.json"),
            }));
        }
        let game: Vec<String> = attempt.current.iter().map(|a| a.name.clone()).collect();
        // Without a patch the list is empty, and the Player restores the
        // methods the applied generation redirected.
        let descriptor = Descriptor::new(generation, &game, patch)?;
        // An empty list restores the loaded code, which no patch means.
        if patch.is_some() && descriptor.assemblies.is_empty() {
            return Err(anyhow!(
                "patch builder result has the verdict patch and no patched assembly"
            ));
        }
        let redirects: u64 = descriptor
            .assemblies
            .iter()
            .map(|assembly| assembly.detours.len() as u64)
            .sum();
        for assembly in &descriptor.assemblies {
            if file_hash(&assembly.patch)? != assembly.patch_sha256 {
                return Err(anyhow!(
                    "patch assembly differs from the hash of its result: {}",
                    assembly.patch
                ));
            }
        }
        let mut data = marshal(&descriptor)?;
        data.push(b'\n');
        let descriptor_path = files.path("apply.json");
        write(&descriptor_path, &data)?;
        let id = random_id();
        let path = Self::request_path(&self.run_directory);
        atomic_json(
            &path,
            &ReloadRequest {
                schema_version: 1,
                request_id: id.clone(),
                generation,
                applier: self.applier.path.clone(),
                applier_sha256: self.applier.sha256.clone(),
                descriptor: descriptor_path,
                descriptor_sha256: sha256(&data),
                requested_utc_unix_milliseconds: unix_milliseconds(SystemTime::now()),
                source_changed_utc_unix_milliseconds: unix_milliseconds(attempt.source_changed),
            },
        )?;
        let answer = wait_code_ack(
            ctx,
            &format!("{path}.ack.json"),
            &id,
            attempt.timeout,
            attempt.exited,
        )?;
        let (ack, data) = match answer {
            Answer::Ack(ack, data) => (ack, data),
            Answer::Unreadable(error, data) => {
                files.ack(&data);
                return Ok(Outcome::RestartRequired(files.failure(format!(
                    "the Player's code reload acknowledgement is unreadable: {error}"
                ))));
            }
            Answer::Exited => {
                return Ok(Outcome::PlayerExited(files.failure(
                    "the Player exited while it applied the code patch".into(),
                )));
            }
            Answer::TimedOut => {
                return Ok(Outcome::TimedOut(files.failure(format!(
                    "the Player did not acknowledge the code patch within {}s",
                    attempt.timeout.as_secs()
                ))));
            }
        };
        files.ack(&data);
        if ack.restart_required {
            let error = match ack.error.is_empty() {
                true => "the Player reported that its code must be restarted".into(),
                false => ack.error.clone(),
            };
            return Ok(Outcome::RestartRequired(files.failure(error)));
        }
        if ack.pid != i64::from(attempt.pid) {
            return Ok(Outcome::RestartRequired(files.failure(format!(
                "process {} acknowledged the code patch of Player {}",
                ack.pid, attempt.pid
            ))));
        }
        if !ack.success {
            // Nothing was written, so the Player must still name the
            // generation this side applied last.
            if ack.applied_generation != self.applied_generation {
                return Ok(Outcome::RestartRequired(files.failure(format!(
                    "the Player runs code generation {} where generation {} was applied: {}",
                    ack.applied_generation, self.applied_generation, ack.error
                ))));
            }
            let mut reasons: Vec<Reason> = ack
                .failures
                .iter()
                .map(|failure| {
                    let member = [failure.assembly.as_str(), failure.member.as_str()]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(": ");
                    let stage = match failure.stage.is_empty() {
                        true => "apply",
                        false => failure.stage.as_str(),
                    };
                    Reason::new(
                        CODE_APPLY_FAILED,
                        &member,
                        &format!("{stage}: {}", failure.message),
                    )
                })
                .collect();
            // The bridge answers without a list when the applier did not run.
            if reasons.is_empty() {
                reasons.push(Reason::new(CODE_APPLY_FAILED, "", &ack.error));
            }
            return Ok(Outcome::ApplyFailed(files.refusal(reasons)));
        }
        if ack.applied_generation != generation {
            return Ok(Outcome::RestartRequired(files.failure(format!(
                "the Player acknowledged code generation {} for the request of generation {generation}",
                ack.applied_generation
            ))));
        }
        // The applier counts one per listed redirect. Another number is a
        // Player that runs something else than the patch it was sent.
        if ack.detours_applied != redirects {
            return Ok(Outcome::RestartRequired(files.failure(format!(
                "the Player acknowledged {} of the {redirects} redirects of code generation {generation}",
                ack.detours_applied
            ))));
        }
        self.applied_generation = generation;
        self.patched = descriptor
            .assemblies
            .iter()
            .map(|assembly| assembly.name.clone())
            .collect();
        self.applied_result = match patch {
            Some(_) => files.path("result.json"),
            None => String::new(),
        };
        let mut notes = result.notes();
        if ack.callback_errors > 0 {
            notes.push(Reason {
                // A fact of this generation, not one that stands from the one before.
                revision: format!("g{generation}"),
                ..Reason::new(
                    CODE_CALLBACK_FAILED,
                    "",
                    &format!(
                        "{} hot reload callback(s) of the game threw or could not be called; see the Player log.",
                        ack.callback_errors
                    ),
                )
            });
        }
        Ok(Outcome::Patched(Patched {
            generation,
            assemblies: self.patched.clone(),
            patched_methods: redirects,
            added_methods: patch.map_or(0, |result| result.summary.added_methods),
            added_fields: patch.map_or(0, |result| result.summary.added_fields),
            restored_methods: ack.detours_restored,
            members: patch.map(BuilderResult::members).unwrap_or_default(),
            notes,
            report: files.path("report.json"),
        }))
    }
}

/// The files of one generation.
struct Files {
    directory: String,
    generation: u64,
    result: Option<Box<RawValue>>,
    acknowledgement: Option<Box<RawValue>>,
}
impl Files {
    fn new(directory: String, generation: u64) -> Self {
        Self {
            directory,
            generation,
            result: None,
            acknowledgement: None,
        }
    }
    fn path(&self, name: &str) -> String {
        sysutil::join(&[&self.directory, name])
    }
    fn refusal(&self, reasons: Vec<Reason>) -> Refusal {
        Refusal {
            reasons,
            report: self.path("report.json"),
        }
    }
    fn failure(&self, error: String) -> Failure {
        Failure {
            error,
            report: self.path("report.json"),
        }
    }
    /// Keeps a copy of the acknowledgement beside the request it answers.
    fn ack(&mut self, data: &[u8]) {
        let _ = write(&self.path("ack.json"), data);
        self.acknowledgement = raw(data);
    }
    /// `report.json`: the builder's result and the Player's acknowledgement
    /// in full; events carry at most 16 entries of each list. A report that
    /// cannot be written loses nothing the Player depends on.
    fn report(&self, outcome: &str, error: &str) {
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory);
        let _ = atomic_json(
            &self.path("report.json"),
            &Report {
                schema_version: 1,
                generation: self.generation,
                outcome,
                error,
                result: self.result.as_deref(),
                ack: self.acknowledgement.as_deref(),
            },
        );
    }
}
