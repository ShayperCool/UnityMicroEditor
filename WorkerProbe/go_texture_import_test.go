package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"

	"unitymicroeditor/worker/runtimeartifact"
)

func TestColdNativePNGMatchesExactUnityNPOTArtifactAndRuntimeProjection(t *testing.T) {
	reference := os.Getenv("UNITY_WORKER_PNG_IMPORT_REPORT")
	project := os.Getenv("UNITY_WORKER_UI_PROJECT")
	if reference == "" || project == "" {
		t.Skip("set UNITY_WORKER_PNG_IMPORT_REPORT and UNITY_WORKER_UI_PROJECT for the offline cold PNG proof")
	}
	var report struct {
		Success           bool                 `json:"success"`
		MainEditorStarted bool                 `json:"mainEditorStarted"`
		WorkerExit        string               `json:"workerExit"`
		Environment       *workerEnvironment   `json:"environment"`
		Content           projectImportContent `json:"content"`
	}
	if err := readJSONFile(reference, &report); err != nil {
		t.Fatal(err)
	}
	if !report.Success || report.MainEditorStarted || report.WorkerExit != "exit 0" || !report.Content.SourceUnchanged || len(report.Content.Assets) != 1 || report.Environment == nil {
		t.Fatalf("cold texture import did not finish with verified source: %+v", report)
	}
	asset := report.Content.Assets[0]
	if asset.Path != "Assets/Dungeon/ShipGen/Art/Pixel/Decals/Poster_D.png" || asset.GUID != "89ca37b1dee52456b89281e63eabc29f" || len(asset.Files) != 1 || asset.Files[0].Name != "" {
		t.Fatalf("unexpected cold NPOT proof asset: %+v", asset)
	}
	input, err := os.ReadFile(asset.Files[0].Path)
	if err != nil || bytesSHA(input) != asset.Files[0].SHA256 || asset.Files[0].SHA256 != "a39f6254bb535ff4700da7c9608d8d689f2b020e637c54229bc1477389f9df9b" {
		t.Fatalf("cold NPOT bytes differ from the native Unity oracle: %v", err)
	}
	runtime, metadata, err := runtimeartifact.AdaptTexture(input, "png")
	if err != nil {
		t.Fatal(err)
	}
	if metadata.Texture == nil || *metadata.Texture != (runtimeartifact.TextureMetadata{Width: 128, Height: 128, Format: 3, MipCount: 8, CompleteImageSize: 65535}) {
		t.Fatalf("cold Unity resize/mip layout differs: %+v", metadata.Texture)
	}
	if len(metadata.Objects) != 1 || metadata.Objects[0].ClassID != 28 || metadata.Objects[0].FileID != "2800000" || metadata.Objects[0].TypeName != "UnityEngine.Texture2D" || len(metadata.Removed) != 1 || metadata.Removed[0].ClassID != 1006 {
		t.Fatalf("cold texture projection changed object identity or removed non-importer data: %+v", metadata)
	}
	output := filepath.Join(t.TempDir(), "warm.runtime.bin")
	warm, err := extractWarmNativeUI(context.Background(), project, asset.Path, output)
	if err != nil {
		t.Fatal(err)
	}
	expected, err := os.ReadFile(output)
	if err != nil || warm.Primary.SHA256 != asset.Files[0].SHA256 || !bytes.Equal(runtime, expected) {
		t.Fatalf("cold native/runtime bytes differ from exact source/meta warm Unity import: %v", err)
	}
	t.Logf("native PNG cold/runtime output %s/%s, layout %+v", metadata.InputSHA256, metadata.OutputSHA256, *metadata.Texture)
}
