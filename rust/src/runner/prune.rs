//! The bound of a session output. Every start stages a host Player
//! (`players/<id>`) and every compile publishes a Player directory
//! (`compiler/.code-revision-<id>/Player`), about 185 MB each in the
//! sample. What no running Player, no recorded preparation and no kept
//! revision entry uses any more is removed; the newest revision entries
//! are kept so that a later start can still take one instead of compiling.
use crate::{compiler::Stamp, sysutil, util::within_directory};
use anyhow::Result;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::SystemTime,
};

/// Revision entries kept, newest first, besides those of a directory that
/// is in use.
pub(crate) const KEPT_REVISIONS: usize = 4;
/// Directories of compiles that published nothing (a compile error's
/// diagnostics), newest first.
pub(crate) const KEPT_UNPUBLISHED: usize = 2;
/// Scene revisions (`<output>/revisions/<id>`) kept, newest first, besides
/// those in use.
pub(crate) const KEPT_SCENE_REVISIONS: usize = 8;
/// Entries of the assembly cache (`<output>/compiler/assemblies/<key>`)
/// kept, newest first, besides those the compiler uses.
pub(crate) const KEPT_ASSEMBLIES: usize = 64;
/// Run directories (`<output>/runs/<id>`: the Player's log, reports and
/// patches of one Player process) kept, newest first, besides the running
/// one.
pub(crate) const KEPT_RUNS: usize = 8;
/// Recorded preparations larger than this are not read; whatever lies in a
/// directory they could name then stays.
const PREPARATION_LIMIT: u64 = 256 << 20;
/// Where `run --prepare-only` records the Player and manifest it prepared
/// in the output, one file per run; a later session on the output keeps
/// what they name. The newest `KEPT_PREPARED` stay.
pub(crate) const PREPARED: &str = "prepared";
pub(crate) const KEPT_PREPARED: usize = 4;

/// Records what `run --prepare-only` prepared, so that a later session on
/// the same output keeps it for a `--prepared` launch, and removes all but
/// the newest records.
pub(crate) fn record_prepared(output: &str, record: &serde_json::Value) -> Result<()> {
    let directory = sysutil::join(&[output, PREPARED]);
    sysutil::mkdir_all(&directory)?;
    let path = sysutil::join(&[&directory, &format!("{}.json", crate::util::random_id())]);
    crate::util::atomic_json(&path, record)?;
    let mut records: Vec<(SystemTime, String)> = fs::read_dir(&directory)
        .map(|items| {
            items
                .flatten()
                .map(|item| sysutil::join(&[&directory, &item.file_name().to_string_lossy()]))
                .filter(|path| path.ends_with(".json"))
                .map(|path| (modified(&path), path))
                .collect()
        })
        .unwrap_or_default();
    records.sort_by(|a, b| b.cmp(a));
    for (_, old) in records.into_iter().skip(KEPT_PREPARED) {
        let _ = fs::remove_file(old);
    }
    Ok(())
}

/// What a prune removed and why the rest stayed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Pruned {
    /// Scene revisions and compiled assemblies removed beyond the newest.
    pub files: usize,
    pub removed: Vec<String>,
    pub entries: Vec<String>,
    pub kept: Vec<String>,
    /// A preparation that could not be read: nothing was removed.
    pub refused: String,
}

