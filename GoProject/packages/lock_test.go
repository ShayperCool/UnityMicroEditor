//go:build linux || darwin || freebsd || openbsd || netbsd || dragonfly

package packages

import (
	"context"
	"errors"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

func TestConcurrentResolutionAndLockCancellation(t *testing.T) {
	root := projectFixture(t, nil)
	packageFixture(t, filepath.Join(root, "Packages", "embedded"), "com.example.local", "1.0.0", nil)
	var wg sync.WaitGroup
	errorsOut := make(chan error, 8)
	hashes := make(chan string, 8)
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			s, err := Resolve(context.Background(), root, "", Options{Offline: true})
			if err != nil {
				errorsOut <- err
			} else {
				hashes <- s.ResolutionHash
			}
		}()
	}
	wg.Wait()
	close(errorsOut)
	close(hashes)
	for err := range errorsOut {
		t.Fatal(err)
	}
	first := ""
	for hash := range hashes {
		if first != "" && hash != first {
			t.Fatal("concurrent package snapshots differed")
		}
		first = hash
	}
	if _, err := LoadCurrent(context.Background(), root, "", Options{}); err != nil {
		t.Fatal(err)
	}
	cache, err := ensureCache(root, "")
	if err != nil {
		t.Fatal(err)
	}
	release, err := acquireResolveLock(context.Background(), cache)
	if err != nil {
		t.Fatal(err)
	}
	defer release()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Millisecond)
	defer cancel()
	if _, err := Resolve(ctx, root, "", Options{Offline: true}); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("blocked resolver ignored context cancellation: %v", err)
	}
}
