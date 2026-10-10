//! Fixed regression cases for scalar resolution (YAML 1.1), override formatting
//! (display text of numbers) and path handling.
use serde_json::{Value, json};
use std::{fs, os::unix::ffi::OsStrExt, path::Path};
use unity_micro_editor::{
    graph::{SourceProject, apply_override},
    yaml::{metadata_guid, parse_unity_yaml},
};

fn scalar(text: &str) -> anyhow::Result<Value> {
    let body = format!("--- !u!1 &1\nGameObject:\n  v: {text}\n");
    Ok(parse_unity_yaml(body.as_bytes())?[0].data["v"].clone())
}

#[test]
fn non_finite_and_non_canonical_spellings_are_strings() {
    for text in [
        "inf", "Infinity", "NaN", "+inf", "-nan", "1e400", "1e309", ".iNf", ".nAn", "_1", "++1",
        "0x+1", "_0x1", "_-1", "0x1p-2",
    ] {
        assert_eq!(scalar(text).unwrap(), json!(text), "{text}");
    }
    for text in [".inf", ".NaN", "-.INF", "5_.5", "5._", "1_e1", "<<"] {
        assert!(scalar(text).is_err(), "{text}");
    }
}

#[test]
fn integer_range_and_prefixes() {
    for text in [
        "0xFFFFFFFFFFFFFFFF",
        "0x8000000000000000",
        "01777777777777777777777",
        "18_446_744_073_709_551_615",
    ] {
        assert!(scalar(text).is_err(), "{text}");
    }
    assert_eq!(scalar("18446744073709551615").unwrap(), json!(u64::MAX));
    assert_eq!(scalar("0x10").unwrap(), json!(16));
    assert_eq!(scalar("007").unwrap(), json!(7));
    assert_eq!(scalar("1_000").unwrap(), json!(1000));
    // `08` is not octal: YAML 1.1 resolution reads it as a float.
    assert_eq!(scalar("08").unwrap(), json!(8.0));
    assert_eq!(scalar("! 5").unwrap(), json!(5));
    assert_eq!(scalar("99999999999999999999").unwrap(), json!(1e20));
}

#[test]
fn control_characters_and_unicode_line_breaks_are_refused() {
    for text in ["a\u{0}b", "a\u{7f}b", "a\u{85}b", "a\u{2028}b", "a\u{1}b"] {
        assert!(scalar(text).is_err(), "{text:?}");
    }
    assert_eq!(scalar("a\tb").unwrap(), json!("a\tb"));
}

#[test]
fn document_headers() {
    let parse = |text: &str| parse_unity_yaml(text.as_bytes());
    assert_eq!(
        parse("--- !u!2147483648 &1\nA:\n  a: 1\n").unwrap()[0].class_id,
        2147483648
    );
    assert!(parse("\u{feff}--- !u!1 &1\nA:\n  a: 1\n").is_ok());
    assert!(parse("\u{feff}\u{feff}--- !u!1 &1\nA:\n  a: 1\n").is_err());
    // Unicode digits are not header digits.
    assert!(parse("--- !u!\u{661} &1\nA:\n  a: 1\n").is_err());
    // Only the first YAML document of a segment is read.
    let documents = parse("--- !u!1 &1\nA:\n  a: 1\n---\nB:\n  b: 2\n").unwrap();
    assert_eq!((documents.len(), documents[0].type_name.as_str()), (1, "A"));
    assert_eq!(
        parse("--- !u!1 &1\nA:\n  guid: ABCDÉ\n").unwrap()[0].data["guid"],
        json!("abcdé")
    );
    assert_eq!(
        parse("no header").unwrap_err().to_string(),
        "Unity text asset has no document headers"
    );
    assert_eq!(
        parse_unity_yaml(b"--- !u!1 &1\nA:\n  a: caf\xe9\n")
            .unwrap_err()
            .to_string(),
        "unsupported binary or oversized Unity text asset"
    );
}

#[test]
fn metadata_guid_reads_non_utf8_files() {
    let meta = b"fileFormatVersion: 2\nguid: ABCDEF0123456789abcdef0123456789\nuserData: caf\xe9\n";
    assert_eq!(
        metadata_guid(meta).as_deref(),
        Some("abcdef0123456789abcdef0123456789")
    );
}

#[test]
fn numeric_override_values_keep_number_formatting_on_string_fields() {
    for (raw, expected) in [
        (json!(1.0), "1"),
        (json!(1e3), "1000"),
        (json!(0.00001), "1e-05"),
        (json!(1.5e10), "1.5e+10"),
        (json!(123456789.125), "1.23456789125e+08"),
        (json!(5.0), "5"),
        (json!(7), "7"),
        (json!("text"), "text"),
    ] {
        let mut fields = json!({"m_Name":"old"}).as_object().unwrap().clone();
        apply_override(&mut fields, "m_Name", &raw, &json!({})).unwrap();
        assert_eq!(fields["m_Name"], json!(expected), "{raw}");
    }
}

