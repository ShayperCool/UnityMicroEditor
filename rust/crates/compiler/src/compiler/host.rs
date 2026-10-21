//! Staging the native host Player with a freshly compiled bridge.
use super::{
    NilVec, join, player_data,
    sys::{self, Spawn, Walk, combined_output},
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{atomic_json, file_hash, micro_editor_root, random_id, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

fn plain(ctx: &Context, executable: &str, arguments: &[&str]) -> (Vec<u8>, Option<String>) {
    combined_output(
        ctx,
        &Spawn {
            directory: "",
            executable,
            arguments,
            group: false,
            wait_delay: None,
            environment: &[],
        },
    )
}

/// Refreshes our managed bridge with the SDK compiler. The native
/// Player and its modules remain the already prepared, matching Unity runtime.
pub fn stage_host(
    ctx: &Context,
    output: &str,
    editor: &str,
    baseline_player: &str,
) -> Result<String> {
    // The bridge is compiled against the Player's own assemblies, which the
    // staged copy holds unchanged.
    let dll = compile_bridge(
        ctx,
        output,
        editor,
        &join(&[&player_data(baseline_player), "Managed"]),
    )?;
    let bridge = file_hash(&dll)?;
    // A prepared Player is never changed, so the same Player with the same
    // bridge is staged once per output and keeps its paths: a start whose
    // sources did not change then publishes the scene it published before.
    let key = sha256(format!("staged-host-v1\0{baseline_player}\0{bridge}").as_bytes());
    let destination = join(&[output, "players", &format!("staged-{}", &key[..32])]);
    let staged = |root: &str| {
        (
            join(&[root, &sysutil::base(baseline_player)]),
            join(&[
                root,
                &sysutil::base(&player_data(baseline_player)),
                "Managed",
                "UnityMicroEditor.dll",
            ]),
        )
    };
    let (player, installed) = staged(&destination);
    if sysutil::lstat(&player).is_ok_and(|info| info.is_regular())
        && file_hash(&installed).is_ok_and(|sha| sha == bridge)
    {
        return Ok(player);
    }
    sys::mkdir_all(&sysutil::dir(&destination))?;
    let partial = join(&[output, "players", &format!(".staging-{}", random_id())]);
    let (copied, error) = plain(
        ctx,
        "cp",
        &[
            "--reflink=auto",
            "-a",
            "--",
            &sysutil::dir(baseline_player),
            &partial,
        ],
    );
    if let Some(error) = error {
        let _ = std::fs::remove_dir_all(&partial);
        bail!(
            "stage native host: {error}: {}",
            String::from_utf8_lossy(&copied)
        );
    }
    let (_, partial_bridge) = staged(&partial);
    sys::write(&partial_bridge, &sys::read(&dll)?)?;
    // A copy left by an interrupted start is replaced whole.
    let _ = std::fs::remove_dir_all(&destination);
    std::fs::rename(&partial, &destination)
        .map_err(|e| sys::os_error("rename", &destination, &e))?;
    Ok(player)
}

/// Compiles the tool's runtime bridge from the installation's sources with
/// the SDK compiler against the Player's `managed` assemblies, and returns
/// the DLL, kept under `cache_root/bridge` by the hash of what it is made
/// from.
pub fn compile_bridge(
    ctx: &Context,
    cache_root: &str,
    editor: &str,
    managed: &str,
) -> Result<String> {
    let tool_root = micro_editor_root()?;
    let runtime_root = join(&[&tool_root, "Packages/com.unitymicroeditor.runtime/Runtime"]);
    let data = join(&[&sysutil::dir(editor), "Data"]);
    let mut sources = Vec::new();
    sys::walk_dir(&runtime_root, &mut |entry| {
        if !entry.is_dir() && entry.path.ends_with(".cs") {
            sources.push(entry.path.to_string());
        }
        Ok(Walk::Continue)
    })?;
    sources.sort();
    // The bridge serves Addressables keys only where the host has the package.
    let addressables = ["Unity.Addressables.dll", "Unity.ResourceManager.dll"]
        .iter()
        .all(|name| !sys::absent(&join(&[managed, name])));
    // The Editor's version symbols, which the bridge selects engine APIs by.
    let versions = crate::release::version_defines(&crate::release::installed_version(editor)?)?;
    let mut input =
        format!("unity-play-managed-bridge-v2\0{editor}\0{addressables}\0").into_bytes();
    for define in &versions {
        input.extend_from_slice(define.as_bytes());
        input.push(0);
    }
    let mut snapshots = Vec::new();
    for path in &sources {
        let contents = sys::read(path)?;
        input.extend_from_slice(path.as_bytes());
        input.extend_from_slice(&contents);
        snapshots.push(contents);
    }
    let cache = join(&[cache_root, "bridge", &sha256(&input)]);
    let dll = join(&[&cache, "UnityMicroEditor.dll"]);
    #[derive(Serialize, Deserialize, Default)]
    #[serde(default)]
    struct Artifact {
        #[serde(deserialize_with = "super::nilable::nz")]
        sha256: String,
    }
    let artifact = join(&[&cache, "artifact.json"]);
    let cached = read_json_file::<Option<Artifact>>(&artifact)
        .ok()
        .map(Option::unwrap_or_default);
    let current = match (&cached, file_hash(&dll)) {
        (Some(cached), Ok(actual)) => !cached.sha256.is_empty() && actual == cached.sha256,
        _ => false,
    };
    if !current {
        sys::mkdir_all(&cache)?;
        let mut stable_sources = Vec::with_capacity(sources.len());
        for (path, contents) in sources.iter().zip(&snapshots) {
            let relative = sysutil::rel(&runtime_root, path)
                .ok_or_else(|| anyhow!("Rel: can't make {path} relative to {runtime_root}"))?;
            let destination = join(&[&cache, "Sources", &relative]);
            sys::mkdir_all(&sysutil::dir(&destination))?;
            sys::write(&destination, contents)?;
            stable_sources.push(destination);
        }
        let sdk = sys::unity_sdk(&data)
            .map_err(|error| anyhow!("Unity SDK compiler is missing: {error}"))?;
        let mut arguments: Vec<String> = [
            "-nologo",
            "-target:library",
            "-nostdlib+",
            "-langversion:9",
            "-warnaserror+",
            "-nowarn:0649,0618",
            "-unsafe+",
        ]
        .map(String::from)
        .to_vec();
        if addressables {
            arguments.push("-define:UNITY_MICRO_EDITOR_ADDRESSABLES".into());
        }
        arguments.extend(versions.iter().map(|define| format!("-define:{define}")));
        arguments.push(format!("-out:{dll}"));
        let mut seen = BTreeSet::new();
        for pattern in [
            join(&[&data, "MonoBleedingEdge/lib/mono/unityaot-linux/*.dll"]),
            join(&[
                &data,
                "MonoBleedingEdge/lib/mono/unityaot-linux/Facades/*.dll",
            ]),
            join(&[managed, "*.dll"]),
        ] {
            for file in sys::glob_lossy(&pattern) {
                let base = sysutil::base(&file);
                if base == "UnityMicroEditor.dll" || !seen.insert(base) {
                    continue;
                }
                arguments.push(format!("-reference:{file}"));
            }
        }
        arguments.extend(stable_sources);
        let response = join(&[&cache, "compile.rsp"]);
        let mut text = String::new();
        for argument in &arguments {
            text.push('"');
            text.push_str(&argument.replace('"', "\\\""));
            text.push_str("\"\n");
        }
        sys::write(&response, text.as_bytes())?;
        let (listing, error) = plain(ctx, &sdk.dotnet, &[&sdk.csc, &format!("@{response}")]);
        if let Some(error) = error {
            let _ = std::fs::remove_file(&dll);
            bail!(
                "compile Player bridge: {error}\n{}",
                String::from_utf8_lossy(&listing)
            );
        }
        atomic_json(
            &artifact,
            &Artifact {
                sha256: file_hash(&dll)?,
            },
        )?;
    }
    Ok(dll)
}

/// The manifest's assemblies relocated into a Player.
pub fn player_assembly_paths(player: &str, manifest: &str) -> Result<NilVec<String>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Source {
        assemblies: NilVec<String>,
    }
    let mut source = read_json_file::<Option<Source>>(manifest)?.unwrap_or_default();
    let managed = super::player_managed(player);
    for path in source.assemblies.iter_mut() {
        *path = join(&[&managed, &sysutil::base(path)]);
    }
    Ok(source.assemblies)
}
