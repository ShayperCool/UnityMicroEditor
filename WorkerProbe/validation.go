package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type contentValidation struct {
	Artifacts        []ArtifactExtraction     `json:"artifacts"`
	Inspections      []ArtifactTextInspection `json:"inspections"`
	Mesh             FixtureMeshFingerprint   `json:"mesh"`
	PCM              FixturePCMFingerprint    `json:"pcm"`
	AudioClip        FixtureAudioClipMetadata `json:"audioClip"`
	ReferenceMatched bool                     `json:"referenceMatched"`
	SourceUnchanged  bool                     `json:"sourceUnchanged"`
	ElapsedSeconds   float64                  `json:"elapsedSeconds"`
}

func receiveMessage(reader io.Reader, output string, report *probeReport, expected byte) (Frame, error) {
	for {
		frame, err := captureFrame(reader, output, report)
		if err != nil {
			return Frame{}, err
		}
		if frame.Payload[0] == expected {
			return frame, nil
		}
		if frame.Payload[0] != 2 && frame.Payload[0] != 3 {
			return Frame{}, fmt.Errorf("unsupported worker message type %d while awaiting %d", frame.Payload[0], expected)
		}
		message, err := decodeWorkerLog(frame.Payload)
		if err != nil {
			return Frame{}, err
		}
		if strings.Contains(message, "Exception:") || strings.Contains(message, "Assertion failed") {
			return Frame{}, fmt.Errorf("native importer diagnostic: %s", message)
		}
	}
}

func validateWorkerVersion(log string) error {
	file, err := os.Open(log)
	if err != nil {
		return err
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, 128*1024))
	if err != nil {
		return err
	}
	for _, line := range strings.Split(string(data), "\n") {
		if !strings.HasPrefix(line, "Unity Editor version:") {
			continue
		}
		fields := strings.Fields(strings.TrimPrefix(line, "Unity Editor version:"))
		if len(fields) > 0 && fields[0] == fixtureUnityVersion {
			return nil
		}
		return fmt.Errorf("worker Unity version differs from supported %s", fixtureUnityVersion)
	}
	return errors.New("worker did not report its Unity version")
}

func validateImportedContent(ctx context.Context, opts options, report *probeReport) (proof contentValidation, returnedErr error) {
	started := time.Now()
	defer func() { proof.ElapsedSeconds = time.Since(started).Seconds() }()
	if err := ctx.Err(); err != nil {
		return proof, err
	}
	fixture, ok := report.Fixture.(Fixture)
	if !ok || len(report.Imports) != 2 || report.WorkerExit != "exit 0" {
		return proof, errors.New("content validation requires two completed imports and a stopped worker")
	}
	store, err := OpenUDSArtifactStore(filepath.Join(fixture.Project, "Library", "DataStore"))
	if err != nil {
		return proof, err
	}
	output := filepath.Join(opts.Output, "artifacts")
	if err := os.Mkdir(output, 0700); err != nil {
		return proof, err
	}
	var modelFound, audioFound, pcmFound bool
	var audioDump []byte
	var resourceBytes uint64
	for jobIndex, job := range report.Imports {
		if len(job.Metadata.Files) == 0 {
			return proof, errors.New("import metadata has no output files")
		}
		for fileIndex, file := range job.Metadata.Files {
			content, extraction, err := store.Read(file.Hash[:])
			if err != nil {
				return proof, err
			}
			path := filepath.Join(output, fmt.Sprintf("%d-%d-%s.bin", jobIndex, fileIndex, extraction.RawHashKey))
			extraction, err = store.Extract(file.Hash[:], path)
			if err != nil {
				return proof, err
			}
			proof.Artifacts = append(proof.Artifacts, extraction)
			if file.Name == "" {
				inspection, err := InspectSerializedArtifact(ctx, opts.Editor, path, path+".txt")
				if err != nil {
					return proof, err
				}
				proof.Inspections = append(proof.Inspections, inspection)
				dump, err := os.ReadFile(inspection.Output)
				if err != nil {
					return proof, err
				}
				if jobIndex == 0 {
					proof.Mesh, err = FingerprintFixtureMesh(dump)
					if err != nil {
						return proof, err
					}
					modelFound = true
				} else {
					audioDump = dump
					audioFound = true
				}
			} else if jobIndex == 1 && file.Name == ".resource" {
				proof.PCM, err = FingerprintFixturePCM(content)
				if err != nil {
					return proof, err
				}
				pcmFound = true
				resourceBytes = uint64(len(content))
			}
		}
	}
	if !modelFound || !audioFound || !pcmFound {
		return proof, errors.New("linked artifacts do not contain the complete Mesh, AudioClip and PCM resource")
	}
	proof.AudioClip, err = ValidateFixtureAudioClipDump(audioDump, resourceBytes)
	if err != nil {
		return proof, err
	}
	if proof.PCM.PCMFloatSHA256 != expectedFixturePCMHash() {
		return proof, errors.New("native PCM differs from the input WAV")
	}
	if opts.Reference != "" {
		if err := compareReference(opts.Reference, fixture, proof); err != nil {
			return proof, err
		}
		proof.ReferenceMatched = true
	}
	for path, expected := range fixture.SourceHashes {
		data, err := os.ReadFile(path)
		if err != nil {
			return proof, err
		}
		if bytesSHA(data) != expected {
			return proof, errors.New("source asset changed during experiment")
		}
	}
	proof.SourceUnchanged = true
	return proof, ctx.Err()
}

