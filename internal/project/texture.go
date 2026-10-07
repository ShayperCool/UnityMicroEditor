package goproject

import (
	"bytes"
	"fmt"
	"image"
	_ "image/jpeg"
	_ "image/png"

	"gopkg.in/yaml.v3"
)

func (s *Session) refreshAuthoredTexture(asset map[string]any) error {
	path := str(asset["path"])
	physical, err := s.source(path)
	if err != nil {
		return err
	}
	if err := s.depend(path); err != nil {
		return err
	}
	content, _, err := s.read(physical, true)
	if err != nil {
		return err
	}
	config, _, err := image.DecodeConfig(bytes.NewReader(content))
	if err != nil {
		return fmt.Errorf("raw texture %s: %w", path, err)
	}
	meta, _, err := s.read(physical+".meta", true)
	if err != nil {
		return err
	}
	var node yaml.Node
	if err := yaml.Unmarshal(meta, &node); err != nil {
		return err
	}
	if len(node.Content) != 1 {
		return fmt.Errorf("raw texture %s has invalid meta", path)
	}
	value, err := yamlValue(node.Content[0], "", 0)
	if err != nil {
		return err
	}
	importer := object(object(value)["TextureImporter"])
	if len(importer) == 0 || number(importer["textureType"]) != 0 || number(importer["textureShape"]) != 1 || number(importer["alphaUsage"]) != 1 {
		return &NeedsImportError{Path: path}
	}
	for _, key := range []string{"alphaIsTransparency", "grayScaleToAlpha", "streamingMipmaps", "vTOnly", "flipGreenChannel", "ignorePngGamma", "applyGammaDecoding"} {
		if number(importer[key]) != 0 {
			return &NeedsImportError{Path: path}
		}
	}
	if swizzle, exists := importer["swizzle"]; exists && number(swizzle) != 50462976 {
		return &NeedsImportError{Path: path}
	}
	settings := map[string]any{}
	for _, item := range array(importer["platformSettings"]) {
		platform := object(item)
		if str(platform["buildTarget"]) == "DefaultTexturePlatform" {
			settings = platform
		}
	}
	for _, item := range array(importer["platformSettings"]) {
		platform := object(item)
		if str(platform["buildTarget"]) == "Standalone" && number(platform["overridden"]) != 0 {
			settings = platform
		}
	}
	if len(settings) == 0 || number(settings["textureCompression"]) != 0 || number(settings["crunchedCompression"]) != 0 || number(settings["textureFormat"]) != -1 {
		return &NeedsImportError{Path: path}
	}
	maximum := int(number(settings["maxTextureSize"]))
	if maximum <= 0 || config.Width > maximum || config.Height > maximum || config.Width <= 0 || config.Height <= 0 || config.Width*config.Height > 16*1024*1024 {
		return &NeedsImportError{Path: path}
	}
	if number(importer["nPOTScale"]) != 0 && (config.Width&(config.Width-1) != 0 || config.Height&(config.Height-1) != 0) {
		return &NeedsImportError{Path: path}
	}
	mipmap := object(importer["mipmaps"])
	for _, key := range []string{"mipMapMode", "fadeOut", "borderMipMap", "mipMapsPreserveCoverage"} {
		if number(mipmap[key]) != 0 {
			return &NeedsImportError{Path: path}
		}
	}
	sampler := object(importer["textureSettings"])
	if number(sampler["mipBias"]) != 0 {
		return &NeedsImportError{Path: path}
	}
	normalized := map[string]any{"textureType": 0, "textureShape": 1, "sRGBTexture": mipmap["sRGBTexture"], "mipmapEnabled": mipmap["enableMipMap"], "isReadable": importer["isReadable"], "filterMode": sampler["filterMode"], "wrapU": sampler["wrapU"], "wrapV": sampler["wrapV"], "aniso": sampler["aniso"]}
	asset["fileId"], asset["classId"], asset["typeName"] = "2800000", 28, "UnityEngine.Texture2D"
	asset["importerJson"], err = compactJSON(normalized)
	return err
}
