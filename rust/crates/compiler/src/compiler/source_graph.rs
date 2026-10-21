//! The source graph functions the compiler pipeline itself calls.
//! The rest of the source graph is built on top of this module.
use super::{
    CompileAssembly, Compiler, CompilerGraphExport, NilMap, NilVec, join,
    nilable::unmarshal,
    sys::{self, compiler_command},
};
use crate::{
    cancel::Context,
    jsondoc, sysutil,
    util::{atomic_json, file_hash, read_agent_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};

pub use sys::managed_plugin;

pub(crate) type RawMap = BTreeMap<String, Box<RawValue>>;
/// JSON of a map of raw values: keys sorted, every raw value compacted and
/// HTML-escaped.
pub(crate) fn marshal_raw_map(map: &RawMap) -> Vec<u8> {
    let mut out = vec![b'{'];
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.extend_from_slice(jsondoc::quote(key).as_bytes());
        out.push(b':');
        out.extend_from_slice(&jsondoc::compact(value.get().as_bytes()));
    }
    out.push(b'}');
    out
}
pub(crate) fn raw_value(bytes: Vec<u8>) -> Result<Box<RawValue>> {
    Ok(RawValue::from_string(String::from_utf8(bytes)?)?)
}
/// Atomically writes a map of raw values as JSON.
pub(crate) fn write_raw_map(filename: &str, map: &RawMap) -> Result<()> {
    atomic_json(filename, &raw_value(marshal_raw_map(map))?)
}

#[derive(Serialize)]
struct DeclarationSource<'a> {
    path: &'a str,
    #[serde(rename = "originalPath")]
    original_path: &'a str,
}
#[derive(Serialize)]
struct DeclarationRequest<'a> {
    #[serde(rename = "assemblyName")]
    assembly_name: &'a str,
    defines: &'a NilVec<String>,
    #[serde(rename = "editorDefines")]
    editor_defines: &'a NilVec<String>,
    sources: NilVec<DeclarationSource<'a>>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Declarations {
    player: NilMap<NilVec<String>>,
}

