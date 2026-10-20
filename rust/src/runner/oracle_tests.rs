//! Replays `testdata/recorded-cases.json`: scripted recorded reference cases.
//! Every step must print exactly the recorded line.
use super::*;
use crate::{
    sysutil::testing::temp_dir,
    util::{marshal, valid_sha256},
};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::Duration,
};

#[derive(Deserialize)]
struct Case {
    name: String,
    steps: Vec<Vec<String>>,
    outputs: Vec<String>,
}

struct Run {
    root: String,
    written: BTreeSet<String>,
    observed: BTreeMap<String, String>,
    watcher: Option<(Context, Arc<Wake>, ProjectWatcher)>,
}

fn result(value: Result<String>) -> String {
    match value {
        Ok(text) => text,
        Err(error) => format!("err: {}", err_text(&error)),
    }
}
fn quoted(text: &str) -> String {
    format!("{text:?}")
}
fn strings(values: &[String]) -> String {
    String::from_utf8(marshal(values).unwrap()).unwrap()
}

impl Run {
    fn path(&self, relative: &str) -> String {
        if relative == "." {
            self.root.clone()
        } else if sysutil::is_abs(relative) {
            relative.to_string()
        } else {
            sysutil::join(&[&self.root, relative])
        }
    }
    fn expand(&self, text: &str) -> String {
        let text = text.replace("$ROOT", &self.root);
        let text = Regex::new(r"\$REPEAT\{(.),([0-9]+)\}")
            .unwrap()
            .replace_all(&text, |found: &regex::Captures| {
                found[1].repeat(found[2].parse().unwrap())
            })
            .into_owned();
        Regex::new(r"\$SHA\{([^}]*)\}")
            .unwrap()
            .replace_all(&text, |found: &regex::Captures| {
                sha256(&fs::read(self.path(&found[1])).unwrap())
            })
            .into_owned()
    }
    fn normalize(&self, mut text: String) -> String {
        for name in &self.written {
            if let Ok(data) = fs::read(self.path(name)) {
                text = text.replace(&sha256(&data), &format!("$SHA{{{name}}}"));
            }
        }
        text.replace(&self.root, "$ROOT").replace('\n', "\\n")
    }
    fn input(&self, text: &str) -> String {
        let path = sysutil::join(&[&self.root, ".input.json"]);
        fs::write(&path, text).unwrap();
        path
    }
    fn published(&self, name: &str) -> String {
        match name.strip_prefix('@') {
            Some(stored) => fs::read_to_string(self.path(stored)).unwrap(),
            None if name == "-" => String::new(),
            None => self.path(name),
        }
    }

