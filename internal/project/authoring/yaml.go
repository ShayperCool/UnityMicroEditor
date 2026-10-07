package authoring

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"regexp"
	"strconv"
	"strings"
	"unicode/utf8"

	"gopkg.in/yaml.v3"
	goproject "unitymicroeditor/internal/project"
)

var documentHeader = regexp.MustCompile(`(?m)^--- !u!(\d+) &(-?\d+)(?: (stripped))?[ \t]*\r?$`)

type yamlDocument struct {
	id           string
	classID      int
	stripped     bool
	header, body []byte
	node         *yaml.Node
	data         map[string]any
	typeName     string
}
type yamlAsset struct {
	prefix  []byte
	docs    []*yamlDocument
	newline string
}

func parseAsset(content []byte) (*yamlAsset, error) {
	parsed, err := goproject.ParseUnityYAML(content)
	if err != nil {
		return nil, failure("invalid_yaml", err.Error(), nil)
	}
	positions := documentHeader.FindAllSubmatchIndex(content, -1)
	a := &yamlAsset{prefix: append([]byte{}, content[:positions[0][0]]...), newline: "\n"}
	if bytes.Contains(content, []byte("\r\n")) {
		a.newline = "\r\n"
	}
	for i, p := range positions {
		end := len(content)
		if i+1 < len(positions) {
			end = positions[i+1][0]
		}
		bodyStart := p[1]
		if bodyStart < end && content[bodyStart] == '\n' {
			bodyStart++
		}
		body := append([]byte{}, content[bodyStart:end]...)
		var node yaml.Node
		if err := yaml.Unmarshal(body, &node); err != nil {
			return nil, err
		}
		d := parsed[i]
		a.docs = append(a.docs, &yamlDocument{d.FileID, d.ClassID, d.Stripped, append([]byte{}, content[p[0]:bodyStart]...), body, node.Content[0].Content[1], d.Data, d.TypeName})
	}
	return a, nil
}
func (a *yamlAsset) bytes() []byte {
	var output bytes.Buffer
	output.Write(a.prefix)
	for _, d := range a.docs {
		output.Write(d.header)
		output.Write(d.body)
	}
	return output.Bytes()
}
func (a *yamlAsset) find(id string) *yamlDocument {
	for _, d := range a.docs {
		if d.id == id {
			return d
		}
	}
	return nil
}
func (a *yamlAsset) replaceFrom(updated *yamlAsset) { *a = *updated }
func (a *yamlAsset) refresh() error {
	updated, err := parseAsset(a.bytes())
	if err == nil {
		a.replaceFrom(updated)
	}
	return err
}
func nodeField(mapping *yaml.Node, name string) (*yaml.Node, int) {
	if mapping.Kind != yaml.MappingNode {
		return nil, -1
	}
	for i := 0; i+1 < len(mapping.Content); i += 2 {
		if mapping.Content[i].Value == name {
			return mapping.Content[i+1], i
		}
	}
	return nil, -1
}
func valueNode(value any) (*yaml.Node, error) {
	var node yaml.Node
	if err := node.Encode(value); err != nil {
		return nil, err
	}
	var normalize func(*yaml.Node)
	normalize = func(n *yaml.Node) {
		if n.Kind == yaml.MappingNode || n.Kind == yaml.SequenceNode {
			n.Style = yaml.FlowStyle
		}
		if n.Kind == yaml.MappingNode {
			for i := 0; i+1 < len(n.Content); i += 2 {
				key, v := n.Content[i], n.Content[i+1]
				if key.Value == "fileID" && v.Kind == yaml.ScalarNode {
					v.Tag = "!!int"
					v.Style = 0
				}
			}
		}
		for _, c := range n.Content {
			normalize(c)
		}
	}
	normalize(&node)
	return &node, nil
}
func encodeNode(node *yaml.Node) (string, error) {
	var buf bytes.Buffer
	encoder := yaml.NewEncoder(&buf)
	encoder.SetIndent(2)
	if err := encoder.Encode(node); err != nil {
		return "", err
	}
	if err := encoder.Close(); err != nil {
		return "", err
	}
	return strings.TrimSuffix(buf.String(), "\n"), nil
}
func lineOffset(body []byte, line int) int {
	if line <= 1 {
		return 0
	}
	line--
	for i, b := range body {
		if b == '\n' {
			line--
			if line == 0 {
				return i + 1
			}
		}
	}
	return len(body)
}
func nodeOffset(body []byte, line, column int) int {
	offset := lineOffset(body, line)
	for i := 1; i < column && offset < len(body); i++ {
		_, width := utf8.DecodeRune(body[offset:])
		offset += width
	}
	return offset
}
func (a *yamlAsset) setTopLevel(id, key string, value any) error {
	d := a.find(id)
	if d == nil {
		return failure("not_found", "serialized object does not exist", map[string]any{"fileId": id})
	}
	_, index := nodeField(d.node, key)
	if index < 0 {
		return failure("unsupported_property", "property does not exist in authored object", map[string]any{"fileId": id, "property": key})
	}
	keyNode := d.node.Content[index]
	start := lineOffset(d.body, keyNode.Line)
	end := len(d.body)
	if index+2 < len(d.node.Content) {
		end = lineOffset(d.body, d.node.Content[index+2].Line)
	}
	// Keep comments and blank lines belonging to the next field byte-for-byte.
	for end > start {
		lastEnd := end
		if d.body[lastEnd-1] == '\n' {
			lastEnd--
		}
		if lastEnd > start && d.body[lastEnd-1] == '\r' {
			lastEnd--
		}
		lastStart := bytes.LastIndexByte(d.body[:lastEnd], '\n') + 1
		trimmed := bytes.TrimSpace(d.body[lastStart:lastEnd])
		if len(trimmed) == 0 || trimmed[0] == '#' {
			end = lastStart
		} else {
			break
		}
	}
	node, err := valueNode(value)
	if err != nil {
		return err
	}
	text, err := encodeNode(node)
	if err != nil {
		return err
	}
	text = strings.ReplaceAll(text, "\n", a.newline+strings.Repeat(" ", keyNode.Column-1))
	replacement := []byte(strings.Repeat(" ", keyNode.Column-1) + key + ": " + text + a.newline)
	d.body = append(append(append([]byte{}, d.body[:start]...), replacement...), d.body[end:]...)
	return a.refresh()
}

