//! Material variants of a shader with an inspector, in a session.
//!
//! The session asks for such a variant when it has none made from the files
//! as they are now: a new or edited variant, or one whose parent, shader,
//! render pipeline asset or settings changed. The inspector of the shader
//! runs in a worker (`inspector.rs`), unless the result is kept from an
//! earlier run, the baker merges what it leaves as a build does, and the
//! session takes the fields of that object in place of the authored ones.
//! Nothing is published of a variant the inspection or the baker refuses.
use super::{
    build_settings::{Sources, material_parents},
    importer::NativeProjectImporter,
    inspector::{InspectionOptions, inspect_material_variants, inspection_cache},
    player_objects::{Build, open_sources, save_materials},
    support::Event,
};
use crate::{
    cancel::Context,
    playerbake::{self, ShaderFacts, Unsupported},
    project::{InspectedMaterial, outdated_material, parse_unity_yaml, value},
    sysutil,
    util::random_id,
};
use anyhow::{Result, anyhow};
use regex::bytes::Regex;
use std::{
    collections::BTreeMap,
    sync::LazyLock,
    time::{Duration, Instant},
};

/// What the baker takes from the project for one chain.
struct ChainSources<'a> {
    sources: &'a Sources,
    /// What a material takes from each shader, by the shader's GUID.
    shaders: BTreeMap<String, ShaderFacts>,
    /// The two documents of each inspected variant, by GUID.
    inspected: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
}

impl playerbake::NativeSources for ChainSources<'_> {
    fn linear_color_space(&self) -> Option<bool> {
        let settings = self
            .sources
            .read("ProjectSettings/ProjectSettings.asset")
            .ok()?;
        let text = String::from_utf8_lossy(&settings);
        let value = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("m_ActiveColorSpace:"))?;
        match value.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        }
    }
    fn shader(&self, guid: &str, _file_id: i64) -> Option<ShaderFacts> {
        self.shaders.get(guid).cloned()
    }
    fn material(&self, guid: &str) -> Option<Vec<u8>> {
        let path = self.sources.path_of(guid)?;
        (super::support::lower_ext(path) == ".mat")
            .then(|| self.sources.read(path).ok())
            .flatten()
    }
    fn inspected_material(&self, guid: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        self.inspected.get(guid).cloned()
    }
}

/// The type every reference of the given sources states, by GUID and file
/// ID.
fn reference_kinds<'a>(texts: impl Iterator<Item = &'a [u8]>) -> BTreeMap<(String, i64), i64> {
    static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\{fileID: (-?[0-9]+), guid: ([0-9a-fA-F]{32}), type: ([0-9])\}").unwrap()
    });
    let mut kinds = BTreeMap::new();
    for text in texts {
        for found in REFERENCE.captures_iter(text) {
            let number = |index: usize| String::from_utf8_lossy(&found[index]).parse::<i64>();
            if let (Ok(file_id), Ok(kind)) = (number(1), number(3)) {
                kinds
                    .entry((
                        String::from_utf8_lossy(&found[2]).to_ascii_lowercase(),
                        file_id,
                    ))
                    .or_insert(kind);
            }
        }
    }
    kinds
}

impl NativeProjectImporter {
    /// Has the inspector of a variant's shader run and hands the session
    /// the fields a build writes of the variant. A variant the inspection
    /// or the baker refuses is an `Unsupported` error that names why.
    pub(crate) fn import_material_variant(&mut self, ctx: &Context, logical: &str) -> Result<()> {
        let started = Instant::now();
        let refused = |reason: String| {
            anyhow::Error::from(Unsupported(reason)).context(format!("bake native {logical}"))
        };
        let sources = open_sources(&self.opts.project)
            .map_err(|error| refused(format!("material variant {logical}: {error:#}")))?;
        let guid = sources
            .guid_of(logical)
            .ok_or_else(|| anyhow!("{logical} has no .meta"))?
            .to_string();
        let source = sources.read(logical)?;
        if let Some(material) = parse_unity_yaml(&source)?
            .into_iter()
            .find(|document| document.class_id == 21)
            && outdated_material(&material)
        {
            return self.upgrade_material(ctx, &sources, logical, &guid, material.fields());
        }
        // The variants among its parents are built by the same inspector
        // before a build writes it.
        let parents = material_parents(&sources, &source)?;
        // A material without a parent is loaded from its authored fields.
        // The session names it when it changed after the baseline was
        // made, as the parent of a variant does.
        if parents.is_empty() {
            self.session.release_authored_material(logical);
            return Ok(());
        }
        let mut variants = vec![logical.to_string()];
        variants.extend(parents.iter().rev().skip(1).rev().cloned());
        let inspection = inspect_material_variants(
            ctx,
            &InspectionOptions {
                editor: self.opts.editor.clone(),
                project: self.opts.project.clone(),
                sources: &sources,
                output: sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]),
                timeout: Duration::from_secs(290),
                // Kept for the project, not for the session.
                cache: inspection_cache(&self.opts.project),
            },
            &variants,
        )?;
        let mut chain = ChainSources {
            sources: &sources,
            shaders: BTreeMap::new(),
            inspected: BTreeMap::new(),
        };
        for (shader, artifact) in &inspection.shaders {
            chain.shaders.insert(
                shader.clone(),
                playerbake::read_imported_shader_facts(artifact)?,
            );
        }
        let mut own = None;
        for material in inspection.materials {
            let inspected = material.result.map_err(refused)?;
            if material.path == logical {
                own = Some(inspected.clone());
            }
            chain.inspected.insert(
                material.guid,
                (inspected.over_authored, inspected.over_inspected),
            );
        }
        let Some(own) = own else {
            return Err(anyhow!("the inspection of {logical} left no result"));
        };
        let mut texts = vec![own.over_authored.clone(), own.over_inspected.clone()];
        for parent in &parents {
            texts.push(sources.read(parent)?);
        }
        let kinds = reference_kinds(texts.iter().map(Vec::as_slice));
        let data_json = playerbake::flatten_inspected_variant(
            &playerbake::InspectedVariant {
                over_authored: &own.over_authored,
                over_inspected: &own.over_inspected,
            },
            &chain,
            &|guid, file_id| kinds.get(&(guid.to_string(), file_id)).copied(),
        )
        .map_err(|error| error.context(format!("bake native {logical}")))?;
        self.session.set_inspected_material(&InspectedMaterial {
            guid: guid.clone(),
            source_path: logical.to_string(),
            ingredient_sha256: own.ingredients,
            data_json,
        })?;
        Event::new("native-material-inspected")
            .with("path", logical)
            .with("guid", &guid)
            .with("inspector", &own.inspector)
            .with("package", &own.package)
            .with("parents", &parents.len())
            .with("workerRuns", &inspection.worker_runs)
            .with("kept", &inspection.kept)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(())
    }
}

