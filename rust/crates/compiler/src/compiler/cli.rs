//! The `compiler-prepare` subcommand.
use super::prepare_compiler_graph;
use crate::{
    flags::FlagSet,
    util::{default_unity_editor, marshal},
};
use anyhow::{Result, bail};
use std::io::Write;

/// Prepares a compiler baseline from an exported graph and prints the result as
/// one JSON line. The caller prints the error and exits with status 1.
pub fn run_compiler_prepare_cli(arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new("compiler-prepare");
    flags.string("project", "", "explicit Unity project root");
    flags.string(
        "editor",
        &default_unity_editor(),
        "matching installed Unity SDK",
    );
    flags.string("player", "", "compatible native Mono host Player");
    flags.string("graph", "", "public Editor compilation graph export");
    flags.string(
        "output",
        "",
        "new compiler preparation JSON under project Temp or Library/UnityMicroEditor",
    );
    flags.parse(arguments)?;
    let (project, player, graph, output) = (
        flags.get_string("project"),
        flags.get_string("player"),
        flags.get_string("graph"),
        flags.get_string("output"),
    );
    if !flags.args().is_empty()
        || project.is_empty()
        || player.is_empty()
        || graph.is_empty()
        || output.is_empty()
    {
        bail!("compiler-prepare requires --project, --player, --graph and --output");
    }
    let ctx = crate::packages::signal_context()?;
    let result = prepare_compiler_graph(
        &ctx,
        &project,
        &flags.get_string("editor"),
        &player,
        &graph,
        &output,
    )?;
    let mut data = marshal(&result)?;
    data.push(b'\n');
    std::io::stdout().lock().write_all(&data)?;
    Ok(())
}