impl Compiler {
    /// Native MonoScript identities come from
    /// active Player declarations and exact native DLL metadata. No target
    /// Editor DLL or GetClass observation is needed.
    pub fn derive_source_host_script_types(
        &mut self,
        ctx: &Context,
        helper: &str,
        graph: &CompilerGraphExport,
        managed: &str,
    ) -> Result<()> {
        let mut requests = Vec::new();
        let mut paths = Vec::new();
        let mut known = BTreeSet::new();
        for assembly in &graph.host_assemblies {
            known.insert(assembly.name.as_str());
            let mut entry = DeclarationRequest {
                assembly_name: &assembly.name,
                defines: &assembly.defines,
                editor_defines: &assembly.defines,
                sources: NilVec::nil(),
            };
            for path in &assembly.sources {
                let expected = graph.compiler_inputs.at(path);
                if expected.is_empty() || !file_hash(path).is_ok_and(|hash| hash == expected) {
                    bail!("native source declaration differs from source graph: {path}");
                }
                entry.sources.push(DeclarationSource {
                    path,
                    original_path: path,
                });
            }
            requests.push(entry);
            paths.push(join(&[managed, &format!("{}.dll", assembly.name)]));
        }
        if requests.is_empty() {
            return Ok(());
        }
        let request_path = join(&[&self.cache, "native-source-declarations.json"]);
        atomic_json(&request_path, &requests)?;
        // What the helper reads: the request, the sources it names, which
        // were checked against the graph above, and the helper itself.
        let mut identity = crate::util::marshal(&requests)?;
        for request in &requests {
            for source in request.sources.iter() {
                identity.extend_from_slice(graph.compiler_inputs.at(source.path).as_bytes());
            }
        }
        identity.extend_from_slice(file_hash(helper)?.as_bytes());
        let kept = join(&[
            &self.stored("source-declarations"),
            &format!("{}.json", crate::sha256(&identity)),
        ]);
        let output = match sys::read(&kept) {
            Ok(output) => output,
            Err(_) => {
                let output = compiler_command(
                    ctx,
                    &self.cache,
                    &self.dotnet,
                    &[helper, "--source-type-declarations", &request_path],
                )
                .map_err(|error| anyhow!("native Player source declarations: {error}"))?;
                sys::mkdir_all(&sysutil::dir(&kept))?;
                let partial = format!("{kept}.{}.tmp", crate::util::random_id());
                sys::write(&partial, &output)?;
                std::fs::rename(&partial, &kept).map_err(|e| sys::os_error("rename", &kept, &e))?;
                output
            }
        };
        let declarations = match unmarshal::<NilMap<Declarations>>(&output) {
            Ok(declarations) if declarations.len() == requests.len() => declarations,
            _ => bail!("native Player source declaration proof is incomplete"),
        };
        let listed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let definitions = match self.type_definitions(ctx, helper, &listed)? {
            definitions if definitions.len() == paths.len() => definitions,
            _ => bail!("native DLL type inventory proof is incomplete"),
        };
        let scripts = self.manifest.scripts.to_vec();
        self.manifest.scripts.truncate();
        for mut script in scripts {
            if !script.host_provided {
                self.manifest.scripts.push(script);
                continue;
            }
            // An assembly outside the graph has the zero name.
            let assembly = if known.contains(script.assembly.as_str()) {
                script.assembly.as_str()
            } else {
                ""
            };
            let Some(names) = declarations
                .get(&script.assembly)
                .and_then(|proof| proof.player.get(&script.path))
            else {
                bail!("native source declaration is absent: {}", script.path);
            };
            let file = sysutil::base(&script.path);
            let base = file
                .strip_suffix(sysutil::ext(&script.path))
                .unwrap_or(&file);
            let mut candidate = "";
            for name in names {
                if name.contains(['+', '`'])
                    || &name[name.rfind('.').map_or(0, |index| index + 1)..] != base
                {
                    continue;
                }
                if !candidate.is_empty() {
                    bail!(
                        "native primary class declaration is ambiguous: {}",
                        script.path
                    );
                }
                candidate = name;
            }
            if candidate.is_empty() {
                continue;
            }
            let dll = join(&[managed, &format!("{assembly}.dll")]);
            if !definitions
                .get(&dll)
                .is_some_and(|names| names.iter().any(|name| name == candidate))
            {
                bail!("verified native profile lacks active source type: {candidate}");
            }
            script.type_name = candidate.to_string();
            self.manifest.scripts.push(script);
        }
        Ok(())
    }
}

/// The pinned public BuildPipeline writes
/// this registration table. SDK source overlays use verified public wrapper
/// methods at raw-loader lifecycle phases; retaining their old native rows
/// would run original methods twice.
pub fn remove_source_native_initializers(
    filename: &str,
    assemblies: &[CompileAssembly],
) -> Result<()> {
    let content = match read_agent_file(filename, 16 << 20) {
        Ok(content) => content,
        Err(error) if sys::is_not_exist(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut table: RawMap = unmarshal(&content)?;
    let rows = table
        .get("root")
        .and_then(|root| unmarshal::<Option<Vec<Option<RawMap>>>>(root.get().as_bytes()).ok())
        .ok_or_else(|| anyhow!("pinned native initializer table has unknown schema"))?;
    let names: BTreeSet<&str> = assemblies
        .iter()
        .map(|assembly| assembly.name.as_str())
        .collect();
    let mut kept = rows.is_some().then(Vec::new);
    for row in rows.unwrap_or_default() {
        let row = row.unwrap_or_default();
        let assembly = row
            .get("assemblyName")
            .and_then(|name| unmarshal::<String>(name.get().as_bytes()).ok())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow!("native initializer row lacks assembly identity"))?;
        if !names.contains(assembly.as_str())
            && let Some(kept) = &mut kept
        {
            kept.push(row);
        }
    }
    let mut encoded = Vec::new();
    match &kept {
        None => encoded.extend_from_slice(b"null"),
        Some(rows) => {
            encoded.push(b'[');
            for (index, row) in rows.iter().enumerate() {
                if index > 0 {
                    encoded.push(b',');
                }
                encoded.extend_from_slice(&marshal_raw_map(row));
            }
            encoded.push(b']');
        }
    }
    table.insert("root".into(), raw_value(encoded)?);
    write_raw_map(filename, &table)
}
