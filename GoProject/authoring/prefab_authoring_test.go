package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"testing"
)

func sourcePrefabAction(t *testing.T, store *Store, path, id string, request PrefabEditRequest) PrefabEditResult {
	t.Helper()
	source, err := store.Read(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	request.Path, request.OperationID, request.ExpectedRevision = path, id, source.Revision
	result, err := store.PrefabEdit(context.Background(), request)
	if err != nil {
		t.Fatalf("%s: %v", id, err)
	}
	if result.Backend != "raw_source" {
		t.Fatalf("unexpected backend %+v", result)
	}
	return result
}
func sourcePrefabObjects(t *testing.T, store *Store, path string) ResolvedHierarchyPage {
	t.Helper()
	result, err := store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: path, IncludeFields: func() *bool { v := true; return &v }()})
	if err != nil {
		t.Fatal(err)
	}
	return result
}
func sourcePrefabObject(t *testing.T, store *Store, path, name string) GameObject {
	t.Helper()
	for _, object := range sourcePrefabObjects(t, store, path).Objects {
		if object.Name == name {
			return object
		}
	}
	t.Fatalf("object %s absent in %s", name, path)
	return GameObject{}
}
func sourcePrefabField(t *testing.T, store *Store, path, name, component, property string) any {
	t.Helper()
	object := sourcePrefabObject(t, store, path, name)
	for _, value := range object.Components {
		if value.Type == component {
			field, err := authoredPropertyValue(value.Fields, property)
			if err != nil {
				t.Fatal(err)
			}
			return field
		}
	}
	t.Fatalf("component %s absent in %+v", component, object)
	return nil
}
func setupSourcePrefab(t *testing.T) (*Store, Source) {
	t.Helper()
	store, scene := fixture(t, sceneFixture)
	_, err := store.AddNativeDefaults(context.Background(), NativeDefaultsRequest{Path: scene.Path, ExpectedRevision: scene.Revision, OperationID: "setup-box", FileID: "100", ComponentType: "BoxCollider", Template: boxDefaultsFixture()})
	if err != nil {
		t.Fatal(err)
	}
	sourcePrefabAction(t, store, scenePath, "create-base", PrefabEditRequest{Action: "create_prefab", FileID: "100", OutputPath: "Assets/Base.prefab"})
	scene, err = store.Read(context.Background(), scenePath)
	if err != nil {
		t.Fatal(err)
	}
	return store, scene
}

