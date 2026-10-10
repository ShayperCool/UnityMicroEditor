use serde_json::{Value, json};
use std::fs;
use unity_micro_editor::graph::{SourceProject, apply_override};

#[test]
fn overrides_preserve_types_and_resize_arrays() {
    let mut fields = json!({"m_Name":"Start","count":2,"large":12934340311651418136u64,"position":{"x":1.5},"list":[{"x":1},{"x":2}],"target":{"fileID":"0"}}).as_object().unwrap().clone();
    apply_override(
        &mut fields,
        "m_Name",
        &json!("After"),
        &json!({"fileID":"0"}),
    )
    .unwrap();
    apply_override(&mut fields, "position.x", &json!("2"), &json!({})).unwrap();
    apply_override(&mut fields, "list.Array.size", &json!("3"), &json!({})).unwrap();
    apply_override(&mut fields, "list.Array.data[2].x", &json!("7"), &json!({})).unwrap();
    apply_override(
        &mut fields,
        "target",
        &Value::Null,
        &json!({"fileID":"9223372036854775807"}),
    )
    .unwrap();
    assert_eq!(fields["m_Name"], "After");
    assert_eq!(fields["position"]["x"], json!(2.0));
    assert_eq!(fields["list"][2]["x"], 7);
    assert_eq!(fields["target"]["fileID"], "9223372036854775807");
    assert!(apply_override(&mut fields, "list.Array.size", &json!("100001"), &json!({})).is_err());
    assert!(apply_override(&mut fields, "missing", &json!("0"), &json!({})).is_err());
    assert_eq!(fields["large"], json!(12934340311651418136u64));
}
fn write_asset(project: &std::path::Path, name: &str, guid: &str, content: &str) {
    fs::create_dir_all(project.join("Assets")).unwrap();
    fs::write(project.join(name), content).unwrap();
    fs::write(
        project.join(format!("{name}.meta")),
        format!("guid: {guid}\n"),
    )
    .unwrap();
}
#[test]
fn authored_graph_reuses_cache_and_refreshes_new_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    write_asset(
        project,
        "Assets/Scene.unity",
        "11111111111111111111111111111111",
        "--- !u!1 &1\nGameObject: {m_Name: First}\n",
    );
    let mut source = SourceProject::new(project).unwrap();
    let graph = source.authored_graph("Assets/Scene.unity").unwrap();
    assert_eq!(graph.objects[0].document.data["m_Name"], "First");
    source.authored_graph("Assets/Scene.unity").unwrap();
    assert_eq!(source.cache.stats.yaml_parses, 0);
    assert_eq!(source.cache.stats.file_reads, 0);
    write_asset(
        project,
        "Assets/Scene.unity",
        "11111111111111111111111111111111",
        "--- !u!1 &1\nGameObject: {m_Name: Updated}\n",
    );
    source.invalidate(&["Assets/Scene.unity".into()]);
    assert_eq!(
        source.authored_graph("Assets/Scene.unity").unwrap().objects[0]
            .document
            .data["m_Name"],
        "Updated"
    );
    write_asset(
        project,
        "Assets/Other.prefab",
        "11111111111111111111111111111111",
        "--- !u!1 &1\nGameObject: {}\n",
    );
    source.invalidate(&["Assets/Other.prefab.meta".into()]);
    assert!(source.authored_graph("Assets/Scene.unity").is_err());
}
#[test]
fn recursive_prefabs_and_unresolved_sources_are_errors() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    let guid = "11111111111111111111111111111111";
    write_asset(
        project,
        "Assets/Loop.prefab",
        guid,
        &format!(
            "--- !u!1001 &1\nPrefabInstance:\n  m_SourcePrefab: {{fileID: 100100000, guid: {guid}}}\n"
        ),
    );
    assert!(
        SourceProject::new(project)
            .unwrap()
            .authored_graph("Assets/Loop.prefab")
            .unwrap_err()
            .to_string()
            .contains("recursive")
    );
    write_asset(
        project,
        "Assets/Loop.prefab",
        guid,
        "--- !u!1001 &1\nPrefabInstance: {}\n",
    );
    assert!(
        SourceProject::new(project)
            .unwrap()
            .authored_graph("Assets/Loop.prefab")
            .is_err()
    );
}
