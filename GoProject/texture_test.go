package goproject

import (
	"bytes"
	"image"
	"image/color"
	"image/png"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const authoredTextureMeta = `fileFormatVersion: 2
guid: 22222222222222222222222222222222
TextureImporter:
  textureType: 0
  textureShape: 1
  alphaUsage: 1
  nPOTScale: 1
  swizzle: 50462976
  isReadable: 0
  mipmaps:
    sRGBTexture: 0
    enableMipMap: 1
  textureSettings:
    filterMode: 0
    wrapU: 1
    wrapV: 2
    aniso: 4
  platformSettings:
  - buildTarget: DefaultTexturePlatform
    maxTextureSize: 2048
    textureFormat: -1
    textureCompression: 0
  - buildTarget: Standalone
    overridden: 0
    maxTextureSize: 2048
    textureFormat: -1
    textureCompression: 1
`

func TestRawTextureUsesEffectiveUncompressedImporter(t *testing.T) {
	session, _, _ := fixtureSession(t)
	session.dependencies = map[string]dependency{}
	var encoded bytes.Buffer
	pixels := image.NewNRGBA(image.Rect(0, 0, 4, 4))
	pixels.SetNRGBA(1, 2, color.NRGBA{R: 9, G: 8, B: 7, A: 6})
	if err := png.Encode(&encoded, pixels); err != nil {
		t.Fatal(err)
	}
	physical := filepath.Join(session.project, "Assets/Texture.png")
	if err := os.WriteFile(physical, encoded.Bytes(), 0644); err != nil {
		t.Fatal(err)
	}
	writeFixtureFile(t, physical+".meta", authoredTextureMeta)
	asset := map[string]any{"path": "Assets/Texture.png", "kind": "texture"}
	if err := session.refreshAsset(asset); err != nil {
		t.Fatal(err)
	}
	var settings map[string]any
	if err := jsonDecodeString(str(asset["importerJson"]), &settings); err != nil {
		t.Fatal(err)
	}
	if number(settings["sRGBTexture"]) != 0 || number(settings["mipmapEnabled"]) != 1 || number(settings["wrapU"]) != 1 || number(settings["wrapV"]) != 2 || number(settings["aniso"]) != 4 {
		t.Fatalf("lost authored linear/mip/sampler settings: %v", settings)
	}
	if actual, _ := os.ReadFile(physical); !bytes.Equal(actual, encoded.Bytes()) {
		t.Fatal("texture source changed")
	}
	writeFixtureFile(t, physical+".meta", strings.Replace(authoredTextureMeta, "overridden: 0", "overridden: 1", 1))
	session.files = map[string]fileRecord{}
	session.observed = map[string]identity{}
	if err := session.refreshAsset(asset); err == nil {
		t.Fatal("accepted effective compressed Standalone override")
	}
}
