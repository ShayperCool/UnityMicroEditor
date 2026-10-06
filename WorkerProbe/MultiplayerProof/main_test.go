package main

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func readyPeers() (snapshot, snapshot) {
	request := sortie{Archetype: 3, Size: 1, ShipSeed: 1234, ScenarioSeed: 5678, Crew: 2, Generation: 1}
	members := []member{{ClientID: 0, Name: "Host", Slot: 0, InShip: true}, {ClientID: 1, Name: "Guest", Slot: 1, InShip: true}}
	mesh := geometry{MeshRenderers: 3, SupportedShaderRenderers: 3, ActiveSolidColliders: 1, Vertices: 120, Indices: 180}
	host := snapshot{
		Schema: 1, PID: 101, Sequence: 20, Ready: true, SessionFound: true, ShipFound: true, ContentReady: true,
		IsServer: true, Listening: true, ConnectedClient: true, ShipAuthority: true, Current: "InShip", Phase: "InShip",
		RoomCode: "A1B2C3",
		Sortie:   request, ShipSeed: 1234, ScenarioSeed: 5678, Generation: 1, Rooms: 12, Zones: 4, Interactables: 30, CrewCount: 2,
		ShipRenderers: 100, ShipColliders: 80, ShipTransforms: 220, ZonePower: []bool{true, false, true, true},
		ShipGeometry: mesh,
		LayoutSHA256: "layout", ScenarioSHA256: "scenario", Members: append([]member{}, members...),
		Players: []player{{OwnerClientID: 0, NetworkObjectID: 8, IsOwner: true, Spawned: true, Alive: true, Position: vector{X: 1}, Room: 0},
			{OwnerClientID: 1, NetworkObjectID: 9, Spawned: true, Alive: true, Position: vector{X: 2}, Room: 0, Description: "Crew 1 owner=False samples=5 lag=0.1s"}},
	}
	for index := range host.Players {
		host.Players[index].Geometry = mesh
		host.Players[index].ShipFloorSupported = true
		host.Players[index].ShipFloorCollider = "Airlock Floor"
		host.Players[index].ShipFloorDistance = .4
	}
	client := host
	client.PID = 202
	client.LocalClientID = 1
	client.IsServer, client.ShipAuthority = false, false
	client.Members = append([]member{}, members...)
	client.Players = append([]player{}, host.Players...)
	client.Players[0].IsOwner = false
	client.Players[0].Description = "Crew 0 owner=False samples=6 lag=0.1s"
	client.Players[1].IsOwner = true
	return host, client
}

func TestPublicEvidenceRejectsFalseMultiplayerSuccess(t *testing.T) {
	if host, client := readyPeers(); validatePair(host, client) != nil {
		t.Fatal("Independent host/client fixture should pass")
	}
	tests := map[string]func(*snapshot, *snapshot){
		"different layout":           func(_, c *snapshot) { c.LayoutSHA256 = "other" },
		"wrong scenario":             func(_, c *snapshot) { c.ScenarioSeed++ },
		"only one avatar":            func(_, c *snapshot) { c.Players = c.Players[:1] },
		"wrong ownership":            func(_, c *snapshot) { c.Players[1].IsOwner = false },
		"different object":           func(_, c *snapshot) { c.Players[1].NetworkObjectID++ },
		"no state traffic":           func(_, c *snapshot) { c.Players[0].Description = "Crew 0 samples=0" },
		"avatar outside":             func(_, c *snapshot) { c.Players[1].Room = -1 },
		"avatar down":                func(h, c *snapshot) { h.Members[1].Down = true; c.Members[1].Down = true },
		"runtime error":              func(_, c *snapshot) { c.Errors++ },
		"disconnected":               func(_, c *snapshot) { c.ConnectedClient = false },
		"far remote pose":            func(_, c *snapshot) { c.Players[0].Position.X += 5 },
		"duplicated owner":           func(h, c *snapshot) { h.Players[1].OwnerClientID = 0; c.Players[1].OwnerClientID = 0 },
		"empty ship mesh":            func(_, c *snapshot) { c.ShipGeometry.Indices = 0 },
		"disabled ship physics":      func(_, c *snapshot) { c.ShipGeometry.ActiveSolidColliders = 0 },
		"unsupported ship materials": func(_, c *snapshot) { c.ShipGeometry.SupportedShaderRenderers = 0 },
		"empty avatar mesh":          func(_, c *snapshot) { c.Players[1].Geometry.Vertices = 0 },
		"missing avatar physics":     func(_, c *snapshot) { c.Players[1].Geometry.ActiveSolidColliders = 0 },
		"no physical floor":          func(_, c *snapshot) { c.Players[1].ShipFloorSupported = false },
		"invalid room index":         func(_, c *snapshot) { c.Players[1].Room = c.Rooms },
		"nonfinite avatar":           func(_, c *snapshot) { c.Players[1].Position.Y = math.NaN() },
		"nonfinite floor distance":   func(_, c *snapshot) { c.Players[1].ShipFloorDistance = math.Inf(1) },
		"different relay room":       func(_, c *snapshot) { c.RoomCode = "AABBCC" },
		"duplicate network object": func(h, c *snapshot) {
			h.Players[1].NetworkObjectID = h.Players[0].NetworkObjectID
			c.Players[1].NetworkObjectID = c.Players[0].NetworkObjectID
		},
	}
	for name, corrupt := range tests {
		t.Run(name, func(t *testing.T) {
			host, client := readyPeers()
			corrupt(&host, &client)
			if err := validatePair(host, client); err == nil {
				t.Fatal("Incomplete or inconsistent evidence was accepted")
			}
		})
	}
}

