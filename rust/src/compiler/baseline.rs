//! Exporting the accepted graph as a new baseline.
use super::{
    CompileAssembly, CompileManifest, Compiler,
    compile::{lifecycle_sources_match, valid_lifecycle_proof},
    join,
    netcode::needs_netcode,
    prepare::{canonical_configuration, descriptor},
};
use crate::{
    cancel::Context,
    sha256,
    util::{agent_immutable_json, file_hash, marshal},
};
use anyhow::{Result, bail};

/// JSON of the `{Mode, Configuration, Assembly}` identity, in that field order.
fn assembly_identity(configuration: &[u8], assembly: &CompileAssembly) -> Result<Vec<u8>> {
    let mut identity = br#"{"Mode":"prepared-compiler-assembly-v1","Configuration":"#.to_vec();
    identity.extend_from_slice(configuration);
    identity.extend_from_slice(br#","Assembly":"#);
    identity.extend_from_slice(&marshal(assembly)?);
    identity.push(b'}');
    Ok(identity)
}

impl Compiler {
    /// `PreparedManifest` binds the accepted DLLs, sources and proofs into a new
    /// restartable baseline. A compilation can replace DLLs even with unchanged
    /// sources, for example when upgrading an old NGO postprocessing baseline.
    pub fn prepared_manifest(&mut self, ctx: &Context) -> Result<CompileManifest> {
        if !self.startup_complete {
            bail!("compiler graph must be accepted before exporting a prepared baseline");
        }
        ctx.err()?;
        self.check_configuration(&[])?;
        let mut result = self.manifest.clone();
        for (source, expected) in &self.sources {
            if !file_hash(source).is_ok_and(|actual| actual == *expected) {
                bail!("source changed before prepared compiler publication: {source}");
            }
            result.compiler_inputs.insert(source, expected.clone());
        }
        for (path, hash) in &self.configuration {
            result.compiler_inputs.insert(path, hash.clone());
        }
        for index in 0..result.assemblies.len() {
            let mut assembly = result.assemblies[index].clone();
            assembly.path = self.current_dlls.at(&assembly.name).to_string();
            if assembly.path.is_empty() {
                bail!("accepted compiler graph lacks DLL: {}", assembly.name);
            }
            assembly.sha256 = file_hash(&assembly.path)?;
            let accepted = self.accepted_dll_hashes.at(&assembly.name);
            if accepted.is_empty() || assembly.sha256 != accepted {
                bail!(
                    "accepted DLL changed before prepared compiler publication: {}",
                    assembly.name
                );
            }
            assembly.lifecycle_barrier = self.proofs.value(&assembly.name);
            if !valid_lifecycle_proof(&assembly.lifecycle_barrier, &assembly.name)
                || !lifecycle_sources_match(&assembly.lifecycle_barrier, &assembly.sources)
            {
                bail!(
                    "accepted compiler graph lacks lifecycle proof: {}",
                    assembly.name
                );
            }
            if needs_netcode(&assembly) && self.netcode_processed.on(&assembly.name) {
                assembly.netcode_postprocessor = self.prepare_netcode_processor(ctx)?.key;
            }
            let configuration = canonical_configuration(&self.configs.value(&assembly.name))?;
            assembly.assembly_input_hash = sha256(&assembly_identity(&configuration, &assembly)?);
            agent_immutable_json(
                &join(&[
                    &self.cache,
                    "baseline-assemblies",
                    &assembly.assembly_input_hash,
                    "compile.json",
                ]),
                &descriptor(&self.manifest, &configuration, &assembly)?,
            )?;
            result.assemblies[index] = assembly;
        }
        for index in 0..result.scripts.len() {
            if !result.scripts[index].host_provided {
                let script = &result.scripts[index];
                let name = self
                    .proofs
                    .get(&script.assembly)
                    .map(|proof| proof.script_types.at(&script.path).to_string())
                    .unwrap_or_default();
                result.scripts[index].type_name = name;
            }
        }
        self.check_configuration(&[])?;
        Ok(result)
    }
}