    fn step(&mut self, step: &[String]) -> String {
        let args: Vec<String> = step[1..].iter().map(|value| self.expand(value)).collect();
        match step[0].as_str() {
            "write" => {
                self.written.insert(step[1].clone());
                let path = self.path(&args[0]);
                fs::create_dir_all(sysutil::dir(&path)).unwrap();
                fs::write(path, &args[1]).unwrap();
                String::new()
            }
            "mkdir" => {
                fs::create_dir_all(self.path(&args[0])).unwrap();
                String::new()
            }
            "remove" => {
                let path = self.path(&args[0]);
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    fs::remove_dir(path).unwrap();
                } else {
                    fs::remove_file(path).unwrap();
                }
                String::new()
            }
            "rename" => {
                fs::rename(self.path(&args[0]), self.path(&args[1])).unwrap();
                String::new()
            }
            "symlink" => {
                std::os::unix::fs::symlink(&args[0], self.path(&args[1])).unwrap();
                String::new()
            }
            "noop" => result(
                scene_runtime_equal(&self.path(&args[0]), &self.path(&args[1]))
                    .map(|equal| equal.to_string()),
            ),
            "arguments" => {
                let (runner, game) = split_game_arguments(&args);
                let validate = match validate_game_arguments(&game) {
                    Ok(()) => "ok".into(),
                    Err(error) => err_text(&error),
                };
                format!(
                    "runner={} game={} validate={validate}",
                    strings(&runner),
                    strings(&game)
                )
            }
            "content-scene" => {
                result(parse_content_scene(&args[0]).map(|(guid, path)| format!("{guid} {path}")))
            }
            "relevant" => args
                .iter()
                .map(|path| relevant_source(path).to_string())
                .collect::<Vec<_>>()
                .join(" "),
            "graphics" => {
                result(require_hardware_graphics(&args[0], &args[1]).map(|()| "ok".into()))
            }
            "publish" => {
                let routes = match RawContentRoutes::read(&self.input(&args[0])) {
                    Ok(routes) => routes,
                    Err(error) => return format!("err: decode: {}", err_text(&error)),
                };
                result(
                    publish_content_routes(&self.path("out"), &routes).map(|path| {
                        let data = fs::read(&path).unwrap();
                        format!(
                            "{} {} {}",
                            sysutil::dir(&path),
                            sysutil::base(&path) == format!("{}.json", sha256(&data)),
                            String::from_utf8(data).unwrap().trim_end_matches('\n')
                        )
                    }),
                )
            }
            "publish-path" => {
                let routes = RawContentRoutes::read(&self.input(&args[1])).unwrap();
                let path = publish_content_routes(&self.path("out"), &routes).unwrap();
                fs::write(self.path(&args[0]), &path).unwrap();
                fs::read_to_string(path)
                    .unwrap()
                    .trim_end_matches('\n')
                    .to_string()
            }
            "tamper-published" => {
                fs::write(self.published(&format!("@{}", args[0])), &args[1]).unwrap();
                String::new()
            }
            "verify-routes" => result(
                read_verified_content_routes(&self.path(&args[0]))
                    .map(|routes| String::from_utf8(marshal(&routes).unwrap()).unwrap()),
            ),
            "routes-equal" => result(
                content_routes_runtime_equal(&self.published(&args[0]), &self.published(&args[1]))
                    .map(|equal| equal.to_string()),
            ),
            "prepared-file" => result(
                require_prepared_file(&args[0], &args[1], &mut self.observed)
                    .map(|()| "ok".into()),
            ),
            "resolve" => result(resolve_prepared_source(&self.path(&args[0]), &args[1])),
            "validate-scene" => {
                let launch = match PreparedLaunch::read(&self.input(&args[0])) {
                    Ok(launch) => launch,
                    Err(error) => return format!("err: decode: {}", err_text(&error)),
                };
                result(
                    validate_prepared_scene(&launch, &args[1], &args[2], &mut BTreeMap::new()).map(
                        |scene| {
                            format!(
                                "ok assemblies={} dependencies={} packs={} native={}",
                                scene.assemblies.len(),
                                scene.dependencies.len(),
                                scene.imported_packs.len(),
                                scene.native_content.len()
                            )
                        },
                    ),
                )
            }
            "launch" => result(PreparedLaunch::read(&self.path(&args[0])).map(|launch| {
                format!(
                    "schema={} project={} player={} playerSha={} manifest={} manifestSha={} routes={} routesSha={} hashes={}",
                    launch.schema_version,
                    quoted(&launch.project),
                    quoted(&launch.player),
                    quoted(&launch.player_sha256),
                    quoted(&launch.manifest),
                    quoted(&launch.manifest_sha256),
                    quoted(&launch.content_routes),
                    quoted(&launch.content_routes_sha256),
                    launch
                        .assembly_hashes
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })),
            "baseline" => result(BaselineBuild::read(&self.path(&args[0])).map(|baseline| {
                format!(
                    "project={} manifest={} player={} catalog={}",
                    quoted(&baseline.project),
                    quoted(&baseline.manifest),
                    quoted(&baseline.player),
                    quoted(&baseline.native_import.catalog)
                )
            })),
            "report" => result(require_player_report(&self.path(&args[0])).map(|()| "ok".into())),
            "ack" => {
                let reply = wait_reload_ack(
                    &Context::background(),
                    &self.path(&args[0]),
                    &args[1],
                    Duration::from_millis(args[2].parse().unwrap()),
                );
                format!(
                    "ack={} err={}",
                    reply
                        .ack
                        .map_or("null".into(), |ack| value::compact_map(&ack).unwrap()),
                    reply.error.map_or("nil".into(), |error| err_text(&error))
                )
            }
            "initializers" => result(
                json::read_runtime_initializers(&self.path(&args[0]))
                    .and_then(|value| value.to_json()),
            ),
            "watch-start" => {
                let ctx = Context::background().with_cancel();
                let wake = Wake::new();
                match ProjectWatcher::new(&ctx, &wake, &self.path(&args[0])) {
                    Ok(watcher) => {
                        self.watcher = Some((ctx, wake, watcher));
                        "watching".into()
                    }
                    Err(error) => format!("err: {}", err_text(&error)),
                }
            }
            "watch-drain" => {
                let (_, _, watcher) = self.watcher.as_ref().unwrap();
                let mut events = Vec::new();
                while let Some(event) = watcher.recv_timeout(Duration::from_millis(300)) {
                    events.push(match event.error {
                        Some(error) => format!("err: {error}"),
                        None => event.path,
                    });
                }
                events.join(",")
            }
            other => panic!("unknown step {other}"),
        }
    }
}

#[test]
fn recorded_cases() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("testdata/recorded-cases.json")).unwrap();
    assert!(cases.len() > 150, "recorded cases are incomplete");
    let filter = std::env::var("UME_RUNNER_ORACLE_CASE").unwrap_or_default();
    let mut failures = Vec::new();
    let mut steps = 0;
    for case in cases.iter().filter(|case| case.name.contains(&filter)) {
        let (_guard, root) = temp_dir();
        let mut run = Run {
            root,
            written: BTreeSet::new(),
            observed: BTreeMap::new(),
            watcher: None,
        };
        assert_eq!(case.steps.len(), case.outputs.len(), "{}", case.name);
        for (index, (step, expected)) in case.steps.iter().zip(&case.outputs).enumerate() {
            let output = run.step(step);
            let actual = run.normalize(output);
            steps += 1;
            if actual != *expected {
                let cut = |text: &str| text.chars().take(600).collect::<String>();
                failures.push(format!(
                    "{} step {index} {:?}\n   recorded: {}\n actual: {}",
                    case.name,
                    step.iter()
                        .map(|part| cut(part).chars().take(80).collect())
                        .collect::<Vec<String>>(),
                    cut(expected),
                    cut(&actual)
                ));
            }
        }
        if let Some((ctx, _, _)) = &run.watcher {
            ctx.cancel();
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {steps} steps differ from the recorded output:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(valid_sha256(&sha256(b"")));
}
