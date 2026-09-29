use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/basic")
        .canonicalize()
        .unwrap()
}

fn qk(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qk"))
        .arg("--workspace")
        .arg(fixture())
        .args(args)
        .output()
        .unwrap()
}

fn successful_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
// Nx's graph order: names sorted, then roots by descending length.
fn lists_projects_in_nx_graph_order_and_format() {
    let output = qk(&["show", "projects", "--json"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[\"worker\",\"codegen\",\"core\",\"web\"]\n"
    );
    let output = qk(&["show", "projects"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "worker\ncodegen\ncore\nweb\n"
    );
}

#[test]
fn applies_cli_selectors_and_excludes() {
    assert_eq!(
        successful_json(qk(&[
            "show",
            "projects",
            "-p",
            "tag:scope:*,worker",
            "--exclude",
            "core",
            "--json"
        ])),
        json!(["worker", "web"])
    );
}

#[test]
fn discovers_workspace_from_nested_project_and_shows_normalized_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(fixture().join("apps/web/worker"))
        .args(["show", "project", "web", "--json"])
        .output()
        .unwrap();
    let project = successful_json(output);
    assert_eq!(project["root"], "apps/web");
    assert_eq!(project["targets"]["build"]["cache"], true);
    assert_eq!(
        project["targets"]["build"]["options"]["command"],
        "echo build web"
    );
}

#[test]
fn graph_file_matches_stdout_and_is_relative_to_invocation_directory() {
    let temp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(temp.path())
        .args(["graph", "--file", "graph.json", "--workspace"])
        .arg(fixture())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let file: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("graph.json")).unwrap()).unwrap();
    assert_eq!(file, successful_json(qk(&["graph"])));
    assert_eq!(file["graph"]["nodes"]["web"]["data"]["root"], "apps/web");
}

#[test]
fn unknown_projects_and_unimplemented_commands_fail_without_stdout() {
    for args in [
        vec!["show", "project", "missing"],
        vec!["affected", "-t", "build"],
        vec!["show", "projects", "--affected"],
    ] {
        let output = qk(&args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn help_and_version_work_without_a_workspace() {
    let temp = TempDir::new().unwrap();
    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_qk"))
            .current_dir(temp.path())
            .arg(argument)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("qk"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(temp.path())
        .args(["show", "projects"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no workspace found"));
}

fn planned(output: Output) -> Vec<String> {
    let graph = successful_json(output);
    let mut tasks: Vec<_> = graph["tasks"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    tasks.sort();
    tasks
}

fn qk_in(directory: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(fixture().join(directory))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn nx_shorthand_runs_target_of_named_project() {
    for args in [
        vec!["build", "web", "--dry-run"],
        vec!["web:build", "--dry-run"],
        vec!["run", "web:build", "--dry-run"],
    ] {
        assert!(
            planned(qk(&args)).contains(&"web:build".to_owned()),
            "{args:?}"
        );
    }
    // Global options work on either side of the shorthand.
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .args(["check", "worker", "--dry-run", "--workspace"])
        .arg(fixture())
        .output()
        .unwrap();
    assert_eq!(planned(output), ["worker:check"]);
}

#[test]
fn nx_shorthand_uses_the_most_specific_project_of_the_current_directory() {
    assert_eq!(
        planned(qk_in("apps/web/worker", &["check", "--dry-run"])),
        ["worker:check"]
    );
    assert!(planned(qk_in("apps/web", &["build", "--dry-run"])).contains(&"web:build".to_owned()));
    assert!(
        planned(qk_in("apps/web", &["run", "test", "--dry-run"])).contains(&"web:test".to_owned())
    );

    let outside = qk_in(".", &["build", "--dry-run"]);
    assert!(!outside.status.success());
    assert!(
        String::from_utf8_lossy(&outside.stderr)
            .contains("no project contains the current directory")
    );
}

#[test]
fn nx_shorthand_reports_unknown_targets_and_leaves_nx_commands_alone() {
    let output = qk(&["serv", "web", "--dry-run"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(r#"project "web" has no target "serv""#),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = qk(&["format:check"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn run_many_accepts_space_separated_lists_like_nx() {
    let spaced = planned(qk(&[
        "run-many",
        "-t",
        "build",
        "test",
        "--exclude",
        "core",
        "worker",
        "--dry-run",
    ]));
    let commas = planned(qk(&[
        "run-many",
        "-t",
        "build,test",
        "--exclude=core,worker",
        "--dry-run",
    ]));
    assert_eq!(spaced, commas);
    assert!(spaced.contains(&"web:test".to_owned()));
    assert!(!spaced.iter().any(|task| task.starts_with("worker:")));
}

#[test]
fn run_many_configuration_applies_only_where_defined_like_nx() {
    let tasks = planned(qk(&[
        "run-many",
        "-t",
        "smoke",
        "build",
        "-c",
        "loud",
        "--dry-run",
    ]));
    assert!(
        tasks.contains(&"codegen:smoke:loud".to_owned()),
        "{tasks:?}"
    );
    assert!(tasks.contains(&"web:build".to_owned()), "{tasks:?}");
}
