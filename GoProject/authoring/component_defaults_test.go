package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

func defaultsFixture(name, extra string) NativeDefaultsTemplate {
	content := strings.Replace(sceneFixture, "  - component: {fileID: 101}\n", "  - component: {fileID: 101}\n"+extra, 1)
	hash := sha256.Sum256([]byte(content))
	return NativeDefaultsTemplate{SchemaVersion: 1, Profile: "verified-profile", UnityVersion: VerifiedUnityVersion,
		UnityRevision: "45d8eee7de74", ComponentType: name, Content: content, SHA256: hex.EncodeToString(hash[:])}
}

func boxDefaultsFixture() NativeDefaultsTemplate {
	template := defaultsFixture("UnityEngine.BoxCollider", "  - component: {fileID: 102}\n")
	template.Content += `--- !u!65 &102
BoxCollider:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  serializedVersion: 3
  m_IsTrigger: 0
  m_Enabled: 1
  m_Size: {x: 1, y: 1, z: 1}
  m_Center: {x: 0, y: 0, z: 0}
  exactCounter: 0
`
	hash := sha256.Sum256([]byte(template.Content))
	template.SHA256 = hex.EncodeToString(hash[:])
	return template
}

func TestNativeDefaultsSourceCASReplayUndoAndExactValues(t *testing.T) {
	ctx := context.Background()
	store, source := fixture(t, sceneFixture)
	request := NativeDefaultsRequest{Path: source.Path, ExpectedRevision: source.Revision, OperationID: "cached-box",
		FileID: "100", ComponentType: "UnityEngine.BoxCollider", Template: boxDefaultsFixture(),
		Fields: map[string]any{"m_IsTrigger": true, "m_Size.x": json.Number("3"), "exactCounter": json.Number("9223372036854775807")}}
	added, err := store.AddNativeDefaults(ctx, request)
	if err != nil {
		t.Fatal(err)
	}
	current, err := store.Read(ctx, source.Path)
	if err != nil || current.Metadata != source.Metadata || !strings.Contains(current.Content, "exactCounter: 9223372036854775807") ||
		!strings.Contains(current.Content, "Original # keep my comment") || !strings.Contains(current.Content, "m_IsTrigger: 1") {
		t.Fatalf("source/default/identity preservation failed: %v\n%s", err, current.Content)
	}
	parsed, err := parseAsset([]byte(current.Content))
	if err != nil {
		t.Fatal(err)
	}
	component := parsed.find(added.CreatedIDs["UnityEngine.BoxCollider"])
	if component == nil || component.classID != 65 || idReference(component.data["m_GameObject"]) != "100" || component.data["serializedVersion"] != int64(3) {
		t.Fatalf("exact cached component ownership/numeric types were not retained: %#v", component)
	}
	reopened, err := NewStore(store.root)
	if err != nil {
		t.Fatal(err)
	}
	replay, err := reopened.AddNativeDefaults(ctx, request)
	if err != nil || !replay.Replayed || replay.Revision != added.Revision || replay.CreatedIDs["UnityEngine.BoxCollider"] != component.id {
		t.Fatalf("journal replay failed: %#v %v", replay, err)
	}
	changed := request
	changed.Fields = map[string]any{"m_IsTrigger": false}
	_, err = reopened.AddNativeDefaults(ctx, changed)
	requireCode(t, err, "idempotency_conflict")
	stale := request
	stale.OperationID = "cached-box-stale"
	_, err = reopened.AddNativeDefaults(ctx, stale)
	requireCode(t, err, "revision_conflict")
	if _, err := reopened.Undo(ctx, UndoRequest{Path: source.Path, OriginalOperationID: request.OperationID, OperationID: "undo-cached-box", ExpectedRevision: added.Revision}); err != nil {
		t.Fatal(err)
	}
	restored, _ := reopened.Read(ctx, source.Path)
	if restored.Content != source.Content || restored.Metadata != source.Metadata {
		t.Fatal("component undo failed to restore exact source bytes")
	}
}

func TestNativeDefaultsPublicationGuardRejectsBeforeWriting(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	changedProfile := errors.New("native settings changed")
	request := NativeDefaultsRequest{Path: source.Path, ExpectedRevision: source.Revision, OperationID: "guarded-box", FileID: "100",
		ComponentType: "UnityEngine.BoxCollider", Template: boxDefaultsFixture(), BeforePublish: func() error { return changedProfile }}
	if _, err := store.AddNativeDefaults(context.Background(), request); !errors.Is(err, changedProfile) {
		t.Fatalf("profile guard failure was lost: %v", err)
	}
	unchanged, _ := store.Read(context.Background(), source.Path)
	if unchanged.Revision != source.Revision {
		t.Fatal("failed profile guard published source")
	}
	request.BeforePublish = func() error { return nil }
	if _, err := store.AddNativeDefaults(context.Background(), request); err != nil {
		t.Fatal("prepublication rejection consumed operation ID", err)
	}
}

