// devbox observability probes.
//
// CO-RE eBPF programs feeding `agent/decode`. Every record written here has a
// mirror struct in `agent/decode/record.go`, and the sizes are asserted in
// that package's tests — a silent layout drift would not fail to compile, it
// would decode plausible-looking garbage.
//
// The map supports in-kernel cgroup selection, but the current loader installs
// its trace-all sentinel. eBPF runs only in a dedicated one-box VM kernel;
// shared-kernel Docker uses proc+packet capture. The namespace boundary, not a
// populated allow-map, is therefore the current containment mechanism.
//
// Built with bpf2go (see generate.go); requires clang, libbpf headers, and a
// kernel with BTF. Loading is exercised only in the privileged Linux CI lane.

//go:build ignore

#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_tracing.h>
// bpf_ntohs lives here, and the network probes below call it.
#include <bpf/bpf_endian.h>

char LICENSE[] SEC("license") = "GPL";

#define COMM_LEN 16
#define FILENAME_LEN 256
#define ARGV_LEN 512
#define PATH_LEN 256

// Ring buffer sized so a burst (a `make -j` storm, a `pip install`) rides out
// without dropping. Drops are counted, never silent.
#define RINGBUF_SIZE (1 << 22) /* 4 MiB */

struct exec_event {
	__u64 ts_mono_ns;
	__u64 cgroup_id;
	__u32 pid;
	__u32 tid;
	__u32 ppid;
	__u32 uid;
	char comm[COMM_LEN];
	char filename[FILENAME_LEN];
	__u32 argv_len;
	char argv[ARGV_LEN];
};

struct net_event {
	__u64 ts_mono_ns;
	__u64 cgroup_id;
	__u32 pid;
	__u32 tid;
	__u32 ppid;
	__u32 uid;
	char comm[COMM_LEN];
	__u8 family;    /* 2 = AF_INET, 10 = AF_INET6 */
	__u8 proto;     /* 6 = TCP, 17 = UDP */
	__u8 direction; /* 0 = connect, 1 = accept */
	__u8 flags;     /* NET_FLAG_*, below */
	__u16 sport;
	__u16 dport;
	__u8 saddr[16];
	__u8 daddr[16];
	/* Final only on a NET_FLAG_CLOSE record. See handle_tcp_close. */
	__u64 bytes_tx;
	__u64 bytes_rx;
	__u64 dur_ns;
};

/* This record settles a connection rather than opening one: the byte counters
 * and the duration are final, and the decoder turns it into a `close` event.
 * It occupies the byte that used to be explicit padding, so the record layout
 * and its 112-byte size are unchanged. */
#define NET_FLAG_CLOSE (1 << 0)
/* A settlement whose opening this agent never saw — a connection older than
 * the agent, or one whose handshake was missed. The identity on the record is
 * the process that closed the socket, not the one that dialled it, and the
 * direction field means nothing. */
#define NET_FLAG_ORPHAN (1 << 1)

struct file_event {
	__u64 ts_mono_ns;
	__u64 cgroup_id;
	__u32 pid;
	__u32 tid;
	__u32 ppid;
	__u32 uid;
	char comm[COMM_LEN];
	__u32 flags;
	__u32 op;
	char path[PATH_LEN];
};

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, RINGBUF_SIZE);
} exec_events SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, RINGBUF_SIZE);
} net_events SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, RINGBUF_SIZE);
} file_events SEC(".maps");

// Cgroup ids the agent is watching. Key zero is the explicit trace-all
// sentinel used by the current one-box VM loader; an empty map traces nothing.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 256);
	__type(key, __u64);
	__type(value, __u8);
} traced_cgroups SEC(".maps");

static __always_inline int cgroup_is_traced(__u64 cgroup_id)
{
	// __u64, matching the map's declared key width. A 4-byte stack slot made
	// the verifier read 8 bytes from a partly uninitialized stack, which can
	// reject the program outright — and a rejected program means no probes
	// load at all.
	__u64 zero = 0;
	// An empty allowlist means no filtering. Checking a lookup against a
	// sentinel key is cheaper than counting the map on every event.
	__u8 *any = bpf_map_lookup_elem(&traced_cgroups, &zero);
	if (any)
		return 1;
	return bpf_map_lookup_elem(&traced_cgroups, &cgroup_id) != NULL;
}

