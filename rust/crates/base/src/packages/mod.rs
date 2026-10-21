//! Resolves explicit Unity project package inputs without invoking the Editor or changing
//! authored manifests/locks. Results are published as immutable snapshots under the
//! project's `Library/UnityMicroEditor/packages`, next to a proposed lock when it differs.
mod builtin;
mod cli;
mod current;
mod download;
mod files;
mod git;
mod lock;
mod resolve;
mod semver;
#[cfg(test)]
mod tests;

pub use cli::cli;
pub use cli::signal_context;
pub use current::load_current;
pub use resolve::resolve;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

pub use crate::cancel::{Canceled as ContextError, Context};

/// Resolver limits and switches. Zero limits select the defaults.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub cache_root: String,
    pub offline: bool,
    pub registry_tokens: BTreeMap<String, String>,
    pub max_download_bytes: i64,
    pub max_expanded_bytes: i64,
    pub max_cache_bytes: i64,
    pub max_files: i64,
    pub max_packages: i64,
    /// Ignores a prior commit lock for explicitly requested updates.
    pub refresh_git: bool,
    /// Tool-provided direct dependencies (`name` to a `file:` request) the
    /// authored manifest does not declare. They resolve as if declared, so the
    /// tool's runtime package never has to be written into the target manifest.
    pub provided: BTreeMap<String, String>,
    /// Registry or built-in packages the tool's runtime package needs from
    /// the selected engine's release line (`name` to a version), resolved as
    /// direct dependencies unless the project declares them itself.
    pub provided_versions: BTreeMap<String, String>,
}

/// The tool's runtime bridge package.
pub const RUNTIME_PACKAGE: &str = "com.unitymicroeditor.runtime";

impl Options {
    /// Provides the tool's runtime package from the installation directory,
    /// with the packages it needs in the release `project` selects.
    /// The Player host is always built with it, while the target manifest
    /// stays as authored.
    pub fn with_runtime_package(mut self, project: &str) -> anyhow::Result<Self> {
        for (name, version) in crate::engine::of_project(project)?.runtime_dependencies {
            self.provided_versions
                .insert(name.to_string(), version.to_string());
        }
        let tool = crate::util::micro_editor_root()?;
        let root = crate::project::lexpath::eval_symlinks(&crate::sysutil::join(&[
            &tool,
            "Packages",
            RUNTIME_PACKAGE,
        ]))?;
        self.provided
            .insert(RUNTIME_PACKAGE.into(), format!("file:{root}"));
        Ok(self)
    }
}

/// Authored manifests, locks and registry documents may spell an unset field as `null`.
fn nullable<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub name: String,
    pub logical_root: String,
    pub path: String,
    pub version: String,
    pub source: String,
    pub requested: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub registry: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subdirectory: String,
    pub content_hash: String,
    pub manifest_hash: String,
    pub dependencies: BTreeMap<String, String>,
    pub depth: i64,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unity_release: String,
    pub testable: bool,
    pub proof: String,
    /// Identifies the selected engine's exact source inventory when `path` reuses a
    /// byte-verified UPM materialized builtin copy.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_content_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_content_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub schema_version: i64,
    pub project: String,
    pub editor: String,
    pub packages: Vec<Package>,
    pub inputs: BTreeMap<String, String>,
    pub manifest_hash: String,
    pub lock_hash: String,
    pub resolution_hash: String,
    pub snapshot_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposed_lock_path: String,
    pub lock_changed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}
impl Snapshot {
    /// Rechecks every authored input and every resolved package tree before consumers
    /// publish compilation. Local package changes invalidate old snapshots.
    pub fn validate(&self, ctx: &Context) -> Result<()> {
        files::validate_snapshot(ctx, self)
    }
    /// Decodes a published snapshot file.
    pub fn from_json(data: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }
}

/// `scopedRegistries` entry of `Packages/manifest.json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ScopedRegistry {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    #[serde(default, deserialize_with = "nullable")]
    pub scopes: Vec<String>,
}
/// The fields of `Packages/manifest.json` that drive resolution.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Manifest {
    #[serde(default, deserialize_with = "nullable")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "nullable")]
    pub registry: String,
    #[serde(default, deserialize_with = "nullable")]
    pub scoped_registries: Vec<ScopedRegistry>,
    #[serde(default, deserialize_with = "nullable")]
    pub resolution_strategy: String,
    #[serde(default, deserialize_with = "nullable")]
    pub testables: Vec<String>,
    #[serde(default)]
    pub enable_lock_file: Option<bool>,
}
/// The fields of a package's `package.json` that drive resolution.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PackageManifest {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub version: String,
    #[serde(default, deserialize_with = "nullable")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(rename = "type", default, deserialize_with = "nullable")]
    pub kind: String,
    #[serde(default, deserialize_with = "nullable")]
    pub unity: String,
    #[serde(default, deserialize_with = "nullable")]
    pub unity_release: String,
}
/// One `packages-lock.json` entry, in Unity's own layout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LockEntry {
    #[serde(default, deserialize_with = "nullable")]
    pub version: String,
    #[serde(default, deserialize_with = "nullable")]
    pub depth: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub source: String,
    #[serde(default, deserialize_with = "nullable")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "String::is_empty"
    )]
    pub url: String,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "String::is_empty"
    )]
    pub hash: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LockFile {
    #[serde(default, deserialize_with = "nullable")]
    pub dependencies: BTreeMap<String, LockEntry>,
}
