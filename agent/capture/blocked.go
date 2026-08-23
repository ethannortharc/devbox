package capture

import (
	"bufio"
	"context"
	"errors"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"syscall"
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

// KmsgPath is the kernel ring buffer.
//
// Opened and then seeked to the end: a fresh descriptor starts at the oldest
// retained record, and replaying those on every restart would report refusals
// that already happened as though they had just happened.
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
//
// `net.JoinHostPort` rather than concatenation: an IPv6 address contains
// colons, so `2001:db8::1` + `:443` reads as another IPv6 address rather than
// as an address and a port. These strings are displayed and used as violation
// identities, so an ambiguous one is both unreadable and unequal to itself
// across any code that normalises it.
func (p BlockedPacket) Target() string {
	if p.DPort == 0 {
		return p.Dst
	}
	return net.JoinHostPort(p.Dst, strconv.FormatUint(uint64(p.DPort), 10))
}

// Flow identifies one connection, for suppressing repeats of it.
//
// The verdict is part of the identity. A UDP socket kept across a policy
// change keeps its five-tuple, so a flow that was flagged under an audited
// `open` and is then genuinely blocked once the posture tightens would have
// been suppressed as a repeat — hiding the first real denial, which is the one
// worth seeing.
func (p BlockedPacket) Flow() string {
	verdict := "block"
	if p.Flagged {
		verdict = "flag"
	}
	return strings.Join([]string{
		verdict,
		p.Proto,
		p.Src, strconv.FormatUint(uint64(p.SPort), 10),
		p.Dst, strconv.FormatUint(uint64(p.DPort), 10),
	}, "|")
}

// DedupeWindow is how long one refused flow stays reported.
//
// The ruleset logs with `ct state new`, which is not the same as once per
// connection: a refused TCP handshake never completes, so every retransmitted
// SYN is still `new` and the kernel logs it again. The rate limit there bounds
// the journal but not the event count, and the contract these events are read
// under — one per refused connection — is the agent's to keep.
//
// It has to outlast the whole handshake, not most of it. With Linux's default
// `tcp_syn_retries=6` the retransmissions go out at 1, 3, 7, 15, 31 and 63
// seconds and the attempt is abandoned around 127 — so a one-minute window let
// the 63-second SYN through as a second event for the same connection, which
// is precisely the duplicate this exists to prevent, arriving late enough to
// look like a separate refusal.
//
// Three minutes covers the sequence with room for a slower configuration.
// Suppressing a genuinely new connection for that long is not a real risk:
// a new connection takes a new ephemeral source port, so its five-tuple —
// and its key — differ.
const DedupeWindow = 3 * time.Minute

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

// reasonFor explains the refusal in the terms of the posture that caused it.
//
// "outside the allowlist" was said for every posture, and it is only true of
// one. `isolated` makes no allowlist decision at all — it refuses egress, full
// stop — and `mirror-only` also consults the curated mirrors, so a user
// reading either explanation would go looking for a list entry that was never
// consulted.
func reasonFor(mode string, flagged bool) string {
	if flagged {
		return "outside the declared allowlist (posture is open, so not blocked)"
	}
	switch mode {
	case "isolated":
		return "posture is isolated: no egress"
	case "mirror-only":
		return "not in the allowlist or the curated package mirrors"
	case "":
		return "refused by the egress policy"
	default:
		return "not in the allowlist"
	}
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
	ev := policy.Violation(boxID, mode, p.Target(), reasonFor(mode, p.Flagged), nil)
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
	// Boot anchors kernel-ring events to the same monotonic timeline as every
	// other source. The log record has wall time but no boot-relative field.
	Boot time.Time
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

	// now is injectable so the dedupe window is testable without sleeping.
	now func() time.Time
}

// maxTrackedFlows caps the dedupe table.
//
// Reached only under sustained refusals from many distinct flows, where the
// sweep below drops everything already outside the window. A cap rather than a
// hard limit on reporting: the events still go out, only the memory is bounded.
const maxTrackedFlows = 4096

// Name implements Source.
func (b *Blocked) Name() string { return "netfilter" }

// Available reports whether the kernel ring buffer can actually be read.
//
// Checked before the handshake rather than discovered during it. `Multi` skips
// a source that reports itself unsupported, but the handshake has by then
// already advertised `policy` capture and printed `netfilter` as active — so
// the collector recorded the feed as healthy while nothing produced it, and a
// box whose agent lacks CAP_SYSLOG looked exactly like a box that never
// violated its policy. Advertising a capability is a claim, and this is what
// makes the claim true before it is made.
func (b *Blocked) Available() error {
	open := b.Open
	if open == nil {
		open = func() (io.ReadCloser, error) { return os.Open(KmsgPath) }
	}
	r, err := open()
	if err != nil {
		return err
	}
	return r.Close()
}

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
	defer func() { _ = r.Close() }()

	// Start at the end, not at the oldest record the ring buffer still holds.
	//
	// A fresh `/dev/kmsg` descriptor begins at the start of what is retained,
	// so every agent restart re-emitted every refusal still in the buffer.
	// Neither the time nor the posture is in the log line — both are stamped
	// on at read — so those replays arrived dated now, labelled with today's
	// posture, and indistinguishable from fresh ones. A box would appear to
	// re-violate its policy every time the agent came back.
	if seeker, ok := r.(io.Seeker); ok {
		_, _ = seeker.Seek(0, io.SeekEnd)
	}

	// Reading blocks, and a blocked read does not notice a cancelled context.
	// Closing the file from the watcher is what unblocks it.
	go func() {
		<-ctx.Done()
		_ = r.Close()
	}()

	// One event per refused connection, which `ct state new` alone does not
	// give: a handshake that is never answered keeps retransmitting, and every
	// retransmission is still `new` to conntrack.
	seen := make(map[string]time.Time)
	now := b.now
	if now == nil {
		now = time.Now
	}

	for {
		scanner := bufio.NewScanner(r)
		for scanner.Scan() {
			pkt, ok := ParseBlocked(scanner.Text())
			if !ok {
				continue
			}

			at := now()
			flow := pkt.Flow()
			if last, ok := seen[flow]; ok && at.Sub(last) < DedupeWindow {
				continue
			}
			seen[flow] = at
			// Bounded: a box under sustained refusal would otherwise accumulate a
			// map entry per flow for as long as the agent runs.
			if len(seen) > maxTrackedFlows {
				for k, t := range seen {
					if at.Sub(t) >= DedupeWindow {
						delete(seen, k)
					}
				}
			}

			mode := ""
			if b.Mode != nil {
				mode = b.Mode()
			}
			captured := pkt.Event(b.BoxID, mode)
			captured.TSWall = event.Now(at)
			if !b.Boot.IsZero() {
				mono := at.Sub(b.Boot)
				if mono < 0 {
					mono = 0
				}
				captured.TSMonoNS = uint64(mono.Nanoseconds())
			}
			if err := Send(ctx, out, captured); err != nil {
				return err
			}
		}
		if ctx.Err() != nil {
			return nil
		}
		if err := scanner.Err(); err != nil {
			// A /dev/kmsg reader which falls behind gets EPIPE once and is then
			// positioned at the next retained record. A fresh Scanner clears its
			// latched error and resumes on that same descriptor.
			if errors.Is(err, syscall.EPIPE) {
				continue
			}
			return &ErrUnsupported{Source: "netfilter", Reason: err.Error()}
		}
		return nil
	}
}