// Fill the identity fields every record shares.
#define FILL_COMMON(rec)                                                       \
	do {                                                                   \
		__u64 id = bpf_get_current_pid_tgid();                         \
		(rec)->ts_mono_ns = bpf_ktime_get_ns();                        \
		(rec)->cgroup_id = bpf_get_current_cgroup_id();                \
		(rec)->pid = id >> 32;                                         \
		(rec)->tid = (__u32)id;                                        \
		(rec)->uid = (__u32)bpf_get_current_uid_gid();                 \
		bpf_get_current_comm(&(rec)->comm, sizeof((rec)->comm));       \
		struct task_struct *task =                                     \
			(struct task_struct *)bpf_get_current_task();          \
		(rec)->ppid = BPF_CORE_READ(task, real_parent, tgid);          \
	} while (0)

SEC("tracepoint/sched/sched_process_exec")
int handle_exec(struct trace_event_raw_sched_process_exec *ctx)
{
	__u64 cgroup_id = bpf_get_current_cgroup_id();
	if (!cgroup_is_traced(cgroup_id))
		return 0;

	struct exec_event *rec = bpf_ringbuf_reserve(&exec_events, sizeof(*rec), 0);
	if (!rec)
		return 0; /* ring full — userspace surfaces the drop counter */

	FILL_COMMON(rec);

	unsigned fname_off = ctx->__data_loc_filename & 0xFFFF;
	bpf_probe_read_kernel_str(&rec->filename, sizeof(rec->filename),
				  (void *)ctx + fname_off);

	// argv lives in user memory behind mm->arg_start; a bounded copy keeps
	// the verifier happy and the record fixed-size.
	struct task_struct *task = (struct task_struct *)bpf_get_current_task();
	unsigned long arg_start = BPF_CORE_READ(task, mm, arg_start);
	unsigned long arg_end = BPF_CORE_READ(task, mm, arg_end);
	unsigned long len = arg_end - arg_start;
	if (len > sizeof(rec->argv))
		len = sizeof(rec->argv);
	// The bound above is what the verifier needs; masking with
	// `len & (sizeof - 1)` additionally turned a full-width argv into a
	// zero-byte copy while argv_len still claimed 512, so the decoder read
	// stale ring-buffer memory as a command line.
	rec->argv_len = (__u32)len;
	if (len > 0)
		bpf_probe_read_user(&rec->argv, (__u32)len, (void *)arg_start);

	bpf_ringbuf_submit(rec, 0);
	return 0;
}

// Sockets seen at connect entry, keyed by thread, so the return probe knows
// which socket the syscall was about.
//
// The socket's destination and local port are only filled in *by* the connect
// call. Reading them at entry — which the first version did — yields zeroes or
// the previous connection's values, so every outbound flow decoded wrong.
// Who opened each in-flight connection, keyed by the socket.
//
// `tcp_finish_connect` runs on the SYN-ACK, in softirq — the current pid and
// cgroup there are whatever the CPU happened to be doing, not the process that
// called connect(). So the identity is captured at connect time, when the
// process is on-CPU, and looked up when the handshake completes.
//
// LRU rather than a plain hash: a connect that never completes leaves an
// entry, and there is no reliable hook to clean up every one of them. LRU
// bounds the map by construction instead of leaking on a box that dials a lot
// of dead peers.
struct conn_owner {
	__u64 cgroup_id;
	__u32 pid;
	__u32 tid;
	__u32 ppid;
	__u32 uid;
	char comm[COMM_LEN];
};

struct {
	__uint(type, BPF_MAP_TYPE_LRU_HASH);
	__uint(max_entries, 8192);
	__type(key, __u64);
	__type(value, struct conn_owner);
} connecting SEC(".maps");

// A connection that reached ESTABLISHED, kept until it closes.
//
// `connecting` only has to survive a handshake; this has to survive the whole
// connection, because the byte counters are not readable until `tcp_close` and
// by then the dialling process is long off-CPU — `tcp_close` runs in whoever
// closes the fd, which after an fd is inherited or passed is not the process
// that opened it. Establishment time lives here too: the kernel keeps no
// "connected at" stamp a probe can read back, so the duration has to be
// measured by remembering one end of it.
//
// LRU for the same reason `connecting` is: a socket whose close we never see
// (the box is torn down, the probe is detached mid-flight) would otherwise
// leak an entry per connection.
struct conn_state {
	struct conn_owner owner;
	__u64 open_ts_ns;
	/* The local port as the connect or accept record published it.
	 *
	 * Not re-read at close: a socket whose teardown finished before the fd
	 * was closed has already been unhashed, and `skc_num` is then 0. That
	 * was observed on a real box — an sshd connection settled with
	 * `sport: 0` while its accept record said 22 — and the flow table joins
	 * a close to its opening on exactly that tuple. Remembering the port is
	 * the only way to keep the two halves of one connection together. */
	__u16 sport;
	__u8 direction; /* 0 = dialled, 1 = accepted */
};

