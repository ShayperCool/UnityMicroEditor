//! Conservative, explicit-project retention for preparation receipts and old run output.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

const JSON_LIMIT: u64 = 256 << 20;

pub fn cli(args: Vec<String>) -> Result<()> {
    let mut project = None;
    let mut apply = false;
    let mut days = 14u64;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => project = args.next(),
            "--apply" => apply = true,
            "--max-age-days" => days = args.next().context("missing days")?.parse()?,
            _ => anyhow::bail!("cache-prune --project PATH [--max-age-days 14] [--apply]"),
        }
    }
    ensure!(days >= 7, "retention must be at least seven days");
    let project = fs::canonicalize(project.context("an explicit --project is required")?)?;
    ensure!(
        project.join("Assets").is_dir() && project.join("ProjectSettings").is_dir(),
        "expected a Unity project"
    );
    let root = project.join("Library/UnityMicroEditor");
    ensure!(
        fs::canonicalize(&root)? == root,
        "cache root must not traverse a symlink"
    );
    let _lock = lock(&root)?;
    ensure!(
        !active(&project)?,
        "project has an active process; nothing removed"
    );
    let candidates = plan(
        &root,
        SystemTime::now(),
        Duration::from_secs(days.checked_mul(86400).context("retention overflow")?),
    )?;
    if apply {
        // Hold the same lock as project preparation/publication throughout planning and deletion.
        ensure!(!active(&project)?, "project became active; nothing removed");
        remove_candidates(&root, &candidates)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"project": project, "applied": apply, "candidates": candidates, "keepSuccessfulPerConfiguration": 2, "maxAgeDays": days})
        )?
    );
    Ok(())
}

fn remove_candidates(root: &Path, candidates: &[PathBuf]) -> Result<()> {
    // Validate the complete set before starting any deletion. Candidates are disjoint
    // top-level entries, so workers cannot remove a parent of another worker's entry.
    for path in candidates {
        ensure!(
            path.starts_with(root)
                && path != root
                && fs::canonicalize(path)? == *path
                && fs::symlink_metadata(path)?.is_dir(),
            "candidate changed: {}",
            path.display()
        );
        ensure!(
            !candidates
                .iter()
                .any(|other| other != path && path.starts_with(other)),
            "nested deletion candidates"
        );
    }
    std::thread::scope(|scope| -> Result<()> {
        let handles: Vec<_> = (0..4)
            .map(|worker| {
                scope.spawn(move || -> Result<()> {
                    for path in candidates.iter().skip(worker).step_by(4) {
                        fs::remove_dir_all(path)
                            .with_context(|| format!("remove {}", path.display()))?;
                        eprintln!("cache-prune removed {}", path.display());
                    }
                    Ok(())
                })
            })
            .collect();
        let mut errors = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(format!("{error:#}")),
                Err(_) => errors.push("cache pruning worker panicked".to_string()),
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    })
}

fn lock(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(root.join("project-actions.lock"))?;
    // SAFETY: file owns a valid descriptor, retained until all pruning completes.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "project actions are locked; nothing removed"
    );
    Ok(file)
}

fn active(project: &Path) -> Result<bool> {
    let own = std::process::id().to_string();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let pid = entry.file_name();
        let pid = pid.to_string_lossy();
        if pid == own || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if fs::read_link(entry.path().join("exe")).is_ok_and(|p| p.starts_with(project)) {
            return Ok(true);
        }
        // Helpers and Editors often run from outside the project, with its path as an argument.
        let Ok(cmd) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let parts: Vec<_> = cmd
            .split(|b| *b == 0)
            .filter_map(|b| std::str::from_utf8(b).ok())
            .collect();
        let exe = parts.first().copied().unwrap_or_default();
        let relevant = [
            "Unity",
            "unity-micro-editor",
            "dotnet",
            "mono",
            "UnityShaderCompiler",
            "UnityPackageManager",
        ]
        .iter()
        .any(|name| Path::new(exe).file_name().is_some_and(|f| f == *name));
        if relevant
            && (fs::read_link(entry.path().join("cwd")).is_ok_and(|p| p.starts_with(project))
                || parts
                    .iter()
                    .skip(1)
                    .any(|p| Path::new(p).starts_with(project)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn directories(parent: &Path) -> Result<Vec<PathBuf>> {
    if !parent.exists() {
        return Ok(Vec::new());
    }
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "symlink directory: {}",
        parent.display()
    );
    let mut result = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            result.push(entry.path());
        }
    }
    result.sort();
    Ok(result)
}

