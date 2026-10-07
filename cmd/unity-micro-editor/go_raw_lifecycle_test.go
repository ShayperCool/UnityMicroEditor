package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

// Exercise the actual session/facade/ownership code with a small scene backend.
// This is an SDK-only test; it does not initialize Unity or start a Player.
func TestRawSessionOwnedDDOLLifecycleSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_LIFECYCLE_SDK")
	if editor == "" {
		t.Skip("set UNITY_GO_LIFECYCLE_SDK to Unity executable; only its .NET SDK is used")
	}
	toolRoot, err := microEditorRoot()
	if err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	runtime := filepath.Join(toolRoot, "Packages/com.unitymicroeditor.runtime/Runtime/Raw")
	for _, name := range []string{"RawSceneSession.cs", "RawOwnedObjects.cs", "RawReloadIdentity.cs", "RawProjectContent.cs"} {
		contents, err := os.ReadFile(filepath.Join(runtime, name))
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(root, name), contents, 0600); err != nil {
			t.Fatal(err)
		}
	}
	fixture, err := os.ReadFile("testdata/RawSessionLifecycle.cs")
	if err != nil {
		t.Fatal(err)
	}
	files := map[string][]byte{
		"Program.cs":            fixture,
		"LifecycleTests.csproj": []byte(`<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework><UseAppHost>false</UseAppHost><TreatWarningsAsErrors>true</TreatWarningsAsErrors><NoWarn>0649</NoWarn></PropertyGroup></Project>`),
		"NuGet.Config":          []byte(`<configuration><packageSources><clear /></packageSources></configuration>`),
	}
	for name, contents := range files {
		if err := os.WriteFile(filepath.Join(root, name), contents, 0600); err != nil {
			t.Fatal(err)
		}
	}
	dotnet := filepath.Join(filepath.Dir(editor), "Data/DotNetSdk/dotnet")
	if _, err := goCompilerCommand(context.Background(), root, dotnet, "build", "LifecycleTests.csproj", "--nologo", "-v:q", "-p:NuGetAudit=false"); err != nil {
		t.Fatal(err)
	}
	output, err := goCompilerCommand(context.Background(), root, dotnet, filepath.Join(root, "bin/Debug/net8.0/LifecycleTests.dll"), root)
	if err != nil {
		t.Fatal(err)
	}
	t.Log(string(output))
}