struct {
	__uint(type, BPF_MAP_TYPE_LRU_HASH);
	__uint(max_entries, 8192);
	__type(key, __u64);
	__type(value, struct conn_state);
} established SEC(".maps");

// Remember an established connection so its close can be attributed and timed.
static __always_inline void remember_established(struct sock *sk,
						 const struct conn_owner *owner,
						 __u64 opened_ns, __u16 sport,
						 __u8 direction)
{
	struct conn_state state = {};
	state.owner = *owner;
	state.open_ts_ns = opened_ns;
	state.sport = sport;
	state.direction = direction;

	__u64 key = (__u64)sk;
	bpf_map_update_elem(&established, &key, &state, BPF_ANY);
}

static __always_inline int connect_enter(struct sock *sk)
{
	__u64 cgroup_id = bpf_get_current_cgroup_id();
	if (!cgroup_is_traced(cgroup_id))
		return 0;

	struct conn_owner owner = {};
	__u64 id = bpf_get_current_pid_tgid();
	owner.cgroup_id = cgroup_id;
	owner.pid = id >> 32;
	owner.tid = (__u32)id;
	owner.uid = (__u32)bpf_get_current_uid_gid();
	struct task_struct *task = (struct task_struct *)bpf_get_current_task();
	owner.ppid = BPF_CORE_READ(task, real_parent, tgid);
	bpf_get_current_comm(&owner.comm, sizeof(owner.comm));

	__u64 key = (__u64)sk;
	bpf_map_update_elem(&connecting, &key, &owner, BPF_ANY);
	return 0;
}

// Fill the address fields the decoder reads, for either family.
//
// Every byte is written, including the flags byte and the counters. The ring
// buffer hands back *reused* memory, not zeroed memory, so a field left
// untouched is whatever the previous record put there — which the Go decoder
// then reports as real bytes transferred and a real duration.
//
// The counters are zeroed here and filled in only by `handle_tcp_close`. A
// connect or accept record describes a connection that has just come into
// existence, and no payload has moved across it yet; anything else this probe
// could write into those fields would be a guess.
static __always_inline void fill_net(struct net_event *rec, struct sock *sk,
				     __u8 direction)
{
	rec->proto = 6; /* IPPROTO_TCP */
	rec->direction = direction;
	rec->flags = 0;
	rec->bytes_tx = 0;
	rec->bytes_rx = 0;
	rec->dur_ns = 0;
	__builtin_memset(rec->saddr, 0, sizeof(rec->saddr));
	__builtin_memset(rec->daddr, 0, sizeof(rec->daddr));

	__u16 family = BPF_CORE_READ(sk, __sk_common.skc_family);
	rec->family = (__u8)family;

	if (family == 10 /* AF_INET6 */) {
		BPF_CORE_READ_INTO(&rec->saddr, sk,
				   __sk_common.skc_v6_rcv_saddr.in6_u.u6_addr8);
		BPF_CORE_READ_INTO(&rec->daddr, sk,
				   __sk_common.skc_v6_daddr.in6_u.u6_addr8);
	} else {
		rec->family = 2; /* AF_INET */
		__u32 saddr = BPF_CORE_READ(sk, __sk_common.skc_rcv_saddr);
		__u32 daddr = BPF_CORE_READ(sk, __sk_common.skc_daddr);
		__builtin_memcpy(rec->saddr, &saddr, 4);
		__builtin_memcpy(rec->daddr, &daddr, 4);
	}

	rec->sport = BPF_CORE_READ(sk, __sk_common.skc_num);
	rec->dport = bpf_ntohs(BPF_CORE_READ(sk, __sk_common.skc_dport));
}

SEC("kprobe/tcp_v4_connect")
int BPF_KPROBE(handle_tcp_v4_connect, struct sock *sk)
{
	return connect_enter(sk);
}

SEC("kprobe/tcp_v6_connect")
int BPF_KPROBE(handle_tcp_v6_connect, struct sock *sk)
{
	return connect_enter(sk);
}

