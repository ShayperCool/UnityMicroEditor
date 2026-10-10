//! Revision-checked source edits with persisted operation receipts.
mod assets;
mod component_defaults;
mod components;

mod edit;
mod hierarchy;
mod inventory;
mod prefab;
mod prefab_authoring;
mod prefab_components;
mod prefab_linking;
mod prefab_overrides;
mod prefab_serialization;
mod prefab_unpack;
mod publication;
mod script;
mod store;
mod structural;
mod yaml_edit;
pub(crate) mod yamlcodec;
pub use assets::AssetCreateRequest;
pub use component_defaults::{NativeDefaultsRequest, NativeDefaultsTemplate};
pub use components::native_component_types;

pub use edit::{EditRequest, EditResult, Operation};
pub use hierarchy::{HierarchyPage, HierarchyRequest};
pub use inventory::{InventoryPage, InventoryRequest};
pub use prefab_authoring::{PrefabEditRequest, PrefabEditResult};
pub use publication::{
    PublicationInput, PublicationOutput, PublicationResult, PublicationUndoRequest,
    StagedPublicationRequest,
};
pub use script::{ScriptRequest, UndoRequest};
pub use store::{
    AuthoringError, Capabilities, Change, MutationResult, ProjectInfo, Source, Store, encode_json,
};
#[cfg(test)]
mod tests;
