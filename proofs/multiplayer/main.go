// This driver observes already launched Players. It never starts Unity or edits
// source content. The optional exercise uses the observer's public API commands.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

type vector struct{ X, Y, Z float64 }
type member struct {
	ClientID     uint64 `json:"clientId"`
	Name         string `json:"name"`
	Slot         int    `json:"slot"`
	InShip, Down bool
}
type player struct {
	OwnerClientID           uint64 `json:"ownerClientId"`
	NetworkObjectID         uint64 `json:"networkObjectId"`
	IsOwner, Spawned, Alive bool
	Position                vector
	Room                    int
	Description             string
	Geometry                geometry
	ShipFloorSupported      bool
	ShipFloorCollider       string
	ShipFloorDistance       float64
}
type geometry struct {
	MeshRenderers, SupportedShaderRenderers, UnsupportedShaderRenderers, ActiveSolidColliders int
	SkinnedMeshRenderers, BoundSkinnedMeshRenderers                                           int
	Vertices, Indices                                                                         int64
}
type door struct {
	DoorID, SyncState, InteractableIndex                         int
	Kind, Prompt                                                 string
	Powered, Locked, Jammed, ExternalControl, CanUseLocal        bool
	OpenAmount, HoldTime, AutoCloseDistance, NearestCrewDistance float64
}
type sortie struct {
	Archetype, Size, ShipSeed, ScenarioSeed, Crew, Generation int
}
type snapshot struct {
	Schema, PID, Sequence                                                                                   int
	ObservedAtSeconds                                                                                       float64
	Ready, SessionFound, ShipFound, ContentReady, IsServer, Listening, ConnectedClient, ShipAuthority       bool
	Current, SessionError, RoomCode, Phase                                                                  string
	LocalClientID                                                                                           uint64 `json:"localClientId"`
	Sortie                                                                                                  sortie
	ShipSeed, ScenarioSeed, Generation, Rooms, Zones, Width, Length, Decks, Cells, CrewCount, Interactables int
	ShipRenderers, ShipColliders, ShipTransforms                                                            int
	ShipGeometry                                                                                            geometry
	LayoutSHA256, ScenarioSHA256                                                                            string
	Errors, Exceptions, ShipStateApplications                                                               int
	Members                                                                                                 []member
	Players                                                                                                 []player
	Doors                                                                                                   []door
	ZonePower                                                                                               []bool
	CommandSequence                                                                                         int
	CommandAction, CommandError                                                                             string
	CommandAccepted                                                                                         bool
}

