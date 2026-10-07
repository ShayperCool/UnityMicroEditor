package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

type probeReport struct {
	SchemaVersion     int                `json:"schemaVersion"`
	Experiment        string             `json:"experiment"`
	Success           bool               `json:"success"`
	Stage             string             `json:"stage"`
	Error             string             `json:"error,omitempty"`
	ProjectContext    any                `json:"projectContext,omitempty"`
	MainEditorStarted bool               `json:"mainEditorStarted"`
	SourceDBSeeded    bool               `json:"sourceDBSeeded"`
	SourceRegistry    any                `json:"sourceRegistry,omitempty"`
	Environment       *workerEnvironment `json:"environment,omitempty"`
	Content           any                `json:"content,omitempty"`
	WorkerPID         int                `json:"workerPid"`
	CoordinatorPID    int                `json:"coordinatorPid"`
	WorkerName        string             `json:"workerName,omitempty"`
	DataStore         any                `json:"dataStore,omitempty"`
	Stages            map[string]float64 `json:"stageSeconds"`
	Frames            []frameEvidence    `json:"frames"`
	Imports           []importEvidence   `json:"imports"`
	CoordinatorRSSKiB int64              `json:"coordinatorRssKiB"`
	WorkerRSSKiB      int64              `json:"workerRssKiB"`
	DataStoreRSSKiB   int64              `json:"dataStoreRssKiB"`
	WorkerPeakRSSKiB  int64              `json:"workerPeakRssKiB"`
	NativeSeconds     float64            `json:"nativeSeconds"`
	ElapsedSeconds    float64            `json:"elapsedSeconds"`
	WorkerExit        string             `json:"workerExit"`
	maxFrames         int
	maxFrameBytes     int
}

type frameEvidence struct {
	Type   byte   `json:"type"`
	Size   int    `json:"size"`
	SHA256 string `json:"sha256"`
	Path   string `json:"path"`
}

type importEvidence struct {
	GUID           string           `json:"guid"`
	ImporterType   int32            `json:"importerType"`
	ElapsedSeconds float64          `json:"elapsedSeconds"`
	ReplyType      byte             `json:"replyType"`
	ResultID       string           `json:"resultId,omitempty"`
	ErrorCode      byte             `json:"errorCode"`
	ErrorMessage   string           `json:"errorMessage,omitempty"`
	ReturnedPaths  []string         `json:"returnedPaths"`
	Metadata       ArtifactMetaInfo `json:"metadata"`
}

type options struct {
	Editor, Project, Output string
	Timeout                 time.Duration
	Context                 string
	Registry                *SourceRegistryReport
	Jobs                    []nativeImportJob
	ProjectContext          any
}

