// Package api is devbox-ztpd's HTTP surface — §10.1, §10.2.
//
// The flow a blank node walks:
//
//	GET  /bootstrap.sh          the script DHCP option 67 points at
//	POST /identify              "here is my serial — who am I?"
//	GET  /config/{name}          the rendered config for that device
//	POST /status                 "I applied it / I failed"
//	GET  /status                 what the operator and the test SDK read
//	GET  /metrics                Prometheus
//
// Every handler is small and the interesting logic lives in
// `statemachine`, so the provisioning rules are tested without a socket.
package api

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"strings"

	"github.com/ethannortharc/devbox/ztpd/statemachine"
)

// Catalog answers "who is this serial, and what config should it have?".
//
// An interface rather than a concrete type because the answer comes from the
// Python source of truth, which reaches the server as rendered files — and
// because a test needs to answer it without either.
type Catalog interface {
	// Lookup maps a hardware serial to a device name and role.
	Lookup(serial string) (name, role string, ok bool)
	// Config returns the rendered configuration for a device.
	Config(name string) (string, bool)
}

// MapCatalog is a Catalog backed by plain maps.
type MapCatalog struct {
	Serials map[string]DeviceIdentity
	Configs map[string]string
}

// DeviceIdentity is what a serial resolves to.
type DeviceIdentity struct {
	Name string
	Role string
}

// Lookup implements Catalog.
func (c *MapCatalog) Lookup(serial string) (string, string, bool) {
	identity, ok := c.Serials[serial]
	return identity.Name, identity.Role, ok
}

// Config implements Catalog.
func (c *MapCatalog) Config(name string) (string, bool) {
	config, ok := c.Configs[name]
	return config, ok
}

// Server is the ZTP HTTP service.
type Server struct {
	registry *statemachine.Registry
	catalog  Catalog
	// bootURL is what the bootstrap script points back at.
	bootURL string
}

// New builds a server.
func New(registry *statemachine.Registry, catalog Catalog, bootURL string) *Server {
	return &Server{registry: registry, catalog: catalog, bootURL: bootURL}
}

// Handler returns every route, including metrics.
//
// Used for the dedicated metrics listener and by tests. The node-facing
// listener must use ProvisioningHandler instead.
func (s *Server) Handler() http.Handler {
	mux := s.provisioningMux()
	mux.HandleFunc("GET /metrics", s.metrics)
	mux.HandleFunc("GET /status", s.status)
	return mux
}

// ProvisioningHandler returns the routes a booting node needs, and no others.
//
// Blank devices boot from the provisioning network, and `/metrics` carries
// every node name and its provisioning state. Serving it there gave anything
// on that network a fabric inventory, which is exactly what the separate
// -metrics listener exists to prevent.
func (s *Server) ProvisioningHandler() http.Handler {
	return s.provisioningMux()
}

func (s *Server) provisioningMux() *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /bootstrap.sh", s.bootstrap)
	mux.HandleFunc("POST /identify", s.identify)
	mux.HandleFunc("GET /config/{name}", s.config)
	mux.HandleFunc("POST /status", s.report)
	// GET /status is *not* here. It returns the whole registry — serials,
	// names, roles, states, config hashes — and the bootstrap script never
	// reads it. Leaving it on the node-facing listener let the provisioning
	// network enumerate the fabric even after /metrics moved off, which is
	// the same exposure with a different path.
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "ok")
	})
	return mux
}

// IdentifyRequest is what a blank node posts to learn who it is.
type IdentifyRequest struct {
	Serial string `json:"serial"`
}

// IdentifyResponse tells it.
type IdentifyResponse struct {
	Name string `json:"name"`
	Role string `json:"role"`
	// ConfigURL is where to fetch the rendered config.
	ConfigURL string `json:"config_url"`
}

// StatusRequest is a node reporting progress.
type StatusRequest struct {
	Serial string `json:"serial"`
	State  string `json:"state"`
	Reason string `json:"reason,omitempty"`
}

