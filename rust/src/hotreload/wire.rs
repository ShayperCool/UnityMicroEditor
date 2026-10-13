//! The documents of a code patch: the patch builder's request and result,
//! the descriptor the applier reads, the request the Player bridge polls and
//! the acknowledgement it writes. Readers ignore unknown members and take a
//! missing or `null` one as its zero value.
use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

/// The assembly reference name of the applier, which patch assemblies bind to.
pub const RUNTIME_ASSEMBLY: &str = "UnityMicroEditor.HotReload";

fn nz<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// One edit that cannot be applied live, or one caveat of an applied patch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reason {
    #[serde(deserialize_with = "nz")]
    pub code: String,
    #[serde(deserialize_with = "nz")]
    pub member: String,
    #[serde(deserialize_with = "nz")]
    pub message: String,
    /// Of a note about a method the patch carries: what that method runs,
    /// as the builder's short hash. A note with the same code, member and
    /// revision as one of the outcome before only still stands; any other
    /// is about code that changed since.
    #[serde(deserialize_with = "nz", skip_serializing_if = "String::is_empty")]
    pub revision: String,
}
impl Reason {
    pub fn new(code: &str, member: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            member: member.into(),
            message: message.into(),
            revision: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuilderAssembly {
    pub name: String,
    /// The DLL the running Player loaded at its start.
    pub loaded: String,
    /// The DLL of the newest compile.
    pub current: String,
    pub netcode: bool,
}

/// `dotnet HotReloadPatcher.dll <request.json>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuilderRequest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    pub generation: u64,
    /// Where the patch assemblies of this generation are written.
    pub output: String,
    #[serde(rename = "runtimeAssembly")]
    pub runtime_assembly: String,
    /// The result of the generation the Player runs; absent while it runs
    /// the code it loaded.
    #[serde(rename = "previousResult", skip_serializing_if = "String::is_empty")]
    pub previous_result: String,
    pub assemblies: Vec<BuilderAssembly>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Detour {
    #[serde(deserialize_with = "nz")]
    pub member: String,
    #[serde(deserialize_with = "nz")]
    pub kind: String,
    #[serde(rename = "originalToken", deserialize_with = "nz")]
    pub original_token: i64,
    #[serde(rename = "replacementToken", deserialize_with = "nz")]
    pub replacement_token: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PatchFile {
    #[serde(rename = "assemblyName", deserialize_with = "nz")]
    pub assembly_name: String,
    #[serde(deserialize_with = "nz")]
    pub path: String,
    #[serde(deserialize_with = "nz")]
    pub sha256: String,
    #[serde(deserialize_with = "nz")]
    pub pdb: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AddedMember {
    #[serde(deserialize_with = "nz")]
    pub member: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ResultAssembly {
    #[serde(deserialize_with = "nz")]
    pub name: String,
    #[serde(rename = "loadedMvid", deserialize_with = "nz")]
    pub loaded_mvid: String,
    #[serde(deserialize_with = "nz")]
    pub verdict: String,
    #[serde(deserialize_with = "nz")]
    pub patch: PatchFile,
    #[serde(deserialize_with = "nz")]
    pub detours: Vec<Detour>,
    #[serde(rename = "addedMethods", deserialize_with = "nz")]
    pub added_methods: Vec<AddedMember>,
    #[serde(rename = "addedFields", deserialize_with = "nz")]
    pub added_fields: Vec<AddedMember>,
    #[serde(deserialize_with = "nz")]
    pub unsupported: Vec<Reason>,
    #[serde(deserialize_with = "nz")]
    pub notes: Vec<Reason>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Summary {
    #[serde(rename = "patchedMethods", deserialize_with = "nz")]
    pub patched_methods: u64,
    #[serde(rename = "addedMethods", deserialize_with = "nz")]
    pub added_methods: u64,
    #[serde(rename = "addedFields", deserialize_with = "nz")]
    pub added_fields: u64,
}

pub const VERDICT_PATCH: &str = "patch";
pub const VERDICT_UNCHANGED: &str = "unchanged";
pub const VERDICT_UNSUPPORTED: &str = "unsupported";

/// The one JSON document the patch builder prints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BuilderResult {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub generation: u64,
    #[serde(deserialize_with = "nz")]
    pub verdict: String,
    #[serde(deserialize_with = "nz")]
    pub assemblies: Vec<ResultAssembly>,
    #[serde(deserialize_with = "nz")]
    pub summary: Summary,
}
impl BuilderResult {
    /// Decodes the builder's output for the request of `generation`.
    pub fn decode(output: &[u8], generation: u64) -> Result<Self> {
        let result: Self = serde_json::from_slice(output)
            .map_err(|error| anyhow::anyhow!("patch builder result is not its JSON: {error}"))?;
        if result.schema_version != 1 || result.generation != generation {
            bail!(
                "patch builder result has schema {} and generation {}, expected 1 and {generation}",
                result.schema_version,
                result.generation
            );
        }
        if ![VERDICT_PATCH, VERDICT_UNCHANGED, VERDICT_UNSUPPORTED]
            .contains(&result.verdict.as_str())
        {
            bail!("patch builder result has the verdict {:?}", result.verdict);
        }
        Ok(result)
    }

    /// Every reason of a refused change, in the order of the assemblies.
    pub fn unsupported(&self) -> Vec<Reason> {
        self.assemblies
            .iter()
            .flat_map(|assembly| assembly.unsupported.iter().cloned())
            .collect()
    }
    pub fn notes(&self) -> Vec<Reason> {
        self.assemblies
            .iter()
            .flat_map(|assembly| assembly.notes.iter().cloned())
            .collect()
    }
    /// The assemblies that carry a patch file.
    pub fn patched(&self) -> impl Iterator<Item = &ResultAssembly> {
        self.assemblies
            .iter()
            .filter(|assembly| assembly.verdict == VERDICT_PATCH)
    }
    /// Redirected, added and field members, in that order.
    pub fn members(&self) -> Vec<String> {
        let mut members = Vec::new();
        for assembly in self.patched() {
            members.extend(assembly.detours.iter().map(|detour| detour.member.clone()));
        }
        for assembly in self.patched() {
            let added = assembly.added_methods.iter().chain(&assembly.added_fields);
            members.extend(added.map(|added| added.member.clone()));
        }
        members
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DescriptorAssembly {
    pub name: String,
    #[serde(rename = "loadedMvid")]
    pub loaded_mvid: String,
    pub patch: String,
    #[serde(rename = "patchSha256")]
    pub patch_sha256: String,
    pub pdb: String,
    pub detours: Vec<Detour>,
}

/// `apply.json`: everything that differs from the assemblies the Player
/// loaded. An empty `assemblies` list restores every redirected method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Descriptor {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    pub generation: u64,
    /// The scope of the applier's callback scan.
    #[serde(rename = "gameAssemblies")]
    pub game_assemblies: Vec<String>,
    pub assemblies: Vec<DescriptorAssembly>,
}
impl Descriptor {
    /// The descriptor of a builder result; `None` restores the loaded code.
    pub fn new(
        generation: u64,
        game_assemblies: &[String],
        result: Option<&BuilderResult>,
    ) -> Result<Self> {
        let mut assemblies = Vec::new();
        for assembly in result.into_iter().flat_map(BuilderResult::patched) {
            let patch = &assembly.patch;
            if patch.path.is_empty() || patch.sha256.is_empty() || assembly.loaded_mvid.is_empty() {
                bail!(
                    "patch builder result names no patch file or MVID for {}",
                    assembly.name
                );
            }
            assemblies.push(DescriptorAssembly {
                name: assembly.name.clone(),
                loaded_mvid: assembly.loaded_mvid.clone(),
                patch: patch.path.clone(),
                patch_sha256: patch.sha256.clone(),
                pdb: patch.pdb.clone(),
                detours: assembly.detours.clone(),
            });
        }
        Ok(Self {
            schema_version: 1,
            generation,
            game_assemblies: game_assemblies.to_vec(),
            assemblies,
        })
    }
}

/// `<run>/code-reload-request.json`, which the bridge polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadRequest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub generation: u64,
    pub applier: String,
    #[serde(rename = "applierSha256")]
    pub applier_sha256: String,
    pub descriptor: String,
    #[serde(rename = "descriptorSha256")]
    pub descriptor_sha256: String,
    #[serde(rename = "requestedUtcUnixMilliseconds")]
    pub requested_utc_unix_milliseconds: i64,
    #[serde(rename = "sourceChangedUtcUnixMilliseconds")]
    pub source_changed_utc_unix_milliseconds: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AckFailure {
    #[serde(deserialize_with = "nz")]
    pub assembly: String,
    #[serde(deserialize_with = "nz")]
    pub member: String,
    #[serde(deserialize_with = "nz")]
    pub stage: String,
    #[serde(deserialize_with = "nz")]
    pub message: String,
}

/// `<request>.ack.json`, written by the applier through the bridge, or by
/// the bridge itself when the applier could not answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Ack {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(rename = "requestId", deserialize_with = "nz")]
    pub request_id: String,
    #[serde(deserialize_with = "nz")]
    pub pid: i64,
    #[serde(deserialize_with = "nz")]
    pub success: bool,
    #[serde(deserialize_with = "nz")]
    pub generation: u64,
    /// The generation the Player runs after this request.
    #[serde(rename = "appliedGeneration", deserialize_with = "nz")]
    pub applied_generation: u64,
    #[serde(rename = "restartRequired", deserialize_with = "nz")]
    pub restart_required: bool,
    #[serde(deserialize_with = "nz")]
    pub error: String,
    #[serde(deserialize_with = "nz")]
    pub failures: Vec<AckFailure>,
    #[serde(rename = "detoursApplied", deserialize_with = "nz")]
    pub detours_applied: u64,
    #[serde(rename = "detoursRestored", deserialize_with = "nz")]
    pub detours_restored: u64,
    #[serde(rename = "callbacksInvoked", deserialize_with = "nz")]
    pub callbacks_invoked: u64,
    #[serde(rename = "callbackErrors", deserialize_with = "nz")]
    pub callback_errors: u64,
}
