//go:build !linux || !ebpf

package capture

import (
	"context"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// NewEBPF refuses when the generated Linux source was not selected at build
// time. This is decided before the handshake so the agent never advertises a
// capture domain it cannot produce.
func NewEBPF(_ string, _ time.Time) (Source, error) {
	return nil, &ErrUnsupported{
		Source: "ebpf",
		Reason: "this binary was built without the Linux `ebpf` build tag",
	}
}

// EBPFBuilt reports whether this binary contains the generated loader.
func EBPFBuilt() bool { return false }

// Run exists so EBPF continues to satisfy Source in portable builds. The
// constructor above prevents it from being selected.
func (e *EBPF) Run(context.Context, chan<- *event.Event) error {
	return &ErrUnsupported{Source: e.Name(), Reason: "not built for this platform"}
}
