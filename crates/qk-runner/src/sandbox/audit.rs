//! What a task's reports say it did beyond its declarations, sorted into
//! findings: reads its key leaves out, by what they are and whose project
//! they lie in, installed packages it leaves out, and writes outside its
//! outputs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use qk_cache::ultracache::IgnoredReads;
use serde::Serialize;

use super::Report;
use super::policy::in_git;
use crate::installs::Installs;

/// What one task did beyond what it declares.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Findings {
    /// Workspace files it read that are not in its cache key, but for those
    /// grouped below.
    pub undeclared_reads: BTreeSet<String>,
    /// Workspace packages' `package.json` read outside its key, as package
    /// managers and bundlers do to resolve workspace packages.
    pub manifest_reads: BTreeSet<String>,
    /// Version and package managers' configuration read outside its key.
    pub toolchain_reads: BTreeSet<String>,
    /// TypeScript configuration read outside its key: likely inputs.
    pub tsconfig_reads: BTreeSet<String>,
    /// Files read outside its key in projects its project depends on, by
    /// project: what running a dependency's code or following imports into
    /// it reads. `^` inputs key them.
    pub dependency_reads: BTreeMap<String, BTreeSet<String>>,
    /// Dotenv files qk loads into its environment that it read outside its
    /// key: what they hold reaches it through the variables qk loads, which
    /// `env` inputs key, so naming them in its ignored reads covers them. A
    /// dotenv file qk does not load for it is read as any other file.
    pub dotenv_reads: BTreeSet<String>,
    /// `.git` files it read: allowed, but not in the key either.
    pub unkeyed_reads: BTreeSet<String>,
    /// Paths it wrote in the workspace outside its outputs.
    pub stray_writes: BTreeSet<String>,
    /// Installed packages it read that its key leaves out, by name, when
    /// it names its `externalDependencies`: naming them there keys them.
    pub unkeyed_packages: BTreeSet<String>,
    /// Installed packages it read that its key leaves out although it counts
    /// every package its importers install: none of those depends on them,
    /// so a package found them through pnpm's hoisting, which links every
    /// installed package where any can resolve it. Declaring each as a
    /// dependency of what imports it keys it.
    pub hoisted_packages: BTreeSet<String>,
}

impl Findings {
    /// Whether it did anything enforce would refuse: beyond reading `.git` and
    /// installed packages its key leaves out, and writing in `.git`, which
    /// enforce allows.
    pub fn went_beyond(&self) -> bool {
        !self.undeclared_reads.is_empty()
            || !self.dotenv_reads.is_empty()
            || !self.manifest_reads.is_empty()
            || !self.toolchain_reads.is_empty()
            || !self.tsconfig_reads.is_empty()
            || !self.dependency_reads.is_empty()
            || self.stray_writes.iter().any(|path| !in_git(path))
    }

    pub fn is_empty(&self) -> bool {
        self.undeclared_reads.is_empty()
            && self.dotenv_reads.is_empty()
            && self.manifest_reads.is_empty()
            && self.toolchain_reads.is_empty()
            && self.tsconfig_reads.is_empty()
            && self.dependency_reads.is_empty()
            && self.unkeyed_reads.is_empty()
            && self.stray_writes.is_empty()
            && self.unkeyed_packages.is_empty()
            && self.hoisted_packages.is_empty()
    }
}

/// What a task's reports are judged against, made as the run starts.
#[derive(Clone, Default)]
pub(super) struct TaskAudit {
    /// The workspace root, resolved, which reports' paths lie beneath.
    root: PathBuf,
    /// Its `qk:ultracache.ignoreReads`: reads that change nothing it does,
    /// which are no finding either.
    ignored: Option<IgnoredReads>,
    /// The manifests of the workspace projects its ignored reads link to,
    /// beyond its key, which are no finding either.
    ignored_manifests: BTreeSet<String>,
    /// The installations its key covers, or its ignored reads reach; `None`
    /// leaves its packages unchecked.
    covered: Option<BTreeSet<String>>,
    /// Whether its key names its `externalDependencies`, even none, in place
    /// of what its importers install.
    naming_external: bool,
    /// The projects its project depends on, directly or not, by name with
    /// their roots, innermost first.
    depended: Vec<(Option<String>, String)>,
    /// Its outputs, warm paths and scratch directories.
    outputs: BTreeSet<PathBuf>,
    /// The dotenv files qk loads into its environment, workspace-relative.
    loaded: BTreeSet<String>,
}

