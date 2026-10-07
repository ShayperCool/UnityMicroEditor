package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

const benchmarkSource = "Assets/Dungeon/Scripts/Audio/AmbientSound.cs"
const benchmarkMethod = "__UnityPlayBenchmarkRevision"

type benchmarkOptions struct {
	Project, Log, Output string
	Apply                bool
	BodyOnly             bool
	Timeout              time.Duration
}

type watchEvent struct {
	Event                string            `json:"event"`
	Manifest             string            `json:"manifest"`
	Report               string            `json:"report"`
	Error                string            `json:"error"`
	Paths                []string          `json:"paths"`
	CompiledAssemblies   []string          `json:"compiledAssemblies"`
	SourceHashes         map[string]string `json:"sourceHashes"`
	DeclarationChanged   []string          `json:"declarationChanged"`
	PID                  int               `json:"pid"`
	CompileSeconds       float64           `json:"compileSeconds"`
	SourceToReadySeconds float64           `json:"sourceToReadySeconds"`
}

type codeMeasurement struct {
	Action                    string     `json:"action"`
	ObservedSeconds           float64    `json:"observedSeconds"`
	SourceSHA256              string     `json:"sourceSha256"`
	PreviousDLLSHA256         string     `json:"previousDllSha256"`
	DLLSHA256                 string     `json:"dllSha256"`
	DLLPath                   string     `json:"dllPath"`
	DLLMarkerPresent          bool       `json:"dllMarkerPresent"`
	RuntimeMarkerInvoked      bool       `json:"runtimeMarkerInvoked"`
	LifecycleManifestVerified bool       `json:"lifecycleManifestVerified"`
	Ready                     any        `json:"ready"`
	Event                     watchEvent `json:"event"`
}

type benchmarkReport struct {
	SchemaVersion  int               `json:"schemaVersion"`
	Success        bool              `json:"success"`
	Stage          string            `json:"stage"`
	Mode           string            `json:"mode"`
	Error          string            `json:"error,omitempty"`
	Source         string            `json:"source"`
	Backup         string            `json:"backup"`
	OriginalSHA256 string            `json:"originalSha256"`
	ModifiedSHA256 string            `json:"modifiedSha256"`
	Restored       bool              `json:"restored"`
	Measurements   []codeMeasurement `json:"measurements"`
}

