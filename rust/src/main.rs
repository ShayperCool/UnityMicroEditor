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
        "complete-player" => {
            // `complete-player <project> <editor> <player>`: the files of an
            // assembled host that name the project's scripts and resources.
            let args: Vec<String> = args.collect();
            let [project, editor, player] = args.as_slice() else {
                anyhow::bail!("complete-player <project> <editor> <player>");
            };
            let ctx = unity_micro_editor::cancel::Context::background();
            let completed = unity_micro_editor::native::host_assembly::complete_player(
                &ctx, project, editor, player,
            )?;
            println!("{}", serde_json::to_string_pretty(&completed)?);
        }
        "assemble-host" => {
            // `assemble-host <project> <editor> <output>`: a host Player
            // assembled by the tool, with no Editor build.
            let args: Vec<String> = args.collect();
            let [project, editor, output] = args.as_slice() else {
                anyhow::bail!("assemble-host <project> <editor> <output>");
            };
            let ctx = unity_micro_editor::cancel::Context::background();
            let resolved =
                unity_micro_editor::packages::resolve(&ctx, project, editor, Default::default())?;
            let roots: Vec<String> = resolved
                .packages
                .iter()
                .map(|package| package.path.clone())
                .collect();
            let assembled = unity_micro_editor::native::host_assembly::assemble_host(
                &ctx, project, editor, output, &roots,
            )?;
            println!("{}", serde_json::to_string_pretty(&assembled)?);
        }
        "scene-objects" => run_scene_objects(args.collect())?,
        "player-objects" => run_player_objects(args.collect())?,
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
            let usage = "player-file compare <ours> <theirs> [--tolerance T] \
                         [--external <name> <ours> <theirs>]...";
            let mut tolerance = 0.0;
            let mut links = Vec::new();
            let mut rest = rest;
            loop {
                rest = match rest {
                    [] => break,
                    ["--tolerance", value, rest @ ..] => {
                        tolerance = value.parse()?;
                        rest
                    }
                    ["--external", name, a, b, rest @ ..] => {
                        links.push((
                            name.to_string(),
                            std::path::PathBuf::from(a),
                            std::path::PathBuf::from(b),
                        ));
                        rest
                    }
                    _ => anyhow::bail!("{usage}"),
                };
            }
            let difference = compare_files(Path::new(ours), Path::new(theirs), tolerance, &links)?;
            println!(
                "matched {} (equal {}), only ours {}, only theirs {}",
                difference.matched,
                difference.equal,
                difference.only_ours.len(),
                difference.only_theirs.len()
            );
            for (class, (matched, equal)) in &difference.classes {
                println!("class {class}: matched {matched}, equal {equal}");
            }
            let mut fields: std::collections::BTreeMap<(String, String), usize> =
                Default::default();
            for (class, objects) in &difference.fields {
                for (_, found) in objects {
                    for field in found {
                        let path: String = field
                            .split(": ")
                            .next()
                            .unwrap_or_default()
                            .split('[')
                            .enumerate()
                            .map(|(i, part)| {
                                if i == 0 {
                                    part.to_string()
                                } else {
                                    format!(
                                        "[]{}",
                                        part.split_once(']').map_or("", |(_, rest)| rest)
                                    )
                                }
                            })
                            .collect();
                        *fields
                            .entry((
                                class.split('<').next().unwrap_or_default().to_string(),
                                path,
                            ))
                            .or_default() += 1;
                    }
                }
            }
            let mut ranked: Vec<_> = fields.into_iter().collect();
            ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            for ((class, path), count) in ranked.iter().take(60) {
                println!("field {count:6} {class} {path}");
            }
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
                for key in keys
                    .iter()
                    .take(if std::env::var_os("UME_COMPARE_ALL").is_some() {
                        usize::MAX
                    } else {
                        20
                    })
                {
                    println!("{label}: {key}");
                }
            }
        }
        ["type-entry", file, class, output] => {
            // The type table entry of a native class, as the engine wrote it.
            let data = fs::read(file)?;
            let class: i32 = class.parse()?;
            let objects = unity_micro_editor::playerbake::read_serialized_of_class(&data, class)?;
            let object = objects
                .first()
                .ok_or_else(|| anyhow::anyhow!("{file} holds no object of class {class}"))?;
            fs::write(output, &object.tree.entry[..])?;
        }
        ["game-managers", project, version, output, rest @ ..] => {
            // `globalgamemanagers` written from the release's pack and the
            // project's settings; `--compare F` lists the fields that differ
            // from another build's.
            use unity_micro_editor::playerbake;
            let pack = playerbake::ManagerPack::of_release(version)?
                .ok_or_else(|| anyhow::anyhow!("no manager pack of {version}"))?;
            let pointers = |source: &serde_json::Value| -> Option<(i64, i64)> {
                let file_id: i64 = source.get("fileID")?.as_str()?.parse().ok()?;
                match source.get("guid")?.as_str()? {
                    "0000000000000000f000000000000000" => {
                        Some((playerbake::BUILTIN_EXTRA_FILE, file_id))
                    }
                    "0000000000000000e000000000000000" => {
                        Some((playerbake::DEFAULT_RESOURCES_FILE, file_id))
                    }
                    _ => None,
                }
            };
            let read =
                |name: &str| fs::read_to_string(Path::new(project).join(name)).unwrap_or_default();
            let settings = playerbake::ShaderSettings::read(
                &read("ProjectSettings/ProjectSettings.asset"),
                &read("ProjectSettings/GraphicsSettings.asset"),
                &read("ProjectSettings/QualitySettings.asset"),
                &read("Packages/manifest.json"),
            )
            .ok();
            let derived = playerbake::DerivedManagers {
                scenes: vec!["Assets/Bootstrap.unity".into()],
                graphics_apis: vec![17],
                scripts: Vec::new(),
                resources: Vec::new(),
                shaders: Vec::new(),
                builtin_shaders: Vec::new(),
                shader_settings: settings.as_ref(),
                pointers: &pointers,
            };
            let (file, reports) =
                playerbake::write_game_managers(Path::new(project), &pack, &derived)?;
            fs::write(output, &file)?;
            for report in &reports {
                for kept in &report.kept {
                    eprintln!("kept {} {kept}", report.class_id);
                }
            }
            if let ["--compare", other] = rest {
                for line in playerbake::compare_managers(&file, &fs::read(other)?, &pack.trees())? {
                    println!(
                        "{}",
                        line.chars()
                            .take(
                                std::env::var("UME_FIELDS_LIMIT")
                                    .ok()
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(400)
                            )
                            .collect::<String>()
                    );
                }
            }
        }
        ["managers-check", pack, others @ ..] => {
            // Decodes the managers of each Player file with the trees of a
            // capture, and re-encodes them, byte for byte.
            let data = fs::read(pack)?;
            let objects = unity_micro_editor::playerbake::read_serialized(&data)?;
            let mut trees = std::collections::BTreeMap::new();
            for object in &objects {
                let outcome = object
                    .tree
                    .decode(&object.data)
                    .and_then(|value| Ok(object.tree.encode(&value)? == object.data));
                println!(
                    "pack {} {:?}",
                    object.tree.class_id,
                    outcome.map_err(|e| e.to_string())
                );
                if std::env::var("UME_FIELDS")
                    .is_ok_and(|class| class == object.tree.class_id.to_string())
                {
                    let count = object.tree.children(0).len();
                    for fields in 1..=count {
                        match object.tree.decode_leading(&object.data, fields) {
                            Ok(unity_micro_editor::playerbake::Val::Map(values)) => {
                                let (name, value) = values.last().unwrap();
                                println!(
                                    "  {name} = {}",
                                    format!("{value:?}").chars().take(160).collect::<String>()
                                );
                            }
                            other => {
                                println!("  stop {:?}", other.err());
                                break;
                            }
                        }
                    }
                }
                trees.insert(object.tree.class_id, object.tree.clone());
            }
            for other in others {
                let file = unity_micro_editor::playerbake::PlainFile::parse(&fs::read(other)?)?;
                for object in &file.objects {
                    let Some(tree) = trees.get(&object.class_id) else {
                        println!(
                            "{other} {} no tree, {} bytes",
                            object.class_id,
                            object.data.len()
                        );
                        continue;
                    };
                    let outcome = tree
                        .decode(&object.data)
                        .and_then(|value| Ok(tree.encode(&value)? == object.data));
                    println!(
                        "{other} {} {:?}",
                        object.class_id,
                        outcome.map_err(|e| e.to_string())
                    );
                    if std::env::var("UME_FIELDS")
                        .is_ok_and(|class| class == object.class_id.to_string())
                    {
                        for fields in 1..=tree.children(0).len() {
                            match tree.decode_leading(&object.data, fields) {
                                Ok(unity_micro_editor::playerbake::Val::Map(values)) => {
                                    let (name, value) = values.last().unwrap();
                                    println!(
                                        "  {name} = {}",
                                        format!("{value:?}")
                                            .chars()
                                            .take(
                                                std::env::var("UME_FIELDS_LIMIT")
                                                    .ok()
                                                    .and_then(|v| v.parse().ok())
                                                    .unwrap_or(300)
                                            )
                                            .collect::<String>()
                                    );
                                }
                                other => {
                                    println!("  stop {:?}", other.err());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        ["fbx", file, rest @ ..] => {
            use unity_micro_editor::playerbake::model;
            print!("{}", model::inspect(Path::new(file))?);
            let meta = fs::read_to_string(format!("{file}.meta"))?;
            let imported = model::import(Path::new(file), &model::Settings::read(&meta)?)?;
            for (id, class, value) in &imported.objects {
                let name = match value.field("m_Name") {
                    Some(unity_micro_editor::playerbake::Val::Str(name)) => name.clone(),
                    _ => String::new(),
                };
                println!("object {id} class {class} {name:?}");
            }
            for document in &imported.documents {
                println!(
                    "document {} {} {}",
                    document.file_id,
                    document.type_name,
                    document
                        .data
                        .get("m_Name")
                        .map(|n| n.to_string())
                        .unwrap_or_default()
                );
            }
            for refusal in &imported.refused {
                println!("refused {refusal}");
            }
            // `--write F`: the objects into a Player file, to compare.
            if let ["--write", output, version] = rest {
                let library = unity_micro_editor::playerbake::Library::of_release(version)
                    .ok_or_else(|| anyhow::anyhow!("no pack for {version}"))??;
                let mut objects = Vec::new();
                let mut data = Vec::new();
                for (id, class, value) in &imported.objects {
                    let kind = library
                        .class(*class as i32)
                        .ok_or_else(|| anyhow::anyhow!("no tree of class {class}"))?;
                    let mut value = value.clone();
                    if *class == 90 {
                        let size = unity_micro_editor::playerbake::blob_size(
                            &kind.tree,
                            "m_Avatar",
                            value.field("m_Avatar").unwrap(),
                        )?;
                        *value.field_mut("m_AvatarSize").unwrap() =
                            unity_micro_editor::playerbake::Val::U(size);
                    }
                    data.push((*id, kind.tree.clone(), kind.tree.encode(&value)?));
                }
                for (id, tree, bytes) in &data {
                    objects.push(unity_micro_editor::playerbake::Object {
                        local_id: *id,
                        tree,
                        data: bytes.clone(),
                        script: None,
                        script_file: 0,
                    });
                }
                let format = unity_micro_editor::playerbake::FileFormat {
                    version: if version.starts_with("6000.0.") {
                        22
                    } else {
                        23
                    },
                    unity_version: Box::leak(version.to_string().into_boxed_str()),
                };
                fs::write(
                    output,
                    unity_micro_editor::playerbake::write_in(format, &objects, &[])?,
                )?;
            }
        }
        // A type tree as the engine's own files store one (`engine/type-*.bin`).
        ["tree", file] => {
            let data: &'static [u8] = Box::leak(fs::read(file)?.into_boxed_slice());
            let tree = unity_micro_editor::playerbake::TypeTree::parse(data)?;
            for node in &tree.nodes {
                println!(
                    "{}{} {} size={} v={} flags={} meta={:#x}",
                    "  ".repeat(node.level as usize),
                    node.type_name,
                    node.name,
                    node.size,
                    node.version,
                    node.type_flags,
                    node.meta
                );
            }
        }
        ["trees", file] => {
            let data = fs::read(file)?;
            let (objects, _) =
                unity_micro_editor::playerbake::read_serialized_with_externals(&data)?;
            let mut names = std::collections::HashMap::new();
            for object in objects.iter().filter(|o| o.tree.class_id == 115) {
                let value = object.tree.decode(&object.data)?;
                let text = |name: &str| match value.field(name) {
                    Some(unity_micro_editor::playerbake::Val::Str(s)) => s.clone(),
                    _ => String::new(),
                };
                names.insert(
                    object.local_id,
                    format!("{}.{}", text("m_Namespace"), text("m_ClassName")),
                );
            }
            let mut seen = std::collections::HashSet::new();
            for object in &objects {
                let label = match object.script {
                    Some(script) => names.get(&script).cloned().unwrap_or_default(),
                    None => object.tree.nodes[0].type_name.clone(),
                };
                if !seen.insert(label.clone()) {
                    continue;
                }
                println!("== {label} (class {})", object.tree.class_id);
                for node in &object.tree.nodes {
                    println!(
                        "{}{} {} size={} v={} flags={} meta={:#x}",
                        "  ".repeat(node.level as usize),
                        node.type_name,
                        node.name,
                        node.size,
                        node.version,
                        node.type_flags,
                        node.meta
                    );
                }
            }
            // The types the objects hold by managed reference.
            if let Some(object) = objects.first() {
                for (class, namespace, assembly, tree) in object.references.iter() {
                    println!("== managed {namespace}.{class} ({assembly})");
                    for node in &tree.nodes {
                        println!(
                            "{}{} {} size={} v={} flags={} meta={:#x}",
                            "  ".repeat(node.level as usize),
                            node.type_name,
                            node.name,
                            node.size,
                            node.version,
                            node.type_flags,
                            node.meta
                        );
                    }
                }
            }
        }
        [
            "shader-preprocess",
            "--editor",
            editor,
            "--project",
            project,
            assets @ ..,
        ] => {
            // Each source as the compiler preprocesses it for an import, as
            // one JSON line.
            let ctx = unity_micro_editor::cancel::Context::background();
            let logs = std::env::temp_dir().join(format!("ume-preprocess-{}", std::process::id()));
            let mut compiler = unity_micro_editor::native::ShaderCompiler::start(
                &ctx,
                editor,
                project,
                &logs.to_string_lossy(),
            )?;
            // `logical=physical` reads a source from elsewhere than the
            // project's path of its name.
            for item in assets {
                let (asset, physical) = item.split_once('=').unwrap_or((item, item));
                let source = fs::read_to_string(Path::new(project).join(physical))?;
                let request =
                    unity_micro_editor::playerbake::shaderlab::PreprocessRequest::standalone(
                        asset, source,
                    );
                let reply = compiler.preprocess(&request)?;
                if std::env::var_os("UME_PARSED_FORM").is_some() {
                    let form = unity_micro_editor::playerbake::shaderlab::parsed_form(&reply)
                        .unwrap_or_else(|error| serde_json::json!({"error": format!("{error:#}")}));
                    let imported = unity_micro_editor::playerbake::shaderlab::import(&reply)?;
                    let info = unity_micro_editor::playerbake::shaderlab::compile_info(
                        &imported,
                        &request.directory,
                        &request.file,
                        &request.source,
                    );
                    println!(
                        "{}",
                        serde_json::json!({"file": asset, "form": form, "info": info})
                    );
                    continue;
                }
                println!("{}", serde_json::to_string(&reply)?);
            }
        }
        [
            "shader-import",
            "--editor",
            editor,
            "--project",
            project,
            "--reference",
            reference,
            assets @ ..,
        ] => {
            // Each source imported as the Editor imports it, against the
            // Editor's artifact of it in `reference` (`<guid>-0000.bin`).
            use unity_micro_editor::playerbake::{self as bake, shaderlab};
            let ctx = unity_micro_editor::cancel::Context::background();
            let logs =
                std::env::temp_dir().join(format!("ume-shader-import-{}", std::process::id()));
            let mut compiler = unity_micro_editor::native::ShaderCompiler::start(
                &ctx,
                editor,
                project,
                &logs.to_string_lossy(),
            )?;
            let tree = shaderlab::editor_shader_tree("6000.0.71f1")
                .ok_or_else(|| anyhow::anyhow!("no Editor Shader tree"))?;
            let (mut equal, mut total) = (0, 0);
            for item in assets {
                let (asset, physical) = item.split_once('=').unwrap_or((item, item));
                let physical = Path::new(project).join(physical);
                let source = fs::read_to_string(&physical)?;
                let meta = fs::read(format!("{}.meta", physical.display()))?;
                let guid = String::from_utf8_lossy(&meta)
                    .lines()
                    .find_map(|l| l.strip_prefix("guid: ").map(|g| g.trim().to_string()))
                    .unwrap_or_default();
                let request = shaderlab::PreprocessRequest::standalone(asset, source);
                let reply = compiler.preprocess(&request)?;
                total += 1;
                let built = shaderlab::editor_object(&reply, &request, &guid, &meta)
                    .inspect(|value| {
                        if let Some(dir) = std::env::var_os("UME_SHADER_DUMP") {
                            let name = format!("{}.json", asset.replace('/', "_"));
                            let _ = fs::write(Path::new(&dir).join(name), value.to_string());
                        }
                    })
                    .and_then(|value| tree.val_of_json(&value))
                    .and_then(|value| tree.encode(&value));
                let reference_file = Path::new(reference).join(format!("{guid}-0000.bin"));
                let theirs = fs::read(&reference_file)
                    .ok()
                    .and_then(|file| bake::read_serialized_of_class(&file, 48).ok())
                    .and_then(|objects| objects.into_iter().next());
                match (built, theirs) {
                    (Err(error), _) => println!("{asset}: {error:#}"),
                    (Ok(_), None) => println!("{asset}: no reference"),
                    (Ok(ours), Some(theirs)) if ours == theirs.data => equal += 1,
                    (Ok(ours), Some(theirs)) => {
                        let at = ours.iter().zip(&theirs.data).position(|(a, b)| a != b);
                        println!(
                            "{asset}: {} bytes against {}, first difference at {at:?}",
                            ours.len(),
                            theirs.data.len()
                        );
                    }
                }
            }
            println!("equal {equal} of {total}");
        }
        [
            "shader-bake",
            "--editor",
            editor,
            "--project",
            project,
            "--source",
            source,
            "--reference",
            reference,
            assets @ ..,
        ] => {
            // Each source imported and baked by the tool, against the
            // Player object the worker built of it (`DIR/<name>.objects`,
            // with the materials that use it in `DIR/<name>.users`). The
            // compiler runs in `project`; the settings and materials are
            // read from the project `source`.
            use unity_micro_editor::playerbake::{self as bake, shaderlab};
            let ctx = unity_micro_editor::cancel::Context::background();
            let logs = std::env::temp_dir().join(format!("ume-shader-bake-{}", std::process::id()));
            let folders = unity_micro_editor::native::open_build_sources(source)?.folders();
            let mut compiler = unity_micro_editor::native::ShaderCompiler::start_with_folders(
                &ctx,
                editor,
                project,
                &logs.to_string_lossy(),
                &folders,
            )?;
            let editor_tree = shaderlab::editor_shader_tree("6000.0.71f1")
                .ok_or_else(|| anyhow::anyhow!("no Editor Shader tree"))?;
            let setting =
                |name: &str| fs::read_to_string(Path::new(source).join(name)).unwrap_or_default();
            let player = setting("ProjectSettings/ProjectSettings.asset");
            let settings = bake::ShaderSettings::read(
                &player,
                &setting("ProjectSettings/GraphicsSettings.asset"),
                &setting("ProjectSettings/QualitySettings.asset"),
                &setting("Packages/manifest.json"),
            )?;
            let platforms = bake::shader_platforms(&player)?;
            // The project's shaders by name, from the assets given.
            let mut shader_names = std::collections::HashMap::new();
            for item in assets {
                let (asset, physical) = item.split_once('=').unwrap_or((item, item));
                let physical = Path::new(project).join(physical);
                if let Ok(text) = fs::read_to_string(&physical)
                    && let Some(name) = text
                        .split_once("Shader")
                        .and_then(|(_, rest)| rest.split('"').nth(1))
                {
                    shader_names.insert(name.to_string(), (asset.to_string(), physical.clone()));
                }
            }
            let (mut equal, mut total) = (0, 0);
            for item in assets {
                let (asset, physical) = item.split_once('=').unwrap_or((item, item));
                total += 1;
                let name = asset.replace(['/', ' '], "_");
                let result = (|| -> Result<bool> {
                    let physical = Path::new(project).join(physical);
                    let text = fs::read_to_string(&physical)?;
                    let meta = fs::read(format!("{}.meta", physical.display()))?;
                    let guid = String::from_utf8_lossy(&meta)
                        .lines()
                        .find_map(|l| l.strip_prefix("guid: ").map(|g| g.trim().to_string()))
                        .unwrap_or_default();
                    let request = shaderlab::PreprocessRequest::standalone(asset, text);
                    let reply = compiler.preprocess(&request)?;
                    let value = editor_tree
                        .val_of_json(&shaderlab::editor_object(&reply, &request, &guid, &meta)?)?;
                    let artifact = bake::write_in(
                        bake::FileFormat::of_release("6000.0.71f1").unwrap(),
                        &[bake::Object {
                            local_id: bake::SHADER_LOCAL_ID,
                            tree: &editor_tree,
                            data: editor_tree.encode(&value)?,
                            script: None,
                            script_file: 0,
                        }],
                        &[],
                    )?;
                    let users =
                        fs::read_to_string(Path::new(reference).join(format!("{name}.users")))
                            .unwrap_or_default();
                    let mut materials = Vec::new();
                    for user in users.trim().split(',').filter(|u| !u.is_empty()) {
                        let file = Path::new(source).join(user);
                        if let Ok(data) = fs::read(&file) {
                            materials.push(unity_micro_editor::native::shader_material_use(
                                user, &data,
                            )?);
                        }
                    }
                    let shader_project = bake::ShaderProject {
                        release: bake::ShaderRelease::Unity6000_0,
                        platforms: platforms.clone(),
                        settings: settings.clone(),
                        materials,
                    };
                    let others = ProjectShaders {
                        names: shader_names.clone(),
                        editor: editor.to_string(),
                        directory: project.to_string(),
                        project: bake::ShaderProject {
                            materials: Vec::new(),
                            ..shader_project.clone()
                        },
                        tree: &editor_tree,
                        folders: folders.clone(),
                        found: Default::default(),
                        depth: Default::default(),
                    };
                    let (ours, _) = bake::bake_shader_object(
                        &artifact,
                        &shader_project,
                        &others,
                        &mut compiler,
                    )?;
                    let reference_file =
                        fs::read(Path::new(reference).join(format!("{name}.objects")))?;
                    let theirs = bake::read_serialized_of_class(&reference_file, 48)?
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("no reference Shader"))?;
                    if let Some(dir) = std::env::var_os("UME_SHADER_DUMP") {
                        let file = bake::write_in(
                            bake::FileFormat::of_release("6000.0.71f1").unwrap(),
                            &[bake::Object {
                                local_id: bake::SHADER_LOCAL_ID,
                                tree: bake::ShaderRelease::Unity6000_0.tree()?,
                                data: ours.clone(),
                                script: None,
                                script_file: 0,
                            }],
                            &[],
                        )?;
                        fs::write(Path::new(&dir).join(format!("{name}.objects")), file)?;
                    }
                    if ours == theirs.data {
                        return Ok(true);
                    }
                    let at = ours.iter().zip(&theirs.data).position(|(a, b)| a != b);
                    println!(
                        "{asset}: {} bytes against {}, first difference at {at:?}",
                        ours.len(),
                        theirs.data.len()
                    );
                    Ok(false)
                })();
                match result {
                    Ok(true) => equal += 1,
                    Ok(false) => {}
                    Err(error) => println!("{asset}: {error:#}"),
                }
            }
            println!("equal {equal} of {total}");
        }
        ["script-layouts", project, editor, player, output] => {
            let ctx = unity_micro_editor::mcp::signal_context()?;
            let layouts =
                unity_micro_editor::compiler::player_script_layouts(&ctx, project, editor, player)?;
            fs::write(output, layouts)?;
        }
        ["sprite-mesh", png, rest @ ..] => {
            // The outlines and tight mesh the generator makes of a PNG's alpha
            // (its rows from the bottom), optionally within `x y w h`.
            use unity_micro_editor::playerbake::{decode_png, spritemesh};
            let image = decode_png(&fs::read(png)?)?;
            let (width, height) = (image.width as usize, image.height as usize);
            let rect: Vec<usize> = rest.iter().map(|v| v.parse()).collect::<Result<_, _>>()?;
            let [x, y, w, h] = match rect.as_slice() {
                [x, y, w, h] => [*x, *y, *w, *h],
                _ => [0, 0, width, height],
            };
            let mut alpha = Vec::with_capacity(w * h);
            for row in y..y + h {
                let top = height - 1 - row;
                for column in x..x + w {
                    alpha.push(image.rgba[(top * width + column) * 4 + 3]);
                }
            }
            let settings = spritemesh::Settings {
                detail: -1.0,
                extrude: 1,
                alpha_tolerance: 0,
                hole_detection: true,
            };
            let outlines = spritemesh::outlines(&alpha, w as i32, h as i32, &settings);
            for (i, outline) in outlines.outlines.iter().enumerate() {
                println!("outline {i}: {outline:?}");
            }
            let mesh = spritemesh::tight_mesh(&alpha, w as i32, h as i32, &settings);
            println!("bounds {:?}", mesh.bounds);
            println!("vertices {:?}", mesh.vertices);
            println!("indices {:?}", mesh.indices);
        }
        ["engine-pack", version, output, files @ ..] => {
            // A release's pack of engine classes from files the engine wrote.
            use unity_micro_editor::playerbake::{FileFormat, Library};
            // `FILE#96,1102` takes those native classes of a file alone.
            let mut library = Library::default();
            for file in files {
                let (path, classes) = match file.split_once('#') {
                    Some((path, classes)) => (path, Some(classes)),
                    None => (*file, None),
                };
                let data = fs::read(path)?;
                let added = match classes {
                    Some(classes) => {
                        let ids: Vec<i32> = classes
                            .split(',')
                            .map(|id| id.parse())
                            .collect::<std::result::Result<_, _>>()?;
                        library.add_classes(&data, &ids)
                    }
                    None => library.add(&data),
                };
                if let Err(error) = added {
                    eprintln!("{path} skipped: {error:#}");
                }
            }
            let format = FileFormat {
                version: if version.starts_with("6000.0.") {
                    22
                } else {
                    23
                },
                unity_version: Box::leak(version.to_string().into_boxed_str()),
            };
            let pack = library.to_pack(format)?;
            println!("{} bytes", pack.len());
            fs::write(output, pack)?;
        }
        ["script-templates", output, files @ ..] => {
            use unity_micro_editor::playerbake as bake;
            let mut trees = Vec::new();
            for file in files {
                let data = fs::read(file)?;
                match bake::read_serialized_with_externals(&data) {
                    Ok((objects, _)) => trees.extend(objects.into_iter().map(|object| object.tree)),
                    Err(error) => eprintln!("{file} skipped: {error:#}"),
                }
            }
            let templates = bake::Templates::of_trees(trees.iter().map(|tree| &**tree));
            let scripts = bake::Templates::of_trees(
                trees
                    .iter()
                    .filter(|tree| tree.class_id == 114)
                    .map(|tree| &**tree),
            );
            let pack = templates.to_pack()?;
            let back = bake::Templates::of_pack(&bake::TypeTree::parse(pack.entry.clone())?);
            if back.header != templates.header
                || back.registry != templates.registry
                || back.values != templates.values
            {
                anyhow::bail!("the pack does not read back as its templates");
            }
            let mut names: Vec<_> = templates.values.keys().collect();
            names.sort();
            for name in names {
                let from = match scripts.values.contains_key(name) {
                    true => "a script",
                    false => "the engine's classes",
                };
                println!(
                    "{name}: {} nodes, from {from}",
                    templates.values[name].len()
                );
            }
            println!(
                "header {} nodes, registry {} nodes",
                templates.header.len(),
                templates.registry.len()
            );
            fs::write(output, &pack.entry)?;
        }
        ["script-trees", layouts, file, rest @ ..] => {
            use unity_micro_editor::playerbake as bake;
            let layouts: bake::Layouts = serde_json::from_slice(&fs::read(layouts)?)?;
            let data = fs::read(file)?;
            let (objects, _) = bake::read_serialized_with_externals(&data)?;
            let mut templates = bake::Templates::of_trees(objects.iter().map(|o| &*o.tree));
            // With a release, the layouts take its pack's templates, which must
            // agree with those of the file.
            if let ["--unity-version", version] = rest {
                let Some(pack) = bake::Templates::of_release(version) else {
                    anyhow::bail!("no templates for {version}");
                };
                let script_values = bake::Templates::of_trees(
                    objects
                        .iter()
                        .filter(|o| o.tree.class_id == 114)
                        .map(|o| &*o.tree),
                );
                for (name, value) in &script_values.values {
                    if pack.values.get(name) != Some(value) {
                        println!("template {name} differs from the pack's");
                    }
                }
                if pack.header != templates.header {
                    println!("the script header differs from the pack's");
                }
                templates = pack;
            }
            let mut names = std::collections::HashMap::new();
            let mut assemblies = std::collections::HashMap::new();
            for object in objects.iter().filter(|o| o.tree.class_id == 115) {
                let value = object.tree.decode(&object.data)?;
                let text = |name: &str| match value.field(name) {
                    Some(bake::Val::Str(s)) => s.clone(),
                    _ => String::new(),
                };
                let namespace = text("m_Namespace");
                let class = text("m_ClassName");
                let full = match namespace.is_empty() {
                    true => class.clone(),
                    false => format!("{namespace}.{class}"),
                };
                names.insert(object.local_id, full);
                assemblies.insert(object.local_id, (class, namespace, text("m_AssemblyName")));
            }
            // The entries the tool encodes from the engine's nodes are the engine's,
            // save the types their objects hold, which a file lists at their end.
            let mut encoded = 0;
            for object in &objects {
                let tree = &object.tree;
                let at = if tree.class_id == 114 { 7 + 16 } else { 7 };
                let script = (tree.class_id == 114)
                    .then(|| <[u8; 16]>::try_from(&tree.entry[7..23]).unwrap());
                let hash = <[u8; 16]>::try_from(&tree.entry[at..at + 16]).unwrap();
                let ours =
                    bake::TypeTree::of_nodes(tree.class_id, script, hash, tree.nodes.clone())?;
                let body = ours.entry.len() - 4;
                let held = tree
                    .entry
                    .get(body..body + 4)
                    .map(|count| 4 + 4 * u32::from_le_bytes(count.try_into().unwrap()) as usize);
                let skip = at + 32;
                if tree.entry.len() != body + held.unwrap_or(0)
                    || ours.entry[skip..body] != tree.entry[skip..body]
                    || ours.entry[..5] != tree.entry[..5]
                {
                    anyhow::bail!(
                        "entry of class {} (local id {}) is encoded otherwise",
                        tree.class_id,
                        object.local_id
                    );
                }
                encoded += 1;
            }
            println!("{encoded} entries encoded as the engine wrote them");
            // The hashes of the engine's script layouts are the tool's.
            let (mut hashed, mut misshashed) = (0, 0);
            for object in objects.iter().filter(|o| o.tree.class_id == 114) {
                let Some((class, namespace, assembly)) = object
                    .script
                    .filter(|_| object.script_file == 0)
                    .and_then(|script| assemblies.get(&script))
                else {
                    continue;
                };
                let hash = bake::properties_hash(class, namespace, assembly, &object.tree.nodes);
                match hash[..] == object.tree.entry[23..39] {
                    true => hashed += 1,
                    false => misshashed += 1,
                }
            }
            println!("{hashed} script layouts hashed as the engine, {misshashed} otherwise");
            let layout = bake::Layout::new(&layouts, &templates);
            let (mut equal, mut differ, mut missing) = (0, 0, 0);
            let mut seen = std::collections::HashSet::new();
            for object in &objects {
                let Some(name) = object
                    .script
                    .filter(|_| object.script_file == 0)
                    .and_then(|script| names.get(&script))
                else {
                    continue;
                };
                if !seen.insert(name.clone()) {
                    continue;
                }
                match layout.script(name) {
                    Ok(nodes) => match bake::first_difference(&nodes, &object.tree.nodes) {
                        None => equal += 1,
                        Some(difference) => {
                            differ += 1;
                            println!("== {name} {difference}");
                        }
                    },
                    Err(error) => {
                        missing += 1;
                        println!("== {name} not laid out: {error:#}");
                    }
                }
            }
            println!("equal {equal}, different {differ}, not laid out {missing}");
        }
        ["closure", rest @ ..] => {
            use unity_micro_editor::playerbake::{FileFormat, Library, Sources, Tags};
            let mut libraries = Vec::new();
            let mut roots = Vec::new();
            let (mut project, mut output, mut version) = (None, None, None);
            let mut builtin: Option<std::path::PathBuf> = None;
            let mut layouts_path: Option<&Path> = None;
            let mut scene_root: Option<&str> = None;
            let mut items = rest.iter();
            while let Some(flag) = items.next() {
                let value = items
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?;
                match *flag {
                    "--library" => libraries.push(Path::new(*value)),
                    "--project" => project = Some(Path::new(*value)),
                    "--root" => roots.push(*value),
                    "--builtin-extra" => builtin = Some(Path::new(*value).to_path_buf()),
                    "--output" => output = Some(Path::new(*value)),
                    "--unity-version" => version = Some(value.to_string()),
                    "--layouts" => layouts_path = Some(Path::new(*value)),
                    // A root whose own objects go to a scene file beside the output.
                    "--scene" => scene_root = Some(*value),
                    // The installation whose texture compressor and audio
                    // encoder are used.
                    "--editor" => {
                        unity_micro_editor::playerbake::texcomp::use_editor(Path::new(*value));
                        unity_micro_editor::playerbake::audio::use_editor(Path::new(*value));
                    }
                    other => anyhow::bail!("unknown flag {other}"),
                }
            }
            let (Some(project), Some(output), Some(version)) = (project, output, version) else {
                anyhow::bail!(
                    "player-file closure --library F... --project P --root ASSET... --output F --unity-version V"
                );
            };
            let format = FileFormat {
                version: if version.starts_with("6000.0.") {
                    22
                } else {
                    23
                },
                unity_version: Box::leak(version.into_boxed_str()),
            };
            let stream = format!(
                "{}.resS",
                output.file_name().unwrap_or_default().to_string_lossy()
            );
            // Without a worker's file, the release's pack gives the engine's
            // classes.
            let mut library = match libraries.is_empty() {
                false => Library::read(&libraries)?,
                true => match Library::of_release(format.unity_version) {
                    Some(library) => library?,
                    None => anyhow::bail!("no pack of engine classes for {}", format.unity_version),
                },
            };
            // Script layouts from the compiled assemblies, in place of the
            // libraries' script trees; the engine's templates from the
            // release's pack, or else from those files.
            if let Some(path) = layouts_path {
                use unity_micro_editor::playerbake as bake;
                let layouts: bake::Layouts = serde_json::from_slice(&fs::read(path)?)?;
                let templates = match bake::Templates::of_release(format.unity_version) {
                    Some(templates) => templates,
                    None => {
                        let mut trees = Vec::new();
                        for file in &libraries {
                            let data = fs::read(file)?;
                            let (objects, _) = bake::read_serialized_with_externals(&data)?;
                            trees.extend(objects.into_iter().map(|object| object.tree));
                        }
                        bake::Templates::of_trees(trees.iter().map(|tree| &**tree))
                    }
                };
                let layout = bake::Layout::new(&layouts, &templates);
                let orders = bake::script_execution_orders(project);
                for (name, error) in library.add_scripts(&layout, &orders)? {
                    println!("script {name} not laid out: {error}");
                }
            }
            if let Some(scene) = scene_root {
                roots.push(scene);
            }
            let scene_guid = match scene_root {
                Some(scene) => {
                    let meta = fs::read_to_string(project.join(format!("{scene}.meta")))?;
                    meta.lines()
                        .find_map(|line| line.strip_prefix("guid: "))
                        .map(|guid| guid.trim().to_string())
                }
                None => None,
            };
            let guids: Vec<String> = roots
                .iter()
                .map(|root| {
                    let meta = fs::read_to_string(project.join(format!("{root}.meta")))?;
                    meta.lines()
                        .find_map(|line| line.strip_prefix("guid: "))
                        .map(|guid| guid.trim().to_string())
                        .ok_or_else(|| anyhow::anyhow!("{root}.meta has no GUID"))
                })
                .collect::<Result<_>>()?;
            let guids: Vec<&str> = guids.iter().map(String::as_str).collect();
            let assembled = unity_micro_editor::playerbake::write_scene_closure(
                &library,
                &Tags::of_project(project)?,
                &Sources::of_project(project)?,
                &guids,
                scene_guid.as_deref(),
                format,
                &stream,
                builtin.as_deref(),
            )?;
            if let Some(scene) = &assembled.scene {
                fs::write(output.with_extension("scene"), scene)?;
            }
            fs::write(output, &assembled.file)?;
            fs::write(output.with_file_name(&stream), &assembled.stream)?;
            if !assembled.resource.is_empty() {
                let name = format!("{}.resource", stream.trim_end_matches(".resS"));
                fs::write(output.with_file_name(name), &assembled.resource)?;
            }
            fs::write(
                output.with_file_name(format!(
                    "{}.externals.json",
                    output.file_name().unwrap_or_default().to_string_lossy()
                )),
                serde_json::to_string(&assembled.externals)?,
            )?;
            println!(
                "sources {}, externals {}, refused {}",
                assembled.sources,
                assembled.externals.len(),
                assembled.refused.len()
            );
            for (guid, why) in &assembled.refused {
                println!("refused {guid}: {why}");
            }
            for (field, count) in &assembled.derived {
                println!("derived {count} {field}");
            }
        }
        ["convert", rest @ ..] => {
            use unity_micro_editor::playerbake::{
                FileFormat, Library, Sources, Tags, write_source,
            };
            let mut libraries = Vec::new();
            let (mut project, mut source, mut output, mut version) = (None, None, None, None);
            let mut items = rest.iter();
            while let Some(flag) = items.next() {
                let value = items
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?;
                match *flag {
                    "--library" => libraries.push(Path::new(*value)),
                    "--project" => project = Some(Path::new(*value)),
                    "--source" => source = Some(*value),
                    "--output" => output = Some(Path::new(*value)),
                    "--unity-version" => version = Some(value.to_string()),
                    other => anyhow::bail!("unknown flag {other}"),
                }
            }
            let (Some(project), Some(source), Some(output), Some(version)) =
                (project, source, output, version)
            else {
                anyhow::bail!(
                    "player-file convert --library F... --project P --source ASSET --output F --unity-version V"
                );
            };
            let format = FileFormat {
                version: if version.starts_with("6000.0.") {
                    22
                } else {
                    23
                },
                unity_version: Box::leak(version.into_boxed_str()),
            };
            let library = Library::read(&libraries)?;
            let written = write_source(
                &library,
                &Tags::of_project(project)?,
                &Sources::of_project(project)?,
                &project.join(source),
                format,
            )?;
            fs::write(output, &written.file)?;
            println!("externals: {}", written.externals.join(" "));
            for (object, fields) in &written.derived {
                if !fields.is_empty() {
                    println!("{object} derived: {}", fields.join(", "));
                }
            }
        }
        _ => anyhow::bail!(
            "player-file dump <file> | compare <ours> <theirs> [--tolerance T] | convert ... | closure ... | trees <file> | script-trees <layouts> <file> [--unity-version V] | script-templates <output> <file>... | script-layouts <project> <editor> <player> <output>"
        ),
    }
    Ok(())
}

/// `game-managers --project P --base globalgamemanagers --output F [--compare F]`:
/// writes the managers authored in the project's `ProjectSettings` over a
/// Player's `globalgamemanagers` (experimental, see `playerbake::managers`).
/// Has a standalone import worker write a scene as a build writes it, and
/// lists what the file holds and refers to.
/// Has a standalone import worker build one asset as a build writes it (a
/// shader with its programs), for a reference of the tool's own writer:
/// `player-objects --project P --editor E --output DIR --target ASSET
/// [--users MATERIAL,...] [--shader-cache DIR]` writes `DIR/objects`.
fn run_player_objects(args: Vec<String>) -> Result<()> {
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
            .ok_or_else(|| anyhow::anyhow!("player-objects requires {name}"))
    };
    use unity_micro_editor::{native, playerbake};
    let build = native::PlayerBuild {
        project: flag("--project")?,
        editor: flag("--editor")?,
        output: flag("--output")?,
        target: flag("--target")?,
        users: flags
            .get("--users")
            .map(|users| users.split(',').map(String::from).collect())
            .unwrap_or_default(),
        timeout: std::time::Duration::from_secs(1800),
        reload_list: String::new(),
        shader_cache: flags.get("--shader-cache").cloned().unwrap_or_default(),
    };
    let written =
        native::write_player_objects(&unity_micro_editor::cancel::Context::background(), &build)?;
    let objects: Vec<playerbake::Object<'_>> = written
        .objects
        .iter()
        .map(|object| playerbake::Object {
            local_id: object.local_id,
            tree: &object.tree,
            data: object.data.clone(),
            script: object.script,
            script_file: object.script_file,
        })
        .collect();
    let format = playerbake::FileFormat::of_release("6000.0.71f1").unwrap();
    let file = playerbake::write_in(format, &objects, &[])?;
    fs::write(format!("{}/objects", flag("--output")?), file)?;
    println!(
        "objects {} externals {} imports {} seconds {:.1}",
        written.objects.len(),
        written.externals.len(),
        written.imports,
        written.seconds
    );
    Ok(())
}

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

/// The shaders of a project by name, baked on demand by the tool's own
/// import and baker, for a shader that takes passes of or falls back on
/// them (`player-file shader-bake`).
struct ProjectShaders<'a> {
    names: std::collections::HashMap<String, (String, std::path::PathBuf)>,
    /// The installation and the directory the compilers run in.
    editor: String,
    directory: String,
    project: unity_micro_editor::playerbake::ShaderProject,
    tree: &'a unity_micro_editor::playerbake::TypeTree,
    /// Where the project's packages lie, for the compilers.
    folders: Vec<(String, String)>,
    found: std::cell::RefCell<
        std::collections::HashMap<String, Option<unity_micro_editor::playerbake::ShaderDependency>>,
    >,
    depth: std::cell::Cell<usize>,
}

impl ProjectShaders<'_> {
    /// The shader's Player object, baked.
    fn bake(
        &self,
        logical: &str,
        physical: &std::path::Path,
    ) -> Result<(
        Vec<u8>,
        Vec<unity_micro_editor::playerbake::External>,
        String,
    )> {
        use unity_micro_editor::playerbake::{self as bake, shaderlab};
        let text = fs::read_to_string(physical)?;
        let meta = fs::read(format!("{}.meta", physical.display()))?;
        let guid = String::from_utf8_lossy(&meta)
            .lines()
            .find_map(|l| l.strip_prefix("guid: ").map(|g| g.trim().to_string()))
            .unwrap_or_default();
        let request = shaderlab::PreprocessRequest::standalone(logical, text);
        // Each bake has a compiler of its own: it may bake what it refers to.
        let logs = std::env::temp_dir().join(format!("ume-shader-bake-{}", std::process::id()));
        let mut compiler = unity_micro_editor::native::ShaderCompiler::start_with_folders(
            &unity_micro_editor::cancel::Context::background(),
            &self.editor,
            &self.directory,
            &logs.to_string_lossy(),
            &self.folders,
        )?;
        let reply = compiler.preprocess(&request)?;
        let value = self
            .tree
            .val_of_json(&shaderlab::editor_object(&reply, &request, &guid, &meta)?)?;
        let artifact = bake::write_in(
            bake::FileFormat::of_release("6000.0.71f1").unwrap(),
            &[bake::Object {
                local_id: bake::SHADER_LOCAL_ID,
                tree: self.tree,
                data: self.tree.encode(&value)?,
                script: None,
                script_file: 0,
            }],
            &[],
        )?;
        let (data, externals) =
            bake::bake_shader_object(&artifact, &self.project, self, &mut compiler)?;
        Ok((data, externals, guid))
    }
}

impl unity_micro_editor::playerbake::ShaderSources for ProjectShaders<'_> {
    fn shader(
        &self,
        name: &str,
    ) -> Result<Option<unity_micro_editor::playerbake::ShaderDependency>> {
        use unity_micro_editor::playerbake as bake;
        if let Some(known) = self.found.borrow().get(name) {
            return Ok(known.clone());
        }
        let Some((logical, physical)) = self.names.get(name).cloned() else {
            return Ok(None);
        };
        if self.depth.get() > 4 {
            anyhow::bail!("shaders refer to each other too deeply at {name}");
        }
        self.depth.set(self.depth.get() + 1);
        let baked = self.bake(&logical, &physical);
        self.depth.set(self.depth.get() - 1);
        let (data, externals, guid) = baked?;
        let dependency = bake::ShaderDependency {
            pointer: bake::ShaderPointer::External { guid },
            links: bake::read_shader_links_with(
                bake::ShaderRelease::Unity6000_0,
                &data,
                &externals,
            )?,
        };
        self.found
            .borrow_mut()
            .insert(name.to_string(), Some(dependency.clone()));
        Ok(Some(dependency))
    }
}
