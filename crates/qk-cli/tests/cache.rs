use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

// Spawn the test binary itself as a portable build step, without Python or Node.
#[test]
fn process_helper() {
    let Ok(mode) = std::env::var("QK_CACHE_TEST_MODE") else {
        return;
    };
    let counter = PathBuf::from(std::env::var("QK_CACHE_TEST_COUNTER").unwrap());
    let runs = fs::read_to_string(&counter)
        .map(|text| text.trim().parse::<u32>().unwrap())
        .unwrap_or(0);
    fs::write(&counter, (runs + 1).to_string()).unwrap();
    let input = fs::read_to_string("src/input.txt").unwrap();
    if mode == "generate" {
        fs::create_dir_all("generated").unwrap();
        fs::write("generated/value.txt", format!("generated:{input}")).unwrap();
        return;
    }
    fs::create_dir_all("dist/nested").unwrap();
    fs::write("dist/nested/out.txt", format!("built:{input}")).unwrap();
    println!("building from {}", input.trim());
    eprintln!("build diagnostics");
    if mode == "fail" {
        std::process::exit(4);
    }
}

fn target(mode: &str, extra: Value) -> Value {
    let mut target = json!({
        "executor": "nx:run-commands",
        "cache": true,
        "outputs": ["{projectRoot}/dist"],
        "options": {
            "command": format!(
                "\"{}\" --exact process_helper --nocapture",
                std::env::current_exe().unwrap().display()
            ),
            "forwardAllArgs": false,
            "env": {"QK_CACHE_TEST_MODE": mode},
        },
    });
    for (key, value) in extra.as_object().unwrap() {
        target[key] = value.clone();
    }
    target
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    counter: PathBuf,
    git_config: PathBuf,
}

impl Fixture {
    fn new(target: Value) -> Self {
        Self::with_targets(json!({"build": target}))
    }

    fn with_targets(targets: Value) -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("nx.json"), "{}").unwrap();
        fs::write(
            root.join("project.json"),
            json!({"name": "app", "targets": targets}).to_string(),
        )
        .unwrap();
        fs::write(root.join("src/input.txt"), "one\n").unwrap();
        fs::write(root.join(".gitignore"), "dist/\ngenerated/\n.qk/\n").unwrap();
        let git_config = temp.path().join("gitconfig");
        fs::write(&git_config, "").unwrap();
        let fixture = Self {
            counter: temp.path().join("runs"),
            root,
            git_config,
            _temp: temp,
        };
        fixture.git(&fixture.root, &["init", "--quiet"]);
        fixture.git(&fixture.root, &["add", "--all"]);
        fixture.git(
            &fixture.root,
            &[
                "-c",
                "user.name=qk",
                "-c",
                "user.email=qk@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        fixture
    }

    fn git(&self, cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn worktree(&self) -> PathBuf {
        let path = self.root.parent().unwrap().join("linked");
        self.git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_str().unwrap(),
            ],
        );
        path
    }

    fn command(&self, root: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_qk"));
        command
            .current_dir(root)
            .arg("--workspace")
            .arg(root)
            .args(args)
            .env("QK_CACHE_TEST_COUNTER", &self.counter)
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("NX_PARALLEL");
        command
    }

    fn qk(&self, root: &Path, args: &[&str]) -> Output {
        self.command(root, args).output().unwrap()
    }

    fn build(&self, root: &Path, args: &[&str]) -> Output {
        let mut all = vec!["run", "app:build"];
        all.extend(args);
        self.qk(root, &all)
    }

    fn runs(&self) -> u32 {
        fs::read_to_string(&self.counter)
            .map(|text| text.parse().unwrap())
            .unwrap_or(0)
    }
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn artifact(root: &Path) -> String {
    fs::read_to_string(root.join("dist/nested/out.txt")).unwrap()
}

