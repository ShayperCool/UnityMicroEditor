//! Native content a session publishes beside its scene: the shaders it
//! builds, validated against their sources and pinned by their hashes.
use super::{
    expand::builtin_guid,
    lexpath,
    session::{Inner, Session},
    strconv,
    types::{NativeContentFile, NativeReplacement},
};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The model formats a native import serves, by lower-case extension with
/// its dot.
pub(crate) fn model_profile(extension: &str) -> bool {
    matches!(extension, ".fbx" | ".obj" | ".dae" | ".blend")
}

pub(crate) fn valid_native_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub(crate) fn valid_native_guid(guid: &str) -> bool {
    valid_native_hex(guid, 32) && guid.bytes().any(|c| c != b'0') && !builtin_guid(guid)
}
pub(crate) fn valid_native_id(id: &str) -> bool {
    strconv::parse_int(id, 10).is_ok_and(|n| n != 0 && n.to_string() == id)
}

pub(crate) fn validate_native_dependency_graph(
    files: &BTreeMap<String, NativeContentFile>,
) -> Result<()> {
    fn visit(
        guid: &str,
        files: &BTreeMap<String, NativeContentFile>,
        state: &mut HashMap<String, u8>,
    ) -> Result<()> {
        match state.get(guid) {
            Some(1) => bail!("native dependency cycle contains {guid}"),
            Some(2) => return Ok(()),
            _ => {}
        }
        let Some(file) = files.get(guid) else {
            bail!("native dependency {guid} has no runtime descriptor");
        };
        state.insert(guid.into(), 1);
        for dependency in file.dependencies.iter() {
            if !dependency.global {
                visit(&dependency.guid, files, state)?;
            }
        }
        state.insert(guid.into(), 2);
        Ok(())
    }
    let mut state = HashMap::new();
    files
        .keys()
        .try_for_each(|guid| visit(guid, files, &mut state))
}

impl Inner {
    pub(crate) fn verify_native_artifacts(&mut self, file: &NativeContentFile) -> Result<()> {
        self.verify_native_artifact(&file.path, &file.sha256)?;
        let mut seen = HashSet::new();
        for resource in file.resources.iter() {
            if resource.virtual_path != format!("VirtualArtifacts/Primary/{}.resource", file.guid)
                || !seen.insert(&resource.virtual_path)
            {
                bail!("native resource alias is invalid or duplicated");
            }
            self.verify_native_artifact(&resource.path, &resource.sha256)?;
        }
        Ok(())
    }
    fn verify_native_artifact(&mut self, path: &str, hash: &str) -> Result<()> {
        if !lexpath::is_abs(path) || lexpath::clean(path) != path || !valid_native_hex(hash, 64) {
            bail!("native artifact descriptor is invalid: {path}");
        }
        let canonical =
            lexpath::eval_symlinks(path).map_err(|e| super::session::path_error(path, e))?;
        if canonical != path {
            bail!("native artifact is symlinked: {path}");
        }
        // Unity's own import cache is not an artifact store; the tool's owned
        // subtree of Library is.
        let library = lexpath::join(&[&self.project, "Library"]);
        let owned = lexpath::join(&[&library, "UnityMicroEditor"]);
        if (path == library || path.starts_with(&format!("{library}/")))
            && !path.starts_with(&format!("{owned}/"))
        {
            bail!("native artifact must be independent of project Library");
        }
        let (_, actual) = self.read(path, false)?;
        if actual != hash {
            bail!("native artifact changed after import: {path}");
        }
        Ok(())
    }

    /// A shader the session built: its source, its objects (shaders only)
    /// and the files it depends on, which have to be published too.
    fn validate_native_replacement(&mut self, r: &NativeReplacement) -> Result<()> {
        if !valid_native_guid(&r.guid)
            || r.content.guid != r.guid
            || !valid_native_hex(&r.source_sha256, 64)
            || !valid_native_hex(&r.meta_sha256, 64)
        {
            bail!("native replacement source identity is invalid");
        }
        let path = r.source_path.as_str();
        if !(path.starts_with("Assets/") || path.starts_with("Packages/"))
            || lexpath::clean(path) != path
        {
            bail!("native replacement source path is invalid: {path}");
        }
        if !matches!(
            lexpath::lower_ext(path).as_str(),
            ".shader" | ".shadergraph" | ".compute"
        ) {
            bail!("native replacement is not a shader: {path}");
        }
        if r.content.objects.is_empty()
            || r.content.objects.iter().any(|object| {
                !valid_native_id(&object.file_id)
                    || !matches!(
                        object.type_name.as_str(),
                        "UnityEngine.Shader" | "UnityEngine.ComputeShader"
                    )
            })
            || !r.content.resources.is_empty()
        {
            bail!("native shader content is invalid: {path}");
        }
        self.verify_native_artifacts(&r.content)?;
        let mut files: BTreeMap<String, NativeContentFile> = self
            .native_overlays
            .iter()
            .map(|(guid, overlay)| (guid.clone(), overlay.content.clone()))
            .collect();
        files.insert(r.guid.clone(), r.content.clone());
        validate_native_dependency_graph(&files)?;
        self.validate_observed("native file changed during validation")
    }

    fn replace_native_content(&mut self, replacement: &NativeReplacement) -> Result<()> {
        self.index_guids()?;
        self.validate_native_replacement(replacement)?;
        self.critical.insert(
            replacement.source_path.clone(),
            replacement.source_sha256.clone(),
        );
        self.critical.insert(
            format!("{}.meta", replacement.source_path),
            replacement.meta_sha256.clone(),
        );
        self.native_overlays
            .insert(replacement.guid.clone(), replacement.clone());
        Ok(())
    }
}

impl Session {
    /// Accepts a shader the session built. Its source and metadata hashes,
    /// its objects and the files it depends on must all match.
    pub fn replace_native_content(&self, replacement: &NativeReplacement) -> Result<()> {
        let mut s = self.lock();
        // This validation has its own read transaction. Preserve preparation
        // counters and observations while keeping freshly verified file hashes.
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let result = s.replace_native_content(replacement);
        s.cache.observed = observed;
        s.cache.stats = stats;
        result
    }
}
