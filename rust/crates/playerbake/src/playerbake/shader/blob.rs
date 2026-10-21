//! The tables a Player reads compiled programs from. Per graphics API a
//! build keeps a list of entries, each either the parameters of a program
//! (what it reads from materials and the engine) or the program itself. The
//! layouts and the rules that fill them were read out of the pinned Editor
//! (`ShaderCompilerCompileSnippet`, `SaveParametersToData`,
//! `SaveGpuProgramToDataWithoutParameters`,
//! `SerializedProgram::DeduplicateParameters`, `SubProgramBlobWriter::Flush`)
//! and confirmed against its builds.
use super::SnippetReply;
use crate::playerbake::{typetree::Val, unsupported};
use anyhow::{Result, bail};
use std::{borrow::Cow, collections::HashMap};

/// The revision of the entry layouts in the pinned engine.
const LAYOUT: u32 = 0x0c13_82ea;

/// Where a program finds a resource: a packed description and a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Binding {
    encoded: u32,
    slot: u32,
}

/// A vector or matrix a program reads from a constant buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Constant {
    name: String,
    offset: u32,
    array: u32,
    kind: u32,
    /// The columns of a vector, the rows of a matrix.
    dim: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Structure {
    name: String,
    offset: u32,
    array: u32,
    size: u32,
    vectors: Vec<Constant>,
    matrices: Vec<Constant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Buffer {
    name: String,
    size: u32,
    vectors: Vec<Constant>,
    matrices: Vec<Constant>,
    structures: Vec<Structure>,
    /// Set on a buffer of the common parameters: the programs hold the rest.
    partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Texture {
    name: String,
    binding: Binding,
    sampler: Binding,
    multisampled: bool,
    dim: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Access {
    name: String,
    binding: Binding,
    original: Binding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BufferBinding {
    name: String,
    binding: Binding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Data {
    name: String,
    binding: Binding,
    array: u32,
    resource: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sampler {
    sampler: u32,
    binding: Binding,
}

/// Everything a program reads, as the compiler's reflection lists it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Parameters {
    vectors: Vec<Constant>,
    matrices: Vec<Constant>,
    textures: Vec<Texture>,
    data: Vec<Data>,
    buffers: Vec<Buffer>,
    bindings: Vec<BufferBinding>,
    accesses: Vec<Access>,
    samplers: Vec<Sampler>,
}

/// A compiler reply, read.
pub(super) struct Reflection {
    pub(super) parameters: Parameters,
    /// Vertex inputs: the mesh channel and the program's input.
    channels: Vec<(i32, i32)>,
    sources: u32,
    statistics: [u32; 4],
}

/// One number of a reflection line. The engine reads a missing one as zero.
fn number(fields: &[&str], index: usize) -> Result<u32> {
    let Some(text) = fields.get(index) else {
        return Ok(0);
    };
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported(format!(
            "the compiler reply field {text:?}, which is not a number"
        )));
    }
    text.parse::<i64>()
        .map(|value| value as u32)
        .map_err(|_| unsupported(format!("the compiler reply field {text:?}")))
}

fn name(fields: &[&str]) -> Result<String> {
    match fields.first() {
        Some(name) if !name.is_empty() => Ok(name.to_string()),
        _ => Err(unsupported("a compiler reply line without a name")),
    }
}

/// Six numbers describe a binding; all but the slot are packed into one.
fn binding(fields: &[&str], at: usize) -> Result<Binding> {
    let field = |index| number(fields, at + index);
    Ok(Binding {
        encoded: (field(0)? & 15)
            | (field(3)? & 15) << 4
            | (field(2)? & 255) << 8
            | (field(4)? & 15) << 16,
        slot: field(1)?,
    })
}

/// What the compiler said about a program that did not compile: the first
/// error with its file and line. An error is a line of three numbers, the
/// last of them the line, followed by the file and the message.
pub(super) fn error_text(reply: &[Vec<u8>]) -> String {
    let lines: Vec<String> = reply
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    let Some(index) = lines.iter().position(|line| line.starts_with("err: ")) else {
        return "no message".into();
    };
    let message = lines.get(index + 2).map_or("no message", String::as_str);
    let line = lines[index].rsplit(' ').next().unwrap_or("");
    match lines.get(index + 1) {
        Some(file) if !file.is_empty() => format!("{file}({line}): {message}"),
        _ => message.to_string(),
    }
}

impl Parameters {
    /// The lists a constant belongs to. A dotted name whose head names a
    /// structure of the last constant buffer is a member of that structure.
    fn owner<'a>(
        &'a mut self,
        name: &'a str,
    ) -> (&'a str, &'a mut Vec<Constant>, &'a mut Vec<Constant>) {
        let Some(buffer) = self.buffers.last_mut() else {
            return (name, &mut self.vectors, &mut self.matrices);
        };
        if let Some((head, rest)) = name.split_once('.')
            && let Some(index) = buffer.structures.iter().position(|s| s.name == head)
        {
            let structure = &mut buffer.structures[index];
            return (rest, &mut structure.vectors, &mut structure.matrices);
        }
        (name, &mut buffer.vectors, &mut buffer.matrices)
    }
}

/// Reads the reflection lines of a compiled program.
pub(super) fn reflect(reply: &SnippetReply) -> Result<Reflection> {
    let mut parameters = Parameters::default();
    let mut channels: Vec<(i32, i32)> = Vec::new();
    let mut sources = 0u32;
    let mut statistics = [0u32; 4];
    for line in &reply.lines {
        let line = std::str::from_utf8(line)
            .map_err(|_| unsupported("a compiler reply that is not text"))?;
        let mut tokens = line.split(' ');
        let kind = tokens.next().unwrap_or("");
        let fields: Vec<&str> = tokens.collect();
        match kind {
            "input:" => {
                let (source, target) = (number(&fields, 0)? as i32, number(&fields, 1)? as i32);
                if target != -1 {
                    match channels.iter_mut().find(|(_, known)| *known == target) {
                        Some(channel) => channel.0 = source,
                        None => channels.push((source, target)),
                    }
                }
                sources |= 1 << (source & 31);
            }
            "cb:" => parameters.buffers.push(Buffer {
                name: name(&fields)?,
                size: number(&fields, 1)?,
                ..Default::default()
            }),
            "const:" => {
                let full = name(&fields)?;
                let (offset, kind, class) = (
                    number(&fields, 1)?,
                    number(&fields, 2)?,
                    number(&fields, 3)?,
                );
                let (rows, columns, array) = (
                    number(&fields, 4)?,
                    number(&fields, 5)?,
                    number(&fields, 6)?,
                );
                match class {
                    2 => {
                        let Some(buffer) = parameters.buffers.last_mut() else {
                            return Err(unsupported("a structure outside a constant buffer"));
                        };
                        buffer.structures.push(Structure {
                            name: full,
                            offset,
                            array,
                            size: rows,
                            vectors: Vec::new(),
                            matrices: Vec::new(),
                        });
                    }
                    // The engine keeps only matrices of four columns.
                    1 if columns != 4 => {}
                    1 => {
                        let (name, _, matrices) = parameters.owner(&full);
                        let name = name.to_string();
                        matrices.push(Constant {
                            name,
                            offset,
                            array,
                            kind,
                            dim: rows,
                        });
                    }
                    _ => {
                        let (name, vectors, _) = parameters.owner(&full);
                        let name = name.to_string();
                        vectors.push(Constant {
                            name,
                            offset,
                            array,
                            kind,
                            dim: columns,
                        });
                    }
                }
            }
            "cbbind:" => parameters.bindings.push(BufferBinding {
                name: name(&fields)?,
                binding: binding(&fields, 1)?,
            }),
            "texbind:" => parameters.textures.push(Texture {
                name: name(&fields)?,
                binding: binding(&fields, 1)?,
                sampler: binding(&fields, 7)?,
                multisampled: number(&fields, 13)? != 0,
                dim: number(&fields, 14)?,
            }),
            "sampler:" => parameters.samplers.push(Sampler {
                sampler: number(&fields, 0)?,
                binding: binding(&fields, 1)?,
            }),
            "uavbind:" => {
                let binding = binding(&fields, 1)?;
                parameters.accesses.push(Access {
                    name: name(&fields)?,
                    binding,
                    original: Binding {
                        encoded: binding.encoded,
                        slot: number(&fields, 7)?,
                    },
                });
            }
            "bufferbind:" => {
                let array = number(&fields, 7)? as i32;
                parameters.data.push(Data {
                    name: name(&fields)?,
                    binding: binding(&fields, 1)?,
                    array: if array > 0 { array as u32 } else { 1 },
                    resource: number(&fields, 8)?,
                });
            }
            "stats:" => {
                for (index, value) in statistics.iter_mut().enumerate() {
                    *value = number(&fields, index)?;
                }
            }
            // A warning beside a program that compiled was not seen in a
            // build, so what it leaves in the object is not known.
            "err:" => return Err(unsupported("a shader program the compiler warns about")),
            other => {
                return Err(unsupported(format!(
                    "the compiler reply line kind {other:?}"
                )));
            }
        }
    }
    Ok(Reflection {
        parameters,
        channels,
        sources,
        statistics,
    })
}

fn put(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_text(out: &mut Vec<u8>, text: &[u8]) {
    put(out, text.len() as u32);
    out.extend_from_slice(text);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn put_constants(out: &mut Vec<u8>, constants: &[Constant]) {
    put(out, constants.len() as u32);
    for constant in constants {
        put_text(out, constant.name.as_bytes());
        // The dimension is a signed byte widened over the type.
        put(
            out,
            (constant.kind << 16) | (constant.dim as u8 as i8 as i32 as u32),
        );
        put(out, constant.array);
        put(out, constant.offset);
    }
}

fn put_binding(out: &mut Vec<u8>, binding: Binding) {
    put(out, binding.encoded);
    put(out, binding.slot);
}

impl Parameters {
    /// The parameter entry of a program.
    pub(super) fn entry(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, LAYOUT);
        put(&mut out, self.buffers.len() as u32 + 1);
        // The constants outside any buffer lead as a buffer without a name.
        put_text(&mut out, b"");
        put(&mut out, 0);
        put_constants(&mut out, &self.vectors);
        put_constants(&mut out, &self.matrices);
        put(&mut out, 0);
        for buffer in &self.buffers {
            put_text(&mut out, buffer.name.as_bytes());
            put(&mut out, buffer.size);
            put_constants(&mut out, &buffer.vectors);
            put_constants(&mut out, &buffer.matrices);
            put(&mut out, buffer.structures.len() as u32);
            for structure in &buffer.structures {
                put_text(&mut out, structure.name.as_bytes());
                put(&mut out, structure.offset);
                put(&mut out, structure.array);
                put(&mut out, structure.size);
                put_constants(&mut out, &structure.vectors);
                put_constants(&mut out, &structure.matrices);
            }
        }
        put(&mut out, self.textures.len() as u32);
        for texture in &self.textures {
            put_text(&mut out, texture.name.as_bytes());
            put_binding(&mut out, texture.binding);
            put_binding(&mut out, texture.sampler);
            put(
                &mut out,
                (texture.dim & 255) << 1 | texture.multisampled as u32,
            );
        }
        put(&mut out, self.accesses.len() as u32);
        for access in &self.accesses {
            put_text(&mut out, access.name.as_bytes());
            put_binding(&mut out, access.binding);
            put_binding(&mut out, access.original);
        }
        put(&mut out, self.bindings.len() as u32);
        for binding in &self.bindings {
            put_text(&mut out, binding.name.as_bytes());
            put_binding(&mut out, binding.binding);
        }
        put(&mut out, self.data.len() as u32);
        for data in &self.data {
            put_text(&mut out, data.name.as_bytes());
            put_binding(&mut out, data.binding);
            put(&mut out, data.array);
            put(&mut out, data.resource);
        }
        put(&mut out, self.samplers.len() as u32);
        for sampler in &self.samplers {
            put_binding(&mut out, sampler.binding);
            put(&mut out, sampler.sampler);
        }
        out
    }
}

/// The program entry: its kind, keywords, code and vertex inputs. The
/// keywords are listed in byte order of their names.
pub(super) fn program(
    reply: &SnippetReply,
    reflection: &Reflection,
    keywords: &[String],
) -> Vec<u8> {
    let mut sorted: Vec<&[u8]> = keywords.iter().map(|keyword| keyword.as_bytes()).collect();
    sorted.sort_unstable();
    let mut out = Vec::new();
    put(&mut out, LAYOUT);
    put(&mut out, reply.program_type as u32);
    for value in reflection.statistics {
        put(&mut out, value);
    }
    put(&mut out, sorted.len() as u32);
    for keyword in sorted {
        put_text(&mut out, keyword);
    }
    put_text(&mut out, &reply.program);
    put(&mut out, reflection.sources);
    put(&mut out, reflection.channels.len() as u32);
    for &(source, target) in &reflection.channels {
        put(&mut out, source as u32);
        put(&mut out, target as u32);
    }
    out
}

/// Drops from `common` what `other` lacks. A dropped place takes the last
/// element, as in the engine, so the order of `common` can change.
fn intersect<T: PartialEq + Clone>(other: &[T], common: &mut Vec<T>) {
    let mut index = 0;
    while index < common.len() {
        if other.contains(&common[index]) {
            index += 1;
        } else {
            common.swap_remove(index);
        }
    }
}

/// Takes the common elements out of a list: all of it when the list holds
/// nothing else, otherwise one by one, each place taking the last element.
fn subtract<T: PartialEq>(common: &[T], list: &mut Vec<T>) -> Result<()> {
    if common.len() == list.len() {
        list.clear();
        return Ok(());
    }
    for element in common {
        let Some(index) = list.iter().position(|known| known == element) else {
            bail!("shader program lacks a parameter that all programs of its pass share");
        };
        list.swap_remove(index);
    }
    Ok(())
}

fn unique<T: PartialEq + Clone>(list: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for element in list {
        if !out.contains(element) {
            out.push(element.clone());
        }
    }
    out
}

/// Moves what every program of a pass reads into the pass's common
/// parameters and leaves each program the rest. `programs` are the programs
/// of every graphics API in the order the pass lists them.
pub(super) fn share(programs: &mut [Parameters]) -> Result<Parameters> {
    let mut common = Parameters::default();
    if programs.is_empty() {
        return Ok(common);
    }
    macro_rules! list {
        ($field:ident) => {
            let mut shared = unique(&programs[0].$field);
            for program in &programs[1..] {
                if shared.is_empty() {
                    break;
                }
                intersect(&program.$field, &mut shared);
            }
            if !shared.is_empty() {
                for program in programs.iter_mut() {
                    subtract(&shared, &mut program.$field)?;
                }
            }
            common.$field = shared;
        };
    }
    list!(vectors);
    list!(matrices);
    list!(textures);
    list!(accesses);
    list!(data);
    list!(samplers);
    list!(bindings);
    for first in 0..programs[0].buffers.len() {
        let mut shared = programs[0].buffers[first].clone();
        shared.partial = true;
        let mut places = vec![first];
        let mut keep = true;
        for program in &programs[1..] {
            let Some(place) = program
                .buffers
                .iter()
                .position(|buffer| buffer.name == shared.name)
            else {
                keep = false;
                break;
            };
            places.push(place);
            let other = &program.buffers[place];
            intersect(&other.vectors, &mut shared.vectors);
            intersect(&other.matrices, &mut shared.matrices);
            if !shared.structures.is_empty() {
                intersect(&other.structures, &mut shared.structures);
            }
            if shared.vectors.is_empty()
                && shared.matrices.is_empty()
                && shared.structures.is_empty()
            {
                keep = false;
                break;
            }
        }
        if !keep {
            continue;
        }
        for (program, place) in programs.iter_mut().zip(places) {
            let buffer = &mut program.buffers[place];
            if !shared.vectors.is_empty() {
                subtract(&shared.vectors, &mut buffer.vectors)?;
            }
            if !shared.matrices.is_empty() {
                subtract(&shared.matrices, &mut buffer.matrices)?;
            }
            if !shared.structures.is_empty() {
                subtract(&shared.structures, &mut buffer.structures)?;
            }
        }
        common.buffers.push(shared);
    }
    Ok(common)
}

/// The names of a pass's common parameters, each with the position it is
/// referred to by: the order in which the engine first meets them.
struct Names(Vec<String>);

impl Names {
    fn index(&mut self, name: &str) -> Val {
        let index = match self.0.iter().position(|known| known == name) {
            Some(index) => index,
            None => {
                self.0.push(name.to_string());
                self.0.len() - 1
            }
        };
        Val::I(index as i64)
    }
}

fn map<const N: usize>(fields: [(&'static str, Val); N]) -> Val {
    Val::Map(
        fields
            .into_iter()
            .map(|(name, value)| (Cow::Borrowed(name), value))
            .collect(),
    )
}

fn signed_byte(value: u32) -> Val {
    Val::I(value as u8 as i8 as i64)
}

fn binding_value(binding: Binding) -> Val {
    map([
        ("m_EncodedData", Val::U(binding.encoded as u64)),
        ("m_Slot", Val::U(binding.slot as u64)),
    ])
}

fn constant_value(constant: &Constant, index: Val, dimension: &'static str) -> Val {
    map([
        ("m_NameIndex", index),
        (
            "m_OffsetInConstantBuffer",
            Val::I(constant.offset as i32 as i64),
        ),
        ("m_ArraySize", Val::I(constant.array as i32 as i64)),
        ("m_Type", signed_byte(constant.kind)),
        (dimension, signed_byte(constant.dim)),
    ])
}

impl Parameters {
    /// The common parameters of a pass as the object holds them, and the
    /// pass's table of their names, sorted by name.
    pub(super) fn value(&self) -> (Val, Val) {
        let mut names = Names(Vec::new());
        // The engine numbers all names before it writes any, in this order.
        let vectors: Vec<Val> = self.vectors.iter().map(|c| names.index(&c.name)).collect();
        let matrices: Vec<Val> = self.matrices.iter().map(|c| names.index(&c.name)).collect();
        let textures: Vec<Val> = self.textures.iter().map(|t| names.index(&t.name)).collect();
        let accesses: Vec<Val> = self.accesses.iter().map(|a| names.index(&a.name)).collect();
        let buffers: Vec<Val> = self.buffers.iter().map(|b| names.index(&b.name)).collect();
        let mut buffer_values = Vec::new();
        for (buffer, index) in self.buffers.iter().zip(buffers) {
            let vectors: Vec<Val> = buffer
                .vectors
                .iter()
                .map(|c| constant_value(c, names.index(&c.name), "m_Dim"))
                .collect();
            let matrices: Vec<Val> = buffer
                .matrices
                .iter()
                .map(|c| constant_value(c, names.index(&c.name), "m_RowCount"))
                .collect();
            let structures: Vec<Val> = buffer
                .structures
                .iter()
                .map(|s| names.index(&s.name))
                .collect();
            let mut structure_values = Vec::new();
            for (structure, index) in buffer.structures.iter().zip(structures) {
                let vectors: Vec<Val> = structure
                    .vectors
                    .iter()
                    .map(|c| constant_value(c, names.index(&c.name), "m_Dim"))
                    .collect();
                let matrices: Vec<Val> = structure
                    .matrices
                    .iter()
                    .map(|c| constant_value(c, names.index(&c.name), "m_RowCount"))
                    .collect();
                structure_values.push(map([
                    ("m_NameIndex", index),
                    (
                        "m_OffsetInConstantBuffer",
                        Val::I(structure.offset as i32 as i64),
                    ),
                    ("m_ArraySize", Val::I(structure.array as i32 as i64)),
                    ("m_StructSize", Val::I(structure.size as i32 as i64)),
                    ("m_VectorMembers", Val::Seq(vectors)),
                    ("m_MatrixMembers", Val::Seq(matrices)),
                ]));
            }
            buffer_values.push(map([
                ("m_NameIndex", index),
                ("m_MatrixParams", Val::Seq(matrices)),
                ("m_VectorParams", Val::Seq(vectors)),
                ("m_StructParams", Val::Seq(structure_values)),
                ("m_Size", Val::I(buffer.size as i32 as i64)),
                ("m_IsPartialCB", Val::Bool(buffer.partial)),
            ]));
        }
        let bindings: Vec<Val> = self
            .bindings
            .iter()
            .map(|binding| {
                map([
                    ("m_NameIndex", names.index(&binding.name)),
                    ("m_Binding", binding_value(binding.binding)),
                    ("m_ArraySize", Val::I(1)),
                    ("m_ResourceType", Val::I(1)),
                ])
            })
            .collect();
        let data: Vec<Val> = self
            .data
            .iter()
            .map(|data| {
                map([
                    ("m_NameIndex", names.index(&data.name)),
                    ("m_Binding", binding_value(data.binding)),
                    ("m_ArraySize", Val::I(data.array as i32 as i64)),
                    ("m_ResourceType", Val::I(data.resource as i32 as i64)),
                ])
            })
            .collect();
        let value = map([
            (
                "m_VectorParams",
                Val::Seq(
                    self.vectors
                        .iter()
                        .zip(vectors)
                        .map(|(c, index)| constant_value(c, index, "m_Dim"))
                        .collect(),
                ),
            ),
            (
                "m_MatrixParams",
                Val::Seq(
                    self.matrices
                        .iter()
                        .zip(matrices)
                        .map(|(c, index)| constant_value(c, index, "m_RowCount"))
                        .collect(),
                ),
            ),
            (
                "m_TextureParams",
                Val::Seq(
                    self.textures
                        .iter()
                        .zip(textures)
                        .map(|(texture, index)| {
                            map([
                                ("m_NameIndex", index),
                                ("m_Binding", binding_value(texture.binding)),
                                ("m_SamplerBinding", binding_value(texture.sampler)),
                                ("m_MultiSampled", Val::Bool(texture.multisampled)),
                                ("m_Dim", signed_byte(texture.dim)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("m_BufferParams", Val::Seq(data)),
            ("m_ConstantBuffers", Val::Seq(buffer_values)),
            ("m_ConstantBufferBindings", Val::Seq(bindings)),
            (
                "m_UAVParams",
                Val::Seq(
                    self.accesses
                        .iter()
                        .zip(accesses)
                        .map(|(access, index)| {
                            map([
                                ("m_NameIndex", index),
                                ("m_Binding", binding_value(access.binding)),
                                ("m_OriginalBinding", binding_value(access.original)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "m_Samplers",
                Val::Seq(
                    self.samplers
                        .iter()
                        .map(|sampler| {
                            map([
                                ("sampler", Val::U(sampler.sampler as u64)),
                                ("m_Binding", binding_value(sampler.binding)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]);
        let mut table: Vec<(usize, &String)> = names.0.iter().enumerate().collect();
        table.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
        let table = Val::Seq(
            table
                .into_iter()
                .map(|(index, name)| {
                    map([
                        ("first", Val::Str(name.clone())),
                        ("second", Val::I(index as i64)),
                    ])
                })
                .collect(),
        );
        (value, table)
    }
}

/// The entries of one graphics API. Equal programs are stored once, and so
/// are equal parameter entries; both kinds share one numbering.
#[derive(Default)]
pub(super) struct Table {
    entries: Vec<Vec<u8>>,
    programs: HashMap<Vec<u8>, u32>,
    parameters: HashMap<Vec<u8>, u32>,
}

impl Table {
    fn add(&mut self, entry: Vec<u8>, program: bool) -> u32 {
        let known = if program {
            &mut self.programs
        } else {
            &mut self.parameters
        };
        if let Some(&index) = known.get(&entry) {
            return index;
        }
        let index = self.entries.len() as u32;
        known.insert(entry.clone(), index);
        self.entries.push(entry);
        index
    }
    pub(super) fn add_parameters(&mut self, entry: Vec<u8>) -> u32 {
        self.add(entry, false)
    }
    pub(super) fn add_program(&mut self, entry: Vec<u8>) -> u32 {
        self.add(entry, true)
    }

    /// The table as stored before compression, cut into segments of at most
    /// `megabytes`: the first starts with the count and the offset, length
    /// and segment of every entry.
    pub(super) fn segments(&self, megabytes: u32) -> Result<Vec<Vec<u8>>> {
        let limit = (megabytes << 10) << 10;
        let header = 4 + 12 * self.entries.len();
        let total: usize = self.entries.iter().map(Vec::len).sum();
        if limit == 0 || total + header > u32::MAX as usize {
            bail!("shader program table exceeds what its layout addresses");
        }
        let limit = limit as usize;
        let mut sizes = Vec::new();
        let (mut segment, mut position) = (0u32, header);
        if total + header > limit {
            // The directory gets a segment of its own.
            sizes.push(header);
            segment = 1;
            position = 0;
        }
        let mut places = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            if position != 0 && position + entry.len() >= limit {
                sizes.push(position);
                segment += 1;
                position = 0;
            }
            places.push((position, segment));
            position += entry.len();
        }
        sizes.push(position);
        let mut segments: Vec<Vec<u8>> = sizes.into_iter().map(|size| vec![0u8; size]).collect();
        segments[0][..4].copy_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (index, (entry, &(position, segment))) in self.entries.iter().zip(&places).enumerate() {
            let row = 4 + 12 * index;
            segments[0][row..row + 4].copy_from_slice(&(position as u32).to_le_bytes());
            segments[0][row + 4..row + 8].copy_from_slice(&(entry.len() as u32).to_le_bytes());
            segments[0][row + 8..row + 12].copy_from_slice(&segment.to_le_bytes());
            segments[segment as usize][position..position + entry.len()].copy_from_slice(entry);
        }
        Ok(segments)
    }
}
