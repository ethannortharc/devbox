// Single Go module covering both in-guest services (see DECISIONS.md ADR-0006):
//
//	agent/  → devbox-obsd, the eBPF observability agent
//	ztpd/   → devbox-ztpd, the ZTP provisioning server
//
// They share `internal/`, ship in the same binary, and are versioned together
// with the Rust host binary, so one module is the honest boundary.
module github.com/ethannortharc/devbox

go 1.26

require (
	github.com/cilium/ebpf v0.20.0
	golang.org/x/sys v0.37.0
)