#[test]
fn second_run_restores_outputs_and_replays_logs() {
    let fixture = Fixture::new(target("build", json!({})));
    let first = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&first).contains("qk: cache miss app:build"));
    assert!(stdout(&first).contains("building from one"));
    assert_eq!(fixture.runs(), 1);

    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    let second = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&second).contains("qk: cache hit app:build"));
    assert!(stdout(&second).contains("building from one"));
    assert!(stderr(&second).contains("build diagnostics"));
    assert_eq!(fixture.runs(), 1);
    assert_eq!(artifact(&fixture.root), "built:one\n");
}

#[test]
fn restore_replaces_stale_outputs() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("dist/nested/out.txt"), "edited").unwrap();
    fs::write(fixture.root.join("dist/stale.txt"), "stale").unwrap();

    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 1);
    assert_eq!(artifact(&fixture.root), "built:one\n");
    assert!(!fixture.root.join("dist/stale.txt").exists());
}

#[test]
fn input_changes_invalidate_and_old_entries_remain() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    let changed = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&changed).contains("qk: cache miss app:build"));
    assert_eq!(fixture.runs(), 2);
    assert_eq!(artifact(&fixture.root), "built:two\n");

    fs::write(fixture.root.join("src/input.txt"), "one\n").unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
    assert_eq!(artifact(&fixture.root), "built:one\n");
}

#[test]
fn linked_worktrees_share_the_cache() {
    let fixture = Fixture::new(target("build", json!({})));
    let linked = fixture.worktree();
    let main_path = success(fixture.qk(&fixture.root, &["cache", "path"]));
    let linked_path = success(fixture.qk(&linked, &["cache", "path"]));
    assert_eq!(stdout(&main_path), stdout(&linked_path));
    let cache = PathBuf::from(stdout(&main_path).trim());
    assert!(
        cache.canonicalize().is_err(),
        "cache path must not be created"
    );
    assert!(cache.ends_with(".git/qk/cache/v1"));

    success(fixture.build(&fixture.root, &[]));
    let reused = success(fixture.build(&linked, &[]));
    assert!(stderr(&reused).contains("qk: cache hit app:build"));
    assert_eq!(fixture.runs(), 1);
    assert_eq!(artifact(&linked), "built:one\n");

    // Restores copy files, so editing one checkout cannot alter the shared entry.
    fs::write(linked.join("dist/nested/out.txt"), "edited").unwrap();
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(artifact(&fixture.root), "built:one\n");

    fs::write(linked.join("src/input.txt"), "branch\n").unwrap();
    success(fixture.build(&linked, &[]));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn corrupt_entries_are_misses() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    let cache =
        PathBuf::from(stdout(&success(fixture.qk(&fixture.root, &["cache", "path"]))).trim());
    for blob in fs::read_dir(cache.join("blobs")).unwrap() {
        fs::write(blob.unwrap().path(), "corrupt").unwrap();
    }
    let rerun = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&rerun).contains("ignoring unusable cache entry"));
    assert_eq!(fixture.runs(), 2);
    assert_eq!(artifact(&fixture.root), "built:one\n");

    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn skip_cache_neither_reads_nor_writes() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &["--skip-nx-cache"]));
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
    success(fixture.build(&fixture.root, &["--skip-cache"]));
    assert_eq!(fixture.runs(), 3);
}

