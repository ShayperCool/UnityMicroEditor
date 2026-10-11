//! The serialized fields of state machine behaviour scripts, read from DLL
//! metadata and cached by exact inputs.
use super::{
    Compiler, NilMap,
    compile::verify_cache_files,
    join,
    nilable::nz,
    player_managed,
    sys::{self, compiler_command},
};
use crate::{
    cancel::Context,
    playerbake::ScriptLayout,
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize)]
struct Script<'a> {
    guid: &'a str,
    assembly: &'a str,
    #[serde(rename = "type")]
    type_name: &'a str,
}

#[derive(Serialize)]
struct Request<'a> {
    assemblies: &'a [String],
    scripts: &'a [Script<'a>],
}

/// Keys are sorted.
#[derive(Serialize)]
struct CacheIdentity<'a> {
    /// Content hash of every assembly, in the order the helper reads them.
    assemblies: &'a [(String, String)],
    helper: &'a NilMap<String>,
    mode: &'static str,
    scripts: &'a [Script<'a>],
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct LayoutCache {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    schema_version: i64,
    #[serde(deserialize_with = "nz")]
    key: String,
    #[serde(rename = "layoutsSHA256", deserialize_with = "nz")]
    layouts_sha256: String,
}

impl Compiler {
    /// The fields the engine serializes of every script that is a state
    /// machine behaviour, by script GUID. Like the type inventory this reads
    /// metadata only: neither game code nor the Editor is loaded. A class the
    /// reader cannot lay out the way the engine does comes back marked as
    /// unsupported.
    pub fn behaviour_layouts(
        &mut self,
        ctx: &Context,
        script_types: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, ScriptLayout>> {
        let helper = self.prepare_lifecycle_helper(ctx)?;
        let managed = player_managed(&self.player);
        let mut defining: BTreeMap<&str, String> = BTreeMap::new();
        let mut scripts = Vec::new();
        for script in &self.manifest.scripts {
            let Some(type_name) = script_types.get(&script.guid) else {
                continue;
            };
            if script.guid.is_empty() || script.assembly.is_empty() {
                continue;
            }
            if !defining.contains_key(script.assembly.as_str()) {
                let mut path = self.current_dlls.at(&script.assembly).to_string();
                if path.is_empty() {
                    path = join(&[&managed, &format!("{}.dll", script.assembly)]);
                }
                if file_hash(&path).is_err() {
                    continue;
                }
                defining.insert(&script.assembly, path);
            }
            scripts.push(Script {
                guid: &script.guid,
                assembly: &script.assembly,
                type_name,
            });
        }
        if scripts.is_empty() {
            return Ok(BTreeMap::new());
        }
        // The compiled assemblies come first: of two files of one assembly
        // name the helper reads the first. The Player's own directory follows
        // for the engine and base library types the fields refer to.
        let mut assemblies: Vec<String> = defining.values().cloned().collect();
        let mut provided = sys::glob_lossy(&join(&[&managed, "*.dll"]));
        provided.sort();
        for path in provided {
            let file = sysutil::base(&path);
            let name = file.strip_suffix(".dll").unwrap_or(&file);
            if !defining.contains_key(name) {
                assemblies.push(path);
            }
        }
        let key = self.behaviour_layout_cache_key(&helper, &assemblies, &scripts)?;
        let directory = join(&[&self.cache, "behaviour-layouts", &key]);
        if let Some(layouts) = read_layout_cache(&directory, &key) {
            return Ok(layouts);
        }
        sys::mkdir_all(&directory)?;
        let request = join(&[&directory, "request.json"]);
        atomic_json(
            &request,
            &Request {
                assemblies: &assemblies,
                scripts: &scripts,
            },
        )?;
        let output = compiler_command(
            ctx,
            &self.cache,
            &self.dotnet,
            &[&helper, "--behaviour-layouts", &request],
        )
        .map_err(|error| anyhow!("read behaviour script layouts: {error}"))?;
        let Ok(layouts) = serde_json::from_slice::<BTreeMap<String, ScriptLayout>>(&output) else {
            bail!("SDK helper returned no behaviour script layouts");
        };
        if layouts
            .keys()
            .any(|guid| !scripts.iter().any(|script| script.guid == guid))
        {
            bail!("SDK helper laid out a script it was not asked for");
        }
        let filename = join(&[&directory, "layouts.json"]);
        atomic_json(&filename, &layouts)?;
        atomic_json(
            &join(&[&directory, "artifact.json"]),
            &LayoutCache {
                schema_version: 1,
                key,
                layouts_sha256: file_hash(&filename)?,
            },
        )?;
        Ok(layouts)
    }

    fn behaviour_layout_cache_key(
        &self,
        helper: &str,
        assemblies: &[String],
        scripts: &[Script<'_>],
    ) -> Result<String> {
        let mut inputs = Vec::with_capacity(assemblies.len());
        for path in assemblies {
            inputs.push((sysutil::base(path), file_hash(path)?));
        }
        let directory = sysutil::dir(helper);
        let helper_files = read_json_file::<NilMap<String>>(&join(&[&directory, "files.json"]))
            .ok()
            .filter(|files| {
                !files.at("LifecycleCompiler.dll").is_empty()
                    && verify_cache_files(&directory, files)
            })
            .ok_or_else(|| anyhow!("behaviour layout SDK helper payload is not verified"))?;
        let encoded = marshal(&CacheIdentity {
            assemblies: &inputs,
            helper: &helper_files,
            mode: "behaviour-layouts-v1",
            scripts,
        })?;
        Ok(sha256(&encoded))
    }
}

fn read_layout_cache(directory: &str, key: &str) -> Option<BTreeMap<String, ScriptLayout>> {
    if sysutil::eval_symlinks(directory).ok()? != directory {
        return None;
    }
    let entry = read_json_file::<Option<LayoutCache>>(&join(&[directory, "artifact.json"]))
        .ok()?
        .unwrap_or_default();
    if entry.schema_version != 1 || entry.key != key || entry.layouts_sha256.is_empty() {
        return None;
    }
    let data = sys::read(&join(&[directory, "layouts.json"])).ok()?;
    if sha256(&data) != entry.layouts_sha256 {
        return None;
    }
    serde_json::from_slice(&data).ok()
}
