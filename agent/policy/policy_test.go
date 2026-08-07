package policy

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// fakeApplier records what would have been run.
type fakeApplier struct {
	mu       sync.Mutex
	rulesets []string
	elements [][2]string // (set, addr)
	failOn   string
}

func (f *fakeApplier) Apply(_ context.Context, ruleset string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.rulesets = append(f.rulesets, ruleset)
	return nil
}

func (f *fakeApplier) AddElement(_ context.Context, set, addr string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.failOn != "" && addr == f.failOn {
		return errors.New("nft refused")
	}
	f.elements = append(f.elements, [2]string{set, addr})
	return nil
}

func dnsEvent(name string, answers ...string) *event.Event {
	return &event.Event{
		TSWall: "2026-08-06T22:00:00.000Z", BoxID: "myapp", PID: 812,
		Type: event.TypeDNS,
		Net:  &event.Net{QName: name, QType: "A", Answers: answers},
	}
}

func TestPermitsMatchesOnLabelBoundaries(t *testing.T) {
	t.Parallel()

	e := New(&fakeApplier{}, []string{"github.com", "*.githubusercontent.com"}, false)

	for _, name := range []string{
		// A bare entry covers the apex and its subdomains.
		"github.com", "codeload.github.com", "GitHub.com", "github.com.",
		// A wildcard entry covers subdomains.
		"raw.githubusercontent.com",
	} {
		if !e.Permits(name) {
			t.Errorf("%q should be permitted", name)
		}
	}
	for _, name := range []string{
		// ...but not its own apex: `*.githubusercontent.com` says subdomains,
		// which is what the console promises, and permitting the apex too is
		// a wider grant than the user wrote.
		"githubusercontent.com",
		"evilgithub.com", "github.com.evil.example", "example.com", "", "   ",
	} {
		if e.Permits(name) {
			t.Errorf("%q must not be permitted", name)
		}
	}
}

func TestMirrorOnlyAddsTheCuratedHosts(t *testing.T) {
	t.Parallel()

	strict := New(&fakeApplier{}, nil, false)
	if strict.Permits("pypi.org") {
		t.Error("pypi.org should not be permitted without mirror-only")
	}

	mirror := New(&fakeApplier{}, nil, true)
	for _, host := range []string{"pypi.org", "files.pythonhosted.org", "crates.io"} {
		if !mirror.Permits(host) {
			t.Errorf("%q should be permitted under mirror-only", host)
		}
	}
	if mirror.Permits("telemetry.example.com") {
		t.Error("mirror-only must not permit arbitrary hosts")
	}
}

func TestOnDNSAddsPermittedAnswers(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{}
	e := New(applier, []string{"pypi.org"}, false)

	added, err := e.OnDNS(context.Background(),
		dnsEvent("pypi.org", "151.101.0.223", "2606:4700::1"))
	if err != nil {
		t.Fatalf("OnDNS: %v", err)
	}
	if len(added) != 2 {
		t.Fatalf("added = %v, want both answers", added)
	}
	if len(applier.elements) != 2 {
		t.Fatalf("elements = %v", applier.elements)
	}
	if applier.elements[0][0] != SetV4 || applier.elements[1][0] != SetV6 {
		t.Errorf("answers went to the wrong sets: %v", applier.elements)
	}
	if e.Added() != 2 {
		t.Errorf("Added() = %d, want 2", e.Added())
	}
}

func TestOnDNSIgnoresLookupsOutsideThePolicy(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{}
	e := New(applier, []string{"pypi.org"}, false)

	added, err := e.OnDNS(context.Background(), dnsEvent("telemetry.example", "1.2.3.4"))
	if err != nil {
		t.Fatalf("OnDNS: %v", err)
	}
	if len(added) != 0 || len(applier.elements) != 0 {
		t.Errorf("a disallowed name must not open the firewall: %v", applier.elements)
	}
}

func TestOnDNSDeduplicates(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{}
	e := New(applier, []string{"pypi.org"}, false)

	for i := 0; i < 3; i++ {
		if _, err := e.OnDNS(context.Background(), dnsEvent("pypi.org", "151.101.0.223")); err != nil {
			t.Fatal(err)
		}
	}
	if len(applier.elements) != 1 {
		t.Errorf("the same address should be added once, got %v", applier.elements)
	}
}

