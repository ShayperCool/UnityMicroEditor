package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
)

func prepareGoSourceCompilerGraph(ctx context.Context, options goSourceGraphOptions, output string) (*goCompilerPreparationResult, error) {
	if err := validateGoSourceOutput(options.Project, output); err != nil {
		return nil, err
	}
	graph, err := buildGoSourceGraph(ctx, options)
	if err != nil {
		return nil, err
	}
	if err := agentCanonical(output); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
		return nil, err
	}
	file, err := os.CreateTemp(filepath.Dir(output), ".go-source-graph-*.json")
	if err != nil {
		return nil, err
	}
	graphFile := file.Name()
	file.Close()
	defer os.Remove(graphFile)
	if err := atomicJSON(graphFile, graph); err != nil {
		return nil, err
	}
	intermediate := filepath.Join(filepath.Dir(output), ".go-source-preparation-"+agentID()+".json")
	defer os.Remove(intermediate)
	result, err := prepareGoCompilerGraph(ctx, options.Project, options.Editor, options.Player, graphFile, intermediate)
	if err != nil {
		return nil, err
	}
	if err := options.Packages.Validate(ctx); err != nil {
		return nil, err
	}
	current, err := buildGoSourceGraph(ctx, options)
	if err != nil {
		return nil, err
	}
	expectedJSON, _ := json.Marshal(graph)
	currentJSON, _ := json.Marshal(current)
	if string(expectedJSON) != string(currentJSON) {
		return nil, errors.New("authored source graph changed before compiler publication")
	}
	if err := publishAgentBaseline(output, result); err != nil {
		return nil, err
	}
	return result, nil
}

// Native MonoScript identities come from active Player declarations and exact
// native DLL metadata. No target Editor DLL or GetClass observation is needed.
func (compiler *goCompiler) deriveGoSourceHostScriptTypes(ctx context.Context, helper string, graph goCompilerGraphExport, managed string) error {
	type source struct {
		Path         string `json:"path"`
		OriginalPath string `json:"originalPath"`
	}
	type request struct {
		AssemblyName  string   `json:"assemblyName"`
		Defines       []string `json:"defines"`
		EditorDefines []string `json:"editorDefines"`
		Sources       []source `json:"sources"`
	}
	requests := []request{}
	paths := []string{}
	byAssembly := map[string]goCompilerHostAssembly{}
	for _, assembly := range graph.HostAssemblies {
		byAssembly[assembly.Name] = assembly
		entry := request{AssemblyName: assembly.Name, Defines: assembly.Defines, EditorDefines: assembly.Defines}
		for _, path := range assembly.Sources {
			hash, err := goFileHash(path)
			if err != nil || graph.CompilerInputs[path] == "" || hash != graph.CompilerInputs[path] {
				return errors.New("Go native source declaration differs from source graph: " + path)
			}
			entry.Sources = append(entry.Sources, source{path, path})
		}
		requests = append(requests, entry)
		paths = append(paths, filepath.Join(managed, assembly.Name+".dll"))
	}
	if len(requests) == 0 {
		return nil
	}
	requestPath := filepath.Join(compiler.cache, "go-native-source-declarations.json")
	if err := atomicJSON(requestPath, requests); err != nil {
		return err
	}
	output, err := goCompilerCommand(ctx, compiler.cache, compiler.dotnet, helper, "--source-type-declarations", requestPath)
	if err != nil {
		return fmt.Errorf("Go native Player source declarations: %w", err)
	}
	var declarations map[string]struct {
		Player map[string][]string `json:"player"`
	}
	if err := json.Unmarshal(output, &declarations); err != nil || len(declarations) != len(requests) {
		return errors.New("Go native Player source declaration proof is incomplete")
	}
	arguments := append([]string{helper, "--type-definitions"}, paths...)
	output, err = goCompilerCommand(ctx, compiler.cache, compiler.dotnet, arguments...)
	if err != nil {
		return err
	}
	var definitions map[string][]string
	if err := json.Unmarshal(output, &definitions); err != nil || len(definitions) != len(paths) {
		return errors.New("Go native DLL type inventory proof is incomplete")
	}
	kept := compiler.manifest.Scripts[:0]
	for _, script := range compiler.manifest.Scripts {
		if !script.HostProvided {
			kept = append(kept, script)
			continue
		}
		assembly := byAssembly[script.Assembly]
		names, exists := declarations[script.Assembly].Player[script.Path]
		if !exists {
			return errors.New("Go native source declaration is absent: " + script.Path)
		}
		base := strings.TrimSuffix(filepath.Base(script.Path), filepath.Ext(script.Path))
		candidate := ""
		for _, name := range names {
			if strings.ContainsAny(name, "+`") || name[strings.LastIndexByte(name, '.')+1:] != base {
				continue
			}
			if candidate != "" {
				return errors.New("Go native primary class declaration is ambiguous: " + script.Path)
			}
			candidate = name
		}
		if candidate == "" {
			continue
		}
		if !slices.Contains(definitions[filepath.Join(managed, assembly.Name+".dll")], candidate) {
			return errors.New("verified native profile lacks active source type: " + candidate)
		}
		script.TypeName = candidate
		kept = append(kept, script)
	}
	compiler.manifest.Scripts = kept
	return nil
}