fn read_json(path: &Path) -> Result<Value> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.len() <= JSON_LIMIT,
        "unsafe or oversized receipt: {}",
        path.display()
    );
    serde_json::from_slice(&fs::read(path)?).with_context(|| format!("parse {}", path.display()))
}

fn strings(value: &Value, found: &mut BTreeSet<PathBuf>) {
    match value {
        Value::String(s) if s.starts_with('/') => {
            let path = PathBuf::from(s);
            // Never interpret parent components as a reference into an owned cache.
            if !path.components().any(|c| c == Component::ParentDir) {
                found.insert(path);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| strings(v, found)),
        Value::Object(o) => {
            for (key, value) in o {
                strings(&Value::String(key.clone()), found);
                strings(value, found);
            }
        }
        _ => {}
    }
}

fn references(directory: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "json") {
            strings(&read_json(&entry.path())?, &mut found);
        }
    }
    let root = directory
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "UnityMicroEditor"));
    if let Some(root) = root {
        let mut visited = BTreeSet::new();
        loop {
            let pending: Vec<_> = found
                .difference(&visited)
                .filter(|p| {
                    p.starts_with(root) && p.extension().is_some_and(|e| e == "json") && p.exists()
                })
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            for path in pending {
                strings(&read_json(&path)?, &mut found);
                visited.insert(path);
            }
        }
    }
    Ok(found)
}

fn old(path: &Path, now: SystemTime, age: Duration) -> Result<bool> {
    Ok(now
        .duration_since(fs::symlink_metadata(path)?.modified()?)
        .is_ok_and(|d| d >= age))
}

fn plan(root: &Path, now: SystemTime, age: Duration) -> Result<Vec<PathBuf>> {
    let preparations = directories(&root.join("preparations"))?;
    let mut kept = BTreeSet::new();
    let mut groups: BTreeMap<String, Vec<(SystemTime, PathBuf)>> = BTreeMap::new();
    for path in &preparations {
        if !path.join("request.json").is_file() {
            kept.insert(path.clone());
            continue;
        }
        let _ = read_json(&path.join("request.json"))?;
        if path.join(".keep").exists()
            || (!path.join("result.json").exists() && !old(path, now, age)?)
        {
            kept.insert(path.clone());
        }
        let result = path.join("result.json");
        if result.exists() {
            let result = read_json(&result)?;
            let request = read_json(&path.join("request.json"))?;
            ensure!(
                result.get("scene").and_then(Value::as_str).is_some(),
                "missing scene: {}",
                path.display()
            );
            // Include engine, backend and explicitly configured inputs; do not merge incompatible hosts.
            let key = serde_json::to_string(&json!([
                result["scene"],
                request["editor"],
                request["backend"],
                request["configuredEngineProfile"],
                request["configuredHostPlayer"],
                request["nativeCatalog"]
            ]))?;
            groups.entry(key).or_default().push((
                fs::metadata(path.join("result.json"))?.modified()?,
                path.clone(),
            ));
        }
    }
    for group in groups.values_mut() {
        group.sort_by(|a, b| b.cmp(a));
        kept.extend(group.iter().take(2).map(|(_, p)| p.clone()));
    }
    // Import worker contexts contain only owned runtime/artifact staging. Keep two warm
    // contexts, all pins, and any context named by retained receipts. Unknown layouts stay.
    let mut imports = directories(&root.join("native-store/imports"))?;
    imports.sort_by_cached_key(|p| {
        std::cmp::Reverse(
            fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    });
    let mut warm_counts = BTreeMap::new();
    for path in &imports {
        let class = if worker(path, "warm") {
            "warm"
        } else if worker(path, "build-0") {
            "build"
        } else if path.join("runtime").is_dir() && path.join("artifacts").is_dir() {
            "runtime"
        } else if path.join("project-context.json").is_file() && path.join("artifacts").is_dir() {
            "incomplete"
        } else {
            "unknown"
        };
        let count = warm_counts.entry(class).or_insert(0);
        *count += 1;
        if *count <= 2
            || class == "unknown"
            || path.join(".keep").exists()
            || (class == "incomplete" && !old(path, now, age)?)
        {
            kept.insert(path.clone());
        }
    }
    let mut runs = imports;

    // Keep the newest two runs in each session even if they are older than the TTL.
    for session in directories(&root.join("Sessions"))? {
        let mut entries = directories(&session.join("runs"))?;
        entries.sort_by_cached_key(|p| {
            std::cmp::Reverse(
                fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
            )
        });
        for (i, path) in entries.into_iter().enumerate() {
            if i < 2 || path.join(".keep").exists() || !old(&path, now, age)? {
                kept.insert(path);
            } else {
                runs.push(path);
            }
        }
        // Prepared launches and session-level state can refer to an old preparation or run.
        for record_dir in [session.clone(), session.join("prepared")] {
            if record_dir.is_dir() {
                for r in references(&record_dir)? {
                    protect(&r, &preparations, &runs, &mut kept);
                }
            }
        }
    }
    // Explicit pins are absolute paths in a JSON array. A directory pin protects its descendants.
    let pins = root.join("retention-pins.json");
    if pins.exists() {
        let v = read_json(&pins)?;
        ensure!(
            v.as_array()
                .is_some_and(|a| a
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| Path::new(s).is_absolute()
                        && !Path::new(s).components().any(|c| c == Component::ParentDir)))),
            "pins must be an array of paths"
        );
        let mut refs = BTreeSet::new();
        strings(&v, &mut refs);
        for r in refs {
            protect(&r, &preparations, &runs, &mut kept);
        }
    }
    for r in references(root)? {
        protect(&r, &preparations, &runs, &mut kept);
    }
    let mut visited = BTreeSet::new();
    loop {
        let unread: Vec<_> = kept.difference(&visited).cloned().collect();
        if unread.is_empty() {
            break;
        }
        for path in unread {
            let mut refs = references(&path)?;
            for child in directories(&path)? {
                let name = child.file_name().unwrap().to_string_lossy();
                if name == "warm" || name.starts_with("build-") || name.starts_with("graph-") {
                    refs.extend(references(&child)?);
                }
            }
            for r in refs {
                protect(&r, &preparations, &runs, &mut kept);
            }
            visited.insert(path);
        }
    }
    Ok(preparations
        .into_iter()
        .chain(runs)
        .filter(|p| !kept.contains(p))
        .collect())
}

