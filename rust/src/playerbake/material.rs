//! `Material` (class 21). A build does not write a material as authored: it
//! sorts its keywords by whether the shader declares them, spells its tags
//! the way the engine first met them, keeps a texture only for a property
//! the shader has and adds every property the shader has and the source
//! lacks. A variant is written as a whole material: what it inherits from
//! its parents is laid under its overrides, and the drawers of its shader
//! set its keywords. Each step is restated from the pinned Editor.
use super::{
    native::{NativeSources, References, scalar},
    source::{get, text},
    typetree::{TypeTree, Val},
    unsupported,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

/// The kind of a shader property (`SerializedProperty::m_Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    Color,
    Vector,
    Float,
    Range,
    Texture,
    Int,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShaderProperty {
    pub name: String,
    pub kind: PropertyKind,
    /// `SerializedProperty::m_Flags`.
    pub flags: u32,
    /// The default as the shader stores it. An integer keeps its bits in
    /// the first number.
    pub default: [f32; 4],
    /// The attributes that name a drawer of the material inspector
    /// (`Toggle(_KEYWORD)`, `KeywordEnum(A,B)`, ...), as authored.
    pub attributes: Vec<String>,
}

/// What a material takes from its shader.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShaderFacts {
    pub name: String,
    pub keywords: Vec<String>,
    pub properties: Vec<ShaderProperty>,
    /// Every tag name and value of the subshaders and passes.
    pub tags: Vec<String>,
    /// The inspector classes the shader names (`CustomEditor`), for every
    /// render pipeline.
    pub custom_editors: Vec<String>,
}

const FLAG_HDR: u32 = 16;
const FLAG_GAMMA: u32 = 32;

static SHADER_TREE: LazyLock<Result<TypeTree, String>> = LazyLock::new(|| {
    TypeTree::parse(&include_bytes!("engine/type-48.bin")[..]).map_err(|error| error.to_string())
});

fn field<'a>(value: &'a Val, name: &str) -> Result<&'a Val> {
    match value.field(name) {
        Some(field) => Ok(field),
        None => bail!("shader object has no {name}"),
    }
}

fn items<'a>(value: &'a Val, name: &str) -> Result<&'a [Val]> {
    match field(value, name)? {
        Val::Seq(items) => Ok(items),
        Val::Zero => Ok(&[]),
        _ => bail!("shader object's {name} is not a sequence"),
    }
}

fn string(value: &Val, name: &str) -> Result<String> {
    match field(value, name)? {
        Val::Str(text) => Ok(text.clone()),
        Val::Zero => Ok(String::new()),
        _ => bail!("shader object's {name} is not a string"),
    }
}

