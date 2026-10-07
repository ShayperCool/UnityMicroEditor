// prepare creates immutable multiplayer test overlays. It never launches Unity
// and never rewrites source scenes, prefabs, or the input prepared Player.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

const helperAssembly = "UnityPlay.MultiplayerProof"
const providerType = helperAssembly + ".RawContentProvider"
const observerType = helperAssembly + ".MultiplayerObserver"
const observerMethod = "__UnityPlay_RuntimeInitialize_MultiplayerProof"

type options struct{ Input, Helper, Output, MetadataHelper, LifecycleProof, Dotnet string }
type preparedEvent struct {
	Event         string `json:"event"`
	Player        string `json:"player"`
	Manifest      string `json:"manifest"`
	ContentRoutes string `json:"contentRoutes"`
}
type routesFile struct {
	SchemaVersion int    `json:"schemaVersion"`
	Project       string `json:"project"`
	Scenes        []struct {
		GUID     string `json:"guid"`
		Manifest string `json:"manifest"`
		SHA256   string `json:"sha256"`
	} `json:"scenes"`
}
type launchDescriptor struct {
	SchemaVersion       int               `json:"schemaVersion"`
	Project             string            `json:"project"`
	Player              string            `json:"player"`
	PlayerSHA256        string            `json:"playerSha256"`
	Manifest            string            `json:"manifest"`
	ManifestSHA256      string            `json:"manifestSha256"`
	ContentRoutes       string            `json:"contentRoutes"`
	ContentRoutesSHA256 string            `json:"contentRoutesSha256"`
	AssemblyHashes      map[string]string `json:"assemblyHashes"`
	InputSHA256         string            `json:"inputSha256"`
	HelperSHA256        string            `json:"helperSha256"`
}

func main() {
	var opts options
	home, _ := os.UserHomeDir()
	defaultEditor := os.Getenv("UNITY_EDITOR")
	if defaultEditor == "" {
		defaultEditor = filepath.Join(home, "Unity/Hub/Editor/6000.6.3f1/Editor/Unity")
	}
	flag.StringVar(&opts.Input, "input", "", "JSON or JSONL prepared event from unity-micro-editor run --prepare-only")
	flag.StringVar(&opts.Helper, "helper", "", "SDK-instrumented UnityPlay.MultiplayerProof.dll")
	flag.StringVar(&opts.Output, "output-launch", "", "absolute launch descriptor path under project Temp")
	flag.StringVar(&opts.MetadataHelper, "metadata-helper", "", "prepared LifecycleCompiler.dll for SDK metadata verification")
	flag.StringVar(&opts.LifecycleProof, "lifecycle-proof", "", "actual SDK lifecycle transformation proof JSON")
	flag.StringVar(&opts.Dotnet, "dotnet", filepath.Join(filepath.Dir(defaultEditor), "Data/DotNetSdk/dotnet"), "Unity SDK dotnet")
	flag.Parse()
	if err := prepare(opts); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func readJSON(path string, value any) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	decoder := json.NewDecoder(io.LimitReader(file, 256<<20))
	decoder.UseNumber()
	if err := decoder.Decode(value); err != nil {
		return err
	}
	return nil
}
func readPrepared(path string) (preparedEvent, error) {
	file, err := os.Open(path)
	if err != nil {
		return preparedEvent{}, err
	}
	defer file.Close()
	decoder := json.NewDecoder(io.LimitReader(file, 256<<20))
	var found preparedEvent
	for {
		var candidate preparedEvent
		if err := decoder.Decode(&candidate); errors.Is(err, io.EOF) {
			break
		} else if err != nil {
			return found, err
		}
		if candidate.Event == "prepared" {
			if found.Event != "" {
				return found, errors.New("input contains multiple prepared events")
			}
			found = candidate
		}
	}
	if found.Event == "" || !filepath.IsAbs(found.Player) || !filepath.IsAbs(found.Manifest) || !filepath.IsAbs(found.ContentRoutes) {
		return found, errors.New("input must contain one prepared event with absolute player/manifest/contentRoutes paths")
	}
	return found, nil
}
func fileSHA(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	hash := sha256.New()
	if _, err := io.Copy(hash, file); err != nil {
		return "", err
	}
	return hex.EncodeToString(hash.Sum(nil)), nil
}
func marshalJSON(value any) ([]byte, error) { return json.Marshal(value) }
func writeNewJSON(path string, value any) error {
	data, err := marshalJSON(value)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	_, err = file.Write(append(data, '\n'))
	return errors.Join(err, file.Sync(), file.Close())
}
func asMap(value any) map[string]any {
	result, _ := value.(map[string]any)
	return result
}
func asArray(value any) []any {
	result, _ := value.([]any)
	return result
}
func text(value any) string {
	if value == nil {
		return ""
	}
	return fmt.Sprint(value)
}
func parseData(value any) (map[string]any, error) {
	var data map[string]any
	decoder := json.NewDecoder(strings.NewReader(text(value)))
	decoder.UseNumber()
	err := decoder.Decode(&data)
	return data, err
}
func encodeData(data map[string]any) (string, error) {
	value, err := json.Marshal(data)
	return string(value), err
}

