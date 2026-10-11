//! Which materials of the project use a shader. A build compiles the
//! `shader_feature` variants of a shader only in the combinations its
//! materials enable, so a baked shader depends on them.
use super::{lexpath, session::Session};
use anyhow::Result;

/// A material that refers to a shader, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderMaterial {
    pub path: String,
    pub sha256: String,
    pub source: Vec<u8>,
}

impl Session {
    /// The `.mat` sources of the project that name the shader of a GUID, in
    /// path order.
    pub fn shader_materials(&self, guid: &str) -> Result<Vec<ShaderMaterial>> {
        let mut s = self.lock();
        s.index_guids()?;
        let mut paths: Vec<String> = s
            .by_path
            .keys()
            .filter(|path| lexpath::lower_ext(path) == ".mat")
            .cloned()
            .collect();
        paths.sort();
        // Reading here is no dependency of the revision being prepared.
        let observed = std::mem::take(&mut s.cache.observed);
        let stats = std::mem::take(&mut s.cache.stats);
        let mut materials = Vec::new();
        let mut failure = None;
        for path in paths {
            let read = s.source(&path).and_then(|physical| s.read(&physical, true));
            match read {
                Ok((Some(source), _)) => {
                    if guid.len() == 32
                        && source
                            .windows(guid.len())
                            .any(|window| window.eq_ignore_ascii_case(guid.as_bytes()))
                    {
                        materials.push(ShaderMaterial {
                            path,
                            sha256: crate::sha256(&source),
                            source: source.to_vec(),
                        });
                    }
                }
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
            None => Ok(materials),
        }
    }
}
