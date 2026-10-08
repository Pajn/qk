//! The memory each running task uses: that of its commands and every process
//! under them, summed, so that a tool's workers count with it. On macOS it is
//! each process's physical footprint, which Activity Monitor shows as its
//! memory: it counts memory the system has compressed or swapped out, and
//! not the shared libraries every process maps. Elsewhere it is the resident
//! memory.
//!
//! A sample can miss a spike between samples. On macOS the system also keeps
//! the most each process has used in its life, so a task's peak is at least
//! that of its largest process.

use std::collections::{BTreeMap, BTreeSet};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, ThreadKind};

use crate::Expected;

/// What a task's processes use, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Used {
    /// Together, now.
    pub now: u64,
    /// The most they can be known to have used: together now, or what one of
    /// them used at its peak.
    pub peak: u64,
}

/// Each task's memory from the process ids of its commands, in one pass over
/// the machine's processes.
pub fn sample<'a>(
    system: &mut System,
    roots: &BTreeMap<&'a str, BTreeSet<u32>>,
) -> BTreeMap<&'a str, Used> {
    let kind = ProcessRefreshKind::nothing();
    #[cfg(not(target_os = "macos"))]
    let kind = kind.with_memory();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let processes: Vec<(u32, Option<u32>, Used)> = system
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

/// The memory each task is expected to use, from its id and target: the most
/// it used in its recent runs, else the median of what the run's other tasks
/// of its target are expected to use. Tasks of a target none of which has
/// been measured have no expectation, and start without waiting for memory.
pub fn expected<'a>(
    tasks: impl Iterator<Item = (&'a str, &'a str)> + Clone,
    expected: &BTreeMap<String, Expected>,
) -> BTreeMap<&'a str, u64> {
    let known = |id: &str| expected.get(id).and_then(|expected| expected.memory);
    let mut by_target: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for (id, target) in tasks.clone() {
        if let Some(memory) = known(id) {
            by_target.entry(target).or_default().push(memory);
        }
    }
    let median: BTreeMap<&str, u64> = by_target
        .into_iter()
        .map(|(target, mut memory)| {
            memory.sort_unstable();
            (target, memory[memory.len() / 2])
        })
        .collect();
    tasks
        .filter_map(|(id, target)| {
            let memory = known(id).or_else(|| median.get(target).copied())?;
            Some((id, memory))
        })
        .collect()
}

/// Whether a task expected to use `need` fits in what is `free`, less a
/// `reserve` kept for the rest of the machine and the `growth` the tasks
/// already running are still expected to add.
pub fn fits(need: u64, free: u64, growth: u64, reserve: u64) -> bool {
    free.saturating_sub(reserve).saturating_sub(growth) >= need
}

#[cfg(target_os = "macos")]
fn memory(process: &sysinfo::Process) -> Used {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: the buffer is a rusage_info_v4, the flavor asked for.
    let read = unsafe {
        libc::proc_pid_rusage(
            process.pid().as_u32() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast(),
        )
    };
    if read == 0 {
        // SAFETY: proc_pid_rusage filled it in.
        let info = unsafe { info.assume_init() };
        Used {
            now: info.ri_phys_footprint,
            peak: info
                .ri_lifetime_max_phys_footprint
                .max(info.ri_phys_footprint),
        }
    } else {
        // Gone since the process list was read, or not the user's to read.
        Used::default()
    }
}

#[cfg(not(target_os = "macos"))]
fn memory(process: &sysinfo::Process) -> Used {
    let now = process.memory();
    Used { now, peak: now }
}

/// The memory of the processes under each set of roots, roots included, from
/// each process's id, parent and memory.
fn trees<'a>(
    processes: &[(u32, Option<u32>, Used)],
    roots: &BTreeMap<&'a str, BTreeSet<u32>>,
) -> BTreeMap<&'a str, Used> {
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
            let mut used = Used::default();
            let mut largest = 0;
            while let Some(pid) = stack.pop() {
                if !seen.insert(pid) {
                    continue;
                }
                let process = memory.get(&pid).copied().unwrap_or_default();
                used.now = used.now.saturating_add(process.now);
                largest = largest.max(process.peak);
                stack.extend(children.get(&pid).into_iter().flatten());
            }
            used.peak = used.now.max(largest);
            (*task, used)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{Used, expected, fits, trees};
    use crate::Expected;

    #[test]
    fn sums_each_task_with_its_descendants() {
        // 1 is qk; 10 and 20 run tasks, 11 and 12 are 10's workers and 21 is
        // a worker of a worker of 20; 30 is unrelated.
        let at = |now| Used { now, peak: now };
        let processes = [
            (1, None, at(50)),
            (10, Some(1), at(5)),
            (11, Some(10), at(100)),
            (12, Some(10), at(200)),
            (20, Some(1), at(7)),
            (22, Some(20), at(0)),
            (21, Some(22), at(1_000)),
            (30, Some(1), at(9_999)),
        ];
        let roots = BTreeMap::from([
            ("app:test", BTreeSet::from([10])),
            ("app:build", BTreeSet::from([20])),
            // Exited between the scheduler reading it and the sample.
            ("app:lint", BTreeSet::from([40])),
        ]);
        assert_eq!(
            trees(&processes, &roots),
            BTreeMap::from([
                ("app:test", at(305)),
                ("app:build", at(1_007)),
                ("app:lint", at(0))
            ])
        );
    }

    #[test]
    fn a_task_peaked_at_least_as_high_as_its_largest_process() {
        let processes = [
            (10, None, Used { now: 5, peak: 5 }),
            // Shrunk since it peaked between samples.
            (
                11,
                Some(10),
                Used {
                    now: 100,
                    peak: 3_000,
                },
            ),
            (
                12,
                Some(10),
                Used {
                    now: 200,
                    peak: 250,
                },
            ),
        ];
        let roots = BTreeMap::from([("app:tsc", BTreeSet::from([10]))]);
        assert_eq!(
            trees(&processes, &roots)["app:tsc"],
            Used {
                now: 305,
                peak: 3_000
            }
        );
    }

    #[test]
    fn a_task_without_history_is_expected_to_use_the_median_of_its_target() {
        let history = |memory| Expected {
            millis: 1,
            threads: 1,
            memory,
        };
        let known = BTreeMap::from([
            ("a:tsc".to_owned(), history(Some(100))),
            ("b:tsc".to_owned(), history(Some(4_000))),
            ("c:tsc".to_owned(), history(Some(200))),
            // Ran, but before memory was measured.
            ("d:tsc".to_owned(), history(None)),
            ("a:lint".to_owned(), history(None)),
        ]);
        let tasks = [
            ("a:tsc", "tsc"),
            ("b:tsc", "tsc"),
            ("c:tsc", "tsc"),
            ("d:tsc", "tsc"),
            ("e:tsc", "tsc"),
            ("a:lint", "lint"),
        ];
        assert_eq!(
            expected(tasks.into_iter(), &known),
            BTreeMap::from([
                ("a:tsc", 100),
                ("b:tsc", 4_000),
                ("c:tsc", 200),
                ("d:tsc", 200),
                ("e:tsc", 200),
            ])
        );
    }

    #[test]
    fn a_task_fits_in_what_is_free_after_the_reserve_and_growth() {
        assert!(fits(4, 10, 3, 3));
        assert!(!fits(5, 10, 3, 3));
        assert!(fits(0, 1, 3, 3));
        assert!(!fits(1, 1, 3, 3));
    }
}