fn worker(path: &Path, child: &str) -> bool {
    path.join(child).join("project-context.json").is_file()
        && path.join(child).join("project").is_dir()
}

fn protect(
    reference: &Path,
    preparations: &[PathBuf],
    runs: &[PathBuf],
    kept: &mut BTreeSet<PathBuf>,
) {
    for path in preparations.iter().chain(runs) {
        if reference.starts_with(path) {
            kept.insert(path.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn prep(root: &Path, name: &str, scene: &str, reference: &str) -> PathBuf {
        let p = root.join("preparations").join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(
            p.join("request.json"),
            json!({"editor":"6000.6.3f1","backend":"source"}).to_string(),
        )
        .unwrap();
        fs::write(
            p.join("result.json"),
            json!({"scene":scene,"referenceGraph":reference}).to_string(),
        )
        .unwrap();
        static ORDER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let order = ORDER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        File::options()
            .write(true)
            .open(p.join("result.json"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + order))
            .unwrap();
        p
    }
    #[test]
    fn keeps_two_per_scene_and_transitive_dependencies() {
        let f = fixture();
        let r = f.path();
        let dependency = prep(r, "a", "A", "");
        let stale = prep(r, "b", "A", "");
        prep(
            r,
            "c",
            "A",
            dependency.join("result.json").to_str().unwrap(),
        );
        prep(r, "d", "A", "");
        prep(r, "other-scene", "B", "");
        let plan = plan(
            r,
            SystemTime::now() + Duration::from_secs(30 * 86400),
            Duration::from_secs(14 * 86400),
        )
        .unwrap();
        assert_eq!(plan, vec![stale]);
    }
    #[test]
    fn pins_recent_and_incomplete_preparations() {
        let f = fixture();
        let r = f.path();
        let p = prep(r, "pinned", "A", "");
        fs::write(p.join(".keep"), "").unwrap();
        prep(r, "new1", "A", "");
        prep(r, "new2", "A", "");
        fs::create_dir_all(r.join("preparations/incomplete")).unwrap();
        fs::write(r.join("preparations/incomplete/request.json"), "{}").unwrap();
        assert!(
            plan(r, SystemTime::now(), Duration::from_secs(14 * 86400))
                .unwrap()
                .is_empty()
        );
        let candidates = plan(
            r,
            SystemTime::now() + Duration::from_secs(30 * 86400),
            Duration::from_secs(14 * 86400),
        )
        .unwrap();
        assert_eq!(candidates, vec![r.join("preparations/incomplete")]);
    }
    #[test]
    fn invalid_receipts_and_symlink_parents_refuse_deletion() {
        let f = fixture();
        let r = f.path();
        let p = prep(r, "bad", "A", "");
        fs::write(p.join("result.json"), "broken").unwrap();
        assert!(plan(r, SystemTime::now(), Duration::ZERO).is_err());
        fs::remove_dir_all(r.join("preparations")).unwrap();
        std::os::unix::fs::symlink("/tmp", r.join("preparations")).unwrap();
        assert!(plan(r, SystemTime::now(), Duration::ZERO).is_err());
    }
    #[test]
    fn import_context_references_and_unknown_layouts_are_kept() {
        let f = fixture();
        let r = f.path().join("UnityMicroEditor");
        fs::create_dir(&r).unwrap();
        let mut contexts = Vec::new();
        for i in 0..4 {
            let p = r.join(format!("native-store/imports/{i}"));
            fs::create_dir_all(p.join("runtime")).unwrap();
            fs::create_dir(p.join("artifacts")).unwrap();
            File::open(&p)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + i))
                .unwrap();
            contexts.push(p);
        }
        let p = prep(
            &r,
            "current",
            "A",
            contexts[0].join("artifacts/index.json").to_str().unwrap(),
        );
        fs::write(contexts[0].join("artifacts/index.json"), "{}").unwrap();
        fs::write(
            p.join("artifacts.json"),
            json!({"files": {contexts[0].join("runtime/file").to_str().unwrap(): "hash"}})
                .to_string(),
        )
        .unwrap();
        let unknown = r.join("native-store/imports/unknown");
        fs::create_dir_all(&unknown).unwrap();
        let candidates = plan(&r, SystemTime::now(), Duration::from_secs(14 * 86400)).unwrap();
        assert!(!candidates.contains(&contexts[0]));
        assert!(!candidates.contains(&unknown));
        assert!(candidates.contains(&contexts[1]));
    }
    #[test]
    fn worker_contexts_keep_two_and_referenced_older_contexts() {
        for stage in ["warm", "build-0"] {
            let f = fixture();
            let r = f.path().join("UnityMicroEditor");
            fs::create_dir(&r).unwrap();
            let mut contexts = Vec::new();
            for i in 0..4 {
                let p = r.join(format!("native-store/imports/{i}"));
                fs::create_dir_all(p.join(format!("{stage}/project"))).unwrap();
                fs::write(p.join(format!("{stage}/project-context.json")), "{}").unwrap();
                File::open(&p)
                    .unwrap()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + i))
                    .unwrap();
                contexts.push(p);
            }
            prep(
                &r,
                "current",
                "A",
                contexts[0]
                    .join("warm/project-context.json")
                    .to_str()
                    .unwrap(),
            );
            assert_eq!(
                plan(&r, SystemTime::now(), Duration::from_secs(14 * 86400)).unwrap(),
                vec![contexts[1].clone()]
            );
        }
    }
    #[test]
    fn parallel_deletion_preserves_symlink_targets_and_refuses_nested_candidates() {
        let f = fixture();
        let root = f.path().join("cache");
        fs::create_dir(&root).unwrap();
        let outside = f.path().join("source");
        fs::write(&outside, "authored source").unwrap();
        let mut candidates = Vec::new();
        for i in 0..8 {
            let p = root.join(i.to_string());
            fs::create_dir(&p).unwrap();
            std::os::unix::fs::symlink(&outside, p.join("link")).unwrap();
            candidates.push(p);
        }
        assert!(
            remove_candidates(&root, &[candidates[0].clone(), candidates[0].join("link")]).is_err()
        );
        assert!(candidates.iter().all(|p| p.exists()));
        remove_candidates(&root, &candidates).unwrap();
        assert_eq!(fs::read_to_string(outside).unwrap(), "authored source");
        assert!(candidates.iter().all(|p| !p.exists()));
    }
    #[test]
    fn project_action_lock_is_shared_with_preparation() {
        let f = fixture();
        let _first = lock(f.path()).unwrap();
        assert!(lock(f.path()).is_err());
    }
}
