package statemachine

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func registry(t *testing.T) (*Registry, func(time.Duration)) {
	t.Helper()

	now := time.Date(2026, 8, 6, 22, 0, 0, 0, time.UTC)
	var mu sync.Mutex
	r := NewRegistry().WithClock(func() time.Time {
		mu.Lock()
		defer mu.Unlock()
		return now
	})
	advance := func(d time.Duration) {
		mu.Lock()
		defer mu.Unlock()
		now = now.Add(d)
	}
	return r, advance
}

// provision walks a node all the way through, returning the final record.
func provision(t *testing.T, r *Registry, serial, name string) Node {
	t.Helper()

	if _, _, err := r.Discover(serial); err != nil {
		t.Fatalf("Discover: %v", err)
	}
	if _, err := r.Identify(serial, name, "leaf"); err != nil {
		t.Fatalf("Identify: %v", err)
	}
	for _, state := range []State{Rendering, Pushing, Verifying, Healthy} {
		if _, err := r.Advance(serial, state, ""); err != nil {
			t.Fatalf("Advance to %s: %v", state, err)
		}
	}
	node, _ := r.Get(serial)
	return node
}

func TestTheHappyPathReachesHealthy(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	node := provision(t, r, "SN-001", "leaf1")

	if node.State != Healthy {
		t.Errorf("state = %s, want healthy", node.State)
	}
	if node.Name != "leaf1" || node.Role != "leaf" {
		t.Errorf("identity was lost: %+v", node)
	}
	if node.Attempts != 1 {
		t.Errorf("attempts = %d, want 1", node.Attempts)
	}
}

func TestSlidingBackwardsIsRefused(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	provision(t, r, "SN-001", "leaf1")

	// A healthy node moving back to verifying is a server bug, and the state
	// machine refuses it rather than logging it.
	_, err := r.Advance("SN-001", Verifying, "")
	if err == nil || !strings.Contains(err.Error(), "illegal transition") {
		t.Errorf("expected an illegal-transition error, got %v", err)
	}
}

func TestSkippingForwardIsAllowed(t *testing.T) {
	t.Parallel()

	// A node whose config is already correct can go straight to verifying
	// without a push; forbidding that would make idempotent re-provisioning
	// impossible.
	r, _ := registry(t)
	if _, _, err := r.Discover("SN-001"); err != nil {
		t.Fatal(err)
	}
	if _, err := r.Identify("SN-001", "leaf1", "leaf"); err != nil {
		t.Fatal(err)
	}
	if _, err := r.Advance("SN-001", Verifying, "config already correct"); err != nil {
		t.Errorf("skipping forward should be allowed: %v", err)
	}
}

func TestARestartIsLegalFromEveryState(t *testing.T) {
	t.Parallel()

	// §10.3: a node that fails half-configured must be able to start over.
	// That is what "the system self-heals" means.
	for _, from := range append(append([]State{}, Order...), Failed) {
		if !CanTransition(from, Discovered) {
			t.Errorf("a restart from %s should be legal", from)
		}
	}
}

func TestFailureIsReachableFromAnywhere(t *testing.T) {
	t.Parallel()

	for _, from := range Order {
		if !CanTransition(from, Failed) {
			t.Errorf("failing from %s should be legal", from)
		}
	}
}

func TestRediscoveryCountsAsARetryAndKeepsTheOriginalStart(t *testing.T) {
	t.Parallel()

	r, tick := registry(t)
	if _, restarted, err := r.Discover("SN-001"); err != nil || restarted {
		t.Fatalf("first sighting: restarted=%v err=%v", restarted, err)
	}
	tick(30 * time.Second)

	if _, err := r.Advance("SN-001", Failed, "boot server unreachable"); err != nil {
		t.Fatal(err)
	}
	tick(10 * time.Second)

	_, restarted, err := r.Discover("SN-001")
	if err != nil || !restarted {
		t.Fatalf("second sighting: restarted=%v err=%v", restarted, err)
	}

	node, _ := r.Get("SN-001")
	if node.Attempts != 2 {
		t.Errorf("attempts = %d, want 2", node.Attempts)
	}
	if node.State != Discovered {
		t.Errorf("state = %s, want discovered", node.State)
	}
	if node.Reason != "" {
		t.Errorf("a restart should clear the failure reason, got %q", node.Reason)
	}
	// The duration must span the whole ordeal, not just the last attempt —
	// that is what an SLO should be measured against.
	if node.Duration() != 40*time.Second {
		t.Errorf("duration = %v, want the full 40s", node.Duration())
	}
}