type nativeImportJob struct {
	guid     string
	importer int32
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "resolve-packages" {
		if err := runAgentPackageResolveCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && (os.Args[1] == "source-graph" || os.Args[1] == "compiler-source-prepare" || os.Args[1] == "engine-profile-capture") {
		if err := runGoSourceGraphCLI(os.Args[1], os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "cache-component-defaults" {
		if err := runAgentComponentDefaultsCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && (os.Args[1] == "auto-prepare" || os.Args[1] == "build-project") {
		var err error
		if os.Args[1] == "auto-prepare" {
			err = runAgentAutoPrepareCLI(os.Args[2:])
		} else {
			err = runAgentBuildProjectCLI(os.Args[2:])
		}
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "mcp" {
		if err := runAgentMCPCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "prepare-project" {
		if err := runAgentProjectPrepareCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "compiler-prepare" {
		if err := runCompilerPrepareCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) == 2 && (os.Args[1] == "version" || os.Args[1] == "profile") {
		fmt.Print(string(engineProfile))
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "native-adopt" {
		if err := runNativeAdoptCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "native-environment" {
		if err := runNativeEnvironmentCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "run" {
		if err := runGoCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "project-import" {
		if err := runProjectImportCLI(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	fmt.Fprintln(os.Stderr, "usage: unity-micro-editor <run|mcp|auto-prepare|build-project|resolve-packages|cache-component-defaults|project-import|native-adopt|native-environment|source-graph|compiler-source-prepare|compiler-prepare|prepare-project|engine-profile-capture|version> [flags]")
	os.Exit(2)
}

func runProbe(ctx context.Context, opts options) (report *probeReport, returnedErr error) {
	started := time.Now()
	project := opts.Context
	if project == "" {
		return nil, errors.New("native import requires a prepared project context")
	}
	output, err := filepath.Abs(opts.Output)
	if err != nil {
		return nil, err
	}
	logs := filepath.Join(output, "logs")
	if err := os.MkdirAll(logs, 0700); err != nil {
		return nil, err
	}
	report = &probeReport{SchemaVersion: 1, Experiment: "native-import-worker-own-coordinator", Stage: "project-sources",
		CoordinatorPID: os.Getpid(), Stages: map[string]float64{}, Frames: []frameEvidence{}, Imports: []importEvidence{}}
	report.ProjectContext = opts.ProjectContext
	report.SourceRegistry = opts.Registry
	report.maxFrames = min(65536, 256+8*len(opts.Jobs))
	report.maxFrameBytes = min(128*1024*1024, 16*1024*1024+64*1024*len(opts.Jobs))
	defer func() {
		report.NativeSeconds = time.Since(started).Seconds()
		report.CoordinatorRSSKiB = rssKiB(os.Getpid())
		returnedErr = errors.Join(returnedErr, ctx.Err())
		if returnedErr != nil {
			report.Error = returnedErr.Error()
			report.Success = false
		}
	}()
	// A project with PlayerSettings supplies exact graphics values.
	report.Environment, err = readWorkerEnvironment(project)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return report, err
	}
	initialize, prepare := initializeMessage(24), prepareMessage(24)
	if report.Environment != nil {
		initialize, prepare = report.Environment.message(initializeType), report.Environment.message(prepareType)
	}
	report.Stages["fixtureAndRegistry"] = time.Since(started).Seconds()
	listener, err := net.ListenTCP("tcp", &net.TCPAddr{IP: net.ParseIP("127.0.0.1")})
	if err != nil {
		return report, err
	}
	defer listener.Close()
	deadline, _ := ctx.Deadline()
	if err := listener.SetDeadline(deadline); err != nil {
		return report, err
	}
	service := &DataStoreService{Editor: opts.Editor, Context: project, Logs: logs}
	stageStarted := time.Now()
	if err := service.Start(ctx); err != nil {
		return report, err
	}
	report.Stages["dataStoreReady"] = time.Since(stageStarted).Seconds()
	defer func() {
		report.DataStoreRSSKiB = rssKiB(service.PID)
		returnedErr = errors.Join(returnedErr, service.Stop())
		status, err := service.Status()
		if err == nil {
			report.DataStore = status
		}
	}()
	stdout, err := os.OpenFile(filepath.Join(logs, "worker-stdout.log"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return report, err
	}
	defer stdout.Close()
	args := []string{"-adb2", "-batchMode", "-noUpm", "-name", "AssetImportWorkerProbe", "-projectPath", project,
		"-logFile", filepath.Join(logs, "worker.log"), "-srvPort", strconv.Itoa(listener.Addr().(*net.TCPAddr).Port),
		"-parentPid", strconv.Itoa(os.Getpid()), "-job-worker-count", "2", "-background-job-worker-count", "2", "-nographics", "-noDiagnosticSwitchPersistentStore"}
	args = append(args, service.WorkerArguments()...)
	worker := exec.Command(opts.Editor, args...)
	worker.Dir, worker.Stdout, worker.Stderr = project, stdout, stdout
	worker.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	stageStarted = time.Now()
	if err := worker.Start(); err != nil {
		return report, err
	}
	report.WorkerPID = worker.Process.Pid
	done := make(chan error, 1)
	go func() { done <- worker.Wait() }()
	var connection *net.TCPConn
	defer func() {
		report.WorkerRSSKiB = rssKiB(worker.Process.Pid)
		if connection != nil {
			connection.SetWriteDeadline(time.Now().Add(time.Second))
			if payload, err := shutdownMessage("own coordinator experiment complete"); err == nil {
				_ = writeFrame(connection, Frame{Kind: 1, Payload: payload})
			}
		}
		select {
		case exit := <-done:
			if exit != nil {
				report.WorkerExit = exit.Error()
			} else {
				report.WorkerExit = "exit 0"
			}
			if exit != nil {
				returnedErr = errors.Join(returnedErr, fmt.Errorf("worker shutdown: %w", exit))
			}
		case <-time.After(3 * time.Second):
			returnedErr = errors.Join(returnedErr, errors.New("worker did not stop after protocol shutdown"))
			_ = syscall.Kill(-worker.Process.Pid, syscall.SIGTERM)
			select {
			case exit := <-done:
				report.WorkerExit = fmt.Sprint(exit)
			case <-time.After(2 * time.Second):
				_ = syscall.Kill(-worker.Process.Pid, syscall.SIGKILL)
				select {
				case exit := <-done:
					report.WorkerExit = fmt.Sprint(exit)
				case <-time.After(2 * time.Second):
					returnedErr = errors.Join(returnedErr, errors.New("worker exit wait exceeded deadline"))
				}
			}
		}
		if connection != nil {
			_ = connection.Close()
		}
		if worker.ProcessState != nil {
			if usage, ok := worker.ProcessState.SysUsage().(*syscall.Rusage); ok {
				report.WorkerPeakRSSKiB = usage.Maxrss
			}
		}
	}()
	// Context cancellation also releases Accept and reads before their deadline.
	stopAccept := context.AfterFunc(ctx, func() { listener.Close() })
	defer stopAccept()
	connection, err = listener.AcceptTCP()
	if err != nil {
		return report, fmt.Errorf("worker connection: %w", err)
	}
	if err := connection.SetDeadline(deadline); err != nil {
		return report, err
	}
	stopRead := context.AfterFunc(ctx, func() { connection.Close() })
	defer stopRead()
	frame, err := readFrame(connection)
	if err != nil {
		return report, err
	}
	if err := validateHandshake(frame); err != nil {
		return report, err
	}
	if err := writeFrame(connection, handshakeFrame()); err != nil {
		return report, err
	}
	report.Stage = "handshake"
	report.Stages["workerHandshake"] = time.Since(stageStarted).Seconds()
	frame, err = captureFrame(connection, output, report)
	if err != nil {
		return report, err
	}
	report.WorkerName, err = decodeWorkerHello(frame.Payload)
	if err != nil {
		return report, err
	}
	if err := validateWorkerVersion(filepath.Join(logs, "worker.log")); err != nil {
		return report, err
	}
	stageStarted = time.Now()
	if err := writeFrame(connection, Frame{Kind: 1, Payload: initialize}); err != nil {
		return report, err
	}
	frame, err = receiveMessage(connection, output, report, workerPreparedType)
	if err != nil {
		return report, err
	}
	if frame.Kind != 1 || len(frame.Payload) != 1 || frame.Payload[0] != workerPreparedType {
		return report, fmt.Errorf("unexpected initialization reply type %d", frame.Payload[0])
	}
	report.Stage = "initialized"
	report.Stages["workerInitialize"] = time.Since(stageStarted).Seconds()
	report.Stages["workerReadyTotal"] = time.Since(started).Seconds()
	jobs := opts.Jobs
	if len(jobs) == 0 {
		return report, errors.New("native import context has no source asset jobs")
	}
	for _, job := range jobs {
		stageStarted = time.Now()
		if err := writeFrame(connection, Frame{Kind: 1, Payload: prepare}); err != nil {
			return report, err
		}
		frame, err = receiveMessage(connection, output, report, workerPreparedType)
		if err != nil {
			return report, err
		}
		if len(frame.Payload) != 1 || frame.Payload[0] != workerPreparedType {
			return report, errors.New("worker did not prepare next import")
		}
		address, err := makeImportAddress(job.guid, job.importer)
		if err != nil {
			return report, err
		}
		if err := writeFrame(connection, Frame{Kind: 1, Payload: importRequestMessage(address, 0)}); err != nil {
			return report, err
		}
		report.Stage = "import-requested"
		frame, err = receiveMessage(connection, output, report, importResponseType)
		if err != nil {
			return report, err
		}
		response, err := decodeImportResponse(frame.Payload)
		if err != nil {
			return report, err
		}
		if response.Address != address {
			return report, errors.New("import response address differs from request")
		}
		if response.ConsistencyFailureCount != 0 {
			return report, errors.New("native importer reported consistency failures")
		}
		report.Imports = append(report.Imports, importEvidence{GUID: job.guid, ImporterType: job.importer, ElapsedSeconds: time.Since(stageStarted).Seconds(), ReplyType: frame.Payload[0], ResultID: response.ResultID, ErrorCode: response.ErrorCode, ErrorMessage: response.ErrorMessage, ReturnedPaths: response.Paths})
		if response.ErrorCode != 0 {
			return report, fmt.Errorf("native importer error %d: %s", response.ErrorCode, response.ErrorMessage)
		}
		if response.ResultID == strings.Repeat("0", 32) {
			return report, errors.New("native importer did not return an artifact identity")
		}
		metadata, err := decodeArtifactMetaInfo(response.ArtifactBlobs[1])
		if err != nil {
			return report, err
		}
		if metadata.Address != address {
			return report, errors.New("artifact metadata address differs from request")
		}
		report.Imports[len(report.Imports)-1].Metadata = metadata
	}
	report.Stage = "native-imports-completed"
	return report, nil
}

func captureFrame(reader io.Reader, output string, report *probeReport) (Frame, error) {
	maximum := report.maxFrames
	if maximum == 0 {
		maximum = 256
	}
	if len(report.Frames) >= maximum {
		return Frame{}, errors.New("worker exceeded frame count limit")
	}
	frame, err := readFrame(reader)
	if err != nil {
		return Frame{}, err
	}
	if frame.Kind != 1 || len(frame.Payload) == 0 {
		return Frame{}, errors.New("expected worker application frame")
	}
	total := len(frame.Payload)
	for _, previous := range report.Frames {
		total += previous.Size
	}
	byteLimit := report.maxFrameBytes
	if byteLimit == 0 {
		byteLimit = 16 * 1024 * 1024
	}
	if total > byteLimit {
		return Frame{}, errors.New("worker exceeded total frame bytes limit")
	}
	path := filepath.Join(output, fmt.Sprintf("frame-%02d-type-%d.bin", len(report.Frames), frame.Payload[0]))
	if err := os.WriteFile(path, frame.Payload, 0600); err != nil {
		return Frame{}, err
	}
	digest := sha256.Sum256(frame.Payload)
	report.Frames = append(report.Frames, frameEvidence{Type: frame.Payload[0], Size: len(frame.Payload), SHA256: hex.EncodeToString(digest[:]), Path: path})
	return frame, nil
}

func rssKiB(pid int) int64 {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return 0
	}
	for _, line := range strings.Split(string(data), "\n") {
		if strings.HasPrefix(line, "VmRSS:") {
			fields := strings.Fields(line)
			if len(fields) >= 2 {
				value, _ := strconv.ParseInt(fields[1], 10, 64)
				return value
			}
		}
	}
	return 0
}
