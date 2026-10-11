//! The artifact of a `ScriptedImporter`: objects of script classes.
//!
//! The Editor points such an object at the script asset that defines its
//! class, by GUID. A Player has no script assets: it finds the class through a
//! `MonoScript` object that names it. So the ContentFile gets one such object
//! per class, named after what the Editor wrote into the object itself, and
//! every object points at it. The objects keep their local IDs, their data
//! and the type trees the Editor wrote for them; the importer object is
//! dropped.
use super::{
    MAXIMUM_BYTES, Reader, adapt::Metadata, adapt::Object, adapt::RemovedObject, be32, be64, le16,
    le32, le64, ordinary_guid, req,
};
use crate::playerbake::{self, TypeTree, Val};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, HashSet};

/// `ScriptedImporter`, the importer object of the artifact.
const SCRIPTED_IMPORTER: i32 = 2089858483;
const MONO_BEHAVIOUR: i32 = 114;
/// Every script asset holds its `MonoScript` at this local ID.
const SCRIPT_LOCAL_ID: i64 = 11500000;

struct ScriptClass {
    local_id: i64,
    assembly: String,
    namespace: String,
    class_name: String,
}

/// `Assembly::Namespace.Class`, as the Editor identifies the class of an
/// object.
fn class_identifier(text: &str) -> Option<(String, String, String)> {
    let (assembly, full) = text.split_once("::")?;
    let (namespace, class_name) = full.rsplit_once('.').unwrap_or(("", full));
    let part = |text: &str| !text.is_empty() && !text.contains([':', '/', '+', '\0', ' ']);
    (part(assembly) && part(class_name) && !namespace.contains([':', '/', '+', '\0', ' '])).then(
        || {
            (
                assembly.to_string(),
                namespace.to_string(),
                class_name.to_string(),
            )
        },
    )
}

/// A pointer value of a decoded object, if the value is one.
fn pointer(value: &Val) -> Option<(i64, i64)> {
    let Val::Map(fields) = value else { return None };
    if fields.len() != 2 {
        return None;
    }
    let number = |name: &str| match value.field(name)? {
        Val::I(number) => Some(*number),
        Val::U(number) => Some(*number as i64),
        Val::Zero => Some(0),
        _ => None,
    };
    Some((number("m_FileID")?, number("m_PathID")?))
}

/// Whether a decoded value points into another file anywhere.
fn points_outside(value: &Val) -> bool {
    if let Some((file, _)) = pointer(value) {
        return file != 0;
    }
    match value {
        Val::Seq(items) => items.iter().any(points_outside),
        Val::Map(fields) => fields.iter().any(|(_, field)| points_outside(field)),
        _ => false,
    }
}

