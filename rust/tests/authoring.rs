use std::{fs, os::unix::fs::PermissionsExt};
use unity_micro_editor::authoring::{ScriptRequest, Store, UndoRequest};
fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("Assets")).unwrap();
    fs::create_dir(temp.path().join("ProjectSettings")).unwrap();
    fs::write(
        temp.path().join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 6000.6.3f1\n",
    )
    .unwrap();
    temp
}
fn create(store: &Store) -> ScriptRequest {
    let request = ScriptRequest {
        path: "Assets/Example.cs".into(),
        expected_revision: "absent".into(),
        operation_id: "create".into(),
        content: "public class Example {}\n".into(),
        guid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
    };
    store.create_script(&request).unwrap();
    request
}
#[test]
fn script_receipts_replay_revision_guards_metadata_and_undo() {
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let request = create(&store);
    assert!(
        Store::new(temp.path())
            .unwrap()
            .create_script(&request)
            .unwrap()
            .replayed
    );
    let original = store.read_script(&request.path).unwrap();
    fs::set_permissions(
        temp.path().join(&request.path),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let mut edit = ScriptRequest {
        expected_revision: original.revision.clone(),
        operation_id: "edit".into(),
        content: "public class Example { public string A = \"<>&\u{2028}\"; }\n".into(),
        guid: String::new(),
        ..request.clone()
    };
    let result = store.edit_script(&edit).unwrap();
    assert!(result.changed);
    assert_eq!(store.read(&edit.path).unwrap().metadata, original.metadata);
    assert_eq!(
        fs::metadata(temp.path().join(&edit.path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert!(store.edit_script(&edit).unwrap().replayed);
    edit.content.push('\n');
    assert_eq!(
        store.edit_script(&edit).unwrap_err().code,
        "idempotency_conflict"
    );
    edit.operation_id = "stale".into();
    assert_eq!(
        store.edit_script(&edit).unwrap_err().code,
        "revision_conflict"
    );
    let undo = UndoRequest {
        path: edit.path.clone(),
        original_operation_id: "edit".into(),
        expected_revision: result.revision,
        operation_id: "undo".into(),
    };
    assert_eq!(
        store.undo_script(&undo).unwrap().revision,
        original.revision
    );
    assert!(store.undo_script(&undo).unwrap().replayed);
    assert_eq!(store.read(&edit.path).unwrap().content, original.content);
    let undo_create = UndoRequest {
        original_operation_id: "create".into(),
        expected_revision: original.revision,
        operation_id: "undo-create".into(),
        ..undo
    };
    assert_eq!(
        store.undo_script(&undo_create).unwrap_err().code,
        "unsupported_undo"
    );
}
#[test]
fn version_identity_paths_and_foreign_changes_are_protected() {
    use std::os::unix::fs::symlink;
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let mut request = create(&store);
    request.guid.clear();
    request.expected_revision = store.read(&request.path).unwrap().revision;
    request.operation_id = "edit".into();
    fs::write(
        temp.path().join(format!("{}.meta", request.path)),
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    )
    .unwrap();
    assert_eq!(
        store.edit_script(&request).unwrap_err().code,
        "revision_conflict"
    );
    fs::write(
        temp.path().join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 6000.7.0f1\n",
    )
    .unwrap();
    assert!(!store.inspect().unwrap().version_matches);
    assert_eq!(
        store.edit_script(&request).unwrap_err().code,
        "unsupported_unity_version"
    );
    assert_eq!(
        store.read("Assets/../Example.cs").unwrap_err().code,
        "invalid_path"
    );
    symlink(temp.path(), temp.path().join("Assets/escape")).unwrap();
    assert_eq!(
        store
            .read("Assets/escape/Assets/Example.cs")
            .unwrap_err()
            .code,
        "invalid_path"
    );
}
#[test]
fn recovery_handles_partial_create_full_commit_and_external_bytes() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let request = create(&store);
    let receipt_path = temp.path().join(format!(
        "Library/UnityMicroEditor/authoring/{}.json",
        unity_micro_editor::sha256(b"create")
    ));
    let committed: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    let mut prepared = committed.clone();
    prepared["state"] = json!("prepared");
    // A typed multi-file receipt survives recovery unchanged.
    prepared["batchResult"] = json!({"operationId":"create","assets":[],"changed":false});
    fs::write(&receipt_path, serde_json::to_vec(&prepared).unwrap()).unwrap();
    assert!(store.create_script(&request).unwrap().replayed);
    let completed: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(completed["batchResult"], prepared["batchResult"]);
    fs::write(&receipt_path, serde_json::to_vec(&prepared).unwrap()).unwrap();
    fs::remove_file(temp.path().join("Assets/Example.cs.meta")).unwrap();
    assert_eq!(store.read(&request.path).unwrap_err().code, "not_found");
    assert!(!temp.path().join(&request.path).exists());
    assert_eq!(
        store.create_script(&request).unwrap_err().code,
        "rolled_back"
    );
    fs::write(temp.path().join(&request.path), "foreign").unwrap();
    fs::write(&receipt_path, serde_json::to_vec(&prepared).unwrap()).unwrap();
    assert_eq!(
        store.read(&request.path).unwrap_err().code,
        "outcome_unknown"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join(&request.path)).unwrap(),
        "foreign"
    );
    assert_eq!(
        STANDARD
            .decode(committed["files"][0]["after"].as_str().unwrap())
            .unwrap(),
        request.content.as_bytes()
    );
}
#[test]
fn yaml_edits_preserve_unicode_crlf_comments_and_unrelated_documents() {
    use serde_json::json;
    use unity_micro_editor::authoring::EditRequest;
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let content = "%YAML 1.1\r\n--- !u!21 &2100000\r\nMaterial:\r\n  m_Name: \"Медь\" # keep this comment\r\n  m_Custom: {label: \"Ёж\", value: 1, sibling: 77} # untouched\r\n  m_Items:\r\n    - 10\r\n    - 20\r\n  # belongs to following field\r\n  m_Flag: 0\r\n  m_Reference: {fileID: 0}\r\n";
    let request = ScriptRequest {
        path: "Assets/Copper.mat".into(),
        expected_revision: "absent".into(),
        operation_id: "create-material".into(),
        content: content.into(),
        guid: "11111111111111111111111111111111".into(),
    };
    let created = store.create_asset(&request).unwrap();
    let edit:EditRequest=serde_json::from_value(json!({"path":request.path,"expectedRevision":created.revision,"operationId":"edit-material","preview":true,"operations":[
        {"action":"set_property","fileId":"2100000","property":"m_Name","value":"Латунь"},
        {"action":"set_property","fileId":"2100000","property":"m_Custom.value","value":1.5},
        {"action":"set_property","fileId":"2100000","property":"m_Items","value":[2,3,4]},
        {"action":"set_property","fileId":"2100000","property":"m_Flag","value":true},
        {"action":"set_property","fileId":"2100000","property":"m_Reference","value":{"fileID":"0","guid":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","type":3}}
    ]})).unwrap();
    let preview = store.edit(&edit).unwrap();
    assert!(preview.preview);
    assert!(
        preview
            .content
            .contains("m_Name: \"Латунь\" # keep this comment\r\n")
    );
    assert!(
        preview
            .content
            .contains("{label: \"Ёж\", value: 1.5, sibling: 77} # untouched\r\n")
    );
    assert!(
        preview
            .content
            .contains("m_Items:\r\n    [2, 3, 4]\r\n  # belongs to following field"),
        "{}",
        preview.content
    );
    assert!(preview.content.contains("m_Flag: 1\r\n"));
    assert!(
        preview
            .content
            .contains("guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(store.read(&edit.path).unwrap().content, content);
    let committed = store
        .edit(&EditRequest {
            preview: false,
            ..edit.clone()
        })
        .unwrap();
    assert_eq!(committed.mutation.revision, preview.mutation.revision);
    assert_eq!(
        store.read(&edit.path).unwrap().metadata,
        store.read(&request.path).unwrap().metadata
    );
    let undo = UndoRequest {
        path: edit.path,
        expected_revision: committed.mutation.revision,
        operation_id: "undo-material".into(),
        original_operation_id: edit.operation_id,
    };
    assert_eq!(store.undo(&undo).unwrap().revision, created.revision);
    assert_eq!(store.read(&request.path).unwrap().content, content);
    // A flow replacement of an indentless sequence is rejected: the YAML
    // reader cannot reparse a flow collection at its key's column.
    let indentless = store.read(&request.path).unwrap();
    fs::write(
        temp.path().join(&request.path),
        indentless.content.replace("    - ", "  - "),
    )
    .unwrap();
    let indentless = store.read(&request.path).unwrap();
    let rejected: EditRequest = serde_json::from_value(json!({"path":request.path,"expectedRevision":indentless.revision,"operationId":"edit-indentless","preview":true,"operations":[
        {"action":"set_property","fileId":"2100000","property":"m_Items","value":[2,3,4]}
    ]})).unwrap();
    assert_eq!(store.edit(&rejected).unwrap_err().code, "invalid_yaml");
}
#[test]
fn yaml_protects_identity_prefab_spans_and_dangling_references() {
    use serde_json::json;
    use unity_micro_editor::authoring::EditRequest;
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let request = ScriptRequest {
        path: "Assets/Test.mat".into(),
        expected_revision: "absent".into(),
        operation_id: "create-material".into(),
        content:
            "--- !u!21 &2100000\nMaterial:\n  m_Name: Test\n  m_Link: {fileID: 0}\n  m_Number: 7\n"
                .into(),
        guid: "11111111111111111111111111111111".into(),
    };
    let created = store.create_asset(&request).unwrap();
    for (property, value, expected) in [
        ("m_Link", json!({"fileID":"99"}), "dangling_reference"),
        ("m_Link.fileID", json!("1"), "protected_property"),
        ("m_Number", json!("text"), "invalid_property_value"),
        ("absent", json!(1), "unsupported_property"),
        ("m_Link", json!({"fileID":99}), "invalid_identity"),
    ] {
        let edit:EditRequest=serde_json::from_value(json!({"path":request.path,"expectedRevision":created.revision,"operationId":"invalid","operations":[{"action":"set_property","fileId":"2100000","property":property,"value":value}]})).unwrap();
        assert_eq!(store.edit(&edit).unwrap_err().code, expected);
        assert_eq!(
            store.read(&request.path).unwrap().revision,
            created.revision
        );
    }
}
#[test]
fn racing_writers_cannot_overwrite_each_other() {
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let mut request = create(&store);
    request.expected_revision = store.read(&request.path).unwrap().revision;
    request.guid.clear();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for i in 0..2 {
        let store = store.clone();
        let mut request = request.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            request.operation_id = format!("writer-{i}");
            request.content = format!("public class Example {{ public int Value = {i}; }}");
            barrier.wait();
            store.edit_script(&request)
        }));
    }
    barrier.wait();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .next()
            .unwrap()
            .code,
        "revision_conflict"
    );
}
#[test]
fn staged_publication_checks_all_dependencies_before_writes_and_undoes_as_one_receipt() {
    use unity_micro_editor::authoring::{
        PublicationInput, PublicationOutput, PublicationUndoRequest, StagedPublicationRequest,
    };
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    create(&store);
    let second = ScriptRequest {
        path: "Assets/Second.cs".into(),
        expected_revision: "absent".into(),
        operation_id: "create-second".into(),
        content: "public class Second {}\n".into(),
        guid: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
    };
    store.create_script(&second).unwrap();
    let original = store.read("Assets/Example.cs").unwrap();
    let second = store.read("Assets/Second.cs").unwrap();
    let version = store
        .dependency_revision("ProjectSettings/ProjectVersion.txt")
        .unwrap();
    assert_eq!(
        store.dependency_revision(&original.path).unwrap(),
        original.revision
    );
    let request = StagedPublicationRequest {
        operation_id: "publish".into(),
        inputs: vec![
            PublicationInput {
                path: original.path.clone(),
                expected_revision: original.revision.clone(),
            },
            PublicationInput {
                path: second.path.clone(),
                expected_revision: second.revision.clone(),
            },
            PublicationInput {
                path: "ProjectSettings/ProjectVersion.txt".into(),
                expected_revision: version,
            },
        ],
        outputs: vec![
            PublicationOutput {
                path: original.path.clone(),
                content: "public class Example { public int value=1; }\n".into(),
                metadata: None,
            },
            PublicationOutput {
                path: second.path.clone(),
                content: "public class Second { public int value=2; }\n".into(),
                metadata: None,
            },
        ],
    };
    let mut invalid = request.clone();
    invalid.inputs[1].expected_revision = "stale".into();
    assert_eq!(
        store.publish_staged(&invalid).unwrap_err().code,
        "revision_conflict"
    );
    assert_eq!(
        store.read(&original.path).unwrap().revision,
        original.revision
    );
    invalid = request.clone();
    invalid.outputs[1].metadata = Some("guid: cccccccccccccccccccccccccccccccc\n".into());
    assert_eq!(
        store.publish_staged(&invalid).unwrap_err().code,
        "protected_property"
    );
    assert_eq!(
        store.read(&original.path).unwrap().revision,
        original.revision
    );
    let result = store.publish_staged(&request).unwrap();
    assert!(result.changed);
    assert_eq!(result.assets.len(), 2);
    assert!(
        Store::new(temp.path())
            .unwrap()
            .publish_staged(&request)
            .unwrap()
            .replayed
    );
    assert_eq!(
        store.read(&original.path).unwrap().metadata,
        original.metadata
    );
    assert_eq!(store.read(&second.path).unwrap().metadata, second.metadata);
    let undo = PublicationUndoRequest {
        operation_id: "undo-publish".into(),
        original_operation_id: "publish".into(),
        expected_revisions: result
            .assets
            .into_iter()
            .map(|asset| (asset.path, asset.revision))
            .collect(),
    };
    store.undo_publication(&undo).unwrap();
    assert!(store.undo_publication(&undo).unwrap().replayed);
    assert_eq!(
        store.read(&original.path).unwrap().revision,
        original.revision
    );
    assert_eq!(store.read(&second.path).unwrap().revision, second.revision);
    let guarded = StagedPublicationRequest {
        operation_id: "guarded-publish".into(),
        ..request
    };
    assert_eq!(
        store
            .publish_staged_with_guard(&guarded, &|| {
                fs::write(
                    temp.path().join("ProjectSettings/ProjectVersion.txt"),
                    "m_EditorVersion: 6000.6.3f1\n# external change\n",
                )?;
                Ok(())
            })
            .unwrap_err()
            .code,
        "revision_conflict"
    );
    assert_eq!(
        store.read(&original.path).unwrap().revision,
        original.revision
    );
}
#[test]
fn publication_recovers_partial_existing_file_writes_and_preserves_foreign_bytes() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    use unity_micro_editor::authoring::{
        PublicationInput, PublicationOutput, StagedPublicationRequest,
    };
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    create(&store);
    let other = ScriptRequest {
        path: "Assets/Second.cs".into(),
        expected_revision: "absent".into(),
        operation_id: "create-second".into(),
        content: "public class Second {}\n".into(),
        guid: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
    };
    store.create_script(&other).unwrap();
    let first = store.read("Assets/Example.cs").unwrap();
    let second = store.read("Assets/Second.cs").unwrap();
    let request = StagedPublicationRequest {
        operation_id: "batch".into(),
        inputs: vec![
            PublicationInput {
                path: first.path.clone(),
                expected_revision: first.revision.clone(),
            },
            PublicationInput {
                path: second.path.clone(),
                expected_revision: second.revision.clone(),
            },
        ],
        outputs: vec![
            PublicationOutput {
                path: first.path.clone(),
                content: "public class Example { int v=1; }\n".into(),
                metadata: None,
            },
            PublicationOutput {
                path: second.path.clone(),
                content: "public class Second { int v=2; }\n".into(),
                metadata: None,
            },
        ],
    };
    store.publish_staged(&request).unwrap();
    let journal = temp.path().join(format!(
        "Library/UnityMicroEditor/authoring/{}.json",
        unity_micro_editor::sha256(b"batch")
    ));
    let mut receipt: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    receipt["state"] = json!("prepared");
    fs::write(&journal, serde_json::to_vec(&receipt).unwrap()).unwrap();
    fs::write(
        temp.path().join(&second.path),
        STANDARD
            .decode(receipt["files"][1]["before"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(store.read(&first.path).unwrap().content, first.content);
    assert_eq!(store.read(&second.path).unwrap().content, second.content);
    assert_eq!(
        store.publish_staged(&request).unwrap_err().code,
        "rolled_back"
    );
    fs::write(&journal, serde_json::to_vec(&receipt).unwrap()).unwrap();
    fs::write(temp.path().join(&second.path), "external bytes").unwrap();
    assert_eq!(store.read(&first.path).unwrap_err().code, "outcome_unknown");
    assert_eq!(
        fs::read_to_string(temp.path().join(&second.path)).unwrap(),
        "external bytes"
    );
}
#[test]
fn native_templates_are_revision_bound_and_local_references_require_explicit_replacement() {
    use serde_json::json;
    use unity_micro_editor::authoring::EditRequest;
    let temp = fixture();
    let store = Store::new(temp.path()).unwrap();
    let scene = "--- !u!1 &10\nGameObject:\n  m_Component:\n  - component: {fileID: 11}\n  - component: {fileID: 12}\n  m_Name: Root\n  m_IsActive: 1\n--- !u!4 &11\nTransform:\n  m_GameObject: {fileID: 10}\n  m_Father: {fileID: 0}\n  m_Children: []\n--- !u!54 &12\nRigidbody:\n  m_GameObject: {fileID: 10}\n  m_Link: {fileID: 10}\n  m_Mass: 1\n";
    fs::write(temp.path().join("Assets/Template.unity"), scene).unwrap();
    fs::write(
        temp.path().join("Assets/Template.unity.meta"),
        "guid: 11111111111111111111111111111111\n",
    )
    .unwrap();
    let template = store.read("Assets/Template.unity").unwrap();
    let target = scene
        .replace("  - component: {fileID: 12}\n", "")
        .split("--- !u!54")
        .next()
        .unwrap()
        .to_owned();
    fs::write(temp.path().join("Assets/Target.unity"), target).unwrap();
    fs::write(
        temp.path().join("Assets/Target.unity.meta"),
        "guid: 22222222222222222222222222222222\n",
    )
    .unwrap();
    let revision = store.read("Assets/Target.unity").unwrap().revision;
    let request = json!({"path":"Assets/Target.unity","expectedRevision":revision,"operationId":"native","operations":[{"action":"add_component","fileId":"10","componentType":"Rigidbody","templatePath":"Assets/Template.unity","templateFileId":"12","templateRevision":template.revision}]});
    let mut edit: EditRequest = serde_json::from_value(request).unwrap();
    assert_eq!(
        store.edit(&edit).unwrap_err().code,
        "template_local_reference"
    );
    edit.operations[0]
        .fields
        .insert("m_Link".into(), json!({"fileID":"0"}));
    edit.operations[0].template_revision = "stale".into();
    assert_eq!(store.edit(&edit).unwrap_err().code, "revision_conflict");
    edit.operations[0].template_revision = template.revision;
    let result = store.edit(&edit).unwrap();
    assert!(result.mutation.changed);
    assert!(
        store
            .read(&edit.path)
            .unwrap()
            .content
            .contains("m_Link:\n    fileID: 0\n")
    );
    assert_eq!(
        store.read(&edit.path).unwrap().metadata,
        "guid: 22222222222222222222222222222222\n"
    );
}
