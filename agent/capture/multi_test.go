package capture

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// endless is a source that runs until cancelled, like every real one.
type endless struct {
	name    string
	stopped chan struct{}
}

func (e *endless) Name() string          { return e.name }
func (e *endless) Domains() []event.Type { return []event.Type{event.TypeExec} }
func (e *endless) Run(ctx context.Context, _ chan<- *event.Event) error {
	<-ctx.Done()
	close(e.stopped)
	return ctx.Err()
}

// failing returns immediately with a fatal error.
type failing struct{ err error }

func (f *failing) Name() string          { return "failing" }
func (f *failing) Domains() []event.Type { return []event.Type{event.TypePolicy} }
func (f *failing) Run(context.Context, chan<- *event.Event) error {
	return f.err
}

// unsupported reports that it cannot run here.
type unsupported struct{}

func (unsupported) Name() string          { return "unsupported" }
func (unsupported) Domains() []event.Type { return []event.Type{event.TypePolicy} }
func (unsupported) Run(context.Context, chan<- *event.Event) error {
	return &ErrUnsupported{Source: "unsupported", Reason: "no kernel here"}
}

func TestMultiStopsTheSiblingWhenOneSourceFails(t *testing.T) {
	t.Parallel()

	// Waiting on the group alone deadlocked the promise this makes. A source
	// that fails queues its error and returns; the sibling keeps running,
	// because a live capture source runs until it is cancelled; and the error
	// is never delivered. Half the capture would be dead with the agent
	// reporting nothing at all — the failure hidden by the part still working.
	boom := errors.New("kmsg went away")
	sibling := &endless{name: "primary", stopped: make(chan struct{})}
	m := NewMulti(sibling, &failing{err: boom})

	done := make(chan error, 1)
	go func() { done <- m.Run(context.Background(), make(chan *event.Event, 1)) }()

	select {
	case err := <-done:
		if !errors.Is(err, boom) {
			t.Fatalf("want the first fatal error, got %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Multi.Run never returned: it is waiting on the healthy source")
	}

	select {
	case <-sibling.stopped:
	case <-time.After(5 * time.Second):
		t.Fatal("the sibling was left running after a fatal failure")
	}
}

func TestMultiKeepsRunningWhenASupplementarySourceIsUnavailable(t *testing.T) {
	t.Parallel()

	// /dev/kmsg needs privileges the primary source does not. Losing the whole
	// capture over the supplementary feed would trade a missing feed for a
	// blind box — and the kernel enforces the posture either way.
	primary := &endless{name: "primary", stopped: make(chan struct{})}
	m := NewMulti(primary, unsupported{})

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- m.Run(ctx, make(chan *event.Event, 1)) }()

	// The unsupported source has returned by now; the primary must not have.
	select {
	case err := <-done:
		t.Fatalf("capture stopped over a supplementary source: %v", err)
	case <-time.After(100 * time.Millisecond):
	}

	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Errorf("a cancelled run is not a failure: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Multi.Run did not return after cancellation")
	}
}

func TestMultiReportsWhatItRuns(t *testing.T) {
	t.Parallel()

	m := NewMulti(&endless{name: "proc"}, &failing{}, nil)
	if got := m.Name(); got != "proc+failing" {
		t.Errorf("name: got %q", got)
	}
	// The union, so the handshake tells the console everything being watched.
	domains := m.Domains()
	if len(domains) != 2 {
		t.Fatalf("want exec and policy, got %v", domains)
	}
}
