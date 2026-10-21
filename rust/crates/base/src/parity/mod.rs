//! Clients of the Unity Editor reference harness in `Integrations/EditorParity`.
//!
//! Each client is a `parity-*` subcommand of the main binary and reports its result
//! through the process exit status, so `run` returns that status instead of an error.
pub mod advanced_check;
pub mod compare;
pub mod compare_png;
pub mod compare_source;
mod image;
pub mod mcp_call;

use crate::flags::{FlagError, FlagSet};
use std::io::Write;

/// The subcommand that performs one MCP request; `parity-advanced-check` runs it as a child.
pub const MCP_CALL_COMMAND: &str = "parity-mcp-call";

/// Runs a `parity-*` subcommand and returns its exit status, or `None` for an unknown name.
pub fn run(command: &str, args: Vec<String>) -> Option<i32> {
    Some(match command {
        MCP_CALL_COMMAND => mcp_call::main(args),
        "parity-compare" => compare::main(args),
        "parity-compare-png" => compare_png::main(args),
        "parity-compare-source" => compare_source::main(args),
        "parity-advanced-check" => advanced_check::main(args),
        _ => return None,
    })
}

/// Parses the flags of one subcommand; a usage error is status 2 and `-h` is status 0.
fn parse(flags: &mut FlagSet, args: Vec<String>) -> Result<(), i32> {
    match flags.parse(args) {
        Ok(()) => Ok(()),
        Err(FlagError::Help) => Err(0),
        Err(FlagError::Invalid(_)) => Err(2),
    }
}

/// Evidence may contain project content, so reports are readable by their owner only.
fn write_private(path: &str, content: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(content)
}

/// Saves a report to the optional output file and prints it.
fn emit(output: &str, report: &str) -> std::io::Result<()> {
    if !output.is_empty() {
        write_private(output, report.as_bytes())?;
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(report.as_bytes())?;
    stdout.flush()
}

/// Indented JSON with a final newline.
fn pretty<T: serde::Serialize>(value: &T) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}
