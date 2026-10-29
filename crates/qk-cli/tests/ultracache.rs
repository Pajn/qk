//! `qk:ultracache`: tasks keyed by what they read when recorded. Recording
//! needs strace, so these run on Linux, and are skipped without strace; see
//! `tools/ultracache/podman.sh` to run them from another system.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

#[path = "support/git.rs"]
mod git_support;
use git_support::fixture_command;

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
            fixture_command("git")
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
    // An exclusion in a directory it lists keeps its declared inputs from
    // covering what it read, so its recordings alone key it.
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(
            r#"["{projectRoot}/**/*""#,
            r#"["{projectRoot}/**/*", "!{projectRoot}/icons/unused.svg""#,
        ),
    );
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
    // Turned off, its recordings are not read. It keeps the declared key it
    // had before, since what a recording keys is no part of it.
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(r#""ignoreReads""#, r#""mode": "off", "ignoreReads""#),
    );
    assert_eq!(build_keyed(root, true), ("local-hit".into(), "none".into()));
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

/// A task run without recording, after a change its recordings missed, keeps
/// its result under its declared key alone. When its declared inputs cover
/// everything its recordings say it read, looked for and listed, the next
/// run takes that result by its declared key; otherwise it runs again.
#[test]
fn a_declared_result_is_taken_when_the_declared_inputs_cover_what_the_task_read() {
    if !has_strace() {
        return;
    }
    for (inputs, rerun) in [
        // Everything the task reads, finds, looks for and lists is in it.
        (r#"["{projectRoot}/**/*""#, ("local-hit", "declared")),
        // It reads icons/ as well, and lists the project's directory.
        (r#"["{projectRoot}/src/**/*""#, ("miss", "missed")),
    ] {
        let temp = fixture();
        let root = temp.path();
        let nx = fs::read_to_string(root.join("nx.json")).unwrap();
        write(
            root,
            "nx.json",
            &nx.replace(r#"["{projectRoot}/**/*""#, inputs),
        );
        assert_eq!(build_keyed(root, true), ("miss".into(), "saved".into()));
        write(root, "app/src/main.txt", "changed");
        assert_eq!(
            build_keyed(root, false),
            ("miss".into(), "missed".into()),
            "{inputs}"
        );
        assert_eq!(
            build_keyed(root, false),
            (rerun.0.into(), rerun.1.into()),
            "{inputs}"
        );
    }
}

/// Coverage is judged by the declared inputs a lookup runs with. A named
/// input reaches no key's settings, so narrowing one keeps a task's weak key
/// and its path sets, and the narrower declaration no longer covers them.
#[test]
fn a_narrowed_named_input_no_longer_covers_what_was_recorded() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    let named = |files: &str| {
        nx.replace(r#"["{projectRoot}/**/*""#, r#"["files""#)
            .replacen(
                "{",
                &format!(r#"{{"namedInputs": {{"files": ["{files}"]}},"#),
                1,
            )
    };
    write(root, "nx.json", &named("{projectRoot}/**/*"));
    assert_eq!(build_keyed(root, true), ("miss".into(), "saved".into()));
    write(root, "nx.json", &named("{projectRoot}/src/**/*"));
    write(root, "app/src/main.txt", "changed");
    assert_eq!(build_keyed(root, false), ("miss".into(), "missed".into()));
    assert_eq!(build_keyed(root, false), ("miss".into(), "missed".into()));
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
fn a_task_in_mode_record_checks_the_result_a_recording_would_take_and_names_what_changed() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    // Declared inputs that leave out a file the task reads.
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(r#""ignoreReads""#, r#""mode": "record", "ignoreReads""#)
            .replace(
                r#""{projectRoot}/**/*","#,
                r#""{projectRoot}/**/*", "!{projectRoot}/src/util.txt","#,
            ),
    );
    let task = build_report(root, true, &[]);
    assert_eq!(kind(&task["recorded"]), "saved");
    // A file it never read: its declared key misses, and it gives the
    // result its recording would have taken.
    write(root, "app/README.md", "# app\n\nStill not read.\n");
    let task = build_report(root, false, &[]);
    assert_eq!(task["cache"], "miss");
    assert_eq!(kind(&task["shadow"]), "wouldHit");
    assert_eq!(task["shadow"]["check"], json!({"matched": true}));
    // A file its recording ignores: another result, told apart from the
    // recorded one even when this run records under the same key.
    write(root, "app/src/notes.txt", "other notes\n");
    let task = build_report(root, true, &[]);
    assert_eq!(task["cache"], "miss");
    assert_eq!(
        task["shadow"]["check"],
        json!({"matched": false, "differences": ["app/dist/out.txt (differs)"]})
    );
    // A file it read that its declared inputs leave out: they hit, and its
    // recording, which misses, names the file as what changed.
    write(root, "app/src/util.txt", "changed\n");
    let task = build_report(root, false, &[]);
    assert_eq!(task["cache"], "local-hit");
    assert_eq!(
        task["shadow"],
        json!({"kind": "wouldMiss", "changed": ["app/src/util.txt (read)"]})
    );
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

/// In mode "record", the path sets the store holds are fetched while the
/// task's declared result is, rather than before it, so that a cache hit
/// waits on the store once: the declared result is asked for while the store
/// still holds back the path sets.
#[test]
fn mode_record_fetches_shared_recordings_alongside_the_declared_result() {
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
    let task = build_report(root, true, &pairs(&a));
    assert_eq!(kind(&task["recorded"]), "saved");
    let entry = format!("/entries/{}", task["key"].as_str().unwrap());
    {
        let mut state = server.state.lock().unwrap();
        state.hold_get_containing.insert("/pathsets/".into());
        state.requests.clear();
    }
    // Released once the declared result is asked for, or after a while
    // without it, so that a run that waits for the path sets still ends.
    let watching = {
        let state = server.state.clone();
        let entry = entry.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let asked = loop {
                let asked = state
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .any(|request| request.starts_with("GET ") && request.contains(&entry));
                if asked || std::time::Instant::now() > deadline {
                    break asked;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            state.lock().unwrap().hold_get_containing.clear();
            asked
        })
    };
    let task = build_report(root, false, &pairs(&b));
    assert!(
        watching.join().unwrap(),
        "the declared result was not asked for while the path sets were held: {:?}",
        server.state.lock().unwrap().requests
    );
    assert_eq!(
        (task["cache"].as_str(), kind(&task["shadow"]).as_str()),
        (Some("remote-hit"), "wouldHit")
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

/// `--record --sandbox`, or `QK_RECORD=audit`, audits each task the cache
/// records from its recording, the cache staying on: a task that runs is
/// audited, one restored from the cache is not.
#[test]
fn a_recorded_run_audits_the_tasks_it_records() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    // And a workspace package linked into node_modules, read where it leads.
    fs::create_dir_all(root.join("libs/lib")).unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    write(root, "libs/lib/notes.txt", "notes\n");
    std::os::unix::fs::symlink("../libs/lib", root.join("node_modules/lib")).unwrap();
    let project = fs::read_to_string(root.join("app/project.json")).unwrap();
    write(
        root,
        "app/project.json",
        &project.replace(
            "mkdir -p dist && ",
            "mkdir -p dist && cat ../README.md ../node_modules/lib/notes.txt > /dev/null && ",
        ),
    );
    let report = root.join(".git/report.json");
    let args = ["run", "app:build", "--report", report.to_str().unwrap()];
    let first = qk(root, &[&args[..], &["--record", "--sandbox"]].concat(), &[]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        stderr.contains("Sandbox audit: 1 of 1 tasks went beyond what they declare"),
        "{stderr}"
    );
    let run: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let task = &run["tasks"][0];
    assert_eq!(task["cache"], "miss");
    assert_eq!(kind(&task["recorded"]), "saved");
    assert_eq!(
        task["audit"],
        json!({"undeclaredReads": ["README.md", "libs/lib/notes.txt"]})
    );
    let second = qk(root, &args, &[("QK_RECORD", "audit")]);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains(
            "Sandbox audit: no task ran\n  1 task restored from the cache, so not audited"
        ),
        "{stderr}"
    );
    let run: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    let task = &run["tasks"][0];
    assert_eq!(task["cache"], "local-hit");
    assert!(task.get("audit").is_none(), "{task}");
}

/// `--sandbox` records a task to audit it, but only `--record` keys results
/// by what a recording read: an audit is no decision to.
#[test]
fn a_sandbox_audit_records_nothing_for_the_cache_without_record() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    let report = root.join(".git/report.json");
    let task = |extra: &[&str]| {
        let args = ["run", "app:build", "--report", report.to_str().unwrap()];
        qk(root, &[&args[..], extra].concat(), &[]);
        let run: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
        run["tasks"][0].clone()
    };
    let audited = task(&["--sandbox"]);
    assert_eq!(audited["cache"], "miss");
    assert_eq!(kind(&audited["recorded"]), "unrecorded");
    assert_eq!(audited["audit"], json!({}));
    // Changed, so it runs again, recorded this time.
    write(root, "app/src/main.txt", "changed\n");
    let recorded = task(&["--sandbox", "--record"]);
    assert_eq!(recorded["cache"], "miss");
    assert_eq!(kind(&recorded["recorded"]), "saved");
}

/// Where no recorder works, a recording run records nothing, says why once,
/// and goes on: a recording only keys what a later run may take. An audit of
/// what is recorded goes on unaudited the same way. A recorder named that
/// cannot record, or one named wrong, is a mistake, and fails the run.
#[test]
fn a_run_where_no_recorder_works_goes_on_unrecorded() {
    let temp = fixture();
    let root = temp.path();
    // The tools the build and qk run, and neither strace nor sudo, which the
    // eBPF recorder's helper needs unless qk runs as root.
    let bin = TempDir::new().unwrap();
    for tool in ["git", "mkdir", "cat", "ls"] {
        let found = Command::new("/bin/sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .unwrap();
        let path = String::from_utf8(found.stdout).unwrap();
        std::os::unix::fs::symlink(path.trim(), bin.path().join(tool)).unwrap();
    }
    let path = bin.path().to_str().unwrap();
    let report = root.join(".git/report.json");
    let args = ["run", "app:build", "--report", report.to_str().unwrap()];
    let named = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .arg("--workspace")
        .arg(root)
        .args(args)
        .env_remove("QK_RECORD")
        .env_remove("CI")
        .envs([
            ("QK_RECORD", "true"),
            ("QK_RECORDER", "strace"),
            ("PATH", path),
        ])
        .output()
        .unwrap();
    assert!(!named.status.success());
    let stderr = String::from_utf8_lossy(&named.stderr);
    assert!(stderr.contains("recording needs strace"), "{stderr}");
    let misnamed = Command::new(env!("CARGO_BIN_EXE_qk"))
        .current_dir(root)
        .arg("--workspace")
        .arg(root)
        .args(["run", "app:build", "--record"])
        .env("QK_RECORDER", "strase")
        .output()
        .unwrap();
    assert!(!misnamed.status.success());
    assert!(
        String::from_utf8_lossy(&misnamed.stderr).contains("unknown QK_RECORDER \"strase\""),
        "{}",
        String::from_utf8_lossy(&misnamed.stderr)
    );
    // As root, the eBPF helper needs no sudo, so `auto` may still record.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let output = qk(
        root,
        &args,
        &[
            ("QK_RECORD", "true"),
            ("QK_RECORDER", "auto"),
            ("PATH", path),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--record records nothing, as no recorder works here"),
        "{stderr}"
    );
    let run: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(run["tasks"][0]["cache"], "miss");
    write(root, "app/src/main.txt", "changed\n");
    let output = qk(
        root,
        &args,
        &[
            ("QK_RECORD", "audit"),
            ("QK_RECORDER", "auto"),
            ("PATH", path),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing is audited, as nothing can be recorded here"),
        "{stderr}"
    );
}

/// A cold run replaces the result its recordings key too: a later run looks
/// them up first, and would restore what the cold run was to replace.
#[test]
fn a_cold_run_replaces_the_result_its_recordings_key() {
    if !has_strace() {
        return;
    }
    let temp = fixture();
    let root = temp.path();
    // Keyed by its recordings alone, as in the first test.
    let nx = fs::read_to_string(root.join("nx.json")).unwrap();
    write(
        root,
        "nx.json",
        &nx.replace(
            r#"["{projectRoot}/**/*""#,
            r#"["{projectRoot}/**/*", "!{projectRoot}/icons/unused.svg""#,
        ),
    );
    assert_eq!(build_keyed(root, true), ("miss".into(), "saved".into()));
    // An ignored read, so the recordings key it as before.
    write(root, "app/src/notes.txt", "replaced notes\n");
    qk(root, &["run", "app:build", "--cold"], &[]);
    assert_eq!(build_keyed(root, false), ("local-hit".into(), "hit".into()));
    let out = fs::read_to_string(root.join("app/dist/out.txt")).unwrap();
    assert!(out.contains("replaced notes"), "{out}");
}
