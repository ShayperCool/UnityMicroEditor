//! Restores only the external PPtrs already authored by the native UI
//! importer. Rules, selectors, values, UXML attributes and element scopes
//! remain native.
use super::{
    NativeOptions, NativeProjectImporter,
    datastore::Waiter,
    mesh_cpu::read_owned_native_mesh_file,
    support::{context_error, lower_ext, mkdir_all, read_file, wrap},
};
use crate::{
    cancel::{self, Context, Source},
    project::{
        List, NativeContentFile, NativeReplacement, NativeUIBinding, NativeUIBindingProof,
        strconv::{atoi, parse_int},
    },
    sha256, sysutil,
    util::{file_hash, marshal},
};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    sync::LazyLock,
    time::Duration,
};

static UI_DUMP_EXTERNAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^path\(([0-9]+)\): .* GUID: ([0-9a-f]{32}) Type: ([0-9]+)$").unwrap()
});
static UI_DUMP_OBJECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ID: (-?[0-9]+) \(ClassID: 114\) ").unwrap());
static UI_DUMP_MANAGED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^rid (-?[0-9]+) \(class: ").unwrap());

#[derive(Default)]
struct UiDumpFrame {
    indent: usize,
    path: String,
    managed_id: String,
    array: bool,
    next: usize,
    pointer: bool,
    file_index: usize,
}

const STYLE_SHEET: &str = "UnityEngine.UIElements.StyleSheet";

/// Reads a `binary2text` dump of one native UI artifact against its descriptor.
pub fn parse_native_ui_bindings(
    dump: &[u8],
    file: &NativeContentFile,
) -> Result<Vec<NativeUIBinding>> {
    if dump.len() > 16 << 20 {
        bail!("native UI dump exceeds bounds");
    }
    let mut owners: HashMap<&str, bool> = HashMap::new();
    let mut kinds: HashMap<&str, &str> = HashMap::new();
    for object in file.objects.iter() {
        owners.insert(
            &object.file_id,
            object.type_name == STYLE_SHEET
                || object.type_name == "UnityEngine.UIElements.VisualTreeAsset",
        );
        kinds.insert(&object.file_id, &object.type_name);
    }
    let mut externals: HashMap<usize, String> = HashMap::new();
    let mut result = Vec::new();
    let mut frames: Vec<UiDumpFrame> = Vec::new();
    let mut owner = String::new();
    let mut managed_index = 0usize;
    // Lines must be under 1 MiB: they end at "\n" and one trailing "\r" is dropped.
    let mut rest = dump;
    while !rest.is_empty() {
        let (raw, tail) = match rest.iter().position(|&byte| byte == b'\n') {
            Some(end) => (&rest[..end], &rest[end + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        if raw.len() >= 1 << 20 {
            bail!("line too long");
        }
        rest = tail;
        let line = String::from_utf8_lossy(raw.strip_suffix(b"\r").unwrap_or(raw));
        let line = line.as_ref();
        if let Some(m) = UI_DUMP_EXTERNAL.captures(line) {
            let index = atoi(&m[1]).ok().and_then(|n| usize::try_from(n).ok());
            match index {
                Some(index)
                    if index == externals.len() + 1
                        && index <= file.dependencies.len()
                        && file.dependencies[index - 1].guid == m[2] =>
                {
                    externals.insert(index, m[2].to_string());
                }
                _ => bail!("native UI dump external table differs from descriptor"),
            }
            continue;
        }
        if line.starts_with("ID: ") {
            let Some(m) = UI_DUMP_OBJECT.captures(line) else {
                bail!("native UI dump has a non-UI object");
            };
            owner = m[1].to_string();
            if !owners.get(owner.as_str()).copied().unwrap_or(false) {
                bail!("native UI dump owner identity differs from descriptor");
            }
            frames.clear();
            managed_index = 0;
            continue;
        }
        if owner.is_empty() || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start_matches('\t').len();
        let text = line.trim();
        while frames.last().is_some_and(|frame| frame.indent >= indent) {
            frames.pop();
        }
        if let Some(m) = UI_DUMP_MANAGED.captures(text) {
            let id = parse_int(&m[1], 10).map_err(|e| anyhow!("{e}"))?;
            if id < 0 {
                continue;
            }
            if !frames
                .last()
                .is_some_and(|parent| parent.path == "references")
                || managed_index > 16383
            {
                bail!("native UI managed registry layout is unsupported");
            }
            frames.push(UiDumpFrame {
                indent,
                path: format!("references.RefIds[{managed_index}].data"),
                managed_id: m[1].to_string(),
                ..Default::default()
            });
            managed_index += 1;
            continue;
        }
        let Some(marker) = text.rfind(" (") else {
            // binary2text prints the managed object's class name as a wrapper.
            if let Some(parent) = frames.last().filter(|parent| !parent.managed_id.is_empty()) {
                let wrapper = UiDumpFrame {
                    indent,
                    path: parent.path.clone(),
                    managed_id: parent.managed_id.clone(),
                    ..Default::default()
                };
                frames.push(wrapper);
            }
            continue;
        };
        let label = text[..marker].trim();
        let kind = &text[marker + 2..];
        let kind = kind.strip_suffix(')').unwrap_or(kind);
        let (name, value) = match label.find(' ') {
            Some(split) => (&label[..split], label[split + 1..].trim()),
            None => (label, ""),
        };
        if name == "size" {
            let count = atoi(value).ok();
            match (frames.last_mut(), count) {
                (Some(parent), Some(0..=16384)) => {
                    parent.array = true;
                    parent.next = 0;
                }
                _ => bail!("native UI array bounds are invalid"),
            }
            continue;
        }
        if let Some(parent) = frames.last_mut().filter(|parent| parent.pointer)
            && (name == "m_FileID" || name == "m_PathID")
        {
            if name == "m_FileID" {
                match atoi(value).ok().and_then(|n| usize::try_from(n).ok()) {
                    Some(index) if index <= file.dependencies.len() => parent.file_index = index,
                    _ => bail!("native UI PPtr external index is invalid"),
                }
                continue;
            }
            let Ok(id) = parse_int(value, 10) else {
                bail!("native UI PPtr local ID is invalid");
            };
            if id == 0 {
                continue;
            }
            let target = id.to_string();
            if parent.file_index == 0 {
                if kinds.get(target.as_str()) == Some(&STYLE_SHEET) {
                    result.push(NativeUIBinding {
                        owner_file_id: owner.clone(),
                        property_path: parent.path.clone(),
                        target_guid: file.guid.clone(),
                        target_file_id: target,
                        managed_reference_id: parent.managed_id.clone(),
                    });
                }
                continue;
            }
            if file.dependencies[parent.file_index - 1].global {
                continue;
            }
            let Some(guid) = externals.get(&parent.file_index).filter(|g| !g.is_empty()) else {
                bail!("native UI PPtr has no external identity");
            };
            result.push(NativeUIBinding {
                owner_file_id: owner.clone(),
                property_path: parent.path.clone(),
                target_guid: guid.clone(),
                target_file_id: target,
                managed_reference_id: parent.managed_id.clone(),
            });
            if result.len() > 4096 {
                bail!("native UI PPtr count exceeds bounds");
            }
            continue;
        }
        let mut path = name.to_string();
        let mut managed = String::new();
        if let Some(parent) = frames.last_mut() {
            managed = parent.managed_id.clone();
            path = format!("{}.{name}", parent.path);
            if name == "data" {
                if !parent.array {
                    bail!("native UI data item has no array");
                }
                path = format!("{}[{}]", parent.path, parent.next);
                parent.next += 1;
            }
        }
        frames.push(UiDumpFrame {
            indent,
            path,
            managed_id: managed,
            pointer: kind.starts_with("PPtr<"),
            ..Default::default()
        });
    }
    if externals.len() != file.dependencies.len() {
        bail!("native UI dump external table is incomplete");
    }
    Ok(result)
}

struct RemoveOnDrop(String);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs a program under the context with stdout and stderr merged.
fn combined_output(ctx: &Context, program: &str, arguments: &[&str]) -> (Vec<u8>, Result<()>) {
    if let Some(error) = context_error(ctx) {
        return (Vec::new(), Err(error));
    }
    let pipe =
        std::io::pipe().and_then(|(reader, writer)| Ok((reader, writer.try_clone()?, writer)));
    let (mut reader, stdout, stderr) = match pipe {
        Ok(pipe) => pipe,
        Err(error) => {
            return (
                Vec::new(),
                Err(anyhow!("pipe: {}", sysutil::errno_text(&error))),
            );
        }
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    let spawned = command.spawn();
    // The parent's copies of the write end must close before the output ends.
    drop(command);
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return (
                Vec::new(),
                Err(anyhow!(
                    "fork/exec {program}: {}",
                    sysutil::errno_text(&error)
                )),
            );
        }
    };
    let process = Waiter::new(child);
    let collector = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = reader.read_to_end(&mut output);
        output
    });
    let mut interrupted = None;
    if cancel::select(&[Source::Flag(&process.done), Source::Context(ctx)], None) == Some(1) {
        interrupted = context_error(ctx);
        let _ = sysutil::kill(i64::from(process.pid), libc::SIGKILL);
        process.done.wait(None);
    }
    let output = collector.join().unwrap_or_default();
    let result = match (process.exit_error(), interrupted) {
        (Some(exit), _) => Err(anyhow!(exit)),
        (None, Some(error)) => Err(error),
        (None, None) => Ok(()),
    };
    (output, result)
}

impl NativeProjectImporter {
    pub(crate) fn bind_native_ui_references(
        &self,
        ctx: &Context,
        entry: NativeReplacement,
    ) -> Result<NativeReplacement> {
        bind_native_ui_references(&self.opts, ctx, entry)
    }
}

/// Asks the pinned standalone reader to verify the exact file. The small UI
/// files are never sent to an Editor, importer or AssetBundle builder.
pub(crate) fn bind_native_ui_references(
    opts: &NativeOptions,
    ctx: &Context,
    mut entry: NativeReplacement,
) -> Result<NativeReplacement> {
    let profile = lower_ext(&entry.source_path);
    if profile != ".uxml" && profile != ".uss" {
        return Ok(entry);
    }
    let project = &opts.project;
    let data = match read_owned_native_mesh_file(project, &entry.content.path) {
        Ok(data) if sha256(&data) == entry.content.sha256 => data,
        _ => bail!("native UI binding input changed"),
    };
    let root = sysutil::join(&[&opts.output, "native/ui-bindings"]);
    mkdir_all(&root, 0o700)?;
    let name = {
        let temporary = tempfile::Builder::new()
            .prefix(".native-ui-")
            .suffix(".txt")
            .tempfile_in(&root)
            .map_err(|e| anyhow!("open {root}: {}", sysutil::errno_text(&e)))?;
        temporary.path().to_string_lossy().into_owned()
    };
    let _cleanup = RemoveOnDrop(name.clone());
    let bounded = ctx.with_timeout(Duration::from_secs(10));
    let reader = sysutil::join(&[&sysutil::dir(&opts.editor), "Data/Tools/binary2text"]);
    let reader_sha = file_hash(&reader)?;
    let (mut log, outcome) = combined_output(&bounded, &reader, &[&entry.content.path, &name]);
    bounded.cancel();
    if let Err(error) = outcome {
        log.truncate(4096);
        return Err(wrap(
            "native UI standalone reader",
            anyhow!("{error:#}: {}", String::from_utf8_lossy(&log)),
        ));
    }
    if !sysutil::lstat(&name).is_ok_and(|info| info.is_regular() && info.size <= 16 << 20) {
        bail!("native UI reader output exceeds bounds");
    }
    let dump = read_file(&name)?;
    let bindings = parse_native_ui_bindings(&dump, &entry.content)
        .map_err(|e| wrap(&format!("native UI bindings {}", entry.source_path), e))?;
    if !read_owned_native_mesh_file(project, &entry.content.path).is_ok_and(|after| after == data) {
        bail!("native UI input changed during standalone read");
    }
    if !entry.content.ui_bindings.is_empty() && *entry.content.ui_bindings != bindings[..] {
        bail!("native UI cached bindings differ from exact native PPtrs");
    }
    if !file_hash(&reader).is_ok_and(|after| after == reader_sha) {
        bail!("native UI reader changed during extraction");
    }
    // A dump without bindings leaves the list absent, encoded as `null`.
    let bindings: List<NativeUIBinding> = if bindings.is_empty() {
        List::nil()
    } else {
        bindings.into()
    };
    let encoded = marshal(&bindings)?;
    let proof = NativeUIBindingProof {
        schema_version: 1,
        mode: "native-ui-pptr-bindings-v1".into(),
        native_sha256: entry.content.sha256.clone(),
        reader_sha256: reader_sha,
        dump_sha256: sha256(&dump),
        bindings_sha256: sha256(&encoded),
    };
    if entry
        .content
        .ui_binding_proof
        .as_ref()
        .is_some_and(|cached| *cached != proof)
    {
        bail!("native UI cached proof differs from current exact native input/reader");
    }
    entry.content.ui_bindings = bindings;
    entry.content.ui_binding_proof = Some(proof);
    Ok(entry)
}