// bootstrap serves the shell script DHCP option 67 points at.
//
// The script lives in a Go raw string, so it must contain no backticks —
// including in comments. Use double quotes when naming a command.
func (s *Server) bootstrap(w http.ResponseWriter, _ *http.Request) {
	// A shell script, because that is what a blank node can run: no
	// interpreter to install, no package to fetch first.
	w.Header().Set("Content-Type", "text/x-shellscript")
	fmt.Fprintf(w, `#!/bin/sh
# devbox ZTP bootstrap — fetched via DHCP option 67.
set -eu

ZTP="%s"
SERIAL="$(cat /sys/class/dmi/id/product_serial 2>/dev/null || cat /etc/machine-id)"

# §10.3 kills ztpd mid-provision. A dropped report is not cosmetic: if the
# final "healthy" is lost, the node finishes and exits while the registry —
# and ztp_fabric_converged — stay stale forever, because nothing schedules a
# reconciliation. So retry across a restart window before giving up, and say
# so on stderr when we do.
report() {
  _state="$1"; _reason="${2:-}"; _try=0
  while [ "$_try" -lt 12 ]; do
    if wget -q -O- --post-data="{\"serial\":\"$SERIAL\",\"state\":\"$_state\",\"reason\":\"$_reason\"}" \
      --header='Content-Type: application/json' "$ZTP/status" >/dev/null 2>&1; then
      return 0
    fi
    _try=$((_try + 1))
    sleep 5
  done
  echo "devbox-ztp: could not report '$_state' after $_try attempts" >&2
  return 1
}

# Who am I?
IDENTITY="$(wget -q -O- --post-data="{\"serial\":\"$SERIAL\"}" \
  --header='Content-Type: application/json' "$ZTP/identify")" || {
  report failed "identify failed"; exit 1; }

NAME="$(echo "$IDENTITY" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')"
[ -n "$NAME" ] || { report failed "no name for serial $SERIAL"; exit 1; }

report rendering

# Fetch. Compare against what is already installed before touching anything:
# re-pushing identical config would restart routing for no reason, which is
# exactly what makes a reconciliation loop unsafe.
mkdir -p /etc/frr
wget -q -O /tmp/frr.conf.new "$ZTP/config/$NAME" || {
  report failed "config fetch failed"; exit 1; }

# The hostname is runtime state, not a file, so it does not survive a reboot
# and has to be set on every run — not only when the config changed. A node
# that rebooted and found its config unchanged used to come back healthy with
# the OS default hostname, which is what an operator reads in every log.
hostname "$NAME"

if [ -f /etc/frr/frr.conf ] && cmp -s /tmp/frr.conf.new /etc/frr/frr.conf; then
  rm -f /tmp/frr.conf.new
  report verifying
else
  report pushing
  mv /tmp/frr.conf.new /etc/frr/frr.conf
  # Not "|| true". A restart that fails leaves the *old* bgpd running with the
  # old configuration, and the verification below then finds established
  # sessions and reports healthy — for a node that never loaded the config it
  # was just given.
  if ! /etc/init.d/frr restart >/dev/null 2>&1 && \
     ! service frr restart >/dev/null 2>&1; then
    report failed "frr restart failed; the new config was not loaded"
    exit 1
  fi
  report verifying
fi

# Self-check, then phone home.
#
# "show bgp summary" exits 0 whenever bgpd is answering, even with every
# neighbour Idle — so the old check reported healthy for a router with no
# routing at all, and the fabric declared convergence on it. What matters is
# that peers reach Established, and that takes a moment after a restart.
# Positively: count sessions that are Established, and require at least one.
#
# Checking for the *absence* of Idle/Active/Connect passes on "No BGP neighbors
# found", on OpenSent/OpenConfirm, and on any state nobody thought to list —
# so a router with no sessions at all reported healthy. Absence of the states
# you remembered is not presence of the state you need.
_ok=0
_try=0
while [ "$_try" -lt 30 ]; do
  _summary="$(vtysh -c 'show bgp summary' 2>/dev/null || true)"
  if [ -n "$_summary" ]; then
    # "show bgp summary" prints one row per neighbour; Established rows carry
    # an uptime in the Up/Down column instead of a state word.
    # FRR writes Up/Down as HH:MM:SS for the first day, then 1d02h, then
    # 1w3d. Matching only the clock form reported a fabric that had been up
    # for a day as failed — the rerun-after-a-week case the idempotence
    # story depends on.
    _est="$(echo "$_summary" | grep -cE 'Established|[0-9]{2}:[0-9]{2}:[0-9]{2}|[0-9]+[dw][0-9]+[hd]' || true)"
    _pending="$(echo "$_summary" | grep -cE 'Idle|Active|Connect|OpenSent|OpenConfirm' || true)"
    if [ "$_est" -gt 0 ] && [ "$_pending" -eq 0 ]; then
      _ok=1
      break
    fi
  fi
  _try=$((_try + 1))
  sleep 2
done

if [ "$_ok" -eq 1 ]; then
  report healthy
else
  report failed "no established bgp session within 60s"
  # Nonzero, so a boot hook or supervisor sees a failure and retries. Falling
  # off the end here exited 0 and looked like a successful provision, with the
  # failure visible only to whoever read the registry.
  exit 1
fi
`, s.bootURL)
}

