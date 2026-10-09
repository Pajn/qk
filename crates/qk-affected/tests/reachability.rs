use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use qk_affected::{Analysis, Decision, Kept, Options, TaskDecision, analyse};
use qk_config::Workspace;
use qk_graph::ProjectGraph;
use tempfile::TempDir;

/// `app` imports one of `lib`'s two modules, and its `build` names the `app`
/// config. `app-e2e` drives `app` through `app`'s entry point, and `other`
/// depends on `lib` without narrowing.
struct Repo {
    _temp: TempDir,
    root: PathBuf,
    base: String,
}

const NX_JSON: &str = r#"{
    "qk:reachability": {
        "app": {
            "anchors": ["{projectRoot}/src/main.ts"],
            "sources": ["{projectRoot}/src/**/*", "{workspaceRoot}/libs/*/src/**/*"]
        }
    },
    "qk:affectedProfiles": {"reach": {"reachability": true}}
}"#;

const APP: &str = r#"{
    "name": "app",
    "implicitDependencies": ["lib"],
    "targets": {
        "build": {"command": "echo build", "inputs": ["default", "^default"], "qk:reachability": "app"}
    }
}"#;

impl Repo {
    fn new(extra: &[(&str, &str)]) -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let mut files = vec![
            ("nx.json", NX_JSON),
            ("apps/app/project.json", APP),
            (
                "apps/app/src/main.ts",
                "import { used } from \"../../../libs/lib/src/used\";\nexport const app = used;\n",
            ),
            (
                "apps/app-e2e/project.json",
                r#"{
                    "name": "app-e2e",
                    "implicitDependencies": ["app"],
                    "targets": {
                        "e2e": {
                            "command": "echo e2e",
                            "inputs": ["default", "^default"],
                            "qk:reachability": {
                                "anchors": ["{workspaceRoot}/apps/app/src/main.ts"],
                                "sources": ["{workspaceRoot}/apps/app/src/**/*", "{workspaceRoot}/libs/*/src/**/*"]
                            }
                        }
                    }
                }"#,
            ),
            ("apps/app-e2e/src/spec.ts", "export const spec = 1;\n"),
            (
                "apps/other/project.json",
                r#"{"name": "other", "implicitDependencies": ["lib"], "targets": {"build": {"command": "echo build", "inputs": ["default", "^default"]}}}"#,
            ),
            (
                "libs/lib/project.json",
                r#"{"name": "lib", "targets": {"build": {"command": "echo build", "inputs": ["default"]}}}"#,
            ),
            ("libs/lib/src/used.ts", "export const used = 1;\n"),
            ("libs/lib/src/unused.ts", "export const unused = 1;\n"),
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

    /// Commits the working tree and analyses it from the base, under `profile`.
    fn analyse(&self, profile: Option<&str>) -> Analysis {
        let head = commit(&self.root);
        self.analyse_at(profile, &head)
    }

    fn analyse_at(&self, profile: Option<&str>, head: &str) -> Analysis {
        let workspace = Workspace::load(&self.root).unwrap();
        let graph = ProjectGraph::build(&workspace).unwrap();
        analyse(
            &workspace,
            &graph,
            &Options {
                affected_profile: profile.map(str::to_owned),
                base: Some(self.base.clone()),
                head: Some(head.to_owned()),
                ..Options::default()
            },
        )
        .unwrap()
    }

