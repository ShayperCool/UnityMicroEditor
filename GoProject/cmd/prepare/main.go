package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"

	goproject "unitymicroeditor/project"
)

func main() {
	project := flag.String("project", ".", "raw project root")
	manifest := flag.String("manifest", "", "validated baseline scene manifest")
	catalog := flag.String("native-catalog", "", "compatible cached Unity importer catalog")
	output := flag.String("output", "", "new revision output root")
	flag.Parse()
	if *manifest == "" || *output == "" {
		fmt.Fprintln(os.Stderr, "--manifest and --output are required")
		os.Exit(2)
	}
	s, err := goproject.NewSession(goproject.Options{Project: *project, Manifest: *manifest, NativeCatalog: *catalog})
	if err == nil {
		var p goproject.Prepared
		p, err = s.Prepare(*output)
		if err == nil {
			json.NewEncoder(os.Stdout).Encode(p)
		}
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
