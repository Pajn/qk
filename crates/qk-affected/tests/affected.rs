use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use qk_affected::{Options, affected_projects};
use qk_config::Workspace;
use qk_graph::ProjectGraph;
use tempfile::TempDir;

/// A git repository whose first commit is the base revision.
struct Repo {
    _temp: TempDir,
    root: PathBuf,
    base: String,
}

impl Repo {
    /// `app` depends on `lib`; `tool` stands alone. `app` declares a
    /// `{workspaceRoot}` input through a named input.
    fn new(extra: &[(&str, &str)]) -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let mut files = vec![
            (
                "nx.json",
                r#"{"namedInputs": {"shared": ["{workspaceRoot}/tsconfig.*.json"]},
                    "pluginsConfig": {"@nx/js": {"projectsAffectedByDependencyUpdates": "auto"}}}"#,
            ),
            (
                "apps/app/project.json",
                r#"{"name": "app", "implicitDependencies": ["lib"],
                    "targets": {"tsc": {"inputs": ["default", "shared"]}}}"#,
            ),
            ("apps/app/src/main.ts", "app"),
            ("libs/lib/project.json", r#"{"name": "lib"}"#),
            ("libs/lib/src/index.ts", "lib"),
            ("tools/tool/project.json", r#"{"name": "tool"}"#),
            ("tools/tool/README.md", "tool"),
            ("README.md", "root"),
            ("tsconfig.app.json", "{}"),
            (".gitignore", "dist/\n"),
            (".nxignore", "libs/lib/generated/\n"),
        ];
        files.extend_from_slice(extra);
        for (path, text) in files {
            write(&root, path, text);
        }
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        let base = commit(&root);
        Self {
            _temp: temp,
            root,
            base,
        }
    }

    fn affected(&self, options: Options) -> Vec<String> {
        let workspace = Workspace::load(&self.root).unwrap();
        let graph = ProjectGraph::build(&workspace).unwrap();
        affected_projects(&workspace, &graph, &options)
            .unwrap()
            .into_iter()
            .collect()
    }

    /// Commits the working tree and reports what changed since the base.
    fn committed(&self) -> Vec<String> {
        let head = commit(&self.root);
        self.affected(Options {
            base: Some(self.base.clone()),
            head: Some(head),
            ..Options::default()
        })
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path) -> String {
    git(root, &["add", "--all"]);
    git(
        root,
        &[
            "-c",
            "user.name=qk",
            "-c",
            "user.email=qk@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "change",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_changed_file_affects_its_project_and_dependents() {
    let repo = Repo::new(&[]);
    write(&repo.root, "libs/lib/src/index.ts", "changed");
    write(&repo.root, "README.md", "changed");
    assert_eq!(repo.committed(), names(&["app", "lib"]));
}

#[test]
fn nx_json_affects_every_project() {
    let repo = Repo::new(&[]);
    write(&repo.root, "nx.json", r#"{"namedInputs": {"shared": []}}"#);
    assert_eq!(repo.committed(), names(&["app", "lib", "tool"]));
}

#[test]
fn workspace_root_inputs_affect_the_projects_naming_them() {
    let repo = Repo::new(&[]);
    write(
        &repo.root,
        "tsconfig.app.json",
        r#"{"compilerOptions": {}}"#,
    );
    assert_eq!(repo.committed(), names(&["app"]));
}

#[test]
fn ignored_files_affect_nothing() {
    let repo = Repo::new(&[]);
    write(&repo.root, "libs/lib/generated/out.ts", "generated");
    assert_eq!(repo.committed(), Vec::<String>::new());
}

#[test]
fn deleting_a_project_manifest_affects_every_project() {
    let repo = Repo::new(&[]);
    std::fs::remove_dir_all(repo.root.join("tools/tool")).unwrap();
    // The deleted project is gone from the head workspace.
    assert_eq!(repo.committed(), names(&["app", "lib"]));
}

#[test]
fn without_a_head_the_working_tree_counts() {
    let repo = Repo::new(&[]);
    write(&repo.root, "tools/tool/README.md", "edited, uncommitted");
    write(&repo.root, "libs/lib/src/new.ts", "untracked");
    let affected = repo.affected(Options {
        base: Some("main".into()),
        ..Options::default()
    });
    assert_eq!(affected, names(&["app", "lib", "tool"]));
}

#[test]
fn pnpm_workspace_resolution_keys_reach_tasks_only_through_the_lockfile() {
    let workspace_file = "packages:\n  - apps/*\ncatalog:\n  react: 19.0.0\n";
    let repo = Repo::new(&[
        ("pnpm-workspace.yaml", workspace_file),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"lint": {"inputs": ["{workspaceRoot}/pnpm-workspace.yaml"]}}}"#,
        ),
    ]);
    write(
        &repo.root,
        "pnpm-workspace.yaml",
        &workspace_file.replace("19.0.0", "19.1.0"),
    );
    assert_eq!(repo.committed(), Vec::<String>::new());
    write(
        &repo.root,
        "pnpm-workspace.yaml",
        "packages:\n  - apps/*\n  - libs/*\ncatalog:\n  react: 19.1.0\n",
    );
    assert_eq!(repo.committed(), names(&["app", "lib"]));
}

