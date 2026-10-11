//! The tables a Player reads compiled programs from: per graphics API a list
//! of entries, each either the parameters of a program (what it reads from
//! materials and the engine) or the program itself.
use super::SnippetReply;
use crate::playerbake::unsupported;
use anyhow::{Result, bail};
use std::collections::HashMap;

/// The revision of the entry layouts in the pinned engine.
const LAYOUT: u32 = 0x0c13_82ea;

/// The entries of one graphics API. Equal entries are stored once.
#[derive(Default)]
pub(super) struct Table {
    entries: Vec<Vec<u8>>,
    index: HashMap<Vec<u8>, u32>,
}

impl Table {
    pub(super) fn add(&mut self, entry: Vec<u8>) -> u32 {
        if let Some(&index) = self.index.get(&entry) {
            return index;
        }
        let index = self.entries.len() as u32;
        self.index.insert(entry.clone(), index);
        self.entries.push(entry);
        index
    }

    /// The table as stored before compression: the count, then offset,
    /// length and segment of every entry, then the entries.
    pub(super) fn segment(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, self.entries.len() as u32);
        let mut offset = 4 + 12 * self.entries.len();
        for entry in &self.entries {
            put(&mut out, offset as u32);
            put(&mut out, entry.len() as u32);
            put(&mut out, 0);
            offset += entry.len();
        }
        for entry in &self.entries {
            out.extend_from_slice(entry);
        }
        out
    }
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

/// A reflection line: its kind and its fields.
struct Line<'a> {
    kind: &'a str,
    fields: Vec<&'a str>,
}

fn lines(reply: &[Vec<u8>]) -> Result<Vec<Line<'_>>> {
    reply
        .iter()
        .map(|line| {
            let line = std::str::from_utf8(line)
                .map_err(|_| unsupported("a compiler reply that is not text"))?;
            let Some((kind, rest)) = line.split_once(": ") else {
                return Err(unsupported(format!("the compiler reply line {line:?}")));
            };
            Ok(Line {
                kind,
                fields: rest.split(' ').collect(),
            })
        })
        .collect()
}

fn field(line: &Line<'_>, index: usize) -> Result<u32> {
    let text = line.fields.get(index).copied().unwrap_or("");
    // A binding is printed unsigned, an input channel signed.
    text.parse::<u32>()
        .or_else(|_| text.parse::<i32>().map(|value| value as u32))
        .map_err(|_| {
            unsupported(format!(
                "the compiler reply field {text:?} of {}",
                line.kind
            ))
        })
}

/// What the compiler said about a program that did not compile.
pub(super) fn error_text(reply: &[Vec<u8>]) -> String {
    let lines: Vec<String> = reply
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    let message = lines
        .iter()
        .position(|line| line.starts_with("err: "))
        .and_then(|index| lines.get(index + 2));
    message.cloned().unwrap_or_else(|| "no message".into())
}

#[derive(Default)]
struct Buffer<'a> {
    name: &'a str,
    size: u32,
    vectors: Vec<(&'a str, [u32; 3])>,
    matrices: Vec<(&'a str, [u32; 3])>,
}

/// The parameter entry of a program, from the compiler's reflection.
pub(super) fn parameters(reply: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut buffers = vec![Buffer::default()];
    let (mut textures, mut bindings) = (Vec::new(), Vec::new());
    for line in lines(reply)? {
        match line.kind {
            "cb" => buffers.push(Buffer {
                name: line.fields.first().copied().unwrap_or(""),
                size: field(&line, 1)?,
                ..Default::default()
            }),
            "const" => {
                if line.fields.len() != 7 {
                    return Err(unsupported("a constant the compiler describes otherwise"));
                }
                let (offset, kind, form) = (field(&line, 1)?, field(&line, 2)?, field(&line, 3)?);
                let (columns, array) = (field(&line, 5)?, field(&line, 6)?);
                if kind != 0 || form > 1 {
                    return Err(unsupported(format!(
                        "the program constant {} of a kind not laid out yet",
                        line.fields[0]
                    )));
                }
                let entry = (line.fields[0], [columns, array, offset]);
                let buffer = buffers.last_mut().unwrap();
                if form == 1 {
                    buffer.matrices.push(entry);
                } else {
                    buffer.vectors.push(entry);
                }
            }
            "cbbind" => bindings.push((line.fields[0], [field(&line, 1)?, field(&line, 2)?])),
            "texbind" => {
                if line.fields.len() != 15 {
                    return Err(unsupported("a texture the compiler describes otherwise"));
                }
                textures.push((
                    line.fields[0],
                    [
                        field(&line, 1)?,
                        field(&line, 2)?,
                        field(&line, 3)?,
                        field(&line, 8)?,
                        field(&line, 14)? << 1,
                    ],
                ));
            }
            "input" => {}
            other => {
                return Err(unsupported(format!(
                    "the program parameter kind {other}, which is not laid out yet"
                )));
            }
        }
    }
    let mut out = Vec::new();
    put(&mut out, LAYOUT);
    put(&mut out, buffers.len() as u32);
    for buffer in &buffers {
        put_text(&mut out, buffer.name.as_bytes());
        put(&mut out, buffer.size);
        for list in [&buffer.vectors, &buffer.matrices] {
            put(&mut out, list.len() as u32);
            for (name, values) in list {
                put_text(&mut out, name.as_bytes());
                values.iter().for_each(|&value| put(&mut out, value));
            }
        }
        put(&mut out, 0);
    }
    put(&mut out, textures.len() as u32);
    for (name, values) in &textures {
        put_text(&mut out, name.as_bytes());
        values.iter().for_each(|&value| put(&mut out, value));
    }
    put(&mut out, 0);
    put(&mut out, bindings.len() as u32);
    for (name, values) in &bindings {
        put_text(&mut out, name.as_bytes());
        values.iter().for_each(|&value| put(&mut out, value));
    }
    put(&mut out, 0);
    put(&mut out, 0);
    Ok(out)
}

/// The program entry: its kind, keywords, code and vertex inputs.
pub(super) fn program(reply: &SnippetReply, keywords: &[String]) -> Result<Vec<u8>> {
    let mut sources = 0u32;
    let mut bound = Vec::new();
    for line in lines(&reply.lines)? {
        if line.kind != "input" {
            continue;
        }
        let (source, target) = (field(&line, 0)?, field(&line, 1)? as i32);
        if source >= 32 {
            bail!("shader program input channel is out of range");
        }
        sources |= 1 << source;
        if target >= 0 {
            bound.push((source, target as u32));
        }
    }
    let mut out = Vec::new();
    put(&mut out, LAYOUT);
    put(&mut out, reply.program_type as u32);
    for _ in 0..4 {
        put(&mut out, 0);
    }
    put(&mut out, keywords.len() as u32);
    for keyword in keywords {
        put_text(&mut out, keyword.as_bytes());
    }
    put_text(&mut out, &reply.program);
    put(&mut out, sources);
    put(&mut out, bound.len() as u32);
    for (source, target) in bound {
        put(&mut out, source);
        put(&mut out, target);
    }
    Ok(out)
}
