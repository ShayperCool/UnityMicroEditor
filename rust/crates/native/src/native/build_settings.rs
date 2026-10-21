//! What a build reads beside the asset it writes, in a project with a
//! scriptable render pipeline.
//!
//! The pipeline's build code loads the pipeline assets the quality levels
//! and the graphics settings name, their renderers and renderer features,
//! the pipeline's global settings and every volume profile under `Assets`,
//! and derives from them which shader variants a build keeps. A standalone
//! worker loads an asset only if it was imported in the same context, so
//! the coordinator has to find all of them from the sources: by following
//! GUID references from the pipeline assets and the volume profiles through
//! every text asset.
//!
//! The settings objects the graphics settings keep per pipeline (the
//! pipeline's global settings) are staged as they are. What they refer to
//! is not: those are the pipeline's own runtime shaders, textures and debug
//! windows, several of which a standalone worker cannot import, and a
//! build's shader code reads the settings held inside the object, not them.
use super::{
    scripted::scripted_importer,
    support::{lower_ext, read_file, stat},
};
use crate::{playerbake::Unsupported, registry::meta_guid, sysutil};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, LazyLock, Mutex},
};

/// The settings files whose references a build follows.
pub const SETTINGS: [&str; 2] = [
    "ProjectSettings/GraphicsSettings.asset",
    "ProjectSettings/QualitySettings.asset",
];
/// The fields of the two settings files that name a render pipeline asset.
const PIPELINE_FIELDS: [&[u8]; 2] = [b"m_CustomRenderPipeline:", b"customRenderPipeline:"];
/// The class of a volume profile, by its script in the pipeline core package.
const VOLUME_PROFILE: &str =
    "Packages/com.unity.render-pipelines.core/Runtime/Volume/VolumeProfile.cs";
/// A text asset larger than this is not read for references.
const TEXT_LIMIT: i64 = 64 << 20;
const BATCH_LIMIT: usize = 3000;

static SHADER_NAME: LazyLock<regex::bytes::Regex> =
    LazyLock::new(|| regex::bytes::Regex::new(r#"(?s-u)^\s*Shader\s*"([^"\r\n]*)""#).unwrap());
static INCLUDE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(
        r#"(?m-u)^[\t ]*#[\t ]*(?:include|include_with_pragmas)[\t ]*(?:"([^"\r\n]+)"|<([^>\r\n]+)>)"#,
    )
    .unwrap()
});
static GUID: LazyLock<regex::bytes::Regex> =
    LazyLock::new(|| regex::bytes::Regex::new(r"guid:\s*([0-9a-fA-F]{32})").unwrap());

/// The `.shader` sources that declare each shader name.
type ShaderNames = Arc<HashMap<String, Vec<String>>>;

/// GUID to logical path, and back.
#[derive(Default)]
struct Index {
    paths: HashMap<String, String>,
    guids: HashMap<String, String>,
}

/// The index of an immutable package, by its directory. Such a directory is
/// named after the package's content.
static PACKAGES: LazyLock<Mutex<HashMap<String, Arc<Index>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn walk(directory: &str, logical: &str, index: &mut Index) -> Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => bail!(sysutil::Error::os("read", directory, &error)),
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // What the Editor does not import.
        if name.starts_with('.') || name.ends_with('~') {
            continue;
        }
        let physical = sysutil::join(&[directory, &name]);
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&physical, &format!("{logical}/{name}"), index)?;
        } else if let Some(asset) = name.strip_suffix(".meta") {
            use std::io::Read;
            let mut head = Vec::with_capacity(512);
            if std::fs::File::open(&physical)
                .and_then(|file| file.take(512).read_to_end(&mut head))
                .is_err()
            {
                continue;
            }
            if let Ok(guid) = meta_guid(&head) {
                let path = format!("{logical}/{asset}");
                index.guids.insert(path.clone(), guid.clone());
                index.paths.insert(guid, path);
            }
        }
    }
    Ok(())
}

/// The sources of a project and of the packages its Editor compilation
/// found.
pub struct Sources {
    project: String,
    assets: Index,
    packages: Vec<Arc<Index>>,
    /// `Packages/<name>` to the directory that holds the package.
    directories: BTreeMap<String, String>,
    /// The `.shader` sources by the name they declare.
    shader_names: Mutex<Option<ShaderNames>>,
}