#[test]
fn failures_are_not_cached() {
    let fixture = Fixture::new(target("fail", json!({})));
    assert_eq!(fixture.build(&fixture.root, &[]).status.code(), Some(4));
    assert_eq!(fixture.build(&fixture.root, &[]).status.code(), Some(4));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn uncacheable_targets_always_execute() {
    let fixture = Fixture::new(target("build", json!({"cache": false})));
    success(fixture.build(&fixture.root, &[]));
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn unsupported_inputs_run_uncached() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["default", {"unknownInputKind": true}]}),
    ));
    let first = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&first).contains("cache bypassed"));
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn declared_inputs_limit_invalidation() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["{projectRoot}/src/**/*", {"env": "QK_CACHE_TEST_FLAVOR"}]}),
    ));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("README.md"), "unrelated").unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 1);

    let flavored = fixture
        .command(&fixture.root, &["run", "app:build"])
        .env("QK_CACHE_TEST_FLAVOR", "spicy")
        .output()
        .unwrap();
    success(flavored);
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn restored_dependencies_keep_dependents_cached() {
    let fixture = Fixture::with_targets(json!({
        "generate": target("generate", json!({"outputs": ["{projectRoot}/generated"]})),
        "build": target("build", json!({"dependsOn": ["generate"]})),
    }));
    let first = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&first).contains("qk: cache miss app:generate"));
    assert!(stderr(&first).contains("qk: cache miss app:build"));
    assert_eq!(fixture.runs(), 2);

    // The dependency's restored fingerprint must match the one its dependent was keyed on.
    fs::remove_dir_all(fixture.root.join("generated")).unwrap();
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    let second = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&second).contains("qk: cache hit app:generate"));
    assert!(stderr(&second).contains("qk: cache hit app:build"));
    assert_eq!(fixture.runs(), 2);
    assert_eq!(
        fs::read_to_string(fixture.root.join("generated/value.txt")).unwrap(),
        "generated:one\n"
    );
}

#[test]
fn extended_glob_exclusions_follow_nx() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": [
            "{projectRoot}/src/**/*",
            "!{projectRoot}/src/**/?(*.)+(spec|test).[jt]s?(x)",
        ]}),
    ));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("src/widget.spec.tsx"), "test").unwrap();
    let excluded = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&excluded).contains("qk: cache hit app:build"));
    fs::write(fixture.root.join("src/widget.tsx"), "source").unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn unsupported_outputs_elsewhere_do_not_bypass() {
    let fixture = Fixture::with_targets(json!({
        "build": target("build", json!({})),
        "e2e": {
            "executor": "nx:noop",
            "outputs": ["{projectRoot}/e2e/../ios/build/*/App.app", "{options.dir}"],
        },
    }));
    let first = success(fixture.qk(&fixture.root, &["run-many", "-t", "build,e2e"]));
    assert!(
        !stderr(&first).contains("cache bypassed"),
        "{}",
        stderr(&first)
    );
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 1);
}

#[test]
fn bypassed_dependencies_are_named() {
    let fixture = Fixture::with_targets(json!({
        "generate": target("generate", json!({
            "cache": false,
            "inputs": [{"unknownInputKind": true}],
            "outputs": ["{projectRoot}/generated"],
        })),
        "build": target("build", json!({"dependsOn": ["generate"]})),
    }));
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains(
            "qk: app:build: cache bypassed (depends on app:generate: unsupported task input declaration)"
        ),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn directory_symlink_inputs_track_their_target() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["{projectRoot}/src/**/*"]}),
    ));
    fs::create_dir_all(fixture.root.join("shared")).unwrap();
    fs::write(fixture.root.join("shared/value.txt"), "a").unwrap();
    std::os::unix::fs::symlink("../shared", fixture.root.join("src/linked")).unwrap();
    success(fixture.build(&fixture.root, &[]));
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 1);

    fs::write(fixture.root.join("shared/value.txt"), "b").unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
}

/// A lockfile where the root installs `lib` and another importer installs `tool`.
fn pnpm_lock(lib: &str, tool: &str) -> String {
    format!(
        "lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      lib:
        specifier: ^1.0.0
        version: {lib}
  tools/other:
    dependencies:
      tool:
        specifier: ^1.0.0
        version: {tool}
packages:
  lib@{lib}:
    resolution: {{integrity: sha512-lib}}
  tool@{tool}:
    resolution: {{integrity: sha512-tool}}
snapshots:
  lib@{lib}: {{}}
  tool@{tool}: {{}}
"
    )
}

