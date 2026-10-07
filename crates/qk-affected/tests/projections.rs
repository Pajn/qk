use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use qk_affected::{Analysis, Options, ProjectionFallbackKind, ProjectionFallbackPolicy, analyse};
use qk_config::Workspace;
use qk_graph::ProjectGraph;
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

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn commit(root: &Path) -> String {
    git(root, &["add", "--all"]);
    git(
        root,
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
    git(root, &["rev-parse", "HEAD"])
}

fn adapter() -> &'static Path {
    static ADAPTER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &ADAPTER.get_or_init(|| {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("adapter.rs");
        std::fs::write(&source, r#"
use std::{env, fs, io::{self, Read}, process};
fn main() {
    let mode = env::args().nth(1).unwrap();
    if mode == "background" {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().create(true).append(true).open(env::args().nth(2).unwrap()).unwrap();
        writeln!(file, "{}", process::id()).unwrap();
        loop { std::thread::park(); }
    }
    let mut request = String::new();
    io::stdin().read_to_string(&mut request).unwrap();
    assert!(request.contains("\"version\":1") && request.contains("revisionRoot") && request.contains("workspaceRoot"));
    if mode.ends_with("descendants") {
        let log = env::args().nth(2).unwrap();
        let child = process::Command::new(env::current_exe().unwrap()).args(["background", &log])
            .stdin(process::Stdio::null()).spawn().unwrap();
        while !fs::read_to_string(&log).unwrap_or_default().lines().any(|pid| pid == child.id().to_string()) {
            std::thread::yield_now();
        }
        if mode == "failure-descendants" { process::exit(7); }
        if mode == "timeout-descendants" { loop { std::thread::park(); } }
    }
    match mode.as_str() {
        "fail" => process::exit(7),
        "invalid" => { print!("not JSON"); return; },
        "escape" => { print!("{{\"version\":1,\"artifacts\":{{\"../escape\":\"x\"}}}}"); return; },
        "undeclared" => { print!("{{\"version\":1,\"artifacts\":{{\"other/path\":\"x\"}}}}"); return; },
        "duplicate" => { print!("{{\"version\":1,\"artifacts\":{{\"apps/app/generated/runtime.js\":\"x\",\"apps/app/generated/runtime.js\":\"y\"}}}}"); return; },
        "version" => { print!("{{\"version\":2,\"artifacts\":{{}}}}"); return; },
        "large" => { print!("{}", "x".repeat(1024*1024+1)); return; },
        "timeout" => loop { std::thread::park(); },
        _ => {}
    }
    let revision_root = env::args().last().unwrap();
    env::set_current_dir(revision_root).unwrap();
    fs::create_dir_all("apps/app/generated").unwrap();
    fs::write("apps/app/generated/scratch", "isolated").unwrap();
    let text = fs::read_to_string("schemas/schema.txt").unwrap();
    let artifacts: Vec<_> = text.lines().filter_map(|line| line.split_once('=')).filter(|(name, _)| *name != "type")
        .map(|(name, value)| format!("\"apps/{name}/generated/runtime.js\":\"{value}\"" )).collect();
    print!("{{\"version\":1,\"artifacts\":{{{}}}}}", artifacts.join(","));
}
"#).unwrap();
        let executable = temp.path().join(format!("adapter{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new("rustc").arg(&source).arg("-o").arg(&executable).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        (temp, executable)
    }).1
}

struct Repo {
    _temp: TempDir,
    root: PathBuf,
    base: String,
}
impl Repo {
    fn new(mode: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        // Worktree checkouts share the adapter's deadline, so only adapters
        // that hang get a short one: a slow checkout is a comparison fallback.
        let timeout = if mode.starts_with("timeout") { 5 } else { 60 };
        let rule = json!({"name":"generated-runtime", "command":[adapter(),mode,"{revisionRoot}"], "sources":["schemas/**"], "outputs":["apps/*/generated/**"], "timeoutSeconds":timeout});
        write(
            &root,
            "nx.json",
            &json!({"qk:affectedProfiles":{"runtime":{"projections":[rule]}}}).to_string(),
        );
        write(&root, "schemas/project.json", r#"{"name":"schema"}"#);
        for name in ["app", "other"] {
            write(
                &root,
                &format!("apps/{name}/project.json"),
                &json!({"name":name, "implicitDependencies":["schema"]}).to_string(),
            );
            write(&root, &format!("apps/{name}/source.txt"), "source");
        }
        write(
            &root,
            "apps/dependent/project.json",
            r#"{"name":"dependent","implicitDependencies":["app"]}"#,
        );
        write(
            &root,
            "schemas/schema.txt",
            "app=one\nother=two\ntype=before",
        );
        write(&root, ".gitignore", "apps/*/generated/\n");
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        let base = commit(&root);
        Self {
            _temp: temp,
            root,
            base,
        }
    }
    fn analyse(&self, head: Option<String>, profile: bool) -> Analysis {
        let workspace = Workspace::load(&self.root).unwrap();
        let graph = ProjectGraph::build(&workspace).unwrap();
        analyse(
            &workspace,
            &graph,
            &Options {
                base: Some(self.base.clone()),
                head,
                affected_profile: profile.then(|| "runtime".into()),
                ..Options::default()
            },
        )
        .unwrap()
    }
    fn changed(&self, text: &str) -> String {
        write(&self.root, "schemas/schema.txt", text);
        commit(&self.root)
    }
}

fn names(analysis: &Analysis) -> BTreeSet<&str> {
    analysis.projects.keys().map(String::as_str).collect()
}

#[test]
fn projection_removes_only_equivalent_source_changes_and_preserves_the_checkout() {
    let repo = Repo::new("ok");
    let head = repo.changed("app=one\nother=two\ntype=after");
    write(&repo.root, "dirty.txt", "keep this");
    let before = git(&repo.root, &["status", "--porcelain"]);
    let worktrees = git(&repo.root, &["worktree", "list", "--porcelain"]);
    let raw = repo.analyse(Some(head.clone()), false);
    assert_eq!(
        names(&raw),
        BTreeSet::from(["app", "dependent", "other", "schema"])
    );
    let projected = repo.analyse(Some(head), true);
    assert!(projected.projects.is_empty());
    assert_eq!(projected.projections[0].status, "applied");
    assert_eq!(projected.projections[0].sources, ["schemas/schema.txt"]);
    assert_eq!(git(&repo.root, &["status", "--porcelain"]), before);
    assert_eq!(
        git(&repo.root, &["worktree", "list", "--porcelain"]),
        worktrees
    );
    assert!(!repo.root.join("apps/app/generated/scratch").exists());
}

#[test]
fn artifact_changes_reach_consumers_before_dependency_propagation() {
    let repo = Repo::new("ok");
    let head = repo.changed("app=changed\nother=two\ntype=after");
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(names(&analysis), BTreeSet::from(["app", "dependent"]));
    assert_eq!(analysis.files, ["apps/app/generated/runtime.js"]);
    assert_eq!(analysis.projections[0].artifacts, analysis.files);
}

#[test]
fn artifact_additions_and_deletions_both_count() {
    for text in ["app=one\ntype=after", "app=one\nother=two\ndependent=new"] {
        let repo = Repo::new("ok");
        let head = repo.changed(text);
        let analysis = repo.analyse(Some(head), true);
        assert_eq!(analysis.projections[0].artifacts.len(), 1);
        assert!(!analysis.projects.is_empty());
    }
}

#[test]
fn unrelated_changes_are_retained_and_no_source_changes_skip_the_adapter() {
    let repo = Repo::new("fail");
    write(&repo.root, "apps/other/source.txt", "changed");
    let head = commit(&repo.root);
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(names(&analysis), BTreeSet::from(["other"]));
    assert_eq!(analysis.projections[0].status, "skipped");
    let repo = Repo::new("ok");
    write(&repo.root, "apps/other/source.txt", "changed");
    let head = repo.changed("app=one\nother=two\ntype=after");
    assert_eq!(
        names(&repo.analyse(Some(head), true)),
        BTreeSet::from(["other"])
    );
}

#[test]
fn adapter_failures_keep_all_original_changes_and_clean_up() {
    for mode in [
        "fail",
        "invalid",
        "escape",
        "undeclared",
        "version",
        "large",
        "timeout",
        "duplicate",
    ] {
        let repo = Repo::new(mode);
        let head = repo.changed("app=one\nother=two\ntype=after");
        let before = git(&repo.root, &["worktree", "list", "--porcelain"]);
        let analysis = repo.analyse(Some(head), true);
        assert_eq!(analysis.projections[0].status, "fallback", "{mode}");
        assert_eq!(
            analysis.projections[0].fallback_kind,
            Some(ProjectionFallbackKind::Adapter)
        );
        let workspace = Workspace::load(&repo.root).unwrap();
        let graph = ProjectGraph::build(&workspace).unwrap();
        let error = analyse(
            &workspace,
            &graph,
            &Options {
                base: Some(repo.base.clone()),
                head: Some(git(&repo.root, &["rev-parse", "HEAD"])),
                affected_profile: Some("runtime".into()),
                fail_on_projection_fallback: Some(ProjectionFallbackPolicy::Adapter),
                ..Options::default()
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("projection adapter failed"),
            "{mode}"
        );

        assert_eq!(analysis.files, ["schemas/schema.txt"]);
        assert_eq!(
            names(&analysis),
            BTreeSet::from(["app", "dependent", "other", "schema"])
        );
        assert_eq!(
            git(&repo.root, &["worktree", "list", "--porcelain"]),
            before
        );
    }
}

#[test]
fn unsupported_comparisons_and_changed_tool_metadata_remain_conservative() {
    let repo = Repo::new("ok");
    let _head = repo.changed("app=one\nother=two\ntype=after");
    assert_eq!(repo.analyse(None, true).projections[0].status, "fallback");
    write(&repo.root, "pnpm-lock.yaml", "new tool installation");
    let head = commit(&repo.root);
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(analysis.projections[0].status, "fallback");
    assert!(
        analysis.projections[0]
            .detail
            .as_ref()
            .unwrap()
            .contains("tool installation")
    );
}

#[test]
fn legacy_profile_key_remains_compatible() {
    let mut repo = Repo::new("ok");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
    let profiles = config
        .as_object_mut()
        .unwrap()
        .remove("qk:affectedProfiles")
        .unwrap();
    config["affectedProfiles"] = profiles;
    write(&repo.root, "nx.json", &config.to_string());
    repo.base = commit(&repo.root);
    let head = repo.changed("app=one\nother=two\ntype=after");
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(analysis.projections[0].status, "applied");
    assert!(analysis.projects.is_empty());
}

#[test]
fn canonical_and_legacy_profile_keys_cannot_coexist() {
    let repo = Repo::new("ok");
    let head = repo.changed("app=one\nother=two\ntype=after");
    let config: Value =
        serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
    // Even identical definitions are ambiguous; local overrides must not
    // accidentally leave both key spellings active in the merged configuration.
    write(
        &repo.root,
        "nx.local.json",
        &json!({"affectedProfiles":config["qk:affectedProfiles"]}).to_string(),
    );
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    let error = analyse(
        &workspace,
        &graph,
        &Options {
            base: Some(repo.base),
            head: Some(head),
            affected_profile: Some("runtime".into()),
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configure only qk:affectedProfiles"),
        "{error}"
    );
}

#[test]
fn invalid_profiles_are_configuration_errors() {
    let repo = Repo::new("ok");
    let head = repo.changed("app=one\nother=two\ntype=after");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
    config["qk:affectedProfiles"]["runtime"]["projections"][0]["sources"] = json!(["../outside"]);
    write(&repo.root, "nx.json", &config.to_string());
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    let options = Options {
        base: Some(repo.base),
        head: Some(head),
        affected_profile: Some("runtime".into()),
        ..Options::default()
    };
    assert!(
        analyse(&workspace, &graph, &options)
            .unwrap_err()
            .to_string()
            .contains("workspace-relative")
    );
}

#[test]
fn explicit_files_never_claim_to_have_revision_snapshots() {
    let repo = Repo::new("ok");
    let head = repo.changed("app=one\nother=two\ntype=after");
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    let analysis = analyse(
        &workspace,
        &graph,
        &Options {
            base: Some(repo.base),
            head: Some(head),
            files: vec!["schemas/schema.txt".into()],
            explicit_files: true,
            affected_profile: Some("runtime".into()),
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(analysis.projections[0].status, "fallback");
    assert!(analysis.projects.contains_key("schema"));
}

#[test]
fn dirty_workspace_metadata_cannot_change_the_projection_contract() {
    let repo = Repo::new("ok");
    let head = repo.changed("app=one\nother=two\ntype=after");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
    config["parallel"] = json!(4);
    write(&repo.root, "nx.json", &config.to_string());
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(analysis.projections[0].status, "fallback");
    assert!(
        analysis.projections[0]
            .detail
            .as_ref()
            .unwrap()
            .contains("differs from the head")
    );
}

#[test]
fn a_later_projection_failure_rolls_back_the_whole_profile() {
    let mut repo = Repo::new("ok");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
    config["qk:affectedProfiles"]["runtime"]["projections"].as_array_mut().unwrap().push(json!({
        "name":"second", "command":[adapter(),"fail","{revisionRoot}"], "sources":["second/**"], "outputs":["second/generated/**"]
    }));
    write(&repo.root, "nx.json", &config.to_string());
    write(&repo.root, "second/source.txt", "before");
    repo.base = commit(&repo.root);
    write(&repo.root, "second/source.txt", "after");
    let head = repo.changed("app=one\nother=two\ntype=after");
    let analysis = repo.analyse(Some(head), true);
    assert!(
        analysis
            .projections
            .iter()
            .all(|report| report.status == "fallback")
    );
    assert!(analysis.files.contains(&"schemas/schema.txt".into()));
    assert!(analysis.files.contains(&"second/source.txt".into()));
    assert!(analysis.projects.contains_key("schema"));
}

#[test]
fn revision_roots_preserve_nested_workspace_paths() {
    let mut repo = Repo::new("ok");
    let entries: Vec<_> = std::fs::read_dir(&repo.root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    let nested = repo.root.join("workspace");
    std::fs::create_dir(&nested).unwrap();
    for entry in entries {
        if entry.file_name() != ".git" {
            std::fs::rename(entry.path(), nested.join(entry.file_name())).unwrap();
        }
    }
    repo.root = nested;
    repo.base = commit(&repo.root);
    let head = repo.changed("app=changed\nother=two");
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(analysis.projections[0].status, "applied");
    assert_eq!(names(&analysis), BTreeSet::from(["app", "dependent"]));
}

#[cfg(unix)]
#[test]
fn adapter_descendants_are_stopped_on_success_failure_and_timeout() {
    for mode in ["descendants", "failure-descendants", "timeout-descendants"] {
        let mut repo = Repo::new(mode);
        let logs = tempfile::tempdir().unwrap();
        let log = logs.path().join("pids");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
        config["qk:affectedProfiles"]["runtime"]["projections"][0]["command"]
            .as_array_mut()
            .unwrap()
            .insert(2, json!(log));
        write(&repo.root, "nx.json", &config.to_string());
        repo.base = commit(&repo.root);
        let head = repo.changed("app=one\nother=two\ntype=after");
        let analysis = repo.analyse(Some(head), true);
        assert_eq!(
            analysis.projections[0].status,
            if mode == "descendants" {
                "applied"
            } else {
                "fallback"
            }
        );
        let pids = std::fs::read_to_string(&log).unwrap();
        assert!(!pids.is_empty());
        for pid in pids.lines() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while Command::new("/bin/kill")
                .args(["-0", pid])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "adapter left descendant {pid} running after {mode}"
                );
                std::thread::yield_now();
            }
        }
    }
}

#[test]
fn both_snapshots_use_committed_head_adapter_even_when_workspace_adapter_is_dirty() {
    let mut repo = Repo::new("ok");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(repo.root.join("nx.json")).unwrap()).unwrap();
    config["qk:affectedProfiles"]["runtime"]["projections"][0]["command"] =
        json!(["git", "show", "HEAD:adapter-manifest.json"]);
    write(&repo.root, "nx.json", &config.to_string());
    write(
        &repo.root,
        "adapter-manifest.json",
        &json!({"version":1,"artifacts":{"apps/app/generated/runtime.js":"old-adapter"}})
            .to_string(),
    );
    repo.base = commit(&repo.root);
    write(
        &repo.root,
        "adapter-manifest.json",
        &json!({"version":1,"artifacts":{"apps/app/generated/runtime.js":"head-adapter"}})
            .to_string(),
    );
    let head = repo.changed("app=one\nother=two\ntype=after");
    write(&repo.root, "adapter-manifest.json", "invalid dirty adapter");
    let analysis = repo.analyse(Some(head), true);
    assert_eq!(analysis.projections[0].status, "applied");
    assert!(analysis.projections[0].artifacts.is_empty());
    assert_eq!(analysis.files, ["adapter-manifest.json"]);
    assert_eq!(
        analysis.original_files.unwrap(),
        ["adapter-manifest.json", "schemas/schema.txt"]
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("adapter-manifest.json")).unwrap(),
        "invalid dirty adapter"
    );
}

#[test]
fn ignored_local_overrides_apply_to_both_snapshots_but_tracked_overrides_remain_protected() {
    let mut repo = Repo::new("ok");
    write(
        &repo.root,
        ".gitignore",
        "apps/*/generated/\nnx.local.json\n",
    );
    repo.base = commit(&repo.root);
    let head = repo.changed("app=one\nother=two\ntype=after");
    write(
        &repo.root,
        "nx.local.json",
        &json!({"qk:affectedProfiles":{"local":{"projections":[{
            "name":"local-runtime", "command":[adapter(),"ok","{revisionRoot}"],
            "sources":["schemas/**"], "outputs":["apps/*/generated/**"]
        }]}}})
        .to_string(),
    );
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    let result = analyse(
        &workspace,
        &graph,
        &Options {
            base: Some(repo.base.clone()),
            head: Some(head),
            affected_profile: Some("local".into()),
            fail_on_projection_fallback: Some(ProjectionFallbackPolicy::All),
            ..Options::default()
        },
    )
    .unwrap();
    assert!(result.files.is_empty());
    assert_eq!(result.projections[0].status, "applied");
    git(&repo.root, &["add", "--force", "nx.local.json"]);
    repo.base = commit(&repo.root);
    let head = repo.changed("app=one\nother=two\ntype=again");
    write(&repo.root, "nx.local.json", "{}");
    assert_eq!(
        repo.analyse(Some(head), true).projections[0].status,
        "fallback"
    );
}

#[test]
fn strict_fallback_reports_output_drift_as_an_error_and_cleans_worktrees() {
    let repo = Repo::new("undeclared");
    let head = repo.changed("app=one\nother=two\ntype=after");
    let before = git(&repo.root, &["worktree", "list", "--porcelain"]);
    let workspace = Workspace::load(&repo.root).unwrap();
    let graph = ProjectGraph::build(&workspace).unwrap();
    let error = analyse(
        &workspace,
        &graph,
        &Options {
            base: Some(repo.base.clone()),
            head: Some(head),
            affected_profile: Some("runtime".into()),
            fail_on_projection_fallback: Some(ProjectionFallbackPolicy::All),
            ..Options::default()
        },
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("affected profile \"runtime\" fell back"));
    assert!(message.contains("outside declared outputs"));
    assert_eq!(
        git(&repo.root, &["worktree", "list", "--porcelain"]),
        before
    );
}

#[test]
fn adapter_policy_allows_profile_introduction_and_installation_changes() {
    for path in ["nx.json", "package.json", "pnpm-lock.yaml"] {
        let mut repo = Repo::new("fail");
        let config = std::fs::read_to_string(repo.root.join("nx.json")).unwrap();
        if path == "nx.json" {
            write(&repo.root, "nx.json", "{}");
            repo.base = commit(&repo.root);
            write(&repo.root, "nx.json", &config);
        } else {
            write(
                &repo.root,
                path,
                if path == "package.json" {
                    r#"{"dependencies":{"example-tool":"1.0.0"}}"#
                } else {
                    "lockfileVersion: '9.0'\nimporters: {}\n"
                },
            );
        }
        let head = repo.changed("app=one\nother=two\ntype=after");
        let workspace = Workspace::load(&repo.root).unwrap();
        let graph = ProjectGraph::build(&workspace).unwrap();
        let mut options = Options {
            base: Some(repo.base.clone()),
            head: Some(head),
            affected_profile: Some("runtime".into()),
            fail_on_projection_fallback: Some(ProjectionFallbackPolicy::Adapter),
            ..Options::default()
        };
        let result = analyse(&workspace, &graph, &options).unwrap();
        assert_eq!(result.projections[0].status, "fallback", "{path}");
        assert_eq!(
            result.projections[0].fallback_kind,
            Some(ProjectionFallbackKind::Comparison)
        );
        assert!(result.files.contains(&"schemas/schema.txt".into()));
        assert!(result.projects.contains_key("app"));
        options.fail_on_projection_fallback = Some(ProjectionFallbackPolicy::All);
        assert!(analyse(&workspace, &graph, &options).is_err(), "{path}");
    }
}
