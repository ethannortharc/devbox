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
	element := fmt.Sprintf("add element inet %s %s { %s }", Table, set, addr)
	cmd := exec.CommandContext(ctx, "nft", element)
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("nft %q: %w: %s", element, err, strings.TrimSpace(string(out)))
	}
	return nil
}

// Enforcer keeps the firewall in step with DNS.
type Enforcer struct {
	applier Applier

	mu sync.RWMutex
	// allow holds the domains the policy names, lower-cased.
	allow map[string]struct{}
	// seen deduplicates addresses already added to a set.
	seen map[string]struct{}
	// mirrorOnly permits the curated package hosts in addition to `allow`.
	mirrorOnly bool
	// added counts successful set insertions, for the metrics surface.
	added int
}

// New builds an enforcer for a set of allowlisted domains.
func New(applier Applier, domains []string, mirrorOnly bool) *Enforcer {
	allow := make(map[string]struct{}, len(domains))
	for _, d := range domains {
		d = strings.ToLower(strings.TrimSpace(strings.TrimPrefix(d, "*.")))
		if d != "" {
			allow[d] = struct{}{}
		}
	}
	return &Enforcer{
		applier:    applier,
		allow:      allow,
		seen:       map[string]struct{}{},
		mirrorOnly: mirrorOnly,
	}
}

// Load applies a complete ruleset.
func (e *Enforcer) Load(ctx context.Context, ruleset string) error {
	// A fresh ruleset destroys and rebuilds the table, so every address the
	// enforcer previously added is gone. Forgetting them is what makes a
	// reload re-add them as DNS answers come in again.
	e.mu.Lock()
	e.seen = map[string]struct{}{}
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

	for allowed := range e.allow {
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

		e.mu.RLock()
		_, dup := e.seen[answer]
		e.mu.RUnlock()
		if dup {
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
		e.seen[answer] = struct{}{}
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
