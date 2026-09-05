package capture

import (
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// EBPF streams the kernel records produced by agent/bpf. The implementation
// is compiled only into Linux binaries built with the `ebpf` tag; every other
// build gets a constructor that refuses loudly instead of degrading silently.
type EBPF struct {
	BoxID string
	Boot  time.Time
}

// Name implements Source.
func (e *EBPF) Name() string { return "ebpf" }

// Domains implements Source. DNS and TLS need packet payload capture; the
// current probes truthfully advertise only what their ring buffers produce.
func (e *EBPF) Domains() []event.Type {
	return []event.Type{
		event.TypeExec, event.TypeConnect, event.TypeAccept,
		// `tcp_close` settles a connection: the connect and accept records
		// say a connection happened, this one says what crossed it.
		event.TypeClose,
		event.TypeFile,
	}
}
