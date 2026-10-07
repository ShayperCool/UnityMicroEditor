package goproject

import "fmt"

func validateNativeMeshCPURetention(file NativeContentFile) error {
	types := map[string]string{}
	for _, object := range file.Objects {
		types[object.FileID] = object.TypeName
	}
	seen := map[string]bool{}
	for _, mesh := range file.MeshCPURetention {
		if !validNativeID(mesh.FileID) || seen[mesh.FileID] || types[mesh.FileID] != "UnityEngine.Mesh" || mesh.OriginalIsReadable {
			return fmt.Errorf("native Mesh CPU retention must name unique originally unreadable Mesh objects: %s", mesh.FileID)
		}
		seen[mesh.FileID] = true
	}
	return nil
}