#[test]
fn lockfile_changes_invalidate_only_tasks_installing_what_changed() {
    // The project is the workspace root, so its default inputs would match the
    // lockfile itself as a file.
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["{projectRoot}/src/**/*"]}),
    ));
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        pnpm_lock("1.0.0", "1.0.0"),
    )
    .unwrap();
    success(fixture.build(&fixture.root, &[]));
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        pnpm_lock("1.0.0", "1.1.0"),
    )
    .unwrap();
    assert!(
        stderr(&success(fixture.build(&fixture.root, &[]))).contains("qk: cache hit app:build")
    );
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        pnpm_lock("1.1.0", "1.1.0"),
    )
    .unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);

    // A lockfile qk cannot read keys tasks by its whole content.
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        "lockfileVersion: '6.0'\n",
    )
    .unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&output).contains("pnpm-lock.yaml: keying tasks by the whole file"));
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        "lockfileVersion: '6.0'\n# edit\n",
    )
    .unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 4);
}

#[test]
fn external_dependency_inputs_track_packages_any_importer_installs() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["{projectRoot}/src/**/*", {"externalDependencies": ["tool"]}]}),
    ));
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        pnpm_lock("1.0.0", "1.0.0"),
    )
    .unwrap();
    success(fixture.build(&fixture.root, &[]));
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        pnpm_lock("1.0.0", "1.1.0"),
    )
    .unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
}

fn cache_size(fixture: &Fixture) -> u64 {
    let output = success(fixture.qk(&fixture.root, &["cache", "prune", "--max-size", "1000GB"]));
    let text = stdout(&output);
    text.trim_end_matches(" bytes.\n")
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn eviction_removes_least_recently_used_entries() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    let one = cache_size(&fixture);
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    success(fixture.build(&fixture.root, &[]));
    // A hit makes the first entry the most recently used.
    fs::write(fixture.root.join("src/input.txt"), "one\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache hit"));
    assert_eq!(fixture.runs(), 2);

    let output = success(fixture.qk(
        &fixture.root,
        &["cache", "prune", "--max-size", &one.to_string()],
    ));
    assert!(
        stdout(&output).starts_with("Evicted 1 entries"),
        "{}",
        stdout(&output)
    );
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache hit"));
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache miss"));
    assert_eq!(fixture.runs(), 3);

    // Runs keep the cache under NX_MAX_CACHE_SIZE.
    let output = fixture
        .command(&fixture.root, &["run", "app:build"])
        .env("NX_MAX_CACHE_SIZE", "1")
        .output()
        .unwrap();
    assert!(stderr(&success(output)).contains("evicted"));
    assert_eq!(cache_size(&fixture), 0);
}

#[path = "support/s3.rs"]
mod s3;

/// Points the fixture at the fake S3 store and returns a command runner with
/// credentials set.
fn with_remote(fixture: &Fixture, server: &s3::FakeS3, extra: Value) -> Value {
    let mut config = json!({
        "bucket": "cache", "region": "us-east-1",
        "endpoint": server.endpoint, "forcePathStyle": true
    });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    fs::write(
        fixture.root.join("nx.json"),
        json!({"s3": config}).to_string(),
    )
    .unwrap();
    config
}

