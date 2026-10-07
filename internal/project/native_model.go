package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"math/big"
	"sort"
	"strconv"
)

var staticModelReferences = map[int32][]string{
	1:  {"m_Component"},
	4:  {"m_GameObject", "m_Children", "m_Father"},
	33: {"m_GameObject", "m_Mesh"},
	23: {"m_GameObject", "m_Materials", "m_StaticBatchRoot", "m_ProbeAnchor", "m_LightProbeVolumeOverride", "m_AdditionalVertexStreams"},
}
var staticModelScalars = map[int32][]string{
	1:  {"m_Name", "m_Layer", "m_TagString", "m_IsActive", "m_StaticEditorFlags"},
	4:  {"m_LocalPosition", "m_LocalRotation", "m_LocalScale"},
	33: {},
	23: {"m_Enabled", "m_CastShadows", "m_ReceiveShadows", "m_DynamicOccludee", "m_MotionVectors", "m_LightProbeUsage", "m_ReflectionProbeUsage", "m_RenderingLayerMask", "m_SortingLayerID", "m_SortingOrder"},
}

func nativeModelGraphSHA256(graph []NativeModelGraphObject) string {
	canonical := append([]NativeModelGraphObject(nil), graph...)
	sort.Slice(canonical, func(i, j int) bool {
		first, _ := strconv.ParseInt(canonical[i].FileID, 10, 64)
		second, _ := strconv.ParseInt(canonical[j].FileID, 10, 64)
		return first < second
	})
	data, err := json.Marshal(canonical)
	if err != nil {
		return ""
	}
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

func nativeInteger(value any, minimum, maximum int64) bool {
	if value == nil {
		return false
	}
	switch value.(type) {
	case bool, string, map[string]any, []any:
		return false
	}
	number, valid := new(big.Rat).SetString(fmt.Sprint(value))
	return valid && number.IsInt() && number.Num().IsInt64() && number.Num().Int64() >= minimum && number.Num().Int64() <= maximum
}
func nativeVector(value any, quaternion bool) bool {
	vector, valid := value.(map[string]any)
	wanted := []string{"x", "y", "z"}
	if quaternion {
		wanted = append(wanted, "w")
	}
	if !valid || len(vector) != len(wanted) {
		return false
	}
	for _, axis := range wanted {
		value, exists := vector[axis]
		if !exists {
			return false
		}
		switch value.(type) {
		case bool, string, map[string]any, []any:
			return false
		}
		number, err := strconv.ParseFloat(fmt.Sprint(value), 32)
		if err != nil || math.IsNaN(number) || math.IsInf(number, 0) {
			return false
		}
	}
	return true
}
func validateStaticModelScalars(graph NativeModelGraphObject) error {
	wanted, supported := staticModelScalars[graph.ClassID]
	if !supported || len(graph.Scalars) != len(wanted) {
		return fmt.Errorf("native model class %d has unsupported scalar fields", graph.ClassID)
	}
	for _, name := range wanted {
		value, exists := graph.Scalars[name]
		valid := exists
		switch name {
		case "m_Name", "m_TagString":
			_, stringValue := value.(string)
			valid = valid && stringValue
		case "m_LocalPosition", "m_LocalRotation", "m_LocalScale":
			valid = valid && nativeVector(value, name == "m_LocalRotation")
		case "m_Layer":
			valid = valid && nativeInteger(value, 0, 31)
		case "m_Enabled", "m_IsActive", "m_ReceiveShadows", "m_DynamicOccludee":
			valid = valid && nativeInteger(value, 0, 1)
		case "m_CastShadows":
			valid = valid && nativeInteger(value, 0, 3)
		case "m_MotionVectors", "m_LightProbeUsage", "m_ReflectionProbeUsage":
			valid = valid && nativeInteger(value, 0, 2)
		case "m_SortingLayerID":
			valid = valid && nativeInteger(value, math.MinInt32, math.MaxInt32)
		case "m_SortingOrder":
			valid = valid && nativeInteger(value, math.MinInt16, math.MaxInt16)
		default:
			valid = valid && nativeInteger(value, 0, math.MaxUint32)
		}
		if !valid {
			return fmt.Errorf("native model scalar %s is invalid at %s", name, graph.FileID)
		}
	}
	return nil
}

// staticNativeModelDocuments consumes only the native adapter's verified static
// model projection. It supplies no guessed hierarchy, names, poses or materials.
// Serialized runtime object bodies and their SHA are verified separately by the
// native replacement transaction before these documents become visible.
func (s *Session) staticNativeModelDocuments(replacement NativeReplacement) ([]Document, error) {
	objects := map[string]string{}
	graphObjects := map[string]NativeModelGraphObject{}
	for _, value := range replacement.Content.Objects {
		if !validNativeID(value.FileID) || objects[value.FileID] != "" {
			return nil, fmt.Errorf("native model object ID is invalid or duplicated: %s", value.FileID)
		}
		class := nativeContentClass(value.TypeName)
		if class != 1 && class != 4 && class != 23 && class != 33 && class != 21 && class != 43 {
			return nil, fmt.Errorf("native model type %s is not a verified static profile", value.TypeName)
		}
		objects[value.FileID] = value.TypeName
	}
	for _, graph := range replacement.ModelGraph {
		if !validNativeID(graph.FileID) || graphObjects[graph.FileID].FileID != "" || nativeContentClass(objects[graph.FileID]) != graph.ClassID {
			return nil, fmt.Errorf("native model graph identity/type is invalid: %s", graph.FileID)
		}
		if _, supported := staticModelReferences[graph.ClassID]; !supported {
			return nil, fmt.Errorf("native model graph class %d is unsupported", graph.ClassID)
		}
		graphObjects[graph.FileID] = graph
	}
	if len(graphObjects) == 0 {
		return nil, fmt.Errorf("native model has no verified static hierarchy")
	}
	for id, typeName := range objects {
		class := nativeContentClass(typeName)
		if class != 21 && class != 43 && graphObjects[id].FileID == "" {
			return nil, fmt.Errorf("native model graph omits object %s", id)
		}
	}
	dependencies := map[string]bool{}
	for _, dependency := range replacement.Content.Dependencies {
		dependencies[dependency.GUID] = true
	}
	targetType := func(target NativeModelGraphTarget) (string, error) {
		if target.FileID == "0" {
			if target.GUID != "" {
				return "", fmt.Errorf("null model pointer has a GUID")
			}
			return "", nil
		}
		if !validNativeID(target.FileID) {
			return "", fmt.Errorf("invalid model pointer local ID %s", target.FileID)
		}
		if target.GUID == "" || target.GUID == replacement.GUID {
			if objects[target.FileID] == "" {
				return "", fmt.Errorf("dangling native model local reference %s", target.FileID)
			}
			return objects[target.FileID], nil
		}
		if !dependencies[target.GUID] {
			return "", fmt.Errorf("native model external pointer has no declared dependency: %s", target.GUID)
		}
		if builtinGUID(target.GUID) {
			return "builtin", nil
		}
		if dependency, found := s.nativeOverlays[target.GUID]; found {
			for _, object := range dependency.Content.Objects {
				if object.FileID == target.FileID {
					return object.TypeName, nil
				}
			}
		}
		return "", fmt.Errorf("native model external pointer does not resolve to a verified runtime object: %s/%s", target.GUID, target.FileID)
	}
	references := map[string]map[string][]NativeModelGraphTarget{}
	documents := make([]Document, 0, len(replacement.ModelGraph))
	for _, graph := range replacement.ModelGraph {
		if err := validateStaticModelScalars(graph); err != nil {
			return nil, err
		}
		wanted := staticModelReferences[graph.ClassID]
		if len(graph.References) != len(wanted) {
			return nil, fmt.Errorf("native model references do not match verified class %d", graph.ClassID)
		}
		data := map[string]any{}
		for name, value := range graph.Scalars {
			data[name] = clone(value)
		}
		if graph.ClassID == 4 {
			// Unity strips this Inspector bookkeeping from Player data. Retain an
			// override target for authored prefab hints; runtime Transform rotation
			// and the verified model fingerprint use m_LocalRotation exclusively.
			data["m_LocalEulerAnglesHint"] = map[string]any{"x": int64(0), "y": int64(0), "z": int64(0)}
		}
		properties := map[string][]NativeModelGraphTarget{}
		for index, property := range graph.References {
			if property.Property != wanted[index] {
				return nil, fmt.Errorf("native model reference property/order differs at %s", graph.FileID)
			}
			list := property.Property == "m_Component" || property.Property == "m_Children" || property.Property == "m_Materials"
			if !list && len(property.Targets) != 1 {
				return nil, fmt.Errorf("native model scalar reference has wrong cardinality: %s", property.Property)
			}
			values := make([]any, 0, len(property.Targets))
			seen := map[string]bool{}
			for _, target := range property.Targets {
				typeName, err := targetType(target)
				if err != nil {
					return nil, err
				}
				local := target.GUID == "" || target.GUID == replacement.GUID
				wantedType := ""
				switch property.Property {
				case "m_Component":
					if !local || target.FileID == "0" || typeName != "UnityEngine.Transform" && typeName != "UnityEngine.MeshRenderer" && typeName != "UnityEngine.MeshFilter" {
						return nil, fmt.Errorf("native GameObject has unsupported component %s", target.FileID)
					}
					if seen[target.FileID] {
						return nil, fmt.Errorf("native GameObject repeats component %s", target.FileID)
					}
					seen[target.FileID] = true
				case "m_GameObject":
					wantedType = "UnityEngine.GameObject"
					if !local || target.FileID == "0" {
						return nil, fmt.Errorf("native component owner must be local")
					}
				case "m_Children":
					wantedType = "UnityEngine.Transform"
					if !local || target.FileID == "0" || seen[target.FileID] {
						return nil, fmt.Errorf("native child pointer invalid or duplicated")
					}
					seen[target.FileID] = true
				case "m_Father", "m_StaticBatchRoot", "m_ProbeAnchor":
					wantedType = "UnityEngine.Transform"
					if !local && property.Property == "m_Father" {
						return nil, fmt.Errorf("native model parent must be local")
					}
				case "m_Mesh", "m_AdditionalVertexStreams":
					wantedType = "UnityEngine.Mesh"
				case "m_Materials":
					wantedType = "UnityEngine.Material"
				case "m_LightProbeVolumeOverride":
					wantedType = "UnityEngine.GameObject"
				}
				if target.FileID != "0" && wantedType != "" && typeName != wantedType && typeName != "builtin" {
					return nil, fmt.Errorf("native model %s target type differs: %s", property.Property, typeName)
				}
				ref := map[string]any{"fileID": json.Number(target.FileID)}
				if !local && target.FileID != "0" {
					ref["guid"], ref["type"] = target.GUID, int64(3)
				}
				if property.Property == "m_Component" {
					values = append(values, map[string]any{"component": ref})
				} else {
					values = append(values, ref)
				}
			}
			properties[property.Property] = property.Targets
			if list {
				data[property.Property] = values
			} else {
				data[property.Property] = values[0]
			}
		}
		references[graph.FileID] = properties
		documents = append(documents, Document{ClassID: int(graph.ClassID), FileID: graph.FileID, TypeName: objects[graph.FileID], Data: data})
	}
	owners := map[string]string{}
	parents := map[string]string{}
	rootCount := 0
	for id, graph := range graphObjects {
		properties := references[id]
		if graph.ClassID == 1 {
			transforms := 0
			for _, target := range properties["m_Component"] {
				component := graphObjects[target.FileID]
				if component.ClassID == 4 {
					transforms++
				}
				if owners[target.FileID] != "" || references[target.FileID]["m_GameObject"][0].FileID != id {
					return nil, fmt.Errorf("native model component ownership disagrees at %s", target.FileID)
				}
				owners[target.FileID] = id
			}
			if transforms != 1 {
				return nil, fmt.Errorf("native GameObject must own exactly one Transform: %s", id)
			}
		} else if graph.ClassID == 4 {
			parent := properties["m_Father"][0].FileID
			parents[id] = parent
			if parent == "0" {
				rootCount++
			}
		}
	}
	if rootCount != 1 {
		return nil, fmt.Errorf("native model must contain one authored root Transform, got %d", rootCount)
	}
	for id, graph := range graphObjects {
		if graph.ClassID != 1 && owners[id] == "" {
			return nil, fmt.Errorf("native component is absent from its owner's component list: %s", id)
		}
		if graph.ClassID != 4 {
			continue
		}
		for _, child := range references[id]["m_Children"] {
			if parents[child.FileID] != id {
				return nil, fmt.Errorf("native child/parent links disagree: %s", child.FileID)
			}
		}
		if parent := parents[id]; parent != "0" {
			found := false
			for _, child := range references[parent]["m_Children"] {
				if child.FileID == id {
					found = true
				}
			}
			if !found {
				return nil, fmt.Errorf("native parent omits its child: %s", id)
			}
		}
		visited := map[string]bool{}
		for cursor := id; cursor != "0"; cursor = parents[cursor] {
			if visited[cursor] {
				return nil, fmt.Errorf("native model hierarchy is cyclic: %s", id)
			}
			visited[cursor] = true
		}
	}
	return documents, nil
}
