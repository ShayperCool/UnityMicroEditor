//! The settings dependency closure of the component defaults profile. The caller
//! supplies the freshly verified package snapshot. Only settings dependencies enter
//! the profile; unrelated assets and gameplay sources do not.
use super::{Any, defaults::settings_names, error_text, join};
use crate::{
    authoring::yamlcodec::{self, Kind, Node},
    cancel::Context,
    compiler::{
        METADATA_GUID_MAXIMUM, MetadataGuidIdentity, MetadataGuidIndex, NilMap, Walk, WalkEntry,
        walk_dir,
    },
    hex, jsondoc,
    packages::Snapshot,
    project::parse_unity_yaml,
    sysutil,
    util::agent_canonical,
};
use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::File,
    io::Read,
};

const CLOSURE_FILES: usize = 200000;
const CLOSURE_ASSETS: usize = 4096;
const CLOSURE_BYTES: i64 = 1 << 30;
const CLOSURE_ASSET_BYTES: i64 = 512 << 20;

struct ClosureAsset {
    path: String,
    logical: String,
    meta: MetadataGuidIdentity,
}

fn absent(path: &str) -> bool {
    matches!(sysutil::lstat(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

pub(crate) fn reference_closure(
    ctx: &Context,
    project: &str,
    snapshot: Option<&Snapshot>,
    inputs: &mut NilMap<String>,
) -> Result<()> {
    ctx.err()?;
    agent_canonical(project)?;
    let mut updates: BTreeMap<String, String> = BTreeMap::new();
    let mut pending: BTreeSet<String> = BTreeSet::new();
    for name in settings_names() {
        let logical = format!("ProjectSettings/{name}");
        let Some(expected) = inputs.get(&logical) else {
            continue;
        };
        let path = join(&[project, &logical]);
        if expected == "absent" {
            if !absent(&path) {
                return Err(anyhow!(
                    "component defaults absent settings appeared: {logical}"
                ));
            }
            continue;
        }
        let (content, hash, _) = closure_read(ctx, &path, 32 << 20, true)?;
        if hash != *expected {
            return Err(anyhow!(
                "component defaults settings changed before dependency capture: {logical}"
            ));
        }
        // Headerless settings still contribute their exact byte hash. Parse their
        // plain YAML only for references; a scalar without GUIDs needs no index.
        let headerless = !content.windows(7).any(|window| window == b"--- !u!");
        let refs = closure_references(&content, headerless).map_err(|error| {
            anyhow!(
                "component defaults settings {logical}: {}",
                error_text(&error)
            )
        })?;
        pending.extend(refs);
    }
    // Preserve the existing empty/builtin-only profile without scanning Assets.
    if pending.is_empty() {
        return Ok(());
    }
    let Some(snapshot) =
        snapshot.filter(|snapshot| snapshot.schema_version == 1 && snapshot.project == project)
    else {
        return Err(anyhow!(
            "component defaults closure requires this project's verified package snapshot"
        ));
    };
    let index = closure_index(ctx, project, snapshot)?;
    let mut visited: HashSet<String> = HashSet::new();
    let mut missing: BTreeSet<String> = BTreeSet::new();
    let mut total: i64 = 0;
    while !pending.is_empty() {
        ctx.err()?;
        for guid in std::mem::take(&mut pending) {
            if !visited.insert(guid.clone()) {
                continue;
            }
            if visited.len() > CLOSURE_ASSETS {
                return Err(anyhow!(
                    "component defaults dependency closure exceeds asset bound"
                ));
            }
            let Some(asset) = index.get(&guid) else {
                // Unity can retain unresolved optional settings fields. Capture the
                // observed absence explicitly; a subsequently imported GUID changes
                // this profile and requires fresh public defaults.
                updates.insert(format!("settingsReferenceGuid/{guid}"), "absent".into());
                missing.insert(guid);
                continue;
            };
            let serialized = serialized_asset(&asset.path);
            let (content, hash, size) =
                closure_read(ctx, &asset.path, CLOSURE_ASSET_BYTES, serialized)?;
            let (meta, meta_hash, meta_size) = closure_read(
                ctx,
                &format!("{}.meta", asset.path),
                METADATA_GUID_MAXIMUM,
                true,
            )?;
            if meta_hash != asset.meta.sha256 {
                return Err(anyhow!(
                    "component defaults metadata changed after indexing: {}",
                    asset.logical
                ));
            }
            total += size + meta_size;
            if total > CLOSURE_BYTES {
                return Err(anyhow!(
                    "component defaults dependency closure exceeds byte bound"
                ));
            }
            updates.insert(asset.logical.clone(), hash);
            updates.insert(format!("{}.meta", asset.logical), meta_hash);
            let mut refs = closure_references(&meta, true).map_err(|error| {
                anyhow!(
                    "component defaults metadata {}: {}",
                    asset.logical,
                    error_text(&error)
                )
            })?;
            if serialized {
                refs.extend(closure_references(&content, false).map_err(|error| {
                    anyhow!(
                        "component defaults asset {}: {}",
                        asset.logical,
                        error_text(&error)
                    )
                })?);
            }
            pending.extend(refs.into_iter().filter(|next| !visited.contains(next)));
        }
    }
    // Detect edits while following dependencies and publish the complete map only.
    for (logical, expected) in inputs.iter() {
        if !logical.starts_with("ProjectSettings/") {
            continue;
        }
        let path = join(&[project, logical]);
        if expected == "absent" {
            if !absent(&path) {
                return Err(anyhow!(
                    "component defaults absent settings appeared: {logical}"
                ));
            }
            continue;
        }
        let (_, hash, _) = closure_read(ctx, &path, 32 << 20, false)?;
        if hash != *expected {
            return Err(anyhow!(
                "component defaults settings changed during dependency capture: {logical}"
            ));
        }
    }
    // Visited in GUID order.
    for asset in index.values() {
        let Some(expected) = updates.get(&asset.logical) else {
            continue;
        };
        let (_, hash, _) = closure_read(ctx, &asset.path, CLOSURE_ASSET_BYTES, false)?;
        let (_, meta_hash, _) = closure_read(
            ctx,
            &format!("{}.meta", asset.path),
            METADATA_GUID_MAXIMUM,
            false,
        )?;
        if hash != *expected || updates.get(&format!("{}.meta", asset.logical)) != Some(&meta_hash)
        {
            return Err(anyhow!(
                "component defaults dependency changed during capture: {}",
                asset.logical
            ));
        }
    }
    if !missing.is_empty() {
        let current = closure_index(ctx, project, snapshot)?;
        for guid in &missing {
            if current.contains_key(guid) {
                return Err(anyhow!(
                    "component defaults unresolved settings GUID appeared during capture: {guid}"
                ));
            }
        }
    }
    for (logical, hash) in updates {
        inputs.insert(logical, hash);
    }
    Ok(())
}

/// Every imported asset of the authored and
/// package roots by GUID.
fn closure_index(
    ctx: &Context,
    project: &str,
    snapshot: &Snapshot,
) -> Result<BTreeMap<String, ClosureAsset>> {
    let mut roots = vec![("Assets".to_string(), join(&[project, "Assets"]))];
    let mut packages: Vec<_> = snapshot.packages.iter().collect();
    packages.sort_by(|first, second| first.logical_root.cmp(&second.logical_root));
    let mut seen_roots = HashSet::new();
    for package in packages {
        if package.name.is_empty()
            || package.logical_root != format!("Packages/{}", package.name)
            || package.path.is_empty()
            || package.content_hash.len() != 64
            || !seen_roots.insert(package.logical_root.clone())
        {
            return Err(anyhow!(
                "component defaults package root identity is invalid"
            ));
        }
        roots.push((package.logical_root.clone(), package.path.clone()));
    }
    let mut result: BTreeMap<String, ClosureAsset> = BTreeMap::new();
    let mut reader = MetadataGuidIndex::new();
    let mut files = 0usize;
    let mut metadata_bytes: i64 = 0;
    for (root_logical, root_path) in &roots {
        agent_canonical(root_path)?;
        if !sysutil::lstat(root_path).is_ok_and(|info| info.is_dir()) {
            return Err(anyhow!(
                "component defaults source root is missing: {root_logical}"
            ));
        }
        walk_dir(root_path, &mut |entry: &WalkEntry| {
            ctx.err()?;
            let path = entry.path;
            let name = sysutil::base(path);
            if entry.kind.is_symlink() {
                return Err(anyhow!(
                    "component defaults source index rejects symlink: {path}"
                ));
            }
            if entry.is_dir() {
                if path != root_path && (name.starts_with('.') || name.ends_with('~')) {
                    return Ok(Walk::SkipDir);
                }
                return Ok(Walk::Continue);
            }
            files += 1;
            if files > CLOSURE_FILES {
                return Err(anyhow!("component defaults GUID index exceeds file bound"));
            }
            if !entry.kind.is_file() {
                return Err(anyhow!(
                    "component defaults source index rejects special file: {path}"
                ));
            }
            let Some(stem) = name.strip_suffix(".meta") else {
                return Ok(Walk::Continue);
            };
            let relative = sysutil::rel(root_path, path)
                .ok_or_else(|| anyhow!("Rel: can't make {path} relative to {root_path}"))?;
            // Import-ignored package folders can have a sibling .meta as well.
            if stem.ends_with('~') || name.starts_with('.') {
                return Ok(Walk::Continue);
            }
            let identity = reader.read(path)?;
            metadata_bytes += identity.bytes;
            if metadata_bytes > CLOSURE_BYTES {
                return Err(anyhow!(
                    "component defaults GUID index exceeds metadata byte bound"
                ));
            }
            let logical = format!(
                "{root_logical}/{}",
                relative.strip_suffix(".meta").unwrap_or(&relative)
            );
            if let Some(previous) = result.get(&identity.guid) {
                return Err(anyhow!(
                    "component defaults duplicate GUID {}: {} and {logical}",
                    identity.guid,
                    previous.logical
                ));
            }
            result.insert(
                identity.guid.clone(),
                ClosureAsset {
                    path: path.strip_suffix(".meta").unwrap_or(path).to_string(),
                    logical,
                    meta: identity,
                },
            );
            Ok(Walk::Continue)
        })?;
    }
    Ok(result)
}

/// Binary imported leaves are hashed with
/// their importer metadata. Serialized Unity objects require readable text; a
/// binary object cannot prove its closure.
fn serialized_asset(path: &str) -> bool {
    matches!(
        sysutil::ext(path).to_lowercase().as_str(),
        ".asset"
            | ".mat"
            | ".prefab"
            | ".unity"
            | ".anim"
            | ".controller"
            | ".overridecontroller"
            | ".playable"
            | ".physicmaterial"
            | ".physicsmaterial2d"
            | ".rendertexture"
            | ".lighting"
            | ".shadervariants"
            | ".guiskin"
            | ".fontsettings"
            | ".terrainlayer"
            | ".preset"
            | ".mask"
            | ".flare"
            | ".cubemap"
    )
}

/// The GUIDs a metadata file (plain
/// YAML) or a serialized Unity asset refers to.
pub(crate) fn closure_references(content: &[u8], metadata: bool) -> Result<BTreeSet<String>> {
    let mut refs = BTreeSet::new();
    if metadata {
        let text = std::str::from_utf8(content).map_err(|_| anyhow!("yaml: invalid UTF-8"))?;
        let Some(root) =
            yamlcodec::parse_first_document(text).map_err(|error| anyhow!(error.message))?
        else {
            return Ok(refs);
        };
        // The document node above the root counts as well.
        let mut budget: i64 = 1_000_000 - 1;
        fn visit(
            node: &Node,
            depth: usize,
            budget: &mut i64,
            refs: &mut BTreeSet<String>,
        ) -> Result<()> {
            *budget -= 1;
            if *budget < 0 || depth > 128 {
                return Err(anyhow!(
                    "component defaults metadata YAML exceeds traversal bound"
                ));
            }
            if node.kind == Kind::Mapping {
                let mut seen = HashSet::new();
                for pair in node.content.chunks(2) {
                    let [key, value] = pair else { continue };
                    if key.kind != Kind::Scalar || !seen.insert(key.value.as_str()) {
                        return Err(anyhow!(
                            "component defaults metadata has non-scalar or duplicate mapping key"
                        ));
                    }
                    if key.value == "guid" {
                        if value.kind != Kind::Scalar {
                            return Err(anyhow!("component defaults GUID must be a scalar"));
                        }
                        closure_guid(&value.value, refs)?;
                    }
                }
            }
            for child in &node.content {
                visit(child, depth + 1, budget, refs)?;
            }
            Ok(())
        }
        visit(&root, 1, &mut budget, &mut refs)?;
        return Ok(refs);
    }
    fn walk(value: &Any, refs: &mut BTreeSet<String>) -> Result<()> {
        match value {
            Any::Object(map) => {
                if let Some(guid) = map.get("guid") {
                    let Any::String(text) = guid else {
                        return Err(anyhow!("component defaults GUID must be a string"));
                    };
                    closure_guid(text, refs)?;
                }
                for child in map.values() {
                    walk(child, refs)?;
                }
            }
            Any::Array(items) => {
                for child in items {
                    walk(child, refs)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    for document in parse_unity_yaml(content)? {
        if let Some(data) = document.data {
            walk(&Any::Object(data), &mut refs)?;
        }
    }
    Ok(refs)
}

fn closure_guid(guid: &str, refs: &mut BTreeSet<String>) -> Result<()> {
    if guid.is_empty() {
        return Ok(());
    }
    if guid.len() != 32 || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "component defaults has invalid GUID {}",
            jsondoc::quoted_text(guid)
        ));
    }
    let guid = guid.to_lowercase();
    if guid == "0".repeat(32)
        || guid == "0000000000000000e000000000000000"
        || guid == "0000000000000000f000000000000000"
    {
        return Ok(());
    }
    refs.insert(guid);
    Ok(())
}

/// The content (when retained), digest and
/// size of a file that did not change while it was read.
fn closure_read(
    ctx: &Context,
    path: &str,
    limit: i64,
    retain: bool,
) -> Result<(Vec<u8>, String, i64)> {
    ctx.err()?;
    agent_canonical(path)?;
    let before = match sysutil::lstat(path) {
        Ok(before)
            if before.is_regular()
                && before.size <= limit
                && !(retain && before.size > 32 << 20) =>
        {
            before
        }
        _ => {
            return Err(anyhow!(
                "component defaults expected bounded regular source: {path}"
            ));
        }
    };
    let file = File::open(path).map_err(|error| sysutil::Error::os("open", path, &error))?;
    if !sysutil::fstat(&file).is_ok_and(|opened| before.same_file(&opened)) {
        return Err(anyhow!(
            "component defaults source changed before reading: {path}"
        ));
    }
    let mut hasher = Sha256::new();
    let mut content = Vec::new();
    let mut count: i64 = 0;
    let mut reader = (&file).take(before.size as u64 + 1);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if retain {
            content.extend_from_slice(&buffer[..read]);
        }
        count += read as i64;
    }
    let unchanged = match (sysutil::fstat(&file), sysutil::lstat(path)) {
        (Ok(after), Ok(current)) => {
            agent_canonical(path).is_ok()
                && before.same_file(&after)
                && before.same_file(&current)
                && count == before.size
                && before.size == after.size
                && before.size == current.size
                && before.modified == after.modified
                && before.modified == current.modified
        }
        _ => false,
    };
    if !unchanged {
        return Err(anyhow!(
            "component defaults source changed while reading: {path}"
        ));
    }
    ctx.err()?;
    Ok((content, hex(&hasher.finalize()), count))
}
