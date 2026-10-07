package main

import (
	"encoding/json"
	"testing"
)

func decode(t *testing.T, text string) any {
	t.Helper()
	var value any
	if err := json.Unmarshal([]byte(text), &value); err != nil {
		t.Fatal(err)
	}
	return value
}

func TestNormalizePreservesSerializedSourceAndUnsupportedComponents(t *testing.T) {
	value := decode(t, `[{"path":"Root","source":{"fileId":"1"},"globalObjectId":"abc","prefabInstance":true,"components":[{"type":"Game.Probe","source":{"fileId":"2"},"fields":{"serialized":{"source":42}}},{"type":"MissingScript"}]}]`).([]any)
	list, err := normalize(value)
	if err != nil {
		t.Fatal(err)
	}
	object := list[0].(map[string]any)
	if object["source"] != nil || object["globalObjectId"] != nil {
		t.Fatal("source identity retained")
	}
	components := object["components"].([]any)
	serialized := components[0].(map[string]any)["fields"].(map[string]any)["serialized"].(map[string]any)
	if serialized["source"] != float64(42) || len(components) != 2 {
		t.Fatal("game data or unsupported component was discarded")
	}
}

func TestComparisonChecksLifecycleAndComponentOrder(t *testing.T) {
	a := decode(t, `{"components":[{"type":"Transform"},{"type":"Probe","fields":{"awakeCount":1}}]}`)
	for _, text := range []string{`{"components":[{"type":"Probe","fields":{"awakeCount":1}},{"type":"Transform"}]}`, `{"components":[{"type":"Transform"},{"type":"Probe","fields":{"awakeCount":2}}]}`} {
		differences := []difference{}
		compare(a, decode(t, text), "objects[0]", 1e-5, &differences)
		if len(differences) == 0 {
			t.Fatal("behavior difference accepted")
		}
	}
}

func TestQuaternionSignEquivalenceAndAbsoluteTolerance(t *testing.T) {
	a := decode(t, `{"rotation":{"x":0,"y":0.5,"z":0,"w":0.8660254},"position":{"x":100000}}`)
	b := decode(t, `{"rotation":{"x":0,"y":-0.5,"z":0,"w":-0.8660254},"position":{"x":100000.000001}}`)
	differences := []difference{}
	compare(a, b, "object", 1e-5, &differences)
	if len(differences) != 0 {
		t.Fatal(differences)
	}
	c := decode(t, `{"rotation":{"x":0,"y":-0.5,"z":0,"w":-0.8660254},"position":{"x":100000.01}}`)
	compare(a, c, "object", 1e-5, &differences)
	if len(differences) != 1 {
		t.Fatal("relative tolerance hid position difference")
	}
}

func TestRejectsErrorsPartialAndDuplicateSnapshots(t *testing.T) {
	for _, text := range []string{`{"success":false,"objects":[]}`, `{"isError":true,"structuredContent":{"objects":[]}}`, `{"result":{"objects":[],"total":2}}`, `{"objects":[],"nextOffset":50}`} {
		if _, err := objects(decode(t, text)); err == nil {
			t.Fatalf("accepted %s", text)
		}
	}
	if _, err := normalize(decode(t, `[{"path":"A","components":[]},{"path":"A","components":[]}]`).([]any)); err == nil {
		t.Fatal("duplicate paths accepted")
	}
}
