package goproject

import (
	"bytes"
	"fmt"
	"math"
	"regexp"
	"strconv"
	"strings"
	"unicode/utf8"

	"gopkg.in/yaml.v3"
)

const maxTextBytes = 32 << 20

var unityHeader = regexp.MustCompile(`(?m)^--- !u!(\d+) &(-?\d+)(?: (stripped))?[ \t]*\r?$`)
var guidLine = regexp.MustCompile(`(?m)^guid:[ \t]*([0-9a-fA-F]{32})[ \t]*\r?$`)

// Document retains Unity's signed 64-bit identities as decimal strings.
type Document struct {
	ClassID  int            `json:"classId"`
	FileID   string         `json:"fileId"`
	TypeName string         `json:"typeName"`
	Data     map[string]any `json:"data"`
	Stripped bool           `json:"stripped"`
}

// ParseUnityYAML accepts Unity text serialization without YAML aliases, duplicate
// keys, implicit dates or non-finite numbers. It never rounds a fileID to float64.
func ParseUnityYAML(content []byte) ([]Document, error) {
	if len(content) > maxTextBytes || !utf8.Valid(content) {
		return nil, fmt.Errorf("unsupported binary or oversized Unity text asset")
	}
	content = bytes.TrimPrefix(content, []byte{0xef, 0xbb, 0xbf})
	positions := unityHeader.FindAllSubmatchIndex(content, -1)
	if len(positions) == 0 {
		return nil, fmt.Errorf("Unity text asset has no document headers")
	}
	seen := map[string]bool{}
	documents := make([]Document, 0, len(positions))
	for i, p := range positions {
		classID, err := strconv.Atoi(string(content[p[2]:p[3]]))
		if err != nil {
			return nil, err
		}
		id := string(content[p[4]:p[5]])
		if _, err := strconv.ParseInt(id, 10, 64); err != nil {
			return nil, fmt.Errorf("invalid fileID %s", id)
		}
		if seen[id] {
			return nil, fmt.Errorf("duplicate fileID %s", id)
		}
		seen[id] = true
		end := len(content)
		if i+1 < len(positions) {
			end = positions[i+1][0]
		}
		var root yaml.Node
		if err := yaml.Unmarshal(content[p[1]:end], &root); err != nil {
			return nil, fmt.Errorf("fileID %s: %w", id, err)
		}
		if len(root.Content) != 1 {
			return nil, fmt.Errorf("fileID %s requires one object", id)
		}
		value, err := yamlValue(root.Content[0], "", 0)
		if err != nil {
			return nil, fmt.Errorf("fileID %s: %w", id, err)
		}
		body, ok := value.(map[string]any)
		if !ok || len(body) != 1 {
			return nil, fmt.Errorf("fileID %s requires one object mapping", id)
		}
		for name, value := range body {
			data, ok := value.(map[string]any)
			if !ok {
				return nil, fmt.Errorf("fileID %s fields are not a mapping", id)
			}
			documents = append(documents, Document{classID, id, name, data, p[6] >= 0})
		}
	}
	return documents, nil
}

func yamlValue(node *yaml.Node, key string, depth int) (any, error) {
	if depth > 128 {
		return nil, fmt.Errorf("Unity YAML nesting exceeds 128")
	}
	switch node.Kind {
	case yaml.AliasNode:
		return nil, fmt.Errorf("YAML aliases are unsupported")
	case yaml.MappingNode:
		result := make(map[string]any, len(node.Content)/2)
		for i := 0; i < len(node.Content); i += 2 {
			k := node.Content[i]
			if k.Kind != yaml.ScalarNode {
				return nil, fmt.Errorf("non-scalar YAML key")
			}
			if _, exists := result[k.Value]; exists {
				return nil, fmt.Errorf("duplicate YAML key %q", k.Value)
			}
			value, err := yamlValue(node.Content[i+1], k.Value, depth+1)
			if err != nil {
				return nil, err
			}
			result[k.Value] = value
		}
		return result, nil
	case yaml.SequenceNode:
		result := make([]any, len(node.Content))
		for i, item := range node.Content {
			v, err := yamlValue(item, "", depth+1)
			if err != nil {
				return nil, err
			}
			result[i] = v
		}
		return result, nil
	case yaml.ScalarNode:
		// Unity native byte arrays are lexical hex scalars. Digit-only buffers
		// must retain leading zeros and must not become YAML octal integers.
		if key == "_typelessdata" || key == "m_IndexBuffer" {
			return node.Value, nil
		}
		if key == "fileID" {
			if _, err := strconv.ParseInt(node.Value, 10, 64); err != nil {
				return nil, fmt.Errorf("invalid fileID %q", node.Value)
			}
			return node.Value, nil
		}
		if key == "guid" {
			return strings.ToLower(node.Value), nil
		}
		switch node.Tag {
		case "!!null":
			return nil, nil
		case "!!int":
			value, err := strconv.ParseInt(strings.ReplaceAll(node.Value, "_", ""), 0, 64)
			if err != nil {
				value, err = strconv.ParseInt(node.Value, 10, 64)
			}
			if err != nil {
				unsigned, unsignedErr := strconv.ParseUint(node.Value, 10, 64)
				if unsignedErr != nil {
					return nil, fmt.Errorf("integer %q exceeds 64 bits", node.Value)
				}
				return unsigned, nil
			}
			return value, nil
		case "!!float":
			value, err := strconv.ParseFloat(node.Value, 64)
			if err != nil || math.IsNaN(value) || math.IsInf(value, 0) {
				return nil, fmt.Errorf("non-finite or invalid float %q", node.Value)
			}
			return value, nil
		case "!!str", "!!bool", "!!timestamp":
			return node.Value, nil
		default:
			return nil, fmt.Errorf("unsupported YAML tag %s", node.Tag)
		}
	default:
		return nil, fmt.Errorf("unsupported YAML node %d", node.Kind)
	}
}
