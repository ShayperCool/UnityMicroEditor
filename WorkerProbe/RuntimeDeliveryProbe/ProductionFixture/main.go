// ProductionFixture publishes a minimal raw scene exercising the production
// ContentFile loader. It does not start Unity or alter the source project.
package main

import (
	"crypto/sha256"
	"unitymicroeditor/worker/runtimeartifact"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
)

type value = map[string]any

func main() {
	root := flag.String("project", "", "absolute source worktree")
	output := flag.String("output", "", "absolute ignored output directory")
	audio := flag.String("audio", "", "optional native project audio serialized artifact")
	resource := flag.String("resource", "", "optional native project audio resource")
	audioGUID := flag.String("audio-guid", "71705265666978617564696f70726f62", "native audio GUID")
	flag.Parse()
	if !filepath.IsAbs(*root) || !filepath.IsAbs(*output) {
		fail(fmt.Errorf("project/output must be absolute"))
	}
	if err := os.MkdirAll(*output, 0755); err != nil {
		fail(err)
	}
	fixtures := filepath.Join(*root, "Temp/UnityPlay/worker-probe/go-final-cli/artifacts")
	if *audio == "" {
		*audio = filepath.Join(fixtures, "1-0-cf98ce63e0e5085e20444dc09b0590fc.bin")
	}
	if *resource == "" {
		*resource = filepath.Join(fixtures, "1-1-ccd6b531022382dd9703569034d61bef.bin")
	}
	meshGUID := "b1d15452ab70b6a669e9b8e3c0b91251"
	builtinGUID := "0000000000000000f000000000000000"
	meshID := "7968783279521552916"
	descriptors := make([]value, 0, 2)
	for _, item := range []struct{ guid, profile, input string }{
		{meshGUID, "fbx", filepath.Join(fixtures, "0-0-dec78b13583b5d38d519af0a8c636e82.bin")},
		{*audioGUID, "wav", *audio},
	} {
		input, err := os.ReadFile(item.input)
		if err != nil {
			fail(err)
		}
		projected, meta, err := runtimeartifact.Adapt(input, item.profile)
		if err != nil {
			fail(err)
		}
		path := filepath.Join(*output, item.guid+".bin")
		write(path, projected)
		descriptor := value{"guid": item.guid, "path": path, "sha256": meta.OutputSHA256, "objects": meta.Objects, "resources": []value{}, "dependencies": []value{}}
		if item.profile == "fbx" {
			descriptor["dependencies"] = []value{{"guid": builtinGUID, "global": true}}
		}
		if item.profile == "wav" {
			bytes, err := os.ReadFile(*resource)
			if err != nil {
				fail(err)
			}
			if len(meta.Resources) != 1 || uint64(len(bytes)) != meta.Resources[0].Size {
				fail(fmt.Errorf("audio resource size mismatch"))
			}
			virtual := meta.Resources[0].VirtualPath
			physical := filepath.Join(*output, filepath.FromSlash(virtual))
			if err := os.MkdirAll(filepath.Dir(physical), 0755); err != nil {
				fail(err)
			}
			write(physical, bytes)
			sum := sha256.Sum256(bytes)
			descriptor["resources"] = []value{{"path": physical, "virtualPath": virtual, "sha256": hex.EncodeToString(sum[:])}}
		}
		descriptors = append(descriptors, descriptor)
	}
	ref := func(id, guid string) value { return value{"fileID": id, "guid": guid, "type": 3} }
	objects := []value{}
	add := func(id string, class int, data value, refs []value, typename string) {
		raw, err := json.Marshal(data)
		if err != nil {
			fail(err)
		}
		objects = append(objects, value{"fileId": id, "classId": class, "gameObjectId": "2001", "sourcePath": "Assets/NativeContentProductionProof.unity", "typeName": typename, "dataJson": string(raw), "references": refs})
	}
	localref := func(property, id string) value {
		return value{"property": property, "fileId": id, "guid": "", "type": 0}
	}
	assetref := func(property, id, guid string) value {
		return value{"property": property, "fileId": id, "guid": guid, "type": 3}
	}
	add("2001", 1, value{"m_Name": "NativeContentProductionProof", "m_IsActive": 1, "m_Layer": 0, "m_TagString": "Untagged", "m_Component": []value{
		{"component": ref("2002", "")}, {"component": ref("2003", "")}, {"component": ref("2005", "")}, {"component": ref("2006", "")},
	}}, []value{}, "")
	add("2002", 4, value{"m_GameObject": ref("2001", ""), "m_LocalPosition": value{"x": 0, "y": 0, "z": 0}, "m_LocalRotation": value{"x": 0, "y": 0, "z": 0, "w": 1}, "m_LocalScale": value{"x": 1, "y": 1, "z": 1}, "m_Father": ref("0", ""), "m_Children": []value{}}, []value{localref("m_GameObject", "2001")}, "")
	add("2003", 33, value{"m_GameObject": ref("2001", ""), "m_Mesh": ref(meshID, meshGUID)}, []value{localref("m_GameObject", "2001"), assetref("m_Mesh", meshID, meshGUID)}, "")
	add("2005", 82, value{"m_GameObject": ref("2001", ""), "m_Enabled": 1, "m_PlayOnAwake": 0, "m_audioClip": ref("8300000", *audioGUID)}, []value{localref("m_GameObject", "2001"), assetref("m_audioClip", "8300000", *audioGUID)}, "")
	add("2006", 114, value{"m_GameObject": ref("2001", ""), "m_Enabled": 1, "meshFilter": ref("2003", ""), "audioSource": ref("2005", "")}, []value{localref("m_GameObject", "2001"), localref("meshFilter", "2003"), localref("audioSource", "2005")}, "UnityPlay.WorkerProbe.NativeContentProductionProof")
	manifest := value{"schemaVersion": 1, "unityVersion": "6000.6.3f1", "sceneName": "NativeContentProductionProof", "scenePath": "Assets/NativeContentProductionProof.unity", "projectRoot": *root, "supported": true, "lifecycleBarrierVersion": 0, "assemblies": []string{filepath.Join(*output, "WorkerRuntimeDeliveryProbe.dll")}, "objects": objects, "nativeContentFiles": descriptors, "diagnostics": []value{}, "assets": []value{
		{"guid": meshGUID, "fileId": meshID, "kind": "imported", "path": "Assets/Dungeon/ShipGen/Art/Custom/Shell/R_Floor.fbx", "classId": 43, "typeName": "UnityEngine.Mesh"},
		{"guid": *audioGUID, "fileId": "8300000", "kind": "imported", "path": "Assets/NativeContentProof.wav", "classId": 83, "typeName": "UnityEngine.AudioClip"},
	}}
	bytes, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		fail(err)
	}
	path := filepath.Join(*output, "scene.json")
	write(path, bytes)
	fmt.Println(path)
}
func write(path string, bytes []byte) {
	if err := os.WriteFile(path, bytes, 0644); err != nil {
		fail(err)
	}
}
func fail(err error) { fmt.Fprintln(os.Stderr, err); os.Exit(1) }