fn remote_build(fixture: &Fixture, env: &[(&str, &str)]) -> Output {
    let mut command = fixture.command(&fixture.root, &["run", "app:build"]);
    command
        .env("AWS_ACCESS_KEY_ID", "key")
        .env("AWS_SECRET_ACCESS_KEY", "secret")
        .env_remove("CI")
        .env_remove("NX_POWERPACK_CACHE_MODE");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn clear_local_cache(fixture: &Fixture) {
    let path = stdout(&success(fixture.qk(&fixture.root, &["cache", "path"])));
    fs::remove_dir_all(path.trim()).unwrap();
}

#[test]
fn remote_cache_restores_what_another_machine_uploaded() {
    let server = s3::FakeS3::start();
    let fixture = Fixture::new(target("build", json!({})));
    with_remote(&fixture, &server, json!({}));
    let first = success(remote_build(&fixture, &[]));
    assert!(
        stderr(&first).contains("qk: cache miss app:build"),
        "{}",
        stderr(&first)
    );
    assert_eq!(server.objects("/cache/qk/v1/entries/").len(), 1);
    assert!(server.objects("/cache/qk/v1/blobs/").len() >= 2);

    // A machine with an empty local cache restores from the remote store.
    clear_local_cache(&fixture);
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    let second = success(remote_build(&fixture, &[]));
    assert!(
        stderr(&second).contains("qk: remote cache hit app:build"),
        "{}",
        stderr(&second)
    );
    assert_eq!(artifact(&fixture.root), "built:one\n");
    assert!(stdout(&second).contains("building from one"));
    assert_eq!(fixture.runs(), 1);
    // And now holds the entry locally.
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    assert!(stderr(&success(remote_build(&fixture, &[]))).contains("qk: cache hit app:build"));
}

#[test]
fn remote_cache_modes_and_missing_credentials() {
    let server = s3::FakeS3::start();
    let fixture = Fixture::new(target("build", json!({})));
    // Read-only local mode never writes.
    with_remote(
        &fixture,
        &server,
        json!({"localMode": "read-only", "ciMode": "read-write"}),
    );
    success(remote_build(&fixture, &[]));
    assert_eq!(server.writes(), 0);
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    success(remote_build(&fixture, &[("CI", "true")]));
    assert!(server.writes() > 0);
    fs::write(fixture.root.join("src/input.txt"), "three\n").unwrap();
    let before = server.writes();
    success(remote_build(
        &fixture,
        &[("CI", "true"), ("NX_POWERPACK_CACHE_MODE", "no-cache")],
    ));
    assert_eq!(server.writes(), before);

    // Without credentials the remote store is off and the local cache works.
    let output = fixture
        .command(&fixture.root, &["run", "app:build"])
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .output()
        .unwrap();
    let output = success(output);
    assert!(
        stderr(&output).contains("remote cache disabled: no credentials"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("qk: cache hit app:build"));
}

#[test]
fn remote_failures_mid_restore_are_misses() {
    let server = s3::FakeS3::start();
    let fixture = Fixture::new(target("build", json!({})));
    with_remote(&fixture, &server, json!({}));
    success(remote_build(&fixture, &[]));
    clear_local_cache(&fixture);
    server.state.lock().unwrap().fail_blob_reads = true;
    let output = success(remote_build(&fixture, &[]));
    assert!(
        stderr(&output).contains("remote cache unavailable"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("qk: cache miss app:build"));
    assert_eq!(fixture.runs(), 2);
    assert_eq!(artifact(&fixture.root), "built:one\n");
}

#[test]
fn history_explains_why_a_task_missed() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    let report = fixture.root.parent().unwrap().join("report.json");
    success(fixture.build(&fixture.root, &["--report", report.to_str().unwrap()]));

    let report: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let task = &report["tasks"][0];
    assert_eq!(task["id"], "app:build");
    assert_eq!(task["cache"], "miss");
    assert_eq!(task["cause"]["changed"], json!(["files"]));
    assert_eq!(task["cause"]["files"]["changed"], json!(["src/input.txt"]));
    assert_eq!(report["criticalPath"]["tasks"], json!(["app:build"]));

    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:build"]),
    ));
    assert!(text.contains("key changed since run"), "{text}");
    assert!(text.contains("changed src/input.txt"), "{text}");
    assert!(text.contains("first recorded run of this task"), "{text}");
    let text = stdout(&success(fixture.qk(&fixture.root, &["show", "run"])));
    assert!(
        text.contains("app:build") && text.contains("changed: files"),
        "{text}"
    );
    let runs = stdout(&success(
        fixture.qk(&fixture.root, &["show", "runs", "--json"]),
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&runs)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Linked worktrees share the history as they share the cache.
    let linked = fixture.worktree();
    let text = stdout(&success(fixture.qk(&linked, &["show", "runs"])));
    assert_eq!(text.lines().count(), 2, "{text}");
}

#[test]
fn workspace_file_resolution_keys_reach_tasks_through_the_lockfile() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["{projectRoot}/src/**/*", "{workspaceRoot}/pnpm-workspace.yaml"]}),
    ));
    fs::write(
        fixture.root.join("pnpm-lock.yaml"),
        pnpm_lock("1.0.0", "1.0.0"),
    )
    .unwrap();
    let workspace = |catalog: &str, packages: &str| {
        format!("packages:\n  - {packages}\ncatalog:\n  lib: {catalog}\n")
    };
    fs::write(
        fixture.root.join("pnpm-workspace.yaml"),
        workspace("1.0.0", "apps/*"),
    )
    .unwrap();
    success(fixture.build(&fixture.root, &[]));
    fs::write(
        fixture.root.join("pnpm-workspace.yaml"),
        workspace("1.1.0", "apps/*"),
    )
    .unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache hit"));
    fs::write(
        fixture.root.join("pnpm-workspace.yaml"),
        workspace("1.1.0", "libs/*"),
    )
    .unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache miss"));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn dependency_inputs_cover_transitive_dependencies_like_nx() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write("nx.json", "{}");
    write(
        "app/project.json",
        r#"{"name":"app","implicitDependencies":["mid"],
            "targets":{"check":{"command":"echo checked","cache":true,"inputs":["^default"]}}}"#,
    );
    write(
        "mid/project.json",
        r#"{"name":"mid","implicitDependencies":["base"]}"#,
    );
    write("base/project.json", r#"{"name":"base"}"#);
    write("base/src/value.txt", "one");
    let check = || {
        let output = Command::new(env!("CARGO_BIN_EXE_qk"))
            .args(["--workspace", root.to_str().unwrap(), "run", "app:check"])
            .output()
            .unwrap();
        stderr(&success(output))
    };
    assert!(check().contains("cache miss"));
    assert!(check().contains("cache hit"));
    // base is reached only through mid, and still counts.
    write("base/src/value.txt", "two");
    assert!(check().contains("cache miss"));
    // So does the root tsconfig, as in Nx.
    write("tsconfig.base.json", "{}");
    assert!(check().contains("cache miss"));
}

