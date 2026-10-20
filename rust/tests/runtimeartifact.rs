//! The built-in lighting data a scene may name.
use std::{env, fs};
use unity_micro_editor::runtimeartifact as ra;

fn fixture(variable: &str) -> Option<String> {
    let value = env::var(variable).ok().filter(|value| !value.is_empty());
    if value.is_none() {
        eprintln!("skipped: set {variable}");
    }
    value
}

#[test]
fn builtin_lighting_sdk_identity() {
    let Some(path) = fixture("UNITY_BUILTIN_LIGHTING_RESOURCE") else {
        return;
    };
    let file = fs::File::open(path).unwrap();
    let size = file.metadata().unwrap().len() as i64;
    let proof = ra::inspect_builtin_lighting(&file, size).unwrap();
    eprintln!("{}", serde_json::to_string(&proof).unwrap());
    assert_eq!((proof.file_id, proof.class_id), (20201, 1120));
    assert_eq!((proof.lightmap_count, proof.light_probe_count), (0, 0));
}

#[test]
fn builtin_lighting_rejects_malformed_and_unverified_native_files() {
    for data in [
        vec![],
        vec![0u8; 64],
        b"same named object does not prove native lighting content".to_vec(),
    ] {
        assert!(ra::inspect_builtin_lighting(&data, data.len() as i64).is_err());
    }
}
