package main

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	goproject "unitymicroeditor/internal/project"
)

// native-adopt reuses a stopped standalone worker report. Every source and UDS
// output is revalidated; this command starts neither a worker nor an Editor.
func runNativeAdoptCLI(args []string) error {
	flags := flag.NewFlagSet("native-adopt", flag.ContinueOnError)
	var opts launchOptions
	var reportPath string
	flags.StringVar(&opts.Project, "project", ".", "source Unity project")
	flags.StringVar(&opts.Baseline, "baseline", "", "validated host current.json")
	flags.StringVar(&opts.Output, "output", "", "owned session cache beneath project Temp or Library/UnityMicroEditor")
	flags.StringVar(&opts.Editor, "editor", defaultUnityEditor(), "matching pinned native executable")
	flags.StringVar(&reportPath, "report", "", "successful project-import result.json")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || opts.Output == "" || reportPath == "" {
		return errors.New("use native-adopt --project <source> --output <session Temp cache> --report <project-import result.json> [--baseline current.json]")
	}
	project, err := filepath.Abs(opts.Project)
	if err != nil {
		return err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	opts.Project = project
	if opts.Baseline == "" {
		opts.Baseline = filepath.Join(project, "Temp/UnityPlay/hybrid-sample/current.json")
	}
	opts.Output, err = filepath.Abs(opts.Output)
	if err != nil {
		return err
	}
	canonical, err := canonicalDestination(opts.Output)
	if err != nil || canonical != opts.Output || !goproject.IsOwnedCachePath(project, canonical) {
		return errors.New("native adoption output must be owned beneath project Temp or Library/UnityMicroEditor without symlink ancestors")
	}
	reportPath, err = filepath.Abs(reportPath)
	if err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	started := time.Now()
	report, prepared, content, err := readColdAdoptionReport(ctx, project, reportPath)
	if err != nil {
		return err
	}
	var baseline baselineBuild
	if err := readJSONFile(opts.Baseline, &baseline); err != nil {
		return err
	}
	if baseline.Project != project || baseline.Manifest == "" {
		return errors.New("native adoption baseline belongs to another project")
	}
	session, err := goproject.NewSession(goproject.Options{Project: project, Manifest: baseline.Manifest, NativeCatalog: baseline.NativeImport.Catalog})
	if err != nil {
		return err
	}
	importer, err := newNativeProjectImporter(opts, session)
	if err != nil {
		return err
	}
	inputs, err := adoptionInputs(importer, prepared)
	if err != nil {
		return err
	}
	logicals := make([]string, 0, len(content.Assets))
	for _, asset := range content.Assets {
		logicals = append(logicals, asset.Path)
	}
	root := filepath.Join(opts.Output, "native/adopt", randomID())
	if err := importer.adoptColdReport(ctx, report, root, reportPath, inputs, logicals); err != nil {
		return err
	}
	emitEvent(map[string]any{"event": "native-adopted", "assetCount": len(logicals), "environmentSha256": importer.environmentSHA256, "report": reportPath, "index": filepath.Join(opts.Output, "native/index.json"), "elapsedSeconds": time.Since(started).Seconds(), "workerStarted": false, "mainEditorStarted": false})
	return nil
}

func adoptionInputs(importer *nativeProjectImporter, prepared ProjectImportContext) (map[string]nativeColdInput, error) {
	inputs := make(map[string]nativeColdInput, len(prepared.Assets))
	guids := make(map[string]bool, len(prepared.Assets))
	for _, asset := range prepared.Assets {
		input, err := importer.coldInput(asset.Path)
		if err != nil {
			return nil, err
		}
		physical, err := resolveProjectImportSource(importer.opts.Project, asset.Path)
		if err != nil || input.guid != asset.GUID || prepared.SourceHashes[physical] != input.sourceSHA || prepared.SourceHashes[physical+".meta"] != input.metaSHA {
			return nil, errors.New("native adoption source/meta hashes differ from the imported revision: " + asset.Path)
		}
		if _, exists := inputs[asset.Path]; exists || guids[asset.GUID] {
			return nil, errors.New("native adoption report contains a duplicate source path or GUID")
		}
		inputs[asset.Path], guids[asset.GUID] = input, true
	}
	return inputs, nil
}

func readColdAdoptionReport(ctx context.Context, project, reportPath string) (*probeReport, ProjectImportContext, projectImportContent, error) {
	var report probeReport
	var prepared ProjectImportContext
	var content projectImportContent
	if !goproject.IsOwnedCachePath(project, reportPath) {
		return nil, prepared, content, errors.New("native adoption report must be in the project's owned cache")
	}
	if err := validateAdoptionFile(reportPath, project, 32<<20); err != nil {
		return nil, prepared, content, err
	}
	data, err := os.ReadFile(reportPath)
	if err != nil {
		return nil, prepared, content, err
	}
	var raw struct {
		Environment    map[string]json.RawMessage `json:"environment"`
		ProjectContext json.RawMessage            `json:"projectContext"`
		Content        json.RawMessage            `json:"content"`
	}
	if err := json.Unmarshal(data, &report); err != nil {
		return nil, prepared, content, err
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, prepared, content, err
	}
	// Missing explicit keys identify pre-v2 reports even if zero-valued fields
	// happen to equal the current environment after JSON decoding.
	for _, key := range []string{"textureMaxSizeOverride", "textureCompressionOverride", "defaultTextureCompressionFormat"} {
		if len(raw.Environment[key]) == 0 || string(raw.Environment[key]) == "null" {
			return nil, prepared, content, errors.New("native adoption requires the complete v2 worker environment")
		}
	}
	if err := json.Unmarshal(raw.ProjectContext, &prepared); err != nil {
		return nil, prepared, content, err
	}
	if err := json.Unmarshal(raw.Content, &content); err != nil {
		return nil, prepared, content, err
	}
	if report.SchemaVersion != 1 || report.Experiment != "native-project-assets-own-coordinator" || !report.Success || report.Stage != "project-artifacts-validated" || report.MainEditorStarted || report.WorkerExit != "exit 0" || prepared.SchemaVersion != 1 || prepared.UnityVersion != fixtureUnityVersion || prepared.SourceProject != project || content.Project != project || !content.SourceUnchanged || len(prepared.Assets) == 0 || len(prepared.Assets) > 4096 || len(prepared.Assets) != len(content.Assets) || len(prepared.Assets) != len(report.Imports) {
		return nil, prepared, content, errors.New("native adoption requires a complete successful stopped-worker source report")
	}
	root := filepath.Dir(reportPath)
	if prepared.Project != filepath.Join(root, "project") || len(prepared.SourceHashes) == 0 || len(prepared.SourceHashes) > 32768 {
		return nil, prepared, content, errors.New("native adoption report context is outside its owned output or source bounds")
	}
	for filename, sha := range prepared.SourceHashes {
		if !validSHA256(sha) {
			return nil, prepared, content, errors.New("native adoption context has an invalid source hash")
		}
		if err := validateAdoptionFile(filename, project, 1<<30); err != nil {
			return nil, prepared, content, err
		}
	}
	if err := VerifyProjectImportSources(ctx, prepared); err != nil {
		return nil, prepared, content, err
	}
	if err := validateAdoptionFile(filepath.Join(prepared.Project, "Library/DataStore/uds.db"), root, 1<<30); err != nil {
		return nil, prepared, content, err
	}
	store, err := OpenUDSArtifactStore(filepath.Join(prepared.Project, "Library/DataStore"))
	if err != nil {
		return nil, prepared, content, err
	}
	for index, asset := range content.Assets {
		if err := ctx.Err(); err != nil {
			return nil, prepared, content, err
		}
		source, imported := prepared.Assets[index], report.Imports[index]
		if asset.Path != source.Path || asset.GUID != source.GUID || imported.GUID != source.GUID || imported.ErrorCode != 0 || imported.ResultID != asset.ResultID || len(asset.Files) == 0 || len(asset.Files) > 1024 || len(asset.Files) != len(imported.Metadata.Files) {
			return nil, prepared, content, errors.New("native adoption content differs from its source/import metadata")
		}
		for fileIndex, file := range asset.Files {
			metadata := imported.Metadata.Files[fileIndex]
			if file.Name != metadata.Name || file.Hash != hex.EncodeToString(metadata.Hash[:]) {
				return nil, prepared, content, errors.New("native adoption artifact key differs from native metadata")
			}
			if err := validateAdoptionFile(file.Path, filepath.Join(root, "artifacts"), maxArtifactBytes); err != nil {
				return nil, prepared, content, err
			}
			_, proof, err := store.Read(metadata.Hash[:])
			if err != nil || !proof.ContentKeyVerified || proof.SHA256 != file.SHA256 || proof.Bytes != file.Bytes {
				return nil, prepared, content, errors.New("native adoption artifact differs from its actual UDS content: " + file.Path)
			}
			sha, err := goFileHash(file.Path)
			if err != nil || sha != proof.SHA256 {
				return nil, prepared, content, errors.New("native adoption extracted artifact changed: " + file.Path)
			}
		}
	}
	report.ProjectContext, report.Content = prepared, content
	return &report, prepared, content, nil
}

func validateAdoptionFile(filename, root string, maximum int64) error {
	if !filepath.IsAbs(filename) || !withinDirectory(filename, root) {
		return errors.New("native adoption input is outside its source/output ownership")
	}
	canonical, err := filepath.EvalSymlinks(filename)
	if err != nil || canonical != filename {
		return errors.New("native adoption input has symlink ancestors: " + filename)
	}
	info, err := os.Stat(filename)
	if err != nil || !info.Mode().IsRegular() || info.Size() < 0 || info.Size() > maximum {
		return fmt.Errorf("native adoption input is not a bounded regular file: %s", filename)
	}
	return nil
}
