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

func TestRepeatedRefusalsOfOneFlowAreOneEvent(t *testing.T) {
	t.Parallel()

	// `ct state new` is not once per connection. A refused TCP handshake is
	// never answered, so the client retransmits its SYN and conntrack still
	// calls each one new — the kernel logs every attempt. The rate limit in
	// the ruleset bounds the journal, not the event count, and the contract
	// these events are read under is one per refused connection.
	ring := strings.Join([]string{kmsgBlocked, kmsgBlocked, kmsgBlocked}, "\n")

	clock := time.Unix(1_700_000_000, 0)
	src := &Blocked{
		BoxID: "myapp",
		Mode:  func() string { return "allowlist" },
		Open: func() (io.ReadCloser, error) {
			return io.NopCloser(strings.NewReader(ring)), nil
		},
		now: func() time.Time { return clock },
	}

	out := make(chan *event.Event, 8)
	if err := src.Run(context.Background(), out); err != nil {
		t.Fatalf("run: %v", err)
	}
	close(out)

	n := 0
	for range out {
		n++
	}
	if n != 1 {
		t.Fatalf("three retransmissions of one flow are one refusal, got %d events", n)
	}
}

func TestTheSameFlowIsReportedAgainAfterTheWindow(t *testing.T) {
	t.Parallel()

	// Suppression must not become silence: a connection refused again ten
	// minutes later is a new fact about the box, not a repeat of an old one.
	ring := strings.Join([]string{kmsgBlocked, kmsgBlocked}, "\n")

	clock := time.Unix(1_700_000_000, 0)
	calls := 0
	src := &Blocked{
		BoxID: "myapp",
		Mode:  func() string { return "allowlist" },
		Open: func() (io.ReadCloser, error) {
			return io.NopCloser(strings.NewReader(ring)), nil
		},
		now: func() time.Time {
			calls++
			if calls > 1 {
				return clock.Add(2 * DedupeWindow)
			}
			return clock
		},
	}

	out := make(chan *event.Event, 8)
	if err := src.Run(context.Background(), out); err != nil {
		t.Fatalf("run: %v", err)
	}
	close(out)

	n := 0
	for range out {
		n++
	}
	if n != 2 {
		t.Fatalf("a refusal after the window is a new event, got %d", n)
	}
}

func TestAnIPv6TargetIsUnambiguous(t *testing.T) {
	t.Parallel()

	// `2001:db8::1` + `:443` parses as another IPv6 address. These strings are
	// displayed and compared as violation identities, so the ambiguous form is
	// both unreadable and unequal to itself across anything that normalises it.
	pkt := BlockedPacket{Dst: "2001:db8::1", DPort: 443}
	if got := pkt.Target(); got != "[2001:db8::1]:443" {
		t.Errorf("target: got %q", got)
	}
	// IPv4 is unchanged.
	if got := (BlockedPacket{Dst: "10.0.0.1", DPort: 80}).Target(); got != "10.0.0.1:80" {
		t.Errorf("target: got %q", got)
	}
}

func TestTheReasonMatchesThePostureThatRefused(t *testing.T) {
	t.Parallel()

	// "outside the allowlist" was said for every posture and is true of one.
	// `isolated` consults no allowlist at all, and `mirror-only` also consults
	// the curated mirrors — so both explanations sent the reader looking for a
	// list entry that was never examined.
	for mode, want := range map[string]string{
		"isolated":    "isolated",
		"mirror-only": "mirrors",
		"allowlist":   "allowlist",
	} {
		got := (BlockedPacket{Dst: "1.2.3.4"}).Event("myapp", mode).Policy.Reason
		if !strings.Contains(got, want) {
			t.Errorf("%s: reason %q should mention %q", mode, got, want)
		}
	}
}

func TestATightenedPostureIsNotSuppressedAsARepeat(t *testing.T) {
	t.Parallel()

	// A UDP socket kept across a policy change keeps its five-tuple. Under an
	// audited `open` the flow was flagged; once the posture tightens the same
	// tuple is genuinely blocked — and a key that ignored the verdict threw
	// that away as a duplicate, hiding the first real denial, which is the one
	// worth seeing.
	flagged := `4,1,1,-;devbox-flagged SRC=10.0.2.15 DST=1.2.3.4 PROTO=UDP SPT=5000 DPT=53`
	blocked := `4,2,2,-;devbox-blocked SRC=10.0.2.15 DST=1.2.3.4 PROTO=UDP SPT=5000 DPT=53`

	clock := time.Unix(1_700_000_000, 0)
	src := &Blocked{
		BoxID: "myapp",
		Mode:  func() string { return "allowlist" },
		Open: func() (io.ReadCloser, error) {
			return io.NopCloser(strings.NewReader(flagged + "\n" + blocked)), nil
		},
		now: func() time.Time { return clock },
	}

	out := make(chan *event.Event, 8)
	if err := src.Run(context.Background(), out); err != nil {
		t.Fatalf("run: %v", err)
	}
	close(out)

	var verdicts []string
	for ev := range out {
		verdicts = append(verdicts, ev.Policy.Verdict)
	}
	if len(verdicts) != 2 || verdicts[0] != "flag" || verdicts[1] != "block" {
		t.Fatalf("a flag and a later block are two facts, got %v", verdicts)
	}
}

func TestTheDedupeWindowOutlastsTheSynRetrySequence(t *testing.T) {
	t.Parallel()

	// Linux retransmits a SYN at 1, 3, 7, 15, 31 and 63 seconds by default and
	// gives up around 127. A one-minute window let the 63-second retry through
	// as a second event for one connection — the duplicate this exists to
	// prevent, arriving late enough to look like a separate refusal.
	if DedupeWindow <= 63*time.Second {
		t.Fatalf("window %v is inside the SYN retry sequence", DedupeWindow)
	}

	clock := time.Unix(1_700_000_000, 0)
	calls := 0
	src := &Blocked{
		BoxID: "myapp",
		Mode:  func() string { return "allowlist" },
		Open: func() (io.ReadCloser, error) {
			return io.NopCloser(strings.NewReader(kmsgBlocked + "\n" + kmsgBlocked)), nil
		},
		now: func() time.Time {
			calls++
			if calls > 1 {
				// The last retransmission of the same handshake.
				return clock.Add(63 * time.Second)
			}
			return clock
		},
	}

	out := make(chan *event.Event, 8)
	if err := src.Run(context.Background(), out); err != nil {
		t.Fatalf("run: %v", err)
	}
	close(out)

	n := 0
	for range out {
		n++
	}
	if n != 1 {
		t.Fatalf("the 63s retransmission belongs to the same refusal, got %d events", n)
	}
}

func TestAvailableAnswersBeforeTheHandshakeCommits(t *testing.T) {
	t.Parallel()

	// The handshake advertises what is being captured, and advertising is a
	// claim. A source that reports itself unsupported once running is skipped
	// silently, so the collector recorded `policy` capture as healthy with
	// nothing producing it — a box missing CAP_SYSLOG looked exactly like a
	// box that never violated its policy.
	ok := &Blocked{Open: func() (io.ReadCloser, error) {
		return io.NopCloser(strings.NewReader("")), nil
	}}
	if err := ok.Available(); err != nil {
		t.Errorf("a readable ring buffer must be available: %v", err)
	}

	denied := &Blocked{Open: func() (io.ReadCloser, error) {
		return nil, io.ErrUnexpectedEOF
	}}
	if denied.Available() == nil {
		t.Error("an unreadable ring buffer must be reported before it is advertised")
	}
}
