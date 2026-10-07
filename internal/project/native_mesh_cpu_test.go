package goproject

import "testing"

func TestNativeMeshCPURetentionNamesOnlyImportedUnreadableMeshes(t *testing.T) {
	file := NativeContentFile{Objects: []NativeContentObject{{FileID: "-4300", TypeName: "UnityEngine.Mesh"}, {FileID: "21", TypeName: "UnityEngine.Material"}},
		MeshCPURetention: []NativeMeshCPURetention{{FileID: "-4300", OriginalIsReadable: false}}}
	if err := validateNativeMeshCPURetention(file); err != nil {
		t.Fatal(err)
	}
	for _, values := range [][]NativeMeshCPURetention{
		{{FileID: "-4300", OriginalIsReadable: true}},
		{{FileID: "-4300"}, {FileID: "-4300"}},
		{{FileID: "21"}}, {{FileID: "missing"}}, {{FileID: "0"}},
	} {
		file.MeshCPURetention = values
		if err := validateNativeMeshCPURetention(file); err == nil {
			t.Fatalf("foreign/duplicate/non-Mesh/readable retention declaration passed: %+v", values)
		}
	}
}
