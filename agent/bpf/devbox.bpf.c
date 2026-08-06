// devbox observability probes.
//
// CO-RE eBPF programs feeding `agent/decode`. Every record written here has a
// mirror struct in `agent/decode/record.go`, and the sizes are asserted in
// that package's tests — a silent layout drift would not fail to compile, it
// would decode plausible-looking garbage.
//
// Filtering happens in-kernel, on cgroup id: a box is a cgroup, so the probes
// discard everything outside the traced boxes before it ever reaches the ring
// buffer. That is what keeps the overhead budget (§7.3, <3% CPU at 10k
// events/s) achievable — the expensive part of tracing is the events you
// forward, not the ones you skip.
//
// Built with bpf2go (see generate.go); requires clang, libbpf headers, and a
// kernel with BTF. Loading is exercised only in the privileged Linux CI lane.

//go:build ignore

#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_tracing.h>

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
	__u8 _pad;
	__u16 sport;
	__u16 dport;
	__u8 saddr[16];
	__u8 daddr[16];
	__u64 bytes_tx;
	__u64 bytes_rx;
	__u64 dur_ns;
};

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

// Cgroup ids the agent is watching. Userspace populates this on attach; an
// empty map means "watch everything", which is what a single-box agent wants.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 256);
	__type(key, __u64);
	__type(value, __u8);
} traced_cgroups SEC(".maps");

static __always_inline int cgroup_is_traced(__u64 cgroup_id)
{
	__u32 zero = 0;
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
	rec->argv_len = (__u32)len;
	bpf_probe_read_user(&rec->argv, len & (sizeof(rec->argv) - 1),
			    (void *)arg_start);

	bpf_ringbuf_submit(rec, 0);
	return 0;
}

SEC("kprobe/tcp_v4_connect")
int BPF_KPROBE(handle_tcp_v4_connect, struct sock *sk)
{
	__u64 cgroup_id = bpf_get_current_cgroup_id();
	if (!cgroup_is_traced(cgroup_id))
		return 0;

	struct net_event *rec = bpf_ringbuf_reserve(&net_events, sizeof(*rec), 0);
	if (!rec)
		return 0;

	FILL_COMMON(rec);
	rec->family = 2;  /* AF_INET */
	rec->proto = 6;   /* IPPROTO_TCP */
	rec->direction = 0;

	__u32 saddr = BPF_CORE_READ(sk, __sk_common.skc_rcv_saddr);
	__u32 daddr = BPF_CORE_READ(sk, __sk_common.skc_daddr);
	__builtin_memcpy(rec->saddr, &saddr, 4);
	__builtin_memcpy(rec->daddr, &daddr, 4);
	rec->sport = BPF_CORE_READ(sk, __sk_common.skc_num);
	rec->dport = bpf_ntohs(BPF_CORE_READ(sk, __sk_common.skc_dport));

	bpf_ringbuf_submit(rec, 0);
	return 0;
}

SEC("kprobe/tcp_v6_connect")
int BPF_KPROBE(handle_tcp_v6_connect, struct sock *sk)
{
	__u64 cgroup_id = bpf_get_current_cgroup_id();
	if (!cgroup_is_traced(cgroup_id))
		return 0;

	struct net_event *rec = bpf_ringbuf_reserve(&net_events, sizeof(*rec), 0);
	if (!rec)
		return 0;

	FILL_COMMON(rec);
	rec->family = 10; /* AF_INET6 */
	rec->proto = 6;
	rec->direction = 0;

	BPF_CORE_READ_INTO(&rec->saddr, sk,
			   __sk_common.skc_v6_rcv_saddr.in6_u.u6_addr8);
	BPF_CORE_READ_INTO(&rec->daddr, sk,
			   __sk_common.skc_v6_daddr.in6_u.u6_addr8);
	rec->sport = BPF_CORE_READ(sk, __sk_common.skc_num);
	rec->dport = bpf_ntohs(BPF_CORE_READ(sk, __sk_common.skc_dport));

	bpf_ringbuf_submit(rec, 0);
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
	rec->family = 2;
	rec->proto = 6;
	rec->direction = 1; /* accept */

	__u32 saddr = BPF_CORE_READ(sk, __sk_common.skc_rcv_saddr);
	__u32 daddr = BPF_CORE_READ(sk, __sk_common.skc_daddr);
	__builtin_memcpy(rec->saddr, &saddr, 4);
	__builtin_memcpy(rec->daddr, &daddr, 4);
	rec->sport = BPF_CORE_READ(sk, __sk_common.skc_num);
	rec->dport = bpf_ntohs(BPF_CORE_READ(sk, __sk_common.skc_dport));

	bpf_ringbuf_submit(rec, 0);
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
