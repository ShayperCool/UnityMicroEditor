//! A material variant as a build writes it, in the form of an authored
//! material without a parent.
//!
//! A session loads a material from its authored fields, since the shader and
//! the textures it refers to live where a published file cannot point. A
//! variant of a shader with an inspector has no authored fields that say
//! what a build makes of it, so the baked object is read back into them: the
//! same values, named and nested as a `.mat` source names them, as JSON.
use super::{
    native::{InspectedVariant, NativeSources, bake_inspected_variant_objects, native_tree},
    typetree::Val,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

fn field<'a>(value: &'a Val, name: &str) -> Result<&'a Val> {
    match value.field(name) {
        Some(field) => Ok(field),
        None => bail!("baked material has no {name}"),
    }
}

fn items<'a>(value: &'a Val, name: &str) -> Result<&'a [Val]> {
    match field(value, name)? {
        Val::Seq(items) => Ok(items),
        Val::Zero => Ok(&[]),
        _ => bail!("baked material's {name} is not a sequence"),
    }
}

fn string(value: &Val) -> Result<String> {
    match value {
        Val::Str(text) => Ok(text.clone()),
        Val::Bytes(bytes) => Ok(String::from_utf8_lossy(bytes).into_owned()),
        Val::Zero => Ok(String::new()),
        _ => bail!("baked material holds a value that is not a string"),
    }
}

fn integer(value: &Val) -> Result<i64> {
    match value {
        Val::Zero => Ok(0),
        Val::Bool(flag) => Ok(*flag as i64),
        Val::I(number) => Ok(*number),
        Val::U(number) => Ok(i64::try_from(*number)?),
        _ => bail!("baked material holds a value that is not an integer"),
    }
}

/// A number as a source spells one the loader reads back to the same bits:
/// every `f32` is a `f64`, whose shortest text names it alone. What is not
/// finite is spelled the way the engine writes it.
fn float(value: &Val) -> Result<Value> {
    let number = match value {
        Val::Zero => 0.0,
        Val::F(number) => *number,
        _ => bail!("baked material holds a value that is not a number"),
    };
    Ok(if number.is_nan() {
        json!("NaN")
    } else if number.is_infinite() {
        json!(if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        })
    } else {
        json!(f64::from(number))
    })
}

fn floats<const N: usize>(value: &Val, names: [&str; N]) -> Result<Value> {
    let mut map = Map::new();
    for name in names {
        map.insert(
            name.to_string(),
            float(value.field(name).unwrap_or(&Val::Zero))?,
        );
    }
    Ok(Value::Object(map))
}

/// The name and the value of an entry of a serialized map.
fn pair(pair: &Val) -> Result<(String, &Val)> {
    Ok((string(field(pair, "first")?)?, field(pair, "second")?))
}

/// The fields of a material variant whose shader names an inspector, as a
/// build writes them (`bake_inspected_variant_objects`), as the JSON of an
/// authored material without a parent. `kind` gives the type a source
/// states for a reference to an object of another asset, by GUID and file
/// ID.
///
/// The keywords are given whole, as one list: which of them the shader
/// declares is for the engine to say when it has the shader the Player
/// loads, and a build's own split depends on the keywords that shader takes
/// over from the shaders it names.
pub fn flatten_inspected_variant(
    inspected: &InspectedVariant<'_>,
    sources: &dyn NativeSources,
    kind: &dyn Fn(&str, i64) -> Option<i64>,
) -> Result<String> {
    let (objects, externals) = bake_inspected_variant_objects(inspected, sources)?;
    let [object] = &objects[..] else {
        bail!(
            "inspected variant bakes to {} objects, not one",
            objects.len()
        );
    };
    let baked = native_tree(i64::from(object.class_id))?.decode(&object.data)?;
    let reference = |value: &Val| -> Result<Value> {
        let (file, id) = match value {
            Val::Zero => (0, 0),
            pointer => (
                integer(pointer.field("m_FileID").unwrap_or(&Val::Zero))?,
                integer(pointer.field("m_PathID").unwrap_or(&Val::Zero))?,
            ),
        };
        if id == 0 {
            return Ok(json!({ "fileID": 0 }));
        }
        let Some(external) = usize::try_from(file - 1)
            .ok()
            .and_then(|index| externals.get(index))
        else {
            bail!("baked material refers to an object of its own file");
        };
        let Some(kind) = kind(&external.guid, id) else {
            bail!(
                "no source of the chain states the reference to {} {id}",
                external.guid
            );
        };
        Ok(json!({ "fileID": id, "guid": external.guid, "type": kind }))
    };
    let named = |name: String, value: Value| Value::Object(Map::from_iter([(name, value)]));

    let mut keywords = Vec::new();
    for list in ["m_ValidKeywords", "m_InvalidKeywords"] {
        for keyword in items(&baked, list)? {
            keywords.push(string(keyword)?);
        }
    }
    keywords.sort();
    let mut tags = Map::new();
    for tag in items(&baked, "stringTagMap")? {
        let (name, value) = pair(tag)?;
        tags.insert(name, Value::String(string(value)?));
    }
    let mut passes = Vec::new();
    for pass in items(&baked, "disabledShaderPasses")? {
        passes.push(Value::String(string(pass)?));
    }
    let saved = field(&baked, "m_SavedProperties")?;
    let (mut textures, mut ints, mut numbers, mut colors) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for texture in items(saved, "m_TexEnvs")? {
        let (name, value) = pair(texture)?;
        textures.push(named(
            name,
            json!({
                "m_Texture": reference(field(value, "m_Texture")?)?,
                "m_Scale": floats(field(value, "m_Scale")?, ["x", "y"])?,
                "m_Offset": floats(field(value, "m_Offset")?, ["x", "y"])?,
            }),
        ));
    }
    for entry in items(saved, "m_Ints")? {
        let (name, value) = pair(entry)?;
        ints.push(named(name, json!(integer(value)?)));
    }
    for entry in items(saved, "m_Floats")? {
        let (name, value) = pair(entry)?;
        numbers.push(named(name, float(value)?));
    }
    for entry in items(saved, "m_Colors")? {
        let (name, value) = pair(entry)?;
        colors.push(named(name, floats(value, ["r", "g", "b", "a"])?));
    }
    let material = json!({
        "serializedVersion": 8,
        "m_Name": string(field(&baked, "m_Name")?)?,
        "m_Shader": reference(field(&baked, "m_Shader")?)?,
        "m_Parent": { "fileID": 0 },
        "m_ModifiedSerializedProperties": 0,
        "m_ValidKeywords": keywords,
        "m_InvalidKeywords": [],
        "m_LightmapFlags": integer(field(&baked, "m_LightmapFlags")?)?,
        "m_EnableInstancingVariants": integer(field(&baked, "m_EnableInstancingVariants")?)?,
        "m_DoubleSidedGI": integer(field(&baked, "m_DoubleSidedGI")?)?,
        "m_CustomRenderQueue": integer(field(&baked, "m_CustomRenderQueue")?)?,
        "stringTagMap": tags,
        "disabledShaderPasses": passes,
        "m_SavedProperties": {
            "serializedVersion": 3,
            "m_TexEnvs": textures,
            "m_Ints": ints,
            "m_Floats": numbers,
            "m_Colors": colors,
        },
        "m_BuildTextureStacks": [],
    });
    Ok(serde_json::to_string(&material)?)
}
