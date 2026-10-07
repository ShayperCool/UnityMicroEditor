package main

import (
	"context"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
)

// stageGoHost refreshes our managed bridge with the SDK compiler. The native
// Player and its modules remain the already prepared, matching Unity runtime.
func stageGoHost(ctx context.Context, opts launchOptions, baseline baselineBuild) (string, error) {
	toolRoot, err := microEditorRoot()
	if err != nil {
		return "", err
	}
	runtimeRoot := filepath.Join(toolRoot, "Packages/com.unitymicroeditor.runtime/Runtime")
	destination := filepath.Join(opts.Output, "players", randomID())
	if err := os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
		return "", err
	}
	copy := exec.CommandContext(ctx, "cp", "--reflink=auto", "-a", "--", filepath.Dir(baseline.Player), destination)
	if data, err := copy.CombinedOutput(); err != nil {
		return "", fmt.Errorf("stage native host: %w: %s", err, data)
	}
	dataDir := filepath.Join(destination, strings.TrimSuffix(filepath.Base(baseline.Player), filepath.Ext(baseline.Player))+"_Data")
	managed := filepath.Join(dataDir, "Managed")
	data := filepath.Join(filepath.Dir(opts.Editor), "Data")
	var sources []string
	if err := filepath.WalkDir(runtimeRoot, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if !entry.IsDir() && strings.HasSuffix(path, ".cs") {
			sources = append(sources, path)
		}
		return nil
	}); err != nil {
		return "", err
	}
	sort.Strings(sources)
	input := []byte("unity-play-managed-bridge-v2\x00" + opts.Editor + "\x00")
	snapshots := make(map[string][]byte)
	for _, path := range sources {
		contents, err := os.ReadFile(path)
		if err != nil {
			return "", err
		}
		input = append(input, []byte(path)...)
		input = append(input, contents...)
		snapshots[path] = contents
	}
	cache := filepath.Join(opts.Output, "bridge", bytesSHA(input))
	dll := filepath.Join(cache, "UnityMicroEditor.dll")
	var cached struct {
		SHA256 string `json:"sha256"`
	}
	actualHash, hashErr := goFileHash(dll)
	if readJSONFile(filepath.Join(cache, "artifact.json"), &cached) != nil || hashErr != nil || cached.SHA256 == "" || actualHash != cached.SHA256 {
		if err := os.MkdirAll(cache, 0700); err != nil {
			return "", err
		}
		stableSources := make([]string, 0, len(sources))
		for _, path := range sources {
			relative, err := filepath.Rel(runtimeRoot, path)
			if err != nil {
				return "", err
			}
			destination := filepath.Join(cache, "Sources", relative)
			if err := os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
				return "", err
			}
			if err := os.WriteFile(destination, snapshots[path], 0600); err != nil {
				return "", err
			}
			stableSources = append(stableSources, destination)
		}
		compilers, err := filepath.Glob(filepath.Join(data, "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll"))
		if err != nil || len(compilers) == 0 {
			return "", fmt.Errorf("Unity SDK compiler is missing: %v", err)
		}
		sort.Strings(compilers)
		arguments := []string{"-nologo", "-target:library", "-nostdlib+", "-langversion:9", "-warnaserror+", "-nowarn:0649,0618", "-unsafe+", "-out:" + dll}
		seen := map[string]bool{}
		for _, pattern := range []string{filepath.Join(data, "MonoBleedingEdge/lib/mono/unityaot-linux/*.dll"), filepath.Join(data, "MonoBleedingEdge/lib/mono/unityaot-linux/Facades/*.dll"), filepath.Join(managed, "*.dll")} {
			files, _ := filepath.Glob(pattern)
			for _, file := range files {
				base := filepath.Base(file)
				if base == "UnityMicroEditor.dll" || seen[base] {
					continue
				}
				seen[base] = true
				arguments = append(arguments, "-reference:"+file)
			}
		}
		arguments = append(arguments, stableSources...)
		response := filepath.Join(cache, "compile.rsp")
		var text strings.Builder
		for _, arg := range arguments {
			text.WriteString("\"" + strings.ReplaceAll(arg, "\"", "\\\"") + "\"\n")
		}
		if err := os.WriteFile(response, []byte(text.String()), 0600); err != nil {
			return "", err
		}
		compiler := exec.CommandContext(ctx, filepath.Join(data, "DotNetSdk/dotnet"), compilers[len(compilers)-1], "@"+response)
		if output, err := compiler.CombinedOutput(); err != nil {
			os.Remove(dll)
			return "", fmt.Errorf("compile Player bridge: %w\n%s", err, output)
		}
		cached.SHA256, err = goFileHash(dll)
		if err != nil {
			return "", err
		}
		if err := atomicJSON(filepath.Join(cache, "artifact.json"), cached); err != nil {
			return "", err
		}
	}
	contents, err := os.ReadFile(dll)
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(filepath.Join(managed, "UnityMicroEditor.dll"), contents, 0600); err != nil {
		return "", err
	}
	return filepath.Join(destination, filepath.Base(baseline.Player)), nil
}

func playerAssemblyPaths(player string, manifest string) ([]string, error) {
	var source struct {
		Assemblies []string `json:"assemblies"`
	}
	if err := readJSONFile(manifest, &source); err != nil {
		return nil, err
	}
	managed := filepath.Join(filepath.Dir(player), strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data", "Managed")
	for index, path := range source.Assemblies {
		source.Assemblies[index] = filepath.Join(managed, filepath.Base(path))
	}
	return source.Assemblies, nil
}
