package main

import (
	"context"
	"os"
	"testing"
	"time"
)

// Profiles only managed preparation. This does not call the native import
// worker, start a Player, or modify any author-owned project source.
func TestGoCompilerWarmProfileSDK(t *testing.T) {
	baseline, player, cache := os.Getenv("UNITY_GO_COMPILER_BASELINE"), os.Getenv("UNITY_GO_COMPILER_PLAYER"), os.Getenv("UNITY_GO_COMPILER_PROFILE_CACHE")
	if baseline == "" || player == "" || cache == "" {
		t.Skip("set compiler baseline/player/profile cache for SDK-only startup timings")
	}
	manifest, _, err := goCompilerManifest(baseline)
	if err != nil {
		t.Fatal(err)
	}
	var previousPlayer string
	for run := 0; run < 2; run++ {
		started := time.Now()
		compiler, err := newGoCompiler(manifest.Project, baseline, player, manifest.Unity, cache)
		if err != nil {
			t.Fatal(err)
		}
		initialized := time.Now()
		update, err := compiler.Compile(context.Background(), nil)
		if err != nil {
			t.Fatal(err)
		}
		compiled := time.Now()
		types, err := compiler.ScriptTypeNames(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if update == nil {
			t.Fatal("expected processed game graph for old raw baseline")
		}
		if run > 0 && (!update.CacheReused || update.Player != previousPlayer || len(update.CompiledAssemblies) != 0) {
			t.Fatalf("unchanged SDK startup failed whole graph cache: %+v", update)
		}
		previousPlayer = update.Player
		t.Logf("run=%d init=%.6fs compile=%.6fs scriptTypes=%.6fs total=%.6fs reused=%v compiled=%v bindings=%d player=%s", run,
			initialized.Sub(started).Seconds(), compiled.Sub(initialized).Seconds(), time.Since(compiled).Seconds(),
			time.Since(started).Seconds(), update.CacheReused, update.CompiledAssemblies, len(types), update.Player)
	}
}
