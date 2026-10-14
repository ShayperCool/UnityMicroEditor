//! Tests for source edits, publication, native component defaults and prefab authoring.
use super::{
    component_defaults::NativeDefaultsTemplate,
    hierarchy::{GameObject, hierarchy},
    prefab_authoring::PrefabPlanner,
    prefab_linking::clone_prefab_document,
    prefab_overrides::authored_property_value,
    store::*,
    yaml_edit::{TextAsset, ids_from_list, reference, reference_list},
    *,
};
use crate::sha256;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
};
use tempfile::TempDir;

const SCENE: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  serializedVersion: 6
  m_Component:
  - component: {fileID: 101}
  m_Layer: 0 # keep my comment
  m_Name: Original
  m_TagString: Untagged
  m_IsActive: 1
  unknownFuture: {value: 00123, label: 'a, b', target: {fileID: 0}}
--- !u!4 &101
Transform:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  serializedVersion: 2
  m_GameObject: {fileID: 100}
  m_LocalPosition: {x: 0, y: 1, z: 0} # position comment
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
  # parent comment
  m_Father: {fileID: 0}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_ObjectHideFlags: 0
  m_Roots:
  - {fileID: 101}
";
const PATH: &str = "Assets/Scenes/Main.unity";
const UNKNOWN: &str = "unknownFuture: {value: 00123, label: 'a, b', target: {fileID: 0}}";

fn write(root: &Path, path: &str, text: &str) {
    fs::write(root.join(path), text).unwrap();
}
fn fixture(scene: &str) -> (TempDir, Store, Source) {
    let temp = tempfile::tempdir().unwrap();
    for dir in ["Assets/Scenes", "Assets/Scripts", "ProjectSettings"] {
        fs::create_dir_all(temp.path().join(dir)).unwrap();
    }
    write(
        temp.path(),
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6000.6.3f1\n",
    );
    write(temp.path(), PATH, scene);
    write(
        temp.path(),
        &format!("{PATH}.meta"),
        "fileFormatVersion: 2\nguid: 12345678901234567890123456789012\nDefaultImporter:\n  externalObjects: {}\n",
    );
    let store = Store::new(temp.path()).unwrap();
    let source = store.read(PATH).unwrap();
    (temp, store, source)
}
fn code<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
    match result {
        Err(error) if error.code == expected => {}
        other => panic!("got {other:?}, want {expected}"),
    }
}
fn request(source: &Source, id: &str, operations: Value) -> EditRequest {
    serde_json::from_value(json!({"path": source.path, "expectedRevision": source.revision, "operationId": id, "operations": operations})).unwrap()
}
fn edit(store: &Store, source: &Source, id: &str, operations: Value) -> Result<EditResult> {
    store.edit(&request(source, id, operations))
}
fn resolved(store: &Store, path: &str) -> HierarchyPage {
    store
        .hierarchy(&HierarchyRequest {
            path: path.into(),
            include_fields: Some(true),
            ..HierarchyRequest::default()
        })
        .unwrap()
}
fn raw(store: &Store) -> HierarchyPage {
    store.locked(|| store.raw_hierarchy(PATH, "")).unwrap()
}
fn script(path: &str, id: &str, content: &str, guid: &str) -> ScriptRequest {
    ScriptRequest {
        path: path.into(),
        expected_revision: "absent".into(),
        operation_id: id.into(),
        content: content.into(),
        guid: guid.into(),
    }
}
fn prefab_fixture() -> String {
    SCENE.split("--- !u!1660057539").next().unwrap().to_string()
}

// ---- source edits ----

