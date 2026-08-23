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
	"sync"
	"time"

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
	// unknown remembers serials the source of truth does not know, bounded.
	unknown *unknownSerials
}

// New builds a server.
func New(registry *statemachine.Registry, catalog Catalog, bootURL string) *Server {
	return &Server{
		registry: registry,
		catalog:  catalog,
		bootURL:  bootURL,
		unknown:  newUnknownSerials(maxUnknownSerials),
	}
}

// maxUnknownSerials bounds what one unauthenticated network can make us hold.
//
// Large enough that a genuinely misconfigured fabric — a whole rack cabled
// before its serials were added to the catalog — is still visible in full;
// small enough that it is a rounding error against the process.
const maxUnknownSerials = 256

// unknownSerials records serials that are not in the source of truth.
//
// Not the registry. An unknown serial used to be recorded there as a failed
// node, which meant anything on the provisioning network — which is
// unauthenticated by design, since a blank device has no credential to offer —
// could post an endless stream of distinct serials and have every one of them
// inserted, persisted synchronously, and never evicted. Memory and the state
// file grew without bound, and because each save writes a full snapshot, every
// subsequent save cost more: the failure mode is that real nodes stop being
// able to provision.
//
// The operator still needs the signal — a device on the network the catalog
// has never heard of is worth seeing — so it is kept here instead: bounded, in
// memory only, and never written to disk.
type unknownSerials struct {
	mu    sync.Mutex
	limit int
	seen  map[string]time.Time
	// order is insertion order, for evicting the oldest once full.
	order []string
}

func newUnknownSerials(limit int) *unknownSerials {
	return &unknownSerials{limit: limit, seen: make(map[string]time.Time)}
}

// Record notes one serial, evicting the oldest if the bound is reached.
func (u *unknownSerials) Record(serial string, at time.Time) {
	u.mu.Lock()
	defer u.mu.Unlock()

	if _, ok := u.seen[serial]; ok {
		// Refresh the time, keep its place: a device retrying every thirty
		// seconds must not evict the rest of the rack.
		u.seen[serial] = at
		return
	}
	if len(u.order) >= u.limit {
		oldest := u.order[0]
		u.order = u.order[1:]
		delete(u.seen, oldest)
	}
	u.order = append(u.order, serial)
	u.seen[serial] = at
}

// Len is how many distinct unknown serials are being held.
func (u *unknownSerials) Len() int {
	u.mu.Lock()
	defer u.mu.Unlock()
	return len(u.seen)
}

// Handler returns every route. Tests use it; neither listener does.
//
// Serving this on the operator listener was an attempt to make the two route
// sets "complementary by construction" — but a superset is not a complement.
// It put `/identify`, `/config/{name}`, and `POST /status` on the management
// network, so anything there could spoof a node's identity or its progress.
// The two listeners each get their own handler, and a test asserts the routes
// absent from each.
func (s *Server) Handler() http.Handler {
	mux := s.provisioningMux()
	mux.HandleFunc("GET /metrics", s.metrics)
	mux.HandleFunc("GET /status", s.status)
	return mux
}

// OperatorHandler returns the read-only inventory routes, and nothing else.
//
// Nothing here mutates state: a management-network client should be able to
// read what the fabric is doing without being able to change it.
func (s *Server) OperatorHandler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /metrics", s.metrics)
	mux.HandleFunc("GET /status", s.status)
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		_, _ = fmt.Fprintln(w, "ok")
	})
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
		_, _ = fmt.Fprintln(w, "ok")
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
	// ConfigHash is the hash of the config the node actually activated.
	//
	// Reported by the node rather than re-read here, because "what this server
	// would serve now" and "what that node installed" are different facts and
	// only the second is being recorded. They diverge whenever the rendered
	// source of truth changes between a fetch and the report that follows it —
	// a ztpd restart with a node retrying its report is enough — and the
	// server's answer is then confidently wrong: `/status` names a config the
	// node never saw and `NeedsPush` says nothing needs pushing.
	ConfigHash string `json:"config_hash,omitempty"`
}

