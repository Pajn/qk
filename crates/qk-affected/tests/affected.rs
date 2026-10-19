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
fn pnpm_workspace_resolution_keys_reach_tasks_through_the_lockfile_unless_an_input_selects_it() {
    use qk_affected::{TaskCause, TaskReason};
    let workspace_file = "packages:\n  - apps/*\n  - libs/*\ncatalog:\n  react: 19.0.0\n";
    let repo = Repo::new(&[
        ("pnpm-workspace.yaml", workspace_file),
        ("pnpm-lock.yaml", &lockfile("19.0.0", "19.0.0")),
        (
            "project.json",
            r#"{"name": "root", "targets": {"lint": {"command": "echo lint", "inputs": ["default"]}}}"#,
        ),
        (
            "tools/tool/project.json",
            r#"{"name": "tool", "targets": {"build": {"command": "echo build", "inputs": ["default"]}}}"#,
        ),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {
                "build": {"command": "echo build", "inputs": ["default"]},
                "lint": {"command": "echo lint", "inputs": ["default", "{workspaceRoot}/pnpm-workspace.yaml"]}
            }}"#,
        ),
    ]);
    // A catalog bump reaches a task through the lockfile, unless an input
    // selects the workspace file, which then counts whole: a root project's
    // own files do.
    write(
        &repo.root,
        "pnpm-workspace.yaml",
        &workspace_file.replace("19.0.0", "19.1.0"),
    );
    let tasks = affected_tasks_of(&repo, &["lib:build", "lib:lint", "root:lint"]);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["lib:lint", "root:lint"]);
    assert!(
        matches!(
            &tasks["lib:lint"],
            TaskCause::Touched { reasons }
                if matches!(&reasons[..], [TaskReason::Input { file }] if file == "pnpm-workspace.yaml")
        ),
        "{tasks:?}"
    );
    // When the bump changes what a project installs, its tasks are affected
    // through its importer.
    write(&repo.root, "pnpm-lock.yaml", &lockfile("19.0.0", "19.1.0"));
    let tasks = affected_tasks_of(&repo, &["lib:build", "lib:lint", "tool:build"]);
    assert_eq!(
        tasks.keys().collect::<Vec<_>>(),
        ["lib:lint", "tool:build"],
        "{tasks:?}"
    );
    assert!(
        matches!(
            &tasks["tool:build"],
            TaskCause::Touched { reasons }
                if matches!(&reasons[..], [TaskReason::Installs { importer }] if importer == "tools/tool")
        ),
        "{tasks:?}"
    );
    // Keys outside resolution reach every task.
    write(
        &repo.root,
        "pnpm-workspace.yaml",
        "packages:\n  - apps/*\ncatalog:\n  react: 19.1.0\n",
    );
    let tasks = affected_tasks_of(&repo, &["lib:build", "lib:lint"]);
    assert!(
        matches!(
            &tasks["lib:build"],
            TaskCause::Touched { reasons } if matches!(&reasons[..], [TaskReason::WorkspaceFile])
        ),
        "{tasks:?}"
    );
}

#[test]
fn an_input_naming_pnpm_workspace_touches_its_project_on_any_change() {
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
    affected_tasks_of(repo, &["app:build", "app:test", "lib:build", "lib:test"])
}

