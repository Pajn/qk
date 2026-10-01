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
    if mode == "generate-types" {
        fs::create_dir_all("types/nested/empty.d.ts").unwrap();
        fs::write("types/value.d.ts", input.lines().next().unwrap_or_default()).unwrap();
        fs::write("types/value.js", &input).unwrap();
        return;
    }
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
            .env_remove("NX_PARALLEL")
            .env_remove("NX_CACHE_DIRECTORY");
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

/// Partial and negated outputs restore selected artifacts without touching
/// files outside the selection, including files beside a missing artifact.
#[test]
fn partial_and_excluded_outputs_preserve_unselected_files() {
    for outputs in [
        json!(["{projectRoot}/dist/nested/*.txt"]),
        json!(["{projectRoot}/dist", "!{projectRoot}/dist/keep"]),
    ] {
        let fixture = Fixture::new(target("build", json!({"outputs": outputs})));
        success(fixture.build(&fixture.root, &[]));
        fs::create_dir_all(fixture.root.join("dist/keep")).unwrap();
        fs::write(fixture.root.join("dist/keep/unrelated"), "keep").unwrap();
        fs::write(fixture.root.join("dist/nested/unrelated.bin"), "beside").unwrap();
        fs::remove_file(fixture.root.join("dist/nested/out.txt")).unwrap();
        let restored = success(fixture.build(&fixture.root, &[]));
        assert!(stderr(&restored).contains("qk: cache hit app:build"));
        assert_eq!(fixture.runs(), 1);
        assert_eq!(artifact(&fixture.root), "built:one\n");
        assert_eq!(
            fs::read_to_string(fixture.root.join("dist/keep/unrelated")).unwrap(),
            "keep"
        );
        // The literal directory selection owns unrelated.bin; the partial
        // selection does not, and must preserve it.
        if outputs.as_array().unwrap().len() == 1 {
            assert_eq!(
                fs::read_to_string(fixture.root.join("dist/nested/unrelated.bin")).unwrap(),
                "beside"
            );
        } else {
            assert!(!fixture.root.join("dist/nested/unrelated.bin").exists());
        }
    }
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

fn modified(path: &Path) -> std::time::SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn restored_outputs_are_newer_than_their_inputs_and_dependencies() {
    let fixture = Fixture::with_targets(json!({
        "gen": target("generate", json!({"outputs": ["{projectRoot}/generated"]})),
        "build": target("build", json!({"dependsOn": ["gen"]})),
    }));
    success(fixture.build(&fixture.root, &[]));
    let generated = fixture.root.join("generated/value.txt");
    let built = fixture.root.join("dist/nested/out.txt");

    // Restored outputs are stamped when restored, not when first cached.
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::remove_dir_all(fixture.root.join("generated")).unwrap();
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    let before = std::time::SystemTime::now();
    success(fixture.build(&fixture.root, &[]));
    assert_eq!(fixture.runs(), 2);
    assert!(modified(&generated) >= before);
    assert!(modified(&built) >= modified(&generated));

    // A dependent whose outputs are kept is stamped again when its
    // dependency's outputs are restored, so it stays the newer.
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::remove_dir_all(fixture.root.join("generated")).unwrap();
    let kept = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&kept).contains("qk: cache hit app:build"));
    assert!(modified(&built) >= modified(&generated));

    // With nothing restored, kept outputs are left as they are.
    let times = (modified(&generated), modified(&built));
    std::thread::sleep(std::time::Duration::from_millis(50));
    success(fixture.build(&fixture.root, &[]));
    assert_eq!((modified(&generated), modified(&built)), times);
    assert_eq!(fixture.runs(), 2);
}

#[test]
/// Reset removes only the selected state and preserves recorded run history.
fn reset_clears_the_cache_and_worktree_state_but_keeps_history() {
    let fixture = Fixture::new(target("build", json!({})));
    success(fixture.build(&fixture.root, &[]));
    let git = fixture.root.join(".git/qk");
    assert!(git.join("outputs").exists());

    success(fixture.qk(&fixture.root, &["reset", "--onlyWorkspaceData"]));
    assert!(!git.join("outputs").exists());
    assert!(git.join("cache").exists());
    let hit = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&hit).contains("qk: cache hit app:build"));

    success(fixture.qk(&fixture.root, &["reset"]));
    assert!(!git.join("cache/v1").exists());
    assert!(git.join("history.db").exists());
    let miss = success(fixture.build(&fixture.root, &[]));
    assert!(stderr(&miss).contains("qk: cache miss app:build"));
    assert_eq!(fixture.runs(), 2);
    // Nothing left to remove is not an error.
    success(fixture.qk(&fixture.root, &["reset", "--only-cache"]));
    success(fixture.qk(&fixture.root, &["reset", "--only-cache"]));
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
    let main_key = server.objects("/cache/qk/v1/warm/").pop().unwrap();
    assert_eq!(server.objects("/cache/qk/v1/warm/").len(), 1);
    // Another machine, on a branch without its own state, starts from main's.
    fresh();
    assert_eq!(on("feature"), "main");
    // Once the branch has saved state, it is preferred.
    fresh();
    assert_eq!(on("feature"), "feature");
    let (branch_key, record) = {
        let state = server.state.lock().unwrap();
        state
            .objects
            .iter()
            .find_map(|(key, bytes)| {
                let record: Value = serde_json::from_slice(bytes).ok()?;
                (key.starts_with("/cache/qk/v1/warm/") && key != &main_key)
                    .then(|| (key.clone(), record))
            })
            .unwrap()
    };
    let mut legacy = record.clone();
    legacy["version"] = json!(1);
    let mut wrong_task = record;
    wrong_task["task"] = json!("another-task");
    for bytes in [
        b"{".to_vec(),
        serde_json::to_vec(&legacy).unwrap(),
        serde_json::to_vec(&wrong_task).unwrap(),
    ] {
        server
            .state
            .lock()
            .unwrap()
            .objects
            .insert(branch_key.clone(), bytes);
        fresh();
        assert_eq!(
            on("feature"),
            "main",
            "invalid branch records must allow default-branch fallback"
        );
    }
}

