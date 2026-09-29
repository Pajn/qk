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
fn lists_projects_as_clean_json_or_sorted_lines() {
    assert_eq!(
        successful_json(qk(&["show", "projects", "--json"])),
        json!(["codegen", "core", "web", "worker"])
    );
    let output = qk(&["show", "projects"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "codegen\ncore\nweb\nworker\n"
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
        json!(["web", "worker"])
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
