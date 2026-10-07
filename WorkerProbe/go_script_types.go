package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"sort"
	"strings"
)

// ScriptTypeNames reads type metadata without loading game code or the Editor.
// The prepared compiler graph ties each MonoScript GUID to its defining DLL.
func (compiler *goCompiler) ScriptTypeNames(ctx context.Context) (map[string]string, error) {
	helper, err := compiler.prepareLifecycleHelper(ctx)
	if err != nil {
		return nil, err
	}
	managed := filepath.Join(filepath.Dir(compiler.player), strings.TrimSuffix(filepath.Base(compiler.player), filepath.Ext(compiler.player))+"_Data", "Managed")
	byAssembly := map[string]string{}
	seenAssemblies := map[string]bool{}
	for _, script := range compiler.manifest.Scripts {
		if script.GUID == "" || script.Assembly == "" || seenAssemblies[script.Assembly] {
			continue
		}
		seenAssemblies[script.Assembly] = true
		path := compiler.currentDLLs[script.Assembly]
		if path == "" {
			path = filepath.Join(managed, script.Assembly+".dll")
		}
		if _, err := goFileHash(path); err == nil {
			byAssembly[script.Assembly] = path
		}
	}
	var paths []string
	for _, path := range byAssembly {
		paths = append(paths, path)
	}
	sort.Strings(paths)
	if len(paths) == 0 {
		if compiler.manifest.NativeOnly && len(compiler.manifest.Scripts) == 0 {
			return map[string]string{}, nil
		}
		return nil, errors.New("script type inventory has no defining assemblies")
	}
	cacheKey, err := compiler.scriptTypeCacheKey(helper, byAssembly)
	if err != nil {
		return nil, err
	}
	if result := compiler.readScriptTypeCache(cacheKey); result != nil {
		return result, nil
	}
	args := append([]string{helper, "--type-definitions"}, paths...)
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, args...)
	if err != nil {
		return nil, fmt.Errorf("read managed script types: %w", err)
	}
	var definitions map[string][]string
	if err := json.Unmarshal(output, &definitions); err != nil || len(definitions) != len(paths) {
		return nil, errors.New("SDK type inventory lacks exact assembly inputs")
	}
	result := map[string]string{}
	for _, script := range compiler.manifest.Scripts {
		path := byAssembly[script.Assembly]
		if script.GUID == "" || path == "" {
			continue
		}
		name := strings.TrimSuffix(filepath.Base(script.Path), filepath.Ext(script.Path))
		var candidates []string
		for _, full := range definitions[path] {
			if strings.Contains(full, "+") || strings.Contains(full, "`") {
				continue
			}
			if full[strings.LastIndexByte(full, '.')+1:] == name {
				candidates = append(candidates, full)
			}
		}
		if len(candidates) == 1 {
			result[script.GUID] = candidates[0]
		}
		if script.HostProvided && script.TypeName != "" {
			for _, full := range definitions[path] {
				if full == script.TypeName {
					result[script.GUID] = full
					break
				}
			}
		}
		if primary := compiler.proofs[script.Assembly].ScriptTypes[script.Path]; primary != "" {
			for _, full := range definitions[path] {
				if full == primary {
					result[script.GUID] = primary
					break
				}
			}
		}
		for expected, guid := range compiler.expectedTypes[script.Assembly] {
			if guid == script.GUID {
				for _, full := range definitions[path] {
					if full == expected {
						result[guid] = full
						break
					}
				}
			}
		}
	}
	if err := compiler.saveScriptTypeCache(cacheKey, result); err != nil {
		return nil, err
	}
	return result, nil
}
