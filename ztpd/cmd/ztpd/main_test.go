package main

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
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

func TestBootURLIsAnAddressANodeCanReach(t *testing.T) {
	t.Parallel()

	// A node fetches the bootstrap script over the network, so `0.0.0.0` and
	// `localhost` are both useless here.
	cfg, err := parseFlags([]string{"-advertise", "10.0.0.1", "-listen", ":8080"}, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	if got := cfg.bootURL(); got != "http://10.0.0.1:8080" {
		t.Errorf("bootURL = %q", got)
	}

	// With no advertised address, the lab's service address is the default.
	cfg, _ = parseFlags(nil, io.Discard)
	if got := cfg.bootURL(); !strings.HasPrefix(got, "http://10.") {
		t.Errorf("bootURL = %q, want a routable default", got)
	}
	if strings.Contains(cfg.bootURL(), "0.0.0.0") || strings.Contains(cfg.bootURL(), "localhost") {
		t.Errorf("bootURL = %q, which no node could fetch", cfg.bootURL())
	}
}

func TestLoadCatalogReadsRenderedArtifacts(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "serials.json"),
		[]byte(`{"SN-001":{"Name":"leaf1","Role":"leaf"}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "leaf1.conf"),
		[]byte("hostname leaf1\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	// A stray file must be ignored, not loaded as a config.
	if err := os.WriteFile(filepath.Join(dir, "README.md"), []byte("notes"), 0o600); err != nil {
		t.Fatal(err)
	}

	catalog, err := loadCatalog(dir)
	if err != nil {
		t.Fatalf("loadCatalog: %v", err)
	}

	name, role, ok := catalog.Lookup("SN-001")
	if !ok || name != "leaf1" || role != "leaf" {
		t.Errorf("lookup = %q %q %v", name, role, ok)
	}
	config, ok := catalog.Config("leaf1")
	if !ok || !strings.Contains(config, "hostname leaf1") {
		t.Errorf("config = %q %v", config, ok)
	}
	if _, ok := catalog.Config("README"); ok {
		t.Error("a stray file was loaded as a device config")
	}
}

func TestAnEmptyCatalogIsRefusedRatherThanServed(t *testing.T) {
	t.Parallel()

	// A server that knows no serials can never provision anything, and would
	// otherwise sit there looking healthy.
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "serials.json"), []byte("{}"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := loadCatalog(dir); err == nil {
		t.Error("an empty catalog should be refused")
	}

	if _, err := loadCatalog(filepath.Join(dir, "nope")); err == nil {
		t.Error("a missing directory should be refused")
	}
}

func TestUnknownFlagIsAnError(t *testing.T) {
	t.Parallel()

	if _, err := parseFlags([]string{"-nope"}, io.Discard); err == nil {
		t.Error("parseFlags accepted an unknown flag")
	}
}
