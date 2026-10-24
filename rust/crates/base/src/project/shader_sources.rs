//! Literal include closure of an authored Shader, as a guard on later edits.
use super::{lexpath, types::ShaderSourceRequirement};
use anyhow::{Result, anyhow, bail};
use regex::bytes::Regex;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::Read,
    sync::LazyLock,
};

static INCLUDE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m-u)^[\t\n\f\r ]*#[\t\n\f\r ]*(?:include|include_with_pragmas)\b([^\r\n]*)")
        .unwrap()
});
static INCLUDE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?-u)^[\t\n\f\r ]*(?:"([^"\r\n]+)"|<([^>\r\n]+)>)[\t\n\f\r ]*$"#).unwrap()
});

pub(crate) enum SourceError {
    /// Carries the text of a does-not-exist error.
    NotExist(String),
    Other(anyhow::Error),
}
impl From<SourceError> for anyhow::Error {
    fn from(error: SourceError) -> Self {
        match error {
            SourceError::NotExist(text) => anyhow!(text),
            SourceError::Other(error) => error,
        }
    }
}
fn io_error(path: &str, error: std::io::Error) -> SourceError {
    if error.kind() == std::io::ErrorKind::NotFound {
        // Symlink resolution names the first missing component.
        SourceError::NotExist(
            crate::sysutil::eval_symlinks(path)
                .err()
                .map_or_else(|| "file does not exist".into(), |e| e.message().to_string()),
        )
    } else {
        SourceError::Other(super::session::path_error(path, error))
    }
}

pub(crate) fn valid_shader_source(logical: &str) -> bool {
    lexpath::clean(logical) == logical
        && !logical.contains('\\')
        && (logical.starts_with("Assets/") || logical.starts_with("Packages/"))
}

pub(crate) fn shader_source_path(project: &str, logical: &str) -> Result<String, SourceError> {
    if !valid_shader_source(logical) {
        return Err(SourceError::Other(anyhow!(
            "Shader source/include path is invalid: {logical}"
        )));
    }
    let physical = lexpath::join(&[project, logical]);
    let canonical = match lexpath::eval_symlinks(&physical) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && logical.starts_with("Packages/") => {
            let parts: Vec<&str> = logical.split('/').collect();
            if parts.len() < 3 || parts[1].contains(['*', '?', '[']) {
                return Err(SourceError::Other(anyhow!(
                    "Shader package path is incomplete or invalid: {logical}"
                )));
            }
            let cache = lexpath::join(&[project, "Library/PackageCache"]);
            let prefix = format!("{}@", parts[1]);
            let mut candidates: Vec<String> = fs::read_dir(&cache)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                        .filter(|name| name.starts_with(&prefix))
                        .collect()
                })
                .unwrap_or_default();
            if candidates.len() > 1 {
                return Err(SourceError::Other(anyhow!(
                    "Shader package source is ambiguous: {logical}"
                )));
            }
            let Some(candidate) = candidates.pop() else {
                return Err(SourceError::NotExist("file does not exist".into()));
            };
            let nested = lexpath::join(&[&cache, &candidate, &parts[2..].join("/")]);
            lexpath::eval_symlinks(&nested).map_err(|e| io_error(&nested, e))?
        }
        other => other.map_err(|e| io_error(&physical, e))?,
    };
    let canonical = lexpath::abs(&canonical).map_err(SourceError::Other)?;
    if !lexpath::rel(project, &canonical).is_ok_and(|r| lexpath::inside(&r)) {
        return Err(SourceError::Other(anyhow!(
            "Shader include leaves current project: {logical}"
        )));
    }
    Ok(canonical)
}

fn read_shader_source(physical: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    fs::File::open(physical)
        .map_err(|e| lexpath::os_error("open", physical, e))?
        .take((4 << 20) + 1)
        .read_to_end(&mut data)?;
    if data.len() > 4 << 20 {
        bail!("Shader source/include exceeds supported size: {physical}");
    }
    Ok(data)
}

/// Removes comments while preserving line boundaries and quoted paths; `//`
/// inside a quoted string is not a comment. A leading byte order mark is
/// blanked too, as the Editor reads past it.
pub fn shader_without_comments(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = input.to_vec();
    if out.starts_with(b"\xEF\xBB\xBF") {
        out[..3].fill(b' ');
    }
    let (mut quoted, mut block, mut line) = (false, false, false);
    let mut i = 0;
    while i < out.len() {
        let c = out[i];
        if line {
            if c == b'\n' || c == b'\r' {
                line = false;
            } else {
                out[i] = b' ';
            }
        } else if block {
            if c == b'*' && out.get(i + 1) == Some(&b'/') {
                out[i] = b' ';
                out[i + 1] = b' ';
                block = false;
                i += 1;
            } else if c != b'\n' && c != b'\r' {
                out[i] = b' ';
            }
        } else if quoted {
            if c == b'\\' && i + 1 < out.len() {
                i += 1;
            } else if c == b'"' {
                quoted = false;
            }
        } else if c == b'"' {
            quoted = true;
        } else if c == b'/' && matches!(out.get(i + 1), Some(b'*' | b'/')) {
            block = out[i + 1] == b'*';
            line = out[i + 1] == b'/';
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 1;
        }
        i += 1;
    }
    if block {
        bail!("unterminated Shader source block comment");
    }
    Ok(out)
}

