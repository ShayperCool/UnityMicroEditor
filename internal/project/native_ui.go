package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"regexp"
	"strconv"
	"strings"
)

var nativeUIPath = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*(?:\[[0-9]{1,5}\])?(?:\.[A-Za-z_][A-Za-z0-9_]*(?:\[[0-9]{1,5}\])?)*$`)
var nativeUIManagedPath = regexp.MustCompile(`^references\.RefIds\[([0-9]{1,5})\]\.data\.`)

func validateNativeUIBindings(file NativeContentFile, files map[string]NativeContentFile) error {
	if len(file.UIBindings) > 0 && file.UIBindingProof == nil {
		return fmt.Errorf("native UI bindings lack extraction proof")
	}
	if p := file.UIBindingProof; p != nil {
		payload, err := json.Marshal(file.UIBindings)
		if err != nil {
			return err
		}
		hash := sha256.Sum256(payload)
		if p.SchemaVersion != 1 || p.Mode != "native-ui-pptr-bindings-v1" || p.NativeSHA256 != file.SHA256 || !validNativeHex(p.ReaderSHA256, 64) || !validNativeHex(p.DumpSHA256, 64) || p.BindingsSHA256 != hex.EncodeToString(hash[:]) {
			return fmt.Errorf("native UI binding extraction proof is invalid")
		}
	}
	owners := map[string]string{}
	for _, value := range file.Objects {
		owners[value.FileID] = value.TypeName
	}
	deps := map[string]bool{}
	for _, dep := range file.Dependencies {
		if !dep.Global {
			deps[dep.GUID] = true
		}
	}
	seen := map[string]bool{}
	if len(file.UIBindings) > 4096 {
		return fmt.Errorf("native UI binding count exceeds bounds")
	}
	for _, binding := range file.UIBindings {
		kind := owners[binding.OwnerFileID]
		if kind != "UnityEngine.UIElements.StyleSheet" && kind != "UnityEngine.UIElements.VisualTreeAsset" {
			return fmt.Errorf("native UI binding owner is not a published UI asset")
		}
		local := binding.TargetGUID == file.GUID
		if !nativeUIPath.MatchString(binding.PropertyPath) || len(binding.PropertyPath) > 1024 || !validNativeGUID(binding.TargetGUID) || !validNativeID(binding.TargetFileID) || (!deps[binding.TargetGUID] && !local) {
			return fmt.Errorf("native UI binding identity/path/dependency is invalid")
		}
		key := binding.OwnerFileID + "/" + binding.PropertyPath
		if seen[key] {
			return fmt.Errorf("duplicate native UI binding property")
		}
		seen[key] = true
		managed := nativeUIManagedPath.FindStringSubmatch(binding.PropertyPath)
		if strings.HasPrefix(binding.PropertyPath, "references.") {
			if len(managed) != 2 || !validNativeID(binding.ManagedReferenceID) || strings.HasPrefix(binding.ManagedReferenceID, "-") {
				return fmt.Errorf("native UI managed binding lacks an exact registry identity")
			}
			index, _ := strconv.Atoi(managed[1])
			if index > 16383 {
				return fmt.Errorf("native UI registry index exceeds bounds")
			}
		} else if binding.ManagedReferenceID != "" {
			return fmt.Errorf("native UI nonmanaged binding has a registry identity")
		}
		target, ok := files[binding.TargetGUID]
		if !ok {
			return fmt.Errorf("native UI target content file is unavailable")
		}
		found := false
		for _, object := range target.Objects {
			if object.FileID == binding.TargetFileID {
				if local && object.TypeName != "UnityEngine.UIElements.StyleSheet" {
					return fmt.Errorf("native UI local target must identify a stylesheet")
				}
				found = true
			}
		}
		if !found {
			return fmt.Errorf("native UI target local ID is not published")
		}
	}
	return nil
}