fn affected_tasks_of(repo: &Repo, tasks: &[&str]) -> BTreeMap<String, qk_affected::TaskCause> {
    use qk_taskgraph::{Request, TaskGraph};
    let workspace = Workspace::load(&repo.root).unwrap();
    let requests: Vec<Request> = tasks.iter().map(|id| Request::parse(id).unwrap()).collect();
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

/// Empty change lists preserve revision metadata without reading task inputs.
#[test]
fn empty_task_changes_skip_input_resolution() {
    use qk_taskgraph::{Request, TaskGraph};
    let repo = Repo::new(&[
        ("nx.json", "{}"),
        (
            "apps/app/project.json",
            r#"{"name":"app","targets":{"build":{"command":"echo build",
                "inputs":[{"json":"{projectRoot}/meta.json","unsupported":true}]}}}"#,
        ),
        ("apps/app/meta.json", "{}"),
    ]);
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    for options in [
        Options {
            base: Some(repo.base.clone()),
            head: Some(repo.base.clone()),
            ..Options::default()
        },
        Options {
            base: Some(repo.base.clone()),
            explicit_files: true,
            ..Options::default()
        },
        Options {
            base: Some(repo.base.clone()),
            uncommitted: true,
            ..Options::default()
        },
        Options {
            base: Some(repo.base.clone()),
            untracked: true,
            ..Options::default()
        },
    ] {
        let projects = qk_affected::analyse(
            &workspace,
            &ProjectGraph::build(&workspace).unwrap(),
            &options,
        )
        .unwrap();
        let tasks = qk_affected::affected_tasks(&workspace, &graph, &options).unwrap();
        assert!(tasks.files.is_empty());
        assert!(tasks.tasks.is_empty());
        assert_eq!(tasks.base, projects.base);
        assert_eq!(tasks.head, projects.head);
        assert_eq!(
            serde_json::to_value(tasks.range).unwrap(),
            serde_json::to_value(projects.range).unwrap()
        );
    }
    // A nonempty list still resolves inputs, including their validation.
    assert!(
        qk_affected::affected_tasks(
            &workspace,
            &graph,
            &Options {
                files: vec!["apps/app/src/main.ts".into()],
                ..Options::default()
            }
        )
        .is_err()
    );
}

/// Symlink target edits and deletions affect consumers with narrow source globs.
#[cfg(unix)]
#[test]
fn symlink_targets_affect_tasks_that_read_the_link() {
    use qk_taskgraph::{Request, TaskGraph};
    for mode in ["file", "directory", "json", "nested", "root"] {
        let inputs = if mode == "json" {
            serde_json::json!([{"json":"{projectRoot}/selected/link"}])
        } else {
            serde_json::json!(["{projectRoot}/selected/**/*"])
        };
        let project = serde_json::json!({"name":"app","targets":{"build":{
            "command":"echo build", "inputs":inputs,
        }}})
        .to_string();
        let repo = Repo::new(&[
            ("nx.json", "{}"),
            ("apps/app/project.json", &project),
            ("data/value.json", r#"{"value":1}"#),
        ]);
        std::fs::create_dir(repo.root.join("apps/app/selected")).unwrap();
        if mode == "nested" {
            write(&repo.root, "other/value.json", r#"{"value":1}"#);
            std::fs::remove_file(repo.root.join("data/value.json")).unwrap();
            std::os::unix::fs::symlink("../other/value.json", repo.root.join("data/value.json"))
                .unwrap();
        }
        let actual = if mode == "nested" {
            "other/value.json"
        } else {
            "data/value.json"
        };
        let target = if mode == "root" {
            "../../.."
        } else if mode == "directory" || mode == "nested" {
            "../../../data"
        } else {
            "../../../data/value.json"
        };
        std::os::unix::fs::symlink(target, repo.root.join("apps/app/selected/link")).unwrap();
        commit(&repo.root);
        let workspace = Workspace::load(&repo.root).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        for deleted in [false, true] {
            if deleted {
                std::fs::remove_file(repo.root.join(actual)).unwrap();
            } else {
                write(&repo.root, actual, r#"{"value":2}"#);
            }
            let tasks = qk_affected::affected_tasks(
                &workspace,
                &graph,
                &Options {
                    files: vec![actual.into()],
                    ..Options::default()
                },
            )
            .unwrap()
            .tasks;
            assert_eq!(
                tasks.keys().map(String::as_str).collect::<Vec<_>>(),
                ["app:build"],
                "{mode}"
            );
        }
    }
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
    // A field the input does not select changes nothing it keys.
    write(
        &repo.root,
        "apps/app/meta.json",
        r#"{"version": 1, "comment": "x"}"#,
    );
    assert!(affected_tasks(&repo).is_empty());
    write(&repo.root, "apps/app/meta.json", r#"{"version": 2}"#);
    let tasks = affected_tasks(&repo);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

/// A path whose glob characters are escaped names one file, which is read
/// without its escapes.
#[test]
fn an_escaped_json_input_path_names_its_file() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": [{"json": "{projectRoot}/\\(group\\)/meta.json", "fields": ["version"]}]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}}}"#,
        ),
        ("apps/app/(group)/meta.json", r#"{"version": 1}"#),
    ]);
    write(
        &repo.root,
        "apps/app/(group)/meta.json",
        r#"{"version": 1, "comment": "x"}"#,
    );
    assert!(affected_tasks_of(&repo, &["app:build"]).is_empty());
    write(
        &repo.root,
        "apps/app/(group)/meta.json",
        r#"{"version": 2}"#,
    );
    let tasks = affected_tasks_of(&repo, &["app:build"]);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

#[test]
fn a_json_input_does_not_narrow_a_file_other_inputs_select_whole() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": ["default", {"json": "{projectRoot}/meta.json", "fields": ["version"]}]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}}}"#,
        ),
        ("apps/app/meta.json", r#"{"version": 1}"#),
    ]);
    write(
        &repo.root,
        "apps/app/meta.json",
        r#"{"version": 1, "comment": "x"}"#,
    );
    let tasks = affected_tasks_of(&repo, &["app:build"]);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

#[test]
fn a_json_input_selects_fields_of_a_yaml_file() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": [{"json": "{projectRoot}/meta.yaml", "fields": ["version"]}]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}}}"#,
        ),
        ("apps/app/meta.yaml", "version: 1\n"),
    ]);
    write(&repo.root, "apps/app/meta.yaml", "version: 1\ncomment: x\n");
    assert!(affected_tasks_of(&repo, &["app:build"]).is_empty());
    write(&repo.root, "apps/app/meta.yaml", "version: 2\ncomment: x\n");
    let tasks = affected_tasks_of(&repo, &["app:build"]);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

