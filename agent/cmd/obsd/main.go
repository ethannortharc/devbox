// Command obsd is devbox-obsd, the in-guest observability agent.
//
// It captures activity inside a box — process execs, network connections, DNS
// lookups, TLS SNI, file access — and streams it to the Rust collector on the
// host over a unix socket (container substrate) or vsock (VM runtimes).
//
// Capture source is chosen by fidelity: eBPF where the kernel has BTF,
// /proc polling where it does not (`-no-ebpf`), and a recorded fixture for
// tests and demos. See `agent/capture`.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/ethannortharc/devbox/agent/capture"
	"github.com/ethannortharc/devbox/agent/event"
	"github.com/ethannortharc/devbox/agent/policy"
	"github.com/ethannortharc/devbox/agent/transport"
	"github.com/ethannortharc/devbox/internal/buildinfo"
)

// config is the agent's runtime configuration, parsed from flags.
type config struct {
	showVersion bool
	socket      string
	boxID       string
	noEBPF      bool
	// fixture replays a recorded JSONL file instead of capturing. Used by the
	// cross-language integration test and by demos.
	fixture string
	// replayInterval paces fixture replay; zero is as fast as accepted.
	replayInterval time.Duration
	// once exits after the source finishes instead of waiting for a signal.
	once bool
	// queue bounds the in-agent buffer between capture and transport.
	queue int
	// policy is the path to the generated egress policy. When set, the agent
	// keeps the firewall's allow sets in step with the DNS it captures.
	policy string
}

// defaultQueue bounds the buffer between capture and transport.
//
// Bounded on purpose (§7.3): if the socket cannot keep up, the agent applies
// backpressure to its own capture loop rather than growing without limit
// inside the box it is supposed to be observing cheaply.
const defaultQueue = 4096

func main() {
	if err := run(os.Args[1:], os.Stdout); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return
		}
		fmt.Fprintf(os.Stderr, "devbox-obsd: %v\n", err)
		os.Exit(1)
	}
}

