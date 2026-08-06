# eBPF probes

CO-RE programs that feed the observability plane (§7.1).

| Program | Attach point | Produces |
|---|---|---|
| `handle_exec` | tracepoint `sched/sched_process_exec` | `exec` — path, argv, cwd |
| `handle_tcp_v4_connect` | kprobe `tcp_v4_connect` | `connect` — 5-tuple |
| `handle_tcp_v6_connect` | kprobe `tcp_v6_connect` | `connect` — 5-tuple |
| `handle_accept` | kretprobe `inet_csk_accept` | `accept` — 5-tuple |
| `handle_openat` | tracepoint `syscalls/sys_enter_openat` | `file` — path, flags, op |

DNS and TLS SNI are **not** eBPF programs: they are parsed from the flow by
`agent/decode` (see `wire.go`). A ClientHello is plaintext by design and a DNS
message is a UDP payload, so neither needs a kernel probe — and keeping them in
userspace keeps the verifier's job small.

## Filtering

Every probe checks `cgroup_is_traced` before reserving a ring-buffer slot. A
box is a cgroup, so anything outside the traced boxes is discarded **in the
kernel**, before it costs a copy or a wakeup. That is what makes the §7.3
overhead budget (< 3% CPU at 10k events/s) reachable — the expense of tracing
is the events you forward, not the ones you skip.

Key `0` in `traced_cgroups` is a sentinel meaning "trace everything", which is
what a single-box agent installs.

## Record layout

Each `struct` here has a mirror in `agent/decode/record.go`, and
`agent/decode`'s tests assert the byte sizes (`ExecRecordSize` and friends).
This matters more than it looks: a field added on one side and not the other
would still compile, and would decode into plausible-looking garbage. Change
the two together, and let the size assertions catch you if you forget.

## Building

Generation is behind the `bpf2go` build tag so a plain `go build` works on any
host, including macOS where eBPF cannot exist at all.

On a Linux host with `clang`, libbpf headers, and `bpftool`:

```bash
bpftool btf dump file /sys/kernel/btf/vmlinux format c > agent/bpf/vmlinux.h
go generate -tags bpf2go ./agent/bpf/...
```

`vmlinux.h` is generated, kernel-specific, and deliberately untracked.

## Testing

- **Decoders** — `agent/decode`, against fixtures. No kernel, no privileges,
  runs everywhere.
- **Loading and attaching** — the `ebpf` job in `.github/workflows/ci.yml`, on
  a privileged Linux runner with BTF.

On macOS the agent runs inside the Lima guest, so eBPF is available there even
though the host cannot load it (§13).
