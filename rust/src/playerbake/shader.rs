//! Bakes a `.shader` into the `Shader` object a Player loads.
//!
//! The import worker parses a shader and leaves an object that holds the
//! parsed passes and the source of every program, but no compiled program. A
//! build compiles each program for every graphics API and keyword variant
//! with `UnityShaderCompiler` and stores the results in a compressed table.
//! This module does the same from the worker's object: it asks a compiler
//! for each program, lays the replies out as a build does and writes the
//! object through the Player's type tree.
use super::{
    Baked, serialized,
    typetree::{TypeTree, Val},
    unsupported,
};
use anyhow::{Result, anyhow, bail};
use std::{borrow::Cow, sync::LazyLock};

/// The local file ID of the `Shader` object of a `.shader` asset.
pub const SHADER_LOCAL_ID: i64 = 4_800_000;
const SHADER_CLASS: i32 = 48;

static SHADER_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-48.bin")[..]).map_err(|error| error.to_string())
});

/// The Player layout of `Shader` in the pinned engine.
pub fn shader_tree() -> Result<&'static TypeTree> {
    SHADER_TREE
        .as_ref()
        .map_err(|error| anyhow!("pinned Shader type tree: {error}"))
}

/// One program to compile: a pass's source for one graphics API with one
/// set of keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetRequest {
    /// The program source as the importer prepared it. It need not be text.
    pub source: Vec<u8>,
    pub asset_directory: String,
    pub asset_name: String,
    pub shader_name: String,
    pub pass_name: String,
    pub leading: [i32; 3],
    pub build_target: i32,
    /// The platform's defines that are on.
    pub enabled: Vec<String>,
    /// The variant's keywords.
    pub keywords: Vec<String>,
    /// The keywords of the pass the variant leaves off, then the platform's
    /// defines that are off.
    pub disabled: Vec<String>,
    /// How many of `disabled` are keywords.
    pub keywords_off: u32,
    /// Declarations of everything that is off, for the compiler to prepend.
    pub constants: String,
    pub middle: (u64, i32, i32),
    /// The graphics API by its compiler platform number.
    pub platform: u32,
    pub requirements: u64,
    /// The program stages of the pass, one bit each.
    pub program_mask: u32,
    pub start_line: i32,
    pub trailing: i32,
}

/// A compiler's answer: what the program takes as input, and its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetReply {
    pub lines: Vec<Vec<u8>>,
    pub compiled: bool,
    pub program_type: i32,
    pub program: Vec<u8>,
}

pub trait SnippetCompiler {
    fn compile(&mut self, request: &SnippetRequest) -> Result<SnippetReply>;
}

/// What a bake takes from the project beside the shader itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShaderProject {
    /// The graphics APIs of the build by compiler platform number, in the
    /// order the project lists them.
    pub platforms: Vec<u32>,
    /// Whether the project works in linear colour space.
    pub linear: bool,
    /// The keyword sets of the materials that use the shader.
    pub materials: Vec<Vec<String>>,
}

/// The `Shader` object of an import worker artifact, as a value.
pub struct ShaderArtifact {
    pub value: Val,
}

/// Reads the `Shader` object out of the artifact the import worker leaves
/// for a `.shader`.
pub fn read_shader_artifact(file: &[u8]) -> Result<ShaderArtifact> {
    let objects = serialized::read(file)?;
    let mut shaders = objects
        .iter()
        .filter(|object| object.tree.class_id == SHADER_CLASS);
    let (Some(shader), None) = (shaders.next(), shaders.next()) else {
        bail!("shader artifact does not hold exactly one Shader object");
    };
    if shader.local_id != SHADER_LOCAL_ID {
        bail!("shader artifact holds its Shader under another file ID");
    }
    let value = shader.tree.decode(&shader.data)?;
    // The value is complete only if it writes back to the same bytes.
    if shader.tree.encode(&value)? != shader.data {
        bail!("shader artifact is not laid out as its type tree states");
    }
    Ok(ShaderArtifact { value })
}