impl NativeProjectImporter {
    /// Has the engine load a material of an older serialization, which
    /// upgrades it, and hands the session the fields the Editor writes of
    /// it then. The worker imports the material's shader first: the
    /// upgrade sorts the keywords into those the shader declares and those
    /// it does not. Those fields stand while the material, its shader and
    /// their `.meta` files are unchanged.
    fn upgrade_material(
        &mut self,
        ctx: &Context,
        sources: &Sources,
        logical: &str,
        guid: &str,
        fields: &value::Map,
    ) -> Result<()> {
        let started = Instant::now();
        let refused = |reason: String| {
            anyhow::Error::from(Unsupported(reason)).context(format!("bake native {logical}"))
        };
        if value::map_file_id(value::object(value::get(fields, "m_Parent"))) != "0" {
            return Err(refused(format!(
                "the material variant {logical}, which is of an older serialization"
            )));
        }
        let shader_guid = value::text(value::object(value::get(fields, "m_Shader")), "guid");
        let Some(shader) = sources.path_of(&shader_guid).map(String::from) else {
            return Err(refused(format!(
                "the material {logical} of an older serialization, whose shader {shader_guid} is no source of the project or its packages"
            )));
        };
        if let Some(kept) =
            self.kept_supply::<InspectedMaterial>(sources, "material-upgrade", guid, |kept| {
                &kept.ingredient_sha256
            })
            && kept.source_path == logical
        {
            self.session.set_inspected_material(&kept)?;
            Event::new("native-material-upgrade-kept")
                .with("path", logical)
                .emit();
            return Ok(());
        }
        let saved = save_materials(
            ctx,
            &Build {
                project: self.opts.project.clone(),
                editor: self.opts.editor.clone(),
                output: sysutil::join(&[&self.opts.native_dir(), "imports", &random_id()]),
                target: shader.clone(),
                users: vec![logical.to_string()],
                timeout: Duration::from_secs(600),
                reload_list: self.reload_list(),
                shader_cache: String::new(),
            },
        )
        .map_err(|error| error.context(format!("bake native {logical}")))?;
        let text = saved
            .into_iter()
            .find_map(|(path, text)| (path == logical).then_some(text))
            .ok_or_else(|| anyhow!("the worker saved no text of {logical}"))?;
        let Some(upgraded) = parse_unity_yaml(&text)?
            .into_iter()
            .find(|document| document.class_id == 21)
        else {
            return Err(anyhow!("the worker's text of {logical} holds no material"));
        };
        if outdated_material(&upgraded) {
            return Err(refused(format!(
                "the material {logical}, which the engine leaves at an older serialization"
            )));
        }
        let mut ingredient_sha256 = BTreeMap::new();
        for path in [logical, &shader] {
            let Some(physical) = sources.physical(path) else {
                return Err(anyhow!("{path} has no file"));
            };
            ingredient_sha256.insert(path.to_string(), crate::util::file_hash(&physical)?);
            ingredient_sha256.insert(
                format!("{path}.meta"),
                crate::util::file_hash(&format!("{physical}.meta"))?,
            );
        }
        let data_json = value::compact_map(upgraded.fields())?;
        let supplied = InspectedMaterial {
            guid: guid.to_string(),
            source_path: logical.to_string(),
            ingredient_sha256,
            data_json,
        };
        self.session.set_inspected_material(&supplied)?;
        self.keep_supply("material-upgrade", guid, &supplied);
        Event::new("native-material-upgraded")
            .with("path", logical)
            .with("guid", guid)
            .with("shader", &shader)
            .seconds("elapsedSeconds", started.elapsed().as_secs_f64())
            .emit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_type_each_reference_states() {
        let text = b"m_Shader: {fileID: 4800000, guid: 933532A4FCC9BAF4FA0491DE14D08ED7, type: 3}\n  m_Texture: {fileID: 2800000, guid: 4459db655324dc581bdb480d2b420ea4, type: 2}\n  m_Parent: {fileID: 0}\n";
        let kinds = reference_kinds([&text[..]].into_iter());
        assert_eq!(
            kinds.get(&("933532a4fcc9baf4fa0491de14d08ed7".to_string(), 4800000)),
            Some(&3)
        );
        assert_eq!(
            kinds.get(&("4459db655324dc581bdb480d2b420ea4".to_string(), 2800000)),
            Some(&2)
        );
        assert_eq!(kinds.len(), 2);
    }
}