fn tags_of(owner: &Val, tags: &mut Vec<String>) -> Result<()> {
    for pair in items(field(owner, "m_Tags")?, "tags")? {
        for side in ["first", "second"] {
            let tag = string(pair, side)?;
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    Ok(())
}

/// Reads what a material needs from a `Shader` object in the Player form of
/// the pinned engine, as a build of the project wrote it.
pub fn read_shader_facts(object: &[u8]) -> Result<ShaderFacts> {
    let tree = SHADER_TREE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned Shader type tree: {error}"))?;
    // The parsed form is the second field; compiled programs follow it.
    let shader = tree.decode_leading(object, 2)?;
    parsed_facts(field(&shader, "m_ParsedForm")?)
}

/// Reads the same from the artifact an import worker leaves of a shader
/// source or a shader graph: the `Shader` object as the Editor holds it,
/// laid out by the type tree the artifact carries. Its parsed form has the
/// fields of the Player's.
pub fn read_imported_shader_facts(artifact: &[u8]) -> Result<ShaderFacts> {
    let shaders = super::serialized::read_of_class(artifact, 48)?;
    let [shader] = &shaders[..] else {
        bail!("import artifact holds {} shaders, not one", shaders.len());
    };
    parsed_facts(field(&shader.tree.decode(&shader.data)?, "m_ParsedForm")?)
}

fn parsed_facts(parsed: &Val) -> Result<ShaderFacts> {
    let mut facts = ShaderFacts {
        name: string(parsed, "m_Name")?,
        ..ShaderFacts::default()
    };
    for keyword in items(parsed, "m_KeywordNames")? {
        match keyword {
            Val::Str(keyword) => facts.keywords.push(keyword.clone()),
            _ => bail!("shader keyword is not a string"),
        }
    }
    for property in items(field(parsed, "m_PropInfo")?, "m_Props")? {
        let kind = match field(property, "m_Type")? {
            Val::I(0) => PropertyKind::Color,
            Val::I(1) => PropertyKind::Vector,
            Val::I(2) => PropertyKind::Float,
            Val::I(3) => PropertyKind::Range,
            Val::I(4) => PropertyKind::Texture,
            Val::I(5) => PropertyKind::Int,
            other => bail!("shader property of unknown type {other:?}"),
        };
        let Val::U(flags) = field(property, "m_Flags")? else {
            bail!("shader property without flags");
        };
        let mut default = [0.0; 4];
        for (index, number) in default.iter_mut().enumerate() {
            match field(property, &format!("m_DefValue[{index}]"))? {
                Val::F(value) => *number = *value,
                _ => bail!("shader property default is not a number"),
            }
        }
        let mut attributes = Vec::new();
        for attribute in items(property, "m_Attributes")? {
            match attribute {
                Val::Str(attribute) => attributes.push(attribute.clone()),
                _ => bail!("shader property attribute is not a string"),
            }
        }
        facts.properties.push(ShaderProperty {
            name: string(property, "m_Name")?,
            kind,
            flags: *flags as u32,
            default,
            attributes,
        });
    }
    let editor = string(parsed, "m_CustomEditorName")?;
    if !editor.is_empty() {
        facts.custom_editors.push(editor);
    }
    for editor in items(parsed, "m_CustomEditorForRenderPipelines")? {
        facts
            .custom_editors
            .push(string(editor, "customEditorName")?);
    }
    for subshader in items(parsed, "m_SubShaders")? {
        tags_of(subshader, &mut facts.tags)?;
        for pass in items(subshader, "m_Passes")? {
            tags_of(pass, &mut facts.tags)?;
            tags_of(field(pass, "m_State")?, &mut facts.tags)?;
        }
    }
    Ok(facts)
}

/// The tags the engine registers when it starts, in its spelling. A tag is
/// found without regard to case and written the way it was first met, so one
/// of these is always written this way.
const ENGINE_TAGS: [&str; 31] = [
    "LIGHTMODE",
    "SHADOWCASTER",
    "SHADOWCOLLECTOR",
    "Vertex",
    "VertexLM",
    "VertexLMRGBM",
    "true",
    "QUEUE",
    "REQUIREOPTIONS",
    "FORCENOSHADOWCASTING",
    "IGNOREPROJECTOR",
    "SHADOWSUPPORT",
    "PASSFLAGS",
    "RenderType",
    "DisableBatching",
    "LodFading",
    "RenderPipeline",
    "Picking",
    "SceneSelectionPass",
    "ALWAYS",
    "FORWARDBASE",
    "FORWARDADD",
    "PREPASSBASE",
    "PREPASSFINAL",
    "DEFERRED",
    "META",
    "MOTIONVECTORS",
    "NEVEREXECUTED",
    "SRPDEFAULTUNLIT",
    "GRABPASS",
    "<invalid>",
];

/// Tags the engine registers later, when it first needs one. Whether that
/// comes before a project asset spells the tag otherwise is not known.
const LATE_ENGINE_TAGS: [&str; 16] = [
    "Background",
    "Format",
    "Grass",
    "GrassBillboard",
    "Name",
    "Opaque",
    "PCF_SOFT_FORCE_INV_PROJECTION_IN_PS",
    "PreviewType",
    "ShadowmapFilter",
    "Size",
    "Skybox",
    "SplatCount",
    "Transparent",
    "TransparentCutout",
    "TreeLeaf",
    "TreeTransparentCutout",
];

/// Spells the tags of one material the way the engine writes them.
struct Tags<'a> {
    shader: &'a ShaderFacts,
    sources: &'a dyn NativeSources,
    written: Vec<String>,
}

impl Tags<'_> {
    fn spell(&mut self, tag: &str) -> Result<String> {
        let known = ENGINE_TAGS
            .iter()
            .find(|other| other.eq_ignore_ascii_case(tag));
        let spelled = if let Some(known) = known {
            known.to_string()
        } else if let Some(known) = self.sources.tag_spelling(tag) {
            known
        } else {
            let elsewhere = LATE_ENGINE_TAGS
                .iter()
                .copied()
                .chain(self.shader.tags.iter().map(String::as_str))
                .find(|other| other.eq_ignore_ascii_case(tag) && *other != tag);
            if let Some(other) = elsewhere {
                return Err(unsupported(format!(
                    "material tag {tag:?} is spelled {other:?} elsewhere; the engine writes the spelling it met first"
                )));
            }
            tag.to_string()
        };
        match self
            .written
            .iter()
            .find(|other| other.eq_ignore_ascii_case(&spelled))
        {
            Some(other) if *other != spelled => Err(unsupported(format!(
                "material spells one tag as {other:?} and as {spelled:?}"
            ))),
            Some(_) => Ok(spelled),
            None => {
                self.written.push(spelled.clone());
                Ok(spelled)
            }
        }
    }
}

