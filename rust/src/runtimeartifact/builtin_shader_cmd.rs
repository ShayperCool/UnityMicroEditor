//! `builtin-shader`: extracts a verified engine Shader from an existing
//! compiled Linux archive. It never compiles content, loads scenes, or starts
//! Unity.
use super::{
    BuiltinShaderIdentity, PlayerShaderMetadata, extract_builtin_player_shader, files,
    inspect_builtin_shader,
    shader_sources::{NativeContentFile, NativeContentObject, RetainedBuiltinShader},
    verify_unityfs_member,
};
use crate::flags::{FlagError, FlagSet};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;
use std::{fs, io::Read, io::Write};

#[derive(Debug, Clone, Default)]
pub struct BuiltinShaderOptions {
    pub project: String,
    pub input: String,
    pub input_sha: String,
    pub bundle: String,
    pub bundle_sha: String,
    pub member: String,
    pub builtin: String,
    pub output: String,
}

fn read_bounded(path: &str, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("open {path}"))?;
    let info = file.metadata().with_context(|| format!("inspect {path}"))?;
    if !info.is_file() || info.len() > limit {
        bail!("built-in Shader input is oversized or not a regular file");
    }
    let mut data = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut data)
        .with_context(|| format!("read {path}"))?;
    Ok(data)
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Proof<'a> {
    compiled: &'a PlayerShaderMetadata,
    native_source: &'a BuiltinShaderIdentity,
}

pub fn extract_builtin(opts: &BuiltinShaderOptions, output: &mut dyn Write) -> Result<()> {
    for path in [
        &opts.project,
        &opts.input,
        &opts.bundle,
        &opts.builtin,
        &opts.output,
    ] {
        if !files::is_normalized_absolute(path) {
            bail!("built-in Shader extraction requires canonical absolute paths");
        }
    }
    if !files::canonical(&opts.project).is_ok_and(|project| project == opts.project) {
        bail!("built-in Shader project cannot contain symlink parents");
    }
    let temp = files::normalize(&format!("{}/Temp", opts.project));
    if opts.output == temp || !files::within(&temp, &opts.output) {
        bail!("built-in Shader output must stay beneath project Temp");
    }
    let member = read_bounded(&opts.input, 64 << 20)?;
    if crate::sha256(&member) != opts.input_sha {
        bail!("built-in Shader member SHA256 differs");
    }
    let bundle = read_bounded(&opts.bundle, 128 << 20)?;
    if crate::sha256(&bundle) != opts.bundle_sha {
        bail!("built-in Shader archive SHA256 differs");
    }
    verify_unityfs_member(&bundle, &opts.member, &member)?;
    let (data, proof) = extract_builtin_player_shader(&member, 108)?;
    let engine = fs::File::open(&opts.builtin).with_context(|| format!("open {}", opts.builtin))?;
    let info = engine
        .metadata()
        .with_context(|| format!("inspect {}", opts.builtin))?;
    let identity = inspect_builtin_shader(&engine, info.len() as i64, 108)?;
    if identity.name != proof.name {
        bail!("native builtin108 name differs from compiled Shader");
    }
    let snapshot = RetainedBuiltinShader {
        schema_version: 1,
        unity_version: identity.unity_version.clone(),
        guid: proof.builtin_guid.clone(),
        file_id: proof.builtin_file_id.clone(),
        shader_name: proof.name.clone(),
        source_guid: proof.guid.clone(),
        source_file_id: proof.source_file_id.clone(),
        archive_file_id: proof.archive_file_id.clone(),
        bundle_path: opts.bundle.clone(),
        bundle_sha256: opts.bundle_sha.clone(),
        member_path: opts.input.clone(),
        member_name: opts.member.clone(),
        member_sha256: opts.input_sha.clone(),
        payload_sha256: proof.payload_sha256.clone(),
        type_tree_sha256: proof.type_tree_sha256.clone(),
        builtin_source_path: opts.builtin.clone(),
        builtin_source_sha256: identity.payload_sha256.clone(),
        compiled_source_revision_verified: false,
        content: NativeContentFile {
            guid: proof.builtin_guid.clone(),
            path: opts.output.clone(),
            sha256: proof.output_sha256.clone(),
            objects: vec![NativeContentObject {
                file_id: proof.builtin_file_id.clone(),
                type_name: "UnityEngine.Shader".into(),
            }],
            resources: Vec::new(),
            dependencies: Vec::new(),
        },
    };
    snapshot.validate(&identity.unity_version)?;
    let output_dir = files::parent(&opts.output);
    files::create_private_dir(output_dir)?;
    if !files::canonical(output_dir).is_ok_and(|parent| parent == output_dir) {
        bail!("built-in Shader output parent is symlinked");
    }
    let snapshot_path = format!("{}.snapshot.json", opts.output);
    let bytes = data.len();
    files::write_new_files(&[
        (opts.output.clone(), data),
        (snapshot_path.clone(), files::json_document(&snapshot)?),
        (
            format!("{}.proof.json", opts.output),
            files::json_document(&Proof {
                compiled: &proof,
                native_source: &identity,
            })?,
        ),
    ])?;
    let event = json!({
        "event": "builtin-shader-extracted",
        "guid": proof.builtin_guid,
        "fileId": proof.builtin_file_id,
        "shaderName": proof.name,
        "bytes": bytes,
        "snapshot": snapshot_path,
        "sha256": proof.output_sha256,
    });
    writeln!(output, "{event}")?;
    Ok(())
}

/// `unity-micro-editor builtin-shader ...`: a bad flag exits 2, `-h` exits 0,
/// extra arguments are ignored.
pub fn builtin_shader_command(args: &[String]) -> Result<()> {
    let mut flags = FlagSet::new("builtin-shader");
    for (name, usage) in [
        ("project", "source project root; output stays in its Temp"),
        ("input", "exact extracted native SerializedFile member"),
        ("input-sha256", "verified native member SHA256"),
        ("bundle", "original compiled Linux UnityFS archive"),
        ("bundle-sha256", "verified original archive SHA256"),
        ("member-name", "exact archive member name"),
        (
            "builtin-source",
            "matching installation Data/Resources/unity_builtin_extra",
        ),
        (
            "output",
            "new immutable shader-only runtime file beneath project Temp",
        ),
    ] {
        flags.string(name, "", usage);
    }
    match flags.parse(args.iter().cloned()) {
        Ok(()) => {}
        Err(FlagError::Help) => std::process::exit(0),
        Err(FlagError::Invalid(_)) => std::process::exit(2),
    }
    let opts = BuiltinShaderOptions {
        project: flags.get_string("project"),
        input: flags.get_string("input"),
        input_sha: flags.get_string("input-sha256"),
        bundle: flags.get_string("bundle"),
        bundle_sha: flags.get_string("bundle-sha256"),
        member: flags.get_string("member-name"),
        builtin: flags.get_string("builtin-source"),
        output: flags.get_string("output"),
    };
    extract_builtin(&opts, &mut std::io::stdout().lock())
}