#[test]
fn leaf_edits_preserve_every_other_byte_and_metadata() {
    for newline in ["\n", "\r\n"] {
        let original = SCENE.replace('\n', newline);
        let (_temp, store, source) = fixture(&original);
        let result = edit(&store, &source, "leaf-edit", json!([
            {"action": "set_property", "fileId": "100", "property": "m_Name", "value": "Changed, with Unicode Я"},
            {"action": "set_property", "fileId": "101", "property": "m_LocalPosition.y", "value": 2.5}]))
        .unwrap();
        let after = store.read(PATH).unwrap();
        let want = original
            .replacen("m_Name: Original", "m_Name: \"Changed, with Unicode Я\"", 1)
            .replacen("{x: 0, y: 1, z: 0}", "{x: 0, y: 2.5, z: 0}", 1);
        assert_eq!(after.content, want);
        assert!(after.metadata == source.metadata && after.guid == source.guid);
        assert!(result.mutation.revision != source.revision && result.mutation.changed);
    }
}
#[test]
fn graph_transaction_and_retry_after_restart() {
    let (temp, store, source) = fixture(SCENE);
    let operations = json!([
        {"action": "create_gameobject", "key": "child", "name": "Child", "parentId": "100"},
        {"action": "set_property", "fileId": "$child.transform", "property": "m_LocalPosition.x", "value": 3},
        {"action": "create_gameobject", "key": "root", "name": "Another root"}]);
    let result = edit(&store, &source, "graph-create", operations.clone())
        .unwrap()
        .mutation;
    let restarted = Store::new(temp.path()).unwrap();
    let replay = edit(&restarted, &source, "graph-create", operations)
        .unwrap()
        .mutation;
    assert!(replay.replayed && replay.revision == result.revision);
    let view = raw(&store);
    assert!(view.objects.len() == 3 && view.roots.len() == 2);
    let (child, root) = (&result.created_ids["child"], &result.created_ids["root"]);
    assert!(
        view.objects
            .iter()
            .all(|object| &object.file_id != child || object.parent_id == "100")
    );
    let latest = store.read(PATH).unwrap();
    edit(
        &store,
        &latest,
        "reparent",
        json!([{"action": "reparent", "fileId": child, "parentId": root}]),
    )
    .unwrap();
    let latest = store.read(PATH).unwrap();
    code(
        edit(
            &store,
            &latest,
            "cycle",
            json!([{"action": "reparent", "fileId": root, "parentId": child}]),
        ),
        "transform_cycle",
    );
    code(
        edit(
            &store,
            &latest,
            "delete-parent",
            json!([{"action": "delete_gameobject", "fileId": root}]),
        ),
        "has_children",
    );
    edit(
        &store,
        &latest,
        "delete-tree",
        json!([{"action": "delete_gameobject", "fileId": root, "recursive": true}]),
    )
    .unwrap();
    let view = raw(&store);
    assert!(view.objects.len() == 1 && view.roots.len() == 1);
}
#[test]
fn preview_never_writes_and_stale_or_reused_ids_fail() {
    let (_temp, store, source) = fixture(SCENE);
    let mut preview = request(
        &source,
        "preview",
        json!([{"action": "create_gameobject", "name": "Preview"}]),
    );
    preview.preview = true;
    let result = store.edit(&preview).unwrap();
    assert!(result.preview && result.mutation.changed && result.content != source.content);
    assert_eq!(store.read(PATH).unwrap().revision, source.revision);
    preview.preview = false;
    store.edit(&preview).unwrap();
    code(
        edit(
            &store,
            &source,
            "stale",
            json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "stale"}]),
        ),
        "revision_conflict",
    );
    preview.operations[0].name = "Different".into();
    code(store.edit(&preview), "idempotency_conflict");
}
#[test]
fn concurrent_writers_and_meta_revision_conflict() {
    let (temp, store, source) = fixture(SCENE);
    let another = Store::new(temp.path()).unwrap();
    let results: Vec<_> = std::thread::scope(|scope| {
        [("a", &store), ("b", &another)]
            .map(|(id, store)| {
                let source = &source;
                scope.spawn(move || {
                    edit(store, source, id, json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "writer"}]))
                })
            })
            .map(|handle| handle.join().unwrap())
            .into_iter()
            .collect()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(results.iter().any(|result| {
        result
            .as_ref()
            .is_err_and(|error| error.code == "revision_conflict")
    }));
    let latest = store.read(PATH).unwrap();
    write(
        temp.path(),
        &format!("{PATH}.meta"),
        &format!("{}# external metadata change\n", latest.metadata),
    );
    code(
        edit(
            &store,
            &latest,
            "stale-meta",
            json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "stale"}]),
        ),
        "revision_conflict",
    );
}
#[test]
fn protected_fields_prefab_instances_and_dangling_references() {
    let (_temp, store, source) = fixture(SCENE);
    code(
        edit(
            &store,
            &source,
            "identity",
            json!([{"action": "set_property", "fileId": "101", "property": "m_GameObject.fileID", "value": "900"}]),
        ),
        "protected_property",
    );
    code(
        edit(
            &store,
            &source,
            "dangling",
            json!([{"action": "set_property", "fileId": "100", "property": "unknownFuture", "value": {"target": {"fileID": "999"}}}]),
        ),
        "dangling_reference",
    );
    let instance = SCENE.replacen(
        "m_PrefabInstance: {fileID: 0}",
        "m_PrefabInstance: {fileID: 555}",
        1,
    ) + "--- !u!1001 &555\nPrefabInstance:\n  m_SourcePrefab: {fileID: 100100000, guid: 99999999999999999999999999999999, type: 3}\n";
    let (_temp, store, source) = fixture(&instance);
    code(
        edit(
            &store,
            &source,
            "prefab-write",
            json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "forbidden"}]),
        ),
        "unsupported_prefab_write",
    );
}
#[test]
fn script_creation_attachment_editing_and_guid_preservation() {
    let (_temp, store, source) = fixture(SCENE);
    let mut create = script(
        "Assets/Scripts/Example.cs",
        "create-script",
        "using UnityEngine; public class Example : MonoBehaviour { public int amount = 2; }\n",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let created = store.create_script(&create).unwrap();
    let authored = store.read_script(&create.path).unwrap();
    assert!(authored.guid == create.guid && authored.revision == created.revision);
    let result = edit(&store, &source, "attach", json!([{"action": "add_component", "fileId": "100", "key": "example", "scriptPath": authored.path, "fields": {"amount": 7}}])).unwrap();
    assert_eq!(raw(&store).objects[0].components.len(), 2);
    store
        .edit_script(&ScriptRequest {
            path: authored.path.clone(),
            expected_revision: authored.revision.clone(),
            operation_id: "edit-script".into(),
            content: authored.content.replacen("= 2", "= 3", 1),
            guid: String::new(),
        })
        .unwrap();
    assert_eq!(
        store.read_script(&authored.path).unwrap().metadata,
        authored.metadata
    );
    let latest = store.read(PATH).unwrap();
    edit(
        &store,
        &latest,
        "detach",
        json!([{"action": "remove_component", "fileId": result.mutation.created_ids["example"]}]),
    )
    .unwrap();
    create.path = "Assets/Scripts/Other.cs".into();
    create.operation_id = "duplicate-script".into();
    code(store.create_script(&create), "duplicate_guid");
}
/// A field added to a script after the scene was saved (by a hot reload
/// patch, for example) has no key in the component yet: set_property writes
/// it when the script declares it as a field Unity serializes, and refuses
/// a name the script does not declare so (stage 5 mcp5-2).
#[test]
fn set_property_adds_a_field_the_script_declares() {
    let (_temp, store, source) = fixture(SCENE);
    let create = script(
        "Assets/Scripts/Lamp.cs",
        "create-lamp",
        "using UnityEngine;\npublic class Lamp : MonoBehaviour\n{\n    public float dim = 0.5f;\n}\n",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    store.create_script(&create).unwrap();
    let attached = edit(&store, &source, "attach-lamp", json!([{"action": "add_component", "fileId": "100", "key": "lamp", "scriptPath": "Assets/Scripts/Lamp.cs", "fields": {"dim": 0.5}}])).unwrap();
    let lamp = attached.mutation.created_ids["lamp"].clone();
    // The script gains fields; the scene does not have them.
    let authored = store.read_script("Assets/Scripts/Lamp.cs").unwrap();
    store
        .edit_script(&ScriptRequest {
            path: authored.path.clone(),
            expected_revision: authored.revision.clone(),
            operation_id: "grow-lamp".into(),
            content: authored.content.replacen(
                "    public float dim = 0.5f;\n",
                "    public float dim = 0.5f;\n    [SerializeField] private float glowCut = 0.9f;\n    public static int count;\n    private int hidden;\n",
                1,
            ),
            guid: String::new(),
        })
        .unwrap();
    let latest = store.read(PATH).unwrap();
    edit(
        &store,
        &latest,
        "set-glow",
        json!([{"action": "set_property", "fileId": lamp, "property": "glowCut", "value": 0.25}]),
    )
    .unwrap();
    let text = fs::read_to_string(_temp.path().join(PATH)).unwrap();
    assert!(text.contains("\n  glowCut: 0.25\n"), "{text}");
    // Set again: now it is an ordinary key.
    let latest = store.read(PATH).unwrap();
    edit(
        &store,
        &latest,
        "set-glow-again",
        json!([{"action": "set_property", "fileId": lamp, "property": "glowCut", "value": 0.5}]),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(_temp.path().join(PATH))
            .unwrap()
            .matches("glowCut:")
            .count(),
        1
    );
    // Not serialized, or not declared at all: the path stays absent.
    for property in ["count", "hidden", "glowCutt"] {
        let latest = store.read(PATH).unwrap();
        code(
            edit(
                &store,
                &latest,
                &format!("set-{property}"),
                json!([{"action": "set_property", "fileId": lamp, "property": property, "value": 1}]),
            ),
            "unsupported_property",
        );
    }
}
#[test]
fn a_declared_field_is_found_in_the_source() {
    use super::edit::declares_serialized_field as declares;
    let source = "public class A : MonoBehaviour {\n  public float speed = 1f;\n  [SerializeField] private int armor;\n  [SerializeField, Range(0, 1)] float tint = 0.5f;\n  public static int count;\n  public const int Max = 3;\n  public readonly int fixedOne = 1;\n  private int hidden;\n  public void Jump() { }\n  public int Health { get; set; }\n  void Update() { int local = 2; return; }\n}\n";
    for name in ["speed", "armor", "tint"] {
        assert!(declares(source, name), "{name}");
    }
    for name in [
        "count", "Max", "fixedOne", "hidden", "Jump", "Health", "local", "missing",
    ] {
        assert!(!declares(source, name), "{name}");
    }
}
/// The declarations Unity serializes as fields of the script's class, and
/// those it does not, read from the class body: attributes on lines of
/// their own, `[NonSerialized]`, properties, delegates, comments, several
/// declarators, another class of the same file (session6-7).
#[test]
fn the_fields_unity_serializes_are_read_from_the_class_body() {
    use super::edit::declares_serialized_field as declares;
    let source = r#"using System;
using UnityEngine;
public class Hero : MonoBehaviour
{
    [NonSerialized] public int cachedScore;
    [System.NonSerialized] public float lastHit;
    public int Speed => speed;
    [SerializeField]
    private int speed = 3;
    [SerializeField]
    [Range(0, 10)] private float armor = 1f;
    public int ammo, clips;
    public Action onDeath;
    public Stats stats;
    public System.Collections.Generic.List<int> waves = new System.Collections.Generic.List<int> { 1, 2 };
    /*
    public int oldField;
    */
    // public int commented;
    public string motto = "public int quoted;";
    [field: SerializeField] public int Gold { get; private set; }
    void Update() { int local = 2; }
}
[Serializable]
public class Stats
{
    public int hp;
}
"#;
    let mut wrong = Vec::new();
    for name in ["cachedScore", "lastHit", "Speed", "onDeath", "hp", "oldField", "commented", "quoted", "Gold", "local"] {
        if declares(source, name) {
            wrong.push(format!("{name}: taken, Unity does not serialize it as a field of Hero"));
        }
    }
    for name in ["speed", "armor", "ammo", "clips", "stats", "waves", "motto"] {
        if !declares(source, name) {
            wrong.push(format!("{name}: refused, Unity serializes it"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// A field of a base class in another file is a field of the component;
/// a value of the wrong type for a new key is refused, as on a key the data
/// holds, and one of the right type is written (session6-6, session6-7).
#[test]
fn set_property_types_a_new_key_and_finds_inherited_fields() {
    let (temp, store, source) = fixture(SCENE);
    store
        .create_script(&script(
            "Assets/Scripts/Unit.cs",
            "create-unit",
            "using UnityEngine;\npublic class Unit : MonoBehaviour\n{\n    public int boost = 1;\n}\n",
            "cccccccccccccccccccccccccccccccc",
        ))
        .unwrap();
    store
        .create_script(&script(
            "Assets/Scripts/Hero.cs",
            "create-hero",
            "using System.Collections.Generic;\nusing UnityEngine;\npublic class Hero : Unit\n{\n    public float dim = 0.5f;\n    public int count;\n    public bool alive;\n    public string title;\n    public Vector3 spawn;\n    public Color tint;\n    public Transform target;\n    public int[] waves;\n    public List<string> tags;\n}\n",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ))
        .unwrap();
    let attached = edit(&store, &source, "attach-hero", json!([{"action": "add_component", "fileId": "100", "key": "hero", "scriptPath": "Assets/Scripts/Hero.cs", "fields": {"dim": 0.5}}])).unwrap();
    let hero = attached.mutation.created_ids["hero"].clone();
    let set = |property: &str, value: Value| {
        let latest = store.read(PATH).unwrap();
        edit(
            &store,
            &latest,
            &format!("set-{property}-{}", value.to_string().len()),
            json!([{"action": "set_property", "fileId": hero, "property": property, "value": value}]),
        )
    };
    for (property, value) in [
        ("count", json!("12")),
        ("alive", json!("true")),
        ("title", json!(42)),
        ("spawn", json!([1, 2, 3])),
        ("tint", json!({"red": 1})),
        ("waves", json!("1,2,3")),
        ("tags", json!([1, 2])),
        ("target", json!("101")),
    ] {
        code(set(property, value.clone()), "invalid_property_value");
    }
    for (property, value) in [
        ("boost", json!(3)),
        ("count", json!(12)),
        ("alive", json!(true)),
        ("title", json!("007")),
        ("spawn", json!({"x": 1, "y": 2, "z": 3})),
        ("tint", json!({"r": 1, "g": 0.5, "b": 0, "a": 1})),
        ("target", json!({"fileID": "101"})),
        ("waves", json!([1, 2, 3])),
        ("tags", json!(["a", "b"])),
    ] {
        set(property, value.clone()).unwrap_or_else(|error| panic!("{property} = {value}: {} {}", error.code, error.message));
    }
    let text = fs::read_to_string(temp.path().join(PATH)).unwrap();
    for line in ["  boost: 3\n", "  count: 12\n", "  alive: 1\n", "  title: \"007\"\n"] {
        assert!(text.contains(line), "{line:?} in {text}");
    }
}

#[test]
fn external_references_prevent_deletion() {
    let (temp, store, source) = fixture(SCENE);
    write(
        temp.path(),
        "Assets/Scenes/Other.unity",
        &SCENE.replacen(
            UNKNOWN,
            &format!(
                "unknownFuture: {{target: {{fileID: 100, guid: {}, type: 3}}}}",
                source.guid
            ),
            1,
        ),
    );
    write(
        temp.path(),
        "Assets/Scenes/Other.unity.meta",
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    );
    code(
        edit(
            &store,
            &source,
            "delete-referenced",
            json!([{"action": "delete_gameobject", "fileId": "100", "recursive": true}]),
        ),
        "referenced_object",
    );
}
#[test]
fn reject_traversal_symlinks_and_unverified_version() {
    let (temp, store, source) = fixture(SCENE);
    for path in [
        "Assets/../ProjectSettings/ProjectVersion.txt",
        "/tmp/a.cs",
        "Assets\\a.cs",
        "Library/a.cs",
    ] {
        code(store.read(path), "invalid_path");
    }
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), temp.path().join("Assets/Linked")).unwrap();
    code(store.read_script("Assets/Linked/A.cs"), "invalid_path");
    write(
        temp.path(),
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6001.0.0f1\n",
    );
    let store = Store::new(temp.path()).unwrap();
    code(
        edit(
            &store,
            &source,
            "wrong-version",
            json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "new"}]),
        ),
        "unsupported_unity_version",
    );
}
#[test]
fn interrupted_transaction_recovery_and_external_conflict() {
    for external in [false, true] {
        let (temp, store, _) = fixture(SCENE);
        let path = "Assets/Scripts/Interrupted.cs";
        let files = [
            (path.to_string(), "new source"),
            (format!("{path}.meta"), "new metadata"),
        ]
        .map(|(path, after)| JournalFile {
            path,
            before: Vec::new(),
            after: after.into(),
            existed: false,
            mode: 0o644,
        })
        .to_vec();
        let journal = Journal {
            operation_id: "interrupted".into(),
            fingerprint: "test".into(),
            state: "prepared".into(),
            files,
            result: MutationResult::default(),
            batch_result: None,
        };
        store.locked(|| Ok(())).unwrap();
        write_journal(&store.state.join("interrupted.json"), &journal).unwrap();
        write(
            temp.path(),
            path,
            if external {
                "external writer"
            } else {
                "new source"
            },
        );
        let read = store.read(PATH);
        if external {
            code(read, "outcome_unknown");
            continue;
        }
        read.unwrap();
        assert!(!temp.path().join(path).exists());
        let recovered: Journal =
            serde_json::from_slice(&fs::read(store.state.join("interrupted.json")).unwrap())
                .unwrap();
        assert_eq!(recovered.state, "rolled_back");
    }
}
#[test]
fn undo_preserves_original_bytes_and_refuses_intervening_writes() {
    let (_temp, store, source) = fixture(SCENE);
    let result = edit(
        &store,
        &source,
        "to-undo",
        json!([
        {"action": "create_gameobject", "key": "added", "name": "Added"},
        {"action": "set_property", "fileId": "100", "property": "m_Name", "value": "Edit"}]),
    )
    .unwrap();
    let undo = UndoRequest {
        path: PATH.into(),
        original_operation_id: "to-undo".into(),
        expected_revision: result.mutation.revision,
        operation_id: "undo".into(),
    };
    assert_eq!(store.undo(&undo).unwrap().revision, source.revision);
    let updated = store.read(PATH).unwrap();
    assert!(updated.content == SCENE && updated.metadata == source.metadata);
    assert!(store.undo(&undo).unwrap().replayed);
    let first = edit(&store, &updated, "first", json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "First"}])).unwrap();
    let latest = store.read(PATH).unwrap();
    let second = edit(&store, &latest, "second", json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "Second"}])).unwrap();
    code(
        store.undo(&UndoRequest {
            path: PATH.into(),
            original_operation_id: first.mutation.operation_id,
            expected_revision: second.mutation.revision,
            operation_id: "unsafe-undo".into(),
        }),
        "revision_conflict",
    );
}
#[test]
fn unicode_flow_leaf_and_plain_comma_string() {
    let (_temp, store, source) =
        fixture(&SCENE.replacen("label: 'a, b'", "label: Я, other: 'unchanged'", 1));
    edit(&store, &source, "unicode-flow", json!([
        {"action": "set_property", "fileId": "100", "property": "unknownFuture.other", "value": "Edited"},
        {"action": "set_property", "fileId": "100", "property": "unknownFuture.label", "value": "New"}]))
    .unwrap();
    assert!(
        store
            .read(PATH)
            .unwrap()
            .content
            .contains("label: \"New\", other: \"Edited\"")
    );
    let text = SCENE.replacen("m_Name: Original", "m_Name: Original, name", 1);
    let (_temp, store, source) = fixture(&text);
    edit(
        &store,
        &source,
        "plain-comma",
        json!([{"action": "set_property", "fileId": "100", "property": "m_Name", "value": "New"}]),
    )
    .unwrap();
    assert_eq!(
        store.read(PATH).unwrap().content,
        text.replacen("m_Name: Original, name", "m_Name: \"New\"", 1)
    );
}
#[test]
fn reference_numbers_and_prefab_extra_roots_are_rejected() {
    let (temp, store, source) = fixture(SCENE);
    code(
        edit(
            &store,
            &source,
            "numeric-id",
            json!([{"action": "set_property", "fileId": "100", "property": "unknownFuture", "value": {"target": {"fileID": 9007199254740992u64}}}]),
        ),
        "invalid_identity",
    );
    let path = "Assets/Scenes/Example.prefab";
    write(temp.path(), path, &prefab_fixture());
    write(
        temp.path(),
        &format!("{path}.meta"),
        "guid: cccccccccccccccccccccccccccccccc\n",
    );
    let prefab = store.read(path).unwrap();
    code(
        edit(
            &store,
            &prefab,
            "extra-root",
            json!([{"action": "create_gameobject", "name": "Extra root"}]),
        ),
        "invalid_prefab_root",
    );
}
#[test]
fn plain_component_edits_preserve_stripped_prefab_child() {
    let records = "--- !u!1001 &555
PrefabInstance:
  m_ObjectHideFlags: 0
  serializedVersion: 2
  m_Modification:
    serializedVersion: 3
    m_TransformParent: {fileID: 101}
    m_Modifications: []
    m_RemovedComponents: []
    m_RemovedGameObjects: []
    m_AddedGameObjects: []
    m_AddedComponents: []
  m_SourcePrefab: {fileID: 100100000, guid: 99999999999999999999999999999999, type: 3}
--- !u!4 &556 stripped
Transform:
  m_CorrespondingSourceObject: {fileID: 6196608569390547586, guid: 99999999999999999999999999999999, type: 3}
  m_PrefabInstance: {fileID: 555}
  m_PrefabAsset: {fileID: 0}
";
    let (_temp, store, source) =
        fixture(&(SCENE.replacen("m_Children: []", "m_Children:\n  - {fileID: 556}", 1) + records));
    store
        .create_script(&script(
            "Assets/Scripts/Ordinary.cs",
            "ordinary-script",
            "using UnityEngine; public class Ordinary : MonoBehaviour { public int counter; }\n",
            "",
        ))
        .unwrap();
    let result = edit(&store, &source, "ordinary-component", json!([
        {"action": "add_component", "fileId": "100", "key": "ordinary", "scriptPath": "Assets/Scripts/Ordinary.cs", "fields": {"counter": 7}},
        {"action": "set_property", "fileId": "100", "property": "m_Name", "value": "Changed parent"}]))
    .unwrap();
    let updated = store.read(PATH).unwrap();
    assert!(updated.content.contains(records));
    assert!(updated.content.contains("m_Children:\n  - {fileID: 556}"));
    edit(&store, &updated, "ordinary-field", json!([{"action": "set_property", "fileId": result.mutation.created_ids["ordinary"], "property": "counter", "value": 9}])).unwrap();
    let latest = store.read(PATH).unwrap();
    for (id, operation) in [
        (
            "reparent-prefab-tree",
            json!({"action": "reparent", "fileId": "100", "parentId": "0"}),
        ),
        (
            "delete-prefab-tree",
            json!({"action": "delete_gameobject", "fileId": "100", "recursive": true}),
        ),
        (
            "edit-stripped",
            json!({"action": "set_property", "fileId": "556", "property": "m_LocalPosition.x", "value": 1}),
        ),
    ] {
        code(
            edit(&store, &latest, id, json!([operation])),
            "unsupported_prefab_write",
        );
    }
}
#[test]
fn unity_boolean_properties_accept_json_booleans() {
    let (_temp, store, source) = fixture(SCENE);
    edit(&store, &source, "deactivate", json!([{"action": "set_property", "fileId": "100", "property": "m_IsActive", "value": false}])).unwrap();
    assert!(!raw(&store).objects[0].active);
    assert_eq!(
        store.read(PATH).unwrap().content,
        SCENE.replacen("m_IsActive: 1", "m_IsActive: 0", 1)
    );
}

