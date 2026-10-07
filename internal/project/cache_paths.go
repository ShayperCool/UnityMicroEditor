package goproject

import (
	"path/filepath"
	"strings"
)

// IsOwnedCachePath accepts a child of an ignored project cache directory.
// Callers must separately reject symlinks and verify the canonical destination.
// Unity clears Temp when reopening a project; Library/UnityMicroEditor persists.
func IsOwnedCachePath(project, path string) bool {
	if !filepath.IsAbs(project) || !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return false
	}
	for _, root := range []string{filepath.Join(project, "Temp"), filepath.Join(project, "Library", "UnityMicroEditor")} {
		relative, err := filepath.Rel(root, path)
		if err == nil && relative != "." && relative != ".." && !filepath.IsAbs(relative) && !strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
			return true
		}
	}
	return false
}
