package capture

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// ── fixture source ───────────────────────────────────────

func writeFixture(t *testing.T, lines ...string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "events.jsonl")
	if err := os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o600); err != nil {
		t.Fatalf("write fixture: %v", err)
	}
	return path
}

const execLine = `{"ts_wall":"2026-08-06T22:00:00.000Z","ts_mono_ns":1,"box_id":"orig","pid":1,"tid":1,"ppid":0,"comm":"sh","uid":0,"type":"exec","exec":{"path":"/bin/sh"}}`
const exitLine = `{"ts_wall":"2026-08-06T22:00:01.000Z","ts_mono_ns":2,"box_id":"orig","pid":1,"tid":1,"ppid":0,"comm":"sh","uid":0,"type":"exit"}`

func collect(t *testing.T, src Source, want int, timeout time.Duration) []*event.Event {
	t.Helper()

	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	out := make(chan *event.Event, want+8)
	done := make(chan error, 1)
	go func() { done <- src.Run(ctx, out) }()

	var got []*event.Event
	for len(got) < want {
		select {
		case e := <-out:
			got = append(got, e)
		case err := <-done:
			if err != nil && ctx.Err() == nil {
				t.Fatalf("%s source failed: %v", src.Name(), err)
			}
			// Drain anything already queued before giving up.
			for {
				select {
				case e := <-out:
					got = append(got, e)
					continue
				default:
				}
				return got
			}
		case <-ctx.Done():
			return got
		}
	}
	return got
}

func TestFixtureReplaysEveryEvent(t *testing.T) {
	t.Parallel()

	src := &Fixture{Path: writeFixture(t, execLine, exitLine)}
	got := collect(t, src, 2, 5*time.Second)

	if len(got) != 2 {
		t.Fatalf("got %d events, want 2", len(got))
	}
	if got[0].Type != event.TypeExec || got[1].Type != event.TypeExit {
		t.Errorf("replay order changed: %s then %s", got[0].Type, got[1].Type)
	}
}

func TestFixtureOverridesTheBoxID(t *testing.T) {
	t.Parallel()

	src := &Fixture{Path: writeFixture(t, execLine), BoxID: "myapp"}
	got := collect(t, src, 1, 5*time.Second)

	if len(got) != 1 {
		t.Fatalf("got %d events", len(got))
	}
	if got[0].BoxID != "myapp" {
		t.Errorf("box_id = %q, want the override", got[0].BoxID)
	}
}

func TestFixtureSkipsBlanksAndComments(t *testing.T) {
	t.Parallel()

	src := &Fixture{Path: writeFixture(t, "# a comment", "", execLine, "   ")}
	got := collect(t, src, 1, 5*time.Second)

	if len(got) != 1 {
		t.Fatalf("got %d events, want 1", len(got))
	}
}

func TestFixtureRejectsAMalformedLine(t *testing.T) {
	t.Parallel()

	src := &Fixture{Path: writeFixture(t, execLine, "{not json}")}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	out := make(chan *event.Event, 8)
	err := src.Run(ctx, out)
	if err == nil || !strings.Contains(err.Error(), "line 2") {
		t.Errorf("expected an error naming line 2, got %v", err)
	}
}

func TestFixtureRejectsAnInvalidEvent(t *testing.T) {
	t.Parallel()

	// Structurally valid JSON, but an exec with no exec sub-object.
	bad := `{"ts_wall":"t","ts_mono_ns":1,"box_id":"b","pid":1,"type":"exec"}`
	src := &Fixture{Path: writeFixture(t, bad)}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := src.Run(ctx, make(chan *event.Event, 8)); err == nil {
		t.Error("an invalid event must not be replayed")
	}
}

func TestFixtureMissingFileIsAnError(t *testing.T) {
	t.Parallel()

	src := &Fixture{Path: filepath.Join(t.TempDir(), "nope.jsonl")}
	if err := src.Run(context.Background(), make(chan *event.Event, 1)); err == nil {
		t.Error("a missing fixture must be an error")
	}
}

func TestSendRespectsCancellation(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	// An unbuffered channel with no reader: only cancellation can unblock it.
	err := Send(ctx, make(chan *event.Event), &event.Event{})
	if err == nil {
		t.Error("Send must return once the context is cancelled")
	}
}