// Handshake completion, which is when an outbound connection is real.
//
// `tcp_finish_connect` runs on the SYN-ACK path, so reaching it means the peer
// answered — which is the only point at which an outbound connection is real.
// The entry probe recorded who dialled; this reports it and clears the entry.
// A connect that never completes leaves no event, and its map entry ages out.
SEC("kprobe/tcp_finish_connect")
int BPF_KPROBE(handle_tcp_finish_connect, struct sock *sk)
{
	__u64 key = (__u64)sk;
	struct conn_owner *owner = bpf_map_lookup_elem(&connecting, &key);
	if (!owner)
		return 0; // not a socket we are tracing

	struct net_event *rec = bpf_ringbuf_reserve(&net_events, sizeof(*rec), 0);
	if (!rec) {
		bpf_map_delete_elem(&connecting, &key);
		return 0;
	}

	// The identity from connect time, not from here: this runs in softirq on
	// the SYN-ACK, where the current process is unrelated to the one that
	// dialled.
	rec->ts_mono_ns = bpf_ktime_get_ns();
	rec->cgroup_id = owner->cgroup_id;
	rec->pid = owner->pid;
	rec->tid = owner->tid;
	rec->ppid = owner->ppid;
	rec->uid = owner->uid;
	__builtin_memcpy(rec->comm, owner->comm, COMM_LEN);
	fill_net(rec, sk, 0 /* outbound */);

	// Before the submit: the record is the ring buffer's once it is handed
	// back, and reading it after that is reading whatever was written next.
	__u64 opened = rec->ts_mono_ns;
	__u16 sport = rec->sport;
	bpf_ringbuf_submit(rec, 0);

	// The dialler's identity moves from `connecting` (which only had to
	// outlive the handshake) to `established` (which has to outlive the
	// connection), so `tcp_close` can credit the bytes to it.
	remember_established(sk, owner, opened, sport, 0 /* dialled */);
	bpf_map_delete_elem(&connecting, &key);
	return 0;
}

SEC("kretprobe/inet_csk_accept")
int BPF_KRETPROBE(handle_accept, struct sock *sk)
{
	if (!sk)
		return 0;

	__u64 cgroup_id = bpf_get_current_cgroup_id();
	if (!cgroup_is_traced(cgroup_id))
		return 0;

	struct net_event *rec = bpf_ringbuf_reserve(&net_events, sizeof(*rec), 0);
	if (!rec)
		return 0;

	FILL_COMMON(rec);
	// Family read from the socket, not assumed: `inet_csk_accept` serves
	// IPv6 listeners too, and hardcoding AF_INET decoded every inbound IPv6
	// connection as a bogus IPv4 flow.
	fill_net(rec, sk, 1 /* inbound */);

	// Copied out before the submit, for the same reason as above.
	struct conn_owner owner = {};
	owner.cgroup_id = rec->cgroup_id;
	owner.pid = rec->pid;
	owner.tid = rec->tid;
	owner.ppid = rec->ppid;
	owner.uid = rec->uid;
	__builtin_memcpy(owner.comm, rec->comm, COMM_LEN);
	__u64 opened = rec->ts_mono_ns;
	__u16 sport = rec->sport;

	bpf_ringbuf_submit(rec, 0);
	remember_established(sk, &owner, opened, sport, 1 /* accepted */);
	return 0;
}

// Connection teardown, which is the first moment the byte counters are true.
//
// Every other network probe here fires while a connection is being *made*:
// `tcp_v*_connect` at the syscall, `tcp_finish_connect` on the SYN-ACK,
// `inet_csk_accept` on the listener. None of them can report traffic, because
// at each of them no payload has crossed the socket yet — which is why the
// byte and duration fields on a connect record were, and remain, zero.
//
// `tcp_sock` carries running totals (`bytes_sent`, `bytes_received`) that are
// final once the socket is being closed, so this probe is where a connection
// is settled. A kprobe, not a kretprobe: `tcp_close` unhashes the socket on
// its way out, and after that the local port is gone and the 5-tuple no longer
// matches the connect record it belongs to.
//
// `tcp_set_state` entering TCP_CLOSE was the alternative. It is rejected here
// because it fires on every state transition of every TCP socket on the box
// including in softirq, where the current process is unrelated to the socket,
// and because it also fires for sockets that never carried a byte. `tcp_close`
// fires once per socket, in the closing process's own context, which is what
// makes the cgroup check below meaningful.
#define TCP_STATE_CLOSE 7
#define TCP_STATE_LISTEN 10

