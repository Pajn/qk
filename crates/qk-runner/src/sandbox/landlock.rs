//! Landlock rules for a task: its policy as the rights beneath each path,
//! through the executor's system calls.
//!
//! A rule set allows rights beneath paths, and refuses every right it
//! handles elsewhere. Rights cannot be taken back beneath an allowed path,
//! so everything outside the workspace is allowed by allowing each sibling
//! along the way down to it. Landlock holds paths that exist rather than
//! patterns, so the `node_modules` directories and dotenv files a policy
//! grants are those there as the run starts.

use std::path::{Path, PathBuf};

use qk_executor::landlock::{Open, Rights};

use super::policy::{Policy, Reach, Scope, is_dotenv};

/// What a policy's patterns name in the workspace as the run starts.
#[derive(Debug, Default)]
pub struct Expansion {
    /// Every `node_modules` directory.
    pub node_modules: Vec<PathBuf>,
    /// Every dotenv file.
    pub dotenv: Vec<PathBuf>,
}

impl Expansion {
    /// The workspace at `root`, without looking inside `node_modules` or
    /// `.git`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn scan(root: &Path) -> Self {
        let mut expansion = Self::default();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    match name.as_ref() {
                        "node_modules" => expansion.node_modules.push(entry.path()),
                        ".git" => {}
                        _ => pending.push(entry.path()),
                    }
                } else if is_dotenv(&name) {
                    expansion.dotenv.push(entry.path());
                }
            }
        }
        expansion
    }

    /// Adds the `node_modules` directories in `made` that it lacks.
    pub fn add_node_modules(&mut self, made: &[PathBuf]) {
        for path in made {
            if !self.node_modules.contains(path) {
                self.node_modules.push(path.clone());
            }
        }
    }
}

/// What `policy` grants, as the rights beneath each path, with whether to
/// reach it through no link: listing anywhere, and in the workspace reading
/// and writing as its grants say. Installed packages' manifests, which it
/// reports, are left to `node_modules`, as Landlock can only refuse.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn rights(policy: &Policy, expansion: &Expansion) -> Vec<(PathBuf, Rights, Open)> {
    let open = |unlinked: bool| {
        if unlinked {
            Open::Unlinked
        } else {
            Open::Following
        }
    };
    let mut rights = Vec::new();
    for grant in policy.grants() {
        let (tree, file) = if grant.writes() {
            (Rights::ALL, Rights::ALL)
        } else {
            (Rights::READ_TREE, Rights::READ)
        };
        match grant.scope {
            Scope::Tree(path) => rights.push((path.to_owned(), tree, open(grant.unlinked))),
            Scope::File(path) => rights.push((path.to_owned(), file, open(grant.unlinked))),
            // A task may replace a `node_modules` it can write with a link, or
            // with a file, after the run recorded it: each is allowed only as
            // a directory reached through no link, so neither leads anywhere.
            Scope::NodeModules => rights.extend(
                expansion
                    .node_modules
                    .iter()
                    .map(|path| (path.clone(), tree, Open::Directory)),
            ),
            Scope::Dotenv => rights.extend(
                expansion
                    .dotenv
                    .iter()
                    .map(|path| (path.clone(), file, open(policy.is_unlinked(path)))),
            ),
            Scope::Directories => {
                rights.push((PathBuf::from("/"), Rights::READ_DIR, Open::Following))
            }
            Scope::Manifests(_) => {}
        }
    }
    rights
}

/// The directories in the workspace bound through a task's overlay, so that
/// what it writes there reaches the workspace directly. A link, which binding
/// would follow, or anything else that is no directory, is left out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn passthrough(policy: &Policy, expansion: &Expansion) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for grant in policy.grants() {
        if !matches!(grant.reach, Reach::Through | Reach::ThroughKept) {
            continue;
        }
        match grant.scope {
            Scope::Tree(path) => paths.push(path.to_owned()),
            Scope::NodeModules => paths.extend(expansion.node_modules.iter().cloned()),
            _ => {}
        }
    }
    paths.retain(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()));
    paths
}

/// Where a task over an overlay may write: what it wrote there is applied
/// to the workspace as it ends, and the rest is not. Every `node_modules` is
/// kept by name, so one the task makes, as a package manager installing on
/// demand does, is kept as one there before.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn keep(policy: &Policy) -> qk_executor::Keep {
    let mut keep = qk_executor::Keep::default();
    for grant in policy.grants() {
        if !matches!(grant.reach, Reach::Kept | Reach::ThroughKept) {
            continue;
        }
        match grant.scope {
            Scope::Tree(path) => keep.paths.push(path.to_owned()),
            Scope::NodeModules => keep.names.push("node_modules".into()),
            _ => {}
        }
    }
    keep
}