func TestSourcePrefabCreateInstantiateApplyRevertAndReplay(t *testing.T) {
	store, scene := setupSourcePrefab(t)
	base, _ := store.Read(context.Background(), "Assets/Base.prefab")
	if !strings.Contains(base.Content, "value: 00123") || !strings.Contains(base.Content, "# position comment") || !strings.Contains(base.Content, "# keep my comment") {
		t.Fatal("creation changed unknown YAML scalar or comments")
	}
	if sourcePrefabObject(t, store, "Assets/Base.prefab", "Base").FileID != "100" {
		t.Fatal("creation changed local source ID")
	}
	request := PrefabEditRequest{Action: "instantiate", Path: scenePath, ExpectedRevision: scene.Revision, OperationID: "instantiate-base", PrefabPath: "Assets/Base.prefab", ParentFileID: "100", Name: "Linked"}
	result, err := store.PrefabEdit(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.CreatedIDs) < 3 {
		t.Fatalf("missing created identities %+v", result)
	}
	store, _ = NewStore(store.root)
	replay, err := store.PrefabEdit(context.Background(), request)
	if err != nil || !replay.Publication.Replayed {
		t.Fatalf("replay %+v %v", replay, err)
	}
	request.Name = "Different"
	_, err = store.PrefabEdit(context.Background(), request)
	requireCode(t, err, "idempotency_conflict")
	sourcePrefabAction(t, store, scenePath, "override-box", PrefabEditRequest{Action: "set_override", TargetPath: "Original/Linked", ComponentType: "BoxCollider", Property: "m_Size.x", Value: json.Number("7")})
	if sourcePrefabField(t, store, scenePath, "Linked", "BoxCollider", "m_Size.x") != int64(7) {
		t.Fatal("override did not resolve")
	}
	apply := sourcePrefabAction(t, store, scenePath, "apply-box", PrefabEditRequest{Action: "apply", TargetPath: "Original/Linked"})
	if len(apply.Publication.Assets) != 2 {
		t.Fatalf("apply not multi-file %+v", apply)
	}
	if sourcePrefabField(t, store, "Assets/Base.prefab", "Base", "BoxCollider", "m_Size.x") != int64(7) {
		t.Fatal("apply did not change source property")
	}
	after, _ := store.Read(context.Background(), "Assets/Base.prefab")
	if after.Metadata != base.Metadata || !strings.Contains(after.Content, "value: 00123") || !strings.Contains(after.Content, "# position comment") {
		t.Fatal("apply changed unrelated source bytes/metadata")
	}
	sourcePrefabAction(t, store, scenePath, "override-box-again", PrefabEditRequest{Action: "set_override", TargetPath: "Original/Linked", ComponentType: "BoxCollider", Property: "m_Size.x", Value: 9})
	sourcePrefabAction(t, store, scenePath, "default-pose", PrefabEditRequest{Action: "set_override", TargetPath: "Original/Linked", ComponentType: "Transform", Property: "m_LocalPosition.x", Value: 42})
	sourcePrefabAction(t, store, scenePath, "revert-box", PrefabEditRequest{Action: "revert", TargetPath: "Original/Linked"})
	if sourcePrefabField(t, store, scenePath, "Linked", "BoxCollider", "m_Size.x") != int64(7) || sourcePrefabField(t, store, scenePath, "Linked", "Transform", "m_LocalPosition.x") != int64(42) {
		t.Fatal("revert changed default root pose or retained property override")
	}
	current, _ := store.Read(context.Background(), scenePath)
	if current.Metadata != scene.Metadata {
		t.Fatal("scene metadata changed")
	}
}

func TestSourcePrefabNestedVariantPartialAndCompleteUnpack(t *testing.T) {
	for _, complete := range []bool{false, true} {
		t.Run(map[bool]string{false: "partial", true: "complete"}[complete], func(t *testing.T) {
			store, scene := setupSourcePrefab(t)
			created, err := store.Edit(context.Background(), editRequest(scene, "container", Operation{Action: "create_gameobject", Key: "container", Name: "Container", ParentID: "100"}))
			if err != nil {
				t.Fatal(err)
			}
			sourcePrefabAction(t, store, scenePath, "nested-child", PrefabEditRequest{Action: "instantiate", PrefabPath: "Assets/Base.prefab", ParentFileID: created.CreatedIDs["container"], Name: "NestedChild"})
			sourcePrefabAction(t, store, scenePath, "create-nested", PrefabEditRequest{Action: "create_prefab", FileID: created.CreatedIDs["container"], OutputPath: "Assets/Nested.prefab"})
			nested, _ := store.Read(context.Background(), "Assets/Nested.prefab")
			if !strings.Contains(nested.Content, "PrefabInstance:") {
				t.Fatal("creation flattened nested prefab")
			}
			nestedAsset, err := parseAsset([]byte(nested.Content))
			if err != nil {
				t.Fatal(err)
			}
			for _, doc := range nestedAsset.docs {
				if doc.stripped {
					ref, _ := doc.data["m_CorrespondingSourceObject"].(map[string]any)
					instanceID, _ := strconv.ParseInt(idReference(doc.data["m_PrefabInstance"]), 10, 64)
					sourceID, _ := strconv.ParseInt(idReference(ref), 10, 64)
					if doc.id != strconv.FormatInt((instanceID^sourceID)&0x7fffffffffffffff, 10) {
						t.Fatal("arbitrary scene alias leaked into prefab source identity", doc.id)
					}
				}
			}
			sourcePrefabAction(t, store, scenePath, "create-variant", PrefabEditRequest{Action: "create_variant", PrefabPath: "Assets/Nested.prefab", OutputPath: "Assets/Variant.prefab"})
			sourcePrefabAction(t, store, scenePath, "variant-instance", PrefabEditRequest{Action: "instantiate", PrefabPath: "Assets/Variant.prefab", Name: "VariantLinked"})
			sourcePrefabAction(t, store, scenePath, "variant-nested-override", PrefabEditRequest{Action: "set_override", TargetPath: "VariantLinked/NestedChild", ComponentType: "BoxCollider", Property: "m_Size.y", Value: 17})
			sourcePrefabAction(t, store, scenePath, "variant-apply", PrefabEditRequest{Action: "apply", TargetPath: "VariantLinked"})
			if sourcePrefabField(t, store, "Assets/Variant.prefab", "NestedChild", "BoxCollider", "m_Size.y") != int64(17) {
				t.Fatal("variant apply not retained in variant")
			}
			if sourcePrefabField(t, store, "Assets/Base.prefab", "Base", "BoxCollider", "m_Size.y") == int64(17) {
				t.Fatal("variant apply modified original base")
			}
			sourcePrefabAction(t, store, scenePath, "unpack-variant", PrefabEditRequest{Action: "unpack", TargetPath: "VariantLinked", Completely: complete})
			object := sourcePrefabObject(t, store, scenePath, "VariantLinked")
			if sourcePrefabField(t, store, scenePath, "NestedChild", "BoxCollider", "m_Size.y") == nil {
				t.Fatal("missing unpacked child")
			}
			if complete && strings.Contains(object.FileID, ":") {
				t.Fatal("complete unpack retained linked root")
			}
			if !complete && !strings.Contains(object.FileID, ":") {
				t.Fatal("partial variant unpack lost immediate base link")
			}
			graph := sourcePrefabObjects(t, store, scenePath)
			if len(graph.UnresolvedPrefabInstances) != 0 {
				t.Fatalf("unresolved %+v", graph)
			}
		})
	}
}