impl TaskAudit {
    pub(super) fn new(root: &Path, depended: Vec<(Option<String>, String)>) -> Self {
        Self {
            root: root.to_owned(),
            depended,
            ..Self::default()
        }
    }

    pub(super) fn ignoring(&mut self, reads: IgnoredReads) {
        self.ignored = Some(reads);
    }

    pub(super) fn ignored(&self) -> Option<&IgnoredReads> {
        self.ignored.as_ref()
    }

    pub(super) fn ignoring_manifests(&mut self, manifests: BTreeSet<String>) {
        self.ignored_manifests = manifests;
    }

    pub(super) fn covering(&mut self, covered: BTreeSet<String>, naming_external: bool) {
        self.covered = Some(covered);
        self.naming_external = naming_external;
    }

    pub(super) fn loading(&mut self, files: impl IntoIterator<Item = String>) {
        self.loaded = files.into_iter().collect();
    }

    pub(super) fn writing(&mut self, outputs: impl IntoIterator<Item = PathBuf>) {
        self.outputs = outputs.into_iter().collect();
    }

    /// The findings its reports make, with `discarded`, what it wrote over an
    /// overlay outside its outputs, which was not kept: a finding as audit
    /// would report it.
    pub(super) fn classify(
        &self,
        installs: Option<&Installs>,
        reports: impl IntoIterator<Item = Report>,
        discarded: impl IntoIterator<Item = String>,
    ) -> Findings {
        let mut findings = Findings::default();
        findings.stray_writes.extend(discarded);
        for report in reports {
            let Ok(relative) = Path::new(&report.path).strip_prefix(&self.root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !report.writes()
                && (self
                    .ignored
                    .as_ref()
                    .is_some_and(|reads| reads.is_match(&relative))
                    || self.ignored_manifests.contains(&relative))
            {
                continue;
            }
            let package = (!report.writes())
                .then(|| installs?.unkeyed(self.covered.as_ref()?, Path::new(&report.path)))
                .flatten();
            // Reads in `node_modules` are reported only to find packages: one
            // the key covers, or a manifest belonging to no package, is not
            // a finding.
            if package.is_none() && relative.split('/').any(|part| part == "node_modules") {
                continue;
            }
            // Making the directory an output lies in, as a tool writing
            // `dist/app.js` makes `dist`, writes no output of its own: what it
            // writes in it is reported by itself.
            let path = Path::new(&report.path);
            if report.writes()
                && path.is_dir()
                && self
                    .outputs
                    .iter()
                    .any(|tree| tree != path && tree.starts_with(path))
            {
                continue;
            }
            let dependency = self
                .depended
                .iter()
                .find(|(_, root)| relative.starts_with(&format!("{root}/")))
                .and_then(|(project, _)| project.clone());
            if let Some(package) = package {
                if self.naming_external {
                    findings.unkeyed_packages.insert(package);
                } else {
                    findings.hoisted_packages.insert(package);
                }
            } else if report.writes() {
                findings.stray_writes.insert(relative);
            } else if in_git(&relative) {
                findings.unkeyed_reads.insert(relative);
            } else if is_dotenv(&relative) {
                // Wherever it lies: one in a project it depends on is no
                // code of that project's it ran.
                if self.loaded.contains(&relative) {
                    findings.dotenv_reads.insert(relative);
                } else {
                    findings.undeclared_reads.insert(relative);
                }
            } else if let Some(project) = dependency
                && qk_input_analysis::ReadKind::of(&relative)
                    == qk_input_analysis::ReadKind::Content
            {
                findings
                    .dependency_reads
                    .entry(project)
                    .or_default()
                    .insert(relative);
            } else {
                match qk_input_analysis::ReadKind::of(&relative) {
                    qk_input_analysis::ReadKind::Manifest => &mut findings.manifest_reads,
                    qk_input_analysis::ReadKind::Toolchain => &mut findings.toolchain_reads,
                    qk_input_analysis::ReadKind::TsConfig => &mut findings.tsconfig_reads,
                    qk_input_analysis::ReadKind::Content => &mut findings.undeclared_reads,
                }
                .insert(relative);
            }
        }
        // A file a task writes outside its outputs and reads, as a version
        // stamp it keeps, is one finding: declaring it an output covers both.
        for path in &findings.stray_writes {
            findings.undeclared_reads.remove(path);
            findings.dotenv_reads.remove(path);
            findings.manifest_reads.remove(path);
            findings.toolchain_reads.remove(path);
            findings.tsconfig_reads.remove(path);
            for paths in findings.dependency_reads.values_mut() {
                paths.remove(path);
            }
        }
        findings
            .dependency_reads
            .retain(|_, paths| !paths.is_empty());
        findings
    }
}

/// Whether `path` names a dotenv file: `.env`, `.env.*` or `.*.env`.
pub fn is_dotenv(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == ".env" || name.starts_with(".env.") || (name.starts_with('.') && name.ends_with(".env"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn read(root: &Path, path: &str) -> Report {
        report(root, "file-read-data", path)
    }

    fn write(root: &Path, path: &str) -> Report {
        report(root, "file-write-create", path)
    }

    fn report(root: &Path, operation: &str, path: &str) -> Report {
        Report {
            task: "app:build".into(),
            process: 1,
            operation: operation.into(),
            path: root.join(path).to_string_lossy().into_owned(),
        }
    }

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn sorts_reads_by_what_they_are() {
        let root = Path::new("/workspace");
        let audit = TaskAudit::new(root, Vec::new());
        let found = audit.classify(
            None,
            [
                ".git",
                ".git/HEAD",
                ".env",
                "apps/web/.env.local",
                "apps/web/.build.env",
                "src/env.ts",
                ".github/workflows/ci.yml",
                "libs/lib/package.json",
                ".nvmrc",
                "tsconfig.base.json",
                "node_modules/.cache/x",
            ]
            .map(|path| read(root, path))
            .into_iter()
            .chain([Report {
                path: "/elsewhere/file".into(),
                ..read(root, "")
            }]),
            [],
        );
        assert_eq!(found.unkeyed_reads, set(&[".git", ".git/HEAD"]));
        // A dotenv file is read outside its inputs as any other.
        assert_eq!(
            found.undeclared_reads,
            set(&[
                ".env",
                ".github/workflows/ci.yml",
                "apps/web/.build.env",
                "apps/web/.env.local",
                "src/env.ts"
            ])
        );
        assert_eq!(found.manifest_reads, set(&["libs/lib/package.json"]));
        assert_eq!(found.toolchain_reads, set(&[".nvmrc"]));
        assert_eq!(found.tsconfig_reads, set(&["tsconfig.base.json"]));
        assert!(found.stray_writes.is_empty() && found.went_beyond());
    }

    /// A dotenv file qk loads for the task is listed apart; one it does not
    /// load is read outside its inputs as any other file, in a project it
    /// depends on too, rather than as that project's code.
    #[test]
    fn tells_dotenv_files_qk_loads_from_others() {
        let root = Path::new("/workspace");
        let mut audit = TaskAudit::new(root, vec![(Some("lib".into()), "libs/lib".into())]);
        audit.loading([".env".to_owned(), "apps/app/.env.local".to_owned()]);
        let found = audit.classify(
            None,
            [
                ".env",
                "apps/app/.env.local",
                ".env.production",
                "libs/lib/.env",
                "libs/lib/src/a.ts",
            ]
            .map(|path| read(root, path)),
            [],
        );
        assert_eq!(found.dotenv_reads, set(&[".env", "apps/app/.env.local"]));
        assert_eq!(
            found.undeclared_reads,
            set(&[".env.production", "libs/lib/.env"])
        );
        assert_eq!(
            found.dependency_reads,
            BTreeMap::from([("lib".to_owned(), set(&["libs/lib/src/a.ts"]))])
        );
        assert!(found.went_beyond());
    }

    /// Reads its ignored reads match, dotenv files among them, and the
    /// manifests of the projects they link to, are no finding; writing there
    /// still is.
    #[test]
    fn leaves_out_ignored_reads() {
        let root = Path::new("/workspace");
        let mut audit = TaskAudit::new(root, Vec::new());
        audit.ignoring(
            IgnoredReads::of_globs(&["apps/app/e2e/**".to_owned(), "**/.env".to_owned()]).unwrap(),
        );
        audit.ignoring_manifests(set(&["packages/core/package.json"]));
        let found = audit.classify(
            None,
            [
                read(root, "apps/app/e2e/spec.js"),
                read(root, "apps/app/.env"),
                read(root, ".env"),
                read(root, "packages/core/package.json"),
                read(root, "packages/core/index.js"),
                write(root, "apps/app/e2e/out.txt"),
            ],
            [],
        );
        assert_eq!(found.undeclared_reads, set(&["packages/core/index.js"]));
        assert!(found.manifest_reads.is_empty());
        assert_eq!(found.stray_writes, set(&["apps/app/e2e/out.txt"]));
    }

    /// An installed package the key leaves out is named by the reads that
    /// find it, as unkeyed where the task names its `externalDependencies`
    /// and hoisted where not; other reads in `node_modules` are no finding.
    #[test]
    fn names_packages_the_key_leaves_out() {
        let temp = crate::installs::tests::workspace();
        let root = temp.path().canonicalize().unwrap();
        let installs = Installs::load(&root).unwrap();
        let reports = || {
            [
                read(&root, "node_modules/fallow/index.js"),
                read(&root, "apps/app/node_modules/react/index.js"),
                read(&root, "node_modules/.modules.yaml"),
                write(&root, "node_modules/.cache/tool/state"),
            ]
        };
        let classify = |naming_external: bool| {
            let mut audit = TaskAudit::new(&root, Vec::new());
            audit.covering(set(&["react@19.0.0"]), naming_external);
            audit.classify(Some(&installs), reports(), [])
        };
        let named = classify(true);
        assert_eq!(named.unkeyed_packages, set(&["fallow"]));
        assert!(named.hoisted_packages.is_empty());
        let hoisted = classify(false);
        assert_eq!(hoisted.hoisted_packages, set(&["fallow"]));
        assert!(hoisted.unkeyed_packages.is_empty());
        for found in [&named, &hoisted] {
            assert!(found.undeclared_reads.is_empty() && found.stray_writes.is_empty());
            assert!(!found.went_beyond());
        }
        // Without a key's packages, none is checked.
        let unchecked = TaskAudit::new(&root, Vec::new()).classify(Some(&installs), reports(), []);
        assert!(unchecked.is_empty(), "{unchecked:?}");
    }

    /// Making the directory an output lies in is no finding; what is
    /// written beside the output in it is.
    #[test]
    fn allows_making_an_outputs_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let mut audit = TaskAudit::new(root, Vec::new());
        audit.writing([root.join("dist/app")]);
        let found = audit.classify(
            None,
            [
                write(root, "dist"),
                write(root, "dist/sibling.js"),
                write(root, ".git/index"),
            ],
            [],
        );
        assert_eq!(found.stray_writes, set(&[".git/index", "dist/sibling.js"]));
        assert!(found.went_beyond());
    }

    /// Reads in a project its project depends on are grouped by that
    /// project, but for those in a project nested in it that it does not
    /// depend on, and for configuration.
    #[test]
    fn groups_reads_in_dependency_projects() {
        let root = Path::new("/workspace");
        let audit = TaskAudit::new(
            root,
            vec![
                (None, "libs/lib/nested".into()),
                (Some("lib".into()), "libs/lib".into()),
                (None, "libs/other".into()),
            ],
        );
        let found = audit.classify(
            None,
            [
                "libs/lib/src/a.ts",
                "libs/lib/nested/c.ts",
                "libs/lib/package.json",
                "libs/other/b.ts",
                "libs/library.ts",
            ]
            .map(|path| read(root, path)),
            [],
        );
        assert_eq!(
            found.dependency_reads,
            BTreeMap::from([("lib".to_owned(), set(&["libs/lib/src/a.ts"]))])
        );
        assert_eq!(
            found.undeclared_reads,
            set(&["libs/library.ts", "libs/lib/nested/c.ts", "libs/other/b.ts"])
        );
        assert_eq!(found.manifest_reads, set(&["libs/lib/package.json"]));
    }

    /// A `^` ignored read names reads in the projects its project depends
    /// on, but not in a project nested in one that it does not depend on.
    #[test]
    fn ignores_reads_named_in_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        for (directory, project) in [
            (
                "apps/app",
                r#"{"name":"app","implicitDependencies":["lib"],"targets":{}}"#,
            ),
            ("libs/lib", r#"{"name":"lib","targets":{}}"#),
            ("libs/lib/nested", r#"{"name":"nested","targets":{}}"#),
            ("libs/other", r#"{"name":"other","targets":{}}"#),
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
            std::fs::write(root.join(directory).join("project.json"), project).unwrap();
        }
        let workspace = qk_config::Workspace::load(root).unwrap();
        let projects = qk_graph::ProjectGraph::build(&workspace).unwrap();
        let reads =
            IgnoredReads::new(&["^**".to_owned()], &workspace, Some(&projects), "app").unwrap();
        let mut audit = TaskAudit::new(
            root,
            vec![
                (None, "libs/lib/nested".into()),
                (None, "libs/other".into()),
                (None, "apps/app".into()),
                (Some("lib".into()), "libs/lib".into()),
            ],
        );
        audit.ignoring(reads);
        let found = audit.classify(
            None,
            [
                "libs/lib/src/a.ts",
                "libs/lib/nested/c.ts",
                "libs/other/b.ts",
            ]
            .map(|path| read(root, path)),
            [],
        );
        assert!(found.dependency_reads.is_empty(), "{found:?}");
        assert_eq!(
            found.undeclared_reads,
            set(&["libs/lib/nested/c.ts", "libs/other/b.ts"])
        );
    }

    /// A file written outside its outputs and read is one finding, the
    /// write; what an overlay discarded is a stray write as it stands.
    #[test]
    fn counts_a_stray_write_read_back_once() {
        let root = Path::new("/workspace");
        let audit = TaskAudit::new(root, vec![(Some("lib".into()), "libs/lib".into())]);
        let found = audit.classify(
            None,
            [
                write(root, "stamp.txt"),
                read(root, "stamp.txt"),
                read(root, "libs/lib/out.js"),
                read(root, "discarded.txt"),
            ],
            [
                "libs/lib/out.js".to_owned(),
                "discarded.txt".to_owned(),
                "node_modules/.cache/x".to_owned(),
            ],
        );
        assert_eq!(
            found.stray_writes,
            set(&[
                "discarded.txt",
                "libs/lib/out.js",
                "node_modules/.cache/x",
                "stamp.txt"
            ])
        );
        assert!(found.undeclared_reads.is_empty(), "{found:?}");
        assert!(found.dependency_reads.is_empty(), "{found:?}");
    }
}
