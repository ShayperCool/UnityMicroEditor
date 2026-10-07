package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	goproject "unitymicroeditor/internal/project"
)

const nativeUIDumpFixture = `External References
path(1): "Library/unity default resources" GUID: 0000000000000000e000000000000000 Type: 0
path(2): "" GUID: cd5ff7a50bd83450bbe76880cf52d119 Type: 3

ID: 1068529831357515114 (ClassID: 114) MonoBehaviour
	m_Script  (PPtr<MonoScript>)
		m_FileID 1 (int)
		m_PathID 11998 (SInt64)
ID: 9197481963319205126 (ClassID: 114) MonoBehaviour
	inlineSheet  (PPtr<$StyleSheet>)
		m_FileID 0 (int)
		m_PathID 1068529831357515114 (SInt64)
	references  (ManagedReferencesRegistry)
		version 2 (int)
		rid -2 (class: , ns: , asm: )
		rid 1000 (class: VisualElementAsset, ns: UnityEngine.UIElements, asm: UnityEngine.UIElementsModule)
			VisualElementAsset
				m_VisualTreeAsset  (PPtr<$VisualTreeAsset>)
					m_FileID 0 (int)
					m_PathID 9197481963319205126 (SInt64)
				m_Stylesheets  (vector)
					size 2 (int)
					data  (PPtr<$StyleSheet>)
						m_FileID 2 (int)
						m_PathID 7433441132597879392 (SInt64)
					data  (PPtr<$StyleSheet>)
						m_FileID 2 (int)
						m_PathID 7433441132597879392 (SInt64)
`

func nativeUIFixtureFile() goproject.NativeContentFile {
	return goproject.NativeContentFile{GUID: "87785bfd71a0a425fb905aaf7743f114", Objects: []goproject.NativeContentObject{{FileID: "1068529831357515114", TypeName: "UnityEngine.UIElements.StyleSheet"}, {FileID: "9197481963319205126", TypeName: "UnityEngine.UIElements.VisualTreeAsset"}}, Dependencies: []goproject.NativeContentDependency{{GUID: "0000000000000000e000000000000000", Global: true}, {GUID: "cd5ff7a50bd83450bbe76880cf52d119"}}}
}

func TestNativeUIExactManagedPPtrSlotsAndLocalInlineSheet(t *testing.T) {
	bindings, err := parseNativeUIBindings([]byte(nativeUIDumpFixture), nativeUIFixtureFile())
	if err != nil {
		t.Fatal(err)
	}
	if len(bindings) != 3 || bindings[0].PropertyPath != "inlineSheet" || bindings[0].TargetGUID != nativeUIFixtureFile().GUID || bindings[1].PropertyPath != "references.RefIds[0].data.m_Stylesheets[0]" || bindings[2].PropertyPath != "references.RefIds[0].data.m_Stylesheets[1]" || bindings[1].ManagedReferenceID != "1000" || bindings[1].TargetFileID != "7433441132597879392" {
		t.Fatalf("authored slots/order/identities changed: %+v", bindings)
	}
	bad := nativeUIFixtureFile()
	bad.Dependencies[1].GUID = "b54a76024e0db49348ae426002a8a519"
	if _, err := parseNativeUIBindings([]byte(nativeUIDumpFixture), bad); err == nil {
		t.Fatal("mismatched dependency identity accepted")
	}
	bad = nativeUIFixtureFile()
	bad.Objects[1].FileID = "19"
	if _, err := parseNativeUIBindings([]byte(nativeUIDumpFixture), bad); err == nil {
		t.Fatal("foreign owner identity accepted")
	}
}

func TestNativeUIActualCacheBindingReadbackWithoutEditor(t *testing.T) {
	index := os.Getenv("UNITY_NATIVE_UI_INDEX")
	if index == "" {
		t.Skip("set UNITY_NATIVE_UI_INDEX for exact project UI runtime cache")
	}
	var entries []goproject.NativeReplacement
	if err := readJSONFile(index, &entries); err != nil {
		t.Fatal(err)
	}
	project := os.Getenv("UNITY_NATIVE_UI_PROJECT")
	if project == "" {
		t.Fatal("UNITY_NATIVE_UI_PROJECT required")
	}
	output := filepath.Join(project, "Temp/UnityPlay/ui-style-proof", randomID())
	importer := &nativeProjectImporter{opts: launchOptions{Project: project, Output: output, Editor: defaultUnityEditor()}}
	counts := map[string]int{}
	for _, entry := range entries {
		if filepath.Ext(entry.SourcePath) != ".uxml" && filepath.Ext(entry.SourcePath) != ".uss" {
			continue
		}
		bound, err := importer.bindNativeUIReferences(context.Background(), entry)
		if err != nil {
			t.Fatal(err)
		}
		counts[entry.SourcePath] = len(bound.Content.UIBindings)
		t.Logf("%s bindings=%+v", entry.SourcePath, bound.Content.UIBindings)
		if bound.Content.Path != entry.Content.Path || bound.Content.SHA256 != entry.Content.SHA256 {
			t.Fatal("UI payload changed during reference extraction")
		}
		if len(bound.Content.UIBindings) > 0 {
			bad := bound
			bad.Content.UIBindings = append([]goproject.NativeUIBinding(nil), bound.Content.UIBindings...)
			bad.Content.UIBindings[0].TargetFileID = "987654321"
			if _, err := importer.bindNativeUIReferences(context.Background(), bad); err == nil {
				t.Fatal("modified claimed binding accepted")
			}
		}
	}
	if (counts["Assets/Dungeon/Net/UI/Lobby.uxml"] != 3 && counts["Assets/Dungeon/ShipGen/UI/ShipHud.uxml"] != 3) || counts["Assets/Dungeon/UI/Terminal.uss"] != 5 {
		t.Fatalf("actual authored root styles/fonts not extracted: %+v", counts)
	}
	t.Logf("Actual standalone native UI bindings: %+v; no Editor/Player", counts)
}