func (s *Server) identify(w http.ResponseWriter, r *http.Request) {
	var req IdentifyRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 4096)).Decode(&req); err != nil {
		httpError(w, http.StatusBadRequest, "malformed identify request: %v", err)
		return
	}

	node, restarted, err := s.registry.Discover(req.Serial)
	if err != nil {
		httpError(w, http.StatusBadRequest, "%v", err)
		return
	}
	_ = restarted

	name, role, ok := s.catalog.Lookup(req.Serial)
	if !ok {
		// An unknown serial is a real condition, not an error to hide: it
		// means a device is on the network that the source of truth does not
		// know about, which an operator wants to see.
		if _, err := s.registry.Advance(req.Serial, statemachine.Failed,
			"serial is not in the source of truth"); err != nil {
			httpError(w, http.StatusInternalServerError, "%v", err)
			return
		}
		httpError(w, http.StatusNotFound,
			"serial %q is not in the source of truth", req.Serial)
		return
	}

	if _, err := s.registry.Identify(node.Serial, name, role); err != nil {
		httpError(w, http.StatusConflict, "%v", err)
		return
	}

	writeJSON(w, http.StatusOK, IdentifyResponse{
		Name:      name,
		Role:      role,
		ConfigURL: fmt.Sprintf("%s/config/%s", s.bootURL, name),
	})
}

func (s *Server) config(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("name")
	config, ok := s.catalog.Config(name)
	if !ok {
		httpError(w, http.StatusNotFound, "no rendered config for %q", name)
		return
	}

	// The hash lets a node (and the registry) tell "same config" from
	// "changed config" without diffing, which is what keeps re-provisioning
	// idempotent.
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.Header().Set("X-Devbox-Config-Hash", Hash(config))
	fmt.Fprint(w, config)
}

func (s *Server) report(w http.ResponseWriter, r *http.Request) {
	var req StatusRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 4096)).Decode(&req); err != nil {
		httpError(w, http.StatusBadRequest, "malformed status report: %v", err)
		return
	}

	state := statemachine.State(strings.TrimSpace(req.State))
	if !statemachine.Valid(state) {
		httpError(w, http.StatusBadRequest, "unknown state %q", req.State)
		return
	}

	node, err := s.registry.Advance(req.Serial, state, req.Reason)
	if err != nil {
		// A refused transition is the node and the server disagreeing about
		// where provisioning is, which is worth a distinct status.
		httpError(w, http.StatusConflict, "%v", err)
		return
	}
	writeJSON(w, http.StatusOK, node)
}

func (s *Server) status(w http.ResponseWriter, _ *http.Request) {
	summary := s.registry.Summarize()
	missing := s.missingSerials()

	writeJSON(w, http.StatusOK, map[string]any{
		"nodes":   s.registry.List(),
		"total":   summary.Total,
		"healthy": summary.Healthy,
		"failed":  summary.Failed,
		// Expected, not observed. `Summarize` can only see nodes that have
		// identified, so a node that never boots is invisible to it — and 19
		// healthy out of an expected 20 reported `converged: true`, which is
		// the one answer a fabric-convergence signal must never get wrong.
		"expected":  len(s.catalogSerials()),
		"missing":   missing,
		"converged": summary.Converged && len(missing) == 0,
		"p95_secs":  summary.P95().Seconds(),
	})
}