impl Sources {
    /// `library` is the project whose `Library` holds the package cache and
    /// `packages` maps each `Packages/<name>` to its directory, relative to
    /// that project for a cached package and to `project` for an embedded
    /// one. A package that lies elsewhere is left out: nothing in it can
    /// be staged.
    pub fn open(project: &str, library: &str, packages: &BTreeMap<String, String>) -> Result<Self> {
        let mut assets = Index::default();
        walk(&sysutil::join(&[project, "Assets"]), "Assets", &mut assets)?;
        let mut sources = Self {
            project: project.to_string(),
            assets,
            packages: Vec::new(),
            directories: BTreeMap::new(),
            shader_names: Mutex::new(None),
        };
        for (root, directory) in packages {
            let (physical, immutable) = if directory.starts_with("Library/PackageCache/") {
                (sysutil::join(&[library, directory]), true)
            } else if directory.starts_with("Packages/") {
                (sysutil::join(&[project, directory]), false)
            } else {
                continue;
            };
            let cached = PACKAGES.lock().unwrap().get(&physical).cloned();
            let index = match cached.filter(|_| immutable) {
                Some(index) => index,
                None => {
                    let mut index = Index::default();
                    walk(&physical, root, &mut index)?;
                    let index = Arc::new(index);
                    if immutable {
                        PACKAGES
                            .lock()
                            .unwrap()
                            .insert(physical.clone(), index.clone());
                    }
                    index
                }
            };
            sources.packages.push(index);
            sources.directories.insert(root.clone(), physical);
        }
        Ok(sources)
    }

    pub fn path_of(&self, guid: &str) -> Option<&str> {
        let guid = guid.to_ascii_lowercase();
        std::iter::once(&self.assets)
            .chain(self.packages.iter().map(Arc::as_ref))
            .find_map(|index| index.paths.get(&guid))
            .map(String::as_str)
    }

    pub fn guid_of(&self, path: &str) -> Option<&str> {
        std::iter::once(&self.assets)
            .chain(self.packages.iter().map(Arc::as_ref))
            .find_map(|index| index.guids.get(path))
            .map(String::as_str)
    }

    /// Where a logical path lies.
    pub fn physical(&self, logical: &str) -> Option<String> {
        if logical.starts_with("Assets/") || logical.starts_with("ProjectSettings/") {
            return Some(sysutil::join(&[&self.project, logical]));
        }
        self.directories.iter().find_map(|(root, directory)| {
            logical
                .strip_prefix(root.as_str())
                .filter(|rest| rest.starts_with('/'))
                .map(|rest| format!("{directory}{rest}"))
        })
    }

    pub fn read(&self, logical: &str) -> Result<Vec<u8>> {
        match self.physical(logical) {
            Some(physical) => read_file(&physical),
            None => bail!("source {logical} lies in a package outside the project"),
        }
    }

    /// The `.shader` sources that declare a shader name, in path order.
    pub fn shaders_named(&self, name: &str) -> Result<Vec<String>> {
        let mut names = self.shader_names.lock().unwrap();
        if names.is_none() {
            let mut table: HashMap<String, Vec<String>> = HashMap::new();
            let mut paths: Vec<&String> = std::iter::once(&self.assets)
                .chain(self.packages.iter().map(Arc::as_ref))
                .flat_map(|index| index.guids.keys())
                .filter(|path| lower_ext(path) == ".shader")
                .collect();
            paths.sort_unstable();
            for path in paths {
                let Ok(data) = self.read(path) else { continue };
                let Ok(text) = crate::project::shader_without_comments(&data) else {
                    continue;
                };
                if let Some(found) = SHADER_NAME.captures(&text) {
                    table
                        .entry(String::from_utf8_lossy(&found[1]).into_owned())
                        .or_default()
                        .push(path.clone());
                }
            }
            *names = Some(Arc::new(table));
        }
        Ok(names
            .as_ref()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default())
    }

    /// The files a shader graph's importer reads from the packages while
    /// it writes a shader out: the templates of the graph targets.
    pub fn graph_templates(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .packages
            .iter()
            .flat_map(|index| index.guids.keys())
            .filter(|path| {
                let name = sysutil::base(path).to_ascii_lowercase();
                name.ends_with(".template") || name.contains(".template.")
            })
            .cloned()
            .collect();
        paths.sort_unstable();
        paths
    }

