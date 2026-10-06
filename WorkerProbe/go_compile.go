package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"
)

type runtimeInitializer struct {
	Assembly   string `json:"assembly"`
	TypeName   string `json:"typeName"`
	MethodName string `json:"methodName"`
	Phase      int    `json:"phase"`
}

type compileUpdate struct {
	Player              string               `json:"player"`
	Assemblies          []string             `json:"assemblies"`
	RuntimeInitializers []runtimeInitializer `json:"runtimeInitializers"`
	CompiledAssemblies  []string             `json:"compiledAssemblies"`
	ElapsedSeconds      float64              `json:"elapsedSeconds"`
	SourceHashes        map[string]string    `json:"sourceHashes"`
	DeclarationChanged  []string             `json:"declarationChanged"`
	CacheReused         bool                 `json:"cacheReused,omitempty"`
}

type goCompileAssembly struct {
	Name              string           `json:"name"`
	Path              string           `json:"path"`
	SHA256            string           `json:"sha256"`
	Sources           []string         `json:"sources"`
	References        []string         `json:"references"`
	AssemblyInputHash string           `json:"assemblyInputHash"`
	LifecycleBarrier  goLifecycleProof `json:"lifecycleBarrier"`
}

type goLifecycleProof struct {
	SchemaVersion           int                   `json:"schemaVersion"`
	Verified                bool                  `json:"verified"`
	Phases                  []string              `json:"phases"`
	ScriptableObjectBarrier bool                  `json:"scriptableObjectBarrier"`
	RuntimeInitializers     []runtimeInitializer  `json:"runtimeInitializers"`
	Callbacks               int                   `json:"callbacks"`
	Methods                 []goLifecycleCallback `json:"methods"`
}

type goLifecycleCallback struct {
	Type      string
	Phase     string
	Source    string
	Order     int
	Synthetic bool
}

type goCompileManifest struct {
	Schema         int                 `json:"schema"`
	Project        string              `json:"project"`
	Unity          string              `json:"unity"`
	Assemblies     []goCompileAssembly `json:"assemblies"`
	CompilerInputs map[string]string   `json:"compilerInputs"`
	Scripts        []struct {
		GUID         string `json:"guid"`
		Path         string `json:"path"`
		Assembly     string `json:"assembly"`
		TypeName     string `json:"typeName"`
		HostProvided bool   `json:"hostProvided"`
	} `json:"scripts"`
}

type goAssemblyConfig struct {
	Name         string   `json:"name"`
	Sources      []string `json:"sources"`
	Dependencies []string `json:"dependencies"`
	Defines      []string `json:"defines"`
	NativeBase   []string `json:"nativeBaseAssemblies"`
	Config       struct {
		AllowUnsafe        bool     `json:"allowUnsafeCode"`
		NoEngineReferences bool     `json:"noEngineReferences"`
		OverrideReferences bool     `json:"overrideReferences"`
		Precompiled        []string `json:"precompiledReferences"`
	} `json:"config"`
}

type goCompiler struct {
	project, editor, cache, player, dotnet, csc, helper string
	manifest                                            goCompileManifest
	configs                                             map[string]goAssemblyConfig
	configurationHashes                                 map[string]string
	sources, configuration, currentDLLs                 map[string]string
	proofs                                              map[string]goLifecycleProof
	baseReferences, engineReferences, generators        []string
	hostRoots                                           []string
	expectedTypes                                       map[string]map[string]string
	netcode                                             *goNetcodeProcessor
	netcodeProcessed                                    map[string]bool
	baselinePlayer                                      string
	startupComplete                                     bool
}

