//! Retained engine shader snapshots pinned next to authored imports.
use super::{
    lexpath,
    native::{valid_native_hex, valid_native_id},
    session::{Inner, Session},
    types::{NativeContentFile, RetainedBuiltinShader},
    value::{self, get},
};
use anyhow::{Result, bail};

pub const BUILTIN_RESOURCE_GUID: &str = "0000000000000000f000000000000000";

impl RetainedBuiltinShader {
    pub fn validate(&self, engine: &str) -> Result<()> {
        if self.schema_version != 1
            || self.compiled_source_revision_verified
            || engine != "6000.6.3f1"
            || self.unity_version != engine
            || self.guid != BUILTIN_RESOURCE_GUID
            || self.file_id != "108"
            || self.shader_name != "Skybox/Panoramic"
            || self.source_guid != "f57c2bc4f17ba48d9b9b6758dcfe812d"
            || self.source_file_id != "4800000"
            || !valid_native_id(&self.archive_file_id)
            || self.member_name.is_empty()
        {
            bail!("unsupported retained built-in Shader identity/version");
        }
        for (path, sha) in [
            (&self.bundle_path, &self.bundle_sha256),
            (&self.member_path, &self.member_sha256),
            (&self.builtin_source_path, &self.builtin_source_sha256),
            (&self.content.path, &self.content.sha256),
        ] {
            if !lexpath::is_abs(path) || lexpath::clean(path) != *path || !valid_native_hex(sha, 64)
            {
                bail!("retained built-in Shader requires exact native paths/hashes");
            }
        }
        let objects = &self.content.objects;
        if !valid_native_hex(&self.payload_sha256, 64)
            || !valid_native_hex(&self.type_tree_sha256, 64)
            || self.content.guid != self.guid
            || objects.len() != 1
            || objects[0].file_id != self.file_id
            || objects[0].type_name != "UnityEngine.Shader"
            || !self.content.resources.is_empty()
            || !self.content.dependencies.is_empty()
        {
            bail!("retained built-in Shader content must contain only its exact Shader object");
        }
        Ok(())
    }
}

impl Inner {
    pub(crate) fn validate_retained_builtin_shaders(&mut self) -> Result<()> {
        let stored = get(&self.baseline, "builtinShaderSnapshots");
        if stored.is_null() {
            return Ok(());
        }
        let snapshots: Vec<RetainedBuiltinShader> =
            value::decode::<Option<Vec<RetainedBuiltinShader>>>(&stored.to_json()?)?
                .unwrap_or_default();
        if snapshots.len() != 1 {
            bail!("unsupported retained built-in Shader snapshot count");
        }
        let engine = self.baseline_text("unityVersion");
        for snapshot in &snapshots {
            snapshot.validate(&engine)?;
            self.verify_native_artifacts(&snapshot.content)?;
        }
        Ok(())
    }

    fn retain_builtin_shader(&mut self, snapshot: &RetainedBuiltinShader) -> Result<()> {
        snapshot.validate(&self.baseline_text("unityVersion"))?;
        self.verify_native_artifacts(&snapshot.content)?;
        let stored = get(&self.baseline, "nativeContentFiles");
        let existing: Vec<NativeContentFile> = if stored.is_null() {
            Vec::new()
        } else {
            value::decode::<Option<Vec<NativeContentFile>>>(&stored.to_json()?)?.unwrap_or_default()
        };
        let mut files = Vec::new();
        for file in existing {
            if file.guid != snapshot.guid {
                files.push(file);
                continue;
            }
            if file.objects.len() != 1
                || file.objects[0].file_id != snapshot.file_id
                || file.objects[0].type_name != "UnityEngine.Shader"
            {
                bail!(
                    "retained built-in Shader conflicts with another built-in content descriptor"
                );
            }
        }
        files.push(snapshot.content.clone());
        self.baseline
            .insert("nativeContentFiles".into(), value::raw(&files)?);
        self.baseline
            .insert("builtinShaderSnapshots".into(), value::raw(&[snapshot])?);
        Ok(())
    }
}

impl Session {
    /// Accepts the verified descriptor produced by the CLI. Hashes are checked
    /// again before publication; authored content stays untouched.
    pub fn retain_builtin_shader(&self, snapshot: &RetainedBuiltinShader) -> Result<()> {
        self.lock().retain_builtin_shader(snapshot)
    }
}