func TestAnUndiscoveredNodeCannotAdvance(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	if _, err := r.Advance("SN-GHOST", Identified, ""); err == nil {
		t.Error("advancing an unknown node should fail")
	}
	if _, err := r.Identify("SN-GHOST", "leaf1", "leaf"); err == nil {
		t.Error("identifying an unknown node should fail")
	}
}

func TestASerialIsRequired(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	if _, _, err := r.Discover(""); err == nil {
		t.Error("a node without a serial cannot be identified, so it cannot start")
	}
}

func TestIdempotentPushSkipsIdenticalConfig(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	if _, _, err := r.Discover("SN-001"); err != nil {
		t.Fatal(err)
	}

	if !r.NeedsPush("SN-001", "hash-abc") {
		t.Error("a node with no config needs a push")
	}
	if err := r.RecordPush("SN-001", "hash-abc"); err != nil {
		t.Fatal(err)
	}
	if r.NeedsPush("SN-001", "hash-abc") {
		t.Error("re-pushing identical config is what makes the cycle non-idempotent")
	}
	if !r.NeedsPush("SN-001", "hash-def") {
		t.Error("changed config must be pushed")
	}
	// An unknown node always needs one; refusing to push would strand it.
	if !r.NeedsPush("SN-GHOST", "hash-abc") {
		t.Error("an unknown node needs a push")
	}
}

func TestSummarizeCountsTheFabric(t *testing.T) {
	t.Parallel()

	r, tick := registry(t)
	provision(t, r, "SN-001", "leaf1")
	tick(20 * time.Second)
	provision(t, r, "SN-002", "leaf2")

	if _, _, err := r.Discover("SN-003"); err != nil {
		t.Fatal(err)
	}
	if _, err := r.Advance("SN-003", Failed, "no DHCP offer"); err != nil {
		t.Fatal(err)
	}

	s := r.Summarize()
	if s.Total != 3 || s.Healthy != 2 || s.Failed != 1 {
		t.Errorf("summary = %+v", s)
	}
	if s.Converged {
		t.Error("a fabric with a failed node has not converged")
	}
	if s.ByState[Healthy] != 2 {
		t.Errorf("by_state = %v", s.ByState)
	}
}

func TestAnEmptyFabricHasNotConverged(t *testing.T) {
	t.Parallel()

	// "Zero of zero nodes are healthy" must not read as success: it almost
	// always means nothing ever connected.
	r, _ := registry(t)
	if r.Summarize().Converged {
		t.Error("an empty fabric must not report converged")
	}
}

func TestP95IsTheTailNotTheAverage(t *testing.T) {
	t.Parallel()

	r, tick := registry(t)
	// Eighteen fast nodes and two slow ones.
	for i := range 18 {
		serial := fmt.Sprintf("SN-%03d", i)
		if _, _, err := r.Discover(serial); err != nil {
			t.Fatal(err)
		}
		tick(10 * time.Second)
		if _, err := r.Advance(serial, Healthy, ""); err != nil {
			t.Fatal(err)
		}
		tick(-10 * time.Second)
	}
	for _, serial := range []string{"SN-SLOW-1", "SN-SLOW-2"} {
		if _, _, err := r.Discover(serial); err != nil {
			t.Fatal(err)
		}
		tick(300 * time.Second)
		if _, err := r.Advance(serial, Healthy, ""); err != nil {
			t.Fatal(err)
		}
		tick(-300 * time.Second)
	}

	p95 := r.Summarize().P95()
	if p95 != 300*time.Second {
		t.Errorf("p95 = %v, want the slow tail", p95)
	}
}

func TestP95OfNothingIsZero(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	if got := r.Summarize().P95(); got != 0 {
		t.Errorf("p95 = %v, want 0", got)
	}
}

func TestListIsOrderedSoOutputIsStable(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	for _, serial := range []string{"SN-003", "SN-001", "SN-002"} {
		if _, _, err := r.Discover(serial); err != nil {
			t.Fatal(err)
		}
	}

	list := r.List()
	if len(list) != 3 {
		t.Fatalf("got %d nodes", len(list))
	}
	for i, want := range []string{"SN-001", "SN-002", "SN-003"} {
		if list[i].Serial != want {
			t.Errorf("list[%d] = %s, want %s", i, list[i].Serial, want)
		}
	}
}

