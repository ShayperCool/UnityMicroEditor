// FastPatchFixture publishes raw scene revisions and patch requests for a
// separately launched Mono Player. No Unity/Python processes are started here.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"time"
)

type value = map[string]any

const sourcePath = "Assets/FastPatchProductionProof.unity"

func main() {
	project := flag.String("project", "", "absolute worktree root")
	output := flag.String("output", "", "absolute ignored output directory")
	phase := flag.Int("phase", 0, "0 prepare, 1 rename, 2 add child, 3 add root")
	flag.Parse()
	if !filepath.IsAbs(*project) || !filepath.IsAbs(*output) || *phase < 0 || *phase > 3 {
		fail(fmt.Errorf("absolute project/output and phase0..3 are required"))
	}
	if err := os.MkdirAll(*output, 0755); err != nil {
		fail(err)
	}
	if *phase == 0 {
		prepare(*project, *output)
		atomic(filepath.Join(*output, "phase.txt"), []byte("0\n"))
		_ = os.Remove(filepath.Join(*output, "request.json"))
		fmt.Println(filepath.Join(*output, "revisions/phase0/scene.json"))
		return
	}
	revision := fmt.Sprintf("phase%d", *phase)
	folder := filepath.Join(*output, "revisions", revision)
	manifest := filepath.Join(folder, "scene.json")
	patch := filepath.Join(folder, "patch.json")
	manifestBytes, err := os.ReadFile(manifest)
	if err != nil {
		fail(err)
	}
	patchBytes, err := os.ReadFile(patch)
	if err != nil {
		fail(err)
	}
	request := value{"schemaVersion": 1, "requestId": fmt.Sprintf("%s-%d", revision, time.Now().UnixNano()), "manifest": manifest, "manifestSha256": hash(manifestBytes), "revision": revision, "patch": patch, "patchSha256": hash(patchBytes)}
	atomic(filepath.Join(*output, "phase.txt"), []byte(strconv.Itoa(*phase)+"\n"))
	writeJSON(filepath.Join(*output, "request.json"), request)
	fmt.Println(filepath.Join(*output, "request.json"))
}

func prepare(project, output string) {
	ref := func(id string) value { return value{"fileID": id} }
	localref := func(property, id string) value {
		return value{"property": property, "fileId": id, "guid": "", "type": 0}
	}
	record := func(id string, class int, owner string, data value, refs []value, typename string) value {
		raw, err := json.Marshal(data)
		if err != nil {
			fail(err)
		}
		return value{"fileId": id, "classId": class, "gameObjectId": owner, "sourcePath": sourcePath, "guid": "", "typeName": typename, "dataJson": string(raw), "references": refs}
	}
	gameObject := func(id, name string, components ...string) value {
		list := []value{}
		for _, component := range components {
			list = append(list, value{"component": ref(component)})
		}
		return record(id, 1, id, value{"m_Name": name, "m_IsActive": 1, "m_Layer": 0, "m_TagString": "Untagged", "m_Component": list}, []value{}, "")
	}
	transform := func(id, owner, parent string, pos value, children ...string) value {
		list := []value{}
		refs := []value{localref("m_GameObject", owner)}
		if parent != "0" {
			refs = append(refs, localref("m_Father", parent))
		}
		for index, child := range children {
			list = append(list, ref(child))
			refs = append(refs, localref(fmt.Sprintf("m_Children[%d]", index), child))
		}
		return record(id, 4, owner, value{"m_GameObject": ref(owner), "m_LocalPosition": pos, "m_LocalRotation": value{"x": 0, "y": 0, "z": 0, "w": 1}, "m_LocalScale": value{"x": 1, "y": 1, "z": 1}, "m_Father": ref(parent), "m_Children": list}, refs, "")
	}
	roots := func(ids ...string) value {
		list := []value{}
		refs := []value{}
		for i, id := range ids {
			list = append(list, ref(id))
			refs = append(refs, localref(fmt.Sprintf("m_Roots[%d]", i), id))
		}
		return record("9223372036854775807", 1660057539, "", value{"m_Roots": list}, refs, "")
	}
	zero := value{"x": 0, "y": 0, "z": 0}
	originalGO := gameObject("1", "OriginalTarget", "2")
	renamedGO := gameObject("1", "RenamedTarget", "2")
	targetTF := transform("2", "1", "0", zero)
	parentWithChild := transform("2", "1", "0", zero, "32")
	observerGO := gameObject("11", "Observer", "12", "13")
	observerTF := transform("12", "11", "0", zero)
	observerScript := record("13", 114, "11", value{"m_GameObject": ref("11"), "m_Enabled": 1, "targetObject": ref("1"), "targetTransform": ref("2")}, []value{localref("m_GameObject", "11"), localref("targetObject", "1"), localref("targetTransform", "2")}, "UnityPlay.WorkerProbe.FastPatchProductionProof")
	childGO := gameObject("31", "ChildAdded", "32")
	childTF := transform("32", "31", "2", value{"x": 1, "y": 0, "z": 0})
	rootGO := gameObject("51", "RootAdded", "52")
	rootTF := transform("52", "51", "0", zero)
	for phase := 0; phase <= 3; phase++ {
		goRecord := originalGO
		tfRecord := targetTF
		if phase > 0 {
			goRecord = renamedGO
		}
		if phase >= 2 {
			tfRecord = parentWithChild
		}
		rootRecord := roots("2", "12")
		if phase >= 3 {
			rootRecord = roots("2", "12", "52")
		}
		records := []value{goRecord, tfRecord, observerGO, observerTF, observerScript, rootRecord}
		if phase >= 2 {
			records = append(records, childGO, childTF)
		}
		if phase >= 3 {
			records = append(records, rootGO, rootTF)
		}
		manifest := value{"schemaVersion": 1, "unityVersion": "6000.6.3f1", "sceneName": "FastPatchProductionProof", "scenePath": sourcePath, "projectRoot": project, "supported": true, "assemblies": []string{filepath.Join(output, "WorkerRuntimeDeliveryProbe.dll")}, "lifecycleBarrierVersion": 0, "objects": records, "assets": []value{}, "diagnostics": []value{}}
		folder := filepath.Join(output, "revisions", fmt.Sprintf("phase%d", phase))
		if err := os.MkdirAll(folder, 0755); err != nil {
			fail(err)
		}
		writeJSON(filepath.Join(folder, "scene.json"), manifest)
		if phase == 0 {
			continue
		}
		patchRecords := []value{renamedGO}
		if phase == 2 {
			patchRecords = []value{parentWithChild, childGO, childTF}
		}
		if phase == 3 {
			patchRecords = []value{rootRecord, rootGO, rootTF}
		}
		patch := value{"schemaVersion": 1, "baseRevision": fmt.Sprintf("phase%d", phase-1), "revision": fmt.Sprintf("phase%d", phase), "objects": patchRecords}
		writeJSON(filepath.Join(folder, "patch.json"), patch)
	}
}

func hash(bytes []byte) string { sum := sha256.Sum256(bytes); return hex.EncodeToString(sum[:]) }
func writeJSON(path string, item any) {
	bytes, err := json.MarshalIndent(item, "", "  ")
	if err != nil {
		fail(err)
	}
	atomic(path, bytes)
}
func atomic(path string, bytes []byte) {
	temporary := path + ".tmp"
	if err := os.WriteFile(temporary, bytes, 0644); err != nil {
		fail(err)
	}
	if err := os.Rename(temporary, path); err != nil {
		fail(err)
	}
}
func fail(err error) { fmt.Fprintln(os.Stderr, err); os.Exit(1) }