fn bits(bits: u32) -> f32 {
    f32::from_bits(bits)
}

/// The engine keeps a default of a gamma-space property in linear space in
/// a project that works in linear color space, and converts it back when a
/// material takes it. These are its two conversions, constant for constant.
fn gamma_to_linear(value: f32) -> f32 {
    if value <= bits(0x3d25_aee6) {
        value / bits(0x414e_b852)
    } else if value < 1.0 {
        ((value + bits(0x3d61_47ae)) / bits(0x3f87_0a3d)).powf(bits(0x4019_999a))
    } else if value == 1.0 {
        value
    } else {
        value.powf(bits(0x400c_cccd))
    }
}

fn linear_to_gamma(value: f32) -> f32 {
    if value <= 0.0 {
        0.0
    } else if value <= bits(0x3b4d_2e1c) {
        value * bits(0x414e_b852)
    } else if value < 1.0 {
        value.powf(bits(0x3ed5_5556)) * bits(0x3f87_0a3d) + bits(0xbd61_47ae)
    } else if value == 1.0 {
        value
    } else {
        value.powf(bits(0x3ee8_ba2f))
    }
}

/// A gamma-space default as a material takes it.
fn gamma_default(value: f32, sources: &dyn NativeSources, name: &str) -> Result<f32> {
    let converted = linear_to_gamma(gamma_to_linear(value));
    if converted.to_bits() == value.to_bits() {
        return Ok(value);
    }
    match sources.linear_color_space() {
        Some(false) => Ok(value),
        // The conversion goes through the C library's power function; only
        // the one the pinned Editor was compared with is trusted.
        Some(true) if cfg!(all(target_os = "linux", target_env = "gnu")) => Ok(converted),
        Some(true) => Err(unsupported(format!(
            "material without {name}, whose default the engine converts between color spaces"
        ))),
        None => Err(unsupported(format!(
            "material without {name} in a project whose color space is not known"
        ))),
    }
}

fn pair(name: &str, value: Val) -> Val {
    Val::map([("first", Val::str(name)), ("second", value)])
}

/// One of the four lists of saved properties: single-entry mappings, of
/// which the engine keeps the first under each name.
fn saved<'a>(properties: &'a Map<String, Value>, list: &str) -> Result<Vec<(&'a str, &'a Value)>> {
    let items = match get(properties, list) {
        Value::Null => return Ok(Vec::new()),
        Value::Array(items) => items,
        _ => bail!("{list} is not a sequence"),
    };
    let mut entries: Vec<(&str, &Value)> = Vec::new();
    for item in items {
        let Value::Object(entry) = item else {
            bail!("{list} holds an entry that is not a mapping");
        };
        let mut fields = entry.iter();
        let (Some((name, value)), None) = (fields.next(), fields.next()) else {
            return Err(unsupported(format!(
                "{list} holds an entry that is not one property"
            )));
        };
        if !entries.iter().any(|(known, _)| known == name) {
            entries.push((name, value));
        }
    }
    Ok(entries)
}

fn strings(fields: &Map<String, Value>, name: &str) -> Result<Vec<String>> {
    match get(fields, name) {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(text).collect(),
        _ => bail!("{name} is not a sequence"),
    }
}

fn number(fields: &Map<String, Value>, kind: &str, name: &str) -> Result<Val> {
    match get(fields, name) {
        Value::String(source) => scalar(kind, source, name),
        _ => Err(unsupported(format!("the source has no {name}"))),
    }
}