// The pinned public BuildPipeline writes this registration table. SDK source
// overlays use verified public wrapper methods at raw-loader lifecycle phases;
// retaining their old native rows would run original methods twice.
func removeGoSourceNativeInitializers(filename string, assemblies []goCompileAssembly) error {
	content, err := readAgentFile(filename, 16<<20)
	if os.IsNotExist(err) {
		return nil
	}
	if err != nil {
		return err
	}
	var table map[string]json.RawMessage
	if err := json.Unmarshal(content, &table); err != nil {
		return err
	}
	var rows []map[string]json.RawMessage
	if err := json.Unmarshal(table["root"], &rows); err != nil {
		return errors.New("pinned native initializer table has unknown schema")
	}
	names := map[string]bool{}
	for _, assembly := range assemblies {
		names[assembly.Name] = true
	}
	kept := rows[:0]
	for _, row := range rows {
		var assembly string
		if json.Unmarshal(row["assemblyName"], &assembly) != nil || assembly == "" {
			return errors.New("native initializer row lacks assembly identity")
		}
		if !names[assembly] {
			kept = append(kept, row)
		}
	}
	table["root"], err = json.Marshal(kept)
	if err != nil {
		return err
	}
	return atomicJSON(filename, table)
}

type goSourceGraphDifference struct {
	Assembly string   `json:"assembly"`
	Field    string   `json:"field"`
	Own      []string `json:"own"`
	Editor   []string `json:"editor"`
}

