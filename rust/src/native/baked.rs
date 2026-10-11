//! Native-format sources the tool bakes itself. No worker runs: Unity has no
//! importer output for them, only Player data derived from the authored YAML.
use super::{
    importer::NativeProjectImporter,
    resolve_project_import_source,
    support::{Event, is_not_exist, lower_ext, mkdir_all, read_file, write_file},
};
use crate::{
    playerbake::{
        self, Baked, ClipFacts, ClipSources, ControllerSources, Mask, ScriptIdentity, Unsupported,
    },
    project::{NativeContentDependency, NativeContentFile, NativeContentObject, NativeReplacement},
    registry::meta_guid,
    sha256, sysutil,
};
use anyhow::{Result, bail};
use std::{collections::BTreeSet, time::Instant};

/// Reads the scripts, clips and masks a baked source refers to.
struct ProjectSources<'a> {
    importer: &'a NativeProjectImporter,
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

impl ClipSources for ProjectSources<'_> {
    fn script(&self, guid: &str) -> Result<ScriptIdentity> {
        let type_name = self.importer.session.lock().script_type(guid).to_string();
        let assembly = self.importer.script_assemblies.get(guid);
        let (Some(assembly), false) = (assembly, type_name.is_empty()) else {
            return Err(
                Unsupported(format!("reference to script {guid} of an unknown class")).into(),
            );
        };
        let path = self.importer.source_path(guid)?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
        let meta = String::from_utf8_lossy(&read_file(&format!("{source}.meta"))?).into_owned();
        let execution_order = meta
            .lines()
            .find_map(|line| line.trim().strip_prefix("executionOrder:"))
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let (namespace, class_name) = type_name.rsplit_once('.').unwrap_or(("", &type_name));
        if class_name.contains('+') {
            return Err(
                Unsupported(format!("reference to nested script class {type_name}")).into(),
            );
        }
        Ok(ScriptIdentity {
            class_name: class_name.to_string(),
            namespace: namespace.to_string(),
            assembly: assembly.clone(),
            execution_order,
            local_id: playerbake::script_local_id(guid),
        })
    }
}

impl ControllerSources for ProjectSources<'_> {
    fn clip(&self, guid: &str, _file_id: &str) -> Result<ClipFacts> {
        let path = self.path(guid, ".anim", "animation clip")?;
        let source = resolve_project_import_source(&self.importer.opts.project, &path)?;
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
    /// then refreshes the baked sources that refer to it. A source using a
    /// feature the baker refuses is reported, not approximated.
    pub(crate) fn import_baked(&mut self, logical: &str) -> Result<()> {
        let guid = self.publish_baked(logical, 0)?;
        self.refresh_dependents(&guid, logical, 0)
    }

    /// A controller holds facts of its clips and, once baked, refers to their
    /// baked form; one built into the catalog never sees a baked clip. The
    /// same holds for an override controller and its controller.
    fn refresh_dependents(&mut self, guid: &str, logical: &str, depth: usize) -> Result<()> {
        if lower_ext(logical) == ".overridecontroller" || depth > 2 {
            return Ok(());
        }
        let mut dependents = BTreeSet::new();
        {
            let session = self.session.lock();
            for extension in [".controller", ".overrideController"] {
                dependents.extend(session.native_asset_paths(extension));
            }
        }
        for entry in self.entries.values() {
            if matches!(
                lower_ext(&entry.source_path).as_str(),
                ".controller" | ".overridecontroller"
            ) {
                dependents.insert(entry.source_path.clone());
            }
        }
        for dependent in dependents {
            if dependent == logical {
                continue;
            }
            let Ok(source) = resolve_project_import_source(&self.opts.project, &dependent) else {
                continue;
            };
            if !read_file(&source).is_ok_and(|data| contains(&data, guid.as_bytes())) {
                continue;
            }
            match self.publish_baked(&dependent, 0) {
                Ok(published) => self.refresh_dependents(&published, &dependent, depth + 1)?,
                Err(error) if error.chain().any(|cause| cause.is::<Unsupported>()) => {
                    Event::new("native-bake-refused")
                        .with("path", &dependent)
                        .with("reason", &format!("{error:#}"))
                        .emit();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Bakes one source and the baked sources it refers to; returns its GUID.
    fn publish_baked(&mut self, logical: &str, depth: usize) -> Result<String> {
        if depth > 4 {
            bail!("baked sources refer to each other too deeply: {logical}");
        }
        let started = Instant::now();
        let source = resolve_project_import_source(&self.opts.project, logical)?;
        let data = read_file(&source)?;
        let meta = read_file(&format!("{source}.meta"))?;
        let guid = meta_guid(&meta)?;
        let source_sha = sha256(&data);
        let meta_sha = sha256(&meta);
        let baked = match lower_ext(logical).as_str() {
            ".anim" => playerbake::bake_clip(&data, &ProjectSources { importer: self }),
            ".controller" => playerbake::bake_controller(&data, &ProjectSources { importer: self }),
            ".overridecontroller" => playerbake::bake_override(&data),
            _ => bail!("no baker for {logical}"),
        };
        // Keeps the cause, so a refusal can be told from a failure.
        let baked: Baked =
            baked.map_err(|error| error.context(format!("bake native {logical}")))?;
        // Every source this one refers to has to be published in baked form.
        for external in &baked.externals {
            let dependency = self.source_path(&external.guid)?;
            if !matches!(lower_ext(&dependency).as_str(), ".anim" | ".controller") {
                return Err(anyhow::Error::from(Unsupported(format!(
                    "reference to {} {}, which is not a baked source",
                    external.guid, dependency
                )))
                .context(format!("bake native {logical}")));
            }
            let current = resolve_project_import_source(&self.opts.project, &dependency)
                .and_then(|source| read_file(&source))
                .map(|data| sha256(&data))?;
            let published = self
                .entries
                .get(&external.guid)
                .is_some_and(|entry| entry.source_sha256 == current);
            if !published {
                self.publish_baked(&dependency, depth + 1)?;
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