/// A task that prints what its warm `scratch/state` held, then records the
/// worktree it ran in.
#[cfg(unix)]
fn scratch_target(warm: Value) -> Value {
    json!({
        "command": "if [ -f scratch/state ]; then cat scratch/state; else echo cold; fi; mkdir -p scratch; basename \"$PWD\" > scratch/state",
        "qk:warm": warm
    })
}

#[cfg(unix)]
#[test]
fn a_worktree_restores_its_own_save_before_a_newer_one() {
    let fixture = Fixture::new(scratch_target(json!({"paths": ["{projectRoot}/scratch"]})));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    // A new worktree starts from the other worktree's save.
    let linked = fixture.worktree();
    assert_eq!(said(&fixture, &linked, &[]), "repo");
    let text = stdout(&success(
        fixture.qk(&linked, &["show", "task", "app:build"]),
    ));
    assert!(
        text.contains("warm state restored from worktree "),
        "{text}"
    );
    // Its save is newer, but the first worktree comes back to its own.
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "repo");
    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:build"]),
    ));
    assert!(text.contains("warm state restored from local:"), "{text}");
}

#[cfg(unix)]
#[test]
fn state_that_does_not_relocate_stays_in_its_worktree() {
    let fixture = Fixture::new(scratch_target(
        json!({"paths": ["{projectRoot}/scratch"], "portable": false}),
    ));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    let linked = fixture.worktree();
    assert_eq!(said(&fixture, &linked, &[]), "cold");
    // Its own save still comes back.
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "repo");
}

#[cfg(unix)]
#[test]
fn preserved_modification_times_come_back_only_to_their_worktree() {
    let fixture = Fixture::new(json!({
        "command": "if [ -f scratch/state ]; then date -r scratch/state +%Y; else echo cold; fi; mkdir -p scratch; touch scratch/state",
        "qk:warm": {"paths": ["{projectRoot}/scratch"], "mtimes": "preserve"}
    }));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    let year = said(&fixture, &fixture.root, &[]);
    assert_ne!(year, "1970");
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), year);
    // Another worktree's timestamps say nothing about this one's sources.
    let linked = fixture.worktree();
    assert_eq!(said(&fixture, &linked, &[]), "1970");
}

#[cfg(unix)]
fn said_with(fixture: &Fixture, root: &Path, env: &[(&str, &str)]) -> String {
    let mut command = fixture.command(root, &["run", "app:build"]);
    for (name, value) in env {
        command.env(name, value);
    }
    stdout(&success(command.output().unwrap()))
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[cfg(unix)]
#[test]
fn a_warm_key_restores_only_saves_that_match_it() {
    let fixture = Fixture::new(scratch_target(json!({
        "paths": ["{projectRoot}/scratch"],
        "key": ["{projectRoot}/toolchain.txt"]
    })));
    fs::write(fixture.root.join("toolchain.txt"), "1\n").unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    fs::write(fixture.root.join("toolchain.txt"), "2\n").unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    // Back on the first toolchain, its save is still kept.
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    fs::write(fixture.root.join("toolchain.txt"), "1\n").unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "repo");
}

#[cfg(unix)]
#[test]
fn restore_keys_accept_a_save_matching_the_leading_parts() {
    let fixture = Fixture::new(scratch_target(json!({
        "paths": ["{projectRoot}/scratch"],
        "key": ["{projectRoot}/toolchain.txt", {"env": "DEPENDENCIES"}],
        "restoreKeys": 1
    })));
    fs::write(fixture.root.join("toolchain.txt"), "1\n").unwrap();
    let run =
        |dependencies: &str| said_with(&fixture, &fixture.root, &[("DEPENDENCIES", dependencies)]);
    assert_eq!(run("a"), "cold");
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    assert_eq!(run("b"), "repo");
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    fs::write(fixture.root.join("toolchain.txt"), "2\n").unwrap();
    assert_eq!(run("b"), "cold");
}

#[cfg(unix)]
#[test]
fn a_new_worktree_prefers_the_save_nearest_behind_its_head() {
    let fixture = Fixture::new(scratch_target(json!({"paths": ["{projectRoot}/scratch"]})));
    let base = fixture.root.parent().unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    // A newer save, made on a commit the next worktree does not have.
    let linked = fixture.worktree();
    fixture.git(
        &linked,
        &[
            "-c",
            "user.name=qk",
            "-c",
            "user.email=qk@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "ahead",
        ],
    );
    assert_eq!(said(&fixture, &linked, &[]), "repo");
    let third = base.join("third");
    fixture.git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            third.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert_eq!(said(&fixture, &third, &[]), "repo");
}

#[cfg(unix)]
#[test]
fn a_warm_group_is_shared_by_the_targets_that_name_it() {
    let tool = |name: &str| {
        json!({
            "command": format!("if [ -f \"$TOOL_CACHE/seen\" ]; then cat \"$TOOL_CACHE/seen\"; else echo cold; fi; mkdir -p \"$TOOL_CACHE\" && echo {name} > \"$TOOL_CACHE/seen\""),
            "qk:warm": {"group": "tool", "env": {"TOOL_CACHE": "{warm}/tool"}}
        })
    };
    let fixture = Fixture::with_targets(json!({"build": tool("build"), "test": tool("test")}));
    let run = |root: &Path, target: &str| {
        stdout(&success(
            fixture.qk(root, &["run", &format!("app:{target}")]),
        ))
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
    };
    assert_eq!(run(&fixture.root, "build"), "cold");
    assert_eq!(run(&fixture.root, "test"), "build");
    // Gone from the worktree, one target restores what the other saved.
    fs::remove_dir_all(fixture.root.join(".git/qk/warm")).unwrap();
    assert_eq!(run(&fixture.root, "build"), "test");
    let linked = fixture.worktree();
    assert_eq!(run(&linked, "test"), "build");
    // Both restoring it at once is serialized.
    fs::remove_dir_all(fixture.root.join(".git/qk/warm")).unwrap();
    success(fixture.qk(
        &fixture.root,
        &["run-many", "-t", "build,test", "--parallel", "2"],
    ));
    assert!(fixture.root.join(".git/qk/warm").is_dir());
}

