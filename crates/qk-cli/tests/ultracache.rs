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

fn qk(root: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .arg("--workspace")
        .arg(root)
        .args(args)
        .env_remove("NX_CACHE_DIRECTORY")
        .env_remove("QK_RECORD")
        .env_remove("CI")
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

/// Runs `app:build`, recording it or not, and says where its result came
/// from, `miss` or `local-hit`, and how its recordings keyed it.
fn build_keyed(root: &Path, record: bool) -> (String, String) {
    let report = root.join(".git/report.json");
    let mut args = vec!["run", "app:build", "--report", report.to_str().unwrap()];
    if record {
        args.push("--record");
    }
    qk(root, &args);
    let run: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let task = &run["tasks"][0];
    (
        task["cache"].as_str().unwrap().to_owned(),
        task["recorded"]["kind"]
            .as_str()
            .unwrap_or("none")
            .to_owned(),
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
