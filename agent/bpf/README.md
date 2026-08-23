# eBPF probes

CO-RE programs that feed the observability plane (§7.1).

| Program | Attach point | Produces |
|---|---|---|
| `handle_exec` | tracepoint `sched/sched_process_exec` | `exec` — path, argv, cwd |
| `handle_tcp_v4_connect` | kprobe `tcp_v4_connect` | records the socket and process for completion |
| `handle_tcp_v6_connect` | kprobe `tcp_v6_connect` | records the socket and process for completion |
| `handle_tcp_finish_connect` | kprobe `tcp_finish_connect` | established outbound `connect` — 5-tuple |
| `handle_accept` | kretprobe `inet_csk_accept` | `accept` — 5-tuple |
| `handle_openat` | tracepoint `syscalls/sys_enter_openat` | `file` — path, flags, op |

DNS and TLS SNI are **not** eBPF programs: they are parsed from the flow by
`agent/decode` (see `wire.go`). A ClientHello is plaintext by design and a DNS
message is a UDP payload, so neither needs a kernel probe — and keeping them in
userspace keeps the verifier's job small.

## Filtering

Every probe checks `cgroup_is_traced` before reserving a ring-buffer slot, and
the map can represent selected cgroups. The current loader deliberately writes
key `0`, the "trace everything" sentinel: an eBPF agent runs inside a dedicated
one-box VM kernel. Shared-kernel Docker does not enable eBPF and uses
proc+packet capture instead. Selective cgroup population is future work, not a
containment property of the current loader.

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

Release artifacts embed the generated CO-RE agent, which runs inside the Lima
guest on macOS. A plain source build embeds the portable proc+packet agent.

## Why connect needs both entry and handshake-completion probes

At `tcp_v*_connect` **entry** the kernel has not yet copied the destination
into `skc_daddr`/`skc_dport`, nor picked the local port. Reading the socket
there yields zeroes or the previous connection's values — every outbound flow
decoded wrong, plausibly enough that nothing looked broken. The entry probe
therefore stashes process identity against the socket pointer. The
`tcp_finish_connect` probe runs on the SYN-ACK path, reads the now-populated
fields, emits the event, and deletes that entry. Attempts which never complete
produce no event; the bounded LRU map eventually evicts their stale entries.

`fill_net` writes **every** field of the record, including `_pad`, the unused
address bytes, and the byte/duration counters. `bpf_ringbuf_reserve` hands back
reused memory, not zeroed memory, so a field left untouched carries whatever
the previous record put there — and the Go decoder reports it as real traffic.
