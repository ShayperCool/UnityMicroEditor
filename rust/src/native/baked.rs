//! Native-format sources the tool bakes itself. No worker runs: Unity has no
//! importer output for them, only Player data derived from the authored YAML.
use super::{
    importer::NativeProjectImporter,
    resolve_project_import_source,
    support::{Event, is_not_exist, lower_ext, mkdir_all, read_file, wrap, write_file},
};
use crate::{
    playerbake::{self, Baked, ClipFacts, ControllerSources, Mask, Unsupported},
    project::{NativeContentDependency, NativeContentFile, NativeContentObject, NativeReplacement},
    registry::meta_guid,
    sha256, sysutil,
};
use anyhow::{Result, bail};
use std::{cell::RefCell, collections::BTreeSet, time::Instant};

/// Reads a controller's clips and masks from the project and records which
/// clips it plays.
struct ProjectSources<'a> {
    importer: &'a NativeProjectImporter,
    clips: RefCell<BTreeSet<String>>,
}

impl ProjectSources<'_> {
    fn path(&self, guid: &str, extension: &str, what: &str) -> Result<String> {
        let path = self.importer.source_path(guid)?;
        if path.is_empty() {
            return Err(
                playerbake::Unsupported(format!("{what} {guid} is not a project source")).into(),
            );
        }
        if lower_ext(&path) != extension {
            return Err(Unsupported(format!("{what} stored in {path}")).into());
        }
        Ok(path)
    }
}

impl ControllerSources for ProjectSources<'_> {
    fn clip(&self, guid: &str, _file_id: &str) -> Result<ClipFacts> {
        let path = self.path(guid, ".anim", "animation clip")?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        self.clips.borrow_mut().insert(path.clone());
        playerbake::read_clip_facts(&read_file(&source)?, &path)
    }
    fn mask(&self, guid: &str) -> Result<Mask> {
        let path = self.path(guid, ".mask", "avatar mask")?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        playerbake::read_mask(&read_file(&source)?)
    }
}

impl NativeProjectImporter {
    fn source_path(&self, guid: &str) -> Result<String> {
        let mut session = self.session.lock();
        session.index_guids()?;
        Ok(session.path_of(guid).to_string())
    }

    /// Bakes an authored animation source and publishes it as native content,
    /// then refreshes the baked controllers that play it. A source using a
    /// feature the baker refuses is reported, not approximated.
    pub(crate) fn import_baked(&mut self, logical: &str) -> Result<()> {
        let guid = self.publish_baked(logical)?;
        if lower_ext(logical) != ".anim" {
            return Ok(());
        }
        // A controller holds facts of its clips and, once baked, refers to
        // their baked form; one built into the catalog never sees this clip.
        let mut controllers = self.session.lock().native_asset_paths(".controller");
        for entry in self.entries.values() {
            if lower_ext(&entry.source_path) == ".controller" {
                controllers.insert(entry.source_path.clone());
            }
        }
        for controller in controllers {
            let Ok(source) = resolve_project_import_source(&self.opts.project, &controller) else {
                continue;
            };
            if !read_file(&source).is_ok_and(|data| contains(&data, guid.as_bytes())) {
                continue;
            }
            match self.publish_baked(&controller) {
                Ok(_) => {}
                Err(error) if error.chain().any(|cause| cause.is::<Unsupported>()) => {
                    Event::new("native-bake-refused")
                        .with("path", &controller)
                        .with("reason", &format!("{error:#}"))
                        .emit();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Bakes one source and its baked dependencies; returns its GUID.
    fn publish_baked(&mut self, logical: &str) -> Result<String> {
        let started = Instant::now();
        let source = resolve_project_import_source(&self.opts.project, logical)?;
        let data = read_file(&source)?;
        let meta = read_file(&format!("{source}.meta"))?;
        let guid = meta_guid(&meta)?;
        let source_sha = sha256(&data);
        let meta_sha = sha256(&meta);
        let (baked, clips) = match lower_ext(logical).as_str() {
            ".anim" => (playerbake::bake_clip(&data), BTreeSet::new()),
            ".controller" => {
                let sources = ProjectSources {
                    importer: self,
                    clips: RefCell::new(BTreeSet::new()),
                };
                let baked = playerbake::bake_controller(&data, &sources);
                (baked, sources.clips.into_inner())
            }
            _ => bail!("no baker for {logical}"),
        };
        let baked: Baked = baked.map_err(|error| wrap(&format!("bake native {logical}"), error))?;
        // Every clip a controller refers to has to be published in baked form.
        for clip in clips {
            let current = resolve_project_import_source(&self.opts.project, &clip)
                .and_then(|source| read_file(&source))
                .map(|data| sha256(&data))?;
            let published = self
                .entries
                .values()
                .any(|entry| entry.source_path == clip && entry.source_sha256 == current);
            if !published {
                self.publish_baked(&clip)?;
            }
        }
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
            source_sha256: source_sha,
            meta_sha256: meta_sha,
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
        for external in baked.externals {
            replacement
                .content
                .dependencies
                .push(NativeContentDependency {
                    guid: external.guid,
                    global: false,
                });
        }
        self.session.replace_native_content(&replacement)?;
        self.entries.insert(guid.clone(), replacement);
        self.publish_index()?;
        Event::new("native-baked")
            .with("path", logical)
            .with("guid", &guid)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(guid)
    }
}

fn contains(data: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && data.windows(needle.len()).any(|window| window == needle)
}
