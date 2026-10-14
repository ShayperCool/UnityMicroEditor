//! The two programs of hot reload, built from the tool's sources with the SDK
//! of the Unity installation: the patch builder (`tools/HotReloadPatcher`,
//! a net8.0 helper) and the applier (`tools/HotReloadRuntime`, compiled for
//! the Player's Mono like the bridge).
use super::wire::RUNTIME_ASSEMBLY;
use crate::{
    cancel::Context,
    compiler::{
        NilMap, Service, ServiceError, compiler_cold, compiler_command, glob, glob_lossy,
        verify_cache_files,
    },
    sha256, sysutil,
    util::{atomic_json, file_hash, marshal, micro_editor_root, read_json_file},
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    sync::Arc,
    thread::JoinHandle,
    time::Duration,
};

/// How long one run of the patch builder may take. It reads two assemblies
/// and writes one in a fraction of a second; a run that does not end would
/// stop the watch loop, which waits for it before it handles the next edit.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(60);

/// The sources of the patch builder. The project file compiles every `.cs`
/// beside it, so a source missing here would be left out of the build and
/// of the cache key; a test compares the list with the directory.
pub(crate) const PATCHER_SOURCES: [&str; 11] = [
    "HotReloadPatcher.csproj",
    "Canon.cs",
    "Codes.cs",
    "Diff.cs",
    "Emit.cs",
    "Model.cs",
    "Netcode.cs",
    "Pdb.cs",
    "Program.cs",
    "Unity.cs",
    "Verify.cs",
];
pub(crate) const PATCHER_DIRECTORY: &str = "tools/HotReloadPatcher";
pub(crate) const APPLIER_DIRECTORY: &str = "tools/HotReloadRuntime";

fn join(parts: &[&str]) -> String {
    sysutil::join(parts)
}
fn mkdir_all(path: &str) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| anyhow!(sysutil::Error::os("mkdir", path, &error)))
}
fn read(path: &str) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| anyhow!(sysutil::Error::os("open", path, &error)))
}
fn write(path: &str, data: &[u8]) -> Result<()> {
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(data))
        .map_err(|error| anyhow!(sysutil::Error::os("write", path, &error)))
}

/// The SDK programs both builds run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sdk {
    pub dotnet: String,
    pub csc: String,
}
impl Sdk {
    /// The .NET host and the newest Roslyn of the installation `editor`
    /// (the `Unity` executable) belongs to.
    pub fn locate(editor: &str) -> Result<Self> {
        let data = join(&[&sysutil::dir(editor), "Data"]);
        let mut compilers = glob_lossy(&join(&[&data, "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll"]));
        compilers.sort();
        let Some(csc) = compilers.pop() else {
            bail!("installed Unity Roslyn SDK is missing");
        };
        Ok(Self {
            dotnet: join(&[&data, "DotNetSdk/dotnet"]),
            csc,
        })
    }
}

