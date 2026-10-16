//! The settings managers a prepared Player reads (`globalgamemanagers`),
//! written from the project's `ProjectSettings` instead of being kept as the
//! host build left them. A preparation keeps the host's own file beside it as
//! the base of every later write, so the managers a build derives and the
//! asset references stay the host's, and an edit of a settings file reaches
//! the next Player without a host build.
use super::sys;
use crate::{playerbake, sha256, sysutil::join};
use anyhow::{Context as _, Result};
use std::path::Path;

/// The host's `globalgamemanagers`, kept in the data directory of a prepared
/// Player. A Player prepared before settings were written lacks it and keeps
/// the host's settings.
pub const HOST_MANAGERS: &str = "UnityMicroEditorHostManagers";
const MANAGERS: &str = "globalgamemanagers";

/// The settings files whose content only the Player's settings managers
/// take: none of them changes how code compiles, so a watched session
/// writes them into the next Player instead of asking for a new preparation.
/// `ProjectSettings.asset` (defines, API level, input handling) and
/// `GraphicsSettings.asset` (the render pipeline) stay configuration.
const PLAYER_ONLY_SETTINGS: [&str; 12] = [
    "AudioManager.asset",
    "ClusterInputManager.asset",
    "DynamicsManager.asset",
    "InputManager.asset",
    "MultiplayerManager.asset",
    "NavMeshAreas.asset",
    "Physics2DSettings.asset",
    "PhysicsCoreProjectSettings2D.asset",
    "QualitySettings.asset",
    "TagManager.asset",
    "TimeManager.asset",
    "VFXManager.asset",
];

/// Whether `path` is a settings file of `project` that only the Player's
/// settings managers read.
pub fn player_only_settings(project: &str, path: &str) -> bool {
    crate::sysutil::dir(path) == join(&[project, "ProjectSettings"])
        && PLAYER_ONLY_SETTINGS.contains(&crate::sysutil::base(path).as_str())
}

fn data_directory(player: &str) -> String {
    super::player_data(player)
}

/// The `globalgamemanagers` the project's settings give the Player
/// executable `player`, and what each manager kept of the host. `None` for a
/// Player prepared without the host's file.
pub fn project_managers(
    project: &str,
    player: &str,
) -> Result<Option<(Vec<u8>, Vec<playerbake::ManagerReport>)>> {
    let base = join(&[&data_directory(player), HOST_MANAGERS]);
    if sys::absent(&base) {
        return Ok(None);
    }
    let base = sys::read(&base)?;
    let trees = playerbake::manager_trees()?;
    playerbake::write_project_managers(Path::new(project), &base, &trees)
        .map(Some)
        .context("write the Player's settings managers from ProjectSettings")
}

/// The hash of the managers the Player executable `player` holds, or empty
/// for a Player prepared without the host's file.
pub fn player_managers_hash(player: &str) -> Result<String> {
    let data = data_directory(player);
    if sys::absent(&join(&[&data, HOST_MANAGERS])) {
        return Ok(String::new());
    }
    Ok(sha256(&sys::read(&join(&[&data, MANAGERS]))?))
}

/// Keeps the host's file of a freshly staged host Player and writes the
/// project's settings over it. Returns what each manager kept of the host.
pub fn prepare_managers(project: &str, player: &str) -> Result<Vec<playerbake::ManagerReport>> {
    let data = data_directory(player);
    let (managers, base) = (join(&[&data, MANAGERS]), join(&[&data, HOST_MANAGERS]));
    if sys::absent(&base) {
        sys::write(&base, &sys::read(&managers)?)?;
    }
    let (file, reports) = project_managers(project, player)?.expect("host managers kept above");
    replace_managers(player, &file)?;
    Ok(reports)
}

/// Writes `file` as the managers of the staged Player executable `player`.
/// Staged files may be read-only copies of the host's.
pub fn replace_managers(player: &str, file: &[u8]) -> Result<String> {
    let managers = join(&[&data_directory(player), MANAGERS]);
    if sys::read(&managers).is_ok_and(|current| current == file) {
        return Ok(managers);
    }
    std::fs::remove_file(&managers)
        .or_else(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(error),
        })
        .with_context(|| format!("replace {managers}"))?;
    sys::write(&managers, file)?;
    Ok(managers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_settings_the_player_alone_reads_leave_the_compiled_graph() {
        let project = "/p";
        for (path, only) in [
            ("/p/ProjectSettings/TagManager.asset", true),
            ("/p/ProjectSettings/DynamicsManager.asset", true),
            ("/p/ProjectSettings/QualitySettings.asset", true),
            ("/p/ProjectSettings/ProjectSettings.asset", false),
            ("/p/ProjectSettings/GraphicsSettings.asset", false),
            ("/p/ProjectSettings/EditorBuildSettings.asset", false),
            ("/p/ProjectSettings/Nested/TagManager.asset", false),
            ("/p/Assets/TagManager.asset", false),
            ("/q/ProjectSettings/TagManager.asset", false),
        ] {
            assert_eq!(player_only_settings(project, path), only, "{path}");
        }
    }

    #[test]
    fn a_player_prepared_without_the_host_file_keeps_its_settings() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().to_str().unwrap();
        let data = join(&[root, "Player_Data"]);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(join(&[&data, MANAGERS]), b"host").unwrap();
        let player = join(&[root, "Player.x86_64"]);
        assert!(project_managers(root, &player).unwrap().is_none());
        assert_eq!(player_managers_hash(&player).unwrap(), "");
        std::fs::write(join(&[&data, HOST_MANAGERS]), b"host").unwrap();
        assert_eq!(player_managers_hash(&player).unwrap(), sha256(b"host"));
    }
}
