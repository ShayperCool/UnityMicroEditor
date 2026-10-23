//! What ties a shader to other shaders: a `Fallback` and the passes it takes
//! with `UsePass`. A build lists those shaders in the object and appends
//! their keywords to the shader's own (`SerializedShader::CreateNamesTable`
//! in the pinned Editor, confirmed against its builds).
use super::{number, sequence, shader_tree, text};
use crate::playerbake::{serialized::External, typetree::Val, unsupported};
use anyhow::{Result, anyhow, bail};
use std::borrow::Cow;

/// Where the object's reference to another shader leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShaderPointer {
    /// The `Shader` object of another published source, by its GUID.
    External { guid: String },
    /// A file and object stated as they are.
    Raw { file_id: i32, path_id: i64 },
}

/// A pass of a shader, as another shader's `UsePass` finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderPass {
    pub name: String,
    /// Whether the pass holds programs of its own.
    pub compiled: bool,
    /// The keywords its programs tell apart, as positions in the shader's
    /// keyword list.
    pub keywords: Vec<u16>,
}

/// What a shader that refers to another takes from it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShaderLinks {
    /// The keyword list of the Player object with the flag of each keyword:
    /// the shader's own, then those it took from the shaders it refers to.
    pub keywords: Vec<(String, u8)>,
    pub fallback: String,
    pub passes: Vec<ShaderPass>,
}

/// A shader another one takes passes from or falls back on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderDependency {
    pub pointer: ShaderPointer,
    pub links: ShaderLinks,
}

/// Finds the shaders a shader names.
pub trait ShaderSources {
    /// The shader of a name, or `None` if the project has none.
    fn shader(&self, name: &str) -> Result<Option<ShaderDependency>>;
}

/// A caller that knows no other shader: a shader that names one is refused.
pub struct NoShaders;
impl ShaderSources for NoShaders {
    fn shader(&self, _: &str) -> Result<Option<ShaderDependency>> {
        Ok(None)
    }
}

fn keyword_table(parsed: &Val) -> Result<Vec<(String, u8)>> {
    let names = sequence(parsed, "m_KeywordNames")?;
    let flags: Vec<u8> = match parsed.field("m_KeywordFlags") {
        Some(Val::Seq(flags)) => flags
            .iter()
            .map(|flag| match flag {
                Val::U(flag) => Ok(*flag as u8),
                Val::Zero => Ok(0),
                _ => bail!("shader keyword flag is not a number"),
            })
            .collect::<Result<_>>()?,
        Some(Val::Bytes(flags)) => flags.clone(),
        Some(Val::Zero) => Vec::new(),
        _ => bail!("shader lacks its keyword flags"),
    };
    if names.len() != flags.len() {
        bail!("shader keyword names and flags differ in number");
    }
    names
        .iter()
        .zip(flags)
        .map(|(name, flag)| match name {
            Val::Str(name) => Ok((name.clone(), flag)),
            _ => Err(unsupported("a shader keyword that is not text")),
        })
        .collect()
}

const PROGRAMS: [&str; 6] = [
    "progVertex",
    "progFragment",
    "progGeometry",
    "progHull",
    "progDomain",
    "progRayTracing",
];

fn passes(parsed: &Val) -> Result<Vec<ShaderPass>> {
    let mut out = Vec::new();
    for subshader in sequence(parsed, "m_SubShaders")? {
        for pass in sequence(subshader, "m_Passes")? {
            let state = pass
                .field("m_State")
                .ok_or_else(|| anyhow!("shader pass lacks its state"))?;
            let mut keywords = Vec::new();
            for program in PROGRAMS {
                let program = pass
                    .field(program)
                    .ok_or_else(|| anyhow!("shader pass lacks a program"))?;
                for keyword in sequence(program, "m_SerializedKeywordStateMask")? {
                    match keyword {
                        Val::U(keyword) => keywords.push(*keyword as u16),
                        Val::Zero => keywords.push(0),
                        _ => bail!("shader keyword mask is not a number"),
                    }
                }
            }
            keywords.sort_unstable();
            keywords.dedup();
            out.push(ShaderPass {
                name: text(state, "m_Name")?.to_string(),
                compiled: number(pass, "m_Type")? == 0,
                keywords,
            });
        }
    }
    Ok(out)
}

/// Reads what a referring shader takes from a `Shader` object in the Player
/// form.
pub fn read_shader_links(object: &[u8]) -> Result<ShaderLinks> {
    let value = shader_tree()?.decode(object)?;
    let parsed = value
        .field("m_ParsedForm")
        .ok_or_else(|| anyhow!("Shader object lacks its parsed form"))?;
    Ok(ShaderLinks {
        keywords: keyword_table(parsed)?,
        fallback: text(parsed, "m_FallbackName")?.to_string(),
        passes: passes(parsed)?,
    })
}