#[test]
fn a_json_input_it_cannot_read_counts_any_change() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": [{"json": "{projectRoot}/meta.toml", "fields": ["version"]}]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}}}"#,
        ),
        ("apps/app/meta.toml", "version = 1\n"),
    ]);
    write(
        &repo.root,
        "apps/app/meta.toml",
        "version = 1\ncomment = \"x\"\n",
    );
    let tasks = affected_tasks_of(&repo, &["app:build"]);
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

/// A dependency's manifest affects its dependents' tasks by the fields their
/// keys read, and its own tasks by all of it.
#[test]
fn dependency_manifests_affect_dependents_by_what_they_decide() {
    let repo = |manifest: &str| {
        Repo::new(&[
            (
                "nx.json",
                r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"]},
                                       "test": {"command": "echo test", "inputs": ["{projectRoot}/src/**/*"]}}}"#,
            ),
            (
                "apps/app/project.json",
                r#"{"name": "app", "implicitDependencies": ["lib"], "targets": {"build": {}, "test": {}}}"#,
            ),
            (
                "libs/lib/project.json",
                r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
            ),
            ("libs/lib/package.json", manifest),
        ])
    };
    let before = r#"{"name": "lib", "version": "1.0.0", "exports": "./src/index.ts"}"#;

    let bumped = repo(before);
    write(
        &bumped.root,
        "libs/lib/package.json",
        r#"{"name": "lib", "version": "1.1.0", "exports": "./src/index.ts"}"#,
    );
    let tasks = affected_tasks(&bumped);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["lib:build", "lib:test"]);

    let moved = repo(before);
    write(
        &moved.root,
        "libs/lib/package.json",
        r#"{"name": "lib", "version": "1.0.0", "exports": "./src/other.ts"}"#,
    );
    let tasks = affected_tasks(&moved);
    assert_eq!(
        tasks.keys().collect::<Vec<_>>(),
        ["app:build", "app:test", "lib:build", "lib:test"]
    );
    assert!(matches!(
        &tasks["app:build"],
        qk_affected::TaskCause::Touched { reasons }
            if matches!(&reasons[..], [qk_affected::TaskReason::Input { file }] if file == "libs/lib/package.json")
    ));
}