// ---- enhancements ----

#[test]
fn inventory_pages_native_inputs_and_binds_stat_metadata_snapshot() {
    let (temp, store, _) = fixture(SCENE);
    for index in 0..120 {
        write(
            temp.path(),
            &format!("Assets/Native{index:03}.png"),
            "\0binary",
        );
        write(
            temp.path(),
            &format!("Assets/Native{index:03}.png.meta"),
            &format!("guid: {:032x}\n", index + 1),
        );
    }
    let inventory = |value: Value| store.inventory(&serde_json::from_value(value).unwrap());
    let page = inventory(json!({"extensions": [".PNG"]})).unwrap();
    assert!(page.entries.len() == 100 && page.total == 120 && page.next_offset == Some(100));
    let second = inventory(
        json!({"extensions": [".png"], "offset": 100, "expectedRevision": page.revision}),
    )
    .unwrap();
    assert!(second.entries.len() == 20 && second.next_offset.is_none());
    write(
        temp.path(),
        "Assets/Native000.png.meta",
        "guid: ffffffffffffffffffffffffffffffff\n",
    );
    code(
        inventory(json!({"offset": 100, "expectedRevision": page.revision})),
        "revision_conflict",
    );
    let page =
        inventory(json!({"pathPrefix": "Assets/Scenes", "includeSourceRevision": true})).unwrap();
    assert!(page.entries.len() == 1 && !page.entries[0].source_revision.is_empty());
    code(
        inventory(json!({"pathPrefix": "Assets/../ProjectSettings"})),
        "invalid_path",
    );
}
#[test]
fn native_component_template_and_explicit_fields() {
    let (_temp, store, source) = fixture(SCENE);
    let fields = json!({"serializedVersion": 3, "m_Enabled": true, "m_IsTrigger": false, "m_Center": {"x": 0, "y": 0, "z": 0}, "m_Size": {"x": 1, "y": 2, "z": 3}, "m_Material": {"fileID": "0"}});
    let result = edit(&store, &source, "native-box", json!([{"action": "add_component", "fileId": "100", "key": "box", "componentType": "UnityEngine.BoxCollider", "fields": fields}])).unwrap();
    let after = store.read(PATH).unwrap();
    assert_eq!(after.metadata, source.metadata);
    let asset = TextAsset::parse(&after.content).unwrap();
    let id = &result.mutation.created_ids["box"];
    let component = asset.find(id).unwrap();
    assert!(
        component.class_id() == 65
            && component.field("serializedVersion") == 3
            && component.field("m_IsTrigger") == 0
    );
    edit(&store, &after, "native-copy", json!([{"action": "add_component", "fileId": "100", "key": "copy", "classId": 65, "templatePath": PATH, "templateFileId": id, "templateRevision": after.revision}])).unwrap();
    let latest = store.read(PATH).unwrap();
    code(
        edit(
            &store,
            &latest,
            "native-stale-template",
            json!([{"action": "add_component", "fileId": "100", "classId": 65, "templatePath": PATH, "templateFileId": id, "templateRevision": after.revision}]),
        ),
        "revision_conflict",
    );
    code(
        edit(
            &store,
            &latest,
            "native-unknown",
            json!([{"action": "add_component", "fileId": "100", "componentType": "Unknown", "fields": fields}]),
        ),
        "unsupported_component",
    );
    code(
        edit(
            &store,
            &latest,
            "native-no-defaults",
            json!([{"action": "add_component", "fileId": "100", "classId": 20}]),
        ),
        "invalid_request",
    );
}
#[test]
fn nested_collection_reference_replacement_preserves_neighbor_bytes() {
    let input = SCENE.replacen(UNKNOWN, "unknownFuture: {value: 00123, label: 'a, b', target: {fileID: 0}, list: [1, 2]} # neighbor", 1);
    let (_temp, store, source) = fixture(&input);
    edit(&store, &source, "nested-reference", json!([
        {"action": "set_property", "fileId": "100", "property": "unknownFuture.target", "value": {"fileID": "101"}},
        {"action": "set_property", "fileId": "100", "property": "unknownFuture.list", "value": [3, 4, 5]}]))
    .unwrap();
    let after = store.read(PATH).unwrap();
    assert_eq!(
        after.content,
        input.replacen(
            "target: {fileID: 0}, list: [1, 2]",
            "target: {fileID: 101}, list: [3, 4, 5]",
            1
        )
    );
    code(
        edit(
            &store,
            &after,
            "bad-ref-guid",
            json!([{"action": "set_property", "fileId": "100", "property": "unknownFuture.target", "value": {"fileID": "1", "guid": "bogus"}}]),
        ),
        "invalid_identity",
    );
}
fn create_base(store: &Store, id: &str) -> MutationResult {
    store
        .create_asset(&script(
            "Assets/Base.prefab",
            id,
            &prefab_fixture(),
            "22222222222222222222222222222222",
        ))
        .unwrap()
}
#[test]
fn source_prefab_instantiation_and_overrides_resolve_through_runtime_expander() {
    let (_temp, store, source) = fixture(SCENE);
    let created = create_base(&store, "prefab-create");
    let graph = resolved(&store, "Assets/Base.prefab");
    assert_eq!(graph.total, 1);
    let revisions = graph.dependency_revisions.clone().unwrap();
    let instantiated = edit(&store, &source, "prefab-instantiate", json!([{"action": "instantiate_prefab", "key": "prefab", "prefabPath": "Assets/Base.prefab", "prefabRevision": created.revision, "dependencyRevisions": revisions, "parentId": "100", "name": "Instance"}])).unwrap();
    let page = resolved(&store, PATH);
    assert!(page.total == 2 && page.unresolved_prefab_instances.is_empty());
    let object = page
        .objects
        .iter()
        .find(|object| object.name == "Instance")
        .unwrap();
    assert!(object.parent_id == "100" && object.file_id.contains(':'));
    let instance = &instantiated.mutation.created_ids["prefab.instance"];
    let latest = store.read(PATH).unwrap();
    edit(&store, &latest, "prefab-position", json!([{"action": "set_prefab_override", "instanceId": instance, "targetFileId": "101", "property": "m_LocalPosition.x", "value": 42, "dependencyRevisions": revisions}])).unwrap();
    let page = resolved(&store, PATH);
    let object = page
        .objects
        .iter()
        .find(|object| object.name == "Instance")
        .unwrap();
    assert_eq!(
        object.components[0].fields.as_ref().unwrap()["m_LocalPosition"]["x"],
        42
    );
    let latest = store.read(PATH).unwrap();
    edit(&store, &latest, "prefab-position-revert", json!([{"action": "revert_prefab_override", "instanceId": instance, "targetFileId": "101", "property": "m_LocalPosition.x", "dependencyRevisions": revisions}])).unwrap();
    assert_eq!(
        store.read("Assets/Base.prefab").unwrap().revision,
        created.revision
    );
    code(
        store.hierarchy(&HierarchyRequest {
            path: PATH.into(),
            expected_revision: page.revision,
            ..HierarchyRequest::default()
        }),
        "revision_conflict",
    );
}
fn staged(id: &str, inputs: &[(&str, &str)], outputs: &[(&str, &str)]) -> StagedPublicationRequest {
    StagedPublicationRequest {
        operation_id: id.into(),
        inputs: inputs
            .iter()
            .map(|(path, revision)| PublicationInput {
                path: path.to_string(),
                expected_revision: revision.to_string(),
            })
            .collect(),
        outputs: outputs
            .iter()
            .map(|(path, content)| PublicationOutput {
                path: path.to_string(),
                content: content.to_string(),
                metadata: None,
            })
            .collect(),
    }
}
#[test]
fn staged_publication_checks_all_inputs_and_replays_after_restart() {
    let (temp, store, scene) = fixture(SCENE);
    let dependency = store
        .create_script(&script(
            "Assets/Scripts/Dependency.cs",
            "dependency",
            "public class Dependency {}",
            "",
        ))
        .unwrap();
    let version = "ProjectSettings/ProjectVersion.txt";
    let project_revision = store.dependency_revision(version).unwrap();
    let mut request = staged(
        "staged-two",
        &[
            (PATH, &scene.revision),
            (&dependency.path, &dependency.revision),
            (version, &project_revision),
        ],
        &[
            (
                PATH,
                &scene
                    .content
                    .replacen("m_Name: Original", "m_Name: Staged", 1),
            ),
            (
                &dependency.path,
                "public class Dependency { public int staged; }",
            ),
        ],
    );
    let result = store.publish_staged(&request).unwrap();
    assert!(result.changed && result.assets.len() == 2);
    let store = Store::new(temp.path()).unwrap();
    let replay = store.publish_staged(&request).unwrap();
    assert!(replay.replayed && replay.assets[0].revision == result.assets[0].revision);
    let latest_scene = store.read(PATH).unwrap();
    request.operation_id = "dependency-conflict".into();
    request.inputs[0].expected_revision = latest_scene.revision.clone();
    request.inputs[1].expected_revision = store.read(&dependency.path).unwrap().revision;
    write(
        temp.path(),
        version,
        "m_EditorVersion: 6000.6.3f1\nchanged: 1\n",
    );
    code(store.publish_staged(&request), "revision_conflict");
    assert_eq!(store.read(PATH).unwrap().revision, latest_scene.revision);
    request.operation_id = "staged-metadata-change".into();
    request.inputs.truncate(2);
    request.outputs[0].metadata = Some(format!("{}changed: 1\n", latest_scene.metadata));
    code(store.publish_staged(&request), "protected_property");
}
#[test]
fn staged_publication_recovery_rolls_back_every_changed_file() {
    let (temp, store, source) = fixture(SCENE);
    let other_path = "Assets/Second.unity";
    write(temp.path(), other_path, &source.content);
    write(
        temp.path(),
        &format!("{other_path}.meta"),
        "guid: 33333333333333333333333333333333\n",
    );
    let other = store.read(other_path).unwrap();
    let changed = source.content.replacen("Original", "Changed", 1);
    let request = staged(
        "staged-interrupted",
        &[(PATH, &source.revision), (other_path, &other.revision)],
        &[(PATH, &changed), (other_path, &changed)],
    );
    let files = [PATH, other_path]
        .map(|path| JournalFile {
            path: path.into(),
            before: source.content.clone().into_bytes(),
            after: changed.clone().into_bytes(),
            existed: true,
            mode: 0o644,
        })
        .to_vec();
    let journal = Journal {
        operation_id: request.operation_id.clone(),
        fingerprint: sha256(&encode_json(&request).unwrap()),
        state: "prepared".into(),
        files,
        result: MutationResult::default(),
        batch_result: Some(PublicationResult {
            operation_id: request.operation_id.clone(),
            assets: Vec::new(),
            changed: false,
            replayed: false,
        }),
    };
    write_journal(&store.receipt_path(&request.operation_id), &journal).unwrap();
    write(temp.path(), PATH, &changed);
    code(store.publish_staged(&request), "rolled_back");
    assert_eq!(store.read(PATH).unwrap().revision, source.revision);
    assert_eq!(store.read(other_path).unwrap().revision, other.revision);
}
#[test]
fn publication_reference_overlay_and_whole_undo() {
    let (temp, store, original) = fixture(SCENE);
    let added = edit(&store, &original, "child-for-overlay", json!([{"action": "create_gameobject", "key": "child", "name": "Child", "parentId": "100"}])).unwrap();
    let scene = store.read(PATH).unwrap();
    let other_path = "Assets/External.asset";
    let link = format!(
        "linked: {{fileID: {}, guid: {}, type: 3}}",
        added.mutation.created_ids["child"], scene.guid
    );
    let other_content =
        format!("--- !u!114 &11400000\nMonoBehaviour:\n  m_GameObject: {{fileID: 0}}\n  {link}\n");
    write(temp.path(), other_path, &other_content);
    write(
        temp.path(),
        &format!("{other_path}.meta"),
        "guid: 44444444444444444444444444444444\n",
    );
    let other = store.read(other_path).unwrap();
    let inputs = [
        (PATH, scene.revision.as_str()),
        (other_path, other.revision.as_str()),
    ];
    code(
        store.publish_staged(&staged(
            "remove-with-overlay",
            &inputs,
            &[(PATH, &original.content)],
        )),
        "referenced_object",
    );
    let result = store
        .publish_staged(&staged(
            "remove-with-overlay",
            &inputs,
            &[
                (PATH, &original.content),
                (
                    other_path,
                    &other_content.replacen(&link, "linked: {fileID: 0}", 1),
                ),
            ],
        ))
        .unwrap();
    let undo = PublicationUndoRequest {
        operation_id: "undo-overlay".into(),
        original_operation_id: result.operation_id.clone(),
        expected_revisions: result
            .assets
            .iter()
            .map(|asset| (asset.path.clone(), asset.revision.clone()))
            .collect(),
    };
    store.undo_publication(&undo).unwrap();
    assert!(store.undo_publication(&undo).unwrap().replayed);
    assert_eq!(store.read(PATH).unwrap().revision, scene.revision);
    assert_eq!(store.read(other_path).unwrap().revision, other.revision);
}
#[test]
fn explicit_material_and_scriptable_object_creation() {
    let (_temp, store, _) = fixture(SCENE);
    let material = "--- !u!21 &2100000\nMaterial:\n  m_Name: Explicit # keep\n  serializedVersion: 8\n  m_Shader: {fileID: 108, guid: 0000000000000000f000000000000000, type: 0}\n  m_SavedProperties:\n    serializedVersion: 3\n    m_TexEnvs: []\n    m_Ints: []\n    m_Floats: []\n    m_Colors: []\n  unknownFuture: {v: 00123}\n";
    let created = store
        .create_asset(&script(
            "Assets/Explicit.mat",
            "material-create",
            material,
            "",
        ))
        .unwrap();
    let source = store.read(&created.path).unwrap();
    edit(&store, &source, "material-color", json!([{"action": "set_property", "fileId": "2100000", "property": "m_SavedProperties.m_Colors", "value": [{"_Color": {"r": 1, "g": 0.5, "b": 0, "a": 1}}]}])).unwrap();
    let after = store.read(&created.path).unwrap();
    assert!(
        after.metadata == source.metadata
            && after.content.contains("unknownFuture: {v: 00123}")
            && after.content.contains("m_Name: Explicit # keep")
    );
    let data_script = store
        .create_script(&script(
            "Assets/Scripts/Data.cs",
            "scriptable-script",
            "public class Data : UnityEngine.ScriptableObject { public int value; }",
            "",
        ))
        .unwrap();
    let data = format!(
        "--- !u!114 &11400000\nMonoBehaviour:\n  m_GameObject: {{fileID: 0}}\n  m_Script: {{fileID: 11500000, guid: {}, type: 3}}\n  m_Name: Data\n  value: 5\n",
        data_script.guid
    );
    let created = store
        .create_asset(&script("Assets/Data.asset", "scriptable-create", &data, ""))
        .unwrap();
    let source = store.read(&created.path).unwrap();
    assert!(source.content == data && source.metadata.contains("mainObjectFileID: 11400000"));
}
#[test]
fn rect_transform_creation_requires_explicit_layout_and_keeps_ownership() {
    let (_temp, store, source) = fixture(SCENE);
    let fields = json!({"m_LocalPosition": {"x": 0, "y": 0, "z": 0}, "m_LocalRotation": {"x": 0, "y": 0, "z": 0, "w": 1}, "m_LocalScale": {"x": 1, "y": 1, "z": 1}, "m_AnchorMin": {"x": 0, "y": 0}, "m_AnchorMax": {"x": 1, "y": 1}, "m_AnchoredPosition": {"x": 0, "y": 0}, "m_SizeDelta": {"x": 100, "y": 50}, "m_Pivot": {"x": 0.5, "y": 0.5}});
    code(
        edit(
            &store,
            &source,
            "rect-no-defaults",
            json!([{"action": "create_gameobject", "name": "Rect", "rectTransform": true}]),
        ),
        "invalid_request",
    );
    code(
        edit(
            &store,
            &source,
            "rect-wrong-parent",
            json!([{"action": "create_gameobject", "name": "Rect", "parentId": "999", "rectTransform": true, "fields": fields}]),
        ),
        "not_found",
    );
    let created = edit(&store, &source, "rect-create", json!([{"action": "create_gameobject", "key": "rect", "name": "Rect", "parentId": "100", "rectTransform": true, "fields": fields}])).unwrap();
    let page = resolved(&store, PATH);
    let object = page
        .objects
        .iter()
        .find(|object| object.file_id == created.mutation.created_ids["rect"])
        .unwrap();
    assert!(object.parent_id == "100" && object.components[0].class_id == 224);
}
#[test]
fn scene_roots_accepts_editor_prefab_instance_identity_and_rejects_duplicates() {
    let (_temp, store, source) = fixture(SCENE);
    let created = create_base(&store, "roots-prefab");
    let revisions = resolved(&store, &created.path)
        .dependency_revisions
        .unwrap();
    let result = edit(&store, &source, "roots-instance", json!([{"action": "instantiate_prefab", "key": "instance", "prefabPath": created.path, "prefabRevision": created.revision, "dependencyRevisions": revisions}])).unwrap();
    let latest = store.read(PATH).unwrap();
    let mut parsed = TextAsset::parse(&latest.content).unwrap();
    let roots = parsed.root_document().unwrap();
    let (roots_id, aliases) = (
        roots.id().to_string(),
        ids_from_list(roots.field("m_Roots")),
    );
    let ids = &result.mutation.created_ids;
    let (instance, transform) = (&ids["instance.instance"], &ids["instance.transform"]);
    let mut references: Vec<String> = aliases
        .into_iter()
        .map(|id| {
            if &id == transform {
                instance.clone()
            } else {
                id
            }
        })
        .collect();
    parsed
        .set_top_level(&roots_id, "m_Roots", &reference_list(&references))
        .unwrap();
    let published = store
        .publish_staged(&staged(
            "editor-sceneroot-publication",
            &[(PATH, &latest.revision)],
            &[(PATH, &parsed.text())],
        ))
        .unwrap();
    assert!(published.changed);
    assert_eq!(resolved(&store, PATH).roots.len(), 2);
    references.push(transform.clone());
    parsed
        .set_top_level(&roots_id, "m_Roots", &reference_list(&references))
        .unwrap();
    code(parsed.validate(&latest.guid), "invalid_graph");
    let position = parsed.position(instance).unwrap();
    parsed.docs[position].document.data["m_Modification"]["m_TransformParent"] = reference("101");
    code(parsed.validate(&latest.guid), "invalid_graph");
}
#[test]
fn raw_scene_roots_preserves_unresolved_prefab_links_without_zero_roots() {
    let base = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &10
GameObject:
  m_Name: Ordinary
  m_IsActive: 1
  m_Component:
  - component: {fileID: 11}
--- !u!4 &11
Transform:
  m_GameObject: {fileID: 10}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!1001 &500
PrefabInstance:
  m_Modification:
    m_TransformParent: {fileID: 0}
  m_SourcePrefab: {fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}
--- !u!4 &501 stripped
Transform:
  m_CorrespondingSourceObject: {fileID: 101, guid: 22222222222222222222222222222222, type: 3}
  m_PrefabInstance: {fileID: 500}
  m_PrefabAsset: {fileID: 0}
";
    for (alias, root, is_resolved) in [
        ("", "500", false),
        ("  m_GameObject: {fileID: 502}\n", "500", false),
        ("  m_GameObject: {fileID: 502}\n", "501", true),
        (
            "  m_GameObject: {fileID: 502}\n  m_Father: {fileID: 0}\n",
            "500",
            true,
        ),
    ] {
        let content = format!(
            "{base}{alias}--- !u!1 &502 stripped\nGameObject:\n  m_CorrespondingSourceObject: {{fileID: 100, guid: 22222222222222222222222222222222, type: 3}}\n  m_PrefabInstance: {{fileID: 500}}\n  m_PrefabAsset: {{fileID: 0}}\n--- !u!1660057539 &9223372036854775807\nSceneRoots:\n  m_Roots:\n  - {{fileID: 11}}\n  - {{fileID: {root}}}\n"
        );
        let asset = TextAsset::parse(&content).unwrap();
        let docs: Vec<_> = asset.docs.iter().map(|doc| doc.document.clone()).collect();
        let view = hierarchy(&Source::default(), &docs, "");
        assert_eq!(view.roots.len(), if is_resolved { 2 } else { 1 });
        assert!(view.roots[0] == "10" && (!is_resolved || view.roots[1] == "502"));
        assert!(is_resolved || view.unresolved_root_ids == [root]);
        assert_eq!(view.unresolved_prefab_instances, ["500"]);
    }
}