/// The object's ties to other shaders.
pub(super) struct Links {
    /// The keyword list of the object: the shader's own, then what the
    /// shaders it refers to add.
    pub(super) keywords: Vec<(String, u8)>,
    /// The shaders the object lists.
    pub(super) dependencies: Vec<Val>,
    pub(super) externals: Vec<External>,
}

/// Resolves the fallback and the used passes of a parsed shader.
pub(super) fn resolve(parsed: &Val, sources: &dyn ShaderSources) -> Result<Links> {
    let mut keywords = keyword_table(parsed)?;
    let find = |name: &str, what: &str| -> Result<ShaderDependency> {
        sources.shader(name)?.ok_or_else(|| {
            unsupported(format!(
                "{what} {name:?}, a shader the project does not hold as a source"
            ))
        })
    };
    // The shaders the object lists: those of used passes in pass order, the
    // fallback, then the fallbacks of the fallback.
    let mut names: Vec<String> = Vec::new();
    let mut found: Vec<ShaderDependency> = Vec::new();
    // What each of them adds to the keywords.
    let mut additions: Vec<Vec<(String, u8)>> = Vec::new();
    for subshader in sequence(parsed, "m_SubShaders")? {
        for pass in sequence(subshader, "m_Passes")? {
            if number(pass, "m_Type")? != 1 {
                continue;
            }
            let used = text(pass, "m_UseName")?;
            let Some((shader, pass_name)) = used.rsplit_once('/') else {
                return Err(unsupported(format!("the used pass {used:?}")));
            };
            let dependency = match names.iter().position(|name| name == shader) {
                Some(index) => found[index].clone(),
                None => {
                    let dependency = find(shader, "a pass of")?;
                    names.push(shader.to_string());
                    found.push(dependency.clone());
                    dependency
                }
            };
            let links = &dependency.links;
            let matching: Vec<&ShaderPass> = links
                .passes
                .iter()
                .filter(|pass| pass.name.eq_ignore_ascii_case(pass_name))
                .collect();
            if matching.is_empty() || matching.iter().any(|pass| !pass.compiled) {
                return Err(unsupported(format!(
                    "the used pass {used:?}, which its shader lacks or takes from a third"
                )));
            }
            // A used pass brings the keywords its programs tell apart.
            let mut indices: Vec<u16> = matching
                .iter()
                .flat_map(|pass| pass.keywords.iter().copied())
                .collect();
            indices.sort_unstable();
            indices.dedup();
            let mut addition = Vec::new();
            for index in indices {
                let Some(keyword) = links.keywords.get(index as usize) else {
                    bail!("used pass refers to a keyword its shader does not list");
                };
                addition.push(keyword.clone());
            }
            additions.push(addition);
        }
    }
    let mut fallback = text(parsed, "m_FallbackName")?.to_string();
    let mut first = true;
    while !fallback.is_empty() && !names.contains(&fallback) {
        let dependency = find(&fallback, "the fallback")?;
        if first {
            // The fallback brings all its keywords, its own fallbacks'
            // among them.
            additions.push(dependency.links.keywords.clone());
        }
        first = false;
        names.push(fallback);
        fallback = dependency.links.fallback.clone();
        found.push(dependency);
    }
    if first
        && !fallback.is_empty()
        && let Some(index) = names.iter().position(|name| *name == fallback)
    {
        // The fallback is also a shader of a used pass.
        additions.push(found[index].links.keywords.clone());
    }
    // In which order several shaders add their keywords was not established.
    let adding = additions
        .iter()
        .filter(|addition| {
            addition
                .iter()
                .any(|(name, _)| !keywords.iter().any(|(known, _)| known == name))
        })
        .count();
    if adding > 1 {
        return Err(unsupported(
            "a shader whose keywords are extended by more than one other shader",
        ));
    }
    for (name, flag) in additions.into_iter().flatten() {
        if !keywords.iter().any(|(known, _)| *known == name) {
            keywords.push((name, flag));
        }
    }
    let mut externals: Vec<External> = Vec::new();
    let mut dependencies = Vec::new();
    for dependency in &found {
        let (file_id, path_id) = match &dependency.pointer {
            ShaderPointer::Raw { file_id, path_id } => (*file_id as i64, *path_id),
            ShaderPointer::External { guid } => {
                let position = match externals.iter().position(|known| known.guid == *guid) {
                    Some(position) => position,
                    None => {
                        externals.push(External {
                            guid: guid.clone(),
                            path: String::new(),
                        });
                        externals.len() - 1
                    }
                };
                (position as i64 + 1, super::SHADER_LOCAL_ID)
            }
        };
        dependencies.push(Val::Map(vec![
            (Cow::Borrowed("m_FileID"), Val::I(file_id)),
            (Cow::Borrowed("m_PathID"), Val::I(path_id)),
        ]));
    }
    Ok(Links {
        keywords,
        dependencies,
        externals,
    })
}