func run(args []string, out io.Writer) error {
	cfg, err := parseFlags(args, out)
	if err != nil {
		return err
	}

	if cfg.showVersion {
		_, err := fmt.Fprintln(out, buildinfo.String(buildinfo.Obsd))
		return err
	}
	if cfg.boxID == "" {
		return errors.New("-box-id is required: events must be attributable to a box")
	}

	source, err := chooseSource(cfg)
	if err != nil {
		return err
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	return stream(ctx, cfg, source, out)
}

// chooseSource picks the capture source with the highest fidelity available,
// and adds the kernel's record of refused connections alongside it.
//
// The firewall has always logged what it dropped and nothing has ever read
// those lines, so `policy` events had no producer at all — the Activity view,
// the behaviour summary and the violation metrics each described a feed
// nothing filled. That reader is `capture.Blocked`, and it belongs beside the
// primary source rather than inside it: refusals come from the kernel ring
// buffer while execs and connections come from eBPF or /proc, and they are one
// timeline to whoever reads them.
func chooseSource(cfg config) (capture.Source, error) {
	primary, err := primarySource(cfg)
	if err != nil {
		return nil, err
	}
	// Not for a fixture: a replay is a recording of one box and the ring
	// buffer is live kernel state from another, and mixing them would put
	// events in a timeline that never happened.
	if cfg.fixture != "" || cfg.policy == "" {
		return primary, nil
	}
	blocked := &capture.Blocked{
		BoxID: cfg.boxID,
		Mode:  func() string { return posture(cfg.policy) },
	}
	// Probed before the handshake, not discovered during the run.
	//
	// `Multi` skips a source that reports itself unsupported, and the
	// handshake would already have advertised `policy` capture and named
	// `netfilter` as active — so the collector recorded the feed as healthy
	// while nothing produced it. Reading /dev/kmsg needs CAP_SYSLOG wherever
	// `kernel.dmesg_restrict` is set, which is most places, and the symptom of
	// lacking it is a box that appears never to have violated its policy.
	if err := blocked.Available(); err != nil {
		fmt.Fprintf(os.Stderr,
			"devbox-obsd: no policy events — cannot read %s (%v). "+
				"The posture is still enforced by the kernel; only the record of "+
				"refusals is missing.\n", capture.KmsgPath, err)
		return primary, nil
	}
	return capture.NewMulti(primary, blocked), nil
}

// posture reads the egress posture currently in force.
//
// Read per event rather than captured once: `devbox policy set` rewrites this
// file under a running agent, and a label fixed at startup would describe the
// posture that happened to be in force when capture began. Violations are rate
// limited in the ruleset itself, so this is not a hot path.
func posture(path string) string {
	raw, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	var spec struct {
		Egress string `json:"egress"`
	}
	if err := json.Unmarshal(raw, &spec); err != nil {
		return ""
	}
	return spec.Egress
}

func primarySource(cfg config) (capture.Source, error) {
	if cfg.fixture != "" {
		return &capture.Fixture{
			Path:     cfg.fixture,
			BoxID:    cfg.boxID,
			Interval: cfg.replayInterval,
		}, nil
	}
	if cfg.noEBPF {
		return &capture.Proc{BoxID: cfg.boxID, Boot: bootTime()}, nil
	}
	// The eBPF source lands with the compiled programs; until then, refusing
	// is better than silently falling back to a source that sees less, because
	// a quiet timeline would read as "the box did nothing".
	return nil, errors.New(
		"eBPF capture is not built into this binary; re-run with -no-ebpf for the " +
			"degraded proc-polling path, or -fixture to replay a recording")
}

// bootTime is the wall-clock time of monotonic zero — the kernel's boot.
//
// It must be the *kernel's* zero, not the agent's start: `ts_mono_ns` is
// compared against eBPF timestamps, which come from the kernel clock, and the
// collector orders events by it. Anchoring to agent startup made every restart
// reset the clock to near zero, so freshly captured events sorted ahead of
// older ones and behavior summaries came out backwards.
//
// Captured once, so a later NTP step cannot make events appear to travel
// backwards relative to each other.
func bootTime() time.Time {
	uptime, err := readUptime("/proc/uptime")
	if err != nil {
		// Not Linux, or no procfs. Ordering within this agent's lifetime is
		// still correct; only cross-restart ordering degrades.
		return time.Now()
	}
	return time.Now().Add(-uptime)
}

// readUptime parses the first field of /proc/uptime — seconds since boot.
func readUptime(path string) (time.Duration, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return 0, err
	}
	field, _, _ := strings.Cut(strings.TrimSpace(string(raw)), " ")
	secs, err := strconv.ParseFloat(field, 64)
	if err != nil {
		return 0, fmt.Errorf("malformed /proc/uptime %q: %w", field, err)
	}
	return time.Duration(secs * float64(time.Second)), nil
}

