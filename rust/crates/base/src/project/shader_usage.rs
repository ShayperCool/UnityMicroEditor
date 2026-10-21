//! What a baked shader takes from other sources of the project: the
//! materials that use it and the shaders it names. A build compiles the
//! `shader_feature` variants of a shader only in the combinations its
//! materials enable, and finds a `Fallback` or `UsePass` shader by name.
use super::{lexpath, session::Session};
use anyhow::Result;
use std::sync::{Arc, LazyLock};

/// A material that refers to a shader, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderMaterial {
    pub path: String,
    pub sha256: String,
    pub source: Vec<u8>,
}

static SHADER_NAME: LazyLock<regex::bytes::Regex> =
    LazyLock::new(|| regex::bytes::Regex::new(r#"(?s-u)^\s*Shader\s*"([^"\r\n]*)""#).unwrap());

/// A source: its path, its GUID and its bytes.
type Source = (String, String, Arc<[u8]>);

impl Session {
    /// Reads every source of an extension, in path order, without making it
    /// a dependency of the revision being prepared.
    fn sources_of(&self, extension: &str) -> Result<Vec<Source>> {
        let mut s = self.lock();
        s.index_guids()?;
        let mut paths: Vec<(String, String)> = s
            .by_path
            .iter()
            .filter(|(path, _)| lexpath::lower_ext(path) == extension)
            .map(|(path, guid)| (path.clone(), guid.clone()))
            .collect();
        paths.sort();
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let mut sources = Vec::new();
        let mut failure = None;
        for (path, guid) in paths {
            match s.source(&path).and_then(|physical| s.read(&physical, true)) {
                Ok((Some(source), _)) => sources.push((path, guid, source)),
                Ok((None, _)) => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        s.cache.observed = observed;
        s.cache.stats = stats;
        match failure {
            Some(error) => Err(error),
            None => Ok(sources),
        }
    }

    /// The `.mat` sources of the project that name the shader of a GUID, in
    /// path order.
    pub fn shader_materials(&self, guid: &str) -> Result<Vec<ShaderMaterial>> {
        if guid.len() != 32 {
            return Ok(Vec::new());
        }
        Ok(self
            .sources_of(".mat")?
            .into_iter()
            .filter(|(_, _, source)| {
                source
                    .windows(guid.len())
                    .any(|window| window.eq_ignore_ascii_case(guid.as_bytes()))
            })
            .map(|(path, _, source)| ShaderMaterial {
                path,
                sha256: crate::sha256(&source),
                source: source.to_vec(),
            })
            .collect())
    }

    /// The `.shader` sources of the project that declare a shader name, each
    /// with its GUID, in path order.
    pub fn shaders_named(&self, name: &str) -> Result<Vec<(String, String)>> {
        Ok(self
            .sources_of(".shader")?
            .into_iter()
            .filter(|(_, _, source)| {
                super::shader_sources::shader_without_comments(source)
                    .ok()
                    .and_then(|text| {
                        SHADER_NAME
                            .captures(&text)
                            .map(|found| found[1] == *name.as_bytes())
                    })
                    .unwrap_or(false)
            })
            .map(|(path, guid, _)| (path, guid))
            .collect())
    }
}
