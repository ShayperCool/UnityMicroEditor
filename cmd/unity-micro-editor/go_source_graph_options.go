package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"unicode"

	"gopkg.in/yaml.v3"
)

type goSourcePlugin struct {
	Path        string
	Enabled     bool
	Explicit    bool
	Constraints []string
}

type goSourcePluginPlatform struct {
	First  map[string]string `yaml:"first"`
	Second struct {
		Enabled  int               `yaml:"enabled"`
		Settings map[string]string `yaml:"settings"`
	} `yaml:"second"`
}

func decodeGoSourcePluginPlatforms(node yaml.Node) ([]goSourcePluginPlatform, error) {
	if node.Kind == 0 {
		return nil, nil
	}
	if node.Kind == yaml.SequenceNode {
		var result []goSourcePluginPlatform
		if err := node.Decode(&result); err != nil {
			return nil, err
		}
		return result, nil
	}
	if node.Kind != yaml.MappingNode {
		return nil, errors.New("unsupported PluginImporter platformData shape")
	}
	var result []goSourcePluginPlatform
	for index := 0; index < len(node.Content); index += 2 {
		name := node.Content[index].Value
		platform := goSourcePluginPlatform{First: map[string]string{"": name}}
		if name == "Linux64" {
			platform.First = map[string]string{"Standalone": name}
		}
		if err := node.Content[index+1].Decode(&platform.Second); err != nil {
			return nil, err
		}
		result = append(result, platform)
	}
	return result, nil
}

func readGoSourcePlugin(path string, defines []string) (goSourcePlugin, error) {
	result := goSourcePlugin{Path: path}
	content, err := readAgentFile(path+".meta", 32<<20)
	if os.IsNotExist(err) {
		return result, nil
	}
	if err != nil {
		return result, err
	}
	var metadata struct {
		Plugin *struct {
			Constraints []string  `yaml:"defineConstraints"`
			Explicit    int       `yaml:"isExplicitlyReferenced"`
			Platforms   yaml.Node `yaml:"platformData"`
		} `yaml:"PluginImporter"`
	}
	// Unity's legacy PluginImporter emits an unquoted empty mapping key. Its
	// public text serialization accepts that spelling; yaml.v3 requires quotes.
	parserContent := regexp.MustCompile(`(?m)^([ \t]+):([ \t]+[^\r\n]*)$`).ReplaceAll(content, []byte("${1}'':${2}"))
	if err := yaml.Unmarshal(parserContent, &metadata); err != nil {
		return result, fmt.Errorf("plugin metadata %s: %w", path, err)
	}
	if metadata.Plugin == nil {
		// GUID-only metadata keeps Unity's default managed-plugin importer:
		// all Player platforms, unless the asset is in an Editor folder.
		result.Enabled = !editorGoSource(path)
		return result, nil
	}
	result.Explicit = metadata.Plugin.Explicit != 0
	result.Constraints = metadata.Plugin.Constraints
	platforms, err := decodeGoSourcePluginPlatforms(metadata.Plugin.Platforms)
	if err != nil {
		return result, fmt.Errorf("plugin metadata %s: %w", path, err)
	}
	anyEnabled := false
	explicitLinux := false
	linuxEnabled := false
	for _, platform := range platforms {
		for group, target := range platform.First {
			if target == "Any" || group == "Any" {
				anyEnabled = platform.Second.Enabled != 0
				if platform.Second.Settings["Exclude Linux64"] == "1" {
					anyEnabled = false
				}
			}
			if group == "Standalone" && target == "Linux64" {
				explicitLinux = true
				linuxEnabled = platform.Second.Enabled != 0
			}
		}
	}
	result.Enabled = anyEnabled
	if explicitLinux {
		result.Enabled = linuxEnabled
	}
	constraints, err := matchGoSourceConstraints(result.Constraints, defines)
	if err != nil {
		return result, err
	}
	result.Enabled = result.Enabled && constraints
	return result, nil
}

func goSourcePluginReferences(config goAssemblyConfig, plugins map[string]goSourcePlugin) ([]string, error) {
	result := slices.Clone(config.CompilerReferences)
	if config.Config.OverrideReferences {
		for _, name := range config.Config.Precompiled {
			plugin, exists := plugins[name]
			if !exists {
				return nil, errors.New("explicit precompiled assembly is unavailable for Linux Mono: " + name)
			}
			result = append(result, plugin.Path)
		}
	} else {
		for _, plugin := range plugins {
			if !plugin.Explicit {
				result = append(result, plugin.Path)
			}
		}
	}
	return sortedGoSourceStrings(result), nil
}

