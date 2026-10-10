//! `shader-extract`: keeps one compiled Player Shader from an extracted Linux
//! SerializedFile member. It never compiles content or starts Unity.
use super::{
    extract_player_shader, files, lower_hex,
    shader_sources::{
        CompiledShaderSnapshot, NativeContentFile, NativeContentObject, NativeReplacement,
        collect_shader_source_requirements,
    },
    verify_unityfs_member,
};
use crate::flags::FlagSet;
use anyhow::{Context, Result, bail};
use regex::bytes::Regex;
use serde::Deserialize;
use serde_json::json;
use std::io::Write;

#[derive(Default)]
struct Options {
    input: String,
    expected: String,
    guid: String,
    name: String,
    output: String,
    project: String,
    source: String,
    origin: String,
    environment: String,
}

fn null_default<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Default, Deserialize)]
struct OriginNode {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    size: i64,
    #[serde(default, deserialize_with = "null_default")]
    path: String,
    #[serde(default, deserialize_with = "null_default")]
    sha256: String,
}

#[derive(Default, Deserialize)]
struct BundleOrigin {
    #[serde(skip)]
    member_name: String,
    #[serde(default, deserialize_with = "null_default")]
    source: String,
    #[serde(default, deserialize_with = "null_default")]
    sha256: String,
    #[serde(default, deserialize_with = "null_default")]
    nodes: Vec<OriginNode>,
}

fn temp(project: &str) -> String {
    files::normalize(&format!("{project}/Temp"))
}

fn verified_origin(o: &Options, member: &[u8]) -> Result<BundleOrigin> {
    let data = files::read_bounded(&o.origin, 4 << 20)?;
    let origins: Option<Vec<BundleOrigin>> = serde_json::from_slice(&data)?;
    let mut selected: Option<(BundleOrigin, String)> = None;
    for origin in origins.unwrap_or_default() {
        let mut found = None;
        for node in &origin.nodes {
            if files::normalize(&node.path) != files::normalize(&o.input) {
                continue;
            }
            if selected.is_some()
                || found.is_some()
                || node.sha256 != o.expected
                || node.size != member.len() as i64
                || node.name.is_empty()
                || !origin.source.starts_with('/')
                || !lower_hex(&origin.sha256, 64)
            {
                bail!("compiled Shader extraction origin is ambiguous or inconsistent");
            }
            found = Some(node.name.clone());
        }
        if let Some(found) = found {
            selected = Some((origin, found));
        }
    }
    let Some((mut selected, found)) = selected else {
        bail!("compiled Shader extracted member is absent from origin manifest");
    };
    let archive = files::read_bounded(&selected.source, 128 << 20)?;
    if crate::sha256(&archive) != selected.sha256 {
        bail!("compiled Shader original UnityFS archive SHA256 differs");
    }
    verify_unityfs_member(&archive, &found, member)?;
    selected.member_name = found;
    Ok(selected)
}

