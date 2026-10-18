//! A content scene the raw loader refused, remembered until what decided it
//! changes. Preparing such a scene costs its whole discovery before the
//! loader sees the object it has no adapter for; a session that starts
//! again with the same sources leaves the scene out at once.
use crate::util::tool_identity;
use crate::{
    project::{Session, UnsupportedClass},
    sysutil,
    util::{atomic_json, file_hash, read_json_file},
};
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Refusal {
    schema_version: i64,
    /// The program that refused it: another may have an adapter.
    tool: String,
    /// Which source each GUID named.
    guid_index: String,
    /// The baseline the scene's session started from.
    baseline: String,
    error: String,
    /// Every source the preparation read, by absolute path, with its hash.
    sources: BTreeMap<String, String>,
}

fn record(directory: &str) -> String {
    sysutil::join(&[directory, "refusal.json"])
}

/// Why the content scene whose importer writes to `directory` was refused,
/// when nothing that decided it has changed since. `root` is a session of
/// the same project, whose GUID index the scene's own would equal.
pub(crate) fn remembered(directory: &str, root: &Session, baseline: &str) -> Option<anyhow::Error> {
    let refusal: Refusal = read_json_file(&record(directory)).ok()?;
    let current = refusal.schema_version == 1
        && Some(refusal.tool.as_str()) == tool_identity()
        && root.guid_index_sha256().ok()? == refusal.guid_index
        && file_hash(baseline).ok()? == refusal.baseline
        && refusal
            .sources
            .iter()
            .all(|(path, sha)| file_hash(path).is_ok_and(|actual| actual == *sha));
    current.then(|| anyhow!(refusal.error))
}

/// Remembers why a content scene was refused, when the reason is an object
/// of a class the loader has no adapter for: which objects a scene holds
/// follows from the sources its preparation read and the GUIDs they name.
/// A record that cannot be written only costs the next start the attempt.
pub(crate) fn remember(
    directory: &str,
    root: &Session,
    scene: &Session,
    baseline: &str,
    error: &anyhow::Error,
) {
    if !error.chain().any(|cause| cause.is::<UnsupportedClass>()) {
        return;
    }
    let (Some(tool), Ok(guid_index), Ok(baseline)) = (
        tool_identity(),
        root.guid_index_sha256(),
        file_hash(baseline),
    ) else {
        return;
    };
    let refusal = Refusal {
        schema_version: 1,
        tool: tool.into(),
        guid_index,
        baseline,
        error: format!("{error:#}"),
        sources: scene.read_sources(),
    };
    if std::fs::create_dir_all(directory).is_ok() {
        let _ = atomic_json(&record(directory), &refusal);
    }
}

/// Forgets a refusal once the scene prepares.
pub(crate) fn forget(directory: &str) {
    let _ = std::fs::remove_file(record(directory));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Options;
    use std::fs;

    const SCENE: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &10
GameObject:
  m_Component:
  - component: {fileID: 11}
  - component: {fileID: 12}
  m_Name: Ship
  m_IsActive: 1
--- !u!4 &11
Transform:
  m_GameObject: {fileID: 10}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!208 &12
NavMeshObstacle:
  m_GameObject: {fileID: 10}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 11}
";

    #[test]
    fn a_refused_scene_is_remembered_until_a_source_it_read_changes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().canonicalize().unwrap();
        let project = project.to_string_lossy().into_owned();
        let scene = format!("{project}/Assets/Ship.unity");
        fs::create_dir_all(sysutil::dir(&scene)).unwrap();
        fs::write(&scene, SCENE).unwrap();
        fs::write(
            format!("{scene}.meta"),
            "guid: 11111111111111111111111111111111\n",
        )
        .unwrap();
        let baseline = format!("{project}/baseline.json");
        fs::write(&baseline, serde_json::json!({"schemaVersion":1,"supported":true,"unityVersion":"6000.6.3f1","projectRoot":project,"scenePath":"Assets/Ship.unity","sceneName":"Ship","objects":[],"assets":[],"sourceDependencies":[]}).to_string()).unwrap();
        let session = || {
            Session::new(Options {
                project: project.clone(),
                manifest: baseline.clone(),
                ..Default::default()
            })
            .unwrap()
        };
        let (ship, other) = (session(), session());
        let output = format!("{project}/output");
        let directory = format!("{output}/content/ship");
        // Only a class without an adapter is remembered.
        remember(&directory, &other, &ship, &baseline, &anyhow!("other"));
        assert!(remembered(&directory, &other, &baseline).is_none());
        let error = ship.prepare(&output).unwrap_err();
        remember(&directory, &other, &ship, &baseline, &error);
        let recalled = remembered(&directory, &other, &baseline).expect("remembered");
        assert!(
            format!("{recalled:#}").contains("class 208"),
            "{recalled:#}"
        );
        fs::write(&scene, SCENE.replacen("m_Name: Ship", "m_Name: Boat", 1)).unwrap();
        assert!(remembered(&directory, &other, &baseline).is_none());
        fs::write(&scene, SCENE).unwrap();
        assert!(remembered(&directory, &other, &baseline).is_some());
        forget(&directory);
        assert!(remembered(&directory, &other, &baseline).is_none());
    }
}
