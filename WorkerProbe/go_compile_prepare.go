package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"

	goproject "unitymicroeditor/project"
	"unitymicroeditor/worker/internal/editorbridge"
)

// This graph is exported through Unity's public CompilationPipeline APIs.
// Preparation verifies the explicit project inputs and instruments sources
// with the installed SDK; it does not execute an Editor or game code.
type goCompilerGraphExport struct {
	UnityVersion            string                    `json:"unityVersion,omitempty"`
	Target                  string                    `json:"target,omitempty"`
	ScriptingBackend        string                    `json:"scriptingBackend,omitempty"`
	BaseReferences          []string                  `json:"baseReferences,omitempty"`
	EngineReferences        []string                  `json:"engineReferences,omitempty"`
	Development             bool                      `json:"development,omitempty"`
	Backend                 string                    `json:"backend,omitempty"`
	EngineProfileSHA256     string                    `json:"engineProfileSha256,omitempty"`
	PackageResolutionSHA256 string                    `json:"packageResolutionSha256,omitempty"`
	SchemaVersion           int                       `json:"schemaVersion"`
	Project                 string                    `json:"project"`
	Unity                   string                    `json:"unity"`
	Assemblies              []goCompilerGraphAssembly `json:"assemblies"`
	Scripts                 []goCompileScript         `json:"scripts"`
	CompilerInputs          map[string]string         `json:"compilerInputs"`
	HostAssemblies          []goCompilerHostAssembly  `json:"hostAssemblies,omitempty"`
	SourceRoots             []goCompilerSourceRoot    `json:"sourceRoots,omitempty"`
}

type goCompilerHostAssembly struct {
	CompiledReferences []string          `json:"compiledReferences,omitempty"`
	ResponseFiles      []string          `json:"responseFiles,omitempty"`
	Configuration      *goAssemblyConfig `json:"configuration,omitempty"`
	Name               string            `json:"name"`
	Path               string            `json:"path"`
	SHA256             string            `json:"sha256"`
	Sources            []string          `json:"sources"`
	Defines            []string          `json:"defines"`
	EditorDefines      []string          `json:"editorDefines"`
}
type goCompilerSourceRoot struct {
	Path   string `json:"path"`
	Source string `json:"source"`
}

type goCompilerGraphAssembly struct {
	ResponseFiles []string `json:"responseFiles,omitempty"`
	goAssemblyConfig
	Path               string   `json:"path"`
	SHA256             string   `json:"sha256"`
	References         []string `json:"references"`
	CompiledReferences []string `json:"compiledReferences"`
}

type goCompilerPreparationResult struct {
	SchemaVersion int               `json:"schemaVersion"`
	Player        string            `json:"player"`
	Compiler      goCompileManifest `json:"compiler"`
}

func canonicalGoConfiguration(config goAssemblyConfig) ([]byte, error) {
	data, err := json.Marshal(config)
	if err != nil {
		return nil, err
	}
	var canonical any
	if err := json.Unmarshal(data, &canonical); err != nil {
		return nil, err
	}
	return json.Marshal(canonical)
}

