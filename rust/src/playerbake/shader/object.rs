//! Assembles the Player's `Shader` object: compiles every variant of every
//! pass for every graphics API, builds the tables of programs and points the
//! passes at their entries (`ShaderCompilation::CompileShaderVariants` and
//! `PassData::PrepareSerializedData` in the pinned Editor).
use super::{
    ShaderProject, ShaderSources, SnippetCompiler, blob, compress, link, sequence,
    variant::{self, PassPlan},
};
use crate::playerbake::{serialized::External, typetree::Val, unsupported};
use anyhow::{Result, anyhow, bail};
use std::{borrow::Cow, collections::BTreeMap};

fn field_mut<'a>(value: &'a mut Val, name: &str) -> Result<&'a mut Val> {
    value
        .field_mut(name)
        .ok_or_else(|| anyhow!("Player Shader layout lacks {name}"))
}

fn unsigned(values: impl IntoIterator<Item = u32>) -> Val {
    Val::Seq(values.into_iter().map(|v| Val::U(v as u64)).collect())
}

/// A pass lists its programs under one of four headings. A build without
/// hardware tier variants files them all under the last.
const TIERS: usize = 4;

/// What a compiled pass adds to the object.
struct Compiled {
    common: Val,
    names: Val,
    programs: Vec<Vec<Val>>,
    parameters: Vec<Vec<u32>>,
}

fn compile_pass(
    variants: &[variant::Variant],
    keyword_names: &[String],
    tables: &mut BTreeMap<u32, blob::Table>,
    compiler: &mut dyn SnippetCompiler,
) -> Result<Compiled> {
    let mut replies = Vec::with_capacity(variants.len());
    let mut reflections = Vec::with_capacity(variants.len());
    for variant in variants {
        let reply = compiler.compile(&variant.request)?;
        // A build leaves a variant that fails out and reports the error. The
        // shader would lack it in a Player, so the source is refused whole.
        if !reply.compiled {
            return Err(unsupported(format!(
                "a shader program that does not compile: {}",
                blob::error_text(&reply.lines)
            )));
        }
        reflections.push(blob::reflect(&reply)?);
        replies.push(reply);
    }
    // What all programs of the pass read moves out of them.
    let mut parameters: Vec<blob::Parameters> = reflections
        .iter()
        .map(|reflection| reflection.parameters.clone())
        .collect();
    let (common, names) = blob::share(&mut parameters)?.value();
    let mut compiled = Compiled {
        common,
        names,
        programs: vec![Vec::new(); TIERS],
        parameters: vec![Vec::new(); TIERS],
    };
    // The parameter entries of a pass precede its programs.
    let mut parameter_indices = Vec::with_capacity(variants.len());
    for (variant, parameters) in variants.iter().zip(&parameters) {
        let Some(table) = tables.get_mut(&variant.request.platform) else {
            bail!("shader variant is compiled for a graphics API outside the build");
        };
        parameter_indices.push(table.add_parameters(parameters.entry()));
    }
    for (index, variant) in variants.iter().enumerate() {
        let (reply, reflection) = (&replies[index], &reflections[index]);
        let table = tables.get_mut(&variant.request.platform).unwrap();
        let program =
            table.add_program(blob::program(reply, reflection, &variant.request.keywords));
        let mut keywords = Vec::with_capacity(variant.request.keywords.len());
        for keyword in &variant.request.keywords {
            let Some(position) = keyword_names.iter().position(|name| name == keyword) else {
                bail!("shader variant enables the keyword {keyword} the shader does not list");
            };
            keywords.push(position as u64);
        }
        keywords.sort_unstable();
        if variant.tier >= TIERS || !(i8::MIN as i32..=i8::MAX as i32).contains(&reply.program_type)
        {
            bail!("shader program kind or tier is outside the object's layout");
        }
        compiled.programs[variant.tier].push(Val::Map(vec![
            (Cow::Borrowed("m_BlobIndex"), Val::U(program as u64)),
            (
                Cow::Borrowed("m_KeywordIndices"),
                Val::Seq(keywords.into_iter().map(Val::U).collect()),
            ),
            (
                Cow::Borrowed("m_ShaderRequirements"),
                Val::I(variant.request.requirements as i64),
            ),
            (
                Cow::Borrowed("m_GpuProgramType"),
                Val::I(reply.program_type as i64),
            ),
        ]));
        compiled.parameters[variant.tier].push(parameter_indices[index]);
    }
    Ok(compiled)
}

