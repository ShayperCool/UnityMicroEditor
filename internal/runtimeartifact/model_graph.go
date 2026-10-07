package runtimeartifact

import (
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"sort"
	"strconv"
)

// ModelGraph records authored topology using GUID/local-ID references, so the
// same graph can be compared with an Editor-imported catalog independently of
// native external table indexes. Array order is part of the fingerprint.
type ModelGraphReference struct {
	FileID string `json:"fileId"`
	GUID   string `json:"guid"`
}
type ModelGraphProperty struct {
	Property string                `json:"property"`
	Targets  []ModelGraphReference `json:"targets"`
}
type ModelGraphObject struct {
	FileID     string               `json:"fileId"`
	ClassID    int32                `json:"classId"`
	References []ModelGraphProperty `json:"references"`
	Scalars    map[string]any       `json:"scalars"`
}

// HashModelGraph hashes the canonical numeric local-ID order. Input graph array
// order is ignored; reference property and target order remain significant.
func HashModelGraph(graph []ModelGraphObject) string {
	canonical := append([]ModelGraphObject(nil), graph...)
	sort.Slice(canonical, func(i, j int) bool {
		a, _ := strconv.ParseInt(canonical[i].FileID, 10, 64)
		b, _ := strconv.ParseInt(canonical[j].FileID, 10, 64)
		return a < b
	})
	bytes, _ := json.Marshal(canonical)
	hash := sha256.Sum256(bytes)
	return hex.EncodeToString(hash[:])
}

// These inline tree hashes constrain offsets to layouts independently checked
// using Unity's binary2text and public ContentFile readback in 6000.6.3f1.
// Unproven graph layouts never qualify for an in-place model replacement.
var modelGraphTrees = map[int32]string{
	1:  "63343cbbd67883d5da96355cf43e2c6e357929a116e48e665d55d28a006ba868",
	4:  "a608a3ce2cf08b0e6f3c73bc2481ac5369c55bccfa02f573a281aaed25d7860f",
	23: "e477f1d8f03c3656b5cfbfc9c221af430c06315a812a0dc764a514814a80aa59",
	33: "7f3907445f792e4b1629c1a44de04bce6e7d587f1549c55410d14f80130d6514",
}

func readModelGraph(input []byte, dataOffset uint64, entries []objectEntry, objects []Object, trees map[int32]string, externals []ExternalReference) ([]ModelGraphObject, string) {
	retained := make(map[string]bool, len(objects))
	for _, o := range objects {
		retained[o.FileID] = true
	}
	graph := []ModelGraphObject{}
	for _, entry := range entries {
		id := strconv.FormatInt(entry.id, 10)
		if !retained[id] {
			continue
		}
		switch entry.classID {
		case 21, 43, 74, 90:
			continue // Separate identities/data hashes cover assets.
		case 1, 4, 23, 33:
			if trees[entry.classID] != modelGraphTrees[entry.classID] {
				return nil, fmt.Sprintf("unproven native class %d type tree", entry.classID)
			}
		default:
			return nil, fmt.Sprintf("native class %d model graph has not been verified", entry.classID)
		}
		object := input[dataOffset+entry.offset : dataOffset+entry.offset+entry.size]
		record, err := readModelGraphObject(id, entry.classID, object, externals)
		if err != nil {
			return nil, fmt.Sprintf("native class %d local ID %s graph: %v", entry.classID, id, err)
		}
		graph = append(graph, record)
	}
	sort.Slice(graph, func(i, j int) bool {
		a, _ := strconv.ParseInt(graph[i].FileID, 10, 64)
		b, _ := strconv.ParseInt(graph[j].FileID, 10, 64)
		return a < b
	})
	return graph, ""
}

