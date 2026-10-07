package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

func prefabFixture() string { return strings.Split(sceneFixture, "--- !u!1660057539")[0] }

func TestInventoryPagesNativeInputsAndBindsStatMetadataSnapshot(t *testing.T) {
	store, _ := fixture(t, sceneFixture)
	for i := 0; i < 120; i++ {
		path := filepath.Join(store.root, "Assets", fmt.Sprintf("Native%03d.png", i))
		write(t, path, "\x00binary")
		write(t, path+".meta", fmt.Sprintf("guid: %032x\n", i+1))
	}
	page, err := store.Inventory(context.Background(), InventoryRequest{Extensions: []string{".PNG"}})
	if err != nil || len(page.Entries) != 100 || page.Total != 120 || page.NextOffset == nil || *page.NextOffset != 100 {
		t.Fatalf("page %+v %v", page, err)
	}
	second, err := store.Inventory(context.Background(), InventoryRequest{Extensions: []string{".png"}, Offset: 100, ExpectedRevision: page.Revision})
	if err != nil || len(second.Entries) != 20 || second.NextOffset != nil {
		t.Fatalf("second %+v %v", second, err)
	}
	write(t, filepath.Join(store.root, "Assets/Native000.png.meta"), "guid: ffffffffffffffffffffffffffffffff\n")
	_, err = store.Inventory(context.Background(), InventoryRequest{Offset: 100, ExpectedRevision: page.Revision})
	requireCode(t, err, "revision_conflict")
	page, err = store.Inventory(context.Background(), InventoryRequest{PathPrefix: "Assets/Scenes", IncludeSourceRevision: true})
	if err != nil || len(page.Entries) != 1 || page.Entries[0].SourceRevision == "" {
		t.Fatalf("filtered %+v %v", page, err)
	}
	_, err = store.Inventory(context.Background(), InventoryRequest{PathPrefix: "Assets/../ProjectSettings"})
	requireCode(t, err, "invalid_path")
}

func TestNativeComponentTemplateAndExplicitFields(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	fields := map[string]any{"serializedVersion": 3, "m_Enabled": true, "m_IsTrigger": false, "m_Center": map[string]any{"x": 0, "y": 0, "z": 0}, "m_Size": map[string]any{"x": 1, "y": 2, "z": 3}, "m_Material": reference("0")}
	result, err := store.Edit(context.Background(), editRequest(source, "native-box", Operation{Action: "add_component", FileID: "100", Key: "box", ComponentType: "UnityEngine.BoxCollider", Fields: fields}))
	if err != nil {
		t.Fatal(err)
	}
	after, _ := store.Read(context.Background(), scenePath)
	if after.Metadata != source.Metadata {
		t.Fatal("metadata changed")
	}
	asset, _ := parseAsset([]byte(after.Content))
	box := asset.find(result.CreatedIDs["box"])
	if box.classID != 65 || box.data["serializedVersion"] != int64(3) || box.data["m_IsTrigger"] != int64(0) {
		t.Fatalf("box %+v", box)
	}
	_, err = store.Edit(context.Background(), editRequest(after, "native-copy", Operation{Action: "add_component", FileID: "100", Key: "copy", ClassID: 65, TemplatePath: scenePath, TemplateFileID: box.id, TemplateRevision: after.Revision}))
	if err != nil {
		t.Fatal(err)
	}
	latest, _ := store.Read(context.Background(), scenePath)
	_, err = store.Edit(context.Background(), editRequest(latest, "native-stale-template", Operation{Action: "add_component", FileID: "100", ClassID: 65, TemplatePath: scenePath, TemplateFileID: box.id, TemplateRevision: after.Revision}))
	requireCode(t, err, "revision_conflict")
	_, err = store.Edit(context.Background(), editRequest(latest, "native-unknown", Operation{Action: "add_component", FileID: "100", ComponentType: "Unknown", Fields: fields}))
	requireCode(t, err, "unsupported_component")
	_, err = store.Edit(context.Background(), editRequest(latest, "native-no-defaults", Operation{Action: "add_component", FileID: "100", ClassID: 20}))
	requireCode(t, err, "invalid_request")
}

