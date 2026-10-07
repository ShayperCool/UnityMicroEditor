package main

import (
	"os"
	"path/filepath"
	"testing"
)

func fixtureSource(t *testing.T) string {
	t.Helper()
	project := t.TempDir()
	source := filepath.Join(project, filepath.FromSlash(modelSource))
	if err := os.MkdirAll(filepath.Dir(source), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source, []byte("Kaydara FBX Binary  \x00fixture"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source+".meta", []byte("fileFormatVersion: 2\nguid: b1d15452ab70b6a669e9b8e3c0b91251\nModelImporter:\n  meshes:\n    globalScale: 1\n"), 0644); err != nil {
		t.Fatal(err)
	}
	return project
}