/// A version that moves a workspace package into or out of the range a
/// dependent declares for it, or a name that stops or starts matching what the
/// dependent declares, adds or removes that dependency, so every task of the
/// dependent is affected, although the version itself is not keyed.
#[test]
fn versions_crossing_a_dependents_range_affect_its_tasks() {
    let repo = |version: &str| {
        Repo::new(&[
            (
                "nx.json",
                r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"]},
                                       "test": {"command": "echo test", "inputs": ["{projectRoot}/src/**/*"]}}}"#,
            ),
            (
                "apps/app/project.json",
                r#"{"name": "app", "targets": {"build": {}, "test": {}}}"#,
            ),
            (
                "apps/app/package.json",
                r#"{"name": "app", "dependencies": {"lib": "^1.0.0"}}"#,
            ),
            (
                "libs/lib/project.json",
                r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
            ),
            (
                "libs/lib/package.json",
                &format!(r#"{{"name": "lib", "version": "{version}"}}"#),
            ),
        ])
    };
    for (before, after) in [("2.0.0", "1.1.0"), ("1.0.0", "2.0.0")] {
        let repo = repo(before);
        write(
            &repo.root,
            "libs/lib/package.json",
            &format!(r#"{{"name": "lib", "version": "{after}"}}"#),
        );
        let tasks = affected_tasks(&repo);
        assert_eq!(
            tasks.keys().collect::<Vec<_>>(),
            ["app:build", "app:test", "lib:build", "lib:test"],
            "{before} -> {after}"
        );
        assert!(matches!(
            &tasks["app:build"],
            qk_affected::TaskCause::Touched { reasons }
                if reasons.iter().any(|reason| matches!(reason, qk_affected::TaskReason::Relinked { file } if file == "libs/lib/package.json"))
        ));
    }
    // Renamed, the package no longer answers to what app declares.
    let renamed = repo("1.0.0");
    write(
        &renamed.root,
        "libs/lib/package.json",
        r#"{"name": "renamed", "version": "1.0.0"}"#,
    );
    let tasks = affected_tasks(&renamed);
    assert_eq!(
        tasks.keys().collect::<Vec<_>>(),
        ["app:build", "app:test", "lib:build", "lib:test"]
    );
    // Within the range, the dependency stays and the version alone does not count.
    let repo = repo("1.0.0");
    write(
        &repo.root,
        "libs/lib/package.json",
        r#"{"name": "lib", "version": "1.1.0"}"#,
    );
    assert_eq!(
        affected_tasks(&repo).keys().collect::<Vec<_>>(),
        ["lib:build", "lib:test"]
    );
}

/// As in Nx, the root manifest is not an input of every task: editing its
/// scripts affects no task outside the root project.
#[test]
fn root_manifest_scripts_affect_no_other_projects_tasks() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"]},
                                   "test": {"command": "echo test", "inputs": ["{projectRoot}/src/**/*"]}}}"#,
        ),
        (
            "package.json",
            r#"{"name": "root", "scripts": {"lint": "echo one"}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "implicitDependencies": ["lib"], "targets": {"build": {}, "test": {}}}"#,
        ),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
        ),
    ]);
    write(
        &repo.root,
        "package.json",
        r#"{"name": "root", "scripts": {"lint": "echo two"}}"#,
    );
    assert!(affected_tasks(&repo).is_empty());
}

