//! Extending the prepared graph with new sources under
//! Assets without changing assembly slots, defines or existing identities.
use super::{
    CompileScript, Compiler, MetadataGuidIndex, NilMap, join,
    nilable::nz,
    sys::{self, Walk},
};
use crate::{
    project::lexpath::rune_lower,
    registry, sha256, sysutil,
    util::{read_json_file, within_directory},
};
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

impl Compiler {
    /// The packages of the project itself (`Packages/<name>`) are the
    /// project's sources too, and a script added to one belongs to an
    /// assembly of the graph that a session cannot extend: the runner
    /// refuses it, and a preparation made before it does not hold it. Every
    /// root of a prepared runtime assembly there (the folder of its
    /// definition, without folders of other definitions) is walked; a
    /// managed file the plan does not hold needs a fresh graph.
    fn check_package_sources(&self, tracked: &BTreeMap<String, String>) -> Result<()> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Platforms {
            #[serde(rename = "includePlatforms")]
            include: Vec<String>,
        }
        let packages = join(&[&self.project, "Packages"]);
        let definition = |filename: &str| matches!(sysutil::ext(filename), ".asmdef" | ".asmref");
        // An assembly for the Editor alone is no part of a Player.
        let editor_only = |filename: &str| {
            sysutil::ext(filename) == ".asmdef"
                && read_json_file::<Option<Platforms>>(filename)
                    .is_ok_and(|platforms| platforms.unwrap_or_default().include == ["Editor"])
        };
        let mut roots: Vec<String> = self
            .manifest
            .compiler_inputs
            .keys()
            .filter(|filename| {
                definition(filename)
                    && within_directory(filename, &packages)
                    && !editor_only(filename)
            })
            .map(|filename| sysutil::dir(filename))
            .collect();
        roots.sort();
        roots.dedup();
        for root in &roots {
            sys::walk_dir(root, &mut |entry| {
                let filename = entry.path;
                if entry.is_dir() {
                    // Unity compiles nothing in a hidden folder or one whose name
                    // ends with `~`, and another definition below makes its
                    // folder another assembly.
                    let base = sysutil::base(filename);
                    let ignored =
                        filename != root.as_str() && (base.starts_with('.') || base.ends_with('~'));
                    let nested = ignored
                        || filename != root.as_str()
                            && fs::read_dir(filename).is_ok_and(|mut entries| {
                                entries.any(|entry| {
                                    entry.is_ok_and(|entry| {
                                        definition(&entry.file_name().to_string_lossy())
                                    })
                                })
                            });
                    return Ok(if nested {
                        Walk::SkipDir
                    } else {
                        Walk::Continue
                    });
                }
                if !self.manifest.compiler_inputs.at(filename).is_empty()
                    || tracked.contains_key(filename)
                {
                    return Ok(Walk::Continue);
                }
                match rune_lower(sysutil::ext(filename)).as_str() {
                    ".cs" => bail!(
                        "a new runtime C# source of a package requires a fresh managed graph: {filename}"
                    ),
                    ".asmdef" | ".asmref" | ".rsp" | ".dll" => {
                        bail!(
                            "new managed configuration requires a fresh managed graph: {filename}"
                        )
                    }
                    _ => Ok(Walk::Continue),
                }
            })?;
        }
        Ok(())
    }

    /// Source path to owning assembly.
    pub fn graph_source_assemblies(&self) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        for assembly in &self.manifest.assemblies {
            for source in &assembly.sources {
                result.insert(source.clone(), assembly.name.clone());
            }
        }
        result
    }

    /// The prepared assembly configuration remains
    /// authoritative. Only source additions under Assets can extend it; they
    /// cannot change assembly slots, defines, packages, compiler options, or
    /// existing MonoScript identities.
    pub fn source_graph_candidate(&self, changed: &[String]) -> Result<Compiler> {
        self.check_fixed_configuration()?;
        let tracked = self.graph_source_assemblies();
        let assets = join(&[&self.project, "Assets"]);
        let mut additions = Vec::new();
        sys::walk_dir(&assets, &mut |entry| {
            let filename = entry.path;
            if entry.is_dir() && (self.host_owned(filename) || sysutil::base(filename) == "Editor")
            {
                return Ok(Walk::SkipDir);
            }
            if entry.is_dir()
                || !self.manifest.compiler_inputs.at(filename).is_empty()
                || tracked.get(filename).is_some_and(|name| !name.is_empty())
                || super::editor_source(filename)
            {
                return Ok(Walk::Continue);
            }
            match rune_lower(sysutil::ext(filename)).as_str() {
                ".cs" => {
                    if sysutil::ext(filename) != ".cs" {
                        bail!("new runtime C# sources require the .cs extension: {filename}");
                    }
                    additions.push(filename.to_string());
                }
                ".asmdef" | ".asmref" | ".rsp" | ".dll" => {
                    bail!("new managed configuration requires a fresh managed graph: {filename}")
                }
                ".meta"
                    if [".asmdef.meta", ".asmref.meta", ".rsp.meta", ".dll.meta"]
                        .iter()
                        .any(|suffix| filename.ends_with(suffix)) =>
                {
                    bail!(
                        "new managed configuration metadata requires a fresh managed graph: {filename}"
                    )
                }
                _ => {}
            }
            Ok(Walk::Continue)
        })?;
        self.check_package_sources(&tracked)?;
        let mut candidate = self.clone();
        if candidate.configuration.is_nil() {
            candidate.configuration = NilMap::make();
        }
        if !additions.is_empty() {
            let roots = self.assembly_source_roots()?;
            let mut guids: BTreeMap<String, String> = BTreeMap::new();
            for script in &self.manifest.scripts {
                if !script.guid.is_empty() {
                    guids.insert(rune_lower(&script.guid), script.path.clone());
                }
            }
            let mut new_guids: BTreeMap<String, String> = BTreeMap::new();
            additions.sort();
            let project = Path::new(&self.project);
            for source in &additions {
                let logical = sysutil::rel(&self.project, source).ok_or_else(|| {
                    anyhow!("Rel: can't make {source} relative to {}", self.project)
                })?;
                registry::expected_file(project, &logical, "", false)?;
                let meta_path = registry::expected_file(
                    project,
                    &format!("{logical}.meta"),
                    "",
                    false,
                )
                .map_err(|error| {
                    anyhow!(
                        "new runtime source requires authored MonoScript .meta: {source}: {error}; the Player keeps its previous code until the script has a .meta with a GUID (script_create writes one, as does the Unity Editor)"
                    )
                })?
                .to_string_lossy()
                .into_owned();
                if !fs::metadata(&meta_path).is_ok_and(|info| info.len() <= 1024 * 1024) {
                    bail!("MonoScript metadata is not bounded: {meta_path}");
                }
                let meta = sys::read(&meta_path)?;
                let guid = registry::meta_guid(&meta)
                    .map_err(|error| anyhow!("new MonoScript {source}: {error}"))?;
                if let Some(previous) = guids.get(&guid).filter(|previous| !previous.is_empty()) {
                    bail!("new MonoScript GUID {guid} is already assigned to {previous}");
                }
                let assembly = self.source_assembly(source, &roots)?;
                guids.insert(guid.clone(), source.clone());
                new_guids.insert(guid.clone(), meta_path.clone());
                candidate.configuration.insert(meta_path, sha256(&meta));
                candidate.manifest.scripts.push(CompileScript {
                    guid,
                    path: source.clone(),
                    assembly: assembly.clone(),
                    ..Default::default()
                });
                for index in 0..candidate.manifest.assemblies.len() {
                    if candidate.manifest.assemblies[index].name != assembly {
                        continue;
                    }
                    let sources = &mut candidate.manifest.assemblies[index].sources;
                    sources.push(source.clone());
                    sources.sort();
                    let sources = sources.clone();
                    let mut config = candidate.configs.value(&assembly);
                    config.sources = sources;
                    candidate.configs.insert(&assembly, config);
                }
            }
            // Asset identities are project-wide, including scenes and editor-only
            // scripts. A new MonoScript must not steal another asset's GUID.
            let mut index = MetadataGuidIndex::new();
            sys::walk_dir(&assets, &mut |entry| {
                let filename = entry.path;
                if entry.is_dir() || !filename.ends_with(".meta") {
                    return Ok(Walk::Continue);
                }
                let identity = index.read(filename)?;
                if let Some(owner) = new_guids.get(&identity.guid)
                    && owner != filename
                {
                    bail!(
                        "new MonoScript GUID {} collides with asset metadata {filename}",
                        identity.guid
                    );
                }
                Ok(Walk::Continue)
            })?;
            // Discovery order cannot change revision identities after a restart.
            candidate
                .manifest
                .scripts
                .sort_by(|a, b| a.path.cmp(&b.path));
        }
        candidate.check_configuration(changed)?;
        Ok(candidate)
    }

    /// Directories owned by prepared asmdef/asmref files.
    pub fn assembly_source_roots(&self) -> Result<BTreeMap<String, String>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Definition {
            #[serde(deserialize_with = "nz")]
            name: String,
            #[serde(deserialize_with = "nz")]
            reference: String,
        }
        let mut roots: BTreeMap<String, String> = BTreeMap::new();
        let mut by_guid: BTreeMap<String, String> = BTreeMap::new();
        let mut assign = |filename: &str, name: String| -> Result<()> {
            let root = sysutil::dir(filename);
            if roots.contains_key(&root) {
                bail!("multiple assembly definitions/references in one source directory: {root}");
            }
            roots.insert(root, name);
            Ok(())
        };
        let mut references = Vec::new();
        for filename in self.configuration.keys() {
            match sysutil::ext(filename) {
                ".asmdef" => {
                    let name = read_json_file::<Option<Definition>>(filename)
                        .ok()
                        .map(|definition| definition.unwrap_or_default().name)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            anyhow!("invalid prepared assembly definition: {filename}")
                        })?;
                    assign(filename, name.clone())?;
                    if let Ok(data) = sys::read(&format!("{filename}.meta")) {
                        match registry::meta_guid(&data) {
                            Ok(guid) if !by_guid.contains_key(&guid) => {
                                by_guid.insert(guid, name);
                            }
                            _ => bail!("invalid or duplicate prepared assembly GUID: {filename}"),
                        }
                    }
                }
                ".asmref" => references.push(filename.clone()),
                _ => {}
            }
        }
        for filename in references {
            let mut name = read_json_file::<Option<Definition>>(&filename)
                .ok()
                .map(|reference| reference.unwrap_or_default().reference)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("invalid prepared assembly reference: {filename}"))?;
            if let Some(guid) = name.strip_prefix("GUID:") {
                name = by_guid.get(&rune_lower(guid)).cloned().unwrap_or_default();
                if name.is_empty() {
                    bail!(
                        "prepared assembly reference has no verified definition GUID: {filename}"
                    );
                }
            }
            assign(&filename, name)?;
        }
        Ok(roots)
    }

    /// The prepared assembly a new source file belongs to.
    pub fn source_assembly(
        &self,
        source: &str,
        roots: &BTreeMap<String, String>,
    ) -> Result<String> {
        let assets = join(&[&self.project, "Assets"]);
        let mut name = String::new();
        let mut folder = sysutil::dir(source);
        while within_directory(&folder, &assets) {
            if let Some(resolved) = roots.get(&folder) {
                name = resolved.clone();
                break;
            }
            if folder == assets {
                break;
            }
            folder = sysutil::dir(&folder);
        }
        if name.is_empty() {
            name = "Assembly-CSharp".into();
            for folder in ["Plugins", "Standard Assets", "Pro Standard Assets"] {
                if within_directory(source, &join(&[&assets, folder])) {
                    name = "Assembly-CSharp-firstpass".into();
                }
            }
        }
        if !self.configs.contains_key(&name) || self.current_dlls.at(&name).is_empty() {
            bail!(
                "new source {source} belongs to {name}, which has no prepared runtime assembly/Mono slot; regenerate source graph"
            );
        }
        Ok(name)
    }
}