func TestNestedCollectionReferenceReplacementPreservesNeighborBytes(t *testing.T) {
	input := strings.Replace(sceneFixture, "unknownFuture: {value: 00123, label: 'a, b', target: {fileID: 0}}", "unknownFuture: {value: 00123, label: 'a, b', target: {fileID: 0}, list: [1, 2]} # neighbor", 1)
	store, source := fixture(t, input)
	_, err := store.Edit(context.Background(), editRequest(source, "nested-reference", Operation{Action: "set_property", FileID: "100", Property: "unknownFuture.target", Value: reference("101")}, Operation{Action: "set_property", FileID: "100", Property: "unknownFuture.list", Value: []any{3, 4, 5}}))
	if err != nil {
		t.Fatal(err)
	}
	after, _ := store.Read(context.Background(), scenePath)
	want := strings.Replace(input, "target: {fileID: 0}, list: [1, 2]", "target: {fileID: 101}, list: [3, 4, 5]", 1)
	if after.Content != want {
		t.Fatalf("unexpected lexical changes:\n%s", after.Content)
	}
	_, err = store.Edit(context.Background(), editRequest(after, "bad-ref-guid", Operation{Action: "set_property", FileID: "100", Property: "unknownFuture.target", Value: map[string]any{"fileID": "1", "guid": "bogus"}}))
	requireCode(t, err, "invalid_identity")
}

func TestSourcePrefabInstantiationAndOverridesResolveThroughRuntimeExpander(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	prefabPath := "Assets/Base.prefab"
	created, err := store.CreateAsset(context.Background(), AssetCreateRequest{Path: prefabPath, ExpectedRevision: "absent", OperationID: "prefab-create", Content: prefabFixture(), GUID: "22222222222222222222222222222222"})
	if err != nil {
		t.Fatal(err)
	}
	graph, err := store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: prefabPath})
	if err != nil || graph.Total != 1 {
		t.Fatalf("prefab graph %+v %v", graph, err)
	}
	request := editRequest(source, "prefab-instantiate", Operation{Action: "instantiate_prefab", Key: "prefab", PrefabPath: prefabPath, PrefabRevision: created.Revision, DependencyRevisions: graph.DependencyRevisions, ParentID: "100", Name: "Instance"})
	instantiated, err := store.Edit(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	page, err := store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: scenePath})
	if err != nil || page.Total != 2 || len(page.UnresolvedPrefabInstances) != 0 {
		t.Fatalf("resolved %+v %v", page, err)
	}
	var instanceObject GameObject
	for _, object := range page.Objects {
		if object.Name == "Instance" {
			instanceObject = object
		}
	}
	if instanceObject.ParentID != "100" || !strings.Contains(instanceObject.FileID, ":") {
		t.Fatalf("instance %+v", instanceObject)
	}
	latest, _ := store.Read(context.Background(), scenePath)
	_, err = store.Edit(context.Background(), editRequest(latest, "prefab-position", Operation{Action: "set_prefab_override", InstanceID: instantiated.CreatedIDs["prefab.instance"], TargetFileID: "101", Property: "m_LocalPosition.x", Value: 42, DependencyRevisions: graph.DependencyRevisions}))
	if err != nil {
		t.Fatal(err)
	}
	page, err = store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: scenePath})
	if err != nil {
		t.Fatal(err)
	}
	for _, object := range page.Objects {
		if object.Name == "Instance" && object.Components[0].Fields["m_LocalPosition"].(map[string]any)["x"] != int64(42) {
			t.Fatal("override did not apply")
		}
	}
	latest, _ = store.Read(context.Background(), scenePath)
	_, err = store.Edit(context.Background(), editRequest(latest, "prefab-position-revert", Operation{Action: "revert_prefab_override", InstanceID: instantiated.CreatedIDs["prefab.instance"], TargetFileID: "101", Property: "m_LocalPosition.x", DependencyRevisions: graph.DependencyRevisions}))
	if err != nil {
		t.Fatal(err)
	}
	unchanged, _ := store.Read(context.Background(), prefabPath)
	if unchanged.Revision != created.Revision {
		t.Fatal("instance override changed source prefab")
	}
	_, err = store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: scenePath, ExpectedRevision: page.Revision})
	requireCode(t, err, "revision_conflict")
}

