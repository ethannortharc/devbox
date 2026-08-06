// Command obsd is devbox-obsd, the in-guest observability agent.
//
// It loads eBPF programs (exec, connect/accept, DNS, file-open), decodes and
// enriches the resulting events, and streams them to the Rust collector on the
// host over vsock (VM runtimes) or a unix socket (container substrate).
//
// The capture pipeline lands in Phase 3. This entry point establishes the flag
// surface and the identity handshake the host binary depends on, so
// provisioning can be wired ahead of the eBPF work.
package main

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"os"

	"github.com/ethannortharc/devbox/internal/buildinfo"
)

// config is the agent's runtime configuration, parsed from flags.
type config struct {
	showVersion bool
	socket      string
	boxID       string
	noEBPF      bool
}

func main() {
	if err := run(os.Args[1:], os.Stdout); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return
		}
		fmt.Fprintf(os.Stderr, "devbox-obsd: %v\n", err)
		os.Exit(1)
	}
}

// run is separated from main so tests can drive the flag surface without
// spawning a process or exiting the test binary.
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

	return errors.New("capture pipeline lands in Phase 3")
}

func parseFlags(args []string, out io.Writer) (config, error) {
	var cfg config

	fs := flag.NewFlagSet("devbox-obsd", flag.ContinueOnError)
	fs.SetOutput(out)
	fs.BoolVar(&cfg.showVersion, "version", false, "print version and exit")
	fs.StringVar(&cfg.socket, "socket", "/run/devbox/obsd.sock",
		"unix socket path or vsock address to stream events to")
	fs.StringVar(&cfg.boxID, "box-id", "",
		"box identifier stamped onto every event")
	fs.BoolVar(&cfg.noEBPF, "no-ebpf", false,
		"degraded mode: proc polling and tap-based DNS/flow only")

	if err := fs.Parse(args); err != nil {
		return config{}, err
	}
	return cfg, nil
}
