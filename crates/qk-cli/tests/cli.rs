use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

// Fixture repositories must not inherit hook locations, signing or global hooks.
fn isolated_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(name);
    }
    command
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/basic")
        .canonicalize()
        .unwrap()
}

fn qk(args: &[&str]) -> Output {
    isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
        let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
            .current_dir(temp.path())
            .arg(argument)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("qk"));
    }
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
fn camel_case_flags_mean_their_kebab_case_spelling_like_nx() {
    let kebab = planned(qk(&[
        "run-many",
        "-t",
        "build",
        "--output-style=static",
        "--exclude-task-dependencies",
        "--dry-run",
    ]));
    let camel = planned(qk(&[
        "run-many",
        "-t",
        "build",
        "--outputStyle=static",
        "--excludeTaskDependencies",
        "--dryRun",
    ]));
    assert_eq!(camel, kebab);
    // Flags after `--` belong to the task.
    let forwarded = qk(&["run", "web:build", "--dry-run", "--", "--outputStyle"]);
    let plan = String::from_utf8(forwarded.stdout).unwrap();
    assert!(plan.contains("\"--outputStyle\""), "{plan}");
}

#[test]
fn camel_case_spellings_of_nx_options_are_accepted() {
    for flag in [
        "--skipNxCache",
        "--disableNxCache",
        "--skipRemoteCache",
        "--disableRemoteCache",
        "--maxParallel=2",
        "--nxBail",
        "--nxIgnoreCycles",
        "--skipSync",
        "--useAgents",
        "--tuiAutoExit=false",
    ] {
        let output = qk(&["run-many", "-t", "build", flag, "--dry-run"]);
        assert!(
            output.status.success(),
            "{flag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = qk(&["reset", "--onlyCache", "--help"]);
    assert!(output.status.success());
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
            isolated_command(env!("CARGO_BIN_EXE_qk"))
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
            isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    let plain = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
        let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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

    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
        isolated_command(env!("CARGO_BIN_EXE_qk"))
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
        let mut child = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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
        let mut command = isolated_command(env!("CARGO_BIN_EXE_qk"));
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
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
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

#[test]
fn reachability_profile_leaves_out_projects_and_explains_them() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let git = |args: &[&str]| {
        let output = isolated_command("git")
            .current_dir(root)
            .args(["-c", "user.name=qk", "-c", "user.email=qk@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let write = |path: &str, content: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    write(
        "nx.json",
        &json!({"qk:affectedProfiles": {"reach": {"reachability": true}}}).to_string(),
    );
    write(
        "apps/app/project.json",
        &json!({
            "name": "app",
            "implicitDependencies": ["lib"],
            "qk:reachability": {
                "anchors": ["{projectRoot}/src/main.ts"],
                "sources": ["{projectRoot}/src/**/*", "{workspaceRoot}/libs/*/src/**/*"]
            }
        })
        .to_string(),
    );
    write(
        "apps/app/src/main.ts",
        "import { used } from \"../../../libs/lib/src/used\";\nexport const app = used;\n",
    );
    write(
        "apps/app-e2e/project.json",
        &json!({"name": "app-e2e", "implicitDependencies": ["app"]}).to_string(),
    );
    write("libs/lib/project.json", &json!({"name": "lib"}).to_string());
    write("libs/lib/src/used.ts", "export const used = 1;\n");
    write("libs/lib/src/unused.ts", "export const unused = 1;\n");
    git(&["init", "--quiet", "--initial-branch=main"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "base"]);
    write("libs/lib/src/unused.ts", "export const unused = 2;\n");
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "head"]);
    let run = |args: &[&str]| {
        let mut args = args.to_vec();
        args.extend([
            "--affected-profile",
            "reach",
            "--base",
            "HEAD^",
            "--head",
            "HEAD",
        ]);
        isolated_command(env!("CARGO_BIN_EXE_qk"))
            .current_dir(root)
            .args(&args)
            .output()
            .unwrap()
    };
    assert_eq!(
        successful_json(run(&["show", "projects", "--affected", "--json"])),
        json!(["lib"])
    );
    let output = run(&["show", "affected"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("after projection"), "{text}");
    assert!(text.contains("Left out by import reachability:"), "{text}");
    assert!(
        text.contains("app-e2e  left out: affected only through app, left out too"),
        "{text}"
    );
    assert!(
        text.contains("app      left out: none of its 1 anchor imports a change"),
        "{text}"
    );
    let output = run(&["show", "affected", "app"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("app is left out: none of its anchors imports a change it is affected through.\nAnchors searched:\n  apps/app/src/main.ts\nChanges it is affected through:\n  libs/lib/src/unused.ts\n"),
        "{text}"
    );
    let explanation = successful_json(run(&["show", "affected", "app", "--json"]));
    assert_eq!(explanation["affected"], false);
    assert_eq!(
        explanation["reachability"],
        json!({"decision": "leftOut", "anchors": ["apps/app/src/main.ts"], "changed": ["libs/lib/src/unused.ts"]})
    );

    write("libs/lib/src/used.ts", "export const used = 2;\n");
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "used"]);
    let output = run(&["show", "affected", "app"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains(
            "Import reachability kept it: apps/app/src/main.ts imports libs/lib/src/used.ts\n"
        ),
        "{text}"
    );
    let explanation = successful_json(run(&["show", "affected", "app", "--json"]));
    assert_eq!(explanation["reachability"]["decision"], "kept");
    assert_eq!(explanation["reachability"]["why"]["kind"], "reached");
}

#[test]
fn reachability_profile_runs_only_the_cases_a_change_reaches() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let git = |args: &[&str]| {
        let output = isolated_command("git")
            .current_dir(root)
            .args(["-c", "user.name=qk", "-c", "user.email=qk@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let write = |path: &str, content: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    let command = if cfg!(windows) {
        r#"if defined QK_AFFECTED_CASES (type "%QK_AFFECTED_CASES%") else (echo every case)"#
    } else {
        r#"if [ -n "$QK_AFFECTED_CASES" ]; then cat "$QK_AFFECTED_CASES"; else echo every case; fi"#
    };
    write(
        "nx.json",
        &json!({"qk:affectedProfiles": {"reach": {"reachability": true}}}).to_string(),
    );
    write(
        "apps/app/project.json",
        &json!({
            "name": "app",
            "targets": {
                "visual": {
                    "command": command,
                    "cache": true,
                    "qk:reachability": {
                        "anchors": ["{projectRoot}/visual/shell.ts"],
                        "cases": ["{projectRoot}/visual/cases/*.ts"],
                        "sources": ["{projectRoot}/visual/**/*.ts"]
                    }
                }
            }
        })
        .to_string(),
    );
    write("apps/app/visual/shell.ts", "export const shell = 1;\n");
    write(
        "apps/app/visual/cases/first.ts",
        "export const first = 1;\n",
    );
    write(
        "apps/app/visual/cases/second.ts",
        "export const second = 1;\n",
    );
    git(&["init", "--quiet", "--initial-branch=main"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "base"]);
    write(
        "apps/app/visual/cases/second.ts",
        "export const second = 2;\n",
    );
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "head"]);
    let run = || {
        let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
            .current_dir(root)
            .args(["affected", "-t", "visual", "--granularity", "task"])
            .args([
                "--affected-profile",
                "reach",
                "--base",
                "HEAD^",
                "--head",
                "HEAD",
            ])
            .args(["--output-style", "static"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr)
    };
    // Twice: a run of some cases is never cached, so it never stands for all.
    for _ in 0..2 {
        let output = run();
        assert!(
            output.contains("apps/app/visual/cases/second.ts"),
            "{output}"
        );
        assert!(
            !output.contains("apps/app/visual/cases/first.ts"),
            "{output}"
        );
        assert!(!output.contains("every case"), "{output}");
    }
    write("apps/app/visual/shell.ts", "export const shell = 2;\n");
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "shell"]);
    let output = run();
    assert!(output.contains("every case"), "{output}");
}

#[test]
fn affected_profile_lists_projects_explains_projection_and_rejects_task_selection() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let git = |args: &[&str]| {
        let output = isolated_command("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    for (path, content) in [
        ("nx.json", json!({"qk:affectedProfiles":{"runtime":{"projections":[{"name":"runtime","command":["git","-C","{revisionRoot}","show","HEAD:manifest.json"],"sources":["schemas/**"],"outputs":["apps/app/generated/**"],"timeoutSeconds":if cfg!(unix) { 3 } else { 60 }}]}}}).to_string()),
        ("schemas/project.json", json!({"name":"schema"}).to_string()),
        ("schemas/schema.txt", "before".into()),
        ("apps/app/project.json", json!({"name":"app","implicitDependencies":["schema"],"targets":{"build":{"command":"echo build"}}}).to_string()),
        ("manifest.json", json!({"version":1,"artifacts":{"apps/app/generated/runtime.js":"same"}}).to_string()),
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    git(&["init", "--quiet", "--initial-branch=main"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=qk",
        "-c",
        "user.email=qk@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "base",
    ]);
    std::fs::write(root.join("schemas/schema.txt"), "after").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=qk",
        "-c",
        "user.email=qk@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "head",
    ]);
    let run = |args: &[&str]| {
        isolated_command(env!("CARGO_BIN_EXE_qk"))
            .current_dir(root)
            .env_remove("NX_HEAD")
            .args(args)
            .output()
            .unwrap()
    };
    let comparison = [
        "--affected-profile",
        "runtime",
        "--base",
        "HEAD^",
        "--head",
        "HEAD",
    ];
    for selection in [
        vec![],
        vec!["--files", "schemas/schema.txt"],
        vec!["--files", "apps/app/project.json"],
        vec!["--stdin"],
        vec!["--uncommitted"],
        vec!["--untracked"],
    ] {
        let mut args = vec![
            "show",
            "projects",
            "--json",
            "--affected-profile",
            "runtime",
            "--base",
            "HEAD^",
            "--fail-on-projection-fallback=adapter",
        ];
        if !selection.is_empty() {
            args.extend(["--head", "HEAD"]);
        }
        args.extend(selection);
        let output = run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("strict projection selection"));
    }
    let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .env("NX_HEAD", "HEAD")
        .args([
            "show",
            "projects",
            "--json",
            "--affected-profile",
            "runtime",
            "--base",
            "HEAD^",
            "--fail-on-projection-fallback=adapter",
        ])
        .output()
        .unwrap();
    assert_eq!(successful_json(output), json!([]));
    let mut args = vec!["show", "projects", "--affected", "--json"];
    args.extend(comparison);
    assert_eq!(successful_json(run(&args)), json!([]));
    let mut args = vec!["show", "affected", "app", "--json"];
    args.extend(comparison);
    let explanation = successful_json(run(&args));
    assert_eq!(explanation["affected"], false);
    assert_eq!(explanation["projections"][0]["status"], "applied");
    assert_eq!(
        explanation["projections"][0]["sources"],
        json!(["schemas/schema.txt"])
    );
    assert_eq!(explanation["originalFiles"], json!(["schemas/schema.txt"]));
    assert_eq!(explanation["files"], json!([]));
    let mut args = vec!["show", "affected", "app"];
    args.extend(comparison);
    let output = run(&args);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("1 changed file between"));
    assert!(text.contains("0 files after projection."));
    assert!(text.contains("1 source change, 0 artifact changes"));
    let output = run(&[
        "show",
        "projects",
        "--affected-profile",
        "runtime",
        "--files",
        "schemas/schema.txt",
        "--fail-on-projection-fallback",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("committed base/head"));
    let output = run(&["show", "projects", "--fail-on-projection-fallback"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--affected-profile"));
    let mut args = vec!["affected", "-t", "build", "--granularity", "task"];
    args.extend(comparison);
    let output = run(&args);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("applies to project selection"));
    let output = run(&[
        "show",
        "tasks",
        "-t",
        "build",
        "--affected-profile",
        "runtime",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("add --affected"));
    let mut args = vec!["show", "tasks", "-t", "build", "--affected"];
    args.extend(comparison);
    let output = run(&args);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("applies to project selection"));
    let output = run(&[
        "show",
        "projects",
        "--affected-profile",
        "missing",
        "--base",
        "HEAD^",
        "--head",
        "HEAD",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown affected profile"));
    std::fs::write(root.join("schemas/schema.txt"), "runtime change").unwrap();
    std::fs::write(
        root.join("manifest.json"),
        json!({"version":1,"artifacts":{
            "apps/app/generated/runtime.js":"changed",
            "apps/app/generated/extra.js":"new"
        }})
        .to_string(),
    )
    .unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=qk",
        "-c",
        "user.email=qk@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "runtime",
    ]);
    let mut args = vec!["show", "affected"];
    args.extend(comparison);
    let output = run(&args);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("2 changed files between"));
    assert!(text.contains("3 files after projection."));
    assert!(text.contains("1 source change, 2 artifact changes"));
    args.push("--json");
    let report = successful_json(run(&args));
    assert_eq!(report["originalFiles"].as_array().unwrap().len(), 2);
    assert_eq!(report["files"].as_array().unwrap().len(), 3);
    let worktrees_before = isolated_command("git")
        .current_dir(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap()
        .stdout;
    let mut hooked = isolated_command(env!("CARGO_BIN_EXE_qk"));
    hooked
        .current_dir(root)
        .env_remove("NX_HEAD")
        .env("GIT_DIR", root.join(".git"))
        .env("GIT_WORK_TREE", root)
        .env("GIT_INDEX_FILE", root.join(".git/index"))
        .env("GIT_OBJECT_DIRECTORY", root.join(".git/objects"))
        .env("GIT_COMMON_DIR", root.join(".git"))
        .env("GIT_PREFIX", "hook/")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.worktree")
        .env("GIT_CONFIG_VALUE_0", root);
    hooked
        .args([
            "show",
            "projects",
            "--json",
            "--fail-on-projection-fallback=adapter",
        ])
        .args(comparison);
    assert_eq!(successful_json(hooked.output().unwrap()), json!(["app"]));
    assert_eq!(
        isolated_command("git")
            .current_dir(root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap()
            .stdout,
        worktrees_before
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let wrapper = TempDir::new().unwrap();
        let git_path = wrapper.path().join("git");
        std::fs::write(
            &git_path,
            r#"#!/bin/sh
if [ "$QK_TEST_GIT_FAILURE_MODE" = metadata ] && [ "$1" = diff ] && [ "$2" = --quiet ]; then
  echo 'fatal: simulated metadata comparison failure' >&2
  exit 128
fi
if [ "$QK_TEST_GIT_FAILURE_MODE" = checkout ] && [ "$1" = -c ] && [ "$3" = worktree ] && [ "$4" = add ]; then
  "$QK_TEST_REAL_GIT" "$@" || exit $?
  previous=
  current=
  for argument do previous="$current"; current="$argument"; done
  "$QK_TEST_REAL_GIT" worktree lock "$previous" || exit $?
  sleep 30
  exit 0
fi
exec "$QK_TEST_REAL_GIT" "$@"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&git_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let real_git = isolated_command("/bin/sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        assert!(real_git.status.success());
        let mut search_path = vec![wrapper.path().to_path_buf()];
        search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        for (mode, policy) in [
            ("metadata", "adapter"),
            ("metadata", "all"),
            ("checkout", "adapter"),
            ("checkout", "all"),
        ] {
            let started = std::time::Instant::now();
            let mut args = vec!["show", "affected", "--json"];
            args.extend(comparison);
            let output = isolated_command(env!("CARGO_BIN_EXE_qk"))
                .current_dir(root)
                .env_remove("NX_HEAD")
                .env("PATH", std::env::join_paths(&search_path).unwrap())
                .env("QK_TEST_GIT_FAILURE_MODE", mode)
                .env(
                    "QK_TEST_REAL_GIT",
                    String::from_utf8(real_git.stdout.clone()).unwrap().trim(),
                )
                .args(args)
                .arg(format!("--fail-on-projection-fallback={policy}"))
                .output()
                .unwrap();
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            if mode == "metadata" {
                assert!(
                    diagnostic.contains("cannot compare workspace metadata"),
                    "{diagnostic}"
                );
                assert!(diagnostic.contains("128"), "{diagnostic}");
                assert!(
                    diagnostic.contains("simulated metadata comparison failure"),
                    "{diagnostic}"
                );
            } else {
                assert!(
                    diagnostic.contains("git worktree add timed out"),
                    "{diagnostic}"
                );
                assert!(started.elapsed() < std::time::Duration::from_secs(10));
                assert_eq!(
                    isolated_command("git")
                        .current_dir(root)
                        .args(["worktree", "list", "--porcelain"])
                        .output()
                        .unwrap()
                        .stdout,
                    worktrees_before
                );
            }
            assert!(!diagnostic.contains("workspace configuration or tool installation changed"));
            if policy == "adapter" {
                assert!(output.status.success());
                let explanation: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(explanation["projections"][0]["fallbackKind"], "comparison");
                assert_eq!(explanation["files"], explanation["originalFiles"]);
                assert_eq!(
                    explanation["files"],
                    json!(["manifest.json", "schemas/schema.txt"])
                );
            } else {
                assert!(!output.status.success());
                assert!(output.stdout.is_empty());
            }
        }
    }

    std::fs::write(
        root.join("package.json"),
        r#"{"dependencies":{"example-tool":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("schemas/schema.txt"),
        "schema with dependency change",
    )
    .unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=qk",
        "-c",
        "user.email=qk@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "dependency",
    ]);
    let mut args = vec![
        "show",
        "projects",
        "--json",
        "--fail-on-projection-fallback=adapter",
    ];
    args.extend(comparison);
    let output = run(&args);
    assert!(output.status.success());
    let notice = String::from_utf8(output.stderr).unwrap();
    assert_eq!(notice.lines().count(), 1);
    assert!(notice.starts_with("qk: notice: affected projection runtime:"));
    assert!(notice.contains("workspace configuration or tool installation changed"));
    let projects: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(projects.as_array().unwrap().contains(&json!("app")));
    args[3] = "--fail-on-projection-fallback=all";
    assert!(!run(&args).status.success());
    args[3] = "--fail-on-projection-fallback=adapter";
    args[1] = "affected";
    let output = run(&args);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("qk: notice:"));
    let explanation: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(explanation["projections"][0]["fallbackKind"], "comparison");
    std::fs::write(root.join("schemas/schema.txt"), "schema with output drift").unwrap();
    std::fs::write(
        root.join("manifest.json"),
        r#"{"version":1,"artifacts":{"apps/stray/generated/runtime.js":"new"}}"#,
    )
    .unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=qk",
        "-c",
        "user.email=qk@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "drift",
    ]);
    args[1] = "projects";
    let output = run(&args);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("apps/stray/generated/runtime.js"));
}
