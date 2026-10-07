package main

import (
	"context"
	"debug/pe"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strings"

	goproject "unitymicroeditor/project"
	"unitymicroeditor/project/packages"
)

type goSourceAsmdef struct {
	Name                  string   `json:"name"`
	RootNamespace         string   `json:"rootNamespace"`
	References            []string `json:"references"`
	IncludePlatforms      []string `json:"includePlatforms"`
	ExcludePlatforms      []string `json:"excludePlatforms"`
	AllowUnsafe           bool     `json:"allowUnsafeCode"`
	OverrideReferences    bool     `json:"overrideReferences"`
	PrecompiledReferences []string `json:"precompiledReferences"`
	AutoReferenced        *bool    `json:"autoReferenced"`
	DefineConstraints     []string `json:"defineConstraints"`
	VersionDefines        []struct {
		Name       string `json:"name"`
		Expression string `json:"expression"`
		Define     string `json:"define"`
	} `json:"versionDefines"`
	NoEngineReferences      bool     `json:"noEngineReferences"`
	OptionalUnityReferences []string `json:"optionalUnityReferences"`
	Path                    string   `json:"-"`
	Root                    string   `json:"-"`
	GUID                    string   `json:"-"`
	Package                 string   `json:"-"`
}

type goSourceSettings struct {
	APICompatibility int
	Defines          []string
	InputHandler     int
	AllowUnsafe      bool
	Arguments        []string
}

func readGoSourceSettings(project string) (goSourceSettings, error) {
	var settings goSourceSettings
	content, err := readAgentFile(filepath.Join(project, "ProjectSettings/ProjectSettings.asset"), 32<<20)
	if os.IsNotExist(err) {
		return settings, nil
	}
	if err != nil {
		return settings, err
	}
	documents, err := goproject.ParseUnityYAML(content)
	if err != nil {
		return settings, err
	}
	if len(documents) != 1 || documents[0].ClassID != 129 {
		return settings, errors.New("project PlayerSettings requires exact class129 YAML")
	}
	data := documents[0].Data
	settings.APICompatibility = int(goSourceNumber(data["apiCompatibilityLevel"]))
	for key, value := range goSourceObject(data["apiCompatibilityLevelPerPlatform"]) {
		if key == "Standalone" || key == "LinuxStandalone64" || key == "1" {
			settings.APICompatibility = int(goSourceNumber(value))
		}
	}
	settings.InputHandler = int(goSourceNumber(data["activeInputHandler"]))
	settings.AllowUnsafe = goSourceNumber(data["allowUnsafeCode"]) != 0
	for key, value := range goSourceObject(data["scriptingDefineSymbols"]) {
		if key == "Standalone" || key == "1" || key == "LinuxStandalone64" {
			settings.Defines = append(settings.Defines, strings.FieldsFunc(goSourceString(value), func(r rune) bool { return r == ';' || r == ',' })...)
		}
	}
	for key, value := range goSourceObject(data["additionalCompilerArguments"]) {
		if key == "Standalone" || key == "1" || key == "LinuxStandalone64" {
			for _, argument := range goSourceArray(value) {
				settings.Arguments = append(settings.Arguments, goSourceString(argument))
			}
		}
	}
	settings.Defines = sortedGoSourceStrings(settings.Defines)
	for _, define := range settings.Defines {
		if !goSourceSymbol.MatchString(define) || strings.HasPrefix(define, "UNITY_EDITOR") {
			return settings, errors.New("invalid authored scripting define: " + define)
		}
	}
	return settings, nil
}

