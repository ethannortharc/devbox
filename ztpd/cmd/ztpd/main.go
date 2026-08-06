// Command ztpd is devbox-ztpd, the zero-touch-provisioning server.
//
// It serves bootstrap scripts and rendered device configs over HTTP, drives
// each node through the provisioning state machine
// (discovered → identified → rendering → pushing → verifying → healthy|failed),
// and exposes status and Prometheus metrics.
//
// The HTTP surface and state machine land in Phase 8. This entry point
// establishes the flag surface and identity handshake.
package main

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"os"

	"github.com/ethannortharc/devbox/internal/buildinfo"
)

// config is the server's runtime configuration, parsed from flags.
type config struct {
	showVersion bool
	listen      string
	metrics     string
	sotPath     string
}

func main() {
	if err := run(os.Args[1:], os.Stdout); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return
		}
		fmt.Fprintf(os.Stderr, "devbox-ztpd: %v\n", err)
		os.Exit(1)
	}
}

func run(args []string, out io.Writer) error {
	cfg, err := parseFlags(args, out)
	if err != nil {
		return err
	}

	if cfg.showVersion {
		_, err := fmt.Fprintln(out, buildinfo.String(buildinfo.Ztpd))
		return err
	}

	return errors.New("provisioning server lands in Phase 8")
}

func parseFlags(args []string, out io.Writer) (config, error) {
	var cfg config

	fs := flag.NewFlagSet("devbox-ztpd", flag.ContinueOnError)
	fs.SetOutput(out)
	fs.BoolVar(&cfg.showVersion, "version", false, "print version and exit")
	fs.StringVar(&cfg.listen, "listen", ":8080",
		"address to serve bootstrap scripts and configs on")
	fs.StringVar(&cfg.metrics, "metrics", ":9090",
		"address to serve Prometheus metrics on")
	fs.StringVar(&cfg.sotPath, "sot", "/etc/devbox/lab/sot.yaml",
		"path to the source-of-truth the rendered configs derive from")

	if err := fs.Parse(args); err != nil {
		return config{}, err
	}
	return cfg, nil
}