    /// The `.asset` sources under `Assets`, in path order.
    fn authored_assets(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = self
            .assets
            .guids
            .keys()
            .filter(|path| lower_ext(path) == ".asset")
            .map(String::as_str)
            .collect();
        paths.sort_unstable();
        paths
    }
}

/// What a build of the project reads, found from its sources.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildSettings {
    /// The assets the build code loads itself: what the two settings files
    /// refer to and the volume profiles.
    pub needs: Vec<String>,
    /// Those and everything they refer to, each after what it refers to.
    pub batch: Vec<String>,
    /// The scripts of the objects in them, and of the importers of those
    /// that a script imports, by GUID.
    pub scripts: BTreeSet<String>,
    /// The volume profiles under `Assets`, in path order.
    pub volume_profiles: Vec<String>,
    /// The hash of every file read, by logical path.
    pub ingredients: BTreeMap<String, String>,
}

fn text_asset(path: &str) -> bool {
    matches!(
        lower_ext(path).as_str(),
        ".asset" | ".mat" | ".prefab" | ".playable" | ".preset"
    )
}

fn references(data: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for capture in GUID.captures_iter(data) {
        let guid = String::from_utf8_lossy(&capture[1]).to_ascii_lowercase();
        if !found.contains(&guid) {
            found.push(guid);
        }
    }
    found
}

struct Closure<'a> {
    sources: &'a Sources,
    settings: BuildSettings,
    seen: BTreeSet<String>,
    /// Only the scripts of an asset are followed, not what it refers to.
    shallow: bool,
}

impl Closure<'_> {
    /// Adds an asset after everything it refers to.
    fn visit(&mut self, path: &str, depth: usize) -> Result<()> {
        if !self.seen.insert(path.to_string()) {
            return Ok(());
        }
        if depth > 64 || self.seen.len() > BATCH_LIMIT {
            bail!(Unsupported(format!(
                "render pipeline settings that refer to more than {BATCH_LIMIT} assets"
            )));
        }
        let Some(physical) = self.sources.physical(path) else {
            bail!(Unsupported(format!(
                "render pipeline settings that refer to {path}, which lies in a package outside the project"
            )));
        };
        let info = stat(&physical)?;
        if info.is_dir() {
            return Ok(());
        }
        let meta = read_file(&format!("{physical}.meta"))?;
        self.settings
            .ingredients
            .insert(format!("{path}.meta"), crate::sha256(&meta));
        if let Some(importer) = scripted_importer(&meta) {
            self.settings.scripts.insert(importer);
        }
        if text_asset(path) && info.size <= TEXT_LIMIT {
            let data = read_file(&physical)?;
            self.settings
                .ingredients
                .insert(path.to_string(), crate::sha256(&data));
            if data.starts_with(b"%YAML") {
                for guid in references(&data) {
                    self.follow(&guid, path, depth)?;
                }
            }
        } else {
            self.settings
                .ingredients
                .insert(path.to_string(), crate::util::file_hash(&physical)?);
        }
        self.settings.batch.push(path.to_string());
        Ok(())
    }

    fn follow(&mut self, guid: &str, from: &str, depth: usize) -> Result<()> {
        // A GUID no source has is a reference the Editor finds missing too.
        let Some(target) = self.sources.path_of(guid) else {
            return Ok(());
        };
        let target = target.to_string();
        if self.shallow && lower_ext(&target) != ".cs" {
            return Ok(());
        }
        match lower_ext(&target).as_str() {
            ".cs" => {
                // The scenes of a build decide the variants of 2D lights.
                if target.ends_with("/Renderer2DData.cs") {
                    bail!(Unsupported(format!(
                        "a 2D renderer ({from}), whose light variants a build chooses from its scenes"
                    )));
                }
                if !target.starts_with("Packages/") {
                    bail!(Unsupported(format!(
                        "render pipeline settings that use the project's own script {target} ({from}), whose assembly a standalone worker does not load"
                    )));
                }
                self.settings.scripts.insert(guid.to_string());
                Ok(())
            }
            ".dll" => bail!(Unsupported(format!(
                "render pipeline settings that use a class of the precompiled assembly {target} ({from})"
            ))),
            _ => self.visit(&target, depth + 1),
        }
    }
}

