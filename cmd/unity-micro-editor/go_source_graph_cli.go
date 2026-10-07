package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"

	"unitymicroeditor/internal/project/packages"
)

// Modes are explicit: harvesting uses a supplied public reference; graph and
// compile only read cached profile facts, authored sources and package proofs.
func runGoSourceGraphCLI(mode string, args []string) error {
	var project, editor, player, profilePath, reference, output string
	var offline bool
	flags := flag.NewFlagSet(mode, flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit target project")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "pinned installed Unity SDK")
	flags.StringVar(&player, "player", "", "matching native Mono host")
	flags.StringVar(&profilePath, "engine-profile", "", "cached pinned engine/native host facts")
	flags.StringVar(&reference, "reference", "", "explicit public reference graph for capture or comparison")
	flags.StringVar(&output, "output", "", "new owned cache JSON output")
	flags.BoolVar(&offline, "offline", false, "require local verified package snapshots")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || project == "" || player == "" || output == "" {
		return errors.New(mode + " requires --project --player --output")
	}
	project, err := filepath.Abs(project)
	if err != nil {
		return err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	if err := validateGoSourceOutput(project, output); err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	snapshot, err := packages.Resolve(ctx, project, editor, packages.Options{Offline: offline})
	if err != nil {
		return err
	}
	if mode == "engine-profile-capture" {
		if reference == "" {
			return errors.New(mode + " requires --reference")
		}
		profile, err := captureGoSourceEngineProfile(ctx, project, editor, player, reference, output, snapshot)
		if err != nil {
			return err
		}
		return json.NewEncoder(os.Stdout).Encode(profile)
	}
	if profilePath == "" {
		return errors.New(mode + " requires --engine-profile")
	}
	profile, err := loadGoSourceEngineProfile(ctx, profilePath, editor, player)
	if err != nil {
		return err
	}
	options := goSourceGraphOptions{Project: project, Editor: editor, Player: player, Profile: profile, Packages: snapshot}
	if mode == "compiler-source-prepare" {
		result, err := prepareGoSourceCompilerGraph(ctx, options, output)
		if err != nil {
			return err
		}
		return json.NewEncoder(os.Stdout).Encode(result)
	}
	if mode != "source-graph" {
		return errors.New("unknown source graph mode")
	}
	graph, err := buildGoSourceGraph(ctx, options)
	if err != nil {
		return err
	}
	if err := agentImmutableJSON(output, graph); err != nil {
		return err
	}
	var differences []goSourceGraphDifference
	if reference != "" {
		var oracle goCompilerGraphExport
		if err := agentReadJSON(reference, 32<<20, &oracle); err != nil {
			return err
		}
		differences = compareGoSourceGraphs(graph, &oracle)
	}
	return json.NewEncoder(os.Stdout).Encode(map[string]any{"backend": "go_authored_source", "graph": output, "engineProfile": profilePath, "sdkAssemblies": len(graph.Assemblies), "nativeAssemblies": len(graph.HostAssemblies), "differences": differences})
}
