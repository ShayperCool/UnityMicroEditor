package main

import (
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"strings"
)

// A process restart must retain the verified compiled graph, rather than
// rebuilding the old preparation's changed assemblies on every launch.
// The entry owns no mutable project content: it names an immutable Player
// and verifies every file before that Player is selected again.
type goCodeRevision struct {
	SchemaVersion int                         `json:"schemaVersion"`
	Key           string                      `json:"key"`
	Player        string                      `json:"player"` // relative to compiler.cache
	Files         map[string]string           `json:"files"`  // relative to Player directory
	Sources       map[string]string           `json:"sources"`
	Proofs        map[string]goLifecycleProof `json:"proofs"`
}

func goPlayerFiles(directory string) (map[string]string, error) {
	canonical, err := filepath.EvalSymlinks(directory)
	if err != nil || canonical != filepath.Clean(directory) {
		return nil, errors.New("compiler Player snapshot must be a canonical directory")
	}
	files := map[string]string{}
	err = filepath.WalkDir(directory, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() {
			return nil
		}
		if !entry.Type().IsRegular() || len(files) >= 4096 {
			return errors.New("compiler Player snapshot contains an unsupported file")
		}
		hash, err := goFileHash(path)
		if err != nil {
			return err
		}
		relative, err := filepath.Rel(directory, path)
		if err != nil {
			return err
		}
		files[filepath.ToSlash(relative)] = hash
		return nil
	})
	if err == nil && len(files) == 0 {
		err = errors.New("compiler Player snapshot has no files")
	}
	return files, err
}

func (compiler *goCompiler) codeRevisionKey(ctx context.Context, sources map[string]string) (string, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	player := compiler.baselinePlayer
	if player == "" {
		player = compiler.player
	}
	playerFiles, err := goPlayerFiles(filepath.Dir(player))
	if err != nil {
		return "", err
	}
	helper, err := compiler.prepareLifecycleHelper(ctx)
	if err != nil {
		return "", err
	}
	var helperFiles map[string]string
	if err := readJSONFile(filepath.Join(filepath.Dir(helper), "files.json"), &helperFiles); err != nil ||
		helperFiles["LifecycleCompiler.dll"] == "" || !verifyGoCacheFiles(filepath.Dir(helper), helperFiles) {
		return "", errors.New("lifecycle revision helper payload is not verified")
	}
	toolFiles := map[string]string{}
	paths := append([]string{compiler.dotnet}, compiler.baseReferences...)
	paths = append(paths, compiler.engineReferences...)
	paths = append(paths, compiler.generators...)
	roslyn, err := filepath.Glob(filepath.Join(filepath.Dir(compiler.csc), "*.dll"))
	if err != nil {
		return "", err
	}
	paths = append(paths, roslyn...)
	for _, generator := range compiler.generators {
		dependencies, err := filepath.Glob(filepath.Join(filepath.Dir(generator), "*.dll"))
		if err != nil {
			return "", err
		}
		paths = append(paths, dependencies...)
	}
	for _, path := range paths {
		if _, done := toolFiles[path]; done {
			continue
		}
		hash, err := goFileHash(path)
		if err != nil {
			return "", err
		}
		toolFiles[path] = hash
	}
	netcodeKey := ""
	for _, assembly := range compiler.manifest.Assemblies {
		if goNeedsNetcode(assembly) {
			processor, err := compiler.prepareNetcodeProcessor(ctx)
			if err != nil {
				return "", err
			}
			netcodeKey = processor.Key
			break
		}
	}
	identity := struct {
		Mode                 string
		PlayerName           string
		PlayerFiles          map[string]string
		Plan                 goCompileManifest
		Sources              map[string]string
		Configuration        map[string]string
		AssemblyConfigs      map[string]string
		ExpectedTypes        map[string]map[string]string
		Tools                map[string]string
		Lifecycle            map[string]string
		NetcodePostprocessor string
	}{"go-code-revision-v1", filepath.Base(player), playerFiles, compiler.manifest, sources,
		compiler.configuration, compiler.configurationHashes, compiler.expectedTypes, toolFiles, helperFiles, netcodeKey}
	encoded, err := json.Marshal(identity)
	if err != nil {
		return "", err
	}
	return bytesSHA(encoded), nil
}