type paths struct{ host, client, output, hostCommand, clientCommand string }
type proof struct {
	Schema       int    `json:"schema"`
	Success      bool   `json:"success"`
	Error        string `json:"error,omitempty"`
	Host, Client snapshot
	Checks       []string       `json:"checks"`
	Exercise     *exerciseProof `json:"exercise,omitempty"`
}
type exerciseProof struct {
	Zone                                                                            int `json:"zone"`
	InitialPower, UpdatedPower, RestoredPower                                       bool
	ClientApplicationsBefore, ClientApplicationsAfter                               int
	DoorID, DoorStateBefore, DoorStateAfter                                         int
	DoorVisualOpenHost, DoorVisualOpenClient                                        float64
	ClientMovementMetres, HostObservedMovementMetres, FinalPositionDifferenceMetres float64
	PublicInteractionPath                                                           string `json:"publicInteractionPath"`
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
func run() error {
	if len(os.Args) > 1 && os.Args[1] == "instrumentation-request" {
		return writeInstrumentationRequest(os.Args[2:])
	}
	mode := "verify"
	arguments := os.Args[1:]
	if len(arguments) > 0 && !strings.HasPrefix(arguments[0], "-") {
		mode, arguments = arguments[0], arguments[1:]
	}
	flags := flag.NewFlagSet("multiplayer-proof", flag.ContinueOnError)
	var files paths
	flags.StringVar(&files.host, "host", "", "Host observer JSON snapshot")
	flags.StringVar(&files.client, "client", "", "Client observer JSON snapshot")
	flags.StringVar(&files.output, "output", "", "Ignored proof output JSON")
	flags.StringVar(&files.hostCommand, "host-command", "", "Host observer command JSON file")
	flags.StringVar(&files.clientCommand, "client-command", "", "Client observer command JSON file")
	deadline := flags.Duration("timeout", 90*time.Second, "Overall deadline")
	if err := flags.Parse(arguments); err != nil {
		return err
	}
	if files.host == "" || files.client == "" || files.output == "" {
		return errors.New("--host, --client and --output are required")
	}
	if mode != "verify" && mode != "exercise" {
		return errors.New("Mode must be verify or exercise")
	}
	ctx, cancel := context.WithTimeout(context.Background(), *deadline)
	defer cancel()
	result := proof{Schema: 1}
	host, client, err := waitPair(ctx, files, nil)
	result.Host, result.Client = host, client
	if err == nil {
		result.Checks = []string{"both live peers reached InShip", "same sortie seeds and canonical layout/scenario SHA-256", "both generated ship hierarchies contain active indexed meshes, supported materials and solid colliders", "two distinct members and two spawned living avatars with active geometry/colliders on both peers", "matching network object identities and complementary avatar ownership", "both remote avatars received state samples", "both players stand in generated ship rooms above raycast-confirmed ship floor colliders", "zero captured Unity errors/exceptions"}
		if mode == "exercise" {
			if files.hostCommand == "" || files.clientCommand == "" {
				err = errors.New("exercise requires --host-command and --client-command")
			} else {
				result.Exercise, err = exercise(ctx, files, host, client)
				if err == nil {
					result.Checks = append(result.Checks, "host sector power change and restoration reached client", "client door command reached host and replicated state/visuals", "client bot movement reached host while both avatars remained aboard")
				}
			}
		}
	}
	if lastHost, lastClient, readError := readPair(files); readError == nil {
		result.Host, result.Client = lastHost, lastClient
	}
	result.Success = err == nil
	if err != nil {
		result.Error = err.Error()
	}
	if writeError := writeJSON(files.output, result); writeError != nil {
		return writeError
	}
	if err != nil {
		return err
	}
	fmt.Printf("Multiplayer %s passed; host PID %d, client PID %d, ship %d, scenario %d, %d rooms. Proof: %s\n", mode, host.PID, client.PID, host.ShipSeed, host.ScenarioSeed, host.Rooms, files.output)
	return nil
}

func writeInstrumentationRequest(args []string) error {
	flags := flag.NewFlagSet("instrumentation-request", flag.ContinueOnError)
	managed := flags.String("managed", "", "Fresh Player Managed references")
	sources := flags.String("sources", "", "Helper source directory")
	output := flags.String("output", "", "Generated source directory")
	request := flags.String("request", "", "LifecycleCompiler request JSON")
	observerOnly := flags.Bool("observer-only", false, "Compile observer only")
	if err := flags.Parse(args); err != nil {
		return err
	}
	for _, path := range []string{*managed, *sources, *output, *request} {
		if !filepath.IsAbs(path) {
			return errors.New("Instrumentation paths must be absolute")
		}
	}
	references, err := filepath.Glob(filepath.Join(*managed, "*.dll"))
	if err != nil || len(references) == 0 {
		return errors.New("No Player Managed references")
	}
	filtered := references[:0]
	for _, reference := range references {
		if filepath.Base(reference) != "UnityPlay.MultiplayerProof.dll" {
			filtered = append(filtered, reference)
		}
	}
	references = filtered
	type sourceInput struct{ Path, OriginalPath, OutputPath string }
	inputs := []sourceInput{{Path: filepath.Join(*sources, "MultiplayerObserver.cs"), OriginalPath: filepath.Join(*sources, "MultiplayerObserver.cs"), OutputPath: filepath.Join(*output, "MultiplayerObserver.cs")}}
	if !*observerOnly {
		inputs = append(inputs, sourceInput{Path: filepath.Join(*sources, "RawContentProvider.cs"), OriginalPath: filepath.Join(*sources, "RawContentProvider.cs"), OutputPath: filepath.Join(*output, "RawContentProvider.cs")})
	}
	if err := os.MkdirAll(*output, 0700); err != nil {
		return err
	}
	return writeJSON(*request, map[string]any{"AssemblyName": "UnityPlay.MultiplayerProof", "References": references, "Defines": []string{}, "Sources": inputs, "MarkerPath": filepath.Join(*output, "LifecycleMarker.cs"), "AllowUnsafe": false})
}

func readSnapshot(path string) (snapshot, error) {
	var result snapshot
	info, err := os.Stat(path)
	if err != nil {
		return result, err
	}
	if time.Since(info.ModTime()) > 3*time.Second {
		return result, fmt.Errorf("Stale observer snapshot %s", path)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return result, err
	}
	err = json.Unmarshal(data, &result)
	return result, err
}
func readPair(files paths) (snapshot, snapshot, error) {
	host, err := readSnapshot(files.host)
	if err != nil {
		return host, snapshot{}, err
	}
	client, err := readSnapshot(files.client)
	return host, client, err
}
func waitPair(ctx context.Context, files paths, condition func(snapshot, snapshot) error) (snapshot, snapshot, error) {
	var host, client snapshot
	var last error
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		var err error
		host, client, err = readPair(files)
		if err == nil {
			err = validatePair(host, client)
		}
		if err == nil && condition != nil {
			err = condition(host, client)
		}
		if err == nil {
			return host, client, nil
		}
		last = err
		select {
		case <-ctx.Done():
			return host, client, fmt.Errorf("Multiplayer proof timeout: %w; last condition: %v", ctx.Err(), last)
		case <-ticker.C:
		}
	}
}

