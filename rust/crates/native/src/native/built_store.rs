//! The Player objects the Editor's build code wrote for a shader, kept for
//! the project under a key of everything they were built from.
//!
//! Every importer of a session keeps what it built beneath its own output:
//! the scene's importer, and one for each content scene and for the content
//! prefabs. A shader of a package that the materials of two of them use was
//! therefore built once for each, minutes apart, into the same bytes. The
//! store lets the second take what the first wrote. Nothing is trusted but
//! the key: an entry is found only under the hash of its inputs, and its
//! file is used only while it has the hash that was recorded with it.
use super::support::{mkdir_all, read_file, write_file};
use crate::{
    sha256, sysutil,
    util::{atomic_json, random_id, read_json_file},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How many builds the store keeps, and how many bytes of files; the least
/// recently used go first. A pipeline's resources are some 90 shaders and
/// compute shaders, of a few kilobytes to 13 MB (a lit shader); a store
/// smaller than that set would drop each build before the next preparation
/// asks for it again.
const KEPT: usize = 1024;
const KEPT_BYTES: u64 = 4 << 30;

/// Everything a build of a source depends on. Two builds with equal inputs
/// run the same code on the same files.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Inputs<'a> {
    /// The tool's build importer, by the hash of its source.
    pub importer: &'a str,
    /// The engine and the import settings of the project.
    pub environment_sha256: &'a str,
    /// The compiled Editor code of the project, which a build runs.
    pub scripts_identity: &'a str,
    pub target: &'a str,
    pub guid: &'a str,
    pub source_sha256: &'a str,
    pub meta_sha256: &'a str,
    /// The materials the variants are chosen for.
    pub users: &'a [String],
    /// What the worker is asked to write, as it is sent.
    pub request: &'a str,
    /// The assets the worker imports, in order.
    pub batch: &'a [String],
    /// The scripts whose assemblies the worker loads.
    pub scripts: &'a BTreeSet<String>,
    /// The hash of every authored file among them, and of the files that
    /// state the project's packages.
    pub ingredients: &'a BTreeMap<String, String>,
}

impl Inputs<'_> {
    pub fn key(&self) -> Result<String> {
        let mut text = b"unity-micro-editor built source 1\0".to_vec();
        text.extend(serde_json::to_vec(self)?);
        Ok(sha256(&text))
    }
}

/// What a build wrote, as its importer publishes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Built {
    /// The serialized file of the objects.
    #[serde(skip)]
    pub file: Vec<u8>,
    /// Local file ID and managed type name of each published object.
    pub objects: Vec<(String, String)>,
    /// The GUIDs of the sources the objects refer to, in the file's order.
    pub externals: Vec<String>,
    /// Where those sources lie, by GUID.
    pub external_paths: BTreeMap<String, String>,
    /// The source object of each published object, as `<GUID>/<local ID>`,
    /// in the order of `objects`, for a file that holds objects of many
    /// sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// The names of the files beside it that hold its streamed data, each
    /// kept as a build of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    /// The local ID of the root GameObject of each prefab the file holds,
    /// by the prefab's GUID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roots: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    schema_version: u32,
    key: String,
    /// For a reader of the directory; not part of the identity.
    target: String,
    file_sha256: String,
    #[serde(flatten)]
    built: Built,
}

fn directory(project: &str) -> String {
    sysutil::join(&[project, "Library/UnityMicroEditor/built-sources"])
}

fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The build kept under a key, if its file is the one that was stored.
pub(super) fn load(project: &str, key: &str) -> Option<Built> {
    if !valid_key(key) {
        return None;
    }
    let directory = directory(project);
    let record = sysutil::join(&[&directory, &format!("{key}.json")]);
    let entry: Entry = read_json_file(&record).ok()?;
    let file = read_file(&sysutil::join(&[&directory, &format!("{key}.bin")])).ok()?;
    if entry.schema_version != 1 || entry.key != key || sha256(&file) != entry.file_sha256 {
        return None;
    }
    // Written again, so that the entries in use are the newest.
    let _ = atomic_json(&record, &entry);
    Some(Built {
        file,
        ..entry.built
    })
}

