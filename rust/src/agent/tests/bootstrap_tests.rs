use super::*;
use crate::{compiler::NilMap, sourcegraph::managed_directory, util::agent_immutable_json};
use bootstrap::{
    engine_bootstrap_files, recover_engine_bootstrap, source_engine_profile_cache_path,
    validate_engine_bootstrap,
};

#[test]
fn receipt_captures_facts_without_playable_baseline() {
    let (_guard, project) = temp_dir();
    let (id, scene) = ("bootstrap-test", "Assets/Test.unity");
    let directory = join(&[&project, "Library/UnityMicroEditor/preparations", id]);
    let host = join(&[&directory, "host/Project.x86_64"]);
    let graph = join(&[&directory, "graph.json"]);
    let catalog = join(&[&directory, "native-catalog.json"]);
    let dependency = join(&[&directory, "host/Project_Data/Managed/Game.dll"]);
    let files = [&host, &graph, &catalog, &dependency];
    for file in files {
        write(file, "original");
    }
    let inventory = engine_bootstrap_files(&ctx(), &project, &host, &graph, &catalog, "").unwrap();
    let mut result = Preparation {
        schema_version: 1,
        project: project.clone(),
        scene: scene.into(),
        request_id: id.into(),
        host_player: host.clone(),
        reference_graph: graph.clone(),
        native_catalog: catalog.clone(),
        artifact_receipt: join(&[&directory, "artifacts.json"]),
        backends: NilMap::from([("preparation", "native_engine_bootstrap".to_string())]),
        ..Default::default()
    };
    agent_immutable_json(&result.artifact_receipt, &inventory).unwrap();
    agent_immutable_json(&join(&[&directory, "result.json"]), &result).unwrap();
    let retry = recover_engine_bootstrap(&ctx(), &project, scene, id, &host).unwrap();
    assert!(
        !retry.reference_graph.is_empty(),
        "retry forgot completed bootstrap/default-warming intent"
    );
    assert!(
        recover_engine_bootstrap(&ctx(), &project, scene, id, "/changed/host").is_err(),
        "retry accepted a different host for its completed bootstrap"
    );
    let ordinary =
        recover_engine_bootstrap(&ctx(), &project, scene, "source-only-request", &host).unwrap();
    assert!(
        ordinary.reference_graph.is_empty(),
        "ordinary source-only preparation acquired defaults-warming intent"
    );
    let validate = |result: &Preparation| {
        validate_engine_bootstrap(&ctx(), &project, scene, id, &directory, result)
    };
    validate(&result).unwrap();
    assert!(
        absent(&join(&[&directory, "current.json"])),
        "native bootstrap required or authored a playable baseline"
    );
    for file in files {
        write(file, "changed");
        assert!(
            validate(&result).is_err(),
            "bootstrap reused a changed artifact: {file}"
        );
        write(file, "original");
    }
    let extra = join(&[&crate::sysutil::dir(&dependency), "Unrecorded.dll"]);
    write(&extra, "new");
    assert!(
        validate(&result).is_err(),
        "bootstrap reused a host with an unrecorded assembly"
    );
    fs::remove_file(&extra).unwrap();
    result.baseline = join(&[&directory, "current.json"]);
    assert!(
        validate(&result).is_err(),
        "legacy playable-baseline receipt accepted as native-only bootstrap"
    );
}

#[test]
fn supplied_inputs_are_read_only_outside_target_cache() {
    let (_guard, project) = temp_dir();
    let (_external_guard, external) = temp_dir();
    let graph = join(&[&project, "Library/UnityMicroEditor/bootstrap/graph.json"]);
    let (host, catalog) = (
        join(&[&external, "Project.x86_64"]),
        join(&[&external, "catalog.json"]),
    );
    for file in [&graph, &host, &catalog] {
        write(file, "source");
    }
    let files = engine_bootstrap_files(&ctx(), &project, &host, &graph, &catalog, "")
        .expect("explicit read-only host/catalog were rejected");
    assert_eq!(files.len(), 3);
    assert!(
        engine_bootstrap_files(&ctx(), &project, &host, &catalog, &catalog, "").is_err(),
        "generated graph outside target cache was accepted"
    );
    fs::remove_file(&catalog).unwrap();
    std::os::unix::fs::symlink(&graph, &catalog).unwrap();
    assert!(
        engine_bootstrap_files(&ctx(), &project, &host, &graph, &catalog, "").is_err(),
        "symlink supplied catalog was accepted"
    );
}

#[test]
fn profile_cache_separates_hosts_with_identical_launchers() {
    let (_guard, project) = temp_dir();
    let first = join(&[
        &project,
        "Library/UnityMicroEditor/host-first/Project.x86_64",
    ]);
    let second = join(&[
        &project,
        "Library/UnityMicroEditor/host-second/Project.x86_64",
    ]);
    let proof = |host: &str| {
        join(&[
            &crate::sysutil::dir(&managed_directory(host)),
            "UnityMicroEditorSourceProfile.json",
        ])
    };
    for host in [&first, &second] {
        write_mode(host, "identical Unity launcher", 0o700);
        write(&proof(host), r#"{"source":"first"}"#);
    }
    let key = |host: &str| source_engine_profile_cache_path(&project, host).unwrap();
    let (first_key, second_key) = (key(&first), key(&second));
    assert_ne!(first_key, second_key, "different host paths collided");
    assert_eq!(key(&first), first_key, "unchanged host key was unstable");
    write(&proof(&first), r#"{"source":"new build"}"#);
    assert_ne!(
        key(&first),
        first_key,
        "changed compiled-source proof reused the old engine profile key"
    );
    fs::remove_file(proof(&first)).unwrap();
    assert!(
        source_engine_profile_cache_path(&project, &first).is_err(),
        "host without compiled-source proof acquired an engine profile key"
    );
}