func goFileHash(filename string) (string, error) {
	file, err := os.Open(filename)
	if err != nil {
		return "", err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Size() > 1024*1024*1024 {
		return "", errors.New("compiler input is not a bounded regular file: " + filename)
	}
	hasher := sha256.New()
	if _, err := io.Copy(hasher, io.LimitReader(file, 1024*1024*1024+1)); err != nil {
		return "", err
	}
	return hex.EncodeToString(hasher.Sum(nil)), nil
}

func goCompilerManifest(filename string) (goCompileManifest, string, error) {
	var wrapper struct {
		Compiler json.RawMessage `json:"compiler"`
		Manifest string          `json:"manifest"`
	}
	data, err := os.ReadFile(filename)
	if err != nil {
		return goCompileManifest{}, "", err
	}
	if err := json.Unmarshal(data, &wrapper); err != nil {
		return goCompileManifest{}, "", err
	}
	if len(wrapper.Compiler) > 0 && string(wrapper.Compiler) != "null" {
		data = wrapper.Compiler
	} else {
		var header struct {
			Schema int `json:"schema"`
		}
		json.Unmarshal(data, &header)
		if header.Schema != 1 {
			if wrapper.Manifest != "" {
				filename = wrapper.Manifest
			}
			filename = filepath.Join(filepath.Dir(filename), "Managed", "compile.json")
			data, err = os.ReadFile(filename)
			if err != nil {
				return goCompileManifest{}, "", err
			}
		}
	}
	var manifest goCompileManifest
	if err := json.Unmarshal(data, &manifest); err != nil {
		return manifest, "", err
	}
	if manifest.Schema != 1 || len(manifest.Assemblies) == 0 || len(manifest.Assemblies) > 256 || len(manifest.CompilerInputs) == 0 {
		return manifest, "", errors.New("baseline lacks a verified raw compiler plan")
	}
	return manifest, filename, nil
}

func readGoAssemblyConfiguration(baselinePath string, assembly goCompileAssembly) (goAssemblyConfig, string, error) {
	var configuration goAssemblyConfig
	for folder := filepath.Dir(baselinePath); ; folder = filepath.Dir(folder) {
		filename := filepath.Join(folder, "compiler-cache", "assemblies", assembly.AssemblyInputHash, "compile.json")
		data, err := os.ReadFile(filename)
		if err == nil {
			var descriptor struct {
				AssemblyConfiguration json.RawMessage     `json:"assemblyConfiguration"`
				CompilerInputs        map[string]string   `json:"compilerInputs"`
				Assemblies            []goCompileAssembly `json:"assemblies"`
			}
			if err := json.Unmarshal(data, &descriptor); err != nil {
				return configuration, "", err
			}
			if err := json.Unmarshal(descriptor.AssemblyConfiguration, &configuration); err != nil {
				return configuration, "", err
			}
			var canonical any
			decoder := json.NewDecoder(bytes.NewReader(descriptor.AssemblyConfiguration))
			decoder.UseNumber()
			if err := decoder.Decode(&canonical); err != nil {
				return configuration, "", err
			}
			encoded, _ := json.Marshal(canonical)
			digest := bytesSHA(encoded)
			if configuration.Name != assembly.Name || !equalStrings(configuration.Sources, assembly.Sources) ||
				!equalStrings(configuration.Dependencies, assembly.References) || descriptor.CompilerInputs["assemblyConfiguration"] != digest ||
				len(descriptor.Assemblies) != 1 || descriptor.Assemblies[0].SHA256 != assembly.SHA256 ||
				configuration.Config.OverrideReferences || len(configuration.Config.Precompiled) != 0 || len(configuration.Defines) == 0 {
				return configuration, "", errors.New("baseline assembly configuration/proof mismatch: " + assembly.Name)
			}
			return configuration, digest, nil
		}
		if !errors.Is(err, os.ErrNotExist) {
			return configuration, "", err
		}
		if filepath.Dir(folder) == folder {
			break
		}
	}
	return configuration, "", errors.New("exact assembly configuration cache is missing: " + assembly.Name)
}

func equalStrings(first, second []string) bool {
	if len(first) != len(second) {
		return false
	}
	for index := range first {
		if first[index] != second[index] {
			return false
		}
	}
	return true
}

func validGoLifecycleProof(proof goLifecycleProof, name string) bool {
	phases := append([]string(nil), proof.Phases...)
	sort.Strings(phases)
	if proof.SchemaVersion != 1 || !proof.Verified || !proof.ScriptableObjectBarrier ||
		!equalStrings(phases, []string{"Awake", "OnDestroy", "OnDisable", "OnEnable"}) {
		return false
	}
	for _, initializer := range proof.RuntimeInitializers {
		if initializer.Assembly != name || initializer.TypeName == "" || initializer.MethodName == "" || initializer.Phase < 0 || initializer.Phase > 4 {
			return false
		}
	}
	if proof.Callbacks != len(proof.Methods) {
		return false
	}
	for _, method := range proof.Methods {
		if method.Type == "" || method.Source == "" || (method.Phase != "Awake" && method.Phase != "OnEnable" && method.Phase != "OnDisable" && method.Phase != "OnDestroy") {
			return false
		}
	}
	return true
}

func goLifecycleSourcesMatch(proof goLifecycleProof, sources []string) bool {
	for _, callback := range proof.Methods {
		found := false
		for _, source := range sources {
			if callback.Source == source {
				found = true
				break
			}
		}
		if !found {
			return false
		}
	}
	return true
}

func newGoCompiler(project, baselineManifest, stagedPlayer, editor, cache string) (*goCompiler, error) {
	project, err := filepath.Abs(project)
	if err != nil {
		return nil, err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return nil, err
	}
	cache, err = filepath.Abs(cache)
	if err != nil {
		return nil, err
	}
	canonicalCache, err := canonicalDestination(cache)
	if err != nil || canonicalCache != cache || !withinDirectory(cache, filepath.Join(project, "Temp")) {
		return nil, errors.New("Go compiler cache must be under owned project Temp")
	}
	manifest, baselinePath, err := goCompilerManifest(baselineManifest)
	if err != nil {
		return nil, err
	}
	if manifest.Project != project || manifest.Unity != editor {
		return nil, errors.New("compiler baseline project or Unity SDK differs from launch")
	}
	compiler := &goCompiler{project: project, editor: editor, cache: cache, player: stagedPlayer, baselinePlayer: stagedPlayer, manifest: manifest,
		configs: map[string]goAssemblyConfig{}, configurationHashes: map[string]string{}, sources: map[string]string{},
		configuration: map[string]string{}, currentDLLs: map[string]string{}, proofs: map[string]goLifecycleProof{}}
	playerData := filepath.Join(filepath.Dir(stagedPlayer), strings.TrimSuffix(filepath.Base(stagedPlayer), filepath.Ext(stagedPlayer))+"_Data")
	var scriptingSlots struct {
		Names []string `json:"names"`
	}
	if err := readJSONFile(filepath.Join(playerData, "ScriptingAssemblies.json"), &scriptingSlots); err != nil {
		return nil, fmt.Errorf("registered Mono assembly slots are missing: %w", err)
	}
	slots := make(map[string]bool)
	for _, name := range scriptingSlots.Names {
		slots[name] = true
	}
	data := filepath.Join(filepath.Dir(editor), "Data")
	compiler.dotnet = filepath.Join(data, "DotNetSdk", "dotnet")
	compilers, _ := filepath.Glob(filepath.Join(data, "DotNetSdk", "sdk", "*", "Roslyn", "bincore", "csc.dll"))
	if len(compilers) == 0 {
		return nil, errors.New("installed Unity Roslyn SDK is missing")
	}
	sort.Strings(compilers)
	compiler.csc = compilers[len(compilers)-1]
	bcl := filepath.Join(data, "MonoBleedingEdge", "lib", "mono", "unityaot-linux")
	compiler.baseReferences, _ = filepath.Glob(filepath.Join(bcl, "*.dll"))
	facades, _ := filepath.Glob(filepath.Join(bcl, "Facades", "*.dll"))
	compiler.baseReferences = deduplicateGoReferences(append(compiler.baseReferences, facades...))
	engine := filepath.Join(data, "PlaybackEngines", "LinuxStandaloneSupport", "Variations", "mono", "Managed")
	compiler.engineReferences, _ = filepath.Glob(filepath.Join(engine, "UnityEngine*.dll"))
	scripting, _ := filepath.Glob(filepath.Join(engine, "Unity.Scripting.dll"))
	compiler.engineReferences = append(compiler.engineReferences, scripting...)
	for _, name := range []string{"Unity.Properties.SourceGenerator.dll", "Unity.SourceGenerators.dll", "Unity.UIToolkit.SourceGenerator.dll"} {
		compiler.generators = append(compiler.generators, filepath.Join(data, "Tools/BuildPipeline/Unity.SourceGenerators", name))
	}
	gameNames := make(map[string]bool)
	for _, assembly := range manifest.Assemblies {
		if gameNames[assembly.Name] {
			return nil, errors.New("duplicate game assembly in compiler plan: " + assembly.Name)
		}
		gameNames[assembly.Name] = true
	}
	ordered := make(map[string]bool)
	for _, assembly := range manifest.Assemblies {
		for _, dependency := range assembly.References {
			if gameNames[dependency] && !ordered[dependency] {
				return nil, errors.New("managed graph is not in dependency order: " + assembly.Name)
			}
		}
		ordered[assembly.Name] = true
		if assembly.Name == "" || filepath.Base(assembly.Name) != assembly.Name || len(assembly.Sources) == 0 ||
			!validGoLifecycleProof(assembly.LifecycleBarrier, assembly.Name) || !goLifecycleSourcesMatch(assembly.LifecycleBarrier, assembly.Sources) {
			return nil, errors.New("compiler baseline assembly lacks verified lifecycle barrier: " + assembly.Name)
		}
		if !slots[assembly.Name+".dll"] {
			return nil, errors.New("game assembly lacks a registered native Mono slot: " + assembly.Name)
		}
		config, hash, err := readGoAssemblyConfiguration(baselinePath, assembly)
		if err != nil {
			return nil, err
		}
		compiler.configs[assembly.Name], compiler.configurationHashes[assembly.Name] = config, hash
		managed := filepath.Join(playerData, "Managed")
		dll := filepath.Join(managed, assembly.Name+".dll")
		if hash, err := goFileHash(dll); err != nil || hash != assembly.SHA256 {
			return nil, errors.New("staged Player game DLL differs from compiler baseline: " + assembly.Name)
		}
		compiler.currentDLLs[assembly.Name], compiler.proofs[assembly.Name] = dll, assembly.LifecycleBarrier
		for _, source := range assembly.Sources {
			if manifest.CompilerInputs[source] == "" {
				return nil, errors.New("baseline lacks runtime source digest: " + source)
			}
			compiler.sources[source] = manifest.CompilerInputs[source]
			if hash := manifest.CompilerInputs[source+".meta"]; hash != "" {
				compiler.configuration[source+".meta"] = hash
			}
		}
	}
	for _, script := range manifest.Scripts {
		if script.HostProvided && script.Assembly == "Dungeon.CLIPlay" && withinDirectory(script.Path, filepath.Join(project, "Assets")) {
			compiler.hostRoots = append(compiler.hostRoots, filepath.Join(project, "Assets/Dungeon/CLIPlay"))
			break
		}
	}
	compiler.collectConfigurationInputs()
	if err := compiler.collectExpectedScriptTypes(baselineManifest); err != nil {
		return nil, err
	}
	if err := compiler.checkConfiguration(nil); err != nil {
		return nil, err
	}
	for _, filename := range append([]string{compiler.dotnet, compiler.csc}, compiler.generators...) {
		if _, err := os.Stat(filename); err != nil {
			return nil, err
		}
		if expected := manifest.CompilerInputs[filename]; expected != "" {
			if actual, err := goFileHash(filename); err != nil || actual != expected {
				return nil, errors.New("Unity compiler SDK input differs from baseline: " + filename)
			}
		}
	}
	return compiler, nil
}

func (compiler *goCompiler) collectConfigurationInputs() {
	for filename, hash := range compiler.manifest.CompilerInputs {
		extension := strings.ToLower(filepath.Ext(filename))
		if extension == ".asmdef" && withinDirectory(filename, filepath.Join(compiler.project, "Assets")) {
			var definition struct {
				Name string `json:"name"`
			}
			if readJSONFile(filename, &definition) == nil && definition.Name == "Dungeon.CLIPlay" {
				compiler.hostRoots = append(compiler.hostRoots, filepath.Dir(filename))
			}
		}
		dllReference := extension == ".dll" || strings.HasSuffix(strings.ToLower(filename), ".dll.meta")
		// The host-owned bridge is independently rebuilt/verified by
		// stageGoHost and references() uses that staged DLL. Its old Player
		// source location is not a compiler input for this Go revision.
		if dllReference && compiler.hostBridgeReference(filename) {
			continue
		}
		configuration := dllReference || extension == ".asmdef" || extension == ".asmref" || extension == ".rsp" ||
			strings.HasSuffix(filename, ".asmdef.meta") || strings.HasSuffix(filename, ".asmref.meta") ||
			strings.HasPrefix(filename, filepath.Join(compiler.project, "ProjectSettings")+string(filepath.Separator)) ||
			filename == filepath.Join(compiler.project, "Packages/manifest.json") || filename == filepath.Join(compiler.project, "Packages/packages-lock.json")
		if configuration && (dllReference || withinDirectory(filename, compiler.project)) {
			compiler.configuration[filename] = hash
		}
	}
}

func (compiler *goCompiler) hostBridgeReference(filename string) bool {
	filename = strings.TrimSuffix(filename, ".meta")
	return filepath.Base(filename) == "Dungeon.CLIPlay.dll" && filepath.Base(filepath.Dir(filename)) == "Managed" &&
		strings.HasSuffix(filepath.Base(filepath.Dir(filepath.Dir(filename))), "_Data") &&
		!withinDirectory(filename, filepath.Join(compiler.project, "Assets")) &&
		!withinDirectory(filename, filepath.Join(compiler.project, "Packages")) &&
		!withinDirectory(filename, filepath.Join(compiler.project, "Library/PackageCache"))
}

func (compiler *goCompiler) checkConfiguration(changed []string) error {
	for filename, expected := range compiler.configuration {
		actual, err := goFileHash(filename)
		if err != nil || actual != expected {
			return errors.New("managed assembly configuration/GUID changed; regenerate source graph: " + filename)
		}
	}
	for source := range compiler.sources {
		if compiler.configuration[source+".meta"] == "" {
			if _, err := os.Lstat(source + ".meta"); !errors.Is(err, os.ErrNotExist) {
				return errors.New("new MonoScript metadata requires a fresh managed graph: " + source)
			}
		}
	}
	for _, filename := range changed {
		if !filepath.IsAbs(filename) {
			filename = filepath.Join(compiler.project, filename)
		}
		if strings.HasSuffix(filename, ".cs") {
			if _, tracked := compiler.sources[filename]; !tracked && !compiler.hostOwned(filename) && !editorGoSource(filename) {
				return errors.New("C# source is outside the prepared managed graph: " + filename)
			}
		}
		if (strings.EqualFold(filepath.Ext(filename), ".dll") || strings.HasSuffix(strings.ToLower(filename), ".dll.meta")) &&
			compiler.configuration[filename] == "" && !compiler.hostOwned(filename) && !editorGoSource(filename) {
			return errors.New("DLL reference/import configuration is outside the prepared managed graph: " + filename)
		}
	}
	return filepath.WalkDir(filepath.Join(compiler.project, "Assets"), func(filename string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() && (compiler.hostOwned(filename) || filepath.Base(filename) == "Editor") {
			return filepath.SkipDir
		}
		if entry.IsDir() || compiler.manifest.CompilerInputs[filename] != "" || editorGoSource(filename) {
			return nil
		}
		switch filepath.Ext(filename) {
		case ".cs", ".asmdef", ".asmref", ".rsp", ".dll":
			return errors.New("new C# source/configuration requires a fresh managed graph: " + filename)
		default:
			return nil
		}
	})
}

func editorGoSource(filename string) bool {
	for _, part := range strings.Split(filepath.ToSlash(filename), "/") {
		if part == "Editor" {
			return true
		}
	}
	return false
}

func (compiler *goCompiler) hostOwned(filename string) bool {
	for _, root := range compiler.hostRoots {
		if withinDirectory(filename, root) {
			return true
		}
	}
	return false
}

func deduplicateGoReferences(paths []string) []string {
	seen := map[string]bool{}
	var result []string
	for _, filename := range paths {
		if !seen[filepath.Base(filename)] {
			result = append(result, filename)
			seen[filepath.Base(filename)] = true
		}
	}
	return result
}

func (compiler *goCompiler) references(assembly goCompileAssembly, built map[string]string) ([]string, error) {
	config := compiler.configs[assembly.Name]
	refs := append([]string(nil), compiler.baseReferences...)
	if !config.Config.NoEngineReferences {
		refs = append(refs, compiler.engineReferences...)
	}
	managed := filepath.Dir(compiler.currentDLLs[assembly.Name])
	host, _ := filepath.Glob(filepath.Join(managed, "*.dll"))
	for _, filename := range host {
		name := strings.TrimSuffix(filepath.Base(filename), ".dll")
		if strings.HasPrefix(name, "UnityEditor") {
			return nil, errors.New("Editor assembly cannot be a runtime compiler reference: " + filename)
		}
		if _, game := compiler.currentDLLs[name]; !game {
			refs = append(refs, filename)
		}
	}
	for _, dependency := range assembly.References {
		if filename, game := built[dependency]; game {
			refs = append(refs, filename)
		} else if filename, game := compiler.currentDLLs[dependency]; game {
			refs = append(refs, filename)
		}
	}
	return deduplicateGoReferences(refs), nil
}

type goCompiledCache struct {
	SchemaVersion int               `json:"schemaVersion"`
	Key           string            `json:"key"`
	Name          string            `json:"name"`
	Files         map[string]string `json:"files"`
	Proof         goLifecycleProof  `json:"proof"`
	NetcodeKey    string            `json:"netcodeKey,omitempty"`
}

func (compiler *goCompiler) Compile(ctx context.Context, changedPaths []string) (*compileUpdate, error) {
	started := time.Now()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	// Initial startup proves the complete prepared graph. A watcher already
	// reports topology/configuration edits; existing scene and native-asset
	// changes cannot alter C# DLLs and should avoid any compiler scan.
	if changedPaths != nil && !compiler.compilerChanges(changedPaths) {
		return nil, nil
	}
	if err := compiler.checkConfiguration(changedPaths); err != nil {
		return nil, err
	}
	currentSources := map[string]string{}
	for _, assembly := range compiler.manifest.Assemblies {
		for _, source := range assembly.Sources {
			hash, err := goFileHash(source)
			if err != nil {
				return nil, err
			}
			currentSources[source] = hash
		}
	}
	dirty := goDirtyAssemblies(compiler.manifest.Assemblies, compiler.sources, currentSources)
	// The old raw baseline predates NGO IL postprocessing. Rebuild its
	// networking assemblies once even when the source bytes are unchanged.
	forcedNetcode := map[string]bool{}
	for _, assembly := range compiler.manifest.Assemblies {
		if goNeedsNetcode(assembly) && !compiler.netcodeProcessed[assembly.Name] {
			forcedNetcode[assembly.Name], dirty[assembly.Name] = true, true
		}
		for _, dependency := range assembly.References {
			if dirty[dependency] {
				dirty[assembly.Name] = true
			}
		}
	}
	if len(dirty) == 0 {
		compiler.startupComplete = true
		return nil, nil
	}
	revisionKey, err := compiler.codeRevisionKey(ctx, currentSources)
	if err != nil {
		return nil, err
	}
	if !compiler.startupComplete {
		if update := compiler.restoreCodeRevision(revisionKey, currentSources); update != nil {
			if err := compiler.checkConfiguration(nil); err != nil {
				return nil, err
			}
			for source, expected := range currentSources {
				if actual, err := goFileHash(source); err != nil || actual != expected {
					return nil, errors.New("runtime source changed during revision reuse: " + source)
				}
			}
			compiler.useCodeRevision(update, compiler.proofs)
			update.ElapsedSeconds = time.Since(started).Seconds()
			return update, nil
		}
	}
	if err := os.MkdirAll(compiler.cache, 0700); err != nil {
		return nil, err
	}
	staging, err := os.MkdirTemp(compiler.cache, ".code-revision-")
	if err != nil {
		return nil, err
	}
	// Staging remains an owned diagnostic on rejection; it is never used
	// by Player or published as a successful compiler revision.
	snapshots := map[string]string{}
	for _, assembly := range compiler.manifest.Assemblies {
		if !dirty[assembly.Name] {
			continue
		}
		for _, source := range assembly.Sources {
			expected := currentSources[source]
			data, err := os.ReadFile(source)
			if err != nil || bytesSHA(data) != expected {
				return nil, errors.New("runtime source changed while snapshotting: " + source)
			}
			destination := filepath.Join(staging, "Sources", bytesSHA([]byte(source)), filepath.Base(source))
			if err := os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
				return nil, err
			}
			if err := os.WriteFile(destination, data, 0600); err != nil {
				return nil, err
			}
			snapshots[source] = destination
		}
	}
	helper, err := compiler.prepareLifecycleHelper(ctx)
	if err != nil {
		return nil, err
	}
	built := map[string]string{}
	proofs := map[string]goLifecycleProof{}
	directDirty := goSourceDirtyAssemblies(compiler.manifest.Assemblies, compiler.sources, currentSources)
	for name := range forcedNetcode {
		directDirty[name] = true
	}
	declarationsChanged := map[string]bool{}
	rebuilt := map[string]bool{}
	var compiled []string
	for _, assembly := range compiler.manifest.Assemblies {
		if !goAssemblyNeedsCompile(assembly, directDirty, declarationsChanged) {
			built[assembly.Name], proofs[assembly.Name] = compiler.currentDLLs[assembly.Name], compiler.proofs[assembly.Name]
			continue
		}
		refs, err := compiler.references(assembly, built)
		if err != nil {
			return nil, err
		}
		dll, proof, err := compiler.compileAssembly(ctx, staging, helper, assembly, refs, snapshots, currentSources)
		if err != nil {
			return nil, err
		}
		built[assembly.Name], proofs[assembly.Name] = dll, proof
		if err := compiler.validateCompiledScriptTypes(ctx, helper, assembly.Name, compiler.currentDLLs[assembly.Name], dll); err != nil {
			return nil, err
		}
		rebuilt[assembly.Name] = true
		changed, err := compiler.declarationsChanged(ctx, helper, compiler.currentDLLs[assembly.Name], dll)
		if err != nil {
			// A fingerprint is an optimization. Unknown metadata retains the
			// original conservative dependent rebuild instead of guessing.
			changed = true
		}
		declarationsChanged[assembly.Name] = changed
		compiled = append(compiled, assembly.Name)
	}
	if err := compiler.checkConfiguration(nil); err != nil {
		return nil, err
	}
	for filename, expected := range currentSources {
		if hash, err := goFileHash(filename); err != nil || hash != expected {
			return nil, errors.New("runtime source changed during compilation; no Player revision published: " + filename)
		}
	}
	playerFolder := filepath.Join(staging, "Player")
	if err := os.Mkdir(playerFolder, 0700); err != nil {
		return nil, err
	}
	if _, err := goCompilerCommand(ctx, "", "cp", "-a", "--reflink=auto", filepath.Dir(compiler.player)+string(filepath.Separator)+".", playerFolder); err != nil {
		return nil, fmt.Errorf("stage immutable Player: %w", err)
	}
	player := filepath.Join(playerFolder, filepath.Base(compiler.player))
	managed := filepath.Join(playerFolder, strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data", "Managed")
	update := &compileUpdate{Player: player, CompiledAssemblies: compiled, SourceHashes: currentSources, RuntimeInitializers: []runtimeInitializer{}}
	for _, assembly := range compiler.manifest.Assemblies {
		destination := filepath.Join(managed, assembly.Name+".dll")
		if rebuilt[assembly.Name] {
			data, err := os.ReadFile(built[assembly.Name])
			if err != nil {
				return nil, err
			}
			if err := os.WriteFile(destination, data, 0600); err != nil {
				return nil, err
			}
			pdb := strings.TrimSuffix(built[assembly.Name], ".dll") + ".pdb"
			if data, err := os.ReadFile(pdb); err == nil {
				if err := os.WriteFile(strings.TrimSuffix(destination, ".dll")+".pdb", data, 0600); err != nil {
					return nil, err
				}
			}
		}
		update.Assemblies = append(update.Assemblies, destination)
		if declarationsChanged[assembly.Name] {
			update.DeclarationChanged = append(update.DeclarationChanged, assembly.Name)
		}
		update.RuntimeInitializers = append(update.RuntimeInitializers, proofs[assembly.Name].RuntimeInitializers...)
	}
	update.ElapsedSeconds = time.Since(started).Seconds()
	if err := atomicJSON(filepath.Join(staging, "compile-update.json"), update); err != nil {
		return nil, err
	}
	if err := compiler.saveCodeRevision(revisionKey, update, proofs); err != nil {
		return nil, err
	}
	compiler.useCodeRevision(update, proofs)
	return update, nil
}

func (compiler *goCompiler) compilerChanges(changed []string) bool {
	for _, filename := range changed {
		if !filepath.IsAbs(filename) {
			filename = filepath.Join(compiler.project, filename)
		}
		filename = filepath.Clean(filename)
		if _, tracked := compiler.configuration[filename]; tracked ||
			withinDirectory(filename, filepath.Join(compiler.project, "ProjectSettings")) ||
			filename == filepath.Join(compiler.project, "Packages/manifest.json") ||
			filename == filepath.Join(compiler.project, "Packages/packages-lock.json") || filepath.Base(filename) == "package.json" {
			return true
		}
		if info, err := os.Stat(filename); err == nil && info.IsDir() {
			return true
		}
		switch filepath.Ext(filename) {
		case ".cs", ".asmdef", ".asmref", ".rsp", ".dll":
			return true
		case "":
			return true // A removed or renamed directory may contain C#.
		case ".meta":
			for _, extension := range []string{".cs.meta", ".asmdef.meta", ".asmref.meta", ".dll.meta", ".rsp.meta"} {
				if strings.HasSuffix(filename, extension) {
					return true
				}
			}
		}
	}
	return false
}

func goDirtyAssemblies(assemblies []goCompileAssembly, previous, current map[string]string) map[string]bool {
	dirty := make(map[string]bool)
	for _, assembly := range assemblies {
		for _, source := range assembly.Sources {
			if current[source] != previous[source] {
				dirty[assembly.Name] = true
			}
		}
		for _, dependency := range assembly.References {
			if dirty[dependency] {
				dirty[assembly.Name] = true
			}
		}
	}
	return dirty
}

func goSourceDirtyAssemblies(assemblies []goCompileAssembly, previous, current map[string]string) map[string]bool {
	dirty := make(map[string]bool)
	for _, assembly := range assemblies {
		for _, source := range assembly.Sources {
			if current[source] != previous[source] {
				dirty[assembly.Name] = true
			}
		}
	}
	return dirty
}

func goAssemblyNeedsCompile(assembly goCompileAssembly, sourceDirty, declarationChanged map[string]bool) bool {
	if sourceDirty[assembly.Name] {
		return true
	}
	for _, dependency := range assembly.References {
		if declarationChanged[dependency] {
			return true
		}
	}
	return false
}

func (compiler *goCompiler) declarationsChanged(ctx context.Context, helper, previousDLL, nextDLL string) (bool, error) {
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, helper, "--declaration-hashes", previousDLL, nextDLL)
	if err != nil {
		return true, err
	}
	var hashes map[string]string
	if err := json.Unmarshal(output, &hashes); err != nil || len(hashes) != 2 || len(hashes[previousDLL]) != 64 || len(hashes[nextDLL]) != 64 {
		return true, errors.New("managed declaration fingerprint has no exact input proof")
	}
	return hashes[previousDLL] != hashes[nextDLL], nil
}

