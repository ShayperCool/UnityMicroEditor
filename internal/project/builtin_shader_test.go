package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func builtinSnapshotFixture(t *testing.T, root string) RetainedBuiltinShader {
	t.Helper()
	path := filepath.Join(root, "compiled-builtin.bin")
	writeFixtureFile(t, path, "immutable compiled native Shader fixture")
	fileSHA := sha256.Sum256([]byte("immutable compiled native Shader fixture"))
	hash := hex.EncodeToString(fileSHA[:])
	sha := strings.Repeat("a", 64)
	return RetainedBuiltinShader{SchemaVersion: 1, UnityVersion: "6000.6.3f1", GUID: BuiltinResourceGUID, FileID: "108", ShaderName: "Skybox/Panoramic",
		SourceGUID: "f57c2bc4f17ba48d9b9b6758dcfe812d", SourceFileID: "4800000", ArchiveFileID: "2756170372350126523",
		BundlePath: filepath.Join(root, "source.bundle"), BundleSHA256: sha, MemberPath: filepath.Join(root, "member"), MemberName: "CAB-builtin", MemberSHA256: sha,
		PayloadSHA256: sha, TypeTreeSHA256: sha, BuiltinSourcePath: filepath.Join(root, "engine-resources"), BuiltinSourceSHA256: sha,
		Content: NativeContentFile{GUID: BuiltinResourceGUID, Path: path, SHA256: hash, Objects: []NativeContentObject{{"108", "UnityEngine.Shader"}}}}
}

func TestRetainBuiltinShaderPreservesAuthoredGraphAndPinsRuntimeFile(t *testing.T) {
	session, scene, output := fixtureSession(t)
	before, _ := os.ReadFile(scene)
	snapshot := builtinSnapshotFixture(t, session.project)
	if err := session.RetainBuiltinShader(snapshot); err != nil {
		t.Fatal(err)
	}
	prepared, err := session.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	if len(array(manifest["builtinShaderSnapshots"])) != 1 || len(array(manifest["nativeContentFiles"])) != 1 {
		t.Fatal("retained built-in Shader/provenance missing from source manifest")
	}
	file := object(array(manifest["nativeContentFiles"])[0])
	if str(file["guid"]) != BuiltinResourceGUID || str(object(array(file["objects"])[0])["fileId"]) != "108" {
		t.Fatal("authored built-in Shader identity was remapped incorrectly")
	}
	after, _ := os.ReadFile(scene)
	if string(before) != string(after) {
		t.Fatal("retaining a native engine Shader rewrote the authored scene")
	}
	writeFixtureFile(t, snapshot.Content.Path, "changed native Shader bytes")
	if _, err := session.Prepare(output); err == nil {
		t.Fatal("published a changed retained Shader artifact")
	}
}

func TestRetainedBuiltinShaderRejectsSubstitutedIdentityOrDependencies(t *testing.T) {
	good := builtinSnapshotFixture(t, t.TempDir())
	for _, corrupt := range []func(*RetainedBuiltinShader){
		func(v *RetainedBuiltinShader) { v.FileID = "101" },
		func(v *RetainedBuiltinShader) { v.ShaderName = "Skybox/Cubemap" },
		func(v *RetainedBuiltinShader) { v.SourceGUID = strings.Repeat("1", 32) },
		func(v *RetainedBuiltinShader) { v.Content.GUID = v.SourceGUID },
		func(v *RetainedBuiltinShader) {
			v.Content.Objects = []NativeContentObject{{"4800000", "UnityEngine.Shader"}}
		},
		func(v *RetainedBuiltinShader) {
			v.Content.Dependencies = []NativeContentDependency{{BuiltinResourceGUID, true}}
		},
		func(v *RetainedBuiltinShader) { v.CompiledSourceRevisionVerified = true },
	} {
		value := good
		corrupt(&value)
		if err := value.Validate("6000.6.3f1"); err == nil {
			t.Fatalf("substituted native engine Shader accepted: %+v", value)
		}
	}
}
