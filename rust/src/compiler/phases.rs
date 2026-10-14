//! Where the time of one compile or one patch attempt went. The runner
//! prints these with `UME_TIMINGS=1`; nothing else reads them, and they are
//! never persisted.
use std::time::{Duration, Instant};

/// The phases of `Compiler::compile`, in the order they run. A phase that
/// did not run in a compile counts 0.
pub const COMPILE_PHASES: [&str; 18] = [
    // The candidate graph: the configuration inputs by content and the walk of `Assets`.
    "graph_candidate",
    // The same check again at the start of `compile_graph`.
    "configuration_check",
    "source_hashes",
    // The identity of the revision: baseline Player directory, tools, helper payload.
    "revision_key",
    "revision_restore",
    "snapshots",
    "references",
    // The content hashes of references, helper, csc and generators that key an assembly.
    "assembly_identity",
    "lifecycle_rewrite",
    "csc",
    "netcode",
    "assembly_cache",
    "type_definitions",
    "declaration_hashes",
    // Configuration inputs and sources once more after the compile.
    "final_check",
    "player_copy",
    "player_dlls",
    // The file map of the new Player directory and the revision entry.
    "save_revision",
];

/// The phases of one patch attempt, in the order they run.
pub const PATCH_PHASES: [&str; 5] = [
    "builder_request",
    "builder",
    "descriptor",
    // From the request written to the acknowledgement read.
    "player_ack",
    "report",
];

/// The time per phase name; a name added twice adds up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phases(Vec<(&'static str, Duration)>);

impl Phases {
    pub fn add(&mut self, name: &'static str, time: Duration) {
        match self.0.iter_mut().find(|(known, _)| *known == name) {
            Some(entry) => entry.1 += time,
            None => self.0.push((name, time)),
        }
    }

    /// Adds the time since `started` and returns now, the start of the next phase.
    pub fn lap(&mut self, name: &'static str, started: Instant) -> Instant {
        let now = Instant::now();
        self.add(name, now.duration_since(started));
        now
    }

    /// Seconds.
    pub fn get(&self, name: &str) -> f64 {
        self.0
            .iter()
            .find(|(known, _)| *known == name)
            .map_or(0.0, |(_, time)| time.as_secs_f64())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Every name of `names` with its seconds, then any other phase that was noted.
    pub fn listed(&self, names: &[&'static str]) -> Vec<(&'static str, f64)> {
        let mut list: Vec<(&'static str, f64)> =
            names.iter().map(|name| (*name, self.get(name))).collect();
        list.extend(
            self.0
                .iter()
                .filter(|(name, _)| !names.contains(name))
                .map(|(name, time)| (*name, time.as_secs_f64())),
        );
        list
    }
}
