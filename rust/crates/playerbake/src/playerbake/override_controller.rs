//! `AnimatorOverrideController` (class 221): a controller and the clips that
//! replace its own. The Player form holds the same references as the source.
use super::{
    override_tree,
    serialized::External,
    source::{get, list, object, text},
    typetree::Val,
    unsupported,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// A reference to an object of another source, or `Zero` for none.
fn pointer(value: &Value, externals: &mut Vec<External>, what: &str) -> Result<Val> {
    let map = match value {
        Value::Null => return Ok(Val::Zero),
        Value::Object(map) => map,
        _ => bail!("{what} is not an object reference"),
    };
    let file_id = text(get(map, "fileID"))?;
    if file_id.is_empty() || file_id == "0" {
        return Ok(Val::Zero);
    }
    let guid = text(get(map, "guid"))?.to_ascii_lowercase();
    if guid.is_empty() {
        return Err(unsupported(format!(
            "{what} stored inside the override controller"
        )));
    }
    let Ok(path_id) = file_id.parse::<i64>() else {
        bail!("{what} has an invalid file ID");
    };
    let position = match externals.iter().position(|external| external.guid == guid) {
        Some(position) => position,
        None => {
            externals.push(External {
                guid,
                path: String::new(),
            });
            externals.len() - 1
        }
    };
    Ok(Val::map([
        ("m_FileID", Val::I(position as i64 + 1)),
        ("m_PathID", Val::I(path_id)),
    ]))
}

/// The serialized object and the sources it refers to: its controller first,
/// then clips in order of first use.
pub fn bake_override_object(fields: &Map<String, Value>) -> Result<(Vec<u8>, Vec<External>)> {
    let (value, externals) = override_value(fields)?;
    Ok((override_tree()?.encode(&value)?, externals))
}

/// The fields of the object and the sources it refers to, as
/// [`bake_override_object`] orders them.
pub fn override_value(fields: &Map<String, Value>) -> Result<(Val, Vec<External>)> {
    let mut externals = Vec::new();
    let controller = pointer(get(fields, "m_Controller"), &mut externals, "controller")?;
    let mut clips = Vec::new();
    for item in list(get(fields, "m_Clips"), "m_Clips")? {
        let pair = object(item, "m_Clips")?;
        clips.push(Val::map([
            (
                "m_OriginalClip",
                pointer(get(pair, "m_OriginalClip"), &mut externals, "original clip")?,
            ),
            (
                "m_OverrideClip",
                pointer(get(pair, "m_OverrideClip"), &mut externals, "override clip")?,
            ),
        ]));
    }
    let value = Val::map([
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        ("m_Controller", controller),
        ("m_Clips", Val::Seq(clips)),
    ]);
    Ok((value, externals))
}