fn numbers<const N: usize>(value: &Value, names: [&'static str; N], path: &str) -> Result<Val> {
    let Value::Object(fields) = value else {
        return Err(unsupported(format!("{path} is not written as its fields")));
    };
    let mut values = Vec::new();
    for name in names {
        let Value::String(source) = get(fields, name) else {
            return Err(unsupported(format!("the source has no {path}.{name}")));
        };
        let value = scalar("float", source, path)?;
        values.push((std::borrow::Cow::Borrowed(name), value));
    }
    Ok(Val::Map(values))
}

fn version(fields: &Map<String, Value>) -> Result<String> {
    match fields.get("serializedVersion") {
        None => Ok("1".to_string()),
        Some(version) => text(version),
    }
}

/// The GUID and file ID an object reference names in another source; `None`
/// for a null reference.
fn other_source(value: &Value, path: &str) -> Result<Option<(String, i64)>> {
    let map = match value {
        Value::Null => return Ok(None),
        Value::Object(map) => map,
        _ => bail!("{path} is not an object reference"),
    };
    let Ok(file_id) = text(get(map, "fileID"))?.parse::<i64>() else {
        bail!("{path} has an invalid file ID");
    };
    if file_id == 0 {
        return Ok(None);
    }
    let guid = text(get(map, "guid"))?.to_ascii_lowercase();
    if guid.is_empty() {
        return Err(unsupported(format!(
            "{path} refers to an object of its own source"
        )));
    }
    Ok(Some((guid, file_id)))
}

/// The saved properties of a material by name, each kind sorted the way a
/// build writes it. A texture stays as authored until it is written, so the
/// sources a material names keep their order of first use.
#[derive(Default)]
struct Saved {
    textures: BTreeMap<String, Value>,
    ints: BTreeMap<String, Val>,
    floats: BTreeMap<String, Val>,
    colors: BTreeMap<String, Val>,
}

impl Saved {
    fn read(properties: &Map<String, Value>) -> Result<Saved> {
        let mut sheet = Saved::default();
        for (name, value) in saved(properties, "m_TexEnvs")? {
            sheet.textures.insert(name.to_string(), value.clone());
        }
        for (name, value) in saved(properties, "m_Ints")? {
            let Value::String(source) = value else {
                bail!("integer property {name} is not a number");
            };
            sheet
                .ints
                .insert(name.to_string(), scalar("int", source, name)?);
        }
        for (name, value) in saved(properties, "m_Floats")? {
            let Value::String(source) = value else {
                bail!("float property {name} is not a number");
            };
            sheet
                .floats
                .insert(name.to_string(), scalar("float", source, name)?);
        }
        for (name, value) in saved(properties, "m_Colors")? {
            sheet.colors.insert(
                name.to_string(),
                numbers(value, ["r", "g", "b", "a"], name)?,
            );
        }
        Ok(sheet)
    }

    /// Lays the overrides of a variant over what it inherits. A property an
    /// ancestor locks keeps the inherited value.
    fn overlay(&mut self, overrides: Saved, locked: &BTreeSet<String>) {
        fn lay<T>(
            base: &mut BTreeMap<String, T>,
            overrides: BTreeMap<String, T>,
            locked: &BTreeSet<String>,
        ) {
            for (name, value) in overrides {
                if !locked.contains(&name) {
                    base.insert(name, value);
                }
            }
        }
        lay(&mut self.textures, overrides.textures, locked);
        lay(&mut self.ints, overrides.ints, locked);
        lay(&mut self.floats, overrides.floats, locked);
        lay(&mut self.colors, overrides.colors, locked);
    }
}

/// The four settings a variant overrides one by one, with the bit each has
/// in `m_ModifiedSerializedProperties` and among the locks
/// (`Material::MaterialSerializedProperty`).
const SETTINGS: [(&str, &str, u32); 4] = [
    ("m_LightmapFlags", "unsigned int", 1 << 1),
    ("m_EnableInstancingVariants", "bool", 1 << 2),
    ("m_DoubleSidedGI", "bool", 1 << 3),
    ("m_CustomRenderQueue", "int", 1 << 4),
];

/// What a material locks for its variants (`m_LockedProperties`): names
/// separated by spaces, a setting as its field name after a colon.
#[derive(Default, Clone)]
struct Locks {
    settings: u32,
    properties: BTreeSet<String>,
}

impl Locks {
    fn read(fields: &Map<String, Value>) -> Result<Locks> {
        let mut locks = Locks::default();
        let listed = match get(fields, "m_LockedProperties") {
            Value::Null => return Ok(locks),
            Value::String(listed) => listed,
            _ => bail!("m_LockedProperties is not a string"),
        };
        for name in listed.split(' ').filter(|name| !name.is_empty()) {
            let setting = name
                .strip_prefix(':')
                .and_then(|field| SETTINGS.iter().find(|(known, ..)| *known == field));
            match setting {
                Some((.., bit)) => locks.settings |= bit,
                None => {
                    locks.properties.insert(name.to_string());
                }
            }
        }
        Ok(locks)
    }

    fn join(&mut self, other: &Locks) {
        self.settings |= other.settings;
        self.properties.extend(other.properties.iter().cloned());
    }
}

/// A material with what it inherits from its parents laid under it
/// (`Material::MergeHierarchy`).
struct Merged {
    /// The reference to the shader: that of the material without a parent.
    shader: Value,
    /// The settings in the order of `SETTINGS`.
    settings: [Val; 4],
    saved: Saved,
    /// What the material locks itself, and what its ancestors lock for it.
    locks: Locks,
    inherited: Locks,
    variant: bool,
}

/// The state the parents of an inspected variant are taken in. A build
/// builds the properties of a variant when it first needs them, so it may
/// meet a parent that is a variant itself before or after that parent's own
/// inspector has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Parents {
    /// As their sources give them.
    Authored,
    /// Each as its inspector leaves it over authored parents.
    BuiltAlone,
    /// Each as its inspector leaves it after its own parents were built.
    BuiltFirst,
}

