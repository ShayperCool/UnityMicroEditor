package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

type goNetcodeProcessor struct {
	Helper string
	Inputs map[string]string
	Key    string
	Files  map[string]string
}

func goNeedsNetcode(assembly goCompileAssembly) bool {
	for _, name := range assembly.References {
		if name == "Unity.Netcode.Runtime" {
			return true
		}
	}
	return false
}

// Package sources are built unchanged without UNITY_EDITOR. The helper shares
// the package's assembly identity for its declared InternalsVisibleTo API.
func (compiler *goCompiler) prepareNetcodeProcessor(ctx context.Context) (*goNetcodeProcessor, error) {
	if compiler.netcode != nil {
		if !verifyGoCacheFiles(filepath.Dir(compiler.netcode.Helper), compiler.netcode.Files) {
			return nil, errors.New("Netcode postprocessor cached executable or dependency changed")
		}
		for filename, expected := range compiler.netcode.Inputs {
			if actual, err := goFileHash(filename); err != nil || actual != expected {
				return nil, errors.New("Netcode postprocessor input changed during session: " + filename)
			}
		}
		return compiler.netcode, nil
	}
	packages, _ := filepath.Glob(filepath.Join(compiler.project, "Library/PackageCache/com.unity.netcode.gameobjects@*/Editor/CodeGen"))
	cecilPackages, _ := filepath.Glob(filepath.Join(compiler.project, "Library/PackageCache/com.unity.nuget.mono-cecil@*"))
	if len(packages) != 1 || len(cecilPackages) != 1 {
		return nil, errors.New("Netcode requires one pinned package CodeGen and Mono.Cecil package")
	}
	toolRoot, err := microEditorRoot()
	if err != nil {
		return nil, err
	}
	sourceRoot := filepath.Join(toolRoot, "tools/NetcodePostProcessor")
	inputs := map[string]string{}
	copies := map[string]string{}
	add := func(filename, destination string) error {
		hash, err := goFileHash(filename)
		if err != nil {
			return err
		}
		inputs[filename], copies[filename] = hash, destination
		return nil
	}
	for _, name := range []string{"NetcodePostProcessor.csproj", "Program.cs"} {
		if err := add(filepath.Join(sourceRoot, name), name); err != nil {
			return nil, err
		}
	}
	sources, _ := filepath.Glob(filepath.Join(packages[0], "*.cs"))
	if len(sources) != 8 {
		return nil, errors.New("unsupported pinned NGO CodeGen source layout")
	}
	for _, filename := range sources {
		if err := add(filename, filepath.Join("CodeGen", filepath.Base(filename))); err != nil {
			return nil, err
		}
	}
	for _, name := range []string{"Mono.Cecil.dll", "Mono.Cecil.Rocks.dll", "Mono.Cecil.Pdb.dll", "Mono.Cecil.Mdb.dll"} {
		if err := add(filepath.Join(cecilPackages[0], name), filepath.Join("Dependencies", name)); err != nil {
			return nil, err
		}
	}
	data := filepath.Join(filepath.Dir(compiler.editor), "Data")
	common := filepath.Join(data, "Tools/BuildPipeline/Compilation/Unity.ILPP.Runner/Unity.CompilationPipeline.Common.dll")
	if err := add(common, filepath.Join("Dependencies", filepath.Base(common))); err != nil {
		return nil, err
	}
	player := compiler.player
	managed := filepath.Join(filepath.Dir(player), strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data", "Managed")
	for _, name := range []string{"Unity.Netcode.Runtime.dll", "Unity.Collections.dll", "UnityEngine.CoreModule.dll", "UnityEngine.ScriptingModule.dll"} {
		filename := filepath.Join(managed, name)
		if name == "Unity.Netcode.Runtime.dll" {
			// The runtime Player linker removes InternalsVisibleTo. The
			// package CodeGen build uses its unstripped runtime reference;
			// target ILPP still resolves the actual Player DLLs from request.
			filename = filepath.Join(compiler.project, "Library/Bee/artifacts/StandaloneLinux64_CodeGen", name)
		}
		if err := add(filename, filepath.Join("Dependencies", name)); err != nil {
			return nil, err
		}
	}
	for _, filename := range []string{compiler.dotnet, compiler.csc} {
		hash, err := goFileHash(filename)
		if err != nil {
			return nil, err
		}
		inputs[filename] = hash
	}
	// Conditional package APIs depend on package version, not on the user's
	// assemblies. Keep this helper's compile mode pinned and in its cache key.
	cacheInputs := map[string]string{"mode:netcode-runtime-v1": "!UNITY_EDITOR"}
	for filename, hash := range inputs {
		name := copies[filename]
		if name == "" {
			name = filename
		}
		cacheInputs[name] = hash
	}
	encoded, _ := json.Marshal(cacheInputs)
	key := bytesSHA(encoded)
	entry := filepath.Join(compiler.cache, "netcode-processor", key)
	var files map[string]string
	if readJSONFile(filepath.Join(entry, "files.json"), &files) == nil && files["Unity.Netcode.Editor.CodeGen.dll"] != "" && verifyGoCacheFiles(entry, files) {
		encodedFiles, _ := json.Marshal(files)
		compiler.netcode = &goNetcodeProcessor{Helper: filepath.Join(entry, "Unity.Netcode.Editor.CodeGen.dll"), Inputs: inputs, Files: files, Key: bytesSHA(append([]byte(key), encodedFiles...))}
		return compiler.netcode, nil
	}
	if err := os.MkdirAll(filepath.Dir(entry), 0700); err != nil {
		return nil, err
	}
	staging, err := os.MkdirTemp(filepath.Dir(entry), ".processor-build-")
	if err != nil {
		return nil, err
	}
	for source, relative := range copies {
		payload, err := os.ReadFile(source)
		if err != nil || bytesSHA(payload) != inputs[source] {
			return nil, errors.New("Netcode processor input changed during build: " + source)
		}
		destination := filepath.Join(staging, relative)
		if err := os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
			return nil, err
		}
		if err := os.WriteFile(destination, payload, 0600); err != nil {
			return nil, err
		}
	}
	if err := os.WriteFile(filepath.Join(staging, "NuGet.Config"), []byte("<configuration><packageSources><clear /></packageSources></configuration>\n"), 0600); err != nil {
		return nil, err
	}
	payload := filepath.Join(staging, "payload")
	if _, err := goCompilerCommand(ctx, staging, compiler.dotnet, "build", filepath.Join(staging, "NetcodePostProcessor.csproj"), "--nologo", "-v:q", "-o", payload, "-p:NuGetAudit=false"); err != nil {
		return nil, fmt.Errorf("build standalone NGO postprocessor: %w", err)
	}
	entries, err := os.ReadDir(payload)
	if err != nil {
		return nil, err
	}
	files = map[string]string{}
	for _, file := range entries {
		if file.Type().IsRegular() {
			hash, err := goFileHash(filepath.Join(payload, file.Name()))
			if err != nil {
				return nil, err
			}
			files[file.Name()] = hash
		}
	}
	if files["Unity.Netcode.Editor.CodeGen.dll"] == "" {
		return nil, errors.New("Netcode processor build produced no executable")
	}
	if err := atomicJSON(filepath.Join(payload, "files.json"), files); err != nil {
		return nil, err
	}
	if err := os.Rename(payload, entry); err != nil {
		return nil, err
	}
	encodedFiles, _ := json.Marshal(files)
	compiler.netcode = &goNetcodeProcessor{Helper: filepath.Join(entry, "Unity.Netcode.Editor.CodeGen.dll"), Inputs: inputs, Files: files, Key: bytesSHA(append([]byte(key), encodedFiles...))}
	return compiler.netcode, nil
}

