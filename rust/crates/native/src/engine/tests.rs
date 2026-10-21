use super::*;
use std::os::unix::fs::PermissionsExt;

fn script(directory: &tempfile::TempDir, body: &str) -> String {
    let path = directory.path().join("Unity");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn project(version: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("ProjectSettings")).unwrap();
    std::fs::write(
        directory.path().join("ProjectSettings/ProjectVersion.txt"),
        format!(
            "m_EditorVersion: {version}\r\nm_EditorVersionWithRevision: {version} (0123456789ab)\r\n"
        ),
    )
    .unwrap();
    directory
}

#[test]
fn a_release_selects_its_own_engine_and_others_are_refused() {
    let engine = supported("6000.6.3f1").unwrap();
    assert_eq!(engine.version, crate::ENGINE_VERSION);
    assert!(std::ptr::eq(engine, verified()));
    assert!(std::ptr::eq(
        supported("6000.0.71f1").unwrap(),
        &unity6000_0::ENGINE
    ));
    assert_eq!(supported_versions(), "6000.6.3f1, 6000.0.71f1");
    let error = supported("6000.0.70f1").unwrap_err().to_string();
    assert_eq!(
        error,
        "Unity 6000.0.70f1 is not supported (supported: 6000.6.3f1, 6000.0.71f1)"
    );
}

#[test]
fn the_project_version_file_names_the_release() {
    let directory = project("6000.6.3f1");
    let path = directory.path().to_string_lossy();
    assert_eq!(project_version(&path).unwrap(), "6000.6.3f1");
    assert_eq!(of_project(&path).unwrap().version, "6000.6.3f1");
    assert!(project_version_of(b"m_EditorVersionWithRevision: x\n").is_err());
    let other = project("2022.3.10f1");
    assert!(of_project(&other.path().to_string_lossy()).is_err());
}

#[test]
fn a_worker_runs_only_on_an_installation_of_the_projects_release() {
    let directory = project("6000.6.3f1");
    let path = directory.path().to_string_lossy();
    let installations = tempfile::tempdir().unwrap();
    let editor = script(&installations, "[ \"$1\" = -version ] && echo 6000.6.3f1");
    assert_eq!(for_worker(&path, &editor).unwrap().version, "6000.6.3f1");

    let other = tempfile::tempdir().unwrap();
    let editor = script(&other, "echo 6000.0.71f1");
    let error = for_worker(&path, &editor).unwrap_err().to_string();
    assert!(error.contains("6000.0.71f1"), "{error}");

    let silent = tempfile::tempdir().unwrap();
    assert!(editor_version(&script(&silent, "exit 0")).is_err());
}

#[test]
fn the_worker_log_names_its_release() {
    let worker = verified().worker;
    let log = b"[Licensing]\nUnity Editor version:    6000.6.3f1 (45d8eee7de74)\n";
    assert_eq!(worker.logged_version(log).as_deref(), Some("6000.6.3f1"));
    assert_eq!(worker.logged_version(b"nothing\n"), None);
}

#[test]
fn worker_messages_are_told_apart_by_the_release() {
    let worker = verified().worker;
    assert_eq!(
        worker.message(&[23]).unwrap(),
        Message::Reply(Reply::Prepared)
    );
    assert_eq!(worker.message(&[23, 0]).unwrap(), Message::Other(23));
    let mut response = vec![0; 128];
    response[0] = 20;
    assert_eq!(
        worker.message(&response).unwrap(),
        Message::Reply(Reply::Imported)
    );
    let mut log = vec![0; 16];
    log[0] = 2;
    log[8] = 8;
    log.extend_from_slice(b"hello\0");
    assert_eq!(worker.message(&log).unwrap(), Message::Log("hello".into()));
    assert_eq!(worker.message(&[5]).unwrap(), Message::Other(5));
}
