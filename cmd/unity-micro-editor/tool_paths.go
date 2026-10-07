package main

import (
	"fmt"
	"os"
	"path/filepath"
)

// Tool sources belong to the installation, independently of the target project.
// The shell entrypoint supplies this path even when the executable lives in a cache.
func microEditorRoot() (string, error) {
	if configured := os.Getenv("UNITY_MICRO_EDITOR_ROOT"); configured != "" {
		return checkedMicroEditorRoot(configured)
	}
	var starts []string
	if current, err := os.Getwd(); err == nil {
		starts = append(starts, current)
	}
	if executable, err := os.Executable(); err == nil {
		starts = append(starts, filepath.Dir(executable))
	}
	for _, start := range starts {
		for current := start; ; current = filepath.Dir(current) {
			for _, candidate := range []string{current, filepath.Join(current, "UnityMicroEditor")} {
				if root, err := checkedMicroEditorRoot(candidate); err == nil {
					return root, nil
				}
			}
			if filepath.Dir(current) == current {
				break
			}
		}
	}
	return "", fmt.Errorf("Unity Micro Editor sources not found; set UNITY_MICRO_EDITOR_ROOT to the installation directory")
}

func checkedMicroEditorRoot(candidate string) (string, error) {
	root, err := filepath.Abs(candidate)
	if err != nil {
		return "", err
	}
	for _, file := range []string{"cmd/unity-micro-editor/engine-profile.json", "tools/LifecycleCompiler/Program.cs", "Packages/com.unitymicroeditor.runtime/package.json"} {
		if info, err := os.Stat(filepath.Join(root, file)); err != nil || !info.Mode().IsRegular() {
			return "", fmt.Errorf("invalid Unity Micro Editor installation: %s", root)
		}
	}
	return root, nil
}

func defaultUnityEditor() string {
	if configured := os.Getenv("UNITY_EDITOR"); configured != "" {
		return configured
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, "Unity/Hub/Editor", fixtureUnityVersion, "Editor/Unity")
}