/// As in Nx, a project's filesets select only the files it owns, so those of a
/// project nested in its root reach only that project's tasks.
#[test]
fn nested_projects_files_affect_only_their_own_tasks() {
    const TASKS: &[&str] = &["app:test", "lib:test", "plugin:test", "root:test"];
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"test": {"command": "echo test"}}}"#,
        ),
        (
            "project.json",
            r#"{"name": "root", "targets": {"test": {}}}"#,
        ),
        ("tool.txt", "before"),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"test": {}}}"#,
        ),
        ("apps/app/src/main.ts", "before"),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"test": {}}}"#,
        ),
        ("libs/lib/src/index.ts", "before"),
        (
            "libs/lib/plugin/project.json",
            r#"{"name": "plugin", "targets": {"test": {}}}"#,
        ),
        ("libs/lib/plugin/src/index.ts", "before"),
    ]);
    write(&repo.root, "apps/app/src/main.ts", "after");
    write(&repo.root, "libs/lib/plugin/src/index.ts", "after");
    std::fs::remove_file(repo.root.join("libs/lib/plugin/src/index.ts")).unwrap();
    write(&repo.root, "libs/lib/plugin/src/added.ts", "added");
    let tasks = affected_tasks_of(&repo, TASKS);
    assert_eq!(
        tasks.keys().collect::<Vec<_>>(),
        ["app:test", "plugin:test"]
    );
    write(&repo.root, "tool.txt", "after");
    write(&repo.root, "libs/lib/src/index.ts", "after");
    let tasks = affected_tasks_of(&repo, TASKS);
    assert_eq!(
        tasks.keys().collect::<Vec<_>>(),
        ["app:test", "lib:test", "plugin:test", "root:test"]
    );
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

/// Ignored deleted artifacts do not invalidate every task; explicit JSON reads still count.
#[test]
fn ignored_deleted_manifests_only_affect_explicit_consumers() {
    for name in ["package.json", "project.json"] {
        for explicit in [false, true] {
            let file = format!("apps/app/artifacts/{name}");
            let inputs = if explicit {
                serde_json::json!([{"json":format!("{{projectRoot}}/artifacts/{name}")}])
            } else {
                serde_json::json!(["{projectRoot}/src/**/*"])
            };
            let config = serde_json::json!({"targetDefaults":{
                "build":{"command":"echo build","inputs":inputs},
                "test":{"command":"echo test","inputs":["{projectRoot}/src/**/*"]}
            }})
            .to_string();
            let repo = Repo::new(&[
                ("nx.json", &config),
                (".nxignore", "apps/app/artifacts/\n"),
                (
                    "apps/app/project.json",
                    r#"{"name":"app","targets":{"build":{},"test":{}}}"#,
                ),
                (
                    "libs/lib/project.json",
                    r#"{"name":"lib","targets":{"build":{},"test":{}}}"#,
                ),
                (&file, r#"{"version":1}"#),
            ]);
            std::fs::remove_file(repo.root.join(&file)).unwrap();
            let tasks = affected_tasks(&repo);
            let expected = if explicit { vec!["app:build"] } else { vec![] };
            assert_eq!(
                tasks.keys().map(String::as_str).collect::<Vec<_>>(),
                expected,
                "{file}, explicit={explicit}"
            );
        }
    }
}

/// `app:serve` is continuous and builds `lib` first. `e2e` lives in its own
/// project, with only its own files as inputs, and is served by `app:serve`
/// as `dependsOn` declares it.
fn served_repo(serve_dependency: &str) -> Repo {
    let e2e = format!(
        r#"{{"name": "app-e2e", "targets": {{"e2e": {{"command": "echo e2e", "inputs": ["default"], "dependsOn": [{serve_dependency}]}}}}}}"#
    );
    Repo::new(&[
        (
            "nx.json",
            r#"{"namedInputs": {"default": ["{projectRoot}/**/*"]},
                "targetDefaults": {"build": {"command": "echo build", "inputs": ["default"], "outputs": ["{projectRoot}/dist"]}}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "implicitDependencies": ["lib"], "targets": {"serve": {"command": "echo serve", "continuous": true, "inputs": ["default"], "dependsOn": ["^build"]}}}"#,
        ),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}}}"#,
        ),
        ("apps/app-e2e/project.json", &e2e),
        ("apps/app-e2e/src/app.spec.ts", "a test"),
    ])
}

