// compare checks shared Editor/Player snapshots without discarding game fields.
package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"sort"
	"strings"
)

type difference struct {
	Path      string `json:"path"`
	Reference any    `json:"reference"`
	Actual    any    `json:"actual"`
	Reason    string `json:"reason"`
}

func objects(value any) ([]any, error) {
	data, ok := value.(map[string]any)
	if !ok {
		return nil, errors.New("snapshot envelope is not an object")
	}
	if success, exists := data["success"].(bool); exists && !success {
		return nil, errors.New("snapshot operation reported success=false")
	}
	if failed, _ := data["isError"].(bool); failed {
		return nil, errors.New("MCP tool reported isError=true")
	}
	if data["error"] != nil {
		return nil, fmt.Errorf("snapshot envelope contains error: %v", data["error"])
	}
	if list, ok := data["objects"].([]any); ok {
		if data["nextOffset"] != nil {
			return nil, errors.New("hierarchy snapshot is paginated; collect all pages before comparing")
		}
		if total, ok := data["total"].(float64); ok && total != float64(len(list)) {
			return nil, errors.New("hierarchy snapshot does not contain its declared total")
		}
		return list, nil
	}
	for _, key := range []string{"result", "structuredContent", "data"} {
		if nested, exists := data[key]; exists {
			if list, err := objects(nested); err == nil {
				return list, nil
			} else if strings.Contains(err.Error(), "reported") || strings.Contains(err.Error(), "paginated") || strings.Contains(err.Error(), "declared") {
				return nil, err
			}
		}
	}
	for _, key := range []string{"content", "contents"} {
		if content, ok := data[key].([]any); ok {
			for _, block := range content {
				if block, ok := block.(map[string]any); ok {
					if text, ok := block["text"].(string); ok {
						var nested any
						if json.Unmarshal([]byte(text), &nested) == nil {
							if list, err := objects(nested); err == nil {
								return list, nil
							}
						}
					}
				}
			}
		}
	}
	return nil, errors.New("no complete shared objects snapshot found")
}

func normalize(list []any) ([]any, error) {
	result := make([]any, 0, len(list))
	seen := map[string]bool{}
	for _, item := range list {
		object, ok := item.(map[string]any)
		if !ok {
			return nil, errors.New("snapshot object is not an object")
		}
		path, ok := object["path"].(string)
		if !ok || path == "" {
			return nil, errors.New("snapshot object has no path")
		}
		if seen[path] {
			return nil, fmt.Errorf("ambiguous duplicate object path %s", path)
		}
		seen[path] = true
		components, ok := object["components"].([]any)
		if !ok {
			return nil, fmt.Errorf("object %s has no component list", path)
		}
		// Ignore the resource owner only if both its reserved name and runtime type match.
		if path == "Raw scene imported assets" && hasComponent(components, "Dungeon.CLIPlay.Raw.RawSceneResources") {
			continue
		}
		copy := make(map[string]any, len(object))
		for key, value := range object {
			if key != "source" && key != "globalObjectId" && key != "prefabInstance" {
				copy[key] = value
			}
		}
		normalizedComponents := make([]any, len(components))
		for index, item := range components {
			component, ok := item.(map[string]any)
			if !ok {
				return nil, fmt.Errorf("object %s component %d is invalid", path, index)
			}
			componentCopy := make(map[string]any, len(component))
			for key, value := range component {
				if key != "source" {
					componentCopy[key] = value
				}
			}
			normalizedComponents[index] = componentCopy
		}
		copy["components"] = normalizedComponents
		result = append(result, copy)
	}
	return result, nil
}

func hasComponent(components []any, name string) bool {
	for _, component := range components {
		if component, ok := component.(map[string]any); ok && component["type"] == name {
			return true
		}
	}
	return false
}

func quaternionEqual(reference, actual map[string]any, tolerance float64) bool {
	if len(reference) != 4 || len(actual) != 4 {
		return false
	}
	for _, sign := range []float64{1, -1} {
		equal := true
		for _, key := range []string{"x", "y", "z", "w"} {
			a, aok := reference[key].(float64)
			b, bok := actual[key].(float64)
			if !aok || !bok || math.Abs(a-sign*b) > tolerance {
				equal = false
				break
			}
		}
		if equal {
			return true
		}
	}
	return false
}

