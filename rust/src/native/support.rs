//! Error identities, file helpers and number layout shared by the module.
use crate::{
    cancel::{Canceled, Context},
    project::value::json_float,
    sysutil::{self, ErrorKind},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
};

/// The first error identity found anywhere in the cause chain.
pub(crate) fn kind_of(error: &anyhow::Error) -> ErrorKind {
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<sysutil::Error>() {
            if error.kind() != ErrorKind::Other {
                return error.kind();
            }
        } else if let Some(error) = cause.downcast_ref::<Canceled>() {
            return match error {
                Canceled::Canceled => ErrorKind::Canceled,
                Canceled::DeadlineExceeded => ErrorKind::DeadlineExceeded,
            };
        }
    }
    ErrorKind::Other
}
/// True when the error chain says a path does not exist.
pub fn is_not_exist(error: &anyhow::Error) -> bool {
    kind_of(error) == ErrorKind::NotExist
}
/// True when the error chain says the context was canceled.
pub fn is_canceled(error: &anyhow::Error) -> bool {
    kind_of(error) == ErrorKind::Canceled
}
/// The full error text, including its context chain.
pub(crate) fn text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}
/// Prefixes an error as `prefix: error`, keeping its identity.
pub(crate) fn wrap(prefix: &str, error: anyhow::Error) -> anyhow::Error {
    anyhow!(sysutil::Error::with_kind(
        kind_of(&error),
        format!("{prefix}: {}", text(&error))
    ))
}
/// Joins the present errors, one per line; none gives `None` and one is returned as is.
pub(crate) fn join(
    errors: impl IntoIterator<Item = Option<anyhow::Error>>,
) -> Option<anyhow::Error> {
    let mut present: Vec<anyhow::Error> = errors.into_iter().flatten().collect();
    if present.len() <= 1 {
        return present.pop();
    }
    let kind = present
        .iter()
        .map(kind_of)
        .find(|kind| *kind != ErrorKind::Other)
        .unwrap_or(ErrorKind::Other);
    let message = present.iter().map(text).collect::<Vec<_>>().join("\n");
    Some(anyhow!(sysutil::Error::with_kind(kind, message)))
}
/// The context's cancellation or deadline error, if it has ended.
pub(crate) fn context_error(ctx: &Context) -> Option<anyhow::Error> {
    ctx.error().map(|error| anyhow!(error))
}
pub(crate) fn check(ctx: &Context) -> Result<()> {
    context_error(ctx).map_or(Ok(()), Err)
}
/// A path error for a failed call: `<operation> <path>: <reason>`.
pub(crate) fn os_error(operation: &str, path: &str, error: &io::Error) -> anyhow::Error {
    anyhow!(sysutil::Error::os(operation, path, error))
}
pub(crate) fn stat(path: &str) -> Result<sysutil::Stat> {
    sysutil::stat(path).map_err(|e| os_error("stat", path, &e))
}
pub(crate) fn eval_symlinks(path: &str) -> Result<String> {
    sysutil::eval_symlinks(path).map_err(|e| anyhow!(e))
}
/// The absolute, lexically cleaned form of a path.
pub(crate) fn abs(path: &str) -> Result<String> {
    Ok(crate::paths::lexical_abs(std::path::Path::new(path))?
        .to_string_lossy()
        .into_owned())
}
/// Reads a whole file.
pub(crate) fn read_file(path: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|e| os_error("open", path, &e))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| os_error("read", path, &e))?;
    Ok(data)
}
/// Writes a whole file with the given mode, truncating an existing one.
pub(crate) fn write_file(path: &str, data: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|e| os_error("open", path, &e))?;
    file.write_all(data)
        .map_err(|e| os_error("write", path, &e))
}
/// Creates a new file for writing; fails if it already exists.
pub(crate) fn create_new(path: &str, mode: u32) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| os_error("open", path, &e))
}
/// Creates a directory and its parents with the given mode.
pub(crate) fn mkdir_all(path: &str, mode: u32) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .map_err(|e| os_error("mkdir", path, &e))
}
/// Creates one directory with the given mode.
pub(crate) fn mkdir(path: &str, mode: u32) -> Result<()> {
    fs::DirBuilder::new()
        .mode(mode)
        .create(path)
        .map_err(|e| os_error("mkdir", path, &e))
}
/// Reads at most `limit` bytes of an open file.
pub(crate) fn read_limited(file: File, limit: u64, path: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    file.take(limit)
        .read_to_end(&mut data)
        .map_err(|e| os_error("read", path, &e))?;
    Ok(data)
}
/// The lower-cased extension of a path.
pub(crate) fn lower_ext(path: &str) -> String {
    rune_lower(sysutil::ext(path))
}

/// An f64 that encodes as JSON text: shortest round-trip form, exponent outside 1e-6..1e21.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct JsonF64(pub f64);
impl Serialize for JsonF64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let text = json_float(self.0).map_err(S::Error::custom)?;
        RawValue::from_string(text)
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for JsonF64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            Option::<f64>::deserialize(deserializer)?.unwrap_or_default(),
        ))
    }
}

/// One event line: keys sorted, values encoded as compact JSON.
pub(crate) struct Event(std::collections::BTreeMap<&'static str, Box<RawValue>>);
impl Event {
    pub fn new(name: &str) -> Self {
        Self(Default::default()).with("event", &name)
    }
    pub fn with<T: Serialize + ?Sized>(mut self, key: &'static str, value: &T) -> Self {
        let raw = crate::jsondoc::marshal(value)
            .ok()
            .and_then(|data| String::from_utf8(data).ok())
            .and_then(|text| RawValue::from_string(text).ok());
        if let Some(raw) = raw {
            self.0.insert(key, raw);
        }
        self
    }
    pub fn seconds(self, key: &'static str, seconds: f64) -> Self {
        self.with(key, &JsonF64(seconds))
    }
    pub fn emit(self) {
        crate::util::emit_event(&self.0);
    }
}
/// True when `needle` occurs in `haystack`; an empty needle always does.
pub(crate) fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
/// A `null` JSON value leaves a field at its default value.
pub(crate) fn nul<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
/// Reads a file and decodes it as JSON.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let data = read_file(path)?;
    serde_json::from_slice(&data).map_err(|e| anyhow!("{e}"))
}
/// The same for a file whose numbers are hashed as written: `1.0` stays
/// `1.0`.
pub(crate) fn read_json_verbatim<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let data = read_file(path)?;
    crate::project::value::decode_use_number(&String::from_utf8_lossy(&data))
}
/// The text of an entry of a store of native content, whose paths into a
/// store are paths into `store`. A store may have been copied from another
/// checkout of the project (a worktree is filled by reflink): its entries
/// name the files of the store they were written in, which the copy holds
/// under the same names. Every file is checked by its hash where it is used,
/// so one the copy lacks is made again.
pub(crate) fn store_entry_text(path: &str, store: &str) -> Result<String> {
    static STORED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#""[^"]*/Library/UnityMicroEditor/native-store/"#).expect("store path")
    });
    let data = read_file(path)?;
    let text = String::from_utf8_lossy(&data);
    let quoted = serde_json::to_string(&format!("{store}/"))?;
    let prefix = &quoted[..quoted.len() - 1];
    Ok(STORED
        .replace_all(&text, regex::NoExpand(prefix))
        .into_owned())
}
/// Per-character simple lower-casing, without Unicode's context-sensitive
/// special cases.
pub(crate) fn rune_lower(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{130}' => 'i',
            c => c.to_lowercase().next().unwrap_or(c),
        })
        .collect()
}