func prepareGoCompilerGraph(ctx context.Context, project, editor, player, graphFile, output string) (*goCompilerPreparationResult, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	var err error
	project, err = filepath.Abs(project)
	if err != nil {
		return nil, err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return nil, err
	}
	output, err = filepath.Abs(output)
	if err != nil {
		return nil, err
	}
	canonical, err := canonicalDestination(output)
	if err != nil || canonical != output || !goproject.IsOwnedCachePath(project, output) {
		return nil, errors.New("compiler preparation output must be under project Temp or Library/UnityMicroEditor without symlinks")
	}
	if _, err := os.Lstat(output); !errors.Is(err, os.ErrNotExist) {
		return nil, errors.New("compiler preparation output already exists; choose a new output path")
	}
	version, err := os.ReadFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"))
	if err != nil || editorbridge.ValidateProjectVersion(version) != nil {
		return nil, errors.New("compiler preparation requires the verified Unity " + fixtureUnityVersion + " project profile")
	}
	var graph goCompilerGraphExport
	if err := readJSONFile(graphFile, &graph); err != nil {
		return nil, err
	}
	if graph.SchemaVersion != 1 || graph.Project != project || graph.Unity != editor || len(graph.Assemblies)+len(graph.HostAssemblies) == 0 || len(graph.Assemblies)+len(graph.HostAssemblies) > 256 || len(graph.Scripts) > 65536 || len(graph.SourceRoots) > 4096 {
		return nil, errors.New("compiler export lacks an exact project/Unity runtime graph")
	}
	ownGraph := graph.Backend == "go_authored_source"
	if ownGraph && (!validSHA256(graph.EngineProfileSHA256) || !validSHA256(graph.PackageResolutionSHA256)) {
		return nil, errors.New("Go-authored source graph lacks engine/package profile fingerprints")
	}
	if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
		return nil, err
	}
	staging, err := os.MkdirTemp(filepath.Dir(output), ".compiler-preparation-")
	if err != nil {
		return nil, err
	}
	published := false
	defer func() {
		if !published {
			os.RemoveAll(staging)
		}
	}()
	playerFolder := filepath.Join(staging, "Player")
	if _, err := goCompilerCommand(ctx, "", "cp", "-a", "--reflink=auto", filepath.Dir(player), playerFolder); err != nil {
		return nil, err
	}
	stagedPlayer := filepath.Join(playerFolder, filepath.Base(player))
	managed := filepath.Join(playerFolder, strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data", "Managed")
	compiler := &goCompiler{project: project, editor: editor, player: stagedPlayer, cache: filepath.Join(staging, "compiler-cache"),
		configs: map[string]goAssemblyConfig{}, configurationHashes: map[string]string{}, configuration: map[string]string{},
		currentDLLs: map[string]string{}, expectedTypes: map[string]map[string]string{},
		manifest: goCompileManifest{Schema: 1, Project: project, Unity: editor, NativeOnly: len(graph.Assemblies) == 0, Scripts: graph.Scripts, CompilerInputs: maps.Clone(graph.CompilerInputs)}}
	if ownGraph {
		compiler.manifest.Backend = graph.Backend
		compiler.manifest.EngineProfileSHA256 = graph.EngineProfileSHA256
		compiler.manifest.PackageResolutionSHA256 = graph.PackageResolutionSHA256
		compiler.manifest.BaseReferences = slices.Clone(graph.BaseReferences)
		compiler.manifest.EngineReferences = slices.Clone(graph.EngineReferences)
	}
	if compiler.manifest.CompilerInputs == nil {
		compiler.manifest.CompilerInputs = map[string]string{}
	}
	if err := compiler.initializeSDK(); err != nil {
		return nil, err
	}
	if ownGraph {
		if len(graph.BaseReferences) == 0 || len(graph.EngineReferences) == 0 {
			return nil, errors.New("Go source compiler requires proven exact engine/BCL reference sets")
		}
		compiler.baseReferences, compiler.engineReferences = slices.Clone(graph.BaseReferences), slices.Clone(graph.EngineReferences)
		for _, path := range append(slices.Clone(graph.BaseReferences), graph.EngineReferences...) {
			if hash, err := goFileHash(path); err != nil || hash != graph.CompilerInputs[path] {
				return nil, errors.New("Go source engine/BCL reference lacks exact input proof: " + path)
			}
		}
	}
	if err := os.MkdirAll(compiler.cache, 0700); err != nil {
		return nil, err
	}
	if ownGraph {
		plugins := map[string]string{}
		for _, assembly := range graph.Assemblies {
			for _, path := range assembly.CompiledReferences {
				if !goSourceManagedPlugin(path) {
					return nil, errors.New("Go source precompiled reference is not managed PE metadata: " + path)
				}
				name := filepath.Base(path)
				hash, err := goFileHash(path)
				if err != nil || graph.CompilerInputs[path] != hash {
					return nil, errors.New("Go source plugin lacks exact current input proof: " + path)
				}
				if previous := plugins[name]; previous != "" && previous != hash {
					return nil, errors.New("Go source plugins disagree on assembly filename: " + name)
				}
				plugins[name] = hash
				destination := filepath.Join(managed, name)
				if existing, err := goFileHash(destination); err == nil && existing != hash {
					return nil, errors.New("precompiled plugin differs from native host; prepare a matching native package profile: " + name)
				} else if err != nil && !os.IsNotExist(err) {
					return nil, err
				}
				if _, err := os.Lstat(destination); os.IsNotExist(err) {
					if err := agentCanonical(path); err != nil {
						return nil, err
					}
					if _, err := goCompilerCommand(ctx, "", "cp", "--reflink=auto", "--", path, destination); err != nil {
						return nil, err
					}
					if err := agentCanonical(destination); err != nil {
						return nil, err
					}
					if actual, err := goFileHash(destination); err != nil || actual != hash {
						return nil, errors.New("plugin changed while staging: " + path)
					}
				}
				compiler.manifest.CompilerInputs[destination] = hash
			}
		}
		var slots []goCompileAssembly
		for name := range plugins {
			slots = append(slots, goCompileAssembly{Name: strings.TrimSuffix(name, ".dll")})
		}
		sort.Slice(slots, func(i, j int) bool { return slots[i].Name < slots[j].Name })
		if err := registerGoCompilerAssemblySlots(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"), slots); err != nil {
			return nil, err
		}
	}
	for _, exported := range graph.Assemblies {
		config := exported.goAssemblyConfig
		if len(exported.References) != 0 {
			if len(config.Dependencies) != 0 && !equalStrings(config.Dependencies, exported.References) {
				return nil, errors.New("compiler export has conflicting dependency lists: " + config.Name)
			}
			config.Dependencies = exported.References
		}
		if config.Name == "" || filepath.Base(config.Name) != config.Name || strings.HasPrefix(config.Name, "UnityEditor") ||
			compiler.configs[config.Name].Name != "" || len(config.Sources) == 0 || len(config.Defines) == 0 ||
			!ownGraph && (config.Config.OverrideReferences || len(config.Config.Precompiled) != 0) {
			return nil, errors.New("unsupported or duplicate runtime assembly configuration: " + config.Name)
		}
		for _, define := range config.Defines {
			if define == "UNITY_EDITOR" || strings.HasPrefix(define, "UNITY_EDITOR_") {
				return nil, errors.New("compiler export uses Editor-only defines: " + config.Name)
			}
		}
		sort.Strings(config.Sources)
		configuration, err := canonicalGoConfiguration(config)
		if err != nil {
			return nil, err
		}
		compiler.configs[config.Name], compiler.configurationHashes[config.Name] = config, bytesSHA(configuration)
		if !ownGraph {
			if actual, err := goFileHash(exported.Path); err != nil {
				return nil, fmt.Errorf("runtime export DLL %s: %w", exported.Path, err)
			} else if exported.SHA256 != "" && exported.SHA256 != actual {
				return nil, errors.New("runtime exported DLL hash differs: " + exported.Path)
			}
			compiler.currentDLLs[config.Name] = exported.Path
		} else {
			// A source graph deliberately has no Editor-produced gameplay DLL.
			// Nonempty sentinels keep native reference selection/order exact until
			// each SDK output is available.
			compiler.currentDLLs[config.Name] = "go-source-unbuilt:" + config.Name
		}
		compiler.manifest.Assemblies = append(compiler.manifest.Assemblies, goCompileAssembly{Name: config.Name, Sources: config.Sources, References: config.Dependencies})
	}
	ordered, err := orderGoCompilerAssemblies(compiler.manifest.Assemblies)
	if err != nil {
		return nil, err
	}
	compiler.manifest.Assemblies = ordered
	// Snapshot all baseline Assets scripts, including deliberately excluded
	// profiles. Future additions remain distinguishable from existing input.
	for _, root := range []string{"Assets", "ProjectSettings", "Packages"} {
		physical := filepath.Join(project, root)
		if _, err := os.Stat(physical); errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err := filepath.WalkDir(physical, func(filename string, entry fs.DirEntry, err error) error {
			if err != nil || entry.IsDir() {
				return err
			}
			extension := strings.ToLower(filepath.Ext(filename))
			tracked := root == "ProjectSettings" || extension == ".cs" || extension == ".asmdef" || extension == ".asmref" || extension == ".rsp" || extension == ".dll" ||
				strings.HasSuffix(filename, ".cs.meta") || strings.HasSuffix(filename, ".asmdef.meta") || strings.HasSuffix(filename, ".asmref.meta") || strings.HasSuffix(filename, ".dll.meta") ||
				filename == filepath.Join(project, "Packages/manifest.json") || filename == filepath.Join(project, "Packages/packages-lock.json") || filepath.Base(filename) == "package.json"
			if tracked {
				hash, err := goFileHash(filename)
				if err != nil {
					return err
				}
				if expected := compiler.manifest.CompilerInputs[filename]; expected != "" && expected != hash {
					return errors.New("compiler export is stale: " + filename)
				}
				compiler.manifest.CompilerInputs[filename] = hash
			}
			return nil
		}); err != nil {
			return nil, err
		}
	}
	snapshots, sourceHashes := map[string]string{}, map[string]string{}
	for _, assembly := range ordered {
		for _, source := range assembly.Sources {
			allowedRoot := withinDirectory(source, filepath.Join(project, "Assets")) || withinDirectory(source, filepath.Join(project, "Packages")) || withinDirectory(source, filepath.Join(project, "Library/PackageCache"))
			if ownGraph {
				for _, root := range graph.SourceRoots {
					if withinDirectory(source, root.Source) {
						allowedRoot = true
					}
				}
			}
			if !filepath.IsAbs(source) || !allowedRoot {
				return nil, errors.New("runtime source is outside explicit project/package roots: " + source)
			}
			data, err := os.ReadFile(source)
			if err != nil {
				return nil, err
			}
			hash := bytesSHA(data)
			if expected := compiler.manifest.CompilerInputs[source]; expected != "" && expected != hash {
				return nil, errors.New("compiler source export is stale: " + source)
			}
			snapshot := filepath.Join(staging, "Sources", hash, filepath.Base(source))
			if err := os.MkdirAll(filepath.Dir(snapshot), 0700); err != nil {
				return nil, err
			}
			if err := os.WriteFile(snapshot, data, 0600); err != nil {
				return nil, err
			}
			snapshots[source], sourceHashes[source], compiler.manifest.CompilerInputs[source] = snapshot, hash, hash
			if hash, err := goFileHash(source + ".meta"); err == nil {
				compiler.configuration[source+".meta"], compiler.manifest.CompilerInputs[source+".meta"] = hash, hash
			}
		}
	}
	compiler.collectConfigurationInputs()
	if err := compiler.validatePreparedScriptIdentities(); err != nil {
		return nil, err
	}
	for _, script := range compiler.manifest.Scripts {
		if script.TypeName != "" && !script.HostProvided {
			compiler.expectScriptType(script.Assembly, script.TypeName, script.GUID)
		}
	}
	helper, err := compiler.prepareLifecycleHelper(ctx)
	if err != nil {
		return nil, err
	}
	if ownGraph {
		if err := compiler.deriveGoSourceHostScriptTypes(ctx, helper, graph, managed); err != nil {
			return nil, err
		}
	} else if err := compiler.filterPreparedHostScripts(ctx, helper, graph); err != nil {
		return nil, err
	}
	if err := compiler.validatePreparedHostScripts(ctx, helper, managed, graph); err != nil {
		return nil, err
	}
	if err := compiler.approvePreparedNativeBases(managed); err != nil {
		return nil, err
	}
	if !ownGraph && compiler.manifest.NativeOnly {
		if err := compiler.validatePreparedHostBridge(ctx, helper, managed); err != nil {
			return nil, err
		}
	}
	for _, assembly := range ordered {
		if ownGraph {
			break
		}
		if slices.Contains(assembly.References, "Dungeon.CLIPlay") {
			if err := compiler.validatePreparedHostBridge(ctx, helper, managed); err != nil {
				return nil, err
			}
			break
		}
	}
	built := map[string]string{}
	hostReferences, _ := filepath.Glob(filepath.Join(managed, "*.dll"))
	for index, assembly := range ordered {
		refs := append([]string(nil), compiler.baseReferences...)
		if ownGraph {
			refs = append(refs, filepath.Join(managed, "Dungeon.CLIPlay.dll"))
		}
		if !compiler.configs[assembly.Name].Config.NoEngineReferences {
			refs = append(refs, compiler.engineReferences...)
		}
		for _, reference := range hostReferences {
			if ownGraph {
				continue
			}
			name := strings.TrimSuffix(filepath.Base(reference), ".dll")
			if compiler.currentDLLs[name] == "" {
				refs = append(refs, reference)
			}
		}
		for _, exported := range graph.Assemblies {
			if exported.Name != assembly.Name {
				continue
			}
			for _, reference := range exported.CompiledReferences {
				name := strings.TrimSuffix(filepath.Base(reference), ".dll")
				if strings.HasPrefix(name, "UnityEditor") {
					return nil, errors.New("Editor-only reference in runtime compiler export: " + reference)
				}
				if compiler.currentDLLs[name] == "" {
					refs = append(refs, reference)
				}
			}
		}
		for _, dependency := range assembly.References {
			if dll := built[dependency]; dll != "" {
				refs = append(refs, dll)
				continue
			}
			if compiler.currentDLLs[dependency] != "" {
				return nil, errors.New("runtime dependency was not built in order: " + dependency)
			}
			dll := filepath.Join(managed, dependency+".dll")
			if _, err := goFileHash(dll); err != nil {
				if ownGraph {
					return nil, fmt.Errorf("Go-authored runtime dependency %s requires a verified native profile or SDK source assembly", dependency)
				}
				// Unity reports project/package assembly references separately
				// from compiledReferences. This export profile places their
				// output DLLs in the explicit project's ScriptAssemblies.
				original := filepath.Join(project, "Library/ScriptAssemblies", dependency+".dll")
				if strings.HasPrefix(dependency, "UnityEditor") {
					return nil, errors.New("Editor-only runtime dependency: " + dependency)
				}
				if hash, err := goFileHash(original); err != nil {
					return nil, fmt.Errorf("runtime dependency %s has neither a host slot nor verified project DLL: %w", dependency, err)
				} else {
					compiler.manifest.CompilerInputs[original] = hash
				}
				data, err := os.ReadFile(original)
				if err != nil {
					return nil, err
				}
				if err := os.WriteFile(dll, data, 0600); err != nil {
					return nil, err
				}
				// The materialized runtime package requires its own Mono slot.
				if err := registerGoCompilerAssemblySlots(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"), []goCompileAssembly{{Name: dependency}}); err != nil {
					return nil, err
				}
			}
			refs = append(refs, dll)
		}
		refs = deduplicateGoReferences(refs)
		for _, reference := range refs {
			hash, err := goFileHash(reference)
			if err != nil {
				return nil, err
			}
			// Player-owned bridge references are independently staged on launch.
			if !compiler.hostBridgeReference(reference) && compiler.currentDLLs[strings.TrimSuffix(filepath.Base(reference), ".dll")] == "" {
				compiler.manifest.CompilerInputs[reference] = hash
			}
		}
		dll, proof, err := compiler.compileAssembly(ctx, staging, helper, assembly, refs, snapshots, sourceHashes)
		if err != nil {
			return nil, err
		}
		previousDLL := compiler.currentDLLs[assembly.Name]
		if ownGraph {
			previousDLL = ""
			if compiler.proofs == nil {
				compiler.proofs = map[string]goLifecycleProof{}
			}
			compiler.proofs[assembly.Name] = proof
		}
		if err := compiler.validateCompiledScriptTypes(ctx, helper, assembly.Name, previousDLL, dll); err != nil {
			return nil, err
		}
		built[assembly.Name] = dll
		assembly.Path = filepath.Join(managed, assembly.Name+".dll")
		assembly.SHA256, err = goFileHash(dll)
		if err != nil {
			return nil, err
		}
		assembly.AssemblyInputHash, assembly.LifecycleBarrier = filepath.Base(filepath.Dir(dll)), proof
		if goNeedsNetcode(assembly) && compiler.netcode != nil {
			assembly.NetcodePostprocessor = compiler.netcode.Key
		}
		compiler.manifest.Assemblies[index] = assembly
		if ownGraph {
			for scriptIndex, script := range compiler.manifest.Scripts {
				if !script.HostProvided && script.Assembly == assembly.Name {
					script.TypeName = proof.ScriptTypes[script.Path]
					compiler.manifest.Scripts[scriptIndex] = script
				}
			}
		}
		for _, extension := range []string{".dll", ".pdb"} {
			data, err := os.ReadFile(strings.TrimSuffix(dll, ".dll") + extension)
			if err != nil {
				return nil, err
			}
			if err := os.WriteFile(filepath.Join(managed, assembly.Name+extension), data, 0600); err != nil {
				return nil, err
			}
		}
		configuration, err := canonicalGoConfiguration(compiler.configs[assembly.Name])
		if err != nil {
			return nil, err
		}
		if err := atomicJSON(filepath.Join(filepath.Dir(dll), "compile.json"), map[string]any{
			"backend": compiler.manifest.Backend, "engineProfileSHA256": compiler.manifest.EngineProfileSHA256, "packageResolutionSHA256": compiler.manifest.PackageResolutionSHA256,
			"assemblyConfiguration": json.RawMessage(configuration), "compilerInputs": map[string]string{"assemblyConfiguration": bytesSHA(configuration)},
			"assemblies": []goCompileAssembly{assembly},
		}); err != nil {
			return nil, err
		}
	}
	if err := registerGoCompilerAssemblySlots(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"), ordered); err != nil {
		return nil, err
	}
	if ownGraph {
		if err := removeGoSourceNativeInitializers(filepath.Join(filepath.Dir(managed), "RuntimeInitializeOnLoads.json"), ordered); err != nil {
			return nil, err
		}
	}
	if compiler.manifest.NativeOnly {
		var registration struct {
			Names []string `json:"names"`
		}
		if err := readJSONFile(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"), &registration); err != nil || !slices.Contains(registration.Names, "Dungeon.CLIPlay.dll") {
			return nil, errors.New("native-only host lacks its runtime bridge registration")
		}
		compiler.manifest.NativeSlotsSHA, err = goFileHash(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"))
		if err != nil {
			return nil, err
		}
	}
	for filename, expected := range compiler.manifest.CompilerInputs {
		if expected == "absent" {
			if _, err := os.Lstat(filename); os.IsNotExist(err) {
				continue
			}
			return nil, errors.New("absent compiler input appeared during preparation: " + filename)
		}
		if actual, err := goFileHash(filename); err != nil || actual != expected {
			return nil, errors.New("compiler input changed during preparation; baseline not published: " + filename)
		}
	}
	result := &goCompilerPreparationResult{SchemaVersion: 1, Player: stagedPlayer, Compiler: compiler.manifest}
	if err := publishAgentBaseline(output, result); err != nil {
		return nil, err
	}
	published = true
	return result, nil
}

// A verified package MonoScript assembly may supply protected/public lifecycle
// methods to a gameplay subclass. The SDK generates a source-owned wrapper
// around those calls; it still rejects inaccessible private native callbacks.
// This runs only after source/profile, native type, slot and DLL digest proofs.
func (compiler *goCompiler) approvePreparedNativeBases(managed string) error {
	verified := map[string]bool{}
	for _, script := range compiler.manifest.Scripts {
		if !script.HostProvided {
			continue
		}
		host := filepath.Join(managed, script.Assembly+".dll")
		actual, err := goFileHash(host)
		if err != nil || compiler.manifest.CompilerInputs[host] == "" || actual != compiler.manifest.CompilerInputs[host] {
			return fmt.Errorf("native lifecycle base lacks its verified package DLL digest: %s", script.Assembly)
		}
		verified[script.Assembly] = true
	}
	for name, config := range compiler.configs {
		for _, claimed := range config.NativeBase {
			if !verified[claimed] || !slices.Contains(config.Dependencies, claimed) {
				return fmt.Errorf("native lifecycle base %s is outside the verified package dependencies of %s", claimed, name)
			}
		}
		config.NativeBase = nil
		for _, dependency := range config.Dependencies {
			if verified[dependency] && !slices.Contains(config.NativeBase, dependency) {
				config.NativeBase = append(config.NativeBase, dependency)
			}
		}
		sort.Strings(config.NativeBase)
		configuration, err := canonicalGoConfiguration(config)
		if err != nil {
			return err
		}
		compiler.configs[name], compiler.configurationHashes[name] = config, bytesSHA(configuration)
	}
	return nil
}

// GetClass returns the Editor's type even for runtime asmdef sources wrapped
// in UNITY_EDITOR. Resolve active declarations with the public Player profile
// before requiring native slots/types; an active declaration still must match
// the native host and cannot be ignored to hide a stale deployment.
func (compiler *goCompiler) filterPreparedHostScripts(ctx context.Context, helper string, graph goCompilerGraphExport) error {
	exported := map[string]goCompilerHostAssembly{}
	for _, assembly := range graph.HostAssemblies {
		validPath := assembly.Path == filepath.Join(compiler.project, "Library/ScriptAssemblies", assembly.Name+".dll")
		if graph.Backend == "go_authored_source" {
			validPath = filepath.IsAbs(assembly.Path) && filepath.Clean(assembly.Path) == assembly.Path
		}
		if assembly.Name == "" || filepath.Base(assembly.Name) != assembly.Name || strings.HasPrefix(assembly.Name, "UnityEditor") || exported[assembly.Name].Name != "" || !validPath {
			return errors.New("duplicate package assembly preprocessing profile")
		}
		if canonical, err := canonicalDestination(assembly.Path); err != nil || canonical != assembly.Path {
			return errors.New("exported package declaration assembly contains a symlink")
		}
		if expected, err := goFileHash(assembly.Path); err != nil || assembly.SHA256 == "" || expected != assembly.SHA256 {
			return errors.New("exported package declaration assembly digest differs: " + assembly.Name)
		}
		compiler.manifest.CompilerInputs[assembly.Path] = assembly.SHA256
		exported[assembly.Name] = assembly
	}
	type syntaxSource struct {
		Path         string `json:"path"`
		OriginalPath string `json:"originalPath"`
	}
	type syntaxRequest struct {
		AssemblyName  string         `json:"assemblyName"`
		Defines       []string       `json:"defines"`
		EditorDefines []string       `json:"editorDefines"`
		Sources       []syntaxSource `json:"sources"`
	}
	requests := map[string]*syntaxRequest{}
	for _, script := range compiler.manifest.Scripts {
		if !script.HostProvided {
			continue
		}
		assembly := exported[script.Assembly]
		if assembly.Name == "" || !slices.Contains(assembly.Sources, script.Path) || len(assembly.Defines) == 0 || !slices.Contains(assembly.EditorDefines, "UNITY_EDITOR") {
			return errors.New("package MonoScript lacks exact Player sources/defines; export a fresh graph: " + script.Path)
		}
		if len(assembly.Sources) > 65536 || len(assembly.Defines) > 4096 || len(assembly.EditorDefines) > 4096 {
			return errors.New("package preprocessing profile exceeds its input bound")
		}
		registered := false
		for _, root := range graph.SourceRoots {
			if withinDirectory(script.Path, root.Source) {
				registered = true
			}
		}
		if !registered {
			return errors.New("package preprocessing source is outside registered package roots: " + script.Path)
		}
		for _, define := range assembly.Defines {
			if define == "UNITY_EDITOR" || strings.HasPrefix(define, "UNITY_EDITOR_") {
				return errors.New("package Player preprocessing contains Editor-only defines: " + script.Assembly)
			}
		}
		if canonical, err := canonicalDestination(script.Path); err != nil || canonical != script.Path {
			return errors.New("package MonoScript source contains a symlink: " + script.Path)
		}
		if info, err := os.Stat(script.Path); err != nil || !info.Mode().IsRegular() || info.Size() > 16*1024*1024 {
			return errors.New("package MonoScript source is not a bounded regular file: " + script.Path)
		}
		data, err := os.ReadFile(script.Path)
		if err != nil || compiler.manifest.CompilerInputs[script.Path] == "" || bytesSHA(data) != compiler.manifest.CompilerInputs[script.Path] {
			return errors.New("package source differs from its exported preprocessing digest: " + script.Path)
		}
		snapshot := filepath.Join(compiler.cache, "HostSources", bytesSHA(data), filepath.Base(script.Path))
		if err := os.MkdirAll(filepath.Dir(snapshot), 0700); err != nil {
			return err
		}
		if err := os.WriteFile(snapshot, data, 0600); err != nil {
			return err
		}
		request := requests[assembly.Name]
		if request == nil {
			request = &syntaxRequest{AssemblyName: assembly.Name, Defines: assembly.Defines, EditorDefines: assembly.EditorDefines}
			requests[assembly.Name] = request
		}
		request.Sources = append(request.Sources, syntaxSource{Path: snapshot, OriginalPath: script.Path})
	}
	if len(requests) == 0 {
		return nil
	}
	var ordered []*syntaxRequest
	for _, request := range requests {
		ordered = append(ordered, request)
	}
	sort.Slice(ordered, func(i, j int) bool { return ordered[i].AssemblyName < ordered[j].AssemblyName })
	requestPath := filepath.Join(compiler.cache, "host-source-declarations.json")
	if err := atomicJSON(requestPath, ordered); err != nil {
		return err
	}
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, helper, "--source-type-declarations", requestPath)
	if err != nil {
		return fmt.Errorf("Player package preprocessing proof: %w", err)
	}
	var declarations map[string]struct {
		Player map[string][]string `json:"player"`
		Editor map[string][]string `json:"editor"`
	}
	if err := json.Unmarshal(output, &declarations); err != nil || len(declarations) != len(requests) {
		return errors.New("Player package preprocessing lacks exact assembly proof")
	}
	kept := compiler.manifest.Scripts[:0]
	for _, script := range compiler.manifest.Scripts {
		if script.HostProvided {
			proof := declarations[script.Assembly]
			editorNames, editorFound := proof.Editor[script.Path]
			if !editorFound || !slices.Contains(editorNames, script.TypeName) {
				return errors.New("exported package MonoScript type lacks its exact Editor source declaration: " + script.TypeName)
			}
			names, found := proof.Player[script.Path]
			if !found {
				return errors.New("Player preprocessing omitted package source: " + script.Path)
			}
			if !slices.Contains(names, script.TypeName) {
				continue
			}
		}
		kept = append(kept, script)
	}
	compiler.manifest.Scripts = kept
	return nil
}

