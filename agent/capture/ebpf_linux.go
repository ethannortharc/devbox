//go:build linux && ebpf

package capture

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/cilium/ebpf/ringbuf"
	devboxbpf "github.com/ethannortharc/devbox/agent/bpf"
	"github.com/ethannortharc/devbox/agent/decode"
	"github.com/ethannortharc/devbox/agent/event"
)

type preparedEBPF struct {
	*EBPF
	runtime  *devboxbpf.Runtime
	close    sync.Once
	closeErr error
}

// NewEBPF constructs and attaches the real Linux source.
//
// Loading here, before the agent restores its nftables policy, is intentional.
// A lazy load let source selection and the handshake claim success, cleared
// the DNS-backed allow sets, and only then discovered missing BPF privileges.
// systemd repeated that destructive sequence every two seconds.
func NewEBPF(boxID string, boot time.Time) (Source, error) {
	runtime, err := devboxbpf.Load()
	if err != nil {
		return nil, fmt.Errorf("start eBPF capture: %w", err)
	}
	return &preparedEBPF{
		EBPF:    &EBPF{BoxID: boxID, Boot: boot},
		runtime: runtime,
	}, nil
}

// EBPFBuilt reports whether this binary contains the generated loader.
func EBPFBuilt() bool { return true }

// Run drains the three ring buffers prepared by NewEBPF.
func (e *preparedEBPF) Run(ctx context.Context, out chan<- *event.Event) error {
	readerCtx, cancelReaders := context.WithCancel(ctx)
	defer cancelReaders()
	clock := decode.Clock{BootWall: e.Boot}
	type stream struct {
		name   string
		reader *ringbuf.Reader
		decode func([]byte, string, decode.Clock) (*event.Event, error)
	}
	streams := []stream{
		{"exec", e.runtime.Exec, decode.DecodeExec},
		{"network", e.runtime.Net, decode.DecodeNet},
		{"file", e.runtime.File, decode.DecodeFile},
	}

	errCh := make(chan error, len(streams))
	var readers sync.WaitGroup
	readers.Add(len(streams))
	for _, input := range streams {
		go func(input stream) {
			defer readers.Done()
			for {
				record, readErr := input.reader.Read()
				if readErr != nil {
					errCh <- fmt.Errorf("read %s ring buffer: %w", input.name, readErr)
					return
				}
				decoded, decodeErr := input.decode(record.RawSample, e.BoxID, clock)
				if decodeErr != nil {
					errCh <- fmt.Errorf("decode %s ring record: %w", input.name, decodeErr)
					return
				}
				if sendErr := Send(readerCtx, out, decoded); sendErr != nil {
					errCh <- sendErr
					return
				}
			}
		}(input)
	}

	var runErr error
	select {
	case <-ctx.Done():
	case runErr = <-errCh:
	}
	cancelReaders()
	// Closing the readers is what releases siblings blocked in Read. Run must
	// not return until every goroutine is gone: the caller closes its event
	// channel immediately after Run, and an outliving writer would panic.
	_ = e.Close()
	readers.Wait()
	if ctx.Err() != nil {
		return nil
	}
	return runErr
}

// Close detaches the probes and unblocks every reader. It is safe after Run
// and also on the path where policy setup fails before Run starts.
func (e *preparedEBPF) Close() error {
	e.close.Do(func() { e.closeErr = e.runtime.Close() })
	return e.closeErr
}
