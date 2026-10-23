//! Compiling the authored source graph with the
//! SDK, and comparing an own graph with a public reference graph.
use super::{SourceGraphOptions, build_source_graph, sorted_strings, validate_source_output};
use crate::{
    cancel::Context,
    compiler::{
        AssemblyConfig, CompilerGraphExport, CompilerPreparationResult, NilVec, PublishBaseline,
        equal_strings, prepare_compiler_graph_with, publish_baseline,
    },
    sysutil,
    util::{agent_canonical, agent_id, atomic_json, marshal, within_directory},
};
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use std::{collections::BTreeMap, fs, os::unix::fs::DirBuilderExt};

struct Remove(String);
impl Drop for Remove {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Prepares the source compiler graph with the default baseline publication.
pub fn prepare_source_compiler_graph(
    ctx: &Context,
    options: &SourceGraphOptions,
    output: &str,
) -> Result<CompilerPreparationResult> {
    prepare_source_compiler_graph_with(ctx, options, output, &publish_baseline)
}

/// [`prepare_source_compiler_graph`] of a graph the caller just built with
/// the same options; the sources are compared with it again before the
/// baseline is published.
pub fn prepare_source_compiler_graph_from(
    ctx: &Context,
    options: &SourceGraphOptions,
    graph: CompilerGraphExport,
    output: &str,
) -> Result<CompilerPreparationResult> {
    prepare(ctx, options, Some(graph), output, &publish_baseline)
}

/// `publish` is the seam for publishing the prepared baseline.
pub fn prepare_source_compiler_graph_with(
    ctx: &Context,
    options: &SourceGraphOptions,
    output: &str,
    publish: PublishBaseline,
) -> Result<CompilerPreparationResult> {
    prepare(ctx, options, None, output, publish)
}

fn prepare(
    ctx: &Context,
    options: &SourceGraphOptions,
    graph: Option<CompilerGraphExport>,
    output: &str,
    publish: PublishBaseline,
) -> Result<CompilerPreparationResult> {
    validate_source_output(&options.project, output)?;
    let graph = match graph {
        Some(graph) => graph,
        None => build_source_graph(ctx, options)?,
    };
    agent_canonical(output)?;
    let directory = sysutil::dir(output);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .map_err(|error| anyhow!(sysutil::Error::os("mkdir", &directory, &error)))?;
    let graph_file = tempfile::Builder::new()
        .prefix(".source-graph-")
        .suffix(".json")
        .tempfile_in(&directory)
        .map_err(|error| {
            anyhow!(sysutil::Error::os(
                "open",
                &sysutil::join(&[&directory, ".source-graph-*.json"]),
                &error
            ))
        })?
        .into_temp_path()
        .keep()?
        .to_string_lossy()
        .into_owned();
    let _graph_file = Remove(graph_file.clone());
    atomic_json(&graph_file, &graph)?;
    let intermediate = sysutil::join(&[
        &directory,
        &format!(".source-preparation-{}.json", agent_id()),
    ]);
    let _intermediate = Remove(intermediate.clone());
    let result = prepare_compiler_graph_with(
        ctx,
        &options.project,
        &options.editor,
        &options.player,
        &graph_file,
        &intermediate,
        publish,
    )?;
    if let Some(packages) = &options.packages {
        packages.validate(ctx)?;
    }
    let current = build_source_graph(ctx, options)?;
    if marshal(&graph)? != marshal(&current)? {
        bail!("authored source graph changed before compiler publication");
    }
    publish(output, &result)?;
    Ok(result)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceGraphDifference {
    pub assembly: String,
    pub field: String,
    pub own: NilVec<String>,
    pub editor: NilVec<String>,
}

struct Node {
    configuration: AssemblyConfig,
    references: NilVec<String>,
    response_files: NilVec<String>,
    complete: bool,
}
fn flatten(graph: &CompilerGraphExport) -> BTreeMap<String, Node> {
    let mut result = BTreeMap::new();
    for assembly in &graph.assemblies {
        let mut configuration = assembly.config.clone();
        if configuration.dependencies.is_empty() {
            configuration.dependencies = assembly.references.clone();
        }
        result.insert(
            assembly.config.name.clone(),
            Node {
                configuration,
                references: assembly.compiled_references.clone(),
                response_files: assembly.response_files.clone(),
                complete: true,
            },
        );
    }
    for assembly in &graph.host_assemblies {
        let configuration = match &assembly.configuration {
            Some(configuration) => (**configuration).clone(),
            None => AssemblyConfig {
                name: assembly.name.clone(),
                sources: assembly.sources.clone(),
                defines: assembly.defines.clone(),
                ..Default::default()
            },
        };
        result.insert(
            assembly.name.clone(),
            Node {
                configuration,
                references: assembly.compiled_references.clone(),
                response_files: assembly.response_files.clone(),
                complete: assembly.configuration.is_some(),
            },
        );
    }
    result
}
fn logical(graph: &CompilerGraphExport, paths: &[String]) -> NilVec<String> {
    let mut result = NilVec::make();
    for path in paths {
        let mut path = path.clone();
        let mut mapped = false;
        for root in &graph.source_roots {
            if within_directory(&path, &root.source) {
                let relative = sysutil::rel(&root.source, &path).unwrap_or_default();
                path = format!("{}/{relative}", root.path);
                mapped = true;
                break;
            }
        }
        if !mapped
            && let Some(relative) = sysutil::rel(&graph.project, &path)
            && !relative.starts_with("..")
        {
            path = relative;
        }
        result.push(path);
    }
    sorted_strings(&result)
}
fn flag(value: bool) -> NilVec<String> {
    NilVec::from([value.to_string()])
}

/// Differences between an own graph and a public
/// Editor reference, ordered by assembly and field.
pub fn compare_source_graphs(
    own: &CompilerGraphExport,
    reference: &CompilerGraphExport,
) -> NilVec<SourceGraphDifference> {
    let (first, second) = (flatten(own), flatten(reference));
    let names: std::collections::BTreeSet<&String> = first.keys().chain(second.keys()).collect();
    let empty = Node {
        configuration: AssemblyConfig::default(),
        references: NilVec::nil(),
        response_files: NilVec::nil(),
        complete: false,
    };
    let mut differences = Vec::new();
    for name in names {
        let (an, bn) = (first.get(name), second.get(name));
        if an.is_some() != bn.is_some() {
            differences.push(SourceGraphDifference {
                assembly: name.clone(),
                field: "assembly".into(),
                own: flag(an.is_some()),
                editor: flag(bn.is_some()),
            });
            continue;
        }
        let (an, bn) = (an.unwrap_or(&empty), bn.unwrap_or(&empty));
        let (a, b) = (&an.configuration, &bn.configuration);
        let mut fields = vec![
            (
                "sources",
                logical(own, &a.sources),
                logical(reference, &b.sources),
            ),
            (
                "defines",
                sorted_strings(&a.defines),
                sorted_strings(&b.defines),
            ),
        ];
        if bn.complete {
            fields.extend([
                (
                    "dependencies",
                    sorted_strings(&a.dependencies),
                    sorted_strings(&b.dependencies),
                ),
                (
                    "unsafe",
                    flag(a.config.allow_unsafe),
                    flag(b.config.allow_unsafe),
                ),
                (
                    "noEngineReferences",
                    flag(a.config.no_engine_references),
                    flag(b.config.no_engine_references),
                ),
                (
                    "overrideReferences",
                    flag(a.config.override_references),
                    flag(b.config.override_references),
                ),
                (
                    "precompiledReferences",
                    sorted_strings(&a.config.precompiled),
                    sorted_strings(&b.config.precompiled),
                ),
                (
                    "compilerOptions",
                    a.compiler_options.clone(),
                    b.compiler_options.clone(),
                ),
                (
                    "responseFiles",
                    logical(own, &an.response_files),
                    logical(reference, &bn.response_files),
                ),
                (
                    "pluginReferences",
                    logical(own, &plugin_reference_paths(own, &an.references)),
                    logical(
                        reference,
                        &plugin_reference_paths(reference, &bn.references),
                    ),
                ),
            ]);
        }
        for (field, own, editor) in fields {
            if !equal_strings(&own, &editor) {
                differences.push(SourceGraphDifference {
                    assembly: name.clone(),
                    field: field.into(),
                    own,
                    editor,
                });
            }
        }
    }
    // Equal keys cannot occur for one assembly.
    differences
        .sort_by_cached_key(|difference| format!("{}{}", difference.assembly, difference.field));
    if differences.is_empty() {
        NilVec::nil()
    } else {
        differences.into()
    }
}

/// The DLL references that live in the
/// project's Assets or in a declared source root.
pub fn plugin_reference_paths(graph: &CompilerGraphExport, paths: &[String]) -> NilVec<String> {
    let mut result = NilVec::make();
    for path in paths {
        if sysutil::ext(path) != ".dll" {
            continue;
        }
        if within_directory(path, &sysutil::join(&[&graph.project, "Assets"]))
            || graph
                .source_roots
                .iter()
                .any(|root| within_directory(path, &root.source))
        {
            result.push(path.clone());
        }
    }
    result
}
