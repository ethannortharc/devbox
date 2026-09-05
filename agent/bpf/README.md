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
userspace keeps the verifier's job small. The two differ in one way that
matters: a DNS message arrives whole in one datagram, while a ClientHello no
longer does. TLS 1.3 now offers a post-quantum key share by default and that
pushes the record past a 1448-byte MSS, so `decode.SNI` parses whatever a
segment actually carries and answers `ErrNeedMore` when `server_name` is not in
it yet, and `agent/capture` joins the flow's next segment — along the sequence
number, capped at 4 segments and 8 KiB — and retries. Doing that in a kernel
probe would mean a TCP reassembler inside the verifier's reach, which is exactly
the trade this split avoids.

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

Regenerate and commit the pair for **every** architecture when any of these
changes:

- `devbox.bpf.c`, in any way at all — not only when a program, map, or record
  struct is added or renamed. Editing the body of an existing probe changes the
  object and leaves the bindings identical, and that case is the one nothing
  else will catch (see below).
- `agent/decode/record.go`'s layout. It and the record structs in
  `devbox.bpf.c` are mirrors of one another, and `agent/decode`'s size
  assertions are what turn a half-applied change into a test failure instead of
  a misdecoded event.
- The pinned `github.com/cilium/ebpf` version, which can change the shape of
  the generated loader.

A stale object does not fail to build. It loads, it attaches, and it decodes
into plausible-looking nonsense — pids that are not pids, paths assembled from
the wrong offsets. That is the failure mode this section exists to prevent.

### What CI does and does not check

The `ebpf` job regenerates the amd64 pair on every run, and gates the two
halves differently:

| file | gate |
|---|---|
| `devbox_amd64_bpfel.go` | byte-compared against what is committed; **differs → job fails** |
| `devbox_amd64_bpfel.o` | not compared; uploaded as the `devbox-bpf-amd64` artifact |

The bindings are comparable because they encode only the loader's API surface —
program, map, and type names — which moves when and only when `devbox.bpf.c`
moves.

The object is not. It embeds BTF derived from the runner's own kernel, so a
clang upgrade or a runner image bump rewrites its bytes with no commit behind
the change. Gating on that would mean a red build that no diff explains and
that regenerating "fixes" only until the next image bump, which is how a check
gets ignored and then deleted.

**So one case is on you, not on CI**: change a probe's body without touching a
program, map, or type name, and the bindings come out identical while the
object does not. CI stays green with a stale object committed. Treat the list
above as the rule and regenerate on any `devbox.bpf.c` edit.

The amd64 pair is produced by CI, not by hand — no arm64 developer machine can
make a valid one. Every run uploads the freshly generated pair as
`devbox-bpf-amd64` (kept 14 days), including runs where the bindings check
failed, which is precisely when you want it: download it and commit both files.

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
