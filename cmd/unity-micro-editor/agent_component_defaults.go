package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"syscall"
	"time"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/project/authoring"
	unitypackages "unitymicroeditor/internal/project/packages"
	"unitymicroeditor/internal/editorbridge"
)

type agentDefaultsProfile struct {
	SchemaVersion int               `json:"schemaVersion"`
	UnityVersion  string            `json:"unityVersion"`
	UnityRevision string            `json:"unityRevision"`
	EngineInputs  map[string]string `json:"engineInputs"`
	ContextInputs map[string]string `json:"contextInputs"`
}

type agentDefaultsCatalog struct {
	SchemaVersion int                                         `json:"schemaVersion"`
	Profile       agentDefaultsProfile                        `json:"profile"`
	ProfileSHA256 string                                      `json:"profileSha256"`
	Templates     map[string]authoring.NativeDefaultsTemplate `json:"templates"`
	Receipts      map[string]string                           `json:"captureReceipts"`
	SourceSHA256  string                                      `json:"sourceSnapshotSha256"`
}

func componentDefaultsName(name string) (string, error) {
	name = strings.TrimPrefix(name, "UnityEngine.")
	for _, candidate := range authoring.NativeComponentTypes() {
		if candidate == name {
			return "UnityEngine." + name, nil
		}
	}
	return "", agentFailure("unsupported_component", "component type is outside the pinned native profile")
}

// Built-in defaults depend on the native engine and its rendering/settings
// context. Gameplay and IDE package changes do not invalidate these templates.
// Custom Editor Reset/default extensions are outside this native-only contract.
func componentDefaultsProfile(ctx context.Context, project, editor string) (agentDefaultsProfile, string, error) {
	profile := agentDefaultsProfile{SchemaVersion: 1, UnityVersion: fixtureUnityVersion,
		EngineInputs: map[string]string{}, ContextInputs: map[string]string{}}
	var embedded struct {
		UnityRevision string `json:"unityRevision"`
	}
	if err := json.Unmarshal(engineProfile, &embedded); err != nil {
		return profile, "", err
	}
	profile.UnityRevision = embedded.UnityRevision
	version, err := readAgentFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), 64<<10)
	validVersion := false
	for _, line := range strings.Split(string(version), "\n") {
		if strings.TrimSpace(line) == "m_EditorVersion: "+fixtureUnityVersion {
			validVersion = true
		}
	}
	if err != nil || !validVersion {
		return profile, "", agentFailure("unsupported_unity_version", "component defaults require the pinned project version")
	}
	if strings.Contains(string(version), "m_EditorVersionWithRevision:") && !strings.Contains(string(version), "("+profile.UnityRevision+")") {
		return profile, "", agentFailure("unsupported_unity_revision", "component defaults require the exact pinned engine revision")
	}
	engineFiles := []string{editor}
	modules, err := filepath.Glob(filepath.Join(filepath.Dir(editor), "Data/Managed/UnityEngine/UnityEngine.*Module.dll"))
	if err != nil || len(modules) == 0 {
		return profile, "", agentFailure("invalid_engine_profile", "installed SDK native module inventory is missing")
	}
	engineFiles = append(engineFiles, modules...)
	for _, path := range engineFiles {
		if err := ctx.Err(); err != nil {
			return profile, "", err
		}
		if err := agentCanonical(path); err != nil {
			return profile, "", err
		}
		hash, err := goFileHash(path)
		if err != nil {
			return profile, "", err
		}
		logical, _ := filepath.Rel(filepath.Dir(editor), path)
		profile.EngineInputs[filepath.ToSlash(logical)] = hash
	}
	for _, name := range []string{"GraphicsSettings.asset", "QualitySettings.asset", "PhysicsManager.asset", "Physics2DSettings.asset", "ProjectSettings.asset"} {
		logical := "ProjectSettings/" + name
		path := filepath.Join(project, filepath.FromSlash(logical))
		if err := agentCanonical(path); err != nil {
			return profile, "", err
		}
		hash, err := goFileHash(path)
		if errors.Is(err, os.ErrNotExist) {
			hash = "absent"
		} else if err != nil {
			return profile, "", err
		}
		profile.ContextInputs[logical] = hash
	}
	// Resolve exact native package roots rather than interpreting PackageCache
	// directory names. The read path is offline and never executes an Editor.
	resolved, err := unitypackages.Resolve(ctx, project, editor, unitypackages.Options{Offline: true})
	if err != nil {
		return profile, "", err
	}
	for _, pkg := range resolved.Packages {
		if strings.HasPrefix(pkg.Name, "com.unity.render-pipelines.") || pkg.Name == "com.unity.ugui" {
			profile.ContextInputs["Packages/"+pkg.Name] = pkg.ContentHash
		}
	}
	if err := componentDefaultsReferenceClosure(ctx, project, resolved, profile.ContextInputs); err != nil {
		return profile, "", err
	}
	tool, err := microEditorRoot()
	if err != nil {
		return profile, "", err
	}
	producer, err := goFileHash(filepath.Join(tool, "Packages/com.unitymicroeditor.runtime/Editor/AgentEditorAuthoring.cs"))
	if err != nil {
		return profile, "", err
	}
	profile.ContextInputs["capturePolicy"] = producer
	encoded, err := json.Marshal(profile)
	return profile, bytesSHA(encoded), err
}

