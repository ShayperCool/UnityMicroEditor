package goproject

import (
	"path/filepath"
	"testing"
)

func TestCacheOwnershipKeepsPersistentStateOutOfUnityLibraryFiles(t *testing.T) {
	project := t.TempDir()
	for _, relative := range []string{"Temp/Session/current.json", "Library/UnityMicroEditor/baselines/current.json"} {
		if !IsOwnedCachePath(project, filepath.Join(project, relative)) {
			t.Fatalf("owned cache rejected: %s", relative)
		}
	}
	for _, relative := range []string{"Temp", "Library/UnityMicroEditor", "Library/ScriptAssemblies/Game.dll", "Assets/Scene.unity", "Library/UnityMicroEditorOther/current.json", "../other/Temp/output"} {
		if IsOwnedCachePath(project, filepath.Join(project, relative)) {
			t.Fatalf("foreign/cache-root path accepted: %s", relative)
		}
	}
}