    fn error(&self, profile: &str) -> String {
        let head = commit(&self.root);
        let workspace = Workspace::load(&self.root).unwrap();
        let graph = ProjectGraph::build(&workspace).unwrap();
        let error = analyse(
            &workspace,
            &graph,
            &Options {
                affected_profile: Some(profile.into()),
                base: Some(self.base.clone()),
                head: Some(head),
                ..Options::default()
            },
        )
        .unwrap_err();
        format!("{error:#}")
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
        .args(["-c", "user.name=qk", "-c", "user.email=qk@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path) -> String {
    git(root, &["add", "--all"]);
    git(
        root,
        &["commit", "--quiet", "--allow-empty", "--message", "change"],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn projects(analysis: &Analysis) -> Vec<&str> {
    analysis.projects.keys().map(String::as_str).collect()
}

fn decisions(analysis: &Analysis) -> BTreeMap<&str, &Decision> {
    analysis
        .reachability
        .iter()
        .map(|(name, decision)| (name.as_str(), decision))
        .collect()
}

/// The decision for the task a project was kept for.
fn kept<'a>(analysis: &'a Analysis, project: &str) -> (&'a str, Option<&'a TaskDecision>) {
    match &analysis.reachability[project] {
        Decision::Kept { task, decision } => (task, decision.as_ref()),
        decision => panic!("{project}: {decision:?}"),
    }
}

#[test]
fn a_change_no_anchor_imports_leaves_the_project_out() {
    let repo = Repo::new(&[]);
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert_eq!(projects(&analysis), ["lib", "other"]);
    let decisions = decisions(&analysis);
    assert!(
        matches!(
            decisions["app"],
            Decision::LeftOut { tasks } if matches!(
                &tasks["app:build"],
                TaskDecision::LeftOut { anchors, changed, .. }
                    if anchors == &["apps/app/src/main.ts"] && changed == &["libs/lib/src/unused.ts"]
            )
        ),
        "{decisions:?}"
    );
    assert!(
        matches!(decisions["app-e2e"], Decision::LeftOut { tasks } if tasks.contains_key("app-e2e:e2e")),
        "{decisions:?}"
    );
    // Ordinary selection is unchanged.
    let ordinary = repo.analyse_at(None, &git(&repo.root, &["rev-parse", "HEAD"]));
    assert_eq!(projects(&ordinary), ["app", "app-e2e", "lib", "other"]);
    assert!(ordinary.reachability.is_empty());
}

#[test]
fn a_change_an_anchor_imports_keeps_the_project_with_the_chain() {
    let repo = Repo::new(&[]);
    write(
        &repo.root,
        "libs/lib/src/used.ts",
        "export const used = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert_eq!(projects(&analysis), ["app", "app-e2e", "lib", "other"]);
    assert!(
        matches!(
            kept(&analysis, "app"),
            ("app:build", Some(TaskDecision::Whole { why: Kept::Reached { anchor, changed, chain } }))
                if anchor == "apps/app/src/main.ts"
                    && changed == "libs/lib/src/used.ts"
                    && chain == &["apps/app/src/main.ts", "libs/lib/src/used.ts"]
        ),
        "{:?}",
        analysis.reachability
    );
    // A project without a narrowed task is not decided.
    assert!(!analysis.reachability.contains_key("other"));
}

#[test]
fn a_change_imports_do_not_carry_keeps_the_project() {
    let repo = Repo::new(&[]);
    write(
        &repo.root,
        "libs/lib/project.json",
        r#"{"name": "lib", "tags": ["changed"], "targets": {"build": {"command": "echo build", "inputs": ["default"]}}}"#,
    );
    let analysis = repo.analyse(Some("reach"));
    assert_eq!(projects(&analysis), ["app", "app-e2e", "lib", "other"]);
    assert!(
        matches!(
            kept(&analysis, "app"),
            (
                "app:build",
                Some(TaskDecision::Whole {
                    why: Kept::Input { .. }
                })
            )
        ),
        "{:?}",
        analysis.reachability
    );
}

#[test]
fn an_import_nothing_places_keeps_the_project() {
    let repo = Repo::new(&[(
        "apps/app/src/main.ts",
        "import { gone } from \"./gone\";\nexport const app = gone;\n",
    )]);
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(
            kept(&analysis, "app"),
            (_, Some(TaskDecision::Whole { why: Kept::Gap { specifier, .. } })) if specifier == "./gone"
        ),
        "{:?}",
        analysis.reachability
    );
    assert!(analysis.projects.contains_key("app"));
}

#[test]
fn a_project_is_decided_by_its_own_changes_too() {
    let repo = Repo::new(&[]);
    write(
        &repo.root,
        "apps/app/src/stray.ts",
        "export const stray = 1;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(decisions(&analysis)["app"], Decision::LeftOut { .. }),
        "a file of its own no anchor imports: {:?}",
        analysis.reachability
    );
    write(
        &repo.root,
        "apps/app/src/main.ts",
        "export const app = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(
            kept(&analysis, "app"),
            (
                _,
                Some(TaskDecision::Whole {
                    why: Kept::Reached { .. }
                })
            )
        ),
        "a changed anchor: {:?}",
        analysis.reachability
    );
}

#[test]
fn a_project_runs_while_any_affected_task_of_it_does() {
    // `test` runs inside a harness that imports `unused`, which the app does not.
    let repo = Repo::new(&[
        (
            "nx.json",
            &NX_JSON.replace(
                r#""app": {"#,
                r#""unit": {
                    "anchors": ["{projectRoot}/test/setup.ts"],
                    "cases": ["{projectRoot}/src/**/*.test.ts"],
                    "sources": ["{projectRoot}/src/**/*", "{projectRoot}/test/**/*", "{workspaceRoot}/libs/*/src/**/*"]
                },
                "app": {"#,
            ),
        ),
        (
            "apps/app/project.json",
            &APP.replace(
                r#""qk:reachability": "app"}"#,
                r#""qk:reachability": "app"},
                "test": {"command": "echo test", "inputs": ["default", "^default"], "qk:reachability": "unit"}"#,
            ),
        ),
        (
            "apps/app/test/setup.ts",
            "import { unused } from \"../../../libs/lib/src/unused\";\nexport const setup = unused;\n",
        ),
        ("apps/app/src/main.test.ts", "export const test = 1;\n"),
    ]);
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(analysis.projects.contains_key("app"));
    assert!(
        matches!(
            kept(&analysis, "app"),
            ("app:test", Some(TaskDecision::Whole { why: Kept::Reached { anchor, .. } }))
                if anchor == "apps/app/test/setup.ts"
        ),
        "{:?}",
        analysis.reachability
    );
}

#[test]
fn the_default_narrows_tasks_that_name_no_config_and_false_declines_it() {
    let repo = Repo::new(&[
        (
            "nx.json",
            &NX_JSON.replace(
                r#""reachability": true"#,
                r#""reachability": {"default": "app"}"#,
            ),
        ),
        (
            "apps/app/project.json",
            r#"{
                "name": "app",
                "implicitDependencies": ["lib"],
                "targets": {
                    "build": {"command": "echo build", "inputs": ["default", "^default"]},
                    "lint": {"command": "echo lint", "inputs": ["default", "^default"], "qk:reachability": false}
                }
            }"#,
        ),
    ]);
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(kept(&analysis, "app"), ("app:lint", None)),
        "{:?}",
        analysis.reachability
    );
    // `other` takes the default too, and has no file it anchors.
    assert!(
        matches!(
            kept(&analysis, "other"),
            ("other:build", Some(TaskDecision::Whole { why: Kept::Unanswered { detail } }))
                if detail.contains("name no file")
        ),
        "{:?}",
        analysis.reachability
    );

    let project = std::fs::read_to_string(repo.root.join("apps/app/project.json")).unwrap();
    write(
        &repo.root,
        "apps/app/project.json",
        &project.replace(r#""qk:reachability": false"#, r#""qk:reachability": "app""#),
    );
    commit(&repo.root);
    let repo = Repo {
        base: git(&repo.root, &["rev-parse", "HEAD"]),
        root: repo.root.clone(),
        _temp: repo._temp,
    };
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 3;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(&decisions(&analysis)["app"], Decision::LeftOut { tasks } if tasks.len() == 2),
        "{:?}",
        analysis.reachability
    );
}

#[test]
fn a_head_other_than_the_checkout_keeps_every_project() {
    let repo = Repo::new(&[]);
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let head = commit(&repo.root);
    write(&repo.root, "apps/other/README.md", "later");
    commit(&repo.root);
    let analysis = repo.analyse_at(Some("reach"), &head);
    assert!(analysis.projects.contains_key("app"));
    assert!(
        matches!(
            kept(&analysis, "app"),
            (_, Some(TaskDecision::Whole { why: Kept::Unanswered { detail } }))
                if detail.contains("not checked out")
        ),
        "{:?}",
        analysis.reachability
    );
}

#[test]
fn invalid_settings_are_refused() {
    let target = |settings: &str| {
        (
            "apps/app/project.json",
            APP.replace(
                r#""qk:reachability": "app""#,
                &format!(r#""qk:reachability": {settings}"#),
            ),
        )
    };
    for (file, message) in [
        (
            target(r#"{"anchors": ["{projectRoot}/src/main.ts"]}"#),
            "needs sources",
        ),
        (
            target(r#"{"sources": ["{projectRoot}/src/**/*"]}"#),
            "needs anchors or cases",
        ),
        (
            target(r#"{"anchors": ["a"], "sources": ["b"], "entry": ["c"]}"#),
            "unknown",
        ),
        (
            target(r#"{"anchors": "a", "sources": ["b"]}"#),
            "array of paths",
        ),
        (target(r#""missing""#), "\"missing\", which is not a config"),
        (
            target("true"),
            "must name a config, be false or be an object",
        ),
        (
            (
                "nx.json",
                NX_JSON.replace(r#""anchors": ["{projectRoot}/src/main.ts"],"#, ""),
            ),
            "nx.json qk:reachability.app needs anchors or cases",
        ),
        (
            (
                "nx.json",
                NX_JSON.replace(
                    r#""reachability": true"#,
                    r#""reachability": {"default": "none"}"#,
                ),
            ),
            "default \"none\" is not a config",
        ),
        (
            (
                "nx.json",
                NX_JSON.replace(
                    r#""reachability": true"#,
                    r#""reachability": {"fallback": "app"}"#,
                ),
            ),
            "unknown reachability field",
        ),
        (
            (
                "apps/other/project.json",
                r#"{"name": "other", "qk:reachability": {"anchors": ["a"], "sources": ["b"]}}"#
                    .into(),
            ),
            "project other declares qk:reachability, which is read from targets",
        ),
    ] {
        let repo = Repo::new(&[(file.0, &file.1)]);
        // A change that does not reach the task still finds the mistake.
        write(&repo.root, "apps/other/README.md", "changed");
        let error = repo.error("reach");
        assert!(error.contains(message), "{}: {error}", file.1);
    }
}

#[test]
fn a_profile_needs_projections_or_reachability() {
    for profile in ["{}", r#"{"reachability": false}"#] {
        let repo = Repo::new(&[(
            "nx.json",
            &format!(r#"{{"qk:affectedProfiles": {{"empty": {profile}}}}}"#),
        )]);
        assert!(
            repo.error("empty")
                .contains("must contain projections or enable reachability")
        );
    }
}

/// `app:visual` has a shell every case renders in and two cases, one of which
/// imports `used`; `app:report` reads its results.
fn visual_repo() -> Repo {
    Repo::new(&[
        (
            "apps/app/project.json",
            r#"{
                "name": "app",
                "implicitDependencies": ["lib"],
                "targets": {
                    "visual": {
                        "command": "echo visual",
                        "inputs": ["default", "^default"],
                        "qk:reachability": {
                            "anchors": ["{projectRoot}/visual/shell.ts"],
                            "cases": ["{projectRoot}/visual/cases/*.ts"],
                            "sources": ["{projectRoot}/src/**/*", "{projectRoot}/visual/**/*.ts", "{workspaceRoot}/libs/*/src/**/*"]
                        }
                    },
                    "report": {"command": "echo report", "dependsOn": ["visual"], "inputs": []}
                }
            }"#,
        ),
        ("apps/app/visual/shell.ts", "export const shell = 1;\n"),
        (
            "apps/app/visual/cases/first.ts",
            "import { used } from \"../../../../libs/lib/src/used\";\nexport const first = used;\n",
        ),
        (
            "apps/app/visual/cases/second.ts",
            "export const second = 1;\n",
        ),
        ("apps/app/visual/harness.json", "{}"),
    ])
}

impl Repo {
    fn tasks(&self, profile: Option<&str>) -> qk_affected::TaskAnalysis {
        use qk_taskgraph::{Request, TaskGraph};
        let head = commit(&self.root);
        let workspace = Workspace::load(&self.root).unwrap();
        let requests: Vec<Request> = ["app:visual", "app:report"]
            .into_iter()
            .map(|id| Request::parse(id).unwrap())
            .collect();
        let graph = TaskGraph::build(&workspace, &requests).unwrap();
        qk_affected::affected_tasks(
            &workspace,
            &graph,
            &Options {
                affected_profile: profile.map(str::to_owned),
                base: Some(self.base.clone()),
                head: Some(head),
                ..Options::default()
            },
        )
        .unwrap()
    }
}

fn task_ids(analysis: &qk_affected::TaskAnalysis) -> Vec<&str> {
    analysis.tasks.keys().map(String::as_str).collect()
}

#[test]
fn a_change_no_case_imports_leaves_the_task_out_with_what_only_it_leads_to() {
    let repo = visual_repo();
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo.tasks(Some("reach"));
    assert!(analysis.tasks.is_empty(), "{:?}", analysis.tasks);
    assert!(
        matches!(
            &analysis.reachability["app:visual"],
            TaskDecision::LeftOut { cases, changed, .. }
                if cases.len() == 2 && changed == &["libs/lib/src/unused.ts"]
        ),
        "{:?}",
        analysis.reachability
    );
    assert!(
        matches!(&analysis.reachability["app:report"], TaskDecision::Through { task } if task == "app:visual"),
        "{:?}",
        analysis.reachability
    );
    let ordinary = {
        write(
            &repo.root,
            "libs/lib/src/unused.ts",
            "export const unused = 3;\n",
        );
        repo.tasks(None)
    };
    assert_eq!(task_ids(&ordinary), ["app:report", "app:visual"]);
}

#[test]
fn only_the_cases_a_change_reaches_are_selected() {
    let repo = visual_repo();
    write(
        &repo.root,
        "libs/lib/src/used.ts",
        "export const used = 2;\n",
    );
    let analysis = repo.tasks(Some("reach"));
    assert_eq!(task_ids(&analysis), ["app:report", "app:visual"]);
    let TaskDecision::Cases { cases, of } = &analysis.reachability["app:visual"] else {
        panic!("{:?}", analysis.reachability);
    };
    assert_eq!(*of, 2);
    assert_eq!(
        cases.keys().collect::<Vec<_>>(),
        ["apps/app/visual/cases/first.ts"]
    );
    assert!(
        matches!(
            &cases["apps/app/visual/cases/first.ts"],
            Kept::Reached { changed, .. } if changed == "libs/lib/src/used.ts"
        ),
        "{cases:?}"
    );
}

#[test]
fn a_changed_case_selects_itself() {
    let repo = visual_repo();
    write(
        &repo.root,
        "apps/app/visual/cases/second.ts",
        "export const second = 2;\n",
    );
    let analysis = repo.tasks(Some("reach"));
    let TaskDecision::Cases { cases, .. } = &analysis.reachability["app:visual"] else {
        panic!("{:?}", analysis.reachability);
    };
    assert_eq!(
        cases.keys().collect::<Vec<_>>(),
        ["apps/app/visual/cases/second.ts"]
    );
}

#[test]
fn the_whole_task_runs_when_its_shell_or_a_non_source_input_changes() {
    let repo = visual_repo();
    write(
        &repo.root,
        "apps/app/visual/shell.ts",
        "export const shell = 2;\n",
    );
    let analysis = repo.tasks(Some("reach"));
    assert!(
        matches!(
            &analysis.reachability["app:visual"],
            TaskDecision::Whole { why: Kept::Reached { anchor, .. } } if anchor == "apps/app/visual/shell.ts"
        ),
        "{:?}",
        analysis.reachability
    );
    write(
        &repo.root,
        "apps/app/visual/harness.json",
        "{\"changed\": true}",
    );
    let analysis = repo.tasks(Some("reach"));
    assert!(
        matches!(
            &analysis.reachability["app:visual"],
            TaskDecision::Whole {
                why: Kept::Input { .. }
            }
        ),
        "{:?}",
        analysis.reachability
    );
}

#[test]
fn task_selection_refuses_profiles_with_projections() {
    use qk_taskgraph::{Request, TaskGraph};
    let repo = visual_repo();
    write(
        &repo.root,
        "nx.json",
        r#"{"qk:affectedProfiles": {"runtime": {"projections": [{"name": "p", "command": ["true"], "sources": ["a/**"], "outputs": ["b/**"]}]}}}"#,
    );
    let head = commit(&repo.root);
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:visual").unwrap()]).unwrap();
    let error = qk_affected::affected_tasks(
        &workspace,
        &graph,
        &Options {
            affected_profile: Some("runtime".into()),
            base: Some(repo.base.clone()),
            head: Some(head),
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("applies to project selection"));
}

#[test]
fn exclusions_take_files_out_of_sources_and_cases() {
    let repo = Repo::new(&[(
        "nx.json",
        &NX_JSON.replace(
            r#""sources": ["{projectRoot}/src/**/*""#,
            r#""sources": ["!{workspaceRoot}/libs/*/src/**/*.test.ts", "{projectRoot}/src/**/*""#,
        ),
    )]);
    write(
        &repo.root,
        "libs/lib/src/unused.test.ts",
        "export const test = 1;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(
            kept(&analysis, "app"),
            (_, Some(TaskDecision::Whole { why: Kept::Input { reason: qk_affected::TaskReason::Input { file } } }))
                if file == "libs/lib/src/unused.test.ts"
        ),
        "an excluded file is not a source: {:?}",
        analysis.reachability
    );
    let repo2 = Repo::new(&[(
        "nx.json",
        &NX_JSON.replace(
            r#""{workspaceRoot}/libs/*/src/**/*"]"#,
            r#""{workspaceRoot}/libs/*/src/**/*", "!{workspaceRoot}/libs/*/src/**/*.test.ts"]"#,
        ),
    )]);
    write(
        &repo2.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo2.analyse(Some("reach"));
    assert!(
        matches!(decisions(&analysis)["app"], Decision::LeftOut { .. }),
        "files the exclusion does not match stay sources: {:?}",
        analysis.reachability
    );

    let visual = visual_repo();
    let project = std::fs::read_to_string(visual.root.join("apps/app/project.json")).unwrap();
    write(
        &visual.root,
        "apps/app/project.json",
        &project.replace(
            r#""cases": ["{projectRoot}/visual/cases/*.ts"]"#,
            r#""cases": ["{projectRoot}/visual/cases/*.ts", "!{projectRoot}/visual/cases/second.ts"]"#,
        ),
    );
    commit(&visual.root);
    let visual = Repo {
        _temp: visual._temp,
        root: visual.root.clone(),
        base: git(&visual.root, &["rev-parse", "HEAD"]),
    };
    write(
        &visual.root,
        "libs/lib/src/used.ts",
        "export const used = 2;\n",
    );
    let analysis = visual.tasks(Some("reach"));
    let TaskDecision::Cases { cases, of } = &analysis.reachability["app:visual"] else {
        panic!("{:?}", analysis.reachability);
    };
    assert_eq!(*of, 1, "the excluded case is not one of the task's cases");
    assert_eq!(
        cases.keys().collect::<Vec<_>>(),
        ["apps/app/visual/cases/first.ts"]
    );
}

#[test]
fn a_manifest_change_dependents_cannot_read_carries_nothing() {
    let manifest = |version: &str, dependencies: &str| {
        format!(
            r#"{{"name": "lib", "version": "{version}", "scripts": {{"test": "echo"}}, "dependencies": {{{dependencies}}}}}"#
        )
    };
    let repo = Repo::new(&[("libs/lib/package.json", &manifest("1.0.0", ""))]);
    write(
        &repo.root,
        "libs/lib/package.json",
        &manifest("1.0.1", "").replace("echo", "echo again"),
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(decisions(&analysis)["app"], Decision::LeftOut { .. }),
        "a version and a script are nothing dependents read: {:?}",
        analysis.reachability
    );
    write(
        &repo.root,
        "libs/lib/package.json",
        &manifest("1.0.1", r#""left-pad": "1.3.0""#).replace("echo", "echo again"),
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(
            kept(&analysis, "app"),
            (
                _,
                Some(TaskDecision::Whole {
                    why: Kept::Input { .. }
                })
            )
        ),
        "a declaration still counts: {:?}",
        analysis.reachability
    );
}

#[test]
fn a_dependency_relinked_by_its_version_runs_the_whole_task() {
    use qk_affected::TaskReason;
    let repo = visual_repo();
    write(
        &repo.root,
        "apps/app/package.json",
        r#"{"name": "app", "dependencies": {"lib": "^1.0.0"}}"#,
    );
    write(
        &repo.root,
        "libs/lib/package.json",
        r#"{"name": "lib", "version": "1.0.0"}"#,
    );
    commit(&repo.root);
    let repo = Repo {
        base: git(&repo.root, &["rev-parse", "HEAD"]),
        root: repo.root.clone(),
        _temp: repo._temp,
    };
    // Out of the range app declares, lib stops being its dependency: what
    // its tasks depend on changed, though no field dependents read did.
    write(
        &repo.root,
        "libs/lib/package.json",
        r#"{"name": "lib", "version": "2.0.0"}"#,
    );
    let analysis = repo.tasks(Some("reach"));
    assert!(
        analysis.tasks.contains_key("app:visual"),
        "{:?}",
        analysis.reachability
    );
    assert!(
        matches!(
            &analysis.reachability["app:visual"],
            TaskDecision::Whole { why: Kept::Input { reason: TaskReason::Relinked { file } } }
                if file == "libs/lib/package.json"
        ),
        "{:?}",
        analysis.reachability
    );
}

#[test]
fn target_defaults_can_name_a_config() {
    let repo = Repo::new(&[
        (
            "nx.json",
            &NX_JSON.replace(
                r#""qk:affectedProfiles""#,
                r#""targetDefaults": {"build": {"qk:reachability": "app"}},
                "qk:affectedProfiles""#,
            ),
        ),
        (
            "apps/app/project.json",
            &APP.replace(r#", "qk:reachability": "app""#, ""),
        ),
        (
            "apps/other/project.json",
            r#"{"name": "other", "implicitDependencies": ["lib"], "targets": {"build": {"command": "echo build", "inputs": ["default", "^default"], "qk:reachability": false}}}"#,
        ),
    ]);
    write(
        &repo.root,
        "libs/lib/src/unused.ts",
        "export const unused = 2;\n",
    );
    let analysis = repo.analyse(Some("reach"));
    assert!(
        matches!(decisions(&analysis)["app"], Decision::LeftOut { .. }),
        "{:?}",
        analysis.reachability
    );
    assert!(analysis.projects.contains_key("other"));
}