func agentDefaultsDirectory(project, profile string) string {
	return filepath.Join(project, "Library/UnityMicroEditor/component-defaults", profile)
}

func readAgentDefaultsCatalog(project string, profile agentDefaultsProfile, hash string) (agentDefaultsCatalog, error) {
	var catalog agentDefaultsCatalog
	path := filepath.Join(agentDefaultsDirectory(project, hash), "catalog.json")
	if err := agentCanonical(path); err != nil {
		return catalog, err
	}
	if err := agentReadJSON(path, 64<<20, &catalog); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return catalog, &authoring.Error{Code: "component_defaults_missing", Message: "capture the version-bound native defaults once before adding components", Details: map[string]any{
				"profileSha256": hash, "captureCommand": "unity-micro-editor cache-component-defaults --project " + project}}
		}
		return catalog, err
	}
	if catalog.SchemaVersion != 1 || catalog.ProfileSHA256 != hash || !reflect.DeepEqual(catalog.Profile, profile) || len(catalog.Templates) != len(authoring.NativeComponentTypes()) {
		return catalog, agentFailure("invalid_component_defaults", "cached native default catalog differs from its exact engine/context profile")
	}
	for _, candidate := range authoring.NativeComponentTypes() {
		name := "UnityEngine." + candidate
		template, exists := catalog.Templates[name]
		if !exists || template.SchemaVersion != 1 || template.ComponentType != name || template.Profile != hash || template.UnityVersion != profile.UnityVersion ||
			template.UnityRevision != profile.UnityRevision || template.SHA256 != bytesSHA([]byte(template.Content)) {
			return catalog, agentFailure("invalid_component_defaults", "cached native default template identity or contents changed: "+name)
		}
		path := filepath.Join(agentDefaultsDirectory(project, hash), candidateTemplateName(name))
		if err := agentCanonical(path); err != nil {
			return catalog, err
		}
		contents, err := readAgentFile(path, 32<<20)
		if err != nil || bytesSHA(contents) != template.SHA256 {
			return catalog, agentFailure("invalid_component_defaults", "cached native default file changed: "+name)
		}
	}
	return catalog, nil
}

func (s *agentProjectService) componentSchema(ctx context.Context, args map[string]any) (any, error) {
	name, err := componentDefaultsName(agentString(args, "componentType"))
	if err != nil {
		return nil, err
	}
	profile, hash, err := componentDefaultsProfile(ctx, s.project, s.editor)
	if err != nil {
		return nil, err
	}
	catalog, err := readAgentDefaultsCatalog(s.project, profile, hash)
	if err != nil {
		return nil, err
	}
	template := catalog.Templates[name]
	documents, err := goproject.ParseUnityYAML([]byte(template.Content))
	if err != nil {
		return nil, err
	}
	components := []any{}
	for _, document := range documents {
		if authoring.NativeComponentTypes()[document.ClassID] != "" || document.ClassID == 224 {
			components = append(components, map[string]any{"fileId": document.FileID, "classId": document.ClassID, "typeName": document.TypeName, "fields": document.Data})
		}
	}
	return map[string]any{"backend": "versioned_component_cache", "originBackend": "unity_editor_public_api", "componentType": name,
		"requestId": agentString(args, "requestId"), "profileSha256": hash, "unityVersion": profile.UnityVersion, "unityRevision": profile.UnityRevision,
		"templateSha256": template.SHA256, "components": components, "editorStarted": false,
		"templatePath": filepath.Join(agentDefaultsDirectory(s.project, hash), candidateTemplateName(name))}, nil
}