/// How long a chain of parents is followed. The engine sets no limit.
const DEEPEST_PARENT: usize = 32;

fn merged(
    fields: &Map<String, Value>,
    sources: &dyn NativeSources,
    ancestors: &mut Vec<(String, i64)>,
    parents: Parents,
) -> Result<Merged> {
    // The engine upgrades a source of an older layout on load.
    if version(fields)? != "8" {
        return Err(unsupported(format!(
            "material of serialized version {}, the engine writes 8",
            version(fields)?
        )));
    }
    let Value::Object(properties) = get(fields, "m_SavedProperties") else {
        return Err(unsupported("material without saved properties"));
    };
    if version(properties)? != "3" {
        return Err(unsupported(format!(
            "material properties of serialized version {}, the engine writes 3",
            version(properties)?
        )));
    }
    if !strings_or_empty(get(fields, "m_BuildTextureStacks")) {
        return Err(unsupported("material with texture stacks"));
    }
    let own = Saved::read(properties)?;
    let locks = Locks::read(fields)?;
    let mut settings = [Val::Zero, Val::Zero, Val::Zero, Val::Zero];
    for (setting, (name, kind, _)) in settings.iter_mut().zip(SETTINGS) {
        *setting = number(fields, kind, name)?;
    }
    let Some((guid, file_id)) = other_source(get(fields, "m_Parent"), "m_Parent")? else {
        return Ok(Merged {
            shader: get(fields, "m_Shader").clone(),
            settings,
            saved: own,
            locks,
            inherited: Locks::default(),
            variant: false,
        });
    };
    // The engine cuts a chain that returns to itself and reports it.
    if ancestors.contains(&(guid.clone(), file_id)) {
        return Err(unsupported(format!(
            "material variant whose parents form a cycle through {guid}"
        )));
    }
    if ancestors.len() >= DEEPEST_PARENT {
        return Err(unsupported(format!(
            "material variant with more than {DEEPEST_PARENT} parents"
        )));
    }
    ancestors.push((guid.clone(), file_id));
    // Without its parent a variant keeps its own overrides over defaults
    // of the engine; whether the parent exists is the project's to say.
    let inspected = match parents {
        Parents::Authored => None,
        Parents::BuiltAlone => sources.inspected_material(&guid).map(|(alone, _)| alone),
        Parents::BuiltFirst => sources.inspected_material(&guid).map(|(_, first)| first),
    };
    let built = inspected.is_some();
    let Some(source) = inspected.or_else(|| sources.material(&guid)) else {
        return Err(unsupported(format!(
            "material variant whose parent {guid} is not known"
        )));
    };
    let documents = crate::yaml::parse_unity_yaml_lexical(&source)?;
    let parent = documents.iter().find(|document| {
        !document.stripped
            && document.class_id == super::native::MATERIAL
            && document.file_id.parse() == Ok(file_id)
    });
    let Some(parent) = parent else {
        return Err(unsupported(format!(
            "material variant whose parent {guid} holds no material {file_id}"
        )));
    };
    // Only the material at the top of a chain has no inspected form.
    if parents != Parents::Authored
        && !built
        && other_source(get(&parent.data, "m_Parent"), "m_Parent")?.is_some()
    {
        return Err(unsupported(format!(
            "material variant whose parent {guid}, a variant itself, has not been inspected"
        )));
    }
    let mut parent = merged(&parent.data, sources, ancestors, parents)?;
    // What the chain inherits is written into the variant; a texture that
    // lives in a parent's own source cannot be named from here.
    for (name, texture) in &parent.saved.textures {
        if let Value::Object(texture) = texture {
            other_source(get(texture, "m_Texture"), name)
                .map_err(|error| error.context(format!("parent {guid}")))?;
        }
    }
    // `Material::MergeSingle`: a parent passes on its own locks and those
    // it is under; a locked or unmodified setting is the parent's.
    let mut inherited = parent.inherited.clone();
    inherited.join(&parent.locks);
    let modified = match number(fields, "unsigned int", "m_ModifiedSerializedProperties")? {
        Val::U(modified) => modified as u32,
        _ => 0,
    };
    for (index, (.., bit)) in SETTINGS.iter().enumerate() {
        if modified & bit == 0 || inherited.settings & bit != 0 {
            settings[index] = parent.settings[index].clone();
        }
    }
    parent.saved.overlay(own, &inherited.properties);
    Ok(Merged {
        shader: parent.shader,
        settings,
        saved: parent.saved,
        locks,
        inherited,
        variant: true,
    })
}

