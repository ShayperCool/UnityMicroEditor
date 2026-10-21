//! The names of the shaders built into the Editor, which a shader of the
//! project may name only for passes the worker copies in.
use crate::{playerbake, sysutil};
use anyhow::{Context as _, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

/// The names of the shaders built into an Editor: those of its
/// `Data/Resources/unity_builtin_extra`, which a worker of that Editor
/// finds by name when a shader source uses one of their passes. Read once
/// per Editor.
pub(crate) fn editor_builtin_shader_names(editor: &str) -> Result<Arc<BTreeSet<String>>> {
    static KNOWN: Mutex<BTreeMap<String, Arc<BTreeSet<String>>>> = Mutex::new(BTreeMap::new());
    let mut known = KNOWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(names) = known.get(editor) {
        return Ok(names.clone());
    }
    let path = sysutil::join(&[&sysutil::dir(editor), "Data/Resources/unity_builtin_extra"]);
    let file = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let names: BTreeSet<String> = playerbake::read_shader_names(&file)
        .with_context(|| format!("read the shaders of {path}"))?
        .into_iter()
        .collect();
    let names = Arc::new(names);
    known.insert(editor.to_string(), names.clone());
    Ok(names)
}