func wireProvider(manifest map[string]any) error {
	objects := asArray(manifest["objects"])
	var scope, owner map[string]any
	used := map[string]bool{}
	for _, value := range objects {
		record := asMap(value)
		used[text(record["fileId"])] = true
		if text(record["typeName"]) == "Dungeon.Net.NetLifetimeScope" {
			if scope != nil {
				return errors.New("Boot has multiple NetLifetimeScope components")
			}
			scope = record
		}
		if text(record["typeName"]) == providerType {
			return errors.New("Boot already contains a proof provider overlay")
		}
	}
	if scope == nil {
		return errors.New("Boot does not contain NetLifetimeScope")
	}
	for _, value := range objects {
		record := asMap(value)
		if text(record["fileId"]) == text(scope["gameObjectId"]) && text(record["classId"]) == "1" {
			owner = record
			break
		}
	}
	if owner == nil {
		return errors.New("NetLifetimeScope owner GameObject is unavailable")
	}
	var componentID string
	for candidate := int64(9000000000000000001); ; candidate++ {
		componentID = fmt.Sprint(candidate)
		if !used[componentID] {
			break
		}
	}
	ownerData, err := parseData(owner["dataJson"])
	if err != nil {
		return err
	}
	components := asArray(ownerData["m_Component"])
	newReference := map[string]any{"fileID": json.Number(componentID)}
	ownerData["m_Component"] = append(components, map[string]any{"component": newReference})
	owner["dataJson"], err = encodeData(ownerData)
	if err != nil {
		return err
	}
	owner["scalarDataJson"] = owner["dataJson"]
	owner["references"] = append(asArray(owner["references"]), map[string]any{"property": fmt.Sprintf("m_Component[%d].component", len(components)), "fileId": componentID, "guid": "", "type": 0})
	scopeData, err := parseData(scope["dataJson"])
	if err != nil {
		return err
	}
	scopeData["contentProvider"] = newReference
	scope["dataJson"], err = encodeData(scopeData)
	if err != nil {
		return err
	}
	refs := []any{}
	for _, value := range asArray(scope["references"]) {
		if text(asMap(value)["property"]) != "contentProvider" {
			refs = append(refs, value)
		}
	}
	scope["references"] = append(refs, map[string]any{"property": "contentProvider", "fileId": componentID, "guid": "", "type": 0})
	providerData := map[string]any{"m_ObjectHideFlags": 0, "m_GameObject": map[string]any{"fileID": json.Number(text(owner["fileId"]))}, "m_Enabled": 1, "m_EditorHideFlags": 0, "m_Script": map[string]any{"fileID": 0}, "m_Name": "", "m_EditorClassIdentifier": helperAssembly + "::" + providerType}
	encoded, err := encodeData(providerData)
	if err != nil {
		return err
	}
	provider := map[string]any{"fileId": componentID, "classId": 114, "gameObjectId": text(owner["fileId"]), "sourcePath": text(manifest["scenePath"]), "guid": "", "typeName": providerType, "skipMissingScript": false, "dataJson": encoded, "scalarDataJson": "{}", "references": []any{map[string]any{"property": "m_GameObject", "fileId": text(owner["fileId"]), "guid": "", "type": 0}, map[string]any{"property": "m_Script", "fileId": "0", "guid": "", "type": 0}}}
	manifest["objects"] = append(objects, provider)
	return nil
}

func addHelperBindings(manifest map[string]any, managed string, observer bool) error {
	for _, key := range []string{"assemblies", "lifecycleBarrierAssemblies"} {
		paths := []any{}
		for _, value := range asArray(manifest[key]) {
			name := filepath.Base(text(value))
			if name == helperAssembly+".dll" {
				return errors.New("prepared scene already binds multiplayer proof assembly")
			}
			if _, err := os.Stat(filepath.Join(managed, name)); err != nil {
				return err
			}
			paths = append(paths, filepath.Join(managed, name))
		}
		if len(paths) == 0 {
			return fmt.Errorf("prepared scene lacks %s", key)
		}
		manifest[key] = append(paths, filepath.Join(managed, helperAssembly+".dll"))
	}
	if observer {
		manifest["runtimeInitializers"] = append(asArray(manifest["runtimeInitializers"]), map[string]any{"assembly": helperAssembly, "typeName": observerType, "methodName": observerMethod, "phase": 3})
	}
	return nil
}

