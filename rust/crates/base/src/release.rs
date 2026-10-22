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

/// The supported release a project selects. A project without a
/// `ProjectVersion.txt` (one no Editor has opened yet) is the verified
/// release's, as an Editor takes such a project for its own.
pub fn of_project(project: &str) -> Result<&'static Release> {
    let path = crate::sysutil::join(&[project, "ProjectSettings/ProjectVersion.txt"]);
    if std::fs::symlink_metadata(&path)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(verified());
    }
    supported(&project_version(project)?)
}

/// The version of the release a project selects; for records that carry it.
pub fn project_release(project: &str) -> Result<&'static str> {
    Ok(of_project(project)?.version)
}

/// The release an installation runs, as `Unity -version` prints it. An
/// executable is asked once per process.
pub fn installed_version(editor: &str) -> Result<String> {
    use std::{
        collections::BTreeMap,
        process::{Command, Stdio},
        sync::{Mutex, mpsc},
        time::Duration,
    };
    static KNOWN: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
    if let Some(version) = KNOWN.lock().unwrap().get(editor) {
        return Ok(version.clone());
    }
    // A version-only invocation opens no project and returns at once; a
    // bound keeps a wrong executable from holding the caller.
    let mut child = Command::new(editor)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run {editor} -version"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let read = stdout.take(4096).read_to_end(&mut output);
        let _ = sender.send(read.map(|_| output));
    });
    let output = receiver.recv_timeout(Duration::from_secs(15));
    let _ = child.kill();
    let _ = child.wait();
    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => bail!("read {editor} -version: {error}"),
        Err(_) => bail!("{editor} -version did not answer"),
    };
    let version = String::from_utf8_lossy(&output).trim().to_string();
    if version.is_empty() || version.contains(char::is_whitespace) {
        bail!("{editor} -version did not print a Unity version");
    }
    KNOWN
        .lock()
        .unwrap()
        .insert(editor.to_string(), version.clone());
    Ok(version)
}

/// The version symbols the Editor defines for code it compiles in a
/// release (`UNITY_6000_0_71`, `UNITY_6000_0`, `UNITY_6000` and every
/// `UNITY_X_Y_OR_NEWER` up to its own).
pub fn version_defines(version: &str) -> Result<Vec<String>> {
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("Unity version {version} has no major.minor.patch form");
    };
    let patch: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let (major, minor): (u32, u32) = (
        major.parse().context("Unity major version")?,
        minor.parse().context("Unity minor version")?,
    );
    if major != 6000 || patch.is_empty() {
        bail!("version symbols are known for Unity 6 releases only, not {version}");
    }
    let mut defines = vec![
        format!("UNITY_{major}_{minor}_{patch}"),
        format!("UNITY_{major}_{minor}"),
        format!("UNITY_{major}"),
    ];
    for (year, minors) in [
        (5, 3..=6),
        (2017, 1..=4),
        (2018, 1..=4),
        (2019, 1..=4),
        (2020, 1..=3),
        (2021, 1..=3),
        (2022, 1..=3),
        (2023, 1..=3),
    ] {
        for old in minors {
            defines.push(format!("UNITY_{year}_{old}_OR_NEWER"));
        }
    }
    for newer in 0..=minor {
        defines.push(format!("UNITY_6000_{newer}_OR_NEWER"));
    }
    Ok(defines)
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
        std::fs::remove_file(directory.path().join("ProjectSettings/ProjectVersion.txt")).unwrap();
        assert_eq!(of_project(project).unwrap(), verified());
    }

    #[test]
    fn version_symbols_match_the_editors() {
        // As 6000.0.71f1 passes them to the compiler (Library/Bee input data).
        let defines = version_defines("6000.0.71f1").unwrap();
        assert_eq!(
            defines[..3],
            ["UNITY_6000_0_71", "UNITY_6000_0", "UNITY_6000"]
        );
        assert_eq!(defines[3], "UNITY_5_3_OR_NEWER");
        assert_eq!(defines.last().unwrap(), "UNITY_6000_0_OR_NEWER");
        assert_eq!(defines.len(), 3 + 4 + 3 * 4 + 4 * 3 + 1);
        let newer = version_defines("6000.6.3f1").unwrap();
        assert!(newer.ends_with(&[
            "UNITY_6000_5_OR_NEWER".into(),
            "UNITY_6000_6_OR_NEWER".into()
        ]));
    }
}
