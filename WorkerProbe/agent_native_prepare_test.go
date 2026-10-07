package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestAgentNativePreparationIncludesFreshTextureAndAudioInputs(t *testing.T) {
	for _, extension := range []string{".png", ".jpg", ".tga", ".tiff", ".fbx", ".flac", ".mat", ".inputactions"} {
		t.Run(extension, func(t *testing.T) {
			project := agentReviewProject(t)
			if err := os.WriteFile(filepath.Join(project, "Assets/NativeInput"+extension), []byte("native source"), 0600); err != nil {
				t.Fatal(err)
			}
			needed, err := agentMayNeedNative(project)
			if err != nil || !needed {
				t.Fatalf("native input %s skipped public preparation: %v", extension, err)
			}
		})
	}
	project := agentReviewProject(t)
	needed, err := agentMayNeedNative(project)
	if err != nil || needed {
		t.Fatal("plain source-only fixture unexpectedly needs heavy content", err)
	}
}

func TestAgentNativePreparationRejectsMalformedSettingsAndLinkedInputs(t *testing.T) {
	project := agentReviewProject(t)
	if err := os.WriteFile(filepath.Join(project, "ProjectSettings/GraphicsSettings.asset"), []byte("not: [valid YAML"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := agentMayNeedNative(project); err == nil {
		t.Fatal("malformed native project settings silently accepted")
	}
	project = agentReviewProject(t)
	if err := os.Symlink(t.TempDir(), filepath.Join(project, "Assets/linked-assets")); err != nil {
		t.Fatal(err)
	}
	if _, err := agentMayNeedNative(project); err == nil {
		t.Fatal("linked native input silently accepted")
	}
}
