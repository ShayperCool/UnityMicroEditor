//! Assemblies of packages nobody edits (registry, built-in, Git) that the
//! host Player does not provide, built as the Editor builds them: Roslyn over
//! the package's own sources, without the lifecycle rewrite of game code,
//! then Unity's own IL post-processor runner (`Unity.ILPP.Runner`) with the
//! post-processors of the installation and the project. The result joins the
//! staged host: its DLL goes into `Managed`, its slot into
//! `ScriptingAssemblies.json` and its runtime initializers into
//! `RuntimeInitializeOnLoads.json`, where the engine runs them, as it does for
//! the package assemblies a host build made.
use super::{
    Compiler, NilMap,
    compile::verify_cache_files,
    sys::{self, compiler_command},
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{atomic_json, marshal, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// One row of `RuntimeInitializeOnLoads.json`, as a build writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct InitializerRow {
    pub assemblyName: String,
    pub nameSpace: String,
    pub className: String,
    pub methodName: String,
    pub loadTypes: i64,
    pub isUnityClass: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct PackageCache {
    key: String,
    name: String,
    files: NilMap<String>,
    initializers: Vec<InitializerRow>,
    #[serde(rename = "postProcessors")]
    post_processors: String,
}

/// What a package build left.
pub struct PackageBuild {
    pub dll: String,
    pub initializers: Vec<InitializerRow>,
    /// The runner's account of the processors it ran.
    pub post_processors: String,
}

/// Unity's IL post-processor runner for the life of one preparation: a
/// server on a Unix socket, configured once with the post-processors and
/// asked once per assembly through `Unity.ILPP.Trigger`, as Bee does.
pub struct IlppRunner {
    child: Child,
    socket: String,
    trigger: String,
}

impl Drop for IlppRunner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(sysutil::dir(&self.socket));
    }
}

fn compilation_tools(editor: &str) -> String {
    sysutil::join(&[
        &sysutil::dir(editor),
        "Data/Tools/BuildPipeline/Compilation",
    ])
}

/// The post-processors the installation ships, and what they load.
pub fn builtin_post_processors(editor: &str) -> (Vec<String>, Vec<String>) {
    let directory = sysutil::join(&[
        &sysutil::dir(editor),
        "Data/Tools/BuildPipeline/BuiltinILPPs",
    ]);
    let mut processors = Vec::new();
    let mut helpers = Vec::new();
    for path in sys::glob_lossy(&sysutil::join(&[&directory, "*.dll"])) {
        if sysutil::base(&path).ends_with(".CodeGen.dll") {
            processors.push(path);
        } else {
            helpers.push(path);
        }
    }
    helpers.push(sysutil::join(&[
        &sysutil::dir(editor),
        "Data/Managed/Unity.CompilationPipeline.Common.dll",
    ]));
    (processors, helpers)
}