#[test]
fn hits_leave_matching_outputs_in_place() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    let output = fixture.root.join("dist/nested/out.txt");
    let before = fs::symlink_metadata(&output).unwrap();
    let hit = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(
        stdout(&hit).contains("[existing outputs match the cache, left as is]"),
        "{}",
        stdout(&hit)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::symlink_metadata(&output).unwrap().ino(), before.ino());
    }
    let _ = before;
    // An edited output is restored from the cache.
    fs::write(&output, "edited").unwrap();
    let hit = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(stdout(&hit).contains("[local cache]"), "{}", stdout(&hit));
    assert_eq!(artifact(&fixture.root), "built:one\n");
    assert_eq!(fixture.runs(), 1);
}

#[cfg(unix)]
#[test]
fn persisted_digests_notice_edits_that_restore_the_modification_time() {
    let fixture = Fixture::new(target("build", json!({})));
    let input = fixture.root.join("src/input.txt");
    // Old enough that its digest is kept for the next run.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&input)
        .unwrap()
        .set_modified(old)
        .unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert!(fixture.root.join(".git/qk/digests.json").is_file());
    // Nothing of qk's appears in the working tree.
    assert!(!fixture.root.join(".qk").exists());
    // Same size, same modification time; only the change time moves.
    fs::write(&input, "two\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&input)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache miss"));
    assert_eq!(fixture.runs(), 2);
}

