// SPDX-License-Identifier: MIT OR Apache-2.0
//
// qk's recorder: the file system calls of the process trees qk hands it,
// sent to user space through a ring buffer.
//
// A process joins a recording with a prctl option no kernel defines, which
// qk calls in the child just before it execs a task's command: with the
// run's secret, the call names the recording's group. Its children join the
// same group as they are forked. Which calls are recorded, and which of
// their arguments are paths, is the `calls` map, which user space fills for
// the architecture it runs on.
//
// A recorded call is one event, sent as it returns: its arguments, as they
// were when it entered, the paths they name, its result, and when it entered
// and returned. Events come in the order calls return, which is not the order
// they took effect in: a close frees its descriptor as it enters, so another
// thread can open the same number and return first. Both times let user
// space tell.
//
// The paths are read on return because a program on entry cannot fault in
// user memory, and a path's page may not be resident until the kernel has
// read it.
//
// The kernel's own types are declared here as far as this program reads
// them, and relocated against the running kernel's BTF as it loads.

#include <linux/types.h>
#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "Dual MIT/GPL";

typedef struct {
	int counter;
} atomic_t;

struct signal_struct {
	/* The process's threads that have not begun to exit. */
	atomic_t live;
} __attribute__((preserve_access_index));

struct task_struct {
	int pid;
	int tgid;
	struct signal_struct *signal;
} __attribute__((preserve_access_index));

#if defined(__TARGET_ARCH_x86)
struct pt_regs {
	unsigned long cs;
	unsigned long r10;
	unsigned long r9;
	unsigned long r8;
	unsigned long dx;
	unsigned long si;
	unsigned long di;
} __attribute__((preserve_access_index));
#elif defined(__TARGET_ARCH_arm64)
struct pt_regs {
	__u64 regs[31];
	__u64 pstate;
	__u64 orig_x0;
} __attribute__((preserve_access_index));
#else
#error "the recorder knows x86-64 and arm64"
#endif

/* The longest path read; one that fills it is reported as cut short. */
#define PATH_BYTES 4096
#define CALLS 512
#define NO_PATH 0xff

enum kind {
	BEGIN = 1,
	FORK = 2,
	CALL = 3,
	EXIT = 4,
};

/* What a call carries, by its number. */
struct call {
	/* Whether to record it at all. */
	__u8 recorded;
	/* The arguments that are paths, or NO_PATH. */
	__u8 paths[2];
	/* The argument pointing to a structure whose first eight bytes are
	 * flags, read in place of the pointer, as openat2's `how`; or NO_PATH. */
	__u8 flags_at;
	/* Only F_DUPFD and F_DUPFD_CLOEXEC of fcntl's commands, its second
	 * argument. */
	__u8 dup_commands_only;
};

/* A 32-bit process's code segment on x86-64, and its mode bit on arm64. */
#define USER32_CS 0x23
#define PSR_MODE32_BIT 0x10

#define F_DUPFD 0
#define F_DUPFD_CLOEXEC 1030

struct settings {
	/* prctl's number, and the run's secret, which a process names to join. */
	__u32 prctl;
	__u32 option;
	__u64 secret;
};

struct header {
	__u8 kind;
	__u8 unused;
	__u16 call;
	__u32 group;
	__u32 tgid;
	__u32 tid;
	__u64 time;
};

struct begin_event {
	struct header header;
};

struct exit_event {
	struct header header;
};

struct fork_event {
	struct header header;
	__u32 child;
	__u32 unused;
};

struct call_event {
	struct header header;
	__u64 args[6];
	__s64 result;
	/* When it returned; the header's time is when it entered. */
	__u64 exited;
	/* Each path's length, its terminating NUL included; 0 for none, and
	 * PATH_BYTES + 1 for one that could not be read. */
	__u16 lengths[2];
	__u32 unused;
	char paths[2 * PATH_BYTES];
};

/* A call entered: its number, when, and its arguments. */
struct entered {
	__u32 call;
	__u32 unused;
	__u64 time;
	__u64 args[6];
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct settings);
} settings SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, CALLS);
	__type(key, __u32);
	__type(value, struct call);
} calls SEC(".maps");

/* The group each recorded process belongs to, by thread group id. */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, __u32);
} groups SEC(".maps");

/* Threads in a recorded call, by thread id. Not an LRU map, which can evict
 * a call in flight before it is full when many CPUs add at once, losing it
 * unnoticed; a thread that exits in a call is removed as it exits. */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, struct entered);
} inflight SEC(".maps");

/* Where a call's event is put together, too large for the stack. */
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct call_event);
} scratch SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 64 * 1024 * 1024);
} events SEC(".maps");

