package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func TestDataStoreReadinessRejectsClientShutdownAndErrorLogs(t *testing.T) {
	initialized := "Initialising UDS system complete:\n  - System type: Service\n"
	started := "Starting Unity Data Store Application\n"
	for _, test := range []struct {
		name  string
		log   string
		ready bool
	}{
		{"service", initialized + started, true},
		{"missing application", initialized, false},
		{"client", "Initialising UDS system complete:\n  - System type: Client\n" + started, false},
		{"shutdown", initialized + started + "Shut down UDS service system complete\n", false},
		{"startup error", initialized + started + "[Error] shared memory allocation failed\n", false},
	} {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "service.log")
			if err := os.WriteFile(path, []byte(test.log), 0600); err != nil {
				t.Fatal(err)
			}
			evidence, err := readDataStoreLog(path)
			if err != nil {
				t.Fatal(err)
			}
			if evidence.ReadyLogged != test.ready {
				t.Fatalf("ready %t, want %t: %+v", evidence.ReadyLogged, test.ready, evidence)
			}
		})
	}
}

func TestDataStoreMissingLogIsPending(t *testing.T) {
	evidence, err := readDataStoreLog(filepath.Join(t.TempDir(), "missing.log"))
	if err != nil || evidence.ReadyLogged {
		t.Fatalf("missing startup log must be pending: %+v %v", evidence, err)
	}
}

func TestDataStoreCanceledStartCreatesNothing(t *testing.T) {
	root := t.TempDir()
	service := DataStoreService{Editor: filepath.Join(root, "Unity"),
		Context: filepath.Join(root, "context"), Logs: filepath.Join(root, "logs")}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := service.Start(ctx); !errors.Is(err, context.Canceled) {
		t.Fatalf("start canceled: %v", err)
	}
	entries, err := os.ReadDir(root)
	if err != nil || len(entries) != 0 {
		t.Fatalf("canceled start wrote files: %v %v", entries, err)
	}
	if err := service.Stop(); err != nil {
		t.Fatal(err)
	}
}

func TestDataStoreMissingExecutableDoesNotCreateContext(t *testing.T) {
	root := t.TempDir()
	installation := filepath.Join(root, "Editor")
	if err := os.Mkdir(installation, 0755); err != nil {
		t.Fatal(err)
	}
	service := DataStoreService{Editor: installation, Context: filepath.Join(root, "context"),
		Logs: filepath.Join(root, "logs")}
	if err := service.Start(context.Background()); err == nil {
		t.Fatal("missing Unity.DataStore executable accepted")
	}
	if _, err := os.Stat(service.Context); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing executable created context: %v", err)
	}
}

func TestDataStoreReadyLogRequiresRunningProcess(t *testing.T) {
	root := t.TempDir()
	service := DataStoreService{Logs: root}
	if err := os.WriteFile(service.serviceLog(), []byte("Initialising UDS system complete:\n  - System type: Service\nStarting Unity Data Store Application\n"), 0600); err != nil {
		t.Fatal(err)
	}
	status, err := service.Status()
	if err != nil || status.Ready || status.Running || !status.LogEvidence.ReadyLogged {
		t.Fatalf("stale ready log accepted: %+v %v", status, err)
	}
}
