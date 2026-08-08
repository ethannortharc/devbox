package capture

import (
	"bufio"
	"context"
	"io"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
	"github.com/ethannortharc/devbox/agent/policy"
)

// Log prefixes the generated nftables ruleset writes.
//
// Two, because the two cases mean different things and the console shows them
// differently: an enforcing posture *refused* the connection, while `open`
// with alerts on merely *noticed* it. One prefix would have made the agent
// guess the verdict from a posture it reads separately, which is the sort of
// agreement-between-two-places this codebase keeps getting wrong.
//
// Must match `policy::nftables::{BLOCK_LOG_PREFIX, FLAG_LOG_PREFIX}`; a test
// on the Rust side pins the spelling from that end.
const (
	BlockedPrefix = "devbox-blocked"
	FlaggedPrefix = "devbox-flagged"
)

// KmsgPath is the kernel ring buffer, read from the start of what is retained.
const KmsgPath = "/dev/kmsg"

// BlockedPacket is what one nftables log line says about a refused connection.
//
// The kernel logs the packet, not the process: there is no pid here and no way
// to recover one, which is why the event this becomes carries
// [UnattributedPID] rather than a plausible guess.
type BlockedPacket struct {
	Proto string
	Src   string
	SPort uint16
	Dst   string
	DPort uint16
	// Flagged distinguishes an `open`-posture audit from a real refusal.
	Flagged bool
}

// Target renders the destination the way a person would name it.
func (p BlockedPacket) Target() string {
	if p.DPort == 0 {
		return p.Dst
	}
	return p.Dst + ":" + strconv.FormatUint(uint64(p.DPort), 10)
}

// ParseBlocked pulls a devbox log line out of the kernel ring buffer.
//
// Pure, and separated from the reading for exactly one reason: the reading
// needs a kernel with the ruleset loaded and this needs a string. Every defect
// in this codebase that survived many review rounds lived in a path no test
// could reach, and the response each time was to pull the decidable part out.
//
// A `/dev/kmsg` line is `<prio>,<seq>,<usec>,<flags>;<message>`, and the
// message is nftables' own `LOG` format: space-separated `KEY=VALUE` after the
// prefix. Unknown keys are ignored rather than refused — the kernel adds
// fields between versions, and a rule that logged something slightly different
// should still be reported.
func ParseBlocked(line string) (BlockedPacket, bool) {
	// Everything after the first `;` if this came from /dev/kmsg; the whole
	// line if it came from a log file that has already been split.
	if i := strings.IndexByte(line, ';'); i >= 0 {
		line = line[i+1:]
	}

	var pkt BlockedPacket
	switch {
	case strings.Contains(line, BlockedPrefix):
		pkt.Flagged = false
	case strings.Contains(line, FlaggedPrefix):
		pkt.Flagged = true
	default:
		return BlockedPacket{}, false
	}

	for _, field := range strings.Fields(line) {
		key, value, ok := strings.Cut(field, "=")
		if !ok {
			continue
		}
		switch key {
		case "SRC":
			pkt.Src = value
		case "DST":
			pkt.Dst = value
		case "PROTO":
			pkt.Proto = strings.ToLower(value)
		case "SPT":
			pkt.SPort = parsePort(value)
		case "DPT":
			pkt.DPort = parsePort(value)
		}
	}

	// A log line naming no destination says nothing worth an event: the whole
	// content of a policy event is what was reached for.
	if pkt.Dst == "" {
		return BlockedPacket{}, false
	}
	return pkt, true
}

func parsePort(s string) uint16 {
	n, err := strconv.ParseUint(s, 10, 16)
	if err != nil {
		return 0
	}
	return uint16(n)
}

// Event builds the policy event for one parsed line.
//
// `mode` is the posture in force, which the kernel log does not carry — the
// agent knows it from the policy file it already reads.
func (p BlockedPacket) Event(boxID, mode string) *event.Event {
	reason := "outside the allowlist"
	if p.Flagged {
		reason = "outside the declared allowlist (posture is open, so not blocked)"
	}

	ev := policy.Violation(boxID, mode, p.Target(), reason, nil)
	if p.Flagged {
		ev.Policy.Verdict = "flag"
	}
	// The kernel logs a packet and not a process. There is no pid to recover,
	// and the collector requires one, so this carries the sentinel that cannot
	// be mistaken for a real process.
	ev.PID = UnattributedPID
	ev.TID = UnattributedPID
	ev.Comm = "netfilter"
	ev.TSWall = event.Now(time.Now())
	ev.Net = &event.Net{
		Proto: p.Proto,
		SAddr: p.Src,
		SPort: p.SPort,
		DAddr: p.Dst,
		DPort: p.DPort,
	}
	return ev
}

// Blocked reports connections the firewall refused, or noticed.
//
// The ruleset has always written these lines and nothing has ever read them,
// so `Violation` was dead code and the Activity view, the behaviour summary
// and the violation metrics all described a feed with no producer. This is the
// producer.
type Blocked struct {
	BoxID string
	// Mode reports the posture in force, for the event's `mode` field.
	//
	// A function rather than a string because the posture changes under a
	// running agent — `devbox policy set` rewrites the file and the agent
	// reloads — and a label captured once would go on describing the posture
	// that happened to be in force when capture started.
	Mode func() string
	// Open returns the kernel ring buffer. Injectable so the loop can be run
	// against a fixture; defaults to /dev/kmsg.
	Open func() (io.ReadCloser, error)
}

// Name implements Source.
func (b *Blocked) Name() string { return "netfilter" }

// Domains implements Source.
func (b *Blocked) Domains() []event.Type { return []event.Type{event.TypePolicy} }

// Run streams one policy event per logged refusal until ctx is cancelled.
//
// A ring buffer that cannot be opened is not fatal. `/dev/kmsg` needs root and
// a kernel that offers it, and an agent that refused to start without it would
// take the whole capture down over the one source that is supplementary — the
// posture is still enforced by the kernel either way. It says so and returns.
func (b *Blocked) Run(ctx context.Context, out chan<- *event.Event) error {
	open := b.Open
	if open == nil {
		open = func() (io.ReadCloser, error) { return os.Open(KmsgPath) }
	}
	r, err := open()
	if err != nil {
		return &ErrUnsupported{Source: "netfilter", Reason: err.Error()}
	}
	defer r.Close()

	// Reading blocks, and a blocked read does not notice a cancelled context.
	// Closing the file from the watcher is what unblocks it.
	go func() {
		<-ctx.Done()
		_ = r.Close()
	}()

	scanner := bufio.NewScanner(r)
	for scanner.Scan() {
		pkt, ok := ParseBlocked(scanner.Text())
		if !ok {
			continue
		}
		mode := ""
		if b.Mode != nil {
			mode = b.Mode()
		}
		if err := Send(ctx, out, pkt.Event(b.BoxID, mode)); err != nil {
			return err
		}
	}
	if ctx.Err() != nil {
		return nil
	}
	return scanner.Err()
}
