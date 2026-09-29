use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture(targets: Value) -> TempDir {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nx.json"), "{}").unwrap();
    fs::write(
        temp.path().join("project.json"),
        json!({"name":"app", "targets":targets}).to_string(),
    )
    .unwrap();
    temp
}

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qk"));
    command
        .current_dir(root)
        .arg("--workspace")
        .arg(root)
        .args(args);
    command.env_remove("NX_PARALLEL");
    command
}

fn run(root: &Path, args: &[&str]) -> Output {
    command(root, args).output().unwrap()
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn helper() -> String {
    format!(
        "\"{}\" --exact process_helper --nocapture",
        std::env::current_exe().unwrap().display()
    )
}

fn helper_target(mode: &str, id: &str) -> Value {
    json!({"executor":"nx:run-commands", "options": {
        "command":helper(), "forwardAllArgs": false,
        "env":{"QK_TEST_MODE":mode, "QK_TEST_ID":id}
    }})
}

// Spawn the test binary itself as a portable subprocess, without Python or Node.
#[test]
fn process_helper() {
    let Ok(mode) = std::env::var("QK_TEST_MODE") else {
        return;
    };
    let id = std::env::var("QK_TEST_ID").unwrap();
    let root = std::env::current_dir().unwrap();
    match mode.as_str() {
        "record" => {
            let values: std::collections::BTreeMap<_, _> = std::env::vars()
                .filter(|(name, _)| name.starts_with("QK_TEST_"))
                .collect();
            fs::write(
                root.join(format!("{id}.json")),
                json!({"cwd":root, "env":values}).to_string(),
            )
            .unwrap();
        }
        "barrier" => {
            fs::write(root.join(format!("{id}.started")), "started").unwrap();
            let other = if id == "a" { "b" } else { "a" };
            wait_for(&root.join(format!("{other}.started")));
        }
        "exclusive" => {
            fs::create_dir(root.join("exclusive.lock"))
                .expect("tasks overlapped despite parallel=1");
            std::thread::sleep(Duration::from_millis(80));
            fs::remove_dir(root.join("exclusive.lock")).unwrap();
        }
        "tree" => {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process_helper", "--nocapture"])
                .env("QK_TEST_MODE", "heartbeat")
                .spawn()
                .unwrap();
            let _ = child.wait();
        }
        "heartbeat" => {
            fs::write(root.join("heartbeat.started"), "started").unwrap();
            // Bound lifetime even if a regression prevents cleanup.
            for value in 0..300 {
                fs::write(root.join("heartbeat"), value.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "fail-after-start" => {
            wait_for(&root.join("heartbeat.started"));
            std::process::exit(9);
        }
        _ => panic!("unknown helper mode"),
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn dependency_diamond_runs_once_before_dependents_and_streams_raw_output() {
    let temp = fixture(json!({
        "build":{"executor":"nx:noop", "dependsOn":["a", "b"]},
        "a":{"command":"echo a>> order.txt", "dependsOn":["shared"]},
        "b":{"command":"echo b>> order.txt", "dependsOn":["shared"]},
        "shared":{"command":"echo shared>> order.txt"},
        "output":{"command":"echo raw-output"}
    }));
    success(run(temp.path(), &["run", "app:build", "--parallel", "2"]));
    let lines = fs::read_to_string(temp.path().join("order.txt")).unwrap();
    let lines: Vec<_> = lines.lines().map(str::trim).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "shared");
    assert!(lines.contains(&"a") && lines.contains(&"b"));
    let output = success(run(temp.path(), &["run", "app:output"]));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "raw-output"
    );
}

#[test]
fn failure_preserves_exit_code_skips_dependents_and_runs_independent_roots() {
    let temp = fixture(json!({
        "build":{"command":"echo wrong> must-not-exist", "dependsOn":["fail"]},
        "fail":{"command":"exit 7"},
        "independent":{"command":"echo ok> independent"}
    }));
    let output = run(
        temp.path(),
        &["run-many", "-t", "build,independent", "--parallel", "1"],
    );
    assert_eq!(output.status.code(), Some(7));
    assert!(!temp.path().join("must-not-exist").exists());
    assert!(temp.path().join("independent").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("skipped app:build"));
}

#[test]
fn validates_entire_plan_before_running_and_dry_run_has_no_side_effects() {
    let temp = fixture(json!({
        "good":{"command":"echo wrong> must-not-exist"},
        "bad":{"executor":"unknown:executor", "dependsOn":["good"]},
        "continuous":{"command":"echo wrong> must-not-exist", "continuous":true}
    }));
    for target in ["app:bad", "app:continuous"] {
        assert!(!run(temp.path(), &["run", target]).status.success());
        assert!(!temp.path().join("must-not-exist").exists());
    }
    let output = success(run(temp.path(), &["run", "app:good", "--dry-run"]));
    let graph: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(graph["tasks"]["app:good"].is_object());
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn configuration_cwd_tokens_and_environment_precedence() {
    let mut target = helper_target("record", "result");
    target["options"]["cwd"] = json!("{workspaceRoot}/working directory");
    target["env"] = json!({"QK_TEST_TARGET":"top", "QK_TEST_OVERRIDE":"top"});
    target["options"]["env"]["QK_TEST_OVERRIDE"] = json!("options");
    target["options"]["env"]["QK_TEST_PROJECT"] = json!("{projectName}:{projectRoot}");
    target["configurations"] = json!({"prod":{"env":{"QK_TEST_OVERRIDE":"configuration"}}});
    let temp = fixture(json!({"build":target}));
    fs::create_dir(temp.path().join("working directory")).unwrap();
    fs::write(
        temp.path().join(".env"),
        "QK_TEST_FILE=base\nQK_TEST_LOCAL=base\nQK_TEST_PARENT=base\n",
    )
    .unwrap();
    fs::write(
        temp.path().join(".env.local"),
        "QK_TEST_LOCAL=local\nQK_TEST_PARENT=local\n",
    )
    .unwrap();
    success(
        command(temp.path(), &["run", "app:build", "-c", "prod"])
            .env("QK_TEST_PARENT", "parent")
            .output()
            .unwrap(),
    );
    let result: Value = serde_json::from_slice(
        &fs::read(temp.path().join("working directory/result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        PathBuf::from(result["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        temp.path()
            .join("working directory")
            .canonicalize()
            .unwrap()
    );
    for (key, expected) in [
        ("QK_TEST_FILE", "base"),
        ("QK_TEST_LOCAL", "local"),
        ("QK_TEST_PARENT", "parent"),
        ("QK_TEST_TARGET", "top"),
        ("QK_TEST_OVERRIDE", "configuration"),
        ("QK_TEST_PROJECT", "app:."),
    ] {
        assert_eq!(result["env"][key], expected, "{key}");
    }
}

#[test]
fn parallel_tasks_overlap_and_parallel_one_serializes() {
    for (mode, parallel) in [("barrier", "2"), ("exclusive", "1")] {
        let temp = fixture(json!({"a":helper_target(mode, "a"), "b":helper_target(mode, "b")}));
        success(run(
            temp.path(),
            &["run-many", "-t", "a,b", "--parallel", parallel],
        ));
    }
}

#[test]
fn command_lists_support_sequential_order_and_stop_on_failure() {
    let temp = fixture(json!({"build":{"executor":"nx:run-commands", "options":{
        "commands":["echo first> order.txt", "exit 9", "echo wrong> must-not-exist"], "parallel":false
    }}}));
    let output = run(temp.path(), &["run", "app:build"]);
    assert_eq!(output.status.code(), Some(9));
    assert!(temp.path().join("order.txt").exists());
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn parallel_command_list_starts_both_commands() {
    let temp = fixture(json!({"build":{"executor":"nx:run-commands", "options":{
        "commands":["echo first> first.txt", {"command":"echo second> second.txt", "forwardAllArgs":false}]
    }}}));
    success(run(temp.path(), &["run", "app:build"]));
    assert!(temp.path().join("first.txt").exists());
    assert!(temp.path().join("second.txt").exists());
}

#[cfg(unix)]
#[test]
fn argument_tokens_and_forwarding_preserve_data_without_shell_execution() {
    let temp = fixture(json!({
        "named":{"command":"printf '%s' {args.value} > result.txt"},
        "all":{"command":"printf '%s\\n'"},
        "quoted":{"command":"echo \"{args.value}\""}
    }));
    let payload = "a 'quoted' $(touch injected) ; {projectName}";
    success(run(
        temp.path(),
        &["run", "app:named", "--", "--value", payload],
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("result.txt")).unwrap(),
        payload
    );
    assert!(!temp.path().join("injected").exists());
    let output = success(run(
        temp.path(),
        &["run", "app:all", "--", payload, "two words"],
    ));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{payload}\ntwo words\n")
    );
    assert!(
        !run(
            temp.path(),
            &["run", "app:quoted", "--", "--value", payload]
        )
        .status
        .success()
    );
}

#[cfg(unix)]
#[test]
fn script_executor_uses_package_manager_and_forwards_arguments() {
    use std::os::unix::fs::PermissionsExt;
    for manager in ["npm", "pnpm"] {
        let temp = fixture(json!({"test":{}}));
        fs::write(
            temp.path().join("package.json"),
            json!({
                "name":"app", "packageManager":format!("{manager}@1.0.0"),
                "scripts":{"test":"ignored by fake manager"}
            })
            .to_string(),
        )
        .unwrap();
        let bin = temp.path().join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(manager);
        fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > manager-args\npwd > manager-cwd\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        success(run(
            temp.path(),
            &["run", "app:test", "--", "--flag", "two words"],
        ));
        let expected = if manager == "npm" {
            "run\ntest\n--\n--flag\ntwo words\n"
        } else {
            "run\ntest\n--flag\ntwo words\n"
        };
        assert_eq!(
            fs::read_to_string(temp.path().join("manager-args")).unwrap(),
            expected
        );
        let cwd = fs::read_to_string(temp.path().join("manager-cwd")).unwrap();
        assert_eq!(
            Path::new(cwd.trim()).canonicalize().unwrap(),
            temp.path().canonicalize().unwrap()
        );
    }
}

#[test]
fn missing_scripts_fail_at_execution_without_running_dependencies() {
    let temp = fixture(
        json!({"missing":{"dependsOn":["prepare"]}, "prepare":{"command":"echo wrong> must-not-exist"}}),
    );
    fs::write(
        temp.path().join("package.json"),
        json!({"name":"app", "nx":{"includedScripts":["missing"]}}).to_string(),
    )
    .unwrap();
    let output = run(temp.path(), &["run", "app:missing"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("has no script"));
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn malformed_dotenv_does_not_expose_secret_values() {
    let temp = fixture(json!({"test":{"executor":"nx:noop"}}));
    fs::write(
        temp.path().join(".env"),
        "QK_TEST_SECRET='secret-without-closing-quote",
    )
    .unwrap();
    let output = run(temp.path(), &["run", "app:test"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid dotenv"));
    assert!(!error.contains("secret-without-closing-quote"));
}

#[cfg(unix)]
#[test]
fn failed_parallel_command_terminates_its_running_sibling() {
    let temp = fixture(json!({"build":{"executor":"nx:run-commands", "options":{
        "commands":[format!("QK_TEST_MODE=tree {}", helper()), format!("QK_TEST_MODE=fail-after-start {}", helper())],
        "env":{"QK_TEST_ID":"tree"}
    }}}));
    let output = run(temp.path(), &["run", "app:build"]);
    assert_eq!(output.status.code(), Some(9));
    let before = fs::read(temp.path().join("heartbeat")).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::read(temp.path().join("heartbeat")).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn cancellation_terminates_descendants_and_returns_130() {
    for signal in ["-INT", "-TERM"] {
        let temp = fixture(json!({"long":helper_target("tree", "tree")}));
        let mut child = command(temp.path(), &["run", "app:long"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(&temp.path().join("heartbeat.started"));
        assert!(
            Command::new("/bin/kill")
                .arg(signal)
                .arg(child.id().to_string())
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("qk did not stop");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(130));
        let before = fs::read(temp.path().join("heartbeat")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            fs::read(temp.path().join("heartbeat")).unwrap(),
            before,
            "descendant survived {signal}"
        );
    }
}
