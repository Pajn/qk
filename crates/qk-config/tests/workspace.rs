use std::path::Path;

use qk_config::{Workspace, find_workspace};
use serde_json::json;
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn example_discovers_nested_projects_and_merges_script_targets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/basic");
    let workspace = Workspace::load(&root).unwrap();
    assert_eq!(
        workspace
            .projects
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["codegen", "core", "web", "worker"]
    );
    let web = &workspace.projects["web"];
    assert_eq!(web.root, "apps/web");
    assert_eq!(workspace.projects["worker"].root, "apps/web/worker");
    assert_eq!(
        web.targets["build"].executor.as_deref(),
        Some("nx:run-commands")
    );
    assert_eq!(web.targets["build"].cache, Some(true));
    assert_eq!(web.targets["build"].options["cwd"], "{projectRoot}");
    assert!(!web.targets["build"].options.contains_key("script"));
    assert_eq!(
        web.targets["test"].executor.as_deref(),
        Some("nx:run-script")
    );
    assert!(!web.targets.contains_key("prepare"));
    assert_eq!(
        workspace.projects["codegen"].targets["build"].cache,
        Some(false)
    );
    assert_eq!(
        find_workspace(&root.join("apps/web/worker")).unwrap(),
        root.canonicalize().unwrap()
    );
}

#[test]
fn merges_jsonc_defaults_and_replaces_arrays_and_nested_option_values() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{
      // JSONC comments and trailing commas are intentional.
      "namedInputs": {"default": ["global"], "production": ["default"]},
      "targetDefaults": {"build": {
        "inputs": ["default"], "outputs": ["old"],
        "options": {"cwd": "root", "env": {"OLD": "1"}},
        "configurations": {"prod": {"command": "old", "env": {"OLD": "1"}}, "dev": {"command": "dev"}},
      }},
    }"#,
    );
    write(
        temp.path(),
        "app/project.json",
        r#"{
      "//": "a comment key",
      "name": "app", "namedInputs": {"default": ["local"]},
      "targets": {"//": "another comment", "build": {
        "executor": "nx:run-commands", "inputs": [], "outputs": ["new"],
        "options": {"env": {"NEW": "2"}},
        "configurations": {"prod": {"command": "new"}}
      }}
    }"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let project = &workspace.projects["app"];
    let target = &project.targets["build"];
    assert_eq!(project.named_inputs["default"], vec![json!("local")]);
    assert_eq!(project.named_inputs["production"], vec![json!("default")]);
    assert_eq!(target.inputs, Some(vec![]));
    assert_eq!(target.outputs, Some(vec!["new".into()]));
    assert_eq!(target.options["cwd"], "root");
    assert_eq!(target.options["env"], json!({"NEW": "2"}));
    assert_eq!(target.configurations["prod"]["command"], "new");
    assert_eq!(target.configurations["prod"]["env"], json!({"OLD": "1"}));
    assert_eq!(target.configurations["dev"]["command"], "dev");
}

#[test]
fn executor_defaults_win_over_target_name_defaults() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"targetDefaults": {
      "build": {"cache": true, "outputs": ["name-default"]},
      "nx:run-commands": {"cache": false, "options": {"cwd": "executor-default"}}
    }}"#,
    );
    write(
        temp.path(),
        "project.json",
        r#"{"name":"app","targets":{"build":{"command":"echo hello"}}}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let target = &workspace.projects["app"].targets["build"];
    assert_eq!(target.cache, Some(false));
    assert_eq!(target.outputs, None);
    assert_eq!(target.options["cwd"], "executor-default");
}

#[test]
fn project_command_overrides_command_shorthand_in_defaults() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"targetDefaults":{"build":{"command":"echo default"}}}"#,
    );
    write(
        temp.path(),
        "project.json",
        r#"{"name":"app","targets":{"build":{"command":"echo project"}}}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(
        workspace.projects["app"].targets["build"].options["command"],
        "echo project"
    );
}

#[test]
fn ignores_generated_and_gitignored_directories_without_requiring_git() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "nx.json", "{}");
    write(temp.path(), ".gitignore", "ignored/\n");
    for directory in [
        "node_modules/foo",
        "target",
        ".git",
        ".nx",
        ".qk",
        "ignored",
    ] {
        write(
            temp.path(),
            &format!("{directory}/project.json"),
            "invalid JSON",
        );
    }
    write(temp.path(), ".hidden/project.json", r#"{"name":"visible"}"#);
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(
        workspace.projects.keys().collect::<Vec<_>>(),
        vec!["visible"]
    );
}

