//! The project's shaders imported and baked by the tool alone, for a file
//! of a Player build that holds them (`resources.assets`): the compiler
//! preprocesses each source as it does for the Editor, the tool lays the
//! reply out as the Editor's `Shader` and bakes it for the Player with the
//! variants the project's materials use. A shader another one falls back
//! on or takes passes of is baked first; a built-in one is not found, as a
//! session's bake does not find it.
use super::{build_settings::Sources, shader_compiler::ShaderCompiler};
use crate::{
    cancel::Context,
    playerbake::{
        self, External, ShaderDependency, ShaderMaterialUse, ShaderPointer, ShaderProject,
        Unsupported, shaderlab,
    },
    sysutil,
};
use anyhow::{Context as _, Result, bail};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
};

/// A baked shader: its Player object and the sources it refers to.
pub type BakedShader = (Vec<u8>, Vec<External>);

pub struct ProjectShaderBakery<'a> {
    ctx: &'a Context,
    project: String,
    editor: String,
    logs: String,
    sources: Sources,
    base: ShaderProject,
    /// How the project's materials use each shader, by the shader's GUID.
    materials: HashMap<String, Vec<ShaderMaterialUse>>,
    baked: RefCell<HashMap<String, std::result::Result<BakedShader, String>>>,
    /// The name each baked shader declares, by GUID.
    names: RefCell<HashMap<String, String>>,
    depth: Cell<usize>,
}

/// How the project's materials use each shader they name, by the shader's
/// GUID and file ID (a built-in shader's GUID is the engine's).
pub(super) fn material_users(roots: &[String]) -> HashMap<(String, i64), Vec<ShaderMaterialUse>> {
    let mut users: HashMap<(String, i64), Vec<ShaderMaterialUse>> = HashMap::new();
    for root in roots {
        let mut materials = Vec::new();
        let _ = crate::compiler::walk_dir(root, &mut |entry| {
            let name = sysutil::base(entry.path);
            if entry.is_dir() && (name.starts_with('.') || name.ends_with('~')) {
                return Ok(crate::compiler::Walk::SkipDir);
            }
            if !entry.is_dir() && name.ends_with(".mat") {
                materials.push(entry.path.to_string());
            }
            Ok(crate::compiler::Walk::Continue)
        });
        for path in materials {
            let Ok(source) = std::fs::read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&source);
            let Some(shader) = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("m_Shader:"))
            else {
                continue;
            };
            let field = |key: &str| {
                shader
                    .split(key)
                    .nth(1)
                    .and_then(|rest| rest.split([',', '}']).next())
                    .map(|value| value.trim().to_string())
            };
            let (Some(file_id), Some(guid)) = (
                field("fileID:").and_then(|id| id.parse::<i64>().ok()),
                field("guid:"),
            ) else {
                continue;
            };
            if let Ok(usage) = super::baked::material_use(&path, &source) {
                users.entry((guid, file_id)).or_default().push(usage);
            }
        }
    }
    for uses in users.values_mut() {
        uses.sort();
        uses.dedup();
    }
    users
}

impl<'a> ProjectShaderBakery<'a> {
    /// A bakery for the Linux host's graphics API, OpenGL Core.
    pub fn new(ctx: &'a Context, project: &str, editor: &str, logs: &str) -> Result<Self> {
        let version = crate::release::project_release(project)?;
        let release = playerbake::ShaderRelease::of(version)
            .ok_or_else(|| Unsupported(format!("baking the shaders of Unity {version}")))?;
        let read = |name: &str| {
            std::fs::read_to_string(sysutil::join(&[project, name])).unwrap_or_default()
        };
        let base = ShaderProject {
            release,
            platforms: vec![15],
            settings: playerbake::ShaderSettings::read(
                &read("ProjectSettings/ProjectSettings.asset"),
                &read("ProjectSettings/GraphicsSettings.asset"),
                &read("ProjectSettings/QualitySettings.asset"),
                &read("Packages/manifest.json"),
            )?,
            materials: Vec::new(),
        };
        let sources = super::player_objects::open_sources(project)?;
        let mut roots = vec![sysutil::join(&[project, "Assets"])];
        roots.extend(
            sources
                .folders()
                .into_iter()
                .map(|(_, directory)| directory),
        );
        let materials = material_users(&roots)
            .into_iter()
            .filter(|((_, file_id), _)| *file_id == 4_800_000)
            .map(|((guid, _), uses)| (guid, uses))
            .collect();
        Ok(Self {
            ctx,
            project: project.to_string(),
            editor: editor.to_string(),
            logs: logs.to_string(),
            sources,
            base,
            materials,
            baked: RefCell::new(HashMap::new()),
            names: RefCell::new(HashMap::new()),
            depth: Cell::new(0),
        })
    }

