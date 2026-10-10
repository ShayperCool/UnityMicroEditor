//! Path checks and guarded file access shared by the two shader commands.
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
};

/// Resolves `.` and `..` lexically, without touching the file system. A
/// relative path that climbs above its start keeps the leading `..`.
pub(crate) fn normalize(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if rooted => {}
            part => parts.push(part),
        }
    }
    match (rooted, parts.is_empty()) {
        (true, _) => format!("/{}", parts.join("/")),
        (false, true) => ".".into(),
        (false, false) => parts.join("/"),
    }
}

/// An absolute path that is already in its normalized spelling.
pub(crate) fn is_normalized_absolute(path: &str) -> bool {
    path.starts_with('/') && normalize(path) == path
}

/// The directory part of a normalized path.
pub(crate) fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(index) => &path[..index],
        None => ".",
    }
}

/// Whether normalized `path` is `root` or lies beneath it.
pub(crate) fn within(root: &str, path: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || root == "/")
}

/// The path with every symlink resolved, as text.
pub(crate) fn canonical(path: &str) -> io::Result<String> {
    fs::canonicalize(path)?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path is not valid UTF-8"))
}

/// Reads a file of at most `limit` bytes.
pub(crate) fn read_bounded(path: &str, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("open {path}"))?;
    let mut data = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut data)
        .with_context(|| format!("read {path}"))?;
    if data.len() as u64 > limit {
        bail!("input exceeds supported size: {path}");
    }
    Ok(data)
}

/// Creates a directory and its missing parents, private to the user.
pub(crate) fn create_private_dir(path: &str) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create directory {path}"))
}

/// Pretty-printed JSON followed by a newline.
pub(crate) fn json_document<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec_pretty(value)?;
    encoded.push(b'\n');
    Ok(encoded)
}

/// Writes a set of new files with mode 0600 and never replaces an existing
/// one. A failure midway removes the files this call created.
pub(crate) fn write_new_files(files: &[(String, Vec<u8>)]) -> Result<()> {
    let mut written: Vec<&str> = Vec::new();
    let mut write = || -> Result<()> {
        for (path, contents) in files {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("create {path}"))?;
            written.push(path);
            file.write_all(contents)
                .with_context(|| format!("write {path}"))?;
        }
        Ok(())
    };
    let result = write();
    if result.is_err() {
        for path in written {
            let _ = fs::remove_file(path);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_path_rules() {
        for (input, expected) in [
            ("", "."),
            ("/", "/"),
            ("//a//b/./c/..", "/a/b"),
            ("/../a", "/a"),
            ("a/../..", ".."),
            ("../../a/", "../../a"),
            ("a/b/../../..", ".."),
            ("./", "."),
        ] {
            assert_eq!(normalize(input), expected, "normalize({input:?})");
        }
        assert!(is_normalized_absolute("/a/b") && is_normalized_absolute("/"));
        for path in ["", "a", "/a/", "/a//b", "/a/./b", "/a/../b"] {
            assert!(!is_normalized_absolute(path), "{path:?}");
        }
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("a"), ".");
        assert!(within("/p/Temp", "/p/Temp") && within("/p/Temp", "/p/Temp/a/b"));
        assert!(within("/", "/a"));
        assert!(!within("/p/Temp", "/p/Temporary") && !within("/p/Temp", "/p"));
    }
}
