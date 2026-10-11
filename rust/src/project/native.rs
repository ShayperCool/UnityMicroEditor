//! Native content overlays: validation, artifact pinning and publication.
use super::{
    expand::builtin_guid,
    lexpath,
    native_model::native_model_graph_sha256,
    session::{Inner, Session, asset_key},
    strconv,
    types::{
        Document, List, NativeContentFile, NativeModelGraphObject, NativeModelGraphReference,
        NativeModelGraphTarget, NativeReplacement,
    },
    value::{self, Map, Value, array, get, map_file_id, sprint},
};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

pub(crate) fn native_content_class(type_name: &str) -> i32 {
    match type_name {
        "UnityEngine.Texture2D" => 28,
        "UnityEngine.AudioClip" => 83,
        "UnityEngine.GameObject" => 1,
        "UnityEngine.Transform" => 4,
        "UnityEngine.MeshRenderer" => 23,
        "UnityEngine.MeshFilter" => 33,
        "UnityEngine.Mesh" => 43,
        "UnityEngine.Shader" => 48,
        "UnityEngine.Material" => 21,
        "UnityEngine.Font" => 128,
        "UnityEngine.AnimationClip" => 74,
        "UnityEngine.RuntimeAnimatorController" => 91,
        "UnityEngine.AnimatorOverrideController" => 221,
        "UnityEngine.UIElements.StyleSheet" | "UnityEngine.UIElements.VisualTreeAsset" => 114,
        _ => 0,
    }
}

pub(crate) fn valid_native_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub(crate) fn valid_native_guid(guid: &str) -> bool {
    valid_native_hex(guid, 32) && guid.bytes().any(|c| c != b'0') && !builtin_guid(guid)
}
pub(crate) fn valid_native_id(id: &str) -> bool {
    strconv::parse_int(id, 10).is_ok_and(|n| n != 0 && n.to_string() == id)
}

