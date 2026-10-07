package authoring

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

const sceneFixture = `%YAML 1.1
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
  m_Layer: 0
  m_Name: Original # keep my comment
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
`
const scenePath = "Assets/Scenes/Main.unity"

func fixture(t *testing.T, scene string) (*Store, Source) {
	t.Helper()
	root := t.TempDir()
	for _, dir := range []string{"Assets/Scenes", "Assets/Scripts", "ProjectSettings"} {
		if err := os.MkdirAll(filepath.Join(root, dir), 0755); err != nil {
			t.Fatal(err)
		}
	}
	write(t, filepath.Join(root, "ProjectSettings/ProjectVersion.txt"), "m_EditorVersion: 6000.6.3f1\n")
	write(t, filepath.Join(root, scenePath), scene)
	write(t, filepath.Join(root, scenePath)+".meta", "fileFormatVersion: 2\nguid: 12345678901234567890123456789012\nDefaultImporter:\n  externalObjects: {}\n")
	s, err := NewStore(root)
	if err != nil {
		t.Fatal(err)
	}
	source, err := s.Read(context.Background(), scenePath)
	if err != nil {
		t.Fatal(err)
	}
	return s, source
}
func write(t *testing.T, path, text string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(text), 0644); err != nil {
		t.Fatal(err)
	}
}
func requireCode(t *testing.T, err error, code string) {
	t.Helper()
	var typed *Error
	if !errors.As(err, &typed) || typed.Code != code {
		t.Fatalf("got %v, want %s", err, code)
	}
}
func editRequest(source Source, id string, operations ...Operation) EditRequest {
	return EditRequest{Path: source.Path, ExpectedRevision: source.Revision, OperationID: id, Operations: operations}
}

