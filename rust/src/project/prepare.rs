//! The `prepare` subcommand: publish one revision from a validated baseline.
use super::{Options, Session};
use std::io::Write;

/// Flag syntax: `-name`, `--name`, `=value` or a separate value; parsing
/// stops at `--` or the first non-flag argument.
fn parse(arguments: &[String], names: &[&str]) -> Result<Vec<Option<String>>, String> {
    let mut values = vec![None; names.len()];
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument.len() < 2 || !argument.starts_with('-') {
            break;
        }
        let name = argument.strip_prefix("--").unwrap_or(&argument[1..]);
        if argument == "--" {
            break;
        }
        if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
            return Err(format!("bad flag syntax: {argument}"));
        }
        index += 1;
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (name, None),
        };
        let Some(slot) = names.iter().position(|known| *known == name) else {
            return Err(format!("flag provided but not defined: -{name}"));
        };
        values[slot] = Some(match inline {
            Some(value) => value,
            None => {
                let value = arguments
                    .get(index)
                    .ok_or_else(|| format!("flag needs an argument: -{name}"))?;
                index += 1;
                value.clone()
            }
        });
    }
    Ok(values)
}

/// Runs `prepare` and returns the process exit code.
pub fn run(arguments: &[String]) -> i32 {
    let values = match parse(
        arguments,
        &["project", "manifest", "native-catalog", "output"],
    ) {
        Ok(values) => values,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let mut values = values.into_iter();
    let mut next = |default: &str| {
        values
            .next()
            .flatten()
            .unwrap_or_else(|| default.to_string())
    };
    // The raw project root defaults to the working directory.
    let (project, manifest, native_catalog, output) = (next("."), next(""), next(""), next(""));
    if manifest.is_empty() || output.is_empty() {
        eprintln!("--manifest and --output are required");
        return 2;
    }
    let prepared = Session::new(Options {
        project,
        manifest,
        native_catalog,
        ..Options::default()
    })
    .and_then(|session| session.prepare(&output))
    .and_then(|prepared| prepared.to_json_text());
    match prepared {
        Ok(json) => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{json}");
            0
        }
        Err(error) => {
            eprintln!("{error:#}");
            1
        }
    }
}
