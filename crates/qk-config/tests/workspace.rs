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
fn rejects_invalid_shapes() {
    for (file, text, expected) in [
        ("project.json", "[]", "JSON object"),
        (
            "nx.json",
            r#"{"targetDefaults":{"build":"vite build"}}"#,
            "must be an object or an array of objects",
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

#[test]
fn skips_target_defaults_incompatible_with_the_target() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"targetDefaults": {
      "test": {"command": "vitest run", "cache": true},
      "lint": {"command": "oxlint", "cache": true},
      "build": {"executor": "nx:run-script", "options": {"script": "compile"}, "cache": true},
      "e2e": {"cache": true, "dependsOn": ["build"]}
    }}"#,
    );
    write(
        temp.path(),
        "package.json",
        r#"{"name": "app", "scripts": {"test": "vitest", "build": "tsc", "e2e": "playwright"}, "nx": {}}"#,
    );
    write(
        temp.path(),
        "project.json",
        r#"{"name": "app", "targets": {"test": {}, "lint": {"command": "eslint"}}}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let targets = &workspace.projects["app"].targets;
    // A run-commands default does not apply to a package script. Defaults for
    // the same executor apply even with a different command or script, and a
    // default without an executor applies to anything.
    assert_eq!(targets["test"].executor.as_deref(), Some("nx:run-script"));
    assert_eq!(targets["test"].cache, None);
    assert_eq!(targets["lint"].options["command"], "eslint");
    assert_eq!(targets["lint"].cache, Some(true));
    assert_eq!(targets["build"].options["script"], "build");
    assert_eq!(targets["build"].cache, Some(true));
    assert_eq!(targets["e2e"].cache, Some(true));
}

#[test]
fn local_overrides_merge_over_the_checked_in_project() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"targetDefaults": {"build": {"cache": true, "outputs": ["{projectRoot}/dist"]}}}"#,
    );
    write(
        temp.path(),
        "apps/app/project.json",
        r#"{"name": "app", "tags": ["web"], "targets": {
            "build": {"command": "vite build", "options": {"cwd": "{projectRoot}", "mode": "production"}},
            "test": {"command": "vitest"}
        }}"#,
    );
    write(
        temp.path(),
        "apps/app/project.local.json",
        r#"{
            // Comments are allowed, as in project.json.
            "tags": ["mine"],
            "targets": {
                "build": {"options": {"mode": "development"}},
                "profile": {"command": "vite build --profile"}
            }
        }"#,
    );
    write(
        temp.path(),
        "libs/lib/package.json",
        r#"{"name": "lib", "scripts": {"test": "vitest"}}"#,
    );
    write(
        temp.path(),
        "libs/lib/project.local.json",
        r#"{"targets": {"test": {"cache": false}}}"#,
    );
    write(temp.path(), "package.json", r#"{"workspaces": ["libs/*"]}"#);
    let workspace = Workspace::load(temp.path()).unwrap();
    let app = &workspace.projects["app"];
    assert_eq!(app.tags, ["web", "mine"]);
    let build = &app.targets["build"];
    assert_eq!(build.options["command"], "vite build");
    assert_eq!(build.options["mode"], "development");
    assert_eq!(build.options["cwd"], "{projectRoot}");
    // Target defaults still apply beneath both files.
    assert_eq!(build.cache, Some(true));
    assert!(app.targets.contains_key("test"));
    assert_eq!(
        app.targets["profile"].options["command"],
        "vite build --profile"
    );
    assert_eq!(workspace.projects["lib"].targets["test"].cache, Some(false));
    assert_eq!(
        workspace.local_overrides,
        ["apps/app/project.local.json", "libs/lib/project.local.json"]
    );
}

#[test]
fn local_overrides_cannot_rename_a_project() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "project.json", r#"{"name": "app"}"#);
    write(temp.path(), "project.local.json", r#"{"name": "mine"}"#);
    let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
    assert!(
        error.contains("project.local.json cannot set name or root"),
        "{error}"
    );
}

#[test]
fn a_local_workspace_file_merges_over_nx_json() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"defaultBase": "main", "parallel": 3,
            "namedInputs": {"default": ["{projectRoot}/**/*"], "production": ["default"]},
            "targetDefaults": {"build": {"cache": true, "options": {"mode": "production", "cwd": "{projectRoot}"}}}}"#,
    );
    write(
        temp.path(),
        "nx.local.json",
        r#"{"parallel": 8,
            "namedInputs": {"production": ["default", "!{projectRoot}/**/*.test.ts"]},
            "targetDefaults": {"build": {"options": {"mode": "development"}}, "lint": {"cache": false}}}"#,
    );
    write(
        temp.path(),
        "project.json",
        r#"{"name": "app", "targets": {"build": {"command": "vite build"}, "lint": {"command": "oxlint"}}}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(workspace.config.default_base.as_deref(), Some("main"));
    assert_eq!(workspace.config.extra["parallel"], 8);
    assert_eq!(
        workspace.config.named_inputs["default"],
        [json!("{projectRoot}/**/*")]
    );
    assert_eq!(workspace.config.named_inputs["production"].len(), 2);
    let app = &workspace.projects["app"];
    assert_eq!(app.targets["build"].cache, Some(true));
    assert_eq!(app.targets["build"].options["mode"], "development");
    assert_eq!(app.targets["build"].options["cwd"], "{projectRoot}");
    assert_eq!(app.targets["lint"].cache, Some(false));
    assert_eq!(workspace.local_overrides, ["nx.local.json"]);
}