func TestReadsCSharpSnapshotFieldNamesAndRejectsStaleReports(t *testing.T) {
	// C# JsonUtility uses these lower camel case names, including ID/SHA fields.
	data := []byte(`{"schema":1,"pid":123,"localClientId":7,"shipStateApplications":4,"layoutSHA256":"abc","sortie":{"archetype":3,"size":2,"shipSeed":-2147483648,"scenarioSeed":2147483647,"crew":2,"generation":1},"players":[{"ownerClientId":7,"networkObjectId":44,"isOwner":true,"position":{"x":1,"y":2,"z":3}}],"doors":[{"doorId":8,"syncState":2,"canUseLocal":true,"autoCloseDistance":5,"nearestCrewDistance":50.25}]}`)
	path := filepath.Join(t.TempDir(), "snapshot.json")
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	peer, err := readSnapshot(path)
	if err != nil || peer.LocalClientID != 7 || peer.LayoutSHA256 != "abc" || peer.ShipStateApplications != 4 || peer.Players[0].NetworkObjectID != 44 || peer.Players[0].Position.Y != 2 || peer.Doors[0].DoorID != 8 || !peer.Doors[0].CanUseLocal {
		t.Fatalf("C# snapshot fields were not recovered: %#v, %v", peer, err)
	}
	if peer.Sortie != (sortie{Archetype: 3, Size: 2, ShipSeed: math.MinInt32, ScenarioSeed: math.MaxInt32, Crew: 2, Generation: 1}) {
		t.Fatalf("Serializable sortie DTO values were not recovered: %+v", peer.Sortie)
	}
	if peer.Doors[0].AutoCloseDistance != 5 || peer.Doors[0].NearestCrewDistance != 50.25 {
		t.Fatalf("Public door auto-close readback values were not recovered: %+v", peer.Doors[0])
	}
	old := time.Now().Add(-5 * time.Second)
	if err := os.Chtimes(path, old, old); err != nil {
		t.Fatal(err)
	}
	if _, err := readSnapshot(path); err == nil {
		t.Fatal("Stale snapshot was accepted")
	}
}

func TestCommandFileContainsActualJsonNumbersAndSeparateFields(t *testing.T) {
	path := filepath.Join(t.TempDir(), "command.json")
	if err := command(path, 5, "use-door", 0, 12); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil || decoded["sequence"] != float64(5) || decoded["door"] != float64(12) || decoded["action"] != "use-door" {
		t.Fatalf("Invalid command payload: %s, %v", data, err)
	}
}

func TestMotionRequiresHorizontalTransport(t *testing.T) {
	if motionReached(vector{}, vector{}, vector{Y: 1}, vector{Y: 1}) {
		t.Fatal("Gravity or a stationary jump must not prove walking motion")
	}
	if !motionReached(vector{}, vector{}, vector{X: 1}, vector{X: .8}) {
		t.Fatal("Actual client walk received on host must pass")
	}
	if motionReached(vector{}, vector{}, vector{X: 1}, vector{}) {
		t.Fatal("Client motion without remote transport must reject")
	}
	if motionReached(vector{}, vector{}, vector{X: 1}, vector{X: 5}) {
		t.Fatal("Divergent remote movement must reject")
	}
	if motionReached(vector{}, vector{}, vector{X: math.NaN()}, vector{X: 1}) {
		t.Fatal("Nonfinite motion must reject")
	}
}