/* Events the ring buffer or a map had no room for, which leave a recording
 * incomplete, by group: one recording's losses fail it alone. Group 0, which
 * no recording has and the helper adds as it loads, counts those no group
 * could be found room for, which fail every recording. */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, __u64);
} dropped SEC(".maps");

static __always_inline void count_drop(__u32 group)
{
	__u64 *count = bpf_map_lookup_elem(&dropped, &group);
	if (!count) {
		__u64 one = 1;
		if (!bpf_map_update_elem(&dropped, &group, &one, BPF_NOEXIST))
			return;
		/* Added by another CPU meanwhile, or no room for it. */
		count = bpf_map_lookup_elem(&dropped, &group);
	}
	if (!count) {
		__u32 unattributed = 0;
		count = bpf_map_lookup_elem(&dropped, &unattributed);
	}
	if (count)
		__sync_fetch_and_add(count, 1);
}

static __always_inline void fill(struct header *header, __u8 kind, __u32 group, __u16 call)
{
	__u64 pid_tgid = bpf_get_current_pid_tgid();
	header->kind = kind;
	header->unused = 0;
	header->call = call;
	header->group = group;
	header->tgid = pid_tgid >> 32;
	header->tid = (__u32)pid_tgid;
	header->time = bpf_ktime_get_boot_ns();
}

static __always_inline __u64 argument(struct pt_regs *regs, int index)
{
#if defined(__TARGET_ARCH_x86)
	switch (index) {
	case 0: return BPF_CORE_READ(regs, di);
	case 1: return BPF_CORE_READ(regs, si);
	case 2: return BPF_CORE_READ(regs, dx);
	case 3: return BPF_CORE_READ(regs, r10);
	case 4: return BPF_CORE_READ(regs, r8);
	default: return BPF_CORE_READ(regs, r9);
	}
#else
	if (index == 0)
		return BPF_CORE_READ(regs, orig_x0);
	return BPF_CORE_READ(regs, regs[index]);
#endif
}

/* Reads the path at `pointer` into `into`, saying how many bytes it took. */
static __always_inline __u16 read_path(char *into, __u64 pointer)
{
	if (!pointer)
		return 0;
	long length = bpf_probe_read_user_str(into, PATH_BYTES, (const void *)pointer);
	if (length < 0)
		return PATH_BYTES + 1;
	/* Kernels before 6.16 count an empty string's NUL only when there are
	 * other bytes: theirs is 0, where later kernels say 1. */
	if (length == 0) {
		into[0] = 0;
		return 1;
	}
	return (__u16)length;
}

/* A process asking to join a recording, with the run's secret. */
static __always_inline int join(struct pt_regs *regs, struct settings *config)
{
	if ((__u32)argument(regs, 0) != config->option || argument(regs, 1) != config->secret)
		return 0;
	__u32 group = (__u32)argument(regs, 2);
	__u32 tgid = bpf_get_current_pid_tgid() >> 32;
	/* A process that cannot join leaves its recording incomplete. */
	if (bpf_map_update_elem(&groups, &tgid, &group, BPF_ANY)) {
		count_drop(group);
		return 0;
	}
	struct begin_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
	if (!event) {
		count_drop(group);
		return 0;
	}
	fill(&event->header, BEGIN, group, 0);
	bpf_ringbuf_submit(event, 0);
	return 0;
}

