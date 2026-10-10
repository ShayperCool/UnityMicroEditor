//! Compiled Shader snapshots guarded by their current source/include closure.
use super::{
    lexpath,
    native::{valid_native_hex, valid_native_id},
    session::{Dependency, Inner, path_error},
    shader_sources::{shader_source_path, validate_shader_source_requirements},
    types::{NativeReplacement, ShaderSourceRequirement},
    value::{self, Map, Value, array, get, object},
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashSet};

impl Inner {
    pub(crate) fn validate_compiled_shader_snapshot(
        &mut self,
        r: &NativeReplacement,
    ) -> Result<()> {
        // Existing experiment descriptors without a snapshot remain supported.
        let Some(p) = &r.compiled_shader_snapshot else {
            return Ok(());
        };
        if lexpath::ext(&r.source_path) != ".shader"
            || p.source_path != r.source_path
            || p.schema_version != 1
            || p.compiled_source_revision_verified
            || p.shader_name.is_empty()
            || p.member_name.is_empty()
            || p.member_name.len() > 4096
            || p.member_name.contains('\0')
            || !valid_native_id(&p.archive_file_id)
            || !valid_native_hex(&p.payload_sha256, 64)
            || !valid_native_hex(&p.type_tree_sha256, 64)
            || p.source_requirements.is_empty()
        {
            bail!("compiled Shader snapshot identity/provenance is unsupported");
        }
        for (path, sha) in [
            (&p.bundle_path, &p.bundle_sha256),
            (&p.member_path, &p.member_sha256),
        ] {
            if !lexpath::is_abs(path) || !valid_native_hex(sha, 64) {
                bail!("compiled Shader snapshot artifact identity is invalid");
            }
            let (_, actual) = self.read(path, false)?;
            if actual != *sha {
                bail!("compiled Shader snapshot artifact changed: {path}");
            }
        }
        let mut seen = HashSet::new();
        let mut own = false;
        for requirement in p.source_requirements.iter() {
            if !seen.insert(&requirement.path) {
                bail!("compiled Shader snapshot has duplicate source requirements");
            }
            let resolved = self.compiled_shader_source(requirement)?;
            let (_, sha) = self.read(&resolved, false)?;
            if sha != requirement.sha256 {
                bail!(
                    "compiled Shader snapshot source/include changed: {}",
                    requirement.path
                );
            }
            if requirement.path == r.source_path {
                if sha != r.source_sha256 {
                    bail!("compiled Shader snapshot source hash disagrees");
                }
                own = true;
            }
        }
        if !own {
            bail!("compiled Shader snapshot has no exact authored source guard");
        }
        validate_shader_source_requirements(
            &self.project,
            &r.source_path,
            &p.source_requirements,
            &p.missing_includes,
        )
    }

    /// Raw authored source stays confined to the project. Package requirements
    /// also admit this project's selected PackageCache source, rather than
    /// silently pinning an old importer project's package copy.
    fn compiled_shader_source(&self, requirement: &ShaderSourceRequirement) -> Result<String> {
        let logical = requirement.path.as_str();
        if !valid_native_hex(&requirement.sha256, 64)
            || !lexpath::is_abs(&requirement.source_path)
            || lexpath::clean(logical) != logical
            || (!logical.starts_with("Assets/") && !logical.starts_with("Packages/"))
        {
            bail!("compiled Shader snapshot source requirement is invalid: {logical}");
        }
        let canonical = shader_source_path(&self.project, logical)?;
        let expected = lexpath::eval_symlinks(&requirement.source_path)
            .map_err(|e| path_error(&requirement.source_path, e))
            .and_then(|resolved| lexpath::abs(&resolved))?;
        if !lexpath::rel(&self.project, &canonical).is_ok_and(|r| lexpath::inside(&r))
            || canonical != expected
        {
            bail!(
                "compiled Shader source requirement is not its current project source: {logical}"
            );
        }
        Ok(canonical)
    }

    pub(crate) fn publish_compiled_shader_snapshot(
        &mut self,
        r: &NativeReplacement,
        over: &mut Map,
    ) -> Result<()> {
        let Some(p) = &r.compiled_shader_snapshot else {
            return Ok(());
        };
        self.validate_compiled_shader_snapshot(r)?;
        for requirement in p.source_requirements.iter() {
            let physical = self.compiled_shader_source(requirement)?;
            let (data, sha) = self.read(&physical, false)?;
            self.dependencies.insert(
                requirement.path.clone(),
                Dependency {
                    path: requirement.path.clone(),
                    sha256: sha,
                    bytes: data.map_or(0, |d| d.len() as i64),
                    source_path: physical,
                },
            );
        }
        for (path, sha) in [
            (&p.bundle_path, &p.bundle_sha256),
            (&p.member_path, &p.member_sha256),
        ] {
            let (data, _) = self.read(path, false)?;
            self.dependencies.insert(
                path.clone(),
                Dependency {
                    path: path.clone(),
                    sha256: sha.clone(),
                    bytes: data.map_or(0, |d| d.len() as i64),
                    source_path: path.clone(),
                },
            );
        }
        // Preserve the explicit provenance limitation in the reviewable manifest.
        let existing = over
            .get("compiledShaderSnapshots")
            .unwrap_or_else(|| get(&self.baseline, "compiledShaderSnapshots"));
        let mut proofs: BTreeMap<String, Value> = array(existing)
            .iter()
            .map(|entry| {
                let fields = object(entry);
                (
                    value::text(fields, "guid").into_owned(),
                    Value::Object(fields.clone()),
                )
            })
            .collect();
        let mut proof = match value::parse_use_number(&value::encode_json(p)?)? {
            Value::Object(fields) => fields,
            _ => Map::new(),
        };
        proof.insert("guid".into(), r.guid.as_str().into());
        proofs.insert(r.guid.clone(), Value::Object(proof));
        over.insert(
            "compiledShaderSnapshots".into(),
            Value::Array(proofs.into_values().collect()),
        );
        Ok(())
    }
}