func TestStagedPublicationChecksAllInputsAndReplaysAfterRestart(t *testing.T) {
	store, scene := fixture(t, sceneFixture)
	script, err := store.CreateScript(context.Background(), ScriptRequest{Path: "Assets/Scripts/Dependency.cs", ExpectedRevision: "absent", OperationID: "dependency", Content: "public class Dependency {}"})
	if err != nil {
		t.Fatal(err)
	}
	projectRevision, _ := store.DependencyRevision(context.Background(), "ProjectSettings/ProjectVersion.txt")
	request := StagedPublicationRequest{OperationID: "staged-two", Inputs: []PublicationInput{{scene.Path, scene.Revision}, {script.Path, script.Revision}, {"ProjectSettings/ProjectVersion.txt", projectRevision}}, Outputs: []PublicationOutput{{Path: scene.Path, Content: strings.Replace(scene.Content, "m_Name: Original", "m_Name: Staged", 1)}, {Path: script.Path, Content: "public class Dependency { public int staged; }"}}}
	result, err := store.PublishStaged(context.Background(), request)
	if err != nil || !result.Changed || len(result.Assets) != 2 {
		t.Fatalf("publish %+v %v", result, err)
	}
	store, _ = NewStore(store.root)
	replay, err := store.PublishStaged(context.Background(), request)
	if err != nil || !replay.Replayed || replay.Assets[0].Revision != result.Assets[0].Revision {
		t.Fatalf("replay %+v %v", replay, err)
	}
	latestScene, _ := store.Read(context.Background(), scene.Path)
	latestScript, _ := store.Read(context.Background(), script.Path)
	request.OperationID = "dependency-conflict"
	request.Inputs[0].ExpectedRevision = latestScene.Revision
	request.Inputs[1].ExpectedRevision = latestScript.Revision
	write(t, filepath.Join(store.root, "ProjectSettings/ProjectVersion.txt"), "m_EditorVersion: 6000.6.3f1\nchanged: 1\n")
	_, err = store.PublishStaged(context.Background(), request)
	requireCode(t, err, "revision_conflict")
	after, _ := store.Read(context.Background(), scene.Path)
	if after.Revision != latestScene.Revision {
		t.Fatal("dependency conflict partially published")
	}
	request.OperationID = "staged-metadata-change"
	request.Inputs = request.Inputs[:2]
	metadata := latestScene.Metadata + "changed: 1\n"
	request.Outputs[0].Metadata = &metadata
	_, err = store.PublishStaged(context.Background(), request)
	requireCode(t, err, "protected_property")
}

func TestStagedPublicationRecoveryRollsBackEveryChangedFile(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	otherPath := "Assets/Second.unity"
	write(t, filepath.Join(store.root, otherPath), source.Content)
	write(t, filepath.Join(store.root, otherPath)+".meta", "guid: 33333333333333333333333333333333\n")
	other, _ := store.Read(context.Background(), otherPath)
	request := StagedPublicationRequest{OperationID: "staged-interrupted", Inputs: []PublicationInput{{source.Path, source.Revision}, {other.Path, other.Revision}}, Outputs: []PublicationOutput{{Path: source.Path, Content: strings.Replace(source.Content, "Original", "Changed", 1)}, {Path: other.Path, Content: strings.Replace(other.Content, "Original", "Changed", 1)}}}
	encoded, _ := json.Marshal(request)
	fingerprint := sha256.Sum256(encoded)
	id := sha256.Sum256([]byte(request.OperationID))
	transaction := journal{OperationID: request.OperationID, Fingerprint: hex.EncodeToString(fingerprint[:]), State: "prepared", BatchResult: &PublicationResult{OperationID: request.OperationID}, Files: []journalFile{{Path: source.Path, Before: []byte(source.Content), After: []byte(request.Outputs[0].Content), Existed: true, Mode: 0644}, {Path: other.Path, Before: []byte(other.Content), After: []byte(request.Outputs[1].Content), Existed: true, Mode: 0644}}}
	if err := writeJournal(filepath.Join(store.state, hex.EncodeToString(id[:])+".json"), transaction); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(store.root, source.Path), request.Outputs[0].Content)
	_, err := store.PublishStaged(context.Background(), request)
	requireCode(t, err, "rolled_back")
	after, _ := store.Read(context.Background(), source.Path)
	if after.Revision != source.Revision {
		t.Fatal("first file did not roll back")
	}
	after, _ = store.Read(context.Background(), other.Path)
	if after.Revision != other.Revision {
		t.Fatal("second file changed")
	}
}

