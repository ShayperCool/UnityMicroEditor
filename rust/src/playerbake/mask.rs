//! `AvatarMask` (class 319). The Player object holds what the source holds:
//! the body part switches and the transform paths with their weights.
use super::{
    Baked, mask_tree,
    serialized::{self, Object},
    source::{float, get, int, list, object, text},
    typetree::Val,
    unsupported,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// An authored `AvatarMask`.
#[derive(Debug, Clone, PartialEq)]
pub struct Mask {
    /// The humanoid body part switches, in `AvatarMaskBodyPart` order.
    pub body_parts: Vec<bool>,
    /// Transform paths and their weights, in authored order.
    pub transforms: Vec<(String, f32)>,
}

/// The body part values as the source stores them.
fn body_parts(fields: &Map<String, Value>) -> Result<Vec<u32>> {
    let dump = match get(fields, "m_Mask") {
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                let Ok(part) = u32::try_from(int(item, "avatar mask body part")?) else {
                    bail!("avatar mask body part is out of range");
                };
                parts.push(part);
            }
            return Ok(parts);
        }
        value => text(value)?,
    };
    // A hex dump of little-endian 32-bit integers, which the Editor ends in
    // the letter of their type.
    let digits = dump.strip_suffix('u').unwrap_or(&dump);
    let (words, rest) = digits.as_bytes().as_chunks::<8>();
    if !rest.is_empty() {
        bail!("avatar mask body parts are not a hex dump");
    }
    let mut parts = Vec::with_capacity(words.len());
    for word in words {
        let Ok(word) = u32::from_str_radix(&String::from_utf8_lossy(word.as_slice()), 16) else {
            bail!("avatar mask body parts are not a hex dump");
        };
        parts.push(word.swap_bytes());
    }
    Ok(parts)
}

fn transforms(fields: &Map<String, Value>) -> Result<Vec<(String, f32)>> {
    let mut transforms = Vec::new();
    for item in list(get(fields, "m_Elements"), "m_Elements")? {
        let element = object(item, "m_Elements")?;
        transforms.push((
            text(get(element, "m_Path"))?,
            float(get(element, "m_Weight"), "avatar mask")?,
        ));
    }
    Ok(transforms)
}

/// What a controller layer takes from the fields of an `AvatarMask` document.
pub(super) fn mask_of(fields: &Map<String, Value>) -> Result<Mask> {
    Ok(Mask {
        body_parts: body_parts(fields)?
            .into_iter()
            .map(|part| part != 0)
            .collect(),
        transforms: transforms(fields)?,
    })
}

/// Reads an authored `.mask` source.
pub fn read_mask(yaml: &[u8]) -> Result<Mask> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let Some(mask) = documents.iter().find(|document| document.class_id == 319) else {
        bail!("mask source does not hold an AvatarMask");
    };
    mask_of(&mask.data)
}

/// The serialized `AvatarMask` object of an authored document.
pub fn bake_mask_object(fields: &Map<String, Value>) -> Result<Vec<u8>> {
    let parts = body_parts(fields)?;
    let elements = transforms(fields)?
        .into_iter()
        .map(|(path, weight)| Val::map([("m_Path", Val::Str(path)), ("m_Weight", Val::F(weight))]))
        .collect();
    mask_tree()?.encode(&Val::map([
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        (
            "m_Mask",
            Val::Seq(parts.into_iter().map(|part| Val::U(part as u64)).collect()),
        ),
        ("m_Elements", Val::Seq(elements)),
    ]))
}

/// Bakes an authored `.mask` source into a ContentFile.
pub fn bake_mask(yaml: &[u8]) -> Result<Baked> {
    // A transform path is free text.
    super::source::refuse_comment_marks(yaml)?;
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let [document] = documents.as_slice() else {
        return Err(unsupported("avatar mask source with several objects"));
    };
    if document.class_id != 319 || document.stripped {
        bail!("source does not hold an AvatarMask");
    }
    let Ok(local_id) = document.file_id.parse::<i64>() else {
        bail!("avatar mask has an invalid file ID");
    };
    let file = serialized::write(
        &[Object {
            local_id,
            tree: mask_tree()?,
            data: bake_mask_object(&document.data)?,
            script: None,
        }],
        &[],
    )?;
    Ok(Baked {
        file,
        objects: vec![(
            document.file_id.clone(),
            "UnityEngine.AvatarMask".to_string(),
        )],
        externals: Vec::new(),
    })
}
