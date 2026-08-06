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
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/ethannortharc/devbox/agent/capture"
	"github.com/ethannortharc/devbox/agent/event"
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

// chooseSource picks the capture source with the highest fidelity available.
func chooseSource(cfg config) (capture.Source, error) {
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

// bootTime estimates the wall-clock time of monotonic zero.
//
// Captured once at startup so a later NTP step cannot make events appear to
// travel backwards relative to each other.
func bootTime() time.Time {
	return time.Now()
}

// stream connects, handshakes, and pumps events until the context ends.
func stream(ctx context.Context, cfg config, source capture.Source, out io.Writer) error {
	conn, err := net.Dial("unix", cfg.socket)
	if err != nil {
		return fmt.Errorf("connect to the collector at %s: %w", cfg.socket, err)
	}
	defer conn.Close()

	domains := make([]string, 0, len(source.Domains()))
	for _, d := range source.Domains() {
		domains = append(domains, string(d))
	}

	if err := transport.Handshake(conn, transport.Hello{
		Version: buildinfo.Version,
		BoxID:   cfg.boxID,
		Capture: domains,
		EBPF:    !cfg.noEBPF && cfg.fixture == "",
	}); err != nil {
		return err
	}
	fmt.Fprintf(out, "devbox-obsd: connected to %s as %q via %s capture\n",
		cfg.socket, cfg.boxID, source.Name())

	events := make(chan *event.Event, cfg.queue)
	srcCtx, cancelSrc := context.WithCancel(ctx)
	defer cancelSrc()

	srcDone := make(chan error, 1)
	go func() {
		srcDone <- source.Run(srcCtx, events)
		close(events)
	}()

	sent := 0
	for {
		select {
		case e, ok := <-events:
			if !ok {
				// Source finished and the channel drained.
				fmt.Fprintf(out, "devbox-obsd: %d event(s) sent\n", sent)
				return <-srcDone
			}
			payload, err := e.Encode()
			if err != nil {
				// One unencodable event must not kill the stream.
				fmt.Fprintf(os.Stderr, "devbox-obsd: skipping an event: %v\n", err)
				continue
			}
			if err := transport.WriteFrame(conn, payload); err != nil {
				cancelSrc()
				return fmt.Errorf("stream to the collector: %w", err)
			}
			sent++

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