func compareGoSourceGraphs(own, reference *goCompilerGraphExport) []goSourceGraphDifference {
	type node struct {
		configuration             goAssemblyConfig
		references, responseFiles []string
		complete                  bool
	}
	flatten := func(graph *goCompilerGraphExport) map[string]node {
		result := map[string]node{}
		for _, assembly := range graph.Assemblies {
			config := assembly.goAssemblyConfig
			if len(config.Dependencies) == 0 {
				config.Dependencies = assembly.References
			}
			result[assembly.Name] = node{config, assembly.CompiledReferences, assembly.ResponseFiles, true}
		}
		for _, assembly := range graph.HostAssemblies {
			config := goAssemblyConfig{Name: assembly.Name, Sources: assembly.Sources, Defines: assembly.Defines}
			if assembly.Configuration != nil {
				config = *assembly.Configuration
			}
			result[assembly.Name] = node{config, assembly.CompiledReferences, assembly.ResponseFiles, assembly.Configuration != nil}
		}
		return result
	}
	first, second := flatten(own), flatten(reference)
	names := []string{}
	for name := range first {
		names = append(names, name)
	}
	for name := range second {
		names = append(names, name)
	}
	names = sortedGoSourceStrings(names)
	logical := func(graph *goCompilerGraphExport, paths []string) []string {
		result := []string{}
		for _, path := range paths {
			mapped := false
			for _, root := range graph.SourceRoots {
				if withinDirectory(path, root.Source) {
					relative, _ := filepath.Rel(root.Source, path)
					path = root.Path + "/" + filepath.ToSlash(relative)
					mapped = true
					break
				}
			}
			if !mapped {
				relative, err := filepath.Rel(graph.Project, path)
				if err == nil && !strings.HasPrefix(relative, "..") {
					path = filepath.ToSlash(relative)
				}
			}
			result = append(result, path)
		}
		return sortedGoSourceStrings(result)
	}
	var differences []goSourceGraphDifference
	for _, name := range names {
		an, aExists := first[name]
		bn, bExists := second[name]
		a, b := an.configuration, bn.configuration
		if aExists != bExists {
			differences = append(differences, goSourceGraphDifference{name, "assembly", []string{fmt.Sprint(aExists)}, []string{fmt.Sprint(bExists)}})
			continue
		}
		for _, field := range []struct {
			name           string
			own, reference []string
		}{{"sources", logical(own, a.Sources), logical(reference, b.Sources)}, {"defines", sortedGoSourceStrings(a.Defines), sortedGoSourceStrings(b.Defines)}} {
			if !equalStrings(field.own, field.reference) {
				differences = append(differences, goSourceGraphDifference{name, field.name, field.own, field.reference})
			}
		}
		if bn.complete {
			if !equalStrings(sortedGoSourceStrings(a.Dependencies), sortedGoSourceStrings(b.Dependencies)) {
				differences = append(differences, goSourceGraphDifference{name, "dependencies", sortedGoSourceStrings(a.Dependencies), sortedGoSourceStrings(b.Dependencies)})
			}
			for _, field := range []struct {
				name string
				a, b []string
			}{
				{"unsafe", []string{fmt.Sprint(a.Config.AllowUnsafe)}, []string{fmt.Sprint(b.Config.AllowUnsafe)}},
				{"noEngineReferences", []string{fmt.Sprint(a.Config.NoEngineReferences)}, []string{fmt.Sprint(b.Config.NoEngineReferences)}},
				{"overrideReferences", []string{fmt.Sprint(a.Config.OverrideReferences)}, []string{fmt.Sprint(b.Config.OverrideReferences)}},
				{"precompiledReferences", sortedGoSourceStrings(a.Config.Precompiled), sortedGoSourceStrings(b.Config.Precompiled)},
				{"compilerOptions", a.CompilerOptions, b.CompilerOptions},
				{"responseFiles", logical(own, an.responseFiles), logical(reference, bn.responseFiles)},
				{"pluginReferences", logical(own, goSourcePluginReferencePaths(own, an.references)), logical(reference, goSourcePluginReferencePaths(reference, bn.references))},
			} {
				if !equalStrings(field.a, field.b) {
					differences = append(differences, goSourceGraphDifference{name, field.name, field.a, field.b})
				}
			}
		}
	}
	sort.Slice(differences, func(i, j int) bool {
		return differences[i].Assembly+differences[i].Field < differences[j].Assembly+differences[j].Field
	})
	return differences
}

func goSourcePluginReferencePaths(graph *goCompilerGraphExport, paths []string) []string {
	result := []string{}
	for _, path := range paths {
		if filepath.Ext(path) != ".dll" {
			continue
		}
		selected := withinDirectory(path, filepath.Join(graph.Project, "Assets"))
		for _, root := range graph.SourceRoots {
			if withinDirectory(path, root.Source) {
				selected = true
				break
			}
		}
		if selected {
			result = append(result, path)
		}
	}
	return result
}