#[test]
fn supports_package_workspaces_and_root_project_opt_in() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "package.json",
        r#"{
      "name":"root", "workspaces":{"packages":["packages/*"]},
      "scripts":{"check":"echo check"}, "nx":{"includedScripts":[]}
    }"#,
    );
    write(
        temp.path(),
        "packages/a/package.json",
        r#"{"name":"a","scripts":{"test":"echo test"}}"#,
    );
    write(
        temp.path(),
        "unlisted/package.json",
        r#"{"name":"unlisted"}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(workspace.projects.len(), 2);
    assert_eq!(workspace.projects["root"].root, ".");
    assert!(workspace.projects["root"].targets.is_empty());
    assert!(workspace.projects["a"].targets.contains_key("test"));
    assert_eq!(
        find_workspace(&temp.path().join("packages/a")).unwrap(),
        temp.path().canonicalize().unwrap()
    );
}

#[test]
fn rejects_duplicate_names_with_both_roots() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "nx.json", "{}");
    for directory in ["a", "b"] {
        write(
            temp.path(),
            &format!("{directory}/project.json"),
            r#"{"name":"same"}"#,
        );
    }
    let error = Workspace::load(temp.path()).unwrap_err().to_string();
    assert!(
        error.contains("duplicate project name") && error.contains("a and b"),
        "{error}"
    );
}

#[test]
fn malformed_config_reports_the_file() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "nx.json", "{}");
    write(temp.path(), "bad/project.json", "{broken}");
    let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
    assert!(error.contains("project.json"), "{error}");
}

#[test]
fn included_scripts_can_declare_targets_without_package_scripts() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "package.json",
        r#"{"name":"root","nx":{"includedScripts":["missing"]}}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let target = &workspace.projects["root"].targets["missing"];
    assert_eq!(target.executor.as_deref(), Some("nx:run-script"));
    assert_eq!(target.options["script"], "missing");
}

#[test]
fn explicit_targets_replace_missing_included_scripts() {
    for source in ["package.json", "project.json"] {
        let temp = TempDir::new().unwrap();
        let targets = json!({
            "build": {"executor": "nx:noop", "dependsOn": ["^build"]},
            "start": {"command": "echo start", "continuous": true}
        });
        let mut package = json!({
            "name": "desktop",
            "scripts": {"check": "echo check", "unlisted": "echo unlisted"},
            "nx": {"includedScripts": ["build", "start", "check"]}
        });
        if source == "package.json" {
            package["nx"]["targets"] = targets;
        } else {
            write(
                temp.path(),
                "project.json",
                &json!({"name": "desktop", "targets": targets}).to_string(),
            );
        }
        write(temp.path(), "package.json", &package.to_string());
        let workspace = Workspace::load(temp.path()).unwrap();
        let project = &workspace.projects["desktop"];
        assert_eq!(project.targets.len(), 3, "{source}");
        assert_eq!(
            project.targets["build"].executor.as_deref(),
            Some("nx:noop")
        );
        assert_eq!(
            project.targets["build"].depends_on,
            Some(vec![json!("^build")])
        );
        assert!(project.targets["build"].options.is_empty());
        assert_eq!(
            project.targets["start"].executor.as_deref(),
            Some("nx:run-commands")
        );
        assert_eq!(project.targets["start"].options["command"], "echo start");
        assert!(!project.targets["start"].options.contains_key("script"));
        assert_eq!(project.targets["start"].continuous, Some(true));
        assert_eq!(project.targets["check"].options["script"], "check");
    }
}

#[test]
fn rejects_root_outside_configuration_directory() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "project.json",
        r#"{"name":"bad","root":"../outside"}"#,
    );
    let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
    assert!(
        error.contains("does not match configuration directory"),
        "{error}"
    );
}

#[test]
fn rejects_invalid_shapes_and_unsupported_filtered_defaults() {
    for (file, text, expected) in [
        ("project.json", "[]", "JSON object"),
        (
            "nx.json",
            r#"{"targetDefaults":{"build":[]}}"#,
            "filtered defaults",
        ),
        (
            "project.json",
            r#"{"name":"bad","targets":{"build":{"options":[]}}}"#,
            "invalid target",
        ),
    ] {
        let temp = TempDir::new().unwrap();
        write(temp.path(), file, text);
        let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn nxignore_excludes_projects_with_gitignore_semantics() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "nx.json", "{}");
    write(temp.path(), ".nxignore", "docs/\n!docs/kept/\n");
    write(temp.path(), "docs/slides/project.json", "invalid JSON");
    write(temp.path(), "docs/kept/project.json", r#"{"name":"kept"}"#);
    write(temp.path(), "apps/app/project.json", r#"{"name":"app"}"#);
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(workspace.projects.keys().collect::<Vec<_>>(), vec!["app"]);
}