#[cfg(unix)]
#[test]
fn a_warm_group_cannot_keep_one_task_s_paths() {
    let fixture = Fixture::new(json!({
        "command": "echo ran",
        "qk:warm": {"group": "tool", "paths": ["{projectRoot}/scratch"]}
    }));
    let output = fixture.build(&fixture.root, &[]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("a qk:warm.group shares {warm} alone"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn excluded_warm_paths_are_neither_saved_nor_inputs() {
    let fixture = Fixture::new(json!({
        "command": "echo $(ls scratch 2>/dev/null); mkdir -p scratch/big; echo kept > scratch/state; echo large > scratch/big/blob",
        "cache": true,
        "outputs": ["{projectRoot}/dist"],
        "qk:warm": {"paths": ["{projectRoot}/scratch", "!{projectRoot}/scratch/big"]}
    }));
    assert_eq!(said(&fixture, &fixture.root, &[]), "");
    // Changing what is excluded changes no key.
    fs::write(fixture.root.join("scratch/big/blob"), "changed\n").unwrap();
    assert!(stderr(&success(fixture.build(&fixture.root, &[]))).contains("cache hit"));
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    fs::write(fixture.root.join("src/input.txt"), "two\n").unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "state");
}

/// A fixture whose `native/` is generated, and so ignored, as a native
/// project is.
#[cfg(unix)]
fn native_fixture(targets: Value) -> Fixture {
    let fixture = Fixture::with_targets(targets);
    let mut ignore = fs::read_to_string(fixture.root.join(".gitignore")).unwrap();
    ignore.push_str("native\n");
    fs::write(fixture.root.join(".gitignore"), ignore).unwrap();
    fixture
}

/// A prebuild that recreates `native/`, and a build that keeps
/// `native/build` in it and prints the year its state was written.
#[cfg(unix)]
fn survive_targets(prebuild: &str) -> Value {
    json!({
        "prebuild": {"command": prebuild},
        "build": {
            "command": "if [ -f native/build/state ]; then date -r native/build/state +%Y; else echo cold; fi; mkdir -p native/build; touch native/build/state",
            "dependsOn": ["prebuild"],
            "qk:warm": {"paths": ["{projectRoot}/native/build"], "survive": ["prebuild"]}
        }
    })
}

#[cfg(unix)]
#[test]
fn surviving_paths_outlast_a_dependency_that_deletes_them() {
    let fixture = native_fixture(survive_targets(
        "rm -rf native && mkdir -p native && echo generated > native/config",
    ));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    // Kept as it was, not restored from the save, which would date it 1970.
    let year = said(&fixture, &fixture.root, &[]);
    assert_ne!(year, "1970");
    assert_ne!(year, "cold");
    // What the dependency generated beside them stays.
    assert_eq!(
        fs::read_to_string(fixture.root.join("native/config")).unwrap(),
        "generated\n"
    );
}

#[cfg(unix)]
#[test]
fn surviving_paths_come_back_when_the_dependency_fails() {
    let fixture = native_fixture(survive_targets("mkdir -p native"));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    let failing = survive_targets("rm -rf native; exit 3");
    fs::write(
        fixture.root.join("project.json"),
        json!({"name": "app", "targets": failing}).to_string(),
    )
    .unwrap();
    let output = fixture.build(&fixture.root, &[]);
    assert!(!output.status.success());
    assert!(fixture.root.join("native/build/state").is_file());
}

#[cfg(unix)]
#[test]
fn surviving_paths_left_aside_by_a_killed_run_come_back() {
    let fixture = native_fixture(survive_targets("mkdir -p native"));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    let slow = survive_targets("sleep 3");
    fs::write(
        fixture.root.join("project.json"),
        json!({"name": "app", "targets": slow}).to_string(),
    )
    .unwrap();
    let mut running = fixture
        .command(&fixture.root, &["run", "app:build"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Killed while the dependency runs, with the paths moved aside.
    let moved = || !fixture.root.join("native/build/state").exists();
    for _ in 0..100 {
        if moved() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(moved());
    running.kill().unwrap();
    running.wait().unwrap();
    fs::write(
        fixture.root.join("project.json"),
        json!({"name": "app", "targets": survive_targets("true")}).to_string(),
    )
    .unwrap();
    let year = said(&fixture, &fixture.root, &[]);
    assert_ne!(year, "1970");
    assert_ne!(year, "cold");
}

#[cfg(unix)]
#[test]
fn a_background_save_finishes_before_the_run_does() {
    let fixture = Fixture::new(scratch_target(
        json!({"paths": ["{projectRoot}/scratch"], "save": "background"}),
    ));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:build"]),
    ));
    assert!(
        text.contains("warm state saved in the background"),
        "{text}"
    );
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    assert_eq!(said(&fixture, &fixture.root, &[]), "repo");
}

#[cfg(unix)]
#[test]
fn show_task_compares_runs_from_warm_state_with_runs_without() {
    let fixture = Fixture::new(scratch_target(json!({"paths": ["{projectRoot}/scratch"]})));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:build"]),
    ));
    assert!(!text.contains("From warm state"), "{text}");
    // On disk already, which counts as warm as much as a restore does.
    assert_eq!(said(&fixture, &fixture.root, &[]), "repo");
    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:build"]),
    ));
    assert!(
        text.contains("From warm state it took ") && text.contains(" over 1 run; without, "),
        "{text}"
    );
}