func (s *agentProjectService) componentAdd(ctx context.Context, args map[string]any) (any, error) {
	name, err := componentDefaultsName(agentString(args, "componentType"))
	if err != nil {
		return nil, err
	}
	profile, hash, err := componentDefaultsProfile(ctx, s.project, s.editor)
	if err != nil {
		return nil, err
	}
	catalog, err := readAgentDefaultsCatalog(s.project, profile, hash)
	if err != nil {
		return nil, err
	}
	store, err := authoring.NewStore(s.project)
	if err != nil {
		return nil, err
	}
	fields, _ := args["fields"].(map[string]any)
	result, err := store.AddNativeDefaults(ctx, authoring.NativeDefaultsRequest{Path: agentString(args, "path"), ExpectedRevision: agentString(args, "expectedRevision"),
		OperationID: agentString(args, "operationId"), FileID: agentString(args, "fileId"), TargetPath: agentString(args, "targetPath"),
		ComponentType: name, Fields: fields, Template: catalog.Templates[name], BeforePublish: s.componentDefaultsGuard(ctx, hash)})
	return map[string]any{"backend": "raw_source", "defaultsBackend": "versioned_component_cache", "profileSha256": hash,
		"editorStarted": false, "operationId": agentString(args, "operationId"), "publication": result}, err
}

func (s *agentProjectService) componentDefaultsGuard(ctx context.Context, expected string) func() error {
	return func() error {
		profile, actual, err := componentDefaultsProfile(ctx, s.project, s.editor)
		if err != nil {
			return err
		}
		if actual != expected {
			return agentFailure("component_defaults_changed", "native engine/settings/package defaults profile changed before source publication")
		}
		_, err = readAgentDefaultsCatalog(s.project, profile, actual)
		return err
	}
}

func candidateTemplateName(name string) string {
	return strings.TrimPrefix(name, "UnityEngine.") + ".unity"
}

