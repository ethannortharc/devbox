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
	"fmt"

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