// ── proc source ──────────────────────────────────────────

func TestProcDomainsAreHonestAboutCoverage(t *testing.T) {
	t.Parallel()

	p := &Proc{}
	domains := p.Domains()
	// Polling cannot see DNS, TLS, or file access. Claiming it can would make
	// an empty timeline look like "nothing happened".
	for _, absent := range []event.Type{event.TypeDNS, event.TypeTLS, event.TypeFile} {
		for _, d := range domains {
			if d == absent {
				t.Errorf("proc capture must not claim %s", absent)
			}
		}
	}
	if len(domains) == 0 {
		t.Error("proc capture claims nothing at all")
	}
}

func TestParseStatus(t *testing.T) {
	t.Parallel()

	info := ParseStatus(strings.Join([]string{
		"Name:\tpip",
		"Umask:\t0022",
		"State:\tS (sleeping)",
		"Tgid:\t812",
		"Pid:\t812",
		"PPid:\t640",
		"Uid:\t1000\t1000\t1000\t1000",
		"Gid:\t100\t100\t100\t100",
	}, "\n"))

	if info.Comm != "pip" {
		t.Errorf("Comm = %q", info.Comm)
	}
	if info.PPID != 640 {
		t.Errorf("PPID = %d", info.PPID)
	}
	if info.UID != 1000 {
		t.Errorf("UID = %d — the *real* uid is the first field", info.UID)
	}
}

func TestParseStatusIgnoresGarbage(t *testing.T) {
	t.Parallel()

	info := ParseStatus("not a status file\n\n:::\nPPid:\tnonsense\n")
	if info.PPID != 0 || info.Comm != "" {
		t.Errorf("garbage should parse to zero values, got %+v", info)
	}
}

func TestParseCmdline(t *testing.T) {
	t.Parallel()

	argv := ParseCmdline([]byte("python3.12\x00-m\x00pip\x00install\x00requests\x00"))
	want := []string{"python3.12", "-m", "pip", "install", "requests"}
	if len(argv) != len(want) {
		t.Fatalf("argv = %v, want %v", argv, want)
	}
	for i := range want {
		if argv[i] != want[i] {
			t.Errorf("argv[%d] = %q, want %q", i, argv[i], want[i])
		}
	}

	if got := ParseCmdline(nil); len(got) != 0 {
		t.Errorf("an empty cmdline should yield nothing, got %v", got)
	}
}

func TestParseNetTCPDecodesLittleEndianAddresses(t *testing.T) {
	t.Parallel()

	// 0100007F:1F90 is 127.0.0.1:8080 — each 4-byte group is a little-endian
	// u32, which is the detail a naive parser gets backwards.
	text := strings.Join([]string{
		"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode",
		"   0: 0100007F:1F90 0500000A:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 54321 1 0000 10 0 0 10 -1",
		"   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000 10 0 0 10 -1",
	}, "\n")

	conns, err := ParseNetTCP(text, false)
	if err != nil {
		t.Fatalf("ParseNetTCP: %v", err)
	}
	if len(conns) != 2 {
		t.Fatalf("got %d connections, want 2", len(conns))
	}

	c := conns[0]
	if c.LocalAddr != "127.0.0.1" || c.LocalPort != 8080 {
		t.Errorf("local = %s:%d, want 127.0.0.1:8080", c.LocalAddr, c.LocalPort)
	}
	if c.RemoteAddr != "10.0.0.5" || c.RemotePort != 443 {
		t.Errorf("remote = %s:%d, want 10.0.0.5:443", c.RemoteAddr, c.RemotePort)
	}
	if c.State != TCPEstablished {
		t.Errorf("state = %d, want ESTABLISHED", c.State)
	}
	if c.Inode != 54321 {
		t.Errorf("inode = %d", c.Inode)
	}

	if conns[1].State == TCPEstablished {
		t.Error("a listening socket must not read as established")
	}
}

func TestParseNetTCPSkipsMalformedRows(t *testing.T) {
	t.Parallel()

	text := strings.Join([]string{
		"  sl  local_address rem_address   st",
		"garbage",
		"   0: ZZZZ:1F90 0500000A:01BB 01 x x x x x 1",
		"   1: 0100007F:1F90 0500000A:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 7 1",
	}, "\n")

	conns, err := ParseNetTCP(text, false)
	if err != nil {
		t.Fatalf("ParseNetTCP: %v", err)
	}
	if len(conns) != 1 {
		t.Fatalf("got %d connections, want only the well-formed one", len(conns))
	}
}

