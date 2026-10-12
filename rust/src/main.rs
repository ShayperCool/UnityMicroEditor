use anyhow::{Result, ensure};
use std::{
    env, fs,
    io::{self, Read},
    path::Path,
};
use unity_micro_editor::{ENGINE_PROFILE, graph::SourceProject, yaml};

const USAGE: &str = "usage: unity-micro-editor <run|mcp|auto-prepare|build-project|resolve-packages|cache-component-defaults|project-import|native-adopt|native-environment|source-graph|compiler-source-prepare|compiler-prepare|prepare-project|engine-profile-capture|version> [flags]";

fn execute() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_default();
    match command.as_str() {
        "version" | "profile" if env::args().len() == 2 => {
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
        "material-inspect" => {
            unity_micro_editor::native::run_material_inspect_cli(&args.collect::<Vec<_>>())?
        }
        "native-adopt" => {
            unity_micro_editor::native::run_native_adopt_cli(&args.collect::<Vec<_>>())?
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
        "shader-extract" => {
            unity_micro_editor::runtimeartifact::shader_extract_command(&args.collect::<Vec<_>>())?
        }
        "builtin-shader" => {
            unity_micro_editor::runtimeartifact::builtin_shader_command(&args.collect::<Vec<_>>())?
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