func (compiler *goCompiler) collectExpectedScriptTypes(baseline string) error {
	compiler.expectedTypes = map[string]map[string]string{}
	var doc struct {
		Manifest    string `json:"manifest"`
		ProjectRoot string `json:"projectRoot"`
		Assets      []struct {
			Kind     string `json:"kind"`
			GUID     string `json:"guid"`
			TypeName string `json:"typeName"`
		} `json:"assets"`
	}
	if err := readJSONFile(baseline, &doc); err != nil {
		return err
	}
	if doc.Manifest != "" {
		if !filepath.IsAbs(doc.Manifest) {
			doc.Manifest = filepath.Join(filepath.Dir(baseline), doc.Manifest)
		}
		if err := readJSONFile(doc.Manifest, &doc); err != nil {
			return err
		}
	}
	if len(doc.Assets) != 0 && doc.ProjectRoot != compiler.project {
		return errors.New("script binding baseline belongs to another project")
	}
	byGUID := map[string]string{}
	for _, script := range compiler.manifest.Scripts {
		if script.GUID != "" {
			byGUID[script.GUID] = script.Assembly
		}
		if script.TypeName != "" && !script.HostProvided {
			compiler.expectScriptType(script.Assembly, script.TypeName, script.GUID)
		}
	}
	for _, asset := range doc.Assets {
		if asset.Kind != "script" || asset.TypeName == "" {
			continue
		}
		if assembly := byGUID[asset.GUID]; compiler.currentDLLs[assembly] != "" {
			compiler.expectScriptType(assembly, asset.TypeName, asset.GUID)
		}
	}
	return nil
}