/// A drawer of the material inspector that turns keywords on and off with
/// the value of its property (`MaterialToggleDrawer`,
/// `MaterialToggleOffDrawer`, `MaterialKeywordEnumDrawer`).
enum Drawer {
    /// The keyword, and whether a value of zero turns it on.
    Toggle(String, bool),
    Enum(Vec<String>),
}

/// The attributes of the inspector that never touch a keyword.
const PASSIVE_ATTRIBUTES: [&str; 6] = [
    "Enum",
    "Header",
    "IntRange",
    "PowerSlider",
    "Space",
    "ToggleUI",
];

/// Reads an attribute the way `MaterialPropertyHandler` does: a class name,
/// then arguments in parentheses, separated by commas. `None` for an
/// attribute that changes no keyword.
fn drawer(property: &ShaderProperty, attribute: &str) -> Result<Option<Drawer>> {
    let refused = |why: &str| {
        unsupported(format!(
            "material variant of a shader whose property {} has the attribute {attribute:?}: {why}",
            property.name
        ))
    };
    let word = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    let (class, arguments) = match attribute.split_once('(') {
        None => (attribute, None),
        Some((class, rest)) => match rest.strip_suffix(')') {
            Some(arguments) if !arguments.contains(['(', ')']) => {
                (class.trim_end(), Some(arguments.trim()))
            }
            _ => return Err(refused("it is not a class with plain arguments")),
        },
    };
    if !word(class) || !property.name.is_ascii() {
        return Err(refused("it is not a class with plain arguments"));
    }
    // A class is found as `Toggle`, `MaterialToggle` or
    // `MaterialToggleDrawer`.
    let short = class
        .strip_suffix("Drawer")
        .and_then(|class| class.strip_prefix("Material"))
        .or_else(|| class.strip_prefix("Material"))
        .unwrap_or(class);
    if PASSIVE_ATTRIBUTES.contains(&short)
        || PASSIVE_ATTRIBUTES.contains(&short.strip_suffix("Decorator").unwrap_or(short))
    {
        return Ok(None);
    }
    if !matches!(short, "Toggle" | "ToggleOff" | "KeywordEnum") {
        // A project can define drawers of its own, which run as its code.
        return Err(refused("the inspector may run a drawer of the project"));
    }
    let arguments: Vec<&str> = match arguments {
        None | Some("") => Vec::new(),
        Some(arguments) => arguments.split(',').map(str::trim).collect(),
    };
    // The engine passes an argument that reads as a number as one, and
    // these classes take text.
    if arguments
        .iter()
        .any(|argument| !argument.is_ascii() || argument.parse::<f64>().is_ok())
    {
        return Err(refused("an argument is not plain text"));
    }
    if !matches!(
        property.kind,
        PropertyKind::Float | PropertyKind::Range | PropertyKind::Int
    ) {
        return Ok(None);
    }
    let name = property.name.to_ascii_uppercase();
    Ok(Some(match (short, arguments.as_slice()) {
        ("Toggle", []) => Drawer::Toggle(format!("{name}_ON"), false),
        ("ToggleOff", []) => Drawer::Toggle(format!("{name}_OFF"), true),
        ("Toggle", [keyword]) => Drawer::Toggle(keyword.to_string(), false),
        ("ToggleOff", [keyword]) => Drawer::Toggle(keyword.to_string(), true),
        ("KeywordEnum", [_, ..]) => Drawer::Enum(
            arguments
                .iter()
                .map(|argument| {
                    format!("{name}_{}", argument.replace(' ', "_").to_ascii_uppercase())
                })
                .collect(),
        ),
        _ => return Err(refused("the class takes other arguments")),
    }))
}