/// Rewrites the fixture's project with these targets.
#[cfg(unix)]
fn set_targets(fixture: &Fixture, targets: Value) {
    fs::write(
        fixture.root.join("project.json"),
        json!({"name": "app", "targets": targets}).to_string(),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn surviving_paths_are_not_put_back_through_a_symlink() {
    let fixture = native_fixture(survive_targets("mkdir -p native"));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    // Outside the workspace, where native/build would lead through the link.
    let outside = fixture.root.parent().unwrap().join("outside");
    fs::create_dir_all(outside.join("build")).unwrap();
    fs::write(outside.join("build/unrelated"), "keep\n").unwrap();
    set_targets(
        &fixture,
        // Failing, so that only qk could write through the link.
        survive_targets(&format!(
            "rm -rf native && ln -s {} native && exit 3",
            outside.display()
        )),
    );
    let _ = fixture.build(&fixture.root, &[]);
    assert_eq!(
        fs::read_to_string(outside.join("build/unrelated")).unwrap(),
        "keep\n"
    );
    assert!(!outside.join("build/state").exists());
    // Once native is a directory again, the kept state comes back.
    set_targets(&fixture, survive_targets("rm -f native; mkdir -p native"));
    let year = said(&fixture, &fixture.root, &[]);
    assert_ne!(year, "1970");
    assert_ne!(year, "cold");
}

#[cfg(unix)]
#[test]
fn surviving_paths_put_back_later_when_their_parent_is_not_a_directory() {
    let fixture = native_fixture(survive_targets("mkdir -p native"));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    set_targets(
        &fixture,
        survive_targets("rm -rf native && echo file > native"),
    );
    let _ = fixture.build(&fixture.root, &[]);
    set_targets(&fixture, survive_targets("rm -f native; mkdir -p native"));
    let year = said(&fixture, &fixture.root, &[]);
    assert_ne!(year, "1970");
    assert_ne!(year, "cold");
}

#[cfg(unix)]
#[test]
fn paths_put_back_beside_one_that_could_not_be_survive_the_next_dependency() {
    let targets = |prebuild: &str| {
        json!({
            "prebuild": {"command": prebuild},
            "build": {
                "command": "for d in one two; do if [ -f $d/build/state ]; then date -r $d/build/state +%Y; else echo cold; fi; done; mkdir -p one/build two/build; touch one/build/state two/build/state",
                "dependsOn": ["prebuild"],
                "qk:warm": {
                    "paths": ["{projectRoot}/one/build", "{projectRoot}/two/build"],
                    "survive": ["prebuild"]
                }
            }
        })
    };
    let fixture = Fixture::with_targets(targets("mkdir -p one two"));
    let mut ignore = fs::read_to_string(fixture.root.join(".gitignore")).unwrap();
    ignore.push_str("one\ntwo\n");
    fs::write(fixture.root.join(".gitignore"), ignore).unwrap();
    let years = |fixture: &Fixture| {
        stdout(&success(fixture.build(&fixture.root, &[])))
            .lines()
            .take(2)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(years(&fixture), ["cold", "cold"]);
    // one/build cannot go back, its parent now a file; two/build can.
    set_targets(
        &fixture,
        targets("rm -rf one two && echo file > one && mkdir two"),
    );
    let _ = fixture.build(&fixture.root, &[]);
    assert!(fixture.root.join("two/build/state").is_file());
    // The next dependency deletes both; both are kept across it.
    set_targets(&fixture, targets("rm -rf one two; mkdir one two"));
    for year in years(&fixture) {
        assert_ne!(year, "cold");
        assert_ne!(year, "1970");
    }
}

#[cfg(unix)]
#[test]
fn surviving_paths_outlast_dependencies_that_run_at_once() {
    let fixture = native_fixture(json!({
        "prepare-a": {"command": "rm -rf native; mkdir -p native; sleep 1"},
        "prepare-b": {"command": "rm -rf native/build; sleep 1"},
        "build": {
            "command": "if [ -f native/build/state ]; then date -r native/build/state +%Y; else echo cold; fi; mkdir -p native/build; touch native/build/state",
            "dependsOn": ["prepare-a", "prepare-b"],
            "qk:warm": {"paths": ["{projectRoot}/native/build"], "survive": ["prepare-a", "prepare-b"]}
        }
    }));
    let run = || {
        let output = success(fixture.qk(
            &fixture.root,
            &[
                "run",
                "app:build",
                "--parallel",
                "2",
                "--output-style",
                "static",
            ],
        ));
        stdout(&output)
            .lines()
            .find(|line| {
                *line == "cold" || (!line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
            })
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(run(), "cold");
    for _ in 0..3 {
        let year = run();
        assert_ne!(year, "1970");
        assert_ne!(year, "cold");
    }
}

#[cfg(unix)]
#[test]
fn an_environment_key_reads_the_task_s_own_environment() {
    let keyed = |value: &str| {
        let mut target = scratch_target(json!({
            "paths": ["{projectRoot}/scratch"],
            "key": [{"env": "TOOLCHAIN"}]
        }));
        target["options"] = json!({"env": {"TOOLCHAIN": value}});
        target
    };
    let fixture = Fixture::new(keyed("1"));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
    fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
    set_targets(&fixture, json!({"build": keyed("2")}));
    assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
}

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn a_local_workspace_file_changes_every_task_and_its_key() {
    let fixture = Fixture::new(json!({
        "command": "echo $MODE",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"]
    }));
    assert_eq!(said(&fixture, &fixture.root, &[]), "");
    fs::write(
        fixture.root.join("nx.local.json"),
        json!({"targetDefaults": {"build": {"options": {"env": {"MODE": "local"}}}}}).to_string(),
    )
    .unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert_eq!(stdout(&output).lines().next(), Some("local"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("qk: using local overrides from nx.local.json"),
        "{stderr}"
    );
}

#[test]
fn parallel_defaults_to_the_workspace_setting() {
    let fixture = Fixture::with_targets(json!({
        "build": {"command": "echo build"},
        "test": {"command": "echo test"}
    }));
    let parallel = |fixture: &Fixture| {
        let output = success(fixture.qk(
            &fixture.root,
            &["run-many", "-t", "build,test", "--output-style", "quiet"],
        ));
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        stderr
            .lines()
            .find_map(|line| {
                line.strip_prefix("Parallel ")?
                    .split(':')
                    .next()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("{stderr}"))
    };
    assert_eq!(parallel(&fixture), "3");
    fs::write(fixture.root.join("nx.local.json"), r#"{"parallel": 1}"#).unwrap();
    assert_eq!(parallel(&fixture), "1");
}

#[cfg(unix)]
fn threaded(command: &str, threads: Value) -> Value {
    json!({"command": format!("mkdir -p dist && {command}"), "qk:threads": threads})
}

#[cfg(unix)]
#[test]
fn threaded_tasks_share_the_cores() {
    let task = |name: &str| {
        threaded(
            &format!("echo $QK_THREADS $WORKERS > dist/{name}"),
            json!({"env": {"WORKERS": "--maxWorkers={threads}"}}),
        )
    };
    let fixture = Fixture::with_targets(json!({"a": task("a"), "b": task("b"), "c": task("c")}));
    let output = success(fixture.qk(
        &fixture.root,
        &[
            "run-many",
            "-t",
            "a,b,c",
            "--parallel",
            "3",
            "--cores",
            "8",
            "--output-style",
            "quiet",
        ],
    ));
    for name in ["a", "b", "c"] {
        assert_eq!(
            fs::read_to_string(fixture.root.join("dist").join(name)).unwrap(),
            "2 --maxWorkers=2\n"
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Threads of 8 cores: app:a 2, app:b 2, app:c 2"),
        "{stderr}"
    );
    success(fixture.qk(&fixture.root, &["run", "app:a", "--cores", "8"]));
    assert_eq!(
        fs::read_to_string(fixture.root.join("dist/a")).unwrap(),
        "8 --maxWorkers=8\n"
    );
    let text = stdout(&success(
        fixture.qk(&fixture.root, &["show", "task", "app:a"]),
    ));
    assert!(text.contains("ran with 8 threads"), "{text}");
}

#[cfg(unix)]
#[test]
fn a_threaded_task_waits_for_its_minimum() {
    let task = |name: &str| {
        threaded(
            &format!(
                "if [ -e dist/running ]; then echo overlap; fi; touch dist/running; sleep 0.3; rm dist/running; echo $QK_THREADS > dist/{name}"
            ),
            json!({"min": 3}),
        )
    };
    let fixture = Fixture::with_targets(json!({"a": task("a"), "b": task("b")}));
    let output = success(fixture.qk(
        &fixture.root,
        &[
            "run-many",
            "-t",
            "a,b",
            "--parallel",
            "2",
            "--cores",
            "4",
            "--output-style",
            "static",
        ],
    ));
    assert!(!stdout(&output).contains("overlap"), "{}", stdout(&output));
    // The first starts with its minimum; the second with every core once it is free.
    let threads = |name: &str| fs::read_to_string(fixture.root.join("dist").join(name)).unwrap();
    assert_eq!((threads("a"), threads("b")), ("3\n".into(), "4\n".into()));
}

#[test]
fn the_thread_count_is_not_part_of_the_key() {
    let fixture = Fixture::new(json!({
        "command": "echo built",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"],
        "qk:threads": true
    }));
    success(fixture.build(&fixture.root, &["--cores", "8"]));
    let output =
        success(fixture.build(&fixture.root, &["--cores", "2", "--output-style", "static"]));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache hit app:build"));
}

#[cfg(unix)]
#[test]
fn a_threaded_task_leaves_cores_for_the_work_still_to_come() {
    let fixture = Fixture::with_targets(json!({
        "tests": threaded("echo $QK_THREADS > dist/tests", json!(true)),
        "codegen": {"command": "sleep 0.2"},
        "tsc": {"command": "true", "dependsOn": ["codegen"]}
    }));
    success(fixture.qk(
        &fixture.root,
        &[
            "run-many",
            "-t",
            "tests,tsc",
            "--parallel",
            "3",
            "--cores",
            "4",
        ],
    ));
    // Two of the three slots can still be taken by codegen and tsc.
    assert_eq!(
        fs::read_to_string(fixture.root.join("dist/tests")).unwrap(),
        "2\n"
    );
}

#[cfg(unix)]
#[test]
fn a_task_without_parallelism_runs_alone() {
    // Each task notes whether another was running when it started or ended.
    let task = |name: &str, alone: bool| {
        let mut task = json!({"command": format!(
            "mkdir -p dist; ls dist | grep -q running && echo {name}-overlap; touch dist/running-{name}; sleep 0.2; ls dist | grep running | grep -vq running-{name} && echo {name}-overlap; rm dist/running-{name}"
        )});
        if alone {
            task["parallelism"] = json!(false);
        }
        task
    };
    let fixture = Fixture::with_targets(json!({
        "a": task("a", false), "b": task("b", true), "c": task("c", false)
    }));
    let output = success(fixture.qk(
        &fixture.root,
        &[
            "run-many",
            "-t",
            "a,b,c",
            "--parallel",
            "3",
            "--output-style",
            "static",
        ],
    ));
    let text = stdout(&output);
    assert!(!text.contains("b-overlap"), "{text}");
    // The others still run beside each other.
    assert!(
        text.contains("a-overlap") || text.contains("c-overlap"),
        "{text}"
    );
}

#[test]
fn a_task_without_parallelism_cannot_depend_on_a_continuous_task() {
    let fixture = Fixture::with_targets(json!({
        "serve": {"command": "sleep 5", "continuous": true},
        "e2e": {"command": "true", "parallelism": false, "dependsOn": ["serve"]}
    }));
    let output = fixture.qk(&fixture.root, &["run", "app:e2e"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "app:e2e does not support parallelism but depends on continuous task app:serve"
        )
    );
}

#[cfg(unix)]
#[test]
fn outputs_read_options_and_arguments_like_nx() {
    let fixture = Fixture::new(json!({
        "command": "mkdir -p {args.outputPath} && echo built > {args.outputPath}/file",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"],
        "options": {"outputPath": "out/app"},
        // An output naming an option that is not set is left out.
        "outputs": ["{options.outputPath}", "{options.missing}/x"]
    }));
    success(fixture.build(&fixture.root, &[]));
    fs::remove_dir_all(fixture.root.join("out")).unwrap();
    let hit = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(
        stderr(&hit).contains("cache hit app:build"),
        "{}",
        stderr(&hit)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("out/app/file")).unwrap(),
        "built\n"
    );
    // An argument sets the option the output reads.
    success(fixture.build(&fixture.root, &["--", "--outputPath=out/other"]));
    fs::remove_dir_all(fixture.root.join("out")).unwrap();
    success(fixture.build(&fixture.root, &["--", "--outputPath=out/other"]));
    assert!(fixture.root.join("out/other/file").is_file());
    assert!(!fixture.root.join("out/app").exists());
}

#[cfg(unix)]
#[test]
fn negated_outputs_are_neither_cached_nor_removed() {
    let fixture = Fixture::new(json!({
        "command": "mkdir -p dist/cache && echo kept > dist/app && echo scratch > dist/cache/tmp",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"],
        "outputs": ["{projectRoot}/dist", "!{projectRoot}/dist/cache"]
    }));
    success(fixture.build(&fixture.root, &[]));
    fs::remove_file(fixture.root.join("dist/app")).unwrap();
    fs::write(fixture.root.join("dist/cache/tmp"), "local").unwrap();
    let hit = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(
        stderr(&hit).contains("cache hit app:build"),
        "{}",
        stderr(&hit)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("dist/app")).unwrap(),
        "kept\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("dist/cache/tmp")).unwrap(),
        "local"
    );
    fs::remove_dir_all(fixture.root.join("dist")).unwrap();
    success(fixture.build(&fixture.root, &[]));
    assert!(fixture.root.join("dist/app").is_file());
    assert!(!fixture.root.join("dist/cache").exists());
}

#[cfg(unix)]
#[test]
fn a_build_target_without_outputs_caches_nx_defaults() {
    let fixture = Fixture::new(json!({
        "command": "mkdir -p build && echo built > build/app",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"]
    }));
    success(fixture.build(&fixture.root, &[]));
    fs::remove_dir_all(fixture.root.join("build")).unwrap();
    let hit = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(
        stderr(&hit).contains("cache hit app:build"),
        "{}",
        stderr(&hit)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("build/app")).unwrap(),
        "built\n"
    );
}

/// `app` at the root depends on `lib`, whose `src` named input is its sources.
fn with_lib(inputs: Value) -> Fixture {
    let fixture = Fixture::new(json!({}));
    fs::write(
        fixture.root.join("project.json"),
        json!({"name": "app", "implicitDependencies": ["lib"], "targets": {"build": {
            "command": "echo built", "cache": true, "inputs": inputs
        }}})
        .to_string(),
    )
    .unwrap();
    fs::create_dir_all(fixture.root.join("libs/lib/src")).unwrap();
    fs::write(
        fixture.root.join("libs/lib/project.json"),
        json!({"name": "lib", "namedInputs": {"src": ["{projectRoot}/src/**/*"]}}).to_string(),
    )
    .unwrap();
    fs::write(fixture.root.join("libs/lib/src/a.txt"), "a").unwrap();
    fs::write(fixture.root.join("libs/lib/other.txt"), "other").unwrap();
    fs::write(fixture.root.join("libs/lib/config.json"), "{}").unwrap();
    fixture
}

fn hit(fixture: &Fixture, root: &Path) -> bool {
    let output = success(fixture.build(root, &["--output-style", "static"]));
    stderr(&output).contains("cache hit app:build")
}

#[test]
fn inputs_of_other_projects_by_name_or_through_dependencies() {
    for inputs in [
        json!([{"input": "src", "projects": ["lib"]}]),
        json!([{"input": "src", "projects": "tag:none"}, {"input": "src", "dependencies": true}]),
        json!([{"input": "src", "projects": "dependencies"}]),
    ] {
        let fixture = with_lib(inputs.clone());
        assert!(!hit(&fixture, &fixture.root), "{inputs}");
        fs::write(fixture.root.join("libs/lib/other.txt"), "changed").unwrap();
        assert!(hit(&fixture, &fixture.root), "{inputs}");
        fs::write(fixture.root.join("libs/lib/src/a.txt"), "changed").unwrap();
        assert!(!hit(&fixture, &fixture.root), "{inputs}");
    }
}

#[test]
fn filesets_of_dependencies() {
    let fixture = with_lib(json!([{"fileset": "{projectRoot}/config.json", "dependencies": true}]));
    assert!(!hit(&fixture, &fixture.root));
    fs::write(fixture.root.join("libs/lib/other.txt"), "changed").unwrap();
    assert!(hit(&fixture, &fixture.root));
    fs::write(fixture.root.join("libs/lib/config.json"), r#"{"a": 1}"#).unwrap();
    assert!(!hit(&fixture, &fixture.root));
}

#[test]
fn json_inputs_key_only_the_fields_they_select() {
    let fixture = with_lib(json!([
        {"json": "{projectRoot}/meta.json", "fields": ["version", "build.target"]},
        {"json": "libs/lib/config.json", "excludeFields": ["comment"]}
    ]));
    let meta = |text: &str| fs::write(fixture.root.join("meta.json"), text).unwrap();
    meta(r#"{"version": 1, "name": "a", "build": {"target": "es2022", "debug": false}}"#);
    assert!(!hit(&fixture, &fixture.root));
    meta(r#"{"version": 1, "name": "b", "build": {"target": "es2022", "debug": true}}"#);
    assert!(hit(&fixture, &fixture.root));
    meta(r#"{"version": 2, "name": "b", "build": {"target": "es2022", "debug": true}}"#);
    assert!(!hit(&fixture, &fixture.root));
    fs::write(
        fixture.root.join("libs/lib/config.json"),
        r#"{"comment": "x"}"#,
    )
    .unwrap();
    assert!(hit(&fixture, &fixture.root));
    fs::write(
        fixture.root.join("libs/lib/config.json"),
        r#"{"comment": "x", "a": 1}"#,
    )
    .unwrap();
    assert!(!hit(&fixture, &fixture.root));
}

#[test]
fn the_working_directory_can_be_an_input() {
    let fixture = with_lib(json!([{"workingDirectory": "relative"}]));
    let from = |directory: &Path| {
        let output = fixture
            .command(
                &fixture.root,
                &["run", "app:build", "--output-style", "static"],
            )
            .current_dir(directory)
            .output()
            .unwrap();
        stderr(&success(output)).contains("cache hit app:build")
    };
    let sub = fixture.root.join("libs");
    assert!(!from(&fixture.root));
    assert!(from(&fixture.root));
    assert!(!from(&sub));
    assert!(from(&sub));
}

#[test]
fn named_inputs_cannot_reach_other_projects() {
    let fixture = with_lib(json!(["shared"]));
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(fixture.root.join("project.json")).unwrap())
            .unwrap();
    project["namedInputs"] = json!({"shared": [{"input": "src", "projects": ["lib"]}]});
    fs::write(fixture.root.join("project.json"), project.to_string()).unwrap();
    let output = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    assert!(
        stderr(&output)
            .contains("named inputs can only refer to named inputs of their own project"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn tasks_see_their_hash() {
    let fixture = Fixture::new(json!({
        "command": "echo hash=$NX_TASK_HASH",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/*"]
    }));
    let output = success(fixture.build(&fixture.root, &["--output-style", "static"]));
    let printed = stdout(&output)
        .lines()
        .find_map(|line| line.strip_prefix("hash="))
        .unwrap()
        .to_owned();
    let run: Value = serde_json::from_str(&stdout(&success(
        fixture.qk(&fixture.root, &["show", "run", "--json"]),
    )))
    .unwrap();
    assert_eq!(run["tasks"][0]["key"], json!(printed));
    assert!(!printed.is_empty());
}

#[test]
fn skipping_the_remote_cache_keeps_to_the_local_one() {
    let server = s3::FakeS3::start();
    let fixture = Fixture::new(target("build", json!({})));
    with_remote(&fixture, &server, json!({}));
    success(remote_build(&fixture, &[("NX_SKIP_REMOTE_CACHE", "true")]));
    assert_eq!(server.writes(), 0);
    // The flag sets the same.
    let mut command = fixture.command(
        &fixture.root,
        &[
            "run",
            "app:build",
            "--skip-remote-cache",
            "--output-style",
            "static",
        ],
    );
    command
        .env("AWS_ACCESS_KEY_ID", "key")
        .env("AWS_SECRET_ACCESS_KEY", "secret")
        .env_remove("CI");
    let output = success(command.output().unwrap());
    assert!(
        stderr(&output).contains("cache hit app:build"),
        "{}",
        stderr(&output)
    );
    assert_eq!(server.writes(), 0);
    success(remote_build(&fixture, &[]));
    fs::write(fixture.root.join("src/input.txt"), "changed\n").unwrap();
    success(remote_build(&fixture, &[]));
    assert!(server.writes() > 0);
}

#[test]
fn the_cache_directory_can_be_set_as_in_nx() {
    let fixture = Fixture::new(json!({"command": "echo built", "cache": true}));
    let run = |env: &[(&str, &str)]| {
        let mut command = fixture.command(
            &fixture.root,
            &["run", "app:build", "--output-style", "static"],
        );
        for (name, value) in env {
            command.env(name, value);
        }
        stderr(&success(command.output().unwrap()))
    };
    let env = [("NX_CACHE_DIRECTORY", "shared-cache")];
    run(&env);
    // The cache inside the workspace is not an input of the task it caches.
    assert!(run(&env).contains("cache hit app:build"));
    assert!(fixture.root.join("shared-cache/qk/v1").is_dir());
    let path = stdout(&success(
        fixture
            .command(&fixture.root, &["cache", "path"])
            .envs(env)
            .output()
            .unwrap(),
    ));
    assert!(
        Path::new(path.trim()).ends_with("shared-cache/qk/v1"),
        "{path}"
    );
    // nx.json's cacheDirectory, when the variable is not set.
    fs::write(
        fixture.root.join("nx.json"),
        r#"{"cacheDirectory": "from-nx-json"}"#,
    )
    .unwrap();
    run(&[]);
    assert!(fixture.root.join("from-nx-json/qk/v1").is_dir());
}

#[test]
fn negated_groups_in_inputs_leave_out_what_they_name() {
    let fixture = Fixture::new(json!({
        "command": "echo built",
        "cache": true,
        "inputs": ["{projectRoot}/src/**/!(*.test|*.spec).ts"]
    }));
    fs::write(fixture.root.join("src/app.ts"), "app").unwrap();
    fs::write(fixture.root.join("src/app.test.ts"), "test").unwrap();
    let hit = |fixture: &Fixture| {
        stderr(&success(
            fixture.build(&fixture.root, &["--output-style", "static"]),
        ))
        .contains("cache hit app:build")
    };
    assert!(!hit(&fixture));
    fs::write(fixture.root.join("src/app.test.ts"), "changed test").unwrap();
    assert!(hit(&fixture));
    fs::write(fixture.root.join("src/app.ts"), "changed app").unwrap();
    assert!(!hit(&fixture));
}

/// Output patterns ignore unrelated artifacts and optionally reach transitive tasks.
#[test]
fn dependency_output_inputs_hash_only_matching_artifacts() {
    for (through_middle, transitive) in [(false, false), (true, false), (true, true)] {
        let producer = target(
            "generate-types",
            json!({"outputs":["{projectRoot}/types"],"inputs":["{projectRoot}/src/**/*"]}),
        );
        let consumer = target(
            "build",
            json!({"inputs":[{"dependentTasksOutputFiles":"**/*.d.ts","transitive":transitive}],
            "dependsOn":[if through_middle {"middle"} else {"types"}]}),
        );
        let fixture = Fixture::with_targets(json!({"types":producer,
            "middle":{"executor":"nx:noop","cache":true,"inputs":[],"outputs":[],"dependsOn":["types"]},
            "build":consumer}));
        fs::write(
            fixture.root.join("src/input.txt"),
            "declaration\nimplementation one",
        )
        .unwrap();
        success(fixture.build(&fixture.root, &[]));
        fs::write(
            fixture.root.join("src/input.txt"),
            "declaration\nimplementation two",
        )
        .unwrap();
        let output = success(fixture.build(&fixture.root, &[]));
        assert!(
            stderr(&output).contains("qk: cache hit app:build"),
            "{}",
            stderr(&output)
        );
        fs::write(
            fixture.root.join("src/input.txt"),
            "new declaration\nimplementation two",
        )
        .unwrap();
        let output = success(fixture.build(&fixture.root, &[]));
        let expected = if through_middle && !transitive {
            "qk: cache hit app:build"
        } else {
            "qk: cache miss app:build"
        };
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
    }
}

/// Broad artifact globs include directories, which must not disable caching.
#[test]
fn dependency_output_glob_skips_directories() {
    let producer = target(
        "generate-types",
        json!({"outputs":["{projectRoot}/types"],"inputs":["{projectRoot}/src/**/*"]}),
    );
    let consumer = target(
        "build",
        json!({"inputs":[{"dependentTasksOutputFiles":"**/*"}],"dependsOn":["types"]}),
    );
    let fixture = Fixture::with_targets(json!({"types":producer,"build":consumer}));
    success(fixture.build(&fixture.root, &[]));
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache hit app:build"),
        "{}",
        stderr(&output)
    );
    fs::write(fixture.root.join("src/input.txt"), "changed").unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache miss app:build"),
        "{}",
        stderr(&output)
    );
}

/// Nx ignore rules exclude tracked inputs and support negated file entries.
#[test]
fn nxignore_filters_cache_inputs_and_reloads_rules() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs":["{projectRoot}/src/**/*"]}),
    ));
    fs::create_dir_all(fixture.root.join("src/ignored")).unwrap();
    fs::write(fixture.root.join("src/ignored/skip.txt"), "one").unwrap();
    fs::write(fixture.root.join("src/ignored/keep.txt"), "one").unwrap();
    fs::write(
        fixture.root.join(".nxignore"),
        "src/ignored/**\n!src/ignored/keep.txt\n",
    )
    .unwrap();
    fixture.git(&fixture.root, &["add", "src/ignored"]);
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("src/ignored/skip.txt"), "two").unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache hit app:build"),
        "{}",
        stderr(&output)
    );
    fs::write(fixture.root.join("src/ignored/keep.txt"), "two").unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache miss app:build"),
        "{}",
        stderr(&output)
    );
    fs::write(fixture.root.join(".nxignore"), "").unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache miss app:build"),
        "{}",
        stderr(&output)
    );
}

/// A negated file cannot reinclude itself below an excluded parent directory.
#[test]
fn nxignore_excluded_parent_wins_over_tracked_file_negation() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs":["{projectRoot}/src/**/*"]}),
    ));
    fs::create_dir_all(fixture.root.join("src/ignored")).unwrap();
    fs::write(fixture.root.join("src/ignored/keep.txt"), "one").unwrap();
    fixture.git(&fixture.root, &["add", "src/ignored"]);
    fs::write(
        fixture.root.join(".nxignore"),
        "src/ignored/\n!src/ignored/keep.txt\n",
    )
    .unwrap();
    let ignore = qk_cache::SourceIgnore::new(&fixture.root).unwrap();
    assert!(ignore.matches("src/ignored/keep.txt"));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("src/ignored/keep.txt"), "two").unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache hit app:build"),
        "{}",
        stderr(&output)
    );
}