impl IlppRunner {
    pub fn start(editor: &str, directory: &str) -> Result<Self> {
        let tools = compilation_tools(editor);
        let runner = sysutil::join(&[&tools, "Unity.ILPP.Runner/Unity.ILPP.Runner"]);
        let trigger = sysutil::join(&[&tools, "Unity.ILPP.Trigger/Unity.ILPP.Trigger"]);
        sys::mkdir_all(directory)?;
        // A Unix socket path holds at most 107 bytes; a staging directory
        // can be longer.
        let short = sys::mkdir_temp(&std::env::temp_dir().to_string_lossy(), "ume-ilpp-")?;
        let socket = sysutil::join(&[&short, "s"]);
        let child = Command::new(&runner)
            .arg(&socket)
            .arg("--log-file")
            .arg(sysutil::join(&[directory, "ilpp-runner.log"]))
            .arg("--no-pid")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| anyhow!("start {runner}: {error}"))?;
        let mut started = Self {
            child,
            socket,
            trigger,
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while !std::path::Path::new(&started.socket).exists() {
            if let Some(status) = started.child.try_wait()? {
                bail!("the IL post-processor runner exited before it listened: {status}");
            }
            if Instant::now() > deadline {
                bail!("the IL post-processor runner did not listen within 60 s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(started)
    }

    fn trigger(&self, ctx: &Context, directory: &str, arguments: &[String]) -> Result<String> {
        let mut all = vec![self.socket.as_str()];
        all.extend(arguments.iter().map(String::as_str));
        let output = compiler_command(ctx, directory, &self.trigger, &all)
            .map_err(|error| anyhow!("IL post-processing: {}", error.message))?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    /// Hands the runner the post-processor assemblies and what they load.
    pub fn configure(
        &mut self,
        ctx: &Context,
        directory: &str,
        processors: &[String],
        helpers: &[String],
    ) -> Result<()> {
        let mut arguments = vec!["c".to_string()];
        arguments.extend(processors.iter().cloned());
        arguments.push("-h".into());
        arguments.extend(helpers.iter().cloned());
        self.trigger(ctx, directory, &arguments)?;
        Ok(())
    }

    /// Post-processes `input` into `output`, a directory; returns the
    /// runner's account of the processors.
    pub fn process(
        &self,
        ctx: &Context,
        input: &str,
        output: &str,
        defines: &[String],
        references: &[String],
    ) -> Result<String> {
        sys::mkdir_all(output)?;
        let mut arguments = vec!["p".to_string(), input.to_string(), output.to_string()];
        arguments.extend(defines.iter().cloned());
        arguments.push("-r".into());
        arguments.extend(references.iter().cloned());
        self.trigger(ctx, output, &arguments)
    }
}

/// The post-processors of one preparation: the runner is started on the
/// first assembly that is not already built.
pub struct PostProcessors {
    pub processors: Vec<String>,
    pub helpers: Vec<String>,
    /// The hashes of the runner, the trigger and every assembly above.
    pub key: String,
    runner: Option<IlppRunner>,
    directory: String,
}

impl PostProcessors {
    /// The installation's post-processors and `codegen`, the ones the
    /// project's packages bring, with what those load (`helpers`).
    pub fn new(
        editor: &str,
        directory: &str,
        codegen: &[String],
        helpers: &[String],
    ) -> Result<Self> {
        let (mut processors, mut all_helpers) = builtin_post_processors(editor);
        processors.extend(codegen.iter().cloned());
        for helper in helpers {
            if !all_helpers.contains(helper) {
                all_helpers.push(helper.clone());
            }
        }
        let tools = compilation_tools(editor);
        let mut identity = BTreeMap::new();
        for path in processors.iter().chain(&all_helpers).cloned().chain([
            sysutil::join(&[&tools, "Unity.ILPP.Runner/Unity.ILPP.Runner.dll"]),
            sysutil::join(&[&tools, "Unity.ILPP.Trigger/Unity.ILPP.Trigger"]),
        ]) {
            identity.insert(path.clone(), crate::util::file_hash(&path)?);
        }
        Ok(Self {
            processors,
            helpers: all_helpers,
            key: sha256(&marshal(&identity)?),
            runner: None,
            directory: directory.to_string(),
        })
    }

    fn runner(&mut self, ctx: &Context, editor: &str) -> Result<&IlppRunner> {
        if self.runner.is_none() {
            let mut runner = IlppRunner::start(editor, &self.directory)?;
            runner.configure(ctx, &self.directory, &self.processors, &self.helpers)?;
            self.runner = Some(runner);
        }
        Ok(self.runner.as_ref().unwrap())
    }
}

/// What one Roslyn compile of a package's sources is made of.
pub struct PlainCompile<'a> {
    pub name: &'a str,
    pub sources: &'a [String],
    pub defines: &'a [String],
    pub compiler_options: &'a [String],
    pub allow_unsafe: bool,
    pub references: &'a [String],
    /// `package-compiles` or `package-codegen`.
    pub kind: &'a str,
    /// The directory Unity's source generators take for the project, which
    /// the paths they record are relative to; the project when empty.
    pub project_view: &'a str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct CompileCache {
    key: String,
    name: String,
    files: NilMap<String>,
}

impl Compiler {
    fn kept(&self, kind: &str, key: &str) -> String {
        sysutil::join(&[&self.project, "Library/UnityMicroEditor", kind, key])
    }

    /// Compiles sources of a package as they are, kept for the project
    /// under the hash of everything the compile is made of.
    pub fn compile_plain(
        &mut self,
        ctx: &Context,
        staging: &str,
        compile: &PlainCompile<'_>,
    ) -> Result<String> {
        let mut identity: BTreeMap<String, String> = BTreeMap::new();
        identity.insert("mode".into(), format!("{}-v1", compile.kind));
        identity.insert("defines".into(), compile.defines.join(";"));
        identity.insert("options".into(), compile.compiler_options.join(" "));
        identity.insert("unsafe".into(), compile.allow_unsafe.to_string());
        identity.insert("view".into(), compile.project_view.to_string());
        let view = match compile.project_view.is_empty() {
            true => self.project.clone(),
            false => compile.project_view.to_string(),
        };
        for source in compile.sources {
            let path = match sysutil::is_abs(source) {
                true => source.clone(),
                false => sysutil::join(&[&view, source]),
            };
            identity.insert(source.clone(), self.hashes.file_hash(&path)?);
        }
        for reference in compile.references {
            identity.insert(
                format!("reference:{}", sysutil::base(reference)),
                self.hashes.file_hash(reference)?,
            );
        }
        for executable in std::iter::once(self.csc.clone()).chain(self.generators.iter().cloned()) {
            let hash = self.hashes.file_hash(&executable)?;
            identity.insert(executable, hash);
        }
        let key = sha256(&marshal(&identity)?);
        let library = format!("{}.dll", compile.name);
        let entry = self.kept(compile.kind, &key);
        let cached = || {
            read_json_file::<Option<CompileCache>>(&sysutil::join(&[&entry, "compile.json"]))
                .ok()
                .flatten()
                .filter(|cached| {
                    cached.key == key
                        && cached.name == compile.name
                        && verify_cache_files(&entry, &cached.files)
                        && !cached.files.at(&library).is_empty()
                })
        };
        if cached().is_some() {
            return Ok(sysutil::join(&[&entry, &library]));
        }
        // The compiler's errors for these inputs, which it gives again.
        let refused = format!("{entry}.refused.txt");
        if let Ok(message) = sys::read(&refused) {
            bail!(
                "compile {} {}: {}",
                compile.kind,
                compile.name,
                String::from_utf8_lossy(&message)
            );
        }
        let folder = sysutil::join(&[staging, compile.kind, compile.name]);
        sys::mkdir_all(&folder)?;
        let dll = sysutil::join(&[&folder, &library]);
        let mut arguments: Vec<String> = [
            "-nologo",
            "-target:library",
            "-nostdlib+",
            "-langversion:9.0",
            "-deterministic+",
            "-debug:portable",
            "-optimize-",
            if compile.allow_unsafe {
                "-unsafe+"
            } else {
                "-unsafe-"
            },
        ]
        .map(String::from)
        .to_vec();
        arguments.push(format!("-out:{dll}"));
        arguments.push(format!("-define:{}", compile.defines.join(";")));
        arguments.extend(compile.compiler_options.iter().cloned());
        arguments.extend(
            compile
                .references
                .iter()
                .map(|reference| format!("-reference:{reference}")),
        );
        arguments.extend(
            self.generators
                .iter()
                .map(|generator| format!("-analyzer:{generator}")),
        );
        let generated = sysutil::join(&[&folder, "Generated"]);
        sys::mkdir_all(&generated)?;
        let additional = sysutil::join(&[
            &folder,
            &format!("{}.UnityAdditionalFile.txt", compile.name),
        ]);
        let view = view.as_str();
        sys::write(&additional, view.as_bytes())?;
        arguments.push(format!("-generatedfilesout:{generated}"));
        arguments.push(format!("-additionalfile:{additional}"));
        arguments.extend(compile.sources.iter().cloned());
        let response = sysutil::join(&[&folder, &format!("{}.rsp", compile.name)]);
        let mut text = String::new();
        for argument in &arguments {
            text.push('"');
            text.push_str(&argument.replace('"', "\\\""));
            text.push_str("\"\n");
        }
        sys::write(&response, text.as_bytes())?;
        compiler_command(
            ctx,
            view,
            &self.dotnet,
            &[&self.csc, &format!("@{response}")],
        )
        .map_err(|error| {
            // Errors of the sources are kept; a compiler that was stopped or
            // failed otherwise is asked again.
            if ctx.error().is_none() && error.message.contains(": error CS") {
                let partial = format!("{refused}.{}.tmp", crate::util::random_id());
                if sys::mkdir_all(&sysutil::dir(&refused)).is_ok()
                    && sys::write(&partial, error.message.as_bytes()).is_ok()
                {
                    let _ = std::fs::rename(&partial, &refused);
                }
            }
            anyhow!(
                "compile {} {}: {}",
                compile.kind,
                compile.name,
                error.message
            )
        })?;
        publish_files(
            &entry,
            &[
                dll.clone(),
                format!("{}.pdb", dll.strip_suffix(".dll").unwrap_or(&dll)),
            ],
            "compile.json",
            |files| CompileCache {
                key: key.clone(),
                name: compile.name.to_string(),
                files,
            },
            || cached().is_some(),
        )?;
        Ok(sysutil::join(&[&entry, &library]))
    }

    /// Post-processes a compiled package assembly and reads its runtime
    /// initializers, kept like its compile.
    #[allow(clippy::too_many_arguments)]
    pub fn post_process_package(
        &mut self,
        ctx: &Context,
        staging: &str,
        helper: &str,
        name: &str,
        compiled: &str,
        defines: &[String],
        refs: &[String],
        post: &mut PostProcessors,
    ) -> Result<PackageBuild> {
        let mut identity: BTreeMap<String, String> = BTreeMap::new();
        identity.insert("mode".into(), "package-build-v2".into());
        identity.insert("compiled".into(), self.hashes.file_hash(compiled)?);
        identity.insert("postProcessors".into(), post.key.clone());
        identity.insert("helper".into(), self.hashes.file_hash(helper)?);
        identity.insert("defines".into(), defines.join(";"));
        for reference in refs {
            identity.insert(
                format!("reference:{}", sysutil::base(reference)),
                self.hashes.file_hash(reference)?,
            );
        }
        let key = sha256(&marshal(&identity)?);
        let library = format!("{name}.dll");
        let entry = self.kept("package-builds", &key);
        let cached = || {
            read_json_file::<Option<PackageCache>>(&sysutil::join(&[&entry, "package-build.json"]))
                .ok()
                .flatten()
                .filter(|cached| {
                    cached.key == key
                        && cached.name == name
                        && verify_cache_files(&entry, &cached.files)
                        && !cached.files.at(&library).is_empty()
                })
        };
        if let Some(cached) = cached() {
            return Ok(PackageBuild {
                dll: sysutil::join(&[&entry, &library]),
                initializers: cached.initializers,
                post_processors: cached.post_processors,
            });
        }
        let processed = sysutil::join(&[staging, "package-builds", name]);
        let editor = self.editor.clone();
        let post_processors = post
            .runner(ctx, &editor)?
            .process(ctx, compiled, &processed, defines, refs)?;
        let dll = sysutil::join(&[&processed, &library]);
        let output = compiler_command(
            ctx,
            &self.cache,
            &self.dotnet,
            &[helper, "--runtime-initializers", &dll],
        )
        .map_err(|error| anyhow!("runtime initializers of {name}: {}", error.message))?;
        let initializers: Vec<InitializerRow> = serde_json::from_slice(&output)
            .map_err(|error| anyhow!("runtime initializers of {name}: {error}"))?;
        publish_files(
            &entry,
            &[
                dll.clone(),
                format!("{}.pdb", dll.strip_suffix(".dll").unwrap_or(&dll)),
            ],
            "package-build.json",
            |files| PackageCache {
                key: key.clone(),
                name: name.to_string(),
                files,
                initializers: initializers.clone(),
                post_processors: post_processors.clone(),
            },
            || cached().is_some(),
        )?;
        Ok(PackageBuild {
            dll: sysutil::join(&[&entry, &library]),
            initializers,
            post_processors,
        })
    }
}

/// Publishes `files` and a record of them as the kept entry `entry`.
fn publish_files<T: Serialize>(
    entry: &str,
    files: &[String],
    record: &str,
    make: impl FnOnce(NilMap<String>) -> T,
    valid: impl Fn() -> bool,
) -> Result<()> {
    sys::mkdir_all(&sysutil::dir(entry))?;
    let payload = sys::mkdir_temp(&sysutil::dir(entry), ".kept-")?;
    let mut hashes = NilMap::make();
    for filename in files {
        let data = sys::read(filename)?;
        let name = sysutil::base(filename);
        sys::write(&sysutil::join(&[&payload, &name]), &data)?;
        hashes.insert(name, sha256(&data));
    }
    atomic_json(&sysutil::join(&[&payload, record]), &make(hashes))?;
    sys::publish(&payload, entry, valid)
}

/// Puts the rows of the assemblies in `rows` into the staged Player's
/// `RuntimeInitializeOnLoads.json`, in place of any it had for them.
pub fn add_native_initializers(filename: &str, rows: &[InitializerRow]) -> Result<()> {
    let mut table: serde_json::Value = match sys::read(filename) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(error) if sys::is_not_exist(&error) => serde_json::json!({ "root": [] }),
        Err(error) => return Err(error),
    };
    let Some(root) = table.get_mut("root").and_then(|root| root.as_array_mut()) else {
        bail!("pinned native initializer table has unknown schema");
    };
    let assemblies: std::collections::BTreeSet<&str> =
        rows.iter().map(|row| row.assemblyName.as_str()).collect();
    root.retain(|row| {
        !row.get("assemblyName")
            .and_then(|name| name.as_str())
            .is_some_and(|name| assemblies.contains(name))
    });
    for row in rows {
        root.push(serde_json::to_value(row)?);
    }
    // Staged copies of host files may be read-only.
    let _ = std::fs::remove_file(filename);
    sys::write(filename, serde_json::to_string(&table)?.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(assembly: &str, method: &str, load: i64) -> InitializerRow {
        InitializerRow {
            assemblyName: assembly.into(),
            nameSpace: "N".into(),
            className: "C".into(),
            methodName: method.into(),
            loadTypes: load,
            isUnityClass: assembly.starts_with("Unity"),
        }
    }

    #[test]
    fn package_initializers_replace_the_rows_of_their_assembly_only() {
        let directory = tempfile::TempDir::new().unwrap();
        let table = directory.path().join("RuntimeInitializeOnLoads.json");
        let table = table.to_str().unwrap();
        std::fs::write(
            table,
            r#"{"root":[{"assemblyName":"Unity.Collections","nameSpace":"","className":"$BurstDirectCallInitializer","methodName":"Initialize","loadTypes":2,"isUnityClass":true},{"assemblyName":"Unity.RenderPipelines.Core.Runtime","nameSpace":"N","className":"Old","methodName":"Gone","loadTypes":0,"isUnityClass":true}]}"#,
        )
        .unwrap();
        add_native_initializers(
            table,
            &[
                row("Unity.RenderPipelines.Core.Runtime", "RuntimeInit", 0),
                row("Game.Package", "Boot", 4),
            ],
        )
        .unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(table).unwrap()).unwrap();
        let rows: Vec<(String, String)> = written["root"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["assemblyName"].as_str().unwrap().to_string(),
                    row["methodName"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("Unity.Collections".to_string(), "Initialize".to_string()),
                (
                    "Unity.RenderPipelines.Core.Runtime".to_string(),
                    "RuntimeInit".to_string()
                ),
                ("Game.Package".to_string(), "Boot".to_string()),
            ]
        );
        assert_eq!(written["root"][2]["isUnityClass"], false);
    }
}
