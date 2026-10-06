package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// These inputs must reach the compiler/preparer even when a text editor replaces
// the file by an atomic rename rather than writing to the existing inode.
func TestWatchCompilerAndAssetInputs(t *testing.T) {
	project := t.TempDir()
	assets := filepath.Join(project, "Assets")
	if err := os.Mkdir(assets, 0700); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	watcher, err := newProjectWatcher(ctx, project)
	if err != nil {
		t.Fatal(err)
	}
	defer watcher.Close()
	for _, name := range []string{"Plugin.dll", "csc.rsp", "texture.jpeg", "hud.uxml", "hud.uss", "music.mp3"} {
		destination := filepath.Join(assets, name)
		temporary := destination + ".tmp"
		if err := os.WriteFile(temporary, []byte("changed"), 0600); err != nil {
			t.Fatal(err)
		}
		if err := os.Rename(temporary, destination); err != nil {
			t.Fatal(err)
		}
		timeout := time.NewTimer(3 * time.Second)
		found := false
		for !found {
			select {
			case event := <-watcher.events:
				if event.err != nil {
					t.Fatal(event.err)
				}
				found = event.path == destination
			case <-timeout.C:
				t.Fatalf("watcher missed atomic save of %s", name)
			}
		}
		timeout.Stop()
	}
}