#[test]
fn a_continuous_dependency_counts_its_inputs_as_the_dependent_s_own() {
    use qk_affected::{TaskCause, TaskReason};
    let repo = served_repo(r#"{"projects": ["app"], "target": "serve"}"#);
    write(&repo.root, "apps/app/src/main.ts", "changed");
    let tasks = affected_tasks_of(&repo, &["app-e2e:e2e"]);
    assert!(matches!(
        &tasks["app-e2e:e2e"],
        TaskCause::Touched { reasons }
            if reasons.iter().any(|reason| matches!(reason, TaskReason::Input { file } if file == "apps/app/src/main.ts"))
    ));

    // A build the server depends on does not carry over: its outputs are the
    // server's to read, not the e2e task's.
    let repo = served_repo(r#"{"projects": ["app"], "target": "serve"}"#);
    write(&repo.root, "libs/lib/src/index.ts", "changed");
    let tasks = affected_tasks_of(&repo, &["app-e2e:e2e"]);
    assert!(tasks.contains_key("lib:build"));
    assert!(!tasks.contains_key("app-e2e:e2e"), "{tasks:?}");
}

#[test]
fn a_continuous_dependency_declared_without_inputs_counts_nothing() {
    let repo = served_repo(r#"{"projects": ["app"], "target": "serve", "inputs": false}"#);
    write(&repo.root, "apps/app/src/main.ts", "changed");
    let tasks = affected_tasks_of(&repo, &["app-e2e:e2e"]);
    assert!(!tasks.contains_key("app-e2e:e2e"), "{tasks:?}");

    // Another entry naming the server with its inputs keeps them.
    let repo = served_repo(
        r#"{"projects": ["app"], "target": "serve", "inputs": false}, {"projects": ["app"], "target": "serve"}"#,
    );
    write(&repo.root, "apps/app/src/main.ts", "changed");
    let tasks = affected_tasks_of(&repo, &["app-e2e:e2e"]);
    assert!(tasks.contains_key("app-e2e:e2e"), "{tasks:?}");
}

/// `check` selects `feature` with `dependencies: true`. `feature` depends on
/// `util`, and `app`, whose task it is, on `feature` and `extra`.
fn selected_dependencies_repo() -> Repo {
    Repo::new(&[
        (
            "nx.json",
            r#"{"namedInputs": {"default": ["{projectRoot}/**/*"]}}"#,
        ),
        (
            "apps/app/project.json",
            r#"{"name": "app", "implicitDependencies": ["feature", "extra"], "targets": {"check": {"command": "echo check",
                "inputs": [{"input": "default", "projects": ["feature"], "dependencies": true}]}}}"#,
        ),
        (
            "libs/feature/project.json",
            r#"{"name": "feature", "implicitDependencies": ["util"]}"#,
        ),
        ("libs/feature/src/index.ts", "feature"),
        ("libs/util/project.json", r#"{"name": "util"}"#),
        ("libs/util/src/index.ts", "util"),
        ("libs/extra/project.json", r#"{"name": "extra"}"#),
        ("libs/extra/src/index.ts", "extra"),
    ])
}

#[test]
fn an_input_selecting_projects_with_dependencies_reads_what_they_depend_on() {
    for (file, affected) in [
        ("libs/util/src/index.ts", true),
        ("libs/feature/src/index.ts", false),
        ("libs/extra/src/index.ts", false),
    ] {
        let repo = selected_dependencies_repo();
        write(&repo.root, file, "changed");
        let tasks = affected_tasks_of(&repo, &["app:check"]);
        assert_eq!(
            tasks.contains_key("app:check"),
            affected,
            "{file}: {tasks:?}"
        );
    }
}

#[test]
fn inputs_marked_always_read_as_without_it() {
    let repo = || {
        Repo::new(&[
            (
                "nx.json",
                r#"{"namedInputs": {"default": ["{projectRoot}/**/*"]},
                    "targetDefaults": {"check": {"command": "echo check", "inputs": [
                        {"fileset": "{projectRoot}/src/**/*", "always": true},
                        {"json": "{projectRoot}/meta.json", "fields": ["version"], "always": true},
                        {"input": "default", "projects": ["lib"], "always": true}
                    ]}}}"#,
            ),
            (
                "apps/app/project.json",
                r#"{"name": "app", "targets": {"check": {}}}"#,
            ),
            ("apps/app/meta.json", r#"{"version": 1}"#),
        ])
    };
    for (file, text) in [
        ("apps/app/src/main.ts", "changed"),
        ("apps/app/meta.json", r#"{"version": 2}"#),
        ("libs/lib/src/index.ts", "changed"),
    ] {
        let repo = repo();
        write(&repo.root, file, text);
        let tasks = affected_tasks_of(&repo, &["app:check"]);
        assert!(tasks.contains_key("app:check"), "{file}: {tasks:?}");
    }
    let repo = repo();
    write(&repo.root, "apps/app/README.md", "not an input");
    assert!(affected_tasks_of(&repo, &["app:check"]).is_empty());
}

/// `lib:build` declares outputs in a tracked folder; `app:check` depends on it
/// and reads its outputs as `inputs` declares, or not at all.
fn reading_repo(inputs: &str) -> Repo {
    let app = format!(
        r#"{{"name": "app", "implicitDependencies": ["lib"], "targets": {{"check": {{"command": "echo check", "dependsOn": ["^build"], "inputs": {inputs}}}}}}}"#
    );
    Repo::new(&[
        (
            "nx.json",
            r#"{"namedInputs": {"default": ["{projectRoot}/**/*"]}}"#,
        ),
        ("apps/app/project.json", &app),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"], "outputs": ["{projectRoot}/out"]}}}"#,
        ),
        ("libs/lib/out/index.js", "built"),
    ])
}

#[test]
fn a_task_is_affected_through_a_dependency_only_when_it_reads_its_outputs() {
    use qk_affected::TaskCause;
    for (inputs, reads) in [
        (r#"["{projectRoot}/**/*"]"#, false),
        (r#"[{"dependentTasksOutputFiles": "**/*.js"}]"#, true),
        (r#"["{workspaceRoot}/libs/lib/out/**/*"]"#, true),
    ] {
        let repo = reading_repo(inputs);
        write(&repo.root, "libs/lib/src/index.ts", "changed");
        let tasks = affected_tasks_of(&repo, &["app:check"]);
        assert!(tasks.contains_key("lib:build"), "{inputs}: {tasks:?}");
        if reads {
            assert!(
                matches!(&tasks["app:check"], TaskCause::Reads { task } if task == "lib:build"),
                "{inputs}: {tasks:?}"
            );
        } else {
            assert!(!tasks.contains_key("app:check"), "{inputs}: {tasks:?}");
        }
    }
}

/// As in Nx, an `includeIgnored` fileset matches a changed path whether
/// ignore rules leave it out or not, and whether or not it is still there.
#[test]
fn include_ignored_filesets_match_ignored_changes() {
    let repo = Repo::new(&[
        (
            "nx.json",
            r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": [{"fileset": "{projectRoot}/generated/**/*", "includeIgnored": true}, {"fileset": "!{projectRoot}/generated/**/*.local.json", "includeIgnored": true}]},
                                   "test": {"command": "echo test", "inputs": ["{projectRoot}/**/*"]}}}"#,
        ),
        (".nxignore", "apps/app/generated/\n"),
        (
            "apps/app/project.json",
            r#"{"name": "app", "targets": {"build": {}, "test": {}}}"#,
        ),
        ("apps/app/generated/schema.json", "before"),
        ("apps/app/generated/dev.local.json", "before"),
        (
            "libs/lib/project.json",
            r#"{"name": "lib", "targets": {"build": {}, "test": {}}}"#,
        ),
    ]);
    write(&repo.root, "apps/app/generated/dev.local.json", "after");
    assert!(affected_tasks_of(&repo, &["app:build", "app:test"]).is_empty());
    write(&repo.root, "apps/app/generated/schema.json", "after");
    assert_eq!(
        affected_tasks_of(&repo, &["app:build", "app:test"])
            .keys()
            .collect::<Vec<_>>(),
        ["app:build"]
    );
    // A path named explicitly counts even when it is gone.
    use qk_taskgraph::{Request, TaskGraph};
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    let tasks = qk_affected::affected_tasks(
        &workspace,
        &graph,
        &Options {
            files: vec!["apps/app/generated/removed.json".into()],
            explicit_files: true,
            ..Options::default()
        },
    )
    .unwrap()
    .tasks;
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build"]);
}

/// As in Nx, an `includeIgnored` fileset reading a dependency's declared
/// outputs carries a change of that dependency to the task.
#[test]
fn include_ignored_filesets_read_their_producers_outputs() {
    let repo = |extension: &str| {
        Repo::new(&[
            (
                "nx.json",
                r#"{"targetDefaults": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"], "outputs": ["{workspaceRoot}/dist/{projectRoot}/**/*.js"]}}}"#,
            ),
            (
                "apps/app/project.json",
                &format!(
                    r#"{{"name": "app", "implicitDependencies": ["lib"], "targets": {{"build": {{"dependsOn": ["^build"],
                        "inputs": [{{"fileset": "{{workspaceRoot}}/dist/libs/lib/**/*.{extension}", "includeIgnored": true}}]}}}}}}"#
                ),
            ),
            (
                "libs/lib/project.json",
                r#"{"name": "lib", "targets": {"build": {}}}"#,
            ),
        ])
    };
    let reading = repo("js");
    write(&reading.root, "libs/lib/src/index.ts", "changed");
    let tasks = affected_tasks_of(&reading, &["app:build"]);
    assert_eq!(tasks.keys().collect::<Vec<_>>(), ["app:build", "lib:build"]);
    assert!(
        matches!(&tasks["app:build"], qk_affected::TaskCause::Reads { task } if task == "lib:build")
    );
    // Files with another extension than any output's are none of them.
    let other = repo("css");
    write(&other.root, "libs/lib/src/index.ts", "changed");
    assert_eq!(
        affected_tasks_of(&other, &["app:build"])
            .keys()
            .collect::<Vec<_>>(),
        ["lib:build"]
    );
}

