//! What the worker supplied a session with, kept for the project.
//!
//! A session asks the importer for the fields of a material or a Mesh of an
//! older serialization, as the engine saves them, and for the entries of a
//! pipeline's global settings a Player build keeps. Each answer takes a
//! worker run, and each new process asked again. The answers are kept under
//! `Library/UnityMicroEditor/session-supplies`, each with the import
//! environment, the project's compiled Editor code and the graphics and
//! quality settings it was made with, and taken only while those and the
//! hash of every file it was made from are unchanged.
use super::{
    build_settings::Sources, importer::NativeProjectImporter, scripted::script_environment_identity,
};
use crate::{
    sysutil,
    util::{atomic_json, file_hash, read_json_file},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
struct Kept<T> {
    context: String,
    record: T,
}

fn path(project: &str, kind: &str, guid: &str) -> String {
    sysutil::join(&[
        project,
        "Library/UnityMicroEditor/session-supplies",
        kind,
        &format!("{guid}.json"),
    ])
}

impl NativeProjectImporter {
    /// The import environment, the compiled Editor code and the settings
    /// that choose the pipeline's assets.
    fn supply_context(&self) -> String {
        let settings = |name: &str| {
            file_hash(&sysutil::join(&[
                &self.opts.project,
                "ProjectSettings",
                name,
            ]))
            .unwrap_or_default()
        };
        format!(
            "{}\0{}\0{}\0{}",
            self.environment_sha256,
            script_environment_identity(&self.opts.project),
            settings("GraphicsSettings.asset"),
            settings("QualitySettings.asset"),
        )
    }

    /// The answer kept for `guid`, while it was made in this context from
    /// files as they are now.
    pub(super) fn kept_supply<T: DeserializeOwned>(
        &self,
        sources: &Sources,
        kind: &str,
        guid: &str,
        ingredients: impl Fn(&T) -> &BTreeMap<String, String>,
    ) -> Option<T> {
        read_kept(
            &self.opts.project,
            kind,
            guid,
            &self.supply_context(),
            &|logical| sources.physical(logical),
            ingredients,
        )
    }

    /// Keeps an answer; a store that fails only costs the next process a run.
    pub(super) fn keep_supply<T: Serialize>(&self, kind: &str, guid: &str, record: &T) {
        if !super::importer::valid_native_guid(guid) {
            return;
        }
        write_kept(
            &self.opts.project,
            kind,
            guid,
            &self.supply_context(),
            record,
        );
    }
}

fn read_kept<T: DeserializeOwned>(
    project: &str,
    kind: &str,
    guid: &str,
    context: &str,
    physical: &dyn Fn(&str) -> Option<String>,
    ingredients: impl Fn(&T) -> &BTreeMap<String, String>,
) -> Option<T> {
    if !super::importer::valid_native_guid(guid) {
        return None;
    }
    let kept: Kept<T> = read_json_file(&path(project, kind, guid)).ok()?;
    if kept.context != context {
        return None;
    }
    let current = ingredients(&kept.record).iter().all(|(logical, expected)| {
        physical(logical)
            .is_some_and(|physical| file_hash(&physical).is_ok_and(|hash| hash == *expected))
    });
    current.then_some(kept.record)
}

fn write_kept<T: Serialize>(project: &str, kind: &str, guid: &str, context: &str, record: &T) {
    let file = path(project, kind, guid);
    let _ = std::fs::create_dir_all(sysutil::dir(&file));
    let _ = atomic_json(
        &file,
        &Kept {
            context: context.to_string(),
            record,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_answer_is_taken_only_in_its_context_from_unchanged_files() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().to_string_lossy().into_owned();
        let source = sysutil::join(&[&project, "Assets/A.mesh"]);
        std::fs::create_dir_all(sysutil::dir(&source)).unwrap();
        std::fs::write(&source, "one").unwrap();
        let guid = "0123456789abcdef0123456789abcdef";
        let record: BTreeMap<String, String> =
            [("Assets/A.mesh".to_string(), file_hash(&source).unwrap())].into();
        write_kept(&project, "mesh-upgrade", guid, "context", &record);
        let physical = |logical: &str| Some(sysutil::join(&[&project, logical]));
        let read = |context: &str| {
            read_kept::<BTreeMap<String, String>>(
                &project,
                "mesh-upgrade",
                guid,
                context,
                &physical,
                |r| r,
            )
        };
        assert_eq!(read("context"), Some(record.clone()));
        assert_eq!(read("another"), None);
        std::fs::write(&source, "two").unwrap();
        assert_eq!(read("context"), None);
    }
}
