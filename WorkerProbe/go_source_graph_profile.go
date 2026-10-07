package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"

	goproject "unitymicroeditor/project"
	"unitymicroeditor/project/packages"
)

type goSourceEngineProfile struct {
	APICompatibility          int                               `json:"apiCompatibility"`
	SchemaVersion             int                               `json:"schemaVersion"`
	Backend                   string                            `json:"backend"`
	UnityVersion              string                            `json:"unityVersion"`
	UnityRevision             string                            `json:"unityRevision"`
	Editor                    string                            `json:"editor"`
	Target                    string                            `json:"target"`
	ScriptingBackend          string                            `json:"scriptingBackend"`
	Development               bool                              `json:"development"`
	Player                    string                            `json:"player"`
	PlayerSHA256              string                            `json:"playerSHA256"`
	ScriptingAssembliesSHA256 string                            `json:"scriptingAssembliesSHA256"`
	RuntimeInitializersSHA256 string                            `json:"runtimeInitializersSHA256"`
	BaseDefines               []string                          `json:"baseDefines"`
	BaseReferences            []string                          `json:"baseReferences"`
	EngineReferences          []string                          `json:"engineReferences"`
	Inputs                    map[string]string                 `json:"inputs"`
	NativeAssemblies          map[string]goSourceNativeAssembly `json:"nativeAssemblies"`
	ProfileSHA256             string                            `json:"profileSHA256"`
	SourceBuildProofSHA256    string                            `json:"sourceBuildProofSHA256"`
}

type goSourceNativeAssembly struct {
	UnresolvedReferences []string          `json:"unresolvedReferences,omitempty"`
	Name                 string            `json:"name"`
	Package              string            `json:"package,omitempty"`
	PackageContentHash   string            `json:"packageContentHash,omitempty"`
	SHA256               string            `json:"sha256"`
	Configuration        goAssemblyConfig  `json:"configuration"`
	SourceHashes         map[string]string `json:"sourceHashes"`
}

type goSourceGraphOptions struct {
	Project  string
	Editor   string
	Player   string
	Profile  *goSourceEngineProfile
	Packages *packages.Snapshot
}

func goSourceProfileHash(profile goSourceEngineProfile) (string, error) {
	profile.ProfileSHA256 = ""
	data, err := json.Marshal(profile)
	return bytesSHA(data), err
}

