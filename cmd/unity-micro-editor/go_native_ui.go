package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"regexp"
	"strconv"
	"strings"
	"time"

	goproject "unitymicroeditor/internal/project"
)

var uiDumpExternal = regexp.MustCompile(`^path\(([0-9]+)\): .* GUID: ([0-9a-f]{32}) Type: ([0-9]+)$`)
var uiDumpObject = regexp.MustCompile(`^ID: (-?[0-9]+) \(ClassID: 114\) `)
var uiDumpManaged = regexp.MustCompile(`^rid (-?[0-9]+) \(class: `)

type uiDumpFrame struct {
	indent          int
	path, managedID string
	array           bool
	next            int
	pointer         bool
	fileIndex       int
}

// Only external PPtrs already authored by the native UI importer are restored.
// Rules, selectors, values, UXML attributes and element scopes remain native.
func parseNativeUIBindings(dump []byte, file goproject.NativeContentFile) ([]goproject.NativeUIBinding, error) {
	if len(dump) > 16<<20 {
		return nil, errors.New("native UI dump exceeds bounds")
	}
	owners := map[string]bool{}
	kinds := map[string]string{}
	for _, object := range file.Objects {
		owners[object.FileID] = object.TypeName == "UnityEngine.UIElements.StyleSheet" || object.TypeName == "UnityEngine.UIElements.VisualTreeAsset"
		kinds[object.FileID] = object.TypeName
	}
	externals := map[int]string{}
	var result []goproject.NativeUIBinding
	var frames []*uiDumpFrame
	owner := ""
	managedIndex := 0
	scanner := bufio.NewScanner(bytes.NewReader(dump))
	scanner.Buffer(make([]byte, 4096), 1<<20)
	for scanner.Scan() {
		line := scanner.Text()
		if m := uiDumpExternal.FindStringSubmatch(line); len(m) != 0 {
			index, _ := strconv.Atoi(m[1])
			if index != len(externals)+1 || index > len(file.Dependencies) || file.Dependencies[index-1].GUID != m[2] {
				return nil, errors.New("native UI dump external table differs from descriptor")
			}
			externals[index] = m[2]
			continue
		}
		if strings.HasPrefix(line, "ID: ") {
			m := uiDumpObject.FindStringSubmatch(line)
			if len(m) == 0 {
				return nil, errors.New("native UI dump has a non-UI object")
			}
			owner = m[1]
			if !owners[owner] {
				return nil, errors.New("native UI dump owner identity differs from descriptor")
			}
			frames = nil
			managedIndex = 0
			continue
		}
		if owner == "" || strings.TrimSpace(line) == "" {
			continue
		}
		indent := len(line) - len(strings.TrimLeft(line, "\t"))
		text := strings.TrimSpace(line)
		for len(frames) > 0 && frames[len(frames)-1].indent >= indent {
			frames = frames[:len(frames)-1]
		}
		var parent *uiDumpFrame
		if len(frames) > 0 {
			parent = frames[len(frames)-1]
		}
		if m := uiDumpManaged.FindStringSubmatch(text); len(m) != 0 {
			id, err := strconv.ParseInt(m[1], 10, 64)
			if err != nil {
				return nil, err
			}
			if id < 0 {
				continue
			}
			if parent == nil || parent.path != "references" || managedIndex > 16383 {
				return nil, errors.New("native UI managed registry layout is unsupported")
			}
			frames = append(frames, &uiDumpFrame{indent: indent, path: fmt.Sprintf("references.RefIds[%d].data", managedIndex), managedID: m[1]})
			managedIndex++
			continue
		}
		marker := strings.LastIndex(text, " (")
		if marker < 0 {
			// binary2text prints the managed object's class name as a wrapper.
			if parent != nil && parent.managedID != "" {
				frames = append(frames, &uiDumpFrame{indent: indent, path: parent.path, managedID: parent.managedID})
			}
			continue
		}
		label := strings.TrimSpace(text[:marker])
		typ := strings.TrimSuffix(text[marker+2:], ")")
		name, value := label, ""
		if split := strings.IndexByte(label, ' '); split >= 0 {
			name, value = label[:split], strings.TrimSpace(label[split+1:])
		}
		if name == "size" {
			count, err := strconv.Atoi(value)
			if parent == nil || err != nil || count < 0 || count > 16384 {
				return nil, errors.New("native UI array bounds are invalid")
			}
			parent.array = true
			parent.next = 0
			continue
		}
		if parent != nil && parent.pointer && (name == "m_FileID" || name == "m_PathID") {
			if name == "m_FileID" {
				index, err := strconv.Atoi(value)
				if err != nil || index < 0 || index > len(file.Dependencies) {
					return nil, errors.New("native UI PPtr external index is invalid")
				}
				parent.fileIndex = index
				continue
			}
			id, err := strconv.ParseInt(value, 10, 64)
			if err != nil {
				return nil, errors.New("native UI PPtr local ID is invalid")
			}
			if id == 0 {
				continue
			}
			if parent.fileIndex == 0 {
				if kinds[strconv.FormatInt(id, 10)] == "UnityEngine.UIElements.StyleSheet" {
					result = append(result, goproject.NativeUIBinding{OwnerFileID: owner, PropertyPath: parent.path, TargetGUID: file.GUID, TargetFileID: strconv.FormatInt(id, 10), ManagedReferenceID: parent.managedID})
				}
				continue
			}
			dep := file.Dependencies[parent.fileIndex-1]
			if dep.Global {
				continue
			}
			guid := externals[parent.fileIndex]
			if guid == "" {
				return nil, errors.New("native UI PPtr has no external identity")
			}
			result = append(result, goproject.NativeUIBinding{OwnerFileID: owner, PropertyPath: parent.path, TargetGUID: guid, TargetFileID: strconv.FormatInt(id, 10), ManagedReferenceID: parent.managedID})
			if len(result) > 4096 {
				return nil, errors.New("native UI PPtr count exceeds bounds")
			}
			continue
		}
		path := name
		managed := ""
		if parent != nil {
			managed = parent.managedID
			path = parent.path + "." + name
			if name == "data" {
				if !parent.array {
					return nil, errors.New("native UI data item has no array")
				}
				path = fmt.Sprintf("%s[%d]", parent.path, parent.next)
				parent.next++
			}
		}
		frames = append(frames, &uiDumpFrame{indent: indent, path: path, managedID: managed, pointer: strings.HasPrefix(typ, "PPtr<")})
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	if len(externals) != len(file.Dependencies) {
		return nil, errors.New("native UI dump external table is incomplete")
	}
	return result, nil
}

// Each process asks the pinned standalone reader to verify the exact file. The
// small UI files are never sent to an Editor, importer or AssetBundle builder.
func (importer *nativeProjectImporter) bindNativeUIReferences(ctx context.Context, entry goproject.NativeReplacement) (goproject.NativeReplacement, error) {
	profile := strings.ToLower(filepath.Ext(entry.SourcePath))
	if profile != ".uxml" && profile != ".uss" {
		return entry, nil
	}
	data, err := readOwnedNativeMeshFile(importer.opts.Project, entry.Content.Path)
	if err != nil || bytesSHA(data) != entry.Content.SHA256 {
		return entry, errors.New("native UI binding input changed")
	}
	root := filepath.Join(importer.opts.Output, "native/ui-bindings")
	if err = os.MkdirAll(root, 0700); err != nil {
		return entry, err
	}
	destination, err := os.CreateTemp(root, ".native-ui-*.txt")
	if err != nil {
		return entry, err
	}
	name := destination.Name()
	destination.Close()
	os.Remove(name)
	defer os.Remove(name)
	bounded, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	reader := filepath.Join(filepath.Dir(importer.opts.Editor), "Data/Tools/binary2text")
	readerSHA, err := goFileHash(reader)
	if err != nil {
		return entry, err
	}
	command := exec.CommandContext(bounded, reader, entry.Content.Path, name)
	if log, err := command.CombinedOutput(); err != nil {
		if len(log) > 4096 {
			log = log[:4096]
		}
		return entry, fmt.Errorf("native UI standalone reader: %w: %s", err, log)
	}
	info, err := os.Lstat(name)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 16<<20 {
		return entry, errors.New("native UI reader output exceeds bounds")
	}
	dump, err := os.ReadFile(name)
	if err != nil {
		return entry, err
	}
	bindings, err := parseNativeUIBindings(dump, entry.Content)
	if err != nil {
		return entry, fmt.Errorf("native UI bindings %s: %w", entry.SourcePath, err)
	}
	after, err := readOwnedNativeMeshFile(importer.opts.Project, entry.Content.Path)
	if err != nil || !bytes.Equal(data, after) {
		return entry, errors.New("native UI input changed during standalone read")
	}
	if len(entry.Content.UIBindings) != 0 && !reflect.DeepEqual(entry.Content.UIBindings, bindings) {
		return entry, errors.New("native UI cached bindings differ from exact native PPtrs")
	}
	readerAfter, err := goFileHash(reader)
	if err != nil || readerAfter != readerSHA {
		return entry, errors.New("native UI reader changed during extraction")
	}
	encoded, err := json.Marshal(bindings)
	if err != nil {
		return entry, err
	}
	proof := &goproject.NativeUIBindingProof{SchemaVersion: 1, Mode: "native-ui-pptr-bindings-v1", NativeSHA256: entry.Content.SHA256, ReaderSHA256: readerSHA, DumpSHA256: bytesSHA(dump), BindingsSHA256: bytesSHA(encoded)}
	if entry.Content.UIBindingProof != nil && !reflect.DeepEqual(entry.Content.UIBindingProof, proof) {
		return entry, errors.New("native UI cached proof differs from current exact native input/reader")
	}
	entry.Content.UIBindings = bindings
	entry.Content.UIBindingProof = proof
	return entry, nil
}
