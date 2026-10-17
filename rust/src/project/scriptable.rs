//! Raw ScriptableObjects with their complete same-file closure.
use super::{
    expand::visit_map_mut,
    manifest::reference_list,
    session::Inner,
    types::Document,
    value::{self, Map, Value, get, map_file_id, object},
};
use anyhow::{Result, anyhow, bail};
use std::collections::HashSet;

impl Inner {
    pub(crate) fn refresh_scriptable_object(
        &mut self,
        asset: &mut Map,
        documents: &[Document],
    ) -> Result<()> {
        let (path, guid, id) = (
            value::text(asset, "path").into_owned(),
            value::text(asset, "guid").into_owned(),
            value::text(asset, "fileId").into_owned(),
        );
        let mut known: HashSet<&str> = HashSet::new();
        let mut selected: Option<&Document> = None;
        for document in documents {
            if document.class_id != 114 {
                bail!(
                    "raw ScriptableObject {path} has unsupported class{} subasset",
                    document.class_id
                );
            }
            known.insert(&document.file_id);
            if document.file_id == id || documents.len() == 1 && id.is_empty() {
                selected = Some(document);
            }
        }
        let Some(selected) = selected else {
            bail!("raw ScriptableObject {path} has no source localID {id}");
        };
        let mut data = selected.fields().clone();
        let mut failure: Option<anyhow::Error> = None;
        visit_map_mut(&mut data, "", &mut |property, reference| {
            let local = map_file_id(reference).into_owned();
            if local == "0" || !value::text(reference, "guid").is_empty() {
                return;
            }
            if !known.contains(local.as_str()) {
                failure = Some(anyhow!(
                    "ScriptableObject {path}.{property} has unknown same-file localID {local}"
                ));
                return;
            }
            reference.insert("guid".into(), guid.as_str().into());
        });
        if let Some(failure) = failure {
            return Err(failure);
        }
        let script = object(get(&data, "m_Script"));
        let script_guid = value::text(script, "guid").into_owned();
        let mut type_name = self.script_type(&script_guid).to_string();
        if script_guid == "0000000000000000e000000000000000" {
            // Pinned Unity 6 serialized PanelSettings identity: script 19101
            // of the Editor's `unity default resources`. An asset saved
            // before the Editor wrote class identifiers has an empty one.
            if map_file_id(script) != "19101"
                || !matches!(
                    value::text(&data, "m_EditorClassIdentifier").as_ref(),
                    "" | "UnityEngine.dll::UnityEngine.UIElements.PanelSettings"
                )
            {
                bail!("raw ScriptableObject {path} has an unverified built-in MonoScript identity");
            }
            type_name = "UnityEngine.UIElements.PanelSettings".into();
        }
        if type_name.is_empty() {
            bail!(
                "raw ScriptableObject {path} requires type metadata for MonoScript {script_guid}/{}",
                map_file_id(script)
            );
        }
        asset.insert("fileId".into(), selected.file_id.as_str().into());
        asset.insert("classId".into(), Value::Int(114));
        asset.insert("typeName".into(), type_name.into());
        asset.insert("name".into(), value::text(&data, "m_Name").as_ref().into());
        let encoded = value::compact_map(&data);
        asset.insert(
            "dataJson".into(),
            encoded.as_deref().unwrap_or_default().into(),
        );
        asset.insert("references".into(), reference_list(&data));
        encoded.map(|_| ())
    }
}
