//! The stage a long project action is in. Project actions are serialized by
//! the service, so one record describes the action that currently runs.
use super::{Any, AnyMap, Service, obj};
use std::time::Instant;

/// The stages of `project_prepare` in execution order. A preparation that
/// reuses cached results skips the stages it does not need.
pub(crate) const PREPARE_STAGES: [&str; 12] = [
    "validate_sources",
    "resolve_packages",
    "build_host",
    "export_engine_graph",
    "import_native_content",
    "capture_engine_profile",
    "capture_component_defaults",
    "build_source_graph",
    "prove_scene",
    "compile_scripts",
    "compile_editor_assemblies",
    "publish_baseline",
];

#[derive(Default, Clone)]
pub(crate) struct Progress {
    pub active: bool,
    pub action: String,
    pub request_id: String,
    pub stage: String,
    pub detail: String,
    pub log_path: String,
    pub started: Option<Instant>,
    pub stage_started: Option<Instant>,
}

impl Progress {
    fn encode(&self) -> AnyMap {
        let seconds = |since: Option<Instant>| -> Any {
            let elapsed = since.map_or(0.0, |since| since.elapsed().as_secs_f64());
            Any::Float((elapsed * 10.0).round() / 10.0)
        };
        let mut result = obj([
            ("action", self.action.clone().into()),
            ("requestId", self.request_id.clone().into()),
            ("stage", self.stage.clone().into()),
            ("elapsedSeconds", seconds(self.started)),
            ("stageElapsedSeconds", seconds(self.stage_started)),
        ]);
        if let Some(index) = PREPARE_STAGES.iter().position(|name| *name == self.stage) {
            result.insert("stageIndex".into(), ((index + 1) as i64).into());
            result.insert("stageCount".into(), (PREPARE_STAGES.len() as i64).into());
        }
        if !self.detail.is_empty() {
            result.insert("detail".into(), self.detail.clone().into());
        }
        if !self.log_path.is_empty() {
            result.insert("logPath".into(), self.log_path.clone().into());
        }
        result
    }
}

impl Service {
    /// Marks the action that now holds the project action lock. A job that
    /// still waits for the lock has no record and reports `queued`.
    pub(crate) fn progress_begin(&self, action: &str, id: &str) {
        let now = Instant::now();
        *self.0.progress.lock().unwrap() = Progress {
            active: true,
            action: action.into(),
            request_id: id.into(),
            stage: "running".into(),
            started: Some(now),
            stage_started: Some(now),
            ..Default::default()
        };
    }
    pub(crate) fn progress_end(&self) {
        self.0.progress.lock().unwrap().active = false;
    }
    /// Enters a stage. `log_path` names the log to read while it runs.
    pub(crate) fn stage(&self, stage: &str, detail: &str, log_path: &str) {
        let mut progress = self.0.progress.lock().unwrap();
        if progress.stage == stage && progress.detail == detail && progress.log_path == log_path {
            return;
        }
        if !self.0.asynchronous {
            // The CLI has no status call to poll; its stderr is the progress channel.
            let log = match log_path {
                "" => String::new(),
                path => format!(" (log: {path})"),
            };
            let separator = if detail.is_empty() { "" } else { ": " };
            eprintln!("[unity-micro-editor] {stage}{separator}{detail}{log}");
        }
        if progress.stage != stage {
            progress.stage_started = Some(Instant::now());
        }
        progress.stage = stage.into();
        progress.detail = detail.into();
        progress.log_path = log_path.into();
    }
    /// The running action, or the one `id` names when that is given.
    pub(crate) fn progress(&self, id: &str) -> Option<AnyMap> {
        let progress = self.0.progress.lock().unwrap();
        (progress.active && (id.is_empty() || progress.request_id == id)).then(|| progress.encode())
    }
    /// The running preparation, if any.
    pub(crate) fn preparation_progress(&self) -> Option<AnyMap> {
        let progress = self.0.progress.lock().unwrap();
        (progress.active && progress.action == "project_prepare").then(|| progress.encode())
    }
}
