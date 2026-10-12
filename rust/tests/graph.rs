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

const GUID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GUID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const GUID_HUB: &str = "cccccccccccccccccccccccccccccccc";
fn leaf_prefab(name: &str, object: i64) -> String {
    let transform = object + 1;
    format!(
        "--- !u!1 &{object}\nGameObject:\n  m_Name: {name}\n  m_Component:\n  - component: {{fileID: {transform}}}\n--- !u!4 &{transform}\nTransform:\n  m_GameObject: {{fileID: {object}}}\n  m_Father: {{fileID: 0}}\n"
    )
}
fn nested_instance(id: i64, guid: &str, parent: i64) -> String {
    format!(
        "--- !u!1001 &{id}\nPrefabInstance:\n  m_Modification:\n    m_TransformParent: {{fileID: {parent}}}\n    m_Modifications: []\n  m_SourcePrefab: {{fileID: 100100000, guid: {guid}, type: 3}}\n"
    )
}
const HUB_ROOT: &str = "--- !u!1 &1\nGameObject:\n  m_Name: Hub\n  m_Component:\n  - component: {fileID: 2}\n--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n  m_Father: {fileID: 0}\n";

/// The shape of a hand-written prefab: the stripped GameObject of one instance
/// (200, source object 10) has the ID 110, which is also what instance 100 XOR
/// its own source object 10 gives. The Editor's import keeps 110 for the
/// stripped object and gives the other one an ID outside this scheme.
#[test]
fn stripped_document_keeps_its_id_over_a_derived_inherited_identity() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    write_asset(project, "Assets/A.prefab", GUID_A, &leaf_prefab("A", 10));
    write_asset(project, "Assets/B.prefab", GUID_B, &leaf_prefab("B", 10));
    let hub = format!(
        "{HUB_ROOT}{}{}--- !u!1 &110 stripped\nGameObject:\n  m_CorrespondingSourceObject: {{fileID: 10, guid: {GUID_B}, type: 3}}\n  m_PrefabInstance: {{fileID: 200}}\n--- !u!65 &3\nBoxCollider:\n  m_GameObject: {{fileID: 110}}\n",
        nested_instance(100, GUID_A, 2),
        nested_instance(200, GUID_B, 2),
    );
    write_asset(project, "Assets/Hub.prefab", GUID_HUB, &hub);
    write_asset(
        project,
        "Assets/Outer.prefab",
        "dddddddddddddddddddddddddddddddd",
        &format!("{HUB_ROOT}{}", nested_instance(1000, GUID_HUB, 2)),
    );
    let mut source = SourceProject::new(project).unwrap();
    let graph = source.authored_graph("Assets/Hub.prefab").unwrap();
    assert_eq!(graph.local_ids["110"], "200:10");
    assert_eq!(graph.local_ids["111"], "100:11");
    assert_eq!(graph.local_ids["195"], "200:11");
    // 200 ^ 10: the stripped document names that object instead.
    assert!(!graph.local_ids.contains_key("194"));
    assert!(!graph.local_ids.values().any(|id| id == "100:10"));
    let collider = graph
        .objects
        .iter()
        .find(|object| object.document.file_id == "3")
        .unwrap();
    assert_eq!(collider.document.data["m_GameObject"]["fileID"], "200:10");
    let identities = |id: &str| -> Vec<String> {
        let object = graph
            .objects
            .iter()
            .find(|object| object.document.file_id == id)
            .unwrap();
        object
            .identities
            .iter()
            .filter(|identity| identity.guid == GUID_HUB)
            .map(|identity| identity.file_id.clone())
            .collect()
    };
    assert_eq!(identities("200:10"), ["110"]);
    assert!(identities("100:10").is_empty());

    // One level up an ID is the instance XOR the ID in the nested prefab: the
    // stripped 110, not 200 ^ 10.
    let outer = source.authored_graph("Assets/Outer.prefab").unwrap();
    assert_eq!(outer.local_ids["902"], "1000:200:10");
    assert_eq!(outer.local_ids["811"], "1000:200:11");
    assert!(!outer.local_ids.contains_key("818"));
    assert!(!outer.local_ids.values().any(|id| id == "1000:100:10"));
}

#[test]
fn two_derived_inherited_identities_that_collide_are_refused_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    write_asset(project, "Assets/A.prefab", GUID_A, &leaf_prefab("A", 10));
    write_asset(project, "Assets/B.prefab", GUID_B, &leaf_prefab("B", 11));
    let hub = format!(
        "{HUB_ROOT}{}{}",
        nested_instance(100, GUID_A, 2),
        nested_instance(101, GUID_B, 2),
    );
    write_asset(project, "Assets/Hub.prefab", GUID_HUB, &hub);
    let error = SourceProject::new(project)
        .unwrap()
        .authored_graph("Assets/Hub.prefab")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("inherited prefab identity collision 110 in Assets/Hub.prefab")
            && error.contains("100:10 (Assets/A.prefab)")
            && error.contains("101:11 (Assets/B.prefab)"),
        "{error}"
    );
}
