package capture

import (
	"context"
	"errors"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

const (
	packetRetryMin = time.Second
	packetRetryMax = 30 * time.Second
)

// RetryingPacket keeps the payload tap self-healing.
//
// AF_PACKET and resolver discovery can fail transiently during early boot.
// Exiting would make systemd reload the nftables table and dropping the source
// forever would leave a domain allowlist with nobody able to populate it. This
// wrapper does neither: process/eBPF capture keeps running while it retries the
// tap in-process, and Domains reports only the child that is currently live.
type RetryingPacket struct {
	boxID string
	boot  time.Time

	mu      sync.RWMutex
	current Source

	newPacket func(string, time.Time) (Source, error)
	retryMin  time.Duration
	retryMax  time.Duration
}

// NewRetryingPacket performs the first preflight synchronously so the initial
// handshake and status file describe reality, then retries any failure in Run.
func NewRetryingPacket(boxID string, boot time.Time) *RetryingPacket {
	r := &RetryingPacket{
		boxID: boxID,
		boot:  boot,
		newPacket: func(boxID string, boot time.Time) (Source, error) {
			return NewPacket(boxID, boot)
		},
		retryMin: packetRetryMin,
		retryMax: packetRetryMax,
	}
	if source, err := r.newPacket(boxID, boot); err != nil {
		_, _ = fmt.Fprintf(os.Stderr, "devbox-obsd: packet capture unavailable (%v); retrying in-process\n", err)
	} else {
		r.current = source
	}
	return r
}

// Name reports the live child source, or that packet capture is retrying.
func (r *RetryingPacket) Name() string {
	r.mu.RLock()
	defer r.mu.RUnlock()
	if r.current == nil {
		return "packet-retrying"
	}
	return r.current.Name()
}

// Domains reports only feeds a currently live child can produce.
func (r *RetryingPacket) Domains() []event.Type {
	r.mu.RLock()
	defer r.mu.RUnlock()
	if r.current == nil {
		return nil
	}
	return r.current.Domains()
}

func (r *RetryingPacket) set(source Source) {
	r.mu.Lock()
	r.current = source
	r.mu.Unlock()
}

func (r *RetryingPacket) get() Source {
	r.mu.RLock()
	defer r.mu.RUnlock()
	return r.current
}

// Close releases the currently active tap, unblocking its Run method.
func (r *RetryingPacket) Close() error {
	return Close(r.get())
}

// Run keeps recreating a failed packet source until the context is cancelled.
func (r *RetryingPacket) Run(ctx context.Context, out chan<- *event.Event) error {
	wait := r.retryMin
	lastFailure := ""

	for {
		source := r.get()
		if source == nil {
			var err error
			source, err = r.newPacket(r.boxID, r.boot)
			if err != nil {
				if message := err.Error(); message != lastFailure {
					_, _ = fmt.Fprintf(os.Stderr, "devbox-obsd: packet capture still unavailable (%v); retrying\n", err)
					lastFailure = message
				}
				if !waitForRetry(ctx, wait) {
					return nil
				}
				wait = min(wait*2, r.retryMax)
				continue
			}
			r.set(source)
			wait = r.retryMin
			_, _ = fmt.Fprintln(os.Stderr, "devbox-obsd: packet capture recovered; DNS/TLS capture is active")
		}

		err := source.Run(ctx, out)
		_ = Close(source)
		r.set(nil)
		if ctx.Err() != nil || isCancellation(err) {
			return nil
		}
		if err == nil {
			err = errors.New("packet capture ended unexpectedly")
		}
		_, _ = fmt.Fprintf(os.Stderr, "devbox-obsd: packet capture stopped (%v); retrying in-process\n", err)
		lastFailure = err.Error()
		if !waitForRetry(ctx, wait) {
			return nil
		}
		wait = min(wait*2, r.retryMax)
	}
}

func waitForRetry(ctx context.Context, delay time.Duration) bool {
	timer := time.NewTimer(delay)
	defer timer.Stop()
	select {
	case <-timer.C:
		return true
	case <-ctx.Done():
		return false
	}
}

func isCancellation(err error) bool {
	return errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded)
}