/// Builds (or reuses) the patch builder and returns its `HotReloadPatcher.dll`.
/// The entry under `cache` is keyed by the sources and the SDK, so the build
/// runs once per version of the tool, never per edit.
pub fn prepare_patcher(ctx: &Context, sdk: &Sdk, cache: &str) -> Result<String> {
    let source_root = join(&[&micro_editor_root()?, PATCHER_DIRECTORY]);
    let mut inputs = BTreeMap::new();
    for filename in PATCHER_SOURCES
        .iter()
        .map(|name| join(&[&source_root, name]))
        .chain([sdk.csc.clone(), sdk.dotnet.clone()])
    {
        let hash = file_hash(&filename)?;
        inputs.insert(filename, hash);
    }
    let entry = join(&[cache, &sha256(&marshal(&inputs)?)]);
    let helper = join(&[&entry, "HotReloadPatcher.dll"]);
    if read_json_file::<NilMap<String>>(&join(&[&entry, "files.json"]))
        .is_ok_and(|metadata| verify_cache_files(&entry, &metadata))
    {
        return Ok(helper);
    }
    mkdir_all(cache)?;
    let staging = tempfile::Builder::new()
        .prefix(".helper-build-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(cache)
        .map_err(|error| anyhow!(sysutil::Error::os("mkdir", cache, &error)))?;
    let stage = staging.path().to_string_lossy().into_owned();
    for name in PATCHER_SOURCES {
        let source = join(&[&source_root, name]);
        let data = match read(&source) {
            Ok(data) if sha256(&data) == inputs[&source] => data,
            _ => bail!("hot reload patch builder source changed during build"),
        };
        write(&join(&[&stage, name]), &data)?;
    }
    write(
        &join(&[&stage, "NuGet.Config"]),
        b"<configuration><packageSources><clear /></packageSources></configuration>\n",
    )?;
    let payload = join(&[&stage, "payload"]);
    compiler_command(
        ctx,
        &stage,
        &sdk.dotnet,
        &[
            "build",
            &join(&[&stage, "HotReloadPatcher.csproj"]),
            "--nologo",
            "-v:q",
            "-o",
            &payload,
            &format!("-p:RoslynPath={}", sysutil::dir(&sdk.csc)),
            "-p:NuGetAudit=false",
            crate::compiler::BUILD_WITHOUT_SERVERS[0],
            crate::compiler::BUILD_WITHOUT_SERVERS[1],
        ],
    )
    .map_err(|error| anyhow!("build hot reload patch builder: {error}"))?;
    let mut metadata = NilMap::make();
    for item in fs::read_dir(&payload)
        .map_err(|error| anyhow!(sysutil::Error::os("open", &payload, &error)))?
    {
        let item = item.map_err(|error| anyhow!(sysutil::Error::os("read", &payload, &error)))?;
        if item.file_type().is_ok_and(|kind| kind.is_file()) {
            let name = item.file_name().to_string_lossy().into_owned();
            let hash = file_hash(&join(&[&payload, &name]))?;
            metadata.insert(name, hash);
        }
    }
    if metadata.at("HotReloadPatcher.dll").is_empty() {
        bail!("hot reload patch builder build produced no executable");
    }
    atomic_json(&join(&[&payload, "files.json"]), &metadata)?;
    // An entry that failed its check above is a damaged cache of ours.
    let _ = fs::remove_dir_all(&entry);
    fs::rename(&payload, &entry).map_err(|error| {
        anyhow!(sysutil::Error::os(
            "rename",
            &format!("{payload} {entry}"),
            &error
        ))
    })?;
    Ok(helper)
}

/// The applier assembly of one Player process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applier {
    pub path: String,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Artifact {
    sha256: String,
}

/// What the applier names of a Player's `Managed`: the engine's object and
/// log, and the bridge's `MethodPatch`. Nothing of the game: a game type
/// without a namespace would hide the class library type of its name
/// (`Console`, `Stopwatch`) in the applier's sources.
pub(crate) const APPLIER_REFERENCES: [&str; 2] =
    ["UnityEngine.CoreModule.dll", "UnityMicroEditor.dll"];

/// Compiles (or reuses) the applier with the bridge's session flags against
/// the class library and the two assemblies of the staged Player's `Managed`
/// it uses, one of them the session's bridge. The result is cached under
/// `cache` by content and stays outside the Player: the bridge loads it from
/// bytes, by the path and hash of a request.
pub fn stage_applier(
    ctx: &Context,
    sdk: &Sdk,
    editor: &str,
    managed: &str,
    cache: &str,
) -> Result<Applier> {
    let source_root = join(&[&micro_editor_root()?, APPLIER_DIRECTORY]);
    let mut sources = glob(&join(&[&source_root, "*.cs"]))?;
    sources.sort();
    if sources.is_empty() {
        bail!("hot reload applier sources are missing: {source_root}");
    }
    let bridge = join(&[managed, "UnityMicroEditor.dll"]);
    let data = join(&[&sysutil::dir(editor), "Data"]);
    let mut references = Vec::new();
    let mut seen = BTreeSet::new();
    for pattern in [
        join(&[&data, "MonoBleedingEdge/lib/mono/unityaot-linux/*.dll"]),
        join(&[
            &data,
            "MonoBleedingEdge/lib/mono/unityaot-linux/Facades/*.dll",
        ]),
    ] {
        for file in glob_lossy(&pattern) {
            if seen.insert(sysutil::base(&file)) {
                references.push(file);
            }
        }
    }
    for name in APPLIER_REFERENCES {
        let file = join(&[managed, name]);
        if !fs::metadata(&file).is_ok_and(|info| info.is_file()) {
            match name == "UnityMicroEditor.dll" {
                true => bail!("staged Player lacks the runtime bridge: {bridge}"),
                false => bail!("staged Player lacks {name}: {file}"),
            }
        }
        if seen.insert(name.to_string()) {
            references.push(file);
        }
    }
    let mut input = format!(
        "unity-play-hot-reload-applier-v2\0{editor}\0{}\0",
        file_hash(&bridge)?
    )
    .into_bytes();
    for base in &seen {
        input.extend_from_slice(base.as_bytes());
        input.push(0);
    }
    let mut snapshots = Vec::new();
    for path in &sources {
        let contents = read(path)?;
        input.extend_from_slice(sysutil::base(path).as_bytes());
        input.push(0);
        input.extend_from_slice(&contents);
        snapshots.push(contents);
    }
    let entry = join(&[cache, &sha256(&input)]);
    let dll = join(&[&entry, &format!("{RUNTIME_ASSEMBLY}.dll")]);
    let artifact = join(&[&entry, "artifact.json"]);
    let cached = read_json_file::<Option<Artifact>>(&artifact)
        .ok()
        .map(Option::unwrap_or_default);
    if let (Some(cached), Ok(actual)) = (&cached, file_hash(&dll))
        && !cached.sha256.is_empty()
        && actual == cached.sha256
    {
        return Ok(Applier {
            path: dll,
            sha256: actual,
        });
    }
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
    arguments.push(format!("-out:{dll}"));
    arguments.extend(references.iter().map(|file| format!("-reference:{file}")));
    for (path, contents) in sources.iter().zip(&snapshots) {
        let destination = join(&[&entry, "Sources", &sysutil::base(path)]);
        mkdir_all(&sysutil::dir(&destination))?;
        write(&destination, contents)?;
        arguments.push(destination);
    }
    let response = join(&[&entry, "compile.rsp"]);
    let mut text = String::new();
    for argument in &arguments {
        text.push('"');
        text.push_str(&argument.replace('"', "\\\""));
        text.push_str("\"\n");
    }
    write(&response, text.as_bytes())?;
    if let Err(error) = compiler_command(
        ctx,
        &entry,
        &sdk.dotnet,
        &[&sdk.csc, &format!("@{response}")],
    ) {
        let _ = fs::remove_file(&dll);
        bail!("compile hot reload applier: {error}");
    }
    let hash = file_hash(&dll)?;
    atomic_json(
        &artifact,
        &Artifact {
            sha256: hash.clone(),
        },
    )?;
    Ok(Applier {
        path: dll,
        sha256: hash,
    })
}

/// The patch builder as a process of a watched session (`--serve`). Two
/// handles are equal when they run the same builder; nothing it holds
/// decides a result.
#[derive(Debug, Clone, Default)]
pub struct BuilderService(pub Option<Arc<Service>>);
impl PartialEq for BuilderService {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_ref().map(|service| service.program())
            == other.0.as_ref().map(|service| service.program())
    }
}
impl Eq for BuilderService {}