var samplesPattern = regexp.MustCompile(`\bsamples=([0-9]+)`)

func validatePair(host, client snapshot) error {
	for role, peer := range map[string]snapshot{"host": host, "client": client} {
		if peer.Schema != 1 || peer.PID <= 0 || !peer.Ready || !peer.SessionFound || !peer.ShipFound || !peer.ContentReady || !peer.Listening || !peer.ConnectedClient || peer.Current != "InShip" || peer.Phase != "InShip" {
			return fmt.Errorf("%s is not ready: current=%s ready=%t", role, peer.Current, peer.Ready)
		}
		if peer.Errors != 0 || peer.Exceptions != 0 || peer.SessionError != "" {
			return fmt.Errorf("%s reported runtime errors", role)
		}
		if len(peer.Members) != 2 || len(peer.Players) != 2 || peer.Rooms <= 0 || peer.Zones <= 0 || peer.Interactables <= 0 || peer.CrewCount != 2 || peer.ShipRenderers <= 0 || peer.ShipColliders <= 0 || peer.ShipTransforms <= 0 {
			return fmt.Errorf("%s lacks generated ship or both crew avatars", role)
		}
		if len(peer.ZonePower) != peer.Zones {
			return fmt.Errorf("%s lacks zone state readback", role)
		}
		if !geometryReady(peer.ShipGeometry) {
			return fmt.Errorf("%s lacks active ship mesh geometry, supported materials or solid colliders", role)
		}
		if peer.ShipSeed != peer.Sortie.ShipSeed || peer.ScenarioSeed != peer.Sortie.ScenarioSeed || peer.Generation != peer.Sortie.Generation || peer.Sortie.Crew != 2 {
			return fmt.Errorf("%s generated ship differs from its network sortie", role)
		}
		owners := 0
		for _, p := range peer.Players {
			if !p.Spawned || !p.Alive || p.Room < 0 || p.Room >= peer.Rooms || !finiteVector(p.Position) {
				return fmt.Errorf("%s avatar %d is unavailable or outside ship", role, p.OwnerClientID)
			}
			if !geometryReady(p.Geometry) || !p.ShipFloorSupported || p.ShipFloorCollider == "" || math.IsNaN(p.ShipFloorDistance) || math.IsInf(p.ShipFloorDistance, 0) || p.ShipFloorDistance < 0 || p.ShipFloorDistance > 2.35 {
				return fmt.Errorf("%s avatar %d lacks active geometry/colliders or a raycast-confirmed ship floor", role, p.OwnerClientID)
			}
			if p.IsOwner {
				owners++
				if p.OwnerClientID != peer.LocalClientID {
					return errors.New("Local ownership ID mismatch")
				}
			} else {
				match := samplesPattern.FindStringSubmatch(p.Description)
				if len(match) != 2 {
					return errors.New("Remote avatar sample count is unavailable")
				}
				count, _ := strconv.Atoi(match[1])
				if count == 0 {
					return errors.New("Remote avatar has not received motion state")
				}
			}
		}
		if owners != 1 {
			return fmt.Errorf("%s has %d local avatars", role, owners)
		}
	}
	if host.PID == client.PID || host.LocalClientID == client.LocalClientID || !host.IsServer || client.IsServer || !host.ShipAuthority || client.ShipAuthority {
		return errors.New("Peers do not have independent host/client authority")
	}
	if host.RoomCode == "" || host.RoomCode != client.RoomCode {
		return errors.New("Peers do not belong to the same relay room")
	}
	if host.Sortie != client.Sortie || host.ShipSeed != client.ShipSeed || host.ScenarioSeed != client.ScenarioSeed || host.LayoutSHA256 == "" || host.LayoutSHA256 != client.LayoutSHA256 || host.ScenarioSHA256 == "" || host.ScenarioSHA256 != client.ScenarioSHA256 {
		return errors.New("Generated ships/scenarios differ")
	}
	for index, p := range host.Players {
		q := client.Players[index]
		if p.OwnerClientID != q.OwnerClientID || p.NetworkObjectID != q.NetworkObjectID || p.IsOwner == q.IsOwner {
			return errors.New("Avatar network identities or peer ownership differ")
		}
		if distance(p.Position, q.Position) > 3 {
			return errors.New("Remote avatar position differs by more than 3 metres")
		}
		m, n := host.Members[index], client.Members[index]
		if m != n || !m.InShip || m.Down || m.ClientID != p.OwnerClientID {
			return errors.New("Crew membership or readiness differs")
		}
	}
	if host.Players[0].OwnerClientID == host.Players[1].OwnerClientID {
		return errors.New("Duplicate avatar owner")
	}
	if host.Players[0].NetworkObjectID == host.Players[1].NetworkObjectID {
		return errors.New("Duplicate avatar network object")
	}
	return nil
}

