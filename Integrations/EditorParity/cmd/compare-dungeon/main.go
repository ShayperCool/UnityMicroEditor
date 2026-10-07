// compare-dungeon compares only the three authored ShipGenDemo roots. Full
// public-field differences remain the responsibility of cmd/compare.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"strconv"
	"strings"
)

var authoredPaths = []string{"Ship Bootstrap", "Global Volume", "Overview Camera"}

type difference struct {
	Path      string `json:"path"`
	Reference any    `json:"reference"`
	Actual    any    `json:"actual"`
}

func decode(data []byte) (map[string]any, error) {
	var result map[string]any
	d := json.NewDecoder(bytes.NewReader(data))
	d.UseNumber()
	if err := d.Decode(&result); err != nil {
		return nil, err
	}
	return result, nil
}
func objectList(value map[string]any) ([]any, error) {
	if value["success"] == false || value["isError"] == true || value["error"] != nil {
		return nil, errors.New("snapshot operation failed")
	}
	if values, ok := value["objects"].([]any); ok {
		if value["nextOffset"] != nil {
			return nil, errors.New("provide complete source/runtime data")
		}
		return values, nil
	}
	if _, ok := value["path"].(string); ok {
		return []any{value}, nil
	}
	for _, key := range []string{"result", "structuredContent", "data"} {
		if child, ok := value[key].(map[string]any); ok {
			if values, err := objectList(child); err == nil {
				return values, nil
			}
		}
	}
	return nil, errors.New("no objects found")
}
func number(value any) (float64, bool) {
	n, err := strconv.ParseFloat(fmt.Sprint(value), 64)
	return n, err == nil && !math.IsNaN(n) && !math.IsInf(n, 0)
}
func equal(a, b any, path string) bool {
	if am, ok := a.(map[string]any); ok {
		bm, ok := b.(map[string]any)
		if !ok || len(am) != len(bm) {
			return false
		}
		if path == "rotation" || path == "localRotation" {
			for _, sign := range []float64{1, -1} {
				match := true
				for _, key := range []string{"x", "y", "z", "w"} {
					av, aok := number(am[key])
					bv, bok := number(bm[key])
					if !aok || !bok || math.Abs(av-sign*bv) > 1e-5 {
						match = false
					}
				}
				if match {
					return true
				}
			}
			return false
		}
		for key, value := range am {
			if !equal(value, bm[key], key) {
				return false
			}
		}
		return true
	}
	if _, ok := a.(json.Number); ok {
		av, aok := number(a)
		bv, bok := number(b)
		return aok && bok && math.Abs(av-bv) <= 1e-5
	}
	return a == b
}
func selected(values []any, key string) (map[string]map[string]any, error) {
	out := map[string]map[string]any{}
	for _, item := range values {
		value, ok := item.(map[string]any)
		if !ok {
			return nil, errors.New("invalid object")
		}
		path, _ := value[key].(string)
		for _, expected := range authoredPaths {
			if path == expected {
				if out[path] != nil {
					return nil, fmt.Errorf("ambiguous selected object %s", path)
				}
				out[path] = value
			}
		}
	}
	if len(out) != len(authoredPaths) {
		return nil, errors.New("the three explicit authored roots are required")
	}
	return out, nil
}
func compare(reference, actual, source []any) ([]difference, error) {
	r, err := selected(reference, "path")
	if err != nil {
		return nil, err
	}
	a, err := selected(actual, "path")
	if err != nil {
		return nil, err
	}
	s, err := selected(source, "name")
	if err != nil {
		return nil, err
	}
	diffs := []difference{}
	add := func(path string, x, y any) { diffs = append(diffs, difference{path, x, y}) }
	for _, path := range authoredPaths {
		for _, key := range []string{"activeSelf", "activeInHierarchy", "siblingIndex", "position", "localPosition", "rotation", "localRotation", "localScale"} {
			if _, exists := r[path][key]; !exists {
				return nil, fmt.Errorf("native reference %s missing %s", path, key)
			}
			if _, exists := a[path][key]; !exists {
				return nil, fmt.Errorf("runtime snapshot %s missing %s", path, key)
			}
			if !equal(r[path][key], a[path][key], key) {
				add(path+"."+key, r[path][key], a[path][key])
			}
		}
		rc, rok := r[path]["components"].([]any)
		ac, aok := a[path]["components"].([]any)
		sc, sok := s[path]["components"].([]any)
		if !rok || !aok || !sok {
			return nil, errors.New("components missing")
		}
		if len(rc) != len(ac) {
			add(path+".componentCount", len(rc), len(ac))
		}
		if len(rc) != len(sc) {
			add(path+".sourceComponentCount", len(rc), len(sc))
		}
		for index, item := range rc {
			rv, ok := item.(map[string]any)
			if !ok {
				return nil, errors.New("invalid reference component")
			}
			label := fmt.Sprintf("%s.components[%d]", path, index)
			if index < len(ac) {
				av, ok := ac[index].(map[string]any)
				if !ok {
					return nil, errors.New("invalid runtime component")
				}
				if rv["type"] != av["type"] {
					add(label+".type", rv["type"], av["type"])
				}
			}
			guid, _ := rv["scriptGuid"].(string)
			if index < len(sc) {
				sv, ok := sc[index].(map[string]any)
				if !ok {
					return nil, errors.New("invalid source component")
				}
				fields, _ := sv["fields"].(map[string]any)
				if fields == nil {
					return nil, errors.New("source comparison requires includeFields=true")
				}
				script, _ := fields["m_Script"].(map[string]any)
				sourceGUID, _ := script["guid"].(string)
				if guid != sourceGUID {
					add(label+".scriptGuid", guid, script["guid"])
				}
			}
		}
	}
	return diffs, nil
}
func compareSource(reference, source []any) ([]difference, error) {
	r, err := selected(reference, "path")
	if err != nil {
		return nil, err
	}
	s, err := selected(source, "name")
	if err != nil {
		return nil, err
	}
	diffs := []difference{}
	add := func(path string, x, y any) { diffs = append(diffs, difference{path, x, y}) }
	for _, path := range authoredPaths {
		if _, ok := s[path]["active"].(bool); !ok {
			return nil, fmt.Errorf("source %s missing active state", path)
		}
		if !equal(r[path]["activeSelf"], s[path]["active"], "activeSelf") {
			add(path+".activeSelf", r[path]["activeSelf"], s[path]["active"])
		}
		if fmt.Sprint(s[path]["parentId"]) != "0" {
			add(path+".parentId", "0", s[path]["parentId"])
		}
		rc, rok := r[path]["components"].([]any)
		sc, sok := s[path]["components"].([]any)
		if !rok || !sok {
			return nil, errors.New("components missing")
		}
		if len(rc) != len(sc) {
			add(path+".componentCount", len(rc), len(sc))
		}
		for index, item := range rc {
			if index >= len(sc) {
				continue
			}
			rv, rok := item.(map[string]any)
			sv, sok := sc[index].(map[string]any)
			if !rok || !sok {
				return nil, errors.New("invalid component")
			}
			fields, _ := sv["fields"].(map[string]any)
			if fields == nil {
				return nil, errors.New("source comparison requires includeFields=true")
			}
			label := fmt.Sprintf("%s.components[%d]", path, index)
			guid, _ := rv["scriptGuid"].(string)
			script, _ := fields["m_Script"].(map[string]any)
			sourceGUID, _ := script["guid"].(string)
			if guid != sourceGUID {
				add(label+".scriptGuid", guid, sourceGUID)
			}
			if guid == "" {
				if strings.TrimPrefix(fmt.Sprint(rv["type"]), "UnityEngine.") != sv["type"] {
					add(label+".nativeType", rv["type"], sv["type"])
				}
			} else if sv["type"] != "MonoBehaviour" {
				add(label+".sourceType", "MonoBehaviour", sv["type"])
			}
			if rv["type"] == "UnityEngine.Transform" {
				for _, pair := range [][2]string{{"localPosition", "m_LocalPosition"}, {"localRotation", "m_LocalRotation"}, {"localScale", "m_LocalScale"}} {
					if r[path][pair[0]] == nil || fields[pair[1]] == nil {
						return nil, fmt.Errorf("%s missing transform field %s", path, pair[0])
					}
					if !equal(r[path][pair[0]], fields[pair[1]], pair[0]) {
						add(path+"."+pair[0], r[path][pair[0]], fields[pair[1]])
					}
				}
			}
		}
	}
	return diffs, nil
}
func read(path string) ([]any, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	value, err := decode(data)
	if err != nil {
		return nil, err
	}
	return objectList(value)
}
func run() error {
	reference := flag.String("reference", "", "native Editor play.json")
	actual := flag.String("actual", "", "three selected runtime snapshots, objects envelope")
	source := flag.String("source", "", "complete scene_hierarchy includeFields=true resolvePrefabs=true receipt")
	sourceOnly := flag.Bool("source-only", false, "compare saved source to native authoring snapshot; does not compare runtime or root enumeration order")
	output := flag.String("output", "", "comparison report JSON")
	flag.Parse()
	if *reference == "" || (!*sourceOnly && *actual == "") || *source == "" || *output == "" {
		return errors.New("provide reference, actual, source and output")
	}
	r, err := read(*reference)
	if err != nil {
		return err
	}
	s, err := read(*source)
	if err != nil {
		return err
	}
	var differences []difference
	if *sourceOnly {
		differences, err = compareSource(r, s)
	} else {
		a, readErr := read(*actual)
		if readErr != nil {
			return readErr
		}
		differences, err = compare(r, a, s)
	}
	if err != nil {
		return err
	}
	report := map[string]any{"schemaVersion": 1, "success": len(differences) == 0, "reference": *reference, "actual": *actual, "source": *source, "objects": 3, "differences": differences, "scope": "Three explicitly named authored roots, active state/transforms/component order/script GUIDs. Generated world and all component fields require separate comparison.", "absoluteTolerance": 1e-5}
	if *sourceOnly {
		report["scope"] = "Saved-source comparison with native Edit Mode authoring: three named roots, root parent, active state, local transforms, component count/order/native types/script GUIDs. Does not compare runtime, root enumeration order or all serialized fields."
		report["mode"] = "source_only"
		delete(report, "actual")
	}
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return err
	}
	if err = os.WriteFile(*output, append(data, '\n'), 0600); err != nil {
		return err
	}
	if len(differences) != 0 {
		return fmt.Errorf("%d authored-root differences", len(differences))
	}
	fmt.Println("Three authored roots match within the declared comparison scope")
	return nil
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
