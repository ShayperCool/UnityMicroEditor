//! Tests of the bound of a session output.
use super::prune::{KEPT_REVISIONS, KEPT_UNPUBLISHED, Pruner};
use crate::sysutil;
use std::{fs, process::Command, thread, time::Duration};

struct Output {
    _guard: tempfile::TempDir,
    project: String,
    output: String,
}

fn output() -> Output {
    let guard = tempfile::TempDir::new().unwrap();
    let root = sysutil::eval_symlinks(&guard.path().to_string_lossy()).unwrap();
    let project = sysutil::join(&[&root, "Project"]);
    let output = sysutil::join(&[&project, "Library/UnityMicroEditor/agent"]);
    fs::create_dir_all(sysutil::join(&[&output, "compiler/code-revisions"])).unwrap();
    fs::create_dir_all(sysutil::join(&[
        &project,
        "Library/UnityMicroEditor/preparations",
    ]))
    .unwrap();
    Output {
        _guard: guard,
        project,
        output,
    }
}

/// A Player directory as a compile publishes one, with its revision entry
/// when `entry` is given. Returns the Player executable.
fn published(output: &Output, id: &str, entry: Option<&str>) -> String {
    let directory = sysutil::join(&[&output.output, "compiler", &format!(".code-revision-{id}")]);
    let player = sysutil::join(&[&directory, "Player/Project.x86_64"]);
    fs::create_dir_all(sysutil::dir(&player)).unwrap();
    fs::write(&player, "player").unwrap();
    fs::write(sysutil::join(&[&directory, "compile-update.json"]), "{}").unwrap();
    if let Some(key) = entry {
        let path = sysutil::join(&[
            &output.output,
            "compiler/code-revisions",
            &format!("{key}.json"),
        ]);
        fs::write(
            &path,
            format!(r#"{{"schemaVersion":1,"key":"{key}","player":".code-revision-{id}/Player/Project.x86_64"}}"#),
        )
        .unwrap();
        // Entries are ordered by their modification time.
        thread::sleep(Duration::from_millis(15));
    }
    player
}

fn staged(output: &Output, id: &str) -> String {
    let player = sysutil::join(&[&output.output, "players", id, "Project.x86_64"]);
    fs::create_dir_all(sysutil::dir(&player)).unwrap();
    fs::write(&player, "host").unwrap();
    player
}

fn exists(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn revision_directory(player: &str) -> String {
    sysutil::dir(&sysutil::dir(player))
}

#[test]
fn prune_keeps_the_baseline_and_what_the_session_names() {
    let output = output();
    let current = staged(&output, "current");
    let earlier = staged(&output, "earlier");
    let running = published(&output, "running", None);
    let unused = published(&output, "unused", None);
    let mut pruner = Pruner::new(&output.project, &output.output);
    let pruned = pruner.prune(&[current.clone(), running.clone()]);
    assert!(
        exists(&current),
        "the staged host of the session was removed"
    );
    assert!(exists(&running), "the Player the session runs was removed");
    assert!(
        !exists(&sysutil::dir(&earlier)),
        "an earlier staged host stayed"
    );
    assert!(
        !exists(&revision_directory(&unused)),
        "an unused Player stayed"
    );
    assert_eq!(pruned.removed.len(), 2, "{pruned:?}");
}

#[test]
fn prune_keeps_a_directory_a_running_process_was_started_from() {
    let output = output();
    let player = published(&output, "foreign", None);
    // A process of this test, started from inside the directory.
    let executable = sysutil::join(&[&sysutil::dir(&player), "sleep"]);
    fs::copy("/bin/sleep", &executable).unwrap();
    // Another test forking meanwhile may hold the copy open for writing until its exec.
    let started = std::time::Instant::now();
    let mut child = loop {
        match Command::new(&executable).arg("30").spawn() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            spawned => break spawned.unwrap(),
        }
    };
    let started = std::time::Instant::now();
    while fs::read_link(format!("/proc/{}/exe", child.id()))
        .map(|path| path.to_string_lossy().into_owned())
        .ok()
        .as_deref()
        != Some(executable.as_str())
        && started.elapsed() < Duration::from_secs(5)
    {
        thread::sleep(Duration::from_millis(5));
    }
    let pruned = Pruner::new(&output.project, &output.output).prune(&[]);
    let kept = exists(&player);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        kept,
        "the directory of a running process was removed: {pruned:?}"
    );
    let pruned = Pruner::new(&output.project, &output.output).prune(&[]);
    assert!(
        !exists(&player),
        "a directory nothing runs from stayed: {pruned:?}"
    );
}