#[test]
fn override_edge_cases() {
    let base = json!({"list":[{"x":1}],"empty":null,"ratio":1.5,"count":2,"m_LocalPosition":null});
    let apply = |path: &str, raw: Value| {
        let mut fields = base.as_object().unwrap().clone();
        apply_override(&mut fields, path, &raw, &json!({})).map(|()| fields)
    };
    assert_eq!(
        apply("list.Array.size", json!(2.0)).unwrap()["list"],
        json!([{"x":1},{"x":1}])
    );
    assert!(apply("list.Array.size", json!("-1")).is_err());
    // An integer-looking value beyond i64 is an error, not a float.
    assert!(apply("empty", json!("99999999999999999999")).is_err());
    assert_eq!(apply("empty", json!("12")).unwrap()["empty"], json!(12));
    assert_eq!(apply("empty", json!("1.5")).unwrap()["empty"], json!(1.5));
    assert_eq!(apply("empty", json!("x")).unwrap()["empty"], json!("x"));
    assert_eq!(
        apply("ratio", json!("0x1p-2")).unwrap()["ratio"],
        json!(0.25)
    );
    assert_eq!(
        apply("ratio", json!("1_0.5")).unwrap()["ratio"],
        json!(10.5)
    );
    assert_eq!(apply("count", json!("1e2")).unwrap()["count"], json!(100.0));
    assert!(apply("count", json!("x")).is_err());
    // A null m_LocalPosition does not make m_RootOrder creatable.
    assert!(apply("m_RootOrder", json!("1")).is_err());
    let failed = apply("missing", json!("1"));
    assert_eq!(
        failed.unwrap_err().to_string(),
        "field missing does not exist"
    );
}

fn write(project: &Path, name: &str, guid: &str, content: &str) {
    let path = project.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    fs::write(
        project.join(format!("{name}.meta")),
        format!("guid: {guid}\n"),
    )
    .unwrap();
}
const BASE: &str = "--- !u!1 &10\nGameObject:\n  m_Component:\n  - component: {fileID: 11}\n  m_Name: Base\n--- !u!4 &11\nTransform:\n  m_GameObject: {fileID: 10}\n  m_Father: {fileID: 0}\n  m_Children: []\n";

#[test]
fn prefab_sources_with_any_extension_case_expand() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    let guid = "22222222222222222222222222222222";
    write(project, "Assets/Base.Prefab", guid, BASE);
    write(
        project,
        "Assets/Scene.unity",
        "11111111111111111111111111111111",
        &format!(
            "--- !u!1001 &100\nPrefabInstance:\n  m_SourcePrefab: {{fileID: 100100000, guid: {guid}, type: 3}}\n  m_Modification:\n    m_Modifications:\n    - target: {{fileID: 10, guid: {guid}, type: 3}}\n      propertyPath: m_Name\n      value: 1e3\n      objectReference: {{fileID: 0}}\n"
        ),
    );
    let graph = SourceProject::new(project)
        .unwrap()
        .authored_graph("Assets/Scene.unity")
        .unwrap();
    assert_eq!(graph.objects.len(), 2);
    // The scene override (a YAML float) lands on the string field as its
    // display text (`1000`, not `1000.0`).
    assert_eq!(graph.objects[0].document.data["m_Name"], json!("1000"));
}

#[test]
fn inserted_objects_convert_indexes() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    let guid = "22222222222222222222222222222222";
    write(
        project,
        "Assets/Base.prefab",
        guid,
        &BASE.replace("m_Children: []", "m_Children:\n  - {fileID: 99}"),
    );
    let scene = |index: &str| {
        format!(
            "--- !u!1001 &100\nPrefabInstance:\n  m_SourcePrefab: {{fileID: 100100000, guid: {guid}, type: 3}}\n  m_Modification:\n    m_AddedGameObjects:\n    - targetCorrespondingSourceObject: {{fileID: 11, guid: {guid}, type: 3}}\n      insertIndex: {index}\n      addedObject: {{fileID: 21}}\n--- !u!1 &20\nGameObject:\n  m_Component:\n  - component: {{fileID: 21}}\n--- !u!4 &21\nTransform:\n  m_GameObject: {{fileID: 20}}\n  m_Father: {{fileID: 0}}\n  m_Children: []\n"
        )
    };
    for (index, expected) in [("0.0", 0), ("text", 0), ("-1", 1), ("1", 1)] {
        write(
            project,
            "Assets/Scene.unity",
            "11111111111111111111111111111111",
            &scene(index),
        );
        let graph = SourceProject::new(project)
            .unwrap()
            .authored_graph("Assets/Scene.unity")
            .unwrap();
        let parent = graph
            .objects
            .iter()
            .find(|o| o.document.file_id == "100:11")
            .unwrap();
        let children = parent.document.data["m_Children"].as_array().unwrap();
        assert_eq!(
            children[expected]["fileID"],
            json!("21"),
            "insertIndex {index}"
        );
    }
}

#[test]
fn non_utf8_names_and_metadata_do_not_break_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    write(
        project,
        "Assets/Scene.unity",
        "11111111111111111111111111111111",
        BASE,
    );
    fs::write(
        project.join("Assets/Scene.unity.meta"),
        b"guid: 11111111111111111111111111111111\nuserData: caf\xe9\n",
    )
    .unwrap();
    fs::write(
        project
            .join("Assets")
            .join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt")),
        "x",
    )
    .unwrap();
    let graph = SourceProject::new(project)
        .unwrap()
        .authored_graph("Assets/Scene.unity")
        .unwrap();
    assert_eq!(graph.guid, "11111111111111111111111111111111");
}
