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
    command.env_remove("NX_CACHE_DIRECTORY");
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("qk: skipped app:build"));
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
    // As in Nx, parallel commands fail the task with 1, whatever the code.
    assert_eq!(output.status.code(), Some(1));
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
        let started = Instant::now();
        let output = success(run(
            temp.path(),
            &["run", "app:bench", "--parallel", parallel],
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(temp.path().join("bench.done").exists());
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
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn dependents_of_continuous_tasks_run_uncached_and_say_why() {
    let temp = fixture(json!({
        "serve": continuous_target("serve", "serve"),
        "bench": {
            "cache": true,
            "dependsOn": ["serve"],
            "executor": "nx:run-commands",
            "options": helper_target("client", "bench")["options"].clone(),
        },
    }));
    let output = success(run(temp.path(), &["run", "app:bench"]));
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "qk: app:bench: cache bypassed (depends on app:serve: continuous tasks are not fingerprinted)"
    ));
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
        "\n✖ qk run app:fail failed\n\nbroken\n"
    );
    assert_eq!(stdout(&["run", "app:build"]), "one\ntwo\n");
    // Held output of a failing task still appears.
    assert!(
        stdout(&["run-many", "-t", "fail", "--output-style", "tui"])
            .contains("> qk run app:fail\n\nbroken")
    );

    // A cache hit replays its log under the same header, marked as such; with
    // no outputs to restore, they match what is on disk.
    stdout(&["run-many", "-t", "cached", "--output-style", "static"]);
    assert_eq!(
        stdout(&["run-many", "-t", "cached", "--output-style", "static"]),
        "\n> qk run app:cached  [existing outputs match the cache, left as is]\n\ncached\n"
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
        grouped.starts_with("\n::group::✅ > qk run app:build\n\none\ntwo\n::endgroup::\n"),
        "{grouped}"
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
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Only the failure, without escape sequences.
    assert_eq!(stdout, "\n✖ qk run app:fail failed\n\nred failure\n");
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
    assert!(String::from_utf8_lossy(&both.stderr).contains("cannot use both {args} and {args.*}"));
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
    assert_eq!(crashed.status.code(), Some(3));
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
        Some(4)
    );
    assert_eq!(
        run(temp.path(), &["run", "app:single"]).status.code(),
        Some(4)
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
    assert!(!bailed.status.success());
    assert!(!String::from_utf8_lossy(&bailed.stdout).contains("zlast\n"));
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

#[cfg(unix)]
#[test]
fn the_task_graph_is_written_as_nx_writes_it() {
    let temp = fixture(json!({
        "serve": {"command": "sleep 1", "continuous": true},
        "build": {"command": "echo build", "cache": true, "outputs": ["{projectRoot}/dist"]},
        "e2e": {"command": "echo e2e", "dependsOn": ["build", "serve"], "parallelism": false}
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
    assert_eq!(tasks["dependencies"]["app:e2e"], json!(["app:build"]));
    assert_eq!(
        tasks["continuousDependencies"]["app:e2e"],
        json!(["app:serve"])
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

#[cfg(target_os = "macos")]
#[test]
fn the_sandbox_reports_and_refuses_what_a_task_does_not_declare() {
    let temp = fixture(json!({
        "build": {
            "command": "cat src/in.txt other/notes.txt .env > /dev/null; mkdir -p dist && echo out > dist/out.txt && echo stray > stray.txt",
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
    success(run(
        temp.path(),
        &[
            "run-many",
            "-t",
            "build,clean",
            "--sandbox",
            "--sandbox-report",
            report.to_str().unwrap(),
        ],
    ));
    let findings: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let build = &findings["tasks"]["app:build"];
    assert_eq!(build["undeclaredReads"], json!(["other/notes.txt"]));
    assert_eq!(build["unkeyedReads"], json!([".env"]));
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
    let stdout = String::from_utf8_lossy(&enforced.stdout);
    assert!(
        stdout.contains("other/notes.txt: Operation not permitted"),
        "{stdout}"
    );
    assert!(!temp.path().join("stray.txt").exists());
    assert!(temp.path().join("dist/out.txt").is_file());
}
