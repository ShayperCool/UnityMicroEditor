//! The shaders a host Player holds in its own files.
//!
//! A build copies a built-in shader an asset uses into the file of that
//! asset, and keeps in the Player's `unity_builtin_extra` only the shaders
//! the project always includes. An imported material names its shader by the
//! Editor's path ID, so the importer has to know where the Player it feeds
//! holds a shader of that name; see `runtimeartifact::bind_builtin_shaders`.
use super::support::lower_ext;
use crate::{
    playerbake,
    runtimeartifact::{BuiltinShaderUse, HostShader},
    sha256, sysutil,
};
use anyhow::{Context as _, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};

const SHADER: i32 = 48;
const SCRIPT: i32 = 114;
/// A Player's metadata is a table of objects; a compiled shader is at most
/// tens of megabytes.
const MAXIMUM_METADATA: u64 = 256 << 20;
const MAXIMUM_SHADER: u64 = 512 << 20;

/// The built-in shaders a model importer gives a generated material, by path
/// ID in the Editor's `unity_builtin_extra` of the pinned engine. The Editor
/// reports the generated material of an `.obj` and of a `.blend` with the
/// reference `0000000000000000f000000000000000:46` and the name `Standard`.
const GENERATED_MATERIAL_SHADERS: [(i64, &str); 1] = [(46, "Standard")];

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HostShaders {
    /// The path IDs of the Player's `unity_builtin_extra`.
    builtin: BTreeSet<i64>,
    /// Shader name to the files that hold one and its path ID there.
    named: BTreeMap<String, Vec<(String, i64)>>,
    /// Stands for everything a bound material depends on.
    pub identity: String,
}

/// The object table of a Player's serialized file, which has no type trees:
/// path ID, class, start and size of every object.
fn object_table(file: &mut File, path: &str) -> Result<Vec<(i64, i32, u64, u64)>> {
    let mut header = [0u8; 48];
    file.read_exact(&mut header)
        .with_context(|| format!("read {path}"))?;
    let word = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().unwrap());
    let long = |at: usize| u64::from_be_bytes(header[at..at + 8].try_into().unwrap());
    let (metadata_size, size, data_offset) = (word(20) as u64, long(24), long(32));
    if word(8) != 23 || header[16] != 0 || metadata_size > MAXIMUM_METADATA || data_offset > size {
        bail!("{path} is not a little-endian SerializedFile v23");
    }
    let mut metadata = vec![0u8; metadata_size as usize];
    file.read_exact(&mut metadata)
        .with_context(|| format!("read {path}"))?;
    let mut at = 0usize;
    let mut take = |count: usize| -> Result<&[u8]> {
        let end = at
            .checked_add(count)
            .filter(|end| *end <= metadata.len())
            .with_context(|| format!("{path}: metadata exceeds its bounds"))?;
        let bytes = &metadata[at..end];
        at = end;
        Ok(bytes)
    };
    while take(1)?[0] != 0 {} // Engine version.
    take(4)?; // Target platform.
    if take(1)?[0] != 0 {
        bail!("{path} carries type trees, which a Player's file does not");
    }
    let count = u32::from_le_bytes(take(4)?.try_into().unwrap());
    let mut classes = Vec::new();
    for _ in 0..count {
        let class = i32::from_le_bytes(take(4)?.try_into().unwrap());
        // Stripped flag, script index, the script's hash for a script
        // class, the layout hash.
        take(3 + if class == SCRIPT { 32 } else { 16 })?;
        classes.push(class);
    }
    let count = u32::from_le_bytes(take(4)?.try_into().unwrap());
    let mut objects = Vec::new();
    for _ in 0..count {
        // Rows are aligned relative to the file, whose header is 48 bytes.
        while !(48 + at).is_multiple_of(4) {
            at += 1;
        }
        let end = at
            .checked_add(24)
            .filter(|end| *end <= metadata.len())
            .with_context(|| format!("{path}: object table exceeds its bounds"))?;
        let row = &metadata[at..end];
        at = end;
        let class = usize::try_from(u32::from_le_bytes(row[20..24].try_into().unwrap()))
            .ok()
            .and_then(|index| classes.get(index))
            .with_context(|| format!("{path}: object of an unknown type"))?;
        let start = u64::from_le_bytes(row[8..16].try_into().unwrap());
        let length = u32::from_le_bytes(row[16..20].try_into().unwrap()) as u64;
        if start > size - data_offset || length > size - data_offset - start {
            bail!("{path}: object lies outside the file");
        }
        objects.push((
            i64::from_le_bytes(row[..8].try_into().unwrap()),
            *class,
            data_offset + start,
            length,
        ));
    }
    Ok(objects)
}