// catalogSerials is every serial the source of truth expects to see.
func (s *Server) catalogSerials() []string {
	catalog, ok := s.catalog.(*MapCatalog)
	if !ok {
		// A catalog that cannot enumerate itself cannot contribute an expected
		// count; convergence then means what it meant before.
		return nil
	}
	serials := make([]string, 0, len(catalog.Serials))
	for serial := range catalog.Serials {
		serials = append(serials, serial)
	}
	sort.Strings(serials)
	return serials
}

// missingSerials is every expected device that has not reported at all.
func (s *Server) missingSerials() []string {
	seen := map[string]struct{}{}
	for _, node := range s.registry.List() {
		seen[node.Serial] = struct{}{}
	}
	var missing []string
	for _, serial := range s.catalogSerials() {
		if _, ok := seen[serial]; !ok {
			missing = append(missing, serial)
		}
	}
	return missing
}

func (s *Server) metrics(w http.ResponseWriter, _ *http.Request) {
	summary := s.registry.Summarize()

	var b strings.Builder
	b.WriteString("# HELP ztp_nodes_total Nodes the server has seen.\n")
	b.WriteString("# TYPE ztp_nodes_total gauge\n")
	fmt.Fprintf(&b, "ztp_nodes_total %d\n", summary.Total)

	b.WriteString("# HELP ztp_nodes_by_state Nodes in each provisioning state.\n")
	b.WriteString("# TYPE ztp_nodes_by_state gauge\n")
	// Every state is emitted, at zero if need be: a series that only appears
	// once it is non-zero cannot be alerted on.
	states := append(append([]statemachine.State{}, statemachine.Order...), statemachine.Failed)
	for _, state := range states {
		fmt.Fprintf(&b, "ztp_nodes_by_state{state=%q} %d\n", state, summary.ByState[state])
	}

	b.WriteString("# HELP ztp_nodes_expected Devices the source of truth expects.\n")
	b.WriteString("# TYPE ztp_nodes_expected gauge\n")
	fmt.Fprintf(&b, "ztp_nodes_expected %d\n", len(s.catalogSerials()))

	b.WriteString("# HELP ztp_nodes_missing Expected devices that have never reported.\n")
	b.WriteString("# TYPE ztp_nodes_missing gauge\n")
	missing := s.missingSerials()
	fmt.Fprintf(&b, "ztp_nodes_missing %d\n", len(missing))

	b.WriteString("# HELP ztp_fabric_converged 1 when every expected node is healthy.\n")
	b.WriteString("# TYPE ztp_fabric_converged gauge\n")
	converged := 0
	if summary.Converged && len(missing) == 0 {
		converged = 1
	}
	fmt.Fprintf(&b, "ztp_fabric_converged %d\n", converged)

	b.WriteString("# HELP node_provision_seconds Provisioning time, 95th percentile.\n")
	b.WriteString("# TYPE node_provision_seconds gauge\n")
	fmt.Fprintf(&b, "node_provision_seconds{quantile=\"0.95\"} %.3f\n", summary.P95().Seconds())

	b.WriteString("# HELP ztp_node_attempts Provisioning attempts per node.\n")
	b.WriteString("# TYPE ztp_node_attempts gauge\n")
	nodes := s.registry.List()
	sort.Slice(nodes, func(i, j int) bool { return nodes[i].Serial < nodes[j].Serial })
	for _, node := range nodes {
		name := node.Name
		if name == "" {
			name = node.Serial
		}
		fmt.Fprintf(&b, "ztp_node_attempts{node=%q} %d\n", name, node.Attempts)
	}

	w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
	fmt.Fprint(w, b.String())
}

// Hash identifies a rendered config.
func Hash(config string) string {
	sum := sha256.Sum256([]byte(config))
	return hex.EncodeToString(sum[:8])
}

func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	if err := json.NewEncoder(w).Encode(body); err != nil {
		// The status line is already sent, so there is nothing useful left to
		// say to the client.
		return
	}
}

func httpError(w http.ResponseWriter, status int, format string, args ...any) {
	http.Error(w, fmt.Sprintf(format, args...), status)
}