    /// The Player object of the shader of `guid`, baked once.
    pub fn bake(&self, guid: &str) -> Result<BakedShader> {
        if let Some(known) = self.baked.borrow().get(guid) {
            return known.clone().map_err(|error| anyhow::anyhow!(error));
        }
        if self.depth.get() > 4 {
            bail!("shaders refer to each other too deeply at {guid}");
        }
        self.depth.set(self.depth.get() + 1);
        let outcome = self.bake_now(guid).map_err(|error| format!("{error:#}"));
        self.depth.set(self.depth.get() - 1);
        self.baked
            .borrow_mut()
            .insert(guid.to_string(), outcome.clone());
        outcome.map_err(|error| anyhow::anyhow!(error))
    }

    /// The name the shader of `guid` declares, once it is baked.
    pub fn name(&self, guid: &str) -> Option<String> {
        self.names.borrow().get(guid).cloned()
    }

    fn bake_now(&self, guid: &str) -> Result<BakedShader> {
        let Some(logical) = self.sources.path_of(guid).map(str::to_string) else {
            bail!("no shader source has the GUID {guid}");
        };
        let physical = self
            .sources
            .physical(&logical)
            .with_context(|| format!("where {logical} lies"))?;
        let version = crate::release::project_release(&self.project)?;
        let tree = shaderlab::editor_shader_tree(version).ok_or_else(|| {
            Unsupported(format!(
                "importing a shader of Unity {version} without the worker"
            ))
        })?;
        let text = String::from_utf8_lossy(&std::fs::read(&physical)?).into_owned();
        let meta = std::fs::read(format!("{physical}.meta"))?;
        let request = shaderlab::PreprocessRequest::standalone(&logical, text);
        // Each bake has a compiler of its own: it may bake what it refers to.
        let mut compiler = ShaderCompiler::start_with_folders(
            self.ctx,
            &self.editor,
            &self.project,
            &self.logs,
            &self.sources.folders(),
        )?;
        let reply = compiler.preprocess(&request)?;
        let value = tree.val_of_json(&shaderlab::editor_object(&reply, &request, guid, &meta)?)?;
        if let Some(playerbake::Val::Str(name)) = value
            .field("m_ParsedForm")
            .and_then(|parsed| parsed.field("m_Name"))
        {
            self.names
                .borrow_mut()
                .insert(guid.to_string(), name.clone());
        }
        let artifact = playerbake::write_in(
            self.base.release.format(),
            &[playerbake::Object {
                local_id: playerbake::SHADER_LOCAL_ID,
                tree: &tree,
                data: tree.encode(&value)?,
                script: None,
                script_file: 0,
            }],
            &[],
        )?;
        let mut project = self.base.clone();
        project.materials = self.materials.get(guid).cloned().unwrap_or_default();
        playerbake::bake_shader_object(&artifact, &project, self, &mut compiler)
    }
}

impl playerbake::ShaderSources for ProjectShaderBakery<'_> {
    fn shader(&self, name: &str) -> Result<Option<ShaderDependency>> {
        let found = self.sources.shaders_named(name)?;
        let logical = match &found[..] {
            [] => return Ok(None),
            [one] => one,
            _ => {
                return Err(Unsupported(format!(
                    "the shader {name:?}, which several sources declare"
                ))
                .into());
            }
        };
        let Some(guid) = self.sources.guid_of(logical).map(str::to_string) else {
            return Ok(None);
        };
        let (object, externals) = self.bake(&guid)?;
        Ok(Some(ShaderDependency {
            pointer: ShaderPointer::External { guid },
            links: playerbake::read_shader_links_with(self.base.release, &object, &externals)?,
        }))
    }
}