#[cfg(not(target_os = "linux"))]
pub fn abi() -> anyhow::Result<u32> {
    anyhow::bail!("Landlock is Linux's")
}

#[cfg(target_os = "linux")]
pub use system::{abi, overlay, ruleset};

#[cfg(target_os = "linux")]
mod system {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};
    use qk_executor::landlock::{self, Open, Rights};

    use super::{Expansion, Policy, keep, passthrough, rights};

    /// The Landlock ABI the kernel offers.
    pub fn abi() -> Result<u32> {
        landlock::abi().context("Landlock is not available")
    }

    struct Rules {
        fd: OwnedFd,
        handled: Rights,
    }

    impl Rules {
        fn new() -> Result<Self> {
            let handled = Rights::handled(abi()?);
            let fd =
                landlock::create_ruleset(handled).context("cannot create a Landlock rule set")?;
            Ok(Self { fd, handled })
        }

        /// Allows `rights` beneath `path`, or on it for a file; a path that does
        /// not exist is skipped.
        fn allow(&self, path: &Path, rights: Rights) -> Result<()> {
            let Some(fd) = open_path(path)? else {
                return Ok(());
            };
            self.allow_open(&fd, path, rights)
        }

        /// As [`Rules::allow`], for `path` beneath `root` opened as `open`
        /// says, through no link: one where a link now stands, in any part of
        /// the way down from `root`, or for a directory anything else, is
        /// skipped as one that does not exist.
        fn allow_unlinked(
            &self,
            root: &Path,
            path: &Path,
            rights: Rights,
            open: Open,
        ) -> Result<()> {
            let Ok(relative) = path.strip_prefix(root) else {
                return Ok(());
            };
            let root = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(root)
                .with_context(|| format!("cannot open {}", root.display()))?;
            let name = open
                .name(relative)
                .with_context(|| format!("cannot open {}", path.display()))?;
            let fd = match landlock::open_beneath(root.as_raw_fd(), &name) {
                Ok(fd) => fd,
                Err(error) => {
                    return match error.raw_os_error() {
                        Some(libc::ENOENT | libc::ELOOP | libc::EXDEV | libc::ENOTDIR) => Ok(()),
                        // Where openat2 is refused, as a seccomp filter may, the
                        // path cannot be opened without following a link that may
                        // stand in it, so the task is not sandboxed and enforce
                        // fails it.
                        Some(libc::ENOSYS) => Err(error).with_context(|| {
                            format!("cannot open {} without following links", path.display())
                        }),
                        _ => Err(error).with_context(|| format!("cannot open {}", path.display())),
                    };
                }
            };
            self.allow_open(&fd, path, rights)
        }

        /// Allows `rights` beneath the opened `fd`, or on it for a file.
        fn allow_open(&self, fd: &OwnedFd, path: &Path, rights: Rights) -> Result<()> {
            landlock::add_rule(self.fd.as_raw_fd(), fd.as_raw_fd(), rights, self.handled)
                .with_context(|| format!("cannot allow {} in the sandbox", path.display()))
        }
    }

    /// Opens `path` for a rule, or `None` where it does not exist.
    fn open_path(path: &Path) -> Result<Option<OwnedFd>> {
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => Ok(Some(file.into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("cannot open {}", path.display())),
        }
    }

    /// Everything outside the workspace at `root`: each sibling on the way
    /// down to it.
    fn outside(root: &Path) -> Result<Vec<PathBuf>> {
        let mut siblings = Vec::new();
        let mut ancestor = PathBuf::from("/");
        for component in root.components().skip(1) {
            let next = ancestor.join(component);
            let entries = std::fs::read_dir(&ancestor)
                .with_context(|| format!("cannot list {}", ancestor.display()))?;
            for entry in entries.flatten() {
                if entry.path() != next {
                    siblings.push(entry.path());
                }
            }
            ancestor = next;
        }
        if ancestor != root {
            bail!("cannot walk down to {}", root.display());
        }
        Ok(siblings)
    }

    /// A task's rule set: everything outside the workspace, and its
    /// [`rights`].
    pub fn ruleset(policy: &Policy, expansion: &Expansion) -> Result<OwnedFd> {
        let rules = Rules::new()?;
        let root = policy.root();
        for path in outside(root)? {
            rules.allow(&path, Rights::ALL)?;
        }
        for (path, rights, open) in rights(policy, expansion) {
            if open != Open::Following {
                rules.allow_unlinked(root, &path, rights, open)?;
            } else {
                rules.allow(&path, rights)?;
            }
        }
        Ok(rules.fd)
    }

    /// A task's rule set over an overlay, as [`ruleset`] builds it, but for
    /// a command to build in its own namespace once the overlay is mounted:
    /// paths outside the workspace are opened now, as the overlay leaves
    /// them as they are, and those in it are listed for the command to open,
    /// as the overlay's. Writes anywhere in the workspace reach the overlay,
    /// to be judged as the task ends, so the workspace may be written
    /// everywhere but read only where `ruleset` would allow it. With it, the
    /// paths bound through the overlay and those whose writes are kept.
    pub fn overlay(
        policy: &Policy,
        expansion: &Expansion,
    ) -> Result<(qk_executor::OverlayRules, Vec<PathBuf>, qk_executor::Keep)> {
        let handled = Rights::handled(abi()?);
        let root = policy.root();
        let mut outside_fds = Vec::new();
        let mut inside = Vec::new();
        let mut add = |path: &Path, rights: Rights, open: Open| -> Result<()> {
            if path.starts_with(root) {
                inside.push((path.to_owned(), rights, open));
            } else if let Some(fd) = open_path(path)? {
                outside_fds.push((fd, rights));
            }
            Ok(())
        };
        for path in outside(root)? {
            add(&path, Rights::ALL, Open::Following)?;
        }
        for (path, rights, open) in rights(policy, expansion) {
            add(&path, rights, open)?;
        }
        let rules = qk_executor::OverlayRules {
            handled,
            outside: outside_fds,
            workspace: handled & !Rights::READ_TREE,
            inside,
        };
        Ok((rules, passthrough(policy, expansion), keep(policy)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::sandbox::{Mode, Plan};

    /// A workspace holding `.git`, qk's state in `.qk`, two `node_modules`
    /// and two dotenv files, and an enforced policy reading and writing in
    /// it.
    fn workspace() -> (tempfile::TempDir, PathBuf, Policy) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for directory in [
            ".git",
            ".qk",
            "node_modules/a",
            "apps/app/node_modules",
            "tmp/app",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for file in [".env", "apps/app/.env.local", "node_modules/a/.env"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let plan = Plan {
            read_directories: vec![root.join("libs/lib")],
            read_files: vec![
                root.join("package.json"),
                root.join("apps/app/src/main.ts"),
                root.join("fixtures/data.json"),
            ],
            read_trees: [root.join("libs/dep/dist"), root.join("fixtures/cache")].into(),
            unlinked: [root.join("fixtures/data.json"), root.join("fixtures/cache")].into(),
            write_trees: [root.join("apps/app/dist"), root.join("tmp/app")].into(),
            scratch: [root.join("tmp/app")].into(),
            packages: None,
        };
        let policy = Policy::new(Mode::Enforce, &root, &root.join(".qk"), plan);
        (temp, root, policy)
    }

    /// Every `node_modules` and dotenv file, but none in `node_modules` or
    /// `.git`, and not a `.git` file as a nested checkout keeps.
    #[test]
    fn expands_patterns_into_what_the_workspace_holds() {
        let (_temp, root, _) = workspace();
        std::fs::write(root.join(".git/.env"), "").unwrap();
        std::fs::create_dir_all(root.join("vendor/checkout")).unwrap();
        std::fs::write(root.join("vendor/checkout/.git"), "").unwrap();
        let mut expansion = Expansion::scan(&root);
        expansion.node_modules.sort();
        expansion.dotenv.sort();
        assert_eq!(
            expansion.node_modules,
            [
                root.join("apps/app/node_modules"),
                root.join("node_modules")
            ]
        );
        assert_eq!(
            expansion.dotenv,
            [root.join(".env"), root.join("apps/app/.env.local")]
        );
    }

    /// The rights granted beneath each path, and how each is
    /// opened: Landlock unions its rules, so their order and
    /// repetition do not matter.
    #[test]
    fn grants_rights_beneath_the_policys_paths() {
        let (_temp, root, policy) = workspace();
        let expansion = Expansion::scan(&root);
        let mut granted: BTreeMap<(PathBuf, Open), Rights> = BTreeMap::new();
        for (path, rights, open) in rights(&policy, &expansion) {
            let path = path
                .strip_prefix(&root)
                .map_or(path.clone(), Path::to_owned);
            *granted.entry((path, open)).or_default() |= rights;
        }
        let expected: BTreeMap<(PathBuf, Open), Rights> = [
            ("/", Open::Following, Rights::READ_DIR),
            ("libs/lib", Open::Following, Rights::READ_TREE),
            ("package.json", Open::Following, Rights::READ),
            ("apps/app/src/main.ts", Open::Following, Rights::READ),
            ("fixtures/data.json", Open::Unlinked, Rights::READ),
            (".env", Open::Following, Rights::READ),
            ("apps/app/.env.local", Open::Following, Rights::READ),
            ("libs/dep/dist", Open::Following, Rights::READ_TREE),
            ("fixtures/cache", Open::Unlinked, Rights::READ_TREE),
            ("apps/app/dist", Open::Following, Rights::ALL),
            ("tmp/app", Open::Following, Rights::ALL),
            ("node_modules", Open::Directory, Rights::ALL),
            ("apps/app/node_modules", Open::Directory, Rights::ALL),
            (".git", Open::Following, Rights::ALL),
            (".qk", Open::Following, Rights::ALL),
        ]
        .into_iter()
        .map(|(path, open, rights)| ((PathBuf::from(path), open), rights))
        .collect();
        assert_eq!(granted, expected);
    }

    /// Over an overlay, `.git`, every `node_modules`, qk's state and the
    /// task's scratch directories pass through; its outputs, warm paths,
    /// scratch directories, qk's state and every `node_modules` are kept.
    #[test]
    fn binds_through_and_keeps_what_the_policy_says() {
        let (_temp, root, policy) = workspace();
        let expansion = Expansion::scan(&root);
        let mut through = passthrough(&policy, &expansion);
        // A recorded `node_modules` an earlier task replaced with a link, or
        // with a file, is not bound: binding would follow the link.
        let mut replaced = Expansion::default();
        replaced.add_node_modules(&[
            root.join("linked/node_modules"),
            root.join("filed/node_modules"),
        ]);
        std::fs::create_dir_all(root.join("linked")).unwrap();
        std::fs::create_dir_all(root.join("filed")).unwrap();
        std::os::unix::fs::symlink(root.join("tmp"), root.join("linked/node_modules")).unwrap();
        std::fs::write(root.join("filed/node_modules"), "").unwrap();
        let swapped = passthrough(&policy, &replaced);
        assert!(
            !swapped.iter().any(|path| path.ends_with("node_modules")),
            "{swapped:?}"
        );
        through.sort();
        assert_eq!(
            through,
            [
                ".git",
                ".qk",
                "apps/app/node_modules",
                "node_modules",
                "tmp/app"
            ]
            .map(|path| root.join(path))
        );
        let mut kept = keep(&policy);
        kept.paths.sort();
        assert_eq!(
            kept.paths,
            [".qk", "apps/app/dist", "tmp/app"].map(|path| root.join(path))
        );
        // Every `node_modules`, by name: one the task makes is kept too.
        assert_eq!(kept.names, ["node_modules"]);
        for (path, kept_there) in [
            ("pkg/node_modules/x/a", true),
            ("node_modules/a/index.js", true),
            ("pkg/stray.js", false),
            ("pkg/node_modules_backup/a", false),
        ] {
            assert_eq!(kept.allows(&root, &root.join(path)), kept_there, "{path}");
        }
        assert!(!kept.allows(&root, Path::new("/elsewhere/node_modules/a")));
        // Those made by earlier tasks are added once each.
        let mut grown = Expansion::default();
        grown.add_node_modules(&[root.join("pkg/node_modules"), root.join("pkg/node_modules")]);
        assert_eq!(grown.node_modules, [root.join("pkg/node_modules")]);
        // In `.git`, or outside the workspace, qk's state is kept but needs no
        // binding of its own; one that does not exist is not bound.
        for state in [
            root.join(".git/qk"),
            PathBuf::from("/elsewhere/qk"),
            root.join(".qk-missing"),
        ] {
            let elsewhere = Policy::new(Mode::Enforce, &root, &state, Plan::default());
            let through = passthrough(&elsewhere, &expansion);
            assert!(!through.contains(&state), "{through:?}");
            assert_eq!(keep(&elsewhere).paths, [state]);
        }
    }
}
