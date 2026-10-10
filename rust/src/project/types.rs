//! Published descriptor types. Field order, omission and absent-versus-empty
//! lists are fixed so their JSON and digests are identical.
use super::value::{Map, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{collections::BTreeMap, ops::Deref};

/// A list that keeps absent (`None`, encoded as `null`) apart from empty (`Some(vec![])`, encoded as `[]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List<T>(pub Option<Vec<T>>);
impl<T> Default for List<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<T> Deref for List<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.0.as_deref().unwrap_or(&[])
    }
}
impl<T> From<Vec<T>> for List<T> {
    fn from(items: Vec<T>) -> Self {
        Self(Some(items))
    }
}
impl<T> FromIterator<T> for List<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        Self(Some(items.into_iter().collect()))
    }
}
impl<T> List<T> {
    pub fn nil() -> Self {
        Self(None)
    }
    /// Appends an item; an absent list becomes present.
    pub fn push(&mut self, item: T) {
        self.0.get_or_insert_with(Vec::new).push(item);
    }
    pub fn is_empty_list(&self) -> bool {
        self.0.as_ref().is_none_or(Vec::is_empty)
    }
}
impl<T: Serialize> Serialize for List<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(Option::<Vec<T>>::deserialize(deserializer)?))
    }
}

/// A JSON `null` leaves a field at its default value.
pub(crate) fn nul<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
#[allow(clippy::ptr_arg)] // serde passes the field type
fn blank(text: &String) -> bool {
    text.is_empty()
}

macro_rules! wire_struct {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$fmeta:meta])* pub $field:ident : $ty:ty,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default, rename_all = "camelCase")]
        pub struct $name { $(#[serde(deserialize_with = "nul")] $(#[$fmeta])* pub $field: $ty,)* }
    };
}

wire_struct! {
    pub struct NativeContentObject {
        pub file_id: String,
        pub type_name: String,
    }
}
wire_struct! {
    pub struct NativeContentResource {
        pub path: String,
        pub virtual_path: String,
        pub sha256: String,
    }
}
wire_struct! {
    pub struct NativeContentDependency {
        pub guid: String,
        pub global: bool,
    }
}
wire_struct! {
    pub struct NativeMeshCPURetention {
        pub file_id: String,
        pub original_is_readable: bool,
    }
}
wire_struct! {
    pub struct NativeUIBinding {
        pub owner_file_id: String,
        pub property_path: String,
        pub target_guid: String,
        pub target_file_id: String,
        #[serde(skip_serializing_if = "blank")]
        pub managed_reference_id: String,
    }
}
wire_struct! {
    pub struct NativeUIBindingProof {
        pub schema_version: i64,
        pub mode: String,
        pub native_sha256: String,
        pub reader_sha256: String,
        pub dump_sha256: String,
        pub bindings_sha256: String,
    }
}
wire_struct! {
    pub struct NativeContentFile {
        pub guid: String,
        pub path: String,
        pub sha256: String,
        pub objects: List<NativeContentObject>,
        pub resources: List<NativeContentResource>,
        pub dependencies: List<NativeContentDependency>,
        #[serde(rename = "meshCpuRetention", skip_serializing_if = "List::is_empty_list")]
        pub mesh_cpu_retention: List<NativeMeshCPURetention>,
        #[serde(skip_serializing_if = "List::is_empty_list")]
        pub ui_bindings: List<NativeUIBinding>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ui_binding_proof: Option<NativeUIBindingProof>,
    }
}
wire_struct! {
    pub struct NativeMeshCPUProjection {
        pub schema_version: i64,
        pub mode: String,
        pub source_path: String,
        pub source_sha256: String,
        pub proof_path: String,
        pub proof_sha256: String,
    }
}
wire_struct! {
    pub struct NativeModelGraphTarget {
        pub file_id: String,
        pub guid: String,
    }
}
wire_struct! {
    pub struct NativeModelGraphReference {
        pub property: String,
        pub targets: List<NativeModelGraphTarget>,
    }
}
wire_struct! {
    pub struct NativeModelGraphObject {
        pub file_id: String,
        pub class_id: i32,
        pub references: List<NativeModelGraphReference>,
        /// `None` is an absent map and encodes as `null`.
        pub scalars: Option<Map>,
    }
}
wire_struct! {
    pub struct ShaderSourceRequirement {
        pub path: String,
        pub source_path: String,
        pub sha256: String,
    }
}
wire_struct! {
    /// Preserves an existing compiled build asset. Source requirements guard
    /// later edits; they do not prove the build came from this source revision.
    pub struct CompiledShaderSnapshot {
        pub schema_version: i64,
        pub compiled_source_revision_verified: bool,
        pub shader_name: String,
        pub source_path: String,
        pub archive_file_id: String,
        pub bundle_path: String,
        pub bundle_sha256: String,
        pub member_path: String,
        pub member_name: String,
        pub member_sha256: String,
        pub payload_sha256: String,
        pub type_tree_sha256: String,
        pub source_requirements: List<ShaderSourceRequirement>,
        pub missing_includes: List<String>,
    }
}
wire_struct! {
    pub struct NativeReplacement {
        pub guid: String,
        pub source_path: String,
        pub source_sha256: String,
        pub meta_sha256: String,
        #[serde(skip_serializing_if = "blank")]
        pub environment_sha256: String,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        pub config_source_sha256: BTreeMap<String, String>,
        pub content: NativeContentFile,
        #[serde(skip_serializing_if = "List::is_empty_list")]
        pub model_graph: List<NativeModelGraphObject>,
        #[serde(skip_serializing_if = "blank")]
        pub model_graph_sha256: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub compiled_shader_snapshot: Option<CompiledShaderSnapshot>,
        #[serde(rename = "meshCpuProjection", skip_serializing_if = "Option::is_none")]
        pub mesh_cpu_projection: Option<NativeMeshCPUProjection>,
    }
}
wire_struct! {
    /// An immutable engine shader snapshot, kept apart from authored imports.
    pub struct RetainedBuiltinShader {
        pub schema_version: i64,
        pub unity_version: String,
        pub guid: String,
        pub file_id: String,
        pub shader_name: String,
        pub source_guid: String,
        pub source_file_id: String,
        pub archive_file_id: String,
        pub bundle_path: String,
        pub bundle_sha256: String,
        pub member_path: String,
        pub member_name: String,
        pub member_sha256: String,
        pub payload_sha256: String,
        pub type_tree_sha256: String,
        pub builtin_source_path: String,
        #[serde(rename = "builtinSourcePayloadSha256")]
        pub builtin_source_sha256: String,
        pub compiled_source_revision_verified: bool,
        pub content: NativeContentFile,
    }
}
wire_struct! {
    /// A saved override Unity cannot apply because its target is absent from a
    /// complete current public native model inventory.
    pub struct PrefabOverrideDiagnostic {
        pub path: String,
        pub instance_file_id: String,
        pub target_guid: String,
        pub target_file_id: String,
        pub property_path: String,
        pub reason: String,
        pub proof_backend: String,
        pub proof_source_path: String,
        pub proof_source_sha256: String,
        pub proof_meta_sha256: String,
        pub proof_inventory_sha256: String,
    }
}
wire_struct! {
    pub struct AuthoredIdentity {
        pub guid: String,
        pub file_id: String,
    }
}
wire_struct! {
    pub struct AuthoredDependency {
        pub path: String,
        pub sha256: String,
    }
}