fn validate_native_ui_objects(profile: &str, r: &NativeReplacement) -> Result<()> {
    if !r.model_graph.is_empty()
        || !r.model_graph_sha256.is_empty()
        || !r.content.resources.is_empty()
    {
        bail!("native UI/font content must have no model graph or streamed resource");
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for value in r.content.objects.iter() {
        if !valid_native_id(&value.file_id) {
            bail!("native UI object ID is invalid");
        }
        *counts.entry(&value.type_name).or_default() += 1;
    }
    let count = |name: &str| counts.get(name).copied().unwrap_or(0);
    match profile {
        ".uss" => {
            if r.content.objects.len() != 1 || count("UnityEngine.UIElements.StyleSheet") != 1 {
                bail!("native USS content requires exactly one StyleSheet");
            }
        }
        ".uxml" => {
            if r.content.objects.len() != 2
                || count("UnityEngine.UIElements.StyleSheet") != 1
                || count("UnityEngine.UIElements.VisualTreeAsset") != 1
            {
                bail!(
                    "native UXML content requires exactly one VisualTreeAsset and its inline StyleSheet"
                );
            }
        }
        ".ttf" | ".otf" => {
            let expected = [
                ("12800000", "UnityEngine.Font"),
                ("2100000", "UnityEngine.Material"),
                ("2800000", "UnityEngine.Texture2D"),
            ];
            if r.content.objects.len() != expected.len() {
                bail!("native font content requires exactly Font, Material and Texture2D");
            }
            for value in r.content.objects.iter() {
                if !expected
                    .iter()
                    .any(|(id, name)| *id == value.file_id && *name == value.type_name)
                {
                    bail!("native font runtime object identity differs");
                }
            }
        }
        _ => bail!("unsupported native UI/font profile"),
    }
    Ok(())
}

pub(crate) fn validate_native_dependency_graph(
    files: &BTreeMap<String, NativeContentFile>,
) -> Result<()> {
    fn visit(
        guid: &str,
        files: &BTreeMap<String, NativeContentFile>,
        state: &mut HashMap<String, u8>,
    ) -> Result<()> {
        match state.get(guid) {
            Some(1) => bail!("native dependency cycle contains {guid}"),
            Some(2) => return Ok(()),
            _ => {}
        }
        let Some(file) = files.get(guid) else {
            bail!("native dependency {guid} has no runtime descriptor");
        };
        state.insert(guid.into(), 1);
        for dependency in file.dependencies.iter() {
            if !dependency.global {
                visit(&dependency.guid, files, state)?;
            }
        }
        state.insert(guid.into(), 2);
        Ok(())
    }
    let mut state = HashMap::new();
    files
        .keys()
        .try_for_each(|guid| visit(guid, files, &mut state))
}

pub(crate) fn match_native_model_graph(
    expected: &[NativeModelGraphObject],
    actual: &[NativeModelGraphObject],
) -> Result<()> {
    if expected.len() != actual.len() || actual.is_empty() {
        bail!("model object count differs or graph was not verified");
    }
    let mut by_id: HashMap<&str, &NativeModelGraphObject> = HashMap::new();
    for g in actual {
        if !valid_native_id(&g.file_id) {
            bail!("invalid model object ID");
        }
        if by_id.insert(&g.file_id, g).is_some() {
            bail!("duplicate model object ID");
        }
    }
    static NONE: Map = Map::new();
    for old in expected {
        let Some(next) = by_id
            .get(old.file_id.as_str())
            .filter(|n| n.class_id == old.class_id)
        else {
            bail!("model identity or type differs: {}", old.file_id);
        };
        if old.references != next.references {
            bail!("model references or order differ: {}", old.file_id);
        }
        if !scalar_maps_equal(
            old.scalars.as_ref().unwrap_or(&NONE),
            next.scalars.as_ref().unwrap_or(&NONE),
            old.class_id == 4,
        ) {
            bail!("model authored fields differ: {}", old.file_id);
        }
    }
    Ok(())
}

fn scalar_maps_equal(a: &Map, b: &Map, float32_pose: bool) -> bool {
    a.len() == b.len()
        && a.iter().all(|(key, value)| {
            b.get(key)
                .is_some_and(|other| native_scalar_equal(value, other, float32_pose))
        })
}
pub(crate) fn native_scalar_equal(first: &Value, second: &Value, float32_pose: bool) -> bool {
    match (first, second) {
        (Value::Object(a), Value::Object(b)) => return scalar_maps_equal(a, b, float32_pose),
        (Value::Object(_), _) => return false,
        (Value::String(a), Value::String(b)) => return a == b,
        (Value::String(_), _) => return false,
        (Value::Null, other) | (other, Value::Null) => return other.is_null(),
        (Value::Bool(a), Value::Bool(b)) => return a == b,
        (Value::Bool(_), _) | (_, Value::Bool(_)) => return false,
        _ => {}
    }
    let (first, second) = (sprint(first), sprint(second));
    let (Some(a), Some(b)) = (value::decimal(&first), value::decimal(&second)) else {
        return false;
    };
    if !float32_pose {
        return a == b;
    }
    let (Ok(x), Ok(y)) = (first.parse::<f64>(), second.parse::<f64>()) else {
        return false;
    };
    let (px, py) = (x as f32, y as f32);
    !px.is_infinite() && !py.is_infinite() && px == py
}

impl Inner {
    /// Reads the authored fields the runtime loader consumes. Unsupported
    /// native component graphs must go through a new catalog.
    pub(crate) fn native_model_graph(&self, guid: &str) -> Result<Vec<NativeModelGraphObject>> {
        let Some(documents) = self.models.get(guid).filter(|d| !d.is_empty()) else {
            bail!("FBX GUID has no cached model hierarchy: {guid}");
        };
        let mut graph = Vec::with_capacity(documents.len());
        for d in documents.iter() {
            let mut g = NativeModelGraphObject {
                file_id: d.file_id.clone(),
                class_id: d.class_id as i32,
                references: List::from(Vec::new()),
                scalars: Some(Map::new()),
            };
            let data = d.fields();
            let mut reference = |name: &str, list: bool, components: bool| -> Result<()> {
                let single;
                let values: &[Value] = if list {
                    array(get(data, name))
                } else {
                    single = [data
                        .get(name)
                        .ok_or_else(|| anyhow!("cached model property {name} is missing"))?
                        .clone()];
                    &single
                };
                let mut targets = Vec::with_capacity(values.len());
                for value in values {
                    let value = if components {
                        value.field("component")
                    } else {
                        value
                    };
                    let Value::Object(fields) = value else {
                        bail!("cached model reference {name} is invalid");
                    };
                    let id = map_file_id(fields).into_owned();
                    let guid = if id == "0" {
                        String::new()
                    } else {
                        value::text(fields, "guid").into_owned()
                    };
                    targets.push(NativeModelGraphTarget { file_id: id, guid });
                }
                g.references.push(NativeModelGraphReference {
                    property: name.into(),
                    targets: List::from(targets),
                });
                Ok(())
            };
            let (references, scalars): (&[&str], &[&str]) = match d.class_id {
                1 => {
                    reference("m_Component", true, true)?;
                    (
                        &[],
                        &[
                            "m_Name",
                            "m_Layer",
                            "m_TagString",
                            "m_IsActive",
                            "m_StaticEditorFlags",
                        ],
                    )
                }
                4 => {
                    reference("m_GameObject", false, false)?;
                    reference("m_Children", true, false)?;
                    (
                        &["m_Father"],
                        &["m_LocalPosition", "m_LocalRotation", "m_LocalScale"],
                    )
                }
                33 => (&["m_GameObject", "m_Mesh"], &[]),
                23 => {
                    reference("m_GameObject", false, false)?;
                    reference("m_Materials", true, false)?;
                    (
                        &[
                            "m_StaticBatchRoot",
                            "m_ProbeAnchor",
                            "m_LightProbeVolumeOverride",
                            "m_AdditionalVertexStreams",
                        ],
                        &[
                            "m_Enabled",
                            "m_CastShadows",
                            "m_ReceiveShadows",
                            "m_DynamicOccludee",
                            "m_MotionVectors",
                            "m_LightProbeUsage",
                            "m_ReflectionProbeUsage",
                            "m_RenderingLayerMask",
                            "m_SortingLayerID",
                            "m_SortingOrder",
                        ],
                    )
                }
                class => bail!("cached model class {class} has no verified graph profile"),
            };
            for name in references {
                reference(name, false, false)?;
            }
            let mut values = Map::new();
            for name in scalars {
                let value = data
                    .get(*name)
                    .ok_or_else(|| anyhow!("cached model scalar {name} is missing"))?;
                values.insert((*name).into(), value.clone());
            }
            g.scalars = Some(values);
            graph.push(g);
        }
        Ok(graph)
    }

    pub(crate) fn verify_native_artifacts(&mut self, file: &NativeContentFile) -> Result<()> {
        super::native_ui::validate_native_mesh_cpu_retention(file)?;
        self.verify_native_artifact(&file.path, &file.sha256)?;
        let mut seen = HashSet::new();
        for resource in file.resources.iter() {
            if resource.virtual_path != format!("VirtualArtifacts/Primary/{}.resource", file.guid)
                || !seen.insert(&resource.virtual_path)
            {
                bail!("native resource alias is invalid or duplicated");
            }
            self.verify_native_artifact(&resource.path, &resource.sha256)?;
        }
        Ok(())
    }
    fn verify_native_artifact(&mut self, path: &str, hash: &str) -> Result<()> {
        if !lexpath::is_abs(path) || lexpath::clean(path) != path || !valid_native_hex(hash, 64) {
            bail!("native artifact descriptor is invalid: {path}");
        }
        let canonical =
            lexpath::eval_symlinks(path).map_err(|e| super::session::path_error(path, e))?;
        if canonical != path {
            bail!("native artifact is symlinked: {path}");
        }
        // Unity's own import cache is not an artifact store; the tool's owned
        // subtree of Library is.
        let library = lexpath::join(&[&self.project, "Library"]);
        let owned = lexpath::join(&[&library, "UnityMicroEditor"]);
        if (path == library || path.starts_with(&format!("{library}/")))
            && !path.starts_with(&format!("{owned}/"))
        {
            bail!("native artifact must be independent of project Library");
        }
        let (_, actual) = self.read(path, false)?;
        if actual != hash {
            bail!("native artifact changed after import: {path}");
        }
        Ok(())
    }

    fn validate_native_replacement(&mut self, r: &NativeReplacement) -> Result<()> {
        if !valid_native_guid(&r.guid)
            || r.content.guid != r.guid
            || !valid_native_hex(&r.source_sha256, 64)
            || !valid_native_hex(&r.meta_sha256, 64)
        {
            bail!("native replacement source identity is invalid");
        }
        let path = r.source_path.as_str();
        if !path.starts_with("Assets/") || lexpath::clean(path) != path {
            bail!("native replacement source path is invalid: {path}");
        }
        let profile = lexpath::lower_ext(path);
        if let Some(projection) = &r.mesh_cpu_projection
            && (profile != ".fbx"
                || projection.schema_version != 1
                || projection.mode != "native-model-mesh-cpu-v1"
                || !valid_native_hex(&projection.source_sha256, 64)
                || !valid_native_hex(&projection.proof_sha256, 64))
        {
            bail!("native Mesh CPU projection provenance is unsupported");
        }
        let new_model =
            profile == ".fbx" && self.models.get(&r.guid).is_none_or(|docs| docs.is_empty());
        let texture = matches!(profile.as_str(), ".png" | ".jpg" | ".jpeg");
        let ui = matches!(profile.as_str(), ".uss" | ".uxml" | ".ttf" | ".otf");
        let shader = profile == ".shader";
        // Baked by this tool from the authored source; see `playerbake`.
        let clip = profile == ".anim";
        let controller = profile == ".controller" || profile == ".overridecontroller";
        for (logical, expected) in &r.config_source_sha256 {
            if !texture
                || logical != "ProjectSettings/EditorSettings.asset"
                || !valid_native_hex(expected, 64)
            {
                bail!("native configuration source guard is unsupported: {logical}");
            }
            let physical = self.source(logical)?;
            if !self
                .read(&physical, true)
                .is_ok_and(|(_, actual)| actual == *expected)
            {
                bail!(
                    "native texture compressor settings changed after artifact validation: {logical}"
                );
            }
        }
        if profile != ".wav"
            && profile != ".fbx"
            && !texture
            && !ui
            && !shader
            && !clip
            && !controller
        {
            bail!("native replacement profile is unsupported: {path}");
        }
        let mut expected: BTreeMap<String, String> = BTreeMap::new();
        let mut known_path = String::new();
        {
            let mut add = |guid: &str, id: &str, type_name: &str, source: &str| -> Result<()> {
                if guid != r.guid {
                    return Ok(());
                }
                if source != path {
                    bail!("native GUID {guid} belongs to {source}, not {path}");
                }
                if expected
                    .get(id)
                    .is_some_and(|old| !old.is_empty() && old != type_name)
                {
                    bail!("catalog has conflicting native object type {id}");
                }
                if !valid_native_id(id) || type_name.is_empty() {
                    bail!("catalog native identity is incomplete: {id}");
                }
                expected.insert(id.into(), type_name.into());
                known_path = source.into();
                Ok(())
            };
            for a in &self.catalog.assets {
                add(
                    &value::text(a, "guid"),
                    &value::text(a, "fileId"),
                    &value::text(a, "typeName"),
                    &value::text(a, "path"),
                )?;
            }
            for a in self.assets.values() {
                if value::text(a, "kind") == "imported" {
                    add(
                        &value::text(a, "guid"),
                        &value::text(a, "fileId"),
                        &value::text(a, "typeName"),
                        &value::text(a, "path"),
                    )?;
                }
            }
            if profile == ".fbx" {
                for model in &self.catalog.model_prefabs {
                    if model.guid == r.guid {
                        for doc in &model.objects {
                            add(&model.guid, &doc.file_id, &doc.type_name, &model.path)?;
                        }
                    }
                }
            }
        }
        if known_path.is_empty() || expected.is_empty() {
            // Index current project metadata rather than trusting caller-provided
            // GUIDs. Refresh once for an asset absent from the previous index.
            if self.path_of(&r.guid).is_empty() || self.guid_of(path).is_empty() {
                self.index_dirty = true;
            }
            self.index_guids()?;
            if self.path_of(&r.guid) != path || self.guid_of(path) != r.guid {
                bail!(
                    "native replacement GUID is not owned by project source {path}: {}",
                    r.guid
                );
            }
            if profile == ".wav" {
                expected.insert("8300000".into(), "UnityEngine.AudioClip".into());
            } else if texture {
                expected.insert("2800000".into(), "UnityEngine.Texture2D".into());
            } else if shader {
                expected.insert("4800000".into(), "UnityEngine.Shader".into());
            } else if ui {
                validate_native_ui_objects(&profile, r)?;
                for value in r.content.objects.iter() {
                    expected.insert(value.file_id.clone(), value.type_name.clone());
                }
            } else if clip || controller {
                for value in r.content.objects.iter() {
                    expected.insert(value.file_id.clone(), value.type_name.clone());
                }
            } else if profile == ".fbx" {
                self.static_native_model_documents(r)?;
                for value in r.content.objects.iter() {
                    expected.insert(value.file_id.clone(), value.type_name.clone());
                }
            } else {
                bail!("new native model requires an existing verified hierarchy: {path}");
            }
        }
        let objects = &r.content.objects;
        if objects.len() != expected.len() {
            bail!(
                "native object identity set changed for {path}: expected {}, got {}",
                expected.len(),
                objects.len()
            );
        }
        let mut found = HashSet::new();
        for o in objects.iter() {
            if !valid_native_id(&o.file_id)
                || found.contains(&o.file_id)
                || expected.get(&o.file_id).map_or("", String::as_str) != o.type_name
            {
                bail!(
                    "native object {} is missing, duplicated, or changed type in {path}",
                    o.file_id
                );
            }
            found.insert(&o.file_id);
        }
        if profile == ".wav" {
            if objects.len() != 1
                || objects[0].type_name != "UnityEngine.AudioClip"
                || !r.model_graph.is_empty()
            {
                bail!("WAV replacement must contain one AudioClip");
            }
        } else if texture {
            if objects.len() != 1
                || objects[0].file_id != "2800000"
                || objects[0].type_name != "UnityEngine.Texture2D"
                || !r.model_graph.is_empty()
            {
                bail!("texture replacement must contain one Texture2D");
            }
        } else if shader {
            if objects.len() != 1
                || objects[0].file_id != "4800000"
                || objects[0].type_name != "UnityEngine.Shader"
                || !r.model_graph.is_empty()
                || !r.model_graph_sha256.is_empty()
                || !r.content.resources.is_empty()
                || !r.content.dependencies.is_empty()
            {
                bail!(
                    "standalone Shader replacement must contain only Shader 4800000 without native dependencies"
                );
            }
        } else if clip {
            if objects.len() != 1
                || objects[0].type_name != "UnityEngine.AnimationClip"
                || !r.model_graph.is_empty()
                || !r.model_graph_sha256.is_empty()
                || !r.content.resources.is_empty()
                || !r.content.dependencies.is_empty()
            {
                bail!(
                    "animation replacement must contain one AnimationClip without native dependencies"
                );
            }
        } else if controller {
            if objects.len() != 1
                || objects[0].type_name
                    != if profile == ".controller" {
                        "UnityEngine.RuntimeAnimatorController"
                    } else {
                        "UnityEngine.AnimatorOverrideController"
                    }
                || !r.model_graph.is_empty()
                || !r.model_graph_sha256.is_empty()
                || !r.content.resources.is_empty()
            {
                bail!("controller replacement must contain exactly its controller object");
            }
        } else if ui {
            validate_native_ui_objects(&profile, r)?;
        } else if new_model {
            if !valid_native_hex(&r.model_graph_sha256, 64)
                || native_model_graph_sha256(&r.model_graph) != r.model_graph_sha256
            {
                bail!("new native model graph differs from the adapter's verified projection");
            }
            self.static_native_model_documents(r)?;
        } else {
            let old_graph = self.native_model_graph(&r.guid)?;
            match_native_model_graph(&old_graph, &r.model_graph)
                .map_err(|e| e.context(format!("FBX authored graph changed in {path}")))?;
        }
        for name in [path.to_string(), format!("{path}.meta")] {
            let source = self.source(&name)?;
            let meta = name.ends_with(".meta");
            let (content, hash) = self.read(&source, meta)?;
            let mut expected_hash = &r.source_sha256;
            if meta {
                expected_hash = &r.meta_sha256;
                let guid = content.as_deref().and_then(crate::yaml::metadata_guid);
                if guid.as_deref() != Some(r.guid.as_str()) {
                    bail!("native metadata GUID changed: {name}");
                }
            }
            if hash != *expected_hash {
                bail!("native source changed after import: {name}");
            }
        }
        self.verify_native_artifacts(&r.content)?;
        self.validate_compiled_shader_snapshot(r)?;
        let mut dependencies = HashSet::new();
        for d in r.content.dependencies.iter() {
            if dependencies.contains(&d.guid) || d.guid == r.guid {
                bail!("duplicate or cyclic native dependency {}", d.guid);
            }
            dependencies.insert(&d.guid);
            if d.global {
                if !builtin_guid(&d.guid) {
                    bail!(
                        "native global dependency is not a built-in GUID: {}",
                        d.guid
                    );
                }
            } else if !valid_native_guid(&d.guid) {
                bail!("native dependency GUID is invalid: {}", d.guid);
            } else if !self.native_overlays.contains_key(&d.guid)
                && !self.derived_native.contains_key(&d.guid)
            {
                bail!(
                    "native dependency {} has no validated runtime descriptor",
                    d.guid
                );
            }
        }
        let mut files: BTreeMap<String, NativeContentFile> = self
            .native_overlays
            .iter()
            .map(|(guid, overlay)| (guid.clone(), overlay.content.clone()))
            .collect();
        files.extend(
            self.derived_native
                .iter()
                .map(|(guid, file)| (guid.clone(), file.clone())),
        );
        files.insert(r.guid.clone(), r.content.clone());
        super::native_ui::validate_native_ui_bindings(&r.content, &files)?;
        validate_native_dependency_graph(&files)?;
        self.validate_observed("native file changed during validation")
    }

    fn replace_native_content(&mut self, replacement: &NativeReplacement) -> Result<()> {
        self.validate_native_replacement(replacement)?;
        let guid = &replacement.guid;
        let new_source = !self
            .assets
            .values()
            .chain(&self.catalog.assets)
            .any(|a| value::text(a, "guid") == *guid);
        let encoded = value::encode_json(replacement)?;
        let owned: NativeReplacement = value::decode_use_number(&encoded)?;
        let mut model_documents: Vec<Document> = Vec::new();
        if lexpath::equal_fold(lexpath::ext(&owned.source_path), ".fbx")
            && self.models.get(&owned.guid).is_none_or(|d| d.is_empty())
        {
            model_documents = self.static_native_model_documents(&owned)?;
        }
        if !model_documents.is_empty() {
            self.models
                .insert(owned.guid.clone(), Arc::new(model_documents));
        }
        self.critical.insert(
            replacement.source_path.clone(),
            replacement.source_sha256.clone(),
        );
        self.critical.insert(
            format!("{}.meta", replacement.source_path),
            replacement.meta_sha256.clone(),
        );
        for value in owned.content.objects.iter() {
            let key = asset_key(&owned.guid, &value.file_id);
            if value.type_name == "UnityEngine.Texture2D"
                && let Some(existing) = self.assets.get_mut(&key)
                && value::text(existing, "kind") == "texture"
            {
                // Importer settings can move an existing raw texture outside the
                // verified public decoder profile. Its accepted native descriptor
                // becomes authoritative for subsequent preparation attempts.
                existing.insert("kind".into(), "imported".into());
                existing.insert("typeName".into(), value.type_name.as_str().into());
                existing.insert("classId".into(), Value::Int(28));
            }
        }
        // A source may be outside the baseline closure while still being a genuine
        // project asset. Publish its verified runtime identities alongside the
        // native descriptor so raw references can resolve it.
        if new_source {
            for value in owned.content.objects.iter() {
                self.assets
                    .entry(asset_key(&owned.guid, &value.file_id))
                    .or_insert_with(|| {
                        value::map([
                            ("kind", "imported".into()),
                            ("guid", owned.guid.as_str().into()),
                            ("path", owned.source_path.as_str().into()),
                            ("fileId", value.file_id.as_str().into()),
                            ("typeName", value.type_name.as_str().into()),
                            (
                                "classId",
                                Value::Int32(native_content_class(&value.type_name)),
                            ),
                            ("name", lexpath::stem(&owned.source_path).into()),
                            ("references", Value::Array(Vec::new())),
                            ("dataJson", "{}".into()),
                            ("importerJson", "{}".into()),
                        ])
                    });
            }
        }
        self.native_overlays.insert(guid.clone(), owned);
        Ok(())
    }

    /// Project paths of the known native assets with the given extension.
    pub(crate) fn native_asset_paths(&self, extension: &str) -> BTreeSet<String> {
        self.assets
            .values()
            .chain(&self.catalog.assets)
            .map(|asset| value::text(asset, "path").into_owned())
            .filter(|path| lexpath::equal_fold(lexpath::ext(path), extension))
            .collect()
    }

    pub(crate) fn publish_native_content(&mut self, over: &mut Map) -> Result<()> {
        if self.native_overlays.is_empty() && self.derived_native.is_empty() {
            return Ok(());
        }
        let mut files: BTreeMap<String, NativeContentFile> = BTreeMap::new();
        let existing = over
            .get("nativeContentFiles")
            .unwrap_or_else(|| get(&self.baseline, "nativeContentFiles"));
        if !existing.is_null() {
            let decoded: List<NativeContentFile> = value::decode_use_number(&existing.to_json()?)?;
            for file in decoded.iter() {
                if files.insert(file.guid.clone(), file.clone()).is_some() {
                    bail!("duplicate baseline native descriptor");
                }
            }
        }
        for (guid, r) in self.native_overlays.clone() {
            self.verify_native_artifacts(&r.content)?;
            // Re-check fresh source expectations even if this GUID is not currently used.
            self.depend(&r.source_path)?;
            self.publish_compiled_shader_snapshot(&r, over)?;
            files.insert(guid, r.content);
        }
        for (guid, file) in self.derived_native.clone() {
            self.verify_native_artifacts(&file)?;
            files.insert(guid, file);
        }
        let ordered: Vec<&NativeContentFile> = files.values().collect();
        over.insert("nativeContentFiles".into(), value::raw(&ordered)?);
        Ok(())
    }
}

impl Session {
    /// Publishes content derived from part of a source under a GUID that no
    /// project source owns, so other native content can depend on it. The
    /// caller keeps it current with the source it was derived from.
    pub fn set_derived_native_content(&self, file: &NativeContentFile) -> Result<()> {
        let mut s = self.lock();
        if !valid_native_guid(&file.guid)
            || file.objects.is_empty()
            || !file.resources.is_empty()
            || !file.dependencies.is_empty()
            || file
                .objects
                .iter()
                .any(|object| !valid_native_id(&object.file_id))
        {
            bail!("derived native content descriptor is invalid");
        }
        s.index_guids()?;
        if !s.path_of(&file.guid).is_empty() || s.native_overlays.contains_key(&file.guid) {
            bail!(
                "derived native content GUID belongs to a source: {}",
                file.guid
            );
        }
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let verified = s.verify_native_artifacts(file);
        s.cache.observed = observed;
        s.cache.stats = stats;
        verified?;
        s.derived_native.insert(file.guid.clone(), file.clone());
        Ok(())
    }

    /// Accepts an already adapted runtime artifact. It does not run importers
    /// or modify the baseline/catalog. Source hashes, object identities and FBX
    /// authored hierarchy must all match before an overlay becomes visible.
    pub fn replace_native_content(&self, replacement: &NativeReplacement) -> Result<()> {
        let mut s = self.lock();
        // This validation has its own read transaction. Preserve preparation
        // counters and observations while keeping freshly verified file hashes.
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let result = s.replace_native_content(replacement);
        s.cache.observed = observed;
        s.cache.stats = stats;
        result
    }
}