pub(super) fn assemble(
    artifact: &Val,
    value: &mut Val,
    project: &ShaderProject,
    sources: &dyn ShaderSources,
    compiler: &mut dyn SnippetCompiler,
) -> Result<Vec<External>> {
    let parsed = artifact
        .field("m_ParsedForm")
        .ok_or_else(|| anyhow!("shader artifact lacks its parsed form"))?;
    // What the importer says of the source does not reach a Player; a
    // warning leaves the parsed form whole.
    for message in sequence(artifact, "errors")? {
        if super::number(message, "warning")? == 0 {
            return Err(unsupported(format!(
                "a shader with an error: {}",
                super::text(message, "message")?
            )));
        }
    }
    // A build ties a shader to its fallback and to the shaders of the
    // passes it uses: the object lists them and takes over their keywords.
    let links = link::resolve(parsed, sources)?;
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
    // The tables of a build are in ascending order of the graphics API.
    let mut tables: BTreeMap<u32, blob::Table> = project
        .platforms
        .iter()
        .map(|&platform| (platform, blob::Table::default()))
        .collect();
    if tables.len() != project.platforms.len() {
        bail!("a graphics API is listed twice");
    }
    let plans = variant::plan(artifact, project)?;
    let mut plans = plans.into_iter();
    let parsed_value = field_mut(value, "m_ParsedForm")?;
    let Val::Seq(subshaders) = field_mut(parsed_value, "m_SubShaders")? else {
        bail!("Player Shader subshaders are not a sequence");
    };
    for subshader in subshaders {
        let Val::Seq(passes) = field_mut(subshader, "m_Passes")? else {
            bail!("Player Shader passes are not a sequence");
        };
        let mut kept = Vec::with_capacity(passes.len());
        for mut pass in std::mem::take(passes) {
            let Some(plan) = plans.next() else {
                bail!("shader passes outnumber their plans");
            };
            match plan {
                PassPlan::Dropped => continue,
                PassPlan::Untouched => {}
                PassPlan::Compiled(variants) => {
                    let compiled = compile_pass(&variants, &keyword_names, &mut tables, compiler)?;
                    *field_mut(&mut pass, "m_NameIndices")? = compiled.names;
                    let vertex = field_mut(&mut pass, "progVertex")?;
                    *field_mut(vertex, "m_CommonParameters")? = compiled.common;
                    *field_mut(vertex, "m_PlayerSubPrograms")? =
                        Val::Seq(compiled.programs.into_iter().map(Val::Seq).collect());
                    *field_mut(vertex, "m_ParameterBlobIndices")? =
                        Val::Seq(compiled.parameters.into_iter().map(unsigned).collect());
                }
            }
            kept.push(pass);
        }
        *passes = kept;
    }
    if plans.next().is_some() {
        bail!("shader pass plans outnumber the passes");
    }
    // Every segment of every table is compressed on its own; they are stored
    // end to end.
    let mut compressed = Vec::new();
    let (mut offsets, mut lengths, mut sizes) = (Vec::new(), Vec::new(), Vec::new());
    for table in tables.values() {
        let (mut offset, mut length, mut size) = (Vec::new(), Vec::new(), Vec::new());
        for segment in table.segments(project.settings.chunk_megabytes)? {
            let packed = compress(&segment)?;
            offset.push(compressed.len() as u32);
            length.push(packed.len() as u32);
            size.push(segment.len() as u32);
            compressed.extend_from_slice(&packed);
        }
        offsets.push(unsigned(offset));
        lengths.push(unsigned(length));
        sizes.push(unsigned(size));
    }
    if compressed.len() > u32::MAX as usize {
        bail!("shader programs exceed what the object addresses");
    }
    *field_mut(value, "platforms")? = unsigned(tables.keys().copied());
    *field_mut(value, "offsets")? = Val::Seq(offsets);
    *field_mut(value, "compressedLengths")? = Val::Seq(lengths);
    *field_mut(value, "decompressedLengths")? = Val::Seq(sizes);
    *field_mut(value, "compressedBlob")? = Val::Bytes(compressed);
    *field_mut(value, "stageCounts")? = unsigned(tables.keys().map(|_| 1));
    *field_mut(value, "m_ShaderIsBaked")? = Val::Bool(true);
    *field_mut(value, "m_Dependencies")? = Val::Seq(links.dependencies);
    let parsed_value = field_mut(value, "m_ParsedForm")?;
    *field_mut(parsed_value, "m_KeywordNames")? = Val::Seq(
        links
            .keywords
            .iter()
            .map(|(name, _)| Val::Str(name.clone()))
            .collect(),
    );
    *field_mut(parsed_value, "m_KeywordFlags")? =
        Val::Bytes(links.keywords.iter().map(|(_, flag)| *flag).collect());
    Ok(links.externals)
}
