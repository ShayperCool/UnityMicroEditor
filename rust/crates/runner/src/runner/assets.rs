//! What a Player of Play mode code asks the asset database for while it
//! runs (`Runtime/PlayModeEditor.cs`). An asset that is neither in a
//! `Resources` folder nor among those the session wrote is written from its
//! source by a worker of its own, while the Player waits, and loaded by the
//! Player. The project remembers it, and later sessions write it with their
//! scene.
use super::{Event, err_text};
use crate::{
    cancel::Context,
    native::{NativeProjectImporter, package_directories},
    sysutil,
    util::atomic_json,
};
use anyhow::{Result, anyhow};
use std::{
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Where the Player of a session puts its requests.
pub fn requests_directory(output: &str) -> String {
    sysutil::join(&[output, "play-mode-assets"])
}

/// The packages the Player finds by `Packages/<name>`, one per line: the
/// logical root, a tab, its directory.
pub fn packages_file(output: &str) -> String {
    sysutil::join(&[output, "play-mode-packages.tsv"])
}

/// The assets the Players of the project's sessions asked for.
fn record(project: &str) -> String {
    sysutil::join(&[project, "Library/UnityMicroEditor/play-mode-assets.json"])
}

/// Writes where the project's packages lie, as its Editor compilation found
/// them, for the Player of `output`.
pub fn write_packages(project: &str, output: &str) -> Result<()> {
    let (library, packages) = package_directories(project)?;
    let mut text = String::new();
    for (root, directory) in packages {
        let physical = match sysutil::is_abs(&directory) {
            true => directory,
            false => sysutil::join(&[&library, &directory]),
        };
        if root.contains(['\t', '\n']) || physical.contains(['\t', '\n']) {
            continue;
        }
        text.push_str(&format!("{root}\t{physical}\n"));
    }
    std::fs::write(packages_file(output), text)
        .map_err(|error| anyhow!("write the package list of the Player: {error}"))
}

/// The assets earlier Players asked for whose sources are still there.
pub fn remembered(project: &str) -> Vec<String> {
    let paths: Vec<String> = std::fs::read(record(project))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    let packages = package_directories(project)
        .map(|(_, packages)| packages)
        .unwrap_or_default();
    paths
        .into_iter()
        .filter(|path| {
            let exists = |file: &str| std::fs::metadata(file).is_ok();
            match path.strip_prefix("Packages/") {
                None => exists(&sysutil::join(&[project, path])),
                Some(_) => packages.iter().any(|(root, _)| {
                    path.strip_prefix(root.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
                }),
            }
        })
        .collect()
}

pub(super) fn remember(project: &str, path: &str) -> Result<()> {
    let file = record(project);
    let mut paths: Vec<String> = std::fs::read(&file)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    if !paths.iter().any(|known| known == path) {
        paths.push(path.to_string());
        paths.sort();
        atomic_json(&file, &paths)?;
    }
    Ok(())
}

/// Answers the requests of the session's Players until `ctx` ends, each with
/// the files and names of what `importer` wrote (`NativeAssets`) or with
/// `{"error": ...}`.
pub fn serve(
    ctx: Context,
    output: &str,
    project: &str,
    mut importer: NativeProjectImporter,
) -> JoinHandle<()> {
    let directory = requests_directory(output);
    let project = project.to_string();
    std::thread::spawn(move || {
        while ctx.error().is_none() {
            let mut requests: Vec<String> = std::fs::read_dir(&directory)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|entry| entry.file_name().into_string().ok())
                        .filter(|name| name.ends_with(".request.json"))
                        .collect()
                })
                .unwrap_or_default();
            requests.sort();
            for name in requests {
                let request = sysutil::join(&[&directory, &name]);
                let response =
                    sysutil::join(&[&directory, &name.replace(".request.json", ".response.json")]);
                let asked: Option<String> = std::fs::read(&request)
                    .ok()
                    .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
                    .and_then(|value| value.get("path")?.as_str().map(String::from));
                let _ = std::fs::remove_file(&request);
                let started = Instant::now();
                let answer = match asked {
                    None => Err(anyhow!("the request names no asset")),
                    Some(path) => importer
                        .write_native_assets(&ctx, std::slice::from_ref(&path))
                        .map(|assets| (path, assets)),
                };
                let written = match answer {
                    Ok((path, assets)) => {
                        let event = Event::new("play-mode-asset")
                            .with("path", &path)
                            .seconds("elapsedSeconds", started.elapsed().as_secs_f64());
                        if let Err(error) = remember(&project, &path) {
                            eprintln!("[unity-micro-editor] {error:#}");
                        }
                        event.emit();
                        atomic_json(&response, &assets)
                    }
                    Err(error) => {
                        let text = err_text(&error);
                        Event::new("play-mode-asset-refused")
                            .with("request", &name)
                            .with("error", &text)
                            .emit();
                        atomic_json(&response, &serde_json::json!({ "error": text }))
                    }
                };
                if let Err(error) = written {
                    eprintln!("[unity-micro-editor] answer {name}: {error:#}");
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}