// stream connects, handshakes, and pumps events until the context ends.
func stream(ctx context.Context, cfg config, source capture.Source, out io.Writer) error {
	// The firewall first, before anything that can fail for unrelated reasons.
	//
	// This used to happen after the collector connection, so a collector that
	// was not listening — a host-side restart, a socket not yet bind-mounted —
	// returned here and systemd restarted the agent into the same failure. The
	// box stayed unrestricted for the whole outage, after a reboot that had
	// already taken its nftables table with it, even though the policy on disk
	// says exactly what to restore and needs nobody's cooperation to do it.
	//
	// Observability depends on the collector. Enforcement does not, and tying
	// them together made the weaker dependency govern the stronger guarantee.
	enforcer, err := loadEnforcer(cfg, true)
	if err != nil {
		return err
	}
	policyStamp := policyFingerprint(cfg.policy)

	domains := make([]string, 0, len(source.Domains()))
	for _, d := range source.Domains() {
		domains = append(domains, string(d))
	}
	hello := transport.Hello{
		Version: buildinfo.Version,
		BoxID:   cfg.boxID,
		Capture: domains,
		EBPF:    !cfg.noEBPF && cfg.fixture == "",
	}

	// Capture starts before the collector is reachable, and keeps running when
	// it goes away.
	//
	// Waiting for the collector first was the remaining half of the same
	// defect. Moving the ruleset load ahead of the dial stopped the restart
	// loop from destroying the nftables table every two seconds, but the sets
	// it creates start *empty* and only DNS answers fill them — and DNS answers
	// arrive through this loop. An allowlist posture therefore still blocked
	// every allowlisted domain for the length of any collector outage, which is
	// precisely the harm the earlier fix was written to prevent.
	//
	// Enforcement depends on capture. Capture does not depend on the transport.
	events := make(chan *event.Event, cfg.queue)
	srcCtx, cancelSrc := context.WithCancel(ctx)
	defer cancelSrc()

	srcDone := make(chan error, 1)
	go func() {
		srcDone <- source.Run(srcCtx, events)
		close(events)
	}()

	// An allowlist names *domains*, and a firewall matches addresses. The
	// bridge is the DNS this agent is already capturing: every answer for an
	// allowlisted name is added to the allow set before the application that
	// asked for it connects.
	//
	// Without this the generated ruleset is not merely incomplete — it is
	// default-deny with an empty allow set, so `allowlist` and `mirror-only`
	// block everything they promise to permit.
	if enforcer != nil {
		fmt.Fprintf(out, "devbox-obsd: enforcing egress policy from %s\n", cfg.policy)
	}

	// The transport is dialed off to one side, so a collector that is not
	// listening cannot hold up the loop above.
	var conn net.Conn
	defer func() {
		if conn != nil {
			conn.Close()
		}
	}()
	linked := make(chan link, 1)
	dialing := false
	connect := func() {
		if dialing {
			return
		}
		dialing = true
		go func() {
			c, err := dialCollector(ctx, cfg.socket, out)
			if err != nil {
				// Only returns an error once we are shutting down.
				linked <- link{}
				return
			}
			if err := transport.Handshake(c, hello); err != nil {
				// Not an outage, and not retryable. A refused handshake is the
				// collector saying this agent is not who it claims to be —
				// wrong box id, wrong protocol version — and retrying that
				// forever would turn a loud misconfiguration into a silent one.
				c.Close()
				linked <- link{fatal: fmt.Errorf("handshake with the collector: %w", err)}
				return
			}
			fmt.Fprintf(out, "devbox-obsd: connected to %s as %q via %s capture\n",
				cfg.socket, cfg.boxID, source.Name())
			linked <- link{conn: c}
		}()
	}
	connect()

	// Events captured while the collector is unreachable are held, not thrown
	// away and not allowed to stall capture.
	//
	// Counting them instead lost the ordinary case to fix the rare one: the
	// collector is normally listening at startup, and capture now begins before
	// the dial completes, so every event in that window went missing. Bounded
	// by the same queue depth as the channel — beyond that the oldest go, which
	// is the honest trade for a box that has been talking to nothing for a long
	// time — and blocking is not an option, because backpressure here reaches
	// the source and takes enforcement down with the transport.
	pending := make([][]byte, 0, cfg.queue)
	dropped := 0
	sent := 0
	hold := func(payload []byte) {
		if len(pending) >= cfg.queue {
			pending = pending[1:]
			dropped++
		}
		pending = append(pending, payload)
	}
	defer func() {
		if dropped > 0 {
			fmt.Fprintf(out, "devbox-obsd: %d event(s) dropped while the collector was away\n", dropped)
		}
	}()

	// Deliver everything still held, waiting for the collector if need be.
	//
	// The source finishing is not a reason to discard what it produced, and
	// with `-once` this is the only chance — the process is about to exit. A
	// fixture replay drains in milliseconds, far quicker than a dial and a
	// handshake, so without this the common case was an agent that captured
	// every event and sent none of them.
	drain := func() error {
		for len(pending) > 0 {
			if conn == nil {
				select {
				case l := <-linked:
					dialing = false
					if l.fatal != nil {
						return l.fatal
					}
					if l.conn == nil {
						return nil // shutting down; nothing to deliver to
					}
					conn = l.conn
				case <-ctx.Done():
					return nil
				}
				continue
			}
			if err := transport.WriteFrame(conn, pending[0]); err != nil {
				conn.Close()
				conn = nil
				connect()
				continue
			}
			pending = pending[1:]
			sent++
		}
		return nil
	}

	for {
		select {
		case e, ok := <-events:
			if !ok {
				// Source finished and the channel drained.
				if err := drain(); err != nil {
					cancelSrc()
					return err
				}
				fmt.Fprintf(out, "devbox-obsd: %d event(s) sent\n", sent)
				err := <-srcDone
				if err != nil || cfg.once {
					return err
				}
				// `-once` is what makes a finished source end the process; its
				// own help text says so, and without this the flag had no
				// effect at all. A fixture replay that finishes without it
				// stays connected and waits for a signal, so the collector
				// keeps a live agent rather than seeing it hang up.
				<-ctx.Done()
				return ctx.Err()
			}
			// Reload when the control plane rewrites the policy. `devbox
			// policy allow` recreates the nftables sets empty and writes a
			// new domain list; an enforcer built once at startup would keep
			// the old list and leave the newly allowed domain blocked until
			// someone restarted the service.
			if cfg.policy != "" {
				if reloaded, changed := reloadEnforcer(cfg, policyStamp, out); reloaded.stamp != "" {
					// The stamp advances either way: a file that cannot be
					// parsed must not be re-read on every subsequent event.
					policyStamp = reloaded.stamp
					if changed {
						enforcer = reloaded.enforcer
					}
				}
			}
			if enforcer != nil && e.Type == event.TypeDNS {
				if added, err := enforcer.OnDNS(ctx, e); err != nil {
					// A failed insertion means one domain stays blocked, not
					// that capture should stop. It is worth saying out loud,
					// because the symptom otherwise looks like a network fault.
					fmt.Fprintf(out, "devbox-obsd: could not allow %v: %v\n", added, err)
				}
			}
			payload, err := e.Encode()
			if err != nil {
				// One unencodable event must not kill the stream.
				fmt.Fprintf(os.Stderr, "devbox-obsd: skipping an event: %v\n", err)
				continue
			}
			if conn == nil {
				// Enforcement already happened above; the reporting waits.
				hold(payload)
				continue
			}
			if err := transport.WriteFrame(conn, payload); err != nil {
				// Not fatal any more. Returning here ended the process, and
				// systemd restarting it re-ran the ruleset load — which wipes
				// the allow sets this agent had spent the session filling.
				fmt.Fprintf(out, "devbox-obsd: lost the collector (%v); capture and enforcement continue\n", err)
				conn.Close()
				conn = nil
				hold(payload)
				connect()
				continue
			}
			sent++

		case l := <-linked:
			dialing = false
			if l.fatal != nil {
				cancelSrc()
				return l.fatal
			}
			conn = l.conn
			// Deliver what was captured while it was away, oldest first, before
			// anything newer.
			for len(pending) > 0 && conn != nil {
				if err := transport.WriteFrame(conn, pending[0]); err != nil {
					fmt.Fprintf(out, "devbox-obsd: lost the collector again (%v)\n", err)
					conn.Close()
					conn = nil
					connect()
					break
				}
				pending = pending[1:]
				sent++
			}

		case <-ctx.Done():
			cancelSrc()
			fmt.Fprintf(out, "devbox-obsd: stopping after %d event(s)\n", sent)
			return nil
		}
	}
}