/// Finds what a build of the project reads. `reloaded` lists assets the
/// pipeline's objects load by path when they wake up, which no reference
/// leads to; they come first.
pub fn build_settings(sources: &Sources, reloaded: &[String]) -> Result<BuildSettings> {
    let mut closure = Closure {
        sources,
        settings: BuildSettings::default(),
        seen: BTreeSet::new(),
        shallow: false,
    };
    for path in reloaded {
        closure.visit(path, 0)?;
    }
    // The pipeline assets, which a build reads through to their renderers
    // and renderer features, and the settings objects the graphics settings
    // keep per pipeline, which it reads as they are.
    let (mut roots, mut shallow) = (Vec::new(), Vec::new());
    for settings in SETTINGS {
        let Some(physical) = sources.physical(settings) else {
            continue;
        };
        let data = match read_file(&physical) {
            Ok(data) => data,
            // A file the project lacks holds the engine's defaults.
            Err(error) if super::is_not_exist(&error) => continue,
            Err(error) => return Err(error),
        };
        closure
            .settings
            .ingredients
            .insert(settings.to_string(), crate::sha256(&data));
        for line in data.split(|byte| *byte == b'\n') {
            let pipeline = PIPELINE_FIELDS
                .iter()
                .any(|field| line.windows(field.len()).any(|window| window == *field));
            for guid in references(line) {
                let Some(path) = sources.path_of(&guid) else {
                    continue;
                };
                let list = if pipeline {
                    &mut roots
                } else if lower_ext(path) == ".asset" {
                    &mut shallow
                } else {
                    continue;
                };
                if !list.contains(&path.to_string()) {
                    list.push(path.to_string());
                }
            }
        }
    }
    if let Some(script) = sources.guid_of(VOLUME_PROFILE) {
        let needle = format!("guid: {script}");
        for path in sources.authored_assets() {
            let Some(physical) = sources.physical(path) else {
                continue;
            };
            if !stat(&physical).is_ok_and(|info| info.is_regular() && info.size <= TEXT_LIMIT) {
                continue;
            }
            let data = read_file(&physical)?;
            if data.starts_with(b"%YAML")
                && data
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            {
                closure.settings.volume_profiles.push(path.to_string());
            }
        }
    }
    let profiles = closure.settings.volume_profiles.clone();
    for path in roots.iter().chain(&profiles) {
        closure.visit(path, 0)?;
    }
    for path in &shallow {
        closure.shallow = true;
        closure.visit(path, 0)?;
    }
    for path in roots.iter().chain(&profiles).chain(&shallow) {
        if !closure.settings.needs.contains(path) {
            closure.settings.needs.push(path.clone());
        }
    }
    Ok(closure.settings)
}

/// The files the given shader texts include, directly or through other
/// files, as logical paths in path order. `from` names the file each text
/// stands for, which relative includes start from. Every branch of a
/// conditional is followed. A name no source has (a file of the engine's
/// own include directory) and an include that is not a literal are left to
/// the compiler, which fails on a file it needs and does not find.
pub fn include_closure(sources: &Sources, texts: &[(&str, &[u8])]) -> Result<Vec<String>> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut pending: Vec<(String, Vec<u8>)> = texts
        .iter()
        .map(|(from, text)| (from.to_string(), text.to_vec()))
        .collect();
    let given: BTreeSet<&str> = texts.iter().map(|(from, _)| *from).collect();
    while let Some((from, text)) = pending.pop() {
        let Ok(text) = crate::project::shader_without_comments(&text) else {
            continue;
        };
        for found in INCLUDE.captures_iter(&text) {
            let name = found
                .get(1)
                .or(found.get(2))
                .map_or(&b""[..], |m| m.as_bytes());
            let name = String::from_utf8_lossy(name).into_owned();
            if name.contains('\\') || sysutil::is_abs(&name) {
                continue;
            }
            let logical = if name.starts_with("Assets/") || name.starts_with("Packages/") {
                sysutil::clean(&name)
            } else {
                sysutil::clean(&format!("{}/{name}", sysutil::dir(&from)))
            };
            if !(logical.starts_with("Assets/") || logical.starts_with("Packages/"))
                || seen.contains(&logical)
            {
                continue;
            }
            let Some(physical) = sources.physical(&logical) else {
                continue;
            };
            if !stat(&physical).is_ok_and(|info| info.is_regular() && info.size <= 4 << 20) {
                continue;
            }
            if seen.len() >= 2048 {
                bail!(Unsupported(
                    "a shader that includes more than 2048 files".into()
                ));
            }
            seen.insert(logical.clone());
            pending.push((logical, read_file(&physical)?));
        }
    }
    Ok(seen
        .into_iter()
        .filter(|path| !given.contains(path.as_str()))
        .collect())
}

