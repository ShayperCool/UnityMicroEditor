// compare-png measures screenshot differences; it does not modify either image.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"image"
	_ "image/png"
	"os"
)

type metrics struct {
	Pixels                        int     `json:"pixels"`
	ChangedPixels                 int     `json:"changedPixels"`
	MaxChannelDifference          int     `json:"maxChannelDifference"`
	MeanAbsoluteChannelDifference float64 `json:"meanAbsoluteChannelDifference"`
}

func measure(reference, actual image.Image, bounds image.Rectangle) metrics {
	result := metrics{Pixels: bounds.Dx() * bounds.Dy()}
	var total int64
	for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
		for x := bounds.Min.X; x < bounds.Max.X; x++ {
			ar, ag, ab, aa := reference.At(x, y).RGBA()
			br, bg, bb, ba := actual.At(x, y).RGBA()
			a := [4]uint32{ar, ag, ab, aa}
			b := [4]uint32{br, bg, bb, ba}
			changed := false
			for c := range a {
				difference := int(a[c]>>8) - int(b[c]>>8)
				if difference < 0 {
					difference = -difference
				}
				total += int64(difference)
				if difference > result.MaxChannelDifference {
					result.MaxChannelDifference = difference
				}
				if difference != 0 {
					changed = true
				}
			}
			if changed {
				result.ChangedPixels++
			}
		}
	}
	if result.Pixels != 0 {
		result.MeanAbsoluteChannelDifference = float64(total) / float64(4*result.Pixels)
	}
	return result
}

func read(path string) (image.Image, string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, "", err
	}
	if len(data) > 32*1024*1024 {
		return nil, "", errors.New("PNG exceeds 32 MiB")
	}
	config, _, err := image.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return nil, "", err
	}
	if config.Width < 1 || config.Height < 1 || config.Width > 16000000/config.Height {
		return nil, "", errors.New("PNG exceeds 16 megapixels")
	}
	value, _, err := image.Decode(bytes.NewReader(data))
	if err != nil {
		return nil, "", err
	}
	hash := sha256.Sum256(data)
	return value, hex.EncodeToString(hash[:]), nil
}

func run() error {
	referencePath := flag.String("reference", "", "Editor PNG")
	actualPath := flag.String("actual", "", "Player PNG")
	ignoreBottom := flag.Int("ignore-bottom", 0, "explicit bottom rows excluded from an additional viewport metric; full-frame differences remain reported")
	output := flag.String("output", "", "optional report path")
	flag.Parse()
	reference, referenceHash, err := read(*referencePath)
	if err != nil {
		return err
	}
	actual, actualHash, err := read(*actualPath)
	if err != nil {
		return err
	}
	if reference.Bounds() != actual.Bounds() {
		return errors.New("screenshots have different dimensions; capture the same dimensions before comparing")
	}
	bounds := reference.Bounds()
	if *ignoreBottom < 0 || *ignoreBottom >= bounds.Dy() {
		return errors.New("invalid ignore-bottom row count")
	}
	report := map[string]any{"schemaVersion": 1, "reference": *referencePath, "actual": *actualPath, "referenceSHA256": referenceHash, "actualSHA256": actualHash, "width": bounds.Dx(), "height": bounds.Dy(), "fullFrame": measure(reference, actual, bounds), "ignoredBottomRows": *ignoreBottom}
	if *ignoreBottom > 0 {
		viewport := bounds
		viewport.Max.Y -= *ignoreBottom
		report["viewportWithoutBottomRows"] = measure(reference, actual, viewport)
	}
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	if *output != "" {
		if err := os.WriteFile(*output, data, 0600); err != nil {
			return err
		}
	}
	_, err = os.Stdout.Write(data)
	return err
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
