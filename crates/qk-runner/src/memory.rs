//! The memory each running task uses: that of its commands and every process
//! under them, summed, so that a tool's workers count with it. On macOS it is
//! each process's physical footprint, which Activity Monitor shows as its
//! memory: it counts memory the system has compressed or swapped out, and
//! not the shared libraries every process maps. Elsewhere it is the resident
//! memory.

use std::collections::{BTreeMap, BTreeSet};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, ThreadKind};

/// Each task's memory, in bytes, from the process ids of its commands, in
/// one pass over the machine's processes.
pub fn sample<'a>(
    system: &mut System,
    roots: &BTreeMap<&'a str, BTreeSet<u32>>,
) -> BTreeMap<&'a str, u64> {
    let kind = ProcessRefreshKind::nothing();
    #[cfg(not(target_os = "macos"))]
    let kind = kind.with_memory();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let processes: Vec<(u32, Option<u32>, u64)> = system
        .processes()
        .values()
        // Threads share their process's memory.
        .filter(|process| process.thread_kind() != Some(ThreadKind::Userland))
        .map(|process| {
            (
                process.pid().as_u32(),
                process.parent().map(Pid::as_u32),
                memory(process),
            )
        })
        .collect();
    trees(&processes, roots)
}

#[cfg(target_os = "macos")]
fn memory(process: &sysinfo::Process) -> u64 {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
    // SAFETY: the buffer is a rusage_info_v0, the flavor asked for.
    let read = unsafe {
        libc::proc_pid_rusage(
            process.pid().as_u32() as libc::c_int,
            libc::RUSAGE_INFO_V0,
            info.as_mut_ptr().cast(),
        )
    };
    if read == 0 {
        // SAFETY: proc_pid_rusage filled it in.
        unsafe { info.assume_init() }.ri_phys_footprint
    } else {
        // Gone since the process list was read, or not the user's to read.
        0
    }
}

#[cfg(not(target_os = "macos"))]
fn memory(process: &sysinfo::Process) -> u64 {
    process.memory()
}

/// The memory of the processes under each set of roots, roots included, from
/// each process's id, parent and memory.
fn trees<'a>(
    processes: &[(u32, Option<u32>, u64)],
    roots: &BTreeMap<&'a str, BTreeSet<u32>>,
) -> BTreeMap<&'a str, u64> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut memory = BTreeMap::new();
    for &(pid, parent, bytes) in processes {
        memory.insert(pid, bytes);
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(pid);
        }
    }
    roots
        .iter()
        .map(|(task, roots)| {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<u32> = roots.iter().copied().collect();
            let mut total = 0u64;
            while let Some(pid) = stack.pop() {
                if !seen.insert(pid) {
                    continue;
                }
                total = total.saturating_add(memory.get(&pid).copied().unwrap_or(0));
                stack.extend(children.get(&pid).into_iter().flatten());
            }
            (*task, total)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::trees;

    #[test]
    fn sums_each_task_with_its_descendants() {
        // 1 is qk; 10 and 20 run tasks, 11 and 12 are 10's workers and 21 is
        // a worker of a worker of 20; 30 is unrelated.
        let processes = [
            (1, None, 50),
            (10, Some(1), 5),
            (11, Some(10), 100),
            (12, Some(10), 200),
            (20, Some(1), 7),
            (22, Some(20), 0),
            (21, Some(22), 1_000),
            (30, Some(1), 9_999),
        ];
        let roots = BTreeMap::from([
            ("app:test", BTreeSet::from([10])),
            ("app:build", BTreeSet::from([20])),
            // Exited between the scheduler reading it and the sample.
            ("app:lint", BTreeSet::from([40])),
        ]);
        assert_eq!(
            trees(&processes, &roots),
            BTreeMap::from([("app:test", 305), ("app:build", 1_007), ("app:lint", 0)])
        );
    }
}