/// A variant gets its keywords from the drawers of its shader when its
/// properties are built (`Material::SetupKeywordsAndPasses`, which runs
/// `MaterialEditor.ApplyMaterialPropertyDrawers` and the `ValidateMaterial`
/// of the shader's inspector). `value` gives the merged value of a number
/// property.
fn apply_drawers(
    shader: &ShaderFacts,
    keywords: &mut Vec<String>,
    value: &dyn Fn(&ShaderProperty) -> Result<Val>,
) -> Result<()> {
    // The inspector of a shader validates a material in its own code.
    if let Some(editor) = shader.custom_editors.first() {
        return Err(unsupported(format!(
            "material variant of the shader {}, which {editor} validates",
            shader.name
        )));
    }
    for property in &shader.properties {
        let mut drawers = Vec::new();
        for attribute in &property.attributes {
            drawers.extend(drawer(property, attribute)?);
        }
        let drawer = match drawers.len() {
            0 => continue,
            1 => drawers.remove(0),
            _ => {
                return Err(unsupported(format!(
                    "material variant of a shader whose property {} has several drawers",
                    property.name
                )));
            }
        };
        let value = value(property)?;
        let mut set = |keyword: &str, on: bool| {
            let listed = keywords.iter().any(|known| known == keyword);
            if on && !listed {
                keywords.push(keyword.to_string());
            } else if !on {
                keywords.retain(|known| known != keyword);
            }
        };
        match drawer {
            Drawer::Toggle(keyword, inverted) => {
                let on = match value {
                    Val::F(value) => value.abs() > 0.001,
                    Val::I(value) => value != 0,
                    _ => bail!("property {} is not a number", property.name),
                };
                set(&keyword, on != inverted);
            }
            Drawer::Enum(names) => {
                let chosen = match value {
                    // The engine truncates; what does not fit names nothing.
                    Val::F(value) if value.is_finite() && value.abs() < 2e9 => value as i64,
                    Val::F(_) => -1,
                    Val::I(value) => value,
                    _ => bail!("property {} is not a number", property.name),
                };
                for (index, keyword) in names.iter().enumerate() {
                    set(keyword, chosen == index as i64);
                }
            }
        }
    }
    Ok(())
}