fn lockfile(app: &str, tool: &str) -> String {
    format!(
        "lockfileVersion: '9.0'
importers:
  apps/app:
    dependencies:
      react:
        specifier: ^19.0.0
        version: {app}
  tools/tool:
    dependencies:
      react:
        specifier: ^19.0.0
        version: {tool}
packages:
  react@19.0.0:
    resolution: {{integrity: sha512-a}}
  react@19.1.0:
    resolution: {{integrity: sha512-b}}
snapshots:
  react@19.0.0: {{}}
  react@19.1.0: {{}}
"
    )
}

#[test]
fn lockfile_changes_affect_the_projects_whose_installs_changed() {
    let repo = Repo::new(&[("pnpm-lock.yaml", &lockfile("19.0.0", "19.0.0"))]);
    write(&repo.root, "pnpm-lock.yaml", &lockfile("19.0.0", "19.1.0"));
    assert_eq!(repo.committed(), names(&["tool"]));
}

#[test]
fn lockfile_changes_affect_every_project_outside_auto_mode() {
    let repo = Repo::new(&[
        ("nx.json", "{}"),
        ("pnpm-lock.yaml", &lockfile("19.0.0", "19.0.0")),
    ]);
    write(&repo.root, "pnpm-lock.yaml", &lockfile("19.0.0", "19.1.0"));
    assert_eq!(repo.committed(), names(&["app", "lib", "tool"]));
}