SEC("kprobe/tcp_close")
int BPF_KPROBE(handle_tcp_close, struct sock *sk)
{
	__u64 key = (__u64)sk;
	struct conn_state *state = bpf_map_lookup_elem(&established, &key);
	__u8 flags = NET_FLAG_CLOSE;

	if (!state) {
		// A connection this agent never saw open. Worth settling — it
		// may have moved most of the box's traffic — but it is marked,
		// because its process identity is only whoever is closing it.
		if (!cgroup_is_traced(bpf_get_current_cgroup_id()))
			return 0;
		// A listener, or a socket that never left TCP_CLOSE, has
		// nothing to settle, and its 5-tuple is zeros — which would
		// decode into a connection to 0.0.0.0:0 that never happened.
		__u8 sk_state = BPF_CORE_READ(sk, __sk_common.skc_state);
		if (sk_state == TCP_STATE_LISTEN || sk_state == TCP_STATE_CLOSE)
			return 0;
		flags |= NET_FLAG_ORPHAN;
	}

	struct net_event *rec = bpf_ringbuf_reserve(&net_events, sizeof(*rec), 0);
	if (!rec) {
		bpf_map_delete_elem(&established, &key);
		return 0;
	}

	__u64 now = bpf_ktime_get_ns();
	__u64 dur = 0;
	__u8 direction = 0;
	if (state) {
		rec->ts_mono_ns = now;
		rec->cgroup_id = state->owner.cgroup_id;
		rec->pid = state->owner.pid;
		rec->tid = state->owner.tid;
		rec->ppid = state->owner.ppid;
		rec->uid = state->owner.uid;
		__builtin_memcpy(rec->comm, state->owner.comm, COMM_LEN);
		direction = state->direction;
		// A monotonic clock cannot go backwards, but the map value is
		// memory anything could have scribbled on; a wrapped subtraction
		// would report a connection that lasted five hundred years.
		if (now > state->open_ts_ns)
			dur = now - state->open_ts_ns;
	} else {
		FILL_COMMON(rec);
	}

	fill_net(rec, sk, direction);
	rec->flags = flags;
	// The remembered port wins over the socket's own, which by now can be
	// zero (see `conn_state.sport`). An orphan has nothing remembered and
	// keeps whatever the socket still says.
	if (state)
		rec->sport = state->sport;

	// `struct tcp_sock` starts with the `struct sock` this probe was handed,
	// so the cast is the standard CO-RE way to reach the TCP-only counters.
	// `bytes_sent` postdates `bytes_received` by several releases, so both
	// are guarded: a kernel without one still loads, and reports zero for
	// that direction rather than refusing to attach at all.
	struct tcp_sock *tp = (struct tcp_sock *)sk;
	if (bpf_core_field_exists(tp->bytes_sent))
		rec->bytes_tx = BPF_CORE_READ(tp, bytes_sent);
	if (bpf_core_field_exists(tp->bytes_received))
		rec->bytes_rx = BPF_CORE_READ(tp, bytes_received);
	rec->dur_ns = dur;

	bpf_ringbuf_submit(rec, 0);
	bpf_map_delete_elem(&established, &key);
	return 0;
}

SEC("tracepoint/syscalls/sys_enter_openat")
int handle_openat(struct trace_event_raw_sys_enter *ctx)
{
	__u64 cgroup_id = bpf_get_current_cgroup_id();
	if (!cgroup_is_traced(cgroup_id))
		return 0;

	struct file_event *rec = bpf_ringbuf_reserve(&file_events, sizeof(*rec), 0);
	if (!rec)
		return 0;

	FILL_COMMON(rec);

	const char *pathname = (const char *)ctx->args[1];
	bpf_probe_read_user_str(&rec->path, sizeof(rec->path), pathname);
	rec->flags = (__u32)ctx->args[2];

	// O_CREAT is 0100 octal; O_WRONLY|O_RDWR are 01 and 02. This is the same
	// classification `agent/decode` names, kept here so userspace does not
	// have to re-derive it.
	if (rec->flags & 0100)
		rec->op = 1; /* create */
	else if (rec->flags & 03)
		rec->op = 2; /* write */
	else
		rec->op = 0; /* open */

	bpf_ringbuf_submit(rec, 0);
	return 0;
}
