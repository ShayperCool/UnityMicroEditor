//! Lexical and symlink path helpers for Unix paths.
use anyhow::{Result, anyhow, bail};
use std::{env, fs, os::unix::fs::MetadataExt};

pub fn is_abs(path: &str) -> bool {
    path.starts_with('/')
}

/// Lexically cleans a path: collapses separators, `.` and `..`.
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// Joins path elements: empty elements are ignored and the result is cleaned.
pub fn join(parts: &[&str]) -> String {
    let kept: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if kept.is_empty() {
        return String::new();
    }
    clean(&kept.join("/"))
}

/// The extension: the suffix starting at the last dot of the final element.
pub fn ext(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut index = bytes.len();
    while index > 0 && bytes[index - 1] != b'/' {
        index -= 1;
        if bytes[index] == b'.' {
            return &path[index..];
        }
    }
    ""
}

pub fn lower_ext(path: &str) -> String {
    rune_lower(ext(path))
}

/// The final element of a path.
pub fn base(path: &str) -> &str {
    if path.is_empty() {
        return ".";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// All but the final element of a path, cleaned.
pub fn dir(path: &str) -> String {
    let end = path.rfind('/').map_or(0, |i| i + 1);
    clean(&path[..end])
}

/// The base name without its extension.
pub fn stem(path: &str) -> &str {
    let name = base(path);
    name.strip_suffix(ext(path)).unwrap_or(name)
}

/// The relative path from base to target, lexically, for two paths of the same kind
/// (both absolute or both relative); an error when there is none.
pub fn rel(base_path: &str, target: &str) -> Result<String> {
    let base_clean = clean(base_path);
    let target_clean = clean(target);
    if target_clean == base_clean {
        return Ok(".".into());
    }
    let base_clean = if base_clean == "." {
        String::new()
    } else {
        base_clean
    };
    if is_abs(&base_clean) != is_abs(&target_clean) {
        bail!("Rel: can't make {target} relative to {base_path}");
    }
    let base_parts: Vec<&str> = base_clean.split('/').filter(|p| !p.is_empty()).collect();
    let target_parts: Vec<&str> = target_clean.split('/').filter(|p| !p.is_empty()).collect();
    let mut common = 0;
    while common < base_parts.len()
        && common < target_parts.len()
        && base_parts[common] == target_parts[common]
    {
        common += 1;
    }
    if base_parts[common..].contains(&"..") {
        bail!("Rel: can't make {target} relative to {base_path}");
    }
    let mut result: Vec<&str> = vec![".."; base_parts.len() - common];
    result.extend(&target_parts[common..]);
    if result.is_empty() {
        return Ok(".".into());
    }
    Ok(result.join("/"))
}

/// True when `relative` (a `rel` result) stays inside its base.
pub fn inside(relative: &str) -> bool {
    relative != ".." && !relative.starts_with("../")
}

/// The working directory: `$PWD` wins when it names the current directory.
pub fn getwd() -> Result<String> {
    if let Ok(pwd) = env::var("PWD")
        && pwd.starts_with('/')
        && let (Ok(a), Ok(b)) = (fs::metadata(&pwd), fs::metadata("."))
        && a.dev() == b.dev()
        && a.ino() == b.ino()
    {
        return Ok(pwd);
    }
    env::current_dir()?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow!("working directory is not UTF-8"))
}

/// The absolute, cleaned form of a path.
pub fn abs(path: &str) -> Result<String> {
    if is_abs(path) {
        return Ok(clean(path));
    }
    Ok(join(&[&getwd()?, path]))
}

/// Resolves symlinks: relative inputs stay relative unless a link is absolute.
pub fn eval_symlinks(path: &str) -> std::io::Result<String> {
    let mut path = path.to_string();
    let mut vol_len = usize::from(path.starts_with('/'));
    let mut dest = path[..vol_len].to_string();
    let mut links = 0;
    let (mut start, mut end) = (vol_len, vol_len);
    while start < path.len() {
        let bytes = path.as_bytes();
        while start < bytes.len() && bytes[start] == b'/' {
            start += 1;
        }
        end = start;
        while end < bytes.len() && bytes[end] != b'/' {
            end += 1;
        }
        if end == start {
            break;
        }
        let component = &path[start..end];
        if component == "." {
            start = end;
            continue;
        }
        if component == ".." {
            let cut = dest[vol_len..].rfind('/').map(|i| i + vol_len);
            match cut {
                Some(r) if &dest[r + 1..] != ".." => dest.truncate(r),
                _ => {
                    if dest.len() > vol_len {
                        dest.push('/');
                    }
                    dest.push_str("..");
                }
            }
            start = end;
            continue;
        }
        if !dest.is_empty() && !dest.ends_with('/') {
            dest.push('/');
        }
        dest.push_str(component);
        let info = fs::symlink_metadata(&dest).map_err(|e| os_error("lstat", &dest, e))?;
        if !info.file_type().is_symlink() {
            if !info.is_dir() && end < path.len() {
                return Err(os_error(
                    "lstat",
                    &dest,
                    std::io::Error::from_raw_os_error(20),
                ));
            }
            start = end;
            continue;
        }
        links += 1;
        if links > 255 {
            return Err(std::io::Error::other("EvalSymlinks: too many links"));
        }
        let link = fs::read_link(&dest)
            .map_err(|e| os_error("readlink", &dest, e))?
            .into_os_string()
            .into_string()
            .map_err(|_| std::io::Error::other("symlink target is not UTF-8"))?;
        path = format!("{link}{}", &path[end..]);
        if link.starts_with('/') {
            dest = "/".into();
            vol_len = 1;
            end = 1;
        } else {
            match dest[vol_len..].rfind('/') {
                Some(r) => dest.truncate(r + vol_len),
                None => dest.truncate(vol_len),
            }
            end = 0;
        }
        start = end;
    }
    let _ = end;
    Ok(clean(&dest))
}

/// Simple per-character lower-casing.
pub fn rune_lower(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Case-insensitive equality for the ASCII extensions compared in this package.
pub fn equal_fold(a: &str, b: &str) -> bool {
    rune_lower(a) == rune_lower(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lexical_helpers() {
        assert_eq!(clean("a//b/../c/."), "a/c");
        assert_eq!(clean("/../a"), "/a");
        assert_eq!(clean("../../a"), "../../a");
        assert_eq!(clean(""), ".");
        assert_eq!(ext("a/b.c/d"), "");
        assert_eq!(ext("a/.unity"), ".unity");
        assert_eq!(ext("a/b.tar.gz"), ".gz");
        assert_eq!(base("a/b/"), "b");
        assert_eq!(dir("a/b"), "a");
        assert_eq!(dir("b"), ".");
        assert_eq!(stem("Assets/A.b.prefab"), "A.b");
        assert_eq!(rel("/a/b", "/a/b/c").unwrap(), "c");
        assert_eq!(rel("/a/b", "/a").unwrap(), "..");
        assert_eq!(rel("/a/b", "/a/c/d").unwrap(), "../c/d");
        assert_eq!(rel("/a", "/a").unwrap(), ".");
        assert_eq!(join(&["/a", "", "b/../c"]), "/a/c");
    }
}

/// A path error's text: `<op> <path>: <errno text>`.
#[derive(Debug)]
pub struct PathError(String);
impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PathError {}

/// Wraps an OS failure as a `PathError` message, keeping its `ErrorKind`.
pub fn os_error(op: &str, path: &str, error: std::io::Error) -> std::io::Error {
    if error.get_ref().is_some_and(|inner| inner.is::<PathError>()) {
        return error;
    }
    let text = error.to_string();
    let reason = match text.find(" (os error ") {
        Some(at) => {
            let mut chars = text[..at].chars();
            chars
                .next()
                .map(|c| c.to_lowercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        }
        None => text,
    };
    std::io::Error::new(error.kind(), PathError(format!("{op} {path}: {reason}")))
}