func TestSourcePrefabDependencyMetadataConflictAndExternalSubtreeReference(t *testing.T) {
	store, scene := setupSourcePrefab(t)
	request := PrefabEditRequest{Action: "instantiate", Path: scenePath, ExpectedRevision: scene.Revision, OperationID: "prepared-dependency", PrefabPath: "Assets/Base.prefab", Name: "Linked"}
	// Persist a plan then alter a dependency before publication: no source write.
	planner, err := store.newPrefabPlanner(request)
	if err != nil {
		t.Fatal(err)
	}
	if err := planner.build(request); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(store.root, "Assets/Base.prefab.meta"), "guid: ffffffffffffffffffffffffffffffff\n")
	_, err = store.PublishStaged(context.Background(), planner.publication(request.OperationID))
	requireCode(t, err, "revision_conflict")
	after, _ := store.Read(context.Background(), scenePath)
	if after.Revision != scene.Revision {
		t.Fatal("conflict partially published")
	}
	store, scene = fixture(t, sceneFixture)
	child, err := store.Edit(context.Background(), editRequest(scene, "child", Operation{Action: "create_gameobject", Key: "child", Name: "Child", ParentID: "100"}))
	if err != nil {
		t.Fatal(err)
	}
	scene, _ = store.Read(context.Background(), scenePath)
	content := strings.Replace(scene.Content, "m_Name: Child", "unknownOutside: {fileID: 100}\n  m_Name: Child", 1)
	write(t, filepath.Join(store.root, scenePath), content)
	scene, _ = store.Read(context.Background(), scenePath)
	_, err = store.PrefabEdit(context.Background(), PrefabEditRequest{Action: "create_prefab", Path: scenePath, ExpectedRevision: scene.Revision, OperationID: "outside-reference", FileID: child.CreatedIDs["child"], OutputPath: "Assets/Rejected.prefab"})
	requireCode(t, err, "external_subtree_reference")
	if _, err := os.Stat(filepath.Join(store.root, "Assets/Rejected.prefab")); !os.IsNotExist(err) {
		t.Fatal("rejected subtree wrote output")
	}
	write(t, filepath.Join(store.root, scenePath), strings.Replace(content, "  unknownOutside: {fileID: 100}\n", "", 1))
	// Existing ordinary descendants still create correctly after losing their parent.
	sourcePrefabAction(t, store, scenePath, "create-child", PrefabEditRequest{Action: "create_prefab", FileID: child.CreatedIDs["child"], OutputPath: "Assets/Child.prefab"})
	if _, err := os.Stat(filepath.Join(store.root, "Assets/Child.prefab.meta")); err != nil {
		t.Fatal(err)
	}
}

