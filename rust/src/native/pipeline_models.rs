//! Models of a project with a scriptable render pipeline, imported with the
//! pipeline's own model code.
//!
//! A model importer makes a material for each material a model holds, and
//! gives a mesh without one the pipeline's default material. In the Editor
//! both come from the Universal pipeline: an asset postprocessor
//! (`FBXMaterialDescriptionPreprocessor`) gives a generated material its Lit
//! shader and its properties from the model's material description, and the
//! pipeline asset names its default material (`Lit.mat`, through the
//! Editor's entries of the global settings). A plain worker run loads no
//! pipeline code and has no pipeline: it names the built-in shader and
//! material instead. Such models are imported in the context a build of the
//! pipeline's Lit shader runs in (`player_objects::shader_plan`): the
//! pipeline's assets, global settings and Editor assemblies, and the
//! graphics and quality settings that make the pipeline current; the Lit
//! shader and the default material are imported before the models.
use super::{
    import::{BuildImport, run_build_import},
    player_objects::{Build, REQUEST_PATH, open_sources, shader_plan},
    probe::{ProbeOptions, ProbeReport},
    scripted::build_environment,
};
use crate::cancel::Context;
use anyhow::{Result, anyhow};
use std::{collections::BTreeMap, time::Duration};

/// The Universal pipeline's (17.6) Lit shader and its default material.
const UNIVERSAL_LIT: &str = "933532a4fcc9baf4fa0491de14d08ed7";
const UNIVERSAL_LIT_MATERIAL: &str = "31321ba15b8f8eb4c954353edc038b1d";

/// Whether a model is imported with the pipeline's model code: a model of a
/// project whose graphics or quality settings name a pipeline asset, unless
/// its materials are imported the legacy way, which names the built-in
/// shader in every project.
pub(crate) fn pipeline_model(project: &str, logical: &str, meta: &[u8]) -> bool {
    super::batch::cold_model_profile(logical)
        && super::host_shaders::names_render_pipeline(project)
        && !String::from_utf8_lossy(meta).contains("materialImportMode: 1")
}

/// One worker run that imports `models` with the pipeline's model code; the
/// report holds the models alone, as a plain run's would.
pub(crate) fn run_pipeline_model_import(
    ctx: &Context,
    project: &str,
    editor: &str,
    output: &str,
    timeout: Duration,
    models: &[String],
) -> (Option<ProbeReport>, Option<anyhow::Error>) {
    let prepared = (|| -> Result<(Vec<String>, super::scripted::ScriptEnvironment)> {
        let sources = open_sources(project)?;
        let lit = sources
            .path_of(UNIVERSAL_LIT)
            .map(String::from)
            .ok_or_else(|| {
                anyhow!("the project's packages hold no Universal Lit shader ({UNIVERSAL_LIT})")
            })?;
        let plan = shader_plan(&Build {
            project: project.to_string(),
            editor: editor.to_string(),
            output: output.to_string(),
            target: lit,
            users: Vec::new(),
            timeout,
            reload_list: crate::sysutil::join(&[
                project,
                "Library/UnityMicroEditor/pipeline-reloads.json",
            ]),
            shader_cache: String::new(),
        })?;
        let mut assets: Vec<String> = plan
            .batch
            .into_iter()
            .filter(|path| path != REQUEST_PATH)
            .collect();
        if let Some(material) = sources.path_of(UNIVERSAL_LIT_MATERIAL)
            && !assets.iter().any(|known| known == material)
        {
            assets.push(material.to_string());
        }
        assets.extend(models.iter().cloned());
        Ok((assets, build_environment(project, &plan.scripts)?))
    })();
    let (assets, environment) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return (None, Some(error)),
    };
    let files = super::context::BuildFiles {
        generated: BTreeMap::new(),
        files: Vec::new(),
    };
    let (mut report, error) = run_build_import(
        ctx,
        ProbeOptions {
            editor: editor.to_string(),
            project: project.to_string(),
            output: output.to_string(),
            timeout,
            ..Default::default()
        },
        &assets,
        &BuildImport {
            environment: &environment,
            files: &files,
            shader_cache: "",
            extras: None,
        },
    );
    // The pipeline's assets, the shader and the material are the run's own.
    if let Some(content) = report.as_mut().and_then(|report| report.content.as_mut()) {
        content
            .assets
            .retain(|asset| models.iter().any(|model| *model == asset.path));
    }
    (report, error)
}
