//! The `source-graph`, `compiler-source-prepare` and
//! `engine-profile-capture` subcommands. Modes are explicit: harvesting uses a
//! supplied public reference; graph and compile only read cached profile
//! facts, authored sources and package proofs.
use super::{
    SourceGraphDifference, SourceGraphOptions, build_source_graph, capture_engine_profile,
    compare_source_graphs, load_engine_profile, prepare_source_compiler_graph,
    validate_source_output,
};
use crate::{
    compiler::{CompilerGraphExport, NilVec},
    flags::FlagSet,
    packages,
    project::lexpath,
    sysutil,
    util::{agent_immutable_json, agent_read_json, default_unity_editor, marshal},
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::io::Write;

fn emit<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    let mut data = marshal(value)?;
    data.push(b'\n');
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&data)?;
    stdout.flush()?;
    Ok(())
}

/// The `source-graph` report. Keys are sorted.
#[derive(Serialize)]
struct Report<'a> {
    backend: &'a str,
    differences: NilVec<SourceGraphDifference>,
    #[serde(rename = "engineProfile")]
    engine_profile: &'a str,
    graph: &'a str,
    #[serde(rename = "nativeAssemblies")]
    native_assemblies: usize,
    #[serde(rename = "sdkAssemblies")]
    sdk_assemblies: usize,
}

/// The caller prints the error and exits with status 1.
pub fn run_source_graph_cli(mode: &str, arguments: Vec<String>) -> Result<()> {
    let mut flags = FlagSet::new(mode);
    flags.string("project", "", "explicit target project");
    flags.string(
        "editor",
        &default_unity_editor(),
        "pinned installed Unity SDK",
    );
    flags.string("player", "", "matching native Mono host");
    flags.string(
        "engine-profile",
        "",
        "cached pinned engine/native host facts",
    );
    flags.string(
        "reference",
        "",
        "explicit public reference graph for capture or comparison",
    );
    flags.string("output", "", "new owned cache JSON output");
    flags.bool("offline", false, "require local verified package snapshots");
    flags.parse(arguments)?;
    let (project, editor, player, profile_path, reference, output) = (
        flags.get_string("project"),
        flags.get_string("editor"),
        flags.get_string("player"),
        flags.get_string("engine-profile"),
        flags.get_string("reference"),
        flags.get_string("output"),
    );
    if !flags.args().is_empty() || project.is_empty() || player.is_empty() || output.is_empty() {
        bail!("{mode} requires --project --player --output");
    }
    let project = sysutil::eval_symlinks(&lexpath::abs(&project)?)?;
    validate_source_output(&project, &output)?;
    let ctx = packages::signal_context()?;
    let snapshot = packages::resolve(
        &ctx,
        &project,
        &editor,
        packages::Options {
            offline: flags.get_bool("offline"),
            ..Default::default()
        }
        .with_runtime_package(&project)?,
    )?;
    if mode == "engine-profile-capture" {
        if reference.is_empty() {
            bail!("{mode} requires --reference");
        }
        let profile = capture_engine_profile(
            &ctx,
            &project,
            &editor,
            &player,
            &reference,
            &output,
            Some(&snapshot),
        )?;
        return emit(&profile);
    }
    if profile_path.is_empty() {
        bail!("{mode} requires --engine-profile");
    }
    let profile = load_engine_profile(&ctx, &profile_path, &editor, &player)?;
    let options = SourceGraphOptions {
        project,
        editor,
        player,
        profile: Some(profile),
        packages: Some(snapshot),
        editor_domain: false,
    };
    if mode == "compiler-source-prepare" {
        return emit(&prepare_source_compiler_graph(&ctx, &options, &output)?);
    }
    if mode != "source-graph" {
        bail!("unknown source graph mode");
    }
    let graph = build_source_graph(&ctx, &options)?;
    agent_immutable_json(&output, &graph)?;
    let mut differences = NilVec::nil();
    if !reference.is_empty() {
        let public_graph: CompilerGraphExport =
            agent_read_json::<Option<_>>(&reference, 32 << 20)?.unwrap_or_default();
        differences = compare_source_graphs(&graph, &public_graph);
    }
    emit(&Report {
        backend: "authored_source",
        differences,
        engine_profile: &profile_path,
        graph: &output,
        native_assemblies: graph.host_assemblies.len(),
        sdk_assemblies: graph.assemblies.len(),
    })
}
