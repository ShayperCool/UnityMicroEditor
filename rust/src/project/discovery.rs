//! Bounded discovery of cold native inputs reachable from authored references.
use super::{
    expand::visit_references,
    lexpath,
    native::{valid_native_guid, valid_native_hex, valid_native_id},
    session::{Inner, NativeDiscoveryPack, NeedsImportError, Session, file_identity, needs_import},
    value::{self, Map, Value, get, map_file_id, number},
};
use crate::cache::{FileRecord, Identity};
use crate::runtimeartifact::{audio_profile, text_profile, texture_profile};
use crate::yaml::MAX_TEXT_BYTES;
use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(crate) const NATIVE_REFERENCE_LINE_BYTES: usize = 64 << 10;
/// A cooked AnimationClip may exceed the ordinary text parser bound. Discovery
/// still scans bounded lines and verifies its entire exact source revision.
pub(crate) const MAX_COOKED_NATIVE_DISCOVERY_BYTES: u64 = 1 << 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slice {
    Line,
    /// The buffer filled before a newline.
    Long,
    End,
}

/// Reads newline-terminated slices over a fixed buffer; a line longer than the buffer is returned in pieces.
pub(crate) struct LineReader<R> {
    inner: R,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
    eof: bool,
}
impl<R: Read> LineReader<R> {
    pub fn new(inner: R, size: usize) -> Self {
        Self {
            inner,
            buffer: vec![0; size.max(16)],
            start: 0,
            end: 0,
            eof: false,
        }
    }
    pub fn read_slice(&mut self) -> std::io::Result<(&[u8], Slice)> {
        let mut searched = 0;
        loop {
            if let Some(at) = self.buffer[self.start + searched..self.end]
                .iter()
                .position(|b| *b == b'\n')
            {
                let from = self.start;
                self.start += searched + at + 1;
                return Ok((&self.buffer[from..self.start], Slice::Line));
            }
            searched = self.end - self.start;
            if self.eof {
                let from = self.start;
                self.start = self.end;
                return Ok((&self.buffer[from..self.end], Slice::End));
            }
            if searched >= self.buffer.len() {
                self.start = self.end;
                return Ok((&self.buffer[..], Slice::Long));
            }
            if self.start > 0 {
                self.buffer.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            match self.inner.read(&mut self.buffer[self.end..]) {
                Ok(0) => self.eof = true,
                Ok(n) => self.end += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    /// Discards the buffered bytes and reads the input to its end.
    pub fn drain(&mut self) -> std::io::Result<()> {
        self.start = self.end;
        let mut chunk = vec![0u8; 128 * 1024];
        while self.inner.read(&mut chunk)? != 0 {}
        self.eof = true;
        Ok(())
    }
    fn discard_line(&mut self) -> std::io::Result<()> {
        loop {
            match self.read_slice()?.1 {
                Slice::Long => continue,
                _ => return Ok(()),
            }
        }
    }
}

struct Hashed {
    file: File,
    hash: Sha256,
    remaining: u64,
}
impl Read for Hashed {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let limit = buffer
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.file.read(&mut buffer[..limit])?;
        self.hash.update(&buffer[..n]);
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Trims leading and trailing Unicode white space.
pub(crate) fn trim_space(bytes: &[u8]) -> &[u8] {
    fn space(c: char) -> bool {
        matches!(
            c,
            '\t' | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\r'
                | ' '
                | '\u{85}'
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
        )
    }
    let mut rest = bytes;
    loop {
        let width = (1..=rest.len().min(4)).find(|n| {
            std::str::from_utf8(&rest[..*n]).is_ok_and(|s| s.chars().next().is_some_and(space))
        });
        match width {
            Some(n) => rest = &rest[n..],
            None => break,
        }
    }
    loop {
        let width = (1..=rest.len().min(4)).find(|n| {
            std::str::from_utf8(&rest[rest.len() - n..])
                .is_ok_and(|s| s.chars().count() == 1 && s.chars().next().is_some_and(space))
        });
        match width {
            Some(n) => rest = &rest[..rest.len() - n],
            None => break,
        }
    }
    rest
}

fn native_line_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut line = line;
    let sequence = line.starts_with(b"- ");
    if sequence {
        line = trim_space(&line[2..]);
        if matches!(line.first(), Some(b'{' | b'[')) {
            return Some((b"", line));
        }
    }
    let mut quote = 0u8;
    let mut index = 0;
    while index < line.len() {
        let c = line[index];
        if quote != 0 {
            if quote == b'"' && c == b'\\' {
                index += 2;
                continue;
            }
            if c == quote {
                if quote == b'\'' && line.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                quote = 0;
            }
        } else if c == b'\'' || c == b'"' {
            quote = c;
        } else if c == b':' && matches!(line.get(index + 1), None | Some(b' ' | b'\t')) {
            let key = trim_space(&line[..index]);
            if matches!(key.first(), None | Some(b'\'' | b'"' | b'?')) {
                return None;
            }
            return Some((key, trim_space(&line[index + 1..])));
        }
        index += 1;
    }
    // Canonical Unity arrays also contain scalar items, e.g. font glyph IDs.
    (sequence || line == b"-").then_some((b"", line))
}

fn native_quoted_scalar_complete(value: &[u8]) -> bool {
    let quote = value[0];
    let mut index = 1;
    while index < value.len() {
        if quote == b'"' && value[index] == b'\\' {
            index += 2;
            continue;
        }
        if value[index] != quote {
            index += 1;
            continue;
        }
        if quote == b'\'' && value.get(index + 1) == Some(&b'\'') {
            index += 2;
            continue;
        }
        let rest = trim_space(&value[index + 1..]);
        return rest.is_empty() || rest[0] == b'#';
    }
    false
}

pub(crate) fn collect_native_guids(value: &Value, targets: &mut BTreeSet<String>) {
    visit_references(value, "", &mut |_, reference| {
        let guid = value::text(reference, "guid");
        if map_file_id(reference) != "0" && valid_native_guid(&guid) {
            targets.insert(guid.into_owned());
        }
    });
}

/// Returns the sorted PPtr GUIDs, or `fallback` when the text is not canonical
/// enough for bounded line scanning. Binary inputs yield nothing.
pub(crate) fn scan_native_reference_guids<R: Read>(
    reader: &mut LineReader<R>,
) -> Result<(Vec<String>, bool)> {
    let mut targets = BTreeSet::new();
    let (mut first, mut block_indent) = (true, -1i64);
    loop {
        let (line, status) = reader.read_slice()?;
        let (long, end) = (status == Slice::Long, status == Slice::End);
        if end && line.is_empty() {
            break;
        }
        let mut line = line;
        if first {
            first = false;
            line = line.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(line);
            // Native binary .asset inputs are handled by their normal loader path.
            if !line.starts_with(b"%YAML") && !line.starts_with(b"--- !u!") {
                return Ok((Vec::new(), false));
            }
        }
        let trimmed = trim_space(line);
        let indent = line
            .iter()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count() as i64;
        if block_indent >= 0 && (trimmed.is_empty() || indent > block_indent) {
            if long {
                reader.discard_line()?;
            }
            continue;
        }
        block_indent = -1;
        if trimmed.is_empty()
            || trimmed[0] == b'%'
            || trimmed[0] == b'#'
            || trimmed.starts_with(b"--- !u!")
        {
            continue;
        }
        if !long && std::str::from_utf8(line).is_err() {
            bail!("unsupported binary Unity text asset");
        }
        let Some((key, value)) = native_line_value(trimmed).filter(|(key, _)| *key != b"fileID")
        else {
            return Ok((Vec::new(), true));
        };
        if value.is_empty() {
            continue;
        }
        match value[0] {
            b'|' | b'>' => block_indent = indent,
            b'&' | b'*' | b'!' => return Ok((Vec::new(), true)),
            b'\'' | b'"' => {
                if long || !native_quoted_scalar_complete(value) {
                    return Ok((Vec::new(), true));
                }
            }
            b'{' | b'[' => {
                if long {
                    return Ok((Vec::new(), true));
                }
                // Ordinary vectors/color maps never contain object references.
                let pointer = value.windows(6).any(|w| w == b"fileID")
                    || value.iter().any(|b| matches!(b, b'&' | b'*' | b'!'));
                if pointer {
                    let parsed = std::str::from_utf8(value)
                        .ok()
                        .and_then(|text| crate::yaml::parse_yaml_value(text).ok());
                    let Some(parsed) = parsed else {
                        return Ok((Vec::new(), true));
                    };
                    collect_native_guids(&Value::from_yaml(&parsed), &mut targets);
                    if targets.len() > 32768 {
                        bail!("native PPtr target count exceeds supported bound");
                    }
                }
            }
            _ => {}
        }
        let _ = key;
        if long {
            // Huge canonical unquoted scalar lines are mesh hex buffers. A brace
            // later in a plain scalar is text, not a YAML flow mapping.
            reader.discard_line()?;
        }
        if end {
            break;
        }
    }
    Ok((targets.into_iter().collect(), false))
}

pub(crate) struct LargeNativeDiscoveryProof {
    logical: String,
    source_sha: String,
    observed: HashMap<PathBuf, Option<Identity>>,
}
impl LargeNativeDiscoveryProof {
    fn validate_current(&self, source_sha: &str) -> Result<()> {
        if source_sha != self.source_sha {
            return Err(needs_import(&self.logical));
        }
        for (path, before) in &self.observed {
            if Identity::read(path).ok() != *before || before.is_none() {
                bail!(
                    "cooked native discovery proof changed while scanning, retry: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

impl Inner {
    fn verified_large_native_discovery(
        &mut self,
        path: &str,
        size: u64,
    ) -> Result<LargeNativeDiscoveryProof> {
        // A clip baked in this session is proven by the revision it was baked
        // from, whatever the catalog was built with.
        let baked = self.native_overlays.values().find(|overlay| {
            lexpath::equal_fold(lexpath::ext(&overlay.source_path), ".anim")
                && self.source(&overlay.source_path).ok().as_deref() == Some(path)
        });
        if let Some(baked) = baked
            && size <= MAX_COOKED_NATIVE_DISCOVERY_BYTES
        {
            return Ok(LargeNativeDiscoveryProof {
                logical: baked.source_path.clone(),
                source_sha: baked.source_sha256.clone(),
                observed: HashMap::new(),
            });
        }
        let catalog = &self.catalog;
        if size > MAX_COOKED_NATIVE_DISCOVERY_BYTES
            || !lexpath::equal_fold(lexpath::ext(path), ".anim")
            || !catalog.success
            || !catalog.cooked_registry_verified
            || catalog.contains_scenes
            || catalog.scene_loader != "raw"
            || (catalog.backend != "unity_editor_public_api" && catalog.backend != "unity_editor")
        {
            bail!(
                "unsupported oversized native discovery source without current cooked AnimationClip proof: {path}"
            );
        }
        let (mut logical, mut guid) = (String::new(), String::new());
        for asset in &self.catalog.assets {
            let asset_path = value::text(asset, "path");
            if !lexpath::equal_fold(lexpath::ext(&asset_path), ".anim")
                || self.source(&asset_path).ok().as_deref() != Some(path)
            {
                continue;
            }
            let asset_guid = value::text(asset, "guid");
            if !logical.is_empty() && (logical != asset_path || guid != asset_guid)
                || get(asset, "kind").as_str() != Some("imported")
                || number(get(asset, "classId")) != 74
                || value::text(asset, "typeName") != "UnityEngine.AnimationClip"
                || !valid_native_guid(&asset_guid)
                || !valid_native_id(&value::text(asset, "fileId"))
                || self.guid_of(&asset_path) != asset_guid
            {
                bail!("oversized native discovery AnimationClip identity is invalid: {path}");
            }
            (logical, guid) = (asset_path.into_owned(), asset_guid.into_owned());
        }
        if logical.is_empty() {
            bail!(
                "oversized native discovery source is absent from the cooked AnimationClip inventory: {path}"
            );
        }
        let meta_logical = format!("{logical}.meta");
        let (mut source_sha, mut meta_sha) = (String::new(), String::new());
        for dependency in &self.catalog.source_dependencies {
            if dependency.path == logical {
                if !valid_native_hex(&dependency.sha256, 64)
                    || !source_sha.is_empty() && source_sha != dependency.sha256
                {
                    bail!("oversized native discovery source proof is invalid: {logical}");
                }
                // Another size is another revision, like another hash.
                if dependency.bytes != size as i64 {
                    return Err(needs_import(&logical));
                }
                source_sha = dependency.sha256.clone();
            } else if dependency.path == meta_logical {
                if !valid_native_hex(&dependency.sha256, 64)
                    || !meta_sha.is_empty() && meta_sha != dependency.sha256
                {
                    bail!("oversized native discovery metadata proof is invalid: {logical}");
                }
                meta_sha = dependency.sha256.clone();
            }
        }
        if source_sha.is_empty()
            || meta_sha.is_empty()
            || self.critical_of(&logical) != source_sha
            || self.critical_of(&meta_logical) != meta_sha
        {
            bail!(
                "oversized native discovery source lacks exact cooked source/metadata hashes: {logical}"
            );
        }
        let meta_path = self.source(&meta_logical)?;
        let (meta, actual_meta_sha) = self.read_text(&meta_path)?;
        if actual_meta_sha != meta_sha
            || crate::yaml::metadata_guid(&meta).as_deref() != Some(guid.as_str())
        {
            return Err(needs_import(&meta_logical));
        }
        let mut proof = LargeNativeDiscoveryProof {
            logical: logical.clone(),
            source_sha,
            observed: HashMap::new(),
        };
        let observed = |s: &Inner, path: &str| s.cache.observed.get(Path::new(path)).copied();
        proof
            .observed
            .insert(meta_path.clone().into(), observed(self, &meta_path));
        let baseline_packs: Option<Vec<NativeDiscoveryPack>> =
            get(&self.baseline, "importedAssetPacks")
                .to_json()
                .and_then(|encoded| value::decode::<Option<Vec<NativeDiscoveryPack>>>(&encoded))
                .ok()
                .flatten();
        let packs = self.catalog.packs.clone();
        let baseline_packs = baseline_packs.unwrap_or_default();
        if baseline_packs.is_empty() || baseline_packs.len() != packs.len() || packs.len() > 1024 {
            bail!("oversized native discovery source lacks bound cooked registry packs: {logical}");
        }
        let mut seen = HashSet::new();
        for (pack, baseline) in packs.iter().zip(&baseline_packs) {
            if pack != baseline
                || !super::is_owned_cache_path(&self.project, &pack.bundle_path)
                || !valid_native_hex(&pack.sha256, 64)
                || pack.bytes <= 0
                || pack.registry_asset.is_empty()
                || !seen.insert(pack.bundle_path.clone())
            {
                bail!("oversized native discovery cooked pack descriptor is invalid: {logical}");
            }
            if lexpath::eval_symlinks(&pack.bundle_path).ok().as_deref()
                != Some(pack.bundle_path.as_str())
            {
                bail!(
                    "oversized native discovery cooked pack is missing or symlinked: {}",
                    pack.bundle_path
                );
            }
            if !file_identity(&pack.bundle_path)
                .is_ok_and(|stamp| stamp.is_regular() && stamp.size as i64 == pack.bytes)
            {
                bail!(
                    "oversized native discovery cooked pack size/type differs: {}",
                    pack.bundle_path
                );
            }
            let (_, actual) = self.read(&pack.bundle_path, false)?;
            if actual != pack.sha256 {
                bail!(
                    "oversized native discovery cooked registry pack changed: {}",
                    pack.bundle_path
                );
            }
            proof.observed.insert(
                pack.bundle_path.clone().into(),
                observed(self, &pack.bundle_path),
            );
        }
        Ok(proof)
    }

    /// Discovery only needs PPtr targets. The current source is hashed while
    /// bounded lines are scanned; noncanonical YAML uses the full parser
    /// without retaining its documents in the session cache.
    pub(crate) fn native_reference_guids(&mut self, path: &str) -> Result<Vec<String>> {
        let before = file_identity(path)?;
        if !before.is_regular() {
            bail!("unsupported nonregular or oversized native discovery source: {path}");
        }
        let mut large_proof = None;
        let mut byte_limit = MAX_TEXT_BYTES as u64;
        if before.size > byte_limit {
            large_proof = Some(self.verified_large_native_discovery(path, before.size)?);
            byte_limit = before.size;
        }
        let key = PathBuf::from(path);
        if self.cache.observed.get(&key).is_some_and(|p| *p != before) {
            bail!("source changed during native discovery, retry: {path}");
        }
        self.cache.observed.insert(key.clone(), before);
        let file = File::open(path).map_err(|e| lexpath::os_error("open", path, e))?;
        let opened = file.metadata()?;
        let mut reader = LineReader::new(
            Hashed {
                file,
                hash: Sha256::new(),
                remaining: byte_limit + 1,
            },
            NATIVE_REFERENCE_LINE_BYTES,
        );
        let (guids, fallback) =
            scan_native_reference_guids(&mut reader).map_err(|e| e.context(path.to_string()))?;
        // A binary source or an early fallback must still get its entire current hash.
        reader.drain()?;
        if reader.inner.remaining == 0 {
            bail!("native discovery source grew beyond its byte bound: {path}");
        }
        let after = file_identity(path)?;
        if before != after || opened.dev() != after.device || opened.ino() != after.inode {
            bail!("source changed while scanning native dependencies, retry: {path}");
        }
        let digest = crate::hex(&reader.inner.hash.finalize());
        if let Some(proof) = &large_proof {
            proof.validate_current(&digest)?;
            if fallback {
                bail!(
                    "oversized cooked AnimationClip requires canonical bounded reference scanning; full YAML fallback is unsupported: {path}"
                );
            }
        }
        // Keep an existing text cache only if it already belongs to this revision.
        let content = self
            .cache
            .files
            .get(&key)
            .filter(|old| old.identity == after)
            .and_then(|old| old.content.clone());
        self.cache.files.insert(
            key.clone(),
            FileRecord {
                identity: after,
                content,
                digest,
            },
        );
        self.cache.stats.file_reads += 1;
        self.cache.stats.bytes_read += before.size;
        if !fallback {
            return Ok(guids);
        }
        let (content, _) = self.read_text(path)?;
        // The fallback retains no additional source text or parsed arrays.
        if let Some(record) = self.cache.files.get_mut(&key) {
            record.content = None;
        }
        let documents =
            super::parse_unity_yaml(&content).map_err(|e| e.context(path.to_string()))?;
        let mut targets = BTreeSet::new();
        static NONE: Map = Map::new();
        for document in documents {
            let data = Value::Object(document.data.unwrap_or_else(|| NONE.clone()));
            collect_native_guids(&data, &mut targets);
        }
        Ok(targets.into_iter().collect())
    }

    fn needs_native_import(&mut self, logical: &str) -> Result<bool> {
        let physical = self.source(logical)?;
        let (_, source_sha) = self.read(&physical, false)?;
        let meta_logical = format!("{logical}.meta");
        let meta_physical = self.source(&meta_logical)?;
        let (_, meta_sha) = self.read(&meta_physical, true)?;
        let guid = self.guid_of(logical).to_string();
        if let Some(native) = self.native_overlays.get(&guid) {
            return Ok(native.source_sha256 != source_sha || native.meta_sha256 != meta_sha);
        }
        let current =
            self.critical_of(logical) == source_sha && self.critical_of(&meta_logical) == meta_sha;
        if lexpath::equal_fold(lexpath::ext(logical), ".fbx") {
            return Ok(!current || self.models.get(&guid).is_none_or(|d| d.is_empty()));
        }
        // An unchanged source the session or the catalog already serves stays
        // there, whatever form the catalog holds it in (a cubemap, for one).
        if current
            && (self.assets.values().any(|asset| {
                value::text(asset, "guid") == guid
                    && matches!(number(get(asset, "classId")), 83 | 28 | 49)
            }) || self
                .catalog
                .assets
                .iter()
                .any(|asset| value::text(asset, "guid") == guid))
        {
            return Ok(false);
        }
        // The formats the raw decoder profile may serve without an import.
        if matches!(
            lexpath::lower_ext(logical).as_str(),
            ".png" | ".jpg" | ".jpeg"
        ) {
            let mut probe = value::map([("path", logical.into())]);
            match self.refresh_authored_texture(&mut probe) {
                Ok(()) => return Ok(false),
                Err(error) if error.downcast_ref::<NeedsImportError>().is_none() => {
                    return Err(error);
                }
                Err(_) => {}
            }
        }
        Ok(true)
    }

    fn pending_native_import_sources(&mut self) -> Result<Vec<String>> {
        self.index_guids()?;
        let mut queue: VecDeque<String> = VecDeque::from([self.baseline_text("scenePath")]);
        queue.extend(self.extra_assets.iter().cloned());
        let (mut seen, mut heavy) = (HashSet::new(), BTreeSet::new());
        // Models a controller refers to supply clips, not geometry: their
        // clips are baked for the controller, and the model is imported only
        // if something else needs it.
        let mut clip_models = BTreeSet::new();
        // A mask is baked into the layers of its controllers.
        let mut masks = BTreeSet::new();
        while let Some(logical) = queue.pop_front() {
            if !seen.insert(logical.clone()) {
                continue;
            }
            if seen.len() > 32768 || queue.len() > 32768 {
                bail!("authored native dependency closure exceeds supported source bound");
            }
            match lexpath::lower_ext(&logical).as_str() {
                ".fbx" => {
                    heavy.insert(logical);
                    continue;
                }
                extension
                    if texture_profile(extension.trim_start_matches('.'))
                        || audio_profile(extension.trim_start_matches('.'))
                        || text_profile(extension.trim_start_matches('.')) =>
                {
                    heavy.insert(logical);
                    continue;
                }
                ".unity"
                | ".prefab"
                | ".asset"
                | ".mat"
                | ".controller"
                | ".overridecontroller"
                | ".anim" => {}
                _ => continue,
            }
            let plays_clips = matches!(
                lexpath::lower_ext(&logical).as_str(),
                ".controller" | ".overridecontroller"
            );
            let physical = self.source(&logical)?;
            for guid in self.native_reference_guids(&physical)? {
                let source = self.path_of(&guid);
                if source.is_empty() {
                    continue;
                }
                if plays_clips && lexpath::lower_ext(source) == ".fbx" {
                    clip_models.insert(source.to_string());
                } else if plays_clips && lexpath::lower_ext(source) == ".mask" {
                    masks.insert(source.to_string());
                } else if !seen.contains(source) {
                    queue.push_back(source.to_string());
                }
            }
        }
        let mut changed_clip_models = Vec::new();
        for logical in clip_models.difference(&heavy) {
            if self.needs_native_import(logical)? {
                changed_clip_models.push(logical.clone());
            }
        }
        self.changed_clip_models = changed_clip_models;
        self.controller_masks = masks.into_iter().collect();
        let mut pending = Vec::new();
        for logical in heavy {
            if self.needs_native_import(&logical)? {
                pending.push(logical);
            }
        }
        Ok(pending)
    }
}

/// An avatar mask a controller refers to, as it is now.
pub struct ControllerMask {
    pub guid: String,
    pub path: String,
    pub source_sha256: String,
    pub meta_sha256: String,
    pub in_catalog: bool,
}

impl Session {
    /// Walks authored references before model expansion and returns only the
    /// reachable cold-profile inputs lacking current native data.
    pub fn pending_native_import_sources(&self) -> Result<Vec<String>> {
        let mut s = self.lock();
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let dependencies = std::mem::take(&mut s.dependencies);
        let result = s.pending_native_import_sources();
        s.cache.observed = observed;
        s.cache.stats = stats;
        s.dependencies = dependencies;
        result.map_err(|e| anyhow!(e))
    }

    /// The models the last walk found to be new or changed that only
    /// controllers refer to, each with its GUID. Their clips reach the Player
    /// through the controllers baked from them.
    pub fn changed_clip_models(&self) -> Vec<(String, String)> {
        let s = self.lock();
        s.changed_clip_models
            .iter()
            .map(|path| (s.guid_of(path).to_string(), path.clone()))
            .collect()
    }

    /// The avatar masks the controllers of the last walk refer to: GUID, path,
    /// the current hashes of the source and of its metadata, and whether the
    /// catalog was built from these.
    pub fn controller_masks(&self) -> Result<Vec<ControllerMask>> {
        let mut s = self.lock();
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let result = s.current_controller_masks();
        s.cache.observed = observed;
        s.cache.stats = stats;
        result
    }
}

impl Inner {
    fn current_controller_masks(&mut self) -> Result<Vec<ControllerMask>> {
        let s = self;
        let mut masks = Vec::new();
        for path in s.controller_masks.clone() {
            let meta_path = format!("{path}.meta");
            let physical = s.source(&path)?;
            let (_, source_sha256) = s.read(&physical, false)?;
            let physical = s.source(&meta_path)?;
            let (_, meta_sha256) = s.read(&physical, true)?;
            masks.push(ControllerMask {
                guid: s.guid_of(&path).to_string(),
                in_catalog: s.native_catalog_source_matches(&path, &source_sha256)
                    && s.native_catalog_source_matches(&meta_path, &meta_sha256),
                path,
                source_sha256,
                meta_sha256,
            });
        }
        Ok(masks)
    }
}