/// What a session needs to patch code: where its two programs are built
/// and what the applier is compiled against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    pub sdk: Sdk,
    pub editor: String,
    /// `Managed` of the session's staged host, with the session's bridge.
    pub managed: String,
    /// The session output, the owner of both caches.
    pub output: String,
    pub patcher: String,
    /// The time one run of the patch builder gets; `BUILD_TIMEOUT` when none
    /// is given.
    pub limit: Option<Duration>,
    /// The builder of a watched session that stays alive between edits;
    /// without it every attempt starts the builder.
    pub service: BuilderService,
}
impl Tools {
    fn patcher_cache(output: &str) -> String {
        join(&[output, "hot-reload/patcher"])
    }
    fn applier_cache(output: &str) -> String {
        join(&[output, "hot-reload/applier"])
    }

    /// Builds both programs. Returns the applier of the first Player.
    pub fn prepare(
        ctx: &Context,
        editor: &str,
        managed: &str,
        output: &str,
    ) -> Result<(Self, Applier)> {
        let sdk = Sdk::locate(editor)?;
        let patcher = prepare_patcher(ctx, &sdk, &Self::patcher_cache(output))?;
        let tools = Self {
            sdk,
            editor: editor.into(),
            managed: managed.into(),
            output: output.into(),
            patcher,
            limit: None,
            service: BuilderService::default(),
        };
        let applier = tools.applier(ctx)?;
        Ok((tools, applier))
    }