#[test]
fn prune_keeps_what_a_recorded_preparation_names() {
    let output = output();
    let named = published(&output, "named", None);
    let other = published(&output, "other", None);
    let preparation = sysutil::join(&[&output.project, "Library/UnityMicroEditor/preparations/p1"]);
    fs::create_dir_all(&preparation).unwrap();
    let current = sysutil::join(&[&preparation, "current.json"]);
    fs::write(
        &current,
        format!(r#"{{"schemaVersion":1,"player":"{named}"}}"#),
    )
    .unwrap();
    let mut pruner = Pruner::new(&output.project, &output.output);
    pruner.prune(&[]);
    assert!(exists(&named), "a Player a preparation names was removed");
    assert!(!exists(&other));
    // The preparation is read again once it changed.
    fs::write(&current, r#"{"schemaVersion":1,"player":"/elsewhere"}"#).unwrap();
    pruner.prune(&[]);
    assert!(
        !exists(&named),
        "a Player no preparation names any more stayed"
    );
}

#[test]
fn prune_keeps_the_newest_revision_entries_and_their_directories() {
    let output = output();
    let mut players = Vec::new();
    for index in 0..KEPT_REVISIONS + 3 {
        players.push(published(
            &output,
            &format!("r{index}"),
            Some(&format!("key{index}")),
        ));
    }
    let mut pruner = Pruner::new(&output.project, &output.output);
    // The oldest is the running one; its entry stays as well.
    let pruned = pruner.prune(&[players[0].clone()]);
    let entry = |index: usize| {
        sysutil::join(&[
            &output.output,
            &format!("compiler/code-revisions/key{index}.json"),
        ])
    };
    for (index, player) in players.iter().enumerate() {
        let newest = index >= players.len() - KEPT_REVISIONS;
        let expected = newest || index == 0;
        assert_eq!(exists(player), expected, "Player {index}: {pruned:?}");
        assert_eq!(exists(&entry(index)), expected, "entry {index}: {pruned:?}");
    }
    assert_eq!(pruned.entries.len(), 2, "{pruned:?}");
}

#[test]
fn prune_bounds_the_output_and_keeps_the_newest_unpublished_compiles() {
    let output = output();
    let current = staged(&output, "current");
    let mut players = Vec::new();
    for index in 0..20 {
        staged(&output, &format!("old{index}"));
        players.push(published(
            &output,
            &format!("p{index}"),
            Some(&format!("k{index}")),
        ));
        let failed = sysutil::join(&[
            &output.output,
            "compiler",
            &format!(".code-revision-f{index}/Sources"),
        ]);
        fs::create_dir_all(&failed).unwrap();
        thread::sleep(Duration::from_millis(2));
    }
    let running = players[3].clone();
    let newest = players.last().unwrap().clone();
    Pruner::new(&output.project, &output.output).prune(&[current, running, newest]);
    let left = fs::read_dir(sysutil::join(&[&output.output, "compiler"]))
        .unwrap()
        .flatten()
        .filter(|item| {
            item.file_name()
                .to_string_lossy()
                .starts_with(".code-revision-")
        })
        .count();
    // The newest entries (the newest compile among them), the running one,
    // and the newest compiles that published nothing.
    assert_eq!(left, KEPT_REVISIONS + 1 + KEPT_UNPUBLISHED);
    let hosts = fs::read_dir(sysutil::join(&[&output.output, "players"]))
        .unwrap()
        .count();
    assert_eq!(hosts, 1, "earlier staged hosts stayed");
    assert!(exists(&sysutil::join(&[
        &output.output,
        "compiler/.code-revision-f19"
    ])));
    assert!(!exists(&sysutil::join(&[
        &output.output,
        "compiler/.code-revision-f0"
    ])));
}

/// The importers of a session read the built-in shaders of the host from
/// the data of the Player they were made with. A Player directory they
/// read is kept, also when it is a compile of the session's start that no
/// Player runs any more and whose revision entry is gone (caches6-1).
#[test]
fn prune_keeps_the_player_the_importers_read() {
    let output = output();
    let host = staged(&output, "current");
    let start = published(&output, "start", None);
    let running = published(&output, "running", None);
    let keep = crate::runner::in_use(
        std::slice::from_ref(&host),
        Some(&running),
        &host,
        std::iter::once(running.as_str()),
        &[start.as_str()],
    );
    let pruned = Pruner::new(&output.project, &output.output).prune(&keep);
    assert!(
        exists(&start),
        "a Player the importers read was removed: {pruned:?}"
    );
    let keep = crate::runner::in_use(
        std::slice::from_ref(&host),
        Some(&running),
        &host,
        std::iter::once(running.as_str()),
        &[host.as_str()],
    );
    let pruned = Pruner::new(&output.project, &output.output).prune(&keep);
    assert!(
        !exists(&start) && exists(&host) && exists(&running),
        "{pruned:?}"
    );
}

/// `run --prepare-only` records the Player it prepared; a later session on
/// the same output keeps it for a `--prepared` launch, and the newest
/// records stay (caches6-2).
#[test]
fn prune_keeps_what_a_prepare_only_run_recorded() {
    let output = output();
    let prepared = staged(&output, "prepared");
    let other = staged(&output, "other");
    crate::runner::prune::record_prepared(
        &output.output,
        &serde_json::json!({"schemaVersion": 1, "player": prepared}),
    )
    .unwrap();
    let pruned = Pruner::new(&output.project, &output.output).prune(&[]);
    assert!(
        exists(&prepared),
        "the prepared Player was removed: {pruned:?}"
    );
    assert!(!exists(&other), "{pruned:?}");
    for _ in 0..super::prune::KEPT_PREPARED {
        thread::sleep(Duration::from_millis(15));
        crate::runner::prune::record_prepared(
            &output.output,
            &serde_json::json!({"schemaVersion": 1}),
        )
        .unwrap();
    }
    let records = fs::read_dir(sysutil::join(&[&output.output, super::prune::PREPARED]))
        .unwrap()
        .count();
    assert_eq!(records, super::prune::KEPT_PREPARED);
    Pruner::new(&output.project, &output.output).prune(&[]);
    assert!(
        !exists(&prepared),
        "a Player of a record that is gone stayed"
    );
}

/// Scene revisions and entries of the assembly cache grow by one per scene
/// save and per source state; the newest stay, with what the session uses
/// and what a record names (caches6-3).
#[test]
fn prune_bounds_scene_revisions_and_compiled_assemblies() {
    use super::prune::{KEPT_ASSEMBLIES, KEPT_SCENE_REVISIONS};
    let output = output();
    let make = |parent: &str, count: usize| -> Vec<String> {
        (0..count)
            .map(|index| {
                let directory = sysutil::join(&[&output.output, parent, &format!("{index:03}")]);
                fs::create_dir_all(&directory).unwrap();
                let file = sysutil::join(&[&directory, "scene.json"]);
                fs::write(&file, "{}").unwrap();
                // Ordered by their modification time, the first the oldest.
                let time = std::time::SystemTime::UNIX_EPOCH
                    + Duration::from_secs(1_000_000 + index as u64);
                fs::File::open(&directory)
                    .unwrap()
                    .set_modified(time)
                    .unwrap();
                file
            })
            .collect()
    };
    let scenes = make("revisions", KEPT_SCENE_REVISIONS + 3);
    let assemblies = make("compiler/assemblies", KEPT_ASSEMBLIES + 2);
    let runs = make("runs", super::prune::KEPT_RUNS + 2);
    crate::runner::prune::record_prepared(
        &output.output,
        &serde_json::json!({"schemaVersion": 1, "manifest": scenes[1]}),
    )
    .unwrap();
    let keep = vec![
        scenes[0].clone(),
        assemblies[0].clone(),
        sysutil::dir(&runs[0]),
    ];
    let pruned = Pruner::new(&output.project, &output.output).prune(&keep);
    let left = |files: &[String]| files.iter().filter(|file| exists(file)).count();
    assert_eq!(left(&scenes), KEPT_SCENE_REVISIONS + 2, "{pruned:?}");
    assert_eq!(left(&assemblies), KEPT_ASSEMBLIES + 1, "{pruned:?}");
    assert!(exists(&scenes[0]) && exists(&scenes[1]) && !exists(&scenes[2]));
    assert!(exists(&assemblies[0]) && !exists(&assemblies[1]));
    assert!(exists(scenes.last().unwrap()) && exists(assemblies.last().unwrap()));
    assert!(exists(&runs[0]) && !exists(&runs[1]) && left(&runs) == super::prune::KEPT_RUNS + 1);
    assert_eq!(pruned.files, 1 + 1 + 1);
}
