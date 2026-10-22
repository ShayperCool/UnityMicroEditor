use crate::{
    ENGINE_VERSION,
    cache::Identity,
    hex,
    lmdb::{self, Record, Tables},
    paths::logical_path,
    protocol::{guid_text, guid_wire, put_u32, put_u64},
    sha256,
    spooky::Spooky128,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use xxhash_rust::xxh3::{Xxh3, xxh3_128};
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub path: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub meta: String,
    #[serde(default)]
    pub guid: String,
    #[serde(default)]
    pub directory: bool,
    /// A script or assembly definition the importer's own code was compiled
    /// from. The worker builds its `MonoScript` objects from these.
    #[serde(default)]
    pub script: bool,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub project: PathBuf,
    #[serde(default)]
    pub unity_version: String,
    pub assets: Vec<Asset>,
    /// Package roots that are not at their logical place in the context:
    /// `Packages/<name>` to a directory relative to the project.
    #[serde(default)]
    pub roots: BTreeMap<String, String>,
    /// Serialized Editor singletons the worker reads from the database
    /// instead of `Library`, by file name such as `Library/ScriptMapper`.
    #[serde(default)]
    pub singletons: BTreeMap<String, Vec<u8>>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub database: PathBuf,
    pub sha256: String,
    pub unity_version: String,
    pub guid_count: usize,
    pub table_counts: BTreeMap<String, usize>,
    pub source_hash_algorithm: String,
    pub file_hashes: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub guid_paths: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub source_bytes: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub source_file_count: usize,
}
fn is_zero<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
struct Entry {
    guid: String,
    wire: [u8; 16],
    directory: bool,
    meta: Vec<u8>,
    script: bool,
}
struct FileHash {
    hash: [u8; 16],
    ticks: u64,
    size: u64,
}
pub fn native_hash(data: &[u8]) -> [u8; 16] {
    xxh3_128(data).to_le_bytes()
}
/// The hash a release keys source files and its database records with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentHash {
    /// XXH3-128, seed 0 (6000.6).
    Xxh3,
    /// SpookyHash V2 128, seeds 0 and 0 (6000.0).
    Spooky,
}
impl ContentHash {
    /// The hash of `data`, the low half first, each half little endian.
    pub fn of(self, data: &[u8]) -> [u8; 16] {
        match self {
            Self::Xxh3 => native_hash(data),
            Self::Spooky => crate::spooky::hash128(data),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Xxh3 => "XXH3-128 low64/high64 little endian",
            Self::Spooky => "SpookyV2-128 low64/high64 little endian",
        }
    }
}
enum Hasher {
    Xxh3(Box<Xxh3>),
    Spooky(Box<Spooky128>),
}
impl Hasher {
    fn new(kind: ContentHash) -> Self {
        match kind {
            ContentHash::Xxh3 => Self::Xxh3(Box::new(Xxh3::new())),
            ContentHash::Spooky => Self::Spooky(Box::default()),
        }
    }
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Xxh3(hash) => hash.update(data),
            Self::Spooky(hash) => hash.update(data),
        }
    }
    fn digest(&self) -> [u8; 16] {
        match self {
            Self::Xxh3(hash) => hash.digest128().to_le_bytes(),
            Self::Spooky(hash) => hash.digest(),
        }
    }
}
/// What of a release's `SourceAssetDB` differs between releases. The tables,
/// their keys and value layouts are the same in 6000.0 and 6000.6.
#[derive(Clone, Copy, Debug)]
pub struct Schema {
    /// The release `ProjectVersion.txt` names.
    pub unity_version: &'static str,
    /// The value of the `DBVersion` record.
    pub db_version: [u8; 4],
    /// The hash of source files, `.meta` files and lists of children.
    pub hash: ContentHash,
    /// Whether a file's modification time is recorded to 100 ns, or to the
    /// whole second (6000.0, whose worker compares it with what it reads).
    pub subsecond_times: bool,
}
/// The source database of the verified release.
pub const VERIFIED: Schema = Schema {
    unity_version: ENGINE_VERSION,
    db_version: [0xe1, 0xdc, 0xee, 0x6c],
    hash: ContentHash::Xxh3,
    subsecond_times: true,
};
/// The `Assets` root or the named `Packages/<name>` root of a clean relative logical path.
pub fn project_registry_path(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && !name.contains(['\\', '\0', '\r', '\n'])
            && crate::sysutil::clean(name) == name
            && !name.starts_with('/')
            && name.len() <= 480,
        "invalid source asset path {}",
        crate::jsondoc::quoted_text(name)
    );
    if name == "Assets" || name.starts_with("Assets/") {
        return Ok("Assets".into());
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() >= 2 && parts[0] == "Packages" && !matches!(parts[1], "" | "." | "..") {
        return Ok(parts[..2].join("/"));
    }
    bail!(
        "source asset path must be inside Assets or a named Packages root: {}",
        crate::jsondoc::quoted_text(name)
    )
}
fn root_path(name: &str) -> Result<&str> {
    logical_path(name)?;
    ensure!(name.len() <= 480, "invalid source asset path");
    if name == "Assets" || name.starts_with("Assets/") {
        return Ok("Assets");
    }
    if name.starts_with("Packages/") {
        return Ok(name
            .splitn(3, '/')
            .take(2)
            .last()
            .map(|second| &name[..9 + second.len()])
            .unwrap());
    }
    bail!("source asset path must be inside Assets or a named Packages root: {name:?}")
}
pub fn expected_file(
    project: &Path,
    logical: &str,
    supplied: &str,
    directory: bool,
) -> Result<PathBuf> {
    let expected = project.join(logical);
    if !supplied.is_empty() {
        let actual = crate::paths::lexical_abs(Path::new(supplied))?;
        ensure!(
            actual == expected,
            "registry path differs from worker-visible source: {logical}"
        );
    }
    ensure!(
        expected.canonicalize().ok().as_ref() == Some(&expected),
        "registry source is missing or has symlink ancestors: {logical}"
    );
    let info = fs::symlink_metadata(&expected)?;
    ensure!(
        if directory {
            info.is_dir()
        } else {
            info.is_file()
        },
        "registry source has an unsupported file type: {logical}"
    );
    Ok(expected)
}
fn hash_file(path: &Path, maximum: u64, buffer: &mut [u8], schema: &Schema) -> Result<FileHash> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= maximum,
        "source file exceeds registry bounds"
    );
    let identity = Identity::of(&before);
    let mut hash = Hasher::new(schema.hash);
    let mut size = 0u64;
    loop {
        let count = file.read(buffer)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        ensure!(size <= maximum, "source file grew while hashing");
        hash.update(&buffer[..count]);
    }
    ensure!(
        size == before.len()
            && identity == Identity::of(&file.metadata()?)
            && identity == Identity::of(&fs::symlink_metadata(path)?),
        "source file changed while preparing its registry"
    );
    let fraction = if schema.subsecond_times {
        before.mtime_nsec() as i128 / 100
    } else {
        0
    };
    let ticks = before.mtime() as i128 * 10_000_000 + fraction + 621355968000000000i128;
    Ok(FileHash {
        hash: hash.digest(),
        ticks: u64::try_from(ticks)?,
        size,
    })
}
fn trim_space(bytes: &[u8]) -> &[u8] {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.trim().as_bytes(),
        Err(_) => bytes.trim_ascii(),
    }
}
pub fn meta_guid(data: &[u8]) -> Result<String> {
    let mut guid: &[u8] = b"";
    for line in data.split(|&b| b == b'\n') {
        // The Editor keeps the first of the GUIDs a committed merge
        // conflict leaves.
        if let Some(value) = line.strip_prefix(b"guid:")
            && guid.is_empty()
        {
            guid = trim_space(value);
        }
    }
    let guid = std::str::from_utf8(guid)
        .ok()
        .filter(|guid| guid_wire(guid).is_ok_and(|wire| wire != [0; 16]))
        .context("source .meta must contain one nonzero 32-hex GUID")?;
    Ok(guid.to_ascii_lowercase())
}
/// Unquotes a double-quoted literal with backslash escapes; \x and octal escapes yield raw bytes.
fn unquote(text: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        text.len() >= 2 && text.ends_with(b"\"") && !text.contains(&b'\n'),
        "invalid syntax"
    );
    let mut input = &text[1..text.len() - 1];
    let mut output = Vec::with_capacity(input.len());
    let digits = |input: &[u8], count: usize, radix: u32| -> Result<u32> {
        ensure!(input.len() >= count, "invalid syntax");
        let mut value = 0u32;
        for &digit in &input[..count] {
            let digit = (digit as char).to_digit(radix).context("invalid syntax")?;
            value = value * radix + digit;
        }
        Ok(value)
    };
    while let Some((&first, rest)) = input.split_first() {
        if first == b'"' {
            bail!("invalid syntax");
        }
        if first != b'\\' {
            let width = match std::str::from_utf8(&input[..input.len().min(4)]) {
                Ok(text) => text.chars().next().map_or(1, char::len_utf8),
                Err(error) if error.valid_up_to() > 0 => {
                    std::str::from_utf8(&input[..error.valid_up_to()])?
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8)
                }
                Err(_) => 0,
            };
            if width == 0 {
                output.extend_from_slice("\u{fffd}".as_bytes());
                input = rest;
            } else {
                output.extend_from_slice(&input[..width]);
                input = &input[width..];
            }
            continue;
        }
        let (&escape, rest) = rest.split_first().context("invalid syntax")?;
        input = rest;
        match escape {
            b'a' => output.push(7),
            b'b' => output.push(8),
            b'f' => output.push(12),
            b'n' => output.push(b'\n'),
            b'r' => output.push(b'\r'),
            b't' => output.push(b'\t'),
            b'v' => output.push(11),
            b'\\' | b'"' => output.push(escape),
            b'x' => {
                output.push(digits(input, 2, 16)? as u8);
                input = &input[2..];
            }
            b'u' | b'U' => {
                let count = if escape == b'u' { 4 } else { 8 };
                let value = char::from_u32(digits(input, count, 16)?).context("invalid syntax")?;
                output.extend_from_slice(value.encode_utf8(&mut [0; 4]).as_bytes());
                input = &input[count..];
            }
            b'0'..=b'7' => {
                ensure!(input.len() >= 2, "invalid syntax");
                let value = digits(&[&[escape], &input[..2]].concat(), 3, 8)?;
                ensure!(value <= 255, "invalid syntax");
                output.push(value as u8);
                input = &input[2..];
            }
            _ => bail!("invalid syntax"),
        }
    }
    Ok(output)
}
fn scalar(text: &[u8]) -> Result<Vec<u8>> {
    let text = trim_space(text);
    if text.starts_with(b"\"") {
        return unquote(text);
    }
    if text.starts_with(b"'") {
        ensure!(
            text.len() >= 2 && text.ends_with(b"'"),
            "unterminated single-quoted importer metadata scalar"
        );
        let mut output = Vec::with_capacity(text.len());
        let mut inner = &text[1..text.len() - 1];
        while let Some(index) = inner.windows(2).position(|pair| pair == b"''") {
            output.extend_from_slice(&inner[..=index]);
            inner = &inner[index + 2..];
        }
        output.extend_from_slice(inner);
        return Ok(output);
    }
    ensure!(
        !text.iter().any(|b| matches!(b, 0 | b'\r' | b'\n'))
            && !matches!(text.first(), Some(b'|' | b'>')),
        "unsupported importer metadata scalar"
    );
    Ok(text.to_vec())
}
pub fn meta_properties(meta: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    properties(meta)
}
fn properties(meta: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut result = BTreeMap::new();
    let (mut mono, mut origin) = (false, false);
    for line in meta.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.is_empty() && line[0] != b' ' {
            mono = line == b"MonoImporter:";
            origin = line == b"AssetOrigin:";
        }
        if !line.starts_with(b"  ") || line.starts_with(b"   ") {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(&line[2..colon]) else {
            continue;
        };
        let text = &line[colon + 1..];
        let origin_field = origin
            && matches!(
                name,
                "productId" | "uploadId" | "packageName" | "packageVersion"
            );
        if !matches!(name, "assetBundleName" | "assetBundleVariant")
            && !(mono && matches!(name, "executionOrder" | "userData"))
            && !origin_field
        {
            continue;
        }
        let value = scalar(text).with_context(|| name.to_string())?;
        let mut name = name.to_owned();
        let bytes = if (mono && name == "executionOrder")
            || (origin && matches!(name.as_str(), "productId" | "uploadId"))
        {
            let number: i32 = std::str::from_utf8(&value)
                .ok()
                .and_then(|value| value.parse().ok())
                .context("invalid native int32")?;
            if mono {
                name = "MonoImporter.executionOrder".into();
            }
            number.to_le_bytes().to_vec()
        } else {
            if mono && name == "userData" {
                name = "MonoImporter.userData".into();
            }
            let mut bytes = value;
            bytes.push(0);
            bytes
        };
        result.insert(name, bytes);
    }
    Ok(result)
}
fn types() -> Vec<Record> {
    let mut types: BTreeMap<String, &str> = [
        ("AssetBundleIndex", "i32"),
        ("ImportLogEntriesCount", "pair<i32,i32>"),
        ("MainObjectLocalIdentifierInFile", "i64"),
        ("MonoImporter.defaultReferences", "pair<str,pptr>[]"),
        ("MonoImporter.executionOrder", "i32"),
        ("MonoImporter.icon", "pptr"),
        ("MonoImporter.userData", "str"),
        ("assetBundleName", "str"),
        ("assetBundleVariant", "str"),
        ("assetPath", "str"),
        ("assetReqImport", "bool"),
        ("coreAssetReqImport", "bool"),
        ("importLogFile", "hash128"),
        ("importerErrorFile", "hash128"),
        ("importerOverride", "imp"),
        ("labels", "str[]"),
        ("managedPlugin", "bool"),
        ("nativePluginPath", "str"),
        ("packageName", "str"),
        ("packageVersion", "str"),
        ("productId", "i32"),
        ("scriptCompilationAssetPath", "str"),
        ("uploadId", "i32"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    for i in 0..32 {
        types.insert(format!("ArtifactFilePropDef{i}"), "hash128");
    }
    types
        .into_iter()
        .map(|(key, value)| Record {
            key: format!("{key}\0").into_bytes(),
            value: format!("{value}\0").into_bytes(),
        })
        .collect()
}
fn root_folder(wire: &[u8; 16], logical: &str, physical: &Path) -> Vec<u8> {
    if logical == "Assets" {
        let mut data = wire.to_vec();
        data.extend_from_slice(&[0, 0, 0, 0, 12, 0, 0, 0, 8, 0, 0, 0, 4, 0, 0, 0, 0]);
        return data;
    }
    let mut data = vec![0; 32];
    data[..16].copy_from_slice(wire);
    put_u32(&mut data, 16, 1);
    for (i, text) in [
        "Packages".to_string(),
        logical[9..].to_string(),
        physical.to_string_lossy().into_owned(),
    ]
    .iter()
    .enumerate()
    {
        let field = 20 + 4 * i;
        let relative = data.len() - field;
        put_u32(&mut data, field, relative as u32);
        data.extend_from_slice(text.as_bytes());
        data.push(0);
    }
    data
}
struct Inventory<'a> {
    project: &'a Path,
    schema: &'a Schema,
    roots: &'a BTreeMap<String, String>,
    entries: BTreeMap<String, Entry>,
    cases: HashMap<String, String>,
    hashes: BTreeMap<String, FileHash>,
    buffer: Vec<u8>,
}
impl Inventory<'_> {
    /// Where the context holds a logical path, relative to the project.
    fn physical(&self, logical: &str) -> Result<String> {
        let root = root_path(logical)?;
        Ok(match self.roots.get(root) {
            Some(directory) => format!("{directory}{}", &logical[root.len()..]),
            None => logical.to_string(),
        })
    }
    fn add(&mut self, asset: &Asset) -> Result<()> {
        let root = root_path(&asset.path)?.to_string();
        let physical = self.physical(&asset.path)?;
        for (supplied, expected) in [
            (&asset.source, self.project.join(&physical)),
            (&asset.meta, self.project.join(format!("{physical}.meta"))),
        ] {
            if !supplied.is_empty() {
                ensure!(
                    crate::paths::lexical_abs(Path::new(supplied))? == expected,
                    "registry path differs from worker-visible source: {}",
                    asset.path
                );
            }
        }
        let lower = asset.path.to_ascii_lowercase();
        if let Some(previous) = self.cases.get(&lower) {
            ensure!(
                *previous == asset.path,
                "case-insensitive source paths collide: {previous} and {}",
                asset.path
            );
        }
        if let Some(previous) = self.entries.get(&asset.path) {
            ensure!(
                previous.directory == asset.directory
                    && (asset.guid.is_empty() || previous.guid.eq_ignore_ascii_case(&asset.guid)),
                "conflicting source registry entry: {}",
                asset.path
            );
            if asset.script {
                self.entries.get_mut(&asset.path).unwrap().script = true;
            }
            return Ok(());
        }
        let filename = expected_file(self.project, &physical, &asset.source, asset.directory)?;
        let meta_name = PathBuf::from(format!("{}.meta", filename.display()));
        let (wire, guid, meta) = if asset.path == "Assets" {
            let mut wire = [0; 16];
            wire[8] = 1;
            (wire, guid_text(&wire), Vec::new())
        } else if fs::symlink_metadata(&meta_name)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            && asset.directory
            && asset.meta.is_empty()
        {
            let wire = self
                .schema
                .hash
                .of(format!("unity-worker-directory-v1\0{}", asset.path).as_bytes());
            (wire, guid_text(&wire), Vec::new())
        } else {
            expected_file(
                self.project,
                &format!("{physical}.meta"),
                &asset.meta,
                false,
            )?;
            let digest = hash_file(&meta_name, 16 * 1024 * 1024, &mut self.buffer, self.schema)?;
            let meta = fs::read(&meta_name)?;
            ensure!(
                self.schema.hash.of(&meta) == digest.hash,
                "source .meta changed while preparing registry"
            );
            let guid = meta_guid(&meta).with_context(|| asset.path.clone())?;
            self.hashes.insert(format!("{}.meta", asset.path), digest);
            (guid_wire(&guid)?, guid, meta)
        };
        ensure!(
            asset.guid.is_empty() || asset.guid.eq_ignore_ascii_case(&guid),
            "source registry GUID override differs from original .meta: {}",
            asset.path
        );
        if !asset.directory {
            self.hashes.insert(
                asset.path.clone(),
                hash_file(&filename, 1024 * 1024 * 1024, &mut self.buffer, self.schema)?,
            );
        }
        self.entries.insert(
            asset.path.clone(),
            Entry {
                guid,
                wire,
                directory: asset.directory,
                meta,
                script: asset.script && !asset.directory,
            },
        );
        self.cases.insert(lower, asset.path.clone());
        if asset.path != root {
            self.add(&Asset {
                path: Path::new(&asset.path)
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                source: String::new(),
                meta: String::new(),
                guid: String::new(),
                directory: true,
                script: false,
            })?;
        }
        Ok(())
    }
}
/// Orders names without regard to case, a run of digits by its number.
fn natural_order(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut left, mut right) = (a.as_bytes(), b.as_bytes());
    loop {
        match (left.first(), right.first()) {
            (None, None) => return a.len().cmp(&b.len()),
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |text: &[u8]| text.iter().take_while(|c| c.is_ascii_digit()).count();
                let (m, n) = (run(left), run(right));
                let digits = |text: &[u8]| {
                    let start = text.iter().take_while(|c| **c == b'0').count();
                    text[start..].to_vec()
                };
                let (p, q) = (digits(&left[..m]), digits(&right[..n]));
                let order = p.len().cmp(&q.len()).then_with(|| p.cmp(&q));
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
                (left, right) = (&left[m..], &right[n..]);
            }
            (Some(x), Some(y)) if x != y => return x.cmp(y),
            _ => (left, right) = (&left[1..], &right[1..]),
        }
    }
}
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
/// Atomically publish a file without replacing another coordinator's result.
pub fn publish_new(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().context("destination has no parent")?;
    let temporary = parent.join(format!(
        ".source-registry-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result: Result<()> = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path).context("publish new immutable file")?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    drop(file);
    let cleanup = fs::remove_file(temporary);
    result?;
    cleanup?;
    Ok(())
}
/// Records the content a source of a live context has now in the context's
/// SourceAssetDB, in place, as the Editor does when it sees a file change:
/// a worker refuses to import a file whose time or size differs from its
/// record. `logical` is the source's path in the project.
pub fn update_source(project: &str, logical: &str) -> Result<()> {
    update_source_with(&VERIFIED, project, logical)
}
/// The same for the release `schema` describes.
pub fn update_source_with(schema: &Schema, project: &str, logical: &str) -> Result<()> {
    let physical = Path::new(project).join(logical);
    let mut buffer = vec![0u8; 128 * 1024];
    let digest = hash_file(&physical, 1 << 30, &mut buffer, schema)?;
    let key = format!("{}\0", logical.to_ascii_lowercase()).into_bytes();
    let database = format!("{project}/Library/SourceAssetDB");
    lmdb::commit(&database, |tables| {
        let table = tables
            .get_mut("hash")
            .context("SourceAssetDB has no hash table")?;
        let record = table
            .iter_mut()
            .find(|record| record.key == key)
            .with_context(|| format!("SourceAssetDB has no record of {logical}"))?;
        ensure!(
            record.value.len() >= 32,
            "SourceAssetDB record of {logical} is short"
        );
        record.value[..16].copy_from_slice(&digest.hash);
        put_u64(&mut record.value, 16, digest.ticks);
        put_u64(&mut record.value, 24, digest.size);
        Ok(())
    })?;
    Ok(())
}

/// The source database of a new context of the verified release.
pub fn write(input: Input) -> Result<Report> {
    write_with(&VERIFIED, input)
}
/// The source database of a new context of the release `schema` describes.
pub fn write_with(schema: &Schema, input: Input) -> Result<Report> {
    let version = if input.unity_version.is_empty() {
        schema.unity_version
    } else {
        &input.unity_version
    };
    ensure!(
        version == schema.unity_version && !input.assets.is_empty() && input.assets.len() <= 100000,
        "registry requires supported Unity version and bounded nonempty source asset set"
    );
    let project = crate::paths::lexical_abs(&input.project)?;
    ensure!(
        project.canonicalize().ok().as_ref() == Some(&project)
            && ["/Temp/", "/Library/UnityMicroEditor/"]
                .iter()
                .any(|owned| project.to_string_lossy().contains(owned)),
        "project registry requires a new owned context under Temp or Library/UnityMicroEditor without symlink ancestors"
    );
    ensure!(
        project.is_dir()
            && project.join("Library").is_dir()
            && project.join("Library").canonicalize()? == project.join("Library"),
        "project registry requires an existing owned Library directory"
    );
    let database = project.join("Library/SourceAssetDB");
    ensure!(
        !database.try_exists()? && fs::symlink_metadata(&database).is_err(),
        "refusing to replace an existing source database; use a new stopped context revision"
    );
    for (root, directory) in &input.roots {
        ensure!(
            root != "Assets"
                && root_path(root).is_ok_and(|parsed| parsed == root)
                && logical_path(directory).is_ok()
                && directory.starts_with("Library/PackageCache/"),
            "registry package root must map a named package into Library/PackageCache: {root}"
        );
    }
    let mut inventory = Inventory {
        project: &project,
        schema,
        roots: &input.roots,
        entries: BTreeMap::new(),
        cases: HashMap::new(),
        hashes: BTreeMap::new(),
        buffer: vec![0; 128 * 1024],
    };
    for asset in &input.assets {
        ensure!(
            !asset.path.ends_with(".meta"),
            "provide assets rather than separate .meta registry entries"
        );
        inventory.add(asset)?;
    }
    let mut tables = Tables::new();
    for name in [
        "DBVersion",
        "PropertyIDToType",
        "GuidPropertyIDToProperty",
        "GuidToChildren",
        "GuidToIsDir",
        "GuidToPath",
        "Misc",
        "PathToGuid",
        "RootFolders",
        "SerializedSingletons",
        "hash",
    ] {
        tables.insert(name.into(), Vec::new());
    }
    tables.get_mut("DBVersion").unwrap().push(Record {
        key: b"Version\0".to_vec(),
        value: schema.db_version.to_vec(),
    });
    if !input.roots.is_empty() {
        // The worker reads the root folders again, and with them where a
        // package lies, only when the database states another refresh than
        // the one it started with.
        tables.get_mut("Misc").unwrap().push(Record {
            key: b"refreshVersion\0".to_vec(),
            value: 1u32.to_le_bytes().to_vec(),
        });
    }
    for (name, data) in &input.singletons {
        ensure!(
            logical_path(name).is_ok()
                && (name.starts_with("Library/") || name.starts_with("ProjectSettings/"))
                && !data.is_empty(),
            "registry singleton must be a serialized Library file: {name}"
        );
        tables
            .get_mut("SerializedSingletons")
            .unwrap()
            .push(Record {
                key: format!("mem://{name}\0").into_bytes(),
                value: data.clone(),
            });
    }
    tables.insert("PropertyIDToType".into(), types());
    let mut guid_paths = BTreeMap::new();
    let mut children: BTreeMap<String, Vec<(&str, [u8; 16])>> = BTreeMap::new();
    let zero = FileHash {
        hash: [0; 16],
        ticks: 0,
        size: 0,
    };
    let mut add = |table: &str, key: Vec<u8>, value: Vec<u8>| {
        tables.get_mut(table).unwrap().push(Record { key, value });
    };
    for (name, entry) in &inventory.entries {
        ensure!(
            !guid_paths.contains_key(&entry.guid),
            "source GUID shared by two paths: {} and {name}",
            guid_paths.get(&entry.guid).unwrap_or(&String::new())
        );
        guid_paths.insert(entry.guid.clone(), name.clone());
        add(
            "GuidToPath",
            entry.wire.to_vec(),
            format!("{name}\0").into_bytes(),
        );
        add(
            "GuidToIsDir",
            entry.wire.to_vec(),
            vec![u8::from(entry.directory)],
        );
        let mut mapping = entry.wire.to_vec();
        mapping.extend_from_slice(
            &inventory
                .hashes
                .get(&format!("{name}.meta"))
                .unwrap_or(&zero)
                .hash,
        );
        mapping.extend_from_slice(&inventory.hashes.get(name).unwrap_or(&zero).hash);
        add(
            "PathToGuid",
            format!("{}\0", name.to_ascii_lowercase()).into_bytes(),
            mapping,
        );
        let mut props = properties(&entry.meta).with_context(|| name.clone())?;
        props.insert("assetPath".into(), format!("{name}\0").into_bytes());
        if entry.script {
            props.insert(
                "scriptCompilationAssetPath".into(),
                format!("{name}\0").into_bytes(),
            );
        }
        for (name, value) in props {
            let mut key = name.into_bytes();
            key.extend_from_slice(&entry.wire);
            add("GuidPropertyIDToProperty", key, value);
        }
        let root = root_path(name)?;
        if name == root {
            add(
                "RootFolders",
                format!("{root}\0").into_bytes(),
                root_folder(
                    &entry.wire,
                    root,
                    &project.join(input.roots.get(root).map_or(root, String::as_str)),
                ),
            );
        } else {
            children
                .entry(
                    Path::new(name)
                        .parent()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                )
                .or_default()
                .push((name.as_str(), entry.wire));
        }
    }
    for (name, mut entries) in children {
        // The Editor lists the children of a folder in the order of its
        // project window and puts the hash of the list before it. A reader
        // takes the first sixteen bytes for that hash, so a list without
        // one loses its first child wherever folders are walked, as an
        // asset search does.
        entries.sort_by(|a, b| natural_order(a.0, b.0));
        let list: Vec<u8> = entries.iter().flat_map(|(_, wire)| *wire).collect();
        let mut value = schema.hash.of(&list).to_vec();
        value.extend_from_slice(&list);
        add(
            "GuidToChildren",
            inventory.entries[&name].wire.to_vec(),
            value,
        );
    }
    for (name, digest) in &inventory.hashes {
        let mut value = vec![0; 40];
        value[..16].copy_from_slice(&digest.hash);
        put_u64(&mut value, 16, digest.ticks);
        put_u64(&mut value, 24, digest.size);
        add(
            "hash",
            format!("{}\0", name.to_ascii_lowercase()).into_bytes(),
            value,
        );
    }
    let data = lmdb::build(&tables)?;
    publish_new(&database, &data)?;
    Ok(Report {
        database,
        sha256: sha256(&data),
        unity_version: version.into(),
        guid_count: inventory.entries.len(),
        table_counts: tables.iter().map(|(k, v)| (k.clone(), v.len())).collect(),
        source_hash_algorithm: schema.hash.name().into(),
        file_hashes: inventory
            .hashes
            .iter()
            .map(|(name, h)| (name.clone(), hex(&h.hash)))
            .collect(),
        guid_paths,
        source_bytes: inventory.hashes.values().map(|h| h.size).sum(),
        source_file_count: inventory.hashes.len(),
    })
}