    /// The tools with the builder kept alive between edits, unless
    /// `UME_COMPILER_COLD=1`. The process starts at the first edit, on the
    /// thread of the watch loop, whose end ends it.
    pub fn served(mut self) -> Self {
        if !compiler_cold() {
            self.service = BuilderService(Some(Arc::new(Service::new(
                "patch builder",
                &self.sdk.dotnet,
                &self.patcher,
            ))));
        }
        self
    }

    /// The applier a Player that starts now is patched with. One process
    /// loads one applier, so the caller keeps it for the life of that process.
    pub fn applier(&self, ctx: &Context) -> Result<Applier> {
        stage_applier(
            ctx,
            &self.sdk,
            &self.editor,
            &self.managed,
            &Self::applier_cache(&self.output),
        )
    }

    /// Runs the patch builder on a request file; the output is its result.
    pub fn build(&self, ctx: &Context, request: &str) -> Result<Vec<u8>> {
        self.build_within(ctx, request, self.limit.unwrap_or(BUILD_TIMEOUT))
    }

    /// [`Tools::build`] with the time the builder gets. When it is over the
    /// builder is stopped and the run fails like any other failed build.
    /// The builder of the session answers when it can; a request it could
    /// not answer, or answered with a failure, runs once in a process of its
    /// own, whose result counts.
    pub fn build_within(&self, ctx: &Context, request: &str, limit: Duration) -> Result<Vec<u8>> {
        if let Some(service) = &self.service.0
            && service.program() == self.patcher
        {
            match service.call(ctx, &sysutil::dir(request), &[request], limit) {
                Ok(reply) if reply.exit == 0 => {
                    return Ok(format!("{}{}", reply.output, reply.error).into_bytes());
                }
                Err(ServiceError::TimedOut(_)) if ctx.error().is_none() => bail!(
                    "the patch builder did not finish within {}s and was stopped",
                    limit.as_secs()
                ),
                _ => ctx.err()?,
            }
        }
        let limited = ctx.with_timeout(limit);
        let output = compiler_command(
            &limited,
            &sysutil::dir(request),
            &self.sdk.dotnet,
            &[&self.patcher, request],
        );
        let expired = limited.error().is_some() && ctx.error().is_none();
        limited.cancel();
        match output {
            Err(_) if expired => bail!(
                "the patch builder did not finish within {}s and was stopped",
                limit.as_secs()
            ),
            output => Ok(output?),
        }
    }
}

/// [`Tools::prepare`] on a thread of its own, so that the first build does
/// not delay the start of the Player.
pub struct Setup {
    ctx: Context,
    thread: Option<JoinHandle<Result<(Tools, Applier)>>>,
}
impl Setup {
    pub fn start(ctx: &Context, editor: &str, managed: &str, output: &str) -> Self {
        let ctx = ctx.with_cancel();
        let (own, editor, managed, output) = (
            ctx.clone(),
            editor.to_string(),
            managed.to_string(),
            output.to_string(),
        );
        let thread = std::thread::spawn(move || Tools::prepare(&own, &editor, &managed, &output));
        Self {
            ctx,
            thread: Some(thread),
        }
    }
    /// Blocks until both programs exist or their build failed. A cancelled
    /// context ends the builds, so this returns soon after it.
    pub fn finish(mut self) -> Result<(Tools, Applier)> {
        let thread = self.thread.take().expect("setup thread");
        thread
            .join()
            .unwrap_or_else(|_| Err(anyhow!("hot reload setup ended unexpectedly")))
    }
}
impl Drop for Setup {
    /// A session that ends before it asked for the result must not leave a
    /// build behind.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.ctx.cancel();
            let _ = thread.join();
        }
    }
}