func (compiler *goCompiler) postprocessNetcode(ctx context.Context, folder, dll string, assembly goCompileAssembly, refs []string, proof goLifecycleProof) (string, goLifecycleProof, error) {
	processor, err := compiler.prepareNetcodeProcessor(ctx)
	if err != nil {
		return "", proof, err
	}
	processed := filepath.Join(folder, "Netcode", assembly.Name+".dll")
	if err := os.MkdirAll(filepath.Dir(processed), 0700); err != nil {
		return "", proof, err
	}
	lifecycleMethods := []string{}
	for _, method := range proof.Methods {
		lifecycleMethods = append(lifecycleMethods, method.Type+"."+method.Phase)
	}
	for _, method := range proof.RuntimeInitializers {
		lifecycleMethods = append(lifecycleMethods, method.TypeName+"."+method.MethodName)
	}
	sort.Strings(lifecycleMethods)
	request := filepath.Join(folder, "netcode-request.json")
	if err := atomicJSON(request, map[string]any{"assembly": dll, "references": refs, "defines": compiler.configs[assembly.Name].Defines,
		"output": processed, "lifecycleMethods": lifecycleMethods}); err != nil {
		return "", proof, err
	}
	output, err := goCompilerCommand(ctx, folder, compiler.dotnet, processor.Helper, request)
	if err != nil {
		return "", proof, fmt.Errorf("NGO IL postprocess %s: %w", assembly.Name, err)
	}
	var result struct {
		SchemaVersion            int                  `json:"schemaVersion"`
		Verified                 bool                 `json:"verified"`
		Assembly                 string               `json:"assembly"`
		Required                 bool                 `json:"required"`
		LifecycleMethodsVerified int                  `json:"lifecycleMethodsVerified"`
		RuntimeInitializers      []runtimeInitializer `json:"runtimeInitializers"`
	}
	if err := json.Unmarshal(output, &result); err != nil || result.SchemaVersion != 1 || !result.Verified || result.Assembly != assembly.Name ||
		result.LifecycleMethodsVerified != len(lifecycleMethods) ||
		(result.Required && len(result.RuntimeInitializers) != 1) || (!result.Required && len(result.RuntimeInitializers) != 0) {
		return "", proof, errors.New("NGO postprocessor returned invalid verification proof")
	}
	proof.RuntimeInitializers = append(proof.RuntimeInitializers, result.RuntimeInitializers...)
	if !validGoLifecycleProof(proof, assembly.Name) {
		return "", proof, errors.New("NGO runtime serialization initializer proof is invalid")
	}
	if err := os.WriteFile(filepath.Join(folder, "netcode-proof.json"), output, 0600); err != nil {
		return "", proof, err
	}
	return processed, proof, nil
}