// Game code must compile against the bridge that will actually run in Player.
// Editor DLLs can contain newer public APIs than an older prepared native host;
// replacing the Player bridge with an Editor-compiled DLL would conceal drift.
func (compiler *goCompiler) validatePreparedHostBridge(ctx context.Context, helper, managed string) error {
	host := filepath.Join(managed, "Dungeon.CLIPlay.dll")
	if _, err := goFileHash(host); err != nil {
		return fmt.Errorf("native host lacks its runtime bridge; rebuild the matching host before compiler preparation: %w", err)
	}
	// Public Editor graphs report this project/package assembly by name rather
	// than in compiledReferences. SDK-only exported fixtures need no Editor DLL.
	editor := filepath.Join(compiler.project, "Library/ScriptAssemblies/Dungeon.CLIPlay.dll")
	before, err := goFileHash(editor)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, helper, "--public-type-definitions", host, editor)
	if err != nil {
		return fmt.Errorf("verify native host runtime API: %w", err)
	}
	var types map[string][]string
	if err := json.Unmarshal(output, &types); err != nil || len(types) != 2 || types[host] == nil || types[editor] == nil {
		return errors.New("native host runtime API inventory lacks exact assembly metadata")
	}
	if after, err := goFileHash(editor); err != nil || after != before {
		return errors.New("Editor runtime bridge changed during host API verification; export a fresh graph")
	}
	available := map[string]bool{}
	for _, name := range types[host] {
		available[name] = true
	}
	for _, name := range types[editor] {
		if !available[name] {
			return fmt.Errorf("prepared native host runtime bridge is stale: public type %s exists in Editor but is absent from Player; rebuild the matching host before compiler preparation", name)
		}
	}
	return nil
}

