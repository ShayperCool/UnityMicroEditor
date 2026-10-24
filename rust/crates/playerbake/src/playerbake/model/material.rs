//! The materials a model's import makes of the file's own
//! (`materialImportMode` other than None, no `externalObjects` mapping), as
//! the Universal Render Pipeline's material description preprocessor makes
//! them: URP Lit, opaque or premultiplied transparent from the description's
//! opacity, the base colour (a texture's or the colour, to gamma space in a
//! linear project), the normal map, the emission and the smoothness from
//! the shininess. Textures are found as the importer finds them: beside the
//! model or in a `Textures` folder on the way up to `Assets`.
use crate::yaml::Document;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// URP Lit (`ShaderPathID.Lit`).
const URP_LIT: &str = "933532a4fcc9baf4fa0491de14d08ed7";

/// What decides a material beyond the file: the project's pipeline and
/// colour space, and where the model lies.
pub struct Context {
    pub urp: bool,
    pub linear: bool,
    pub model: PathBuf,
}

impl Context {
    pub fn of_project(project: &Path, model: &Path) -> Self {
        let read = |name: &str| std::fs::read_to_string(project.join(name)).unwrap_or_default();
        let graphics = read("ProjectSettings/GraphicsSettings.asset");
        let quality = read("ProjectSettings/QualitySettings.asset");
        let manifest = read("Packages/manifest.json");
        let urp = crate::playerbake::names_render_pipeline(&graphics, &quality)
            && manifest.contains("com.unity.render-pipelines.universal");
        let linear = read("ProjectSettings/ProjectSettings.asset")
            .lines()
            .any(|line| line.trim() == "m_ActiveColorSpace: 1");
        Self {
            urp,
            linear,
            model: model.to_path_buf(),
        }
    }

    /// A texture the file names, as the project's source the importer finds
    /// for it: its GUID.
    fn texture(&self, texture: &ufbx::Texture) -> Option<String> {
        let name = [&texture.relative_filename, &texture.filename]
            .iter()
            .filter_map(|n| {
                Path::new(&n.replace('\\', "/"))
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
            })
            .find(|n| !n.is_empty())?;
        let mut directory = self.model.parent()?.to_path_buf();
        loop {
            for candidate in [
                directory.join(&name),
                directory.join("Textures").join(&name),
            ] {
                if let Ok(meta) = std::fs::read(format!("{}.meta", candidate.display()))
                    && let Some(guid) = crate::yaml::metadata_guid(&meta)
                {
                    return Some(guid);
                }
            }
            if directory.file_name().is_some_and(|n| n == "Assets") || !directory.pop() {
                return None;
            }
        }
    }
}

fn color(c: [f32; 4]) -> Value {
    json!({ "r": c[0], "g": c[1], "b": c[2], "a": c[3] })
}

fn gamma(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else if c < 1.0 {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    } else {
        c.powf(1.0 / 2.2)
    }
}