func TestNativeDefaultsRequiredPeerReferencesAndOwnerRemapping(t *testing.T) {
	store, source := fixture(t, sceneFixture)
	template := defaultsFixture("UnityEngine.ParticleSystem", "  - component: {fileID: 102}\n  - component: {fileID: 103}\n")
	template.Content += `--- !u!198 &102
ParticleSystem:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  renderer: {fileID: 103}
--- !u!199 &103
ParticleSystemRenderer:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  system: {fileID: 102}
`
	hash := sha256.Sum256([]byte(template.Content))
	template.SHA256 = hex.EncodeToString(hash[:])
	result, err := store.AddNativeDefaults(context.Background(), NativeDefaultsRequest{Path: source.Path, ExpectedRevision: source.Revision,
		OperationID: "cached-particles", TargetPath: "Original", ComponentType: "UnityEngine.ParticleSystem", Template: template})
	if err != nil {
		t.Fatal(err)
	}
	current, _ := store.Read(context.Background(), source.Path)
	asset, _ := parseAsset([]byte(current.Content))
	system := asset.find(result.CreatedIDs["UnityEngine.ParticleSystem"])
	renderer := asset.find(result.CreatedIDs["UnityEngine.ParticleSystemRenderer"])
	if system == nil || renderer == nil || idReference(system.data["renderer"]) != renderer.id || idReference(renderer.data["system"]) != system.id || len(defaultsComponentIDs(asset.find("100"))) != 3 {
		t.Fatal("required components and peer references were not transferred together")
	}
}

func TestNativeDefaultsCanvasReplacesTransformWithoutChangingIdentity(t *testing.T) {
	withUnknown := strings.Replace(sceneFixture, "  m_Father: {fileID: 0}\n", "  m_Father: {fileID: 0}\n  futureTransformField: {exactValue: 9223372036854775807}\n", 1)
	store, source := fixture(t, withUnknown)
	template := defaultsFixture("UnityEngine.Canvas", "  - component: {fileID: 102}\n")
	template.Content = strings.Replace(template.Content, "--- !u!4 &101\nTransform:", "--- !u!224 &101\nRectTransform:", 1)
	template.Content = strings.Replace(template.Content, "  m_Father: {fileID: 0}\n", "  m_Father: {fileID: 0}\n  m_AnchorMin: {x: 0.5, y: 0.5}\n  m_AnchorMax: {x: 0.5, y: 0.5}\n  m_AnchoredPosition: {x: 0, y: 0}\n  m_SizeDelta: {x: 100, y: 100}\n  m_Pivot: {x: 0.5, y: 0.5}\n", 1)
	template.Content += `--- !u!223 &102
Canvas:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  m_Enabled: 1
  m_RenderMode: 0
`
	hash := sha256.Sum256([]byte(template.Content))
	template.SHA256 = hex.EncodeToString(hash[:])
	result, err := store.AddNativeDefaults(context.Background(), NativeDefaultsRequest{Path: source.Path, ExpectedRevision: source.Revision,
		OperationID: "cached-canvas", FileID: "100", ComponentType: "UnityEngine.Canvas", Template: template})
	if err != nil {
		t.Fatal(err)
	}
	current, _ := store.Read(context.Background(), source.Path)
	asset, _ := parseAsset([]byte(current.Content))
	rect := asset.find("101")
	if rect == nil || rect.classID != 224 || result.CreatedIDs["UnityEngine.RectTransform"] != "101" || idReference(rect.data["m_Father"]) != "0" ||
		idReference(asset.rootDocument().data["m_Roots"].([]any)[0]) != "101" || rect.data["m_LocalPosition"].(map[string]any)["y"] != int64(1) || asset.docs[1].id != "101" {
		t.Fatal("Transform replacement changed identity, order, root ownership or source position")
	}
	unknown, exists := rect.data["futureTransformField"].(map[string]any)
	if !exists || unknown["exactValue"] != int64(9223372036854775807) {
		t.Fatal("RectTransform conversion dropped an unrelated authored Transform field")
	}
}

func TestNativeDefaultsRejectChangedHashUnsupportedCompanionsAndOwnershipFields(t *testing.T) {
	for _, invalid := range []string{"hash", "revision", "companion", "local-reference", "ownership", "unknown-field"} {
		t.Run(invalid, func(t *testing.T) {
			store, source := fixture(t, sceneFixture)
			template := boxDefaultsFixture()
			fields := map[string]any{}
			switch invalid {
			case "hash":
				template.Content += "# changed\n"
			case "revision":
				template.UnityRevision = ""
			case "companion":
				template.Content = strings.Replace(template.Content, "--- !u!65 &102\nBoxCollider:", "--- !u!114 &102\nMonoBehaviour:", 1)
			case "local-reference":
				template.Content += "  other: {fileID: 777}\n"
			case "ownership":
				fields["m_GameObject"] = map[string]any{"fileID": "101"}
			case "unknown-field":
				fields["unknown"] = 4
			}
			if invalid == "companion" || invalid == "local-reference" {
				hash := sha256.Sum256([]byte(template.Content))
				template.SHA256 = hex.EncodeToString(hash[:])
			}
			_, err := store.AddNativeDefaults(context.Background(), NativeDefaultsRequest{Path: source.Path, ExpectedRevision: source.Revision,
				OperationID: "reject-default", FileID: "100", ComponentType: "UnityEngine.BoxCollider", Template: template, Fields: fields})
			if err == nil {
				t.Fatal("invalid native default/field request was accepted")
			}
			current, _ := store.Read(context.Background(), source.Path)
			if current.Content != source.Content || current.Metadata != source.Metadata {
				t.Fatal("rejected defaults changed authored source")
			}
		})
	}
}