/// The value of a material as a build writes it.
pub(super) fn bake(
    fields: &Map<String, Value>,
    references: &mut References<'_>,
    sources: &dyn NativeSources,
    inspected: Option<Parents>,
) -> Result<Val> {
    let Merged {
        shader: shader_source,
        settings,
        saved:
            Saved {
                textures: authored,
                mut ints,
                mut floats,
                mut colors,
            },
        variant,
        ..
    } = merged(
        fields,
        sources,
        &mut Vec::new(),
        inspected.unwrap_or(Parents::Authored),
    )?;
    if inspected.is_some() && !variant {
        return Err(unsupported("inspected material that is not a variant"));
    }
    let shader = match &shader_source {
        Value::Object(reference) => {
            let guid = text(get(reference, "guid"))?.to_ascii_lowercase();
            let file_id = text(get(reference, "fileID"))?.parse::<i64>().unwrap_or(0);
            (!guid.is_empty() && file_id != 0)
                .then(|| sources.shader(&guid, file_id))
                .flatten()
        }
        _ => None,
    };
    // Without its shader a material loses its properties, and whether the
    // shader exists is the project's to say.
    let Some(shader) = shader else {
        return Err(unsupported("material whose shader is not known"));
    };
    // A build rewrites the disabled passes of these two legacy shaders.
    if shader.name.starts_with("Particles/Standard ") {
        return Err(unsupported(format!(
            "material of the shader {}",
            shader.name
        )));
    }

    let shader_reference = references.pointer(&shader_source, "m_Shader")?;

    // A property the shader has and the material lacks takes the shader's
    // default.
    for property in &shader.properties {
        let name = property.name.as_str();
        let gamma = property.flags & FLAG_GAMMA != 0
            || (property.kind == PropertyKind::Color && property.flags & FLAG_HDR == 0);
        let default = |index: usize| -> Result<f32> {
            let value = property.default[index];
            if gamma && index < 3 {
                gamma_default(value, sources, name)
            } else {
                Ok(value)
            }
        };
        match property.kind {
            PropertyKind::Texture => {}
            PropertyKind::Int => {
                if !ints.contains_key(name) {
                    let value = property.default[0].to_bits() as i32;
                    ints.insert(name.to_string(), Val::I(value as i64));
                }
            }
            PropertyKind::Float | PropertyKind::Range => {
                if !floats.contains_key(name) {
                    floats.insert(name.to_string(), Val::F(default(0)?));
                }
            }
            PropertyKind::Color | PropertyKind::Vector => {
                if !colors.contains_key(name) {
                    let value = Val::map([
                        ("r", Val::F(default(0)?)),
                        ("g", Val::F(default(1)?)),
                        ("b", Val::F(default(2)?)),
                        ("a", Val::F(default(3)?)),
                    ]);
                    colors.insert(name.to_string(), value);
                }
            }
        }
    }

    let mut keywords = strings(fields, "m_ValidKeywords")?;
    keywords.extend(strings(fields, "m_InvalidKeywords")?);
    // The engine has run the drawers and the inspector of an inspected
    // variant; its keywords are what they left.
    if variant && inspected.is_none() {
        apply_drawers(&shader, &mut keywords, &|property| {
            let sheet = match property.kind {
                PropertyKind::Int => &ints,
                _ => &floats,
            };
            match sheet.get(&property.name) {
                Some(value) => Ok(value.clone()),
                None => bail!("property {} has no value", property.name),
            }
        })?;
    }
    keywords.sort();
    let (valid, invalid): (Vec<String>, Vec<String>) = keywords
        .into_iter()
        .partition(|keyword| shader.keywords.contains(keyword));

    let mut tags = Tags {
        shader: &shader,
        sources,
        written: Vec::new(),
    };
    // The engine reads the overrides in the order of their authored names
    // and a later one replaces an earlier one of the same tag.
    let mut overrides: Vec<(String, String)> = Vec::new();
    match get(fields, "stringTagMap") {
        Value::Null => {}
        Value::Object(map) => {
            for (name, value) in map {
                let (name, value) = (tags.spell(name)?, tags.spell(&text(value)?)?);
                match overrides.iter_mut().find(|(known, _)| *known == name) {
                    Some(known) => known.1 = value,
                    None => overrides.push((name, value)),
                }
            }
        }
        _ => bail!("stringTagMap is not a mapping"),
    }
    overrides.sort();
    let mut passes = Vec::new();
    for pass in strings(fields, "disabledShaderPasses")? {
        passes.push(Val::Str(tags.spell(&pass)?));
    }

    // A texture is kept only for a texture property the shader has.
    let mut textures: BTreeMap<&str, Val> = BTreeMap::new();
    for property in &shader.properties {
        if property.kind != PropertyKind::Texture {
            continue;
        }
        let name = property.name.as_str();
        let value = match authored.get(name) {
            Some(Value::Object(texture)) => Val::map([
                (
                    "m_Texture",
                    references.pointer(get(texture, "m_Texture"), name)?,
                ),
                (
                    "m_Scale",
                    numbers(get(texture, "m_Scale"), ["x", "y"], name)?,
                ),
                (
                    "m_Offset",
                    numbers(get(texture, "m_Offset"), ["x", "y"], name)?,
                ),
            ]),
            Some(_) => bail!("texture property {name} is not a mapping"),
            None => Val::map([
                ("m_Texture", Val::Zero),
                (
                    "m_Scale",
                    Val::map([("x", Val::F(1.0)), ("y", Val::F(1.0))]),
                ),
                ("m_Offset", Val::Zero),
            ]),
        };
        textures.insert(name, value);
    }
    fn list<Name: AsRef<str>>(entries: BTreeMap<Name, Val>) -> Val {
        Val::Seq(
            entries
                .into_iter()
                .map(|(name, value)| pair(name.as_ref(), value))
                .collect(),
        )
    }
    let names = |names: Vec<String>| Val::Seq(names.into_iter().map(Val::Str).collect());
    let [lightmap_flags, instancing, double_sided, render_queue] = settings;
    Ok(Val::map([
        ("m_Name", Val::Str(text(get(fields, "m_Name"))?)),
        ("m_Shader", shader_reference),
        ("m_ValidKeywords", names(valid)),
        ("m_InvalidKeywords", names(invalid)),
        ("m_LightmapFlags", lightmap_flags),
        ("m_EnableInstancingVariants", instancing),
        ("m_DoubleSidedGI", double_sided),
        ("m_CustomRenderQueue", render_queue),
        (
            "stringTagMap",
            Val::Seq(
                overrides
                    .iter()
                    .map(|(name, value)| pair(name, Val::str(value)))
                    .collect(),
            ),
        ),
        ("disabledShaderPasses", Val::Seq(passes)),
        (
            "m_SavedProperties",
            Val::map([
                ("m_TexEnvs", list(textures)),
                ("m_Ints", list(ints)),
                ("m_Floats", list(floats)),
                ("m_Colors", list(colors)),
            ]),
        ),
        ("m_BuildTextureStacks", Val::Zero),
    ]))
}

fn strings_or_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}
