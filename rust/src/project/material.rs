//! Authored materials: runtime properties stay raw, parents are validated.
use super::{
    lexpath,
    manifest::reference_list,
    native::valid_native_guid,
    session::Inner,
    types::Document,
    value::{self, Map, Value, array, file_id, get, number, object},
};
use anyhow::{Result, bail};
use std::collections::HashSet;

const ASSET_VERSION: &str =
    "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion";

impl Inner {
    /// A .mat already contains authored runtime properties. Its compiled Shader
    /// and imported textures are resolved separately.
    pub(crate) fn refresh_authored_material(&mut self, asset: &mut Map) -> Result<()> {
        let path = value::text(asset, "path").into_owned();
        let physical = self.source(&path)?;
        self.depend(&path)?;
        let documents = self.parse(&physical)?;
        let mut material: Option<&Document> = None;
        for doc in documents.iter() {
            if doc.class_id == 21 {
                if material.is_some() {
                    bail!("material {path} has multiple runtime objects");
                }
                material = Some(doc);
            } else if doc.class_id != 114
                || value::str(doc.get("m_EditorClassIdentifier")) != ASSET_VERSION
            {
                bail!(
                    "material {path} has unsupported subasset class {}",
                    doc.class_id
                );
            }
        }
        let Some(material) = material else {
            bail!("material {path} has no class21 document");
        };
        // A variant of a shader with an inspector is published as a build
        // writes it: whole, without a parent.
        let inspected = self.inspected_material_fields(&path, material)?;
        let data = inspected.as_ref().unwrap_or_else(|| material.fields());
        if number(get(data, "serializedVersion")) != 8 {
            bail!("material {path} requires serializedVersion8");
        }
        if file_id(get(data, "m_Shader")) == "0" {
            bail!("material {path} has no Shader");
        }
        if inspected.is_none() {
            self.validate_material_parents(&path, material, &mut HashSet::new())?;
        }
        for group in ["m_TexEnvs", "m_Ints", "m_Floats", "m_Colors"] {
            for item in array(get(data, "m_SavedProperties").field(group)) {
                if object(item).len() != 1 {
                    bail!("material {path} has malformed {group} property");
                }
            }
        }
        asset.insert("fileId".into(), material.file_id.as_str().into());
        asset.insert("classId".into(), Value::Int(21));
        asset.insert("typeName".into(), "UnityEngine.Material".into());
        asset.insert("name".into(), value::text(data, "m_Name").as_ref().into());
        let encoded = value::compact_map(data);
        asset.insert(
            "dataJson".into(),
            encoded.as_deref().unwrap_or_default().into(),
        );
        asset.insert("references".into(), reference_list(data));
        encoded.map(|_| ())
    }

    /// Variant header defaults are not overrides when the serialized mask is
    /// zero. Nonzero masks need a separately verified serialized-property mapping.
    fn validate_material_parents(
        &mut self,
        path: &str,
        material: &Document,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        if !visiting.insert(path.to_string()) {
            bail!("material parent cycle at {path}");
        }
        let result = self.validate_material_parent(path, material, visiting);
        visiting.remove(path);
        result
    }
    fn validate_material_parent(
        &mut self,
        path: &str,
        material: &Document,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        let data = material.fields();
        let parent_reference = object(get(data, "m_Parent"));
        if value::map_file_id(parent_reference) == "0" {
            return Ok(());
        }
        if value::text(data, "m_ModifiedSerializedProperties") != "0" {
            bail!("material variant {path} has unsupported modified serialized properties");
        }
        let parent_guid = value::text(parent_reference, "guid");
        if !valid_native_guid(&parent_guid) {
            bail!("material variant {path} has invalid parent GUID");
        }
        let parent_path = self.path_of(&parent_guid).to_string();
        if parent_path.is_empty() || lexpath::ext(&parent_path) != ".mat" {
            // Imported parents can be subassets of a native model. Their Material
            // and Shader identities are checked through public runtime objects.
            return Ok(());
        }
        self.depend(&parent_path)?;
        let physical = self.source(&parent_path)?;
        let documents = self.parse(&physical)?;
        let parent_id = value::map_file_id(parent_reference);
        let mut parent: Option<&Document> = None;
        for document in documents.iter() {
            if document.class_id == 21 && document.file_id == parent_id {
                if parent.is_some() {
                    bail!("material variant {path} has ambiguous parent identity");
                }
                parent = Some(document);
            }
        }
        let Some(parent) = parent else {
            bail!("material variant {path} has no class21 parent {parent_path}");
        };
        let (shader, parent_shader) = (
            object(get(data, "m_Shader")),
            object(parent.get("m_Shader")),
        );
        if value::map_file_id(shader) != value::map_file_id(parent_shader)
            || value::text(shader, "guid") != value::text(parent_shader, "guid")
        {
            bail!("material variant {path} has a different Shader from its parent");
        }
        self.validate_material_parents(&parent_path, parent, visiting)
    }
}
