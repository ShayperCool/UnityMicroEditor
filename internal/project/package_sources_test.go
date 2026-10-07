package goproject

import (
	"os"
	"path/filepath"
	"testing"
)

func TestOnlyDeclaredPackageSourcesMayReadLibrary(t *testing.T) {
	project := t.TempDir()
	packageRoot := filepath.Join(project, "Library/PackageCache/test.runtime@abc")
	if err := os.MkdirAll(packageRoot, 0700); err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(packageRoot, "Script.cs")
	if err := os.WriteFile(source, []byte("source"), 0600); err != nil {
		t.Fatal(err)
	}
	session := &Session{project: project}
	if _, err := session.checkedPath(source, packageRoot); err == nil {
		t.Fatal("undeclared Library source accepted")
	}
	session.catalog.SourceRoots = []sourceRoot{{Path: "Packages/test.runtime", Source: packageRoot}}
	if _, err := session.checkedPath(source, packageRoot); err != nil {
		t.Fatal(err)
	}
	artifactRoot := filepath.Join(project, "Library/Artifacts/fake")
	if err := os.MkdirAll(artifactRoot, 0700); err != nil {
		t.Fatal(err)
	}
	artifact := filepath.Join(artifactRoot, "Script.cs")
	if err := os.WriteFile(artifact, []byte("not package source"), 0600); err != nil {
		t.Fatal(err)
	}
	session.catalog.SourceRoots = append(session.catalog.SourceRoots, sourceRoot{Path: "Packages/fake", Source: artifactRoot})
	if _, err := session.checkedPath(artifact, artifactRoot); err == nil {
		t.Fatal("Library artifact accepted as package source")
	}
	escaped := filepath.Join(project, "Library/ScriptAssemblies")
	if err := os.MkdirAll(escaped, 0700); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(escaped, "Unsafe.cs")
	if err := os.WriteFile(outside, []byte("outside"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := session.checkedPath(outside, packageRoot); err == nil {
		t.Fatal("package root escape accepted")
	}
}
