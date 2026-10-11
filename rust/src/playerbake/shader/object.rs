//! Assembles the Player's `Shader` object: compiles every variant of every
//! pass for every graphics API, builds the tables of programs and points the
//! passes at their entries.
use super::{
    ShaderProject, SnippetCompiler, blob, compress, number, sequence, text,
    variant::{self, Snippet},
};
use crate::playerbake::{typetree::Val, unsupported};
use anyhow::{Result, bail};
use std::{borrow::Cow, collections::BTreeMap};

/// One compiled variant as a pass lists it.
struct SubProgram {
    blob: u32,
    parameters: u32,
    keywords: Vec<u16>,
    requirements: u64,
    program_type: i32,
}

fn field_mut<'a>(value: &'a mut Val, name: &str) -> Result<&'a mut Val> {
    value
        .field_mut(name)
        .ok_or_else(|| anyhow::anyhow!("Player Shader layout lacks {name}"))
}

fn unsigned(values: impl IntoIterator<Item = u32>) -> Val {
    Val::Seq(values.into_iter().map(|v| Val::U(v as u64)).collect())
}

pub(super) fn assemble(
    artifact: &Val,
    value: &mut Val,
    project: &ShaderProject,
    compiler: &mut dyn SnippetCompiler,
) -> Result<()> {
    let parsed = artifact
        .field("m_ParsedForm")
        .ok_or_else(|| anyhow::anyhow!("shader artifact lacks its parsed form"))?;
    let shader_name = text(parsed, "m_Name")?.to_string();
    if !sequence(artifact, "errors")?.is_empty() {
        return Err(unsupported("a shader the importer reports errors for"));
    }
    let info = artifact
        .field("m_CompileInfo")
        .ok_or_else(|| anyhow::anyhow!("shader artifact lacks its compile information"))?;
    let mut snippets = BTreeMap::new();
    for entry in sequence(info, "m_Snippets")? {
        let id = number(entry, "first")?;
        let snippet = entry
            .field("second")
            .ok_or_else(|| anyhow::anyhow!("shader artifact snippet is incomplete"))?;
        snippets.insert(id, Snippet::read(snippet)?);
    }
    let keyword_names: Vec<String> = sequence(parsed, "m_KeywordNames")?
        .iter()
        .map(|name| match name {
            Val::Str(name) => Ok(name.clone()),
            _ => Err(unsupported("a shader keyword that is not text")),
        })
        .collect::<Result<_>>()?;
    if project.platforms.is_empty() {
        bail!("a shader is baked for at least one graphics API");
    }
    // Per pass, in the order of the parsed form: the variants of every
    // graphics API.
    let mut passes: Vec<Vec<SubProgram>> = Vec::new();
    let mut tables = Vec::new();
    for &platform in &project.platforms {
        let mut table = blob::Table::default();
        let mut pass_index = 0;
        for subshader in sequence(parsed, "m_SubShaders")? {
            for pass in sequence(subshader, "m_Passes")? {
                if passes.len() <= pass_index {
                    passes.push(Vec::new());
                }
                let state = pass
                    .field("m_State")
                    .ok_or_else(|| anyhow::anyhow!("shader pass lacks its state"))?;
                let pass_type = number(pass, "m_Type")?;
                let id = number(state, "gpuProgramID")?;
                let mask = number(pass, "m_ProgramMask")? as u32;
                let snippet = snippets.get(&id);
                let variants = match (pass_type, snippet) {
                    (0, Some(snippet)) => variant::enumerate(
                        snippet,
                        &shader_name,
                        text(state, "m_Name")?,
                        mask,
                        platform,
                        project,
                        &keyword_names,
                    )?,
                    (0, None) => {
                        return Err(unsupported("a pass without a program to compile"));
                    }
                    _ => {
                        return Err(unsupported(
                            "a pass taken from another shader or a grab pass",
                        ));
                    }
                };
                let mut compiled = Vec::with_capacity(variants.len());
                for variant in &variants {
                    let reply = compiler.compile(&variant.request)?;
                    if !reply.compiled {
                        return Err(unsupported(format!(
                            "a shader program that does not compile: {}",
                            blob::error_text(&reply.lines)
                        )));
                    }
                    compiled.push(reply);
                }
                // The parameter tables of a pass's variants come before its
                // programs.
                let parameters: Vec<u32> = compiled
                    .iter()
                    .map(|reply| Ok(table.add(blob::parameters(&reply.lines)?)))
                    .collect::<Result<_>>()?;
                for ((variant, reply), parameters) in variants.iter().zip(&compiled).zip(parameters)
                {
                    let program = blob::program(reply, &variant.request.keywords)?;
                    passes[pass_index].push(SubProgram {
                        blob: table.add(program),
                        parameters,
                        keywords: variant.keyword_indices.clone(),
                        requirements: variant.request.requirements,
                        program_type: reply.program_type,
                    });
                }
                pass_index += 1;
            }
        }
        tables.push(table.segment());
    }
    // Every table is compressed on its own and they are stored end to end.
    let mut compressed = Vec::new();
    let (mut offsets, mut lengths, mut sizes) = (Vec::new(), Vec::new(), Vec::new());
    for table in &tables {
        let packed = compress(table)?;
        offsets.push(Val::Seq(vec![Val::U(compressed.len() as u64)]));
        lengths.push(Val::Seq(vec![Val::U(packed.len() as u64)]));
        sizes.push(Val::Seq(vec![Val::U(table.len() as u64)]));
        compressed.extend_from_slice(&packed);
    }
    let mut passes = passes.into_iter();
    let parsed = field_mut(value, "m_ParsedForm")?;
    let Val::Seq(subshaders) = field_mut(parsed, "m_SubShaders")? else {
        bail!("Player Shader subshaders are not a sequence");
    };
    for subshader in subshaders {
        let Val::Seq(pass_values) = field_mut(subshader, "m_Passes")? else {
            bail!("Player Shader passes are not a sequence");
        };
        for pass in pass_values {
            let programs = passes.next().unwrap_or_default();
            let vertex = field_mut(pass, "progVertex")?;
            let listed: Vec<Val> = programs
                .iter()
                .map(|program| {
                    Val::Map(vec![
                        (Cow::Borrowed("m_BlobIndex"), Val::U(program.blob as u64)),
                        (
                            Cow::Borrowed("m_KeywordIndices"),
                            Val::Seq(program.keywords.iter().map(|&k| Val::U(k as u64)).collect()),
                        ),
                        (
                            Cow::Borrowed("m_ShaderRequirements"),
                            Val::I(program.requirements as i64),
                        ),
                        (
                            Cow::Borrowed("m_GpuProgramType"),
                            Val::I(program.program_type as i64),
                        ),
                    ])
                })
                .collect();
            // A pass lists its programs by hardware tier; a build without
            // tier variants files them all under the last.
            let tiers = |last: Val| Val::Seq(vec![Val::Zero, Val::Zero, Val::Zero, last]);
            *field_mut(vertex, "m_PlayerSubPrograms")? = tiers(Val::Seq(listed));
            *field_mut(vertex, "m_ParameterBlobIndices")? =
                tiers(unsigned(programs.iter().map(|program| program.parameters)));
        }
    }
    *field_mut(value, "platforms")? = unsigned(project.platforms.iter().copied());
    *field_mut(value, "offsets")? = Val::Seq(offsets);
    *field_mut(value, "compressedLengths")? = Val::Seq(lengths);
    *field_mut(value, "decompressedLengths")? = Val::Seq(sizes);
    *field_mut(value, "compressedBlob")? = Val::Bytes(compressed);
    *field_mut(value, "stageCounts")? = unsigned(project.platforms.iter().map(|_| 1));
    *field_mut(value, "m_ShaderIsBaked")? = Val::Bool(true);
    Ok(())
}