func readModelGraphObject(id string, classID int32, object []byte, externals []ExternalReference) (ModelGraphObject, error) {
	result := ModelGraphObject{FileID: id, ClassID: classID, References: []ModelGraphProperty{}, Scalars: map[string]any{}}
	r := reader{data: object, pos: 40}
	ptr := func() (ModelGraphReference, error) {
		data, err := r.take(12)
		if err != nil {
			return ModelGraphReference{}, err
		}
		index := int32(binary.LittleEndian.Uint32(data[:4]))
		local := int64(binary.LittleEndian.Uint64(data[4:]))
		target := ModelGraphReference{FileID: strconv.FormatInt(local, 10), GUID: ""}
		if index < 0 || int(index) > len(externals) {
			return target, fmt.Errorf("external PPtr index %d outside table", index)
		}
		if local != 0 && index != 0 {
			target.GUID = externals[index-1].GUID
		}
		return target, nil
	}
	one := func(property string) error {
		target, err := ptr()
		if err == nil {
			result.References = append(result.References, ModelGraphProperty{property, []ModelGraphReference{target}})
		}
		return err
	}
	list := func(property string) error {
		count, err := r.u32()
		if err != nil {
			return err
		}
		if count > 100000 || uint64(count)*12 > uint64(len(object)-r.pos) {
			return fmt.Errorf("reference array count is out of bounds")
		}
		targets := make([]ModelGraphReference, 0, count)
		for i := uint32(0); i < count; i++ {
			p, err := ptr()
			if err != nil {
				return err
			}
			targets = append(targets, p)
		}
		result.References = append(result.References, ModelGraphProperty{property, targets})
		return nil
	}
	var err error
	switch classID {
	case 1:
		err = list("m_Component")
		if err != nil {
			break
		}
		var layer uint32
		var name, tag string
		if layer, err = r.u32(); err != nil {
			break
		}
		if name, err = r.string(); err != nil {
			break
		}
		if tag, err = r.string(); err != nil {
			break
		}
		if _, err = r.take(16); err != nil {
			break
		} // Editor icon reference and navigation layer.
		var static uint32
		if static, err = r.u32(); err != nil {
			break
		}
		var active []byte
		if active, err = r.take(1); err != nil {
			break
		}
		if active[0] > 1 {
			err = fmt.Errorf("invalid GameObject active flag")
			break
		}
		result.Scalars = map[string]any{"m_Name": name, "m_Layer": layer, "m_TagString": tag, "m_IsActive": uint32(active[0]), "m_StaticEditorFlags": static}
	case 4:
		if err = one("m_GameObject"); err != nil {
			break
		}
		r.pos = 96
		if err = list("m_Children"); err != nil {
			break
		}
		err = one("m_Father")
		if err == nil && r.pos+12 != len(object) {
			err = fmt.Errorf("unexpected Transform payload suffix")
		}
		if err == nil {
			for _, field := range []struct {
				name          string
				offset, count int
			}{{"m_LocalRotation", 52, 4}, {"m_LocalPosition", 68, 3}, {"m_LocalScale", 80, 3}} {
				value := map[string]any{}
				names := []string{"x", "y", "z", "w"}
				for index := 0; index < field.count; index++ {
					number := math.Float32frombits(binary.LittleEndian.Uint32(object[field.offset+index*4:]))
					if math.IsNaN(float64(number)) || math.IsInf(float64(number), 0) {
						err = fmt.Errorf("non-finite Transform %s", field.name)
						break
					}
					if number == 0 {
						number = 0
					} // Canonicalize signed zero for catalog JSON parity.
					value[names[index]] = number
				}
				if err != nil {
					break
				}
				result.Scalars[field.name] = value
			}
		}
	case 33:
		if err = one("m_GameObject"); err != nil {
			break
		}
		err = one("m_Mesh")
		if err == nil && r.pos != len(object) {
			err = fmt.Errorf("unexpected MeshFilter payload suffix")
		}
	case 23:
		if err = one("m_GameObject"); err != nil {
			break
		}
		r.pos = 84
		if err = list("m_Materials"); err != nil {
			break
		}
		if _, err = r.take(4); err != nil {
			break
		}
		for _, property := range []string{"m_StaticBatchRoot", "m_ProbeAnchor", "m_LightProbeVolumeOverride"} {
			if err = one(property); err != nil {
				break
			}
		}
		if err != nil {
			break
		}
		if len(object) < 208 {
			err = fmt.Errorf("unexpected MeshRenderer payload suffix")
			break
		}
		for _, field := range []struct {
			name   string
			offset int
		}{{"m_Enabled", 52}, {"m_CastShadows", 53}, {"m_ReceiveShadows", 54}, {"m_DynamicOccludee", 55}, {"m_MotionVectors", 57}, {"m_LightProbeUsage", 58}, {"m_ReflectionProbeUsage", 59}} {
			result.Scalars[field.name] = uint32(object[field.offset])
		}
		result.Scalars["m_RenderingLayerMask"] = binary.LittleEndian.Uint32(object[76:80])
		result.Scalars["m_SortingLayerID"] = int32(binary.LittleEndian.Uint32(object[len(object)-24:]))
		result.Scalars["m_SortingOrder"] = int16(binary.LittleEndian.Uint16(object[len(object)-18:]))
		r.pos = len(object) - 12
		err = one("m_AdditionalVertexStreams")
	}
	return result, err
}