#[test]
fn target_defaults_resolve_like_nx_23() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"targetDefaults": {
            "test-*": {"cache": true},
            "test-e2e*": {"options": {"shard": 1}},
            "build": [
                {"cache": true},
                {"filter": {"projects": ["tag:web"]}, "options": {"mode": "web"}},
                {"filter": {"projects": "lib"}, "options": {"mode": "lib"}},
                {"filter": {"executor": "nx:run-script"}, "options": {"script": "never"}}
            ],
            "lint": [{"filter": {"plugin": "nx/core/package-json"}, "cache": true}],
            "nx:run-commands": [{"filter": {"projects": "nothing"}, "cache": false}],
            "b*": {"outputs": ["dist"]},
            "deploy": [{"executor": "nx:run-script"}],
            "d*": {"cache": true}
        }}"#,
    );
    write(
        temp.path(),
        "apps/web/project.json",
        r#"{"name": "web", "tags": ["web"], "targets": {
            "build": {"command": "vite build"},
            "test-unit": {"command": "vitest"},
            "test-e2e-ci": {"command": "playwright test"},
            "deploy": {"command": "wrangler deploy"},
            "lint": {"command": "oxlint"}
        }}"#,
    );
    write(
        temp.path(),
        "libs/lib/project.json",
        r#"{"name": "lib", "targets": {"build": {"command": "tsc"}}}"#,
    );
    write(
        temp.path(),
        "libs/pkg/package.json",
        r#"{"name": "pkg", "scripts": {"lint": "oxlint"}}"#,
    );
    write(
        temp.path(),
        "package.json",
        r#"{"workspaces": ["libs/pkg"]}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let web = &workspace.projects["web"].targets;
    // Glob keys, the longest matching one winning alone.
    assert_eq!(web["test-unit"].cache, Some(true));
    assert_eq!(web["test-e2e-ci"].cache, None);
    assert_eq!(web["test-e2e-ci"].options["shard"], 1);
    // The executor key's only entry is filtered out, so the name key applies,
    // and its matching entries merge in order; the glob key does not add.
    assert_eq!(web["build"].cache, Some(true));
    assert_eq!(web["build"].options["mode"], "web");
    assert!(!web["build"].options.contains_key("script"));
    assert_eq!(web["build"].outputs, None);
    let lib = &workspace.projects["lib"].targets["build"];
    assert_eq!(
        (lib.cache, lib.options["mode"].as_str()),
        (Some(true), Some("lib"))
    );
    // An entry for another executor is left out, but its key still wins.
    assert_eq!(web["deploy"].cache, None);
    // filter.plugin tells package.json scripts from project.json targets.
    assert_eq!(workspace.projects["pkg"].targets["lint"].cache, Some(true));
    assert_eq!(web["lint"].cache, None);
}

#[test]
fn a_local_workspace_file_replaces_filtered_defaults() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"targetDefaults": {"build": [{"cache": true}], "test": {"cache": true, "options": {"a": 1}}}}"#,
    );
    write(
        temp.path(),
        "nx.local.json",
        r#"{"targetDefaults": {"build": [{"cache": false}], "test": {"options": {"b": 2}}}}"#,
    );
    write(
        temp.path(),
        "project.json",
        r#"{"name": "app", "targets": {"build": {"command": "x"}, "test": {"command": "y"}}}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let targets = &workspace.projects["app"].targets;
    assert_eq!(targets["build"].cache, Some(false));
    assert_eq!(targets["test"].cache, Some(true));
    assert_eq!(
        (
            targets["test"].options["a"].clone(),
            targets["test"].options["b"].clone()
        ),
        (json!(1), json!(2))
    );
}

#[test]
fn nx_json_extends_a_file_or_package_export_one_level() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        root,
        "nx.json",
        r#"{"extends": "./nx.base.json", "defaultBase": "develop"}"#,
    );
    write(
        root,
        "nx.base.json",
        r#"{
            "extends": "./ignored.json",
            "defaultBase": "main",
            "parallel": 5,
            "namedInputs": {"production": ["default"]}
        }"#,
    );
    write(root, "ignored.json", r#"{"parallel": 9}"#);
    let workspace = Workspace::load(root).unwrap();
    // nx.json's own settings win whole, and the base's `extends` is not followed.
    assert_eq!(workspace.config.default_base.as_deref(), Some("develop"));
    assert_eq!(workspace.config.extra["parallel"], 5);
    assert_eq!(
        workspace.config.named_inputs["production"],
        [json!("default")]
    );
    assert_eq!(workspace.extended.as_deref(), Some("nx.base.json"));

    // A package subpath resolves through the package's exports, as Nx's
    // `nx/presets/npm.json` does.
    write(
        root,
        "nx.json",
        r#"{"extends": "@scope/config/presets/shared.json"}"#,
    );
    write(
        root,
        "node_modules/@scope/config/package.json",
        r#"{"exports": {
            "./presets/*": {"custom": "./src/*.json", "default": "./dist/*.json"},
            "./presets/*.json": {"custom": "./src/*.json", "default": "./dist/*.json"}
        }}"#,
    );
    write(
        root,
        "node_modules/@scope/config/dist/shared.json",
        r#"{"parallel": 2}"#,
    );
    let workspace = Workspace::load(root).unwrap();
    assert_eq!(workspace.config.extra["parallel"], 2);
    assert_eq!(
        workspace.extended.as_deref(),
        Some("node_modules/@scope/config/dist/shared.json")
    );

    write(root, "nx.json", r#"{"extends": "missing/preset.json"}"#);
    let error = Workspace::load(root).unwrap_err();
    assert!(format!("{error:#}").contains("no node_modules contains missing"));
}
