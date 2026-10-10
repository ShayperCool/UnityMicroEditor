//! Ownership test for the project's ignored cache directories.
use super::lexpath;

/// Accepts a child of an ignored project cache directory. Callers must
/// separately reject symlinks and verify the canonical destination. Unity
/// clears Temp when reopening a project; Library/UnityMicroEditor persists.
pub fn is_owned_cache_path(project: &str, path: &str) -> bool {
    if !lexpath::is_abs(project) || !lexpath::is_abs(path) || lexpath::clean(path) != path {
        return false;
    }
    [
        lexpath::join(&[project, "Temp"]),
        lexpath::join(&[project, "Library", "UnityMicroEditor"]),
    ]
    .iter()
    .any(|root| {
        lexpath::rel(root, path).is_ok_and(|relative| {
            relative != "." && !lexpath::is_abs(&relative) && lexpath::inside(&relative)
        })
    })
}
