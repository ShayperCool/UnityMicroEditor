use anyhow::{Result, ensure};
use std::{
    env, fs,
    io::{self, Read},
    path::Path,
};
use unity_micro_editor::{ENGINE_PROFILE, graph::SourceProject, yaml};

const USAGE: &str = "usage: unity-micro-editor <run|mcp|auto-prepare|build-project|resolve-packages|cache-component-defaults|project-import|native-environment|source-graph|compiler-source-prepare|compiler-prepare|prepare-project|engine-profile-capture|version> [flags]";

fn execute() -> Result<()> {
    let mut arguments: Vec<String> = env::args().skip(1).collect();
    unity_micro_editor::memory::take_argument(&mut arguments).map_err(anyhow::Error::msg)?;
    let mut args = arguments.into_iter();
    let command = args.next().unwrap_or_default();
    match command.as_str() {
        "version" | "profile" if args.len() == 0 => {
            print!("{ENGINE_PROFILE}");
        }
        "mcp" => unity_micro_editor::agent::run_mcp_cli(args.collect())?,
        "auto-prepare" => unity_micro_editor::agent::run_auto_prepare_cli(args.collect())?,
        "build-project" => unity_micro_editor::agent::run_build_project_cli(args.collect())?,
        "prepare-project" => unity_micro_editor::agent::run_project_prepare_cli(args.collect())?,
        "cache-component-defaults" => {
            unity_micro_editor::agent::run_component_defaults_cli(args.collect())?
        }
        "parse-yaml" => {
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("parse-yaml requires a file"))?;
            ensure!(args.next().is_none(), "unexpected argument");
            let data = read_text(Path::new(&path))?;
            serde_json::to_writer(io::stdout().lock(), &yaml::parse_unity_yaml(&data)?)?;
            println!();
        }
        "authored-graph" => {
            let project = args.next().ok_or_else(|| {
                anyhow::anyhow!("authored-graph requires an explicit project and scene")
            })?;
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("authored-graph requires a scene"))?;
            ensure!(args.next().is_none(), "unexpected argument");
            serde_json::to_writer(
                io::stdout().lock(),
                &SourceProject::new(Path::new(&project))?.authored_graph(&path)?,
            )?;
            println!();
        }
        "prepare" => {
            let arguments: Vec<String> = args.collect();
            std::process::exit(unity_micro_editor::project::prepare::run(&arguments));
        }
        // Source tools only: no runtime session or project actions.
        "mcp-standalone" => {
            unity_micro_editor::mcp::run(&unity_micro_editor::mcp::signal_context()?, args)?
        }
        "project-import" => {
            unity_micro_editor::native::run_project_import_cli(&args.collect::<Vec<_>>())?
        }
        "native-environment" => {
            unity_micro_editor::native::run_native_environment_cli(&args.collect::<Vec<_>>())?
        }
        "resolve-packages" => unity_micro_editor::packages::cli(args)?,
        "source-graph" | "compiler-source-prepare" | "engine-profile-capture" => {
            if let Err(error) =
                unity_micro_editor::sourcegraph::run_source_graph_cli(&command, args.collect())
            {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
        "compiler-prepare" => {
            if let Err(error) =
                unity_micro_editor::compiler::run_compiler_prepare_cli(args.collect())
            {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        "run" => unity_micro_editor::runner::run_cli(&args.collect::<Vec<_>>())?,
        "game-managers" => run_game_managers(args.collect())?,
        "scene-objects" => run_scene_objects(args.collect())?,
        "player-file" => run_player_file(args.collect())?,
        "debug-build-environment" => {
            let args: Vec<String> = args.collect();
            let (project, scripts) = args.split_first().ok_or_else(|| {
                anyhow::anyhow!("debug-build-environment <project> <script guid>...")
            })?;
            println!(
                "{}",
                unity_micro_editor::native::debug_build_environment(project, scripts)?
            );
        }
        // Clients of the Unity Editor reference in `Integrations/EditorParity`.
        name if name.starts_with("parity-") => {
            match unity_micro_editor::parity::run(name, args.collect()) {
                Some(status) => std::process::exit(status),
                None => {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}
fn main() {
    if let Err(error) = execute() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn read_text(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let info = file.metadata()?;
    ensure!(
        info.is_file() && info.len() <= yaml::MAX_TEXT_BYTES as u64,
        "expected a regular text source at most 32 MiB"
    );
    let mut data = Vec::with_capacity(info.len() as usize);
    file.take(yaml::MAX_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() <= yaml::MAX_TEXT_BYTES,
        "text source grew while reading"
    );
    Ok(data)
}

/// `player-file dump <file>` prints the objects of a Player serialized file
/// under the keys `player-file compare <ours> <theirs> [--tolerance T]`
/// pairs them by; the comparison lists what differs per class.
fn run_player_file(args: Vec<String>) -> Result<()> {
    use unity_micro_editor::playerbake::{compare_files, inspect_file};
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["dump", file] => {
            let mut out = io::stdout().lock();
            for object in inspect_file(Path::new(file))? {
                serde_json::to_writer(
                    &mut out,
                    &serde_json::json!({"key": object.key, "class": object.class, "localId": object.local_id, "value": object.value}),
                )?;
                println!();
            }
        }
        ["compare", ours, theirs, rest @ ..] => {
            let tolerance = match rest {
                [] => 0.0,
                ["--tolerance", value] => value.parse()?,
                _ => anyhow::bail!("player-file compare <ours> <theirs> [--tolerance T]"),
            };
            let difference = compare_files(Path::new(ours), Path::new(theirs), tolerance)?;
            println!(
                "matched {} (equal {}), only ours {}, only theirs {}",
                difference.matched,
                difference.equal,
                difference.only_ours.len(),
                difference.only_theirs.len()
            );
            for (class, objects) in &difference.fields {
                println!("{class}: {} differ", objects.len());
                for (key, fields) in objects.iter().take(3) {
                    println!("  {key}");
                    for field in fields {
                        println!("    {field}");
                    }
                }
            }
            for (label, keys) in [
                ("only ours", &difference.only_ours),
                ("only theirs", &difference.only_theirs),
            ] {
                for key in keys.iter().take(20) {
                    println!("{label}: {key}");
                }
            }
        }
        _ => anyhow::bail!("player-file dump <file> | compare <ours> <theirs> [--tolerance T]"),
    }
    Ok(())
}

/// `game-managers --project P --base globalgamemanagers --output F [--compare F]`:
/// writes the managers authored in the project's `ProjectSettings` over a
/// Player's `globalgamemanagers` (experimental, see `playerbake::managers`).
/// Has a standalone import worker write a scene as a build writes it, and
/// lists what the file holds and refers to.
fn run_scene_objects(args: Vec<String>) -> Result<()> {
    let mut flags = std::collections::BTreeMap::new();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?;
        flags.insert(flag, value);
    }
    let flag = |name: &str| {
        flags
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("scene-objects requires {name}"))
    };
    use unity_micro_editor::native;
    let build = native::PlayerBuild {
        project: flag("--project")?,
        editor: flag("--editor")?,
        output: flag("--output")?,
        target: flag("--scene")?,
        users: Vec::new(),
        timeout: std::time::Duration::from_secs(600),
        reload_list: String::new(),
        shader_cache: String::new(),
    };
    let written = native::write_scene_objects(
        &unity_micro_editor::cancel::Context::background(),
        &build,
        flags.contains_key("--with-dependencies"),
    )?;
    let scene_externals = unity_micro_editor::playerbake::external_paths(&written.scene)?;
    println!(
        "scene {} bytes, refers to {scene_externals:?}",
        written.scene.len()
    );
    if let Some(dependencies) = &written.dependencies {
        println!(
            "dependencies {} bytes, {} objects, refer to {:?}",
            dependencies.len(),
            written.report.dependencies.len(),
            unity_micro_editor::playerbake::external_paths(dependencies)?
        );
    }
    let output = flag("--output")?;
    fs::write(format!("{output}/scene"), &written.scene)?;
    if let Some(dependencies) = &written.dependencies {
        fs::write(format!("{output}/deps"), dependencies)?;
    }
    println!(
        "objects {} externals {} imports {} seconds {:.1}",
        written.report.objects.len(),
        written.report.externals.len(),
        written.imports,
        written.seconds
    );
    Ok(())
}

fn run_game_managers(args: Vec<String>) -> Result<()> {
    let mut flags = std::collections::BTreeMap::new();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?;
        flags.insert(flag, value);
    }
    let flag = |name: &str| {
        flags
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("game-managers requires {name}"))
    };
    use unity_micro_editor::playerbake;
    let trees = playerbake::manager_trees()?;
    let base = fs::read(flag("--base")?)?;
    let (file, reports) =
        playerbake::write_project_managers(Path::new(&flag("--project")?), &base, &trees)?;
    if let Some(other) = flags.get("--compare") {
        for line in playerbake::compare_managers(&file, &fs::read(other)?, &trees)? {
            eprintln!("{line}");
        }
    }
    fs::write(flag("--output")?, file)?;
    serde_json::to_writer_pretty(io::stdout().lock(), &reports)?;
    println!();
    Ok(())
}