func (compiler *goCompiler) validatePreparedScriptIdentities() error {
	sourceAssemblies := compiler.graphSourceAssemblies()
	bySource, byGUID := map[string]bool{}, map[string]string{}
	for _, script := range compiler.manifest.Scripts {
		if script.GUID == "" || !script.HostProvided && sourceAssemblies[script.Path] != script.Assembly || bySource[script.Path] {
			return errors.New("exported MonoScript has no exact unique runtime source/assembly: " + script.Path)
		}
		if canonical, err := canonicalDestination(script.Path + ".meta"); err != nil || canonical != script.Path+".meta" {
			return errors.New("MonoScript metadata path contains a symlink: " + script.Path)
		}
		if info, err := os.Stat(script.Path + ".meta"); err != nil || !info.Mode().IsRegular() || info.Size() > 1024*1024 {
			return errors.New("MonoScript metadata is not a bounded regular file: " + script.Path)
		}
		data, err := os.ReadFile(script.Path + ".meta")
		if err != nil {
			return fmt.Errorf("runtime MonoScript metadata %s: %w", script.Path, err)
		}
		guid, err := registryMetaGUID(data)
		if err != nil || guid != script.GUID || byGUID[guid] != "" {
			return errors.New("exported MonoScript GUID differs from authored metadata or is duplicated: " + script.Path)
		}
		bySource[script.Path], byGUID[guid] = true, script.Path
	}
	var paths []string
	for source := range sourceAssemblies {
		paths = append(paths, source)
	}
	sort.Strings(paths)
	for _, source := range paths {
		if bySource[source] {
			continue
		}
		data, err := os.ReadFile(source + ".meta")
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return fmt.Errorf("runtime MonoScript metadata %s: %w", source, err)
		}
		guid, err := registryMetaGUID(data)
		if err != nil || byGUID[guid] != "" {
			return errors.New("runtime source MonoScript identity is invalid or duplicated: " + source)
		}
		compiler.manifest.Scripts = append(compiler.manifest.Scripts, goCompileScript{GUID: guid, Path: source, Assembly: sourceAssemblies[source]})
		byGUID[guid] = source
	}
	sort.Slice(compiler.manifest.Scripts, func(i, j int) bool { return compiler.manifest.Scripts[i].Path < compiler.manifest.Scripts[j].Path })
	return nil
}

