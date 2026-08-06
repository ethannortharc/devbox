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

//go:generate go run github.com/cilium/ebpf/cmd/bpf2go -type exec_event -type net_event -type file_event -cc clang -target bpfel Devbox devbox.bpf.c -- -I./include
