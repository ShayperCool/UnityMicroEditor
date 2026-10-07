package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"unitymicroeditor/internal/project/packages"
)

func main() {
	project := flag.String("project", "", "explicit Unity project")
	editor := flag.String("editor", "", "explicit Unity Editor executable for built-in package inventory")
	offline := flag.Bool("offline", false, "use verified existing sources without network")
	refreshGit := flag.Bool("refresh-git", false, "explicitly resolve Git refs again instead of retaining locked commits")
	current := flag.Bool("current", false, "validate and return the current durable snapshot without resolving/network")
	flag.Parse()
	if *project == "" {
		fmt.Fprintln(os.Stderr, "--project is required")
		os.Exit(2)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	var snapshot *packages.Snapshot
	var err error
	if *current {
		snapshot, err = packages.LoadCurrent(ctx, *project, *editor, packages.Options{})
	} else {
		snapshot, err = packages.Resolve(ctx, *project, *editor, packages.Options{Offline: *offline, RefreshGit: *refreshGit})
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetIndent("", "  ")
	if err = encoder.Encode(snapshot); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