/// Lays a value read with one tree out for another: the fields the target
/// lacks are dropped, as the engine drops Editor-only data in a build. A
/// field only the target has cannot be filled and is an error.
fn project(tree: &TypeTree, index: usize, value: &Val) -> Result<Val> {
    let node = &tree.nodes[index];
    let children = tree.children(index);
    if children.is_empty() {
        return Ok(value.clone());
    }
    if let Some((_, element)) = tree.element(index) {
        return match value {
            Val::Seq(items) => Ok(Val::Seq(
                items
                    .iter()
                    .map(|item| project(tree, element, item))
                    .collect::<Result<_>>()?,
            )),
            Val::Str(_) | Val::Bytes(_) | Val::Zero => Ok(value.clone()),
            _ => bail!("shader artifact field {} is not a sequence", node.name),
        };
    }
    let Val::Map(fields) = value else {
        bail!("shader artifact field {} is not a structure", node.name);
    };
    let mut result = Vec::with_capacity(children.len());
    for child in children {
        let name = &tree.nodes[child].name;
        let Some((_, field)) = fields.iter().find(|(key, _)| key == name) else {
            bail!("shader artifact lacks the Player field {name}");
        };
        result.push((Cow::Owned(name.clone()), project(tree, child, field)?));
    }
    Ok(Val::Map(result))
}

fn text<'a>(value: &'a Val, name: &str) -> Result<&'a str> {
    match value.field(name) {
        Some(Val::Str(text)) => Ok(text),
        _ => Err(unsupported(format!("shader field {name} that is not text"))),
    }
}

fn sequence<'a>(value: &'a Val, name: &str) -> Result<&'a [Val]> {
    match value.field(name) {
        Some(Val::Seq(items)) => Ok(items),
        Some(Val::Zero) => Ok(&[]),
        _ => bail!("shader artifact lacks the sequence {name}"),
    }
}

fn number(value: &Val, name: &str) -> Result<i64> {
    match value.field(name) {
        Some(Val::I(number)) => Ok(*number),
        Some(Val::U(number)) => Ok(*number as i64),
        Some(Val::Bool(flag)) => Ok(*flag as i64),
        _ => bail!("shader artifact lacks the number {name}"),
    }
}

/// LZ4 in its high compression form at level 9, as a build compresses a
/// table of programs. The reference implementation is linked, since the
/// bytes depend on its match search.
fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let size =
        i32::try_from(data.len()).map_err(|_| anyhow!("shader program table is too large"))?;
    // SAFETY: the bound is the library's own for this input size, and both
    // buffers outlive the call with the lengths passed.
    unsafe {
        let bound = lz4_sys::LZ4_compressBound(size);
        if bound <= 0 {
            bail!("shader program table is too large");
        }
        let mut out = vec![0u8; bound as usize];
        let written = lz4_sys::LZ4_compress_HC(
            data.as_ptr().cast(),
            out.as_mut_ptr().cast(),
            size,
            bound,
            9,
        );
        if written <= 0 {
            bail!("shader program table did not compress");
        }
        out.truncate(written as usize);
        Ok(out)
    }
}

mod blob;
mod variant;

/// Bakes the worker's artifact of a `.shader` into a ContentFile holding the
/// Player's `Shader` object. Every program is compiled by `compiler`.
pub fn bake_shader(
    artifact: &[u8],
    project_settings: &ShaderProject,
    compiler: &mut dyn SnippetCompiler,
) -> Result<Baked> {
    let data = bake_shader_object(artifact, project_settings, compiler)?;
    let file = serialized::write(
        &[serialized::Object {
            local_id: SHADER_LOCAL_ID,
            tree: shader_tree()?,
            data,
            script: None,
        }],
        &[],
    )?;
    Ok(Baked {
        file,
        objects: vec![(SHADER_LOCAL_ID.to_string(), "UnityEngine.Shader".into())],
        externals: Vec::new(),
    })
}

/// The bytes of the Player's `Shader` object alone.
pub fn bake_shader_object(
    artifact: &[u8],
    project_settings: &ShaderProject,
    compiler: &mut dyn SnippetCompiler,
) -> Result<Vec<u8>> {
    let artifact = read_shader_artifact(artifact)?;
    let tree = shader_tree()?;
    let mut value = project(tree, 0, &artifact.value)?;
    object::assemble(&artifact.value, &mut value, project_settings, compiler)?;
    tree.encode(&value)
}

mod object;