func TestLeafEditsPreserveEveryOtherByteAndMetadata(t *testing.T) {
	for _, newline := range []string{"\n", "\r\n"} {
		t.Run(strings.ReplaceAll(newline, "\n", "LF"), func(t *testing.T) {
			original := strings.ReplaceAll(sceneFixture, "\n", newline)
			s, source := fixture(t, original)
			request := editRequest(source, "leaf-edit", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "Changed, with Unicode Я"}, Operation{Action: "set_property", FileID: "101", Property: "m_LocalPosition.y", Value: json.Number("2.5")})
			result, err := s.Edit(context.Background(), request)
			if err != nil {
				t.Fatal(err)
			}
			after, err := s.Read(context.Background(), source.Path)
			if err != nil {
				t.Fatal(err)
			}
			want := strings.Replace(original, "m_Name: Original", `m_Name: "Changed, with Unicode Я"`, 1)
			want = strings.Replace(want, "{x: 0, y: 1, z: 0}", "{x: 0, y: 2.5, z: 0}", 1)
			if after.Content != want {
				t.Fatalf("unrelated YAML bytes changed:\n%s\nwant:\n%s", after.Content, want)
			}
			if after.Metadata != source.Metadata || after.GUID != source.GUID {
				t.Fatal("metadata changed")
			}
			if result.Revision == source.Revision || !result.Changed {
				t.Fatal("revision did not change")
			}
		})
	}
}
func TestGraphTransactionAndRetryAfterRestart(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	request := editRequest(source, "graph-create", Operation{Action: "create_gameobject", Key: "child", Name: "Child", ParentID: "100"}, Operation{Action: "set_property", FileID: "$child.transform", Property: "m_LocalPosition.x", Value: json.Number("3")}, Operation{Action: "create_gameobject", Key: "root", Name: "Another root"})
	result, err := s.Edit(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	restarted, err := NewStore(s.Root())
	if err != nil {
		t.Fatal(err)
	}
	replay, err := restarted.Edit(context.Background(), request)
	if err != nil || !replay.Replayed || replay.Revision != result.Revision {
		t.Fatalf("retry: %+v %v", replay, err)
	}
	h, err := s.Hierarchy(context.Background(), scenePath, "")
	if err != nil {
		t.Fatal(err)
	}
	if len(h.Objects) != 3 || len(h.Roots) != 2 {
		t.Fatalf("hierarchy: %+v", h)
	}
	for _, object := range h.Objects {
		if object.FileID == result.CreatedIDs["child"] && object.ParentID != "100" {
			t.Fatal("child ownership wrong")
		}
	}
	latest, err := s.Read(context.Background(), scenePath)
	if err != nil {
		t.Fatal(err)
	}
	_, err = s.Edit(context.Background(), editRequest(latest, "reparent", Operation{Action: "reparent", FileID: result.CreatedIDs["child"], ParentID: result.CreatedIDs["root"]}))
	if err != nil {
		t.Fatal(err)
	}
	latest, _ = s.Read(context.Background(), scenePath)
	_, err = s.Edit(context.Background(), editRequest(latest, "cycle", Operation{Action: "reparent", FileID: result.CreatedIDs["root"], ParentID: result.CreatedIDs["child"]}))
	requireCode(t, err, "transform_cycle")
	_, err = s.Edit(context.Background(), editRequest(latest, "delete-parent", Operation{Action: "delete_gameobject", FileID: result.CreatedIDs["root"]}))
	requireCode(t, err, "has_children")
	_, err = s.Edit(context.Background(), editRequest(latest, "delete-tree", Operation{Action: "delete_gameobject", FileID: result.CreatedIDs["root"], Recursive: true}))
	if err != nil {
		t.Fatal(err)
	}
	h, err = s.Hierarchy(context.Background(), scenePath, "")
	if err != nil || len(h.Objects) != 1 || len(h.Roots) != 1 {
		t.Fatalf("deletion: %+v %v", h, err)
	}
}
func TestPreviewNeverWritesAndStaleOrReusedIDsFail(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	request := editRequest(source, "preview", Operation{Action: "create_gameobject", Name: "Preview"})
	request.Preview = true
	preview, err := s.Edit(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	if !preview.Preview || !preview.Changed || preview.Content == source.Content {
		t.Fatal("missing preview")
	}
	after, _ := s.Read(context.Background(), scenePath)
	if after.Revision != source.Revision {
		t.Fatal("preview wrote source")
	}
	request.Preview = false
	_, err = s.Edit(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	_, err = s.Edit(context.Background(), editRequest(source, "stale", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "stale"}))
	requireCode(t, err, "revision_conflict")
	request.Operations[0].Name = "Different"
	_, err = s.Edit(context.Background(), request)
	requireCode(t, err, "idempotency_conflict")
}
func TestConcurrentWritersAndMetaRevisionConflict(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	another, err := NewStore(s.Root())
	if err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	results := make(chan error, 2)
	for i, store := range []*Store{s, another} {
		wg.Add(1)
		go func(index int, store *Store) {
			defer wg.Done()
			_, err := store.Edit(context.Background(), editRequest(source, string(rune('a'+index)), Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "writer"}))
			results <- err
		}(i, store)
	}
	wg.Wait()
	close(results)
	success, conflicts := 0, 0
	for err := range results {
		if err == nil {
			success++
		} else {
			requireCode(t, err, "revision_conflict")
			conflicts++
		}
	}
	if success != 1 || conflicts != 1 {
		t.Fatalf("success=%d conflict=%d", success, conflicts)
	}
	latest, _ := s.Read(context.Background(), scenePath)
	write(t, filepath.Join(s.Root(), scenePath)+".meta", latest.Metadata+"# external metadata change\n")
	_, err = s.Edit(context.Background(), editRequest(latest, "stale-meta", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "stale"}))
	requireCode(t, err, "revision_conflict")
}
func TestProtectedFieldsPrefabInstancesAndDanglingReferences(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	_, err := s.Edit(context.Background(), editRequest(source, "identity", Operation{Action: "set_property", FileID: "101", Property: "m_GameObject.fileID", Value: "900"}))
	requireCode(t, err, "protected_property")
	_, err = s.Edit(context.Background(), editRequest(source, "dangling", Operation{Action: "set_property", FileID: "100", Property: "unknownFuture", Value: map[string]any{"target": map[string]any{"fileID": "999"}}}))
	requireCode(t, err, "dangling_reference")
	instance := strings.Replace(sceneFixture, "m_PrefabInstance: {fileID: 0}", "m_PrefabInstance: {fileID: 555}", 1) + "--- !u!1001 &555\nPrefabInstance:\n  m_SourcePrefab: {fileID: 100100000, guid: 99999999999999999999999999999999, type: 3}\n"
	store, prefabSource := fixture(t, instance)
	_, err = store.Edit(context.Background(), editRequest(prefabSource, "prefab-write", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "forbidden"}))
	requireCode(t, err, "unsupported_prefab_write")
}
func TestScriptCreationAttachmentEditingAndGUIDPreservation(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	request := ScriptRequest{Path: "Assets/Scripts/Example.cs", ExpectedRevision: "absent", OperationID: "create-script", Content: "using UnityEngine; public class Example : MonoBehaviour { public int amount = 2; }\n", GUID: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
	scriptResult, err := s.CreateScript(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	script, err := s.ReadScript(context.Background(), request.Path)
	if err != nil {
		t.Fatal(err)
	}
	if script.GUID != request.GUID || script.Revision != scriptResult.Revision {
		t.Fatal("script identity")
	}
	result, err := s.Edit(context.Background(), editRequest(source, "attach", Operation{Action: "add_component", FileID: "100", Key: "example", ScriptPath: script.Path, Fields: map[string]any{"amount": json.Number("7")}}))
	if err != nil {
		t.Fatal(err)
	}
	h, err := s.Hierarchy(context.Background(), scenePath, "")
	if err != nil || len(h.Objects[0].Components) != 2 {
		t.Fatalf("attachment: %+v %v", h, err)
	}
	edit := ScriptRequest{Path: script.Path, ExpectedRevision: script.Revision, OperationID: "edit-script", Content: strings.Replace(script.Content, "= 2", "= 3", 1)}
	_, err = s.EditScript(context.Background(), edit)
	if err != nil {
		t.Fatal(err)
	}
	updated, _ := s.ReadScript(context.Background(), script.Path)
	if updated.Metadata != script.Metadata {
		t.Fatal("script metadata changed")
	}
	latest, _ := s.Read(context.Background(), scenePath)
	_, err = s.Edit(context.Background(), editRequest(latest, "detach", Operation{Action: "remove_component", FileID: result.CreatedIDs["example"]}))
	if err != nil {
		t.Fatal(err)
	}
	request.Path = "Assets/Scripts/Other.cs"
	request.OperationID = "duplicate-script"
	_, err = s.CreateScript(context.Background(), request)
	requireCode(t, err, "duplicate_guid")
}
func TestExternalReferencesPreventDeletion(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	write(t, filepath.Join(s.Root(), "Assets/Scenes/Other.unity"), strings.Replace(sceneFixture, "unknownFuture: {value: 00123, label: 'a, b', target: {fileID: 0}}", "unknownFuture: {target: {fileID: 100, guid: "+source.GUID+", type: 3}}", 1))
	write(t, filepath.Join(s.Root(), "Assets/Scenes/Other.unity.meta"), "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n")
	_, err := s.Edit(context.Background(), editRequest(source, "delete-referenced", Operation{Action: "delete_gameobject", FileID: "100", Recursive: true}))
	requireCode(t, err, "referenced_object")
}
func TestRejectTraversalSymlinksAndUnverifiedVersion(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	for _, path := range []string{"Assets/../ProjectSettings/ProjectVersion.txt", "/tmp/a.cs", "Assets\\a.cs", "Library/a.cs"} {
		_, err := s.Read(context.Background(), path)
		requireCode(t, err, "invalid_path")
	}
	if err := os.Symlink(t.TempDir(), filepath.Join(s.Root(), "Assets/Linked")); err != nil {
		t.Fatal(err)
	}
	_, err := s.ReadScript(context.Background(), "Assets/Linked/A.cs")
	requireCode(t, err, "invalid_path")
	write(t, filepath.Join(s.Root(), "ProjectSettings/ProjectVersion.txt"), "m_EditorVersion: 6001.0.0f1\n")
	store, err := NewStore(s.Root())
	if err != nil {
		t.Fatal(err)
	}
	_, err = store.Edit(context.Background(), editRequest(source, "wrong-version", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "new"}))
	requireCode(t, err, "unsupported_unity_version")
}
func TestInterruptedTransactionRecoveryAndExternalConflict(t *testing.T) {
	for _, external := range []bool{false, true} {
		t.Run(map[bool]string{false: "rollback", true: "conflict"}[external], func(t *testing.T) {
			s, _ := fixture(t, sceneFixture)
			path := "Assets/Scripts/Interrupted.cs"
			metaPath := path + ".meta"
			j := journal{OperationID: "interrupted", Fingerprint: "test", State: "prepared", Files: []journalFile{{Path: path, After: []byte("new source"), Mode: 0644}, {Path: metaPath, After: []byte("new metadata"), Mode: 0644}}}
			if err := writeJournal(filepath.Join(s.state, "interrupted.json"), j); err != nil {
				t.Fatal(err)
			}
			content := "new source"
			if external {
				content = "external writer"
			}
			write(t, filepath.Join(s.Root(), path), content)
			_, err := s.Read(context.Background(), scenePath)
			if external {
				requireCode(t, err, "outcome_unknown")
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if _, err := os.Stat(filepath.Join(s.Root(), path)); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("partial file not rolled back")
			}
			data, err := os.ReadFile(filepath.Join(s.state, "interrupted.json"))
			if err != nil {
				t.Fatal(err)
			}
			var recovered journal
			if err := json.Unmarshal(data, &recovered); err != nil {
				t.Fatal(err)
			}
			if recovered.State != "rolled_back" {
				t.Fatal("journal not rolled back")
			}
		})
	}
}

func TestUndoPreservesOriginalBytesAndRefusesInterveningWrites(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	result, err := s.Edit(context.Background(), editRequest(source, "to-undo", Operation{Action: "create_gameobject", Key: "added", Name: "Added"}, Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "Edit"}))
	if err != nil {
		t.Fatal(err)
	}
	request := UndoRequest{Path: scenePath, OriginalOperationID: "to-undo", ExpectedRevision: result.Revision, OperationID: "undo"}
	undo, err := s.Undo(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	if undo.Revision != source.Revision {
		t.Fatal("undo did not restore original revision")
	}
	updated, err := s.Read(context.Background(), scenePath)
	if err != nil || updated.Content != sceneFixture || updated.Metadata != source.Metadata {
		t.Fatalf("undo changed original bytes: %v", err)
	}
	replay, err := s.Undo(context.Background(), request)
	if err != nil || !replay.Replayed {
		t.Fatalf("undo replay %+v %v", replay, err)
	}
	first, err := s.Edit(context.Background(), editRequest(updated, "first", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "First"}))
	if err != nil {
		t.Fatal(err)
	}
	latest, _ := s.Read(context.Background(), scenePath)
	second, err := s.Edit(context.Background(), editRequest(latest, "second", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "Second"}))
	if err != nil {
		t.Fatal(err)
	}
	_, err = s.Undo(context.Background(), UndoRequest{Path: scenePath, OriginalOperationID: first.OperationID, ExpectedRevision: second.Revision, OperationID: "unsafe-undo"})
	requireCode(t, err, "revision_conflict")
}
func TestUnicodeFlowLeafAndPlainCommaString(t *testing.T) {
	fixtureText := strings.Replace(sceneFixture, "label: 'a, b'", "label: Я, other: 'unchanged'", 1)
	s, source := fixture(t, fixtureText)
	_, err := s.Edit(context.Background(), editRequest(source, "unicode-flow", Operation{Action: "set_property", FileID: "100", Property: "unknownFuture.other", Value: "Edited"}, Operation{Action: "set_property", FileID: "100", Property: "unknownFuture.label", Value: "New"}))
	if err != nil {
		t.Fatal(err)
	}
	updated, _ := s.Read(context.Background(), scenePath)
	if !strings.Contains(updated.Content, `label: "New", other: "Edited"`) {
		t.Fatal(updated.Content)
	}
	text := strings.Replace(sceneFixture, "m_Name: Original # keep my comment", "m_Name: Original, name # keep my comment", 1)
	s, source = fixture(t, text)
	_, err = s.Edit(context.Background(), editRequest(source, "plain-comma", Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "New"}))
	if err != nil {
		t.Fatal(err)
	}
	updated, _ = s.Read(context.Background(), scenePath)
	if updated.Content != strings.Replace(text, "m_Name: Original, name", `m_Name: "New"`, 1) {
		t.Fatal("plain comma string was damaged")
	}
}
func TestReferenceNumbersAndPrefabExtraRootsAreRejected(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	_, err := s.Edit(context.Background(), editRequest(source, "numeric-id", Operation{Action: "set_property", FileID: "100", Property: "unknownFuture", Value: map[string]any{"target": map[string]any{"fileID": float64(9007199254740992)}}}))
	requireCode(t, err, "invalid_identity")
	prefabPath := "Assets/Scenes/Example.prefab"
	write(t, filepath.Join(s.Root(), prefabPath), strings.Split(sceneFixture, "--- !u!1660057539")[0])
	write(t, filepath.Join(s.Root(), prefabPath)+".meta", "guid: cccccccccccccccccccccccccccccccc\n")
	prefab, err := s.Read(context.Background(), prefabPath)
	if err != nil {
		t.Fatal(err)
	}
	_, err = s.Edit(context.Background(), editRequest(prefab, "extra-root", Operation{Action: "create_gameobject", Name: "Extra root"}))
	requireCode(t, err, "invalid_prefab_root")
}

