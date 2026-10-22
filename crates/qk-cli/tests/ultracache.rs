//! `qk:ultracache`: tasks keyed by what they read when recorded. Recording
//! needs strace, so these run on Linux, and are skipped without strace; see
//! `tools/ultracache/podman.sh` to run them from another system.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// `tools/ultracache/fixture`, copied into a git repository of its own.
fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/ultracache/fixture");
    for entry in walkdir::WalkDir::new(&source) {
        let entry = entry.unwrap();
        let target = temp
            .path()
            .join(entry.path().strip_prefix(&source).unwrap());
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).unwrap();
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
    for args in [
        &["init", "--quiet"][..],
        &["add", "--all"],
        &[
            "-c",
            "user.name=qk",
            "-c",
            "user.email=qk@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", temp.path())
                .status()
                .unwrap()
                .success()
        );
    }
    temp
}

fn has_strace() -> bool {
    let found = Command::new("strace").arg("-V").output().is_ok();
    if !found {
        eprintln!("skipped: recording needs strace");
    }
    found
}

fn qk(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .arg("--workspace")
        .arg(root)
        .args(args)
        .env_remove("NX_CACHE_DIRECTORY")
        .env_remove("QK_RECORD")
        .env_remove("CI")
        // Only a test's own credentials reach the store.
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "qk {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs `app:build`, recording it or not, and returns its report.
fn build_report(root: &Path, record: bool, env: &[(&str, &str)]) -> Value {
    let report = root.join(".git/report.json");
    let mut args = vec!["run", "app:build", "--report", report.to_str().unwrap()];
    if record {
        args.push("--record");
    }
    qk(root, &args, env);
    let run: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    run["tasks"][0].clone()
}

fn kind(value: &Value) -> String {
    value["kind"].as_str().unwrap_or("none").to_owned()
}

/// Runs `app:build`, recording it or not, and says where its result came
/// from, `miss` or `local-hit`, and how its recordings keyed it.
fn build_keyed(root: &Path, record: bool) -> (String, String) {
    let task = build_report(root, record, &[]);
    (
        task["cache"].as_str().unwrap().to_owned(),
        kind(&task["recorded"]),
    )
}

fn build(root: &Path, record: bool) -> String {
    build_keyed(root, record).0
}

fn write(root: &Path, path: &str, text: &str) {
    fs::write(root.join(path), text).unwrap();
}

#[test]
fn a_recorded_task_is_keyed_by_what_it_read_listed_and_looked_for() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    assert_eq!(build_keyed(root, true), ("miss".into(), "saved".into()));
    assert_eq!(build_keyed(root, false), ("local-hit".into(), "hit".into()));
    // Its declared inputs take in the whole project; what it read does not.
    write(root, "app/README.md", "# app\n\nStill not read.\n");
    assert_eq!(build(root, false), "local-hit");
    // A file it reads, ignoreReads aside.
    write(root, "app/src/notes.txt", "other notes\n");
    assert_eq!(build(root, false), "local-hit");
    write(root, "app/src/main.txt", "changed\n");
    assert_eq!(build_keyed(root, false), ("miss".into(), "missed".into()));
    assert_eq!(build(root, true), "miss");
    assert_eq!(build(root, false), "local-hit");
    // A new file in a directory it lists.
    write(root, "app/icons/c.svg", "<svg id=\"c\"/>\n");
    assert_eq!(build(root, false), "miss");
    assert_eq!(build(root, true), "miss");
    assert_eq!(build(root, false), "local-hit");
    // A file it only checked, as a tool comparing timestamps does.
    write(root, "app/src/checked.txt", "changed\n");
    assert_eq!(build(root, false), "miss");
    assert_eq!(build(root, true), "miss");
    assert_eq!(build(root, false), "local-hit");
    // A file where it looked for one and found none.
    write(root, "app/config.local", "local\n");
    assert_eq!(build(root, false), "miss");
    assert_eq!(build(root, true), "miss");
    assert_eq!(build(root, false), "local-hit");
    // An `always` input, which counts whether read or not.
    write(root, "app/always.txt", "changed\n");
    assert_eq!(build(root, false), "miss");
    // An earlier path set still matches the workspace it was recorded in.
    write(root, "app/always.txt", "always\n");
    assert_eq!(build(root, false), "local-hit");
}

#[test]
fn a_task_turned_off_or_never_recorded_keeps_its_declared_key() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    // Never recorded: keyed by its declared inputs, as without the field.
    assert_eq!(
        build_keyed(root, false),
        ("miss".into(), "unrecorded".into())
    );
    assert_eq!(
        build_keyed(root, false),
        ("local-hit".into(), "unrecorded".into())
    );
    write(root, "app/README.md", "edited\n");
    assert_eq!(build(root, false), "miss");
    // Turned off, its recordings are not read.
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(r#""ignoreReads""#, r#""mode": "off", "ignoreReads""#),
    );
    assert_eq!(build_keyed(root, true), ("miss".into(), "none".into()));
    write(root, "app/README.md", "edited again\n");
    assert_eq!(build(root, false), "miss");
}

#[test]
fn a_task_that_changes_what_it_read_is_not_recorded() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let project = fs::read_to_string(root.join("app/project.json")).unwrap();
    write(
        root,
        "app/project.json",
        &project.replace(
            " > dist/listing.txt",
            " > dist/listing.txt && touch src/util.txt",
        ),
    );
    // Its result is still kept under its declared key.
    assert_eq!(build_keyed(root, true), ("miss".into(), "failed".into()));
    assert_eq!(
        build_keyed(root, false),
        ("local-hit".into(), "unrecorded".into())
    );
}

