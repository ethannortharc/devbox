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
	if !strings.Contains(out.String(), "devbox-ztpd") {
		t.Errorf("version output = %q, want it to name the service", out.String())
	}
}

func TestFlagDefaults(t *testing.T) {
	t.Parallel()

	cfg, err := parseFlags(nil, io.Discard)
	if err != nil {
		t.Fatalf("parseFlags(nil) returned %v", err)
	}
	// Metrics must not share the provisioning port: a node fetching its config
	// should never be able to reach the metrics surface by accident.
	if cfg.listen == cfg.metrics {
		t.Errorf("listen and metrics share %q; they must be separate", cfg.listen)
	}
	if cfg.sotPath == "" {
		t.Error("source-of-truth path must have a default")
	}
}

func TestUnknownFlagIsAnError(t *testing.T) {
	t.Parallel()

	if _, err := parseFlags([]string{"-nope"}, io.Discard); err == nil {
		t.Error("parseFlags accepted an unknown flag")
	}
}