func exercise(ctx context.Context, files paths, initialHost, initialClient snapshot) (*exerciseProof, error) {
	proof := &exerciseProof{Zone: 0, ClientApplicationsBefore: initialClient.ShipStateApplications}
	if len(initialHost.ZonePower) == 0 || len(initialClient.ZonePower) == 0 {
		return proof, errors.New("No ship zones to exercise")
	}
	proof.InitialPower = initialHost.ZonePower[0]
	commandSequence := initialHost.CommandSequence + 1
	if err := command(files.hostCommand, commandSequence, "toggle-zone-power", 0, 0); err != nil {
		return proof, err
	}
	host, client, err := waitPair(ctx, files, func(h, c snapshot) error {
		if err := acknowledged(h, commandSequence, "toggle-zone-power"); err != nil {
			return err
		}
		if h.ZonePower[0] == proof.InitialPower || c.ZonePower[0] != h.ZonePower[0] || c.ShipStateApplications <= proof.ClientApplicationsBefore {
			return errors.New("Waiting for client to apply host power change")
		}
		return nil
	})
	if err != nil {
		return proof, err
	}
	proof.UpdatedPower, proof.ClientApplicationsAfter = host.ZonePower[0], client.ShipStateApplications
	commandSequence++
	if err := command(files.hostCommand, commandSequence, "toggle-zone-power", 0, 0); err != nil {
		return proof, err
	}
	host, client, err = waitPair(ctx, files, func(h, c snapshot) error {
		if err := acknowledged(h, commandSequence, "toggle-zone-power"); err != nil {
			return err
		}
		if h.ZonePower[0] != proof.InitialPower || c.ZonePower[0] != proof.InitialPower {
			return errors.New("Waiting for power restoration")
		}
		return nil
	})
	if err != nil {
		return proof, err
	}
	proof.RestoredPower = client.ZonePower[0]
	selected, selectedOK := selectClosedDoorPair(host, client)
	if !selectedOK {
		return proof, errors.New("No closed usable generated ship door that can remain open with the current crew positions")
	}
	proof.DoorID, proof.DoorStateBefore = selected.DoorID, selected.SyncState
	clientSequence := client.CommandSequence + 1
	if err := command(files.clientCommand, clientSequence, "use-door", 0, selected.DoorID); err != nil {
		return proof, err
	}
	host, client, err = waitPair(ctx, files, func(h, c snapshot) error {
		if err := acknowledged(c, clientSequence, "use-door"); err != nil {
			return err
		}
		a, aOK := doorByID(h, selected.DoorID)
		b, bOK := doorByID(c, selected.DoorID)
		if !aOK || !bOK || a.SyncState == selected.SyncState || b.SyncState != a.SyncState || a.OpenAmount < .8 || b.OpenAmount < .8 {
			return errors.New("Waiting for client door request and host/client open state")
		}
		return nil
	})
	if err != nil {
		return proof, err
	}
	a, _ := doorByID(host, selected.DoorID)
	b, _ := doorByID(client, selected.DoorID)
	proof.DoorStateAfter, proof.DoorVisualOpenHost, proof.DoorVisualOpenClient = a.SyncState, a.OpenAmount, b.OpenAmount
	proof.PublicInteractionPath = "client ship.Commands.Use -> SessionNetwork.UseRpc -> host ServerUse -> ShipDoor.Use -> replicated ship state"
	initialLocal, _ := playerByOwner(client, client.LocalClientID)
	initialRemote, _ := playerByOwner(host, client.LocalClientID)
	clientSequence++
	if err := command(files.clientCommand, clientSequence, "start-bot", 0, 0); err != nil {
		return proof, err
	}
	// Always stop the driver's bot, including on a failed motion observation.
	defer func() { _ = command(files.clientCommand, clientSequence+1, "stop-bot", 0, 0) }()
	host, client, err = waitPair(ctx, files, func(h, c snapshot) error {
		if err := acknowledged(c, clientSequence, "start-bot"); err != nil {
			return err
		}
		local, ok := playerByOwner(c, c.LocalClientID)
		remote, remoteOK := playerByOwner(h, c.LocalClientID)
		if !ok || !remoteOK || !motionReached(initialLocal.Position, initialRemote.Position, local.Position, remote.Position) {
			return errors.New("Waiting for client movement to reach host")
		}
		return nil
	})
	if err != nil {
		return proof, err
	}
	local, _ := playerByOwner(client, client.LocalClientID)
	remote, _ := playerByOwner(host, client.LocalClientID)
	proof.ClientMovementMetres = horizontalDistance(initialLocal.Position, local.Position)
	proof.HostObservedMovementMetres = horizontalDistance(initialRemote.Position, remote.Position)
	proof.FinalPositionDifferenceMetres = distance(local.Position, remote.Position)
	stopSequence := clientSequence + 1
	if err := command(files.clientCommand, stopSequence, "stop-bot", 0, 0); err != nil {
		return proof, err
	}
	_, _, err = waitPair(ctx, files, func(_, c snapshot) error { return acknowledged(c, stopSequence, "stop-bot") })
	if err != nil {
		return proof, err
	}
	return proof, nil
}

