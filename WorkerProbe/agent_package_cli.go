package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"os"
	"os/signal"
	"syscall"

	"unitymicroeditor/project/packages"
)

func runAgentPackageResolveCLI(args []string) error {
	var project, editor string
	var offline, current, refreshGit bool
	flags := flag.NewFlagSet("resolve-packages", flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit target Unity project")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "pinned SDK used only to read built-in package sources")
	flags.BoolVar(&offline, "offline", false, "require locally verified package sources")
	flags.BoolVar(&current, "current", false, "validate the durable current snapshot without resolution/network")
	flags.BoolVar(&refreshGit, "refresh-git", false, "explicitly resolve unlocked Git refs again")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || project == "" {
		return errors.New("resolve-packages requires --project")
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	var result *packages.Snapshot
	var err error
	if current {
		result, err = packages.LoadCurrent(ctx, project, editor, packages.Options{})
	} else {
		result, err = packages.Resolve(ctx, project, editor, packages.Options{Offline: offline, RefreshGit: refreshGit})
	}
	if err != nil {
		return err
	}
	return json.NewEncoder(os.Stdout).Encode(result)
}