/// The paths that recorded preparations name inside the output, read once
/// per version of each file.
#[derive(Debug, Default)]
pub(crate) struct Pruner {
    output: String,
    preparations: String,
    read: BTreeMap<String, (Stamp, BTreeSet<String>)>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Entry {
    player: String,
}

fn directories(parent: &str, keep: impl Fn(&str) -> bool) -> Vec<String> {
    let Ok(items) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<String> = items
        .flatten()
        .filter(|item| item.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|item| item.file_name().to_string_lossy().into_owned())
        .filter(|name| keep(name))
        .map(|name| sysutil::join(&[parent, &name]))
        .collect();
    found.sort();
    found
}

/// Whether `path` is `directory` or lies in it.
fn holds(directory: &str, path: &str) -> bool {
    path == directory || within_directory(path, directory)
}

fn modified(path: &str) -> SystemTime {
    fs::symlink_metadata(path)
        .and_then(|info| info.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The executables of the processes that run now. One that cannot be read
/// belongs to another user and cannot run from a session output of ours.
fn running_executables() -> Vec<String> {
    let Ok(items) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    items
        .flatten()
        .filter(|item| {
            item.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter_map(|item| fs::read_link(item.path().join("exe")).ok())
        .map(|path| {
            let text = path.to_string_lossy().into_owned();
            text.strip_suffix(" (deleted)").unwrap_or(&text).to_string()
        })
        .collect()
}

impl Pruner {
    /// The pruner of the session output `output` of `project`.
    pub fn new(project: &str, output: &str) -> Self {
        Self {
            output: output.into(),
            preparations: sysutil::join(&[project, "Library/UnityMicroEditor/preparations"]),
            read: BTreeMap::new(),
        }
    }

    /// Every path under the output that a recorded preparation names, or
    /// the name of one that cannot be read.
    fn recorded(&mut self) -> Result<BTreeSet<String>, String> {
        let mut named = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let prefix = format!("{}/", self.output);
        // The records of the preparations of the project, and those of
        // `run --prepare-only` on this output (caches6-2).
        let mut records = directories(&self.preparations, |_| true);
        records.push(sysutil::join(&[&self.output, PREPARED]));
        for preparation in records {
            let Ok(items) = fs::read_dir(&preparation) else {
                continue;
            };
            for item in items.flatten() {
                let path = sysutil::join(&[&preparation, &item.file_name().to_string_lossy()]);
                let Ok(info) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if !info.is_file() || !path.ends_with(".json") {
                    continue;
                }
                seen.insert(path.clone());
                let stamp = Stamp::of(&info);
                if let Some((known, paths)) = self.read.get(&path)
                    && *known == stamp
                {
                    named.extend(paths.iter().cloned());
                    continue;
                }
                if info.len() > PREPARATION_LIMIT {
                    return Err(path);
                }
                let Ok(text) = fs::read(&path) else {
                    return Err(path);
                };
                let text = String::from_utf8_lossy(&text);
                let mut paths = BTreeSet::new();
                for (start, _) in text.match_indices(&prefix) {
                    let rest = &text[start..];
                    let end = rest.find(['"', '\n']).unwrap_or(rest.len());
                    paths.insert(rest[..end].to_string());
                }
                named.extend(paths.iter().cloned());
                self.read.insert(path, (stamp, paths));
            }
        }
        self.read.retain(|path, _| seen.contains(path));
        Ok(named)
    }

    /// Removes the Player directories of the output that nothing uses.
    /// `keep` names files or directories that must stay with the
    /// directories that hold them: the staged host and the prepared
    /// baseline of the session, the Player directory of the running
    /// process, of the newest compile and of the compile it was started on.
    /// Also kept: a directory that holds the executable of any running
    /// process, one that a recorded preparation names, and one that a kept
    /// revision entry names. Must not run beside a compile or a Player start.
    pub fn prune(&mut self, keep: &[String]) -> Pruned {
        let mut pruned = Pruned::default();
        let recorded = match self.recorded() {
            Ok(recorded) => recorded,
            Err(path) => {
                pruned.refused = path;
                return pruned;
            }
        };
        self.bound_files(keep, &recorded, &mut pruned);
        let compiler = sysutil::join(&[&self.output, "compiler"]);
        let mut candidates = directories(&sysutil::join(&[&self.output, "players"]), |_| true);
        candidates.extend(directories(&compiler, |name| {
            name.starts_with(".code-revision-")
        }));
        if candidates.is_empty() {
            return pruned;
        }
        let running = running_executables();
        let used = |directory: &str| {
            keep.iter()
                .chain(&running)
                .chain(&recorded)
                .any(|path| holds(directory, path))
        };
        // Revision entries, newest first: the first few stay, and so does
        // one whose directory is in use for another reason.
        let revisions = sysutil::join(&[&compiler, "code-revisions"]);
        let mut entries: Vec<(SystemTime, String, String)> = fs::read_dir(&revisions)
            .map(|items| {
                items
                    .flatten()
                    .map(|item| sysutil::join(&[&revisions, &item.file_name().to_string_lossy()]))
                    .filter(|path| path.ends_with(".json"))
                    .map(|path| {
                        let player = fs::metadata(&path)
                            .ok()
                            .filter(|info| info.is_file() && info.len() <= 8 << 20)
                            .and_then(|_| crate::util::read_json_file::<Entry>(&path).ok())
                            .map(|entry| entry.player)
                            .filter(|player| !player.is_empty() && !sysutil::is_abs(player))
                            .map(|player| sysutil::join(&[&compiler, &player]))
                            .unwrap_or_default();
                        (modified(&path), path, player)
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut named = Vec::new();
        for (index, (_, path, player)) in entries.iter().enumerate() {
            let in_use = !player.is_empty()
                && candidates
                    .iter()
                    .any(|directory| within_directory(player, directory) && used(directory));
            if index < KEPT_REVISIONS || in_use {
                named.push(player.clone());
            } else if fs::remove_file(path).is_ok() {
                pruned.entries.push(path.clone());
            }
        }
        let mut unpublished: Vec<(SystemTime, String)> = Vec::new();
        for directory in candidates {
            if used(&directory) || named.iter().any(|player| holds(&directory, player)) {
                pruned.kept.push(directory);
                continue;
            }
            // A compile writes compile-update.json before it publishes.
            let rejected = within_directory(&directory, &compiler)
                && fs::symlink_metadata(sysutil::join(&[&directory, "compile-update.json"]))
                    .is_err();
            if rejected {
                unpublished.push((modified(&directory), directory));
                continue;
            }
            remove(&directory, &mut pruned);
        }
        unpublished.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (index, (_, directory)) in unpublished.into_iter().enumerate() {
            if index < KEPT_UNPUBLISHED {
                pruned.kept.push(directory);
            } else {
                remove(&directory, &mut pruned);
            }
        }
        pruned
    }

    /// The scene revisions, the compiled assemblies and the run directories
    /// of the output, one per scene save, per source state and per Player
    /// process: the newest stay, with those `keep` names (the running
    /// Player's scene and run, the compiler's current assemblies) and those
    /// a record names (the manifest of a prepare-only run). A scene revision
    /// and an assembly are made again when they are needed and gone
    /// (caches6-3).
    fn bound_files(&self, keep: &[String], recorded: &BTreeSet<String>, pruned: &mut Pruned) {
        for (parent, kept) in [
            (sysutil::join(&[&self.output, "revisions"]), KEPT_SCENE_REVISIONS),
            (
                sysutil::join(&[&self.output, "compiler", "assemblies"]),
                KEPT_ASSEMBLIES,
            ),
            (sysutil::join(&[&self.output, "runs"]), KEPT_RUNS),
        ] {
            let mut entries: Vec<(SystemTime, String)> = directories(&parent, |_| true)
                .into_iter()
                .map(|directory| (modified(&directory), directory))
                .collect();
            if entries.len() <= kept {
                continue;
            }
            entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for (_, directory) in entries.into_iter().skip(kept) {
                if keep.iter().chain(recorded).any(|path| holds(&directory, path)) {
                    continue;
                }
                if fs::remove_dir_all(&directory).is_ok() {
                    pruned.files += 1;
                }
            }
        }
    }
}

/// One line on standard error when anything was removed or a preparation
/// could not be read.
pub(crate) fn report(pruned: &Pruned) {
    if !pruned.refused.is_empty() {
        eprintln!(
            "[unity-micro-editor] unused Player directories are kept: the preparation {} cannot be read",
            pruned.refused
        );
    }
    if !pruned.removed.is_empty() || !pruned.entries.is_empty() {
        eprintln!(
            "[unity-micro-editor] removed {} unused Player directories and {} revision entries; {} kept",
            pruned.removed.len(),
            pruned.entries.len(),
            pruned.kept.len()
        );
    }
    if pruned.files > 0 {
        eprintln!(
            "[unity-micro-editor] removed {} scene revisions, compiled assemblies and run directories beyond the newest",
            pruned.files
        );
    }
}

fn remove(directory: &str, pruned: &mut Pruned) {
    match fs::remove_dir_all(directory) {
        Ok(()) => pruned.removed.push(directory.into()),
        Err(error) => eprintln!(
            "[unity-micro-editor] could not remove the unused Player directory {directory}: {error}"
        ),
    }
}