SEC("tp_btf/sys_enter")
int BPF_PROG(on_enter, struct pt_regs *regs, long id)
{
	__u32 zero = 0;
	__u32 tgid = bpf_get_current_pid_tgid() >> 32;
	__u32 *group = bpf_map_lookup_elem(&groups, &tgid);
	if (!group) {
		struct settings *config = bpf_map_lookup_elem(&settings, &zero);
		if (config && id == config->prctl)
			return join(regs, config);
		return 0;
	}
	/* A 32-bit call numbers its calls and passes their arguments another
	 * way, which this program does not read: the recording is incomplete. */
#if defined(__TARGET_ARCH_x86)
	if (BPF_CORE_READ(regs, cs) == USER32_CS) {
#else
	if (BPF_CORE_READ(regs, pstate) & PSR_MODE32_BIT) {
#endif
		count_drop(*group);
		return 0;
	}
	if (id < 0 || id >= CALLS)
		return 0;
	__u32 number = (__u32)id;
	struct call *call = bpf_map_lookup_elem(&calls, &number);
	if (!call || !call->recorded)
		return 0;
	if (call->dup_commands_only) {
		__u64 command = argument(regs, 1);
		if (command != F_DUPFD && command != F_DUPFD_CLOEXEC)
			return 0;
	}
	struct entered entered = {
		.call = number,
		.time = bpf_ktime_get_boot_ns(),
	};
	for (int index = 0; index < 6; index++)
		entered.args[index] = argument(regs, index);
	__u32 tid = (__u32)bpf_get_current_pid_tgid();
	if (bpf_map_update_elem(&inflight, &tid, &entered, BPF_ANY))
		count_drop(*group);
	return 0;
}

SEC("tp_btf/sys_exit")
int BPF_PROG(on_exit, struct pt_regs *regs, long result)
{
	__u32 zero = 0;
	__u32 tid = (__u32)bpf_get_current_pid_tgid();
	struct entered *entered = bpf_map_lookup_elem(&inflight, &tid);
	if (!entered)
		return 0;
	/* As soon as can be: a later close of what it opened must enter after. */
	__u64 exited = bpf_ktime_get_boot_ns();
	__u32 tgid = bpf_get_current_pid_tgid() >> 32;
	__u32 *group = bpf_map_lookup_elem(&groups, &tgid);
	struct call_event *event = bpf_map_lookup_elem(&scratch, &zero);
	__u32 number = entered->call;
	struct call *call = bpf_map_lookup_elem(&calls, &number);
	if (!group || !event || !call) {
		bpf_map_delete_elem(&inflight, &tid);
		return 0;
	}
	fill(&event->header, CALL, *group, (__u16)number);
	event->header.time = entered->time;
	for (int index = 0; index < 6; index++)
		event->args[index] = entered->args[index];
	bpf_map_delete_elem(&inflight, &tid);
	event->result = result;
	event->exited = exited;
	if (call->flags_at < 6) {
		__u64 flags = 0;
		bpf_probe_read_user(&flags, sizeof(flags),
				    (const void *)event->args[call->flags_at & 7]);
		event->args[call->flags_at & 7] = flags;
	}
	__u16 first = 0, second = 0;
	if (call->paths[0] < 6)
		first = read_path(event->paths, event->args[call->paths[0] & 7]);
	/* The second path follows the first, which is at most PATH_BYTES. */
	__u32 offset = first > PATH_BYTES ? 0 : first;
	if (offset > PATH_BYTES)
		offset = PATH_BYTES;
	if (call->paths[1] < 6)
		second = read_path(event->paths + offset, event->args[call->paths[1] & 7]);
	event->lengths[0] = first;
	event->lengths[1] = second;
	event->unused = 0;
	__u32 size = sizeof(*event) - sizeof(event->paths);
	size += first > PATH_BYTES ? 0 : first;
	size += second > PATH_BYTES ? 0 : second;
	if (size > sizeof(*event))
		return 0;
	if (bpf_ringbuf_output(&events, event, size, 0))
		count_drop(event->header.group);
	return 0;
}

SEC("tp_btf/sched_process_fork")
int BPF_PROG(on_fork, struct task_struct *parent, struct task_struct *child)
{
	__u32 parent_tgid = BPF_CORE_READ(parent, tgid);
	__u32 child_tgid = BPF_CORE_READ(child, tgid);
	/* A new thread stays in its process's group. */
	if (child_tgid == parent_tgid)
		return 0;
	__u32 *group = bpf_map_lookup_elem(&groups, &parent_tgid);
	if (!group)
		return 0;
	__u32 joined = *group;
	/* A child that cannot join leaves its recording incomplete. */
	if (bpf_map_update_elem(&groups, &child_tgid, &joined, BPF_ANY)) {
		count_drop(joined);
		return 0;
	}
	struct fork_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
	if (!event) {
		count_drop(joined);
		return 0;
	}
	fill(&event->header, FORK, joined, 0);
	event->header.tgid = parent_tgid;
	event->child = child_tgid;
	event->unused = 0;
	bpf_ringbuf_submit(event, 0);
	return 0;
}

SEC("tp_btf/sched_process_exit")
int BPF_PROG(on_process_exit, struct task_struct *task)
{
	/* A thread killed in a recorded call never returns from it. */
	__u32 tid = BPF_CORE_READ(task, pid);
	bpf_map_delete_elem(&inflight, &tid);
	/* The process leaves its group once its last thread exits, whichever
	 * thread that is: the kernel counts a thread out of `live` before this
	 * tracepoint. Two threads exiting at once may both see none left; the
	 * one that removes the process says so. */
	if (BPF_CORE_READ(task, signal, live.counter) != 0)
		return 0;
	__u32 tgid = BPF_CORE_READ(task, tgid);
	__u32 *group = bpf_map_lookup_elem(&groups, &tgid);
	if (!group)
		return 0;
	__u32 left = *group;
	if (bpf_map_delete_elem(&groups, &tgid))
		return 0;
	struct exit_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
	if (!event) {
		count_drop(left);
		return 0;
	}
	fill(&event->header, EXIT, left, 0);
	event->header.tgid = tgid;
	bpf_ringbuf_submit(event, 0);
	return 0;
}