#[test]
fn root_package_dependencies_affect_the_projects_installing_them() {
    let repo = Repo::new(&[
        (
            "package.json",
            r#"{"name": "root", "devDependencies": {"react": "^19.0.0"}}"#,
        ),
        ("pnpm-lock.yaml", &lockfile("19.0.0", "19.0.0")),
    ]);
    // Only the tool installs react once the lockfile moves the app off it.
    write(
        &repo.root,
        "package.json",
        r#"{"name": "root", "devDependencies": {"react": "^19.1.0"}}"#,
    );
    write(
        &repo.root,
        "pnpm-lock.yaml",
        &lockfile("19.0.0", "19.1.0").replace("  apps/app:\n    dependencies:\n      react:\n        specifier: ^19.0.0\n        version: 19.0.0\n", "  apps/app: {}\n"),
    );
    assert_eq!(repo.committed(), names(&["app", "tool"]));
    write(&repo.root, "package.json", r#"{"name": "root"}"#);
    assert_eq!(repo.committed(), names(&["app", "lib", "tool"]));
}

#[test]
fn root_tsconfig_path_changes_affect_the_projects_they_map_into() {
    let paths = |target: &str, strict: bool| {
        format!(
            r#"{{"compilerOptions": {{"strict": {strict}, "paths": {{"@tool": ["{target}"]}}}}}}"#
        )
    };
    let repo = Repo::new(&[("tsconfig.base.json", &paths("tools/tool/a.ts", true))]);
    write(
        &repo.root,
        "tsconfig.base.json",
        &paths("tools/tool/b.ts", true),
    );
    // The app names the file through its `{workspaceRoot}/tsconfig.*.json` input.
    assert_eq!(repo.committed(), names(&["app", "tool"]));
    write(
        &repo.root,
        "tsconfig.base.json",
        &paths("tools/tool/b.ts", false),
    );
    assert_eq!(repo.committed(), names(&["app", "lib", "tool"]));
}

fn analyse(repo: &Repo, base: &str, head: &str) -> qk_affected::Analysis {
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    qk_affected::analyse(
        &workspace,
        &graph,
        &Options {
            base: Some(base.to_owned()),
            head: Some(head.to_owned()),
            ..Options::default()
        },
    )
    .unwrap()
}

#[test]
fn explains_each_project_through_a_shortest_path_to_its_reasons() {
    use qk_affected::{Cause, Reason};
    let repo = Repo::new(&[]);
    write(&repo.root, "libs/lib/src/index.ts", "changed");
    write(
        &repo.root,
        "tsconfig.app.json",
        r#"{"compilerOptions": {}}"#,
    );
    let first = commit(&repo.root);
    let analysis = analyse(&repo, &repo.base, &first);
    assert_eq!(analysis.chain("app"), Some(vec!["app"]));
    assert_eq!(analysis.chain("lib"), Some(vec!["lib"]));
    assert_eq!(analysis.chain("tool"), None);
    let Cause::Touched { reasons } = &analysis.projects["app"] else {
        panic!("app is touched by its workspace input");
    };
    assert!(matches!(
        &reasons[..],
        [Reason::WorkspaceInput { file, target, input }]
            if file == "tsconfig.app.json" && target == "tsc" && input == "{workspaceRoot}/tsconfig.*.json"
    ));
    assert_eq!(
        reasons[0].to_string(),
        "tsconfig.app.json changed, matching input {workspaceRoot}/tsconfig.*.json of target tsc"
    );

    write(&repo.root, "libs/lib/src/index.ts", "changed again");
    let analysis = analyse(&repo, &first, &commit(&repo.root));
    assert_eq!(analysis.chain("app"), Some(vec!["app", "lib"]));
    assert!(matches!(
        &analysis.projects["app"],
        Cause::DependsOn { project, kind, also } if project == "lib" && kind == "implicit" && also.is_empty()
    ));
}

#[test]
fn explains_lockfile_touches_by_what_changed() {
    use qk_affected::{Cause, Reason};
    let repo = Repo::new(&[("pnpm-lock.yaml", &lockfile("19.0.0", "19.0.0"))]);
    write(&repo.root, "pnpm-lock.yaml", &lockfile("19.0.0", "19.1.0"));
    let analysis = analyse(&repo, &repo.base, &commit(&repo.root));
    let Cause::Touched { reasons } = &analysis.projects["tool"] else {
        panic!("tool is touched by the lockfile");
    };
    let [
        Reason::Installs {
            importer,
            direct,
            added,
            removed,
            changed,
            ..
        },
    ] = &reasons[..]
    else {
        panic!("{reasons:?}");
    };
    assert_eq!(importer, "tools/tool");
    assert_eq!(direct, &["react: react@19.0.0 -> react@19.1.0"]);
    assert_eq!(added, &["react@19.1.0"]);
    assert_eq!(removed, &["react@19.0.0"]);
    assert!(changed.is_empty());
    assert_eq!(
        reasons[0].to_string(),
        "what tools/tool installs changed in pnpm-lock.yaml (1 package)"
    );
}

fn task_graph_repo() -> Repo {
    Repo::new(&[
        (
            "nx.json",
            r#"{"namedInputs": {"default": ["{projectRoot}/**/*"], "production": ["default", "!{projectRoot}/**/*.test.ts"]},
                "targetDefaults": {"build": {"command": "echo build", "inputs": ["production", "^production"], "dependsOn": ["^build"]},
                                   "test": {"command": "echo test", "inputs": ["default"]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "implicitDependencies": ["lib"], "targets": {"build": {}, "test": {}}}"#,
        ),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
        ),
    ])
}

fn affected_tasks(repo: &Repo) -> BTreeMap<String, qk_affected::TaskCause> {
    use qk_taskgraph::{Request, TaskGraph};
    let workspace = Workspace::load(&repo.root).unwrap();
    let requests: Vec<Request> = ["app:build", "app:test", "lib:build", "lib:test"]
        .into_iter()
        .map(|id| Request::parse(id).unwrap())
        .collect();
    let graph = TaskGraph::build(&workspace, &requests).unwrap();
    let head = commit(&repo.root);
    qk_affected::affected_tasks(
        &workspace,
        &graph,
        &Options {
            base: Some(repo.base.clone()),
            head: Some(head),
            ..Options::default()
        },
    )
    .unwrap()
    .tasks
}

#[test]
fn a_task_is_affected_when_its_inputs_change() {
    use qk_affected::{TaskCause, TaskReason};
    let repo = task_graph_repo();
    write(&repo.root, "libs/lib/src/index.test.ts", "a test");
    let tasks = affected_tasks(&repo);
    // The test file is not a production input, so no build is affected.
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["lib:test"]);
    assert!(matches!(
        &tasks["lib:test"],
        TaskCause::Touched { reasons } if matches!(&reasons[..], [TaskReason::Input { file }] if file == "libs/lib/src/index.test.ts")
    ));

    let repo = task_graph_repo();
    write(&repo.root, "libs/lib/src/index.ts", "source");
    let tasks = affected_tasks(&repo);
    assert_eq!(
        tasks.keys().collect::<Vec<_>>(),
        ["app:build", "lib:build", "lib:test"]
    );
    // app:build reads lib's production files through ^production.
    assert!(matches!(&tasks["app:build"], TaskCause::Touched { .. }));
}

