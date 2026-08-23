package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/capture"
	"github.com/ethannortharc/devbox/agent/event"
	"github.com/ethannortharc/devbox/agent/transport"
)

type statusPacket struct{}

func (statusPacket) Name() string { return "packet" }
func (statusPacket) Domains() []event.Type {
	return []event.Type{event.TypeDNS, event.TypeTLS}
}
func (statusPacket) Run(context.Context, chan<- *event.Event) error { return nil }

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
	if !cfg.packet {
		t.Error("packet capture must be enabled by default; DNS policy depends on it")
	}
	if cfg.stdio {
		t.Error("stdio is selected only by the host supervisor")
	}
	if cfg.noTransport || !cfg.restore {
		t.Errorf("unexpected lifecycle defaults: %+v", cfg)
	}
	if cfg.queue < 1 {
		t.Errorf("queue depth = %d; the buffer must be bounded but non-empty", cfg.queue)
	}
}

func TestRestorePolicyFlagControlsFirewallOwnership(t *testing.T) {
	t.Parallel()

	cfg, err := parseFlags([]string{"-restore-policy=false"}, io.Discard)
	if err != nil {
		t.Fatalf("parseFlags returned %v", err)
	}
	if ownsPolicy(cfg) {
		t.Fatal("an exec observer with restoration disabled still owns DNS allow-set updates")
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

func TestPCAPFlagsAreBoundedAndExclusive(t *testing.T) {
	t.Parallel()

	cfg, err := parseFlags([]string{
		"-pcap", "-pcap-proto", "udp",
		"-pcap-saddr", "10.0.0.2", "-pcap-sport", "53000",
		"-pcap-daddr", "1.1.1.1", "-pcap-dport", "53",
		"-pcap-duration", "3s", "-pcap-packets", "12",
	}, io.Discard)
	if err != nil {
		t.Fatalf("parse pcap flags: %v", err)
	}
	if !cfg.pcap || cfg.pcapProto != "udp" || cfg.pcapDPort != 53 || cfg.pcapPackets != 12 {
		t.Fatalf("pcap cfg = %+v", cfg)
	}
	for _, args := range [][]string{
		{"-pcap", "-stdio"},
		{"-pcap", "-no-transport"},
		{"-pcap", "-fixture", "events.jsonl"},
		{"-pcap", "-pcap-dport", "70000"},
	} {
		if _, err := parseFlags(args, io.Discard); err == nil {
			t.Fatalf("accepted incompatible pcap flags %v", args)
		}
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

	proc, err := chooseSource(config{boxID: "b", noEBPF: true, packet: false})
	if err != nil {
		t.Fatalf("proc source: %v", err)
	}
	if proc.Name() != "proc" {
		t.Errorf("source = %q, want proc", proc.Name())
	}

	// Capture preflight must always leave a usable source. A portable build
	// degrades to /proc; an eBPF build may do the same when this test process
	// lacks the kernel capabilities needed to attach probes.
	ebpf, err := chooseSource(config{boxID: "b", packet: false})
	if err != nil {
		t.Fatalf("automatic source: %v", err)
	}
	if ebpf.Name() != "ebpf" && ebpf.Name() != "proc" {
		t.Fatalf("automatic source = %q, want ebpf or proc fallback", ebpf.Name())
	}
	if !capture.EBPFBuilt() && ebpf.Name() != "proc" {
		t.Fatalf("portable build source = %q, want proc", ebpf.Name())
	}
	if sourceIncludes(ebpf, "ebpf") != (ebpf.Name() == "ebpf") {
		t.Fatalf("sourceIncludes disagrees with selected source %q", ebpf.Name())
	}
}

func TestCaptureStatusReportsEffectiveDomainsNotRequestedFlags(t *testing.T) {
	t.Parallel()

	cfg := config{boxID: "b", packet: true, policy: "/etc/devbox/policy.json"}
	degraded := currentCaptureStatus(cfg, &capture.Proc{BoxID: "b"})
	for _, domain := range degraded.Capture {
		if domain == "dns" {
			t.Fatalf("proc fallback falsely advertised DNS: %+v", degraded)
		}
	}

	recovered := currentCaptureStatus(cfg, capture.NewMulti(
		&capture.Proc{BoxID: "b"},
		statusPacket{},
	))
	if !contains(recovered.Capture, "dns") || !contains(recovered.Capture, "tls") {
		t.Fatalf("live packet source was not published: %+v", recovered)
	}
	if !recovered.PolicyConfigured {
		t.Fatal("the status lost that the DNS feed is wired to policy")
	}
}

func TestCaptureStatusIsAtomicReadableAndOwnerScoped(t *testing.T) {
	t.Parallel()

	path := filepath.Join(t.TempDir(), "run", "obsd-status.json")
	status := captureStatus{PID: os.Getpid(), BoxID: "b", Capture: []string{"dns"}}
	encoded, err := json.Marshal(status)
	if err != nil {
		t.Fatal(err)
	}
	if err := writeCaptureStatus(path, encoded); err != nil {
		t.Fatalf("writeCaptureStatus: %v", err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var got captureStatus
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("published partial JSON: %v", err)
	}
	if got.PID != os.Getpid() || !contains(got.Capture, "dns") {
		t.Fatalf("status = %+v", got)
	}
	removeCaptureStatus(path, os.Getpid()+1)
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("another process removed this status: %v", err)
	}
	removeCaptureStatus(path, os.Getpid())
	if !os.IsNotExist(statError(path)) {
		t.Fatalf("owner status still exists at %s", path)
	}
}

func contains(values []string, want string) bool {
	for _, value := range values {
		if value == want {
			return true
		}
	}
	return false
}

func statError(path string) error {
	_, err := os.Stat(path)
	return err
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

// TestFramesReachAListeningCollector runs the agent against a real socket.
//
// Nothing in this package did, and that gap has a cost. The write deadline was
// added by rewriting every `transport.WriteFrame(conn, …)` call — including the
// one inside the wrapper being introduced, so `writeFrame` called itself. It
// compiled, `go vet` was clean, and every test here passed, because not one of
// them ever reached a collector that would accept a frame. The only thing that
// caught it was a Rust test in another language's suite.
//
// A fake collector is a listener, a handshake and a frame count. That is cheap
// enough that its absence was an oversight rather than a decision.
func TestFramesReachAListeningCollector(t *testing.T) {
	t.Parallel()

	// Short path: a unix socket is capped near 104 bytes and `t.TempDir()`
	// spends most of that on the test's name.
	dir, err := os.MkdirTemp("", "obsd")
	if err != nil {
		t.Fatalf("temp dir: %v", err)
	}
	defer os.RemoveAll(dir)
	sock := filepath.Join(dir, "c.sock")

	listener, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer listener.Close()

	frames := make(chan int, 1)
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			frames <- -1
			return
		}
		defer conn.Close()

		var hello transport.Hello
		if err := transport.ReadJSON(conn, &hello); err != nil {
			frames <- -1
			return
		}
		if err := transport.WriteJSON(conn, transport.HelloAck{
			Accepted: true,
			Protocol: transport.ProtocolVersion,
		}); err != nil {
			frames <- -1
			return
		}
		n := 0
		for {
			if _, err := transport.ReadFrame(conn); err != nil {
				frames <- n
				return
			}
			n++
		}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	cfg := config{
		socket:  sock,
		boxID:   "box-under-test",
		fixture: "../../event/testdata/events.jsonl",
		queue:   64,
		once:    true,
	}
	source, err := chooseSource(cfg)
	if err != nil {
		t.Fatalf("chooseSource: %v", err)
	}

	var out bytes.Buffer
	if err := stream(ctx, cfg, source, &out); err != nil {
		t.Fatalf("stream: %v\n%s", err, out.String())
	}

	// The agent has exited, so the collector's read side is done too.
	select {
	case n := <-frames:
		if n <= 0 {
			t.Fatalf("the collector received %d frames\n%s", n, out.String())
		}
		if !strings.Contains(out.String(), "event(s) sent") {
			t.Errorf("the agent did not report sending: %q", out.String())
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("the collector never finished reading\n%s", out.String())
	}
}

func TestStdioMonitorTreatsHostEOFAsSessionEnd(t *testing.T) {
	t.Parallel()

	agent, host := net.Pipe()
	defer agent.Close()
	done := make(chan error, 1)
	go func() { done <- monitorStdio(context.Background(), agent) }()

	if err := transport.WriteFrame(host, nil); err != nil {
		t.Fatalf("heartbeat: %v", err)
	}
	select {
	case err := <-done:
		t.Fatalf("a valid heartbeat ended the session: %v", err)
	case <-time.After(20 * time.Millisecond):
	}
	_ = host.Close()
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("host EOF looked like a healthy session")
		}
	case <-time.After(time.Second):
		t.Fatal("idle agent did not notice the host disappeared")
	}
}