func registerHelper(path string) error {
	var registry struct {
		Names []string `json:"names"`
		Types []int    `json:"types"`
	}
	if err := readJSON(path, &registry); err != nil {
		return err
	}
	if len(registry.Names) != len(registry.Types) {
		return errors.New("Player assembly registry length mismatch")
	}
	flags := -1
	for index, name := range registry.Names {
		if name == helperAssembly+".dll" {
			return errors.New("Player already registers helper assembly")
		}
		if name == "Dungeon.CLIPlay.dll" {
			flags = registry.Types[index]
		}
	}
	if flags != 16 {
		return errors.New("Player does not expose the expected game DLL registration flags")
	}
	registry.Names = append(registry.Names, helperAssembly+".dll")
	registry.Types = append(registry.Types, flags)
	data, err := json.Marshal(registry)
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0600)
}

func validateHelper(opts options) error {
	if filepath.Base(opts.Helper) != helperAssembly+".dll" || opts.MetadataHelper == "" || opts.LifecycleProof == "" {
		return errors.New("--helper, --metadata-helper and --lifecycle-proof are required")
	}
	var proof struct {
		Verified                bool
		SchemaVersion           int
		Phases                  []string
		ScriptableObjectBarrier bool
	}
	if err := readJSON(opts.LifecycleProof, &proof); err != nil {
		return err
	}
	if !proof.Verified || proof.SchemaVersion != 1 || !proof.ScriptableObjectBarrier {
		return errors.New("helper lacks verified SDK lifecycle instrumentation proof")
	}
	command := exec.Command(opts.Dotnet, opts.MetadataHelper, "--type-definitions", opts.Helper)
	output, err := command.CombinedOutput()
	if err != nil {
		return fmt.Errorf("SDK metadata verification failed: %w: %s", err, output)
	}
	var definitions map[string][]string
	if err := json.Unmarshal(output, &definitions); err != nil {
		return err
	}
	found := map[string]bool{}
	for _, typeName := range definitions[opts.Helper] {
		found[typeName] = true
	}
	if !found[providerType] || !found[observerType] {
		return errors.New("helper lacks required public provider/observer types")
	}
	return nil
}