/// A branch taken from `origin/main` after it moved on, while the local `main`
/// stayed behind: `tool` changed on `origin/main`, `lib` on the branch.
/// Returns the stale local `main`.
fn behind_origin(repo: &Repo) -> String {
    let remote = repo.root.join(".git/remote.git");
    git(
        &repo.root,
        &["init", "--quiet", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repo.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo.root, &["push", "--quiet", "-u", "origin", "main"]);
    write(&repo.root, "tools/tool/README.md", "landed");
    commit(&repo.root);
    git(&repo.root, &["push", "--quiet", "origin", "main"]);
    git(&repo.root, &["switch", "--quiet", "-c", "feature"]);
    git(&repo.root, &["branch", "-f", "main", &repo.base]);
    write(&repo.root, "libs/lib/src/index.ts", "changed");
    commit(&repo.root);
    repo.base.clone()
}

fn analyse_default(repo: &Repo, base: Option<&str>) -> qk_affected::Analysis {
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    qk_affected::analyse(
        &workspace,
        &graph,
        &Options {
            base: base.map(str::to_owned),
            ..Options::default()
        },
    )
    .unwrap()
}

#[test]
fn a_stale_local_base_branch_is_compared_from_its_upstream() {
    let repo = Repo::new(&[]);
    behind_origin(&repo);
    let analysis = analyse_default(&repo, None);
    assert_eq!(
        analysis.projects.keys().cloned().collect::<Vec<_>>(),
        names(&["app", "lib"])
    );
    let range = analysis.range.unwrap();
    assert_eq!(range.upstream.as_deref(), Some("origin/main"));
    assert_eq!((range.commits, range.landed), (1, 0));
}

#[test]
fn commits_that_already_landed_are_counted_when_the_base_predates_them() {
    let repo = Repo::new(&[]);
    let old = behind_origin(&repo);
    let range = analyse_default(&repo, Some(&old)).range.unwrap();
    assert_eq!((range.commits, range.landed), (2, 1));
    assert_eq!(range.default_branch.as_deref(), Some("origin/main"));
    // On the default branch itself the range is its own history.
    git(
        &repo.root,
        &["switch", "--quiet", "--detach", "origin/main"],
    );
    let range = analyse_default(&repo, Some(&old)).range.unwrap();
    assert_eq!((range.commits, range.landed), (1, 0));
}

#[test]
fn a_changed_json_input_affects_the_task() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": [{"json": "{projectRoot}/meta.json", "fields": ["version"]}]},
                                   "test": {"command": "echo test", "inputs": ["{projectRoot}/src/**/*"]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}, "test": {}}}"#,
        ),
        ("apps/app/meta.json", r#"{"version": 1}"#),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
        ),
    ]);
    write(&repo.root, "apps/app/meta.json", r#"{"version": 2}"#);
    let tasks = affected_tasks(&repo);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

/// Ignore rules prune source filesets, but explicit JSON inputs still affect keys.
#[test]
fn ignored_json_inputs_affect_tasks_without_reincluding_ignored_sources() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": [{"json": "{projectRoot}/meta.json", "fields": ["version"]}]},
                                   "test": {"command": "echo test", "inputs": ["{projectRoot}/src/**/*"]}}}"#,
        ),
        (".nxignore", "apps/app/meta.json\napps/app/src/ignored.ts\n"),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}, "test": {}}}"#,
        ),
        ("apps/app/meta.json", r#"{"version": 1}"#),
        ("apps/app/src/ignored.ts", "before"),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
        ),
    ]);
    write(&repo.root, "apps/app/src/ignored.ts", "after");
    assert!(affected_tasks(&repo).is_empty());
    write(&repo.root, "apps/app/meta.json", r#"{"version": 2}"#);
    let tasks = affected_tasks(&repo);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

/// Changed output candidates remain inputs of consumers, never of their producer.
#[test]
fn changed_declared_outputs_only_affect_tasks_that_consume_them() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"], "outputs": ["{projectRoot}/src/generated.txt"]},
                                   "test": {"command": "echo test", "inputs": ["{projectRoot}/src/**/*"]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}, "test": {}}}"#,
        ),
        ("apps/app/src/generated.txt", "before"),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
        ),
    ]);
    write(&repo.root, "apps/app/src/generated.txt", "after");
    let tasks = affected_tasks(&repo);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:test"]);
    std::fs::remove_file(repo.root.join("apps/app/src/generated.txt")).unwrap();
    let tasks = affected_tasks(&repo);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:test"]);
}

