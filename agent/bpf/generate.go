//go:build bpf2go

// Package bpf holds the CO-RE eBPF programs and their generated loaders.
//
// Code generation is behind the `bpf2go` build tag so a plain `go build` on any
// host — including macOS, where eBPF cannot exist — stays green. The generated
// files land in this package and are compiled only on Linux.
//
// Regenerate on a Linux host with clang, libbpf headers, and bpftool:
//
//	bpftool btf dump file /sys/kernel/btf/vmlinux format c > agent/bpf/vmlinux.h
//	go generate -tags bpf2go ./agent/bpf/...
//
// The privileged Linux CI lane (`ebpf` in .github/workflows/ci.yml) does this
// and then runs the load tests; every other lane uses the recorded fixtures in
// `agent/decode`, which need no kernel at all.
package bpf

// The kprobe argument macros need a concrete register ABI, so `bpfel` is not
// sufficient here. The generated vmlinux.h describes the running build host,
// therefore the object must use that same GOARCH. Devbox guests are native
// architecture; cross-building an agent also needs BTF from the target guest.
//go:generate go run github.com/cilium/ebpf/cmd/bpf2go -tags ebpf -cc clang -target $GOARCH Devbox devbox.bpf.c -- -I./include
