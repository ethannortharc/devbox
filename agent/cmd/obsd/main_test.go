package main

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
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

// TestBackoffClimbsToACeiling covers the part of `dialCollector` that can be
// wrong on its own.
//
// The loop itself needs a socket nobody is listening on and a clock, and the
// defect it exists to prevent is not in the loop: it is that exiting on a dial
// failure let systemd restart the agent, and a restart re-runs the ruleset
// load, which destroys the table and every address DNS capture had resolved
// into the allow sets. Nothing repopulated them, because the agent never
// reached its event loop — so the whole allowlist was re-blocked every two
// seconds for the length of the outage.
func TestBackoffClimbsToACeiling(t *testing.T) {
	t.Parallel()

	const ceiling = 5 * time.Second
	wait := 250 * time.Millisecond
	seen := []time.Duration{wait}
	for i := 0; i < 10; i++ {
		wait = nextBackoff(wait, ceiling)
		seen = append(seen, wait)
	}

	for i := 1; i < len(seen); i++ {
		if seen[i] < seen[i-1] {
			t.Errorf("backoff went backwards: %v then %v", seen[i-1], seen[i])
		}
		if seen[i] > ceiling {
			t.Errorf("backoff %v exceeded the ceiling %v", seen[i], ceiling)
		}
	}
	if got := seen[len(seen)-1]; got != ceiling {
		t.Errorf("backoff should settle at the ceiling, got %v", got)
	}
	// A ceiling below the current wait must clamp down, not run away.
	if got := nextBackoff(time.Hour, ceiling); got != ceiling {
		t.Errorf("nextBackoff(1h, 5s) = %v, want the ceiling", got)
	}
}

// TestDialCollectorWaitsRatherThanLettingTheUnitRestart covers the loop itself,
// not just its arithmetic.
//
// This is the behaviour the whole change is for: exiting here let systemd
// restart the agent, and a restart re-runs the ruleset load, which destroys the
// nftables table and every address DNS capture had resolved into the allow
// sets. Nothing repopulated them because the agent never reached its event
// loop, so the entire allowlist was re-blocked every two seconds for the length
// of the outage.
//
// A real socket and a real absence of one, so the test exercises the same
// `net.Dial` the agent does.
func TestDialCollectorWaitsRatherThanLettingTheUnitRestart(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	// `t.TempDir()` puts the test's name in the path, and a unix socket path is
	// capped near 104 bytes — long enough to fail here and nowhere else, with
	// `net.Listen` reporting "invalid argument" from inside a goroutine while
	// the test merely times out. A short directory keeps the socket bindable.
	dir, err := os.MkdirTemp("", "obsd")
	if err != nil {
		t.Fatalf("temp dir: %v", err)
	}
	defer os.RemoveAll(dir)
	sock := filepath.Join(dir, "c.sock")

	// Nothing is listening when the dial starts; the collector turns up late,
	// which is exactly the host-side restart this is written for.
	listenErr := make(chan error, 1)
	listeners := make(chan net.Listener, 1)
	go func() {
		time.Sleep(400 * time.Millisecond)
		l, err := net.Listen("unix", sock)
		if err != nil {
			listenErr <- err
			return
		}
		listeners <- l
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			c.Close()
		}
	}()

	var out bytes.Buffer
	conn, err := dialCollector(ctx, sock, &out)
	if err != nil {
		select {
		case le := <-listenErr:
			t.Fatalf("the fixture never listened: %v", le)
		default:
			t.Fatalf("dialCollector gave up on a collector that came back: %v", err)
		}
	}
	conn.Close()

	select {
	case l := <-listeners:
		l.Close()
	default:
	}

	// A silent wait is its own failure mode: the operator needs to know the
	// agent is up and enforcing while the collector is not.
	if !strings.Contains(out.String(), "not answering") {
		t.Errorf("the outage was never reported: %q", out.String())
	}
	if !strings.Contains(out.String(), "reachable again") {
		t.Errorf("the recovery was never reported: %q", out.String())
	}
	if !strings.Contains(out.String(), "stays enforced") {
		t.Errorf("the message should say enforcement survives: %q", out.String())
	}
}

// TestDialCollectorStopsWhenAsked keeps the wait from outliving a shutdown, and
// reports the dial failure rather than the cancellation — the former is the
// actionable half.
func TestDialCollectorStopsWhenAsked(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	sock := filepath.Join(t.TempDir(), "nobody-is-here.sock")
	_, err := dialCollector(ctx, sock, io.Discard)
	if err == nil {
		t.Fatal("dialCollector returned a connection to nothing")
	}
	if !strings.Contains(err.Error(), "connect to the collector") {
		t.Errorf("error should name the dial, not the cancellation: %v", err)
	}
}

// TestCaptureAndEnforcementRunWithoutACollector is the point of decoupling the
// transport.
//
// The agent used to wait for the collector before starting capture, so during
// an outage the nft allow sets — which `loadEnforcer` creates *empty*, and
// which only captured DNS answers fill — stayed empty and an allowlist posture
// blocked every domain it promised to permit. Enforcement depends on capture;
// capture must not depend on the transport.
func TestCaptureAndEnforcementRunWithoutACollector(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	cfg := config{
		// Nothing is listening here, and nothing ever will be.
		socket:  filepath.Join(t.TempDir(), "absent.sock"),
		boxID:   "box-under-test",
		fixture: "../../event/testdata/events.jsonl",
		queue:   16,
	}

	source, err := chooseSource(cfg)
	if err != nil {
		t.Fatalf("chooseSource: %v", err)
	}

	var out bytes.Buffer
	err = stream(ctx, cfg, source, &out)
	// Ending because we asked it to stop is the pass condition. Ending because
	// it could not reach the collector is the bug.
	if err != nil && !errors.Is(err, context.DeadlineExceeded) && !errors.Is(err, context.Canceled) {
		t.Fatalf("stream aborted rather than carrying on without a collector: %v", err)
	}

	log := out.String()
	if strings.Contains(log, "connect to the collector") && !strings.Contains(log, "not answering") {
		t.Errorf("the dial failure was treated as fatal: %q", log)
	}
	// The source ran to completion with nowhere to send — which is the loop
	// having processed its events, and the loop is where enforcement happens.
	// Asserted on that rather than on a drop count: events are held now, and a
	// fixture smaller than the queue drops none of them.
	if !strings.Contains(log, "event(s) sent") {
		t.Errorf("capture never ran without a collector: %q", log)
	}
}
