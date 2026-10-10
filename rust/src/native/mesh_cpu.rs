//! Retained CPU flags are a derived runtime preparation, not a new asset
//! import. Existing verified native cache files are upgraded without Unity.
use super::{
    NativeOptions,
    support::{create_new, eval_symlinks, mkdir_all, os_error, read_file},
};
use crate::{
    project::{List, NativeMeshCPUProjection, NativeMeshCPURetention, NativeReplacement},
    runtimeartifact::{self, MeshCpuProjection},
    sha256, sysutil,
    util::file_hash,
};
use anyhow::{Result, bail};
use std::io::Write;

pub(crate) fn retain_native_model_mesh_cpu(
    opts: &NativeOptions,
    mut entry: NativeReplacement,
) -> Result<NativeReplacement> {
    if !sysutil::ext(&entry.source_path).eq_ignore_ascii_case(".fbx") {
        return Ok(entry);
    }
    let (mut source_path, mut source_sha) =
        (entry.content.path.clone(), entry.content.sha256.clone());
    if let Some(projection) = &entry.mesh_cpu_projection {
        if projection.schema_version != 1 || projection.mode != "native-model-mesh-cpu-v1" {
            bail!("unsupported native Mesh CPU projection");
        }
        source_path = projection.source_path.clone();
        source_sha = projection.source_sha256.clone();
    }
    let data = match read_owned_native_mesh_file(&opts.project, &source_path) {
        Ok(data) if sha256(&data) == source_sha => data,
        _ => bail!("native Mesh CPU source payload changed: {source_path}"),
    };
    let (output, proof) = runtimeartifact::retain_model_mesh_cpu(&data)
        .map_err(|e| super::support::wrap(&format!("native Mesh CPU {}", entry.source_path), e))?;
    let retained: List<NativeMeshCPURetention> = proof
        .meshes
        .iter()
        .map(|mesh| NativeMeshCPURetention {
            file_id: mesh.file_id.clone(),
            original_is_readable: mesh.original_is_readable,
        })
        .collect();
    if let Some(projection) = &entry.mesh_cpu_projection {
        // Absent and empty retention lists differ.
        match read_owned_native_mesh_file(&opts.project, &entry.content.path) {
            Ok(actual)
                if sha256(&actual) == entry.content.sha256
                    && output == actual
                    && retained == entry.content.mesh_cpu_retention => {}
            _ => bail!(
                "native Mesh CPU projected payload/retention identities differ from original imported geometry"
            ),
        }
        let proof_bytes = match read_owned_native_mesh_file(&opts.project, &projection.proof_path) {
            Ok(bytes) if sha256(&bytes) == projection.proof_sha256 => bytes,
            _ => bail!("native Mesh CPU projection proof changed"),
        };
        if !serde_json::from_slice::<MeshCpuProjection>(&proof_bytes).is_ok_and(|c| c == proof) {
            bail!("native Mesh CPU projection proof no longer matches exact input/output");
        }
        return Ok(entry);
    }
    if !entry.content.mesh_cpu_retention.is_empty() {
        bail!("native Mesh CPU retention has no geometry projection provenance");
    }
    if retained.is_empty() {
        return Ok(entry);
    }
    let key = sha256(format!("native-model-mesh-cpu-v1\0{}\0{source_sha}", entry.guid).as_bytes());
    let filename = sysutil::join(&[&opts.output, "native/mesh-cpu", &format!("{key}.bin")]);
    mkdir_all(&sysutil::dir(&filename), 0o700)?;
    write_immutable_native_mesh_file(&filename, &output)?;
    let proof_bytes = serde_json::to_vec(&proof)?;
    let proof_path = format!("{filename}.projection.json");
    write_immutable_native_mesh_file(&proof_path, &proof_bytes)?;
    entry.content.path = filename;
    entry.content.sha256 = proof.output_sha256.clone();
    entry.content.mesh_cpu_retention = retained;
    entry.mesh_cpu_projection = Some(NativeMeshCPUProjection {
        schema_version: 1,
        mode: proof.mode.clone(),
        source_path,
        source_sha256: source_sha,
        proof_path,
        proof_sha256: sha256(&proof_bytes),
    });
    Ok(entry)
}

pub(crate) fn read_owned_native_mesh_file(project: &str, path: &str) -> Result<Vec<u8>> {
    if !sysutil::is_abs(path)
        || sysutil::clean(path) != path
        || !sysutil::is_owned_cache_path(project, path)
    {
        bail!(
            "native Mesh CPU file must be owned beneath project Temp or Library/UnityMicroEditor"
        );
    }
    if !eval_symlinks(path).is_ok_and(|canonical| canonical == path) {
        bail!("native Mesh CPU file cannot contain symlinks");
    }
    if !sysutil::stat(path).is_ok_and(|info| info.is_regular() && info.size <= 64 << 20) {
        bail!("native Mesh CPU file is not a bounded regular file");
    }
    read_file(path)
}

pub(crate) fn write_immutable_native_mesh_file(path: &str, data: &[u8]) -> Result<()> {
    let mut file = match create_new(path, 0o600) {
        Ok(file) => file,
        Err(error) if super::support::kind_of(&error) == sysutil::ErrorKind::Exist => {
            if !sysutil::lstat(path).is_ok_and(|info| info.is_regular()) {
                bail!("native Mesh CPU immutable output is symlinked or not regular");
            }
            if !file_hash(path).is_ok_and(|actual| actual == sha256(data)) {
                bail!("native Mesh CPU immutable output has conflicting bytes");
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    file.write_all(data)
        .map_err(|e| os_error("write", path, &e))
}
