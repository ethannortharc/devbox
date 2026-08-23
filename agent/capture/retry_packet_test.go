package capture

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

type livePacketStub struct{}

func (livePacketStub) Name() string { return "packet" }
func (livePacketStub) Domains() []event.Type {
	return []event.Type{event.TypeDNS, event.TypeTLS}
}
func (livePacketStub) Run(ctx context.Context, out chan<- *event.Event) error {
	if err := Send(ctx, out, &event.Event{Type: event.TypeDNS}); err != nil {
		return err
	}
	<-ctx.Done()
	return ctx.Err()
}

func TestPacketCaptureRecoversInProcessAndUpdatesItsDomains(t *testing.T) {
	t.Parallel()

	attempts := 0
	source := &RetryingPacket{
		boxID: "b",
		boot:  time.Now(),
		newPacket: func(string, time.Time) (Source, error) {
			attempts++
			if attempts == 1 {
				return nil, errors.New("resolver not ready")
			}
			return livePacketStub{}, nil
		},
		retryMin: time.Millisecond,
		retryMax: 2 * time.Millisecond,
	}

	if got := source.Domains(); len(got) != 0 {
		t.Fatalf("unacquired tap advertised domains: %v", got)
	}
	ctx, cancel := context.WithCancel(context.Background())
	out := make(chan *event.Event, 1)
	done := make(chan error, 1)
	go func() { done <- source.Run(ctx, out) }()

	select {
	case <-out:
	case <-time.After(time.Second):
		t.Fatal("packet tap never recovered")
	}
	if got := source.Domains(); len(got) != 2 || got[0] != event.TypeDNS {
		t.Fatalf("recovered domains = %v", got)
	}
	if attempts != 2 {
		t.Fatalf("factory attempts = %d, want initial failure plus recovery", attempts)
	}

	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("cancelled retry source: %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("retrying packet source did not stop")
	}
}