/// The command body; `args` excludes the subcommand name.
pub fn shader_extract(args: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut flags = FlagSet::new("shader-extract");
    for (name, usage) in [
        ("input", "exact extracted Linux SerializedFile member"),
        ("input-sha256", "verified source member SHA256"),
        (
            "guid",
            "authored Shader source GUID embedded in Player object",
        ),
        (
            "shader-name",
            "exact Shader name, verified against the embedded source GUID",
        ),
        ("output", "immutable shader-only output beneath Temp"),
        (
            "project",
            "source project; required to produce a guarded cache replacement",
        ),
        ("source", "canonical authored Assets/...shader path"),
        (
            "origin",
            "extraction origin JSON with exact UnityFS archive/member identities",
        ),
        (
            "environment-sha256",
            "current native-environment fingerprint for cache replay",
        ),
    ] {
        flags.string(name, "", usage);
    }
    flags.parse(args.iter().cloned())?;
    let mut o = Options {
        input: flags.get_string("input"),
        expected: flags.get_string("input-sha256"),
        guid: flags.get_string("guid"),
        name: flags.get_string("shader-name"),
        output: flags.get_string("output"),
        project: flags.get_string("project"),
        source: flags.get_string("source"),
        origin: flags.get_string("origin"),
        environment: flags.get_string("environment-sha256"),
    };
    let absolute = |path: &str| path.starts_with('/');
    let output = files::normalize(&o.output);
    if !flags.args().is_empty()
        || !absolute(&o.input)
        || !absolute(&o.output)
        || !output.contains("/Temp/")
        || !lower_hex(&o.expected, 64)
    {
        bail!(
            "use absolute --input, --output beneath Temp, --input-sha256, --guid and --shader-name"
        );
    }
    let guarded = !o.project.is_empty()
        || !o.source.is_empty()
        || !o.origin.is_empty()
        || !o.environment.is_empty();
    if guarded
        && (!absolute(&o.project)
            || !absolute(&o.origin)
            || !lower_hex(&o.environment, 64)
            || o.source.is_empty())
    {
        bail!(
            "guarded cache output requires --project, --source, --origin and --environment-sha256"
        );
    }
    let data = files::read_bounded(&o.input, 64 << 20)?;
    if crate::sha256(&data) != o.expected {
        bail!("compiled Shader archive member SHA256 differs");
    }
    let (projected, proof) = extract_player_shader(&data, &o.guid, &o.name)?;
    let mut replacement = None;
    if guarded {
        o.project = files::canonical(&o.project)
            .with_context(|| format!("resolve project {}", o.project))?;
        if !files::within(&temp(&o.project), &output) {
            bail!("guarded Shader output must be inside this project's Temp directory");
        }
        let origin = verified_origin(&o, &data)?;
        let (requirements, missing) = collect_shader_source_requirements(&o.project, &o.source)?;
        let source_sha = requirements
            .iter()
            .rfind(|requirement| requirement.path == o.source)
            .map(|requirement| requirement.sha256.clone())
            .unwrap_or_default();
        let meta = files::read_bounded(&format!("{}/{}.meta", o.project, o.source), 4 << 20)?;
        let identities: Vec<_> = Regex::new(r"(?m-u)^guid: ([0-9a-f]{32})\s*$")?
            .captures_iter(&meta)
            .collect();
        if identities.len() != 1 || &identities[0][1] != proof.guid.as_bytes() {
            bail!("authored Shader .meta GUID differs from embedded Player identity");
        }
        replacement = Some(NativeReplacement {
            guid: proof.guid.clone(),
            source_path: o.source.clone(),
            source_sha256: source_sha,
            meta_sha256: crate::sha256(&meta),
            environment_sha256: o.environment.clone(),
            content: NativeContentFile {
                guid: proof.guid.clone(),
                path: output.clone(),
                sha256: proof.output_sha256.clone(),
                objects: vec![NativeContentObject {
                    file_id: proof.source_file_id.clone(),
                    type_name: "UnityEngine.Shader".into(),
                }],
                resources: Vec::new(),
                dependencies: Vec::new(),
            },
            compiled_shader_snapshot: Some(CompiledShaderSnapshot {
                schema_version: 1,
                compiled_source_revision_verified: false,
                shader_name: proof.name.clone(),
                source_path: o.source.clone(),
                archive_file_id: proof.archive_file_id.clone(),
                bundle_path: origin.source,
                bundle_sha256: origin.sha256,
                member_path: o.input.clone(),
                member_name: origin.member_name,
                member_sha256: o.expected.clone(),
                payload_sha256: proof.payload_sha256.clone(),
                type_tree_sha256: proof.type_tree_sha256.clone(),
                source_requirements: requirements,
                missing_includes: missing,
            }),
        });
    }
    let output_dir = files::parent(&output);
    files::create_private_dir(output_dir)?;
    let parent = files::canonical(output_dir)
        .with_context(|| format!("resolve output directory {output_dir}"))?;
    if parent != output_dir || guarded && !files::within(&temp(&o.project), &parent) {
        bail!("Shader output parent contains a symlink or escapes owned Temp");
    }
    let proof_path = format!("{}.proof.json", o.output);
    let replacement_path = format!("{}.replacement.json", o.output);
    let mut new_files = vec![
        (o.output.clone(), projected),
        (proof_path.clone(), files::json_document(&proof)?),
    ];
    if let Some(replacement) = &replacement {
        new_files.push((replacement_path.clone(), files::json_document(replacement)?));
    }
    if new_files
        .iter()
        .any(|(path, _)| std::fs::symlink_metadata(path).is_ok())
    {
        bail!("Shader output already exists; use a new revision path");
    }
    files::write_new_files(&new_files)?;
    let mut event = json!({
        "event": "compiled-shader-extracted",
        "path": o.output,
        "sha256": proof.output_sha256,
        "guid": proof.guid,
        "fileId": proof.source_file_id,
        "proof": proof_path,
    });
    if let Some(snapshot) = replacement.and_then(|replacement| replacement.compiled_shader_snapshot)
    {
        event["replacement"] = json!(replacement_path);
        event["compiledSourceRevisionVerified"] = json!(false);
        event["sourceRequirements"] = json!(snapshot.source_requirements.len());
        event["missingIncludes"] = json!(snapshot.missing_includes.len());
    }
    writeln!(stdout, "{event}")?;
    Ok(())
}

/// `unity-micro-editor shader-extract ...`
pub fn shader_extract_command(args: &[String]) -> Result<()> {
    shader_extract(args, &mut std::io::stdout().lock())
}