func TestPublicationReferenceOverlayAndWholeUndo(t *testing.T) {
	store, original := fixture(t, sceneFixture)
	added, err := store.Edit(context.Background(), editRequest(original, "child-for-overlay", Operation{Action: "create_gameobject", Key: "child", Name: "Child", ParentID: "100"}))
	if err != nil {
		t.Fatal(err)
	}
	scene, _ := store.Read(context.Background(), scenePath)
	otherPath := "Assets/External.asset"
	otherContent := "--- !u!114 &11400000\nMonoBehaviour:\n  m_GameObject: {fileID: 0}\n  linked: {fileID: " + added.CreatedIDs["child"] + ", guid: " + scene.GUID + ", type: 3}\n"
	write(t, filepath.Join(store.root, otherPath), otherContent)
	write(t, filepath.Join(store.root, otherPath)+".meta", "guid: 44444444444444444444444444444444\n")
	other, _ := store.Read(context.Background(), otherPath)
	request := StagedPublicationRequest{OperationID: "remove-with-overlay", Inputs: []PublicationInput{{scene.Path, scene.Revision}, {other.Path, other.Revision}}, Outputs: []PublicationOutput{{Path: scene.Path, Content: original.Content}}}
	_, err = store.PublishStaged(context.Background(), request)
	requireCode(t, err, "referenced_object")
	request.Outputs = append(request.Outputs, PublicationOutput{Path: other.Path, Content: strings.Replace(otherContent, "linked: {fileID: "+added.CreatedIDs["child"]+", guid: "+scene.GUID+", type: 3}", "linked: {fileID: 0}", 1)})
	result, err := store.PublishStaged(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	revisions := map[string]string{}
	for _, asset := range result.Assets {
		revisions[asset.Path] = asset.Revision
	}
	undo := PublicationUndoRequest{OperationID: "undo-overlay", OriginalOperationID: result.OperationID, ExpectedRevisions: revisions}
	_, err = store.UndoPublication(context.Background(), undo)
	if err != nil {
		t.Fatal(err)
	}
	replay, err := store.UndoPublication(context.Background(), undo)
	if err != nil || !replay.Replayed {
		t.Fatalf("undo replay %+v %v", replay, err)
	}
	after, _ := store.Read(context.Background(), scenePath)
	if after.Revision != scene.Revision {
		t.Fatal("whole publication undo did not restore source")
	}
	after, _ = store.Read(context.Background(), otherPath)
	if after.Revision != other.Revision {
		t.Fatal("whole publication undo did not restore external reference")
	}
}

func TestExplicitMaterialAndScriptableObjectCreation(t *testing.T) {
	store, _ := fixture(t, sceneFixture)
	material := "--- !u!21 &2100000\nMaterial:\n  m_Name: Explicit # keep\n  serializedVersion: 8\n  m_Shader: {fileID: 108, guid: 0000000000000000f000000000000000, type: 0}\n  m_SavedProperties:\n    serializedVersion: 3\n    m_TexEnvs: []\n    m_Ints: []\n    m_Floats: []\n    m_Colors: []\n  unknownFuture: {v: 00123}\n"
	created, err := store.CreateAsset(context.Background(), AssetCreateRequest{Path: "Assets/Explicit.mat", ExpectedRevision: "absent", OperationID: "material-create", Content: material})
	if err != nil {
		t.Fatal(err)
	}
	source, _ := store.Read(context.Background(), created.Path)
	_, err = store.Edit(context.Background(), editRequest(source, "material-color", Operation{Action: "set_property", FileID: "2100000", Property: "m_SavedProperties.m_Colors", Value: []any{map[string]any{"_Color": map[string]any{"r": 1, "g": 0.5, "b": 0, "a": 1}}}}))
	if err != nil {
		t.Fatal(err)
	}
	after, _ := store.Read(context.Background(), created.Path)
	if after.Metadata != source.Metadata || !strings.Contains(after.Content, "unknownFuture: {v: 00123}") || !strings.Contains(after.Content, "m_Name: Explicit # keep") {
		t.Fatal("material edit changed unrelated content")
	}
	script, err := store.CreateScript(context.Background(), ScriptRequest{Path: "Assets/Scripts/Data.cs", ExpectedRevision: "absent", OperationID: "scriptable-script", Content: "public class Data : UnityEngine.ScriptableObject { public int value; }"})
	if err != nil {
		t.Fatal(err)
	}
	data := "--- !u!114 &11400000\nMonoBehaviour:\n  m_GameObject: {fileID: 0}\n  m_Script: {fileID: 11500000, guid: " + script.GUID + ", type: 3}\n  m_Name: Data\n  value: 5\n"
	created, err = store.CreateAsset(context.Background(), AssetCreateRequest{Path: "Assets/Data.asset", ExpectedRevision: "absent", OperationID: "scriptable-create", Content: data})
	if err != nil {
		t.Fatal(err)
	}
	source, _ = store.Read(context.Background(), created.Path)
	if source.Content != data || !strings.Contains(source.Metadata, "mainObjectFileID: 11400000") {
		t.Fatal("explicit ScriptableObject serialization was changed")
	}
}

func TestRectTransformCreationRequiresExplicitLayoutAndKeepsOwnership(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	fields := map[string]any{"m_LocalPosition": map[string]any{"x": 0, "y": 0, "z": 0}, "m_LocalRotation": map[string]any{"x": 0, "y": 0, "z": 0, "w": 1}, "m_LocalScale": map[string]any{"x": 1, "y": 1, "z": 1}, "m_AnchorMin": map[string]any{"x": 0, "y": 0}, "m_AnchorMax": map[string]any{"x": 1, "y": 1}, "m_AnchoredPosition": map[string]any{"x": 0, "y": 0}, "m_SizeDelta": map[string]any{"x": 100, "y": 50}, "m_Pivot": map[string]any{"x": 0.5, "y": 0.5}}
	_, err := store.Edit(context.Background(), editRequest(source, "rect-no-defaults", Operation{Action: "create_gameobject", Name: "Rect", RectTransform: true}))
	requireCode(t, err, "invalid_request")
	_, err = store.Edit(context.Background(), editRequest(source, "rect-wrong-parent", Operation{Action: "create_gameobject", Name: "Rect", ParentID: "999", RectTransform: true, Fields: fields}))
	requireCode(t, err, "not_found")
	created, err := store.Edit(context.Background(), editRequest(source, "rect-create", Operation{Action: "create_gameobject", Key: "rect", Name: "Rect", ParentID: "100", RectTransform: true, Fields: fields}))
	if err != nil {
		t.Fatal(err)
	}
	page, err := store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: scenePath})
	if err != nil {
		t.Fatal(err)
	}
	for _, object := range page.Objects {
		if object.FileID == created.CreatedIDs["rect"] && (object.ParentID != "100" || object.Components[0].ClassID != 224) {
			t.Fatalf("RectTransform ownership %+v", object)
		}
	}
}