func TestScanProcsReadsAFakeProcfs(t *testing.T) {
	t.Parallel()

	root := t.TempDir()
	pid := filepath.Join(root, "812")
	if err := os.MkdirAll(pid, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(pid, "status"),
		[]byte("Name:\tpip\nPPid:\t640\nUid:\t1000\t1000\t1000\t1000\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(pid, "cmdline"),
		[]byte("pip\x00install\x00requests\x00"), 0o600); err != nil {
		t.Fatal(err)
	}
	// A non-pid directory must be ignored, not error.
	if err := os.MkdirAll(filepath.Join(root, "self"), 0o755); err != nil {
		t.Fatal(err)
	}

	procs, err := ScanProcs(root)
	if err != nil {
		t.Fatalf("ScanProcs: %v", err)
	}
	if len(procs) != 1 {
		t.Fatalf("got %d processes, want 1", len(procs))
	}

	info := procs[812]
	if info.Comm != "pip" || info.PPID != 640 || info.UID != 1000 {
		t.Errorf("info = %+v", info)
	}
	if len(info.Argv) != 3 {
		t.Errorf("argv = %v", info.Argv)
	}
	if info.Exe != "pip" {
		t.Errorf("Exe = %q — with no exe link, argv[0] is the best name", info.Exe)
	}
}

func TestScanProcsOnAMissingRoot(t *testing.T) {
	t.Parallel()

	if _, err := ScanProcs(filepath.Join(t.TempDir(), "nope")); err == nil {
		t.Error("a missing procfs must be an error")
	}
}

func TestProcPollsEstablishedConnections(t *testing.T) {
	t.Parallel()

	// The claim in `Domains()` has to be true: `-no-ebpf` advertises connect
	// coverage, so the poll loop must actually read /proc/net/tcp.
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "net"), 0o755); err != nil {
		t.Fatal(err)
	}
	tcp := strings.Join([]string{
		"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode",
		"   0: 0100007F:1F90 0500000A:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 54321 1",
		"   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1",
	}, "\n")
	if err := os.WriteFile(filepath.Join(root, "net", "tcp"), []byte(tcp), 0o600); err != nil {
		t.Fatal(err)
	}

	p := &Proc{Root: root, BoxID: "myapp", Boot: time.Now()}
	out := make(chan *event.Event, 8)
	seen := map[string]struct{}{}

	if err := p.pollConnections(context.Background(), out, seen); err != nil {
		t.Fatalf("pollConnections: %v", err)
	}
	close(out)

	var events []*event.Event
	for e := range out {
		events = append(events, e)
	}
	if len(events) != 1 {
		t.Fatalf("got %d events, want only the established one", len(events))
	}

	e := events[0]
	if err := e.Validate(); err != nil {
		t.Fatalf("a polled connection must be a valid event: %v", err)
	}
	if e.Type != event.TypeConnect {
		t.Errorf("type = %s", e.Type)
	}
	if e.Net.DAddr != "10.0.0.5" || e.Net.DPort != 443 {
		t.Errorf("peer = %s:%d", e.Net.DAddr, e.Net.DPort)
	}
	if e.BoxID != "myapp" {
		t.Errorf("box_id = %q", e.BoxID)
	}

	// A second sweep sees the same socket and must not report it twice.
	out2 := make(chan *event.Event, 8)
	if err := p.pollConnections(context.Background(), out2, seen); err != nil {
		t.Fatal(err)
	}
	close(out2)
	if len(out2) != 0 {
		t.Errorf("the same connection was reported twice")
	}
}

func TestProcConnectionPollingToleratesAMissingTcp6(t *testing.T) {
	t.Parallel()

	// A kernel built without IPv6 has no /proc/net/tcp6; that is not a reason
	// to end capture.
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "net"), 0o755); err != nil {
		t.Fatal(err)
	}
	p := &Proc{Root: root, BoxID: "b", Boot: time.Now()}
	if err := p.pollConnections(context.Background(), make(chan *event.Event, 1), map[string]struct{}{}); err != nil {
		t.Errorf("pollConnections: %v", err)
	}
}
