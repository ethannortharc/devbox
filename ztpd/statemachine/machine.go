// Package statemachine drives a node from blank to healthy — §10.2.
//
//	discovered → identified → rendering → pushing → verifying → healthy
//	                                                          ↘ failed
//
// Two properties matter more than the states themselves, and both come from
// §10.3's chaos requirements:
//
//   - **Idempotent retry.** A node that fails half-configured must be able to
//     start over from `discovered` and reach `healthy`. That is what "the
//     system self-heals" means, and it is why a restart is a legal transition
//     from every state rather than an error.
//
//   - **No silent regression.** Anything *other* than a restart may only move
//     forward. A node sliding from `verifying` back to `pushing` is a bug in
//     the server, and the state machine refuses it rather than logging it.
package statemachine

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

// State is one step in the provisioning sequence.
type State string

// The states, in order.
const (
	Discovered State = "discovered"
	Identified State = "identified"
	Rendering  State = "rendering"
	Pushing    State = "pushing"
	Verifying  State = "verifying"
	Healthy    State = "healthy"
	Failed     State = "failed"
)

// Order is the forward sequence. `failed` is terminal and sits outside it.
var Order = []State{Discovered, Identified, Rendering, Pushing, Verifying, Healthy}

// index returns a state's position in the sequence, or -1.
func index(s State) int {
	for i, known := range Order {
		if known == s {
			return i
		}
	}
	return -1
}

// Valid reports whether s is a known state.
func Valid(s State) bool {
	return s == Failed || index(s) >= 0
}

// Terminal reports whether provisioning has finished, one way or the other.
func Terminal(s State) bool {
	return s == Healthy || s == Failed
}

// CanTransition reports whether `from → to` is legal.
//
// A restart (to `discovered`) is legal from anywhere: that is what idempotent
// recovery looks like under chaos. `failed` is reachable from anywhere.
// Everything else must move strictly forward.
func CanTransition(from, to State) bool {
	if !Valid(from) || !Valid(to) {
		return false
	}
	if to == Discovered || to == Failed {
		return true
	}
	if from == Healthy {
		// A healthy node only changes by restarting, which the case above
		// already allows.
		return false
	}
	return index(to) > index(from)
}

// Node is one device's provisioning record.
type Node struct {
	Serial string `json:"serial"`
	// Name is empty until the node is identified against the source of truth.
	Name  string `json:"name,omitempty"`
	Role  string `json:"role,omitempty"`
	State State  `json:"state"`

	FirstSeen time.Time `json:"first_seen"`
	UpdatedAt time.Time `json:"updated_at"`
	// Attempts counts how many times provisioning started for this serial.
	// Under chaos this is expected to exceed 1 — that is recovery working.
	Attempts int    `json:"attempts"`
	Reason   string `json:"reason,omitempty"`
	// ConfigHash identifies the config last pushed, so a re-push of identical
	// config can be skipped (which is what makes the whole cycle idempotent).
	ConfigHash string `json:"config_hash,omitempty"`
}

// Duration from first sighting to now (or to the terminal state).
func (n *Node) Duration() time.Duration {
	return n.UpdatedAt.Sub(n.FirstSeen)
}

// Registry tracks every node the server has seen.
//
// Safe for concurrent use: a fabric bringing up twenty nodes at once means
// twenty goroutines touching this.
type Registry struct {
	mu    sync.RWMutex
	nodes map[string]*Node
	// now is injectable so tests do not have to sleep.
	now func() time.Time
	// persist, when set, is called after every mutation, before the caller is
	// told the mutation happened. See Persisting.
	persist func()
}

// Persisting makes every mutation durable before it is acknowledged.
//
// §10.3 kills `ztpd` at an arbitrary moment. With a periodic save, a node that
// identified and reported `pushing` in the second before the kill came back to
// a registry that had never heard of it — `Attempts` reset to 1, and both the
// retry assertion and the whole-ordeal SLO became fiction precisely in the
// scenario they exist to measure.
//
// The cost is a small synchronous write per state transition. At fabric scale
// (tens of nodes, a handful of transitions each) that is nothing, and it buys
// the guarantee the chaos test is actually asserting.
func (r *Registry) Persisting(path string, onError func(error)) *Registry {
	r.persist = func() {
		if err := r.Save(path); err != nil && onError != nil {
			onError(err)
		}
	}
	return r
}