func (compiler *goCompiler) expectScriptType(assembly, name, guid string) {
	// The compiler's GUID map chooses the defining assembly. Metadata TypeDef
	// names omit any optional assembly-qualified suffix from the manifest.
	if index := strings.IndexByte(name, ','); index >= 0 {
		name = strings.TrimSpace(name[:index])
	}
	if compiler.expectedTypes[assembly] == nil {
		compiler.expectedTypes[assembly] = map[string]string{}
	}
	compiler.expectedTypes[assembly][name] = guid
}

func (compiler *goCompiler) validateCompiledScriptTypes(ctx context.Context, helper, assembly, previousDLL, nextDLL string) error {
	hasScripts := len(compiler.expectedTypes[assembly]) != 0
	for _, script := range compiler.manifest.Scripts {
		if script.Assembly == assembly && !script.HostProvided && script.GUID != "" {
			hasScripts = true
		}
	}
	if !hasScripts {
		return nil
	}
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, helper, "--type-definitions", previousDLL, nextDLL)
	if err != nil {
		return fmt.Errorf("validate baseline script types in %s: %w", assembly, err)
	}
	var definitions map[string][]string
	if err := json.Unmarshal(output, &definitions); err != nil || len(definitions) != 2 || definitions[previousDLL] == nil || definitions[nextDLL] == nil {
		return errors.New("compiled DLL has no exact baseline/new TypeDef proof: " + assembly)
	}
	return compiler.validateRequiredScriptTypes(assembly, definitions[previousDLL], definitions[nextDLL])
}

