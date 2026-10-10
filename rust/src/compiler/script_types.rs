//! The MonoScript GUID to managed type inventory, read from DLL metadata and cached by
//! exact inputs.
use super::{
    CompileScript, Compiler, LifecycleProof, NilMap, NilVec,
    compile::verify_cache_files,
    join,
    nilable::{nz, unmarshal},
    player_managed,
    sys::{self, compiler_command},
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct ScriptTypeCache {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    schema_version: i64,
    #[serde(deserialize_with = "nz")]
    key: String,
    #[serde(rename = "typesSHA256", deserialize_with = "nz")]
    types_sha256: String,
}
/// Keys are sorted.
#[derive(Serialize)]
struct CacheIdentity<'a> {
    assemblies: &'a NilMap<String>,
    #[serde(rename = "expectedTypes")]
    expected_types: &'a NilMap<NilMap<String>>,
    helper: &'a NilMap<String>,
    mode: &'static str,
    proofs: &'a NilMap<LifecycleProof>,
    scripts: &'a NilVec<CompileScript>,
}

impl Compiler {
    /// Reads type metadata without loading game code or the
    /// Editor. The prepared compiler graph ties each MonoScript GUID to its
    /// defining DLL.
    pub fn script_type_names(&mut self, ctx: &Context) -> Result<NilMap<String>> {
        let helper = self.prepare_lifecycle_helper(ctx)?;
        let managed = player_managed(&self.player);
        let mut by_assembly: BTreeMap<String, String> = BTreeMap::new();
        let mut seen = std::collections::BTreeSet::new();
        for script in &self.manifest.scripts {
            if script.guid.is_empty()
                || script.assembly.is_empty()
                || !seen.insert(script.assembly.clone())
            {
                continue;
            }
            let mut path = self.current_dlls.at(&script.assembly).to_string();
            if path.is_empty() {
                path = join(&[&managed, &format!("{}.dll", script.assembly)]);
            }
            if file_hash(&path).is_ok() {
                by_assembly.insert(script.assembly.clone(), path);
            }
        }
        let mut paths: Vec<&str> = by_assembly.values().map(String::as_str).collect();
        paths.sort_unstable();
        if paths.is_empty() {
            if self.manifest.native_only && self.manifest.scripts.is_empty() {
                return Ok(NilMap::make());
            }
            bail!("script type inventory has no defining assemblies");
        }
        let cache_key = self.script_type_cache_key(&helper, &by_assembly)?;
        if let Some(result) = self.read_script_type_cache(&cache_key) {
            return Ok(result);
        }
        let mut arguments = vec![helper.as_str(), "--type-definitions"];
        arguments.extend(&paths);
        let output = compiler_command(ctx, &self.cache, &self.dotnet, &arguments)
            .map_err(|error| anyhow!("read managed script types: {error}"))?;
        let definitions = match unmarshal::<NilMap<NilVec<String>>>(&output) {
            Ok(definitions) if definitions.len() == paths.len() => definitions,
            _ => bail!("SDK type inventory lacks exact assembly inputs"),
        };
        let mut result = NilMap::make();
        for script in &self.manifest.scripts {
            let Some(path) = by_assembly.get(&script.assembly) else {
                continue;
            };
            if script.guid.is_empty() {
                continue;
            }
            let names: &[String] = definitions.get(path).map_or(&[], |names| names);
            let file = sysutil::base(&script.path);
            let name = file
                .strip_suffix(sysutil::ext(&script.path))
                .unwrap_or(&file);
            let candidates: Vec<&String> = names
                .iter()
                .filter(|full| {
                    !full.contains('+')
                        && !full.contains('`')
                        && &full[full.rfind('.').map_or(0, |index| index + 1)..] == name
                })
                .collect();
            if let [only] = candidates.as_slice() {
                result.insert(&script.guid, (*only).clone());
            }
            if script.host_provided
                && !script.type_name.is_empty()
                && names.contains(&script.type_name)
            {
                result.insert(&script.guid, script.type_name.clone());
            }
            if let Some(primary) = self
                .proofs
                .get(&script.assembly)
                .and_then(|proof| proof.script_types.get(&script.path))
                && !primary.is_empty()
                && names.contains(primary)
            {
                result.insert(&script.guid, primary.clone());
            }
            // With several expected names for one GUID the last in name order wins.
            if let Some(expected) = self.expected_types.get(&script.assembly) {
                for (expected, guid) in expected {
                    if *guid == script.guid && names.contains(expected) {
                        result.insert(guid, expected.clone());
                    }
                }
            }
        }
        self.save_script_type_cache(&cache_key, &result)?;
        Ok(result)
    }

    pub fn script_type_cache_key(
        &self,
        helper: &str,
        assemblies: &BTreeMap<String, String>,
    ) -> Result<String> {
        let mut inputs = NilMap::make();
        for (name, path) in assemblies {
            inputs.insert(name, file_hash(path)?);
        }
        let directory = sysutil::dir(helper);
        let helper_files = read_json_file::<NilMap<String>>(&join(&[&directory, "files.json"]))
            .ok()
            .filter(|files| {
                !files.at("LifecycleCompiler.dll").is_empty()
                    && verify_cache_files(&directory, files)
            })
            .ok_or_else(|| anyhow!("script inventory SDK helper payload is not verified"))?;
        let encoded = marshal(&CacheIdentity {
            assemblies: &inputs,
            expected_types: &self.expected_types,
            helper: &helper_files,
            mode: "script-types-v2",
            proofs: &self.proofs,
            scripts: &self.manifest.scripts,
        })?;
        Ok(sha256(&encoded))
    }

    pub fn read_script_type_cache(&self, key: &str) -> Option<NilMap<String>> {
        let directory = join(&[&self.cache, "script-types", key]);
        if sysutil::eval_symlinks(&directory).ok()? != directory {
            return None;
        }
        let entry =
            read_json_file::<Option<ScriptTypeCache>>(&join(&[&directory, "artifact.json"]))
                .ok()?
                .unwrap_or_default();
        if entry.schema_version != 1 || entry.key != key || entry.types_sha256.is_empty() {
            return None;
        }
        let filename = join(&[&directory, "types.json"]);
        let info = fs::symlink_metadata(&filename).ok()?;
        if !info.is_file() || info.len() > 2 * 1024 * 1024 {
            return None;
        }
        let data = sys::read(&filename).ok()?;
        if sha256(&data) != entry.types_sha256 {
            return None;
        }
        let result = unmarshal::<NilMap<String>>(&data).ok()?;
        if result.is_nil() {
            return None;
        }
        let known = |guid: &str| {
            !guid.is_empty()
                && self
                    .manifest
                    .scripts
                    .iter()
                    .any(|script| script.guid == guid)
        };
        result
            .iter()
            .all(|(guid, name)| {
                known(guid)
                    && !name.is_empty()
                    && name.len() <= 1024
                    && !name.contains(['\0', '\r', '\n'])
            })
            .then_some(result)
    }

    pub fn save_script_type_cache(&self, key: &str, types: &NilMap<String>) -> Result<()> {
        let directory = join(&[&self.cache, "script-types", key]);
        sys::mkdir_all(&directory)?;
        let filename = join(&[&directory, "types.json"]);
        atomic_json(&filename, types)?;
        atomic_json(
            &join(&[&directory, "artifact.json"]),
            &ScriptTypeCache {
                schema_version: 1,
                key: key.into(),
                types_sha256: file_hash(&filename)?,
            },
        )
    }
}
