//! The memory the control groups this process is in allow it, on Linux. In a
//! container the limit that ends a run is its group's, which can be well
//! below what the machine has, while `/proc/meminfo` still describes the
//! machine. Only the unified hierarchy (cgroup v2) is read.

use std::path::{Path, PathBuf};

/// What the tightest of a process's control groups allows, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    /// The most the group may use.
    pub total: u64,
    /// What is left of that, counting what the kernel can reclaim as left.
    pub free: u64,
}

/// The control groups a process is in, as their directories.
#[derive(Clone, Debug, Default)]
pub struct Groups(Vec<PathBuf>);

impl Groups {
    /// Those of this process; none where there is no unified hierarchy.
    pub fn of_this_process() -> Self {
        std::fs::read_to_string("/proc/self/cgroup")
            .map(|membership| Self::under(Path::new("/sys/fs/cgroup"), &membership))
            .unwrap_or_default()
    }

    /// The group `membership` names, as `/proc/<pid>/cgroup` gives it, and
    /// every group that one is in, below the hierarchy mounted at `root`.
    fn under(root: &Path, membership: &str) -> Self {
        let Some(path) = membership.lines().find_map(|line| line.strip_prefix("0::")) else {
            return Self::default();
        };
        let mut directory = root.to_path_buf();
        let mut directories = vec![directory.clone()];
        // `..` names a group outside the namespace's root, which is not
        // mounted here.
        for name in path.split('/').filter(|name| !matches!(*name, "" | "..")) {
            directory.push(name);
            directories.push(directory.clone());
        }
        Self(directories)
    }

    /// The least any of the groups allows and has left; none when no group
    /// limits memory. A limit on an outer group binds the inner ones too.
    pub fn limit(&self) -> Option<Limit> {
        self.0
            .iter()
            .filter_map(|directory| limit_of(directory))
            .reduce(|tightest, limit| Limit {
                total: tightest.total.min(limit.total),
                free: tightest.free.min(limit.free),
            })
    }
}

fn limit_of(directory: &Path) -> Option<Limit> {
    let read = |file: &str| std::fs::read_to_string(directory.join(file)).ok();
    // `max` where the group sets no limit; the root group has no such file.
    let total: u64 = read("memory.max")?.trim().parse().ok()?;
    let used: u64 = read("memory.current")?.trim().parse().ok()?;
    // What the group is charged for includes cached file data, which the
    // kernel gives up before the group runs out, as the machine's own
    // available memory counts it.
    let reclaimable = read("memory.stat")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (key, bytes) = line.split_once(' ')?;
            matches!(key, "inactive_file" | "active_file" | "slab_reclaimable")
                .then(|| bytes.trim().parse::<u64>().ok())?
        })
        .fold(0u64, u64::saturating_add);
    Some(Limit {
        total,
        free: total.saturating_sub(used.saturating_sub(reclaimable)),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Groups, Limit};

    fn group(directory: &Path, max: &str, current: u64, stat: &str) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("memory.max"), format!("{max}\n")).unwrap();
        std::fs::write(directory.join("memory.current"), format!("{current}\n")).unwrap();
        std::fs::write(directory.join("memory.stat"), stat).unwrap();
    }

    #[test]
    fn a_group_allows_what_its_limit_leaves() {
        let root = tempfile::tempdir().unwrap();
        group(root.path(), "1000", 700, "anon 700\n");
        assert_eq!(
            Groups::under(root.path(), "0::/\n").limit(),
            Some(Limit {
                total: 1_000,
                free: 300
            })
        );
    }

    #[test]
    fn cached_file_data_counts_as_left() {
        let root = tempfile::tempdir().unwrap();
        group(
            root.path(),
            "1000",
            900,
            "anon 300\nfile 600\ninactive_file 350\nactive_file 200\nshmem 50\nslab_reclaimable 20\nslab_unreclaimable 30\n",
        );
        assert_eq!(
            Groups::under(root.path(), "0::/\n").limit(),
            Some(Limit {
                total: 1_000,
                free: 670
            })
        );
    }

    #[test]
    fn the_tightest_of_a_group_and_those_it_is_in_binds() {
        let root = tempfile::tempdir().unwrap();
        // The root group has no limit file at all.
        std::fs::create_dir_all(root.path()).unwrap();
        group(&root.path().join("outer"), "1000", 900, "");
        group(&root.path().join("outer/inner"), "400", 100, "");
        assert_eq!(
            Groups::under(root.path(), "0::/outer/inner\n").limit(),
            Some(Limit {
                total: 400,
                free: 100
            })
        );
    }

    #[test]
    fn groups_without_a_limit_allow_what_the_machine_has() {
        let root = tempfile::tempdir().unwrap();
        group(&root.path().join("user"), "max", 900, "");
        assert_eq!(Groups::under(root.path(), "0::/user\n").limit(), None);
        // No unified hierarchy: only the per-controller ones of cgroup v1.
        assert_eq!(Groups::under(root.path(), "5:memory:/user\n").limit(), None);
    }

    #[test]
    fn a_group_outside_the_mounted_hierarchy_is_read_from_its_root() {
        let root = tempfile::tempdir().unwrap();
        group(root.path(), "500", 100, "");
        assert_eq!(
            Groups::under(root.path(), "0::/../..\n").limit(),
            Some(Limit {
                total: 500,
                free: 400
            })
        );
    }
}