/// A project root with glob characters, as a route group gives one, names
/// its directory in filesets, json inputs and includeIgnored filesets.
#[test]
fn a_project_root_with_glob_characters_is_a_directory_in_inputs() {
    for (inputs, file, text) in [
        (r#"["{projectRoot}/src/**/*"]"#, "src/page.ts", "changed"),
        (
            r#"[{"json": "{projectRoot}/meta.json", "fields": ["version"]}]"#,
            "meta.json",
            r#"{"version": 2}"#,
        ),
        (
            r#"[{"fileset": "{projectRoot}/src/**/*", "includeIgnored": true}]"#,
            "src/page.ts",
            "changed",
        ),
    ] {
        let project = format!(
            r#"{{"name": "marketing", "targets": {{"build": {{"command": "echo build", "inputs": {inputs}}}}}}}"#
        );
        let repo = Repo::new(&[
            ("apps/(marketing)/project.json", &project),
            ("apps/(marketing)/src/page.ts", "page"),
            ("apps/(marketing)/meta.json", r#"{"version": 1}"#),
        ]);
        write(&repo.root, &format!("apps/(marketing)/{file}"), text);
        let tasks = affected_tasks_of(&repo, &["marketing:build"]);
        assert!(tasks.contains_key("marketing:build"), "{inputs}: {tasks:?}");
    }
}

/// A fileset from an escaped project root reads that directory alone, so a
/// dependency writing elsewhere below the same parent is no producer of it.
#[test]
fn a_fileset_from_an_escaped_root_reads_no_sibling_outputs() {
    let repo = Repo::new(&[
        (
            "apps/(marketing)/project.json",
            r#"{"name": "marketing", "implicitDependencies": ["other"], "targets": {"build": {"command": "echo build", "dependsOn": ["^build"], "inputs": ["{projectRoot}/**/*"]}}}"#,
        ),
        (
            "apps/other/project.json",
            r#"{"name": "other", "targets": {"build": {"command": "echo build", "inputs": ["{projectRoot}/src/**/*"], "outputs": ["{projectRoot}/dist"]}}}"#,
        ),
        ("apps/other/src/index.ts", "index"),
    ]);
    write(&repo.root, "apps/other/src/index.ts", "changed");
    assert_eq!(
        affected_tasks_of(&repo, &["marketing:build"])
            .keys()
            .collect::<Vec<_>>(),
        ["other:build"]
    );
}
