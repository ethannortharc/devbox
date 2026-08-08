package capture

import (
	"context"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// A real `/dev/kmsg` line for a packet the devbox ruleset dropped.
//
// Kept verbatim rather than assembled from parts: the value of this fixture is
// that it is the format the kernel actually emits, and a constructed one would
// only prove the parser agrees with my idea of it.
const kmsgBlocked = `4,1183,948312771,-;devbox-blocked IN= OUT=eth0 SRC=10.0.2.15 ` +
	`DST=93.184.216.34 LEN=60 TOS=0x00 PREC=0x00 TTL=64 ID=54321 DF PROTO=TCP ` +
	`SPT=51234 DPT=443 WINDOW=64240 RES=0x00 SYN URGP=0`

const kmsgFlagged = `4,1184,948312999,-;devbox-flagged IN= OUT=eth0 SRC=10.0.2.15 ` +
	`DST=140.82.121.4 LEN=60 TOS=0x00 PREC=0x00 TTL=64 ID=11111 DF PROTO=TCP ` +
	`SPT=51235 DPT=80 WINDOW=64240 RES=0x00 SYN URGP=0`

func TestParseBlockedReadsTheKernelsFormat(t *testing.T) {
	t.Parallel()

	pkt, ok := ParseBlocked(kmsgBlocked)
	if !ok {
		t.Fatal("a devbox-blocked line must parse")
	}
	if pkt.Dst != "93.184.216.34" || pkt.DPort != 443 {
		t.Errorf("destination is the whole point: got %s:%d", pkt.Dst, pkt.DPort)
	}
	if pkt.Src != "10.0.2.15" || pkt.SPort != 51234 {
		t.Errorf("source: got %s:%d", pkt.Src, pkt.SPort)
	}
	if pkt.Proto != "tcp" {
		t.Errorf("proto: got %q", pkt.Proto)
	}
	if pkt.Flagged {
		t.Error("a blocked line is a refusal, not an observation")
	}
	if pkt.Target() != "93.184.216.34:443" {
		t.Errorf("target: got %q", pkt.Target())
	}
}

func TestParseBlockedDistinguishesAnObservationFromARefusal(t *testing.T) {
	t.Parallel()

	// Two prefixes rather than one, so the agent never has to infer the
	// verdict from a posture it reads out of a separate file. Under `open`
	// nothing was blocked — the connection happened and was merely noticed.
	pkt, ok := ParseBlocked(kmsgFlagged)
	if !ok {
		t.Fatal("a devbox-flagged line must parse")
	}
	if !pkt.Flagged {
		t.Fatal("an open-posture audit line is not a refusal")
	}

	ev := pkt.Event("myapp", "open")
	if ev.Policy.Verdict != "flag" {
		t.Errorf("verdict: got %q, want flag", ev.Policy.Verdict)
	}
	if ev.Policy.Mode != "open" {
		t.Errorf("mode: got %q", ev.Policy.Mode)
	}
}

func TestParseBlockedIgnoresEverythingElseInTheRingBuffer(t *testing.T) {
	t.Parallel()

	// /dev/kmsg carries the whole kernel log. Anything that is not ours must
	// produce no event at all rather than an empty one.
	for _, line := range []string{
		`6,1,12345,-;Linux version 6.8.0`,
		`4,2,12346,-;some-other-tool DST=1.2.3.4`,
		``,
		`4,3,12347,-;devbox-blocked IN= OUT=eth0 SRC=10.0.2.15 PROTO=TCP`, // no DST
	} {
		if _, ok := ParseBlocked(line); ok {
			t.Errorf("must not produce an event: %q", line)
		}
	}
}

func TestABlockedEventValidates(t *testing.T) {
	t.Parallel()

	// The collector rejects malformed events, and a producer whose output it
	// refuses is a producer that silently does nothing — which is the failure
	// this whole source exists to end.
	pkt, _ := ParseBlocked(kmsgBlocked)
	ev := pkt.Event("myapp", "allowlist")

	if err := ev.Validate(); err != nil {
		t.Fatalf("the collector would refuse this event: %v", err)
	}
	if ev.Type != event.TypePolicy {
		t.Errorf("type: got %q", ev.Type)
	}
	// The kernel logs a packet, not a process. No plausible pid may be
	// invented — see UnattributedPID.
	if ev.PID != UnattributedPID {
		t.Errorf("pid: got %d, want the unattributed sentinel", ev.PID)
	}
}

func TestBlockedStreamsOneEventPerLoggedRefusal(t *testing.T) {
	t.Parallel()

	ring := strings.Join([]string{
		`6,1,1,-;Linux version 6.8.0`,
		kmsgBlocked,
		`6,2,2,-;something unrelated`,
		kmsgFlagged,
	}, "\n")

	src := &Blocked{
		BoxID: "myapp",
		Mode:  func() string { return "allowlist" },
		Open: func() (io.ReadCloser, error) {
			return io.NopCloser(strings.NewReader(ring)), nil
		},
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	out := make(chan *event.Event, 8)
	done := make(chan error, 1)
	go func() { done <- src.Run(ctx, out) }()

	if err := <-done; err != nil {
		t.Fatalf("run: %v", err)
	}
	close(out)

	var got []*event.Event
	for ev := range out {
		got = append(got, ev)
	}
	if len(got) != 2 {
		t.Fatalf("want one event per devbox line, got %d", len(got))
	}
	if got[0].Policy.Verdict != "block" || got[1].Policy.Verdict != "flag" {
		t.Errorf("verdicts: %q then %q", got[0].Policy.Verdict, got[1].Policy.Verdict)
	}
}

func TestAnUnreadableRingBufferDoesNotKillCapture(t *testing.T) {
	t.Parallel()

	// /dev/kmsg needs privileges the primary source does not, and this feed is
	// supplementary — the kernel enforces the posture whether or not anyone is
	// reading the log. Taking the whole capture down over it would trade a
	// missing feed for a blind box.
	src := &Blocked{
		BoxID: "myapp",
		Open:  func() (io.ReadCloser, error) { return nil, io.ErrUnexpectedEOF },
	}

	err := src.Run(context.Background(), make(chan *event.Event, 1))
	var unsupported *ErrUnsupported
	if !asErrUnsupported(err, &unsupported) {
		t.Fatalf("want ErrUnsupported so Multi can skip it, got %v", err)
	}
}

func asErrUnsupported(err error, target **ErrUnsupported) bool {
	u, ok := err.(*ErrUnsupported)
	if ok {
		*target = u
	}
	return ok
}
