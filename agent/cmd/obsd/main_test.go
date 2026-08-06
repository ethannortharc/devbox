package main

import (
	"bytes"
	"io"
	"strings"
	"testing"
)

func TestVersionFlagPrintsIdentity(t *testing.T) {
	t.Parallel()

	var out bytes.Buffer
	if err := run([]string{"-version"}, &out); err != nil {
		t.Fatalf("run(-version) returned %v", err)
	}
	if !strings.Contains(out.String(), "devbox-obsd") {
		t.Errorf("version output = %q, want it to name the service", out.String())
	}
}

func TestBoxIDIsRequired(t *testing.T) {
	t.Parallel()

	err := run(nil, io.Discard)
	if err == nil || !strings.Contains(err.Error(), "-box-id") {
		t.Errorf("run() without -box-id returned %v, want a box-id error", err)
	}
}

func TestFlagDefaults(t *testing.T) {
	t.Parallel()

	cfg, err := parseFlags(nil, io.Discard)
	if err != nil {
		t.Fatalf("parseFlags(nil) returned %v", err)
	}
	if cfg.socket != "/run/devbox/obsd.sock" {
		t.Errorf("default socket = %q", cfg.socket)
	}
	if cfg.noEBPF {
		t.Error("eBPF must be enabled by default; -no-ebpf is the degraded path")
	}
}

func TestFlagsParse(t *testing.T) {
	t.Parallel()

	cfg, err := parseFlags([]string{
		"-box-id", "myapp",
		"-socket", "vsock://2:1024",
		"-no-ebpf",
	}, io.Discard)
	if err != nil {
		t.Fatalf("parseFlags returned %v", err)
	}
	if cfg.boxID != "myapp" {
		t.Errorf("boxID = %q, want myapp", cfg.boxID)
	}
	if cfg.socket != "vsock://2:1024" {
		t.Errorf("socket = %q", cfg.socket)
	}
	if !cfg.noEBPF {
		t.Error("-no-ebpf did not take effect")
	}
}

func TestUnknownFlagIsAnError(t *testing.T) {
	t.Parallel()

	if _, err := parseFlags([]string{"-nope"}, io.Discard); err == nil {
		t.Error("parseFlags accepted an unknown flag")
	}
}
