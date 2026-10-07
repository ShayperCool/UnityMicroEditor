package main

import (
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
)

func (compiler *goCompiler) graphSourceAssemblies() map[string]string {
	result := map[string]string{}
	for _, assembly := range compiler.manifest.Assemblies {
		for _, source := range assembly.Sources {
			result[source] = assembly.Name
		}
	}
	return result
}

// The prepared assembly configuration remains authoritative. Only source
// additions under Assets can extend it; they cannot change assembly slots,
// defines, packages, compiler options, or existing MonoScript identities.
func (compiler *goCompiler) sourceGraphCandidate(changed []string) (*goCompiler, error) {
	if err := compiler.checkFixedConfiguration(); err != nil {
		return nil, err
	}
	tracked := compiler.graphSourceAssemblies()
	var additions []string
	err := filepath.WalkDir(filepath.Join(compiler.project, "Assets"), func(filename string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() && (compiler.hostOwned(filename) || filepath.Base(filename) == "Editor") {
			return filepath.SkipDir
		}
		if entry.IsDir() || compiler.manifest.CompilerInputs[filename] != "" || tracked[filename] != "" || editorGoSource(filename) {
			return nil
		}
		switch strings.ToLower(filepath.Ext(filename)) {
		case ".cs":
			if filepath.Ext(filename) != ".cs" {
				return errors.New("new runtime C# sources require the .cs extension: " + filename)
			}
			additions = append(additions, filename)
		case ".asmdef", ".asmref", ".rsp", ".dll":
			return errors.New("new managed configuration requires a fresh managed graph: " + filename)
		case ".meta":
			for _, suffix := range []string{".asmdef.meta", ".asmref.meta", ".rsp.meta", ".dll.meta"} {
				if strings.HasSuffix(filename, suffix) {
					return errors.New("new managed configuration metadata requires a fresh managed graph: " + filename)
				}
			}
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	candidate := *compiler
	candidate.currentDLLs = maps.Clone(compiler.currentDLLs)
	candidate.acceptedDLLHashes = maps.Clone(compiler.acceptedDLLHashes)
	candidate.netcodeProcessed = maps.Clone(compiler.netcodeProcessed)
	candidate.configuration = maps.Clone(compiler.configuration)
	if candidate.configuration == nil {
		candidate.configuration = map[string]string{}
	}
	candidate.configs = maps.Clone(compiler.configs)
	candidate.manifest.Assemblies = slices.Clone(compiler.manifest.Assemblies)
	candidate.manifest.Scripts = slices.Clone(compiler.manifest.Scripts)
	for index := range candidate.manifest.Assemblies {
		candidate.manifest.Assemblies[index].Sources = slices.Clone(compiler.manifest.Assemblies[index].Sources)
	}
	if len(additions) != 0 {
		roots, err := compiler.assemblySourceRoots()
		if err != nil {
			return nil, err
		}
		guids := map[string]string{}
		for _, script := range compiler.manifest.Scripts {
			if script.GUID != "" {
				guids[strings.ToLower(script.GUID)] = script.Path
			}
		}
		newGUIDs := map[string]string{}
		sort.Strings(additions)
		for _, source := range additions {
			logical, err := filepath.Rel(compiler.project, source)
			if err != nil {
				return nil, err
			}
			if _, err := registryExpectedFile(compiler.project, filepath.ToSlash(logical), "", false); err != nil {
				return nil, err
			}
			metaPath, err := registryExpectedFile(compiler.project, filepath.ToSlash(logical)+".meta", "", false)
			if err != nil {
				return nil, fmt.Errorf("new runtime source requires authored MonoScript .meta: %s: %w", source, err)
			}
			info, err := os.Stat(metaPath)
			if err != nil || info.Size() > 1024*1024 {
				return nil, errors.New("MonoScript metadata is not bounded: " + metaPath)
			}
			meta, err := os.ReadFile(metaPath)
			if err != nil {
				return nil, err
			}
			guid, err := registryMetaGUID(meta)
			if err != nil {
				return nil, fmt.Errorf("new MonoScript %s: %w", source, err)
			}
			if previous := guids[guid]; previous != "" {
				return nil, fmt.Errorf("new MonoScript GUID %s is already assigned to %s", guid, previous)
			}
			assembly, err := compiler.sourceAssembly(source, roots)
			if err != nil {
				return nil, err
			}
			guids[guid], newGUIDs[guid] = source, metaPath
			candidate.configuration[metaPath] = bytesSHA(meta)
			candidate.manifest.Scripts = append(candidate.manifest.Scripts, goCompileScript{GUID: guid, Path: source, Assembly: assembly})
			for index := range candidate.manifest.Assemblies {
				if candidate.manifest.Assemblies[index].Name == assembly {
					candidate.manifest.Assemblies[index].Sources = append(candidate.manifest.Assemblies[index].Sources, source)
					sort.Strings(candidate.manifest.Assemblies[index].Sources)
					config := candidate.configs[assembly]
					config.Sources = slices.Clone(candidate.manifest.Assemblies[index].Sources)
					candidate.configs[assembly] = config
				}
			}
		}
		// Asset identities are project-wide, including scenes and editor-only
		// scripts. A new MonoScript must not steal another asset's GUID.
		metadataIndex := newGoMetadataGUIDIndex()
		if err := filepath.WalkDir(filepath.Join(compiler.project, "Assets"), func(filename string, entry fs.DirEntry, err error) error {
			if err != nil || entry.IsDir() || !strings.HasSuffix(filename, ".meta") {
				return err
			}
			identity, err := metadataIndex.read(filename)
			if err != nil {
				return err
			}
			if newGUIDs[identity.GUID] != "" && newGUIDs[identity.GUID] != filename {
				return fmt.Errorf("new MonoScript GUID %s collides with asset metadata %s", identity.GUID, filename)
			}
			return nil
		}); err != nil {
			return nil, err
		}
		// Discovery order cannot change revision identities after a restart.
		sort.Slice(candidate.manifest.Scripts, func(i, j int) bool { return candidate.manifest.Scripts[i].Path < candidate.manifest.Scripts[j].Path })
	}
	if err := candidate.checkConfiguration(changed); err != nil {
		return nil, err
	}
	return &candidate, nil
}

func (compiler *goCompiler) assemblySourceRoots() (map[string]string, error) {
	roots, byGUID := map[string]string{}, map[string]string{}
	assign := func(filename, name string) error {
		root := filepath.Dir(filename)
		if _, exists := roots[root]; exists {
			return errors.New("multiple assembly definitions/references in one source directory: " + root)
		}
		roots[root] = name
		return nil
	}
	var references []string
	for filename := range compiler.configuration {
		switch filepath.Ext(filename) {
		case ".asmdef":
			var definition struct {
				Name string `json:"name"`
			}
			if err := readJSONFile(filename, &definition); err != nil || definition.Name == "" {
				return nil, errors.New("invalid prepared assembly definition: " + filename)
			}
			if err := assign(filename, definition.Name); err != nil {
				return nil, err
			}
			if data, err := os.ReadFile(filename + ".meta"); err == nil {
				guid, err := registryMetaGUID(data)
				if err != nil || byGUID[guid] != "" {
					return nil, errors.New("invalid or duplicate prepared assembly GUID: " + filename)
				}
				byGUID[guid] = definition.Name
			}
		case ".asmref":
			references = append(references, filename)
		}
	}
	for _, filename := range references {
		var reference struct {
			Reference string `json:"reference"`
		}
		if err := readJSONFile(filename, &reference); err != nil || reference.Reference == "" {
			return nil, errors.New("invalid prepared assembly reference: " + filename)
		}
		name := reference.Reference
		if strings.HasPrefix(name, "GUID:") {
			name = byGUID[strings.ToLower(strings.TrimPrefix(name, "GUID:"))]
			if name == "" {
				return nil, errors.New("prepared assembly reference has no verified definition GUID: " + filename)
			}
		}
		if err := assign(filename, name); err != nil {
			return nil, err
		}
	}
	return roots, nil
}

func (compiler *goCompiler) sourceAssembly(source string, roots map[string]string) (string, error) {
	assets := filepath.Join(compiler.project, "Assets")
	name := ""
	for folder := filepath.Dir(source); withinDirectory(folder, assets); folder = filepath.Dir(folder) {
		if resolved, exists := roots[folder]; exists {
			name = resolved
			break
		}
		if folder == assets {
			break
		}
	}
	if name == "" {
		name = "Assembly-CSharp"
		for _, folder := range []string{"Plugins", "Standard Assets", "Pro Standard Assets"} {
			if withinDirectory(source, filepath.Join(assets, folder)) {
				name = "Assembly-CSharp-firstpass"
			}
		}
	}
	if _, exists := compiler.configs[name]; !exists || compiler.currentDLLs[name] == "" {
		return "", fmt.Errorf("new source %s belongs to %s, which has no prepared runtime assembly/Mono slot; regenerate source graph", source, name)
	}
	return name, nil
}
