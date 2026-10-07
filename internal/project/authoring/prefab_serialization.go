package authoring

import (
	"bytes"
	"encoding/json"
	"strings"

	"gopkg.in/yaml.v3"
)

// Unity's reader accepts a narrower YAML subset than yaml.v3. In particular,
// nonempty modification arrays must be block sequences, not flow sequences
// placed alone on the next line beneath their field name.
func setPrefabModificationField(asset *yamlAsset, instanceID, key string, value any) error {
	doc := asset.find(instanceID)
	if doc == nil {
		return failure("not_found", "prefab instance is absent", nil)
	}
	mod, rootIndex := nodeField(doc.node, "m_Modification")
	if mod == nil || mod.Kind != yaml.MappingNode {
		return failure("invalid_prefab", "prefab modification map is absent", nil)
	}
	replacement, err := valueNode(value)
	if err != nil {
		return err
	}
	old, index := nodeField(mod, key)
	if old != nil {
		preservePrefabRecordNodes(old, replacement)
	}
	prefabNodeStyles(replacement)
	if mod.Style&yaml.FlowStyle == 0 && index >= 0 {
		keyNode := mod.Content[index]
		start := lineOffset(doc.body, keyNode.Line)
		end := len(doc.body)
		if index+2 < len(mod.Content) {
			end = lineOffset(doc.body, mod.Content[index+2].Line)
		} else {
			if rootIndex+2 < len(doc.node.Content) {
				end = lineOffset(doc.body, doc.node.Content[rootIndex+2].Line)
			}
		}
		end = trimPrefabTrailingComments(doc.body, start, end)
		text, err := prefabFieldText(key, keyNode.Column-1, replacement, asset.newline)
		if err != nil {
			return err
		}
		doc.body = append(append(append([]byte{}, doc.body[:start]...), []byte(text)...), doc.body[end:]...)
		return asset.refresh()
	}
	// Normalize only a previously inline modification map. Preserve unknown
	// sibling nodes, their tags, styles and comments during this conversion.
	copy := *mod
	copy.Style = 0
	copy.Content = append([]*yaml.Node{}, mod.Content...)
	if index >= 0 {
		copy.Content[index+1] = replacement
	} else {
		copy.Content = append(copy.Content, &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: key}, replacement)
	}
	rootKey := doc.node.Content[rootIndex]
	start := lineOffset(doc.body, rootKey.Line)
	end := len(doc.body)
	if rootIndex+2 < len(doc.node.Content) {
		end = lineOffset(doc.body, doc.node.Content[rootIndex+2].Line)
	}
	end = trimPrefabTrailingComments(doc.body, start, end)
	text, err := prefabFieldText("m_Modification", rootKey.Column-1, &copy, asset.newline)
	if err != nil {
		return err
	}
	doc.body = append(append(append([]byte{}, doc.body[:start]...), []byte(text)...), doc.body[end:]...)
	return asset.refresh()
}

func trimPrefabTrailingComments(body []byte, start, end int) int {
	for end > start {
		lineEnd := end
		if body[lineEnd-1] == '\n' {
			lineEnd--
		}
		if lineEnd > start && body[lineEnd-1] == '\r' {
			lineEnd--
		}
		lineStart := bytes.LastIndexByte(body[:lineEnd], '\n') + 1
		line := bytes.TrimSpace(body[lineStart:lineEnd])
		if len(line) == 0 || line[0] == '#' {
			end = lineStart
		} else {
			break
		}
	}
	return end
}
func prefabFieldText(key string, indent int, node *yaml.Node, newline string) (string, error) {
	text, err := encodeNode(node)
	if err != nil {
		return "", err
	}
	prefix := strings.Repeat(" ", indent)
	if (node.Kind == yaml.MappingNode || node.Kind == yaml.SequenceNode) && node.Style&yaml.FlowStyle == 0 {
		childIndent := strings.Repeat(" ", indent+2)
		return prefix + key + ":" + newline + childIndent + strings.ReplaceAll(text, "\n", newline+childIndent) + newline, nil
	}
	return prefix + key + ": " + text + newline, nil
}
func prefabNodeStyles(node *yaml.Node) {
	if node.Kind == yaml.SequenceNode {
		if len(node.Content) == 0 {
			node.Style = yaml.FlowStyle
		} else {
			node.Style = 0
		}
	}
	if node.Kind == yaml.MappingNode {
		if ref, _ := nodeField(node, "fileID"); ref != nil {
			node.Style = yaml.FlowStyle
		} else {
			node.Style = 0
		}
	}
	for _, child := range node.Content {
		prefabNodeStyles(child)
	}
}

// Reuse the original AST for unchanged records, preserving lexical scalar tags
// and unknown attributes rather than decoding/re-encoding their plain values.
func preservePrefabRecordNodes(old, replacement *yaml.Node) {
	if old.Kind != yaml.SequenceNode || replacement.Kind != yaml.SequenceNode {
		return
	}
	for i, candidate := range replacement.Content {
		for _, previous := range old.Content {
			var before, after any
			if previous.Decode(&before) != nil || candidate.Decode(&after) != nil {
				continue
			}
			canonicalPrefabRefs(before)
			canonicalPrefabRefs(after)
			a, _ := json.Marshal(before)
			b, _ := json.Marshal(after)
			if bytes.Equal(a, b) {
				replacement.Content[i] = previous
				break
			}
		}
	}
}
func canonicalPrefabRefs(value any) {
	switch value := value.(type) {
	case map[string]any:
		if _, exists := value["fileID"]; exists {
			value["fileID"] = idReference(value)
		}
		for _, child := range value {
			canonicalPrefabRefs(child)
		}
	case []any:
		for _, child := range value {
			canonicalPrefabRefs(child)
		}
	}
}