func parseFlags(args []string, out io.Writer) (config, error) {
	cfg := config{queue: defaultQueue}

	fs := flag.NewFlagSet("devbox-obsd", flag.ContinueOnError)
	fs.SetOutput(out)
	fs.BoolVar(&cfg.showVersion, "version", false, "print version and exit")
	fs.StringVar(&cfg.socket, "socket", "/run/devbox/obsd.sock",
		"unix socket path or vsock address to stream events to")
	fs.StringVar(&cfg.boxID, "box-id", "",
		"box identifier stamped onto every event")
	fs.BoolVar(&cfg.noEBPF, "no-ebpf", false,
		"degraded mode: proc polling and tap-based DNS/flow only")
	fs.StringVar(&cfg.fixture, "fixture", "",
		"replay a recorded JSONL event file instead of capturing")
	fs.DurationVar(&cfg.replayInterval, "replay-interval", 0,
		"delay between replayed fixture events (0 = as fast as accepted)")
	fs.BoolVar(&cfg.once, "once", false,
		"exit when the source finishes instead of waiting for a signal")
	fs.IntVar(&cfg.queue, "queue", defaultQueue,
		"in-agent event buffer depth")
	fs.StringVar(&cfg.policy, "policy", "",
		"egress policy JSON; keeps the nftables allow sets in step with DNS")

	if err := fs.Parse(args); err != nil {
		return config{}, err
	}
	if cfg.queue < 1 {
		return config{}, fmt.Errorf("-queue must be at least 1, got %d", cfg.queue)
	}
	if cfg.socket != "" && strings.HasPrefix(cfg.socket, "vsock://") {
		return config{}, errors.New("vsock transport lands with the VM runtimes; use a unix socket")
	}
	return cfg, nil
}