func TestOnDNSRejectsMalformedAnswers(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{}
	e := New(applier, []string{"pypi.org"}, false)

	// These would otherwise become arguments to a command running as root.
	added, err := e.OnDNS(context.Background(), dnsEvent("pypi.org",
		"1.2.3.4; nft flush ruleset",
		"$(reboot)",
		"}; add rule inet devbox output accept; #",
		"151.101.0.223",
	))
	if err != nil {
		t.Fatalf("OnDNS: %v", err)
	}
	if len(added) != 1 || added[0] != "151.101.0.223" {
		t.Errorf("only the real address should be added, got %v", added)
	}
}

func TestOnDNSIgnoresNonDNSEvents(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{}
	e := New(applier, []string{"pypi.org"}, false)

	for _, ev := range []*event.Event{
		nil,
		{Type: event.TypeConnect, Net: &event.Net{QName: "pypi.org"}},
		{Type: event.TypeDNS}, // no net sub-object
	} {
		if _, err := e.OnDNS(context.Background(), ev); err != nil {
			t.Errorf("unexpected error: %v", err)
		}
	}
	if len(applier.elements) != 0 {
		t.Errorf("nothing should have been added: %v", applier.elements)
	}
}

func TestOnDNSSurfacesAnApplierFailure(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{failOn: "151.101.0.223"}
	e := New(applier, []string{"pypi.org"}, false)

	_, err := e.OnDNS(context.Background(), dnsEvent("pypi.org", "151.101.0.223"))
	if err == nil {
		t.Fatal("a failed insertion must be reported, not swallowed")
	}
	if !strings.Contains(err.Error(), "pypi.org") {
		t.Errorf("the error should name the domain: %v", err)
	}
}

func TestLoadForgetsPreviouslyAddedAddresses(t *testing.T) {
	t.Parallel()

	applier := &fakeApplier{}
	e := New(applier, []string{"pypi.org"}, false)

	if _, err := e.OnDNS(context.Background(), dnsEvent("pypi.org", "151.101.0.223")); err != nil {
		t.Fatal(err)
	}
	// A reload destroys and rebuilds the table, so the set is empty again.
	if err := e.Load(context.Background(), "table inet devbox {}"); err != nil {
		t.Fatal(err)
	}
	if _, err := e.OnDNS(context.Background(), dnsEvent("pypi.org", "151.101.0.223")); err != nil {
		t.Fatal(err)
	}

	if len(applier.elements) != 2 {
		t.Errorf("the address must be re-added after a reload, got %v", applier.elements)
	}
	if len(applier.rulesets) != 1 {
		t.Errorf("rulesets = %d, want 1", len(applier.rulesets))
	}
}

func TestViolationCarriesTheProcessIdentity(t *testing.T) {
	t.Parallel()

	base := &event.Event{
		TSWall: "2026-08-06T22:00:00.000Z", TSMonoNS: 42,
		PID: 812, TID: 812, PPID: 640, Comm: "pip", UID: 1000, CgroupID: 7,
	}
	ev := Violation("myapp", "allowlist", "telemetry.example", "not in the allowlist", base)

	if err := ev.Validate(); err != nil {
		t.Fatalf("a violation must be a valid event: %v", err)
	}
	if ev.Policy.Verdict != "block" || ev.Policy.Mode != "allowlist" {
		t.Errorf("policy = %+v", ev.Policy)
	}
	// Without the pid, a violation cannot be attributed to a process, which
	// is the only thing that makes it actionable.
	if ev.PID != 812 || ev.Comm != "pip" || ev.CgroupID != 7 {
		t.Errorf("identity was lost: %+v", ev)
	}
}

func TestViolationWithoutABaseEventStillValidates(t *testing.T) {
	t.Parallel()

	ev := Violation("myapp", "isolated", "example.com", "no egress", nil)
	ev.TSWall = "2026-08-06T22:00:00.000Z"
	ev.PID = 1
	if err := ev.Validate(); err != nil {
		t.Errorf("Validate: %v", err)
	}
}