func compare(reference, actual any, path string, tolerance float64, differences *[]difference) {
	if len(*differences) >= 100 {
		return
	}
	add := func(reason string) {
		*differences = append(*differences, difference{Path: path, Reference: reference, Actual: actual, Reason: reason})
	}
	switch reference := reference.(type) {
	case map[string]any:
		actualMap, ok := actual.(map[string]any)
		if !ok {
			add("type mismatch")
			return
		}
		if (strings.HasSuffix(path, ".rotation") || strings.HasSuffix(path, ".localRotation")) && quaternionEqual(reference, actualMap, tolerance) {
			return
		}
		keys := make([]string, 0, len(reference)+len(actualMap))
		for key := range reference {
			keys = append(keys, key)
		}
		for key := range actualMap {
			if _, exists := reference[key]; !exists {
				keys = append(keys, key)
			}
		}
		sort.Strings(keys)
		for _, key := range keys {
			a, aok := reference[key]
			b, bok := actualMap[key]
			if !aok || !bok {
				*differences = append(*differences, difference{Path: path + "." + key, Reference: a, Actual: b, Reason: "field missing"})
				continue
			}
			compare(a, b, path+"."+key, tolerance, differences)
		}
	case []any:
		actualList, ok := actual.([]any)
		if !ok {
			add("type mismatch")
			return
		}
		if len(reference) != len(actualList) {
			add("array length mismatch")
		}
		for index := 0; index < len(reference) && index < len(actualList); index++ {
			compare(reference[index], actualList[index], fmt.Sprintf("%s[%d]", path, index), tolerance, differences)
		}
	case float64:
		number, ok := actual.(float64)
		if !ok || math.Abs(reference-number) > tolerance {
			add("numeric mismatch")
		}
	default:
		if reference != actual {
			add("value mismatch")
		}
	}
}

func readSnapshot(path string) ([]any, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if len(data) > 16*1024*1024 {
		return nil, errors.New("snapshot exceeds 16 MiB")
	}
	var value any
	if err := json.Unmarshal(data, &value); err != nil {
		return nil, err
	}
	list, err := objects(value)
	if err != nil {
		return nil, err
	}
	return normalize(list)
}

func run() (bool, error) {
	referencePath := flag.String("reference", "", "Editor shared snapshot JSON")
	actualPath := flag.String("actual", "", "Player shared snapshot JSON or MCP/runtime response")
	output := flag.String("output", "", "optional comparison report JSON")
	tolerance := flag.Float64("tolerance", 1e-5, "absolute floating-point tolerance")
	flag.Parse()
	if *referencePath == "" || *actualPath == "" || *tolerance < 0 || math.IsNaN(*tolerance) || math.IsInf(*tolerance, 0) {
		return false, errors.New("provide reference, actual and a finite nonnegative tolerance")
	}
	reference, err := readSnapshot(*referencePath)
	if err != nil {
		return false, fmt.Errorf("reference: %w", err)
	}
	actual, err := readSnapshot(*actualPath)
	if err != nil {
		return false, fmt.Errorf("actual: %w", err)
	}
	differences := []difference{}
	compare(reference, actual, "objects", *tolerance, &differences)
	success := len(differences) == 0
	report := map[string]any{"schemaVersion": 1, "success": success, "reference": *referencePath, "actual": *actualPath, "referenceObjects": len(reference), "actualObjects": len(actual), "absoluteTolerance": *tolerance,
		"normalizations": []string{"object/component source identities", "Editor globalObjectId and prefabInstance identity markers", "reserved RawSceneResources owner", "quaternion q and -q represent the same rotation"}, "differences": differences}
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return false, err
	}
	data = append(data, '\n')
	if *output != "" {
		if err := os.WriteFile(*output, data, 0600); err != nil {
			return false, err
		}
	}
	_, err = os.Stdout.Write(data)
	return success, err
}

func main() {
	success, err := run()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	if !success {
		os.Exit(1)
	}
}
