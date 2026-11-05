//! What a task may do in the workspace, as one list of grants: what its
//! Seatbelt profile allows, what its Landlock rules and overlay let through,
//! and what a Linux audit holds its recording against.
//!
//! Beyond its plan, every task may list any directory and use
//! `node_modules` and qk's own state; enforce also lets it read and write
//! `.git`, which audit reports. Audit reports reading an installed package's
//! manifest whose package its key leaves out.

use std::path::{Path, PathBuf};

use super::{Mode, Packages, Plan};

/// What a task does to a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    Read,
    Write,
}

/// What becomes of an access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Allow,
    /// Allowed, and reported.
    Report,
    Refuse,
}

/// Where a grant reaches in the workspace.
#[derive(Clone, Copy, Debug)]
pub(super) enum Scope<'a> {
    /// A directory and all beneath it, or a file.
    Tree(&'a Path),
    /// A file.
    File(&'a Path),
    /// Every `node_modules`, at any depth.
    NodeModules,
    /// Every directory, to list it: what a listing finds is not read.
    Directories,
    /// Audit: the `package.json` of each installed package, read to find
    /// one its key leaves out, reported again after `node_modules` is
    /// allowed. Seatbelt names the store directories `packages` lists, to
    /// keep the log its reports pass through quiet; Linux judges every one,
    /// and the findings leave out those of packages the key covers alike.
    Manifests(&'a Packages),
}

/// How writes beneath a grant reach the workspace over an overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    /// It grants no writes.
    ReadOnly,
    /// They land in the overlay, and are kept as the task ends.
    Kept,
    /// The path is bound through the overlay, so they reach it directly.
    Through,
    /// Bound through where it lies as the task starts, and kept where the
    /// task makes it.
    ThroughKept,
}

/// One thing a task may do.
#[derive(Clone, Copy, Debug)]
pub(super) struct Grant<'a> {
    pub scope: Scope<'a>,
    pub reach: Reach,
    /// Whether the path holds only while no link stands where it was found
    /// as the run started: Landlock, which follows links, allows it through
    /// none, so a link an earlier task put in its place leads nowhere new.
    pub unlinked: bool,
}

impl Grant<'_> {
    fn new(scope: Scope<'_>, reach: Reach) -> Grant<'_> {
        Grant {
            scope,
            reach,
            unlinked: false,
        }
    }

    pub(super) fn writes(&self) -> bool {
        self.reach != Reach::ReadOnly
    }

    /// What it makes of an access it covers.
    pub(super) fn verdict(&self) -> Verdict {
        match self.scope {
            Scope::Manifests(_) => Verdict::Report,
            _ => Verdict::Allow,
        }
    }

    /// Whether it covers `access` to `path`, absolute, beneath `root`.
    fn covers(&self, root: &Path, path: &Path, access: Access, is_dir: bool) -> bool {
        if access == Access::Write && !self.writes() {
            return false;
        }
        let name = || path.file_name().and_then(|name| name.to_str());
        let in_node_modules = || {
            path.strip_prefix(root).is_ok_and(|relative| {
                relative
                    .components()
                    .any(|component| component.as_os_str() == "node_modules")
            })
        };
        match self.scope {
            Scope::Tree(tree) => path.starts_with(tree),
            Scope::File(file) => path == file,
            Scope::NodeModules => in_node_modules(),
            Scope::Directories => is_dir,
            Scope::Manifests(_) => !is_dir && name() == Some("package.json") && in_node_modules(),
        }
    }
}

/// What one task may do in the workspace, by mode.
#[derive(Clone, Debug)]
pub(super) struct Policy {
    mode: Mode,
    root: PathBuf,
    state: PathBuf,
    git: PathBuf,
    plan: Plan,
}

impl Policy {
    /// `plan` in the workspace at `root`, resolved, with qk's state for it at
    /// `state`.
    pub(super) fn new(mode: Mode, root: &Path, state: &Path, plan: Plan) -> Self {
        Self {
            mode,
            root: root.to_owned(),
            state: state.to_owned(),
            git: root.join(".git"),
            plan,
        }
    }