// Package MonoScripts use their native host's defining assembly. Editor and
// Player DLL bytes legitimately differ because of UNITY_EDITOR, so verify the
// exported Editor digest, the source/meta identity, and the exact native type
// definition separately; retain the actual host DLL digest in the baseline.
func (compiler *goCompiler) validatePreparedHostScripts(ctx context.Context, helper, managed string, graph goCompilerGraphExport) error {
	var registration struct {
		Names []string `json:"names"`
	}
	if err := readJSONFile(filepath.Join(filepath.Dir(managed), "ScriptingAssemblies.json"), &registration); err != nil {
		return fmt.Errorf("native host scripting registrations: %w", err)
	}
	for _, root := range graph.SourceRoots {
		if len(strings.Split(root.Path, "/")) != 2 || !strings.HasPrefix(root.Path, "Packages/") || filepath.ToSlash(filepath.Clean(root.Path)) != root.Path || !filepath.IsAbs(root.Source) {
			return errors.New("invalid compiler package source root")
		}
		if canonical, err := canonicalDestination(root.Source); err != nil || canonical != root.Source {
			return errors.New("compiler package source root contains a symlink")
		}
		if info, err := os.Stat(root.Source); err != nil || !info.IsDir() {
			return errors.New("compiler package source root is absent")
		}
	}
	exported := map[string]goCompilerHostAssembly{}
	for _, assembly := range graph.HostAssemblies {
		validPath := assembly.Path == filepath.Join(compiler.project, "Library/ScriptAssemblies", assembly.Name+".dll")
		if graph.Backend == "go_authored_source" {
			validPath = filepath.IsAbs(assembly.Path) && filepath.Clean(assembly.Path) == assembly.Path
		}
		if assembly.Name == "" || filepath.Base(assembly.Name) != assembly.Name || strings.HasPrefix(assembly.Name, "UnityEditor") || exported[assembly.Name].Name != "" || !validPath {
			return errors.New("invalid or duplicate exported host assembly identity")
		}
		exported[assembly.Name] = assembly
	}
	byAssembly := map[string]string{}
	for _, script := range compiler.manifest.Scripts {
		if !script.HostProvided {
			continue
		}
		assembly := exported[script.Assembly]
		if assembly.Name == "" || script.TypeName == "" {
			return fmt.Errorf("package MonoScript lacks a public defining assembly/type: %s", script.Path)
		}
		registered := false
		for _, root := range graph.SourceRoots {
			if !strings.HasPrefix(root.Path, "Packages/") || filepath.ToSlash(filepath.Clean(root.Path)) != root.Path || !filepath.IsAbs(root.Source) {
				return errors.New("invalid compiler package source root")
			}
			if withinDirectory(script.Path, root.Source) {
				registered = true
			}
		}
		if !registered {
			return errors.New("package MonoScript is outside its registered public source roots: " + script.Path)
		}
		for _, path := range []string{script.Path, script.Path + ".meta"} {
			canonical, err := canonicalDestination(path)
			if err != nil || canonical != path {
				return errors.New("package MonoScript source contains a symlink: " + path)
			}
			hash, err := goFileHash(path)
			if err != nil || compiler.manifest.CompilerInputs[path] == "" || hash != compiler.manifest.CompilerInputs[path] {
				return errors.New("package MonoScript source/meta differs from exported digest: " + path)
			}
		}
		if byAssembly[script.Assembly] != "" {
			continue
		}
		if !slices.Contains(registration.Names, assembly.Name+".dll") {
			return fmt.Errorf("native host package assembly %s has no native Mono slot; build matching package/profile host before preparation", assembly.Name)
		}
		if expected, err := goFileHash(assembly.Path); err != nil || assembly.SHA256 == "" || expected != assembly.SHA256 {
			return errors.New("exported package assembly digest differs: " + assembly.Name)
		}
		host := filepath.Join(managed, assembly.Name+".dll")
		hash, err := goFileHash(host)
		if err != nil {
			return fmt.Errorf("native host lacks package assembly %s; build matching package/profile host before preparation: %w", assembly.Name, err)
		}
		compiler.manifest.CompilerInputs[host] = hash
		byAssembly[script.Assembly] = host
	}
	if len(byAssembly) == 0 {
		return nil
	}
	paths := make([]string, 0, len(byAssembly))
	for _, path := range byAssembly {
		paths = append(paths, path)
	}
	sort.Strings(paths)
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, append([]string{helper, "--type-definitions"}, paths...)...)
	if err != nil {
		return fmt.Errorf("verify native package types: %w", err)
	}
	var definitions map[string][]string
	if err := json.Unmarshal(output, &definitions); err != nil || len(definitions) != len(paths) {
		return errors.New("native package type proof lacks exact assembly metadata")
	}
	for _, script := range compiler.manifest.Scripts {
		if script.HostProvided && !slices.Contains(definitions[byAssembly[script.Assembly]], script.TypeName) {
			return fmt.Errorf("native host package assembly %s lacks exported MonoScript type %s; rebuild the matching host", script.Assembly, script.TypeName)
		}
	}
	return nil
}