func splitGoSourceArguments(text string) ([]string, error) {
	var result []string
	var token strings.Builder
	quoted := false
	escaped := false
	for _, r := range text {
		if escaped {
			token.WriteRune(r)
			escaped = false
			continue
		}
		if r == '\\' && quoted {
			escaped = true
			continue
		}
		if r == '"' {
			quoted = !quoted
			continue
		}
		if unicode.IsSpace(r) && !quoted {
			if token.Len() > 0 {
				result = append(result, token.String())
				token.Reset()
			}
			continue
		}
		token.WriteRune(r)
	}
	if quoted || escaped {
		return nil, errors.New("unterminated response-file argument")
	}
	if token.Len() > 0 {
		result = append(result, token.String())
	}
	return result, nil
}

func applyGoSourceResponseFiles(project string, sources, authoredArguments []string, config *goAssemblyConfig, plugins map[string]goSourcePlugin, inputs map[string]string) error {
	paths := map[string]bool{filepath.Join(project, "Assets/csc.rsp"): true}
	if len(sources) > 0 {
		common := filepath.Dir(sources[0])
		for _, source := range sources[1:] {
			for !withinDirectory(source, common) && filepath.Dir(common) != common {
				common = filepath.Dir(common)
			}
		}
		paths[filepath.Join(common, "csc.rsp")] = true
	}
	arguments := slices.Clone(authoredArguments)
	responsePaths := make([]string, 0, len(paths))
	for path := range paths {
		responsePaths = append(responsePaths, path)
	}
	slices.Sort(responsePaths)
	for _, path := range responsePaths {
		content, err := readAgentFile(path, 1<<20)
		if os.IsNotExist(err) {
			inputs[path] = "absent"
			continue
		}
		if err != nil {
			return err
		}
		inputs[path] = bytesSHA(content)
		config.ResponseFiles = append(config.ResponseFiles, path)
		parsed, err := splitGoSourceArguments(string(content))
		if err != nil {
			return fmt.Errorf("%s: %w", path, err)
		}
		arguments = append(arguments, parsed...)
	}
	for _, argument := range arguments {
		option, value, hasValue := strings.Cut(strings.TrimLeft(argument, "-/"), ":")
		lower := strings.ToLower(option)
		switch lower {
		case "define", "d":
			if !hasValue {
				return errors.New("response define requires value")
			}
			for _, define := range strings.FieldsFunc(value, func(r rune) bool { return r == ';' || r == ',' }) {
				if !goSourceSymbol.MatchString(define) || strings.HasPrefix(define, "UNITY_EDITOR") {
					return errors.New("invalid Player response define: " + define)
				}
				config.Defines = append(config.Defines, define)
			}
		case "unsafe+", "unsafe":
			config.Config.AllowUnsafe = true
		case "unsafe-":
			config.Config.AllowUnsafe = false
		case "reference", "r":
			if !hasValue || strings.Contains(value, "=") {
				return errors.New("response references require one unaliased managed plugin")
			}
			name := filepath.Base(value)
			plugin, ok := plugins[name]
			if !ok {
				return errors.New("response reference is outside active managed plugins: " + value)
			}
			if value != name && filepath.Clean(filepath.Join(project, value)) != plugin.Path {
				return errors.New("response reference does not match its canonical plugin: " + value)
			}
			config.CompilerReferences = append(config.CompilerReferences, plugin.Path)
		case "langversion":
			if !hasValue || !(value == "9" || value == "9.0") {
				return errors.New("pinned lifecycle SDK currently requires C#9 response langversion")
			}
		case "nullable":
			if value != "enable" && value != "disable" && value != "warnings" && value != "annotations" {
				return errors.New("invalid nullable option")
			}
			config.CompilerOptions = append(config.CompilerOptions, "-nullable:"+value)
		case "nowarn", "warn", "w", "warnaserror+", "warnaserror-", "warnaserror":
			if hasValue && !regexp.MustCompile(`^[0-9A-Za-z_,;]+$`).MatchString(value) {
				return errors.New("invalid warning compiler option")
			}
			config.CompilerOptions = append(config.CompilerOptions, "-"+option)
			if hasValue {
				config.CompilerOptions[len(config.CompilerOptions)-1] += " :" + value
				config.CompilerOptions[len(config.CompilerOptions)-1] = strings.ReplaceAll(config.CompilerOptions[len(config.CompilerOptions)-1], " :", ":")
			}
		case "optimize+", "optimize-", "checked+", "checked-", "deterministic+":
			config.CompilerOptions = append(config.CompilerOptions, "-"+lower)
		default:
			return errors.New("unsupported response/compiler option must not silently change SDK behavior: " + argument)
		}
	}
	config.Defines = sortedGoSourceStrings(config.Defines)
	config.ResponseFiles = sortedGoSourceStrings(config.ResponseFiles)
	return nil
}