    pub(super) fn mode(&self) -> Mode {
        self.mode
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// Whether audit reports its installed packages' manifests.
    pub(super) fn checks_packages(&self) -> bool {
        self.plan.packages.is_some()
    }

    /// The same, without reporting installed packages.
    pub(super) fn without_packages(&self) -> Self {
        let mut policy = self.clone();
        policy.plan.packages = None;
        policy
    }

    /// What it may do, in order: where Seatbelt finds rules for one
    /// operation that conflict, the later wins.
    pub(super) fn grants(&self) -> impl Iterator<Item = Grant<'_>> {
        let plan = &self.plan;
        let unlinked = |scope, path: &PathBuf| Grant {
            unlinked: plan.unlinked.contains(path),
            ..Grant::new(scope, Reach::ReadOnly)
        };
        // Not keyed, but a task may need it.
        let git = (self.mode == Mode::Enforce)
            .then(|| Grant::new(Scope::Tree(&self.git), Reach::Through));
        let packages = plan
            .packages
            .as_ref()
            .map(|packages| Grant::new(Scope::Manifests(packages), Reach::ReadOnly));
        // qk's own state for the worktree, where warm directories live. In
        // `.git`, or outside the workspace, it needs no binding of its own.
        let state = if self.state.starts_with(&self.root) && !self.state.starts_with(&self.git) {
            Reach::ThroughKept
        } else {
            Reach::Kept
        };
        git.into_iter()
            .chain([Grant::new(Scope::NodeModules, Reach::ThroughKept)])
            .chain(packages)
            .chain([
                Grant::new(Scope::Directories, Reach::ReadOnly),
                Grant::new(Scope::Tree(&self.state), state),
            ])
            .chain(
                plan.read_directories
                    .iter()
                    .map(|directory| Grant::new(Scope::Tree(directory), Reach::ReadOnly)),
            )
            .chain(
                plan.read_files
                    .iter()
                    .map(move |file| unlinked(Scope::File(file), file)),
            )
            .chain(
                plan.read_trees
                    .iter()
                    .map(move |tree| unlinked(Scope::Tree(tree), tree)),
            )
            // Its scratch directories keep nothing between runs, and are
            // bound through as `node_modules` is.
            .chain(plan.write_trees.iter().map(|tree| {
                let reach = if plan.scratch.contains(tree) {
                    Reach::ThroughKept
                } else {
                    Reach::Kept
                };
                Grant::new(Scope::Tree(tree), reach)
            }))
    }

    /// What becomes of `access` to `path`, absolute, which `is_dir` says is
    /// a directory. Outside the workspace everything is allowed. Inside, a
    /// reporting grant that covers it wins, as Linux audit has always judged
    /// installed packages' manifests; then any other grant allows it; and
    /// what none covers audit reports and enforce refuses.
    pub(super) fn judge(&self, path: &Path, access: Access, is_dir: bool) -> Verdict {
        if !path.starts_with(&self.root) {
            return Verdict::Allow;
        }
        let covering = || {
            self.grants()
                .filter(move |grant| grant.covers(&self.root, path, access, is_dir))
        };
        if covering().any(|grant| grant.verdict() == Verdict::Report) {
            Verdict::Report
        } else if covering().next().is_some() {
            Verdict::Allow
        } else {
            match self.mode {
                Mode::Audit => Verdict::Report,
                Mode::Enforce => Verdict::Refuse,
            }
        }
    }

    /// Its outputs, warm paths and scratch directories.
    pub(super) fn outputs(&self) -> impl Iterator<Item = &Path> {
        self.plan.write_trees.iter().map(PathBuf::as_path)
    }

    /// Its scratch directories, which are directories whatever their names:
    /// Landlock, which holds only paths that exist, needs them made first.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn scratch(&self) -> impl Iterator<Item = &Path> {
        self.plan.scratch.iter().map(PathBuf::as_path)
    }
}

