package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"

	"unitymicroeditor/internal/runtimeartifact"
)

func main() {
	input := flag.String("input", "", "hash-verified native worker SerializedFile")
	profile := flag.String("profile", "", "fbx, wav, or png")
	output := flag.String("output", "", "new runtime SerializedFile")
	flag.Parse()
	if err := run(*input, *output, *profile); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run(input, output, profile string) error {
	if !filepath.IsAbs(output) {
		return errors.New("output must be an absolute new path")
	}
	file, err := os.Open(input)
	if err != nil {
		return err
	}
	data, readErr := io.ReadAll(io.LimitReader(file, (64<<20)+1))
	if err := errors.Join(readErr, file.Close()); err != nil {
		return err
	}
	var adapted []byte
	var metadata runtimeartifact.Metadata
	if profile == "png" {
		adapted, metadata, err = runtimeartifact.AdaptTexture(data, profile)
	} else {
		adapted, metadata, err = runtimeartifact.Adapt(data, profile)
	}
	if err != nil {
		return err
	}
	file, err = os.OpenFile(output, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return err
	}
	_, writeErr := file.Write(adapted)
	if err := errors.Join(writeErr, file.Close()); err != nil {
		return err
	}
	return json.NewEncoder(os.Stdout).Encode(metadata)
}
