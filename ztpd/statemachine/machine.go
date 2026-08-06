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
	"fmt"
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
		return node, false, nil
	}

	// A node that comes back is starting over. Keeping FirstSeen makes the
	// provisioning duration measure the whole ordeal rather than the last
	// attempt, which is what an SLO should be measured against.
	node.State = Discovered
	node.UpdatedAt = now
	node.Attempts++
	node.Reason = ""
	return node, true, nil
}

// Advance moves a node to a new state.
func (r *Registry) Advance(serial string, to State, reason string) (*Node, error) {
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
	return r.Advance(serial, Identified, "")
}

// RecordPush stores the hash of the config pushed to a node.
func (r *Registry) RecordPush(serial, hash string) error {
	r.mu.Lock()
	defer r.mu.Unlock()

	node, ok := r.nodes[serial]
	if !ok {
		return fmt.Errorf("node %q has not been discovered", serial)
	}
	node.ConfigHash = hash
	node.UpdatedAt = r.now()
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
