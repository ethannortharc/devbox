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
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/ethannortharc/devbox/internal/buildinfo"
	"github.com/ethannortharc/devbox/ztpd/api"
	"github.com/ethannortharc/devbox/ztpd/statemachine"
)

// config is the server's runtime configuration, parsed from flags.
type config struct {
	showVersion bool
	listen      string
	metrics     string
	sotPath     string
	advertise   string
}

// bootURL is what the bootstrap script and the config URLs point back at.
//
// A node fetches these over the network, so the address has to be one it can
// reach — not `0.0.0.0` and not `localhost`.
func (c config) bootURL() string {
	host := c.advertise
	if host == "" {
		host = "10.0.0.1"
	}
	_, port, err := net.SplitHostPort(c.listen)
	if err != nil || port == "" {
		port = "8080"
	}
	return fmt.Sprintf("http://%s", net.JoinHostPort(host, port))
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

	catalog, err := loadCatalog(cfg.sotPath)
	if err != nil {
		return err
	}

	statePath := filepath.Join(cfg.sotPath, "ztpd-state.json")
	registry, err := statemachine.Load(statePath)
	if err != nil {
		return err
	}
	// §10.3 kills this process mid-provision and expects the fabric to
	// self-heal, so a mutation is durable *before* the node is told it
	// happened. A 2s ticker lost whatever landed in the last tick — which is
	// exactly the window the chaos test aims at.
	registry.Persisting(statePath, func(err error) {
		fmt.Fprintf(os.Stderr, "devbox-ztpd: could not persist state: %v\n", err)
	})
	server := api.New(registry, catalog, cfg.bootURL())
	fmt.Fprintf(out, "%s listening on %s (%d device(s) known)\n",
		buildinfo.String(buildinfo.Ztpd), cfg.listen, len(catalog.Serials))
	fmt.Fprintf(out, "  DHCP option 67 should point at %s/bootstrap.sh\n", cfg.bootURL())

	// Metrics get their own listener, as the flag advertises. A node fetching
	// its config should not be able to reach the metrics surface by accident,
	// and a scrape target of `:9090` that refuses connections is worse than
	// no flag at all.
	metricsMux := http.NewServeMux()
	metricsMux.Handle("GET /metrics", server.Handler())
	metricsSrv := &http.Server{
		Addr:              cfg.metrics,
		Handler:           metricsMux,
		ReadHeaderTimeout: 5 * time.Second,
	}
	go func() {
		if err := metricsSrv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			fmt.Fprintf(os.Stderr, "devbox-ztpd: metrics listener: %v\n", err)
		}
	}()
	fmt.Fprintf(out, "  metrics on %s\n", cfg.metrics)

	srv := &http.Server{
		Addr:              cfg.listen,
		Handler:           server.Handler(),
		ReadHeaderTimeout: 5 * time.Second,
	}
	return srv.ListenAndServe()
}

// loadCatalog reads the rendered artifacts the Python side produced.
//
// The source-of-truth directory holds `serials.json` (serial → device) and one
// `<device>.conf` per device. Rendering happens in `labkit`; ztpd only serves
// what is there, which keeps the Go side free of templating and the Python
// side free of HTTP.
func loadCatalog(dir string) (*api.MapCatalog, error) {
	catalog := &api.MapCatalog{
		Serials: map[string]api.DeviceIdentity{},
		Configs: map[string]string{},
	}

	raw, err := os.ReadFile(filepath.Join(dir, "serials.json"))
	if err != nil {
		return nil, fmt.Errorf("read the serial map: %w", err)
	}
	if err := json.Unmarshal(raw, &catalog.Serials); err != nil {
		return nil, fmt.Errorf("parse the serial map: %w", err)
	}

	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", dir, err)
	}
	for _, entry := range entries {
		name, ok := strings.CutSuffix(entry.Name(), ".conf")
		if !ok {
			continue
		}
		config, err := os.ReadFile(filepath.Join(dir, entry.Name()))
		if err != nil {
			return nil, fmt.Errorf("read %s: %w", entry.Name(), err)
		}
		catalog.Configs[name] = string(config)
	}

	if len(catalog.Serials) == 0 {
		return nil, fmt.Errorf("%s knows no serials; nothing could ever provision", dir)
	}
	return catalog, nil
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
	fs.StringVar(&cfg.sotPath, "sot", "/etc/devbox/ztp",
		"directory holding serials.json and the rendered <device>.conf files")
	fs.StringVar(&cfg.advertise, "advertise", "",
		"address nodes should reach this server at (defaults to 10.0.0.1)")

	if err := fs.Parse(args); err != nil {
		return config{}, err
	}
	return cfg, nil
}
