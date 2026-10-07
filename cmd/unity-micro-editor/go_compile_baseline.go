package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"path/filepath"
	"slices"
)

// PreparedManifest binds the accepted DLLs, sources and proofs into a new
// restartable baseline. A compilation can replace DLLs even with unchanged
// sources, for example when upgrading an old NGO postprocessing baseline.
func (compiler *goCompiler) PreparedManifest(ctx context.Context) (goCompileManifest, error) {
	var result goCompileManifest
	if !compiler.startupComplete {
		return result, errors.New("compiler graph must be accepted before exporting a prepared baseline")
	}
	if err := ctx.Err(); err != nil {
		return result, err
	}
	if err := compiler.checkConfiguration(nil); err != nil {
		return result, err
	}
	result = compiler.manifest
	result.Assemblies = slices.Clone(compiler.manifest.Assemblies)
	result.Scripts = slices.Clone(compiler.manifest.Scripts)
	result.CompilerInputs = maps.Clone(compiler.manifest.CompilerInputs)
	for source, expected := range compiler.sources {
		if actual, err := goFileHash(source); err != nil || actual != expected {
			return goCompileManifest{}, fmt.Errorf("source changed before prepared compiler publication: %s", source)
		}
		result.CompilerInputs[source] = expected
	}
	for path, hash := range compiler.configuration {
		result.CompilerInputs[path] = hash
	}
	for index, assembly := range result.Assemblies {
		assembly.Path = compiler.currentDLLs[assembly.Name]
		if assembly.Path == "" {
			return goCompileManifest{}, errors.New("accepted compiler graph lacks DLL: " + assembly.Name)
		}
		var err error
		assembly.SHA256, err = goFileHash(assembly.Path)
		if err != nil {
			return goCompileManifest{}, err
		}
		if compiler.acceptedDLLHashes[assembly.Name] == "" || assembly.SHA256 != compiler.acceptedDLLHashes[assembly.Name] {
			return goCompileManifest{}, errors.New("accepted DLL changed before prepared compiler publication: " + assembly.Name)
		}
		assembly.LifecycleBarrier = compiler.proofs[assembly.Name]
		if !validGoLifecycleProof(assembly.LifecycleBarrier, assembly.Name) || !goLifecycleSourcesMatch(assembly.LifecycleBarrier, assembly.Sources) {
			return goCompileManifest{}, errors.New("accepted compiler graph lacks lifecycle proof: " + assembly.Name)
		}
		if goNeedsNetcode(assembly) && compiler.netcodeProcessed[assembly.Name] {
			processor, err := compiler.prepareNetcodeProcessor(ctx)
			if err != nil {
				return goCompileManifest{}, err
			}
			assembly.NetcodePostprocessor = processor.Key
		}
		configuration, err := canonicalGoConfiguration(compiler.configs[assembly.Name])
		if err != nil {
			return goCompileManifest{}, err
		}
		identity, err := json.Marshal(struct {
			Mode          string
			Configuration json.RawMessage
			Assembly      goCompileAssembly
		}{"prepared-compiler-assembly-v1", configuration, assembly})
		if err != nil {
			return goCompileManifest{}, err
		}
		assembly.AssemblyInputHash = bytesSHA(identity)
		descriptor := map[string]any{"assemblyConfiguration": json.RawMessage(configuration),
			"backend": compiler.manifest.Backend, "engineProfileSHA256": compiler.manifest.EngineProfileSHA256, "packageResolutionSHA256": compiler.manifest.PackageResolutionSHA256,
			"compilerInputs": map[string]string{"assemblyConfiguration": bytesSHA(configuration)},
			"assemblies":     []goCompileAssembly{assembly}}
		if err := agentImmutableJSON(filepath.Join(compiler.cache, "baseline-assemblies", assembly.AssemblyInputHash, "compile.json"), descriptor); err != nil {
			return goCompileManifest{}, err
		}
		result.Assemblies[index] = assembly
	}
	for index, script := range result.Scripts {
		if !script.HostProvided {
			script.TypeName = compiler.proofs[script.Assembly].ScriptTypes[script.Path]
			result.Scripts[index] = script
		}
	}
	if err := compiler.checkConfiguration(nil); err != nil {
		return goCompileManifest{}, err
	}
	return result, nil
}