/// Ignore matching consistently prunes excluded parents and honors Nx negations.
#[test]
fn nxignore_parent_and_untracked_negations_match_source_discovery() {
    let repo = Repo::new(&[
        (
            ".nxignore",
            "libs/lib/generated/\n!libs/lib/generated/keep.ts\n",
        ),
        ("libs/lib/generated/keep.ts", "tracked"),
    ]);
    assert_eq!(
        repo.affected(Options {
            files: vec!["libs/lib/generated/keep.ts".into()],
            explicit_files: true,
            ..Options::default()
        }),
        Vec::<String>::new()
    );
    let repo = Repo::new(&[
        (".gitignore", "libs/lib/src/*.ts\n"),
        (".nxignore", "!libs/lib/src/keep.ts\n"),
    ]);
    write(&repo.root, "libs/lib/src/keep.ts", "untracked");
    // Named, the negation includes it.
    assert_eq!(
        repo.affected(Options {
            files: vec!["libs/lib/src/keep.ts".into()],
            explicit_files: true,
            ..Options::default()
        }),
        names(&["app", "lib"])
    );
    // Untracked files are what Git lists, as in Nx, which leaves out what
    // .gitignore excludes whatever .nxignore says.
    assert_eq!(
        repo.affected(Options {
            untracked: true,
            ..Options::default()
        }),
        Vec::<String>::new()
    );
}

/// Names outside ASCII are read as Git stores them, not as it quotes them in
/// line output, so a change to one is found wherever it comes from.
#[test]
fn changes_to_files_named_outside_ascii_are_found() {
    let repo = Repo::new(&[("libs/lib/src/café.ts", "lib")]);
    write(&repo.root, "libs/lib/src/café.ts", "edited");
    assert_eq!(
        repo.affected(Options {
            uncommitted: true,
            ..Options::default()
        }),
        names(&["app", "lib"])
    );
    let repo = Repo::new(&[]);
    write(&repo.root, "libs/lib/src/naïve.ts", "untracked");
    assert_eq!(
        repo.affected(Options {
            untracked: true,
            ..Options::default()
        }),
        names(&["app", "lib"])
    );
    assert_eq!(repo.committed(), names(&["app", "lib"]));
}

#[test]
fn unreadable_lockfile_and_root_dependency_changes_affect_every_project() {
    for head_lockfile in [Some("not a lockfile"), None] {
        let repo = Repo::new(&[
            (
                "package.json",
                r#"{"name":"root","devDependencies":{"react":"^19.0.0"}}"#,
            ),
            ("pnpm-lock.yaml", &lockfile("19.0.0", "19.0.0")),
        ]);
        write(
            &repo.root,
            "package.json",
            r#"{"name":"root","devDependencies":{"react":"^19.1.0"}}"#,
        );
        match head_lockfile {
            Some(text) => write(&repo.root, "pnpm-lock.yaml", text),
            None => std::fs::remove_file(repo.root.join("pnpm-lock.yaml")).unwrap(),
        }
        assert_eq!(repo.committed(), names(&["app", "lib", "tool"]));
    }
}

#[test]
fn deleted_mandatory_workspace_files_affect_tasks_with_narrow_inputs() {
    for file in [
        ".gitignore",
        ".nxignore",
        "tsconfig.base.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
    ] {
        let repo = Repo::new(&[
            (
                "nx.json",
                r#"{"targetDefaults":{"build":{"command":"echo build","inputs":["{projectRoot}/src/**/*"]},"test":{"command":"echo test","inputs":["{projectRoot}/src/**/*"]}}}"#,
            ),
            (
                "apps/app/project.json",
                r#"{"name":"app","targets":{"build":{},"test":{}}}"#,
            ),
            (
                "libs/lib/project.json",
                r#"{"name":"lib","targets":{"build":{},"test":{}}}"#,
            ),
            (file, "{}"),
        ]);
        std::fs::remove_file(repo.root.join(file)).unwrap();
        let tasks = affected_tasks(&repo);
        assert_eq!(
            tasks.keys().map(String::as_str).collect::<Vec<_>>(),
            ["app:build", "app:test", "lib:build", "lib:test"],
            "{file}"
        );
        assert!(tasks.values().all(|cause| matches!(cause, qk_affected::TaskCause::Touched {reasons} if reasons.iter().any(|reason| matches!(reason,qk_affected::TaskReason::Input {file:changed} if changed==file)))));
    }
}