func (compiler *goCompiler) codeRevisionPath(key string) string {
	return filepath.Join(compiler.cache, "code-revisions", key+".json")
}

func (compiler *goCompiler) saveCodeRevision(key string, update *compileUpdate, proofs map[string]goLifecycleProof) error {
	player, err := filepath.Rel(compiler.cache, update.Player)
	if err != nil || !withinDirectory(update.Player, compiler.cache) {
		return errors.New("compiled revision Player is outside owned compiler cache")
	}
	files, err := goPlayerFiles(filepath.Dir(update.Player))
	if err != nil {
		return err
	}
	entry := goCodeRevision{SchemaVersion: 1, Key: key, Player: filepath.ToSlash(player), Files: files,
		Sources: update.SourceHashes, Proofs: proofs}
	if err := os.MkdirAll(filepath.Dir(compiler.codeRevisionPath(key)), 0700); err != nil {
		return err
	}
	return atomicJSON(compiler.codeRevisionPath(key), entry)
}

func (compiler *goCompiler) restoreCodeRevision(key string, sources map[string]string) *compileUpdate {
	filename := compiler.codeRevisionPath(key)
	info, err := os.Lstat(filename)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 8*1024*1024 {
		return nil
	}
	var entry goCodeRevision
	if readJSONFile(filename, &entry) != nil || entry.SchemaVersion != 1 || entry.Key != key ||
		!reflect.DeepEqual(entry.Sources, sources) || len(entry.Proofs) != len(compiler.manifest.Assemblies) ||
		filepath.IsAbs(entry.Player) || filepath.ToSlash(filepath.Clean(entry.Player)) != entry.Player {
		return nil
	}
	player := filepath.Join(compiler.cache, filepath.FromSlash(entry.Player))
	canonical, err := filepath.EvalSymlinks(player)
	if err != nil || canonical != player || !withinDirectory(player, compiler.cache) ||
		filepath.Base(player) != filepath.Base(compiler.player) {
		return nil
	}
	info, err = os.Stat(player)
	if err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0111 == 0 {
		return nil
	}
	files, err := goPlayerFiles(filepath.Dir(player))
	if err != nil || !reflect.DeepEqual(files, entry.Files) {
		return nil
	}
	managed := filepath.Join(filepath.Dir(player), strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data", "Managed")
	update := &compileUpdate{Player: player, CacheReused: true, SourceHashes: sources,
		CompiledAssemblies: []string{}, RuntimeInitializers: []runtimeInitializer{}, DeclarationChanged: []string{}, DLLHashes: map[string]string{}}
	for _, assembly := range compiler.manifest.Assemblies {
		proof, exists := entry.Proofs[assembly.Name]
		if !exists || !validGoLifecycleProof(proof, assembly.Name) || !goLifecycleSourcesMatch(proof, assembly.Sources) {
			return nil
		}
		dll := filepath.Join(managed, assembly.Name+".dll")
		relative, err := filepath.Rel(filepath.Dir(player), dll)
		if err != nil || entry.Files[filepath.ToSlash(relative)] == "" {
			return nil
		}
		update.Assemblies = append(update.Assemblies, dll)
		update.DLLHashes[assembly.Name] = entry.Files[filepath.ToSlash(relative)]
		update.RuntimeInitializers = append(update.RuntimeInitializers, proof.RuntimeInitializers...)
	}
	// Assignment waits until the complete descriptor and Player have passed.
	compiler.proofs = entry.Proofs
	return update
}

func (compiler *goCompiler) useCodeRevision(update *compileUpdate, proofs map[string]goLifecycleProof) {
	compiler.player, compiler.sources, compiler.proofs = update.Player, update.SourceHashes, proofs
	compiler.startupComplete = true
	if compiler.netcodeProcessed == nil {
		compiler.netcodeProcessed = map[string]bool{}
	}
	compiler.acceptedDLLHashes = update.DLLHashes
	for index, assembly := range compiler.manifest.Assemblies {
		compiler.currentDLLs[assembly.Name] = update.Assemblies[index]
		if goNeedsNetcode(assembly) {
			compiler.netcodeProcessed[assembly.Name] = true
		}
	}
}
