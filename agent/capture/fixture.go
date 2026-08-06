package capture

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// Fixture replays a recorded JSONL event file.
//
// This is not a toy: it is how the cross-language integration test drives a
// real agent process against a real collector deterministically, and how the
// console can be demonstrated without provisioning anything. The events are
// real captures in the canonical schema — only their origin is synthetic.
type Fixture struct {
	// Path to a JSONL file, one event per line.
	Path string
	// BoxID overrides the box_id on every event, so one fixture can stand in
	// for any box.
	BoxID string
	// Interval paces replay. Zero replays as fast as the consumer accepts,
	// which is what tests want.
	Interval time.Duration
	// Loop restarts at the end instead of finishing.
	Loop bool
}

// Name implements Source.
func (f *Fixture) Name() string { return "fixture" }

// Domains implements Source.
func (f *Fixture) Domains() []event.Type { return event.AllTypes }

// Run implements Source.
func (f *Fixture) Run(ctx context.Context, out chan<- *event.Event) error {
	for {
		if err := f.replayOnce(ctx, out); err != nil {
			return err
		}
		if !f.Loop {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
	}
}

func (f *Fixture) replayOnce(ctx context.Context, out chan<- *event.Event) error {
	file, err := os.Open(f.Path)
	if err != nil {
		return fmt.Errorf("open fixture %s: %w", f.Path, err)
	}
	defer file.Close()

	scanner := bufio.NewScanner(file)
	// Event lines can carry a long argv; the default 64 KiB token limit is
	// tight enough to bite on a real exec.
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)

	line := 0
	for scanner.Scan() {
		line++
		text := strings.TrimSpace(scanner.Text())
		if text == "" || strings.HasPrefix(text, "#") {
			continue
		}

		e, err := event.Decode([]byte(text))
		if err != nil {
			return fmt.Errorf("%s line %d: %w", f.Path, line, err)
		}
		if f.BoxID != "" {
			e.BoxID = f.BoxID
		}
		if err := e.Validate(); err != nil {
			return fmt.Errorf("%s line %d: %w", f.Path, line, err)
		}

		if err := Send(ctx, out, e); err != nil {
			return err
		}

		if f.Interval > 0 {
			select {
			case <-time.After(f.Interval):
			case <-ctx.Done():
				return ctx.Err()
			}
		}
	}
	if err := scanner.Err(); err != nil {
		return fmt.Errorf("read fixture %s: %w", f.Path, err)
	}
	return nil
}
