package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"
)

// DataStoreService owns one native storage service in an isolated scratch
// directory. Stop attached import workers before stopping the service.
type DataStoreService struct {
	Editor    string
	Context   string
	Logs      string
	Namespace string
	PID       int

	command   *exec.Cmd
	stdout    *os.File
	done      chan struct{}
	started   bool
	forced    bool
	readyTime time.Duration
	mu        sync.Mutex
	exitErr   error
	finished  bool
}

type DataStoreLogEvidence struct {
	Initialized        bool `json:"initialized"`
	Service            bool `json:"service"`
	ApplicationStarted bool `json:"applicationStarted"`
	ShutdownLogged     bool `json:"shutdownLogged"`
	StartupErrorLogged bool `json:"startupErrorLogged"`
	ReadyLogged        bool `json:"readyLogged"`
}

type DataStoreStatus struct {
	PID          int                  `json:"pid"`
	Running      bool                 `json:"running"`
	Ready        bool                 `json:"ready"`
	ExitError    string               `json:"exitError,omitempty"`
	ReadySeconds float64              `json:"readySeconds"`
	ForcedStop   bool                 `json:"forcedStop"`
	ServiceLog   string               `json:"serviceLog"`
	StdoutLog    string               `json:"stdoutLog"`
	LogEvidence  DataStoreLogEvidence `json:"logEvidence"`
}

func readDataStoreLog(path string) (DataStoreLogEvidence, error) {
	file, err := os.Open(path)
	if errors.Is(err, os.ErrNotExist) {
		return DataStoreLogEvidence{}, nil
	}
	if err != nil {
		return DataStoreLogEvidence{}, err
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, 128*1024))
	if err != nil {
		return DataStoreLogEvidence{}, err
	}
	text := string(data)
	evidence := DataStoreLogEvidence{
		Initialized:        strings.Contains(text, "Initialising UDS system complete:"),
		Service:            strings.Contains(text, "  - System type: Service"),
		ApplicationStarted: strings.Contains(text, "Starting Unity Data Store Application"),
		ShutdownLogged: strings.Contains(text, "Unity Data Store Application Complete") ||
			strings.Contains(text, "Shut down UDS service system complete"),
		StartupErrorLogged: strings.Contains(text, "[Error]"),
	}
	evidence.ReadyLogged = evidence.Initialized && evidence.Service &&
		evidence.ApplicationStarted && !evidence.ShutdownLogged && !evidence.StartupErrorLogged
	return evidence, nil
}

func (s *DataStoreService) serviceLog() string {
	return filepath.Join(s.Logs, "datastore-service.log")
}

func (s *DataStoreService) stdoutLog() string {
	return filepath.Join(s.Logs, "datastore-service-stdout.log")
}

func (s *DataStoreService) WorkerArguments() []string {
	return []string{"-shared-postfix", s.Namespace}
}

func (s *DataStoreService) arguments() []string {
	return []string{"--memory-size", "1073741824", "--shared-postfix", s.Namespace,
		"--working-directory", "Library/DataStore", "--silent-console",
		"--shutdown-grace", "0", "--client-process-check", "0",
		"--uds-log-file", s.serviceLog(), "--debug-log", "2"}
}

