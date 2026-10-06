package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestNativeMeshCookingSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_LIFECYCLE_SDK")
	if editor == "" {
		t.Skip("set UNITY_GO_LIFECYCLE_SDK; only the installed .NET SDK is used")
	}
	toolRoot, err := microEditorRoot()
	if err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	for _, name := range []string{"RawMeshCooking.cs", "NativeMeshColliderRuntime.cs", "RawJson.cs", "RawOwnedObjects.cs"} {
		source, err := os.ReadFile(filepath.Join(toolRoot, "Packages/com.unitymicroeditor.runtime/Runtime/Raw", name))
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(root, name), source, 0600); err != nil {
			t.Fatal(err)
		}
	}
	fixture, err := os.ReadFile("testdata/RawMeshCooking.cs")
	if err != nil {
		t.Fatal(err)
	}
	for name, contents := range map[string][]byte{
		"Program.cs":   fixture,
		"Tests.csproj": []byte(`<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework><UseAppHost>false</UseAppHost><TreatWarningsAsErrors>true</TreatWarningsAsErrors></PropertyGroup></Project>`),
		"NuGet.Config": []byte(`<configuration><packageSources><clear /></packageSources></configuration>`),
	} {
		if err := os.WriteFile(filepath.Join(root, name), contents, 0600); err != nil {
			t.Fatal(err)
		}
	}
	dotnet := filepath.Join(filepath.Dir(editor), "Data/DotNetSdk/dotnet")
	if _, err := goCompilerCommand(context.Background(), root, dotnet, "build", "Tests.csproj", "--nologo", "-v:q", "-p:NuGetAudit=false"); err != nil {
		t.Fatal(err)
	}
	output, err := goCompilerCommand(context.Background(), root, dotnet, filepath.Join(root, "bin/Debug/net8.0/Tests.dll"))
	if err != nil {
		t.Fatal(err)
	}
	t.Log(string(output))
}