/// A Unity document; signed 64-bit identities stay decimal strings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Document {
    #[serde(deserialize_with = "nul")]
    pub class_id: i64,
    #[serde(deserialize_with = "nul")]
    pub file_id: String,
    #[serde(deserialize_with = "nul")]
    pub type_name: String,
    /// `None` is an absent map (`"data":null`).
    pub data: Option<Map>,
    #[serde(deserialize_with = "nul")]
    pub stripped: bool,
}
static NO_DATA: Map = Map::new();
impl Document {
    pub fn fields(&self) -> &Map {
        self.data.as_ref().unwrap_or(&NO_DATA)
    }
    pub fn get(&self, key: &str) -> &Value {
        super::value::get(self.fields(), key)
    }
    pub fn from_yaml(document: &crate::yaml::Document) -> Self {
        Self {
            class_id: document.class_id,
            file_id: document.file_id.clone(),
            type_name: document.type_name.clone(),
            data: Some(
                document
                    .data
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from_yaml(v)))
                    .collect(),
            ),
            stripped: document.stripped,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoredGraphObject {
    #[serde(flatten)]
    pub document: Document,
    pub source_path: String,
    pub guid: String,
    pub identities: Vec<AuthoredIdentity>,
}

/// The recursive authored prefab expansion used by scene preparation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoredGraph {
    pub path: String,
    pub guid: String,
    pub objects: Vec<AuthoredGraphObject>,
    pub local_ids: BTreeMap<String, String>,
    pub dependencies: Vec<AuthoredDependency>,
    pub ignored_prefab_override_diagnostics: Vec<PrefabOverrideDiagnostic>,
}
impl AuthoredGraph {
    /// The published JSON text.
    pub fn to_json_text(&self) -> anyhow::Result<String> {
        super::value::encode_json(self)
    }
}
