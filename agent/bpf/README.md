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

`devbox_<arch>_bpfel.go` and `devbox_<arch>_bpfel.o` are **tracked**, one pair
per guest architecture. They are generated output, and committing generated
output is a cost — but the alternative was worse. bpf2go compiles kprobe
register access against the build host's own BTF, so the object can only be
produced on a Linux machine of the target architecture with a BTF kernel; a
developer on macOS has neither. While the pair was untracked, `build.rs` had no
object to embed and every build from source — which is every build outside a
tagged release — silently shipped the portable proc+packet agent instead. The
symptom was not an error. It was `devbox watch` reporting connections with pid
`4294967295`, no file events at all, and a behaviour summary reading `↑0B ↓0B`:
a capture that looked like a working one.

`vmlinux.h` stays untracked. It is 5 MB of one kernel's types, it is an input
rather than an artifact, and unlike the object it can be regenerated anywhere
the object can.

### Regenerating

On a Linux host of the target architecture with `clang`, libbpf headers, and
`bpftool`:

```bash
bpftool btf dump file /sys/kernel/btf/vmlinux format c > agent/bpf/vmlinux.h
go generate -tags bpf2go ./agent/bpf
```

`go generate` passes `-target $GOARCH`, so the pair it writes is named for the
architecture of the host doing the generating. Commit both files.

Inside a devbox NixOS guest, which is the usual way to reach an arm64 kernel
from a macOS host:

```bash
nix-shell -p go clang llvm libbpf --run '
  export NIX_HARDENING_ENABLE=""
  go generate -tags bpf2go ./agent/bpf'
```

`NIX_HARDENING_ENABLE=""` is required: nixpkgs' cc-wrapper injects
`-fzero-call-used-regs=used-gpr`, which clang rejects outright for the `bpfel`
target.

### When it must be regenerated

- `devbox.bpf.c` changed at all — programs, maps, or record structs.
- `agent/decode/record.go` changed its layout. The two are mirrors and
  `agent/decode`'s size assertions are what catch a half-applied change.
- The pinned `github.com/cilium/ebpf` version changed, which can change the
  shape of the generated loader.

A stale object does not fail to build. It loads, attaches, and decodes into
plausible-looking nonsense — which is why the `ebpf` CI job regenerates the
amd64 pair on every run and fails if it differs from what is committed.

The amd64 pair is produced by CI, not by hand: no arm64 developer machine can
make a valid one. When the job finds none committed it uploads the generated
pair as the `devbox-bpf-amd64` artifact to be downloaded and committed.

## Testing

- **Decoders** — `agent/decode`, against fixtures. No kernel, no privileges,
  runs everywhere.
- **Loading and attaching** — the `ebpf` job in `.github/workflows/ci.yml`, on
  a privileged Linux runner with BTF.

A source build embeds the CO-RE agent whenever this checkout has the object for
the guest architecture — `build.rs` looks for `devbox_<arch>_bpfel.o` and adds
`-tags ebpf` when it finds one. Without it the build still succeeds, embedding
the portable proc+packet agent and warning that it did.

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
