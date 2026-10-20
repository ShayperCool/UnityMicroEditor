//! The names of the shaders of a file that carries type trees.
use super::typetree::Val;
use anyhow::{Context as _, Result, bail};

fn field<'a>(value: &'a Val, name: &str) -> Result<&'a Val> {
    match value.field(name) {
        Some(field) => Ok(field),
        None => bail!("shader object has no {name}"),
    }
}

fn string(value: &Val, name: &str) -> Result<String> {
    match field(value, name)? {
        Val::Str(text) => Ok(text.clone()),
        Val::Zero => Ok(String::new()),
        _ => bail!("shader object's {name} is not a string"),
    }
}

/// The names of the shaders of a file that carries type trees, such as the
/// Editor's `unity_builtin_extra`, whose shaders lead with the fields of an
/// Editor object. Only the fields up to the parsed form are read.
pub fn read_shader_names(file: &[u8]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for shader in super::serialized::read_of_class(file, 48)? {
        let tree = &shader.tree;
        let Some(fields) = tree
            .children(0)
            .iter()
            .position(|child| tree.nodes[*child].name == "m_ParsedForm")
        else {
            bail!("shader {} has no parsed form", shader.local_id);
        };
        let value = tree.decode_leading(&shader.data, fields + 1)?;
        names.push(
            string(field(&value, "m_ParsedForm")?, "m_Name")
                .with_context(|| format!("shader {}", shader.local_id))?,
        );
    }
    Ok(names)
}
