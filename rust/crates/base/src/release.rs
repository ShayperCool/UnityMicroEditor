//! The Unity releases the tool knows, and the release a project selects.
//!
//! A project's `ProjectVersion.txt` names the release it was last opened
//! with; every record the tool writes for that project (receipts, profiles,
//! proofs) carries that release, and an installation serves it only when
//! `Unity -version` reports the same. The private protocols of each release
//! live in `engine`; this list is what the layers below it check against.
use anyhow::{Context as _, Result, anyhow, bail};
use std::io::Read;

/// One Unity release.
#[derive(Debug, PartialEq, Eq)]
pub struct Release {
    /// As `ProjectVersion.txt` and `Unity -version` name it.
    pub version: &'static str,
    pub revision: &'static str,
}

/// Every known release; the first is the verified profile.
pub static RELEASES: &[Release] = &[
    Release {
        version: "6000.6.3f1",
        revision: "45d8eee7de74",
    },
    Release {
        version: "6000.0.71f1",
        revision: "907bc2d768b5",
    },
];

/// The verified release.
pub fn verified() -> &'static Release {
    &RELEASES[0]
}

/// The versions the tool supports, comma-separated.
pub fn supported_versions() -> String {
    RELEASES
        .iter()
        .map(|release| release.version)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The release of a version.
pub fn supported(version: &str) -> Result<&'static Release> {
    RELEASES
        .iter()
        .find(|release| release.version == version)
        .ok_or_else(|| {
            anyhow!(
                "Unity {version} is not supported (supported: {})",
                supported_versions()
            )
        })
}

/// Whether the tool knows a version.
pub fn is_supported(version: &str) -> bool {
    supported(version).is_ok()
}

/// The release named by the `m_EditorVersion` line of a `ProjectVersion.txt`.
pub fn project_version_of(text: &[u8]) -> Result<String> {
    String::from_utf8_lossy(text)
        .lines()
        .find_map(|line| line.trim().strip_prefix("m_EditorVersion:"))
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
        .context("ProjectVersion.txt names no m_EditorVersion")
}

/// The release a project was last opened with.
pub fn project_version(project: &str) -> Result<String> {
    let path = crate::sysutil::join(&[project, "ProjectSettings/ProjectVersion.txt"]);
    let file = std::fs::File::open(&path).with_context(|| format!("open {path}"))?;
    let mut text = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut text)
        .with_context(|| format!("read {path}"))?;
    if text.len() > 64 * 1024 {
        bail!("{path} exceeds supported bounds");
    }
    project_version_of(&text)
}

/// The supported release a project selects.
pub fn of_project(project: &str) -> Result<&'static Release> {
    supported(&project_version(project)?)
}

/// The version of the release a project selects; for records that carry it.
pub fn project_release(project: &str) -> Result<&'static str> {
    Ok(of_project(project)?.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_selects_its_release() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().to_str().unwrap();
        std::fs::create_dir_all(directory.path().join("ProjectSettings")).unwrap();
        let version = |text: &str| {
            std::fs::write(
                directory.path().join("ProjectSettings/ProjectVersion.txt"),
                text,
            )
            .unwrap();
            of_project(project).map(|release| release.revision)
        };
        assert_eq!(
            version("m_EditorVersion: 6000.0.71f1\r\nm_EditorVersionWithRevision: 6000.0.71f1 (907bc2d768b5)\r\n").unwrap(),
            "907bc2d768b5"
        );
        assert_eq!(
            version("m_EditorVersion: 6000.6.3f1\n").unwrap(),
            "45d8eee7de74"
        );
        let error = version("m_EditorVersion: 2022.3.62f1\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Unity 2022.3.62f1 is not supported (supported: 6000.6.3f1, 6000.0.71f1)"
        );
        assert!(version("m_EditorVersionWithRevision: 6000.6.3f1 (45d8eee7de74)\n").is_err());
        assert_eq!(verified().version, crate::ENGINE_VERSION);
    }
}
