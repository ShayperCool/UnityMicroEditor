package goproject

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func newNativeShaderFixture(t *testing.T) (*Session, string, NativeReplacement) {
	t.Helper()
	s, output, replacement := newNativeTextureFixture(t)
	old := filepath.Join(s.project, replacement.SourcePath)
	replacement.SourcePath = "Assets/Authored.shader"
	source := filepath.Join(s.project, replacement.SourcePath)
	if err := os.Rename(old, source); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(old+".meta", source+".meta"); err != nil {
		t.Fatal(err)
	}
	replacement.Content.Objects = []NativeContentObject{{FileID: "4800000", TypeName: "UnityEngine.Shader"}}
	return s, output, replacement
}

func TestNativeShaderPublishesExactStandaloneObject(t *testing.T) {
	s, output, replacement := newNativeShaderFixture(t)
	baseline := clone(s.baseline)
	if err := s.ReplaceNativeContent(replacement); err != nil {
		t.Fatal(err)
	}
	if _, err := s.Prepare(output); err != nil {
		t.Fatal(err)
	}
	asset := s.assets[assetKey(replacement.GUID, "4800000")]
	if str(asset["kind"]) != "imported" || str(asset["typeName"]) != "UnityEngine.Shader" || asset["classId"] != int32(48) || !reflect.DeepEqual(baseline, s.baseline) {
		t.Fatalf("native Shader identity changed: %+v", asset)
	}
}

func TestNativeShaderRejectsAdditionalGraphResourcesAndDependencies(t *testing.T) {
	for _, name := range []string{"wrong ID", "wrong type", "extra object", "model graph", "graph hash", "resource", "dependency", "source changed"} {
		t.Run(name, func(t *testing.T) {
			s, _, replacement := newNativeShaderFixture(t)
			before := clone(s.assets)
			switch name {
			case "wrong ID":
				replacement.Content.Objects[0].FileID = "4800001"
			case "wrong type":
				replacement.Content.Objects[0].TypeName = "UnityEngine.Material"
			case "extra object":
				replacement.Content.Objects = append(replacement.Content.Objects, NativeContentObject{FileID: "1", TypeName: "UnityEngine.GameObject"})
			case "model graph":
				replacement.ModelGraph = []NativeModelGraphObject{{FileID: "1", ClassID: 1}}
			case "graph hash":
				replacement.ModelGraphSHA256 = replacement.Content.SHA256
			case "resource":
				replacement.Content.Resources = []NativeContentResource{{}}
			case "dependency":
				replacement.Content.Dependencies = []NativeContentDependency{{GUID: "0000000000000000f000000000000000", Global: true}}
			case "source changed":
				writeFixtureFile(t, filepath.Join(s.project, replacement.SourcePath), "new shader source")
			}
			if err := s.ReplaceNativeContent(replacement); err == nil {
				t.Fatal("invalid native Shader accepted")
			}
			if len(s.nativeOverlays) != 0 || !reflect.DeepEqual(before, s.assets) {
				t.Fatal("rejected Shader changed session content")
			}
		})
	}
}