func (s *DataStoreService) Status() (DataStoreStatus, error) {
	evidence, err := readDataStoreLog(s.serviceLog())
	if err != nil {
		return DataStoreStatus{}, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	status := DataStoreStatus{PID: s.PID, Running: s.command != nil && !s.finished,
		ReadySeconds: s.readyTime.Seconds(), ForcedStop: s.forced,
		ServiceLog: s.serviceLog(), StdoutLog: s.stdoutLog(), LogEvidence: evidence}
	status.Ready = status.Running && evidence.ReadyLogged
	if s.exitErr != nil {
		status.ExitError = s.exitErr.Error()
	}
	return status, nil
}

func (s *DataStoreService) Start(ctx context.Context) error {
	if s.started {
		return errors.New("DataStoreService instances can only be started once")
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	s.started = true
	var err error
	s.Context, err = filepath.Abs(s.Context)
	if err != nil {
		return err
	}
	s.Logs, err = filepath.Abs(s.Logs)
	if err != nil {
		return err
	}
	editor, err := filepath.Abs(s.Editor)
	if err != nil {
		return err
	}
	info, err := os.Stat(editor)
	if err != nil {
		return fmt.Errorf("Unity installation: %w", err)
	}
	installation := editor
	if !info.IsDir() {
		installation = filepath.Dir(editor)
	}
	executable := filepath.Join(installation, "Data", "Tools", "Unity.DataStore")
	info, err = os.Stat(executable)
	if err != nil {
		return fmt.Errorf("Unity.DataStore executable: %w", err)
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0111 == 0 {
		return errors.New("Unity.DataStore must be a regular executable")
	}
	var namespace [16]byte
	if _, err := rand.Read(namespace[:]); err != nil {
		return fmt.Errorf("DataStore namespace: %w", err)
	}
	s.Namespace = hex.EncodeToString(namespace[:])
	if err := os.MkdirAll(filepath.Join(s.Context, "Library", "DataStore"), 0755); err != nil {
		return err
	}
	if err := os.MkdirAll(s.Logs, 0755); err != nil {
		return err
	}
	// Stale startup markers cannot satisfy readiness for this process.
	if err := os.WriteFile(s.serviceLog(), nil, 0600); err != nil {
		return err
	}
	s.stdout, err = os.OpenFile(s.stdoutLog(), os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	s.command = exec.Command(executable, s.arguments()...)
	s.command.Dir = s.Context
	s.command.Stdout = s.stdout
	s.command.Stderr = s.stdout
	s.command.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	started := time.Now()
	if err := s.command.Start(); err != nil {
		s.command = nil
		s.stdout.Close()
		s.stdout = nil
		return fmt.Errorf("launch Unity.DataStore: %w", err)
	}
	s.PID = s.command.Process.Pid
	s.done = make(chan struct{})
	go func() {
		err := s.command.Wait()
		s.mu.Lock()
		s.exitErr, s.finished = err, true
		s.mu.Unlock()
		close(s.done)
	}()
	deadline := time.NewTimer(10 * time.Second)
	defer deadline.Stop()
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	for {
		status, err := s.Status()
		if err != nil {
			return errors.Join(fmt.Errorf("DataStore readiness: %w", err), s.Stop())
		}
		if status.Ready {
			s.mu.Lock()
			s.readyTime = time.Since(started)
			s.mu.Unlock()
			return nil
		}
		if status.LogEvidence.StartupErrorLogged {
			return errors.Join(fmt.Errorf("Unity.DataStore reported startup errors; see %s", s.serviceLog()), s.Stop())
		}
		select {
		case <-s.done:
			status, _ := s.Status()
			return errors.Join(fmt.Errorf("Unity.DataStore exited before ready: %s; see %s", status.ExitError, s.serviceLog()), s.Stop())
		case <-ctx.Done():
			return errors.Join(ctx.Err(), s.Stop())
		case <-deadline.C:
			return errors.Join(fmt.Errorf("Unity.DataStore readiness deadline expired; see %s", s.serviceLog()), s.Stop())
		case <-ticker.C:
		}
	}
}

func (s *DataStoreService) signalGroup(sig syscall.Signal) error {
	select {
	case <-s.done:
		return nil
	default:
	}
	err := syscall.Kill(-s.PID, sig)
	if errors.Is(err, syscall.ESRCH) {
		return nil
	}
	return err
}

func (s *DataStoreService) Stop() error {
	if s.command == nil || s.done == nil {
		return nil
	}
	defer func() {
		if s.stdout != nil {
			s.stdout.Close()
			s.stdout = nil
		}
	}()
	if err := s.signalGroup(syscall.SIGTERM); err != nil {
		return fmt.Errorf("stop DataStore process group: %w", err)
	}
	grace := time.NewTimer(3 * time.Second)
	defer grace.Stop()
	select {
	case <-s.done:
		return nil
	case <-grace.C:
	}
	s.mu.Lock()
	s.forced = true
	s.mu.Unlock()
	if err := s.signalGroup(syscall.SIGKILL); err != nil {
		return fmt.Errorf("kill DataStore process group: %w", err)
	}
	deadline := time.NewTimer(3 * time.Second)
	defer deadline.Stop()
	select {
	case <-s.done:
		return nil
	case <-deadline.C:
		return errors.New("Unity.DataStore did not exit after SIGKILL")
	}
}
