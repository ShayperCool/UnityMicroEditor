//! A check on a `Shader` object that was not assembled here: the Editor's
//! build code writes one inside a worker for a project with a scriptable
//! render pipeline. A compile that fails there leaves the passes and their
//! variant counts in place and the programs out, so the tables are read
//! back before such an object is published.
use super::sequence;
use crate::playerbake::{
    typetree::{TypeTree, Val},
    unsupported,
};
use anyhow::{Result, bail};

fn unsigned(value: &Val) -> Result<usize> {
    match value {
        Val::U(number) => Ok(*number as usize),
        Val::I(number) if *number >= 0 => Ok(*number as usize),
        Val::Zero => Ok(0),
        _ => bail!("shader program table holds a value that is not a count"),
    }
}

fn numbers(value: &Val) -> Result<Vec<usize>> {
    match value {
        Val::Seq(items) => items.iter().map(unsigned).collect(),
        Val::Bytes(bytes) => Ok(bytes.iter().map(|byte| *byte as usize).collect()),
        Val::Zero => Ok(Vec::new()),
        _ => bail!("shader program table holds a list that is not of counts"),
    }
}

fn decompress(packed: &[u8], size: usize) -> Result<Vec<u8>> {
    let (Ok(input), Ok(output)) = (i32::try_from(packed.len()), i32::try_from(size)) else {
        bail!("shader program table is too large");
    };
    let mut out = vec![0u8; size];
    // SAFETY: both buffers outlive the call and are passed with their sizes.
    let written = unsafe {
        lz4_sys::LZ4_decompress_safe(
            packed.as_ptr().cast(),
            out.as_mut_ptr().cast(),
            input,
            output,
        )
    };
    if written != output {
        bail!("shader program table does not decompress to its stated size");
    }
    Ok(out)
}

/// The number of programs the passes of a Player `Shader` object select,
/// read with the type tree it was written with, after checking that every graphics API of the object has a table whose
/// entries are all there. An object with programs and an empty table, or
/// with an empty entry, is refused.
pub fn verify_shader_programs(tree: &TypeTree, object: &[u8]) -> Result<usize> {
    let value = tree.decode(object)?;
    let Some(parsed) = value.field("m_ParsedForm") else {
        bail!("Player Shader lacks its parsed form");
    };
    let mut programs = 0;
    for subshader in sequence(parsed, "m_SubShaders")? {
        for pass in sequence(subshader, "m_Passes")? {
            let Some(vertex) = pass.field("progVertex") else {
                continue;
            };
            for tier in sequence(vertex, "m_PlayerSubPrograms")? {
                if let Val::Seq(list) = tier {
                    programs += list.len();
                }
            }
        }
    }
    let platforms = numbers(value.field("platforms").unwrap_or(&Val::Zero))?;
    let blob: Vec<u8> = match value.field("compressedBlob") {
        Some(Val::Bytes(blob)) => blob.clone(),
        Some(Val::Seq(items)) => items
            .iter()
            .map(|item| unsigned(item).map(|byte| byte as u8))
            .collect::<Result<_>>()?,
        _ => Vec::new(),
    };
    let lists = |name: &str| -> Result<Vec<Vec<usize>>> {
        sequence(&value, name)?.iter().map(numbers).collect()
    };
    let (offsets, packed, sizes) = (
        lists("offsets")?,
        lists("compressedLengths")?,
        lists("decompressedLengths")?,
    );
    if offsets.len() != platforms.len()
        || packed.len() != platforms.len()
        || sizes.len() != platforms.len()
    {
        bail!("Player Shader program tables and graphics APIs differ in number");
    }
    if programs == 0 {
        return Ok(0);
    }
    if platforms.is_empty() {
        return Err(unsupported(
            "a shader whose build holds programs for no graphics API",
        ));
    }
    let mut total = 0;
    for (index, platform) in platforms.iter().enumerate() {
        if offsets[index].len() != packed[index].len() || offsets[index].len() != sizes[index].len()
        {
            bail!("Player Shader program table segments are inconsistent");
        }
        let mut segments = Vec::with_capacity(offsets[index].len());
        for ((offset, length), size) in offsets[index].iter().zip(&packed[index]).zip(&sizes[index])
        {
            let Some(data) = blob.get(*offset..*offset + *length) else {
                bail!("Player Shader program table lies outside its data");
            };
            segments.push(decompress(data, *size)?);
        }
        let entries = segments
            .first()
            .and_then(|first| first.get(..4))
            .map(|count| u32::from_le_bytes(count.try_into().unwrap()) as usize)
            .unwrap_or(0);
        // A graphics API the project does not build for keeps an empty
        // table.
        total += entries;
        if entries == 0 {
            continue;
        }
        let Some(directory) = segments[0].get(4..4 + 12 * entries) else {
            bail!("Player Shader program directory is cut short");
        };
        for row in directory.as_chunks::<12>().0 {
            let field =
                |at: usize| u32::from_le_bytes(row[at..at + 4].try_into().unwrap()) as usize;
            let (position, length, segment) = (field(0), field(4), field(8));
            if length == 0 {
                return Err(unsupported(format!(
                    "a shader whose build holds an empty program for graphics API {platform}, as a failed compile leaves"
                )));
            }
            if segments
                .get(segment)
                .and_then(|data| data.get(position..position + length))
                .is_none()
            {
                bail!("Player Shader program lies outside its table");
            }
        }
    }
    if total == 0 {
        return Err(unsupported(
            "a shader whose build left every program out, as a failed compile does",
        ));
    }
    Ok(programs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playerbake::shader::shader_tree;

    /// A Player `Shader` object with one program for two graphics APIs.
    fn object(change: impl Fn(&mut Val)) -> Vec<u8> {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/tests/testdata/playerbake/shaders/c01_types_vk.shader.bin"
        );
        let tree = shader_tree().unwrap();
        let mut value = tree.decode(&std::fs::read(fixture).unwrap()).unwrap();
        change(&mut value);
        tree.encode(&value).unwrap()
    }

    #[test]
    fn counts_the_programs_of_a_whole_shader() {
        let tree = shader_tree().unwrap();
        assert!(verify_shader_programs(tree, &object(|_| {})).unwrap() > 0);
    }

    #[test]
    fn refuses_a_shader_whose_programs_were_left_out() {
        let tree = shader_tree().unwrap();
        // What a compile that found no include file leaves: the passes with
        // their programs listed, and tables without entries.
        let emptied = object(|value| {
            for name in ["offsets", "compressedLengths", "decompressedLengths"] {
                let Some(Val::Seq(lists)) = value.field_mut(name) else {
                    panic!("fixture lacks {name}");
                };
                for list in lists {
                    *list = Val::Seq(Vec::new());
                }
            }
            *value.field_mut("compressedBlob").unwrap() = Val::Bytes(Vec::new());
        });
        let refused = verify_shader_programs(tree, &emptied).unwrap_err();
        assert!(refused.is::<crate::playerbake::Unsupported>());
    }
}
