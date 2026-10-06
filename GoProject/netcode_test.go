package goproject

import (
	"reflect"
	"testing"
)

func TestXXHash32MatchesKnownVectors(t *testing.T) {
	for _, fixture := range []struct {
		text string
		hash uint32
	}{
		{"", 0x02cc5d05}, {"a", 0x550d7456}, {"abc", 0x32d153ff},
		// Independently evaluated by the installed NGO package's XXHash.cs,
		// compiled with the Unity SDK and executed by its standalone Mono.
		{"GlobalObjectId_V1-3-11111111111111111111111111111111-55-0", 0x89706b6f},
		{"GlobalObjectId_V1-3-d3492bcf1044c4bbc8cb697713855d96-1979069788112063146-0", 0x637d7bc2},
	} {
		if actual := xxHash32([]byte(fixture.text)); actual != fixture.hash {
			t.Fatalf("xxhash32 %q: got %08x expected %08x", fixture.text, actual, fixture.hash)
		}
	}
}

func TestNetworkPrefabDerivedIdentityIsStableAndLeavesAuthoredDataUntouched(t *testing.T) {
	source := map[string]any{"GlobalObjectIdHash": int64(0), "SomeSetting": true}
	record := &expandedObject{Document: Document{FileID: "41:55"}, SourcePath: "Assets/Net/Player.prefab", GUID: fixtureGUID, Targets: map[string]bool{targetKey(fixtureGUID, "55"): true}}
	first, err := derivedNetworkData(record, "Unity.Netcode.NetworkObject", source)
	if err != nil || number(first["GlobalObjectIdHash"]) == 0 {
		t.Fatalf("identity unavailable %v %v", first, err)
	}
	record.FileID = "999:55"
	second, err := derivedNetworkData(record, "Unity.Netcode.NetworkObject", source)
	if err != nil || !reflect.DeepEqual(first, second) {
		t.Fatalf("same source prefab acquired instance-dependent identity: %v %v", second, err)
	}
	if source["GlobalObjectIdHash"] != int64(0) || first["SomeSetting"] != true {
		t.Fatal("authored source or unrelated settings changed")
	}
	existing := map[string]any{"GlobalObjectIdHash": int64(123)}
	retained, err := derivedNetworkData(record, "Unity.Netcode.NetworkObject", existing)
	if err != nil || !reflect.DeepEqual(retained, existing) {
		t.Fatal("replaced an already validated source hash")
	}
	record.SourcePath = "Assets/Net/Boot.unity"
	if _, err := derivedNetworkData(record, "Unity.Netcode.NetworkObject", source); err == nil {
		t.Fatal("silently invented an in-scene identity")
	}
}
