package main

import (
	"encoding/json"
	"testing"
)

func fixture() ([]any, []any, []any) {
	var reference, actual, source []any
	for _, path := range authoredPaths {
		r := map[string]any{"path": path, "activeSelf": true, "activeInHierarchy": true, "siblingIndex": json.Number("0"), "position": map[string]any{"x": json.Number("0")}, "components": []any{map[string]any{"type": "Game.Probe", "scriptGuid": "expected"}}}
		a := map[string]any{"path": path, "activeSelf": true, "activeInHierarchy": true, "siblingIndex": json.Number("0"), "position": map[string]any{"x": json.Number("0")}, "components": []any{map[string]any{"type": "Game.Probe"}}}
		for _, key := range []string{"localPosition", "rotation", "localRotation", "localScale"} {
			r[key] = map[string]any{"x": json.Number("0"), "y": json.Number("0"), "z": json.Number("0"), "w": json.Number("1")}
			a[key] = map[string]any{"x": json.Number("0"), "y": json.Number("0"), "z": json.Number("0"), "w": json.Number("1")}
		}
		s := map[string]any{"name": path, "components": []any{map[string]any{"fields": map[string]any{"m_Script": map[string]any{"guid": "expected"}}}}}
		reference = append(reference, r)
		actual = append(actual, a)
		source = append(source, s)
	}
	return reference, actual, source
}
func TestOnlyExplicitRootsAreCompared(t *testing.T) {
	r, a, s := fixture()
	a = append(a, map[string]any{"path": "Generated Ship random 73945"})
	diffs, err := compare(r, a, s)
	if err != nil || len(diffs) != 0 {
		t.Fatalf("diffs=%v err=%v", diffs, err)
	}
}
func TestOrderScriptGuidAndPoseMismatchesRemainVisible(t *testing.T) {
	r, a, s := fixture()
	a[0].(map[string]any)["components"].([]any)[0].(map[string]any)["type"] = "Wrong.Probe"
	a[1].(map[string]any)["position"].(map[string]any)["x"] = json.Number("1")
	s[2].(map[string]any)["components"].([]any)[0].(map[string]any)["fields"].(map[string]any)["m_Script"].(map[string]any)["guid"] = "wrong"
	diffs, err := compare(r, a, s)
	if err != nil || len(diffs) != 3 {
		t.Fatalf("diffs=%v err=%v", diffs, err)
	}
}
func TestMissingOrAmbiguousNamedRootsAreRejected(t *testing.T) {
	r, a, s := fixture()
	if _, err := compare(r, a[:2], s); err == nil {
		t.Fatal("missing root accepted")
	}
	if _, err := compare(r, append(a, a[0]), s); err == nil {
		t.Fatal("ambiguous root accepted")
	}
}
func TestFailureEnvelopeCannotBeUsedAsComparisonData(t *testing.T) {
	value, err := decode([]byte(`{"success":false,"objects":[]}`))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := objectList(value); err == nil {
		t.Fatal("failed source operation accepted")
	}
}

func sourceFixture() ([]any, []any) {
	var reference, source []any
	for _, path := range authoredPaths {
		position := map[string]any{"x": json.Number("0"), "y": json.Number("0"), "z": json.Number("0")}
		rotation := map[string]any{"x": json.Number("0"), "y": json.Number("0"), "z": json.Number("0"), "w": json.Number("1")}
		scale := map[string]any{"x": json.Number("1"), "y": json.Number("1"), "z": json.Number("1")}
		reference = append(reference, map[string]any{"path": path, "activeSelf": true, "localPosition": position, "localRotation": rotation, "localScale": scale, "components": []any{map[string]any{"type": "UnityEngine.Transform", "scriptGuid": ""}, map[string]any{"type": "Game.Probe", "scriptGuid": "expected"}}})
		source = append(source, map[string]any{"name": path, "active": true, "parentId": "0", "components": []any{map[string]any{"type": "Transform", "fields": map[string]any{"m_LocalPosition": position, "m_LocalRotation": rotation, "m_LocalScale": scale}}, map[string]any{"type": "MonoBehaviour", "fields": map[string]any{"m_Script": map[string]any{"guid": "expected"}}}}})
	}
	return reference, source
}
func TestSourceComparisonDoesNotInferRootOrderFromFileIDSort(t *testing.T) {
	r, s := sourceFixture()
	s[0], s[2] = s[2], s[0]
	diffs, err := compareSource(r, s)
	if err != nil || len(diffs) != 0 {
		t.Fatalf("diffs=%v err=%v", diffs, err)
	}
}
func TestSourceActiveParentPoseAndComponentChangesRemainVisible(t *testing.T) {
	r, s := sourceFixture()
	s[0].(map[string]any)["active"] = false
	s[1].(map[string]any)["parentId"] = "99"
	s[2].(map[string]any)["components"].([]any)[0].(map[string]any)["fields"].(map[string]any)["m_LocalPosition"] = map[string]any{"x": json.Number("2"), "y": json.Number("0"), "z": json.Number("0")}
	s[2].(map[string]any)["components"].([]any)[1].(map[string]any)["fields"].(map[string]any)["m_Script"].(map[string]any)["guid"] = "different"
	diffs, err := compareSource(r, s)
	if err != nil || len(diffs) != 4 {
		t.Fatalf("diffs=%v err=%v", diffs, err)
	}
}
