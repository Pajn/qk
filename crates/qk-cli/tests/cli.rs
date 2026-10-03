use std::path::{Path, PathBuf};
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
    for args in [vec!["show", "project", "missing"], vec!["release"]] {
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

#[test]
fn root_configuration_fallback_preserves_the_requested_name_for_dependencies() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("nx.json"), "{}").unwrap();
    for (name, mut target) in [
        (
            "app",
            json!({"dependsOn":["tool:build"], "configurations":{"prod":{}}}),
        ),
        (
            "tool",
            json!({"defaultConfiguration":"dev", "dependsOn":["leaf:build"], "configurations":{"dev":{}}}),
        ),
        ("leaf", json!({"configurations":{"prod":{}}})),
    ] {
        target["executor"] = json!("nx:noop");
        std::fs::create_dir(temp.path().join(name)).unwrap();
        std::fs::write(
            temp.path().join(name).join("project.json"),
            json!({"name":name, "targets":{"build":target}}).to_string(),
        )
        .unwrap();
    }
    for args in [
        vec!["run-many"],
        vec![
            "affected",
            "--files=tool/project.json",
            "--granularity=task",
        ],
    ] {
        let graph = successful_json(
            Command::new(env!("CARGO_BIN_EXE_qk"))
                .arg("--workspace")
                .arg(temp.path())
                .args(args)
                .args(["-t", "build", "-p", "tool", "-c", "prod", "--dry-run"])
                .output()
                .unwrap(),
        );
        assert_eq!(graph["roots"], json!(["tool:build:dev"]));
        assert_eq!(
            graph["tasks"]["tool:build:dev"]["dependencies"],
            json!(["leaf:build:prod"])
        );
        assert!(graph["tasks"]["leaf:build:prod"].is_object());
    }
    let inspect = |args: &[&str]| {
        successful_json(
            Command::new(env!("CARGO_BIN_EXE_qk"))
                .arg("--workspace")
                .arg(temp.path())
                .args(args)
                .output()
                .unwrap(),
        )
    };
    assert_eq!(
        inspect(&["show", "target", "app:build", "-c", "prod", "--json"]),
        inspect(&["show", "target", "app:build:prod", "--json"])
    );
}

