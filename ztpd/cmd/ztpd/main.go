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
		// The service node's own address, discovered rather than assumed.
		//
		// `10.0.0.1` was a guess that happens to be wrong for the built-in
		// `ztp-fabric`: `svc` is endpoint A of the first derived /31, so IPAM
		// gives it `10.0.0.0` and gives `.1` to spine1. Every node was told to
		// fetch its config from the spine, where nothing is listening.
		host = advertiseHost(c.listen)
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
	// The operator routes, and only they. Two earlier attempts got this wrong
	// in opposite directions: dispatching one path to the full handler left
	// `GET /status` on neither listener, and then serving the full handler
	// here put every *provisioning* route on the management network, where
	// anything could spoof a node's identity. Each listener has its own
	// handler now, and tests assert what is absent from both.
	metricsSrv := &http.Server{
		Addr:    cfg.metrics,
		Handler: server.OperatorHandler(),
		// The same bounds as the provisioning listener. This one is on the
		// management network rather than the open one, which lowers the odds
		// and does not change the shape: a listener with no read or write
		// timeout can be held open by anything that reaches it, and "anything
		// that reaches it" is a weaker guarantee than it sounds on a network
		// that also carries a fabric.
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      30 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	// Bound synchronously, before startup is declared a success. A detached
	// ListenAndServe that fails to bind leaves the process running and the
	// banner claiming metrics are available, so monitoring sees a permanently
	// missing endpoint from a server that started cleanly.
	metricsLn, err := net.Listen("tcp", cfg.metrics)
	if err != nil {
		return fmt.Errorf("metrics listener on %s: %w", cfg.metrics, err)
	}
	go func() {
		if err := metricsSrv.Serve(metricsLn); err != nil && !errors.Is(err, http.ErrServerClosed) {
			fmt.Fprintf(os.Stderr, "devbox-ztpd: metrics listener: %v\n", err)
		}
	}()
	fmt.Fprintf(out, "  metrics on %s\n", cfg.metrics)

	// The node-facing listener serves provisioning only. `Handler()` includes
	// GET /metrics, so registering it here also published node names and
	// provisioning state on the network blank devices boot from — defeating
	// the port separation the -metrics flag exists to provide.
	srv := &http.Server{
		Addr:    cfg.listen,
		Handler: server.ProvisioningHandler(),
		// Headers *and* body, and the write side too.
		//
		// `ReadHeaderTimeout` stops applying the moment the headers are in, so
		// a client could complete them and then dribble the body of
		// `POST /identify` forever. This listener is on the provisioning
		// network, which is unauthenticated by design — a blank device has no
		// credential to present — so enough half-open requests exhaust
		// descriptors and goroutines and real devices stop being able to
		// provision. Bounding only the headers bounded the cheap half.
		//
		// Generous against what these requests actually are: a few hundred
		// bytes of JSON from a device on the same segment. A node that cannot
		// finish that in thirty seconds has a problem no timeout will fix.
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      30 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	return srv.ListenAndServe()
}

// firstNonLoopbackIPv4 is this host's own address on the fabric.
//
// A ZTP server sits on the provisioning network by definition, so its first
// non-loopback IPv4 address is the one nodes can reach. `-advertise` overrides
// it for the multi-homed case; the default is now at least *this machine*
// rather than an address from a topology that may not be the one running.
// advertiseHost is the address nodes should fetch from.
//
// If `-listen` names a specific address, that is the answer: the operator has
// already said which interface serves provisioning, and guessing a different
// one contradicts them. Only a wildcard bind has to be resolved to something
// concrete.
func advertiseHost(listen string) string {
	if host, _, err := net.SplitHostPort(listen); err == nil && host != "" {
		if ip := net.ParseIP(host); ip != nil && !ip.IsUnspecified() {
			return host
		}
	}
	return firstNonLoopbackIPv4()
}

func firstNonLoopbackIPv4() string {
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		return "127.0.0.1"
	}
	for _, addr := range addrs {
		ipnet, ok := addr.(*net.IPNet)
		if !ok || ipnet.IP.IsLoopback() {
			continue
		}
		if v4 := ipnet.IP.To4(); v4 != nil {
			return v4.String()
		}
	}
	// Nothing else to offer. A node cannot reach this, and the banner prints
	// the URL, so it is visible rather than silently wrong.
	return "127.0.0.1"
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
	// Loopback by default, not `:9090`. On a multi-homed ZTP host — which is
	// every real one, since it sits between a management network and the
	// provisioning network — a bare port binds every interface, so splitting
	// the operator routes onto their own port did nothing to keep blank
	// devices from scraping the fabric inventory. An operator who wants it
	// reachable can name a management address explicitly.
	fs.StringVar(&cfg.metrics, "metrics", "127.0.0.1:9090",
		"address to serve metrics and the status inventory on; defaults to "+
			"loopback so the provisioning network cannot reach it")
	fs.StringVar(&cfg.sotPath, "sot", "/etc/devbox/ztp",
		"directory holding serials.json and the rendered <device>.conf files")
	fs.StringVar(&cfg.advertise, "advertise", "",
		"address nodes should reach this server at (defaults to 10.0.0.1)")

	if err := fs.Parse(args); err != nil {
		return config{}, err
	}
	return cfg, nil
}