func main() {
	var opts benchmarkOptions
	flag.StringVar(&opts.Project, "project", ".", "source project with an already running Go watch session")
	flag.StringVar(&opts.Log, "log", "", "JSONL stdout from that existing watch session")
	flag.StringVar(&opts.Output, "output", "", "new directory under project Temp for backup/evidence")
	flag.BoolVar(&opts.Apply, "apply", false, "perform temporary IL edit and wait for edit/restore code-updated events")
	flag.BoolVar(&opts.BodyOnly, "body-only", false, "change an existing private method float constant without adding declarations")
	flag.DurationVar(&opts.Timeout, "timeout", 90*time.Second, "deadline per update, also used for independent restore wait")
	flag.Parse()
	if flag.NArg() != 0 || opts.Log == "" || opts.Output == "" || opts.Timeout <= 0 || opts.Timeout > 5*time.Minute {
		fmt.Fprintln(os.Stderr, "use --project <project> --log <running watch JSONL> --output <new Temp path> [--apply] --timeout 90s")
		os.Exit(2)
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	report, err := runBenchmark(ctx, opts)
	if report != nil {
		data, marshalErr := json.MarshalIndent(report, "", "  ")
		if marshalErr == nil {
			if writeErr := os.WriteFile(filepath.Join(opts.Output, "result.json"), append(data, '\n'), 0600); writeErr != nil {
				err = errors.Join(err, writeErr)
			}
			fmt.Println(string(data))
		} else {
			err = errors.Join(err, marshalErr)
		}
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func fingerprint(data []byte) string {
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

func makeBenchmarkEdit(original []byte, revision int) ([]byte, error) {
	anchor := []byte("        private AudioService _audio;")
	if bytes.Count(original, anchor) != 1 || bytes.Contains(original, []byte(benchmarkMethod)) {
		return nil, errors.New("AmbientSound source no longer has the reviewed benchmark anchor, or contains an old benchmark method")
	}
	// Public static method emits a real IL body and metadata without any
	// call site, game state, serialized field or per-frame behavior change.
	method := fmt.Sprintf("        public static int %s() => %d;\n\n", benchmarkMethod, revision)
	return bytes.Replace(original, anchor, append([]byte(method), anchor...), 1), nil
}

func makeBenchmarkBodyEdit(original []byte) ([]byte, []byte, error) {
	before := []byte("Random.Range(0.3f, 1f)")
	after := []byte("Random.Range(0.30000004f, 1f)")
	if bytes.Count(original, before) != 1 || bytes.Contains(original, after) || bytes.Contains(original, []byte(benchmarkMethod)) {
		return nil, nil, errors.New("AmbientSound no longer has the reviewed private body constant, or contains an old benchmark edit")
	}
	// Adjacent float values differ by about 30 ns in the random lower bound.
	// The coroutine's declaration remains identical; ldc.r4 changes in its
	// actual MoveNext IL body without adding metadata declarations.
	marker := make([]byte, 5)
	marker[0] = 0x22 // ECMA-335 ldc.r4
	binary.LittleEndian.PutUint32(marker[1:], math.Float32bits(0.30000004))
	return bytes.Replace(original, before, after, 1), marker, nil
}

func ownedBenchmarkPaths(opts benchmarkOptions) (benchmarkOptions, string, error) {
	project, err := filepath.Abs(opts.Project)
	if err != nil {
		return opts, "", err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return opts, "", err
	}
	output, err := filepath.Abs(opts.Output)
	if err != nil {
		return opts, "", err
	}
	relative, err := filepath.Rel(filepath.Join(project, "Temp"), output)
	if err != nil || relative == "." || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) || filepath.IsAbs(relative) {
		return opts, "", errors.New("benchmark output must be a new directory inside project Temp")
	}
	ancestor := output
	for {
		if _, err := os.Lstat(ancestor); err == nil {
			canonical, err := filepath.EvalSymlinks(ancestor)
			if err != nil || canonical != ancestor {
				return opts, "", errors.New("benchmark output has symlink ancestors")
			}
			break
		} else if !errors.Is(err, os.ErrNotExist) {
			return opts, "", err
		}
		ancestor = filepath.Dir(ancestor)
	}
	if _, err := os.Lstat(output); !errors.Is(err, os.ErrNotExist) {
		return opts, "", errors.New("benchmark output already exists")
	}
	source := filepath.Join(project, filepath.FromSlash(benchmarkSource))
	canonical, err := filepath.EvalSymlinks(source)
	if err != nil || canonical != source {
		return opts, "", errors.New("benchmark source must be the regular owned AmbientSound.cs without symlinks")
	}
	info, err := os.Stat(source)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 1024*1024 {
		return opts, "", errors.New("benchmark source exceeds reviewed bounds")
	}
	opts.Project, opts.Output = project, output
	return opts, source, nil
}

func runBenchmark(ctx context.Context, opts benchmarkOptions) (report *benchmarkReport, returnedErr error) {
	opts, source, err := ownedBenchmarkPaths(opts)
	if err != nil {
		return nil, err
	}
	original, err := os.ReadFile(source)
	if err != nil {
		return nil, err
	}
	marker := []byte(benchmarkMethod)
	mode := "method-declaration"
	var modified []byte
	if opts.BodyOnly {
		modified, marker, err = makeBenchmarkBodyEdit(original)
		mode = "method-body"
	} else {
		modified, err = makeBenchmarkEdit(original, int(time.Now().UnixNano()%1000000000))
	}
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(opts.Output), 0700); err != nil {
		return nil, err
	}
	if err := os.Mkdir(opts.Output, 0700); err != nil {
		return nil, err
	}
	backup := filepath.Join(opts.Output, "AmbientSound.cs.original")
	if err := os.WriteFile(backup, original, 0600); err != nil {
		return nil, err
	}
	report = &benchmarkReport{SchemaVersion: 1, Stage: "prepared", Mode: mode, Source: source, Backup: backup,
		OriginalSHA256: fingerprint(original), ModifiedSHA256: fingerprint(modified), Measurements: []codeMeasurement{}}
	defer func() {
		if returnedErr != nil {
			report.Success, report.Error = false, returnedErr.Error()
		}
	}()
	if !opts.Apply {
		report.Success, report.Restored = true, true
		return report, nil
	}
	initial, err := latestReadyEvent(opts.Log)
	if err != nil {
		return report, err
	}
	oldDLL, err := eventAssembly(initial, "Dungeon.Runtime.dll")
	if err != nil {
		return report, err
	}
	oldBytes, err := os.ReadFile(oldDLL)
	if err != nil {
		return report, err
	}
	oldHash := fingerprint(oldBytes)
	if bytes.Contains(oldBytes, marker) {
		return report, errors.New("baseline DLL already contains this benchmark marker; cannot prove a new edit")
	}
	// Root launches the watcher before this driver; give its post-readiness
	// watch registration a brief settle without launching another Player.
	select {
	case <-ctx.Done():
		return report, ctx.Err()
	case <-time.After(150 * time.Millisecond):
	}
	log, err := os.Open(opts.Log)
	if err != nil {
		return report, err
	}
	defer log.Close()
	if _, err := log.Seek(0, io.SeekEnd); err != nil {
		return report, err
	}
	reader := &eventTail{file: log}
	changed := false
	previousDLLHash := oldHash
	previousPID := initial.PID
	defer func() {
		if !changed {
			return
		}
		// Restoration owns an independent bounded context even if the edit
		// phase was cancelled, so normal SIGINT cannot leave test code behind.
		restoreCtx, cancel := context.WithTimeout(context.Background(), opts.Timeout)
		defer cancel()
		started := time.Now()
		restoreErr := restoreBenchmarkSource(source, original, modified)
		if restoreErr == nil {
			report.Restored = true
			event, err := reader.waitCodeMarker(restoreCtx, source, fingerprint(original), marker, false)
			if err == nil {
				measurement, verifyErr := verifyCodeEvent(event, source, fingerprint(original), previousDLLHash, marker, false, time.Since(started).Seconds())
				if verifyErr == nil && event.PID == previousPID {
					verifyErr = errors.New("restored C# revision did not start a new Player process")
				}
				report.Measurements = append(report.Measurements, measurement)
				restoreErr = verifyErr
			} else {
				restoreErr = err
			}
		}
		returnedErr = errors.Join(returnedErr, restoreErr)
	}()
	current, err := os.ReadFile(source)
	if err != nil || !bytes.Equal(current, original) {
		return report, errors.New("source changed after benchmark backup; refusing to overwrite")
	}
	started := time.Now()
	changed = true // Restore also handles a partially failed WriteFile.
	if err := os.WriteFile(source, modified, 0644); err != nil {
		return report, err
	}
	editCtx, cancel := context.WithTimeout(ctx, opts.Timeout)
	defer cancel()
	event, err := reader.waitCodeMarker(editCtx, source, fingerprint(modified), marker, true)
	if err != nil {
		return report, err
	}
	measurement, err := verifyCodeEvent(event, source, fingerprint(modified), oldHash, marker, true, time.Since(started).Seconds())
	report.Measurements = append(report.Measurements, measurement)
	if err != nil {
		return report, err
	}
	if event.PID == initial.PID {
		return report, errors.New("changed C# revision did not start a new Player process")
	}
	previousDLLHash, previousPID = measurement.DLLSHA256, event.PID
	report.Success, report.Stage = true, "code-edit-and-restore"
	return report, nil
}

func restoreBenchmarkSource(source string, original, modified []byte) error {
	current, err := os.ReadFile(source)
	if err != nil {
		return err
	}
	if bytes.Equal(current, original) {
		return nil
	}
	if !bytes.Equal(current, modified) {
		return errors.New("benchmark source now contains another edit; original backup is preserved, refusing to overwrite that edit")
	}
	return os.WriteFile(source, original, 0644)
}

func latestReadyEvent(filename string) (watchEvent, error) {
	data, err := os.ReadFile(filename)
	if err != nil || len(data) > 16*1024*1024 {
		return watchEvent{}, errors.New("watch event log is missing or outside benchmark bounds")
	}
	var latest watchEvent
	for _, line := range bytes.Split(data, []byte{'\n'}) {
		var event watchEvent
		if json.Unmarshal(line, &event) == nil && (event.Event == "ready" || event.Event == "code-updated") {
			latest = event
		}
	}
	if latest.Manifest == "" || latest.Report == "" || latest.PID == 0 {
		return latest, errors.New("watch session has no ready event with an exact manifest/report/PID")
	}
	return latest, nil
}

func eventAssembly(event watchEvent, name string) (string, error) {
	data, err := os.ReadFile(event.Manifest)
	if err != nil || len(data) > 64*1024*1024 {
		return "", errors.New("code event manifest is unavailable or outside bounds")
	}
	var manifest struct {
		Assemblies                 []string `json:"assemblies"`
		LifecycleBarrierVersion    int      `json:"lifecycleBarrierVersion"`
		LifecycleBarrierAssemblies []string `json:"lifecycleBarrierAssemblies"`
	}
	if err := json.Unmarshal(data, &manifest); err != nil || manifest.LifecycleBarrierVersion != 1 {
		return "", errors.New("code event manifest lacks a verified lifecycle barrier")
	}
	for _, assembly := range manifest.Assemblies {
		if filepath.Base(assembly) == name {
			for _, barrierAssembly := range manifest.LifecycleBarrierAssemblies {
				if barrierAssembly == assembly {
					return assembly, nil
				}
			}
			return "", errors.New("game DLL is not covered by the lifecycle manifest")
		}
	}
	return "", errors.New("game DLL is missing from the exact code event manifest")
}

func verifyCodeEvent(event watchEvent, source, sourceHash, previousHash string, markerBytes []byte, marker bool, elapsed float64) (codeMeasurement, error) {
	result := codeMeasurement{Action: "restore", ObservedSeconds: elapsed, SourceSHA256: sourceHash, PreviousDLLSHA256: previousHash, Event: event}
	if marker {
		result.Action = "edit"
	}
	found := false
	for _, assembly := range event.CompiledAssemblies {
		if assembly == "Dungeon.Runtime" {
			found = true
		}
	}
	if event.Event != "code-updated" || !found || event.PID == 0 {
		return result, errors.New("watcher did not report the expected Dungeon.Runtime compilation")
	}
	dll, err := eventAssembly(event, "Dungeon.Runtime.dll")
	if err != nil {
		return result, err
	}
	data, err := os.ReadFile(dll)
	if err != nil || len(data) > 64*1024*1024 {
		return result, errors.New("updated game DLL is unavailable or outside bounds")
	}
	result.DLLPath, result.DLLSHA256 = dll, fingerprint(data)
	result.DLLMarkerPresent = bytes.Contains(data, markerBytes)
	if result.DLLMarkerPresent != marker || (marker && result.DLLSHA256 == previousHash) {
		return result, errors.New("new compiled DLL does not prove the expected real IL/metadata change")
	}
	ready, err := os.ReadFile(event.Report + ".ready.json")
	if err != nil || len(ready) > 1024*1024 {
		return result, errors.New("updated Player readiness proof is missing")
	}
	var proof struct {
		SchemaVersion int    `json:"schemaVersion"`
		PID           int    `json:"pid"`
		Errors        int    `json:"errors"`
		Exceptions    int    `json:"exceptions"`
		FatalError    string `json:"fatalError"`
		Graphics      string `json:"graphicsDeviceName"`
		Scene         struct {
			IsLoaded bool `json:"isLoaded"`
		} `json:"scene"`
	}
	if err := json.Unmarshal(ready, &proof); err != nil || proof.SchemaVersion != 1 || proof.PID != event.PID || proof.Errors != 0 ||
		proof.Exceptions != 0 || proof.FatalError != "" || !proof.Scene.IsLoaded || proof.Graphics == "" {
		return result, errors.New("updated Player did not prove a loaded error-free scene on the expected process")
	}
	var readyJSON any
	json.Unmarshal(ready, &readyJSON)
	result.Ready, result.LifecycleManifestVerified = readyJSON, true
	current, err := os.ReadFile(source)
	if err != nil || fingerprint(current) != sourceHash {
		return result, errors.New("benchmark source changed before readiness acknowledgement")
	}
	return result, nil
}

type eventTail struct {
	file    *os.File
	pending []byte
}

func (tail *eventTail) next() ([]watchEvent, error) {
	buffer := make([]byte, 64*1024)
	var events []watchEvent
	for {
		n, err := tail.file.Read(buffer)
		if n > 0 {
			tail.pending = append(tail.pending, buffer[:n]...)
			if len(tail.pending) > 2*1024*1024 {
				return nil, errors.New("watch event line exceeds benchmark bounds")
			}
			for {
				end := bytes.IndexByte(tail.pending, '\n')
				if end < 0 {
					break
				}
				var event watchEvent
				if err := json.Unmarshal(tail.pending[:end], &event); err != nil {
					return nil, fmt.Errorf("invalid watch event: %w", err)
				}
				events = append(events, event)
				tail.pending = tail.pending[end+1:]
			}
		}
		if err == io.EOF {
			return events, nil
		}
		if err != nil {
			return nil, err
		}
	}
}

func (tail *eventTail) waitCodeUpdate(ctx context.Context, source string) (watchEvent, error) {
	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()
	for {
		events, err := tail.next()
		if err != nil {
			return watchEvent{}, err
		}
		for _, event := range events {
			relevant := false
			for _, filename := range event.Paths {
				if filename == source {
					relevant = true
				}
			}
			if event.Event == "update-rejected" && relevant {
				return event, errors.New("watcher rejected benchmark update: " + event.Error)
			}
			if event.Event == "code-updated" && relevant {
				return event, nil
			}
		}
		select {
		case <-ctx.Done():
			return watchEvent{}, ctx.Err()
		case <-ticker.C:
		}
	}
}

func eventSourceHash(event watchEvent, source string) string {
	if hash := event.SourceHashes[source]; hash != "" {
		return hash
	}
	if hash := event.SourceHashes[benchmarkSource]; hash != "" {
		return hash
	}
	data, err := os.ReadFile(event.Manifest)
	if err != nil || len(data) > 64*1024*1024 {
		return ""
	}
	var manifest struct {
		ProjectRoot  string `json:"projectRoot"`
		Dependencies []struct {
			Path   string `json:"path"`
			SHA256 string `json:"sha256"`
		} `json:"sourceDependencies"`
	}
	if json.Unmarshal(data, &manifest) != nil {
		return ""
	}
	for _, dependency := range manifest.Dependencies {
		if dependency.Path == source || (dependency.Path == benchmarkSource && filepath.Join(manifest.ProjectRoot, filepath.FromSlash(dependency.Path)) == source) {
			return dependency.SHA256
		}
	}
	return ""
}

func (tail *eventTail) waitCodeMarker(ctx context.Context, source, expectedSourceHash string, markerBytes []byte, marker bool) (watchEvent, error) {
	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()
	for {
		events, err := tail.next()
		if err != nil {
			return watchEvent{}, err
		}
		for _, event := range events {
			if event.Event == "update-rejected" {
				for _, path := range event.Paths {
					if path == source {
						return event, errors.New("watcher rejected benchmark update: " + event.Error)
					}
				}
			}
			if event.Event != "code-updated" || eventSourceHash(event, source) != expectedSourceHash {
				continue
			}
			dll, err := eventAssembly(event, "Dungeon.Runtime.dll")
			if err != nil {
				return event, err
			}
			data, err := os.ReadFile(dll)
			if err != nil {
				return event, err
			}
			if bytes.Contains(data, markerBytes) == marker {
				return event, nil
			}
		}
		select {
		case <-ctx.Done():
			return watchEvent{}, ctx.Err()
		case <-ticker.C:
		}
	}
}