#[test]
fn skip_worktree_files_absent_from_disk_are_not_inputs() {
    let fixture = Fixture::new(target("build", json!({})));
    fs::write(fixture.root.join("src/sparse.txt"), "tracked\n").unwrap();
    fixture.git(&fixture.root, &["add", "src/sparse.txt"]);
    fixture.git(
        &fixture.root,
        &[
            "-c",
            "user.name=qk",
            "-c",
            "user.email=qk@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "sparse",
        ],
    );
    fixture.git(
        &fixture.root,
        &["update-index", "--skip-worktree", "src/sparse.txt"],
    );
    fs::remove_file(fixture.root.join("src/sparse.txt")).unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache miss app:build"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&success(fixture.build(&fixture.root, &[]))).contains("qk: cache hit app:build")
    );
}

#[test]
fn dependents_stay_cached_when_a_dependency_reproduces_its_outputs() {
    let fixture = Fixture::with_targets(json!({
        "generate": target("generate", json!({
            "outputs": ["{projectRoot}/generated"],
            "inputs": ["{projectRoot}/src/**/*"],
        })),
        "build": target("build", json!({"dependsOn": ["generate"], "inputs": ["{projectRoot}/generated/**/*"]})),
    }));
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
    // An input the generator does not read: it runs again, writes the same
    // output, and the build that depends on it is still a hit.
    fs::write(fixture.root.join("src/unrelated.txt"), "noise").unwrap();
    let output = stderr(&success(fixture.build(&fixture.root, &[])));
    assert!(output.contains("qk: cache miss app:generate"), "{output}");
    assert!(output.contains("qk: cache hit app:build"), "{output}");
    assert_eq!(fixture.runs(), 3);
    // A change to what it generates reaches the build.
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    let output = stderr(&success(fixture.build(&fixture.root, &[])));
    assert!(output.contains("qk: cache miss app:build"), "{output}");
}

#[test]
fn dotenv_files_are_not_keyed() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs": ["{projectRoot}/src/**/*"]}),
    ));
    fs::write(fixture.root.join(".env.local"), "TOKEN=one\n").unwrap();
    success(fixture.build(&fixture.root, &[]));
    // Per-machine values and credentials: another value must not miss.
    fs::write(fixture.root.join(".env.local"), "TOKEN=two\n").unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache hit"));
}

#[cfg(unix)]
#[test]
fn input_keys_ignore_permission_bits_other_than_executable() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(target("build", json!({})));
    let input = fixture.root.join("src/input.txt");
    fs::set_permissions(&input, fs::Permissions::from_mode(0o644)).unwrap();
    success(fixture.build(&fixture.root, &[]));
    // A group-writable checkout keys the same.
    fs::set_permissions(&input, fs::Permissions::from_mode(0o664)).unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache hit"));
    fs::set_permissions(&input, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache miss"));
}

#[cfg(unix)]
fn said(fixture: &Fixture, root: &Path, args: &[&str]) -> String {
    let mut all = vec!["run", "app:build"];
    all.extend(args);
    let output = success(fixture.qk(root, &all));
    stdout(&output)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[cfg(unix)]
#[test]
fn warm_outputs_start_a_miss_from_the_previous_build() {
    let fixture = Fixture::new(json!({
        "command": "if [ -f dist/state ]; then cat dist/state; else echo cold; fi; if [ dist/state -ot src/input.txt ]; then echo older; fi; mkdir -p dist; cat src/input.txt > dist/state",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"],
        "outputs": ["{projectRoot}/dist"],
        "qk:warm": {"outputs": true}
    }));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    // A fresh checkout: no outputs, and a change that misses the cache.
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    // Restored state looks older than the checkout, whatever the clock says.
    let output = success(fixture.qk(&fixture.root, &["run", "app:build"]));
    assert_eq!(
        stdout(&output).lines().take(2).collect::<Vec<_>>(),
        ["one", "older"]
    );
    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:build"]),
    ));
    assert!(
        text.contains("warm state restored from local: 1 file,"),
        "{text}"
    );
    assert!(text.contains("warm state saved in"), "{text}");
    // Local state wins over the saved one.
    fs::write(fixture.root.join("dist/state"), "local\n").unwrap();
    fs::write(fixture.root.join("src/input.txt"), "three\n").unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "local");
}

