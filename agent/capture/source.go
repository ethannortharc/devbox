// Package capture produces events from whatever the host kernel allows.
//
// Three sources, in descending order of fidelity:
//
//	eBPF     kernel-level, full pid correlation — needs Linux with BTF
//	proc     /proc polling, exec and connect only — any Linux
//	fixture  replays a recorded JSONL file — any host, for tests and demos
//
// The Source interface is what keeps `cmd/obsd` from caring which one is in
// play, and what makes the degraded `--no-ebpf` path (§13) a substitution
// rather than a branch through the whole agent.
package capture

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"

	"github.com/ethannortharc/devbox/agent/event"
)

// Source produces events until its context is cancelled.
//
// Run must return promptly once ctx is done, and must close nothing the caller
// owns. Events are delivered on out; a blocked send is backpressure, which is
// the correct behaviour — the alternative is an unbounded buffer.
type Source interface {
	// Name identifies the source in the handshake and in the UI, so nobody
	// mistakes proc-polling coverage for kernel coverage.
	Name() string

	// Domains lists the event types this source can produce, which the
	// console shows as "what is actually being watched".
	Domains() []event.Type

	// Run streams events until ctx is cancelled or the source fails.
	Run(ctx context.Context, out chan<- *event.Event) error
}

// Send delivers one event, honouring cancellation.
//
// Every source funnels through this so cancellation semantics are identical
// across all of them.
func Send(ctx context.Context, out chan<- *event.Event, e *event.Event) error {
	select {
	case out <- e:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// ErrUnsupported reports that a source cannot run on this host.
type ErrUnsupported struct {
	Source string
	Reason string
}

func (e *ErrUnsupported) Error() string {
	return fmt.Sprintf("%s capture is unavailable: %s", e.Source, e.Reason)
}

// Multi runs several sources into one stream.
//
// The policy feed needs this: refusals come from the kernel ring buffer while
// execs and connections come from eBPF or /proc, and they are one timeline to
// whoever reads them. Composing at this level keeps `cmd/obsd` from growing a
// second pump and a second set of shutdown rules.
type Multi struct {
	sources []Source
}

// NewMulti composes sources, ignoring nils so a caller can pass an optional
// one without a branch.
func NewMulti(sources ...Source) *Multi {
	kept := make([]Source, 0, len(sources))
	for _, s := range sources {
		if s != nil {
			kept = append(kept, s)
		}
	}
	return &Multi{sources: kept}
}

// Name implements Source, naming every source it runs.
func (m *Multi) Name() string {
	names := make([]string, 0, len(m.sources))
	for _, s := range m.sources {
		names = append(names, s.Name())
	}
	return strings.Join(names, "+")
}

// Domains implements Source, as the union of what its sources produce.
func (m *Multi) Domains() []event.Type {
	seen := make(map[event.Type]bool)
	var all []event.Type
	for _, s := range m.sources {
		for _, d := range s.Domains() {
			if !seen[d] {
				seen[d] = true
				all = append(all, d)
			}
		}
	}
	return all
}

// Run streams every source until all finish or ctx is cancelled.
//
// A source that reports [ErrUnsupported] is skipped rather than fatal: the
// kernel ring buffer needs privileges the primary source does not, and losing
// the whole capture because the supplementary feed is unavailable would be a
// worse outcome than losing the feed. Any other error is returned, and the
// first one wins.
func (m *Multi) Run(ctx context.Context, out chan<- *event.Event) error {
	var wg sync.WaitGroup
	errs := make(chan error, len(m.sources))

	for _, s := range m.sources {
		wg.Add(1)
		go func(s Source) {
			defer wg.Done()
			if err := s.Run(ctx, out); err != nil {
				var unsupported *ErrUnsupported
				if errors.As(err, &unsupported) {
					return
				}
				errs <- err
			}
		}(s)
	}

	wg.Wait()
	close(errs)
	return <-errs
}