func TestPlainComponentEditsPreserveStrippedPrefabChild(t *testing.T) {
	prefabRecords := `--- !u!1001 &555
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
`
	s, source := fixture(t, strings.Replace(sceneFixture, "m_Children: []", "m_Children:\n  - {fileID: 556}", 1)+prefabRecords)
	_, err := s.CreateScript(context.Background(), ScriptRequest{Path: "Assets/Scripts/Ordinary.cs", ExpectedRevision: "absent", OperationID: "ordinary-script", Content: "using UnityEngine; public class Ordinary : MonoBehaviour { public int counter; }\n"})
	if err != nil {
		t.Fatal(err)
	}
	result, err := s.Edit(context.Background(), editRequest(source, "ordinary-component", Operation{Action: "add_component", FileID: "100", Key: "ordinary", ScriptPath: "Assets/Scripts/Ordinary.cs", Fields: map[string]any{"counter": 7}}, Operation{Action: "set_property", FileID: "100", Property: "m_Name", Value: "Changed parent"}))
	if err != nil {
		t.Fatal(err)
	}
	updated, err := s.Read(context.Background(), scenePath)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(updated.Content, prefabRecords) {
		t.Fatal("prefab records changed")
	}
	if !strings.Contains(updated.Content, "m_Children:\n  - {fileID: 556}") {
		t.Fatal("stripped child link changed")
	}
	_, err = s.Edit(context.Background(), editRequest(updated, "ordinary-field", Operation{Action: "set_property", FileID: result.CreatedIDs["ordinary"], Property: "counter", Value: 9}))
	if err != nil {
		t.Fatal(err)
	}
	latest, _ := s.Read(context.Background(), scenePath)
	_, err = s.Edit(context.Background(), editRequest(latest, "reparent-prefab-tree", Operation{Action: "reparent", FileID: "100", ParentID: "0"}))
	requireCode(t, err, "unsupported_prefab_write")
	_, err = s.Edit(context.Background(), editRequest(latest, "delete-prefab-tree", Operation{Action: "delete_gameobject", FileID: "100", Recursive: true}))
	requireCode(t, err, "unsupported_prefab_write")
	_, err = s.Edit(context.Background(), editRequest(latest, "edit-stripped", Operation{Action: "set_property", FileID: "556", Property: "m_LocalPosition.x", Value: 1}))
	requireCode(t, err, "unsupported_prefab_write")
}

func TestUnityBooleanPropertiesAcceptJSONBooleans(t *testing.T) {
	s, source := fixture(t, sceneFixture)
	_, err := s.Edit(context.Background(), editRequest(source, "deactivate", Operation{Action: "set_property", FileID: "100", Property: "m_IsActive", Value: false}))
	if err != nil {
		t.Fatal(err)
	}
	h, err := s.Hierarchy(context.Background(), scenePath, "")
	if err != nil {
		t.Fatal(err)
	}
	if h.Objects[0].Active {
		t.Fatal("active state did not change")
	}
	updated, _ := s.Read(context.Background(), scenePath)
	if updated.Content != strings.Replace(sceneFixture, "m_IsActive: 1", "m_IsActive: 0", 1) {
		t.Fatal("boolean serialization damaged bytes")
	}
}