// Cache bootstrap is explicit. Ordinary component_schema/component_add never
// execute this path, including when the version/context cache is missing.
func (s *agentProjectService) captureComponentDefaults(ctx context.Context) (agentDefaultsCatalog, error) {
	if s.noEditor {
		return agentDefaultsCatalog{}, agentFailure("editor_disabled", "native default capture is an explicit Editor bootstrap; this service forbids Editor execution")
	}
	if _, err := unitypackages.Resolve(ctx, s.project, s.editor, unitypackages.Options{}); err != nil {
		return agentDefaultsCatalog{}, err
	}
	profile, hash, err := componentDefaultsProfile(ctx, s.project, s.editor)
	if err != nil {
		return agentDefaultsCatalog{}, err
	}
	if cached, err := readAgentDefaultsCatalog(s.project, profile, hash); err == nil {
		return cached, nil
	} else {
		var code *authoring.Error
		if !errors.As(err, &code) || code.Code != "component_defaults_missing" {
			return cached, err
		}
	}
	directory := agentDefaultsDirectory(s.project, hash)
	if err := agentCanonical(directory); err != nil {
		return agentDefaultsCatalog{}, err
	}
	// Failed captures cannot pin all future attempts to stale gameplay source.
	// Profile identity stays version/context-bound; each capture owns a fresh
	// authored snapshot and independently preserved receipts.
	stage, snapshot, err := cloneAgentStaging(ctx, s.project, filepath.Join(directory, "capture-attempts", agentID()))
	if err != nil {
		return agentDefaultsCatalog{}, err
	}
	client, err := editorbridge.New(s.lifetime, editorbridge.Options{Project: stage, Editor: s.editor, StartupTimeout: 10 * time.Minute})
	if err != nil {
		return agentDefaultsCatalog{}, err
	}
	defer client.Close()
	result := agentDefaultsCatalog{SchemaVersion: 1, Profile: profile, ProfileSHA256: hash,
		Templates: map[string]authoring.NativeDefaultsTemplate{}, Receipts: map[string]string{}}
	encoded, _ := json.Marshal(snapshot.Files)
	result.SourceSHA256 = bytesSHA(encoded)
	names := []string{}
	for _, name := range authoring.NativeComponentTypes() {
		names = append(names, "UnityEngine."+name)
	}
	sort.Strings(names)
	for index, name := range names {
		args, _ := json.Marshal(map[string]any{"componentType": name})
		id := fmt.Sprintf("%s-%02d", hash[:32], index)
		response, callErr := client.Do(ctx, editorbridge.Request{ID: id, Action: "component_schema", Arguments: args})
		data, err := agentRequireResponse(response, callErr)
		if err != nil {
			return result, err
		}
		var captured struct {
			TemplatePath string `json:"templatePath"`
			UnityVersion string `json:"unityVersion"`
		}
		if err := json.Unmarshal(data, &captured); err != nil || captured.UnityVersion != profile.UnityVersion || !goproject.IsOwnedCachePath(stage, captured.TemplatePath) {
			return result, errors.New("component capture lacks exact native version/owned template identity")
		}
		if err := agentCanonical(captured.TemplatePath); err != nil {
			return result, err
		}
		content, err := readAgentFile(captured.TemplatePath, 32<<20)
		if err != nil {
			return result, err
		}
		template := authoring.NativeDefaultsTemplate{SchemaVersion: 1, Profile: hash, UnityVersion: profile.UnityVersion, UnityRevision: profile.UnityRevision,
			ComponentType: name, Content: string(content), SHA256: bytesSHA(content)}
		result.Templates[name], result.Receipts[name] = template, response.Receipt
		if err := agentImmutableFile(filepath.Join(directory, candidateTemplateName(name)), content); err != nil {
			return result, err
		}
	}
	if err := client.Close(); err != nil {
		return result, err
	}
	if err := verifyAgentSnapshot(ctx, snapshot); err != nil {
		return result, err
	}
	current, currentHash, err := componentDefaultsProfile(ctx, s.project, s.editor)
	if err != nil || currentHash != hash || !reflect.DeepEqual(current, profile) {
		return result, errors.Join(errors.New("native default profile changed during capture"), err)
	}
	if err := agentImmutableJSON(filepath.Join(directory, "catalog.json"), result); err != nil {
		return result, err
	}
	return readAgentDefaultsCatalog(s.project, profile, hash)
}

func agentImmutableFile(path string, contents []byte) error {
	if err := agentCanonical(path); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	if previous, err := readAgentFile(path, 32<<20); err == nil {
		if string(previous) != string(contents) {
			return agentFailure("immutable_cache_conflict", "existing component default template differs")
		}
		return nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(path), ".component-template-")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	if _, err = file.Write(contents); err != nil {
		file.Close()
		return err
	}
	if err = file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := os.Link(file.Name(), path); err != nil {
		if errors.Is(err, os.ErrExist) {
			previous, readErr := readAgentFile(path, 32<<20)
			if readErr == nil && string(previous) == string(contents) {
				return nil
			}
		}
		return err
	}
	parent, err := os.Open(filepath.Dir(path))
	if err != nil {
		return err
	}
	defer parent.Close()
	return parent.Sync()
}

func runAgentComponentDefaultsCLI(args []string) error {
	var project, editor string
	flags := flag.NewFlagSet("cache-component-defaults", flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit Unity project whose native profile defaults should be cached")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "matching installed Unity Editor used once in isolated capture staging")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if project == "" || flags.NArg() != 0 {
		return errors.New("cache-component-defaults requires an explicit --project")
	}
	var err error
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	project, err = filepath.Abs(project)
	if err != nil {
		return err
	}
	editor, err = filepath.EvalSymlinks(editor)
	if err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	service := &agentProjectService{project: project, editor: editor, lifetime: ctx}
	defer service.Close()
	value, err := service.locked(ctx, func() (any, error) { return service.captureComponentDefaults(ctx) })
	if err != nil {
		return err
	}
	catalog := value.(agentDefaultsCatalog)
	return json.NewEncoder(os.Stdout).Encode(map[string]any{"schemaVersion": 1, "backend": "versioned_component_cache", "profileSha256": catalog.ProfileSHA256,
		"catalog": filepath.Join(agentDefaultsDirectory(project, catalog.ProfileSHA256), "catalog.json"), "componentCount": len(catalog.Templates)})
}