// saved runs the persist hook, if one is installed.
//
// Called after the mutating lock is released — Save takes the read lock, and
// taking it while the write lock is held would deadlock.
func (r *Registry) saved() {
	if r.persist != nil {
		r.persist()
	}
}

// Snapshot is the registry's persistent form.
//
// §10.3 kills `ztpd` mid-provision and expects the fabric to self-heal. An
// in-memory-only registry loses first-seen times, attempt counts, and config
// hashes on that restart — so returning nodes look like first attempts and
// both the whole-ordeal SLO and the `attempts > 1` assertion become fiction.
type Snapshot struct {
	Nodes []Node `json:"nodes"`
}

// Save writes the registry to a file, atomically.
func (r *Registry) Save(path string) error {
	r.mu.RLock()
	snapshot := Snapshot{Nodes: make([]Node, 0, len(r.nodes))}
	for _, node := range r.nodes {
		snapshot.Nodes = append(snapshot.Nodes, *node)
	}
	r.mu.RUnlock()

	sort.Slice(snapshot.Nodes, func(i, j int) bool {
		return snapshot.Nodes[i].Serial < snapshot.Nodes[j].Serial
	})

	data, err := json.Marshal(snapshot)
	if err != nil {
		return fmt.Errorf("encode the registry: %w", err)
	}
	if dir := filepath.Dir(path); dir != "" {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return fmt.Errorf("create %s: %w", dir, err)
		}
	}

	// Write-then-rename: a crash mid-write must not leave a truncated file
	// that the next start refuses to load.
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return fmt.Errorf("write %s: %w", tmp, err)
	}
	return os.Rename(tmp, path)
}

// Load restores a registry from a file.
//
// A missing file is an empty registry, not an error: the first start of a
// fabric has nothing to restore.
func Load(path string) (*Registry, error) {
	registry := NewRegistry()

	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return registry, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}

	var snapshot Snapshot
	if err := json.Unmarshal(data, &snapshot); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	for i := range snapshot.Nodes {
		node := snapshot.Nodes[i]
		registry.nodes[node.Serial] = &node
	}
	return registry, nil
}

// NewRegistry builds an empty registry.
func NewRegistry() *Registry {
	return &Registry{
		nodes: map[string]*Node{},
		now:   time.Now,
	}
}

// WithClock replaces the time source, for tests.
func (r *Registry) WithClock(now func() time.Time) *Registry {
	r.now = now
	return r
}

// Discover records a node presenting itself, starting or restarting it.
//
// Returns the node and whether this was a restart.
func (r *Registry) Discover(serial string) (*Node, bool, error) {
	if serial == "" {
		return nil, false, fmt.Errorf("a node must present a serial")
	}

	// LIFO: the unlock runs first, then the save — Save takes the read lock,
	// and taking it under the write lock would deadlock.
	mutated := false
	defer func() {
		if mutated {
			r.saved()
		}
	}()
	r.mu.Lock()
	defer r.mu.Unlock()

	now := r.now()
	node, seen := r.nodes[serial]
	if !seen {
		node = &Node{
			Serial:    serial,
			State:     Discovered,
			FirstSeen: now,
			UpdatedAt: now,
			Attempts:  1,
		}
		r.nodes[serial] = node
		mutated = true
		return node, false, nil
	}

	// A node that comes back is starting over. Keeping FirstSeen makes the
	// provisioning duration measure the whole ordeal rather than the last
	// attempt, which is what an SLO should be measured against.
	node.State = Discovered
	node.UpdatedAt = now
	node.Attempts++
	node.Reason = ""
	mutated = true
	return node, true, nil
}

