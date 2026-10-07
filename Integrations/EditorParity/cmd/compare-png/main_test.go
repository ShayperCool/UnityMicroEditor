package main

import (
	"image"
	"image/color"
	"testing"
)

func TestFullFrameAlwaysReportsExcludedOverlay(t *testing.T) {
	reference := image.NewNRGBA(image.Rect(0, 0, 2, 2))
	actual := image.NewNRGBA(reference.Bounds())
	for y := 0; y < 2; y++ {
		for x := 0; x < 2; x++ {
			reference.SetNRGBA(x, y, color.NRGBA{A: 255})
			actual.SetNRGBA(x, y, color.NRGBA{A: 255})
		}
	}
	actual.SetNRGBA(1, 1, color.NRGBA{R: 100, A: 255})
	full := measure(reference, actual, reference.Bounds())
	viewport := measure(reference, actual, image.Rect(0, 0, 2, 1))
	if full.ChangedPixels != 1 || full.MaxChannelDifference != 100 || full.MeanAbsoluteChannelDifference != 6.25 {
		t.Fatal(full)
	}
	if viewport.ChangedPixels != 0 {
		t.Fatal(viewport)
	}
}