// Unity's public Editor resave writes a prefab instance ID in SceneRoots,
// whereas earlier source can contain its stripped root Transform alias.
func TestSceneRootsAcceptsEditorPrefabInstanceIdentityAndRejectsDuplicates(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	created, err := store.CreateAsset(context.Background(), AssetCreateRequest{Path: "Assets/Base.prefab", ExpectedRevision: "absent", OperationID: "roots-prefab", Content: prefabFixture(), GUID: "22222222222222222222222222222222"})
	if err != nil {
		t.Fatal(err)
	}
	graph, err := store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: created.Path})
	if err != nil {
		t.Fatal(err)
	}
	edit, err := store.Edit(context.Background(), editRequest(source, "roots-instance", Operation{Action: "instantiate_prefab", Key: "instance", PrefabPath: created.Path, PrefabRevision: created.Revision, DependencyRevisions: graph.DependencyRevisions}))
	if err != nil {
		t.Fatal(err)
	}
	latest, _ := store.Read(context.Background(), source.Path)
	parsed, _ := parseAsset([]byte(latest.Content))
	rootDocument := parsed.rootDocument()
	aliases := idsFromList(rootDocument.data["m_Roots"])
	instanceID, transformID := edit.CreatedIDs["instance.instance"], edit.CreatedIDs["instance.transform"]
	references := []any{}
	for _, id := range aliases {
		if id == transformID {
			id = instanceID
		}
		references = append(references, reference(id))
	}
	if err := parsed.setTopLevel(rootDocument.id, "m_Roots", references); err != nil {
		t.Fatal(err)
	}
	unityContent := string(parsed.bytes())
	result, err := store.PublishStaged(context.Background(), StagedPublicationRequest{OperationID: "editor-sceneroot-publication", Inputs: []PublicationInput{{Path: latest.Path, ExpectedRevision: latest.Revision}}, Outputs: []PublicationOutput{{Path: latest.Path, Content: unityContent}}})
	if err != nil || !result.Changed {
		t.Fatalf("Editor SceneRoots rejected: %+v %v", result, err)
	}
	resolved, err := store.ResolvedHierarchy(context.Background(), HierarchyRequest{Path: latest.Path})
	if err != nil || len(resolved.Roots) != 2 {
		t.Fatalf("runtime expansion lost prefab root: %+v %v", resolved, err)
	}
	references = append(references, reference(transformID))
	if err := parsed.setTopLevel(rootDocument.id, "m_Roots", references); err != nil {
		t.Fatal(err)
	}
	err = parsed.validate(latest.GUID)
	requireCode(t, err, "invalid_graph")
	instance := parsed.find(instanceID)
	modification := instance.data["m_Modification"].(map[string]any)
	modification["m_TransformParent"] = reference("101")
	instance.data["m_Modification"] = modification
	err = parsed.validate(latest.GUID)
	requireCode(t, err, "invalid_graph")
}