// Advance moves a node to a new state.
func (r *Registry) Advance(serial string, to State, reason string) (*Node, error) {
	mutated := false
	defer func() {
		if mutated {
			r.saved()
		}
	}()
	r.mu.Lock()
	defer r.mu.Unlock()

	node, ok := r.nodes[serial]
	if !ok {
		return nil, fmt.Errorf("node %q has not been discovered", serial)
	}
	if !CanTransition(node.State, to) {
		return nil, fmt.Errorf(
			"illegal transition for %q: %s → %s", serial, node.State, to)
	}

	node.State = to
	node.UpdatedAt = r.now()
	node.Reason = reason
	mutated = true
	return node, nil
}

// Identify attaches a node's identity from the source of truth.
func (r *Registry) Identify(serial, name, role string) (*Node, error) {
	if name == "" {
		return nil, fmt.Errorf("cannot identify %q as an empty name", serial)
	}

	r.mu.Lock()
	node, ok := r.nodes[serial]
	if ok {
		node.Name = name
		node.Role = role
	}
	r.mu.Unlock()

	if !ok {
		return nil, fmt.Errorf("node %q has not been discovered", serial)
	}
	// Advance persists, which covers the name and role written just above.
	return r.Advance(serial, Identified, "")
}

// RecordPush stores the hash of the config pushed to a node.
func (r *Registry) RecordPush(serial, hash string) error {
	mutated := false
	defer func() {
		if mutated {
			r.saved()
		}
	}()
	r.mu.Lock()
	defer r.mu.Unlock()

	node, ok := r.nodes[serial]
	if !ok {
		return fmt.Errorf("node %q has not been discovered", serial)
	}
	node.ConfigHash = hash
	node.UpdatedAt = r.now()
	mutated = true
	return nil
}

// NeedsPush reports whether a node's config differs from what it already has.
//
// This is what makes re-provisioning idempotent: a node that already holds the
// right config is verified, not rewritten.
func (r *Registry) NeedsPush(serial, hash string) bool {
	r.mu.RLock()
	defer r.mu.RUnlock()

	node, ok := r.nodes[serial]
	if !ok {
		return true
	}
	return node.ConfigHash != hash
}

// Get returns a copy of one node's record.
func (r *Registry) Get(serial string) (Node, bool) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	node, ok := r.nodes[serial]
	if !ok {
		return Node{}, false
	}
	return *node, true
}

// List returns every node, ordered by serial so output is stable.
func (r *Registry) List() []Node {
	r.mu.RLock()
	defer r.mu.RUnlock()

	out := make([]Node, 0, len(r.nodes))
	for _, node := range r.nodes {
		out = append(out, *node)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Serial < out[j].Serial })
	return out
}

// Summary counts nodes by state.
type Summary struct {
	Total   int           `json:"total"`
	ByState map[State]int `json:"by_state"`
	Healthy int           `json:"healthy"`
	Failed  int           `json:"failed"`
	// Converged is true when every node has reached healthy — the
	// `fabric_converged == 100%` SLO from §10.3.
	Converged bool            `json:"converged"`
	Durations []time.Duration `json:"-"`
}

// Summarize counts the fabric's provisioning state.
func (r *Registry) Summarize() Summary {
	r.mu.RLock()
	defer r.mu.RUnlock()

	summary := Summary{ByState: map[State]int{}}
	for _, node := range r.nodes {
		summary.Total++
		summary.ByState[node.State]++
		switch node.State {
		case Healthy:
			summary.Healthy++
		case Failed:
			summary.Failed++
		}
		if Terminal(node.State) {
			summary.Durations = append(summary.Durations, node.Duration())
		}
	}
	// An empty fabric has not converged: "zero of zero are healthy" must not
	// read as success, because it almost always means nothing ever connected.
	summary.Converged = summary.Total > 0 && summary.Healthy == summary.Total
	return summary
}

// P95 returns the 95th-percentile provisioning duration.
//
// p95 rather than a mean because the SLO that matters is "almost every node
// provisions quickly", and a mean hides one node taking five minutes.
func (s Summary) P95() time.Duration {
	if len(s.Durations) == 0 {
		return 0
	}
	sorted := append([]time.Duration(nil), s.Durations...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i] < sorted[j] })

	// Nearest rank.
	rank := (len(sorted)*95 + 99) / 100
	if rank < 1 {
		rank = 1
	}
	return sorted[rank-1]
}