func selectClosedDoor(doors []door) (door, bool) {
	selected := door{DoorID: -1}
	for _, d := range doors {
		if d.DoorID < 0 || !d.CanUseLocal || d.Prompt == "" || d.Locked || d.Jammed || d.ExternalControl || d.Kind == "Airlock" || d.Kind == "Open" ||
			d.SyncState&3 != 0 || math.IsNaN(d.OpenAmount) || math.IsInf(d.OpenAmount, 0) || d.OpenAmount < 0 || d.OpenAmount >= .2 || !doorCanRemainOpen(d) {
			continue
		}
		manual, selectedManual := !d.Powered && d.Kind != "Hatch", !selected.Powered && selected.Kind != "Hatch"
		if selected.DoorID < 0 || manual && !selectedManual || manual == selectedManual &&
			(d.NearestCrewDistance < selected.NearestCrewDistance || d.NearestCrewDistance == selected.NearestCrewDistance && d.DoorID < selected.DoorID) {
			selected = d
		}
	}
	return selected, selected.DoorID >= 0
}

func doorCanRemainOpen(d door) bool {
	if d.Kind != "Standard" && d.Kind != "Bulkhead" && d.Kind != "Hatch" ||
		math.IsNaN(d.NearestCrewDistance) || math.IsInf(d.NearestCrewDistance, 0) || d.NearestCrewDistance < 0 {
		return false
	}
	// ShipDoor.Use cranks unpowered sliding doors permanently. Hatches always
	// toggle _requested and share the powered door's crew-distance autoclose.
	if !d.Powered && d.Kind != "Hatch" {
		return true
	}
	return !math.IsNaN(d.AutoCloseDistance) && !math.IsInf(d.AutoCloseDistance, 0) &&
		d.AutoCloseDistance >= 0 && d.NearestCrewDistance <= d.AutoCloseDistance
}