/// Nx negations can bring untracked Git-ignored source files into discovery.
#[test]
fn nxignore_can_reinclude_untracked_gitignored_files() {
    let fixture = Fixture::new(target(
        "build",
        json!({"inputs":["{projectRoot}/src/**/*"]}),
    ));
    fs::write(fixture.root.join(".gitignore"), "dist/\n.qk/\nsrc/*.txt\n").unwrap();
    fs::write(fixture.root.join(".nxignore"), "!src/keep.txt\n").unwrap();
    fs::write(fixture.root.join("src/keep.txt"), "one").unwrap();
    let files = qk_cache::source_files(&fixture.root).unwrap();
    assert!(files.contains("src/keep.txt"));
    success(fixture.build(&fixture.root, &[]));
    fs::write(fixture.root.join("src/keep.txt"), "two").unwrap();
    let output = success(fixture.build(&fixture.root, &[]));
    assert!(
        stderr(&output).contains("qk: cache miss app:build"),
        "{}",
        stderr(&output)
    );
}

/// Warm environment overrides participate in keys for both save modes.
#[cfg(unix)]
#[test]
fn warm_environment_keys_use_the_final_execution_environment() {
    for save in ["wait", "background"] {
        let keyed = |value: &str| {
            let mut target = scratch_target(json!({
                "paths": ["{projectRoot}/scratch"],
                "key": [{"env": "TOOLCHAIN"}],
                "env": {"TOOLCHAIN": value}, "save": save
            }));
            target["options"] = json!({"env": {"TOOLCHAIN": "base"}});
            target
        };
        let fixture = Fixture::new(keyed("1"));
        assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
        fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
        set_targets(&fixture, json!({"build": keyed("2")}));
        assert_eq!(said(&fixture, &fixture.root, &[]), "cold");
        fs::remove_dir_all(fixture.root.join("scratch")).unwrap();
        assert_eq!(said(&fixture, &fixture.root, &[]), "repo");
    }
}