impl HostShaders {
    /// Reads the shaders of a Player's data directory: those of its
    /// `resources.assets`, `globalgamemanagers.assets` and
    /// `sharedassets<n>.assets` by name, and the path IDs of its
    /// `Resources/unity_builtin_extra`.
    pub(crate) fn scan(data: &str) -> Result<Self> {
        let mut shaders = Self::default();
        let mut identity = String::from("host-shaders-v1");
        let builtin = sysutil::join(&[data, "Resources/unity_builtin_extra"]);
        if let Ok(mut file) = File::open(&builtin) {
            for (path_id, class, _, _) in object_table(&mut file, &builtin)? {
                if class == SHADER {
                    shaders.builtin.insert(path_id);
                    identity.push_str(&format!("\0builtin\0{path_id}"));
                }
            }
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(data).with_context(|| format!("read {data}"))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if lower_ext(&name) == ".assets" {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            let path = sysutil::join(&[data, &name]);
            let mut file = File::open(&path).with_context(|| format!("open {path}"))?;
            for (path_id, class, start, length) in object_table(&mut file, &path)? {
                if class != SHADER {
                    continue;
                }
                if length > MAXIMUM_SHADER {
                    bail!("{path}: shader {path_id} exceeds {MAXIMUM_SHADER} bytes");
                }
                let mut object = vec![0u8; length as usize];
                file.seek(SeekFrom::Start(start))?;
                file.read_exact(&mut object)
                    .with_context(|| format!("read {path}"))?;
                let facts = playerbake::read_shader_facts(&object)
                    .with_context(|| format!("{path}: shader {path_id}"))?;
                identity.push_str(&format!(
                    "\0{}\0{name}\0{path_id}\0{}",
                    facts.name,
                    sha256(&object)
                ));
                shaders
                    .named
                    .entry(facts.name)
                    .or_default()
                    .push((name.clone(), path_id));
            }
        }
        shaders.identity = sha256(identity.as_bytes());
        Ok(shaders)
    }

    /// Where the Player holds the shader of a generated material. Refused:
    /// a shader whose name is not established, one the Player lacks or holds
    /// more than once, and a material with keywords, since the Player's copy
    /// has the variants of its own materials only.
    pub(crate) fn resolve(&self, used: &BuiltinShaderUse) -> Result<HostShader> {
        if self.builtin.contains(&used.shader) {
            return Ok(HostShader::Builtin);
        }
        let Some((_, name)) = GENERATED_MATERIAL_SHADERS
            .iter()
            .find(|(path_id, _)| *path_id == used.shader)
        else {
            bail!(
                "the generated material {} uses the built-in shader with the path ID {}, whose name is not established for this engine",
                used.material,
                used.shader
            );
        };
        let copies = self.named.get(*name).map(Vec::as_slice).unwrap_or_default();
        let [(file, path_id)] = copies else {
            if copies.is_empty() {
                bail!(
                    "the generated material {} uses the built-in shader {name}, which the host Player does not hold; a build would add it",
                    used.material
                );
            }
            bail!(
                "the generated material {} uses the built-in shader {name}, of which the host Player holds {} copies",
                used.material,
                copies.len()
            );
        };
        if !used.keywords.is_empty() {
            bail!(
                "the generated material {} enables the keywords {} of the built-in shader {name}; the host Player holds that shader with the variants of its own materials only, and a build would compile these",
                used.material,
                used.keywords.join(", ")
            );
        }
        Ok(HostShader::Copy {
            file: file.clone(),
            path_id: *path_id,
        })
    }
}

/// The shaders of a host Player, read once per Player and state of its data
/// directory. An empty path stands for a caller that has no Player.
pub(crate) fn host_shaders(player: &str) -> Result<Arc<HostShaders>> {
    static KNOWN: Mutex<BTreeMap<String, Arc<HostShaders>>> = Mutex::new(BTreeMap::new());
    if player.is_empty() {
        bail!(
            "no host Player is known, whose files hold the built-in shaders of generated materials"
        );
    }
    let data = crate::compiler::player_data(player);
    // The files of a host do not change while it is used; a rebuilt host is
    // another directory or other files.
    let mut state = String::new();
    let mut entries: Vec<_> = std::fs::read_dir(&data)
        .with_context(|| format!("read {data}"))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            state.push_str(&format!(
                "{}\0{}\0{:?}\0",
                entry.file_name().to_string_lossy(),
                metadata.len(),
                metadata.modified().ok()
            ));
        }
    }
    let key = format!("{data}\0{}", sha256(state.as_bytes()));
    let mut known = KNOWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(shaders) = known.get(&key) {
        return Ok(shaders.clone());
    }
    let shaders = Arc::new(HostShaders::scan(&data)?);
    known.insert(key, shaders.clone());
    Ok(shaders)
}