struct Closure<'a> {
    project: &'a str,
    source: &'a str,
    seen: HashSet<String>,
    missing: BTreeSet<String>,
    files: BTreeMap<String, ShaderSourceRequirement>,
}
impl Closure<'_> {
    fn visit(&mut self, logical: &str) -> Result<()> {
        if self.seen.contains(logical) {
            return Ok(());
        }
        if self.seen.len() >= 2048 {
            bail!("Shader include closure exceeds the supported size");
        }
        self.seen.insert(logical.into());
        let physical = match shader_source_path(self.project, logical) {
            Err(SourceError::NotExist(_)) if logical != self.source => {
                self.missing.insert(logical.into());
                return Ok(());
            }
            other => other.map_err(|e| {
                anyhow::Error::from(e).context(format!("resolve Shader source/include {logical}"))
            })?,
        };
        let data = read_shader_source(&physical)?;
        self.files.insert(
            logical.into(),
            ShaderSourceRequirement {
                path: logical.into(),
                source_path: physical,
                sha256: crate::hex(&Sha256::digest(&data)),
            },
        );
        let cleaned = shader_without_comments(&data)
            .map_err(|e| e.context(format!("read Shader includes {logical}")))?;
        for directive in INCLUDE_DIRECTIVE.captures_iter(&cleaned) {
            let argument = &directive[1];
            let Some(literal) = INCLUDE_LITERAL.captures(argument) else {
                bail!(
                    "Shader snapshot requires a literal include in {logical}: {}",
                    String::from_utf8_lossy(argument).trim()
                );
            };
            let name = literal
                .get(1)
                .or(literal.get(2))
                .map_or(&b""[..], |m| m.as_bytes());
            let name = String::from_utf8_lossy(name).into_owned();
            if name.contains('\\') || lexpath::is_abs(&name) {
                bail!("Shader include path is unsupported in {logical}: {name}");
            }
            let mut included = lexpath::clean(&name);
            if !included.starts_with("Assets/") && !included.starts_with("Packages/") {
                // The compiler looks beside the including file, then beside
                // the shader it compiles.
                let beside = |file: &str| lexpath::clean(&format!("{}/{name}", lexpath::dir(file)));
                included = beside(logical);
                let in_shader_folder = beside(self.source);
                if in_shader_folder != included
                    && valid_shader_source(&in_shader_folder)
                    && matches!(
                        shader_source_path(self.project, &included),
                        Err(SourceError::NotExist(_))
                    )
                    && shader_source_path(self.project, &in_shader_folder).is_ok()
                {
                    included = in_shader_folder;
                }
            }
            if !valid_shader_source(&included) {
                bail!("Shader include escapes source roots in {logical}: {name}");
            }
            self.visit(&included)?;
        }
        Ok(())
    }
}

/// Snapshots all literal includes, including conditional branches. Missing
/// optional includes are recorded as absent and must remain absent. This is a
/// guard on later edits, not build provenance.
pub fn collect_shader_source_requirements(
    project: &str,
    source: &str,
) -> Result<(Vec<ShaderSourceRequirement>, Vec<String>)> {
    let canonical = lexpath::eval_symlinks(project)
        .map_err(|e| super::session::path_error(project, e))
        .and_then(|resolved| lexpath::abs(&resolved))?;
    if !valid_shader_source(source) || lexpath::ext(source) != ".shader" {
        bail!("Shader source path is invalid: {source}");
    }
    let mut closure = Closure {
        project: &canonical,
        source,
        seen: HashSet::new(),
        missing: BTreeSet::new(),
        files: BTreeMap::new(),
    };
    closure.visit(source)?;
    Ok((
        closure.files.into_values().collect(),
        closure.missing.into_iter().collect(),
    ))
}

pub fn validate_shader_source_requirements(
    project: &str,
    source: &str,
    expected: &[ShaderSourceRequirement],
    missing: &[String],
) -> Result<()> {
    let (actual, absent) = collect_shader_source_requirements(project, source)?;
    let mut owned = expected.to_vec();
    owned.sort_by(|a, b| a.path.cmp(&b.path));
    if actual != owned {
        bail!("compiled Shader snapshot source/include closure changed or was incomplete");
    }
    let mut wanted = missing.to_vec();
    wanted.sort();
    if absent != wanted {
        bail!("compiled Shader snapshot optional include availability changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{collect_shader_source_requirements, shader_without_comments};

    #[test]
    fn an_include_is_found_beside_the_shader_when_not_beside_the_includer() {
        let project = tempfile::tempdir().unwrap();
        let shaders = project.path().join("Assets/Shaders");
        std::fs::create_dir_all(shaders.join("Inc")).unwrap();
        std::fs::write(shaders.join("A.shader"), "#include \"Inc/B.cginc\"\n").unwrap();
        std::fs::write(shaders.join("Inc/B.cginc"), "#include \"Inc/C.cginc\"\n").unwrap();
        std::fs::write(shaders.join("Inc/C.cginc"), "").unwrap();
        let project = project.path().to_str().unwrap();
        let (files, missing) =
            collect_shader_source_requirements(project, "Assets/Shaders/A.shader").unwrap();
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Assets/Shaders/A.shader",
                "Assets/Shaders/Inc/B.cginc",
                "Assets/Shaders/Inc/C.cginc"
            ]
        );
        assert!(missing.is_empty());
    }

    #[test]
    fn a_byte_order_mark_and_comments_are_blanked() {
        let source = b"\xEF\xBB\xBF// note\nShader \"A//B\" { /* x */ }";
        let cleaned = shader_without_comments(source).unwrap();
        assert_eq!(cleaned.len(), source.len());
        assert_eq!(&cleaned[..], b"          \nShader \"A//B\" {         }");
    }
}
