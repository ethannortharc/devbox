// Package policy is the enforcement half of §8.
//
// The Rust control plane decides *what* the rules are and hands the agent a
// generated nftables ruleset; this package applies it and then keeps the named
// allow sets in sync with what DNS actually answers.
//
// That last part is the whole trick. An allowlist names domains, and a domain's
// addresses change — a CDN rotation, a failover, a geo-routed answer. Rather
// than re-resolving on a timer and hoping, the agent watches the DNS traffic it
// is already capturing: when a lookup for an allowlisted name returns an
// address, that address is added to the set. The firewall learns from the same
// resolution the application is about to use.
package policy

import (
	"context"
	"fmt"
	"net/netip"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// Table and set names, matching `src/policy/nftables.rs`.
const (
	Table = "devbox"
	SetV4 = "allow_v4"
	SetV6 = "allow_v6"
)

// Applier runs nftables commands. Swappable so the sync logic can be tested
// without root, a kernel, or nftables installed.
type Applier interface {
	// Apply loads a complete ruleset (equivalent to `nft -f -`).
	Apply(ctx context.Context, ruleset string) error
	// AddElement adds one address to a named set.
	AddElement(ctx context.Context, set, addr string) error
}

// NFT is the real applier, shelling out to `nft`.
type NFT struct{}

// Apply implements Applier.
func (NFT) Apply(ctx context.Context, ruleset string) error {
	cmd := exec.CommandContext(ctx, "nft", "-f", "-")
	cmd.Stdin = strings.NewReader(ruleset)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("nft -f -: %w: %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

// AddElement implements Applier.
func (NFT) AddElement(ctx context.Context, set, addr string) error {
	// The address is validated by the caller before it gets here; validating
	// again is cheap and this string becomes an argument to a root command.
	if _, err := netip.ParseAddr(addr); err != nil {
		return fmt.Errorf("refusing to add %q to %s: not an address", addr, set)
	}
	// `add element` on an element that already exists returns EEXIST and
	// leaves its timeout alone, so a refresh before expiry would do nothing
	// and the address would lapse anyway — the exact failure the refresh
	// window exists to prevent. Deleting first makes the add unconditional.
	//
	// The delete is best-effort: the element is usually absent, and the pair
	// is not atomic in the sense that matters here — a packet arriving in the
	// gap is denied and retried by the application, whereas an element that
	// silently stopped being renewed fails for an hour.
	// Tokenized, not one string. `exec.Command` does not shell-split, so
	// passing "add element inet devbox allow_v4 { 1.2.3.4 }" handed nft a
	// single argv entry it cannot parse — every insertion failed, and the
	// default-deny allow set stayed empty for the life of the box. The
	// enforcer has never actually added an address.
	//
	// The braces are their own arguments because nft's parser wants them as
	// separate tokens once the shell is not doing the splitting.
	// Delete first so the add is unconditional: `add element` on an existing
	// element returns EEXIST without renewing its timeout, and renewal is the
	// whole point of re-adding an address that is still in use.
	_ = exec.CommandContext(ctx, "nft", elementArgs("delete", set, addr)...).Run()

	cmd := exec.CommandContext(ctx, "nft", elementArgs("add", set, addr)...)
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("nft add %s %s: %w: %s", set, addr, err,
			strings.TrimSpace(string(out)))
	}
	return nil
}

// elementArgs builds the argv for an nft set-element operation.
//
// Separate from the call so the tokenization is testable without nft present:
// the bug it exists to prevent is invisible at runtime, because a failed
// insertion looks like a domain that simply cannot be reached.
func elementArgs(op, set, addr string) []string {
	return []string{op, "element", "inet", Table, set, "{", addr, "}"}
}

// AllowTTL is how long a DNS-derived allow-set entry is trusted.
//
// Must match the `timeout` the generated ruleset puts on the sets
// (`policy::nftables::ALLOW_TTL_SECS`): the kernel expires the element, and
// this decides when the agent is willing to add it again.
const AllowTTL = time.Hour

// AllowRefreshAfter is how long the agent waits before renewing an address it
// has already added. Comfortably inside AllowTTL, so an element is refreshed
// while it is still live rather than after it has lapsed.
const AllowRefreshAfter = AllowTTL * 3 / 4

// now is injectable so the expiry logic is testable without sleeping.
var now = time.Now

// Enforcer keeps the firewall in step with DNS.
type Enforcer struct {
	applier Applier

	mu sync.RWMutex
	// allow holds the domains the policy names, lower-cased.
	// allow maps a domain to whether the *apex* is permitted. A wildcard entry
	// (`*.example.com`) stores false: subdomains only.
	allow map[string]bool
	// seen records when each address was last added, so an entry that has
	// aged out of the kernel set is added again rather than suppressed
	// forever. See AllowTTL.
	seen map[string]time.Time
	// mirrorOnly permits the curated package hosts in addition to `allow`.
	mirrorOnly bool
	// added counts successful set insertions, for the metrics surface.
	added int
}

// New builds an enforcer for a set of allowlisted domains.
func New(applier Applier, domains []string, mirrorOnly bool) *Enforcer {
	// `*.example.com` means subdomains, which is what the console says it
	// means. Stripping the prefix and forgetting it had been there made the
	// entry permit the apex as well — a wider grant than the user wrote.
	allow := make(map[string]bool, len(domains))
	for _, d := range domains {
		d = strings.ToLower(strings.TrimSpace(d))
		wildcard := strings.HasPrefix(d, "*.")
		d = strings.TrimPrefix(d, "*.")
		if d != "" {
			// A bare entry also covers the apex; a wildcard entry does not.
			// If both forms appear, the broader one wins.
			allow[d] = allow[d] || !wildcard
		}
	}
	return &Enforcer{
		applier:    applier,
		allow:      allow,
		seen:       map[string]time.Time{},
		mirrorOnly: mirrorOnly,
	}
}

// Load applies a complete ruleset.
func (e *Enforcer) Load(ctx context.Context, ruleset string) error {
	// A fresh ruleset destroys and rebuilds the table, so every address the
	// enforcer previously added is gone. Forgetting them is what makes a
	// reload re-add them as DNS answers come in again.
	e.mu.Lock()
	e.seen = map[string]time.Time{}
	e.mu.Unlock()

	return e.applier.Apply(ctx, ruleset)
}

// Permits reports whether a resolved name is one the policy allows.
func (e *Enforcer) Permits(name string) bool {
	name = strings.ToLower(strings.TrimSuffix(strings.TrimSpace(name), "."))
	if name == "" {
		return false
	}

	e.mu.RLock()
	defer e.mu.RUnlock()

	for allowed, apexToo := range e.allow {
		if name == allowed {
			if apexToo {
				return true
			}
			continue // a wildcard entry does not cover its own apex
		}
		if suffixMatch(name, allowed) {
			return true
		}
	}
	if e.mirrorOnly {
		for _, host := range MirrorHosts {
			if suffixMatch(name, host) {
				return true
			}
		}
	}
	return false
}

// OnDNS reacts to a captured DNS response, adding permitted answers to the
// firewall's allow sets.
//
// Returns the addresses it added, so the caller can log or count them.
func (e *Enforcer) OnDNS(ctx context.Context, ev *event.Event) ([]string, error) {
	if ev == nil || ev.Type != event.TypeDNS || ev.Net == nil {
		return nil, nil
	}
	if !e.Permits(ev.Net.QName) {
		return nil, nil
	}

	var added []string
	for _, answer := range ev.Net.Answers {
		addr, err := netip.ParseAddr(answer)
		if err != nil {
			// A malformed answer never reaches the command line.
			continue
		}

		// Refreshed *before* the kernel entry expires, not after.
		//
		// The nftables element ages out at AllowTTL. Waiting the same AllowTTL
		// before re-adding means a resolution that lands just inside the
		// window is skipped, the element then expires, and the application —
		// still holding a valid cached DNS answer, so it will not look up
		// again — is blocked until its own cache turns over. Re-adding once
		// the entry is into its final quarter keeps the element alive for as
		// long as the domain is genuinely in use, and costs one nft call per
		// address per refresh window.
		e.mu.RLock()
		at, dup := e.seen[answer]
		e.mu.RUnlock()
		if dup && now().Sub(at) < AllowRefreshAfter {
			continue
		}

		set := SetV4
		if addr.Is6() && !addr.Is4In6() {
			set = SetV6
		}
		// Marked seen only *after* nft accepts it. Recording it first would
		// mean a transient failure permanently skipped the address, leaving an
		// allowlisted domain blocked until the whole policy is reloaded.
		if err := e.applier.AddElement(ctx, set, addr.String()); err != nil {
			return added, fmt.Errorf("allow %s (%s): %w", ev.Net.QName, answer, err)
		}

		e.mu.Lock()
		e.seen[answer] = now()
		e.added++
		e.mu.Unlock()
		added = append(added, answer)
	}
	return added, nil
}

// Added reports how many addresses have been inserted into the allow sets.
func (e *Enforcer) Added() int {
	e.mu.RLock()
	defer e.mu.RUnlock()
	return e.added
}

// Violation builds a `policy` event for a connection the posture refused.
//
// This is what turns an nftables drop into something the timeline, the
// behaviour summary, and the metrics all understand.
func Violation(boxID, mode, target, reason string, base *event.Event) *event.Event {
	ev := &event.Event{
		BoxID: boxID,
		Type:  event.TypePolicy,
		Policy: &event.Policy{
			Verdict: "block",
			Mode:    mode,
			Target:  target,
			Reason:  reason,
		},
	}
	if base != nil {
		ev.TSWall = base.TSWall
		ev.TSMonoNS = base.TSMonoNS
		ev.CgroupID = base.CgroupID
		ev.PID, ev.TID, ev.PPID = base.PID, base.TID, base.PPID
		ev.Comm, ev.UID = base.Comm, base.UID
	}
	return ev
}

// suffixMatch reports whether name equals suffix or ends with ".suffix".
//
// Label-boundary matching, mirroring `policy::suffix_match` in Rust:
// `evilgithub.com` must not match `github.com`.
func suffixMatch(name, suffix string) bool {
	if name == suffix {
		return true
	}
	cut := len(name) - len(suffix)
	if cut <= 0 {
		return false
	}
	return name[cut-1] == '.' && name[cut:] == suffix
}

// MirrorHosts is the `mirror-only` curated list, mirroring
// `src/policy/mirrors.rs`. Kept in sync by a test that asserts the two agree.
var MirrorHosts = []string{
	"archive.ubuntu.com",
	"auth.docker.io",
	"bitbucket.org",
	"cache.nixos.org",
	"channels.nixos.org",
	"codeload.github.com",
	"crates.io",
	"deb.debian.org",
	"dl-cdn.alpinelinux.org",
	"files.pythonhosted.org",
	"ghcr.io",
	"github.com",
	"githubusercontent.com",
	"gitlab.com",
	"go.dev",
	"golang.org",
	"index.crates.io",
	"index.rubygems.org",
	"nixos.org",
	"npmjs.com",
	"plugins.gradle.org",
	"production.cloudflare.docker.com",
	"proxy.golang.org",
	"pypi.org",
	"pythonhosted.org",
	"quay.io",
	"registry-1.docker.io",
	"registry.npmjs.org",
	"registry.yarnpkg.com",
	"repo.maven.apache.org",
	"repo1.maven.org",
	"rubygems.org",
	"rust-lang.org",
	"security.debian.org",
	"security.ubuntu.com",
	"sr.ht",
	"static.crates.io",
	"sum.golang.org",
}