// ---- native component defaults ----

fn template(name: &str, extra: &str, documents: &str) -> NativeDefaultsTemplate {
    let content = SCENE.replacen(
        "  - component: {fileID: 101}\n",
        &format!("  - component: {{fileID: 101}}\n{extra}"),
        1,
    ) + documents;
    NativeDefaultsTemplate {
        schema_version: 1,
        profile: "verified-profile".into(),
        unity_version: crate::ENGINE_VERSION.into(),
        unity_revision: "45d8eee7de74".into(),
        component_type: name.into(),
        sha256: sha256(content.as_bytes()),
        content,
    }
}
fn rehash(mut template: NativeDefaultsTemplate) -> NativeDefaultsTemplate {
    template.sha256 = sha256(template.content.as_bytes());
    template
}
fn native(class_id: i32, id: &str, name: &str, fields: &str) -> String {
    format!(
        "--- !u!{class_id} &{id}\n{name}:\n  m_ObjectHideFlags: 0\n  m_CorrespondingSourceObject: {{fileID: 0}}\n  m_PrefabInstance: {{fileID: 0}}\n  m_PrefabAsset: {{fileID: 0}}\n  m_GameObject: {{fileID: 100}}\n{fields}"
    )
}
fn box_template() -> NativeDefaultsTemplate {
    template(
        "UnityEngine.BoxCollider",
        "  - component: {fileID: 102}\n",
        &native(
            65,
            "102",
            "BoxCollider",
            "  serializedVersion: 3\n  m_IsTrigger: 0\n  m_Enabled: 1\n  m_Size: {x: 1, y: 1, z: 1}\n  m_Center: {x: 0, y: 0, z: 0}\n  exactCounter: 0\n",
        ),
    )
}
fn light_template() -> NativeDefaultsTemplate {
    template(
        "UnityEngine.Light",
        "  - component: {fileID: 102}\n",
        &native(
            108,
            "102",
            "Light",
            "  m_Enabled: 1\n  m_Type: 2\n  m_Intensity: 1\n  unknownFuture: {value: 00123, text: 'a, b'} # preserve component\n",
        ),
    )
}
fn defaults(
    source: &Source,
    id: &str,
    component: &str,
    template: NativeDefaultsTemplate,
    fields: Value,
) -> NativeDefaultsRequest {
    NativeDefaultsRequest {
        path: source.path.clone(),
        expected_revision: source.revision.clone(),
        operation_id: id.into(),
        file_id: "100".into(),
        target_path: String::new(),
        component_type: component.into(),
        fields: serde_json::from_value(fields).unwrap(),
        template,
    }
}
#[test]
fn native_defaults_source_cas_replay_undo_and_exact_values() {
    let (temp, store, source) = fixture(SCENE);
    let request = defaults(
        &source,
        "cached-box",
        "UnityEngine.BoxCollider",
        box_template(),
        json!({"m_IsTrigger": true, "m_Size.x": 3, "exactCounter": 9223372036854775807i64}),
    );
    let added = store.add_native_defaults(&request, None).unwrap();
    let current = store.read(PATH).unwrap();
    assert!(
        current.metadata == source.metadata
            && current
                .content
                .contains("exactCounter: 9223372036854775807")
            && current.content.contains("m_Layer: 0 # keep my comment")
            && current.content.contains("m_IsTrigger: 1")
    );
    let parsed = TextAsset::parse(&current.content).unwrap();
    let id = &added.created_ids["UnityEngine.BoxCollider"];
    let component = parsed.find(id).unwrap();
    assert!(
        component.class_id() == 65
            && component.field("m_GameObject")["fileID"] == "100"
            && component.field("serializedVersion") == 3
    );
    let reopened = Store::new(temp.path()).unwrap();
    let replay = reopened.add_native_defaults(&request, None).unwrap();
    assert!(
        replay.replayed
            && replay.revision == added.revision
            && &replay.created_ids["UnityEngine.BoxCollider"] == id
    );
    let mut changed = request.clone();
    changed.fields = BTreeMap::from([("m_IsTrigger".to_string(), json!(false))]);
    code(
        reopened.add_native_defaults(&changed, None),
        "idempotency_conflict",
    );
    let mut stale = request.clone();
    stale.operation_id = "cached-box-stale".into();
    code(
        reopened.add_native_defaults(&stale, None),
        "revision_conflict",
    );
    reopened
        .undo(&UndoRequest {
            path: PATH.into(),
            original_operation_id: request.operation_id,
            operation_id: "undo-cached-box".into(),
            expected_revision: added.revision,
        })
        .unwrap();
    let restored = reopened.read(PATH).unwrap();
    assert!(restored.content == source.content && restored.metadata == source.metadata);
}
#[test]
fn native_defaults_publication_guard_rejects_before_writing() {
    let (_temp, store, source) = fixture(SCENE);
    let request = defaults(
        &source,
        "guarded-box",
        "UnityEngine.BoxCollider",
        box_template(),
        json!({}),
    );
    let rejected = store.add_native_defaults(
        &request,
        Some(&|| {
            Err(failure(
                "component_defaults_changed",
                "native settings changed",
                None,
            ))
        }),
    );
    code(rejected, "component_defaults_changed");
    assert_eq!(store.read(PATH).unwrap().revision, source.revision);
    store
        .add_native_defaults(&request, Some(&|| Ok(())))
        .unwrap();
}
#[test]
fn native_defaults_required_peer_references_and_owner_remapping() {
    let (_temp, store, source) = fixture(SCENE);
    let particles = template(
        "UnityEngine.ParticleSystem",
        "  - component: {fileID: 102}\n  - component: {fileID: 103}\n",
        &(native(198, "102", "ParticleSystem", "  renderer: {fileID: 103}\n")
            + &native(
                199,
                "103",
                "ParticleSystemRenderer",
                "  system: {fileID: 102}\n",
            )),
    );
    let mut request = defaults(
        &source,
        "cached-particles",
        "UnityEngine.ParticleSystem",
        particles,
        json!({}),
    );
    request.file_id = String::new();
    request.target_path = "Original".into();
    let result = store.add_native_defaults(&request, None).unwrap();
    let asset = TextAsset::parse(&store.read(PATH).unwrap().content).unwrap();
    let system = asset
        .find(&result.created_ids["UnityEngine.ParticleSystem"])
        .unwrap();
    let renderer = asset
        .find(&result.created_ids["UnityEngine.ParticleSystemRenderer"])
        .unwrap();
    assert!(
        system.field("renderer")["fileID"] == renderer.id()
            && renderer.field("system")["fileID"] == system.id()
    );
    assert_eq!(asset.component_ids(asset.position("100").unwrap()).len(), 3);
}
#[test]
fn native_defaults_canvas_replaces_transform_without_changing_identity() {
    let scene = SCENE.replacen(
        "  m_Father: {fileID: 0}\n",
        "  m_Father: {fileID: 0}\n  futureTransformField: {exactValue: 9223372036854775807}\n",
        1,
    );
    let (_temp, store, source) = fixture(&scene);
    let mut canvas = template(
        "UnityEngine.Canvas",
        "  - component: {fileID: 102}\n",
        &native(223, "102", "Canvas", "  m_Enabled: 1\n  m_RenderMode: 0\n"),
    );
    canvas.content = canvas
        .content
        .replacen("--- !u!4 &101\nTransform:", "--- !u!224 &101\nRectTransform:", 1)
        .replacen("  m_Father: {fileID: 0}\n", "  m_Father: {fileID: 0}\n  m_AnchorMin: {x: 0.5, y: 0.5}\n  m_AnchorMax: {x: 0.5, y: 0.5}\n  m_AnchoredPosition: {x: 0, y: 0}\n  m_SizeDelta: {x: 100, y: 100}\n  m_Pivot: {x: 0.5, y: 0.5}\n", 1);
    let result = store
        .add_native_defaults(
            &defaults(
                &source,
                "cached-canvas",
                "UnityEngine.Canvas",
                rehash(canvas),
                json!({}),
            ),
            None,
        )
        .unwrap();
    let asset = TextAsset::parse(&store.read(PATH).unwrap().content).unwrap();
    let rect = asset.find("101").unwrap();
    assert!(rect.class_id() == 224 && result.created_ids["UnityEngine.RectTransform"] == "101");
    assert!(
        rect.field("m_Father")["fileID"] == "0"
            && rect.field("m_LocalPosition")["y"] == 1
            && asset.docs[1].id() == "101"
    );
    assert_eq!(
        ids_from_list(asset.root_document().unwrap().field("m_Roots"))[0],
        "101"
    );
    assert_eq!(
        rect.field("futureTransformField")["exactValue"],
        9223372036854775807i64
    );
}
#[test]
fn native_defaults_reject_changed_hash_unsupported_companions_and_ownership_fields() {
    for invalid in [
        "hash",
        "revision",
        "companion",
        "local-reference",
        "ownership",
        "unknown-field",
    ] {
        let (_temp, store, source) = fixture(SCENE);
        let mut template = box_template();
        let mut fields = json!({});
        match invalid {
            "hash" => template.content += "# changed\n",
            "revision" => template.unity_revision = String::new(),
            "companion" => {
                template.content = template.content.replacen(
                    "--- !u!65 &102\nBoxCollider:",
                    "--- !u!114 &102\nMonoBehaviour:",
                    1,
                );
                template = rehash(template);
            }
            "local-reference" => {
                template.content += "  other: {fileID: 777}\n";
                template = rehash(template);
            }
            "ownership" => fields = json!({"m_GameObject": {"fileID": "101"}}),
            _ => fields = json!({"unknown": 4}),
        }
        let result = store.add_native_defaults(
            &defaults(
                &source,
                "reject-default",
                "UnityEngine.BoxCollider",
                template,
                fields,
            ),
            None,
        );
        assert!(result.is_err(), "{invalid} was accepted");
        let current = store.read(PATH).unwrap();
        assert!(current.content == source.content && current.metadata == source.metadata);
    }
}

