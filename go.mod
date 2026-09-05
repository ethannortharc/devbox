// Single Go module for the in-guest services (see DECISIONS.md ADR-0006):
//
//	agent/  → devbox-obsd, the eBPF observability agent
//
// It shares `internal/` with the host build, ships inside the same binary, and
// is versioned together with the Rust host binary, so one module is the honest
// boundary.
module github.com/ethannortharc/devbox

go 1.26

require (
	github.com/cilium/ebpf v0.20.0
	golang.org/x/sys v0.37.0
)