type goSourceVersion struct {
	numbers   [3]int
	channel   string
	iteration int
	pre       string
}

var goSourceVersionPattern = regexp.MustCompile(`^(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:([abfpcx])(\d+)|-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$`)

func parseGoSourceVersion(text string) (goSourceVersion, error) {
	var result goSourceVersion
	match := goSourceVersionPattern.FindStringSubmatch(text)
	if match == nil {
		return result, errors.New("invalid Unity/package version: " + text)
	}
	for index := 0; index < 3; index++ {
		if match[index+1] != "" {
			value, err := strconv.Atoi(match[index+1])
			if err != nil {
				return result, err
			}
			result.numbers[index] = value
		}
	}
	result.channel = match[4]
	result.iteration, _ = strconv.Atoi(match[5])
	result.pre = match[6]
	return result, nil
}
func compareGoSourceVersion(first, second goSourceVersion) int {
	for index := 0; index < 3; index++ {
		if first.numbers[index] < second.numbers[index] {
			return -1
		}
		if first.numbers[index] > second.numbers[index] {
			return 1
		}
	}
	if first.pre != second.pre {
		if first.pre == "" {
			return 1
		}
		if second.pre == "" {
			return -1
		}
		a, b := strings.Split(first.pre, "."), strings.Split(second.pre, ".")
		for index := 0; index < len(a) && index < len(b); index++ {
			if a[index] == b[index] {
				continue
			}
			ai, ae := strconv.Atoi(a[index])
			bi, be := strconv.Atoi(b[index])
			if ae == nil && be == nil {
				if ai < bi {
					return -1
				}
				return 1
			}
			if ae == nil {
				return -1
			}
			if be == nil {
				return 1
			}
			if a[index] < b[index] {
				return -1
			}
			return 1
		}
		if len(a) < len(b) {
			return -1
		}
		return 1
	}
	rank := func(channel string) int {
		switch channel {
		case "a":
			return 0
		case "b":
			return 1
		case "f":
			return 2
		case "p", "c", "x":
			return 3
		}
		return 4
	}
	if rank(first.channel) < rank(second.channel) {
		return -1
	}
	if rank(first.channel) > rank(second.channel) {
		return 1
	}
	if first.iteration < second.iteration {
		return -1
	}
	if first.iteration > second.iteration {
		return 1
	}
	return 0
}
func matchGoSourceVersion(current, expression string) (bool, error) {
	version, err := parseGoSourceVersion(current)
	if err != nil {
		return false, err
	}
	expression = strings.TrimSpace(expression)
	if expression == "" {
		return true, nil
	}
	if expression[0] == '[' || expression[0] == '(' {
		if len(expression) < 2 || (expression[len(expression)-1] != ']' && expression[len(expression)-1] != ')') {
			return false, errors.New("invalid version interval: " + expression)
		}
		body := expression[1 : len(expression)-1]
		lower, upper, comma := strings.Cut(body, ",")
		if !comma {
			if expression[0] != '[' || expression[len(expression)-1] != ']' {
				return false, errors.New("exact version must use brackets")
			}
			exact, err := parseGoSourceVersion(strings.TrimSpace(body))
			return compareGoSourceVersion(version, exact) == 0, err
		}
		if strings.TrimSpace(lower) != "" {
			bound, err := parseGoSourceVersion(strings.TrimSpace(lower))
			if err != nil {
				return false, err
			}
			comparison := compareGoSourceVersion(version, bound)
			if comparison < 0 || comparison == 0 && expression[0] == '(' {
				return false, nil
			}
		}
		if strings.TrimSpace(upper) != "" {
			bound, err := parseGoSourceVersion(strings.TrimSpace(upper))
			if err != nil {
				return false, err
			}
			comparison := compareGoSourceVersion(version, bound)
			if comparison > 0 || comparison == 0 && expression[len(expression)-1] == ')' {
				return false, nil
			}
		}
		return true, nil
	}
	minimum, err := parseGoSourceVersion(expression)
	return compareGoSourceVersion(version, minimum) >= 0, err
}