// loadEnforcer builds the DNS→nftables bridge, if a policy was given.
//
// Returns nil when no policy is configured, which is the `open` posture and
// the fixture/replay paths: nothing to enforce, and no root needed.
func loadEnforcer(cfg config, loadRuleset bool) (*policy.Enforcer, error) {
	if cfg.policy == "" {
		return nil, nil
	}
	raw, err := os.ReadFile(cfg.policy)
	if os.IsNotExist(err) {
		// Not an error. The unit passes `-policy` unconditionally because the
		// path cannot be checked at Nix evaluation time without baking in the
		// answer forever — so an agent that starts before the first policy is
		// staged enforces nothing and picks one up on the next reload.
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read the policy at %s: %w", cfg.policy, err)
	}
	var spec struct {
		Egress  string   `json:"egress"`
		Allow   []string `json:"allow"`
		Ruleset string   `json:"ruleset"`
		// Generation names the allow sets this policy owns, so an enforcer
		// that has been superseded cannot insert into the table that replaced
		// it. Absent for a policy written before this existed.
		Generation string `json:"generation"`
	}
	if err := json.Unmarshal(raw, &spec); err != nil {
		return nil, fmt.Errorf("parse the policy at %s: %w", cfg.policy, err)
	}
	// `open` usually means there is nothing to enforce — but an `open` posture
	// that audits gets a table too, and that table decides what to flag by
	// consulting the same allow sets. Only the agent can fill them, from the
	// DNS it captures. Returning nil here left them empty, so every connection
	// to an allowlisted domain fell through to `devbox-flagged` and the audit
	// reported the entire allowlist as a violation of itself.
	//
	// The ruleset is the signal, because the control plane writes one only for
	// the postures that have something to enforce or observe. That keeps the
	// two sides agreeing on a fact rather than on a second flag they would
	// each have to interpret.
	if spec.Egress == "open" && spec.Ruleset == "" {
		return nil, nil
	}

	enforcer := policy.New(policy.NFT{}, spec.Allow, spec.Egress == "mirror-only").
		WithGeneration(spec.Generation)
	if loadRuleset && spec.Ruleset != "" {
		// Only at startup. `Load` destroys and rebuilds the table, so doing it
		// on every reload would discard every address the previous enforcer had
		// resolved into the allow sets — briefly denying traffic that was
		// already permitted, each time the user edits the policy. On a reload
		// the control plane has just installed the table itself; the agent only
		// needs the new domain list.
		if err := enforcer.Load(context.Background(), spec.Ruleset); err != nil {
			return nil, fmt.Errorf("load the egress ruleset: %w", err)
		}
	}
	return enforcer, nil
}

// link is the result of one attempt to reach the collector.
//
// `fatal` separates "not listening yet" from "refused us". The first is an
// outage and is retried; the second is a misconfiguration — a wrong box id, a
// protocol version the collector will never accept — and retrying it forever
// would turn a loud failure into a silent one.
type link struct {
	conn  net.Conn
	fatal error
}