func TestRawSceneRootsPreservesUnresolvedPrefabLinksWithoutZeroRoots(t *testing.T) {
	base := `%YAML 1.1
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
`
	for _, scenario := range []struct {
		name, alias, root string
		resolved          bool
	}{
		{"Editor link without local owner", "", "500", false},
		{"owner with inherited unknown parent", "  m_GameObject: {fileID: 502}\n", "500", false},
		{"referenced transform with local owner", "  m_GameObject: {fileID: 502}\n", "501", true},
		{"explicit local root owner", "  m_GameObject: {fileID: 502}\n  m_Father: {fileID: 0}\n", "500", true},
	} {
		t.Run(scenario.name, func(t *testing.T) {
			content := base + scenario.alias + "--- !u!1 &502 stripped\nGameObject:\n  m_CorrespondingSourceObject: {fileID: 100, guid: 22222222222222222222222222222222, type: 3}\n  m_PrefabInstance: {fileID: 500}\n  m_PrefabAsset: {fileID: 0}\n--- !u!1660057539 &9223372036854775807\nSceneRoots:\n  m_Roots:\n  - {fileID: 11}\n  - {fileID: " + scenario.root + "}\n"
			asset, err := parseAsset([]byte(content))
			if err != nil {
				t.Fatal(err)
			}
			view, err := hierarchy(Source{Path: scenePath}, asset, "")
			if err != nil {
				t.Fatal(err)
			}
			wantRoots := 1
			if scenario.resolved {
				wantRoots = 2
			}
			if len(view.Roots) != wantRoots || view.Roots[0] != "10" || scenario.resolved && view.Roots[1] != "502" {
				t.Fatalf("incorrect raw roots %+v", view)
			}
			if !scenario.resolved && (len(view.UnresolvedRootIDs) != 1 || view.UnresolvedRootIDs[0] != scenario.root) {
				t.Fatalf("unresolved prefab root identity lost %+v", view)
			}
			if len(view.UnresolvedPrefabInstances) != 1 || view.UnresolvedPrefabInstances[0] != "500" {
				t.Fatal("raw view falsely claimed prefab expansion")
			}
		})
	}
}