func orderGoCompilerAssemblies(assemblies []goCompileAssembly) ([]goCompileAssembly, error) {
	byName := map[string]goCompileAssembly{}
	for _, assembly := range assemblies {
		if _, exists := byName[assembly.Name]; exists {
			return nil, errors.New("duplicate runtime assembly: " + assembly.Name)
		}
		byName[assembly.Name] = assembly
	}
	state := map[string]int{}
	var ordered []goCompileAssembly
	var visit func(string) error
	visit = func(name string) error {
		if state[name] == 2 {
			return nil
		}
		if state[name] == 1 {
			return errors.New("cyclic runtime assembly graph: " + name)
		}
		state[name] = 1
		assembly := byName[name]
		for _, dependency := range assembly.References {
			if _, managed := byName[dependency]; managed {
				if err := visit(dependency); err != nil {
					return err
				}
			}
		}
		state[name] = 2
		ordered = append(ordered, assembly)
		return nil
	}
	for _, assembly := range assemblies {
		if err := visit(assembly.Name); err != nil {
			return nil, err
		}
	}
	return ordered, nil
}

func registerGoCompilerAssemblySlots(filename string, assemblies []goCompileAssembly) error {
	var doc map[string]json.RawMessage
	if err := readJSONFile(filename, &doc); err != nil {
		return err
	}
	var names []string
	var types []int
	if json.Unmarshal(doc["names"], &names) != nil || json.Unmarshal(doc["types"], &types) != nil || len(names) != len(types) || len(names) > 4096 {
		return errors.New("host Mono assembly registration lacks exact matching names/types")
	}
	seen := map[string]bool{}
	for _, name := range names {
		if seen[name] {
			return errors.New("host Mono registration has duplicate assembly slot: " + name)
		}
		seen[name] = true
	}
	for _, assembly := range assemblies {
		name := assembly.Name + ".dll"
		if !seen[name] {
			names, types = append(names, name), append(types, 16)
			seen[name] = true
		}
	}
	doc["names"], _ = json.Marshal(names)
	doc["types"], _ = json.Marshal(types)
	return atomicJSON(filename, doc)
}
