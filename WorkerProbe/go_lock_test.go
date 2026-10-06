package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestGoSessionLockLifecycle(t *testing.T) {
	output := t.TempDir()
	first, err := lockGoSession(output)
	if err != nil {
		t.Fatal(err)
	}
	defer first.Close()
	if second, err := lockGoSession(output); err == nil {
		second.Close()
		t.Fatal("two launchers acquired the same session output")
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	afterStop, err := lockGoSession(output)
	if err != nil {
		t.Fatalf("closed session kept its lock: %v", err)
	}
	afterStop.Close()
}

func TestGoSessionLockRejectsSymlink(t *testing.T) {
	output := t.TempDir()
	target := filepath.Join(t.TempDir(), "foreign")
	if err := os.WriteFile(target, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(target, filepath.Join(output, ".session.lock")); err != nil {
		t.Fatal(err)
	}
	if lock, err := lockGoSession(output); err == nil {
		lock.Close()
		t.Fatal("session followed an existing lock symlink")
	}
	data, err := os.ReadFile(target)
	if err != nil || string(data) != "keep" {
		t.Fatalf("foreign file changed: %q %v", data, err)
	}
}