// The curated list exists in both languages. They must agree, or `mirror-only`
// means one thing to the policy engine and another to the firewall.
func TestMirrorHostsMatchTheRustList(t *testing.T) {
	t.Parallel()

	source, err := os.ReadFile(filepath.Join("..", "..", "src", "policy", "mirrors.rs"))
	if err != nil {
		t.Skipf("cannot read the Rust list: %v", err)
	}

	// Hosts are the quoted strings inside the `hosts:` arrays.
	blocks := regexp.MustCompile(`(?s)hosts:\s*&\[(.*?)\]`).FindAllSubmatch(source, -1)
	if len(blocks) == 0 {
		t.Fatal("found no host lists in mirrors.rs; the parser needs updating")
	}

	quoted := regexp.MustCompile(`"([^"]+)"`)
	seen := map[string]bool{}
	var rust []string
	for _, block := range blocks {
		for _, m := range quoted.FindAllSubmatch(block[1], -1) {
			host := string(m[1])
			if !seen[host] {
				seen[host] = true
				rust = append(rust, host)
			}
		}
	}
	sort.Strings(rust)

	goList := append([]string(nil), MirrorHosts...)
	sort.Strings(goList)

	if len(rust) != len(goList) {
		t.Fatalf("mirror lists differ in length: Rust has %d, Go has %d\nRust: %v\nGo:   %v",
			len(rust), len(goList), rust, goList)
	}
	for i := range rust {
		if rust[i] != goList[i] {
			t.Errorf("mirror lists differ at %d: Rust %q, Go %q", i, rust[i], goList[i])
		}
	}
}

// TestAllowTTLMatchesTheRuleset pins the two halves of an expiry together.
//
// The kernel expires an allow-set element on the timeout the *Rust* ruleset
// writes; this agent decides when it is willing to add the address again. If
// the agent's window were longer, a domain in continuous use would be blocked
// for the gap between them — a default-deny posture failing closed on traffic
// it had already allowed, and only after an hour of running, which is the
// worst possible time to discover it.
//
// Parsed from the source rather than duplicated in a shared config, for the
// same reason as the mirror list (ADR-0022): the constant has exactly one
// home, and this fails loudly if it moves.
func TestAllowTTLMatchesTheRuleset(t *testing.T) {
	source, err := os.ReadFile(filepath.Join("..", "..", "src", "policy", "nftables.rs"))
	if err != nil {
		t.Fatalf("read nftables.rs: %v", err)
	}

	match := regexp.MustCompile(`ALLOW_TTL_SECS: u64 = (\d+)`).FindSubmatch(source)
	if match == nil {
		t.Fatal("could not find ALLOW_TTL_SECS in nftables.rs; the parser needs updating")
	}
	secs, err := strconv.Atoi(string(match[1]))
	if err != nil {
		t.Fatalf("ALLOW_TTL_SECS is not a number: %v", err)
	}

	if want := time.Duration(secs) * time.Second; AllowTTL != want {
		t.Fatalf("AllowTTL is %v but the ruleset expires elements after %v; "+
			"an agent window longer than the kernel's blocks allowlisted traffic",
			AllowTTL, want)
	}
}

// TestNFTArgumentsAreTokenized pins the shape of the nft invocation.
//
// `exec.Command` does not shell-split, so the whole rule as one string was a
// single argv entry nft could not parse. Every insertion failed and the
// default-deny allow set stayed empty for the life of the box — the enforcer
// never added an address, and nothing noticed because a blocked domain looks
// exactly like a network problem.
func TestNFTArgumentsAreTokenized(t *testing.T) {
	t.Parallel()

	args := elementArgs("add", SetV4, "192.0.2.1")
	for _, arg := range args {
		if strings.ContainsAny(arg, " \t") {
			t.Errorf("argument %q contains whitespace; nft receives it as one token", arg)
		}
	}

	want := []string{"add", "element", "inet", Table, SetV4, "{", "192.0.2.1", "}"}
	if len(args) != len(want) {
		t.Fatalf("got %v, want %v", args, want)
	}
	for i := range want {
		if args[i] != want[i] {
			t.Fatalf("got %v, want %v", args, want)
		}
	}
}
