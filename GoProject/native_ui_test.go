package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"strings"
	"testing"
)

func nativeUIProofForTest(file *NativeContentFile) {
	data, _ := json.Marshal(file.UIBindings)
	hash := sha256.Sum256(data)
	file.UIBindingProof = &NativeUIBindingProof{SchemaVersion: 1, Mode: "native-ui-pptr-bindings-v1", NativeSHA256: file.SHA256, ReaderSHA256: strings.Repeat("a", 64), DumpSHA256: strings.Repeat("b", 64), BindingsSHA256: hex.EncodeToString(hash[:])}
}
func TestNativeUIBindingValidationExactOwnerDependenciesAndRegistryGuard(t *testing.T) {
	const owner = "87785bfd71a0a425fb905aaf7743f114"
	const style = "cd5ff7a50bd83450bbe76880cf52d119"
	file := NativeContentFile{GUID: owner, SHA256: strings.Repeat("c", 64), Objects: []NativeContentObject{{FileID: "19", TypeName: "UnityEngine.UIElements.VisualTreeAsset"}, {FileID: "10", TypeName: "UnityEngine.UIElements.StyleSheet"}}, Dependencies: []NativeContentDependency{{GUID: style}}, UIBindings: []NativeUIBinding{{OwnerFileID: "19", PropertyPath: "inlineSheet", TargetGUID: owner, TargetFileID: "10"}, {OwnerFileID: "19", PropertyPath: "references.RefIds[0].data.m_Stylesheets[0]", ManagedReferenceID: "1000", TargetGUID: style, TargetFileID: "7433"}}}
	nativeUIProofForTest(&file)
	files := map[string]NativeContentFile{owner: file, style: {GUID: style, Objects: []NativeContentObject{{FileID: "7433", TypeName: "UnityEngine.UIElements.StyleSheet"}}}}
	if err := validateNativeUIBindings(file, files); err != nil {
		t.Fatal(err)
	}
	for _, mutate := range []func(*NativeContentFile){
		func(f *NativeContentFile) { f.UIBindings[1].ManagedReferenceID = "" },
		func(f *NativeContentFile) { f.UIBindings[1].TargetFileID = "999" },
		func(f *NativeContentFile) { f.UIBindings[1].TargetGUID = "a54a76024e0db49348ae426002a8a519" },
		func(f *NativeContentFile) {
			f.UIBindings[1].PropertyPath = "references.RefIds[-1].data.m_Stylesheets[0]"
		},
		func(f *NativeContentFile) { f.UIBindings[1].OwnerFileID = "999" },
		func(f *NativeContentFile) { f.UIBindings = append(f.UIBindings, f.UIBindings[1]) },
		func(f *NativeContentFile) { f.UIBindings[0].TargetFileID = "19" },
	} {
		bad := file
		bad.UIBindings = append([]NativeUIBinding(nil), file.UIBindings...)
		mutate(&bad)
		nativeUIProofForTest(&bad)
		if err := validateNativeUIBindings(bad, files); err == nil {
			t.Fatalf("bad native UI descriptor accepted: %+v", bad)
		}
	}
	bad := file
	bad.UIBindingProof = nil
	if err := validateNativeUIBindings(bad, files); err == nil {
		t.Fatal("missing extraction proof accepted")
	}
	bad = file
	bad.SHA256 = strings.Repeat("d", 64)
	if err := validateNativeUIBindings(bad, files); err == nil {
		t.Fatal("proof from different native payload accepted")
	}
}
