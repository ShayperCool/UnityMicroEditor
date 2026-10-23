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
                for key in keys.iter().take(20) {
                    println!("{label}: {key}");
                }
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
            let paths: Vec<&Path> = files.iter().map(Path::new).collect();
            let mut library = Library::default();
            for path in paths {
                if let Err(error) = library.add(&fs::read(path)?) {
                    eprintln!("{} skipped: {error:#}", path.display());
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
                let Some((class, namespace, assembly)) =
                    object.script.and_then(|script| assemblies.get(&script))
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
                let Some(name) = object.script.and_then(|script| names.get(&script)) else {
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
            use unity_micro_editor::playerbake::{
                FileFormat, Library, Sources, Tags, write_closure,
            };
            let mut libraries = Vec::new();
            let mut roots = Vec::new();
            let (mut project, mut output, mut version) = (None, None, None);
            let mut builtin: Option<std::path::PathBuf> = None;
            let mut layouts_path: Option<&Path> = None;
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
                    // The installation whose texture compressor is used.
                    "--editor" => {
                        unity_micro_editor::playerbake::texcomp::use_editor(Path::new(*value))
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
            let assembled = write_closure(
                &library,
                &Tags::of_project(project)?,
                &Sources::of_project(project)?,
                &guids,
                format,
                &stream,
                builtin.as_deref(),
            )?;
            fs::write(output, &assembled.file)?;
            fs::write(output.with_file_name(&stream), &assembled.stream)?;
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
