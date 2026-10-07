package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"unitymicroeditor/project/authoring"
)

func agentDefaultsTestFixture(t *testing.T) (*agentProjectService, agentDefaultsProfile, string) {
	t.Helper()
	project := agentReviewProject(t)
	if err := os.WriteFile(filepath.Join(project, "Packages/manifest.json"), []byte(`{"dependencies":{}}`), 0600); err != nil {
		t.Fatal(err)
	}
	editorRoot := t.TempDir()
	for _, relative := range []string{"Data/Managed/UnityEngine", "Data/Resources/PackageManager/BuiltInPackages"} {
		if err := os.MkdirAll(filepath.Join(editorRoot, relative), 0700); err != nil {
			t.Fatal(err)
		}
	}
	editor := filepath.Join(editorRoot, "Unity")
	if err := os.WriteFile(editor, []byte("#!/bin/sh\ntouch '"+filepath.Join(project, "editor-executed")+"'\nexit 99\n"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(editorRoot, "Data/Managed/UnityEngine/UnityEngine.CoreModule.dll"), []byte("synthetic native module fingerprint"), 0600); err != nil {
		t.Fatal(err)
	}
	service := &agentProjectService{project: project, editor: editor, noEditor: true, lifetime: context.Background()}
	profile, hash, err := componentDefaultsProfile(context.Background(), project, editor)
	if err != nil {
		t.Fatal(err)
	}
	return service, profile, hash
}

func installAgentDefaultsTestCatalog(t *testing.T, service *agentProjectService, profile agentDefaultsProfile, hash string) {
	t.Helper()
	catalog := agentDefaultsCatalog{SchemaVersion: 1, Profile: profile, ProfileSHA256: hash,
		Templates: map[string]authoring.NativeDefaultsTemplate{}, Receipts: map[string]string{}, SourceSHA256: bytesSHA([]byte("source snapshot"))}
	for class, name := range authoring.NativeComponentTypes() {
		content := fmt.Sprintf("%%YAML 1.1\n%%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_ObjectHideFlags: 0\n  m_PrefabInstance: {fileID: 0}\n  m_Component:\n  - component: {fileID: 2}\n  - component: {fileID: 3}\n  m_Name: Template\n  m_IsActive: 0\n--- !u!4 &2\nTransform:\n  m_ObjectHideFlags: 0\n  m_PrefabInstance: {fileID: 0}\n  m_GameObject: {fileID: 1}\n  m_Father: {fileID: 0}\n  m_Children: []\n--- !u!%d &3\n%s:\n  m_ObjectHideFlags: 0\n  m_PrefabInstance: {fileID: 0}\n  m_GameObject: {fileID: 1}\n  m_Enabled: 1\n", class, name)
		full := "UnityEngine." + name
		catalog.Templates[full] = authoring.NativeDefaultsTemplate{SchemaVersion: 1, Profile: hash, UnityVersion: profile.UnityVersion,
			UnityRevision: profile.UnityRevision, ComponentType: full, Content: content, SHA256: bytesSHA([]byte(content))}
		if err := agentImmutableFile(filepath.Join(agentDefaultsDirectory(service.project, hash), candidateTemplateName(full)), []byte(content)); err != nil {
			t.Fatal(err)
		}
	}
	if err := agentImmutableJSON(filepath.Join(agentDefaultsDirectory(service.project, hash), "catalog.json"), catalog); err != nil {
		t.Fatal(err)
	}
}

func TestComponentDefaultsCacheReadNeverExecutesEditor(t *testing.T) {
	service, profile, hash := agentDefaultsTestFixture(t)
	_, err := service.componentSchema(context.Background(), map[string]any{"componentType": "BoxCollider", "requestId": "missing"})
	var typed *authoring.Error
	if !errors.As(err, &typed) || typed.Code != "component_defaults_missing" {
		t.Fatalf("missing defaults did not return explicit cache miss: %v", err)
	}
	installAgentDefaultsTestCatalog(t, service, profile, hash)
	value, err := service.componentSchema(context.Background(), map[string]any{"componentType": "BoxCollider", "requestId": "cached"})
	if err != nil {
		t.Fatal(err)
	}
	result := value.(map[string]any)
	if result["backend"] != "versioned_component_cache" || result["editorStarted"] != false || result["profileSha256"] != hash {
		t.Fatal("cached schema does not advertise exact source/cache semantics")
	}
	if service.client != nil {
		t.Fatal("cache read constructed an Editor client")
	}
	if _, err := os.Stat(filepath.Join(service.project, "editor-executed")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("ordinary schema query executed the Editor")
	}
	if _, err := service.editorClient(context.Background()); !errors.As(err, &typed) || typed.Code != "editor_disabled" {
		t.Fatal("no-editor policy permitted an Editor client", err)
	}
}

func TestComponentDefaultsCacheRejectsChangedNativeFileAndSDK(t *testing.T) {
	service, profile, hash := agentDefaultsTestFixture(t)
	installAgentDefaultsTestCatalog(t, service, profile, hash)
	path := filepath.Join(agentDefaultsDirectory(service.project, hash), "BoxCollider.unity")
	contents, _ := os.ReadFile(path)
	if err := os.WriteFile(path, append(contents, []byte("# changed\n")...), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := readAgentDefaultsCatalog(service.project, profile, hash); err == nil {
		t.Fatal("changed native default file was accepted")
	}
	if err := os.WriteFile(path, contents, 0600); err != nil {
		t.Fatal(err)
	}
	catalogPath := filepath.Join(agentDefaultsDirectory(service.project, hash), "catalog.json")
	data, _ := os.ReadFile(catalogPath)
	var catalog agentDefaultsCatalog
	json.Unmarshal(data, &catalog)
	template := catalog.Templates["UnityEngine.BoxCollider"]
	template.UnityRevision = "different"
	catalog.Templates["UnityEngine.BoxCollider"] = template
	data, _ = json.Marshal(catalog)
	if err := os.WriteFile(catalogPath, data, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := readAgentDefaultsCatalog(service.project, profile, hash); err == nil {
		t.Fatal("changed native default revision was accepted")
	}
	if err := os.WriteFile(filepath.Join(filepath.Dir(service.editor), "Data/Managed/UnityEngine/UnityEngine.CoreModule.dll"), []byte("new native module"), 0600); err != nil {
		t.Fatal(err)
	}
	_, changedHash, err := componentDefaultsProfile(context.Background(), service.project, service.editor)
	if err != nil || changedHash == hash {
		t.Fatal("changed SDK module did not invalidate native defaults", err)
	}
}

func TestComponentDefaultsPublicationGuardRejectsChangedSettings(t *testing.T) {
	service, profile, hash := agentDefaultsTestFixture(t)
	installAgentDefaultsTestCatalog(t, service, profile, hash)
	guard := service.componentDefaultsGuard(context.Background(), hash)
	if err := guard(); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(service.project, "ProjectSettings/PhysicsManager.asset"), []byte("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!55 &1\nPhysicsManager:\n  m_Gravity: {x: 0, y: -5, z: 0}\n"), 0600); err != nil {
		t.Fatal(err)
	}
	var typed *authoring.Error
	if err := guard(); !errors.As(err, &typed) || typed.Code != "component_defaults_changed" {
		t.Fatalf("changed settings accepted at publication: %v", err)
	}
}
