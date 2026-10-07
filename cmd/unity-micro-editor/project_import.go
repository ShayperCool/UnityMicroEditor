package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

type repeatedAssetFlag []string

func (assets *repeatedAssetFlag) String() string { return strings.Join(*assets, ",") }
func (assets *repeatedAssetFlag) Set(value string) error {
	if len(*assets) >= 4096 {
		return errors.New("project import exceeds source asset count bound")
	}
	*assets = append(*assets, value)
	return nil
}

type projectImportedFile struct {
	Name   string             `json:"name"`
	Path   string             `json:"path"`
	Hash   string             `json:"hash"`
	SHA256 string             `json:"sha256"`
	Bytes  uint64             `json:"bytes"`
	Proof  ArtifactExtraction `json:"proof"`
}

type projectImportedAsset struct {
	Path     string                `json:"path"`
	GUID     string                `json:"guid"`
	ResultID string                `json:"resultId"`
	Files    []projectImportedFile `json:"files"`
}

type projectImportContent struct {
	Project         string                 `json:"project"`
	SourceUnchanged bool                   `json:"sourceUnchanged"`
	Assets          []projectImportedAsset `json:"assets"`
	ElapsedSeconds  float64                `json:"elapsedSeconds"`
}

func runProjectImportCLI(args []string) error {
	flags := flag.NewFlagSet("project-import", flag.ContinueOnError)
	var opts options
	var assets repeatedAssetFlag
	flags.StringVar(&opts.Editor, "editor", defaultUnityEditor(), "supported native Unity executable")
	flags.StringVar(&opts.Project, "project", ".", "source Unity project, read only")
	flags.StringVar(&opts.Output, "output", "", "new directory beneath source project Temp")
	flags.Var(&assets, "asset", "native source asset path; repeat for multiple imports")
	flags.DurationVar(&opts.Timeout, "timeout", 45*time.Second, "whole import deadline, at most 5m")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || opts.Output == "" || len(assets) == 0 || opts.Timeout <= 0 || opts.Timeout > 5*time.Minute {
		return errors.New("use project-import --project <source> --output <new Temp directory> --asset Assets/path.fbx [--asset ...] --timeout 45s")
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	ctx, deadlineCancel := context.WithTimeout(ctx, opts.Timeout)
	defer deadlineCancel()
	report, err := runProjectImport(ctx, opts, assets)
	if report != nil {
		data, marshalErr := json.MarshalIndent(report, "", "  ")
		if marshalErr != nil {
			return errors.Join(err, marshalErr)
		}
		if writeErr := os.WriteFile(filepath.Join(opts.Output, "result.json"), append(data, '\n'), 0600); writeErr != nil {
			err = errors.Join(err, writeErr)
			report.Success, report.Error = false, err.Error()
			data, _ = json.MarshalIndent(report, "", "  ")
		}
		fmt.Println(string(data))
	}
	return err
}

func runProjectImport(ctx context.Context, opts options, assets []string) (report *probeReport, returnedErr error) {
	started := time.Now()
	prepared, err := CreateProjectImportContext(ctx, opts.Project, opts.Output, assets)
	if err != nil {
		return nil, err
	}
	input := ProjectRegistryInput{Project: prepared.Project, UnityVersion: prepared.UnityVersion}
	opts.Jobs = nil
	for _, asset := range prepared.Assets {
		input.Assets = append(input.Assets, ProjectRegistryAsset{Path: asset.Path, GUID: asset.GUID})
		opts.Jobs = append(opts.Jobs, nativeImportJob{asset.GUID, -1})
	}
	registry, err := WriteProjectSourceRegistry(input)
	if err != nil {
		return nil, err
	}
	opts.Context, opts.Registry, opts.ProjectContext, opts.Mode = prepared.Project, &registry, prepared, "import"
	preparationSeconds := time.Since(started).Seconds()
	report, err = runProbe(ctx, opts)
	if report == nil {
		return nil, err
	}
	report.Experiment = "native-project-assets-own-coordinator"
	report.NativeSeconds += preparationSeconds
	report.Stages["projectContextAndRegistry"] = preparationSeconds
	report.Stages["workerReadyTotal"] += preparationSeconds
	defer func() {
		report.ElapsedSeconds = time.Since(started).Seconds()
		report.CoordinatorRSSKiB = rssKiB(os.Getpid())
		returnedErr = errors.Join(returnedErr, ctx.Err())
		if returnedErr != nil {
			report.Success, report.Error = false, returnedErr.Error()
		}
	}()
	if err != nil {
		return report, err
	}
	content, err := extractProjectImportContent(ctx, opts.Output, prepared, report)
	report.Content = content
	if err != nil {
		return report, err
	}
	report.Success, report.Stage = true, "project-artifacts-validated"
	return report, nil
}

func extractProjectImportContent(ctx context.Context, output string, prepared ProjectImportContext, report *probeReport) (content projectImportContent, returnedErr error) {
	started := time.Now()
	content.Project, content.Assets = prepared.SourceProject, []projectImportedAsset{}
	defer func() { content.ElapsedSeconds = time.Since(started).Seconds() }()
	if report.WorkerExit != "exit 0" || len(report.Imports) != len(prepared.Assets) {
		return content, errors.New("project artifact extraction requires all imports and a stopped worker")
	}
	store, err := OpenUDSArtifactStore(filepath.Join(prepared.Project, "Library", "DataStore"))
	if err != nil {
		return content, err
	}
	artifacts := filepath.Join(output, "artifacts")
	if err := os.Mkdir(artifacts, 0700); err != nil {
		return content, err
	}
	for index, source := range prepared.Assets {
		if err := ctx.Err(); err != nil {
			return content, err
		}
		imported := report.Imports[index]
		if imported.GUID != source.GUID || imported.ErrorCode != 0 || len(imported.Metadata.Files) == 0 || len(imported.Metadata.Files) > 1024 {
			return content, errors.New("project import metadata has no valid linked output set")
		}
		asset := projectImportedAsset{Path: source.Path, GUID: source.GUID, ResultID: imported.ResultID, Files: []projectImportedFile{}}
		for fileIndex, file := range imported.Metadata.Files {
			// Metadata names may be paths chosen by native importers. They
			// remain metadata; output basenames are generated by us.
			destination := filepath.Join(artifacts, fmt.Sprintf("%s-%04d.bin", source.GUID, fileIndex))
			proof, err := store.Extract(file.Hash[:], destination)
			if err != nil {
				return content, fmt.Errorf("extract %s output %d: %w", source.Path, fileIndex, err)
			}
			asset.Files = append(asset.Files, projectImportedFile{Name: file.Name, Path: proof.Output,
				Hash: proof.RawHashKey, SHA256: proof.SHA256, Bytes: proof.Bytes, Proof: proof})
		}
		content.Assets = append(content.Assets, asset)
	}
	if err := VerifyProjectImportSources(ctx, prepared); err != nil {
		return content, err
	}
	content.SourceUnchanged = true
	return content, nil
}