func expectedFixturePCMHash() string {
	pcm := fixtureWAV()[44:]
	var values bytes.Buffer
	binary.Write(&values, binary.LittleEndian, int32(len(pcm)/2))
	for len(pcm) >= 2 {
		value := float32(int16(binary.LittleEndian.Uint16(pcm[:2]))) / 32768
		binary.Write(&values, binary.LittleEndian, math.Float32bits(value))
		pcm = pcm[2:]
	}
	return bytesSHA(values.Bytes())
}

func compareReference(path string, fixture Fixture, content contentValidation) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	var reference struct {
		Success      bool   `json:"success"`
		UnityVersion string `json:"unityVersion"`
		Model        struct {
			GUID   string `json:"guid"`
			Meshes []struct {
				FixtureMeshFingerprint
				Submeshes []struct {
					IndexCount    int    `json:"indexCount"`
					IndicesSHA256 string `json:"indicesSha256"`
				} `json:"submeshes"`
			} `json:"meshes"`
		} `json:"model"`
		Audio struct {
			GUID           string `json:"guid"`
			Samples        int    `json:"samples"`
			Channels       int    `json:"channels"`
			Frequency      int    `json:"frequency"`
			PCMFloatSHA256 string `json:"pcmFloatSha256"`
		} `json:"audio"`
	}
	if err := json.NewDecoder(io.LimitReader(file, 1024*1024)).Decode(&reference); err != nil {
		return err
	}
	if !reference.Success || reference.UnityVersion != fixtureUnityVersion || reference.Model.GUID != fixture.ModelGUID || reference.Audio.GUID != fixture.AudioGUID || len(reference.Model.Meshes) != 1 {
		return errors.New("reference identity, version or mesh count differs")
	}
	mesh := reference.Model.Meshes[0]
	if len(mesh.Submeshes) != 1 {
		return errors.New("reference submesh layout differs")
	}
	mesh.IndexCount, mesh.IndicesSHA256 = mesh.Submeshes[0].IndexCount, mesh.Submeshes[0].IndicesSHA256
	if mesh.FixtureMeshFingerprint != content.Mesh {
		return errors.New("native mesh arrays differ from the Editor reference")
	}
	audio := reference.Audio
	if audio.Samples != content.PCM.Samples || audio.Channels != content.PCM.Channels || audio.Frequency != content.PCM.Frequency || audio.PCMFloatSHA256 != content.PCM.PCMFloatSHA256 {
		return errors.New("native PCM differs from the Editor reference")
	}
	return nil
}