// The public reference is used once to record pinned engine compilation facts.
// Target source graphs are always derived from authored files and packages.
func captureGoSourceEngineProfile(ctx context.Context, project, editor, player, referenceGraph, output string, snapshot *packages.Snapshot) (*goSourceEngineProfile, error) {
	if err := validateGoSourceOutput(project, output); err != nil {
		return nil, err
	}
	if snapshot == nil || snapshot.Project != project || snapshot.Editor != editor {
		return nil, errors.New("engine profile requires an exact resolved package snapshot")
	}
	if err := snapshot.Validate(ctx); err != nil {
		return nil, err
	}
	var graph goCompilerGraphExport
	if err := readJSONFile(referenceGraph, &graph); err != nil {
		return nil, err
	}
	if graph.Project != project || graph.Unity != editor || graph.SchemaVersion != 1 {
		return nil, errors.New("public engine profile reference belongs to another project/Editor")
	}
	if graph.Backend != "unity_editor" || !graph.Development || graph.UnityVersion != fixtureUnityVersion || graph.Target != "StandaloneLinux64" || graph.ScriptingBackend != "Mono2x" {
		return nil, errors.New("engine profile requires an explicit public Development Linux Mono graph")
	}
	buildProofPath := filepath.Join(filepath.Dir(goSourceManagedDirectory(player)), "UnityMicroEditorSourceProfile.json")
	buildProof, err := readGoSourceBuildProof(buildProofPath)
	if err != nil {
		return nil, err
	}
	settings, err := readGoSourceSettings(project)
	if err != nil {
		return nil, err
	}
	profile := &goSourceEngineProfile{SchemaVersion: 1, Backend: "unity_public_engine_profile", UnityVersion: fixtureUnityVersion, UnityRevision: "45d8eee7de74", Editor: editor, Target: "LinuxStandalone64", ScriptingBackend: "Mono", Development: true, Player: player, Inputs: map[string]string{}, NativeAssemblies: map[string]goSourceNativeAssembly{}}
	profile.APICompatibility = settings.APICompatibility
	profile.SourceBuildProofSHA256, err = goFileHash(buildProofPath)
	if err != nil {
		return nil, err
	}
	profile.Inputs[buildProofPath] = profile.SourceBuildProofSHA256
	profile.PlayerSHA256, err = goFileHash(player)
	if err != nil {
		return nil, err
	}
	managed := goSourceManagedDirectory(player)
	profile.ScriptingAssembliesSHA256, err = goFileHash(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"))
	if err != nil {
		return nil, err
	}
	initializers := filepath.Join(filepath.Dir(managed), "RuntimeInitializeOnLoads.json")
	if hash, e := goFileHash(initializers); e == nil {
		profile.RuntimeInitializersSHA256 = hash
		profile.Inputs[initializers] = hash
	} else if !os.IsNotExist(e) {
		return nil, e
	}
	var defines []string
	allConfigs := []goAssemblyConfig{}
	for _, assembly := range graph.Assemblies {
		allConfigs = append(allConfigs, assembly.goAssemblyConfig)
		captureGoSourceProfileReferences(profile, editor, assembly.CompiledReferences)
	}
	for _, assembly := range graph.HostAssemblies {
		allConfigs = append(allConfigs, goAssemblyConfig{Name: assembly.Name, Sources: assembly.Sources, Defines: assembly.Defines})
		captureGoSourceProfileReferences(profile, editor, assembly.CompiledReferences)
	}
	if len(allConfigs) == 0 {
		return nil, errors.New("public profile has no Player define evidence")
	}
	defines = slices.Clone(allConfigs[0].Defines)
	for _, config := range allConfigs[1:] {
		defines = slices.DeleteFunc(defines, func(define string) bool { return !slices.Contains(config.Defines, define) })
	}
	defines = slices.DeleteFunc(defines, func(define string) bool {
		return slices.Contains(settings.Defines, define) || define == "ENABLE_INPUT_SYSTEM" || define == "ENABLE_LEGACY_INPUT_MANAGER" || strings.HasPrefix(define, "UNITY_EDITOR")
	})
	profile.BaseDefines = sortedGoSourceStrings(defines)
	profile.BaseReferences = sortedGoSourceStrings(profile.BaseReferences)
	profile.EngineReferences = sortedGoSourceStrings(profile.EngineReferences)
	if len(profile.BaseReferences) == 0 || len(profile.EngineReferences) == 0 {
		return nil, errors.New("public profile lacks exact BCL/engine reference evidence")
	}
	for _, path := range append(slices.Clone(profile.BaseReferences), profile.EngineReferences...) {
		hash, err := goFileHash(path)
		if err != nil {
			return nil, err
		}
		profile.Inputs[path] = hash
	}
	if !slices.Contains(profile.BaseDefines, "UNITY_STANDALONE_LINUX") || !slices.Contains(profile.BaseDefines, "ENABLE_MONO") {
		return nil, errors.New("public profile lacks pinned Linux Mono compilation symbols")
	}
	for _, exported := range graph.HostAssemblies {
		path := filepath.Join(managed, exported.Name+".dll")
		hash, err := goFileHash(path)
		if err != nil {
			return nil, fmt.Errorf("host engine profile lacks %s: %w", exported.Name, err)
		}
		fact := goSourceNativeAssembly{Name: exported.Name, SHA256: hash, Configuration: goAssemblyConfig{Name: exported.Name, Sources: slices.Clone(exported.Sources), Defines: slices.Clone(exported.Defines)}, SourceHashes: map[string]string{}}
		if exported.Configuration != nil {
			fact.Configuration = *exported.Configuration
			fact.Configuration.Sources = slices.Clone(exported.Configuration.Sources)
		}
		built, exists := buildProof.Assemblies[exported.Name]
		if !exists || built.Name != exported.Name || built.DLLSHA256 != hash || len(built.References) == 0 {
			return nil, errors.New("native host lacks exact compiled-source proof: " + exported.Name)
		}
		for index, source := range fact.Configuration.Sources {
			logical := goSourceLogicalPath(project, snapshot, source)
			if logical == "" {
				for _, root := range graph.SourceRoots {
					if withinDirectory(source, root.Source) {
						relative, _ := filepath.Rel(root.Source, source)
						logical = root.Path + "/" + filepath.ToSlash(relative)
						break
					}
				}
			}
			if logical == "" {
				return nil, errors.New("public native package source is outside resolved roots: " + source)
			}
			fact.Configuration.Sources[index] = logical
			for _, pkg := range snapshot.Packages {
				if strings.HasPrefix(logical, pkg.LogicalRoot+"/") {
					if fact.Package == "" {
						fact.Package = pkg.Name
						fact.PackageContentHash = pkg.ContentHash
					}
					relative := strings.TrimPrefix(logical, pkg.LogicalRoot+"/")
					current := filepath.Join(pkg.Path, filepath.FromSlash(relative))
					oldHash, oldErr := goFileHash(source)
					currentHash, currentErr := goFileHash(current)
					if oldErr != nil || currentErr != nil || oldHash != currentHash {
						return nil, errors.New("public native source differs from resolved package content: " + logical)
					}
					break
				}
			}
			for _, input := range []string{source, source + ".meta"} {
				if hash, e := goFileHash(input); e == nil {
					key := logical
					if strings.HasSuffix(input, ".meta") {
						key += ".meta"
					}
					fact.SourceHashes[key] = hash
					if built.Sources[key] != hash {
						return nil, errors.New("native host source-body proof differs from current public source: " + key)
					}
				} else if !os.IsNotExist(e) {
					return nil, e
				}
			}
		}
		for logical, expected := range built.Sources {
			path := goSourcePhysicalPath(project, snapshot, logical)
			if path == "" {
				return nil, errors.New("native source-build proof path is outside explicit source roots: " + logical)
			}
			if actual, err := goFileHash(path); err != nil || actual != expected {
				return nil, errors.New("native host source/config proof differs: " + logical)
			}
			fact.SourceHashes[logical] = expected
		}
		for logical, expected := range built.References {
			path := goSourcePhysicalPath(project, snapshot, logical)
			if path == "" && filepath.IsAbs(logical) && withinDirectory(logical, filepath.Join(filepath.Dir(editor), "Data")) {
				path = logical
			}
			if actual, err := goFileHash(path); err != nil || actual != expected {
				return nil, errors.New("native host compilation reference differs: " + logical)
			}
		}
		for _, path := range exported.CompiledReferences {
			logical := goSourceLogicalPath(project, snapshot, path)
			if logical == "" {
				logical = path
			}
			actual, err := goFileHash(path)
			if err != nil || built.References[logical] != actual {
				return nil, errors.New("public native reference is missing/different in source-build proof: " + path)
			}
		}
		for index, path := range fact.Configuration.ResponseFiles {
			logical := goSourceLogicalPath(project, snapshot, path)
			if logical == "" {
				return nil, errors.New("native response path outside declared package/source roots")
			}
			fact.Configuration.ResponseFiles[index] = logical
		}
		configuration, err := canonicalGoConfiguration(fact.Configuration)
		if err != nil {
			return nil, err
		}
		compiledConfiguration, err := canonicalGoConfiguration(built.Configuration)
		if err != nil {
			return nil, err
		}
		if string(configuration) != string(compiledConfiguration) {
			return nil, errors.New("native host compiled configuration differs from public graph: " + exported.Name)
		}
		profile.NativeAssemblies[exported.Name] = fact
		if exported.Name != "Dungeon.CLIPlay" {
			profile.Inputs[path] = hash
		}
	}
	if err := captureGoSourceOptionalNativeReferences(ctx, snapshot, profile); err != nil {
		return nil, err
	}
	managedDLLs, _ := filepath.Glob(filepath.Join(managed, "*.dll"))
	for _, path := range managedDLLs {
		name := strings.TrimSuffix(filepath.Base(path), ".dll")
		if profile.NativeAssemblies[name].Name != "" {
			continue
		}
		hash, err := goFileHash(path)
		if err != nil {
			return nil, err
		}
		profile.NativeAssemblies[name] = goSourceNativeAssembly{Name: name, SHA256: hash}
		if name != "Dungeon.CLIPlay" {
			profile.Inputs[path] = hash
		}
	}
	for _, pattern := range []string{filepath.Join(filepath.Dir(editor), "Data/Managed/UnityEngine/*.dll"), filepath.Join(filepath.Dir(editor), "Data/NetStandard/ref/2.1.0/*.dll")} {
		paths, _ := filepath.Glob(pattern)
		for _, path := range paths {
			hash, err := goFileHash(path)
			if err != nil {
				return nil, err
			}
			profile.Inputs[path] = hash
		}
	}
	profile.Inputs[filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json")] = profile.ScriptingAssembliesSHA256
	profile.ProfileSHA256, err = goSourceProfileHash(*profile)
	if err != nil {
		return nil, err
	}
	if err := agentImmutableJSON(output, profile); err != nil {
		return nil, err
	}
	return profile, nil
}

type goSourceBuildProof struct {
	SchemaVersion    int    `json:"schemaVersion"`
	Backend          string `json:"backend"`
	UnityVersion     string `json:"unityVersion"`
	Target           string `json:"target"`
	ScriptingBackend string `json:"scriptingBackend"`
	Development      bool   `json:"development"`
	Assemblies       map[string]struct {
		Name          string            `json:"name"`
		DLLSHA256     string            `json:"dllSha256"`
		Configuration goAssemblyConfig  `json:"configuration"`
		Sources       map[string]string `json:"sources"`
		References    map[string]string `json:"references"`
	} `json:"assemblies"`
}

func readGoSourceBuildProof(path string) (goSourceBuildProof, error) {
	var proof goSourceBuildProof
	if err := agentCanonical(path); err != nil {
		return proof, err
	}
	if err := agentReadJSON(path, 32<<20, &proof); err != nil {
		return proof, fmt.Errorf("native host requires fresh public source-build proof: %w", err)
	}
	if proof.SchemaVersion != 1 || proof.Backend != "unity_editor_build_sources" || proof.UnityVersion != fixtureUnityVersion || proof.Target != "StandaloneLinux64" || proof.ScriptingBackend != "Mono2x" || !proof.Development || len(proof.Assemblies) == 0 {
		return proof, errors.New("native source-build proof lacks pinned Development Linux Mono provenance")
	}
	return proof, nil
}
func goSourcePhysicalPath(project string, snapshot *packages.Snapshot, logical string) string {
	if filepath.ToSlash(filepath.Clean(logical)) != logical || filepath.IsAbs(logical) {
		return ""
	}
	var path string
	if strings.HasPrefix(logical, "Assets/") {
		path = filepath.Join(project, filepath.FromSlash(logical))
	}
	for _, pkg := range snapshot.Packages {
		if strings.HasPrefix(logical, pkg.LogicalRoot+"/") {
			path = filepath.Join(pkg.Path, filepath.FromSlash(strings.TrimPrefix(logical, pkg.LogicalRoot+"/")))
			break
		}
	}
	if path == "" || goSourceLogicalPath(project, snapshot, path) != logical || agentCanonical(path) != nil {
		return ""
	}
	return path
}

func validateGoSourceOutput(project, output string) error {
	if !filepath.IsAbs(output) || filepath.Clean(output) != output || !goproject.IsOwnedCachePath(project, output) {
		return errors.New("Go source output must be a strict child of owned project cache")
	}
	if err := agentCanonical(output); err != nil {
		return err
	}
	if _, err := os.Lstat(output); !errors.Is(err, os.ErrNotExist) {
		return errors.New("Go source output already exists; choose a new output")
	}
	return nil
}

func captureGoSourceProfileReferences(profile *goSourceEngineProfile, editor string, paths []string) {
	data := filepath.Join(filepath.Dir(editor), "Data")
	for _, path := range paths {
		if withinDirectory(path, filepath.Join(data, "NetStandard")) || withinDirectory(path, filepath.Join(data, "MonoBleedingEdge/lib/mono")) {
			profile.BaseReferences = append(profile.BaseReferences, path)
		} else if withinDirectory(path, filepath.Join(data, "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed")) {
			profile.EngineReferences = append(profile.EngineReferences, path)
		}
	}
}

// Unity's built-in packages can carry GUID references to optional, uninstalled
// packages. The already built public Player proves this exact unchanged native
// package assembly compiled without that reference. Record those exceptions
// explicitly; a new authored or modified package assembly cannot inherit them.
func captureGoSourceOptionalNativeReferences(ctx context.Context, snapshot *packages.Snapshot, profile *goSourceEngineProfile) error {
	definitions := map[string]goSourceAsmdef{}
	guids := map[string]bool{}
	for _, pkg := range snapshot.Packages {
		if err := filepath.WalkDir(pkg.Path, func(path string, entry os.DirEntry, err error) error {
			if err != nil {
				return err
			}
			if err := ctx.Err(); err != nil {
				return err
			}
			if entry.IsDir() {
				if path != pkg.Path && (strings.HasPrefix(entry.Name(), ".") || strings.HasSuffix(entry.Name(), "~")) {
					return filepath.SkipDir
				}
				return nil
			}
			if filepath.Ext(path) != ".asmdef" {
				return nil
			}
			content, err := readAgentFile(path, 1<<20)
			if err != nil {
				return err
			}
			var definition goSourceAsmdef
			if err := json.Unmarshal(content, &definition); err != nil {
				return err
			}
			guid, err := readGoMetadataGUID(path + ".meta")
			if err != nil {
				return err
			}
			guids[guid] = true
			definition.Package = pkg.Name
			definitions[definition.Name] = definition
			return nil
		}); err != nil {
			return err
		}
	}
	for name, fact := range profile.NativeAssemblies {
		definition := definitions[name]
		if fact.Package == "" || fact.Package != definition.Package || len(fact.SourceHashes) == 0 {
			continue
		}
		for _, reference := range definition.References {
			missing := strings.HasPrefix(reference, "GUID:") && !guids[strings.ToLower(strings.TrimPrefix(reference, "GUID:"))]
			if !strings.HasPrefix(reference, "GUID:") {
				_, exists := definitions[reference]
				missing = !exists && profile.NativeAssemblies[reference].Name == ""
			}
			if missing {
				fact.UnresolvedReferences = append(fact.UnresolvedReferences, reference)
			}
		}
		fact.UnresolvedReferences = sortedGoSourceStrings(fact.UnresolvedReferences)
		profile.NativeAssemblies[name] = fact
	}
	return nil
}

func loadGoSourceEngineProfile(ctx context.Context, filename, editor, player string) (*goSourceEngineProfile, error) {
	var profile goSourceEngineProfile
	if err := agentReadJSON(filename, 16<<20, &profile); err != nil {
		return nil, err
	}
	if profile.SchemaVersion != 1 || profile.Backend != "unity_public_engine_profile" || profile.UnityVersion != fixtureUnityVersion || profile.UnityRevision != "45d8eee7de74" || profile.Editor != editor || profile.Player != player || profile.Target != "LinuxStandalone64" || profile.ScriptingBackend != "Mono" || !profile.Development || len(profile.BaseDefines) == 0 || len(profile.Inputs) == 0 || len(profile.BaseReferences) == 0 || len(profile.EngineReferences) == 0 || !validSHA256(profile.SourceBuildProofSHA256) {
		return nil, errors.New("engine profile does not match pinned Linux Mono host")
	}
	hash, err := goSourceProfileHash(profile)
	if err != nil || hash != profile.ProfileSHA256 {
		return nil, errors.New("engine profile fingerprint differs")
	}
	for path, expected := range profile.Inputs {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		if actual, err := goFileHash(path); err != nil || actual != expected {
			return nil, errors.New("engine/native profile input changed: " + path)
		}
	}
	if actual, err := goFileHash(player); err != nil || actual != profile.PlayerSHA256 {
		return nil, errors.New("engine profile Player executable changed")
	}
	return &profile, nil
}

func goSourceManagedDirectory(player string) string {
	return filepath.Join(filepath.Dir(player), strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data", "Managed")
}
func sortedGoSourceStrings(values []string) []string {
	result := slices.Clone(values)
	sort.Strings(result)
	return slices.Compact(result)
}
func goSourceLogicalPath(project string, snapshot *packages.Snapshot, path string) string {
	if withinDirectory(path, filepath.Join(project, "Assets")) {
		relative, _ := filepath.Rel(project, path)
		return filepath.ToSlash(relative)
	}
	for _, pkg := range snapshot.Packages {
		if withinDirectory(path, pkg.Path) {
			relative, _ := filepath.Rel(pkg.Path, path)
			return pkg.LogicalRoot + "/" + filepath.ToSlash(relative)
		}
	}
	return ""
}

func goSourceProfileInputs(profile *goSourceEngineProfile) map[string]string {
	return maps.Clone(profile.Inputs)
}