func selectClosedDoorPair(host, client snapshot) (door, bool) {
	eligible := make([]door, 0, len(client.Doors))
	for _, d := range client.Doors {
		authority, ok := doorByID(host, d.DoorID)
		if !ok || authority.Kind != d.Kind || authority.Powered != d.Powered || authority.Locked || authority.Jammed || authority.ExternalControl ||
			authority.SyncState&3 != 0 || !doorCanRemainOpen(authority) {
			continue
		}
		eligible = append(eligible, d)
	}
	return selectClosedDoor(eligible)
}

func motionReached(initialLocal, initialRemote, local, remote vector) bool {
	return finiteVector(initialLocal) && finiteVector(initialRemote) && finiteVector(local) && finiteVector(remote) &&
		horizontalDistance(initialLocal, local) >= .5 && horizontalDistance(initialRemote, remote) >= .4 && distance(local, remote) <= 2.5
}

func horizontalDistance(a, b vector) float64 {
	return math.Hypot(a.X-b.X, a.Z-b.Z)
}
func acknowledged(peer snapshot, sequence int, action string) error {
	if peer.CommandSequence != sequence || peer.CommandAction != action {
		return errors.New("Waiting for command acknowledgement")
	}
	if !peer.CommandAccepted || peer.CommandError != "" {
		return fmt.Errorf("Command rejected: %s", peer.CommandError)
	}
	return nil
}
func command(path string, sequence int, action string, zone, doorID int) error {
	return writeJSON(path, map[string]any{"sequence": sequence, "action": action, "zone": zone, "door": doorID})
}
func playerByOwner(peer snapshot, owner uint64) (player, bool) {
	for _, p := range peer.Players {
		if p.OwnerClientID == owner {
			return p, true
		}
	}
	return player{}, false
}
func doorByID(peer snapshot, id int) (door, bool) {
	for _, d := range peer.Doors {
		if d.DoorID == id {
			return d, true
		}
	}
	return door{}, false
}
func distance(a, b vector) float64 {
	return math.Sqrt((a.X-b.X)*(a.X-b.X) + (a.Y-b.Y)*(a.Y-b.Y) + (a.Z-b.Z)*(a.Z-b.Z))
}
func finiteVector(v vector) bool {
	return !math.IsNaN(v.X) && !math.IsInf(v.X, 0) && !math.IsNaN(v.Y) && !math.IsInf(v.Y, 0) && !math.IsNaN(v.Z) && !math.IsInf(v.Z, 0)
}
func geometryReady(g geometry) bool {
	return g.MeshRenderers > 0 && g.SupportedShaderRenderers > 0 && g.SupportedShaderRenderers <= g.MeshRenderers && g.Vertices > 0 && g.Indices > 0 && g.ActiveSolidColliders > 0
}
func writeJSON(path string, value any) error {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	temporary := path + ".tmp"
	if err := os.WriteFile(temporary, data, 0644); err != nil {
		return err
	}
	return os.Rename(temporary, path)
}
