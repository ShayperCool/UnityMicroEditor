//! Material variants of a shader with an inspector.
//!
//! A build writes such a variant as a whole material: its parents are laid
//! under it and the inspector of its shader, which is C# of a package,
//! derives keywords, the render queue, tags and passes from the merged
//! values. The authored fields of the variant say none of that, so the
//! native importer supplies them (`native::inspect_material_variants`,
//! `playerbake::flatten_inspected_variant`) and the session publishes those
//! in place of the authored ones, as a material without a parent. They are
//! taken only while every file they were made from is unchanged; otherwise
//! the importer is asked again.
use super::{
    lexpath,
    native::valid_native_guid,
    session::{Inner, Session, needs_import},
    types::Document,
    value::{self, Map, Value, get, object},
};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashSet},
    sync::LazyLock,
};

/// The fields of one variant as a build writes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectedMaterial {
    pub guid: String,
    pub source_path: String,
    /// The hash of every file the fields were made from, by logical path:
    /// the variant, its parents, its shader with what that is made of, the
    /// render pipeline asset, the settings and the package files.
    pub ingredient_sha256: BTreeMap<String, String>,
    /// The fields, as the JSON of an authored material without a parent.
    pub data_json: String,
}

/// The serialization of a material the session reads from its text. The
/// engine upgrades an older one when it loads it, which takes the shader:
/// it sorts the keywords into those the shader declares and those it does
/// not.
pub(crate) const MATERIAL_VERSION: i64 = 8;

/// Whether a material is of an older serialization than the session reads.
pub fn outdated_material(material: &Document) -> bool {
    value::number(get(material.fields(), "serializedVersion")) < MATERIAL_VERSION
}

/// How long a chain of parents is followed.
const DEEPEST_PARENT: usize = 32;

static CUSTOM_EDITOR: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r#"(?m-u)^\s*CustomEditor(?:ForRenderPipeline)?\s+""#).unwrap()
});

impl Session {
    /// Takes the fields of a variant as a build writes them. They replace
    /// the authored fields while their ingredients are unchanged.
    pub fn set_inspected_material(&self, material: &InspectedMaterial) -> Result<()> {
        if !valid_native_guid(&material.guid)
            || !material
                .ingredient_sha256
                .contains_key(&material.source_path)
        {
            bail!("inspected material descriptor is invalid");
        }
        let fields: Value = value::decode_use_number(&material.data_json)?;
        let Value::Object(fields) = fields else {
            bail!("inspected material fields are not an object");
        };
        if value::map_file_id(object(get(&fields, "m_Parent"))) != "0" {
            bail!("inspected material fields name a parent");
        }
        self.lock()
            .inspected_materials
            .insert(material.guid.clone(), (material.clone(), fields));
        Ok(())
    }
}

impl Session {
    /// Lets a material that is not such a variant change after the
    /// baseline was made. The parent of an inspected variant is no asset
    /// of the revision, only a file the variant's fields were made from,
    /// and those fields are checked against it by hash.
    pub fn release_authored_material(&self, path: &str) {
        let mut s = self.lock();
        s.critical.remove(path);
        s.critical.remove(&format!("{path}.meta"));
    }
}

impl Inner {
    /// Whether the material at the top of a variant's chain uses a shader
    /// source that names an inspector: a shader graph, whose shader always
    /// does, or a `.shader` with a `CustomEditor`. A chain that leaves the
    /// material sources of the project (a parent inside a model, a shader
    /// built into the engine) is not one of these.
    fn variant_with_inspector(&mut self, path: &str, material: &Document) -> Result<bool> {
        let mut seen = HashSet::from([path.to_string()]);
        let mut fields = material.fields().clone();
        loop {
            let parent = object(get(&fields, "m_Parent"));
            let id = value::map_file_id(parent).into_owned();
            if id == "0" {
                break;
            }
            let parent_path = self.path_of(&value::text(parent, "guid")).to_string();
            if lexpath::ext(&parent_path) != ".mat"
                || seen.len() > DEEPEST_PARENT
                || !seen.insert(parent_path.clone())
            {
                return Ok(false);
            }
            let physical = self.source(&parent_path)?;
            let documents = self.parse(&physical)?;
            let Some(found) = documents
                .iter()
                .find(|document| document.class_id == 21 && document.file_id == id)
            else {
                return Ok(false);
            };
            fields = found.fields().clone();
        }
        let shader = value::text(object(get(&fields, "m_Shader")), "guid").into_owned();
        let shader_path = self.path_of(&shader).to_string();
        match lexpath::lower_ext(&shader_path).as_str() {
            ".shadergraph" => Ok(true),
            ".shader" => {
                let physical = self.source(&shader_path)?;
                let (text, _) = self.read(&physical, true)?;
                Ok(text.is_some_and(|text| CUSTOM_EDITOR.is_match(&text)))
            }
            _ => Ok(false),
        }
    }

    /// The fields a variant of a shader with an inspector is published
    /// with, and those of a material of an older serialization as the
    /// engine upgrades it; `None` for any other material. When the importer
    /// has not supplied them from the files as they are now, it is asked
    /// to.
    pub(crate) fn inspected_material_fields(
        &mut self,
        path: &str,
        material: &Document,
    ) -> Result<Option<Map>> {
        if !outdated_material(material)
            && (value::map_file_id(object(material.get("m_Parent"))) == "0"
                || !self.variant_with_inspector(path, material)?)
        {
            return Ok(None);
        }
        let guid = self.guid_of(path).to_string();
        let Some((known, fields)) = self.inspected_materials.get(&guid).cloned() else {
            return Err(needs_import(path));
        };
        if known.source_path != path {
            return Err(needs_import(path));
        }
        for (ingredient, expected) in &known.ingredient_sha256 {
            let current = self
                .source(ingredient)
                .and_then(|physical| self.read(&physical, false))
                .map(|(_, hash)| hash);
            if current.ok().as_ref() != Some(expected) {
                return Err(needs_import(path));
            }
        }
        // A change of what the fields were made from has to be seen. A
        // parent is no asset of the revision any more, and the hash the
        // baseline holds of it is not what the fields were made from: the
        // hashes just compared are.
        for ingredient in known.ingredient_sha256.keys() {
            if lexpath::lower_ext(ingredient.strip_suffix(".meta").unwrap_or(ingredient)) == ".mat"
            {
                self.critical.remove(ingredient);
            }
            if !ingredient.ends_with(".meta") {
                self.depend(ingredient)?;
            }
        }
        Ok(Some(fields))
    }
}