func prepare(opts options) error {
	if opts.Input == "" || opts.Helper == "" || !filepath.IsAbs(opts.Output) {
		return errors.New("--input, --helper and absolute --output-launch are required")
	}
	for _, path := range []*string{&opts.Input, &opts.Helper, &opts.MetadataHelper, &opts.LifecycleProof} {
		if *path == "" {
			continue
		}
		absolute, err := filepath.Abs(*path)
		if err != nil {
			return err
		}
		*path = absolute
	}
	if err := validateHelper(opts); err != nil {
		return err
	}
	event, err := readPrepared(opts.Input)
	if err != nil {
		return err
	}
	var boot map[string]any
	if err := readJSON(event.Manifest, &boot); err != nil {
		return err
	}
	project := text(boot["projectRoot"])
	if !filepath.IsAbs(project) || boot["supported"] != true || text(boot["unityVersion"]) != "6000.6.3f1" {
		return errors.New("unsupported prepared source project")
	}
	owned := filepath.Join(project, "Temp") + string(filepath.Separator)
	output := filepath.Clean(opts.Output)
	if !strings.HasPrefix(output, owned) {
		return errors.New("multiplayer launch output must remain under source project Temp")
	}
	var routes routesFile
	if err := readJSON(event.ContentRoutes, &routes); err != nil {
		return err
	}
	if routes.SchemaVersion != 1 || routes.Project != project || len(routes.Scenes) == 0 {
		return errors.New("prepared content routes do not match source project")
	}
	inputHash, err := fileSHA(opts.Input)
	if err != nil {
		return err
	}
	helperHash, err := fileSHA(opts.Helper)
	if err != nil {
		return err
	}
	manifestHash, err := fileSHA(event.Manifest)
	if err != nil {
		return err
	}
	routesHash, err := fileSHA(event.ContentRoutes)
	if err != nil {
		return err
	}
	revisionHash := sha256.Sum256([]byte("multiplayer-overlay-v1\x00" + inputHash + helperHash + manifestHash + routesHash))
	directory := filepath.Join(filepath.Dir(output), "overlay-"+hex.EncodeToString(revisionHash[:12])+"-"+fmt.Sprint(time.Now().UnixNano()))
	if err := os.MkdirAll(directory, 0700); err != nil {
		return err
	}
	completed := false
	defer func() {
		if !completed {
			os.RemoveAll(directory)
		}
	}()
	playerDir := filepath.Join(directory, "Player")
	copyCommand := exec.Command("cp", "--reflink=auto", "-a", "--", filepath.Dir(event.Player), playerDir)
	if output, err := copyCommand.CombinedOutput(); err != nil {
		return fmt.Errorf("copy prepared Player: %w: %s", err, output)
	}
	player := filepath.Join(playerDir, filepath.Base(event.Player))
	dataDir := filepath.Join(playerDir, strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data")
	managed := filepath.Join(dataDir, "Managed")
	helper, err := os.ReadFile(opts.Helper)
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(managed, helperAssembly+".dll"), helper, 0600); err != nil {
		return err
	}
	if err := registerHelper(filepath.Join(dataDir, "ScriptingAssemblies.json")); err != nil {
		return err
	}
	if err := addHelperBindings(boot, managed, true); err != nil {
		return err
	}
	if err := wireProvider(boot); err != nil {
		return err
	}
	boot["revision"] = filepath.Base(directory)
	bootPath := filepath.Join(directory, "Boot", "scene.json")
	if err := writeNewJSON(bootPath, boot); err != nil {
		return err
	}
	assemblyHashes := map[string]string{}
	collect := func(manifest map[string]any) error {
		for _, value := range asArray(manifest["assemblies"]) {
			path := text(value)
			hash, err := fileSHA(path)
			if err != nil {
				return err
			}
			assemblyHashes[path] = hash
		}
		return nil
	}
	if err := collect(boot); err != nil {
		return err
	}
	for index := range routes.Scenes {
		route := &routes.Scenes[index]
		actual, err := fileSHA(route.Manifest)
		if err != nil || actual != route.SHA256 {
			return fmt.Errorf("prepared content manifest changed for %s", route.GUID)
		}
		var ship map[string]any
		if err := readJSON(route.Manifest, &ship); err != nil {
			return err
		}
		if text(ship["projectRoot"]) != project {
			return errors.New("content scene switches source projects")
		}
		if err := addHelperBindings(ship, managed, false); err != nil {
			return err
		}
		ship["revision"] = filepath.Base(directory) + "-" + route.GUID
		path := filepath.Join(directory, route.GUID, "scene.json")
		if err := writeNewJSON(path, ship); err != nil {
			return err
		}
		if err := collect(ship); err != nil {
			return err
		}
		route.Manifest = path
		route.SHA256, err = fileSHA(path)
		if err != nil {
			return err
		}
	}
	routesPath := filepath.Join(directory, "content-routes.json")
	if err := writeNewJSON(routesPath, routes); err != nil {
		return err
	}
	launch := launchDescriptor{SchemaVersion: 1, Project: project, Player: player, Manifest: bootPath, ContentRoutes: routesPath, AssemblyHashes: assemblyHashes, InputSHA256: inputHash, HelperSHA256: helperHash}
	launch.PlayerSHA256, err = fileSHA(player)
	if err != nil {
		return err
	}
	launch.ManifestSHA256, err = fileSHA(bootPath)
	if err != nil {
		return err
	}
	launch.ContentRoutesSHA256, err = fileSHA(routesPath)
	if err != nil {
		return err
	}
	for path, expected := range map[string]string{opts.Input: inputHash, opts.Helper: helperHash, event.Manifest: manifestHash, event.ContentRoutes: routesHash} {
		actual, err := fileSHA(path)
		if err != nil || actual != expected {
			return fmt.Errorf("preparation input changed: %s", path)
		}
	}
	if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
		return err
	}
	temporary := output + "." + fmt.Sprint(time.Now().UnixNano()) + ".tmp"
	if err := writeNewJSON(temporary, launch); err != nil {
		return err
	}
	if err := os.Rename(temporary, output); err != nil {
		os.Remove(temporary)
		return err
	}
	completed = true
	encoded, _ := json.Marshal(map[string]any{"event": "multiplayer-prepared", "launch": output, "player": player, "manifest": bootPath, "contentRoutes": routesPath})
	fmt.Println(string(bytes.TrimSpace(encoded)))
	return nil
}