/// Projects the artifact of a scripted importer whose objects are all of
/// script classes. Native import content hashes must be verified by the
/// caller before using this projection.
pub fn adapt_scripted(input: &[u8]) -> Result<(Vec<u8>, Metadata)> {
    if input.len() < 64
        || input.len() > MAXIMUM_BYTES
        || be32(&input[8..12]) != 23
        || input[16] != 0
    {
        bail!(
            "native runtime projection requires little-endian SerializedFile v23, at most 64 MiB"
        );
    }
    let file_size = be64(&input[24..32]);
    let data_offset = be64(&input[32..40]);
    let metadata_end = 48 + be32(&input[20..24]) as u64;
    if file_size != input.len() as u64 || metadata_end > data_offset || data_offset > file_size {
        bail!("native SerializedFile header bounds are invalid");
    }
    let (metadata_end, data_offset) = (metadata_end as usize, data_offset as usize);
    let payload = (input.len() - data_offset) as u64;
    let mut r = Reader::new(&input[48..metadata_end]);
    req(
        r.cstring_bytes(64),
        |version| *version == b"0.0.0" || *version == b"6000.6.3f1",
        "artifact must be from the pinned Unity 6000.6.3f1 format",
    )?;
    req(
        r.u32(),
        |platform| *platform as i32 == -2,
        "artifact must declare native Editor target -2 before runtime projection",
    )?;
    req(
        r.take(1),
        |inline| inline[0] == 1,
        "native runtime projection requires inline type trees",
    )?;
    let type_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 256,
        "artifact type count is invalid",
    )?;
    // Each type with the position of its class in the script table.
    let mut types: Vec<(i32, i16, Option<TypeTree>)> = Vec::new();
    for _ in 0..type_count {
        let start = r.pos;
        let class_id = r.u32()? as i32;
        let flags = r.take(3)?;
        let script = le16(&flags[1..]) as i16;
        // The importer object is of a script class too: its entry names a
        // script without being a `MonoBehaviour`.
        if flags[0] != 0 || (class_id == MONO_BEHAVIOUR && script < 0) {
            bail!("stripped type or script type without a class in a scripted import");
        }
        r.take(if script >= 0 { 48 } else { 32 })?;
        let size = req(r.u32(), |size| *size >= 16, "native type tree is missing")?;
        r.take(size as usize)?;
        req(
            r.u32(),
            |dependencies| *dependencies == 0,
            "objects held by managed reference are unsupported in a scripted import",
        )?;
        let tree = (class_id == MONO_BEHAVIOUR)
            .then(|| TypeTree::parse(r.data[start..r.pos].to_vec()))
            .transpose()?;
        types.push((class_id, script, tree));
    }
    let object_count = req(
        r.u32(),
        |count| *count != 0 && *count <= 100000,
        "native object table count is invalid",
    )?;
    r.align();
    let mut rows = Vec::with_capacity(object_count as usize);
    let mut ids = HashSet::new();
    for _ in 0..object_count {
        let row = r.take(24)?;
        let (id, offset, size) = (
            le64(&row[..8]) as i64,
            le64(&row[8..16]),
            le32(&row[16..20]) as u64,
        );
        let type_index = le32(&row[20..24]) as usize;
        if type_index >= types.len()
            || id == 0
            || !ids.insert(id)
            || offset > payload
            || size > payload - offset
            || size < 4
        {
            bail!("native object identity or data bounds are invalid");
        }
        let start = data_offset + offset as usize;
        rows.push((id, type_index, &input[start..start + size as usize]));
    }
    let script_count = req(
        r.u32(),
        |count| *count <= 256,
        "native script type table is invalid",
    )?;
    let mut script_types = Vec::with_capacity(script_count as usize);
    for _ in 0..script_count {
        r.align();
        let row = r.take(12)?;
        script_types.push((le32(&row[..4]) as i64, le64(&row[4..]) as i64));
    }
    let external_count = req(
        r.u32(),
        |externals| *externals <= 1024,
        "native external reference table is invalid",
    )?;
    let mut externals = Vec::with_capacity(external_count as usize);
    for _ in 0..external_count {
        r.cstring_bytes(256)?;
        let guid = ordinary_guid(r.take(16)?);
        let kind = r.u32()?;
        r.cstring_bytes(4096)?;
        externals.push((guid, kind));
    }
    req(
        r.u32(),
        |types| *types == 0,
        "native serialized reference types are unsupported",
    )?;
    r.cstring_bytes(4096)?;
    if r.data[r.pos..].iter().any(|&byte| byte != 0) {
        bail!("unrecognized native metadata suffix");
    }

    let mut result = Metadata {
        profile: "scripted".into(),
        serialized_version: 23,
        source_target: -2,
        runtime_target: 24,
        ..Default::default()
    };
    // The script asset of each script type: an external reference to a
    // script, whose one object every script asset holds at the same ID.
    let script_guid = |position: i16| -> Result<&str> {
        let (file, path) = script_types
            .get(position as usize)
            .copied()
            .ok_or_else(|| anyhow!("native script type index is invalid"))?;
        match externals.get((file - 1) as usize) {
            Some((guid, 3)) if file >= 1 && path == SCRIPT_LOCAL_ID => Ok(guid.as_str()),
            _ => bail!("script type of a scripted import is not a script asset"),
        }
    };
    let mut classes: BTreeMap<String, ScriptClass> = BTreeMap::new();
    let mut objects = Vec::new();
    for (id, type_index, data) in &rows {
        let (class_id, script, tree) = &types[*type_index];
        let local_id = id.to_string();
        if *class_id == SCRIPTED_IMPORTER || *class_id == 1001480554 {
            result.removed.push(RemovedObject {
                file_id: local_id,
                class_id: *class_id,
                reason: "editor-only".into(),
            });
            continue;
        }
        let Some(tree) = tree else {
            bail!("native class {class_id} is unsupported in scripted runtime profile");
        };
        if le32(&data[..4]) & 16 != 0 {
            result.removed.push(RemovedObject {
                file_id: local_id,
                class_id: *class_id,
                reason: "dont-save-in-build".into(),
            });
            continue;
        }
        let guid = script_guid(*script)?;
        let mut value = tree.decode(data)?;
        let identifier = match value.field("m_EditorClassIdentifier") {
            Some(Val::Str(text)) => class_identifier(text),
            _ => None,
        };
        let Some((assembly, namespace, class_name)) = identifier else {
            bail!("object {local_id} of a scripted import does not name its class");
        };
        let script_id = playerbake::script_local_id(guid);
        match classes.get(guid) {
            Some(known)
                if (&known.assembly, &known.namespace, &known.class_name)
                    != (&assembly, &namespace, &class_name) =>
            {
                bail!("objects of one script of a scripted import name different classes");
            }
            Some(_) => {}
            None => {
                classes.insert(
                    guid.to_string(),
                    ScriptClass {
                        local_id: script_id,
                        assembly,
                        namespace: namespace.clone(),
                        class_name: class_name.clone(),
                    },
                );
            }
        }
        // The pointer to the script asset becomes one to the object in this
        // file. No other pointer may leave the file: the Player would look
        // for a file this one does not bring along.
        let expected = script_types[*script as usize];
        match value.field_mut("m_Script") {
            Some(script) if pointer(script) == Some(expected) => {
                *script = Val::map([("m_FileID", Val::I(0)), ("m_PathID", Val::I(script_id))]);
            }
            _ => bail!("object {local_id} of a scripted import does not point at its script"),
        }
        if points_outside(&value) {
            bail!("object {local_id} of a scripted import refers to another asset");
        }
        let encoded = tree.encode(&value)?;
        if encoded.len() != data.len() {
            bail!("object {local_id} of a scripted import does not keep its layout");
        }
        let type_name = if namespace.is_empty() {
            class_name
        } else {
            format!("{namespace}.{class_name}")
        };
        result.objects.push(Object {
            file_id: local_id,
            class_id: *class_id,
            type_name,
            sha256: crate::sha256(&encoded),
        });
        objects.push((*id, tree, encoded, script_id));
    }
    if objects.is_empty() {
        bail!("native runtime profile has no usable objects");
    }
    if classes
        .values()
        .any(|class| ids.contains(&class.local_id) || class.local_id == 0)
    {
        bail!("an object of a scripted import has the local ID of a script object");
    }
    // The scripts come first, as a build lists them.
    let script_tree = playerbake::script_tree()?;
    let mut written = Vec::with_capacity(classes.len() + objects.len());
    for class in classes.values() {
        let hash = (0..16)
            .map(|index| (format!("bytes[{index}]").into(), Val::U(0)))
            .collect();
        written.push(playerbake::Object {
            local_id: class.local_id,
            tree: script_tree,
            data: script_tree.encode(&Val::map([
                ("m_Name", Val::str(&class.class_name)),
                ("m_ExecutionOrder", Val::I(0)),
                ("m_PropertiesHash", Val::Map(hash)),
                ("m_ClassName", Val::str(&class.class_name)),
                ("m_Namespace", Val::str(&class.namespace)),
                ("m_AssemblyName", Val::str(&class.assembly)),
            ]))?,
            script: None,
        });
    }
    for (id, tree, data, script) in objects {
        written.push(playerbake::Object {
            local_id: id,
            tree,
            data,
            script: Some(script),
        });
    }
    let output = playerbake::write(&written, &[])?;
    result.input_sha256 = crate::sha256(input);
    result.output_sha256 = crate::sha256(&output);
    Ok((output, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_class_an_object_names() {
        assert_eq!(
            class_identifier("Unity.InputSystem::UnityEngine.InputSystem.InputActionAsset"),
            Some((
                "Unity.InputSystem".into(),
                "UnityEngine.InputSystem".into(),
                "InputActionAsset".into()
            ))
        );
        assert_eq!(
            class_identifier("Assembly-CSharp::Probe"),
            Some(("Assembly-CSharp".into(), String::new(), "Probe".into()))
        );
        assert_eq!(
            class_identifier("UnityEngine.InputSystem.InputActionAsset"),
            None
        );
        assert_eq!(class_identifier("A::Outer+Inner"), None);
        assert_eq!(class_identifier("::Probe"), None);
    }

    #[test]
    fn finds_pointers_out_of_the_file() {
        let local = Val::map([("m_FileID", Val::I(0)), ("m_PathID", Val::I(7))]);
        let other = Val::map([("m_FileID", Val::I(2)), ("m_PathID", Val::I(7))]);
        assert!(!points_outside(&Val::Seq(vec![local.clone(), Val::Zero])));
        assert!(points_outside(&Val::map([(
            "items",
            Val::Seq(vec![local, other])
        )])));
    }
}