var goSourceSymbol = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*$`)

func goSourceObject(value any) map[string]any { result, _ := value.(map[string]any); return result }
func goSourceArray(value any) []any           { result, _ := value.([]any); return result }
func goSourceString(value any) string         { result, _ := value.(string); return result }
func readGoMetadataGUID(path string) (string, error) {
	identity, err := newGoMetadataGUIDIndex().read(path)
	if err != nil {
		return "", err
	}
	if identity.GUID == "" {
		return "", errors.New("managed source metadata has no GUID: " + path)
	}
	return identity.GUID, nil
}

func goSourceNumber(value any) float64 {
	switch value := value.(type) {
	case int:
		return float64(value)
	case int64:
		return float64(value)
	case float64:
		return value
	case json.Number:
		result, _ := value.Float64()
		return result
	}
	return 0
}

type goSourceTree struct{ logical, physical, pkg string }

func buildGoSourceGraph(ctx context.Context, options goSourceGraphOptions) (*goCompilerGraphExport, error) {
	project, err := filepath.EvalSymlinks(options.Project)
	if err != nil {
		return nil, err
	}
	project, err = filepath.Abs(project)
	if err != nil {
		return nil, err
	}
	if options.Profile == nil || options.Packages == nil || options.Packages.Project != project || options.Packages.Editor != options.Editor {
		return nil, errors.New("Go source graph requires explicit matching engine/package profiles")
	}
	if err := options.Packages.Validate(ctx); err != nil {
		return nil, err
	}
	profile := options.Profile
	hash, err := goSourceProfileHash(*profile)
	if err != nil || profile.ProfileSHA256 != hash || profile.Editor != options.Editor || profile.Player != options.Player || profile.UnityVersion != fixtureUnityVersion || profile.UnityRevision != "45d8eee7de74" || profile.Target != "LinuxStandalone64" || profile.ScriptingBackend != "Mono" {
		return nil, errors.New("Go source graph engine profile mismatch")
	}
	for path, expected := range profile.Inputs {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		if actual, err := goFileHash(path); err != nil || actual != expected {
			return nil, errors.New("Go source graph engine/native input changed: " + path)
		}
	}
	settings, err := readGoSourceSettings(project)
	if err != nil {
		return nil, err
	}
	if settings.APICompatibility != profile.APICompatibility {
		return nil, errors.New("Player API compatibility differs from cached engine/BCL profile")
	}
	base := append(slices.Clone(profile.BaseDefines), settings.Defines...)
	if settings.InputHandler == 1 || settings.InputHandler == 2 {
		base = append(base, "ENABLE_INPUT_SYSTEM")
	}
	if settings.InputHandler == 0 || settings.InputHandler == 2 {
		base = append(base, "ENABLE_LEGACY_INPUT_MANAGER")
	}
	base = sortedGoSourceStrings(base)
	automaticEngineAssemblies := goSourceAutomaticEngineAssemblies(profile)
	baseCompilerOptions := goSourceBaseCompilerOptions(profile)
	engineAssemblyNames := map[string]bool{}
	for _, path := range profile.EngineReferences {
		engineAssemblyNames[strings.TrimSuffix(filepath.Base(path), ".dll")] = true
	}
	graph := &goCompilerGraphExport{SchemaVersion: 1, Project: project, Unity: options.Editor, Backend: "go_authored_source", EngineProfileSHA256: profile.ProfileSHA256, PackageResolutionSHA256: options.Packages.ResolutionHash, CompilerInputs: maps.Clone(options.Packages.Inputs)}
	graph.Development = true
	graph.BaseReferences, graph.EngineReferences = slices.Clone(profile.BaseReferences), slices.Clone(profile.EngineReferences)
	if graph.CompilerInputs == nil {
		graph.CompilerInputs = map[string]string{}
	}
	settingsPath := filepath.Join(project, "ProjectSettings/ProjectSettings.asset")
	if hash, err := goFileHash(settingsPath); err == nil {
		graph.CompilerInputs[settingsPath] = hash
	} else if os.IsNotExist(err) {
		graph.CompilerInputs[settingsPath] = "absent"
	} else {
		return nil, err
	}
	for path, value := range goSourceProfileInputs(profile) {
		graph.CompilerInputs[path] = value
	}
	trees := []goSourceTree{{"Assets", filepath.Join(project, "Assets"), ""}}
	versions := map[string]string{"Unity": fixtureUnityVersion}
	pkgMap := map[string]packages.Package{}
	for _, pkg := range options.Packages.Packages {
		versions[pkg.Name] = pkg.Version
		pkgMap[pkg.Name] = pkg
		if pkg.Path == "" || pkg.LogicalRoot != "Packages/"+pkg.Name {
			return nil, errors.New("resolved package has invalid source root: " + pkg.Name)
		}
		trees = append(trees, goSourceTree{pkg.LogicalRoot, pkg.Path, pkg.Name})
		graph.SourceRoots = append(graph.SourceRoots, goCompilerSourceRoot{Path: pkg.LogicalRoot, Source: pkg.Path})
	}
	definitions := map[string]*goSourceAsmdef{}
	byGUID := map[string]string{}
	roots := map[string]string{}
	asmrefs := map[string]string{}
	sources := map[string]string{}
	plugins := map[string]goSourcePlugin{}
	metadataIndex := newGoMetadataGUIDIndex()
	metadataOwners := map[string]string{}
	for _, tree := range trees {
		if err := filepath.WalkDir(tree.physical, func(path string, entry fs.DirEntry, walkErr error) error {
			if walkErr != nil {
				return walkErr
			}
			if err := ctx.Err(); err != nil {
				return err
			}
			if entry.Type()&os.ModeSymlink != 0 {
				return errors.New("source graph rejects symlink: " + path)
			}
			if entry.IsDir() {
				if path != tree.physical && (strings.HasPrefix(entry.Name(), ".") || strings.HasSuffix(entry.Name(), "~")) {
					return filepath.SkipDir
				}
				return nil
			}
			ext := strings.ToLower(filepath.Ext(path))
			if ext != ".cs" && ext != ".asmdef" && ext != ".asmref" && ext != ".dll" && ext != ".rsp" {
				return nil
			}
			if len(sources)+len(definitions)+len(asmrefs) > 100000 {
				return errors.New("Go source graph exceeds supported source bound")
			}
			var content []byte
			if ext == ".dll" {
				hash, err := goFileHash(path)
				if err != nil {
					return err
				}
				graph.CompilerInputs[path] = hash
			} else {
				content, err = readAgentFile(path, 32<<20)
				if err != nil {
					return err
				}
				graph.CompilerInputs[path] = bytesSHA(content)
			}
			if metaHash, err := goFileHash(path + ".meta"); err == nil {
				graph.CompilerInputs[path+".meta"] = metaHash
				identity, err := metadataIndex.read(path + ".meta")
				if err != nil {
					return err
				}
				if identity.GUID != "" {
					if previous := metadataOwners[identity.GUID]; previous != "" && previous != path {
						return errors.New("managed source metadata GUID collision: " + identity.GUID + " between " + previous + " and " + path)
					}
					metadataOwners[identity.GUID] = path
				}
			} else if !os.IsNotExist(err) {
				return err
			}
			switch ext {
			case ".cs":
				sources[path] = tree.pkg
			case ".asmdef":
				var definition goSourceAsmdef
				if err := json.Unmarshal(content, &definition); err != nil {
					return fmt.Errorf("asmdef %s: %w", path, err)
				}
				if definition.Name == "" || filepath.Base(definition.Name) != definition.Name || strings.ContainsAny(definition.Name, "\x00\r\n;:") {
					return errors.New("invalid asmdef name: " + path)
				}
				if definitions[definition.Name] != nil {
					return errors.New("duplicate asmdef assembly name: " + definition.Name)
				}
				guid, err := readGoMetadataGUID(path + ".meta")
				if err != nil {
					return err
				}
				if byGUID[guid] != "" {
					return errors.New("duplicate asmdef GUID: " + guid)
				}
				definition.Path, definition.Root, definition.GUID, definition.Package = path, filepath.Dir(path), guid, tree.pkg
				if roots[definition.Root] != "" {
					return errors.New("multiple assembly roots in one directory: " + definition.Root)
				}
				definitions[definition.Name] = &definition
				byGUID[guid] = definition.Name
				roots[definition.Root] = definition.Name
			case ".asmref":
				var reference struct {
					Reference string `json:"reference"`
				}
				if err := json.Unmarshal(content, &reference); err != nil || reference.Reference == "" {
					return errors.New("invalid asmref: " + path)
				}
				if asmrefs[filepath.Dir(path)] != "" {
					return errors.New("multiple asmrefs in one directory: " + filepath.Dir(path))
				}
				asmrefs[filepath.Dir(path)] = reference.Reference
			case ".dll":
				plugin, err := readGoSourcePlugin(path, base)
				if err != nil {
					return err
				}
				if plugin.Enabled && goSourceManagedPlugin(path) {
					if _, exists := plugins[filepath.Base(path)]; exists {
						return errors.New("duplicate active managed plugin filename: " + filepath.Base(path))
					}
					plugins[filepath.Base(path)] = plugin
				}
			}
			return nil
		}); err != nil {
			return nil, err
		}
	}
	resolve := func(reference string) (string, error) {
		if strings.HasPrefix(reference, "GUID:") {
			reference = byGUID[strings.ToLower(strings.TrimPrefix(reference, "GUID:"))]
		}
		if reference == "" {
			return "", errors.New("assembly reference GUID is unresolved")
		}
		if definitions[reference] == nil && profile.NativeAssemblies[reference].Name == "" {
			return "", errors.New("assembly reference is unresolved: " + reference)
		}
		return reference, nil
	}
	for root, reference := range asmrefs {
		if roots[root] != "" {
			return nil, errors.New("asmdef/asmref share directory: " + root)
		}
		name, err := resolve(reference)
		if err != nil {
			return nil, err
		}
		roots[root] = name
	}
	configs := map[string]goAssemblyConfig{}
	active := map[string]bool{}
	ownerPackage := map[string]string{}
	for name, definition := range definitions {
		defines := slices.Clone(base)
		for _, version := range definition.VersionDefines {
			if !goSourceSymbol.MatchString(version.Define) {
				return nil, errors.New("invalid asmdef version define: " + definition.Path)
			}
			if current := versions[version.Name]; current != "" {
				matched, err := matchGoSourceVersion(current, version.Expression)
				if err != nil {
					return nil, fmt.Errorf("%s version define: %w", name, err)
				}
				if matched {
					defines = append(defines, version.Define)
				}
			}
		}
		defines = sortedGoSourceStrings(defines)
		constraints, err := matchGoSourceConstraints(definition.DefineConstraints, defines)
		if err != nil {
			return nil, fmt.Errorf("%s constraints: %w", name, err)
		}
		included := len(definition.IncludePlatforms) == 0 || slices.Contains(definition.IncludePlatforms, "LinuxStandalone64") || slices.Contains(definition.IncludePlatforms, "Standalone")
		active[name] = included && !slices.Contains(definition.ExcludePlatforms, "LinuxStandalone64") && !slices.Contains(definition.ExcludePlatforms, "Standalone") && constraints && !goSourceTestAssembly(definition)
		config := goAssemblyConfig{Name: name, Defines: defines}
		config.CompilerOptions = slices.Clone(baseCompilerOptions)
		config.Config.AllowUnsafe = definition.AllowUnsafe
		config.Config.NoEngineReferences = definition.NoEngineReferences
		config.Config.OverrideReferences = definition.OverrideReferences
		config.Config.Precompiled = slices.Clone(definition.PrecompiledReferences)
		for _, reference := range definition.References {
			if !active[name] {
				break
			}
			dependency, err := resolve(reference)
			if err != nil {
				fact := profile.NativeAssemblies[name]
				pkg := pkgMap[definition.Package]
				if fact.Package != "" && fact.Package == pkg.Name && fact.PackageContentHash == pkg.ContentHash && slices.Contains(fact.UnresolvedReferences, reference) {
					continue
				}
				return nil, fmt.Errorf("%s: %w", name, err)
			}
			if engineAssemblyNames[dependency] && !definition.NoEngineReferences {
				continue
			}
			if definitions[dependency] == nil && plugins[dependency+".dll"].Path != "" {
				continue
			}
			config.Dependencies = append(config.Dependencies, dependency)
		}
		configs[name] = config
		ownerPackage[name] = definition.Package
	}
	orderedSources := make([]string, 0, len(sources))
	for path := range sources {
		orderedSources = append(orderedSources, path)
	}
	sort.Strings(orderedSources)
	for _, path := range orderedSources {
		name := ""
		for folder := filepath.Dir(path); ; folder = filepath.Dir(folder) {
			if roots[folder] != "" {
				name = roots[folder]
				break
			}
			if filepath.Dir(folder) == folder {
				break
			}
		}
		if name == "" {
			logical := goSourceLogicalPath(project, options.Packages, path)
			if editorGoSource(path) {
				continue
			}
			name = "Assembly-CSharp"
			if strings.HasPrefix(logical, "Assets/Plugins/") || strings.HasPrefix(logical, "Assets/Standard Assets/") || strings.HasPrefix(logical, "Assets/Pro Standard Assets/") {
				name = "Assembly-CSharp-firstpass"
			}
			if configs[name].Name == "" {
				config := goAssemblyConfig{Name: name, Defines: slices.Clone(base)}
				config.CompilerOptions = slices.Clone(baseCompilerOptions)
				config.Config.AllowUnsafe = settings.AllowUnsafe
				configs[name] = config
				active[name] = true
			}
		}
		if !active[name] {
			continue
		}
		config := configs[name]
		config.Sources = append(config.Sources, path)
		configs[name] = config
	}
	for name, config := range configs {
		if !active[name] || len(config.Sources) == 0 {
			continue
		}
		if !config.Config.NoEngineReferences {
			for _, dependency := range automaticEngineAssemblies {
				if dependency != name && active[dependency] && len(configs[dependency].Sources) > 0 {
					config.Dependencies = append(config.Dependencies, dependency)
				}
			}
		}
		if name == "Assembly-CSharp" || name == "Assembly-CSharp-firstpass" {
			for dependency, definition := range definitions {
				if active[dependency] && len(configs[dependency].Sources) > 0 && (definition.AutoReferenced == nil || *definition.AutoReferenced) {
					config.Dependencies = append(config.Dependencies, dependency)
				}
			}
			if name == "Assembly-CSharp" && len(configs["Assembly-CSharp-firstpass"].Sources) > 0 {
				config.Dependencies = append(config.Dependencies, "Assembly-CSharp-firstpass")
			}
		}
		for _, dependency := range config.Dependencies {
			if definitions[dependency] != nil && (!active[dependency] || len(configs[dependency].Sources) == 0) {
				return nil, fmt.Errorf("active assembly %s references excluded/empty assembly %s", name, dependency)
			}
		}
		config.Dependencies = sortedGoSourceStrings(config.Dependencies)
		config.Sources = sortedGoSourceStrings(config.Sources)
		if err := applyGoSourceResponseFiles(project, config.Sources, settings.Arguments, &config, plugins, graph.CompilerInputs); err != nil {
			return nil, err
		}
		config.CompilerReferences, err = goSourcePluginReferences(config, plugins)
		if err != nil {
			return nil, err
		}
		configs[name] = config
	}
	for _, name := range sortedGoSourceConfigNames(configs) {
		config := configs[name]
		if !active[name] || len(config.Sources) == 0 {
			continue
		}
		pkg := pkgMap[ownerPackage[name]]
		fact := profile.NativeAssemblies[name]
		native := pkg.Name != "" && fact.Name != "" && fact.Package == pkg.Name && fact.PackageContentHash == pkg.ContentHash && equalStrings(sortedGoSourceStrings(fact.Configuration.Defines), config.Defines)
		if native {
			for _, source := range config.Sources {
				logical := goSourceLogicalPath(project, options.Packages, source)
				if fact.SourceHashes[logical] == "" || fact.SourceHashes[logical] != graph.CompilerInputs[source] || fact.SourceHashes[logical+".meta"] != graph.CompilerInputs[source+".meta"] {
					native = false
					break
				}
			}
		}
		if name == "Dungeon.CLIPlay" {
			// Editor-only adapters may change independently. The bridge's exact
			// runtime source and asmdef/config proof must still match its DLL.
			native = fact.Name == name && equalStrings(sortedGoSourceStrings(fact.Configuration.Defines), config.Defines) && fact.Configuration.Config.AllowUnsafe == config.Config.AllowUnsafe && fact.Configuration.Config.NoEngineReferences == config.Config.NoEngineReferences && fact.Configuration.Config.OverrideReferences == config.Config.OverrideReferences && equalStrings(sortedGoSourceStrings(fact.Configuration.Dependencies), config.Dependencies) && equalStrings(sortedGoSourceStrings(fact.Configuration.Config.Precompiled), sortedGoSourceStrings(config.Config.Precompiled))
			for logical, expected := range fact.SourceHashes {
				path := goSourcePhysicalPath(project, options.Packages, logical)
				if path == "" || graph.CompilerInputs[path] != expected {
					native = false
					break
				}
			}
			for _, source := range config.Sources {
				logical := goSourceLogicalPath(project, options.Packages, source)
				if fact.SourceHashes[logical] != graph.CompilerInputs[source] || fact.SourceHashes[logical+".meta"] != graph.CompilerInputs[source+".meta"] {
					native = false
					break
				}
			}
			if !native {
				return nil, errors.New("runtime bridge source/configuration differs from its native source-build proof; prepare a matching SDK bridge profile")
			}
		}
		for _, source := range config.Sources {
			guid, err := readGoMetadataGUID(source + ".meta")
			if err != nil {
				return nil, err
			}
			graph.Scripts = append(graph.Scripts, goCompileScript{GUID: guid, Path: source, Assembly: name, HostProvided: native})
		}
		if native {
			graph.HostAssemblies = append(graph.HostAssemblies, goCompilerHostAssembly{Name: name, Path: filepath.Join(goSourceManagedDirectory(options.Player), name+".dll"), SHA256: fact.SHA256, Sources: config.Sources, Defines: config.Defines, EditorDefines: append(slices.Clone(config.Defines), "UNITY_EDITOR", "UNITY_EDITOR_LINUX"), Configuration: &config, CompiledReferences: config.CompilerReferences, ResponseFiles: config.ResponseFiles})
		} else {
			graph.Assemblies = append(graph.Assemblies, goCompilerGraphAssembly{goAssemblyConfig: config, References: config.Dependencies, CompiledReferences: config.CompilerReferences, ResponseFiles: config.ResponseFiles})
		}
	}
	if len(graph.Assemblies)+len(graph.HostAssemblies) > 256 {
		return nil, errors.New("Go source graph exceeds256 runtime assemblies")
	}
	if err := options.Packages.Validate(ctx); err != nil {
		return nil, err
	}
	return graph, nil
}

// These native script assemblies participate in Unity's default engine
// reference set. Infer the pinned table from complete public build configs,
// rather than turning arbitrary autoReferenced user asmdefs into engine refs.
func goSourceAutomaticEngineAssemblies(profile *goSourceEngineProfile) []string {
	var result []string
	for name, candidate := range profile.NativeAssemblies {
		if !strings.HasPrefix(name, "UnityEngine.") || candidate.Package == "" || len(candidate.SourceHashes) == 0 {
			continue
		}
		count := 0
		shared := true
		for otherName, fact := range profile.NativeAssemblies {
			if otherName == name || fact.Configuration.Config.NoEngineReferences || len(fact.SourceHashes) == 0 {
				continue
			}
			count++
			if !slices.Contains(fact.Configuration.Dependencies, name) {
				shared = false
				break
			}
		}
		if shared && count > 0 {
			result = append(result, name)
		}
	}
	return sortedGoSourceStrings(result)
}

func goSourceBaseCompilerOptions(profile *goSourceEngineProfile) []string {
	var result []string
	first := true
	for _, name := range sortedGoSourceNativeNames(profile.NativeAssemblies) {
		fact := profile.NativeAssemblies[name]
		if len(fact.SourceHashes) == 0 {
			continue
		}
		if first {
			result = slices.Clone(fact.Configuration.CompilerOptions)
			first = false
			continue
		}
		result = slices.DeleteFunc(result, func(option string) bool { return !slices.Contains(fact.Configuration.CompilerOptions, option) })
	}
	// Only defaults whose behavior can be preserved exactly are harvested.
	// Arbitrary authored compiler options are parsed separately from source.
	result = slices.DeleteFunc(result, func(option string) bool {
		return !strings.HasPrefix(option, "/nowarn:") && !strings.HasPrefix(option, "-nowarn:")
	})
	return result
}
func sortedGoSourceNativeNames(facts map[string]goSourceNativeAssembly) []string {
	var names []string
	for name := range facts {
		names = append(names, name)
	}
	return sortedGoSourceStrings(names)
}

func goSourceManagedPlugin(path string) bool {
	image, err := pe.Open(path)
	if err != nil {
		return false
	}
	defer image.Close()
	switch header := image.OptionalHeader.(type) {
	case *pe.OptionalHeader32:
		return header.NumberOfRvaAndSizes > 14 && header.DataDirectory[14].VirtualAddress != 0 && header.DataDirectory[14].Size != 0
	case *pe.OptionalHeader64:
		return header.NumberOfRvaAndSizes > 14 && header.DataDirectory[14].VirtualAddress != 0 && header.DataDirectory[14].Size != 0
	}
	return false
}

func goSourceTestAssembly(definition *goSourceAsmdef) bool {
	if definition.Name == "UnityEngine.TestRunner" || definition.Name == "UnityEditor.TestRunner" {
		return true
	}
	for _, entries := range [][]string{definition.OptionalUnityReferences, definition.References, definition.PrecompiledReferences, definition.DefineConstraints} {
		for _, value := range entries {
			if value == "TestAssemblies" || value == "UnityEngine.TestRunner" || value == "UnityEditor.TestRunner" || value == "nunit.framework.dll" || value == "UNITY_INCLUDE_TESTS" {
				return true
			}
		}
	}
	return false
}

func sortedGoSourceConfigNames(configs map[string]goAssemblyConfig) []string {
	names := make([]string, 0, len(configs))
	for name := range configs {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

func matchGoSourceConstraints(constraints, defines []string) (bool, error) {
	for _, constraint := range constraints {
		matched := false
		for _, term := range strings.Split(constraint, "||") {
			term = strings.TrimSpace(term)
			negative := strings.HasPrefix(term, "!")
			if negative {
				term = strings.TrimPrefix(term, "!")
			}
			if !goSourceSymbol.MatchString(term) {
				return false, errors.New("unsupported define constraint: " + constraint)
			}
			present := slices.Contains(defines, term)
			matched = matched || (present != negative)
		}
		if !matched {
			return false, nil
		}
	}
	return true, nil
}
