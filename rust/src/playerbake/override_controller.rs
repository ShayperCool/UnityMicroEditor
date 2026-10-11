//! `AnimatorOverrideController` (class 221): a controller and the clips that
//! replace its own. The Player form holds the same references as the source.
use super::{
    Baked, override_tree,
    serialized::{self, External, Object},
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
            externals.push(External { guid });
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
    let data = override_tree()?.encode(&Val::map([
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        ("m_Controller", controller),
        ("m_Clips", Val::Seq(clips)),
    ]))?;
    Ok((data, externals))
}

/// Bakes an authored `.overrideController` source into a ContentFile.
pub fn bake_override(yaml: &[u8]) -> Result<Baked> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let [document] = documents.as_slice() else {
        return Err(unsupported(
            "override controller source with several objects",
        ));
    };
    if document.class_id != 221 || document.stripped {
        bail!("source does not hold an AnimatorOverrideController");
    }
    let Ok(local_id) = document.file_id.parse::<i64>() else {
        bail!("override controller has an invalid file ID");
    };
    let (data, externals) = bake_override_object(&document.data)?;
    let file = serialized::write(
        &[Object {
            local_id,
            tree: override_tree()?,
            data,
            script: None,
        }],
        &externals,
    )?;
    Ok(Baked {
        file,
        objects: vec![(
            document.file_id.clone(),
            "UnityEngine.AnimatorOverrideController".to_string(),
        )],
        externals,
    })
}
