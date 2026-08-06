package main

import (
	"bytes"
	"io"
	"strings"
	"testing"
	"time"
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
	if cfg.queue < 1 {
		t.Errorf("queue depth = %d; the buffer must be bounded but non-empty", cfg.queue)
	}
}

func TestFlagsParse(t *testing.T) {
	t.Parallel()

	cfg, err := parseFlags([]string{
		"-box-id", "myapp",
		"-socket", "/tmp/obsd.sock",
		"-no-ebpf",
		"-fixture", "/tmp/events.jsonl",
		"-replay-interval", "5ms",
		"-queue", "16",
	}, io.Discard)
	if err != nil {
		t.Fatalf("parseFlags returned %v", err)
	}
	if cfg.boxID != "myapp" || cfg.socket != "/tmp/obsd.sock" {
		t.Errorf("cfg = %+v", cfg)
	}
	if !cfg.noEBPF || cfg.fixture == "" {
		t.Errorf("cfg = %+v", cfg)
	}
	if cfg.replayInterval != 5*time.Millisecond {
		t.Errorf("replayInterval = %v", cfg.replayInterval)
	}
	if cfg.queue != 16 {
		t.Errorf("queue = %d", cfg.queue)
	}
}

func TestUnknownFlagIsAnError(t *testing.T) {
	t.Parallel()

	if _, err := parseFlags([]string{"-nope"}, io.Discard); err == nil {
		t.Error("parseFlags accepted an unknown flag")
	}
}

func TestZeroQueueIsRejected(t *testing.T) {
	t.Parallel()

	if _, err := parseFlags([]string{"-queue", "0"}, io.Discard); err == nil {
		t.Error("a zero-depth queue would deadlock the agent")
	}
}

func TestVsockIsRejectedUntilItExists(t *testing.T) {
	t.Parallel()

	// Better to say so than to fail later with a confusing dial error.
	_, err := parseFlags([]string{"-socket", "vsock://2:1024"}, io.Discard)
	if err == nil || !strings.Contains(err.Error(), "vsock") {
		t.Errorf("expected a clear vsock error, got %v", err)
	}
}

func TestSourceSelection(t *testing.T) {
	t.Parallel()

	fixture, err := chooseSource(config{boxID: "b", fixture: "/tmp/x.jsonl"})
	if err != nil {
		t.Fatalf("fixture source: %v", err)
	}
	if fixture.Name() != "fixture" {
		t.Errorf("source = %q, want fixture", fixture.Name())
	}

	proc, err := chooseSource(config{boxID: "b", noEBPF: true})
	if err != nil {
		t.Fatalf("proc source: %v", err)
	}
	if proc.Name() != "proc" {
		t.Errorf("source = %q, want proc", proc.Name())
	}

	// Asking for eBPF from a binary built without it must fail loudly rather
	// than quietly degrade — a quiet timeline reads as "nothing happened".
	if _, err := chooseSource(config{boxID: "b"}); err == nil {
		t.Error("eBPF capture should refuse rather than silently fall back")
	}
}
