use std::fs;
use std::path::{Path, PathBuf};
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
    command.env_remove("NX_CACHE_DIRECTORY");
    // Output styles follow CI; tests that want it set it themselves.
    command.env_remove("CI");
    command.env_remove("GITHUB_ACTIONS");
    // What a task or package script running the tests would pass on to exec.
    command.env_remove("NX_TASK_TARGET_PROJECT");
    command.env_remove("npm_lifecycle_event");
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
    #[cfg(windows)]
    if mode == "signal-watch" {
        signal_windows_watch(std::env::var("QK_WATCH_PID").unwrap().parse().unwrap());
        return;
    }
    let id = std::env::var("QK_TEST_ID").unwrap();
    let root = std::env::current_dir().unwrap();
    match mode.as_str() {
        "large-string-control-output" => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"before\x1b]0;").unwrap();
            stdout.write_all(&vec![b'x'; 256 * 1024]).unwrap();
            stdout
                .write_all(b"\x1b\\after\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07\n\x1bPunfinished")
                .unwrap();
            stdout.flush().unwrap();
            std::process::exit(1);
        }
        "large-colored-output" => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&b"\x1b[31mx\x1b[0m".repeat(32 * 1024))
                .unwrap();
            stdout.write_all(b"\n\x1b[").unwrap();
            stdout.flush().unwrap();
            std::process::exit(1);
        }
        "large-output" => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "BEGIN-{id}").unwrap();
            let bytes = vec![b'x'; 256 * 1024];
            stdout.write_all(&bytes).unwrap();
            stdout.write_all(b"\0\xff").unwrap();
            writeln!(stdout, "\nEND-{id}").unwrap();
            stdout.flush().unwrap();
            std::process::exit(i32::from(id == "fail"));
        }
        "watch-portable" => {
            let records = PathBuf::from(std::env::var("QK_WATCH_RECORDS").unwrap());
            let n = fs::read_dir(&records)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                })
                .count()
                + 1;
            fs::write(
                records.join(format!("{n}.tmp")),
                json!({
                    "project":std::env::var("NX_PROJECT_NAME").unwrap(),
                    "files":std::env::var("NX_FILE_CHANGES").unwrap()
                })
                .to_string(),
            )
            .unwrap();
            fs::rename(
                records.join(format!("{n}.tmp")),
                records.join(format!("{n}.json")),
            )
            .unwrap();
            let mut ticks = 0;
            while !records.join(format!("release-{n}")).exists() {
                ticks += 1;
                fs::write(records.join(format!("{n}.tick")), ticks.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        "diamond" => {
            let dependencies: &[&str] = match id.as_str() {
                "shared" => &[],
                "a" | "b" => &["shared"],
                "build" => &["a", "b"],
                _ => panic!("unknown diamond task"),
            };
            for dependency in dependencies {
                assert_eq!(
                    fs::read_to_string(root.join(format!("{dependency}.done"))).unwrap(),
                    format!("{dependency}\n")
                );
            }
            // Each task owns its file; concurrent shell redirects to one file
            // fail with a sharing violation on Windows.
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join(format!("{id}.done")))
                .unwrap();
            writeln!(file, "{id}").unwrap();
        }
        "threads" => {
            fs::write(
                root.join(format!("{id}.json")),
                json!({
                    "threads": std::env::var("QK_THREADS").ok(),
                    "workers": std::env::var("QK_TEST_WORKERS").ok(),
                })
                .to_string(),
            )
            .unwrap();
        }
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
            fs::write(root.join("heartbeat"), "0").unwrap();
            fs::write(root.join("heartbeat.started"), "started").unwrap();
            // Bound lifetime even if a regression prevents cleanup.
            for value in 1..300 {
                fs::write(root.join("heartbeat"), value.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "serve" => {
            fs::write(root.join(format!("{id}.started")), "started").unwrap();
            // Bound lifetime even if a regression prevents stopping it.
            for value in 0..1500 {
                fs::write(root.join(format!("{id}.beat")), value.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "client" => {
            wait_for(&root.join("serve.started"));
            fs::write(root.join(format!("{id}.done")), "done").unwrap();
        }
        "exit-3" => {
            fs::write(root.join(format!("{id}.started")), "started").unwrap();
            std::process::exit(3);
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
/// Shared dependencies run once before both branches, and raw output stays intact.
fn dependency_diamond_runs_once_before_dependents_and_streams_raw_output() {
    let mut targets = json!({"output":{"command":"echo raw-output"}});
    for (id, dependencies) in [
        ("shared", vec![]),
        ("a", vec!["shared"]),
        ("b", vec!["shared"]),
        ("build", vec!["a", "b"]),
    ] {
        targets[id] = helper_target("diamond", id);
        targets[id]["dependsOn"] = json!(dependencies);
    }
    let temp = fixture(targets);
    success(run(temp.path(), &["run", "app:build", "--parallel", "2"]));
    for id in ["shared", "a", "b", "build"] {
        assert_eq!(
            fs::read_to_string(temp.path().join(format!("{id}.done"))).unwrap(),
            format!("{id}\n")
        );
    }
    let output = success(run(temp.path(), &["run", "app:output"]));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "raw-output"
    );
}

#[test]
fn failure_exits_with_1_skips_dependents_and_runs_independent_roots() {
    let temp = fixture(json!({
        "build":{"command":"echo wrong> must-not-exist", "dependsOn":["fail"]},
        "fail":{"command":"exit 7"},
        "independent":{"command":"echo ok> independent"}
    }));
    let output = run(
        temp.path(),
        &["run-many", "-t", "build,independent", "--parallel", "1"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!temp.path().join("must-not-exist").exists());
    assert!(temp.path().join("independent").exists());
    // Quiet, the default off a terminal, names both in its summary.
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.contains("1 of 3 tasks failed"), "{summary}");
    assert!(
        summary.contains("since a dependency failed: app:build"),
        "{summary}"
    );
    let output = run(
        temp.path(),
        &[
            "run-many",
            "-t",
            "build,independent",
            "--parallel",
            "1",
            "--output-style",
            "static",
        ],
    );
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(
        summary.contains("Tasks not run because their dependencies failed:"),
        "{summary}"
    );
    assert!(summary.contains(" app:build\n"), "{summary}");
}

#[test]
fn validates_entire_plan_before_running_and_dry_run_has_no_side_effects() {
    let temp = fixture(json!({
        "good":{"command":"echo wrong> must-not-exist"},
        "bad":{"executor":"unknown:executor", "dependsOn":["good"]}
    }));
    assert!(!run(temp.path(), &["run", "app:bad"]).status.success());
    assert!(!temp.path().join("must-not-exist").exists());
    let output = success(run(temp.path(), &["run", "app:good", "--dry-run"]));
    let graph: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(graph["tasks"]["app:good"].is_object());
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn working_directory_may_be_created_by_a_dependency() {
    // As `expo prebuild` creates the native project a Gradle task runs in.
    let temp = fixture(json!({
        "generate":{"command":"mkdir generated"},
        "build":{"command":"echo built> out", "dependsOn":["generate"],
                 "options":{"cwd":"generated"}}
    }));
    success(run(temp.path(), &["run", "app:build"]));
    assert!(temp.path().join("generated/out").exists());
    fs::remove_dir_all(temp.path().join("generated")).unwrap();
    success(run(temp.path(), &["run", "app:build", "--dry-run"]));
}

#[test]
fn missing_working_directory_fails_its_task_when_it_runs() {
    let temp = fixture(json!({
        "independent":{"command":"echo ok> independent"},
        "build":{"command":"echo wrong> must-not-exist", "dependsOn":["independent"],
                 "options":{"cwd":"missing"}}
    }));
    let output = run(temp.path(), &["run", "app:build"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("working directory does not exist"),
        "{stderr}"
    );
    assert!(temp.path().join("independent").exists());
    assert!(!temp.path().join("missing").exists());
}

#[test]
fn configuration_cwd_tokens_and_environment_precedence() {
    let mut target = helper_target("record", "result");
    target["options"]["cwd"] = json!("{workspaceRoot}/working directory");
    target["env"] = json!({"QK_TEST_TARGET":"top", "QK_TEST_OVERRIDE":"top"});
    target["options"]["env"]["QK_TEST_OVERRIDE"] = json!("options");
    target["options"]["env"]["QK_TEST_BASE_ONLY"] = json!("base");
    target["configurations"] = json!({"prod":{"env":{
        "QK_TEST_OVERRIDE":"configuration", "QK_TEST_PROJECT":"{projectName}:{projectRoot}"
    }}});
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
            .env("QK_TEST_MODE", "record")
            .env("QK_TEST_ID", "result")
            .env_remove("QK_TEST_BASE_ONLY")
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
    assert!(result["env"].get("QK_TEST_BASE_ONLY").is_none());
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
    assert_eq!(output.status.code(), Some(1));
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
fn arguments_go_into_commands_as_nx_puts_them() {
    let temp = fixture(json!({
        "named":{"command":"printf '%s|' {args.value} > result.txt"},
        "quoted":{"command":"printf '%s|' \"{args.value}\" > result.txt"},
        "all":{"command":"printf '%s\\n'"}
    }));
    // A value goes in as it is, for the shell to read.
    success(run(
        temp.path(),
        &["run", "app:named", "--", "--value", "one two"],
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("result.txt")).unwrap(),
        "one|two|"
    );
    success(run(
        temp.path(),
        &["run", "app:quoted", "--", "--value", "one two"],
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("result.txt")).unwrap(),
        "one two|"
    );
    // Appended arguments are double-quoted when the shell would split them.
    let output = success(run(
        temp.path(),
        &[
            "run",
            "app:all",
            "--",
            "two words",
            "--name=a b",
            "plain",
            "'{\"a\":",
            "1}'",
        ],
    ));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "two words\n--name=a b\nplain\n{\"a\": 1}\n"
    );
}

#[cfg(unix)]
#[test]
fn script_executor_reads_only_its_script_like_nx() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nx.json"), "{}").unwrap();
    fs::write(temp.path().join("package-lock.json"), "{}").unwrap();
    fs::write(
        temp.path().join("package.json"),
        r#"{"workspaces": ["app"]}"#,
    )
    .unwrap();
    let project = temp.path().join("app");
    fs::create_dir_all(project.join("other")).unwrap();
    fs::write(
        project.join("package.json"),
        json!({"name": "app", "scripts": {"test": "ignored by fake manager"}}).to_string(),
    )
    .unwrap();
    fs::write(
        project.join("project.json"),
        json!({"name": "app", "targets": {"test": {"options": {
            "cwd": "app/other", "env": {"FROM_OPTION": "set"}, "parallel": "not a boolean", "anything": 1
        }, "env": {"FROM_TARGET": "set"}}}})
        .to_string(),
    )
    .unwrap();
    let bin = temp.path().join("node_modules/.bin");
    fs::create_dir_all(&bin).unwrap();
    let path = bin.join("npm");
    fs::write(
        &path,
        "#!/bin/sh\npwd > ../manager-cwd\necho \"[$FROM_OPTION] [$FROM_TARGET]\" > ../manager-env\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    success(run(temp.path(), &["run", "app:test"]));
    let cwd = fs::read_to_string(temp.path().join("manager-cwd")).unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        project.canonicalize().unwrap()
    );
    // Its options' env is not read; the target-level env is qk's own.
    assert_eq!(
        fs::read_to_string(temp.path().join("manager-env")).unwrap(),
        "[] [set]\n"
    );
}

#[cfg(unix)]
#[test]
fn script_executor_uses_package_manager_and_forwards_arguments() {
    use std::os::unix::fs::PermissionsExt;
    for manager in ["npm", "pnpm", "yarn", "bun"] {
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
        let expected = match manager {
            "npm" | "bun" => "run\ntest\n--\n--flag\ntwo words\n",
            "yarn" => "test\n--flag\ntwo words\n",
            _ => "run\ntest\n--flag\ntwo words\n",
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

#[cfg(unix)]
#[test]
fn dotenv_files_are_read_as_npm_dotenv_reads_them() {
    let temp = fixture(
        json!({"show": {"command": "printf '%s|' \"$UNCLOSED\" \"$QUOTED\" \"$EXPANDED\" \"$MULTI\""}}),
    );
    // A line it cannot read as quoted is read unquoted, as npm's dotenv does;
    // single quotes do not stop expansion, as dotenv-expand has it.
    fs::write(
        temp.path().join(".env"),
        "UNCLOSED='no closing quote\nQUOTED='$UNCLOSED'\nEXPANDED=${MISSING:-default}\nMULTI=\"one\ntwo\"\nnot a variable\n",
    )
    .unwrap();
    let output = success(run(temp.path(), &["run", "app:show"]));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "'no closing quote|'no closing quote|default|one\ntwo|"
    );
}

#[cfg(unix)]
#[test]
fn failed_parallel_command_terminates_its_running_sibling() {
    let temp = fixture(json!({"build":{"executor":"nx:run-commands", "options":{
        "commands":[format!("QK_TEST_MODE=tree {}", helper()), format!("QK_TEST_MODE=fail-after-start {}", helper())],
        "env":{"QK_TEST_ID":"tree"}
    }}}));
    let output = run(temp.path(), &["run", "app:build"]);
    // As in Nx, parallel commands fail the task with 1, whatever the code.
    assert_eq!(output.status.code(), Some(1));
    let before = fs::read(temp.path().join("heartbeat")).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::read(temp.path().join("heartbeat")).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn cancellation_terminates_descendants_and_exits_as_the_signal() {
    // As Nx's signalToCode: 128 plus the signal's number.
    for (signal, code) in [("-INT", 130), ("-TERM", 143), ("-HUP", 129)] {
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
        assert_eq!(status.code(), Some(code), "{signal}");
        let before = fs::read(temp.path().join("heartbeat")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            fs::read(temp.path().join("heartbeat")).unwrap(),
            before,
            "descendant survived {signal}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_command_ignoring_sigterm_is_killed_after_the_grace_period() {
    let temp = fixture(json!({"stubborn": {"command": "trap '' TERM; touch started; sleep 30"}}));
    let mut child = command(temp.path(), &["run", "app:stubborn"])
        .env("NX_PROCESS_KILL_GRACE_PERIOD", "200")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&temp.path().join("started"));
    let sent = Instant::now();
    assert!(
        Command::new("/bin/kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if sent.elapsed() > Duration::from_secs(4) {
            let _ = child.kill();
            panic!("the grace period was not applied");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(130));
}

fn continuous_target(mode: &str, id: &str) -> Value {
    let mut target = helper_target(mode, id);
    target["continuous"] = json!(true);
    target
}

/// Asserts a helper's heartbeat file has stopped changing.
fn assert_stopped(path: &Path) {
    let before = fs::read(path).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        fs::read(path).unwrap(),
        before,
        "{} still running",
        path.display()
    );
}

#[test]
fn finite_dependents_start_after_continuous_dependency_starts_then_stop_it() {
    for parallel in ["1", "3"] {
        let temp = fixture(json!({
            "serve": continuous_target("serve", "serve"),
            "bench": {
                "dependsOn": ["serve"],
                "executor": "nx:run-commands",
                "options": helper_target("client", "bench")["options"].clone(),
            },
        }));
        let output = success(run(
            temp.path(),
            &["run", "app:bench", "--parallel", parallel],
        ));
        assert!(temp.path().join("bench.done").exists());
        // Stopped rather than left to run out its lifetime, whose last beat is
        // 1499; a count is not thrown off by a slow machine as a clock is. A
        // beat stopped mid-write leaves the file empty.
        let beats: u32 = fs::read_to_string(temp.path().join("serve.beat"))
            .unwrap()
            .parse()
            .unwrap_or(0);
        assert!(beats < 1499, "app:serve ran out its lifetime");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("qk: started app:serve (continuous)"),
            "{stderr}"
        );
        assert!(
            stderr.contains("qk: stopping app:serve (no longer needed)"),
            "{stderr}"
        );
        assert!(stderr.contains("qk: stopped app:serve"), "{stderr}");
        assert_stopped(&temp.path().join("serve.beat"));
    }
}

#[cfg(unix)]
#[test]
fn stopping_sends_sigterm_before_killing() {
    let temp = fixture(json!({
        "serve": {
            "continuous": true,
            "command": "trap 'echo cleaned > cleaned; exit 0' TERM; touch serve.started; while :; do sleep 0.05; done",
        },
        "client": {
            "dependsOn": ["serve"],
            "command": "while [ ! -f serve.started ]; do sleep 0.02; done",
        },
    }));
    success(run(temp.path(), &["run", "app:client"]));
    assert_eq!(
        fs::read_to_string(temp.path().join("cleaned"))
            .unwrap()
            .trim(),
        "cleaned"
    );
}

#[cfg(unix)]
#[test]
fn continuous_roots_run_until_cancelled() {
    let temp = fixture(json!({
        "watch": continuous_target("serve", "watch"),
        "serve": {
            "continuous": true,
            "dependsOn": ["watch"],
            "executor": "nx:run-commands",
            "options": helper_target("serve", "serve")["options"].clone(),
        },
    }));
    let mut child = command(temp.path(), &["run", "app:serve", "--parallel", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&temp.path().join("serve.started"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "continuous root exited early"
    );
    assert!(
        Command::new("/bin/kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
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
    assert_stopped(&temp.path().join("serve.beat"));
    assert_stopped(&temp.path().join("watch.beat"));
}

#[test]
fn continuous_task_exiting_on_its_own_reports_its_exit_code() {
    let temp = fixture(json!({"serve": continuous_target("exit-3", "serve")}));
    let output = run(temp.path(), &["run", "app:serve"]);
    assert_eq!(output.status.code(), Some(1));
}

fn threaded_helper(id: &str, continuous: bool) -> Value {
    let mut target = helper_target("threads", id);
    target["continuous"] = json!(continuous);
    target["qk:threads"] = json!({"env": {"QK_TEST_WORKERS": "{threads}"}});
    target
}

fn thread_record(root: &Path, id: &str) -> Value {
    serde_json::from_slice(&fs::read(root.join(format!("{id}.json"))).unwrap()).unwrap()
}

#[test]
fn continuous_root_with_threads_runs_without_a_core_allocation() {
    let temp = fixture(json!({"watch": threaded_helper("watch", true)}));
    success(
        command(temp.path(), &["run", "app:watch", "--cores", "8"])
            .env_remove("QK_THREADS")
            .env_remove("QK_TEST_WORKERS")
            .output()
            .unwrap(),
    );
    assert_eq!(
        thread_record(temp.path(), "watch"),
        json!({"threads": null, "workers": null})
    );
}

#[test]
fn continuous_thread_settings_do_not_reduce_finite_thread_shares() {
    let temp = fixture(json!({
        "watch": threaded_helper("watch", true),
        "a": threaded_helper("a", false),
        "b": threaded_helper("b", false),
    }));
    success(
        command(
            temp.path(),
            &[
                "run-many",
                "-t",
                "a,b,watch",
                "--parallel",
                "2",
                "--cores",
                "8",
            ],
        )
        .env_remove("QK_THREADS")
        .env_remove("QK_TEST_WORKERS")
        .output()
        .unwrap(),
    );
    for id in ["a", "b"] {
        assert_eq!(
            thread_record(temp.path(), id),
            json!({"threads": "4", "workers": "4"})
        );
    }
    assert_eq!(
        thread_record(temp.path(), "watch"),
        json!({"threads": null, "workers": null})
    );
}

#[test]
fn dependents_of_continuous_tasks_are_keyed_with_their_server_s_inputs() {
    for (dependency, server_counts) in [
        (json!("serve"), true),
        (json!({"target": "serve", "inputs": false}), false),
    ] {
        let mut serve = continuous_target("serve", "serve");
        serve["inputs"] = json!(["{projectRoot}/server.txt"]);
        let temp = fixture(json!({
            "serve": serve,
            "bench": {
                "cache": true,
                "inputs": [],
                "dependsOn": [dependency],
                "command": "echo ran >> runs.log",
            },
        }));
        fs::write(temp.path().join("server.txt"), "one").unwrap();
        for _ in 0..2 {
            let output = success(run(temp.path(), &["run", "app:bench"]));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains("cache bypassed"), "{stderr}");
        }
        // As in Nx, the server's inputs count as the task's own, unless its
        // dependsOn entry leaves them out. On Windows, cmd writes `ran \r\n`.
        fs::write(temp.path().join("server.txt"), "two").unwrap();
        success(run(temp.path(), &["run", "app:bench"]));
        let runs = fs::read_to_string(temp.path().join("runs.log")).unwrap();
        let expected = if server_counts { 2 } else { 1 };
        assert_eq!(runs.lines().count(), expected, "{dependency}: {runs:?}");
    }
}

/// `serve` creates `marker` after a pause and then announces it, and `e2e`
/// fails unless `marker` exists when it starts.
#[cfg(unix)]
fn served_fixture(ready_when: Value, wait_for: Option<&str>) -> TempDir {
    let mut dependency = json!({"target": "serve"});
    if let Some(wait_for) = wait_for {
        dependency["waitFor"] = json!(wait_for);
    }
    fixture(json!({
        "serve": {
            "continuous": true,
            "readyWhen": ready_when,
            "command": "sleep 0.5; touch marker; echo listening; sleep 30"
        },
        "e2e": {"command": "test -f marker", "dependsOn": [dependency]}
    }))
}

#[cfg(unix)]
#[test]
fn tasks_waiting_for_ready_start_once_the_check_passes() {
    for ready_when in [
        json!({"logMatches": "listening"}),
        json!({"command": "test -f marker", "interval": 50}),
    ] {
        let temp = served_fixture(ready_when.clone(), Some("ready"));
        let output = run(temp.path(), &["run", "app:e2e"]);
        assert!(
            output.status.success(),
            "{ready_when}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("waiting for app:serve to be ready")
        );
    }
    // Without waitFor, the task starts as soon as the server has.
    let temp = served_fixture(json!({"logMatches": "listening"}), None);
    assert!(!run(temp.path(), &["run", "app:e2e"]).status.success());
}

#[cfg(unix)]
#[test]
fn a_failed_check_fails_the_server_and_skips_the_tasks_waiting_for_it() {
    let temp = served_fixture(
        json!({"logMatches": "never", "timeout": 200}),
        Some("ready"),
    );
    let output = run(temp.path(), &["run", "app:e2e"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "Task \"app:serve\" did not become ready within 200ms (readyWhen: logMatches \"never\")"
        ),
        "{stderr}"
    );
    let reason = stderr.find("did not become ready").unwrap();
    let skipped = stderr.find("skipped app:e2e (dependency failed)").unwrap();
    assert!(reason < skipped, "{stderr}");
    assert!(stderr.contains("failed app:serve (exit 1)"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn an_invalid_check_fails_only_when_something_waits_for_it() {
    let temp = served_fixture(json!({"port": "{options.port}"}), Some("ready"));
    let output = run(temp.path(), &["run", "app:e2e"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has an invalid \"readyWhen\": \"{options.port}\" has no value"),
        "{stderr}"
    );
    // The server never ran, so the waiting task is skipped, not told it exited.
    assert!(
        stderr.contains("skipped app:e2e (dependency failed)"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("exited before it became ready"),
        "{stderr}"
    );
    // Nothing waits: it runs unchecked, with a warning.
    let temp = fixture(json!({
        "serve": {"continuous": true, "readyWhen": {"port": "{options.port}"}, "command": "touch marker; sleep 30"},
        "e2e": {"command": "sleep 0.5; test -f marker", "dependsOn": ["serve"]}
    }));
    let output = success(run(temp.path(), &["run", "app:e2e"]));
    assert!(String::from_utf8_lossy(&output.stderr).contains("runs without a readiness check"));
}

#[cfg(unix)]
#[test]
fn a_null_ready_when_turns_off_a_target_default() {
    let defaults = json!({"targetDefaults": {"serve": {"readyWhen": {"logMatches": "never", "timeout": 100}}}});
    // Without the null, the default's check applies and times out.
    let control = fixture(json!({
        "serve": {"continuous": true, "command": "sleep 30"},
        "e2e": {"command": "true", "dependsOn": [{"target": "serve", "waitFor": "ready"}]}
    }));
    fs::write(control.path().join("nx.json"), defaults.to_string()).unwrap();
    let output = run(control.path(), &["run", "app:e2e"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("did not become ready within 100ms"),
        "{stderr}"
    );
    let temp = served_fixture(Value::Null, Some("ready"));
    fs::write(temp.path().join("nx.json"), defaults.to_string()).unwrap();
    // Unchecked, a waiting task starts once the server has started.
    let output = run(temp.path(), &["run", "app:e2e"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("did not become ready"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn tasks_waiting_for_a_server_that_exits_before_it_is_ready_fail() {
    let temp = fixture(json!({
        "serve": {"continuous": true, "readyWhen": {"logMatches": "never"}, "command": "exit 0"},
        "e2e": {"command": "echo e2e-ran", "dependsOn": [{"target": "serve", "waitFor": "ready"}]}
    }));
    let output = run(temp.path(), &["run", "app:e2e"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // As in Nx, the server keeps its own status and the waiting task fails.
    assert!(
        stderr.contains("qk: app:e2e: Task \"app:serve\" exited before it became ready."),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("e2e-ran"));
}

#[test]
fn output_styles_follow_nx() {
    let temp = fixture(json!({
        "build": {"command": "echo one && echo two"},
        "fail": {"command": "echo broken && exit 3"},
        "cached": {"command": "echo cached", "cache": true, "inputs": []}
    }));
    let stdout = |args: &[&str]| {
        let output = command(temp.path(), args)
            .env_remove("NX_DEFAULT_OUTPUT_STYLE")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n")
            .replace(" \n", "\n")
    };
    let stream = stdout(&["run-many", "-t", "build", "--output-style", "stream"]);
    assert_eq!(stream, "app: one\napp: two\n");
    let raw = stdout(&[
        "run-many",
        "-t",
        "build",
        "--output-style",
        "stream-without-prefixes",
    ]);
    assert_eq!(raw, "one\ntwo\n");
    // Without a terminal or CI, several tasks default to quiet, which shows a
    // failed task's output only; one task defaults to raw.
    assert_eq!(stdout(&["run-many", "-t", "build"]), "");
    assert_eq!(
        stdout(&["run-many", "-t", "fail"]),
        "\n✖ nx run app:fail failed\n\nbroken\n"
    );
    assert_eq!(stdout(&["run", "app:build"]), "one\ntwo\n");
    // Held output of a failing task still appears.
    assert!(
        stdout(&["run-many", "-t", "fail", "--output-style", "tui"])
            .contains("> nx run app:fail\n\nbroken")
    );

    // A cache hit replays its log under the same header, marked as such; with
    // no outputs to restore, they match what is on disk.
    stdout(&["run-many", "-t", "cached", "--output-style", "static"]);
    assert_eq!(
        stdout(&["run-many", "-t", "cached", "--output-style", "static"]),
        "\n> nx run app:cached  [existing outputs match the cache, left as is]\n\ncached\n"
    );

    let grouped = command(
        temp.path(),
        &["run-many", "-t", "build", "--output-style", "static"],
    )
    .env("GITHUB_ACTIONS", "true")
    .output()
    .unwrap();
    let grouped = String::from_utf8(grouped.stdout)
        .unwrap()
        .replace("\r\n", "\n")
        .replace(" \n", "\n");
    assert!(
        grouped.starts_with("\n::group::✅ > nx run app:build\n\none\ntwo\n::endgroup::\n"),
        "{grouped}"
    );
}

/// Runs qk outside CI, without colour, and returns its stdout and stderr.
fn plain(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String, Option<i32>) {
    let output = command(root, args)
        .env_remove("NX_DEFAULT_OUTPUT_STYLE")
        .env("NO_COLOR", "1")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let text = |bytes: Vec<u8>| {
        String::from_utf8(bytes)
            .unwrap()
            .replace("\r\n", "\n")
            .replace(" \n", "\n")
    };
    (
        text(output.stdout),
        text(output.stderr),
        output.status.code(),
    )
}

#[test]
fn static_failures_only_collapses_what_did_not_fail_as_nx_does() {
    let temp = fixture(json!({
        "dep": {"command": "echo dep-output"},
        "build": {"command": "echo built", "dependsOn": ["dep"]},
        "fail": {"command": "echo broken && exit 3"}
    }));
    let (stdout, stderr, _) = plain(
        temp.path(),
        &[
            "run-many",
            "-t",
            "dep",
            "fail",
            "--output-style",
            "static-failures-only",
        ],
        &[],
    );
    assert!(stdout.contains("✔  nx run app:dep\n"), "{stdout}");
    assert!(!stdout.contains("dep-output"), "{stdout}");
    assert!(stdout.contains("> nx run app:fail\n\nbroken"), "{stdout}");
    assert!(
        stderr.contains(
            "Output of 1 successful task was not shown. Run with --verbose or --output-style=static to see it."
        ),
        "{stderr}"
    );
    // --verbose prints every task in full, as `static` does.
    let (stdout, _, _) = plain(
        temp.path(),
        &[
            "run-many",
            "-t",
            "dep",
            "--output-style",
            "static-failures-only",
            "--verbose",
        ],
        &[],
    );
    assert!(
        stdout.contains("> nx run app:dep\n\ndep-output"),
        "{stdout}"
    );
    // A single task shows its own project's tasks in full, and the others
    // collapsed; NX_DEFAULT_OUTPUT_STYLE names the style too.
    let (stdout, _, _) = plain(
        temp.path(),
        &["run", "app:build"],
        &[("NX_DEFAULT_OUTPUT_STYLE", "static-failures-only")],
    );
    assert!(
        stdout.contains("> nx run app:dep\n\ndep-output"),
        "{stdout}"
    );
    assert!(stdout.contains("> nx run app:build\n\nbuilt"), "{stdout}");
}

#[test]
fn summary_prints_counts_and_where_failed_output_is() {
    let temp = fixture(json!({
        "ok": {"command": "echo fine"},
        "fail": {"command": "echo broken && exit 3"},
        "after": {"command": "echo never", "dependsOn": ["fail"]}
    }));
    let (stdout, stderr, _) = plain(
        temp.path(),
        &["run-many", "-t", "ok", "after", "--output-style=summary"],
        &[],
    );
    assert!(
        !stdout.contains("fine") && !stdout.contains("broken"),
        "{stdout}"
    );
    assert!(
        stderr.contains("3 tasks: 1 succeeded, 1 failed, 1 skipped"),
        "{stderr}"
    );
    assert!(stderr.contains("✖ nx run app:fail\n"), "{stderr}");
    let log = stderr
        .lines()
        .find_map(|line| line.trim().strip_prefix("full log: "))
        .unwrap_or_else(|| panic!("{stderr}"));
    // On Windows, cmd writes `broken \r\n`.
    assert_eq!(fs::read_to_string(log).unwrap().trim_end(), "broken");
    assert!(stderr.contains("Re-run with --output-style=static to inline logs."));
}

/// A task that fails before it prints anything, as one waiting for a server
/// that exits does, names no log, rather than an earlier run's.
#[cfg(unix)]
#[test]
fn summary_never_names_an_earlier_run_s_log() {
    let temp = fixture(json!({"e2e": {"command": "echo broken && exit 3"}}));
    let (_, stderr, _) = plain(
        temp.path(),
        &["run", "app:e2e", "--output-style=summary"],
        &[],
    );
    assert!(stderr.contains("full log: "), "{stderr}");
    fs::write(
        temp.path().join("project.json"),
        json!({"name": "app", "targets": {
            "serve": {"continuous": true, "readyWhen": {"logMatches": "never"}, "command": "exit 0"},
            "e2e": {"command": "echo e2e-ran", "dependsOn": [{"target": "serve", "waitFor": "ready"}]}
        }})
        .to_string(),
    )
    .unwrap();
    let (_, stderr, code) = plain(
        temp.path(),
        &["run", "app:e2e", "--output-style=summary"],
        &[],
    );
    assert_ne!(code, Some(0));
    assert!(stderr.contains("exited before it became ready"), "{stderr}");
    assert!(!stderr.contains("full log: "), "{stderr}");
}

#[test]
fn unknown_output_styles_fail_and_unknown_defaults_are_ignored() {
    let temp = fixture(json!({"build": {"command": "echo built"}}));
    let (_, stderr, code) = plain(
        temp.path(),
        &["run", "app:build", "--output-style=fancy"],
        &[],
    );
    assert_eq!(code, Some(2));
    assert!(stderr.contains("fancy"), "{stderr}");
    let (stdout, _, code) = plain(
        temp.path(),
        &["run", "app:build"],
        &[("NX_DEFAULT_OUTPUT_STYLE", "fancy")],
    );
    assert_eq!((stdout.as_str(), code), ("built\n", Some(0)));
}

#[test]
fn ci_is_detected_as_nx_detects_it() {
    let temp = fixture(json!({"build": {"command": "echo built"}}));
    // In CI a run defaults to static, which heads each task; CI=0 is CI.
    for env in [[("CI", "0")], [("BUILDKITE", "true")]] {
        let (stdout, _, _) = plain(temp.path(), &["run-many", "-t", "build"], &env);
        assert!(
            stdout.contains("> nx run app:build\n\nbuilt"),
            "{env:?}: {stdout}"
        );
    }
    let (stdout, _, _) = plain(
        temp.path(),
        &["run-many", "-t", "build"],
        &[("CI", "false")],
    );
    assert_eq!(stdout, "");
}

#[test]
fn force_color_zero_keeps_qk_output_plain() {
    let temp = fixture(json!({"fail": {"command": "exit 3"}}));
    let output = command(
        temp.path(),
        &["run-many", "-t", "fail", "--output-style", "static"],
    )
    .env("CI", "true")
    .env("FORCE_COLOR", "0")
    .env_remove("NO_COLOR")
    .output()
    .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Running target fail"), "{stderr}");
    assert!(!stderr.contains('\x1b'), "{stderr:?}");
}

#[test]
fn long_running_targets_stream_unless_a_static_style_is_named() {
    let temp = fixture(json!({
        "serve": {"command": "echo serving"},
        "build": {"command": "echo built"}
    }));
    // CI defaults to static without naming it, so a server's output streams,
    // without a prefix unless NX_PREFIX_OUTPUT asks for one, as in Nx.
    let (stdout, _, _) = plain(
        temp.path(),
        &["run-many", "-t", "serve", "build"],
        &[("CI", "true")],
    );
    assert!(stdout.lines().any(|line| line == "serving"), "{stdout}");
    assert!(stdout.contains("> nx run app:build\n\nbuilt"), "{stdout}");
    let (stdout, _, _) = plain(
        temp.path(),
        &["run-many", "-t", "serve"],
        &[("CI", "true"), ("NX_PREFIX_OUTPUT", "true")],
    );
    assert!(stdout.contains("app: serving"), "{stdout}");
    let (stdout, _, _) = plain(
        temp.path(),
        &["run-many", "-t", "serve", "--output-style", "static"],
        &[],
    );
    assert!(stdout.contains("> nx run app:serve\n\nserving"), "{stdout}");
    // NX_STREAM_OUTPUT decides for every task.
    let (stdout, _, _) = plain(
        temp.path(),
        &["run-many", "-t", "build", "--output-style", "static"],
        &[("NX_STREAM_OUTPUT", "true")],
    );
    assert!(
        stdout.contains("built") && !stdout.contains("> nx run"),
        "{stdout}"
    );
}

#[test]
fn large_output_preserves_static_groups_quiet_failures_and_long_line_prefixes() {
    let temp = fixture(json!({
        "a": helper_target("large-output", "a"),
        "b": helper_target("large-output", "b"),
        "fail": helper_target("large-output", "fail"),
        "colored": helper_target("large-colored-output", "colored"),
        "controls": helper_target("large-string-control-output", "controls")
    }));
    let invoke = |targets: &str, style: &str| {
        command(
            temp.path(),
            &[
                "run-many",
                "-t",
                targets,
                "--output-style",
                style,
                "--parallel",
                "2",
            ],
        )
        .env("GITHUB_ACTIONS", "true")
        .env("NO_COLOR", "1")
        .env_remove("FORCE_COLOR")
        .output()
        .unwrap()
    };
    let body = |id: &str| {
        let mut body = format!("BEGIN-{id}\n").into_bytes();
        body.extend_from_slice(&vec![b'x'; 256 * 1024]);
        body.extend_from_slice(b"\0\xff\n");
        body.extend_from_slice(format!("END-{id}\n").as_bytes());
        body
    };
    let output = success(invoke("a,b", "static"));
    for id in ["a", "b"] {
        let header = format!("::group::✅ > nx run app:{id}\n\n");
        let start = output
            .stdout
            .windows(header.len())
            .position(|bytes| bytes == header.as_bytes())
            .unwrap()
            + header.len();
        let end = output.stdout[start..]
            .windows(b"::endgroup::\n".len())
            .position(|bytes| bytes == b"::endgroup::\n")
            .unwrap()
            + start;
        let grouped = &output.stdout[start..end];
        let expected = body(id);
        assert!(grouped.ends_with(&expected));
        assert!(
            !grouped
                .windows(b"::group::".len())
                .any(|bytes| bytes == b"::group::")
        );
    }
    assert!(success(invoke("a", "quiet")).stdout.is_empty());
    let failed = invoke("fail", "quiet");
    assert!(!failed.status.success());
    assert!(
        failed
            .stdout
            .starts_with("\n✖ nx run app:fail failed\n\n".as_bytes())
    );
    assert!(failed.stdout.ends_with(&body("fail")));

    let prefixed = success(invoke("a", "stream"));
    let mut expected = b"app: ".to_vec();
    expected.extend_from_slice(&vec![b'x'; 256 * 1024]);
    expected.extend_from_slice(b"\0\xff\napp: END-a\n");
    assert!(prefixed.stdout.ends_with(&expected));
    let colored = invoke("colored", "quiet");
    assert!(!colored.status.success());
    let mut expected = vec![b'x'; 32 * 1024];
    expected.push(b'\n');
    assert!(colored.stdout.ends_with(&expected));
    assert!(!colored.stdout.contains(&0x1b));
    let controls = invoke("controls", "quiet");
    assert!(!controls.status.success());
    assert!(controls.stdout.ends_with(b"beforeafterlink\n"));
    assert!(!controls.stdout.contains(&0x1b));
}

#[test]
fn a_static_run_streams_its_task_between_banners_like_nx_run() {
    let temp = fixture(json!({
        "dep": {"command": "echo dep", "cache": true, "inputs": []},
        "build": {"command": "echo built", "dependsOn": ["dep"]},
        "fail": {"command": "echo broken && exit 3", "dependsOn": ["dep"]}
    }));
    let run = |task: &str| {
        let output = command(temp.path(), &["run", task])
            .env_remove("NX_DEFAULT_OUTPUT_STYLE")
            .env("CI", "true")
            .env("GITHUB_ACTIONS", "true")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap().replace("\r\n", "\n");
        (text(output.stdout), text(output.stderr))
    };
    let (stdout, stderr) = run("app:build");
    assert_eq!(
        stdout,
        "\n::group::✅ > nx run app:dep\n\ndep\n::endgroup::\n\n> nx run app:build\n\nbuilt\n"
    );
    assert!(
        stderr.starts_with(
            "\n QK   Running target build for project app and 1 task it depends on:\n"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "\n QK   Successfully ran target build for project app and 1 task it depends on\n"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("qk: "), "{stderr}");
    // A dependency from the cache shows only its header.
    let (stdout, _) = run("app:build");
    assert!(
        stdout.starts_with(
            "\n::group::⏩ > nx run app:dep  [existing outputs match the cache, left as is]\n\n::endgroup::\n"
        ),
        "{stdout}"
    );
    let (_, stderr) = run("app:fail");
    assert!(
        stderr.contains("Running target fail for project app and 1 task it depends on failed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Failed tasks:\n\n- app:fail (exit 3)\n"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn tasks_load_their_dotenv_files_and_nx_variables_like_nx() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(
        root.join("app/project.json"),
        json!({"name": "app", "targets": {
            "show": {"command": "echo $ROOT_ONLY $SHARED $SPECIFIC $FROM_PROCESS $FROM_TARGET $NX_TASK_TARGET_PROJECT:$NX_TASK_TARGET_TARGET $FORCE_COLOR",
                     "options": {"env": {"FROM_TARGET": "target"}}}
        }})
        .to_string(),
    )
    .unwrap();
    fs::write(
        root.join(".env"),
        "ROOT_ONLY=root\nSHARED=root\nFROM_TARGET=root\nFROM_PROCESS=root\n",
    )
    .unwrap();
    // The project's target-specific file wins over the root's general one.
    fs::write(
        root.join("app/.env.show"),
        "SHARED=project\nSPECIFIC=show\n",
    )
    .unwrap();
    let output = command(root, &["run", "app:show"])
        .env("FROM_PROCESS", "process")
        .env_remove("FORCE_COLOR")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(success(output).stdout).unwrap(),
        "root project show process target app:show true\n"
    );
}

#[cfg(unix)]
#[test]
fn a_task_s_dotenv_files_expand_across_each_other_as_in_nx() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(
        root.join("app/project.json"),
        json!({"name": "app", "targets": {"show": {"command": "echo $URL $FROM_ROOT"}}})
            .to_string(),
    )
    .unwrap();
    // The project's file names what only the root's defines.
    fs::write(root.join("app/.env.show"), "URL=http://${HOST}:${PORT}\n").unwrap();
    fs::write(
        root.join(".env"),
        "HOST=localhost\nPORT=8080\nFROM_ROOT=$HOST\n",
    )
    .unwrap();
    let output = success(command(root, &["run", "app:show"]).output().unwrap());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "http://localhost:8080 localhost\n"
    );
}

#[cfg(unix)]
#[test]
fn root_dotenv_files_set_run_controls_as_in_nx() {
    let temp = fixture(json!({"build": {"command": "echo built"}}));
    // The root dotenv files load into qk's own environment, `.local.env`
    // first.
    fs::write(
        temp.path().join(".local.env"),
        "NX_DEFAULT_OUTPUT_STYLE=static\n",
    )
    .unwrap();
    fs::write(temp.path().join(".env"), "NX_DEFAULT_OUTPUT_STYLE=stream\n").unwrap();
    let output = command(temp.path(), &["run-many", "-t", "build"])
        .env_remove("NX_DEFAULT_OUTPUT_STYLE")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("> nx run app:build"), "{stdout}");
    // NX_LOAD_DOT_ENV_FILES=false leaves them out.
    let output = command(temp.path(), &["run-many", "-t", "build"])
        .env_remove("NX_DEFAULT_OUTPUT_STYLE")
        .env("NX_LOAD_DOT_ENV_FILES", "false")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
}

#[cfg(unix)]
#[test]
fn without_dotenv_files_a_shell_variable_matching_a_root_file_stays() {
    let temp = fixture(json!({"show": {"command": "echo url=$API_URL"}}));
    fs::write(temp.path().join(".env"), "API_URL=x\n").unwrap();
    let output = command(temp.path(), &["run", "app:show"])
        .env("NX_LOAD_DOT_ENV_FILES", "false")
        .env("API_URL", "x")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(success(output).stdout).unwrap(),
        "url=x\n"
    );
}

#[cfg(unix)]
#[test]
fn an_atomized_target_reads_its_own_dotenv_files() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::create_dir_all(root.join("app")).unwrap();
    let atomized = |spec: &str| json!({"command": "echo $SPEC", "metadata": {"nonAtomizedTarget": "e2e-ci", "spec": spec}});
    fs::write(
        root.join("app/project.json"),
        json!({
            "name": "app",
            "metadata": {"targetGroups": {"E2E (CI)": ["e2e-ci--a", "e2e-ci--b", "e2e-ci"]}},
            "targets": {
                "e2e-ci--a": atomized("a"),
                "e2e-ci--b": atomized("b"),
                "e2e-ci": {"executor": "nx:noop"},
            },
        })
        .to_string(),
    )
    .unwrap();
    fs::write(root.join("app/.env.e2e-ci--a"), "SPEC=a\n").unwrap();
    fs::write(root.join("app/.env.e2e-ci--b"), "SPEC=b\n").unwrap();
    let output = success(command(root, &["run", "app:e2e-ci--b"]).output().unwrap());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "b\n");
}

#[cfg(unix)]
#[test]
fn tasks_see_the_variables_nx_sets_and_not_those_of_an_enclosing_run() {
    let temp = fixture(json!({
        "test": {"command": "echo verbose=$NX_VERBOSE_LOGGING capture=$NX_TERMINAL_CAPTURE_STDERR cli=$NX_SET_CLI stream=$NX_STREAM_OUTPUT hash=${NX_TASK_HASH:+set} pids=$NX_INVOCATION_ANCESTOR_PIDS"}
    }));
    let output = command(temp.path(), &["run", "app:test"])
        .env_remove("NX_VERBOSE_LOGGING")
        .env("NX_SET_CLI", "true")
        .env("NX_STREAM_OUTPUT", "true")
        .env("NX_INVOCATION_ANCESTOR_PIDS", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(success(output).stdout).unwrap();
    let pids = stdout.trim().rsplit_once("pids=").unwrap().1.to_owned();
    assert!(pids.starts_with("1,") && pids.len() > 2, "{stdout}");
    assert!(
        stdout.starts_with("verbose=false capture=true cli= stream= hash=set "),
        "{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn a_task_running_itself_again_is_refused_as_in_nx() {
    let qk = env!("CARGO_BIN_EXE_qk");
    let temp = fixture(json!({
        "loop": {"command": format!("'{qk}' run app:loop")},
        "outer": {"command": format!("'{qk}' run app:inner")},
        "inner": {"command": "echo inner"}
    }));
    let output = run(temp.path(), &["run", "app:loop"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Recursive task invocation detected")
            && stderr.contains("app:loop -> app:loop"),
        "{stderr}"
    );
    // Running another task from a task is fine.
    let output = success(run(temp.path(), &["run", "app:outer"]));
    assert!(String::from_utf8_lossy(&output.stdout).contains("inner"));
}

#[cfg(unix)]
#[test]
fn unknown_run_commands_options_are_forwarded_like_nx() {
    let temp = fixture(json!({
        "serve": {"command": "echo serving", "options": {"port": 3000, "dependsOn": ["^tsc"]}},
        "named": {"command": "echo port {args.port}", "options": {"port": 3000}}
    }));
    let stdout = |args: &[&str]| String::from_utf8(success(run(temp.path(), args)).stdout).unwrap();
    assert_eq!(stdout(&["run", "app:serve"]), "serving --port=3000\n");
    assert_eq!(
        stdout(&["run", "app:serve", "--", "--port=4000"]),
        "serving --port=4000\n"
    );
    assert_eq!(stdout(&["run", "app:named"]), "port 3000\n");
}

#[cfg(unix)]
#[test]
fn target_names_with_colons_and_missing_arguments_resolve_like_nx() {
    let temp = fixture(json!({
        "install:ios": {"command": "echo installing ios"},
        "install": {"command": "echo install {args.platform}done", "configurations": {"ios": {"command": "echo configured ios"}}}
    }));
    let stdout = |args: &[&str]| String::from_utf8(success(run(temp.path(), args)).stdout).unwrap();
    // The project has a target named install:ios, so that wins.
    assert_eq!(stdout(&["run", "app:install:ios"]), "installing ios\n");
    assert_eq!(
        stdout(&["run", "app:install", "-c", "ios"]),
        "configured ios\n"
    );
    // A missing {args.platform} interpolates as nothing.
    assert_eq!(stdout(&["run", "app:install"]), "install done\n");
}

#[cfg(unix)]
#[test]
fn quiet_output_is_plain_text_for_agents() {
    let temp = fixture(json!({
        "fail": {"command": "printf '\\033[31mred failure\\033[0m\\n' && exit 2"},
        "ok": {"command": "echo fine"}
    }));
    let output = command(temp.path(), &["run-many", "-t", "fail", "ok"])
        .env_remove("FORCE_COLOR")
        .env_remove("NX_DEFAULT_OUTPUT_STYLE")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Only the failure, without escape sequences.
    assert_eq!(stdout, "\n✖ nx run app:fail failed\n\nred failure\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("1 of 2 tasks failed"), "{stderr}");
    assert!(!stderr.contains("qk: checking"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn the_args_option_is_forwarded_and_readable_like_nx() {
    let temp = fixture(json!({
        "text": {"command": "echo run", "options": {"args": "--watch=false --max-workers 2"}},
        "list": {"command": "echo run", "options": {"args": ["--one", "--two=2"]}},
        "named": {"command": "echo workers {args.maxWorkers}", "options": {"args": "--max-workers=3"}},
        "all": {"command": "echo all {args}", "options": {"port": 1, "args": "--last"}},
        "both": {"command": "echo {args} {args.port}"}
    }));
    let stdout = |args: &[&str]| String::from_utf8(success(run(temp.path(), args)).stdout).unwrap();
    assert_eq!(
        stdout(&["run", "app:text"]),
        "run --watch=false --max-workers 2\n"
    );
    assert_eq!(
        stdout(&["run", "app:text", "--", "--extra"]),
        "run --watch=false --max-workers 2 --extra\n"
    );
    assert_eq!(stdout(&["run", "app:list"]), "run --one --two=2\n");
    // Read by its camel-case name too, and an argument wins.
    assert_eq!(stdout(&["run", "app:named"]), "workers 3\n");
    assert_eq!(
        stdout(&["run", "app:named", "--", "--maxWorkers=5"]),
        "workers 5\n"
    );
    // In {args}, as in Nx, the option comes after the arguments.
    assert_eq!(
        stdout(&["run", "app:all", "--", "--cli"]),
        "all --port=1 --cli --last\n"
    );
    // Arguments naming a run-commands option set it instead of reaching the command.
    assert_eq!(
        stdout(&["run", "app:text", "--", "--args=--from-cli"]),
        "run --from-cli\n"
    );
    let both = run(temp.path(), &["run", "app:both"]);
    assert!(!both.status.success());
    assert!(
        String::from_utf8_lossy(&both.stderr)
            .contains("should not contain both {args} and {args.*} values")
    );
}

#[cfg(unix)]
#[test]
fn arguments_naming_run_commands_options_set_them() {
    let temp = fixture(json!({
        "where": {"command": "echo $(basename \"$PWD\") $MODE"}
    }));
    fs::create_dir(temp.path().join("sub")).unwrap();
    let output = success(run(
        temp.path(),
        &["run", "app:where", "--", "--cwd=sub", "--env.MODE=cli"],
    ));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "sub cli\n");
    // As in Nx, their tokens resolve for the task's project.
    let output = success(run(
        temp.path(),
        &[
            "run",
            "app:where",
            "--",
            "--cwd={workspaceRoot}/{projectRoot}/sub",
            "--env.MODE={projectName}",
        ],
    ));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "sub app\n");
}

#[cfg(unix)]
#[test]
fn serving_dependencies_reject_exclusive_tasks_before_running_commands() {
    for continuous in [false, true] {
        for exclusive_server in [false, true] {
            let temp = fixture(json!({
                "serve": {
                    "command": "touch server-started; echo ready; sleep 2",
                    "continuous": continuous,
                    "parallelism": !exclusive_server,
                    "options": {"readyWhen": "ready"}
                },
                "build": {
                    "command": "touch build-started",
                    "parallelism": exclusive_server,
                    "dependsOn": ["serve"]
                }
            }));
            let mut child = command(temp.path(), &["run", "app:build"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("exclusive task validation did not finish");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(!output.status.success());
            let kind = if continuous {
                "continuous"
            } else {
                "readyWhen"
            };
            let expected = if exclusive_server {
                format!(
                    "{kind} task app:serve does not support parallelism but app:build depends on it"
                )
            } else {
                format!(
                    "app:build does not support parallelism but depends on {kind} task app:serve"
                )
            };
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(&expected), "{stderr}");
            assert!(!temp.path().join("server-started").exists());
            assert!(!temp.path().join("build-started").exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn ready_when_starts_dependents_once_the_output_appears() {
    let temp = fixture(json!({
        // The marker is written just before the ready text.
        "serve": {"command": "sleep 0.3; touch marker; echo 'listening on :3000'; sleep 30", "options": {"readyWhen": "listening"}},
        "watch": {"command": "sleep 0.3; touch watched; echo up; sleep 30", "continuous": true, "options": {"readyWhen": ["up"]}},
        "e2e": {"command": "test -e marker && test -e watched && echo e2e saw both", "dependsOn": ["serve", "watch"]},
        "both": {"command": "echo one; echo two >&2; sleep 30", "options": {"readyWhen": ["one", "two"]}},
        "crash": {"command": "exit 3", "options": {"readyWhen": "never"}},
        "after": {"command": "echo after", "dependsOn": ["crash"]},
        "serial": {"executor": "nx:run-commands", "options": {"commands": ["echo a"], "readyWhen": "a", "parallel": false}}
    }));
    let started = Instant::now();
    let output = success(run(
        temp.path(),
        &["run", "app:e2e", "--output-style", "static"],
    ));
    assert!(String::from_utf8_lossy(&output.stdout).contains("e2e saw both"));
    // Both were stopped once nothing needed them.
    assert!(started.elapsed() < Duration::from_secs(15));
    // Requested directly, the task is done once ready.
    let started = Instant::now();
    success(run(temp.path(), &["run", "app:both"]));
    assert!(started.elapsed() < Duration::from_secs(15));
    let crashed = run(
        temp.path(),
        &["run", "app:after", "--output-style", "static"],
    );
    assert_eq!(crashed.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&crashed.stdout).contains("after\n"));
    let serial = run(temp.path(), &["run", "app:serial"]);
    assert!(
        String::from_utf8_lossy(&serial.stderr)
            .contains("readyWhen can only be used when parallel is true")
    );
}

#[cfg(unix)]
#[test]
fn env_files_color_and_the_options_nx_sets_itself() {
    let temp = fixture(json!({
        "show": {"command": "echo $FROM_FILE $BOTH $SET_OUTSIDE", "options": {
            "envFile": "config/{projectName}.env", "env": {"BOTH": "option"}
        }},
        "missing": {"command": "true", "options": {"envFile": "absent.env"}},
        "color": {"command": "echo $FORCE_COLOR", "options": {"color": true, "tty": true, "usePty": false, "streamOutput": true, "verbose": false}}
    }));
    fs::create_dir(temp.path().join("config")).unwrap();
    fs::write(
        temp.path().join("config/app.env"),
        "FROM_FILE=file\nBOTH=file\nSET_OUTSIDE=file\n",
    )
    .unwrap();
    fs::write(temp.path().join(".env"), "FROM_DOTENV=dotenv\n").unwrap();
    let stdout = |output: Output| String::from_utf8(success(output).stdout).unwrap();
    assert_eq!(
        stdout(
            command(temp.path(), &["run", "app:show"])
                .env("SET_OUTSIDE", "process")
                .output()
                .unwrap()
        ),
        "file option process\n"
    );
    let missing = run(temp.path(), &["run", "app:missing"]);
    assert!(String::from_utf8_lossy(&missing.stderr).contains("envFile absent.env does not exist"));
    assert_eq!(
        stdout(
            command(temp.path(), &["run", "app:color"])
                .env("FORCE_COLOR", "0")
                .output()
                .unwrap()
        ),
        "true\n"
    );
    // NX_LOAD_DOT_ENV_FILES=false turns every dotenv file off, as in Nx.
    let temp = fixture(
        json!({"show": {"command": "echo [$FROM_DOTENV] [$FROM_FILE]", "options": {"envFile": "absent.env"}}}),
    );
    fs::write(temp.path().join(".env"), "FROM_DOTENV=dotenv\n").unwrap();
    assert_eq!(
        stdout(
            command(temp.path(), &["run", "app:show"])
                .env("NX_LOAD_DOT_ENV_FILES", "false")
                .output()
                .unwrap()
        ),
        "[] []\n"
    );
}

#[cfg(unix)]
#[test]
fn argument_placeholders_fall_back_like_nx_24() {
    let temp = fixture(json!({
        "port": {"command": "echo port {args.port ?? env.PORT ?? 4200}", "options": {"env": {"FROM_ENV": "env"}}},
        "zero": {"command": "echo {args.count ?? 1}"},
        "chained": {"command": "echo {args.first ?? args.second ?? env.FROM_ENV}", "options": {"env": {"FROM_ENV": "env"}}},
        "nested": {"command": "echo {args.host ?? {projectName}.local}"},
        "glob": {"command": "echo '{env.ts,env.d.ts}' {other}"},
        "dead": {"command": "echo {args.a ?? fixed ?? args.b}"},
        "env_value": {"command": "echo \"$V\"", "options": {"env": {"V": "{args.v ?? env.FILE}"}}}
    }));
    let stdout = |output: Output| String::from_utf8(success(output).stdout).unwrap();
    let run_with = |args: &[&str], port: Option<&str>| {
        let mut command = command(temp.path(), args);
        command.env_remove("PORT");
        if let Some(port) = port {
            command.env("PORT", port);
        }
        command.output().unwrap()
    };
    assert_eq!(stdout(run_with(&["run", "app:port"], None)), "port 4200\n");
    assert_eq!(
        stdout(run_with(&["run", "app:port"], Some("5001"))),
        "port 5001\n"
    );
    // An empty value is unset.
    assert_eq!(
        stdout(run_with(&["run", "app:port"], Some(""))),
        "port 4200\n"
    );
    assert_eq!(
        stdout(run_with(
            &["run", "app:port", "--", "--port=3000"],
            Some("5001")
        )),
        "port 3000\n"
    );
    // 0 and false are values.
    assert_eq!(
        stdout(run(temp.path(), &["run", "app:zero", "--", "--count=0"])),
        "0\n"
    );
    assert_eq!(stdout(run(temp.path(), &["run", "app:chained"])), "env\n");
    assert_eq!(
        stdout(run(
            temp.path(),
            &["run", "app:chained", "--", "--second=two"]
        )),
        "two\n"
    );
    assert_eq!(
        stdout(run(temp.path(), &["run", "app:nested"])),
        "app.local\n"
    );
    // Braces that are no token stay text.
    assert_eq!(
        stdout(run(temp.path(), &["run", "app:glob"])),
        "{env.ts,env.d.ts} {other}\n"
    );
    // As in Nx, only commands take placeholders; an env value is as written.
    let output = command(temp.path(), &["run", "app:env_value", "--", "--v=x"])
        .env("FILE", "chosen.env")
        .output()
        .unwrap();
    assert_eq!(stdout(output), "{args.v ?? env.FILE} --v=x\n");
    let dead = run(temp.path(), &["run", "app:dead"]);
    assert!(
        String::from_utf8_lossy(&dead.stderr).contains("can never be used"),
        "{}",
        String::from_utf8_lossy(&dead.stderr)
    );
}

#[cfg(unix)]
#[test]
fn an_env_file_sets_what_the_task_environment_leaves_empty_like_nx_24() {
    let temp = fixture(json!({
        "show": {"command": "echo [$EMPTY] [$SET]", "options": {"envFile": "app.env"}}
    }));
    fs::write(temp.path().join("app.env"), "EMPTY=file\nSET=file\n").unwrap();
    let output = command(temp.path(), &["run", "app:show"])
        .env("EMPTY", "")
        .env("SET", "process")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(success(output).stdout).unwrap(),
        "[file] [process]\n"
    );
    // A project's dotenv file can turn the env file off, as the task's
    // environment decides.
    fs::write(
        temp.path().join(".env.show"),
        "NX_LOAD_DOT_ENV_FILES=false\n",
    )
    .unwrap();
    let output = command(temp.path(), &["run", "app:show"])
        .env_remove("EMPTY")
        .env_remove("SET")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(success(output).stdout).unwrap(),
        "[] []\n"
    );
}

#[cfg(unix)]
#[test]
fn command_entries_prefix_and_colour_their_output_like_nx() {
    let temp = fixture(json!({
        "prefixed": {"executor": "nx:run-commands", "options": {"commands": [
            {"command": "printf 'one\\n\\ntw'; sleep 0.1; printf 'o\\n'", "prefix": "[a]", "description": "documentation only"},
            "echo plain"
        ]}},
        "painted": {"executor": "nx:run-commands", "options": {"commands": [
            {"command": "echo hi", "prefix": "p", "prefixColor": "blue", "color": "red", "bgColor": "bgWhite"}
        ]}},
        "serial": {"executor": "nx:run-commands", "options": {"parallel": false, "commands": [
            {"command": "echo hi", "prefix": "p"}
        ]}},
        "unknown": {"executor": "nx:run-commands", "options": {"commands": [
            {"command": "echo hi", "color": "chartreuse"}
        ]}}
    }));
    let output = success(
        command(temp.path(), &["run", "app:prefixed"])
            .env("NO_COLOR", "1")
            .output()
            .unwrap(),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Blank lines stay bare, and a line split between reads gets one prefix.
    let mut lines: Vec<&str> = stdout.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["", "[a] one", "[a] two", "plain"], "{stdout:?}");
    let output = success(
        command(temp.path(), &["run", "app:painted"])
            .env_remove("NO_COLOR")
            .env("FORCE_COLOR", "1")
            .output()
            .unwrap(),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\x1b[47m\x1b[31m\x1b[1m\x1b[34mp\x1b[39m\x1b[22m hi\n\x1b[39m\x1b[49m"
    );
    let serial = run(temp.path(), &["run", "app:serial"]);
    assert!(
        String::from_utf8_lossy(&serial.stderr).contains("can only be set when parallel is true")
    );
    let unknown = run(temp.path(), &["run", "app:unknown"]);
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("\"chartreuse\" is not a colour"));
}

#[cfg(unix)]
#[test]
fn command_shapes_and_exit_codes_follow_nx() {
    let temp = fixture(json!({
        "words": {"executor": "nx:run-commands", "options": {"command": ["echo", "joined", "words"]}},
        "nothing": {"executor": "nx:run-commands", "options": {"commands": []}},
        "side": {"executor": "nx:run-commands", "options": {"commands": ["exit 4", "sleep 5"]}},
        "serial": {"executor": "nx:run-commands", "options": {"parallel": false, "commands": ["exit 4", "echo never"]}},
        "single": {"command": "exit 4"}
    }));
    let output = success(run(temp.path(), &["run", "app:words"]));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "joined words\n");
    success(run(temp.path(), &["run", "app:nothing"]));
    // Side by side, a failure stops the others and the task fails with 1.
    let started = Instant::now();
    assert_eq!(
        run(temp.path(), &["run", "app:side"]).status.code(),
        Some(1)
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(
        run(temp.path(), &["run", "app:serial"]).status.code(),
        Some(1)
    );
    assert_eq!(
        run(temp.path(), &["run", "app:single"]).status.code(),
        Some(1)
    );
}

#[cfg(unix)]
#[test]
fn the_package_manager_is_detected_like_nx() {
    use std::os::unix::fs::PermissionsExt;
    // nx.json cli.packageManager, then the lockfile, then packageManager.
    for (nx, lockfile, declared, expected) in [
        (Some("yarn"), Some("pnpm-lock.yaml"), None, "yarn"),
        (None, Some("bun.lock"), Some("pnpm@9.0.0"), "bun"),
        (None, Some("yarn.lock"), None, "yarn"),
        (None, None, Some("pnpm@9.0.0"), "pnpm"),
        (None, None, None, "npm"),
    ] {
        let temp = fixture(json!({"test": {}}));
        let mut config = json!({});
        if let Some(nx) = nx {
            config["cli"] = json!({"packageManager": nx});
        }
        fs::write(temp.path().join("nx.json"), config.to_string()).unwrap();
        if let Some(lockfile) = lockfile {
            fs::write(temp.path().join(lockfile), "").unwrap();
        }
        let mut package = json!({"name": "app", "scripts": {"test": "unused"}});
        if let Some(declared) = declared {
            package["packageManager"] = json!(declared);
        }
        fs::write(temp.path().join("package.json"), package.to_string()).unwrap();
        let bin = temp.path().join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        for manager in ["npm", "pnpm", "yarn", "bun"] {
            let path = bin.join(manager);
            fs::write(&path, format!("#!/bin/sh\necho {manager}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let output = success(
            command(temp.path(), &["run", "app:test"])
                .env_remove("npm_config_user_agent")
                .output()
                .unwrap(),
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            expected,
            "{nx:?} {lockfile:?} {declared:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn dependencies_by_glob_and_with_forwarded_options() {
    let temp = fixture(json!({
        "lint-js": {"command": "echo lint-js"},
        "lint-css": {"command": "echo lint-css"},
        "check": {"command": "echo check", "dependsOn": ["lint-*"]},
        "bundle": {"command": "echo bundle {args.mode} {args.region}"},
        "deploy": {
            "executor": "nx:noop",
            "options": {"mode": "production", "nested": {"region": "eu"}},
            "configurations": {"staging": {"mode": "staging"}},
            "dependsOn": [{"target": "bundle", "options": "forward"}]
        }
    }));
    let output = success(run(
        temp.path(),
        &["run", "app:check", "--output-style", "static"],
    ));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("lint-js\n") && stdout.contains("lint-css\n"),
        "{stdout}"
    );
    let output = success(run(
        temp.path(),
        &["run", "app:deploy", "--output-style", "static"],
    ));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("bundle production\n")
    );
    let output = success(run(
        temp.path(),
        &["run", "app:deploy:staging", "--output-style", "static"],
    ));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("bundle staging\n")
    );
}

#[cfg(unix)]
#[test]
fn the_task_a_run_is_for_reads_its_input() {
    use std::io::Write;
    let temp = fixture(json!({
        "read": {"command": "read line; echo got $line"},
        "other": {"command": "echo other"}
    }));
    let piped = |args: &[&str]| {
        let mut child = command(temp.path(), args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        String::from_utf8(success(child.wait_with_output().unwrap()).stdout).unwrap()
    };
    assert_eq!(piped(&["run", "app:read"]), "got hello\n");
    // Among several, no task takes the input.
    let several = piped(&["run-many", "-t", "read,other", "--output-style", "static"]);
    assert!(
        several.contains("got\n") && !several.contains("hello"),
        "{several}"
    );
}

#[cfg(unix)]
#[test]
fn the_task_a_run_is_for_gets_the_terminal() {
    let temp = fixture(json!({
        "read": {"command": "if [ -t 0 ]; then echo terminal; fi; read line; echo got $line"}
    }));
    let qk = format!(
        "{} --workspace {} run app:read",
        env!("CARGO_BIN_EXE_qk"),
        temp.path().display()
    );
    // script(1) runs qk on a terminal of its own.
    let mut script = Command::new("script");
    if cfg!(target_os = "macos") {
        script.args(["-q", "/dev/null", "/bin/sh", "-c", &qk]);
    } else {
        script.args(["-qec", &qk, "/dev/null"]);
    }
    let mut child = script
        .current_dir(temp.path())
        .env_remove("NX_PARALLEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        stdin.write_all(b"typed\n").unwrap();
        std::thread::sleep(Duration::from_millis(500));
    }
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the task never read its input"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut output = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut output).unwrap();
    assert!(output.contains("terminal"), "{output:?}");
    assert!(output.contains("got typed"), "{output:?}");
}

/// Runs `line` in a shell on a terminal of its own, through script(1), with
/// `qk` naming the binary under test; types `input` once it has started, and
/// returns what the terminal showed, with its line endings as `\n`.
#[cfg(unix)]
fn on_terminal(root: &Path, line: &str, env: &[(&str, &str)], input: &[u8]) -> String {
    let line = format!("qk=\"{}\"; {line}", env!("CARGO_BIN_EXE_qk"));
    let mut script = Command::new("script");
    if cfg!(target_os = "macos") {
        script.args(["-q", "/dev/null", "/bin/sh", "-c", &line]);
    } else {
        script.args(["-qec", &line, "/dev/null"]);
    }
    script
        .current_dir(root)
        .env_remove("NX_PARALLEL")
        .env_remove("NX_CACHE_DIRECTORY")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .envs(env.iter().copied());
    let mut child = script
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        if !input.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            stdin.write_all(input).unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            // Its terminal's session ends with it, so a stuck run fails
            // rather than holding the test's output open.
            let _ = child.kill();
            panic!("qk never finished on the terminal");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut output = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut output).unwrap();
    output.replace("\r\n", "\n")
}

#[cfg(unix)]
#[test]
fn captured_tasks_run_under_a_pseudo_terminal_on_a_terminal() {
    let probe = "if [ -t 1 ] && [ -t 2 ]; then echo out=terminal; else echo out=pipe; fi; \
                 if [ -t 0 ]; then echo in=terminal; else echo in=other; fi";
    let temp = fixture(json!({
        "check": {"command": probe},
        "script": {"executor": "nx:run-commands", "options": {"command": probe, "usePty": false}},
        "side": {"executor": "nx:run-commands", "options": {"commands": [probe, "true"]}},
        "serial": {"executor": "nx:run-commands", "options": {"commands": [probe, "true"], "parallel": false}},
        "prefixed": {"executor": "nx:run-commands", "options": {"commands": [{"command": probe, "prefix": "p"}]}}
    }));
    let static_run = |target: &str, env: &[(&str, &str)]| {
        on_terminal(
            temp.path(),
            &format!(
                "\"$qk\" --workspace '{}' run-many -t {target} --output-style static",
                temp.path().display()
            ),
            env,
            b"",
        )
    };
    // Only the task a run is for reads the terminal's input.
    for target in ["check", "serial"] {
        let output = static_run(target, &[]);
        assert!(output.contains("out=terminal"), "{target}: {output:?}");
        assert!(output.contains("in=other"), "{target}: {output:?}");
    }
    for target in ["script", "side", "prefixed"] {
        let output = static_run(target, &[]);
        assert!(output.contains("out=pipe"), "{target}: {output:?}");
    }
    let output = static_run("check", &[("NX_NATIVE_COMMAND_RUNNER", "false")]);
    assert!(output.contains("out=pipe"), "{output:?}");
    // Without a terminal, pipes.
    let piped = run(
        temp.path(),
        &["run-many", "-t", "check", "--output-style", "static"],
    );
    assert!(
        String::from_utf8(success(piped).stdout)
            .unwrap()
            .contains("out=pipe")
    );
}

#[cfg(unix)]
#[test]
fn the_task_a_run_is_for_reads_a_pseudo_terminal_on_a_cache_miss() {
    let temp = fixture(json!({
        "read": {
            "command": "if [ -t 0 ] && [ -t 1 ]; then echo terminal; fi; read line; echo got $line",
            "cache": true,
            "inputs": []
        }
    }));
    let line = format!(
        "\"$qk\" --workspace '{}' run app:read; stty -a",
        temp.path().display()
    );
    let output = on_terminal(temp.path(), &line, &[], b"typed\n");
    assert!(output.contains("terminal\n"), "{output:?}");
    assert!(output.contains("got typed\n"), "{output:?}");
    // The terminal is back in canonical mode once the task ends.
    assert!(
        output.contains("icanon") && !output.contains("-icanon"),
        "{output:?}"
    );
    // A hit replays the log without reading any input.
    let hit = on_terminal(temp.path(), &line, &[], b"");
    assert!(hit.contains("got typed\n"), "{hit:?}");
}

#[cfg(unix)]
#[test]
fn piped_input_reaches_its_end_under_a_pseudo_terminal() {
    let temp = fixture(json!({
        "cat": {
            "command": "if [ -t 1 ]; then echo terminal; fi; cat",
            "cache": true,
            "inputs": []
        }
    }));
    // Output on the terminal, input from a pipe: `cat` sees where it ends.
    let line = format!(
        "printf 'piped\\n' | \"$qk\" --workspace '{}' run app:cat",
        temp.path().display()
    );
    let output = on_terminal(temp.path(), &line, &[], b"");
    assert!(output.contains("terminal\npiped\n"), "{output:?}");
}

#[cfg(unix)]
#[test]
fn nx_run_flags_work_as_in_nx() {
    let temp = fixture(json!({
        "a": {"command": "echo a"},
        "b": {"command": "echo b"},
        "fail": {"command": "exit 2"},
        "zlast": {"command": "echo zlast"},
        "build": {"command": "echo build", "configurations": {"production": {"command": "echo production build"}}},
        "check": {"command": "echo check", "dependsOn": ["build"]},
        "verbose": {"command": "echo verbose=$NX_VERBOSE_LOGGING skip=$NX_SKIP_NX_CACHE"},
        "ping": {"command": "echo ping", "dependsOn": ["pong"]},
        "pong": {"command": "echo pong", "dependsOn": ["ping"]}
    }));
    let output = |args: &[&str]| run(temp.path(), args);
    let stdout = |args: &[&str]| String::from_utf8(success(output(args)).stdout).unwrap();
    let parallel = |args: &[&str], env: Option<&str>| {
        let mut command = command(
            temp.path(),
            &[&["run-many", "-t", "a,b", "--output-style", "quiet"], args].concat(),
        );
        if let Some(env) = env {
            command.env("NX_PARALLEL", env);
        }
        let stderr = String::from_utf8(success(command.output().unwrap()).stderr).unwrap();
        stderr
            .lines()
            .find_map(|line| {
                line.strip_prefix("Parallel ")?
                    .split(':')
                    .next()
                    .map(str::to_owned)
            })
            .unwrap()
    };
    assert_eq!(parallel(&["--parallel=false"], None), "1");
    assert_eq!(parallel(&["--parallel"], Some("2")), "2");
    assert_eq!(parallel(&["--parallel"], None), "3");
    assert_eq!(parallel(&["--parallel=4"], None), "4");
    assert_eq!(
        parallel(&["--parallel=100%"], None),
        std::thread::available_parallelism().unwrap().to_string()
    );
    // --prod is -c production.
    assert_eq!(
        stdout(&["run", "app:build", "--prod"]),
        "production build\n"
    );
    // --exclude-task-dependencies runs only what was asked for.
    assert_eq!(
        stdout(&["run", "app:check", "--exclude-task-dependencies"]),
        "check\n"
    );
    // --nx-bail starts nothing after the first failure.
    let bailed = output(&[
        "run-many",
        "-t",
        "fail,zlast",
        "--parallel=1",
        "--nx-bail",
        "--output-style",
        "static",
    ]);
    // It fails with 1, where Nx exits with 130 as if interrupted.
    assert_eq!(bailed.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&bailed.stdout).contains("zlast\n"));
    // NX_BAIL does the same, unless --nx-bail=false says otherwise.
    let mut command = command(
        temp.path(),
        &[
            "run-many",
            "-t",
            "fail,zlast",
            "--parallel=1",
            "--output-style",
            "static",
        ],
    );
    let bailed = command.env("NX_BAIL", "true").output().unwrap();
    assert!(!String::from_utf8_lossy(&bailed.stdout).contains("zlast\n"));
    let continued = command.arg("--nxBail=false").output().unwrap();
    assert!(String::from_utf8_lossy(&continued.stdout).contains("zlast\n"));
    let continued = output(&[
        "run-many",
        "-t",
        "fail,zlast",
        "--parallel=1",
        "--output-style",
        "static",
    ]);
    assert!(String::from_utf8_lossy(&continued.stdout).contains("zlast\n"));
    // Cycles fail unless ignored, as in Nx.
    assert!(
        String::from_utf8_lossy(&output(&["run", "app:ping"]).stderr)
            .contains("task dependency cycle")
    );
    let ignored = success(output(&[
        "run",
        "app:ping",
        "--nx-ignore-cycles",
        "--output-style",
        "static",
    ]));
    assert!(String::from_utf8_lossy(&ignored.stdout).contains("pong\n"));
    assert!(String::from_utf8_lossy(&ignored.stderr).contains("the task graph has a cycle"));
    // --verbose and a skipped cache reach tasks as Nx sets them.
    assert_eq!(
        stdout(&["run", "app:verbose", "--verbose", "--skip-nx-cache"]),
        "verbose=true skip=true\n"
    );
    // Nx's own options are accepted.
    assert_eq!(
        stdout(&[
            "run",
            "app:a",
            "--batch",
            "--skip-sync",
            "--no-cloud",
            "--tui=false",
            "--runner",
            "default",
            "--skip-remote-cache"
        ]),
        "a\n"
    );
}

#[test]
fn empty_selections_write_machine_readable_task_graphs() {
    let temp = fixture(json!({"build": {"command": "echo executed > marker"}}));
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=qk",
            "-c",
            "user.email=qk@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        success(
            Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .output()
                .unwrap(),
        );
    }
    for mut args in [
        vec!["run-many", "-t", "missing"],
        vec!["run-many", "-t", "build", "--exclude=app"],
        vec!["affected", "-t", "build", "--base=HEAD", "--head=HEAD"],
        vec![
            "affected",
            "-t",
            "build",
            "--base=HEAD",
            "--head=HEAD",
            "--granularity=project",
        ],
        vec!["affected", "-t", "missing", "--base=HEAD", "--head=HEAD"],
        vec![
            "affected",
            "-t",
            "missing",
            "--base=HEAD",
            "--head=HEAD",
            "--granularity=task",
        ],
    ] {
        args.push("--graph=stdout");
        let output = success(run(temp.path(), &args));
        let graph: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(graph["graph"]["nodes"]["app"].is_object());
        assert_eq!(graph["tasks"]["roots"], json!([]));
        for name in ["tasks", "dependencies", "continuousDependencies"] {
            assert_eq!(graph["tasks"][name], json!({}));
        }
        let file = temp.path().join("empty.json");
        fs::write(&file, "previous graph").unwrap();
        args.pop();
        let flag = format!("--graph={}", file.display());
        args.push(&flag);
        success(run(temp.path(), &args));
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(file).unwrap()).unwrap(),
            graph
        );
        assert!(!temp.path().join("marker").exists());
    }
    let disabled = success(run(
        temp.path(),
        &[
            "affected",
            "-t",
            "build",
            "--base=HEAD",
            "--head=HEAD",
            "--graph=false",
        ],
    ));
    assert!(disabled.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn the_task_graph_is_written_as_nx_writes_it() {
    let temp = fixture(json!({
        "serve": {"command": "sleep 1", "continuous": true},
        "build": {"command": "echo build", "cache": true, "outputs": ["{projectRoot}/dist"]},
        "e2e": {"command": "echo e2e", "dependsOn": ["build", {"target": "serve", "waitFor": "ready", "inputs": false}], "parallelism": false}
    }));
    let graph: Value = serde_json::from_slice(
        &success(run(
            temp.path(),
            &["run", "app:e2e", "--graph=stdout", "--", "--grep=x"],
        ))
        .stdout,
    )
    .unwrap();
    assert!(graph["graph"]["nodes"]["app"].is_object());
    let tasks = &graph["tasks"];
    assert_eq!(tasks["roots"], json!(["app:build", "app:serve"]));
    // Edges as Nx 24 writes them, with how a continuous one is depended on.
    assert_eq!(
        tasks["dependencies"]["app:e2e"],
        json!([{"id": "app:build"}])
    );
    assert_eq!(
        tasks["continuousDependencies"]["app:e2e"],
        json!([{"id": "app:serve", "waitFor": "ready", "inputs": false}])
    );
    let e2e = &tasks["tasks"]["app:e2e"];
    assert_eq!(e2e["target"], json!({"project": "app", "target": "e2e"}));
    assert_eq!(
        e2e["overrides"]["__overrides_unparsed__"],
        json!(["--grep=x"])
    );
    assert_eq!(
        (e2e["parallelism"].clone(), e2e["cache"].clone()),
        (json!(false), json!(false))
    );
    assert_eq!(tasks["tasks"]["app:build"]["outputs"], json!(["dist"]));
    let file = temp.path().join("graph.json");
    success(run(
        temp.path(),
        &["run", "app:e2e", &format!("--graph={}", file.display())],
    ));
    assert!(file.is_file());
}

#[cfg(unix)]
#[test]
fn a_target_without_a_project_finds_one_like_nx() {
    let temp = fixture(json!({"hello": {"command": "echo root"}}));
    fs::create_dir_all(temp.path().join("libs/lib/src")).unwrap();
    fs::write(
        temp.path().join("libs/lib/project.json"),
        json!({"name": "lib", "targets": {"hello": {"command": "echo lib"}}}).to_string(),
    )
    .unwrap();
    let hello = |directory: &Path, env: Option<&str>| {
        let mut command = command(temp.path(), &["run", "hello"]);
        command.current_dir(directory);
        match env {
            Some(name) => command.env("NX_DEFAULT_PROJECT", name),
            None => command.env_remove("NX_DEFAULT_PROJECT"),
        };
        String::from_utf8(success(command.output().unwrap()).stdout).unwrap()
    };
    let lib = temp.path().join("libs/lib/src");
    assert_eq!(hello(temp.path(), None), "root\n");
    // In the root project, NX_DEFAULT_PROJECT picks another.
    assert_eq!(hello(temp.path(), Some("lib")), "lib\n");
    // Inside another project, that project, whatever the variable says.
    assert_eq!(hello(&lib, Some("app")), "lib\n");
    // Without a root project, nx.json's defaultProject.
    fs::remove_file(temp.path().join("project.json")).unwrap();
    fs::write(temp.path().join("nx.json"), r#"{"defaultProject": "lib"}"#).unwrap();
    assert_eq!(hello(temp.path(), None), "lib\n");
}

/// Keep system-log tests serial to reduce pressure on the lossy log stream.
#[cfg(target_os = "macos")]
static AUDIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
#[test]
fn the_sandbox_reports_and_refuses_what_a_task_does_not_declare() {
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = fixture(json!({
        "build": {
            "command": "cat src/in.txt other/notes.txt .env > /dev/null; mkdir -p dist && echo out > dist/out.txt && echo stray > stray.txt && cat stray.txt > /dev/null",
            "inputs": ["{projectRoot}/src/**/*"],
            "outputs": ["{projectRoot}/dist"]
        },
        "clean": {"command": "cat src/in.txt > /dev/null", "inputs": ["{projectRoot}/src/**/*"]}
    }));
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::create_dir_all(temp.path().join("other")).unwrap();
    fs::write(temp.path().join("src/in.txt"), "in").unwrap();
    fs::write(temp.path().join("other/notes.txt"), "notes").unwrap();
    fs::write(temp.path().join(".env"), "X=1\n").unwrap();
    let report = temp.path().join("sandbox.json");
    let audit = run(
        temp.path(),
        &[
            "run-many",
            "-t",
            "build,clean",
            "--sandbox",
            "--sandbox-report",
            report.to_str().unwrap(),
        ],
    );
    success(audit);
    let findings: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let build = &findings["tasks"]["app:build"];
    assert_eq!(build["undeclaredReads"], json!(["other/notes.txt"]));
    assert_eq!(build["unkeyedReads"], json!([".env"]));
    // Read back after writing it, it is listed once, as a write.
    assert_eq!(build["strayWrites"], json!(["stray.txt"]));
    assert!(findings["tasks"].get("app:clean").is_none(), "{findings}");
    // Audit lets the task do it all.
    assert!(temp.path().join("stray.txt").is_file());
    fs::remove_file(temp.path().join("stray.txt")).unwrap();
    let enforced = run(
        temp.path(),
        &[
            "run",
            "app:build",
            "--sandbox=enforce",
            "--output-style",
            "static",
        ],
    );
    assert!(!enforced.status.success());
    // The task `run` is for streams, so its errors stay on stderr.
    let stderr = String::from_utf8_lossy(&enforced.stderr);
    assert!(
        stderr.contains("other/notes.txt: Operation not permitted"),
        "{stderr}"
    );
    assert!(!temp.path().join("stray.txt").exists());
    assert!(temp.path().join("dist/out.txt").is_file());
}

/// Only a task that started is listed as running unsandboxed: one skipped
/// after its dependency failed never ran.
#[cfg(target_os = "macos")]
#[test]
fn the_sandbox_lists_only_tasks_that_started() {
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A fileset with a field qk does not know cannot be resolved.
    let unresolved = json!([{"fileset": "{projectRoot}/**/*", "unknownField": true}]);
    let temp = fixture(json!({
        "fail": {"command": "exit 1"},
        "after": {"command": "true", "dependsOn": ["fail"], "inputs": unresolved},
        "alone": {"command": "true", "inputs": unresolved}
    }));
    let report = temp.path().join("sandbox.json");
    let output = run(
        temp.path(),
        &[
            "run-many",
            "-t",
            "after,alone",
            "--sandbox",
            "--sandbox-report",
            report.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let findings: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let unsandboxed: Vec<&String> = findings["unsandboxed"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(unsandboxed, ["app:alone"], "{findings}");
}

/// A sandboxed run skips the cache, but a task's log is still kept for
/// `qk show log`, failed or not.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_task_keeps_its_log() {
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = fixture(json!({
        "test": {"command": "echo checking && echo broken >&2 && exit 1", "cache": true}
    }));
    let failed = run(temp.path(), &["run", "app:test", "--sandbox"]);
    assert!(!failed.status.success());
    let latest: Value =
        serde_json::from_slice(&success(run(temp.path(), &["show", "run", "--json"])).stdout)
            .unwrap();
    let log = success(run(
        temp.path(),
        &["show", "log", latest["id"].as_str().unwrap(), "app:test"],
    ));
    assert!(String::from_utf8_lossy(&log.stdout).contains("checking"));
    assert!(String::from_utf8_lossy(&log.stderr).contains("broken"));
}

/// A workspace with an `app` project in `apps/app` whose targets read a
/// package only the root installs, laid out as pnpm installs it: `fallow` and
/// `other` from the root, `helper` from `apps/app`, and `stale` left from an
/// earlier install.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn root_tool_fixture(targets: Value) -> TempDir {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::create_dir_all(root.join("apps/app")).unwrap();
    fs::write(
        root.join("apps/app/project.json"),
        json!({"name": "app", "targets": targets}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'
importers:
  .:
    devDependencies:
      fallow:
        specifier: ^1.0.0
        version: 1.0.0
      other:
        specifier: ^1.0.0
        version: 1.0.0
  apps/app:
    dependencies:
      helper:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  fallow@1.0.0:
    resolution: {integrity: sha512-fallow}
  helper@1.0.0:
    resolution: {integrity: sha512-helper}
  other@1.0.0:
    resolution: {integrity: sha512-other}
snapshots:
  fallow@1.0.0: {}
  helper@1.0.0: {}
  other@1.0.0: {}
",
    )
    .unwrap();
    for (directory, name) in [
        ("fallow@1.0.0", "fallow"),
        ("helper@1.0.0", "helper"),
        ("other@1.0.0", "other"),
        ("stale@0.1.0", "stale"),
    ] {
        let package = root.join(format!(
            "node_modules/.pnpm/{directory}/node_modules/{name}"
        ));
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("index.js"), "").unwrap();
        fs::write(package.join("package.json"), "{}").unwrap();
    }
    std::os::unix::fs::symlink(
        ".pnpm/fallow@1.0.0/node_modules/fallow",
        root.join("node_modules/fallow"),
    )
    .unwrap();
    temp
}

#[cfg(target_os = "macos")]
#[test]
fn the_sandbox_reports_a_package_read_that_the_key_leaves_out() {
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // `helper`, which `apps/app` installs, is in either key; `stale` is in
    // no lockfile snapshot, so it is no package.
    let read = "cat ../../node_modules/fallow/index.js ../../node_modules/fallow/package.json \
        ../../node_modules/.pnpm/helper@1.0.0/node_modules/helper/package.json \
        ../../node_modules/.pnpm/stale@0.1.0/node_modules/stale/package.json";
    let targets = |external: Value| {
        json!({
            // Naming its external dependencies, even none, leaves the root
            // importer out of the key.
            "lint": {"command": read, "options": {"cwd": "apps/app"}, "inputs": ["default", {"externalDependencies": external}]},
            // Without, every installed package counts.
            "build": {"command": read, "options": {"cwd": "apps/app"}, "inputs": ["default"]}
        })
    };
    let audit = |temp: &TempDir| {
        let report = temp.path().join("sandbox.json");
        let output = success(run(
            temp.path(),
            &[
                "run-many",
                "-t",
                "lint,build",
                "--sandbox",
                "--sandbox-report",
                report.to_str().unwrap(),
            ],
        ));
        let findings: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
        (
            findings,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let unnamed = root_tool_fixture(targets(json!([])));
    let (findings, stderr) = audit(&unnamed);
    assert_eq!(
        findings["tasks"]["app:lint"]["unkeyedPackages"],
        json!(["fallow"]),
        "{findings}"
    );
    // Reads of a package the key covers, and of no package, are no finding.
    assert_eq!(
        findings["tasks"]["app:lint"]["undeclaredReads"],
        json!([]),
        "{findings}"
    );
    assert!(findings["tasks"].get("app:build").is_none(), "{findings}");
    assert!(
        stderr.contains(
            "1 installed package read, not in its key; name it in its externalDependencies:\n      fallow\n"
        ),
        "{stderr}"
    );
    // Enforce lets a task read any installed package.
    success(run(
        unnamed.path(),
        &["run", "app:lint", "--sandbox=enforce"],
    ));
    let named = root_tool_fixture(targets(json!(["fallow"])));
    let (findings, _) = audit(&named);
    assert_eq!(findings["tasks"], json!({}), "{findings}");
}

#[cfg(target_os = "macos")]
#[test]
fn warm_suggest_lists_the_directories_a_task_wrote_outside_its_outputs() {
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = fixture(json!({
        "build": {
            "command": "mkdir -p dist .tool/a && echo out > dist/out.txt && echo cached > .tool/a/entry && echo generated > src/generated.txt",
            "outputs": ["{projectRoot}/dist"]
        }
    }));
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/in.txt"), "in").unwrap();
    // A tool's directories are ignored, which is what sets them apart.
    fs::write(temp.path().join(".gitignore"), "dist/\n.tool/\n").unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "--quiet"]);
    let output = run(temp.path(), &["warm", "suggest", "app:build"]);
    let stdout = String::from_utf8_lossy(&success(output).stdout).into_owned();
    assert!(stdout.contains("\n  .tool  "), "{stdout}");
    // A write beside sources is not the task's directory to keep.
    assert!(!stdout.contains("src"), "{stdout}");
}

#[cfg(target_os = "linux")]
#[test]
fn the_sandbox_refuses_what_a_task_does_not_declare_on_linux() {
    let temp = fixture(json!({
        "build": {
            "command": "cat src/in.txt .env node_modules/dep/index.js /etc/hostname > /dev/null && ls other > /dev/null && echo out > dist/out.txt && echo ok",
            "inputs": ["{projectRoot}/src/**/*"],
            "outputs": ["{projectRoot}/dist"]
        },
        "peek": {"command": "cat other/notes.txt", "inputs": ["{projectRoot}/src/**/*"]},
        "stray": {"command": "echo stray > stray.txt", "inputs": ["{projectRoot}/src/**/*"]}
    }));
    for directory in ["src", "other", "node_modules/dep"] {
        fs::create_dir_all(temp.path().join(directory)).unwrap();
    }
    fs::write(temp.path().join("src/in.txt"), "in").unwrap();
    fs::write(temp.path().join("other/notes.txt"), "notes").unwrap();
    fs::write(temp.path().join(".env"), "X=1\n").unwrap();
    fs::write(temp.path().join("node_modules/dep/index.js"), "").unwrap();
    let sandboxed = |target: &str| {
        run(
            temp.path(),
            &[
                "run",
                target,
                "--sandbox=enforce",
                "--output-style",
                "static",
            ],
        )
    };
    // Its inputs, dotenv files, node_modules, outside the workspace, listing,
    // and writing its outputs, which did not exist yet.
    let built = success(sandboxed("app:build"));
    assert!(String::from_utf8_lossy(&built.stdout).contains("ok"));
    assert!(temp.path().join("dist/out.txt").is_file());
    let peek = sandboxed("app:peek");
    assert!(!peek.status.success());
    // The task `run` is for streams, so its errors stay on stderr.
    assert!(String::from_utf8_lossy(&peek.stderr).contains("Permission denied"));
    assert!(!sandboxed("app:stray").status.success());
    assert!(!temp.path().join("stray.txt").exists());
    // Audit needs reports Landlock does not give.
    let audit = run(temp.path(), &["run", "app:build", "--sandbox"]);
    assert!(String::from_utf8_lossy(&audit.stderr).contains("use --sandbox=enforce"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sandbox_allows_linked_input_trees_without_granting_unrelated_access() {
    let temp = fixture(json!({
        "build": {
            "command": "mkdir -p dist && cat alias/value.txt alias/ignored/value.txt alias/file.txt alias/nested/value.txt selected.txt > dist/out.txt",
            "inputs": ["{projectRoot}/alias", "{projectRoot}/selected.txt"],
            "outputs": ["{projectRoot}/dist"]
        },
        "peek": {
            "command": "cat other/value.txt",
            "inputs": ["{projectRoot}/alias"]
        },
        "write": {
            "command": "echo changed > alias/value.txt",
            "inputs": ["{projectRoot}/alias"]
        }
    }));
    for directory in ["shared/ignored", "nested", "other"] {
        fs::create_dir_all(temp.path().join(directory)).unwrap();
    }
    for (path, content) in [
        ("shared/value.txt", "direct\n"),
        ("shared/ignored/value.txt", "ignored\n"),
        ("file.txt", "file\n"),
        ("nested/value.txt", "nested\n"),
        ("other/value.txt", "unselected\n"),
    ] {
        fs::write(temp.path().join(path), content).unwrap();
    }
    fs::write(temp.path().join(".gitignore"), "shared/ignored/\ndist/\n").unwrap();
    for (target, alias) in [
        ("shared", "alias"),
        ("../file.txt", "shared/file.txt"),
        ("../nested", "shared/nested"),
        ("file.txt", "selected.txt"),
    ] {
        std::os::unix::fs::symlink(target, temp.path().join(alias)).unwrap();
    }
    let enforced = |target: &str| run(temp.path(), &["run", target, "--sandbox=enforce"]);
    success(enforced("app:build"));
    assert_eq!(
        fs::read_to_string(temp.path().join("dist/out.txt")).unwrap(),
        "direct\nignored\nfile\nnested\nfile\n"
    );
    assert!(!enforced("app:peek").status.success());
    assert!(!enforced("app:write").status.success());
    assert_eq!(
        fs::read_to_string(temp.path().join("shared/value.txt")).unwrap(),
        "direct\n"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sandbox_allows_structured_json_inputs_and_their_linked_targets() {
    let temp = fixture(json!({
        "build": {
            "command": "mkdir -p dist && cat settings.json selected.json > dist/out.txt",
            "inputs": [
                {"json": "{projectRoot}/settings.json", "fields": ["selected"]},
                {"json": "{projectRoot}/selected.json", "excludeFields": ["comment"]}
            ],
            "outputs": ["{projectRoot}/dist"]
        },
        "peek": {
            "command": "cat other.json",
            "inputs": [{"json": "{projectRoot}/settings.json", "fields": ["selected"]}]
        }
    }));
    for path in ["settings.json", "linked.json", "other.json"] {
        fs::write(temp.path().join(path), "{\"selected\":1}").unwrap();
    }
    std::os::unix::fs::symlink("linked.json", temp.path().join("selected.json")).unwrap();
    let enforced = |target: &str| run(temp.path(), &["run", target, "--sandbox=enforce"]);
    success(enforced("app:build"));
    assert_eq!(
        fs::read_to_string(temp.path().join("dist/out.txt")).unwrap(),
        "{\"selected\":1}{\"selected\":1}"
    );
    assert!(!enforced("app:peek").status.success());
}

#[cfg(unix)]
#[test]
/// Nested exec uses existing confinement and rejects unconfined sandbox requests.
fn exec_inherits_the_enclosing_task_sandbox() {
    let executable = env!("CARGO_BIN_EXE_qk");
    let temp = fixture(json!({
        "build": {"command": format!("\"{executable}\" exec --sandbox=enforce -- echo built")}
    }));
    let output = success(run(temp.path(), &["run", "app:build", "--sandbox=enforce"]));
    assert!(String::from_utf8_lossy(&output.stdout).contains("built"));

    // An unsandboxed task must not inherit a caller-supplied marker.
    let output = command(temp.path(), &["run", "app:build"])
        .env("QK_TASK_SANDBOX", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot apply"));
}

#[cfg(unix)]
#[test]
/// Exec selects project order and switches behavior for tasks and package scripts.
fn exec_runs_in_projects_dependencies_first_as_nx_does() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nx.json"), "{}").unwrap();
    for (directory, project) in [
        ("libs/core", json!({"name": "core"})),
        (
            "apps/web",
            json!({"name": "web", "implicitDependencies": ["core"]}),
        ),
        ("tools/other", json!({"name": "other"})),
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
        fs::write(
            root.join(directory).join("project.json"),
            project.to_string(),
        )
        .unwrap();
    }
    let text = |output: Output| String::from_utf8(success(output).stdout).unwrap();
    // Each argument is quoted as Nx quotes it, so the shell expands variables.
    let names = [
        "exec",
        "-p",
        "web",
        "--",
        "echo",
        "$NX_PROJECT_NAME",
        "$NX_PROJECT_ROOT_PATH",
    ];
    assert_eq!(text(run(root, &names)), "core libs/core\nweb apps/web\n");
    assert!(text(run(root, &["exec", "-p", "core", "--", "pwd"])).ends_with("libs/core\n"));
    assert_eq!(
        text(run(
            root,
            &[
                "exec",
                "-p",
                "web",
                "--exclude-task-dependencies",
                "--",
                "echo",
                "$NX_PROJECT_NAME"
            ]
        )),
        "web\n"
    );

    // The first failure stops the rest, and exits 1.
    let failed = run(
        root,
        &[
            "exec",
            "-p",
            "web",
            "--",
            "sh",
            "-c",
            "echo $NX_PROJECT_NAME; exit 3",
        ],
    );
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&failed.stdout), "core\n");

    // Inside a task, the command runs as it is, for the task's project.
    let inside = command(root, &["exec", "--", "echo", "$NX_PROJECT_NAME"])
        .env("NX_TASK_TARGET_PROJECT", "web")
        .output()
        .unwrap();
    assert_eq!(text(inside), "web\n");

    // From a package script, the script's target runs instead.
    fs::write(
        root.join("apps/web/package.json"),
        json!({"name": "web", "scripts": {"build": "qk exec -- echo script"}}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("apps/web/project.json"),
        json!({"name": "web", "targets": {"build": {"command": "echo target"}}}).to_string(),
    )
    .unwrap();
    let script = command(root, &["exec", "--", "echo", "script"])
        .current_dir(root.join("apps/web"))
        .env("npm_lifecycle_event", "build")
        .output()
        .unwrap();
    assert!(text(script).contains("target\n"));

    assert!(!run(root, &["exec", "-p", "web"]).status.success());

    // Run directly, the command cannot be sandboxed or dry-run, so asking
    // for either fails without running it.
    for option in ["--dry-run", "--sandbox=enforce"] {
        let refused = run(root, &["exec", option, "--", "touch", "ran"]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("cannot apply"));
    }
    assert!(!root.join("ran").exists());
    assert!(!root.join("tools/other/ran").exists());
}

/// Watch includes dependencies, ignores generated files and stops on cancellation.
#[cfg(unix)]
#[test]
fn watch_runs_callbacks_for_project_changes_and_cancels() {
    use std::io::{BufRead, BufReader};
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nx.json"), "{}").unwrap();
    for (directory, project) in [
        (
            "app",
            json!({"name":"app","implicitDependencies":["lib"],
        "targets":{"build":{"command":"echo build", "outputs":["{projectRoot}/dist"]}}}),
        ),
        ("lib", json!({"name":"lib"})),
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
        fs::write(
            root.join(directory).join("project.json"),
            project.to_string(),
        )
        .unwrap();
        fs::write(root.join(directory).join("input.txt"), "one").unwrap();
    }
    let callbacks = temp.path().join("callbacks");
    // Callback records live outside the workspace, so they cannot trigger watch.
    let script = format!(
        "echo \"$NX_PROJECT_NAME|$NX_FILE_CHANGES\" >> '{}'; echo callback",
        callbacks.display()
    );
    let mut child = WatchChild(
        command(
            root,
            &[
                "watch",
                "-p",
                "app",
                "--includeDependencies",
                "--initialRun",
                "--",
                &script,
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let (send, receive) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let _ = send.send(line.unwrap());
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
        .contains("qk: watching")
    {}
    assert_eq!(fs::read_to_string(&callbacks).unwrap(), "app|\n");
    fs::write(root.join("lib/input.txt"), "two").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // Native events may batch the dependency edit with other source paths.
    // NX_FILE_CHANGES contains the whole batch for each changed project.
    while !fs::read_to_string(&callbacks).unwrap().lines().any(|line| {
        line.strip_prefix("lib|")
            .is_some_and(|files| files.split_whitespace().any(|file| file == "lib/input.txt"))
    }) {
        assert!(
            Instant::now() < deadline,
            "watch did not observe dependency edit; callbacks: {:?}",
            fs::read_to_string(&callbacks).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let count = fs::read_to_string(&callbacks).unwrap().lines().count();
    fs::create_dir_all(root.join("app/dist")).unwrap();
    fs::write(root.join("app/dist/out.txt"), "generated").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fs::read_to_string(&callbacks).unwrap().lines().count(),
        count
    );
    // Retain source edits when an editor temporarily writes invalid config.
    fs::write(root.join("nx.json"), "{").unwrap();
    fs::write(root.join("lib/input.txt"), "three").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
        .contains("watch configuration not reloaded")
    {}
    fs::write(root.join("nx.json"), "{}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(&callbacks).unwrap().lines().count() == count {
        assert!(
            Instant::now() < deadline,
            "configuration reload lost source edits"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        fs::read_to_string(&callbacks)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .contains("lib/input.txt")
    );
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(130));
    reader.join().unwrap();
}

/// Always reap a watcher, including when an assertion fails.
struct WatchChild(std::process::Child);
impl std::ops::Deref for WatchChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for WatchChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for WatchChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                .output();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// --all discovers new projects and queues edits made during a running callback.
#[cfg(unix)]
#[test]
fn watch_all_discovers_projects_and_queues_changes() {
    use std::io::{BufRead, BufReader};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("nx.json"), "{}").unwrap();
    let records = temp.path().join("records");
    let started = temp.path().join("started");
    let script = format!(
        "echo start > '{}'; sleep 0.6; echo \"$NX_PROJECT_NAME|$NX_FILE_CHANGES\" >> '{}'",
        started.display(),
        records.display()
    );
    let mut child = WatchChild(
        command(&root, &["watch", "--all", "--", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (send, receive) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let _ = send.send(line.unwrap());
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
        .contains("qk: watching")
    {}
    fs::create_dir_all(root.join("new")).unwrap();
    fs::write(root.join("new/project.json"), r#"{"name":"new"}"#).unwrap();
    fs::write(root.join("new/first.txt"), "first").unwrap();
    wait_for(&started);
    // This event arrives while the first callback is sleeping.
    fs::write(root.join("new/second.txt"), "second").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&records)
        .unwrap_or_default()
        .contains("new/second.txt")
    {
        assert!(Instant::now() < deadline, "pending callback was lost");
        std::thread::sleep(Duration::from_millis(20));
    }
    let lines = fs::read_to_string(&records).unwrap();
    assert!(lines.lines().count() >= 2, "{lines}");
    assert!(
        lines.lines().all(|line| line.starts_with("new|")),
        "{lines}"
    );
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(130));
    reader.join().unwrap();
}

/// Exercise queued callbacks and cancellation while a callback runs on every OS.
#[test]
fn watch_portable_queues_changes_ignores_files_and_cancels_callback() {
    use std::io::{BufRead, BufReader};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::write(root.join(".nxignore"), "app/ignored.txt\n").unwrap();
    fs::write(root.join("app/project.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(root.join("app/first.txt"), "one").unwrap();
    fs::write(root.join("app/second.txt"), "one").unwrap();
    // Keep creation events out of the queued-callback sequence.
    fs::write(root.join("app/ignored.txt"), "before watch").unwrap();
    let records = temp.path().join("records");
    fs::create_dir_all(&records).unwrap();
    // The libtest skip argument mentions the project variable for watch selection.
    let callback = format!("{} --skip NX_PROJECT_NAME", helper());
    let mut cmd = command(&root, &["watch", "--all", "--", &callback]);
    cmd.env("QK_TEST_MODE", "watch-portable")
        .env("QK_TEST_ID", "watch")
        .env("QK_WATCH_RECORDS", &records)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Isolate Ctrl+C from the test runner and other parallel test processes.
        cmd.creation_flags(0x00000010); // CREATE_NEW_CONSOLE
    }
    let mut child = WatchChild(cmd.spawn().unwrap());
    let (sender, receiver) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let _ = sender.send(line.unwrap());
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
        .contains("qk: watching")
    {}
    fs::write(root.join("app/ignored.txt"), "ignored").unwrap();
    // Native backends may report a parent directory and include eligible
    // siblings. Check excluded paths in the callback rather than its count.
    fs::write(root.join("app/first.txt"), "two").unwrap();
    wait_for(&records.join("1.json"));
    let first: Value = serde_json::from_slice(&fs::read(records.join("1.json")).unwrap()).unwrap();
    assert_eq!(first["project"], "app");
    let files: Vec<_> = first["files"].as_str().unwrap().split(' ').collect();
    assert!(files.contains(&"app/first.txt"), "{first}");
    assert!(!files.contains(&"app/ignored.txt"), "{first}");
    fs::write(root.join("app/second.txt"), "two").unwrap();
    fs::write(records.join("release-1"), "release").unwrap();
    wait_for(&records.join("2.json"));
    wait_for(&records.join("2.tick"));
    let queued: Value = serde_json::from_slice(&fs::read(records.join("2.json")).unwrap()).unwrap();
    assert_eq!(queued["project"], "app");
    let files: Vec<_> = queued["files"].as_str().unwrap().split(' ').collect();
    assert!(files.contains(&"app/second.txt"), "{queued}");
    assert!(!files.contains(&"app/ignored.txt"), "{queued}");
    #[cfg(unix)]
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    #[cfg(windows)]
    success(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process_helper", "--nocapture"])
            .env("QK_TEST_MODE", "signal-watch")
            .env("QK_WATCH_PID", child.id().to_string())
            .output()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "watch did not stop on Ctrl+C");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
    let ticks = fs::read(records.join("2.tick")).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        fs::read(records.join("2.tick")).unwrap(),
        ticks,
        "callback survived cancellation"
    );
    reader.join().unwrap();
}

/// Send Ctrl+C only to the watcher's private console from an isolated helper.
#[cfg(windows)]
fn signal_windows_watch(pid: u32) {
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(pid: u32) -> i32;
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    unsafe extern "system" fn ignore_signal(_: u32) -> i32 {
        1
    }
    unsafe {
        FreeConsole();
        assert_ne!(AttachConsole(pid), 0, "{}", std::io::Error::last_os_error());
        assert_ne!(SetConsoleCtrlHandler(Some(ignore_signal), 1), 0);
        assert_ne!(
            GenerateConsoleCtrlEvent(0, 0),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        FreeConsole();
    }
}

#[test]
fn input_analysis_rejects_nonexecuting_options_and_invalid_previous_reports() {
    let temp = fixture(json!({"build": {"command": "echo executed > marker"}}));
    for flag in ["--dry-run", "--graph", "--sandbox"] {
        let output = run(temp.path(), &["inputs", "analyze", "app:build", flag]);
        assert!(!output.status.success());
        assert!(!temp.path().join("marker").exists());
    }
    fs::write(
        temp.path().join("previous.json"),
        r#"{"schemaVersion": 999}"#,
    )
    .unwrap();
    let output = run(
        temp.path(),
        &[
            "inputs",
            "analyze",
            "app:build",
            "--previous",
            "previous.json",
        ],
    );
    assert!(!output.status.success());
    assert!(!temp.path().join("marker").exists());
    let temp = fixture(json!({
        "seed": {"command": "echo executed > marker"},
        "serve": {"command": "echo executed > marker", "continuous": true, "dependsOn": ["seed"]},
        "ready": {"executor": "nx:run-commands", "options": {"command": "echo executed > marker", "readyWhen": "ready"}}
    }));
    for task in ["app:serve", "app:ready"] {
        let output = run(temp.path(), &["inputs", "analyze", task]);
        assert!(!output.status.success());
        assert!(!temp.path().join("marker").exists());
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn input_analysis_recognizes_nested_links_and_structured_link_targets() {
    #[cfg(target_os = "macos")]
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(target_os = "linux")]
    if Command::new("strace").arg("-V").output().is_err() {
        return;
    }
    let temp = fixture(json!({
        "build": {
            "command": "cat selected/file.txt selected/nested/value.txt settings.json other/value.txt",
            "inputs": [
                "{projectRoot}/selected",
                {"json": "{projectRoot}/settings.json", "fields": ["value"]}
            ]
        }
    }));
    for directory in ["shared", "nested", "other"] {
        fs::create_dir(temp.path().join(directory)).unwrap();
    }
    for (path, content) in [
        ("file.txt", "linked file"),
        ("nested/value.txt", "linked directory"),
        ("data.json", "{\"value\":1}"),
        ("other/value.txt", "uncovered"),
    ] {
        fs::write(temp.path().join(path), content).unwrap();
    }
    for (target, alias) in [
        ("shared", "selected"),
        ("../file.txt", "shared/file.txt"),
        ("../nested", "shared/nested"),
        ("data.json", "settings.json"),
    ] {
        std::os::unix::fs::symlink(target, temp.path().join(alias)).unwrap();
    }
    let output = run(
        temp.path(),
        &[
            "inputs",
            "analyze",
            "app:build",
            "--report",
            "analysis.json",
        ],
    );
    success(output);
    let report: Value =
        serde_json::from_slice(&fs::read(temp.path().join("analysis.json")).unwrap()).unwrap();
    let task = &report["tasks"]["app:build"];
    for (path, requested, category, alias) in [
        ("file.txt", "selected/file.txt", "input", "selected"),
        (
            "nested/value.txt",
            "selected/nested/value.txt",
            "input",
            "selected",
        ),
        (
            "data.json",
            "settings.json",
            "structuredInput",
            "settings.json",
        ),
    ] {
        assert!(
            !task["uncoveredAccesses"]
                .as_array()
                .unwrap()
                .contains(&json!(path)),
            "{report}"
        );
        let reads: Vec<_> = task["accesses"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|access| {
                (access["path"] == path || access["path"] == requested)
                    && (access["operation"] == "readData" || access["operation"] == "openRead")
            })
            .collect();
        #[cfg(target_os = "linux")]
        assert!(!reads.is_empty(), "{report}");
        for read in reads {
            assert_eq!(read["category"], category, "{report}");
            assert_eq!(read["keyedPath"], alias, "{report}");
        }
    }
    assert!(
        task["uncoveredAccesses"]
            .as_array()
            .unwrap()
            .contains(&json!("other/value.txt")),
        "{report}"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn input_analysis_reports_a_package_read_that_the_key_leaves_out() {
    #[cfg(target_os = "macos")]
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(target_os = "linux")]
    if Command::new("strace").arg("-V").output().is_err() {
        return;
    }
    let analyze = |external: Value| {
        let temp = root_tool_fixture(json!({
            "lint": {
                "command": "cat ../../node_modules/fallow/index.js ../../node_modules/fallow/package.json",
                "options": {"cwd": "apps/app"},
                "inputs": ["default", {"externalDependencies": external}]
            }
        }));
        let output = success(run(
            temp.path(),
            &["inputs", "analyze", "app:lint", "--report", "analysis.json"],
        ));
        let report: Value =
            serde_json::from_slice(&fs::read(temp.path().join("analysis.json")).unwrap()).unwrap();
        (
            report["tasks"]["app:lint"]["unkeyedPackages"].clone(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (packages, stderr) = analyze(json!([]));
    assert_eq!(packages, json!({"fallow": 2}));
    assert!(
        stderr.contains("1 installed package read but not in the key; name it in the target's externalDependencies\n    fallow (2 files)"),
        "{stderr}"
    );
    let (packages, _) = analyze(json!(["fallow"]));
    assert_eq!(packages, json!({}));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn input_analysis_cancellation_stops_descendants_and_saves_an_advisory_report() {
    #[cfg(target_os = "macos")]
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(target_os = "linux")]
    if Command::new("strace").arg("-V").output().is_err() {
        return;
    }
    let temp = fixture(json!({"long": helper_target("tree", "tree")}));
    let mut child = WatchChild(
        command(
            temp.path(),
            &[
                "inputs",
                "analyze",
                "app:long",
                "--report",
                "cancelled.json",
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    // On macOS the recorder first waits up to 10 seconds for the system log
    // and up to 10 more for it to carry a probe report.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !temp.path().join("heartbeat.started").exists() {
        if child.try_wait().unwrap().is_some() {
            use std::io::Read;
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("recorder did not start: {stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "recorder did not launch its task"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        Command::new("/bin/kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "recorder did not stop");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(130));
    assert_stopped(&temp.path().join("heartbeat"));
    let report: Value =
        serde_json::from_slice(&fs::read(temp.path().join("cancelled.json")).unwrap()).unwrap();
    assert_eq!(report["tasks"]["app:long"]["outcome"], "cancelled");
    assert!(
        report["tasks"]["app:long"]["successfulRuns"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn input_analysis_records_descendants_outputs_and_uncovered_inputs_without_cache_reuse() {
    #[cfg(target_os = "macos")]
    let _audit = AUDIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(target_os = "linux")]
    if Command::new("strace").arg("-V").output().is_err() {
        eprintln!("strace is not installed; recorder integration not checked");
        return;
    }
    let temp = fixture(json!({
        "seed": {"command": "mkdir -p dependency-dist; echo seed > dependency-dist/value", "outputs": ["{projectRoot}/dependency-dist"], "cache": true},
        "build": {
            "command": "cat src/a.txt src/alias.txt other/hidden.txt settings.json dependency-dist/value; test -e missing.json || :; ls templates; printf generated > generated.tmp; cat generated.tmp; mkdir -p dist; echo out > dist/out.txt; echo run >> dist/runs.txt",
            "inputs": ["{projectRoot}/src/**/*", {"json": "{projectRoot}/settings.json", "fields": ["value"]}],
            "outputs": ["{projectRoot}/dist"], "dependsOn": ["seed"], "cache": true,
            "options": {"env": {"QK_FIXTURE_SECRET": "do-not-store-this-secret"}}
        },
        "fail": {"command": "cat src/a.txt; exit 7", "inputs": ["{projectRoot}/src/**/*"]}
    }));
    for path in ["src", "other", "templates"] {
        fs::create_dir(temp.path().join(path)).unwrap();
    }
    fs::write(temp.path().join("src/a.txt"), "input").unwrap();
    fs::write(temp.path().join("src/unused.txt"), "unused").unwrap();
    fs::write(temp.path().join("other/hidden.txt"), "hidden").unwrap();
    fs::write(temp.path().join("settings.json"), r#"{"value": 1}"#).unwrap();
    fs::write(temp.path().join("linked.txt"), "linked").unwrap();
    std::os::unix::fs::symlink("../linked.txt", temp.path().join("src/alias.txt")).unwrap();
    let output = run(
        temp.path(),
        &[
            "inputs",
            "analyze",
            "app:build",
            "--report",
            "first.json",
            "--suggestions",
            "draft.json",
        ],
    );
    success(output);
    let draft: Value =
        serde_json::from_slice(&fs::read(temp.path().join("draft.json")).unwrap()).unwrap();
    assert_eq!(draft["reviewRequired"], true);
    let fragment = &draft["tasks"]["app:build"];
    if !fragment.is_null() {
        assert!(
            fragment["targets"]["build"]["inputs"]
                .as_array()
                .unwrap()
                .contains(&json!({"json": "{projectRoot}/settings.json", "fields": ["value"]}))
        );
    }
    let project: Value =
        serde_json::from_slice(&fs::read(temp.path().join("project.json")).unwrap()).unwrap();
    assert_eq!(
        project["targets"]["build"]["inputs"][0],
        "{projectRoot}/src/**/*"
    );
    let bytes = fs::read(temp.path().join("first.json")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("do-not-store-this-secret"));
    let report: Value = serde_json::from_slice(&bytes).unwrap();
    let task = &report["tasks"]["app:build"];
    assert_eq!(task["outcome"], "success");
    assert_eq!(task["coverage"]["partial"], true);
    assert!(task["configurationFragment"].is_null()); // Existing specific inputs stay intact.
    assert!(
        draft["reviews"]["app:build"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("retained"))
    );
    assert!(
        task["accessReview"]["contentReads"]
            .as_array()
            .unwrap()
            .contains(&json!("other/hidden.txt"))
    );
    #[cfg(target_os = "linux")]
    assert!(
        task["observedInputs"]
            .as_array()
            .unwrap()
            .contains(&json!("src/a.txt")),
        "{report}"
    );
    // Seatbelt logs can omit individual accesses, so macOS asserts observed
    // coverage and classification rather than completeness of the trace.
    assert!(
        !task["observedInputs"].as_array().unwrap().is_empty(),
        "{report}"
    );
    if task["accesses"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["path"] == "linked.txt" || event["path"] == "src/alias.txt")
    {
        assert!(
            task["observedInputs"]
                .as_array()
                .unwrap()
                .contains(&json!("src/alias.txt")),
            "{report}"
        );
    }
    assert!(
        !task["uncoveredAccesses"]
            .as_array()
            .unwrap()
            .contains(&json!("linked.txt"))
    );
    assert!(
        task["unobservedInputs"]
            .as_array()
            .unwrap()
            .contains(&json!("src/unused.txt")),
        "{report}"
    );
    assert!(
        !task["unobservedInputs"]
            .as_array()
            .unwrap()
            .contains(&json!("nx.json"))
    );
    assert!(
        task["uncoveredAccesses"]
            .as_array()
            .unwrap()
            .contains(&json!("other/hidden.txt")),
        "{report}"
    );
    assert!(
        !task["uncoveredAccesses"]
            .as_array()
            .unwrap()
            .contains(&json!("settings.json"))
    );
    let events = task["accesses"].as_array().unwrap();
    for (path, category) in [
        ("dependency-dist/value", "dependencyOutput"),
        ("dist/out.txt", "ownOutput"),
        ("generated.tmp", "generated"),
    ] {
        assert!(
            events
                .iter()
                .any(|event| event["path"] == path && event["category"] == category),
            "{path}: {report}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        assert!(
            events
                .iter()
                .any(|event| event["path"] == "src/alias.txt"
                    && event["resolvedPath"] == "linked.txt"),
            "{report}"
        );
        assert!(
            events
                .iter()
                .any(|event| event["path"] == "missing.json" && event["result"] == "missing"),
            "{report}"
        );
    }
    success(run(
        temp.path(),
        &[
            "inputs",
            "analyze",
            "app:build",
            "--report",
            "second.json",
            "--previous",
            "first.json",
        ],
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("dist/runs.txt")).unwrap(),
        "run\nrun\n"
    );
    let second: Value =
        serde_json::from_slice(&fs::read(temp.path().join("second.json")).unwrap()).unwrap();
    assert_eq!(
        second["tasks"]["app:build"]["successfulRuns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let failed = run(
        temp.path(),
        &["inputs", "analyze", "app:fail", "--report", "failed.json"],
    );
    assert_eq!(failed.status.code(), Some(1));
    let failed: Value =
        serde_json::from_slice(&fs::read(temp.path().join("failed.json")).unwrap()).unwrap();
    assert_eq!(failed["tasks"]["app:fail"]["outcome"], "failed:7");
    assert!(
        failed["tasks"]["app:fail"]["candidateFilesets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Suggestions require an actual audited execution, not a planning command.
#[test]
fn warm_suggest_rejects_options_that_skip_execution() {
    let temp = fixture(json!({"build": {"command": "echo unused"}}));
    for options in [vec!["--dry-run"], vec!["--graph", "graph.json"]] {
        let mut args = vec!["warm", "suggest", "app:build"];
        args.extend(options);
        let output = run(temp.path(), &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires task execution"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("wrote nothing"));
    }
}

#[test]
fn colon_target_inspection_uses_the_execution_identifier() {
    let temp = fixture(json!({
        "install": {"executor":"nx:noop", "configurations":{"ios":{}}},
        "install:ios": {"executor":"nx:noop", "inputs":[], "outputs":[], "configurations":{"ci":{}}},
        "test:unit:watch": {"executor":"nx:noop", "inputs":[], "outputs":[]}
    }));
    for (identifier, target, configuration) in [
        ("app:install:ios", "install:ios", None),
        ("app:install:ios:ci", "install:ios", Some("ci")),
        ("app:test:unit:watch", "test:unit:watch", None),
    ] {
        let plan: Value = serde_json::from_slice(
            &success(run(temp.path(), &["run", identifier, "--dry-run"])).stdout,
        )
        .unwrap();
        let root = plan["roots"][0].as_str().unwrap();
        assert_eq!(plan["tasks"][root]["target"], target);
        assert_eq!(plan["tasks"][root]["configuration"], json!(configuration));
        let inspected: Value = serde_json::from_slice(
            &success(run(temp.path(), &["show", "target", identifier, "--json"])).stdout,
        )
        .unwrap();
        assert_eq!(inspected["target"], target);
        for category in ["inputs", "outputs"] {
            let inspected: Value = serde_json::from_slice(
                &success(run(
                    temp.path(),
                    &["show", "target", category, identifier, "--json"],
                ))
                .stdout,
            )
            .unwrap();
            assert_eq!(inspected["target"], target);
        }
    }
    let configured: Value = serde_json::from_slice(
        &success(run(
            temp.path(),
            &["show", "target", "app:install:ios", "-c", "ci", "--json"],
        ))
        .stdout,
    )
    .unwrap();
    assert_eq!(configured["target"], "install:ios");
    assert_eq!(configured["configuration"], "ci");
}

#[test]
fn task_path_helper() {
    if std::env::var_os("QK_TASK_PATH_HELPER").is_none() {
        return;
    }
    fs::write(
        "task-path.json",
        serde_json::to_vec(
            &std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect::<Vec<_>>(),
        )
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn task_path_prioritizes_local_tools_then_the_node_directory() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap().join("repo");
    let cwd = root.join("packages/app");
    let early = temp.path().join("early");
    let node = temp.path().join("node-bin");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&early).unwrap();
    fs::create_dir_all(&node).unwrap();
    fs::copy(
        std::env::current_exe().unwrap(),
        node.join(format!("node{}", std::env::consts::EXE_SUFFIX)),
    )
    .unwrap();
    let path = std::env::join_paths([early.clone(), node.clone()]).unwrap();
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::write(root.join("project.json"), json!({"name":"app", "targets":{"build":{"command":format!("\"{}\" --exact task_path_helper --nocapture", std::env::current_exe().unwrap().display()), "options":{"cwd":"packages/app/../app", "env":{"QK_TASK_PATH_HELPER":"1", "PATH":early.to_str().unwrap()}}}}}).to_string()).unwrap();
    success(
        command(&root, &["run", "app:build"])
            .env("PATH", &path)
            .output()
            .unwrap(),
    );
    let actual: Vec<PathBuf> =
        serde_json::from_slice(&fs::read(cwd.join("task-path.json")).unwrap()).unwrap();
    let task_cwd = qk_config::Workspace::load(&root)
        .unwrap()
        .root
        .join("packages/app");
    let mut expected: Vec<_> = task_cwd
        .ancestors()
        .map(|dir| dir.join("node_modules/.bin"))
        .collect();
    expected.extend([node.canonicalize().unwrap(), early.clone()]);
    assert_eq!(actual, expected);
    fs::remove_file(node.join(format!("node{}", std::env::consts::EXE_SUFFIX))).unwrap();
    expected.remove(task_cwd.ancestors().count());
    success(
        command(&root, &["run", "app:build"])
            .env("PATH", &path)
            .output()
            .unwrap(),
    );
    let actual: Vec<PathBuf> =
        serde_json::from_slice(&fs::read(cwd.join("task-path.json")).unwrap()).unwrap();
    assert_eq!(actual, expected);
    #[cfg(unix)]
    {
        fs::copy(
            std::env::current_exe().unwrap(),
            node.join("version-manager"),
        )
        .unwrap();
        std::os::unix::fs::symlink("version-manager", node.join("node")).unwrap();
        let later = temp.path().join("later-node-bin");
        fs::create_dir_all(&later).unwrap();
        fs::copy(std::env::current_exe().unwrap(), later.join("node")).unwrap();
        let path = std::env::join_paths([early.clone(), node.clone(), later]).unwrap();
        success(
            command(&root, &["run", "app:build"])
                .env("PATH", &path)
                .output()
                .unwrap(),
        );
        let actual: Vec<PathBuf> =
            serde_json::from_slice(&fs::read(cwd.join("task-path.json")).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }
}

#[cfg(unix)]
#[test]
fn node_proxy_selection_precedes_task_environment_and_cwd_overrides() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let proxy = root.join("proxy-bin");
    fs::create_dir_all(&proxy).unwrap();
    for dir in ["runtime-a", "runtime-b", "nested"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for dir in ["runtime-a", "runtime-b"] {
        fs::copy(
            std::env::current_exe().unwrap(),
            root.join(dir).join("node"),
        )
        .unwrap();
    }
    fs::write(
        proxy.join("manager"),
        "#!/bin/sh\nprintf 'probe\\n' >> \"$PROBE_LOG\"\nprintf '%s\\n' \"$NODE_REAL_PATH\"\n",
    )
    .unwrap();
    fs::set_permissions(proxy.join("manager"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("manager", proxy.join("node")).unwrap();
    let definition = |runtime: &str, cwd: &str| {
        json!({"command":helper().replace("process_helper", "task_path_helper"), "cache":true, "inputs":["{workspaceRoot}/project.json", {"runtime":helper().replace("process_helper", "runtime_path_input_helper")}], "outputs":[format!("{{workspaceRoot}}/{cwd}/task-path.json")], "options":{"cwd":cwd,"env":{
            "PATH":proxy.to_str().unwrap(), "QK_TASK_PATH_HELPER":"1", "PROBE_LOG":root.join("probes").to_str().unwrap(), "NODE_REAL_PATH":root.join(runtime).join("node").to_str().unwrap()
        }}})
    };
    fs::write(root.join("nx.json"), "{}").unwrap();
    fs::write(
        root.join("project.json"),
        json!({"name":"app","targets":{
            "one":definition("runtime-a", "."), "two":definition("runtime-a", "."),
            "three":definition("runtime-b", "."), "four":definition("runtime-a", "nested")
        }})
        .to_string(),
    )
    .unwrap();
    for _ in 0..2 {
        success(
            command(
                &root,
                &["run-many", "-t", "one,two,three,four", "--parallel", "1"],
            )
            .env("PATH", &proxy)
            .env("PROBE_LOG", root.join("probes"))
            .env("NODE_REAL_PATH", root.join("runtime-a/node"))
            .output()
            .unwrap(),
        );
    }
    assert_eq!(
        fs::read_to_string(root.join("probes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::remove_file(proxy.join("node")).unwrap();
    fs::copy(proxy.join("manager"), proxy.join("node")).unwrap();
    success(
        command(
            &root,
            &[
                "run-many",
                "-t",
                "one,two,three,four",
                "--parallel",
                "1",
                "--skip-nx-cache",
            ],
        )
        .env("PATH", &proxy)
        .env("PROBE_LOG", root.join("probes"))
        .env("NODE_REAL_PATH", root.join("runtime-a/node"))
        .output()
        .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(root.join("probes"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let actual: Vec<PathBuf> =
        serde_json::from_slice(&fs::read(root.join("nested/task-path.json")).unwrap()).unwrap();
    assert!(actual.contains(&root.join("runtime-a")));
}

#[test]
fn runtime_path_input_helper() {
    if std::env::var_os("QK_TASK_PATH_HELPER").is_none() {
        return;
    }
    println!("runtime stable");
    std::process::exit(0);
}

#[cfg(unix)]
#[test]
fn cwd_preserves_symlink_parent_resolution_while_path_ancestors_are_normalized() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap().join("repo");
    let tools = root.join("tools");
    let outside = root.parent().unwrap().join("outside");
    fs::create_dir_all(&tools).unwrap();
    fs::create_dir_all(outside.join("package")).unwrap();
    std::os::unix::fs::symlink(outside.join("package"), tools.join("link")).unwrap();
    fs::write(root.join("nx.json"), "{}").unwrap();
    let definition = |cwd: String| json!({"command":helper().replace("process_helper", "task_path_helper"), "options":{"cwd":cwd,"env":{"QK_TASK_PATH_HELPER":"1"}}});
    fs::write(
        root.join("project.json"),
        json!({"name":"app","targets":{
            "relative":definition("tools/link/..".into()),
            "absolute":definition(tools.join("link/..").to_str().unwrap().into())
        }})
        .to_string(),
    )
    .unwrap();
    for task in ["app:relative", "app:absolute"] {
        success(run(&root, &["run", task]));
        assert!(!tools.join("task-path.json").exists());
        let paths: Vec<PathBuf> =
            serde_json::from_slice(&fs::read(outside.join("task-path.json")).unwrap()).unwrap();
        assert_eq!(paths[0], tools.join("node_modules/.bin"));
        fs::remove_file(outside.join("task-path.json")).unwrap();
    }
}
