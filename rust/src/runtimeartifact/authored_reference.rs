//! The sources a model names that are not imported.
//!
//! A renderer of a model can name a material that is a `.mat` source rather
//! than one the importer made: one the model's `externalObjects` remap it to,
//! or the default material of a render pipeline, which the Universal
//! pipeline gives a part without a material of its own. A ContentFile reaches
//! another file through that file's own ContentFile only, and an authored
//! material is loaded from its source, not imported. The renderers of a model
//! are not placed from the ContentFile, though: the scene builds them from
//! the model's static graph, which keeps the material's identity, and binds
//! the authored material there. So the file of the adapted model drops the
//! reference and the renderers' pointers along it.
//!
//! An animation event of a model's clip can pass a script (its `MonoScript`)
//! as its object parameter. Neither a ContentFile nor the scene loader here
//! holds a `MonoScript`; the event keeps its function, number and string and
//! passes no object.
use super::{adapt::Metadata, be64, builtin_reference::layout};
use crate::playerbake::{Val, read_serialized_with_externals};
use anyhow::{Result, bail};

pub const MESH_RENDERER: i32 = 23;
pub const SKINNED_MESH_RENDERER: i32 = 137;
pub const ANIMATION_CLIP: i32 = 74;

/// Rewrites the pointers of a value: `None` for a pointer into a dropped
/// file, the new index otherwise. Returns how many pointers were cleared.
fn repoint(value: &mut Val, remap: &dyn Fn(i64) -> Option<i64>) -> usize {
    match value {
        Val::Map(fields) => {
            let file = fields
                .iter()
                .find(|(name, _)| name == "m_FileID")
                .map(|(_, value)| value);
            let path = fields.iter().any(|(name, _)| name == "m_PathID");
            if let (Some(Val::I(file)), true) = (file, path) {
                let (file, cleared) = match remap(*file) {
                    Some(index) => (index, 0),
                    None => (0, 1),
                };
                for (name, field) in fields.iter_mut() {
                    match name.as_ref() {
                        "m_FileID" => *field = Val::I(file),
                        "m_PathID" if cleared == 1 => *field = Val::I(0),
                        _ => {}
                    }
                }
                return cleared;
            }
            fields
                .iter_mut()
                .map(|(_, field)| repoint(field, remap))
                .sum()
        }
        Val::Seq(items) => items.iter_mut().map(|item| repoint(item, remap)).sum(),
        _ => 0,
    }
}