#[path = "support/s3.rs"]
#[allow(dead_code)]
mod s3;

/// The fixture, sharing a remote store, and the environment of a machine of
/// its own: its cache directory and the store's credentials.
fn with_remote(root: &Path, server: &s3::FakeS3) -> impl Fn(&str) -> Vec<(String, String)> {
    let path = root.join("nx.json");
    let mut nx: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    nx["s3"] = serde_json::json!({
        "bucket": "cache", "region": "us-east-1",
        "endpoint": server.endpoint, "forcePathStyle": true
    });
    fs::write(&path, nx.to_string()).unwrap();
    let root = root.to_owned();
    move |machine| {
        vec![
            (
                "NX_CACHE_DIRECTORY".into(),
                root.join(".git").join(machine).to_str().unwrap().into(),
            ),
            ("AWS_ACCESS_KEY_ID".into(), "key".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]
    }
}

fn pairs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
}

#[test]
fn recordings_are_shared_through_the_remote_store() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let server = s3::FakeS3::start();
    let machine = with_remote(root, &server);
    let (a, b) = (machine("a"), machine("b"));
    let task = build_report(root, true, &pairs(&a));
    assert_eq!(
        (task["cache"].as_str(), kind(&task["recorded"]).as_str()),
        (Some("miss"), "saved")
    );
    // Another machine, with a file the task never read edited: its declared
    // key misses, and what it read is in the store.
    write(root, "app/README.md", "# app\n\nStill not read.\n");
    let task = build_report(root, false, &pairs(&b));
    assert_eq!(
        (task["cache"].as_str(), kind(&task["recorded"]).as_str()),
        (Some("remote-hit"), "hit")
    );
    // A read file edited: the shared path set keys it, and misses.
    write(root, "app/src/main.txt", "changed\n");
    let task = build_report(root, false, &pairs(&b));
    assert_eq!(
        (task["cache"].as_str(), kind(&task["recorded"]).as_str()),
        (Some("miss"), "missed")
    );
}

#[test]
fn a_task_in_mode_record_is_keyed_by_its_inputs_and_says_what_its_recordings_would_do() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(r#""ignoreReads""#, r#""mode": "record", "ignoreReads""#),
    );
    let shadow = |record: bool| {
        let task = build_report(root, record, &[]);
        (
            task["cache"].as_str().unwrap().to_owned(),
            kind(&task["recorded"]),
            kind(&task["shadow"]),
        )
    };
    let expect = |cache: &str, recorded: &str, shadow: &str| {
        (cache.to_owned(), recorded.to_owned(), shadow.to_owned())
    };
    assert_eq!(shadow(true), expect("miss", "saved", "unrecorded"));
    assert_eq!(shadow(false), expect("local-hit", "none", "wouldHit"));
    // Declared inputs miss what the recording would have hit.
    write(root, "app/README.md", "# app\n\nStill not read.\n");
    assert_eq!(shadow(false), expect("miss", "none", "wouldHit"));
    // And the recording misses where the declared key, now kept, hits.
    write(root, "app/src/main.txt", "changed\n");
    assert_eq!(shadow(false), expect("miss", "none", "wouldMiss"));
    assert_eq!(shadow(false), expect("local-hit", "none", "wouldMiss"));
}

#[test]
fn a_task_without_recordings_cached_locally_asks_the_store_for_none() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let server = s3::FakeS3::start();
    let machine = with_remote(root, &server);
    let a = machine("a");
    let task = build_report(root, false, &pairs(&a));
    assert_eq!(task["cache"].as_str(), Some("miss"));
    server.state.lock().unwrap().requests.clear();
    let task = build_report(root, false, &pairs(&a));
    assert_eq!(
        (task["cache"].as_str(), kind(&task["recorded"]).as_str()),
        (Some("local-hit"), "unrecorded")
    );
    let requests = server.state.lock().unwrap().requests.clone();
    assert!(
        !requests
            .iter()
            .any(|request| request.contains("/pathsets/")),
        "{requests:?}"
    );
}

#[test]
fn mode_record_looks_up_shared_recordings_behind_a_local_declared_hit() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(r#""ignoreReads""#, r#""mode": "record", "ignoreReads""#),
    );
    let server = s3::FakeS3::start();
    let machine = with_remote(root, &server);
    let (a, b) = (machine("a"), machine("b"));
    // `b` keeps its declared result before any recording exists, without
    // the store's credentials, which the helper clears from what tests
    // inherit, so that `a` has it to run rather than take.
    let local: Vec<(String, String)> = b
        .iter()
        .filter(|(name, _)| name == "NX_CACHE_DIRECTORY")
        .cloned()
        .collect();
    let task = build_report(root, false, &pairs(&local));
    assert_eq!(
        (task["cache"].as_str(), kind(&task["shadow"]).as_str()),
        (Some("miss"), "unrecorded")
    );
    // `a` records, from the cache it shares, and uploads what it read.
    let task = build_report(root, true, &pairs(&a));
    assert_eq!(kind(&task["recorded"]), "saved");
    // `b` hits its own declared result, and still finds `a`'s recording.
    let task = build_report(root, false, &pairs(&b));
    assert_eq!(
        (task["cache"].as_str(), kind(&task["shadow"]).as_str()),
        (Some("local-hit"), "wouldHit")
    );
}