/// The Editor's document of the material of a file's material, or `None`
/// for a project whose pipeline the tool does not make materials for.
pub fn document(material: &ufbx::Material, file_id: i64, context: &Context) -> Option<Document> {
    if !context.urp {
        return None;
    }
    let fbx = &material.fbx;
    let prop = |name: &str| ufbx::find_prop(&material.element.props, name);
    let number = |map: &ufbx::MaterialMap| map.has_value.then_some(map.value_vec4.x as f32);
    // Opacity, or one less the transparency.
    let mut opacity = match prop("Opacity") {
        Some(p) => p.value_vec4.x as f32,
        None => {
            let mut opacity = 1.0;
            if let Some(factor) = number(&fbx.transparency_factor) {
                opacity = if factor == 1.0 { 1.0 } else { 1.0 - factor };
            }
            if opacity == 1.0 && fbx.transparency_color.has_value {
                let x = fbx.transparency_color.value_vec4.x as f32;
                opacity = if x == 1.0 { 1.0 } else { 1.0 - x };
            }
            opacity
        }
    };
    if !opacity.is_finite() {
        opacity = 1.0;
    }
    let transparent = opacity < 1.0 || fbx.transparency_color.texture.is_some();
    let mut keywords: Vec<&str> = Vec::new();
    let mut floats: Vec<(&str, f32)> = Vec::new();
    let mut colors: Vec<(&str, [f32; 4])> = Vec::new();
    let mut textures: Vec<(&str, String)> = Vec::new();
    let mut tags = Map::new();
    let mut queue = -1;
    if transparent {
        floats.extend([
            ("_Mode", 3.0),
            ("_SrcBlend", 1.0),
            ("_DstBlend", 10.0),
            ("_ZWrite", 0.0),
        ]);
        floats.push(("_Surface", 1.0));
        keywords.extend(["_ALPHAPREMULTIPLY_ON", "_SURFACE_TYPE_TRANSPARENT"]);
        tags.insert("RenderType".into(), json!("Transparent"));
        queue = 3000;
    } else {
        floats.extend([
            ("_Mode", 0.0),
            ("_SrcBlend", 1.0),
            ("_DstBlend", 0.0),
            ("_ZWrite", 1.0),
        ]);
        floats.push(("_Surface", 0.0));
    }
    let diffuse = &fbx.diffuse_color;
    match diffuse.texture.as_ref().and_then(|t| context.texture(t)) {
        Some(guid) => {
            let factor = number(&fbx.diffuse_factor).unwrap_or(1.0);
            textures.push(("_BaseMap", guid));
            colors.push(("_BaseColor", [factor, factor, factor, opacity]));
        }
        None if diffuse.has_value => {
            let v = diffuse.value_vec4;
            let mut c = [v.x as f32, v.y as f32, v.z as f32, opacity];
            if context.linear {
                for channel in c.iter_mut().take(3) {
                    *channel = gamma(*channel);
                }
            }
            colors.push(("_BaseColor", c));
        }
        None => {}
    }
    let normal = fbx
        .bump
        .texture
        .as_ref()
        .or(fbx.normal_map.texture.as_ref())
        .and_then(|t| context.texture(t));
    if let Some(guid) = normal {
        textures.push(("_BumpMap", guid));
        keywords.push("_NORMALMAP");
        if let Some(scale) = number(&fbx.bump_factor) {
            floats.push(("_BumpScale", scale));
        }
    }
    let mut flags = 4;
    let emission_factor = number(&fbx.emission_factor).unwrap_or(0.0);
    match fbx
        .emission_color
        .texture
        .as_ref()
        .and_then(|t| context.texture(t))
    {
        Some(guid) => {
            colors.push(("_EmissionColor", [1.0; 4]));
            textures.push(("_EmissionMap", guid));
            if emission_factor > 0.0 {
                keywords.push("_EMISSION");
                flags |= 1;
            }
        }
        None if fbx.emission_color.has_value => {
            let v = fbx.emission_color.value_vec4;
            let length = (v.x * v.x + v.y * v.y + v.z * v.z + v.w * v.w).sqrt();
            if length > v.w {
                let scaled = [v.x, v.y, v.z, v.w].map(|c| (c * emission_factor as f64) as f32);
                colors.push(("_EmissionColor", scaled));
                if emission_factor > 0.0 {
                    keywords.push("_EMISSION");
                    flags |= 1;
                }
            }
        }
        None => {}
    }
    let smoothness = match number(&fbx.specular_exponent) {
        Some(shininess) => (shininess * 0.01).sqrt(),
        None => 0.0,
    };
    floats.push(("_Smoothness", smoothness));
    keywords.sort();
    keywords.dedup();
    let texture_envs: Vec<Value> = textures
        .iter()
        .map(|(name, guid)| {
            json!({ *name: {
                "m_Texture": { "fileID": "2800000", "guid": guid, "type": "3" },
                "m_Scale": { "x": 1, "y": 1 },
                "m_Offset": { "x": 0, "y": 0 },
            }})
        })
        .collect();
    let data = json!({
        "serializedVersion": "8",
        "m_Name": material.element.name.to_string(),
        "m_Shader": { "fileID": "4800000", "guid": URP_LIT, "type": "3" },
        "m_Parent": { "fileID": "0" },
        "m_ModifiedSerializedProperties": "0",
        "m_ValidKeywords": keywords,
        "m_InvalidKeywords": [],
        "m_LightmapFlags": flags.to_string(),
        "m_EnableInstancingVariants": "0",
        "m_DoubleSidedGI": "0",
        "m_CustomRenderQueue": queue.to_string(),
        "stringTagMap": Value::Object(tags),
        "disabledShaderPasses": [],
        "m_LockedProperties": "",
        "m_SavedProperties": {
            "serializedVersion": "3",
            "m_TexEnvs": texture_envs,
            "m_Ints": [],
            "m_Floats": floats.iter().map(|(n, v)| json!({ *n: v })).collect::<Vec<_>>(),
            "m_Colors": colors.iter().map(|(n, c)| json!({ *n: color(*c) })).collect::<Vec<_>>(),
        },
        "m_BuildTextureStacks": [],
        "m_AllowLocking": "1",
    });
    let Value::Object(data) = data else {
        return None;
    };
    Some(Document {
        class_id: 21,
        file_id: file_id.to_string(),
        type_name: "Material".into(),
        data,
        stripped: false,
    })
}