func (compiler *goCompiler) validateRequiredScriptTypes(assembly string, previous, next []string) error {
	required := map[string]string{}
	for name, guid := range compiler.expectedTypes[assembly] {
		required[name] = guid
	}
	// Compiler scripts cover the entire baseline project, including scripts
	// absent from the current scene. Unity script files conventionally declare
	// their primary class with the filename; use only names proven in old DLL.
	for _, script := range compiler.manifest.Scripts {
		if script.Assembly != assembly || script.HostProvided || script.GUID == "" {
			continue
		}
		base := strings.TrimSuffix(filepath.Base(script.Path), filepath.Ext(script.Path))
		for _, name := range previous {
			simple := name[strings.LastIndexAny(name, ".+")+1:]
			if index := strings.IndexByte(simple, '`'); index >= 0 {
				simple = simple[:index]
			}
			if simple == base {
				required[name] = script.GUID
			}
		}
	}
	available := map[string]bool{}
	for _, name := range next {
		available[name] = true
	}
	for name, guid := range required {
		if !available[name] {
			return fmt.Errorf("compiled assembly %s removed or renamed baseline script type %s (GUID %s); regenerate script bindings before replacing Player", assembly, name, guid)
		}
	}
	return nil
}

func goCompilerCommand(ctx context.Context, directory, executable string, arguments ...string) ([]byte, error) {
	command := exec.CommandContext(ctx, executable, arguments...)
	command.Dir = directory
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	command.Cancel = func() error { return syscall.Kill(-command.Process.Pid, syscall.SIGKILL) }
	command.WaitDelay = 2 * time.Second
	command.Env = append(os.Environ(), "DOTNET_CLI_TELEMETRY_OPTOUT=1", "DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1", "DOTNET_NOLOGO=1")
	output, err := command.CombinedOutput()
	if err != nil {
		return output, fmt.Errorf("%s: %w\n%s", filepath.Base(executable), err, output)
	}
	return output, nil
}