/// Whether `path` is `.git` or lies in it, which enforce lets a task read and
/// write.
pub fn in_git(path: &str) -> bool {
    path == ".git" || path.starts_with(".git/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan in `/w`, with qk's state in `.git`, and a policy for it in
    /// `mode`, which in audit reports installed packages as `Sandbox::start`
    /// has it.
    fn policy(mode: Mode) -> Policy {
        let root = Path::new("/w");
        let plan = Plan {
            read_directories: vec![root.join("libs/lib")],
            read_files: vec![root.join("package.json"), root.join("fixtures/data.json")],
            read_trees: [root.join("libs/dep/dist")].into(),
            unlinked: [root.join("fixtures/data.json")].into(),
            write_trees: [root.join("apps/app/dist"), root.join("tmp/app")].into(),
            scratch: [root.join("tmp/app")].into(),
            packages: (mode == Mode::Audit).then(|| Packages::AllBut(Vec::new())),
        };
        Policy::new(mode, root, &root.join(".git/qk"), plan)
    }

    #[test]
    fn judges_each_access_by_mode() {
        use Access::{Read, Write};
        use Verdict::{Allow, Refuse, Report};
        let (audit, enforce) = (policy(Mode::Audit), policy(Mode::Enforce));
        let unchecked = audit.without_packages();
        for (path, access, is_dir, audited, enforced) in [
            // Not keyed: enforce allows `.git`, audit reports it. A dotenv
            // file is read as any other.
            (".git/HEAD", Read, false, Report, Allow),
            (".git/index", Write, false, Report, Allow),
            ("apps/app/.env.local", Read, false, Report, Refuse),
            (".build.env", Read, false, Report, Refuse),
            ("apps/app/.env.local", Write, false, Report, Refuse),
            ("src/env.ts", Read, false, Report, Refuse),
            // Installed packages, and what tools keep beside them.
            (
                "node_modules/.pnpm/a@1.0.0/node_modules/a/index.js",
                Read,
                false,
                Allow,
                Allow,
            ),
            (
                "apps/app/node_modules/.cache/tool",
                Write,
                false,
                Allow,
                Allow,
            ),
            (
                "apps/app/node_modules/a/package.json",
                Read,
                false,
                Report,
                Allow,
            ),
            (
                "apps/app/node_modules/a/package.json",
                Write,
                false,
                Allow,
                Allow,
            ),
            // qk's state, its scratch directory and its outputs.
            (".git/qk/warm/app/cache", Write, false, Allow, Allow),
            ("tmp/app/x", Write, false, Allow, Allow),
            ("apps/app/dist/main.js", Write, false, Allow, Allow),
            ("apps/app/dist-other/main.js", Write, false, Report, Refuse),
            // Its inputs, read and not written.
            ("libs/lib/src/a.ts", Read, false, Allow, Allow),
            ("libs/dep/dist/index.js", Read, false, Allow, Allow),
            ("libs/dep/dist/index.js", Write, false, Report, Refuse),
            ("package.json", Read, false, Allow, Allow),
            ("package.json", Write, false, Report, Refuse),
            ("fixtures/data.json", Read, false, Allow, Allow),
            ("fixtures/other.json", Read, false, Report, Refuse),
            // Any directory may be listed.
            ("src", Read, true, Allow, Allow),
            ("src", Write, false, Report, Refuse),
        ] {
            let path = Path::new("/w").join(path);
            assert_eq!(
                audit.judge(&path, access, is_dir),
                audited,
                "audit: {access:?} {}",
                path.display()
            );
            assert_eq!(
                enforce.judge(&path, access, is_dir),
                enforced,
                "enforce: {access:?} {}",
                path.display()
            );
        }
        // Without its packages checked, a manifest is read as any file in
        // `node_modules`.
        let manifest = Path::new("/w/node_modules/a/package.json");
        assert_eq!(unchecked.judge(manifest, Read, false), Allow);
        // Outside the workspace, anything goes.
        for policy in [&audit, &enforce] {
            assert_eq!(policy.judge(Path::new("/etc/hosts"), Write, false), Allow);
        }
    }

    /// Each grant is judged as it says, so what renders it allows just what
    /// `judge` does.
    #[test]
    fn judges_what_each_grant_reaches_as_the_grant() {
        for policy in [policy(Mode::Audit), policy(Mode::Enforce)] {
            let root = policy.root();
            for grant in policy.grants() {
                let access = if grant.writes() {
                    Access::Write
                } else {
                    Access::Read
                };
                let (path, is_dir) = match grant.scope {
                    Scope::Tree(tree) => (tree.join("x"), false),
                    Scope::File(file) => (file.to_owned(), false),
                    Scope::NodeModules => (root.join("apps/app/node_modules/a/index.js"), false),
                    Scope::Directories => (root.join("apps"), true),
                    Scope::Manifests(_) => (root.join("node_modules/a/package.json"), false),
                };
                assert_eq!(
                    policy.judge(&path, access, is_dir),
                    grant.verdict(),
                    "{:?}: {grant:?}",
                    policy.mode()
                );
            }
        }
    }
}
