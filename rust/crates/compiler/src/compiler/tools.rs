//! The C# tools of `tools/` that read assemblies with Unity's own Cecil, built
//! once per source, Cecil and SDK into the project's store, and what they read.
use super::{
    Compiler, NilMap, player_managed,
    sys::{self, compiler_command},
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, micro_editor_root, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;

impl Compiler {
    /// `tools/<name>` (`<name>.csproj` and `Program.cs`), built into the store
    /// entry `<kind>/<key>`, keyed by its sources, the Cecil and the SDK: the
    /// path of its `<name>.dll`.
    pub(crate) fn built_tool(&self, ctx: &Context, name: &str, kind: &str) -> Result<String> {
        let sources = sysutil::join(&[&micro_editor_root()?, "tools", name]);
        let project = format!("{name}.csproj");
        let dll = format!("{name}.dll");
        // Unity's own Cecil, which IL2CPP ships with every installation.
        let cecil = sysutil::join(&[&sysutil::dir(&self.editor), "Data/il2cpp/build/deploy"]);
        let mut inputs: BTreeMap<String, String> = BTreeMap::new();
        for file in [project.as_str(), "Program.cs"] {
            inputs.insert(file.into(), file_hash(&sysutil::join(&[&sources, file]))?);
        }
        inputs.insert(
            "Mono.Cecil.dll".into(),
            file_hash(&sysutil::join(&[&cecil, "Mono.Cecil.dll"]))?,
        );
        inputs.insert("dotnet".into(), file_hash(&self.dotnet)?);
        let key = sha256(&marshal(&inputs)?);
        let entry = sysutil::join(&[&self.stored(kind), &key]);
        let valid = |entry: &str| {
            read_json_file::<NilMap<String>>(&sysutil::join(&[entry, "files.json"])).is_ok_and(
                |files| {
                    !files.at(&dll).is_empty()
                        && files.iter().all(|(file, hash)| {
                            file_hash(&sysutil::join(&[entry, file]))
                                .is_ok_and(|actual| actual == *hash)
                        })
                },
            )
        };
        if !valid(&entry) {
            sys::mkdir_all(&sysutil::dir(&entry))?;
            let staging = sys::mkdir_temp(&sysutil::dir(&entry), ".tool-build-")?;
            for file in [project.as_str(), "Program.cs"] {
                let payload = sys::read(&sysutil::join(&[&sources, file]))?;
                if sha256(&payload) != inputs[file] {
                    bail!("the source of {name} changed while it was built: {file}");
                }
                sys::write(&sysutil::join(&[&staging, file]), &payload)?;
            }
            sys::write(
                &sysutil::join(&[&staging, "NuGet.Config"]),
                b"<configuration><packageSources><clear /></packageSources></configuration>\n",
            )?;
            let payload = sysutil::join(&[&staging, "payload"]);
            compiler_command(
                ctx,
                &staging,
                &self.dotnet,
                &[
                    "build",
                    &sysutil::join(&[&staging, &project]),
                    "--nologo",
                    "-v:q",
                    "-o",
                    &payload,
                    "-p:NuGetAudit=false",
                    &format!("-p:CecilPath={cecil}"),
                    sys::BUILD_WITHOUT_SERVERS[0],
                    sys::BUILD_WITHOUT_SERVERS[1],
                ],
            )
            .map_err(|error| anyhow!("build {name}: {error}"))?;
            let mut files = NilMap::make();
            for file in sys::regular_files(&payload)? {
                files.insert(file.clone(), file_hash(&sysutil::join(&[&payload, &file]))?);
            }
            if files.at(&dll).is_empty() {
                bail!("the build of {name} produced no {dll}");
            }
            atomic_json(&sysutil::join(&[&payload, "files.json"]), &files)?;
            sys::publish(&payload, &entry, || valid(&entry))?;
            let _ = std::fs::remove_dir_all(&staging);
        }
        Ok(sysutil::join(&[&entry, &dll]))
    }

    /// What `tools/ScriptLayouts` reads of the Player's assemblies, those of
    /// the newest compile in place of the Player's own: the serialized fields
    /// of every script class, as JSON, kept in the store by the tool and the
    /// assemblies.
    pub fn script_layouts(&self, ctx: &Context) -> Result<Vec<u8>> {
        let tool = self.built_tool(ctx, "ScriptLayouts", "script-layouts-tool")?;
        let managed = player_managed(&self.player);
        let mut assemblies: BTreeMap<String, String> = BTreeMap::new();
        for file in sys::regular_files(&managed)? {
            if let Some(name) = file.strip_suffix(".dll") {
                assemblies.insert(name.to_string(), sysutil::join(&[&managed, &file]));
            }
        }
        for (name, path) in self.current_dlls.iter() {
            if !path.is_empty() {
                assemblies.insert(name.clone(), path.clone());
            }
        }
        let mut inputs: BTreeMap<String, String> = BTreeMap::new();
        inputs.insert("tool".into(), tool.clone());
        for (name, path) in &assemblies {
            inputs.insert(format!("{name}.dll"), file_hash(path)?);
        }
        let key = sha256(&marshal(&inputs)?);
        let file = sysutil::join(&[&self.stored("script-layouts"), &format!("{key}.json")]);
        if let Ok(data) = std::fs::read(&file)
            && serde_json::from_slice::<serde_json::Value>(&data).is_ok()
        {
            return Ok(data);
        }
        let mut arguments = vec![tool.as_str()];
        arguments.extend(assemblies.values().map(String::as_str));
        let output = compiler_command(ctx, &managed, &self.dotnet, &arguments)
            .map_err(|error| anyhow!("read the script layouts: {error}"))?;
        if serde_json::from_slice::<serde_json::Value>(&output).is_err() {
            bail!("ScriptLayouts printed no layouts");
        }
        sys::mkdir_all(&sysutil::dir(&file))?;
        let staging = format!("{file}.{}", std::process::id());
        sys::write(&staging, &output)?;
        sys::rename(&staging, &file)?;
        Ok(output)
    }
}

/// `Compiler::script_layouts` of the Player `player` (its executable) for a
/// project, with the tools built by the installation `editor` into the
/// project's compiler store.
pub fn player_script_layouts(
    ctx: &Context,
    project: &str,
    editor: &str,
    player: &str,
) -> Result<Vec<u8>> {
    let sdk = sys::unity_sdk(&sysutil::join(&[&sysutil::dir(editor), "Data"]))?;
    let compiler = Compiler {
        project: project.into(),
        editor: editor.into(),
        store: super::project_compiler_store(project),
        player: player.into(),
        dotnet: sdk.dotnet,
        csc: sdk.csc,
        ..Compiler::default()
    };
    compiler.script_layouts(ctx)
}