/// Keeps a build under its key and drops the builds beyond [`KEPT`].
pub(super) fn store(project: &str, key: &str, target: &str, built: &Built) -> Result<()> {
    if !valid_key(key) {
        anyhow::bail!("built source key is not a SHA-256");
    }
    let directory = directory(project);
    mkdir_all(&directory, 0o700)?;
    // The file first: a record names a file that is whole.
    let file = sysutil::join(&[&directory, &format!("{key}.bin")]);
    let partial = format!("{file}.{}.tmp", random_id());
    write_file(&partial, &built.file, 0o600)?;
    if let Err(error) = std::fs::rename(&partial, &file) {
        let _ = std::fs::remove_file(&partial);
        return Err(error.into());
    }
    atomic_json(
        &sysutil::join(&[&directory, &format!("{key}.json")]),
        &Entry {
            schema_version: 1,
            key: key.into(),
            target: target.into(),
            file_sha256: sha256(&built.file),
            built: built.clone(),
        },
    )?;
    prune(&directory, KEPT, KEPT_BYTES);
    Ok(())
}

/// Removes all but the most recently written entries, at most `kept` of them
/// and `bytes` of their files. A reader that loses a file to it builds again.
fn prune(directory: &str, kept: usize, bytes: u64) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut records: Vec<(std::time::SystemTime, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let key = name.strip_suffix(".json")?.to_string();
            let written = entry.metadata().ok()?.modified().ok()?;
            valid_key(&key).then_some((written, key))
        })
        .collect();
    // Newest first: an entry is kept while both budgets hold.
    records.sort_by(|a, b| b.cmp(a));
    let mut total = 0u64;
    let mut dropped = Vec::new();
    for (index, (_, key)) in records.into_iter().enumerate() {
        total += std::fs::metadata(sysutil::join(&[directory, &format!("{key}.bin")]))
            .map_or(0, |info| info.len());
        if index >= kept || total > bytes {
            dropped.push(key);
        }
    }
    for key in dropped {
        for extension in ["json", "bin"] {
            let _ =
                std::fs::remove_file(sysutil::join(&[directory, &format!("{key}.{extension}")]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysutil::testing::temp_dir;

    fn inputs<'a>(
        users: &'a [String],
        scripts: &'a BTreeSet<String>,
        ingredients: &'a BTreeMap<String, String>,
    ) -> Inputs<'a> {
        Inputs {
            importer: "importer",
            environment_sha256: "environment",
            scripts_identity: "compiled",
            target: "Packages/pipeline/Lit.shader",
            guid: "933532a4fcc9baf4fa0491de14d08ed7",
            source_sha256: "source",
            meta_sha256: "meta",
            users,
            request: "request",
            batch: users,
            scripts,
            ingredients,
        }
    }

    #[test]
    fn key_changes_with_every_input() {
        let users = vec!["Assets/A.mat".to_string()];
        let scripts = BTreeSet::from(["script".to_string()]);
        let ingredients = BTreeMap::from([("Assets/A.mat".to_string(), "a".to_string())]);
        let base = inputs(&users, &scripts, &ingredients);
        let key = base.key().unwrap();
        assert!(valid_key(&key));
        assert_eq!(key, inputs(&users, &scripts, &ingredients).key().unwrap());
        let other_users = vec!["Assets/B.mat".to_string()];
        let other_scripts = BTreeSet::new();
        let other_ingredients = BTreeMap::from([("Assets/A.mat".to_string(), "b".to_string())]);
        let mut keys = BTreeSet::from([key]);
        for changed in [
            Inputs {
                importer: "other",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                environment_sha256: "other",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                scripts_identity: "other",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                target: "Packages/pipeline/Unlit.shader",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                guid: "0406db5a14f94604a8c57ccfbc9f3b46",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                source_sha256: "other",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                meta_sha256: "other",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                request: "other",
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                users: &other_users,
                ..inputs(&users, &scripts, &ingredients)
            },
            Inputs {
                batch: &other_users,
                ..inputs(&users, &scripts, &ingredients)
            },
            inputs(&users, &other_scripts, &ingredients),
            inputs(&users, &scripts, &other_ingredients),
        ] {
            assert!(
                keys.insert(changed.key().unwrap()),
                "a changed input kept the key"
            );
        }
    }

    #[test]
    fn a_build_is_found_only_under_its_key_and_only_whole() {
        let (_guard, project) = temp_dir();
        let built = Built {
            file: b"player objects".to_vec(),
            objects: vec![("4800000".into(), "UnityEngine.Shader".into())],
            externals: vec!["2cbfcd0349c944ee788f6e2919b9d66c".into()],
            external_paths: BTreeMap::from([(
                "2cbfcd0349c944ee788f6e2919b9d66c".to_string(),
                "Assets/Screen.shader".to_string(),
            )]),
            sources: vec!["2cbfcd0349c944ee788f6e2919b9d66c/4800000".into()],
            resources: Vec::new(),
            roots: BTreeMap::new(),
        };
        let (key, other) = (sha256(b"one"), sha256(b"two"));
        assert_eq!(load(&project, &key), None);
        store(&project, &key, "Packages/pipeline/Lit.shader", &built).unwrap();
        assert_eq!(load(&project, &key), Some(built.clone()));
        assert_eq!(load(&project, &other), None);
        assert_eq!(load(&project, "../escape"), None);
        assert!(store(&project, "../escape", "", &built).is_err());
        // A record copied to another key is not that key's build.
        let directory = directory(&project);
        for extension in ["json", "bin"] {
            std::fs::copy(
                sysutil::join(&[&directory, &format!("{key}.{extension}")]),
                sysutil::join(&[&directory, &format!("{other}.{extension}")]),
            )
            .unwrap();
        }
        assert_eq!(load(&project, &other), None);
        // Nor is a file that is no longer the one that was stored.
        std::fs::write(
            sysutil::join(&[&directory, &format!("{key}.bin")]),
            b"other",
        )
        .unwrap();
        assert_eq!(load(&project, &key), None);
        // Storing again makes it whole.
        store(&project, &key, "Packages/pipeline/Lit.shader", &built).unwrap();
        assert_eq!(load(&project, &key), Some(built));
    }

    #[test]
    fn the_oldest_builds_are_dropped() {
        let (_guard, project) = temp_dir();
        let directory = directory(&project);
        let built = Built {
            file: b"x".to_vec(),
            ..Default::default()
        };
        let keys: Vec<String> = (0..4u8).map(|index| sha256(&[index])).collect();
        for (index, key) in keys.iter().enumerate() {
            store(&project, key, "", &built).unwrap();
            // Apart in time, whatever the file system's resolution.
            let record = std::fs::File::options()
                .write(true)
                .open(sysutil::join(&[&directory, &format!("{key}.json")]))
                .unwrap();
            record
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_000 + index as u64),
                )
                .unwrap();
        }
        prune(&directory, 2, u64::MAX);
        let left: Vec<bool> = keys
            .iter()
            .map(|key| load(&project, key).is_some())
            .collect();
        assert_eq!(left, [false, false, true, true]);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 4);
        // By size: of the two left, only the newest file fits.
        for (index, key) in keys.iter().enumerate().skip(2) {
            std::fs::File::options()
                .write(true)
                .open(sysutil::join(&[&directory, &format!("{key}.json")]))
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(2_000 + index as u64),
                )
                .unwrap();
        }
        let size = built.file.len() as u64;
        prune(&directory, usize::MAX, size);
        let left: Vec<bool> = keys
            .iter()
            .map(|key| load(&project, key).is_some())
            .collect();
        assert_eq!(left, [false, false, false, true]);
    }
}