#[test]
fn affected_selects_touched_projects_and_their_dependents() {
    let output = qk(&[
        "show",
        "projects",
        "--affected",
        "--files",
        "packages/core/index.js",
        "--json",
    ]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[\"worker\",\"core\",\"web\"]\n"
    );
    let tasks = planned(qk(&[
        "affected",
        "-t",
        "build",
        "--files",
        "packages/core/index.js",
        "--dry-run",
    ]));
    // web:build still depends on codegen:build, which is not affected.
    assert_eq!(tasks, ["codegen:build", "core:build", "web:build"]);
    let output = qk(&["affected", "-t", "build", "--files", "README.md"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no affected tasks"));
}

#[test]
fn show_affected_explains_why() {
    let files = "packages/core/index.js,tools/codegen/x.ts";
    let output = qk(&["show", "affected", "web", "--files", files]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("web is affected because it depends on codegen:"),
        "{text}"
    );
    assert!(text.contains("  web -> codegen (implicit)"), "{text}");
    assert!(text.contains("also depends on affected core"), "{text}");
    assert!(text.contains("  tools/codegen/x.ts changed"), "{text}");

    let report = successful_json(qk(&["show", "affected", "web", "--files", files, "--json"]));
    assert_eq!(report["affected"], true);
    assert_eq!(report["chain"][0]["dependsOn"], "codegen");
    assert_eq!(report["chain"][1]["reasons"][0]["reason"], "file");

    // A root file touches no project here.
    let output = qk(&["show", "affected", "--files", "README.md"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("1 changed file between"), "{text}");
    assert_eq!(text.lines().count(), 1, "{text}");
    let output = qk(&["show", "affected", "core", "--files", "README.md"]);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("core is not affected.")
    );
    assert!(!qk(&["show", "affected", "missing"]).status.success());
}

#[test]
fn task_granularity_selects_tasks_whose_inputs_changed() {
    // README.md is not a production input, so no build reads it.
    let readme = "packages/core/README.md";
    let tasks = planned(qk(&[
        "affected",
        "-t",
        "build",
        "--granularity",
        "project",
        "--files",
        readme,
        "--dry-run",
    ]));
    assert!(tasks.contains(&"core:build".to_owned()), "{tasks:?}");
    let output = qk(&[
        "affected",
        "-t",
        "build",
        "--granularity",
        "task",
        "--files",
        readme,
    ]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no affected tasks"));
    let text = String::from_utf8(
        qk(&[
            "show",
            "tasks",
            "-t",
            "build",
            "test",
            "--affected",
            "--files",
            "packages/core/index.js",
        ])
        .stdout,
    )
    .unwrap();
    assert!(
        text.contains("core:build  input packages/core/index.js changed"),
        "{text}"
    );
    let report = successful_json(qk(&[
        "show",
        "tasks",
        "-t",
        "build",
        "--affected",
        "--files",
        readme,
        "--json",
    ]));
    assert_eq!(report["tasks"], json!({}));
}

#[test]
fn graph_can_include_the_packages_the_lockfile_installs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/parity/fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .args(["--workspace", root.to_str().unwrap(), "graph", "--external"])
        .output()
        .unwrap();
    let graph = &successful_json(output)["graph"];
    let react_dom = "npm:react-dom@19.1.0(react@19.1.0)";
    assert_eq!(
        graph["externalNodes"][react_dom]["data"]["packageName"],
        "react-dom"
    );
    let targets = |source: &str| -> Vec<String> {
        graph["dependencies"][source]
            .as_array()
            .unwrap()
            .iter()
            .map(|edge| edge["target"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(targets("web").contains(&react_dom.to_owned()));
    assert!(targets(react_dom).contains(&"npm:scheduler@0.26.0".to_owned()));
    // Without the flag the graph matches nx graph --file.
    let plain = Command::new(env!("CARGO_BIN_EXE_qk"))
        .args(["--workspace", root.to_str().unwrap(), "graph"])
        .output()
        .unwrap();
    assert!(
        successful_json(plain)["graph"]
            .get("externalNodes")
            .is_none()
    );
}

#[test]
/// Graph filtering follows dependency closures and preserves Nx edge behavior.
fn graph_focus_and_exclude_keep_what_nx_keeps() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/parity/fixture");
    let graph = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_qk"))
            .args(["--workspace", root.to_str().unwrap(), "graph"])
            .args(args)
            .output()
            .unwrap();
        successful_json(output)["graph"].clone()
    };
    let names =
        |object: &Value| -> Vec<String> { object.as_object().unwrap().keys().cloned().collect() };
    // What `nx graph --file=stdout --focus=ui` keeps: ui's dependencies and dependents.
    let focused = graph(&["--focus", "ui"]);
    assert_eq!(
        names(&focused["nodes"]),
        ["@fixture/utils", "tooling", "ui", "web", "web-e2e"]
    );
    assert_eq!(names(&focused["nodes"]), names(&focused["dependencies"]));
    // Nx keeps web-e2e's edge to the excluded web.
    let excluded = graph(&["--focus", "ui", "--exclude", "web", "--print"]);
    assert_eq!(
        names(&excluded["nodes"]),
        ["@fixture/utils", "tooling", "ui", "web-e2e"]
    );
    assert_eq!(excluded["dependencies"]["web-e2e"][0]["target"], "web");
    assert_eq!(graph(&["--file", "stdout"]), graph(&[]));
    let external = graph(&["--focus", "web", "--external"]);
    assert!(external["dependencies"].get("mobile").is_none());

    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .args([
            "--workspace",
            root.to_str().unwrap(),
            "graph",
            "--focus",
            "missing",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn show_projects_filters_by_type_and_target_like_nx() {
    let text = |args: &[&str]| {
        let output = qk(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(text(&["show", "projects", "--type", "app"]), "web\n");
    assert_eq!(
        text(&["show", "projects", "-t", "build", "--sep", " "]),
        "codegen core web\n"
    );
    assert_eq!(
        text(&[
            "show",
            "projects",
            "--with-target",
            "build",
            "--type",
            "lib",
            "--json"
        ]),
        "[\"codegen\",\"core\"]\n"
    );
    assert!(
        !qk(&["show", "projects", "--json", "--sep", ","])
            .status
            .success()
    );
}

/// Inspection resolves configurations and memberships without executing runtime inputs.
#[test]
fn inspects_targets_inputs_and_outputs_without_running_commands() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(
        root.join("nx.json"),
        r#"{"namedInputs":{"source":["{projectRoot}/src/**/*"]}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("src/a.ts"), "source").unwrap();
    std::fs::write(root.join("dist/a.js"), "output").unwrap();
    std::fs::write(root.join("project.json"), json!({"name":"app", "targets":{
        "build":{"command":"echo never", "inputs":["source",{"env":"FOO"},{"runtime":"echo ran > runtime-ran"}],
            "outputs":["{projectRoot}/dist", "{options.missing}"],
            "configurations":{"release":{"command":"echo release"}}}
    }}).to_string()).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_qk"))
            .current_dir(root)
            .args(args)
            .output()
            .unwrap()
    };
    let target = successful_json(run(&[
        "show",
        "target",
        "app:build",
        "-c",
        "release",
        "--json",
    ]));
    assert_eq!(target["options"]["command"], "echo release");
    let inputs = successful_json(run(&["show", "target", "inputs", "build", "--json"]));
    assert!(
        inputs["files"]
            .as_array()
            .unwrap()
            .contains(&json!("src/a.ts"))
    );
    assert_eq!(inputs["environment"], json!(["FOO"]));
    assert_eq!(inputs["runtime"], json!(["echo ran > runtime-ran"]));
    assert!(!root.join("runtime-ran").exists());
    assert!(
        run(&[
            "show",
            "target",
            "inputs",
            "app:build",
            "--check",
            "src",
            "FOO",
            ".",
            "./"
        ])
        .status
        .success()
    );
    assert!(
        !run(&["show", "target", "inputs", "app:build", "--check", "absent"])
            .status
            .success()
    );
    let outputs = successful_json(run(&["show", "target", "outputs", "app:build", "--json"]));
    assert!(
        outputs["expandedOutputs"]
            .as_array()
            .unwrap()
            .contains(&json!("dist/a.js"))
    );
    assert_eq!(outputs["unresolvedOutputs"], json!(["{options.missing}"]));
    assert!(
        run(&[
            "show",
            "target",
            "outputs",
            "app:build",
            "--check",
            "dist/future.js"
        ])
        .status
        .success()
    );
    assert!(
        !run(&[
            "show",
            "target",
            "outputs",
            "app:build",
            "--check",
            "src/a.ts"
        ])
        .status
        .success()
    );
}

/// Stdin file selection preserves spaces and treats empty input as no changes.
#[test]
fn affected_reads_changed_paths_from_stdin() {
    use std::io::Write;
    use std::process::Stdio;
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(root.join("nx.json"), "{}").unwrap();
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::write(
        root.join("app/project.json"),
        r#"{"name":"app","targets":{"build":{"executor":"nx:noop"}}}"#,
    )
    .unwrap();
    std::fs::write(root.join("app/src/a file.ts"), "source").unwrap();
    let run = |args: &[&str], input: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_qk"))
            .current_dir(root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    assert_eq!(
        successful_json(run(
            &["show", "projects", "--stdin", "--json"],
            "app/src/a file.ts\r\n\n"
        )),
        json!(["app"])
    );
    assert_eq!(
        successful_json(run(&["show", "projects", "--stdin", "--json"], "")),
        json!([])
    );
    assert!(
        run(
            &["affected", "-t", "build", "--stdin"],
            "app/src/a file.ts\n"
        )
        .status
        .success()
    );
    assert!(
        !run(
            &[
                "show",
                "projects",
                "--stdin",
                "--files",
                "app/src/a file.ts"
            ],
            ""
        )
        .status
        .success()
    );
}

/// Doctor reports the declared compatibility boundary without running target code.
#[test]
fn doctor_reports_unsupported_features_without_side_effects() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(root.join("nx.json"), r#"{"plugins":["missing-plugin"]}"#).unwrap();
    std::fs::write(
        root.join("project.json"),
        json!({"name":"app","targets":{
            "build":{"command":"echo ran > ran","syncGenerators":["missing:sync"]},
            "native":{"executor":"missing:build"}
        }})
        .to_string(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["errors"], 1);
    assert_eq!(report["warnings"], 2);
    assert_eq!(report["schemaVersion"], 1);
    assert!(
        report["acceptedNoopOptions"]
            .as_array()
            .unwrap()
            .contains(&json!("--batch"))
    );
    assert!(
        report["acceptedNoopOptions"]
            .as_array()
            .unwrap()
            .contains(&json!("--no-tui"))
    );
    assert!(!root.join("ran").exists());
    std::fs::write(
        root.join("project.json"),
        r#"{"name":"app","targets":{"build":{"command":"echo never"}}}"#,
    )
    .unwrap();
    let run = |strict: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_qk"));
        command.current_dir(root).arg("doctor");
        if strict {
            command.arg("--strict");
        }
        command.output().unwrap()
    };
    assert!(run(false).status.success());
    assert_eq!(run(true).status.code(), Some(1));
    std::fs::write(
        root.join("nx.json"),
        r#"{"sync":{"globalGenerators":["missing:sync"]}}"#,
    )
    .unwrap();
    assert_eq!(run(true).status.code(), Some(1));
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["warnings"], 1);
    assert_eq!(
        report["findings"][0]["location"],
        "nx.json.sync.globalGenerators"
    );
    std::fs::write(root.join("nx.json"), "{}").unwrap();
    assert!(run(true).status.success());
}