var propertySegment = regexp.MustCompile(`^([^\x00\r\n.\[\]]{1,256})(?:\[(\d+)\])?$`)

func parseProperty(path string) ([]string, error) {
	if len(path) > 1024 || path == "" {
		return nil, failure("unsupported_property", "property path is empty or too long", nil)
	}
	parts := strings.Split(path, ".")
	for _, part := range parts {
		if !propertySegment.MatchString(part) {
			return nil, failure("unsupported_property", "use field.subfield or field[index] property paths", nil)
		}
	}
	return parts, nil
}
func resolveProperty(root *yaml.Node, parts []string) (*yaml.Node, error) {
	current := root
	for _, part := range parts {
		match := propertySegment.FindStringSubmatch(part)
		var index int
		current, index = nodeField(current, match[1])
		if index < 0 {
			return nil, failure("unsupported_property", "property path is absent in authored data", nil)
		}
		if match[2] != "" {
			i, err := strconv.Atoi(match[2])
			if err != nil || current.Kind != yaml.SequenceNode || i >= len(current.Content) {
				return nil, failure("unsupported_property", "array element is absent in authored data", nil)
			}
			current = current.Content[i]
		}
	}
	return current, nil
}

// scalarEnd finds the exact lexical extent of an existing scalar. Collection
// changes use setTopLevel; leaf edits leave sibling whitespace/comments intact.
func scalarEnd(body []byte, start int, node *yaml.Node) (int, error) {
	if node.Style&(yaml.LiteralStyle|yaml.FoldedStyle) != 0 {
		return 0, failure("unsupported_property", "block scalar edits require script/file tooling", nil)
	}
	if start >= len(body) {
		return 0, fmt.Errorf("invalid YAML scalar position")
	}
	quote := body[start]
	if quote == '\'' || quote == '"' {
		for i := start + 1; i < len(body); i++ {
			if quote == '"' && body[i] == '\\' {
				i++
				continue
			}
			if body[i] != quote {
				continue
			}
			if quote == '\'' && i+1 < len(body) && body[i+1] == '\'' {
				i++
				continue
			}
			return i + 1, nil
		}
		return 0, fmt.Errorf("unterminated quoted YAML scalar")
	}
	if node.Tag == "!!str" {
		end := start + len(node.Value)
		if end <= len(body) && string(body[start:end]) == node.Value {
			return end, nil
		}
		return 0, failure("unsupported_property", "multiline plain strings are unsupported", nil)
	}
	end := start
	for end < len(body) && body[end] != '\n' && body[end] != '\r' {
		c := body[end]
		if c == ',' || c == '}' || c == ']' || (c == '#' && (end == start || body[end-1] == ' ' || body[end-1] == '\t')) {
			break
		}
		end++
	}
	for end > start && (body[end-1] == ' ' || body[end-1] == '\t') {
		end--
	}
	if end == start {
		return 0, failure("unsupported_property", "empty implicit scalar is unsupported", nil)
	}
	return end, nil
}
func normalizeJSON(value any) (any, error) {
	if raw, ok := value.(json.RawMessage); ok {
		var result any
		decoder := json.NewDecoder(bytes.NewReader(raw))
		decoder.UseNumber()
		if err := decoder.Decode(&result); err != nil {
			return nil, err
		}
		return normalizeJSON(result)
	}
	switch v := value.(type) {
	case bool:
		// Unity's text serializer stores booleans as integer 0/1.
		if v {
			return int64(1), nil
		}
		return int64(0), nil
	case float64:
		if math.IsNaN(v) || math.IsInf(v, 0) {
			return nil, failure("invalid_property_value", "non-finite numbers are unsupported", nil)
		}
		if math.Trunc(v) == v && math.Abs(v) <= 1<<53 {
			return int64(v), nil
		}
		return v, nil
	case json.Number:
		if i, err := v.Int64(); err == nil {
			return i, nil
		}
		if f, err := v.Float64(); err == nil {
			return f, nil
		}
		return nil, fmt.Errorf("invalid JSON number")
	case map[string]any:
		if id, exists := v["fileID"]; exists {
			text, ok := id.(string)
			if !ok {
				return nil, failure("invalid_identity", "fileID values must be decimal strings", nil)
			}
			if _, err := strconv.ParseInt(text, 10, 64); err != nil {
				return nil, failure("invalid_identity", "fileID must fit a signed 64-bit integer", nil)
			}
			if guid, exists := v["guid"]; exists {
				text, ok := guid.(string)
				if !ok || !scriptGUID.MatchString(strings.ToLower(text)) {
					return nil, failure("invalid_identity", "reference GUID must be a 32-hex string", nil)
				}
			}
		}
		out := map[string]any{}
		for key, item := range v {
			normalized, err := normalizeJSON(item)
			if err != nil {
				return nil, err
			}
			if key == "guid" && v["fileID"] != nil {
				normalized = strings.ToLower(normalized.(string))
			}
			out[key] = normalized
		}
		return out, nil
	case []any:
		out := make([]any, len(v))
		for i, item := range v {
			normalized, err := normalizeJSON(item)
			if err != nil {
				return nil, err
			}
			out[i] = normalized
		}
		return out, nil
	default:
		return value, nil
	}
}
func (a *yamlAsset) setProperty(id, path string, value any) (any, error) {
	d := a.find(id)
	if d == nil {
		return nil, failure("not_found", "serialized object does not exist", map[string]any{"fileId": id})
	}
	if d.stripped || idReference(d.data["m_PrefabInstance"]) != "0" {
		return nil, failure("unsupported_prefab_write", "prefab instance and stripped objects require override-aware authoring", map[string]any{"fileId": id})
	}
	parts, err := parseProperty(path)
	if err != nil {
		return nil, err
	}
	for _, part := range parts {
		key := propertySegment.FindStringSubmatch(part)[1]
		if protectedProperty(key) {
			return nil, failure("protected_property", "use graph operations for object identity and ownership", map[string]any{"property": path})
		}
	}
	old, err := resolveProperty(d.node, parts)
	if err != nil {
		return nil, err
	}
	var before any
	if err := old.Decode(&before); err != nil {
		return nil, err
	}
	value, err = normalizeJSON(value)
	if err != nil {
		return nil, err
	}
	if old.Kind != yaml.ScalarNode {
		newNode, err := valueNode(value)
		if err != nil {
			return nil, err
		}
		if newNode.Kind != old.Kind {
			return nil, failure("invalid_property_value", "collection replacement must preserve mapping or array shape", nil)
		}
		if err := a.replaceCollection(id, old, newNode); err != nil {
			return nil, err
		}
		return before, nil
	}
	newNode, err := valueNode(value)
	if err != nil {
		return nil, err
	}
	if newNode.Kind != yaml.ScalarNode {
		return nil, failure("invalid_property_value", "a scalar property requires a scalar value", nil)
	}
	if old.Tag == "!!str" {
		if _, ok := value.(string); !ok {
			return nil, failure("invalid_property_value", "a string property requires a string", nil)
		}
		newNode.Tag = "!!str"
		newNode.Style = yaml.DoubleQuotedStyle
	}
	if old.Tag == "!!int" {
		// Unity serializes integral-valued floats as e.g. x: 0. Lexical YAML
		// integer tags cannot establish a Unity field's numeric type.
		if newNode.Tag != "!!int" && newNode.Tag != "!!float" {
			return nil, failure("invalid_property_value", "a numeric property requires a number", nil)
		}
		if path == "m_Layer" || path == "m_IsActive" || path == "m_Enabled" || path == "m_ObjectHideFlags" {
			if newNode.Tag != "!!int" {
				return nil, failure("invalid_property_value", "this Unity property requires an integer", nil)
			}
		}
	}
	if old.Tag == "!!float" && newNode.Tag != "!!float" && newNode.Tag != "!!int" {
		return nil, failure("invalid_property_value", "a numeric property requires a number", nil)
	}
	start := nodeOffset(d.body, old.Line, old.Column)
	end, err := scalarEnd(d.body, start, old)
	if err != nil {
		return nil, err
	}
	text, err := encodeNode(newNode)
	if err != nil {
		return nil, err
	}
	if strings.Contains(text, "\n") {
		return nil, failure("unsupported_property", "multiline scalar edits are unsupported", nil)
	}
	d.body = append(append(append([]byte{}, d.body[:start]...), []byte(text)...), d.body[end:]...)
	if err := a.refresh(); err != nil {
		return nil, err
	}
	return before, nil
}

