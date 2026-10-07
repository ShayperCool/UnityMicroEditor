package goproject

import (
	"strings"
	"testing"
)

func TestUnityYAMLPreservesIdentitiesAndNumbers(t *testing.T) {
	input := "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &9223372036854775807\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: ABCDEF0123456789ABCDEF0123456789, type: 3}\n  value: 12934340311651418136\n  negative: -0.25\n  trueString: true\n"
	docs, err := ParseUnityYAML([]byte(input))
	if err != nil {
		t.Fatal(err)
	}
	if docs[0].FileID != "9223372036854775807" || fileID(docs[0].Data["m_Script"]) != "11500000" {
		t.Fatal("fileID lost precision")
	}
	if str(docs[0].Data["value"]) != "12934340311651418136" {
		t.Fatal("uint64 lost precision")
	}
	if object(docs[0].Data["m_Script"])["guid"] != "abcdef0123456789abcdef0123456789" {
		t.Fatal("GUID not normalized")
	}
	if docs[0].Data["trueString"] != "true" {
		t.Fatal("implicit boolean changed authored scalar")
	}
}

func TestUnityYAMLRejectsAmbiguousGraphs(t *testing.T) {
	for _, body := range []string{"  a: 1\n  a: 2\n", "  a: &shared [1]\n  b: *shared\n", "  a: .nan\n", "  a: !custom 1\n"} {
		if _, err := ParseUnityYAML([]byte("--- !u!1 &1\nGameObject:\n" + body)); err == nil {
			t.Fatalf("accepted unsupported body %q", body)
		}
	}
	if _, err := ParseUnityYAML([]byte("--- !u!1 &1\nGameObject: {}\n--- !u!4 &1\nTransform: {}\n")); err == nil {
		t.Fatal("accepted duplicate identity")
	}
	if _, err := ParseUnityYAML([]byte(strings.Repeat("x", maxTextBytes+1))); err == nil {
		t.Fatal("accepted oversized text")
	}
}