/// The material sources a material source inherits from, nearest first.
pub fn material_parents(sources: &Sources, material: &[u8]) -> Result<Vec<String>> {
    static PARENT: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
        regex::bytes::Regex::new(r"m_Parent:\s*\{fileID:\s*-?[0-9]+,\s*guid:\s*([0-9a-fA-F]{32})")
            .unwrap()
    });
    let mut parents: Vec<String> = Vec::new();
    let mut data = material.to_vec();
    while let Some(found) = PARENT.captures(&data) {
        let guid = String::from_utf8_lossy(&found[1]).to_ascii_lowercase();
        let Some(path) = sources.path_of(&guid) else {
            bail!(Unsupported(format!(
                "a material variant whose parent {guid} the project has no source for"
            )));
        };
        if parents.len() >= 32 || parents.iter().any(|known| known == path) {
            bail!(Unsupported(format!(
                "a material variant whose chain of parents returns to itself or is longer than 32 ({path})"
            )));
        }
        parents.push(path.to_string());
        data = sources.read(path)?;
    }
    Ok(parents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, path: &str, data: &str, guid: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, data).unwrap();
        std::fs::write(
            format!("{}.meta", file.display()),
            format!("fileFormatVersion: 2\nguid: {guid}\n"),
        )
        .unwrap();
    }

    fn guid(index: u8) -> String {
        format!("{index:02x}").repeat(16)
    }

    fn project(extra: &[(&str, String, String)]) -> (tempfile::TempDir, Sources) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let core = "Library/PackageCache/com.unity.render-pipelines.core@1";
        write(
            root,
            &format!("{core}/Runtime/Volume/VolumeProfile.cs"),
            "class VolumeProfile {}",
            &guid(1),
        );
        write(
            root,
            &format!("{core}/Runtime/Bloom.shader"),
            "Shader \"B\" {}",
            &guid(2),
        );
        write(
            root,
            &format!("{core}/Runtime/Data.cs"),
            "class Data {}",
            &guid(3),
        );
        write(
            root,
            "Assets/Settings/Pipeline.asset",
            &format!(
                "%YAML 1.1\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n  renderer: {{fileID: 11400000, guid: {}, type: 2}}\n  gone: {{fileID: 2, guid: {}, type: 3}}\n",
                guid(3),
                guid(5),
                guid(0x77)
            ),
            &guid(4),
        );
        write(
            root,
            "Assets/Settings/Renderer.asset",
            &format!(
                "%YAML 1.1\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n  shader: {{fileID: 4800000, guid: {}, type: 3}}\n  owner: {{fileID: 11400000, guid: {}, type: 2}}\n",
                guid(3),
                guid(2),
                guid(4)
            ),
            &guid(5),
        );
        write(
            root,
            "Assets/Volumes/Profile.asset",
            &format!(
                "%YAML 1.1\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n",
                guid(1)
            ),
            &guid(6),
        );
        write(
            root,
            "ProjectSettings/QualitySettings.asset",
            &format!(
                "%YAML 1.1\n    customRenderPipeline: {{fileID: 11400000, guid: {}, type: 2}}\n",
                guid(4)
            ),
            &guid(0x70),
        );
        write(
            root,
            &format!("{core}/Runtime/Debug.shader"),
            "Shader \"D\" {}",
            &guid(0x0a),
        );
        write(
            root,
            "Assets/Settings/Global.asset",
            &format!(
                "%YAML 1.1\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n  debug: {{fileID: 4800000, guid: {}, type: 3}}\n",
                guid(0x0b),
                guid(0x0a)
            ),
            &guid(0x0c),
        );
        write(
            root,
            &format!("{core}/Runtime/Global.cs"),
            "class Global {}",
            &guid(0x0b),
        );
        write(
            root,
            "ProjectSettings/GraphicsSettings.asset",
            &format!(
                "%YAML 1.1\n  m_CustomRenderPipeline: {{fileID: 0}}\n  m_SRPDefaultSettings:\n    Pipeline: {{fileID: 11400000, guid: {}, type: 2}}\n",
                guid(0x0c)
            ),
            &guid(0x71),
        );
        for (path, data, id) in extra {
            write(root, path, data, id);
        }
        let project = root.to_str().unwrap().to_string();
        let packages = BTreeMap::from([(
            "Packages/com.unity.render-pipelines.core".to_string(),
            core.to_string(),
        )]);
        let sources = Sources::open(&project, &project, &packages).unwrap();
        (directory, sources)
    }

    #[test]
    fn follows_the_settings_to_everything_a_build_loads() {
        let (_directory, sources) = project(&[]);
        let settings = build_settings(&sources, &[]).unwrap();
        // The renderer and the pipeline refer to each other; each is listed
        // once, the shader before the renderer that names it.
        assert_eq!(
            settings.batch,
            [
                "Packages/com.unity.render-pipelines.core/Runtime/Bloom.shader",
                "Assets/Settings/Renderer.asset",
                "Assets/Settings/Pipeline.asset",
                "Assets/Volumes/Profile.asset",
                // The settings object of the pipeline, without the shader
                // it refers to.
                "Assets/Settings/Global.asset",
            ]
        );
        assert_eq!(
            settings.needs,
            [
                "Assets/Settings/Pipeline.asset",
                "Assets/Volumes/Profile.asset",
                "Assets/Settings/Global.asset"
            ]
        );
        assert_eq!(settings.volume_profiles, ["Assets/Volumes/Profile.asset"]);
        assert_eq!(
            settings.scripts,
            BTreeSet::from([guid(1), guid(3), guid(0x0b)])
        );
        assert!(
            settings
                .ingredients
                .contains_key("ProjectSettings/QualitySettings.asset")
        );
        assert!(
            settings
                .ingredients
                .contains_key("Assets/Settings/Renderer.asset.meta")
        );
    }

    #[test]
    fn refuses_settings_whose_classes_a_worker_cannot_load() {
        let own = format!(
            "%YAML 1.1\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n  feature: {{fileID: 11400000, guid: {}, type: 2}}\n",
            guid(3),
            guid(9)
        );
        let feature = format!(
            "%YAML 1.1\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n",
            guid(8)
        );
        let (_directory, sources) = project(&[
            ("Assets/Settings/Pipeline.asset", own, guid(4)),
            ("Assets/Settings/Feature.asset", feature, guid(9)),
            ("Assets/Code/Feature.cs", "class Feature {}".into(), guid(8)),
        ]);
        let refused = build_settings(&sources, &[]).unwrap_err();
        assert!(refused.is::<Unsupported>());
        assert!(format!("{refused:#}").contains("Assets/Code/Feature.cs"));
    }

    #[test]
    fn lists_the_parents_of_a_material_variant() {
        let parent = "%YAML 1.1\nMaterial:\n  m_Parent: {fileID: 0}\n".to_string();
        let middle = format!(
            "%YAML 1.1\nMaterial:\n  m_Parent: {{fileID: 2100000, guid: {}, type: 2}}\n",
            guid(0x21)
        );
        let (_directory, sources) = project(&[
            ("Assets/M/Base.mat", parent, guid(0x21)),
            ("Assets/M/Middle.mat", middle, guid(0x22)),
        ]);
        let variant = format!(
            "Material:\n  m_Parent: {{fileID: 2100000, guid: {}, type: 2}}\n",
            guid(0x22)
        );
        assert_eq!(
            material_parents(&sources, variant.as_bytes()).unwrap(),
            ["Assets/M/Middle.mat", "Assets/M/Base.mat"]
        );
        let lost = format!(
            "Material:\n  m_Parent: {{fileID: 2100000, guid: {}, type: 2}}\n",
            guid(0x55)
        );
        assert!(
            material_parents(&sources, lost.as_bytes())
                .unwrap_err()
                .is::<Unsupported>()
        );
    }
}