// dialCollector waits for the collector rather than letting the unit restart.
//
// Exiting here was destructive in a way that is invisible from this line. A
// restart re-runs `loadEnforcer` with `loadRuleset` true, and that `Load`
// destroys and rebuilds the nftables table — discarding every address DNS
// capture had resolved into the allow sets. With the collector down nothing
// repopulates them, because the agent never reaches its event loop. So systemd
// restarting every two seconds re-blocked the entire allowlist every two
// seconds, for as long as the outage lasted, and allowlisted domains simply
// stopped resolving to anything permitted.
//
// The ordering above exists so enforcement does not depend on observability.
// Reconnecting in-process is what makes that true across an outage instead of
// only at the first instant of one: the table is loaded once and stays loaded
// while this waits.
func dialCollector(ctx context.Context, socket string, out io.Writer) (net.Conn, error) {
	const (
		firstWait   = 250 * time.Millisecond
		longestWait = 5 * time.Second
		// Long outages must not be silent, and must not be a log flood either.
		reportEvery = time.Minute
	)

	wait := firstWait
	started := time.Now()
	lastReport := time.Time{}

	for attempt := 1; ; attempt++ {
		conn, err := net.Dial("unix", socket)
		if err == nil {
			if attempt > 1 {
				fmt.Fprintf(out, "devbox-obsd: collector reachable again after %s\n",
					time.Since(started).Round(time.Second))
			}
			return conn, nil
		}

		if lastReport.IsZero() || time.Since(lastReport) >= reportEvery {
			fmt.Fprintf(out,
				"devbox-obsd: collector at %s is not answering (%v); egress policy stays enforced, retrying\n",
				socket, err)
			lastReport = time.Now()
		}

		select {
		case <-ctx.Done():
			// Asked to stop, so report the reason we were waiting rather than
			// the cancellation — the dial failure is the actionable half.
			return nil, fmt.Errorf("connect to the collector at %s: %w", socket, err)
		case <-time.After(wait):
		}
		wait = nextBackoff(wait, longestWait)
	}
}

// nextBackoff doubles a wait up to a ceiling.
//
// Pulled out because `dialCollector` needs a socket nobody is listening on to
// exercise, and this is the part that can be wrong on its own.
func nextBackoff(current, ceiling time.Duration) time.Duration {
	if next := current * 2; next < ceiling {
		return next
	}
	return ceiling
}

// reloadState is what a reload produces: the new enforcer and the stamp that
// identifies the policy it was built from.
type reloadState struct {
	enforcer *policy.Enforcer
	stamp    string
}

// reloadEnforcer rebuilds the enforcer when the policy file has changed.
//
// Cheap enough to check per DNS event: a stat, and a rebuild only when the
// fingerprint moves. A failed reload keeps the old enforcer — a policy file
// caught mid-write should not disable enforcement.
func reloadEnforcer(cfg config, stamp string, out io.Writer) (reloadState, bool) {
	current := policyFingerprint(cfg.policy)
	if current == stamp {
		return reloadState{}, false
	}
	// An absent policy file means the control plane has retired it, not that
	// the reload failed. Keeping the old enforcer then let a stale domain list
	// insert addresses into a table built for a *newer*, tighter policy, where
	// they lived out the TTL. Absent means enforce nothing until a new policy
	// appears; the table's own default-deny still applies.
	if _, statErr := os.Stat(cfg.policy); os.IsNotExist(statErr) {
		fmt.Fprintf(out, "devbox-obsd: policy withdrawn; adding no further addresses\n")
		return reloadState{stamp: current}, true
	}

	enforcer, err := loadEnforcer(cfg, false)
	if err != nil {
		fmt.Fprintf(out, "devbox-obsd: policy reload failed, keeping the old one: %v\n", err)
		// Stamped, so a persistently broken file is not retried on every event
		// — but reported as *not* changed, so the caller keeps the enforcer it
		// has. Returning a nil enforcer with `changed` would have been read as
		// "policy reloaded to nothing" and silently stopped enforcement.
		return reloadState{stamp: current}, false
	}
	fmt.Fprintf(out, "devbox-obsd: policy reloaded from %s\n", cfg.policy)
	return reloadState{enforcer: enforcer, stamp: current}, true
}

// policyFingerprint identifies a version of the policy file.
//
// Size and mtime rather than a hash: the file is rewritten wholesale by the
// control plane, and reading it on every event to hash it would be the
// expensive thing this is avoiding.
func policyFingerprint(path string) string {
	if path == "" {
		return ""
	}
	info, err := os.Stat(path)
	if err != nil {
		return "absent"
	}
	return fmt.Sprintf("%d:%d", info.Size(), info.ModTime().UnixNano())
}