/// Drops the external references of an adapted model to sources that are
/// not imported, clearing the pointers along them and renumbering the
/// others. `owners` gives, for the GUID of a reference, the classes whose
/// objects may point along it; none keeps the reference. Returns `None` when
/// the file drops nothing. Refused: an object of another class that points
/// along a dropped reference.
pub fn unbind_source_references(
    file: &[u8],
    metadata: &mut Metadata,
    owners: &dyn Fn(&str) -> &'static [i32],
) -> Result<Option<Vec<u8>>> {
    let dropped: Vec<(usize, &'static [i32])> = metadata
        .external_references
        .iter()
        .enumerate()
        .map(|(position, external)| (position, owners(&external.guid)))
        .filter(|(_, classes)| !classes.is_empty())
        .collect();
    if dropped.is_empty() {
        return Ok(None);
    }
    // File index 0 is the file itself; external `n` is index `n + 1`.
    let remap = |index: i64| -> Option<i64> {
        if index <= 0 {
            return Some(index);
        }
        let position = (index - 1) as usize;
        if dropped.iter().any(|(gone, _)| *gone == position) {
            return None;
        }
        Some(index - dropped.iter().filter(|(gone, _)| *gone < position).count() as i64)
    };
    let layout = layout(file)?;
    let (objects, _) = read_serialized_with_externals(file)?;
    let mut data = file[layout.data_offset..].to_vec();
    for object in &objects {
        let Some(kept) = metadata
            .objects
            .iter_mut()
            .find(|kept| kept.file_id == object.local_id.to_string())
        else {
            continue;
        };
        let mut value = object.tree.decode(&object.data)?;
        for (gone, classes) in &dropped {
            let index = *gone as i64 + 1;
            if !classes.contains(&object.tree.class_id)
                && repoint(&mut value.clone(), &|at| (at != index).then_some(at)) != 0
            {
                bail!(
                    "object {} of class {} of the model names a source that is not imported ({})",
                    object.local_id,
                    object.tree.class_id,
                    metadata.external_references[*gone].guid
                );
            }
        }
        repoint(&mut value, &remap);
        let encoded = object.tree.encode(&value)?;
        if encoded == object.data {
            continue;
        }
        let Some(&(_, at, size)) = layout
            .objects
            .iter()
            .find(|(local_id, _, _)| *local_id == object.local_id)
        else {
            bail!("model object is missing from the object table");
        };
        if encoded.len() != size {
            bail!("a rewritten pointer changed the size of its object");
        }
        let at = at - layout.data_offset;
        data[at..at + size].copy_from_slice(&encoded);
        kept.sha256 = crate::sha256(&encoded);
    }
    // The count of external references comes right before the first.
    let count_at = layout.externals[0].0 - 4;
    let kept = layout.externals.len() - dropped.len();
    let mut metadata_bytes = file[48..count_at].to_vec();
    metadata_bytes.extend_from_slice(&(kept as u32).to_le_bytes());
    for (position, &(start, end)) in layout.externals.iter().enumerate() {
        if !dropped.iter().any(|(gone, _)| *gone == position) {
            metadata_bytes.extend_from_slice(&file[start..end]);
        }
    }
    let last = layout.externals[layout.externals.len() - 1].1;
    metadata_bytes.extend_from_slice(&file[last..layout.metadata_end]);
    let mut data_offset = 48 + metadata_bytes.len();
    while !data_offset.is_multiple_of(16) {
        data_offset += 1;
    }
    if be64(&file[32..40]) as usize != layout.data_offset {
        bail!("model data offset differs from its header");
    }
    let mut output = file[..48].to_vec();
    output[20..24].copy_from_slice(&(metadata_bytes.len() as u32).to_be_bytes());
    output[24..32].copy_from_slice(&((data_offset + data.len()) as u64).to_be_bytes());
    output[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
    output.extend_from_slice(&metadata_bytes);
    output.resize(data_offset, 0);
    output.extend_from_slice(&data);
    let mut position = 0;
    metadata.external_references.retain(|_| {
        position += 1;
        !dropped.iter().any(|(gone, _)| *gone == position - 1)
    });
    metadata.output_sha256 = crate::sha256(&output);
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtimeartifact::builtin_reference::BUILTIN_EXTRA_GUID;

    /// The artifact a worker left for an `.obj` without a material of its
    /// own, adapted: one generated material that names `Standard`, the only
    /// external reference.
    fn adapted() -> (Vec<u8>, Metadata) {
        let artifact =
            include_bytes!("../../tests/testdata/native/generated-material.obj.artifact");
        crate::runtimeartifact::adapt(artifact, "fbx").unwrap()
    }

    #[test]
    fn a_file_without_unimported_sources_is_left_alone() {
        let (file, mut metadata) = adapted();
        let before = metadata.clone();
        assert_eq!(
            unbind_source_references(&file, &mut metadata, &|_| &[]).unwrap(),
            None
        );
        assert_eq!(metadata, before);
    }

    #[test]
    fn only_the_given_classes_may_point_along_a_dropped_reference() {
        // The material points at the shader along the one reference.
        let (file, mut metadata) = adapted();
        let error = unbind_source_references(&file, &mut metadata, &|guid| {
            if guid == BUILTIN_EXTRA_GUID {
                &[MESH_RENDERER]
            } else {
                &[]
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("of class 21"), "{error}");
    }

    #[test]
    fn a_dropped_reference_leaves_the_file_readable_without_it() {
        let (file, mut metadata) = adapted();
        // Turn the shader's pointer into a renderer's material, as far as
        // the rewrite can tell: clear it first so only the table changes.
        let layout = layout(&file).unwrap();
        let (objects, _) = read_serialized_with_externals(&file).unwrap();
        let mut cleared = file.clone();
        for object in &objects {
            let mut value = object.tree.decode(&object.data).unwrap();
            if repoint(&mut value, &|index| (index != 1).then_some(index)) == 0 {
                continue;
            }
            let encoded = object.tree.encode(&value).unwrap();
            let &(_, at, size) = layout
                .objects
                .iter()
                .find(|(id, _, _)| *id == object.local_id)
                .unwrap();
            assert_eq!(encoded.len(), size);
            cleared[at..at + size].copy_from_slice(&encoded);
        }
        let output = unbind_source_references(&cleared, &mut metadata, &|guid| {
            if guid == BUILTIN_EXTRA_GUID {
                &[MESH_RENDERER]
            } else {
                &[]
            }
        })
        .unwrap()
        .unwrap();
        let (objects, externals) = read_serialized_with_externals(&output).unwrap();
        assert!(externals.is_empty());
        assert_eq!(objects.len(), metadata.objects.len());
        assert!(metadata.external_references.is_empty());
        assert_eq!(metadata.output_sha256, crate::sha256(&output));
    }
}