func (compiler *goCompiler) prepareLifecycleHelper(ctx context.Context) (string, error) {
	if compiler.helper != "" {
		return compiler.helper, nil
	}
	toolRoot, err := microEditorRoot()
	if err != nil {
		return "", err
	}
	sourceRoot := filepath.Join(toolRoot, "LifecycleCompiler")
	inputs := map[string]string{}
	for _, filename := range []string{filepath.Join(sourceRoot, "LifecycleCompiler.csproj"), filepath.Join(sourceRoot, "Program.cs"), compiler.csc, compiler.dotnet} {
		hash, err := goFileHash(filename)
		if err != nil {
			return "", err
		}
		inputs[filename] = hash
	}
	encoded, _ := json.Marshal(inputs)
	key := bytesSHA(encoded)
	entry := filepath.Join(compiler.cache, "lifecycle", key)
	var metadata map[string]string
	if readJSONFile(filepath.Join(entry, "files.json"), &metadata) == nil && verifyGoCacheFiles(entry, metadata) {
		compiler.helper = filepath.Join(entry, "LifecycleCompiler.dll")
		return compiler.helper, nil
	}
	if err := os.MkdirAll(filepath.Dir(entry), 0700); err != nil {
		return "", err
	}
	staging, err := os.MkdirTemp(filepath.Dir(entry), ".helper-build-")
	if err != nil {
		return "", err
	}
	for _, name := range []string{"LifecycleCompiler.csproj", "Program.cs"} {
		data, err := os.ReadFile(filepath.Join(sourceRoot, name))
		if err != nil || bytesSHA(data) != inputs[filepath.Join(sourceRoot, name)] {
			return "", errors.New("lifecycle helper source changed during build")
		}
		if err := os.WriteFile(filepath.Join(staging, name), data, 0600); err != nil {
			return "", err
		}
	}
	if err := os.WriteFile(filepath.Join(staging, "NuGet.Config"), []byte("<configuration><packageSources><clear /></packageSources></configuration>\n"), 0600); err != nil {
		return "", err
	}
	payload := filepath.Join(staging, "payload")
	if _, err := goCompilerCommand(ctx, staging, compiler.dotnet, "build", filepath.Join(staging, "LifecycleCompiler.csproj"), "--nologo", "-v:q", "-o", payload,
		"-p:RoslynPath="+filepath.Dir(compiler.csc), "-p:NuGetAudit=false"); err != nil {
		return "", fmt.Errorf("build SDK lifecycle helper: %w", err)
	}
	files, err := os.ReadDir(payload)
	if err != nil {
		return "", err
	}
	metadata = map[string]string{}
	for _, file := range files {
		if file.Type().IsRegular() {
			hash, err := goFileHash(filepath.Join(payload, file.Name()))
			if err != nil {
				return "", err
			}
			metadata[file.Name()] = hash
		}
	}
	if metadata["LifecycleCompiler.dll"] == "" {
		return "", errors.New("SDK helper build produced no verified executable")
	}
	if err := atomicJSON(filepath.Join(payload, "files.json"), metadata); err != nil {
		return "", err
	}
	if err := os.Rename(payload, entry); err != nil {
		return "", err
	}
	compiler.helper = filepath.Join(entry, "LifecycleCompiler.dll")
	return compiler.helper, nil
}