func TestExerciseSelectsClosedDoor(t *testing.T) {
	usable := func(id int) door {
		return door{DoorID: id, CanUseLocal: true, Prompt: "Open door", Kind: "Standard", Powered: true, AutoCloseDistance: 5, NearestCrewDistance: 3}
	}
	requested, cranked, visuallyOpen := usable(1), usable(2), usable(3)
	requested.SyncState, cranked.SyncState, visuallyOpen.OpenAmount = 1, 2, .9
	closed, unpowered := usable(4), usable(5)
	unpowered.Powered = false
	selected, ok := selectClosedDoor([]door{requested, cranked, visuallyOpen, closed, unpowered})
	if !ok || selected.DoorID != 5 {
		t.Fatal("Exercise must prefer a closed manual door and avoid toggling open doors shut")
	}
	if _, ok := selectClosedDoor([]door{requested, cranked, visuallyOpen}); ok {
		t.Fatal("Already-open doors must not satisfy an opening exercise")
	}
}

func TestDoorExerciseRespectsAuthoredAutoCloseDistance(t *testing.T) {
	usable := func(id int, distance float64) door {
		return door{DoorID: id, CanUseLocal: true, Prompt: "Open door", Kind: "Standard", Powered: true, AutoCloseDistance: 5, NearestCrewDistance: distance}
	}
	far, near, nearest := usable(0, 50), usable(9, 4), usable(11, 2)
	selected, ok := selectClosedDoor([]door{far, near, nearest})
	if !ok || selected.DoorID != nearest.DoorID {
		t.Fatal("Far powered doors auto-close immediately; exercise must choose the nearest eligible door")
	}
	boundary := usable(4, 5)
	if !doorCanRemainOpen(boundary) {
		t.Fatal("The source auto-close comparison is strictly greater; equal distance remains open")
	}
	boundary.NearestCrewDistance = math.Nextafter(5, math.Inf(1))
	if doorCanRemainOpen(boundary) {
		t.Fatal("Distance beyond the authored boundary must reject")
	}
	manual := far
	manual.Powered = false
	if !doorCanRemainOpen(manual) {
		t.Fatal("Unpowered sliding doors crank open without the request auto-close condition")
	}
	manual.Kind = "Bulkhead"
	if !doorCanRemainOpen(manual) {
		t.Fatal("An unpowered bulkhead uses the same permanent crank path")
	}
	manual.Kind = "Hatch"
	if doorCanRemainOpen(manual) {
		t.Fatal("An unpowered hatch toggles requested and still auto-closes when crew are far away")
	}
	for _, invalid := range []float64{-1, math.NaN(), math.Inf(1)} {
		near.NearestCrewDistance = invalid
		if doorCanRemainOpen(near) {
			t.Fatalf("Invalid crew distance %v must reject", invalid)
		}
	}
	near.NearestCrewDistance = 2
	for _, invalid := range []float64{-1, math.NaN(), math.Inf(1)} {
		near.AutoCloseDistance = invalid
		if doorCanRemainOpen(near) {
			t.Fatalf("Invalid powered auto-close distance %v must reject", invalid)
		}
	}
}

func TestDoorExerciseRequiresHostAutoCloseEligibility(t *testing.T) {
	clientDoor := door{DoorID: 7, CanUseLocal: true, Prompt: "Open door", Kind: "Standard", Powered: true, AutoCloseDistance: 5, NearestCrewDistance: 4.9}
	hostDoor := clientDoor
	hostDoor.NearestCrewDistance = 5.1
	if _, ok := selectClosedDoorPair(snapshot{Doors: []door{hostDoor}}, snapshot{Doors: []door{clientDoor}}); ok {
		t.Fatal("Client proximity does not override the authoritative host auto-close decision")
	}
	hostDoor.NearestCrewDistance = 4.8
	if selected, ok := selectClosedDoorPair(snapshot{Doors: []door{hostDoor}}, snapshot{Doors: []door{clientDoor}}); !ok || selected.DoorID != 7 {
		t.Fatal("A closed door eligible on both peers should be selected")
	}
	if _, ok := selectClosedDoorPair(snapshot{}, snapshot{Doors: []door{clientDoor}}); ok {
		t.Fatal("A client door without a corresponding authoritative object must reject")
	}
}

func TestStopBotRequiresAcknowledgement(t *testing.T) {
	peer := snapshot{CommandSequence: 7, CommandAction: "start-bot", CommandAccepted: true}
	if acknowledged(peer, 8, "stop-bot") == nil {
		t.Fatal("Published stop command without Player acknowledgement must reject")
	}
	peer.CommandSequence, peer.CommandAction = 8, "stop-bot"
	if acknowledged(peer, 8, "stop-bot") != nil {
		t.Fatal("Player stop acknowledgement should pass")
	}
	peer.CommandAccepted = false
	if acknowledged(peer, 8, "stop-bot") == nil {
		t.Fatal("Failed bot stop must reject")
	}
}