// replaceCollection changes the selected collection's lexical span. It keeps
// surrounding fields and comments, including siblings in the same flow mapping.
func (a *yamlAsset) replaceCollection(id string, old, replacement *yaml.Node) error {
	d := a.find(id)
	start := nodeOffset(d.body, old.Line, old.Column)
	end := len(d.body)
	block := old.Style&yaml.FlowStyle == 0
	if !block {
		quote := byte(0)
		depth := 0
		found := false
		for i := start; i < len(d.body); i++ {
			char := d.body[i]
			if quote != 0 {
				if quote == '"' && char == '\\' {
					i++
					continue
				}
				if char == quote {
					if quote == '\'' && i+1 < len(d.body) && d.body[i+1] == '\'' {
						i++
						continue
					}
					quote = 0
				}
				continue
			}
			if char == '\'' || char == '"' {
				quote = char
				continue
			}
			if char == '#' && (i == start || d.body[i-1] == ' ' || d.body[i-1] == '\t') {
				for i < len(d.body) && d.body[i] != '\n' {
					i++
				}
				continue
			}
			if char == '{' || char == '[' {
				depth++
			}
			if char == '}' || char == ']' {
				depth--
				if depth == 0 {
					end, found = i+1, true
					break
				}
			}
		}
		if !found {
			return failure("invalid_yaml", "collection lexical span could not be established", nil)
		}
	} else {
		var visit func(*yaml.Node)
		visit = func(node *yaml.Node) {
			if node == old {
				return
			}
			offset := nodeOffset(d.body, node.Line, node.Column)
			if offset > start {
				line := lineOffset(d.body, node.Line)
				if line < end {
					end = line
				}
			}
			for _, child := range node.Content {
				visit(child)
			}
		}
		visit(d.node)
		for end > start {
			lastEnd := end
			if d.body[lastEnd-1] == '\n' {
				lastEnd--
			}
			if lastEnd > start && d.body[lastEnd-1] == '\r' {
				lastEnd--
			}
			lastStart := bytes.LastIndexByte(d.body[:lastEnd], '\n') + 1
			trimmed := bytes.TrimSpace(d.body[lastStart:lastEnd])
			if len(trimmed) == 0 || trimmed[0] == '#' {
				end = lastStart
			} else {
				break
			}
		}
	}
	text, err := encodeNode(replacement)
	if err != nil {
		return err
	}
	if block {
		text += a.newline
	}
	d.body = append(append(append([]byte{}, d.body[:start]...), []byte(text)...), d.body[end:]...)
	return a.refresh()
}
func protectedProperty(key string) bool {
	switch key {
	case "fileID", "guid", "serializedVersion", "m_GameObject", "m_Component", "m_Children", "m_Father", "m_Roots", "m_Script", "m_PrefabInstance", "m_PrefabAsset", "m_CorrespondingSourceObject", "m_SourcePrefab", "m_Modification", "m_AddedGameObjects", "m_RemovedGameObjects", "m_RemovedComponents", "m_AddedComponents":
		return true
	}
	return false
}
func idReference(value any) string {
	if mapping, ok := value.(map[string]any); ok {
		switch id := mapping["fileID"].(type) {
		case string:
			return id
		case int64:
			return strconv.FormatInt(id, 10)
		case int:
			return strconv.Itoa(id)
		}
	}
	return "0"
}
func externalReference(value any) bool {
	mapping, ok := value.(map[string]any)
	if !ok {
		return false
	}
	guid, _ := mapping["guid"].(string)
	return guid != "" && guid != strings.Repeat("0", 32)
}
func reference(id string) map[string]any { return map[string]any{"fileID": id} }
func referenceList(ids []string) []any {
	result := make([]any, len(ids))
	for i, id := range ids {
		result[i] = reference(id)
	}
	return result
}
func idsFromList(value any) []string {
	values, _ := value.([]any)
	ids := make([]string, 0, len(values))
	for _, v := range values {
		ids = append(ids, idReference(v))
	}
	return ids
}
func removeID(ids []string, remove string) []string {
	out := make([]string, 0, len(ids))
	for _, id := range ids {
		if id != remove {
			out = append(out, id)
		}
	}
	return out
}