// ---- prefab authoring ----

fn prefab(store: &Store, path: &str, id: &str, mut value: Value) -> PrefabEditResult {
    value["path"] = json!(path);
    value["operationId"] = json!(id);
    value["expectedRevision"] = json!(store.read(path).unwrap().revision);
    let result = store
        .prefab_edit(&serde_json::from_value(value).unwrap(), None)
        .unwrap_or_else(|error| panic!("{id}: {error:?}"));
    assert_eq!(result.backend, "raw_source");
    result
}
fn object(store: &Store, path: &str, name: &str) -> GameObject {
    resolved(store, path)
        .objects
        .into_iter()
        .find(|object| object.name == name)
        .unwrap_or_else(|| panic!("object {name} absent in {path}"))
}
fn field(store: &Store, path: &str, name: &str, component: &str, property: &str) -> Value {
    let object = object(store, path, name);
    let component = object
        .components
        .iter()
        .find(|value| value.type_name == component)
        .unwrap_or_else(|| panic!("component {component} absent"));
    authored_property_value(component.fields.as_ref().unwrap(), property).unwrap()
}
fn setup() -> (TempDir, Store, Source) {
    let (temp, store, scene) = fixture(SCENE);
    store
        .add_native_defaults(
            &defaults(
                &scene,
                "setup-box",
                "BoxCollider",
                box_template(),
                json!({}),
            ),
            None,
        )
        .unwrap();
    prefab(
        &store,
        PATH,
        "create-base",
        json!({"action": "create_prefab", "fileId": "100", "outputPath": "Assets/Base.prefab"}),
    );
    let scene = store.read(PATH).unwrap();
    (temp, store, scene)
}
#[test]
fn source_prefab_create_instantiate_apply_revert_and_replay() {
    let (temp, store, scene) = setup();
    let base = store.read("Assets/Base.prefab").unwrap();
    assert!(
        base.content.contains("value: 00123")
            && base.content.contains("# position comment")
            && base.content.contains("# keep my comment")
    );
    assert_eq!(object(&store, "Assets/Base.prefab", "Base").file_id, "100");
    let mut request: PrefabEditRequest = serde_json::from_value(json!({"action": "instantiate", "path": PATH, "expectedRevision": scene.revision, "operationId": "instantiate-base", "prefabPath": "Assets/Base.prefab", "parentFileId": "100", "name": "Linked"})).unwrap();
    assert!(store.prefab_edit(&request, None).unwrap().created_ids.len() >= 3);
    let store = Store::new(temp.path()).unwrap();
    assert!(
        store
            .prefab_edit(&request, None)
            .unwrap()
            .publication
            .replayed
    );
    request.name = "Different".into();
    code(store.prefab_edit(&request, None), "idempotency_conflict");
    let linked = "Original/Linked";
    prefab(
        &store,
        PATH,
        "override-box",
        json!({"action": "set_override", "targetPath": linked, "componentType": "BoxCollider", "property": "m_Size.x", "value": 7}),
    );
    assert_eq!(field(&store, PATH, "Linked", "BoxCollider", "m_Size.x"), 7);
    let apply = prefab(
        &store,
        PATH,
        "apply-box",
        json!({"action": "apply", "targetPath": linked}),
    );
    assert_eq!(apply.publication.assets.len(), 2);
    assert_eq!(
        field(
            &store,
            "Assets/Base.prefab",
            "Base",
            "BoxCollider",
            "m_Size.x"
        ),
        7
    );
    let after = store.read("Assets/Base.prefab").unwrap();
    assert!(
        after.metadata == base.metadata
            && after.content.contains("value: 00123")
            && after.content.contains("# position comment")
    );
    prefab(
        &store,
        PATH,
        "override-box-again",
        json!({"action": "set_override", "targetPath": linked, "componentType": "BoxCollider", "property": "m_Size.x", "value": 9}),
    );
    prefab(
        &store,
        PATH,
        "default-pose",
        json!({"action": "set_override", "targetPath": linked, "componentType": "Transform", "property": "m_LocalPosition.x", "value": 42}),
    );
    prefab(
        &store,
        PATH,
        "revert-box",
        json!({"action": "revert", "targetPath": linked}),
    );
    assert_eq!(field(&store, PATH, "Linked", "BoxCollider", "m_Size.x"), 7);
    assert_eq!(
        field(&store, PATH, "Linked", "Transform", "m_LocalPosition.x"),
        42
    );
    assert_eq!(store.read(PATH).unwrap().metadata, scene.metadata);
}
#[test]
fn source_prefab_nested_variant_partial_and_complete_unpack() {
    for complete in [false, true] {
        let (_temp, store, scene) = setup();
        let created = edit(&store, &scene, "container", json!([{"action": "create_gameobject", "key": "container", "name": "Container", "parentId": "100"}])).unwrap();
        let container = &created.mutation.created_ids["container"];
        prefab(
            &store,
            PATH,
            "nested-child",
            json!({"action": "instantiate", "prefabPath": "Assets/Base.prefab", "parentFileId": container, "name": "NestedChild"}),
        );
        prefab(
            &store,
            PATH,
            "create-nested",
            json!({"action": "create_prefab", "fileId": container, "outputPath": "Assets/Nested.prefab"}),
        );
        let nested = store.read("Assets/Nested.prefab").unwrap();
        assert!(nested.content.contains("PrefabInstance:"));
        for doc in TextAsset::parse(&nested.content)
            .unwrap()
            .docs
            .iter()
            .filter(|doc| doc.stripped())
        {
            let instance: i64 = doc.field("m_PrefabInstance")["fileID"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let source: i64 = doc.field("m_CorrespondingSourceObject")["fileID"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(doc.id(), ((instance ^ source) & i64::MAX).to_string());
        }
        prefab(
            &store,
            PATH,
            "create-variant",
            json!({"action": "create_variant", "prefabPath": "Assets/Nested.prefab", "outputPath": "Assets/Variant.prefab"}),
        );
        prefab(
            &store,
            PATH,
            "variant-instance",
            json!({"action": "instantiate", "prefabPath": "Assets/Variant.prefab", "name": "VariantLinked"}),
        );
        prefab(
            &store,
            PATH,
            "variant-nested-override",
            json!({"action": "set_override", "targetPath": "VariantLinked/NestedChild", "componentType": "BoxCollider", "property": "m_Size.y", "value": 17}),
        );
        prefab(
            &store,
            PATH,
            "variant-apply",
            json!({"action": "apply", "targetPath": "VariantLinked"}),
        );
        assert_eq!(
            field(
                &store,
                "Assets/Variant.prefab",
                "NestedChild",
                "BoxCollider",
                "m_Size.y"
            ),
            17
        );
        assert_ne!(
            field(
                &store,
                "Assets/Base.prefab",
                "Base",
                "BoxCollider",
                "m_Size.y"
            ),
            17
        );
        prefab(
            &store,
            PATH,
            "unpack-variant",
            json!({"action": "unpack", "targetPath": "VariantLinked", "completely": complete}),
        );
        let unpacked = object(&store, PATH, "VariantLinked");
        assert!(!field(&store, PATH, "NestedChild", "BoxCollider", "m_Size.y").is_null());
        assert_eq!(unpacked.file_id.contains(':'), !complete);
        assert!(
            resolved(&store, PATH)
                .unresolved_prefab_instances
                .is_empty()
        );
    }
}
#[test]
fn source_prefab_dependency_metadata_conflict_and_external_subtree_reference() {
    let (temp, store, scene) = setup();
    let request: PrefabEditRequest = serde_json::from_value(json!({"action": "instantiate", "path": PATH, "expectedRevision": scene.revision, "operationId": "prepared-dependency", "prefabPath": "Assets/Base.prefab", "name": "Linked"})).unwrap();
    // Persist a plan then alter a dependency before publication: no source write.
    let publication = store
        .locked(|| {
            let mut planner = PrefabPlanner::new(&store, &request)?;
            planner.build(&request)?;
            Ok(planner.publication(&request.operation_id))
        })
        .unwrap();
    write(
        temp.path(),
        "Assets/Base.prefab.meta",
        "guid: ffffffffffffffffffffffffffffffff\n",
    );
    code(store.publish_staged(&publication), "revision_conflict");
    assert_eq!(store.read(PATH).unwrap().revision, scene.revision);
    let (temp, store, scene) = fixture(SCENE);
    let child = edit(&store, &scene, "child", json!([{"action": "create_gameobject", "key": "child", "name": "Child", "parentId": "100"}])).unwrap();
    let child = &child.mutation.created_ids["child"];
    let content = store.read(PATH).unwrap().content.replacen(
        "m_Name: Child",
        "unknownOutside: {fileID: 100}\n  m_Name: Child",
        1,
    );
    write(temp.path(), PATH, &content);
    let scene = store.read(PATH).unwrap();
    code(
        store.prefab_edit(&serde_json::from_value(json!({"action": "create_prefab", "path": PATH, "expectedRevision": scene.revision, "operationId": "outside-reference", "fileId": child, "outputPath": "Assets/Rejected.prefab"})).unwrap(), None),
        "external_subtree_reference",
    );
    assert!(!temp.path().join("Assets/Rejected.prefab").exists());
    write(
        temp.path(),
        PATH,
        &content.replacen("  unknownOutside: {fileID: 100}\n", "", 1),
    );
    // Existing ordinary descendants still create correctly after losing their parent.
    prefab(
        &store,
        PATH,
        "create-child",
        json!({"action": "create_prefab", "fileId": child, "outputPath": "Assets/Child.prefab"}),
    );
    assert!(temp.path().join("Assets/Child.prefab.meta").exists());
}
#[test]
fn source_prefab_added_components_apply_revert_and_unpack() {
    for action in ["apply", "revert", "partial", "complete"] {
        let (_temp, store, _) = setup();
        prefab(
            &store,
            PATH,
            "variant-for-add",
            json!({"action": "create_variant", "prefabPath": "Assets/Base.prefab", "outputPath": "Assets/Variant.prefab"}),
        );
        prefab(
            &store,
            PATH,
            "instantiate-for-add",
            json!({"action": "instantiate", "prefabPath": "Assets/Variant.prefab", "name": "Linked"}),
        );
        let added = prefab(
            &store,
            PATH,
            "add-light",
            json!({"action": "add_component", "targetPath": "Linked", "componentType": "UnityEngine.Light", "defaultsTemplate": light_template(), "fields": {"m_Intensity": 3}}),
        );
        let id = added.created_ids["UnityEngine.Light"].clone();
        assert!(!id.is_empty());
        assert_eq!(field(&store, PATH, "Linked", "Light", "m_Intensity"), 3);
        prefab(
            &store,
            PATH,
            "edit-added-light",
            json!({"action": "set_override", "targetPath": "Linked", "componentType": "Light", "property": "m_Intensity", "value": 4}),
        );
        match action {
            "apply" => {
                let result = prefab(
                    &store,
                    PATH,
                    "apply-added-light",
                    json!({"action": "apply", "targetPath": "Linked"}),
                );
                assert_eq!(result.publication.assets.len(), 2);
                assert_eq!(
                    field(
                        &store,
                        "Assets/Variant.prefab",
                        "Variant",
                        "Light",
                        "m_Intensity"
                    ),
                    4
                );
                let current = store.read(PATH).unwrap();
                let asset = TextAsset::parse(&current.content).unwrap();
                assert!(asset.find(&id).is_some_and(|doc| doc.stripped()));
                assert!(current.content.contains("m_PrefabInstance:"));
            }
            "revert" => {
                prefab(
                    &store,
                    PATH,
                    "revert-added-light",
                    json!({"action": "revert", "targetPath": "Linked"}),
                );
                assert!(
                    object(&store, PATH, "Linked")
                        .components
                        .iter()
                        .all(|component| component.type_name != "Light")
                );
            }
            _ => {
                prefab(
                    &store,
                    PATH,
                    "unpack-added-light",
                    json!({"action": "unpack", "targetPath": "Linked", "completely": action == "complete"}),
                );
                assert_eq!(field(&store, PATH, "Linked", "Light", "m_Intensity"), 4);
                assert!(
                    action != "complete" || !object(&store, PATH, "Linked").file_id.contains(':')
                );
            }
        }
    }
}
#[test]
fn prefab_document_remap_preserves_unknown_bytes_and_signed_identities() {
    let input = prefab_fixture().replacen(
        "target: {fileID: 0}",
        "target: {fileID: -5824371076177060002}",
        1,
    );
    let asset = TextAsset::parse(&input).unwrap();
    let mapping = HashMap::from(
        [
            ("100", "200"),
            ("101", "201"),
            ("-5824371076177060002", "8107781485188562762"),
        ]
        .map(|(from, to)| (from.to_string(), to.to_string())),
    );
    let copy = clone_prefab_document(&asset.docs[0], &mapping, "", true).unwrap();
    let want = asset.docs[0]
        .body
        .replacen("fileID: 101", "fileID: 201", 1)
        .replacen(
            "fileID: -5824371076177060002",
            "fileID: 8107781485188562762",
            1,
        );
    assert_eq!(copy.body, want);
}
#[test]
fn prefab_modification_serialization_matches_unity_block_subset() {
    let (temp, store, _) = setup();
    prefab(
        &store,
        PATH,
        "serialization-variant",
        json!({"action": "create_variant", "prefabPath": "Assets/Base.prefab", "outputPath": "Assets/Variant.prefab"}),
    );
    prefab(
        &store,
        PATH,
        "serialization-instance",
        json!({"action": "instantiate", "prefabPath": "Assets/Variant.prefab", "name": "Linked"}),
    );
    let variant = store.read("Assets/Variant.prefab").unwrap();
    write(
        temp.path(),
        &variant.path,
        &variant.content.replacen(
            "    m_RemovedComponents:",
            "    unknownFuture: {value: 00123, text: 'a, b'} # untouched\n    m_RemovedComponents:",
            1,
        ),
    );
    prefab(
        &store,
        PATH,
        "serialization-override",
        json!({"action": "set_override", "targetPath": "Linked", "componentType": "BoxCollider", "property": "m_Size", "value": {"x": 3, "y": 7, "z": 9}}),
    );
    prefab(
        &store,
        PATH,
        "serialization-apply",
        json!({"action": "apply", "targetPath": "Linked"}),
    );
    let after = store.read(&variant.path).unwrap().content;
    assert!(
        !after
            .lines()
            .any(|line| line.trim_start().starts_with("[{objectReference"))
    );
    assert!(
        after.contains("m_Modifications:\n      - objectReference: {fileID: 0}"),
        "{after}"
    );
    assert!(after.contains("unknownFuture: {value: 00123, text: 'a, b'} # untouched"));
    assert!(after.contains("propertyPath: m_Size.y") && after.contains("value: \"7\""));
}
#[test]
fn source_prefab_publication_reattaches_current_guard_to_persisted_plan() {
    let (temp, store, source) = setup();
    let calls = Cell::new(0);
    let request: PrefabEditRequest = serde_json::from_value(json!({"action": "instantiate", "path": PATH, "expectedRevision": source.revision, "operationId": "guarded-prefab", "prefabPath": "Assets/Base.prefab", "name": "Guarded"})).unwrap();
    let rejected = store.prefab_edit(
        &request,
        Some(&|| {
            calls.set(calls.get() + 1);
            Err(failure(
                "revision_conflict",
                "native default context changed",
                None,
            ))
        }),
    );
    code(rejected, "revision_conflict");
    assert!(store.read(PATH).unwrap().revision == source.revision && calls.get() == 1);
    let restarted = Store::new(temp.path()).unwrap();
    let result = restarted
        .prefab_edit(
            &request,
            Some(&|| {
                calls.set(calls.get() + 1);
                Ok(())
            }),
        )
        .unwrap();
    assert!(result.publication.changed && calls.get() == 2);
    // Confirmed receipt replay performs no new write, hence no external guard.
    let replay = restarted
        .prefab_edit(
            &request,
            Some(&|| panic!("confirmed replay attempted a source write")),
        )
        .unwrap();
    assert!(replay.publication.replayed);
}
