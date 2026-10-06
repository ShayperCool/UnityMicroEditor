package main

import "testing"

func TestPlayerRequiresHardwareRenderer(t *testing.T) {
	for _, device := range []string{
		"NVIDIA GeForce RTX 4060/PCIe/SSE2",
		"AMD Radeon RX 6600 (radeonsi, LLVM 20.1.0, DRM 3.61)",
		"Mesa Intel(R) Xe Graphics (TGL GT2)",
	} {
		if err := requireHardwareGraphics(device, "OpenGLCore"); err != nil {
			t.Fatalf("hardware renderer %q rejected: %v", device, err)
		}
	}
	for _, candidate := range []struct{ device, kind string }{
		{"", "OpenGLCore"},
		{"Null Device", "Null"},
		{"llvmpipe (LLVM 20.1.0, 256 bits)", "OpenGLCore"},
		{"softpipe", "OpenGLCore"},
		{"lavapipe", "Vulkan"},
		{"Google SwiftShader", "Vulkan"},
		{"GDI Generic", "OpenGLCore"},
		{"Microsoft Basic Render Driver", "Direct3D11"},
	} {
		if err := requireHardwareGraphics(candidate.device, candidate.kind); err == nil {
			t.Fatalf("missing/software renderer accepted: %+v", candidate)
		}
	}
}