func TestGetReturnsACopy(t *testing.T) {
	t.Parallel()

	r, _ := registry(t)
	if _, _, err := r.Discover("SN-001"); err != nil {
		t.Fatal(err)
	}

	node, ok := r.Get("SN-001")
	if !ok {
		t.Fatal("SN-001 should exist")
	}
	node.State = Healthy // mutate the copy

	fresh, _ := r.Get("SN-001")
	if fresh.State != Discovered {
		t.Error("Get must return a copy; the registry was mutated from outside")
	}

	if _, ok := r.Get("SN-GHOST"); ok {
		t.Error("an unknown serial should not be found")
	}
}

func TestConcurrentProvisioningIsSafe(t *testing.T) {
	t.Parallel()

	// A fabric bringing up twenty nodes at once means twenty goroutines
	// touching this registry.
	r := NewRegistry()
	var wg sync.WaitGroup

	for i := range 20 {
		wg.Add(1)
		go func(n int) {
			defer wg.Done()
			serial := fmt.Sprintf("SN-%03d", n)
			if _, _, err := r.Discover(serial); err != nil {
				t.Errorf("Discover: %v", err)
				return
			}
			if _, err := r.Identify(serial, fmt.Sprintf("leaf%d", n), "leaf"); err != nil {
				t.Errorf("Identify: %v", err)
				return
			}
			for _, state := range []State{Rendering, Pushing, Verifying, Healthy} {
				if _, err := r.Advance(serial, state, ""); err != nil {
					t.Errorf("Advance: %v", err)
					return
				}
			}
		}(i)
	}
	wg.Wait()

	s := r.Summarize()
	if s.Total != 20 || s.Healthy != 20 || !s.Converged {
		t.Errorf("summary = %+v", s)
	}
}

func TestValidAndTerminal(t *testing.T) {
	t.Parallel()

	for _, s := range append(append([]State{}, Order...), Failed) {
		if !Valid(s) {
			t.Errorf("%s should be valid", s)
		}
	}
	if Valid("nonsense") {
		t.Error("an unknown state should not validate")
	}

	if !Terminal(Healthy) || !Terminal(Failed) {
		t.Error("healthy and failed are terminal")
	}
	if Terminal(Pushing) {
		t.Error("pushing is not terminal")
	}
}

func TestTransitionsInvolvingUnknownStatesAreRefused(t *testing.T) {
	t.Parallel()

	if CanTransition("nonsense", Healthy) || CanTransition(Discovered, "nonsense") {
		t.Error("an unknown state must not participate in a transition")
	}
}

func TestARegistrySurvivesARestart(t *testing.T) {
	t.Parallel()

	// §10.3 kills ztpd mid-provision. Losing the registry would make every
	// returning node look like a first attempt, and both the whole-ordeal SLO
	// and the `attempts > 1` assertion fiction.
	path := filepath.Join(t.TempDir(), "state", "ztpd-state.json")

	first := NewRegistry()
	if _, _, err := first.Discover("SN-001"); err != nil {
		t.Fatal(err)
	}
	if _, err := first.Advance("SN-001", Failed, "ztpd went away"); err != nil {
		t.Fatal(err)
	}
	if err := first.RecordPush("SN-001", "hash-abc"); err != nil {
		t.Fatal(err)
	}
	if err := first.Save(path); err != nil {
		t.Fatalf("Save: %v", err)
	}

	second, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}

	node, ok := second.Get("SN-001")
	if !ok {
		t.Fatal("the node should have survived")
	}
	if node.State != Failed || node.Reason != "ztpd went away" {
		t.Errorf("node = %+v", node)
	}
	if node.ConfigHash != "hash-abc" {
		t.Error("the config hash must survive, or an identical re-push is not skipped")
	}

	// And a rediscovery is correctly counted as a retry.
	if _, restarted, err := second.Discover("SN-001"); err != nil || !restarted {
		t.Errorf("restarted=%v err=%v", restarted, err)
	}
	node, _ = second.Get("SN-001")
	if node.Attempts != 2 {
		t.Errorf("attempts = %d, want 2", node.Attempts)
	}
}

func TestLoadingAMissingFileIsAnEmptyRegistryNotAnError(t *testing.T) {
	t.Parallel()

	// The first start of a fabric has nothing to restore.
	registry, err := Load(filepath.Join(t.TempDir(), "nope.json"))
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if registry.Summarize().Total != 0 {
		t.Error("a missing file should load as empty")
	}
}

func TestLoadingACorruptFileIsAnError(t *testing.T) {
	t.Parallel()

	path := filepath.Join(t.TempDir(), "state.json")
	if err := os.WriteFile(path, []byte("{not json}"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(path); err == nil {
		t.Error("a corrupt state file should be reported, not silently discarded")
	}
}
