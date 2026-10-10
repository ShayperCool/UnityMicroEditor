//! Native-format sources the tool bakes itself. No worker runs: Unity has no
//! importer output for them, only Player data derived from the authored YAML.
use super::{
    importer::NativeProjectImporter,
    resolve_project_import_source,
    support::{Event, is_not_exist, mkdir_all, read_file, wrap, write_file},
};
use crate::{
    playerbake,
    project::{NativeContentFile, NativeContentObject, NativeReplacement},
    registry::meta_guid,
    sha256, sysutil,
};
use anyhow::{Result, bail};
use std::time::Instant;

impl NativeProjectImporter {
    /// Bakes an authored animation clip and publishes it as native content.
    /// A clip using a feature the baker refuses is reported, not approximated.
    pub(crate) fn import_baked(&mut self, logical: &str) -> Result<()> {
        let started = Instant::now();
        let source = resolve_project_import_source(&self.opts.project, logical)?;
        let data = read_file(&source)?;
        let meta = read_file(&format!("{source}.meta"))?;
        let guid = meta_guid(&meta)?;
        let baked = playerbake::bake_clip(&data)
            .map_err(|error| wrap(&format!("bake native {logical}"), error))?;
        let file_sha = sha256(&baked.file);
        // Content-addressed, so a published revision's file is never rewritten.
        let filename = sysutil::join(&[
            &self.opts.output,
            "native/baked",
            &format!("{guid}-{file_sha}.bin"),
        ]);
        match read_file(&filename) {
            Ok(existing) if existing == baked.file => {}
            Ok(_) => bail!("baked native artifact changed on disk: {filename}"),
            Err(error) if is_not_exist(&error) => {
                mkdir_all(&sysutil::dir(&filename), 0o700)?;
                write_file(&filename, &baked.file, 0o600)?;
            }
            Err(error) => return Err(error),
        }
        let mut replacement = NativeReplacement {
            guid: guid.clone(),
            source_path: logical.into(),
            source_sha256: sha256(&data),
            meta_sha256: sha256(&meta),
            environment_sha256: self.environment_sha256.clone(),
            content: NativeContentFile {
                guid: guid.clone(),
                path: filename,
                sha256: file_sha,
                ..Default::default()
            },
            ..Default::default()
        };
        for (file_id, type_name) in baked.objects {
            replacement
                .content
                .objects
                .push(NativeContentObject { file_id, type_name });
        }
        self.session.replace_native_content(&replacement)?;
        self.entries.insert(guid.clone(), replacement);
        self.publish_index()?;
        Event::new("native-baked")
            .with("path", logical)
            .with("guid", &guid)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(())
    }
}