#[cfg(unix)]
#[test]
fn warm_directories_follow_the_task_across_worktrees() {
    let fixture = Fixture::new(json!({
        "command": "if [ -f \"${TOOL_CACHE:-/nonexistent}/seen\" ]; then echo warm; else echo cold; fi; if [ -n \"$TOOL_CACHE\" ]; then mkdir -p \"$TOOL_CACHE\" && touch \"$TOOL_CACHE/seen\"; fi",
        "qk:warm": {"env": {"TOOL_CACHE": "{warm}/tool"}}
    }));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    assert_eq!(said(&fixture, &fixture.root, &[]), "warm");
    // Removed, as in a fresh clone, it is restored from the last save.
    let state = fixture.root.join(".git/qk/warm");
    fs::remove_dir_all(&state).unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "warm");
    // A linked worktree has its own directory, restored from the shared store.
    let linked = fixture.worktree();
    assert_eq!(said(&fixture, &linked, &[]), "warm");
    // Without the cache nothing is restored and the variable is not set.
    assert_eq!(said(&fixture, &fixture.root, &["--skip-cache"]), "cold");
}

#[cfg(unix)]
#[test]
fn warm_state_is_shared_through_the_remote_by_branch() {
    let server = s3::FakeS3::start();
    let fixture = Fixture::new(json!({
        "command": "if [ -f \"$TOOL_CACHE/seen\" ]; then cat \"$TOOL_CACHE/seen\"; else echo cold; fi; mkdir -p \"$TOOL_CACHE\" && echo \"$GITHUB_REF_NAME\" > \"$TOOL_CACHE/seen\"",
        "qk:warm": {"env": {"TOOL_CACHE": "{warm}/tool"}}
    }));
    with_remote(&fixture, &server, json!({}));
    let on = |branch: &str| {
        let output = success(remote_build(&fixture, &[("GITHUB_REF_NAME", branch)]));
        stdout(&output)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let fresh = || {
        clear_local_cache(&fixture);
        let _ = fs::remove_dir_all(fixture.root.join(".git/qk/warm"));
    };
    assert_eq!(on("main"), "cold");
    assert_eq!(server.objects("/cache/qk/v1/warm/").len(), 1);
    // Another machine, on a branch without its own state, starts from main's.
    fresh();
    assert_eq!(on("feature"), "main");
    // Once the branch has saved state, it is preferred.
    fresh();
    assert_eq!(on("feature"), "feature");
}

#[test]
fn local_overrides_change_the_task_and_its_key() {
    let fixture = Fixture::new(json!({
        "command": "cat src/input.txt",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"]
    }));
    assert_eq!(said(&fixture, &fixture.root, &[]), "one");
    let mut ignore = fs::read_to_string(fixture.root.join(".gitignore")).unwrap();
    ignore.push_str("project.local.json\n");
    fs::write(fixture.root.join(".gitignore"), ignore).unwrap();
    fs::write(
        fixture.root.join("project.local.json"),
        json!({"targets": {"build": {"command": "echo local"}}}).to_string(),
    )
    .unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert_eq!(stdout(&output).lines().next(), Some("local"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("qk: using local overrides from project.local.json"),
        "{stderr}"
    );
    // Without it, the checked-in definition's entry still applies.
    fs::remove_file(fixture.root.join("project.local.json")).unwrap();
    let output = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(stdout(&output).contains("one"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache hit app:build"));
}
