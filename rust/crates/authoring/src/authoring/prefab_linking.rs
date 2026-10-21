use super::{
    Operation,
    hierarchy::id,
    prefab::{append_instance, name_modification},
    prefab_authoring::{PrefabEditRequest, PrefabPlanner},
    prefab_serialization::has_comment,
    store::*,
    yaml_edit::{
        TextAsset, TextDocument, flow_end, json_numbers, normalize, reference, scalar_end,
        visit_references_mut,
    },
    yamlcodec::{self, Kind, Node, Style, internal},
};
use crate::{
    graph::{AuthoredGraph, AuthoredIdentity, GraphObject},
    sha256,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
pub(crate) const HEADER: &str = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n";
pub(crate) fn source_identity(
    file_id: &str,
    identities: &[AuthoredIdentity],
    guid: &str,
) -> Result<String> {
    let mut found = String::new();
    for identity in identities.iter().filter(|identity| identity.guid == guid) {
        if !found.is_empty() && found != identity.file_id {
            return Err(failure(
                "ambiguous_identity",
                "prefab source has multiple local aliases for one target",
                None,
            ));
        }
        found = identity.file_id.clone();
    }
    if found.is_empty() {
        return Err(failure(
            "invalid_identity",
            "target lacks the requested prefab source identity",
            Some(json!({"guid": guid, "fileId": file_id})),
        ));
    }
    Ok(found)
}
pub(crate) fn parse_file_id(text: &str) -> Result<i64> {
    text.parse::<i64>()
        .map_err(|_| internal(format!("parsing {text:?}: invalid syntax")))
}
/// The identity an expanded object has inside the prefab whose GUID is `guid`.
pub(crate) fn prefab_graph_identity(
    file_id: &str,
    identities: &[AuthoredIdentity],
    guid: &str,
) -> Result<String> {
    if file_id.contains(':') {
        let mut inherited = 0i64;
        for part in file_id.split(':') {
            inherited ^= parse_file_id(part)?;
        }
        let candidate = (inherited & i64::MAX).to_string();
        if identities
            .iter()
            .any(|identity| identity.guid == guid && identity.file_id == candidate)
        {
            return Ok(candidate);
        }
    }
    source_identity(file_id, identities, guid)
}
fn father(object: &GraphObject) -> String {
    id(object.document.data.get("m_Father").unwrap_or(&Value::Null))
}
pub(crate) fn owner_of(object: &GraphObject) -> String {
    id(object
        .document
        .data
        .get("m_GameObject")
        .unwrap_or(&Value::Null))
}
/// The resolved root GameObject and Transform of a prefab graph.
pub(crate) fn prefab_roots(graph: &AuthoredGraph) -> Result<(&GraphObject, &GraphObject)> {
    let mut transform: Option<&GraphObject> = None;
    for value in &graph.objects {
        if [4, 224].contains(&value.document.class_id) && father(value) == "0" {
            if transform.is_some() {
                return Err(failure(
                    "invalid_prefab_root",
                    "prefab must have one resolved root",
                    None,
                ));
            }
            transform = Some(value);
        }
    }
    let owner = transform.and_then(|transform| {
        let owner = owner_of(transform);
        graph
            .objects
            .iter()
            .find(|value| value.document.class_id == 1 && value.document.file_id == owner)
            .map(|owner| (owner, transform))
    });
    owner.ok_or_else(|| failure("invalid_prefab_root", "prefab root owner is absent", None))
}
impl PrefabPlanner<'_> {
    pub(crate) fn instantiate(&mut self, request: &PrefabEditRequest) -> Result<()> {
        let mut parent = "0".to_string();
        if !request.parent_file_id.is_empty() || !request.parent_path.is_empty() {
            let target = self.target(&request.parent_file_id, &request.parent_path)?;
            parent = self.local_owner(&self.graph.objects[target].document.file_id);
            if parent.is_empty() {
                return Err(failure(
                    "unsupported_parent",
                    "instantiation under an inherited parent requires an explicit local parent alias",
                    None,
                ));
            }
        }
        let source = self.store.read_source(&request.prefab_path)?;
        let (_, revisions) = self.include_graph(&request.prefab_path)?;
        let created = self.store.instantiate_prefab(
            &mut self.asset,
            &self.source.path,
            &request.operation_id,
            &Operation {
                prefab_path: request.prefab_path.clone(),
                prefab_revision: source.revision,
                dependency_revisions: revisions,
                key: "prefab".into(),
                name: request.name.clone(),
                parent_id: parent,
                ..Operation::default()
            },
        )?;
        for (key, value) in created {
            self.created.insert(format!("prefab{key}"), value);
        }
        self.write_scene()
    }
    pub(crate) fn local_owner(&self, expanded: &str) -> String {
        if let Some(doc) = self.asset.find(expanded).filter(|doc| doc.class_id() == 1) {
            return doc.id().to_string();
        }
        self.asset
            .docs
            .iter()
            .find(|doc| {
                doc.class_id() == 1
                    && self.graph.local_ids.get(doc.id()).map(String::as_str) == Some(expanded)
            })
            .map_or_else(String::new, |doc| doc.id().to_string())
    }
    fn new_asset(&mut self, request: &PrefabEditRequest, asset: &TextAsset) -> Result<()> {
        self.store.asset_path(&request.output_path, &[".prefab"])?;
        if self.store.dependency_revision_inner(&request.output_path)? != "absent" {
            return Err(failure(
                "revision_conflict",
                "new prefab source or metadata already exists",
                Some(json!({"path": request.output_path})),
            ));
        }
        let digest = sha256(
            format!(
                "unitymicroeditor-prefab-guid-v1\0{}\0{}",
                request.operation_id, request.output_path
            )
            .as_bytes(),
        );
        let guid = digest[..32].to_string();
        self.store.check_guid_available(&guid)?;
        let metadata = format!(
            "fileFormatVersion: 2\nguid: {guid}\nPrefabImporter:\n  externalObjects: {{}}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
        );
        self.inputs
            .insert(request.output_path.clone(), "absent".into());
        self.created.insert("assetGuid".into(), guid);
        self.write_asset(&request.output_path, asset.text(), Some(metadata))
    }
    pub(crate) fn create_variant(&mut self, request: &PrefabEditRequest) -> Result<()> {
        let (graph, revisions) = self.include_graph(&request.prefab_path)?;
        if revisions
            .get(&request.output_path)
            .is_some_and(|revision| !revision.is_empty())
        {
            return Err(failure(
                "prefab_cycle",
                "variant would depend on itself",
                None,
            ));
        }
        let source = self.store.read_source(&request.prefab_path)?;
        let mut asset = TextAsset::empty(HEADER, "\n");
        let instance = asset.new_id(&request.operation_id, "variant-instance");
        let (owner, _) = prefab_roots(&graph)?;
        let target =
            prefab_graph_identity(&owner.document.file_id, &owner.identities, &source.guid)?;
        append_instance(
            &mut asset,
            &instance,
            &source.guid,
            "0",
            vec![name_modification(
                &target,
                &source.guid,
                prefab_name(&request.output_path),
            )],
        )?;
        self.created.insert("variant.instance".into(), instance);
        self.new_asset(request, &asset)
    }
    pub(crate) fn create_prefab(&mut self, request: &PrefabEditRequest) -> Result<()> {
        let target = self.target(&request.file_id, &request.target_path)?;
        let target_id = self.graph.objects[target].document.file_id.clone();
        let selected = self.selected(&target_id)?;
        if target_id.contains(':') {
            return Err(failure(
                "unsupported_subtree",
                "creating a prefab from an inherited instance root requires an explicit variant; choose create_variant",
                None,
            ));
        }
        let transform = self
            .transform(&target_id)
            .ok_or_else(|| failure("invalid_graph", "prefab root Transform is absent", None))?;
        let transform_id = self.graph.objects[transform].document.file_id.clone();
        // The selected root loses its outside parent before closed-reference
        // validation. Every other source reference must stay within this subtree.
        let mut input = TextAsset::parse(&self.asset.text())?;
        input.set_top_level(&transform_id, "m_Father", &reference("0"))?;
        let mut included = HashSet::new();
        for doc in &self.asset.docs {
            if self
                .graph
                .local_ids
                .get(doc.id())
                .is_some_and(|expanded| selected.contains(expanded))
            {
                included.insert(doc.id().to_string());
            }
            if doc.class_id() == 1001 {
                let prefix = format!("{}:", doc.id());
                let (mut inside, mut outside) = (false, false);
                for object in self.objects.keys().filter(|key| key.starts_with(&prefix)) {
                    if selected.contains(object) {
                        inside = true;
                    } else {
                        outside = true;
                    }
                }
                if inside && outside {
                    return Err(failure(
                        "unsupported_subtree",
                        "prefab subtree cuts through an inherited nested instance",
                        None,
                    ));
                }
                if inside {
                    included.insert(doc.id().to_string());
                }
            }
        }
        // Ordinary IDs are retained. Inherited stripped aliases must become the
        // canonical prefab-local XOR identity rather than an arbitrary scene alias:
        // otherwise Unity propagates a different source ID through later variants.
        let mut mapping: HashMap<String, String> = included
            .iter()
            .map(|file_id| (file_id.clone(), file_id.clone()))
            .collect();
        for doc in &input.docs {
            if !included.contains(doc.id()) || !doc.stripped() {
                continue;
            }
            let instance = parse_file_id(&id(doc.field("m_PrefabInstance")))?;
            let source = parse_file_id(&id(doc.field("m_CorrespondingSourceObject")))?;
            mapping.insert(
                doc.id().to_string(),
                ((instance ^ source) & i64::MAX).to_string(),
            );
        }
        let mut used: HashMap<&str, &str> = HashMap::new();
        for (source, target) in &mapping {
            if used
                .insert(target, source)
                .is_some_and(|previous| previous != source)
            {
                return Err(failure(
                    "invalid_identity",
                    "canonical prefab alias collides with another source document",
                    Some(json!({"fileId": target})),
                ));
            }
        }
        let mut output = TextAsset::empty(&self.asset.prefix, self.asset.newline);
        for doc in &input.docs {
            if included.contains(doc.id()) {
                output.docs.push(clone_prefab_document(
                    doc,
                    &mapping,
                    &self.source.guid,
                    true,
                )?);
            }
        }
        if output.docs.is_empty() {
            return Err(failure(
                "invalid_subtree",
                "prefab subtree contains no authored documents",
                None,
            ));
        }
        output.refresh()?;
        output.set_top_level(&transform_id, "m_Father", &reference("0"))?;
        output.set_property(
            &target_id,
            "m_Name",
            &Value::String(prefab_name(&request.output_path).to_string()),
        )?;
        self.created.insert("prefab".into(), target_id);
        self.created.insert("prefab.transform".into(), transform_id);
        self.new_asset(request, &output)
    }
}
fn prefab_name(output_path: &str) -> &str {
    let base = output_path.rsplit('/').next().unwrap_or(output_path);
    base.strip_suffix(".prefab").unwrap_or(base)
}
fn line_start(body: &str, offset: usize) -> usize {
    body[..offset].rfind('\n').map_or(0, |index| index + 1)
}
fn prefab_collection_span(doc: &TextDocument, node: &Node) -> Result<(usize, usize)> {
    let start = node.start;
    if node.flow {
        let end = flow_end(&doc.body, start, false)
            .ok_or_else(|| failure("invalid_yaml", "reference lexical span is incomplete", None))?;
        return Ok((start, end));
    }
    let mut end = doc.body.len();
    doc.node.visit(&mut |value| {
        if (value.index < node.index || value.index > node.last) && value.start > start {
            end = end.min(line_start(&doc.body, value.start));
        }
    });
    Ok((start, end))
}
struct Edit {
    start: usize,
    end: usize,
    text: String,
}
fn collect_edits(
    doc: &TextDocument,
    node: &Node,
    mapping: &HashMap<String, String>,
    source_guid: &str,
    require_closed: bool,
    newline: &str,
    edits: &mut Vec<Edit>,
) -> Result<()> {
    if node.kind == Kind::Mapping
        && let Some((identity, _)) = node.field("fileID")
        && identity.kind == Kind::Scalar
    {
        let guid = node
            .field("guid")
            .map_or("", |(guid, _)| guid.value.as_str());
        let file_id = identity.value.as_str();
        if file_id == "0" || (!guid.is_empty() && guid != source_guid) {
            return Ok(());
        }
        let Some(replacement) = mapping.get(file_id).filter(|value| !value.is_empty()) else {
            if require_closed {
                return Err(failure(
                    "external_subtree_reference",
                    "prefab subtree references an object outside the selected source subtree",
                    Some(json!({"fileId": file_id})),
                ));
            }
            return Ok(());
        };
        if !guid.is_empty() {
            // Self-GUID references become local PPtrs. Preserve every other
            // field/tag on this PPtr rather than rebuilding the object.
            let mut local = node.clone();
            local.content.clear();
            for pair in node.content.as_chunks::<2>().0 {
                if pair[0].value == "guid" || pair[0].value == "type" {
                    continue;
                }
                let mut value = pair[1].clone();
                if pair[0].value == "fileID" {
                    value.value = replacement.clone();
                    value.tag = yamlcodec::INT.into();
                    value.tagged = false;
                    value.style = Style::Plain;
                }
                local.content.push(pair[0].clone());
                local.content.push(value);
            }
            let (start, end) = prefab_collection_span(doc, node)?;
            if has_comment(&doc.body[start..end]) {
                return Err(yamlcodec::unported_comment());
            }
            let mut text = yamlcodec::encode_node(&local)?;
            if !node.flow {
                text.push_str(newline);
            }
            edits.push(Edit { start, end, text });
            return Ok(());
        }
        if replacement != file_id {
            edits.push(Edit {
                start: identity.start,
                end: scalar_end(&doc.body, identity)?,
                text: replacement.clone(),
            });
        }
        return Ok(());
    }
    for child in &node.content {
        collect_edits(
            doc,
            child,
            mapping,
            source_guid,
            require_closed,
            newline,
            edits,
        )?;
    }
    Ok(())
}
/// Copy one document, rewriting only PPtr identity spans. Re-encoding an entire
/// copied document would alter unknown serialized fields, tags, comments and styles.
pub(crate) fn clone_prefab_document(
    doc: &TextDocument,
    mapping: &HashMap<String, String>,
    source_guid: &str,
    require_closed: bool,
) -> Result<TextDocument> {
    let mut asset = TextAsset::parse(&format!("{HEADER}{}{}", doc.header, doc.body))?;
    let mut edits = Vec::new();
    collect_edits(
        &asset.docs[0],
        &asset.docs[0].node,
        mapping,
        source_guid,
        require_closed,
        asset.newline,
        &mut edits,
    )?;
    let copy = &mut asset.docs[0];
    if let Some(file_id) = mapping.get(copy.id()).filter(|value| !value.is_empty()) {
        copy.header = format!(
            "--- !u!{} &{file_id}{}\n",
            copy.class_id(),
            if copy.stripped() { " stripped" } else { "" }
        );
    }
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    for edit in edits {
        copy.body.replace_range(edit.start..edit.end, &edit.text);
    }
    asset.refresh()?;
    Ok(asset.docs.remove(0))
}
/// Copy resolved object data with its references moved to `mapping`.
pub(crate) fn remap_prefab_data(
    data: &Map<String, Value>,
    mapping: &HashMap<String, String>,
    source_guid: &str,
    require_closed: bool,
) -> Result<Map<String, Value>> {
    let mut copied = json_numbers(&Value::Object(data.clone()));
    visit_references_mut(&mut copied, &mut |reference| {
        let file_id = id(&Value::Object(reference.clone()));
        let guid = reference
            .get("guid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if file_id == "0" || (!guid.is_empty() && guid != source_guid) {
            return Ok(());
        }
        if let Some(replacement) = mapping.get(&file_id).filter(|value| !value.is_empty()) {
            reference.insert("fileID".into(), Value::String(replacement.clone()));
            if guid == source_guid {
                reference.remove("guid");
                reference.remove("type");
            }
            return Ok(());
        }
        if require_closed {
            return Err(failure(
                "external_subtree_reference",
                "prefab subtree references an object outside the selected source subtree",
                Some(json!({"fileId": file_id})),
            ));
        }
        Ok(())
    })?;
    match normalize(&copied)? {
        Value::Object(map) => Ok(map),
        _ => unreachable!(),
    }
}