// bootstrap serves the shell script DHCP option 67 points at.
//
// The script lives in a Go raw string, so it must contain no backticks —
// including in comments. Use double quotes when naming a command.
func (s *Server) bootstrap(w http.ResponseWriter, _ *http.Request) {
	// A shell script, because that is what a blank node can run: no
	// interpreter to install, no package to fetch first.
	w.Header().Set("Content-Type", "text/x-shellscript")
	_, _ = fmt.Fprintf(w, `#!/bin/sh
# devbox ZTP bootstrap — fetched via DHCP option 67.
set -eu

ZTP="%s"
# "cat" on an empty DMI file succeeds, so "||" never fired — a board that
# reports a blank serial produced an empty identity instead of falling back.
# "set -e" is on, and a failed command substitution fails the assignment — so
# on ARM and anything else without a DMI serial file, the script exited here
# before the fallback could run. The trailing "|| true" keeps the assignment
# succeeding with an empty value, and the emptiness check does the real work.
SERIAL="${DEVBOX_ZTP_SERIAL:-}"
if [ -z "$SERIAL" ]; then
  SERIAL="$(cat /sys/class/dmi/id/product_serial 2>/dev/null || true)"
  [ -n "$SERIAL" ] || SERIAL="$(cat /etc/machine-id 2>/dev/null || true)"
fi
# Stripped to what a serial can legitimately contain. It is interpolated into
# the JSON bodies below, so a quote or a backslash from DMI — which is not a
# trusted source; it is whatever the board vendor wrote — would produce a
# malformed request or inject a field.
SERIAL="$(echo "$SERIAL" | tr -cd 'A-Za-z0-9._:-' | cut -c1-128)"

# §10.3 kills ztpd mid-provision. A dropped report is not cosmetic: if the
# final "healthy" is lost, the node finishes and exits while the registry —
# and ztp_fabric_converged — stay stale forever, because nothing schedules a
# reconciliation. So retry across a restart window before giving up, and say
# so on stderr when we do.
report() {
  _state="$1"; _reason="${2:-}"; _try=0
  while [ "$_try" -lt 12 ]; do
    if wget -q -O- --post-data="{\"serial\":\"$SERIAL\",\"state\":\"$_state\",\"reason\":\"$_reason\",\"config_hash\":\"${CFG_HASH:-}\"}" \
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
FRR_DIR="${DEVBOX_ZTP_FRR_DIR:-/etc/frr}"
FRR_CONF="$FRR_DIR/frr.conf"
mkdir -p "$FRR_DIR"
wget -q -O /tmp/frr.conf.new "$ZTP/config/$NAME" || {
  report failed "config fetch failed"; exit 1; }

# The hostname is runtime state, not a file, so it does not survive a reboot
# and has to be set on every run — not only when the config changed. A node
# that rebooted and found its config unchanged used to come back healthy with
# the OS default hostname, which is what an operator reads in every log.
if [ "${DEVBOX_ZTP_SKIP_HOSTNAME:-0}" != "1" ]; then
  hostname "$NAME"
fi

# Compare against the config that was last successfully *activated*, not the
# one sitting on disk.
#
# The file has to be moved into place before FRR can load it, so on a failed
# restart the on-disk copy already equalled the new one. The next supervisor
# retry then compared them, found no difference, and skipped the restart — for
# good. The node was left either with no daemon at all or with a daemon still
# running the previous configuration, and in the second case the verification
# below finds established sessions and reports healthy.
#
# The marker is written only after a restart returns success, so a failure
# leaves it stale and the retry does the work again. That is the whole
# difference between "the file is in place" and "the daemon has read it".
#
# And the marker records that a restart once succeeded, not that FRR is running
# now. A daemon that crashed or was stopped afterwards leaves the config still
# matching, so this branch skipped the restart and went straight to
# verification — which fails, after which every supervisor retry takes the same
# branch and skips the same restart. The node stayed down permanently, and the
# only thing that would have freed it was a config change nobody had a reason
# to make.
#
# Liveness is asked in the same terms the verification below uses: whether
# vtysh gets an answer. A daemon that cannot answer cannot be verified either,
# so the two agree by construction rather than by two people remembering to
# keep them in step.
ACTIVATED="$FRR_DIR/.devbox-activated"
frr_answers() {
  if [ -n "${DEVBOX_ZTP_NETNS:-}" ]; then
    [ -n "$(vtysh -N "$DEVBOX_ZTP_NETNS" -c 'show bgp summary' 2>/dev/null || true)" ]
  else
    [ -n "$(vtysh -c 'show bgp summary' 2>/dev/null || true)" ]
  fi
}
restart_frr() {
  if [ -z "${DEVBOX_ZTP_NETNS:-}" ]; then
    /etc/init.d/frr restart >/dev/null 2>&1 || service frr restart >/dev/null 2>&1
    return
  fi

  _run="${DEVBOX_ZTP_RUN_DIR:-/run/devbox-ztp}"
  mkdir -p "$_run" "/run/frr/$DEVBOX_ZTP_NETNS" "/etc/frr/$DEVBOX_ZTP_NETNS"
  touch "/etc/frr/$DEVBOX_ZTP_NETNS/vtysh.conf"
  for _daemon in bgpd zebra mgmtd; do
    _pidfile="$_run/$_daemon.pid"
    if [ -s "$_pidfile" ]; then
      kill "$(cat "$_pidfile")" 2>/dev/null || true
    fi
  done
  # mgmtd first, and it takes no -f.
  #
  # FRR 10 moved interface configuration into mgmtd's northbound datastore.
  # Without it running, "interface lo" is not a command zebra knows: it logs
  # "No such command", carries on, and the node comes up with its BGP
  # configuration and none of its addresses. A node provisioned this way has
  # nothing else to configure it -- that is the whole point -- so its loopback
  # never appeared, the prefix it advertised was never valid, and no peer could
  # reach it. Every per-node check still passed, and only the fabric-wide
  # reachability matrix failed.
  # Only if this FRR has it: mgmtd arrived in 9 and owns interface config from
  # 10. A node running FRR 8 -- Debian bookworm ships 8.4 -- has no such binary
  # and does not need one, because there zebra still owns it.
  if command -v mgmtd >/dev/null 2>&1; then
    mgmtd -u root -g root -d -i "$_run/mgmtd.pid" \
      -z "$_run/zserv.api" -N "$DEVBOX_ZTP_NETNS" || return 1
  fi
  zebra -u root -g root -d -f "$FRR_CONF" -i "$_run/zebra.pid" \
    -z "$_run/zserv.api" -N "$DEVBOX_ZTP_NETNS" || return 1
  bgpd -u root -g root -d -f "$FRR_CONF" -i "$_run/bgpd.pid" \
    -z "$_run/zserv.api" -N "$DEVBOX_ZTP_NETNS" || return 1

  # Then distribute the file, because no single daemon reading it applies all
  # of it: each one's -f keeps the commands it owns and silently drops the
  # rest. vtysh holds a session to all of them and hands each line to whichever
  # owns it, which is what an integrated frr.conf is for.
  vtysh -N "$DEVBOX_ZTP_NETNS" -f "$FRR_CONF" >/dev/null 2>&1
}
#
# The marker also has to still describe what is *installed*. It records what a
# restart once loaded; the frr.conf file is what the daemon reads. Edit that
# file and restart FRR, or reboot the node, and the two diverge -- the daemon
# is running the edit while the marker names the old config. Skipping on the
# marker alone then left the edit in place and reported the marker hash as
# active, so the server believed the fabric had converged on a configuration no
# node was running, and skipped the push that would have corrected it.
#
# Comparing the installed file too costs nothing and makes the skip mean "this
# node is running exactly what was activated", which is what the caller reads
# it as.
if [ -f "$ACTIVATED" ] && cmp -s /tmp/frr.conf.new "$ACTIVATED" \
  && cmp -s "$FRR_CONF" "$ACTIVATED" && frr_answers; then
  rm -f /tmp/frr.conf.new
  report verifying
else
  report pushing
  mv /tmp/frr.conf.new "$FRR_CONF"
  # Not "|| true". A restart that fails leaves the *old* bgpd running with the
  # old configuration, and the verification below then finds established
  # sessions and reports healthy — for a node that never loaded the config it
  # was just given.
  if ! restart_frr; then
    report failed "frr restart failed; the new config was not loaded"
    exit 1
  fi
  cp "$FRR_CONF" "$ACTIVATED"
  report verifying
fi

# What was actually activated, for the report below to carry.
#
# The server used to re-read its own catalog when the node said healthy, which
# answers "what would I serve now" rather than "what did that node install".
# The two differ across a source-of-truth change, and a node retrying its report
# across a ztpd restart is exactly when they do.
#
# Hashed from the marker, not from frr.conf. The marker is written only after a
# restart returns success, so it names the configuration the daemon actually
# loaded; frr.conf is merely what is on disk. A manual edit between runs makes
# them differ, and the skip branch above trusts the marker — so hashing the file
# would report a configuration that was never activated, and the server would
# then skip the push that would have corrected it.
#
# The server truncates sha256 to eight bytes, which is the first sixteen hex
# characters of what sha256sum prints.
CFG_HASH="$(sha256sum "$ACTIVATED" 2>/dev/null | cut -c1-16)"

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
  if [ -n "${DEVBOX_ZTP_NETNS:-}" ]; then
    _summary="$(vtysh -N "$DEVBOX_ZTP_NETNS" -c 'show bgp summary' 2>/dev/null || true)"
  else
    _summary="$(vtysh -c 'show bgp summary' 2>/dev/null || true)"
  fi
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

# A resolver that sends a query, not the C library.
#
# "getent hosts" was the obvious way to write this and cannot work on the image
# devbox builds. A NixOS substrate runs nsncd, and glibc hands every name
# lookup to it over a unix socket; nsncd lives in the root network namespace
# and answers from the *host's* resolv.conf. A node in a lab namespace
# therefore asks a resolver that cannot see its fabric, gets nothing, and
# reports the fabric's DNS broken. tcpdump inside the node during that check
# shows no DNS packet at all, which is the tell: the query never reached the
# network.
#
# -type=A because the lab addresses IPv4 only, and because a bare lookup asks
# for A and AAAA together and fails if either leg does.
#
# Three ways down, because a substrate is whatever the user brought. nslookup
# on PATH is busybox's applet on the Nix image and bind's elsewhere; both take
# -type. busybox reaches the same applet where only the multi-call binary is
# installed -- a Debian slim image with no dnsutils, which is what the e2e
# fixture is. getent is the last resort and is correct wherever nsncd is not in
# the way; the two conditions do not overlap in practice, because the image
# that runs nsncd is the one that carries busybox.
dns_answers() {
  if command -v nslookup >/dev/null 2>&1; then
    nslookup -type=A "$1" >/dev/null 2>&1
  elif command -v busybox >/dev/null 2>&1; then
    busybox nslookup -type=A "$1" >/dev/null 2>&1
  else
    getent hosts "$1" >/dev/null 2>&1
  fi
}

if [ "$_ok" -eq 1 ] && [ -n "${DEVBOX_ZTP_DNS_NAME:-}" ]; then
  dns_answers "$DEVBOX_ZTP_DNS_NAME" || {
    report failed "DNS self-check failed"; exit 1; }
fi

if [ "$_ok" -eq 1 ] && [ -n "${DEVBOX_ZTP_NTP:-}" ]; then
  chronyd -Q -t 10 "server $DEVBOX_ZTP_NTP iburst" >/dev/null 2>&1 || {
    report failed "NTP self-check failed"; exit 1; }
fi

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

	// The catalog first, and nothing is written until it answers.
	//
	// This used to `Discover` — insert into the registry, and persist — before
	// asking who the serial belonged to, so an unknown serial became a stored,
	// permanent, failed node. On a provisioning network, which is
	// unauthenticated by design because a blank device has no credential to
	// present, that made the state file a thing any client could grow without
	// limit, one distinct serial at a time.
	name, role, ok := s.catalog.Lookup(req.Serial)
	if !ok {
		// Still a real condition and still worth seeing — a device is on the
		// network that the source of truth has never heard of — but held in a
		// bounded structure that never reaches disk.
		if req.Serial == "" {
			httpError(w, http.StatusBadRequest, "a node must present a serial")
			return
		}
		s.unknown.Record(req.Serial, time.Now())
		httpError(w, http.StatusNotFound,
			"serial %q is not in the source of truth", req.Serial)
		return
	}

	node, restarted, err := s.registry.Discover(req.Serial)
	if err != nil {
		httpError(w, http.StatusBadRequest, "%v", err)
		return
	}
	_ = restarted

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
	_, _ = fmt.Fprint(w, config)
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

	// Record what was activated, once the node says it took.
	//
	// `RecordPush` existed and nothing ever called it, so every real node kept
	// an empty `ConfigHash`: `/status` could not say which configuration a node
	// was actually running, and `NeedsPush` had nothing to compare against, so
	// the re-push decision it exists to make was never available.
	//
	// Recorded here rather than from the config fetch, because fetching is not
	// activating — a node can pull a config and then fail to load it. `healthy`
	// is the node's acknowledgement that FRR took it. And recorded from the
	// config *this server served*, not from anything the node reports: the
	// point is to know what was installed, which the node has no more authority
	// over than the server does.
	if state == statemachine.Healthy {
		activated := strings.TrimSpace(req.ConfigHash)
		if activated == "" {
			// A node from before the field existed. Re-reading the catalog is
			// what this did unconditionally, and it is wrong exactly when it
			// matters — but a missing hash is worse than a stale one, so it
			// stays as the fallback and only as the fallback.
			if name, _, ok := s.catalog.Lookup(req.Serial); ok {
				if config, ok := s.catalog.Config(name); ok {
					activated = Hash(config)
				}
			}
		}
		if activated != "" {
			{
				if err := s.registry.RecordPush(req.Serial, activated); err != nil {
					// Not fatal to the report: the node *is* healthy, and
					// saying otherwise would be a worse answer than a missing
					// hash. Worth surfacing, because a registry that cannot
					// persist is about to lose more than this.
					httpError(w, http.StatusInternalServerError,
						"node is healthy but its configuration hash could not be recorded: %v", err)
					return
				}
				// Re-read, so the response carries the hash just stored rather
				// than the snapshot taken before it.
				if updated, ok := s.registry.Get(req.Serial); ok {
					node = &updated
				}
			}
		}
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
		"expected": len(s.catalogSerials()),
		"missing":  missing,
		// Over the *expected* serials, not every serial ever seen. A stray
		// request from the provisioning network registers an unknown serial as
		// failed, and counting it held convergence false forever — a fabric
		// that is entirely healthy reporting otherwise because something
		// knocked on the door once.
		"converged": s.expectedConverged(summary) && len(missing) == 0,
		"p95_secs":  summary.P95().Seconds(),
	})
}

// expectedConverged reports whether every serial the catalog names is healthy.
//
// Distinct from `Summary.Converged`, which is over observed nodes: a serial the
// source of truth does not know about is an operational signal worth surfacing
// (it appears in the node list as failed) but it is not part of *this fabric's*
// convergence, and letting it veto that answer means any stray request can
// silence the signal permanently.
func (s *Server) expectedConverged(summary statemachine.Summary) bool {
	expected := s.catalogSerials()
	if len(expected) == 0 {
		// Nothing declared: fall back to what was observed, which is what the
		// answer meant before a catalog existed.
		return summary.Converged
	}
	for _, serial := range expected {
		node, ok := s.registry.Get(serial)
		if !ok || node.State != statemachine.Healthy {
			return false
		}
	}
	return true
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
	// An empty JSON array, not null. Callers consume this as a collection, and
	// `null` makes strongly typed clients reject an otherwise healthy status.
	missing := make([]string, 0)
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
	if s.expectedConverged(summary) && len(missing) == 0 {
		converged = 1
	}
	fmt.Fprintf(&b, "ztp_fabric_converged %d\n", converged)

	// Held here rather than in the registry, so it cannot distort the SLO
	// below or grow the state file — but the operator still needs to see that
	// something on the provisioning network is not in the catalog, which is
	// the whole reason the signal exists.
	b.WriteString("# HELP ztp_unknown_serials Distinct serials seen that the catalog does not know.\n")
	b.WriteString("# TYPE ztp_unknown_serials gauge\n")
	fmt.Fprintf(&b, "ztp_unknown_serials %d\n", s.unknown.Len())

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
		fmt.Fprintf(&b, "ztp_node_attempts{node=%s} %d\n", promLabel(name), node.Attempts)
	}

	w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
	_, _ = fmt.Fprint(w, b.String())
}

// promLabel renders a label value the Prometheus text format accepts.
//
// Go's %q emits Go escapes — \t, \x1b — and Prometheus rejects them, so a
// serial containing a control character would break *every* scrape until the
// node was removed. An unauthenticated provisioning client can register an
// arbitrary serial, which makes that a denial of service on the metrics
// surface rather than a cosmetic bug.
//
// The exposition format allows \\, \" and \n and nothing else, so anything
// outside printable ASCII is replaced rather than escaped.
func promLabel(value string) string {
	var b strings.Builder
	b.WriteByte('"')
	for _, r := range value {
		switch {
		case r == '\\':
			b.WriteString(`\\`)
		case r == '"':
			b.WriteString(`\"`)
		case r == '\n':
			b.WriteString(`\n`)
		case r < 0x20 || r > 0x7e:
			b.WriteByte('?')
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
	return b.String()
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