/// Whether the graphics or quality settings of a project name a scriptable
/// render pipeline asset; a settings file the project lacks names none.
pub(crate) fn names_render_pipeline(project: &str) -> bool {
    let read = |name: &str| {
        std::fs::read_to_string(sysutil::join(&[project, "ProjectSettings", name]))
            .unwrap_or_default()
    };
    playerbake::names_render_pipeline(
        &read("GraphicsSettings.asset"),
        &read("QualitySettings.asset"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(shader: i64, keywords: &[&str]) -> BuiltinShaderUse {
        BuiltinShaderUse {
            material: "defaultMat".into(),
            shader,
            keywords: keywords.iter().map(|keyword| keyword.to_string()).collect(),
        }
    }

    fn host(copies: &[(&str, i64)]) -> HostShaders {
        HostShaders {
            builtin: BTreeSet::from([7, 10753]),
            named: BTreeMap::from([(
                "Standard".to_string(),
                copies
                    .iter()
                    .map(|(file, path_id)| (file.to_string(), *path_id))
                    .collect(),
            )]),
            identity: String::new(),
        }
    }

    #[test]
    fn a_generated_material_resolves_to_the_players_copy_of_its_shader() {
        let shaders = host(&[("resources.assets", 5)]);
        assert_eq!(
            shaders.resolve(&used(46, &[])).unwrap(),
            HostShader::Copy {
                file: "resources.assets".into(),
                path_id: 5
            }
        );
        // A shader the Player keeps among its built-in resources needs no copy.
        assert_eq!(shaders.resolve(&used(7, &[])).unwrap(), HostShader::Builtin);
    }

    #[test]
    fn what_the_player_cannot_show_as_a_build_would_is_refused_by_name() {
        let refusal = |shaders: &HostShaders, used: &BuiltinShaderUse| {
            shaders.resolve(used).unwrap_err().to_string()
        };
        let shaders = host(&[("resources.assets", 5)]);
        assert_eq!(
            refusal(&shaders, &used(45, &[])),
            "the generated material defaultMat uses the built-in shader with the path ID 45, whose name is not established for this engine"
        );
        assert_eq!(
            refusal(&shaders, &used(46, &["_EMISSION", "_NORMALMAP"])),
            "the generated material defaultMat enables the keywords _EMISSION, _NORMALMAP of the built-in shader Standard; the host Player holds that shader with the variants of its own materials only, and a build would compile these"
        );
        assert_eq!(
            refusal(&host(&[]), &used(46, &[])),
            "the generated material defaultMat uses the built-in shader Standard, which the host Player does not hold; a build would add it"
        );
        assert_eq!(
            refusal(
                &host(&[("resources.assets", 5), ("sharedassets0.assets", 9)]),
                &used(46, &[])
            ),
            "the generated material defaultMat uses the built-in shader Standard, of which the host Player holds 2 copies"
        );
    }

    #[test]
    fn no_player_is_a_refusal() {
        assert!(
            host_shaders("")
                .unwrap_err()
                .to_string()
                .starts_with("no host Player is known")
        );
    }

    #[test]
    fn a_players_file_lists_its_objects() {
        // Two types, a script type among them, and two objects.
        let mut metadata = b"6000.6.3f1\0".to_vec();
        metadata.extend_from_slice(&24u32.to_le_bytes());
        metadata.push(0);
        metadata.extend_from_slice(&2u32.to_le_bytes());
        metadata.extend_from_slice(&SCRIPT.to_le_bytes());
        metadata.extend_from_slice(&[0u8; 3 + 32]);
        metadata.extend_from_slice(&SHADER.to_le_bytes());
        metadata.extend_from_slice(&[0u8; 3 + 16]);
        metadata.extend_from_slice(&2u32.to_le_bytes());
        for (path_id, start, length, kind) in [(3i64, 0u64, 8u32, 0u32), (5, 8, 4, 1)] {
            while !(48 + metadata.len()).is_multiple_of(4) {
                metadata.push(0);
            }
            metadata.extend_from_slice(&path_id.to_le_bytes());
            metadata.extend_from_slice(&start.to_le_bytes());
            metadata.extend_from_slice(&length.to_le_bytes());
            metadata.extend_from_slice(&kind.to_le_bytes());
        }
        let mut data_offset = 48 + metadata.len();
        while !data_offset.is_multiple_of(16) {
            data_offset += 1;
        }
        let mut bytes = vec![0u8; 48];
        bytes[8..12].copy_from_slice(&23u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        bytes[24..32].copy_from_slice(&((data_offset + 12) as u64).to_be_bytes());
        bytes[32..40].copy_from_slice(&(data_offset as u64).to_be_bytes());
        bytes.extend_from_slice(&metadata);
        bytes.resize(data_offset + 12, 0);
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("resources.assets");
        std::fs::write(&path, &bytes).unwrap();
        let path = path.to_string_lossy().into_owned();
        let objects = object_table(&mut File::open(&path).unwrap(), &path).unwrap();
        assert_eq!(
            objects,
            [
                (3, SCRIPT, data_offset as u64, 8),
                (5, SHADER, data_offset as u64 + 8, 4)
            ]
        );
        // A header that lies about the metadata is refused.
        let mut broken = bytes.clone();
        broken[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(&path, &broken).unwrap();
        assert!(object_table(&mut File::open(&path).unwrap(), &path).is_err());
    }
}