func lightDefaultsFixture() NativeDefaultsTemplate {
	template := defaultsFixture("UnityEngine.Light", "  - component: {fileID: 102}\n")
	template.Content += `--- !u!108 &102
Light:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  m_Enabled: 1
  m_Type: 2
  m_Intensity: 1
  unknownFuture: {value: 00123, text: 'a, b'} # preserve component
`
	hash := sha256.Sum256([]byte(template.Content))
	template.SHA256 = hex.EncodeToString(hash[:])
	return template
}

func TestSourcePrefabAddedComponentsApplyRevertAndUnpack(t *testing.T) {
	for _, action := range []string{"apply", "revert", "partial", "complete"} {
		t.Run(action, func(t *testing.T) {
			store, _ := setupSourcePrefab(t)
			sourcePrefabAction(t, store, scenePath, "variant-for-add", PrefabEditRequest{Action: "create_variant", PrefabPath: "Assets/Base.prefab", OutputPath: "Assets/Variant.prefab"})
			sourcePrefabAction(t, store, scenePath, "instantiate-for-add", PrefabEditRequest{Action: "instantiate", PrefabPath: "Assets/Variant.prefab", Name: "Linked"})
			template := lightDefaultsFixture()
			added := sourcePrefabAction(t, store, scenePath, "add-light", PrefabEditRequest{Action: "add_component", TargetPath: "Linked", ComponentType: "UnityEngine.Light", DefaultsTemplate: &template, Fields: map[string]any{"m_Intensity": json.Number("3")}})
			id := added.CreatedIDs["UnityEngine.Light"]
			if id == "" {
				t.Fatal("missing component identity")
			}
			if sourcePrefabField(t, store, scenePath, "Linked", "Light", "m_Intensity") != int64(3) {
				t.Fatal("added component did not resolve")
			}
			sourcePrefabAction(t, store, scenePath, "edit-added-light", PrefabEditRequest{Action: "set_override", TargetPath: "Linked", ComponentType: "Light", Property: "m_Intensity", Value: 4})
			switch action {
			case "apply":
				result := sourcePrefabAction(t, store, scenePath, "apply-added-light", PrefabEditRequest{Action: "apply", TargetPath: "Linked"})
				if len(result.Publication.Assets) != 2 {
					t.Fatal("component apply not multi-file")
				}
				if sourcePrefabField(t, store, "Assets/Variant.prefab", "Variant", "Light", "m_Intensity") != int64(4) {
					t.Fatal("component not applied to variant")
				}
				current, _ := store.Read(context.Background(), scenePath)
				asset, _ := parseAsset([]byte(current.Content))
				if doc := asset.find(id); doc == nil || !doc.stripped {
					t.Fatal("component apply did not preserve scene identity as source alias")
				}
				if !strings.Contains(current.Content, "m_PrefabInstance:") {
					t.Fatal("component apply unpacked object")
				}
			case "revert":
				sourcePrefabAction(t, store, scenePath, "revert-added-light", PrefabEditRequest{Action: "revert", TargetPath: "Linked"})
				object := sourcePrefabObject(t, store, scenePath, "Linked")
				for _, component := range object.Components {
					if component.Type == "Light" {
						t.Fatal("revert retained component addition")
					}
				}
			default:
				sourcePrefabAction(t, store, scenePath, "unpack-added-light", PrefabEditRequest{Action: "unpack", TargetPath: "Linked", Completely: action == "complete"})
				if sourcePrefabField(t, store, scenePath, "Linked", "Light", "m_Intensity") != int64(4) {
					t.Fatal("unpack lost authored component value")
				}
				object := sourcePrefabObject(t, store, scenePath, "Linked")
				if action == "complete" && strings.Contains(object.FileID, ":") {
					t.Fatal("complete unpack retained link")
				}
			}
		})
	}
}