func verifyGoCacheFiles(directory string, hashes map[string]string) bool {
	if len(hashes) == 0 {
		return false
	}
	for name, expected := range hashes {
		if filepath.Base(name) != name || expected == "" {
			return false
		}
		actual, err := goFileHash(filepath.Join(directory, name))
		if err != nil || actual != expected {
			return false
		}
	}
	return true
}

func (compiler *goCompiler) compileAssembly(ctx context.Context, staging, helper string, assembly goCompileAssembly,
	refs []string, snapshots, hashes map[string]string) (string, goLifecycleProof, error) {
	config := compiler.configs[assembly.Name]
	identity := map[string]string{"configuration": compiler.configurationHashes[assembly.Name]}
	for _, source := range assembly.Sources {
		identity[source] = hashes[source]
		if expected := compiler.configuration[source+".meta"]; expected != "" {
			identity[source+".meta"] = expected
		}
	}
	for _, reference := range refs {
		hash, err := goFileHash(reference)
		if err != nil {
			return "", goLifecycleProof{}, err
		}
		identity["reference:"+filepath.Base(reference)] = hash
	}
	for _, executable := range append([]string{helper, compiler.csc}, compiler.generators...) {
		hash, err := goFileHash(executable)
		if err != nil {
			return "", goLifecycleProof{}, err
		}
		identity[executable] = hash
	}
	netcodeKey := ""
	if goNeedsNetcode(assembly) {
		processor, err := compiler.prepareNetcodeProcessor(ctx)
		if err != nil {
			return "", goLifecycleProof{}, err
		}
		netcodeKey = processor.Key
		identity["netcodePostprocessor"] = netcodeKey
	}
	encoded, _ := json.Marshal(identity)
	key := bytesSHA(encoded)
	entry := filepath.Join(compiler.cache, "assemblies", key)
	var cached goCompiledCache
	if readJSONFile(filepath.Join(entry, "compile-cache.json"), &cached) == nil && cached.SchemaVersion == 1 && cached.Key == key && cached.Name == assembly.Name &&
		cached.NetcodeKey == netcodeKey &&
		validGoLifecycleProof(cached.Proof, assembly.Name) && goLifecycleSourcesMatch(cached.Proof, assembly.Sources) &&
		verifyGoCacheFiles(entry, cached.Files) && cached.Files[assembly.Name+".dll"] != "" {
		return filepath.Join(entry, assembly.Name+".dll"), cached.Proof, nil
	}
	folder := filepath.Join(staging, "Compile", assembly.Name)
	if err := os.MkdirAll(folder, 0700); err != nil {
		return "", goLifecycleProof{}, err
	}
	sources := []map[string]string{}
	executionOrders := map[string]int32{}
	for index, source := range assembly.Sources {
		sources = append(sources, map[string]string{"Path": snapshots[source], "OriginalPath": source,
			"OutputPath": filepath.Join(folder, "Lifecycle", fmt.Sprint(index), filepath.Base(source))})
		if data, err := os.ReadFile(source + ".meta"); err == nil {
			properties, err := registryMetaProperties(data)
			if err != nil {
				return "", goLifecycleProof{}, err
			}
			if value := properties["MonoImporter.executionOrder"]; len(value) == 4 {
				executionOrders[source] = int32(uint32(value[0]) | uint32(value[1])<<8 | uint32(value[2])<<16 | uint32(value[3])<<24)
			}
		}
	}
	marker := filepath.Join(folder, "UnityPlayLifecycleMarker.cs")
	request := filepath.Join(folder, "lifecycle-request.json")
	if err := atomicJSON(request, map[string]any{"AssemblyName": assembly.Name, "References": refs, "Defines": config.Defines, "Sources": sources,
		"MarkerPath": marker, "AllowUnsafe": config.Config.AllowUnsafe, "ExecutionOrders": executionOrders, "NativeBaseAssemblies": config.NativeBase}); err != nil {
		return "", goLifecycleProof{}, err
	}
	output, err := goCompilerCommand(ctx, staging, compiler.dotnet, helper, request)
	if err != nil {
		return "", goLifecycleProof{}, fmt.Errorf("verify lifecycle %s: %w", assembly.Name, err)
	}
	var wireProof struct {
		SchemaVersion           int
		Verified                bool
		Phases                  []string
		ScriptableObjectBarrier bool
		Callbacks               []goLifecycleCallback
		RuntimeInitializers     []struct {
			Assembly, TypeName, MethodName string
			Phase                          int
		}
	}
	if err := json.Unmarshal(output, &wireProof); err != nil {
		return "", goLifecycleProof{}, err
	}
	proof := goLifecycleProof{SchemaVersion: wireProof.SchemaVersion, Verified: wireProof.Verified,
		Phases: wireProof.Phases, ScriptableObjectBarrier: wireProof.ScriptableObjectBarrier, RuntimeInitializers: []runtimeInitializer{},
		Callbacks: len(wireProof.Callbacks), Methods: wireProof.Callbacks}
	for _, initializer := range wireProof.RuntimeInitializers {
		proof.RuntimeInitializers = append(proof.RuntimeInitializers, runtimeInitializer{initializer.Assembly, initializer.TypeName, initializer.MethodName, initializer.Phase})
	}
	if !validGoLifecycleProof(proof, assembly.Name) || !goLifecycleSourcesMatch(proof, assembly.Sources) {
		return "", proof, errors.New("SDK lifecycle proof is invalid for " + assembly.Name)
	}
	dll := filepath.Join(folder, assembly.Name+".dll")
	arguments := []string{"-nologo", "-target:library", "-nostdlib+", "-langversion:9.0", "-deterministic+", "-debug:portable", "-optimize-", "-unsafe-", "-out:" + dll, "-define:" + strings.Join(config.Defines, ";")}
	if config.Config.AllowUnsafe {
		arguments[7] = "-unsafe+"
	}
	for _, reference := range refs {
		arguments = append(arguments, "-reference:"+reference)
	}
	for _, generator := range compiler.generators {
		arguments = append(arguments, "-analyzer:"+generator)
	}
	generated := filepath.Join(folder, "Generated")
	if err := os.Mkdir(generated, 0700); err != nil {
		return "", proof, err
	}
	additional := filepath.Join(folder, assembly.Name+".UnityAdditionalFile.txt")
	if err := os.WriteFile(additional, []byte(compiler.project), 0600); err != nil {
		return "", proof, err
	}
	arguments = append(arguments, "-generatedfilesout:"+generated, "-additionalfile:"+additional)
	for _, source := range sources {
		arguments = append(arguments, source["OutputPath"])
	}
	arguments = append(arguments, marker)
	response := filepath.Join(folder, assembly.Name+".rsp")
	var lines []string
	for _, argument := range arguments {
		lines = append(lines, "\""+strings.ReplaceAll(argument, "\"", "\\\"")+"\"")
	}
	if err := os.WriteFile(response, []byte(strings.Join(lines, "\n")+"\n"), 0600); err != nil {
		return "", proof, err
	}
	output, err = goCompilerCommand(ctx, compiler.project, compiler.dotnet, compiler.csc, "@"+response)
	if err != nil || strings.Contains(string(output), "CS8032") || strings.Contains(string(output), "CS8784") || strings.Contains(string(output), "CS8785") {
		return "", proof, fmt.Errorf("compile %s failed: %v\n%s", assembly.Name, err, output)
	}
	if _, err := os.Stat(dll); err != nil {
		return "", proof, err
	}
	if goNeedsNetcode(assembly) {
		dll, proof, err = compiler.postprocessNetcode(ctx, folder, dll, assembly, refs, proof)
		if err != nil {
			return "", proof, err
		}
	}
	if err := os.MkdirAll(filepath.Dir(entry), 0700); err != nil {
		return "", proof, err
	}
	payload, err := os.MkdirTemp(filepath.Dir(entry), ".assembly-")
	if err != nil {
		return "", proof, err
	}
	files := map[string]string{}
	for _, filename := range []string{dll, strings.TrimSuffix(dll, ".dll") + ".pdb"} {
		data, err := os.ReadFile(filename)
		if err != nil {
			return "", proof, err
		}
		name := filepath.Base(filename)
		if err := os.WriteFile(filepath.Join(payload, name), data, 0600); err != nil {
			return "", proof, err
		}
		files[name] = bytesSHA(data)
	}
	if err := atomicJSON(filepath.Join(payload, "compile-cache.json"), goCompiledCache{SchemaVersion: 1, Key: key, Name: assembly.Name, Files: files, Proof: proof, NetcodeKey: netcodeKey}); err != nil {
		return "", proof, err
	}
	if err := os.Rename(payload, entry); err != nil {
		return "", proof, err
	}
	return filepath.Join(entry, assembly.Name+".dll"), proof, nil
}