func TestPrefabDocumentRemapPreservesUnknownBytesAndSignedIdentities(t *testing.T) {
	input := strings.Replace(prefabFixture(), "target: {fileID: 0}", "target: {fileID: -5824371076177060002}", 1)
	asset, err := parseAsset([]byte(input))
	if err != nil {
		t.Fatal(err)
	}
	copy, err := clonePrefabDocument(asset.docs[0], map[string]string{"100": "200", "101": "201", "-5824371076177060002": "8107781485188562762"}, "", true)
	if err != nil {
		t.Fatal(err)
	}
	got := string(copy.body)
	want := strings.Replace(strings.Replace(string(asset.docs[0].body), "fileID: 101", "fileID: 201", 1), "fileID: -5824371076177060002", "fileID: 8107781485188562762", 1)
	if got != want {
		t.Fatalf("remap altered unrelated source:\n%s", got)
	}
}

func TestPrefabModificationSerializationMatchesUnityBlockSubset(t *testing.T) {
	store, _ := setupSourcePrefab(t)
	sourcePrefabAction(t, store, scenePath, "serialization-variant", PrefabEditRequest{Action: "create_variant", PrefabPath: "Assets/Base.prefab", OutputPath: "Assets/Variant.prefab"})
	sourcePrefabAction(t, store, scenePath, "serialization-instance", PrefabEditRequest{Action: "instantiate", PrefabPath: "Assets/Variant.prefab", Name: "Linked"})
	variant, _ := store.Read(context.Background(), "Assets/Variant.prefab")
	variant.Content = strings.Replace(variant.Content, "    m_RemovedComponents:", "    unknownFuture: {value: 00123, text: 'a, b'} # untouched\n    m_RemovedComponents:", 1)
	write(t, filepath.Join(store.root, variant.Path), variant.Content)
	sourcePrefabAction(t, store, scenePath, "serialization-override", PrefabEditRequest{Action: "set_override", TargetPath: "Linked", ComponentType: "BoxCollider", Property: "m_Size", Value: map[string]any{"x": 3, "y": 7, "z": 9}})
	sourcePrefabAction(t, store, scenePath, "serialization-apply", PrefabEditRequest{Action: "apply", TargetPath: "Linked"})
	after, _ := store.Read(context.Background(), variant.Path)
	if regexp.MustCompile(`(?m)^\s*\[\{objectReference`).MatchString(after.Content) || !strings.Contains(after.Content, "m_Modifications:\n      - objectReference: {fileID: 0}") {
		t.Fatalf("Unity rejects nonempty flow arrays on their own line:\n%s", after.Content)
	}
	if !strings.Contains(after.Content, "unknownFuture: {value: 00123, text: 'a, b'} # untouched") {
		t.Fatal("modification writer changed an unknown sibling")
	}
	if !strings.Contains(after.Content, "propertyPath: m_Size.y") || !strings.Contains(after.Content, `value: "7"`) {
		t.Fatal("typed override serialization was lost")
	}
}

func TestSourcePrefabPublicationReattachesCurrentGuardToPersistedPlan(t *testing.T) {
	store, source := setupSourcePrefab(t)
	calls := 0
	request := PrefabEditRequest{Action: "instantiate", Path: scenePath, ExpectedRevision: source.Revision, OperationID: "guarded-prefab", PrefabPath: "Assets/Base.prefab", Name: "Guarded", BeforePublish: func() error { calls++; return failure("revision_conflict", "native default context changed", nil) }}
	_, err := store.PrefabEdit(context.Background(), request)
	requireCode(t, err, "revision_conflict")
	after, _ := store.Read(context.Background(), scenePath)
	if after.Revision != source.Revision || calls != 1 {
		t.Fatal("failed guard wrote source or was not called")
	}
	restarted, _ := NewStore(store.root)
	request.BeforePublish = func() error { calls++; return nil }
	result, err := restarted.PrefabEdit(context.Background(), request)
	if err != nil || !result.Publication.Changed || calls != 2 {
		t.Fatalf("persisted plan omitted fresh guard: %+v calls%d %v", result, calls, err)
	}
	// Confirmed receipt replay performs no new write, hence no external guard.
	request.BeforePublish = func() error { t.Fatal("confirmed replay attempted a source write"); return nil }
	replay, err := restarted.PrefabEdit(context.Background(), request)
	if err != nil || !replay.Publication.Replayed {
		t.Fatalf("confirmed replay %+v %v", replay, err)
	}
}
