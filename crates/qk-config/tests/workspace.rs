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
    // Tokens in options resolve as the workspace loads.
    assert_eq!(web.targets["build"].options["cwd"], "apps/web");
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
    // ripgrep's `.ignore` files are not Nx's.
    write(temp.path(), ".ignore", "hidden-by-ignore/\n");
    for directory in [
        "node_modules/foo",
        "a/node_modules/foo",
        ".git",
        ".nx/cache",
        ".nx/workspace-data",
        ".yarn/cache",
        ".qk",
        "ignored",
    ] {
        write(
            temp.path(),
            &format!("{directory}/project.json"),
            "invalid JSON",
        );
    }
    for (directory, name) in [
        (".hidden", "visible"),
        ("target", "target"),
        (".nx/tools", "nx-tools"),
        ("hidden-by-ignore", "ignore-file"),
    ] {
        write(
            temp.path(),
            &format!("{directory}/project.json"),
            &format!(r#"{{"name":"{name}"}}"#),
        );
    }
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(
        workspace.projects.keys().collect::<Vec<_>>(),
        vec!["ignore-file", "nx-tools", "target", "visible"]
    );
}

#[test]
fn reads_the_repository_gitignores_above_a_nested_workspace() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join(".git")).unwrap();
    write(temp.path(), ".gitignore", "generated/\n");
    let root = temp.path().join("workspace");
    write(&root, "nx.json", "{}");
    write(&root, "generated/project.json", "invalid JSON");
    write(&root, "app/project.json", r#"{"name":"app"}"#);
    let workspace = Workspace::load(&root).unwrap();
    assert_eq!(workspace.projects.keys().collect::<Vec<_>>(), vec!["app"]);
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
fn takes_a_given_root_as_written_inside_the_workspace() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "tools/app/project.json",
        r#"{"name":"app","root":"apps/app","targets":{"build":{
            "command":"tsc","options":{"out":"{projectRoot}/dist"}
        }}}"#,
    );
    write(temp.path(), "project.json", r#"{"name":"top","root":""}"#);
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(workspace.projects["app"].root, "apps/app");
    // Option tokens resolve to that root, not the configuration's directory.
    assert_eq!(
        workspace.projects["app"].targets["build"].options["out"],
        "apps/app/dist"
    );
    assert_eq!(workspace.projects["top"].root, ".");
    write(
        temp.path(),
        "tools/app/project.json",
        r#"{"name":"app","root":"../outside"}"#,
    );
    let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
    assert!(
        error.contains("must be a path inside the workspace"),
        "{error}"
    );
}

/// As Nx reads a project's dependencies from its root's package.json, a
/// project whose configuration names another root takes that root's
/// package.json, not the one beside the configuration.
#[test]
fn a_given_root_takes_its_own_package_json() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "tools/app/project.json",
        r#"{"name":"app","root":"apps/app"}"#,
    );
    write(
        temp.path(),
        "tools/app/package.json",
        r#"{"name":"beside","dependencies":{"left":"1"}}"#,
    );
    write(
        temp.path(),
        "apps/app/package.json",
        r#"{"name":"at-root","dependencies":{"right":"1"}}"#,
    );
    write(temp.path(), "project.json", r#"{"name":"top"}"#);
    let workspace = Workspace::load(temp.path()).unwrap();
    let package = &workspace.packages["app"];
    assert_eq!(package.name.as_deref(), Some("at-root"));
    assert!(package.dependency_ranges().contains_key("right"));
    assert!(!package.dependency_ranges().contains_key("left"));
}

#[test]
fn names_unnamed_projects_as_nx_does() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "nx.json", "{}");
    write(
        temp.path(),
        "package.json",
        r#"{"name": "root", "workspaces": ["packages/*"]}"#,
    );
    write(temp.path(), "libs/Data/Access/project.json", "{}");
    write(temp.path(), "packages/Unnamed/package.json", "{}");
    write(
        temp.path(),
        "packages/named/package.json",
        r#"{"name": "@scope/named"}"#,
    );
    write(temp.path(), "packages/named/project.json", "{}");
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(
        workspace
            .projects
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["@scope/named", "access", "unnamed"]
    );
    // An unnamed root package is no project.
    write(temp.path(), "package.json", r#"{"nx": {}}"#);
    let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
    assert!(
        error.contains("root package.json must specify a name"),
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
    // A run-commands default does not apply to a package script, nor a
    // run-script default naming another script. A command default applies
    // beneath another command, which replaces its own, and a default without
    // an executor applies to anything.
    assert_eq!(targets["test"].executor.as_deref(), Some("nx:run-script"));
    assert_eq!(targets["test"].cache, None);
    assert_eq!(targets["lint"].options["command"], "eslint");
    assert_eq!(targets["lint"].cache, Some(true));
    assert_eq!(targets["build"].options["script"], "build");
    assert_eq!(targets["build"].cache, None);
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
    assert_eq!(build.options["cwd"], "apps/app");
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
    assert_eq!(app.targets["build"].options["cwd"], ".");
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
            "*": {"options": {"star": true}},
            "test-e2e*": {"options": {"shard": 1}},
            "build": [
                {"cache": true},
                {"filter": {"projects": ["tag:web"]}, "options": {"mode": "web"}},
                {"filter": {"projects": "lib"}, "options": {"mode": "lib"}},
                {"filter": {"projects": ["libs/*"], "unknown": true}, "outputs": ["out"]},
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
            "lint": {"command": "oxlint"},
            "serve": {"command": "vite"},
            "serve/dev": {"command": "vite --mode dev"}
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
    // A glob key's `*` does not cross a `/` in a target name.
    assert_eq!(web["serve/dev"].options.get("star"), None);
    assert_eq!(web["serve"].options["star"], true);
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
    // Project patterns read roots, and unknown filter keys are ignored.
    assert_eq!(lib.outputs, Some(vec!["out".to_owned()]));
    // No target comes from a plugin in nx.json, so filter.plugin never matches.
    assert_eq!(workspace.projects["pkg"].targets["lint"].cache, None);
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
/// Resolve file and package presets, respecting export conditions and main fallbacks.
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

    // Conditions apply in the order they are declared, as in Node.
    write(
        root,
        "node_modules/@scope/config/package.json",
        r#"{"exports": {"./presets/*.json": {"default": "./dist/*.json", "require": "./src/*.json"}}}"#,
    );
    write(
        root,
        "node_modules/@scope/config/src/shared.json",
        r#"{"parallel": 7}"#,
    );
    assert_eq!(Workspace::load(root).unwrap().config.extra["parallel"], 2);

    // Without exports, a bare package resolves to its `main`, else `index`.
    write(
        root,
        "node_modules/plain/package.json",
        r#"{"main": "preset.json"}"#,
    );
    write(root, "node_modules/plain/preset.json", r#"{"parallel": 4}"#);
    write(root, "nx.json", r#"{"extends": "plain"}"#);
    assert_eq!(Workspace::load(root).unwrap().config.extra["parallel"], 4);
    write(root, "node_modules/plain/package.json", "{}");
    write(root, "node_modules/plain/index.json", r#"{"parallel": 6}"#);
    assert_eq!(Workspace::load(root).unwrap().config.extra["parallel"], 6);

    write(
        root,
        "node_modules/plain/package.json",
        r#"{"main": "./presets"}"#,
    );
    write(
        root,
        "node_modules/plain/presets/index.json",
        r#"{"parallel": 8}"#,
    );
    assert_eq!(Workspace::load(root).unwrap().config.extra["parallel"], 8);
    write(
        root,
        "node_modules/plain/package.json",
        r#"{"main": "missing"}"#,
    );
    assert_eq!(Workspace::load(root).unwrap().config.extra["parallel"], 6);

    for exports in [
        r#"{".": {"require": null, "default": "./preset.json"}}"#,
        r#"{".": {"node": {"require": null}, "default": "./preset.json"}}"#,
    ] {
        write(
            root,
            "node_modules/plain/package.json",
            &format!(r#"{{"exports": {exports}}}"#),
        );
        assert!(format!("{:#}", Workspace::load(root).unwrap_err()).contains("does not export"));
    }
    // An unresolved nested condition allows the next active condition.
    write(
        root,
        "node_modules/plain/package.json",
        r#"{"exports": {".": {"node": {"import": null}, "default": "./preset.json"}}}"#,
    );
    assert_eq!(Workspace::load(root).unwrap().config.extra["parallel"], 4);

    write(root, "nx.json", r#"{"extends": "missing/preset.json"}"#);
    let error = Workspace::load(root).unwrap_err();
    assert!(format!("{error:#}").contains("no node_modules contains missing"));
}

// A package manager such as pnpm links packages into node_modules; the
// extended file is keyed by its real path, since keys read no input through a
// link.
#[cfg(unix)]
#[test]
fn nx_json_extends_a_linked_package_by_its_real_path() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "nx.json", r#"{"extends": "linked/preset.json"}"#);
    write(
        root,
        "node_modules/.pnpm/linked@1.0.0/node_modules/linked/preset.json",
        r#"{"parallel": 2}"#,
    );
    std::os::unix::fs::symlink(
        ".pnpm/linked@1.0.0/node_modules/linked",
        root.join("node_modules/linked"),
    )
    .unwrap();
    let workspace = Workspace::load(root).unwrap();
    assert_eq!(workspace.config.extra["parallel"], 2);
    assert_eq!(
        workspace.extended.as_deref(),
        Some("node_modules/.pnpm/linked@1.0.0/node_modules/linked/preset.json")
    );
}

fn load(files: &[(&str, serde_json::Value)]) -> anyhow::Result<Workspace> {
    let temp = TempDir::new().unwrap();
    for (path, value) in files {
        write(temp.path(), path, &value.to_string());
    }
    Workspace::load(temp.path())
}

#[test]
fn spreads_merge_project_targets_over_package_targets_and_defaults() {
    let workspace = load(&[
        (
            "nx.json",
            json!({"targetDefaults": {"build": {
                "inputs": ["default"], "dependsOn": ["^build"], "cache": true,
                "options": {"env": {"FROM": "defaults", "SHARED": "defaults"}}
            }}}),
        ),
        (
            "package.json",
            json!({"name": "app", "scripts": {"build": "tsc"}, "nx": {"targets": {"build": {
                "inputs": ["...", "{projectRoot}/package.json"],
                "configurations": {"prod": {"minify": true}, "dev": {}}
            }}}}),
        ),
        (
            "project.json",
            json!({"name": "app", "targets": {"build": {
                "cache": false, "...": true, "parallelism": false,
                "inputs": ["mine", "..."],
                "dependsOn": ["...", "lint"],
                "options": {"env": {"SHARED": "project", "...": true, "OWN": "project"}},
                "configurations": {"dev": {"x": 1}, "...": true, "prod": {"...": true, "sourceMap": false}}
            }}}),
        ),
    ])
    .unwrap();
    let build = &workspace.projects["app"].targets["build"];
    assert_eq!(build.executor.as_deref(), Some("nx:run-script"));
    assert_eq!(
        build.inputs,
        Some(vec![
            json!("mine"),
            json!("default"),
            json!("{projectRoot}/package.json")
        ])
    );
    assert_eq!(build.depends_on, Some(vec![json!("^build"), json!("lint")]));
    // Written before the spread, `cache` yields to the defaults'.
    assert_eq!(build.cache, Some(true));
    assert_eq!(build.parallelism, Some(false));
    assert_eq!(
        build.options["env"],
        json!({"SHARED": "defaults", "FROM": "defaults", "OWN": "project"})
    );
    assert_eq!(
        serde_json::to_value(&build.configurations).unwrap(),
        // `dev` is written before the spread, so the base's wins whole.
        json!({"prod": {"minify": true, "sourceMap": false}, "dev": {}})
    );
}

#[test]
fn spreads_with_nothing_beneath_expand_to_nothing() {
    let workspace = load(&[(
        "project.json",
        json!({"name": "app", "targets": {"build": {
            "command": "tsc", "...": true, "outputs": ["...", "dist"],
            "options": {"...": true, "cwd": "src"}
        }}}),
    )])
    .unwrap();
    let build = &workspace.projects["app"].targets["build"];
    assert_eq!(build.outputs, Some(vec!["dist".to_owned()]));
    assert_eq!(build.options["cwd"], "src");
    assert!(!build.options.contains_key("..."));
    assert!(!build.extra.contains_key("..."));
}

#[test]
fn integer_like_keys_beside_a_spread_are_an_error() {
    let error = format!(
        "{:#}",
        load(&[(
            "project.json",
            json!({"name": "app", "targets": {"build": {
                "command": "tsc", "options": {"env": {"...": true, "1": "x"}}
            }}}),
        )])
        .unwrap_err()
    );
    assert!(error.contains("integer-like key (\"1\")"), "{error}");
}

#[test]
fn a_target_running_differently_drops_everything_beneath() {
    let workspace = load(&[
        (
            "package.json",
            json!({"name": "app", "nx": {"targets": {
                "build": {"command": "tsc", "cache": true, "dependsOn": ["^build"],
                          "options": {"cwd": "src"}, "configurations": {"prod": {}}},
                "lint": {"command": "eslint", "cache": true}
            }}}),
        ),
        (
            "project.json",
            json!({"name": "app", "targets": {
                "build": {"command": "vite build"},
                "lint": {"command": "eslint", "outputs": []}
            }}),
        ),
    ])
    .unwrap();
    let targets = &workspace.projects["app"].targets;
    let build = &targets["build"];
    assert_eq!(build.options["command"], "vite build");
    assert_eq!(
        (
            build.cache,
            &build.depends_on,
            build.options.get("cwd"),
            build.configurations.len()
        ),
        (None, &None, None, 0)
    );
    // The same command merges.
    assert_eq!(targets["lint"].cache, Some(true));
    assert_eq!(targets["lint"].outputs, Some(vec![]));
}

#[test]
fn package_targets_saying_how_they_run_replace_the_script() {
    let workspace = load(&[(
        "package.json",
        json!({"name": "app", "scripts": {"build": "tsc", "test": "vitest", "lint": "eslint"},
        "nx": {"targets": {
            "build": {"command": "vite build", "metadata": {"own": true}},
            "test": {"cache": true},
            "lint": {"executor": "nx:run-script", "options": {"script": "lint"}, "cache": true}
        }}}),
    )])
    .unwrap();
    let targets = &workspace.projects["app"].targets;
    assert_eq!(targets["build"].options["command"], "vite build");
    assert!(!targets["build"].options.contains_key("script"));
    assert_eq!(targets["test"].options["script"], "test");
    assert_eq!(targets["test"].cache, Some(true));
    assert_eq!(targets["lint"].cache, Some(true));
}

#[test]
fn a_project_json_target_running_differently_removes_the_script() {
    // The script's target is gone rather than replaced, so package.json's
    // own fields for the target stay beneath project.json's command.
    let workspace = load(&[
        (
            "package.json",
            json!({"name": "app", "scripts": {"build": "tsc", "check": "tsc --noEmit"},
                   "nx": {"targets": {"build": {"dependsOn": ["^build"]}}}}),
        ),
        (
            "project.json",
            json!({"name": "app", "targets": {
                "build": {"command": "vite build"},
                "check": {"executor": "nx:run-script", "options": {"script": "check"}, "cache": true}
            }}),
        ),
    ])
    .unwrap();
    let targets = &workspace.projects["app"].targets;
    assert_eq!(targets["build"].options["command"], "vite build");
    assert_eq!(targets["build"].depends_on, Some(vec![json!("^build")]));
    assert_eq!(targets["check"].cache, Some(true));
}

#[test]
fn target_defaults_stay_beneath_a_target_the_files_replace() {
    // `includedScripts` keeps the script target, which project.json's
    // command replaces; Nx drops the target default with it, qk does not.
    let workspace = load(&[
        (
            "nx.json",
            json!({"targetDefaults": {"build": {"cache": true}}}),
        ),
        (
            "package.json",
            json!({"name": "app", "scripts": {"build": "tsc"}, "nx": {"includedScripts": ["build"]}}),
        ),
        (
            "project.json",
            json!({"name": "app", "targets": {"build": {"command": "vite build"}}}),
        ),
    ])
    .unwrap();
    let build = &workspace.projects["app"].targets["build"];
    assert_eq!(build.options["command"], "vite build");
    assert_eq!(build.cache, Some(true));
}

#[test]
fn glob_target_keys_merge_onto_every_matching_target() {
    let workspace = load(&[
        (
            "package.json",
            json!({"name": "app", "scripts": {"e2e-ci--a": "x", "e2e-ci--b": "y", "lint": "z"}}),
        ),
        (
            "project.json",
            json!({"name": "app", "targets": {
                "e2e-ci--*": {"dependsOn": ["build"], "cache": true},
                "nothing-*": {"command": "echo"}
            }}),
        ),
    ])
    .unwrap();
    let targets = &workspace.projects["app"].targets;
    for name in ["e2e-ci--a", "e2e-ci--b"] {
        assert_eq!(targets[name].cache, Some(true), "{name}");
        assert_eq!(targets[name].depends_on, Some(vec![json!("build")]));
    }
    assert_eq!(targets["lint"].cache, None);
    assert!(!targets.contains_key("e2e-ci--*"));
    // A key matching nothing is a target of its own.
    assert!(targets.contains_key("nothing-*"));
}

#[test]
fn targets_that_run_nothing_are_no_ops_or_left_out() {
    let workspace = load(&[(
        "project.json",
        json!({"name": "app", "targets": {
            "all": {"dependsOn": ["build"]},
            "empty": {"cache": true},
            "none": {"dependsOn": []},
            "build": {"command": "tsc"}
        }}),
    )])
    .unwrap();
    let targets = &workspace.projects["app"].targets;
    assert_eq!(targets["all"].executor.as_deref(), Some("nx:noop"));
    assert!(!targets.contains_key("empty"));
    assert!(!targets.contains_key("none"));
}

#[test]
fn invalid_target_combinations_are_errors() {
    for (target, expected) in [
        (
            json!({"executor": "nx:run-commands", "command": "tsc"}),
            "should not have executor and command both configured",
        ),
        (
            json!({"command": "vite", "cache": true, "continuous": true}),
            "has both \"cache\" and \"continuous\" set to true",
        ),
    ] {
        let error = format!(
            "{:#}",
            load(&[(
                "project.json",
                json!({"name": "app", "targets": {"serve": target}})
            )])
            .unwrap_err()
        );
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn tokens_resolve_in_every_option_as_the_workspace_loads() {
    let workspace = load(&[
        ("nx.json", json!({})),
        (
            "libs/ui/project.json",
            json!({"name": "ui", "targets": {"build": {
                "executor": "nx:run-script",
                "options": {
                    "script": "build-{projectName}",
                    "cwd": "{workspaceRoot}/{projectRoot}",
                    "root": "{workspaceRoot}",
                    "nested": {"list": ["{projectRoot}/a", {"deep": "{workspaceRoot}/b"}]}
                },
                "configurations": {"prod": {"out": "{workspaceRoot}/dist/{projectName}"}},
                "outputs": ["{workspaceRoot}/dist/{projectRoot}"]
            }}}),
        ),
    ])
    .unwrap();
    let build = &workspace.projects["ui"].targets["build"];
    assert_eq!(
        serde_json::Value::Object(build.options.clone()),
        json!({
            "script": "build-ui", "cwd": "libs/ui", "root": "",
            "nested": {"list": ["libs/ui/a", {"deep": "b"}]}
        })
    );
    assert_eq!(build.configurations["prod"]["out"], "dist/ui");
    // Outputs keep their tokens until a task resolves them.
    assert_eq!(
        build.outputs,
        Some(vec!["{workspaceRoot}/dist/{projectRoot}".to_owned()])
    );
    let error = format!(
        "{:#}",
        load(&[(
            "project.json",
            json!({"name": "app", "targets": {"build": {
                "command": "tsc", "options": {"env": {"A": "x/{workspaceRoot}"}}
            }}}),
        )])
        .unwrap_err()
    );
    assert!(
        error.contains("{workspaceRoot} token is only valid at the beginning of an option"),
        "{error}"
    );
}

#[test]
fn metadata_and_generators_merge_across_files() {
    let workspace = load(&[
        (
            "package.json",
            json!({"name": "app", "nx": {
                "metadata": {"technologies": ["node"], "owner": "a"},
                "generators": {"@nx/js:lib": {"bundler": "tsc", "unitTestRunner": "jest"}},
                "targets": {"build": {"command": "tsc", "metadata": {"technologies": ["tsc"], "help": {"a": 1}}}}
            }}),
        ),
        (
            "project.json",
            json!({"name": "app",
                "metadata": {"technologies": ["react"]},
                "generators": {"@nx/js:lib": {"bundler": "vite"}, "@nx/react:component": {"style": "css"}},
                "targets": {"build": {"metadata": {"technologies": ["vite"], "help": {"b": 2}}}}
            }),
        ),
    ])
    .unwrap();
    let app = &workspace.projects["app"];
    // Nx replaces a package's own metadata with what it reads from the
    // package, and project.json's merges over that.
    assert_eq!(
        app.extra["metadata"],
        json!({
            "targetGroups": {},
            "js": {"packageName": "app", "isInPackageManagerWorkspaces": true},
            "technologies": ["react"]
        })
    );
    assert_eq!(
        app.extra["generators"],
        json!({
            "@nx/js:lib": {"bundler": "vite", "unitTestRunner": "jest"},
            "@nx/react:component": {"style": "css"}
        })
    );
    assert_eq!(
        app.targets["build"].extra["metadata"],
        json!({"technologies": ["tsc", "vite"], "help": {"a": 1, "b": 2}})
    );
}

#[test]
fn project_json_merges_over_package_json_as_in_nx() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "nx.json", "{}");
    write(
        temp.path(),
        "package.json",
        r#"{"name": "root", "workspaces": ["libs/*"], "packageManager": "pnpm@9.0.0"}"#,
    );
    write(
        temp.path(),
        "libs/lib/package.json",
        r#"{"name": "@scope/lib", "version": "1.0.0", "description": "A library",
            "main": "index.js", "scripts": {"build": "tsc", "test": "vitest", "lint": "eslint"},
            "nx": {"implicitDependencies": ["other"],
                   "generators": {"@nx/js:lib": {"a": 1, "b": 1}, "kept": {"x": 1}},
                   "metadata": {"ignored": true},
                   "targets": {"test": {"command": "vitest run"}}}}"#,
    );
    write(
        temp.path(),
        "libs/lib/project.json",
        r#"{"implicitDependencies": "third",
            "generators": {"@nx/js:lib": {"b": 2}},
            "metadata": {"targetGroups": {"NPM Scripts": ["extra"], "Mine": ["a"]}, "owner": "me"},
            "targets": {"lint": {"command": "oxlint"}}}"#,
    );
    write(
        temp.path(),
        "libs/other/project.json",
        r#"{"name": "other"}"#,
    );
    write(
        temp.path(),
        "libs/third/project.json",
        r#"{"name": "third"}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    let lib = &workspace.projects["@scope/lib"];
    // Implicit dependencies concatenate; a single string is one entry.
    assert_eq!(lib.implicit_dependencies, ["other", "third"]);
    // Generators merge by generator and option.
    assert_eq!(
        lib.extra["generators"],
        json!({"@nx/js:lib": {"a": 1, "b": 2}, "kept": {"x": 1}})
    );
    // Nx replaces the package's metadata with its own, then merges
    // project.json's over it, arrays concatenating.
    assert_eq!(
        lib.extra["metadata"],
        json!({
            "targetGroups": {"NPM Scripts": ["build", "test", "extra"], "Mine": ["a"]},
            "description": "A library",
            "js": {
                "packageName": "@scope/lib",
                "packageVersion": "1.0.0",
                "packageMain": "index.js",
                "isInPackageManagerWorkspaces": true
            },
            "owner": "me"
        })
    );
    // A script project.json runs another way is no package target.
    assert_eq!(lib.targets["lint"].extra.get("metadata"), None);
    assert_eq!(
        lib.targets["build"].extra["metadata"],
        json!({"scriptContent": "tsc", "runCommand": "pnpm run build"})
    );
    // A package.json target saying how it runs replaces the script's target.
    assert_eq!(lib.targets["test"].extra.get("metadata"), None);
    assert_eq!(lib.targets["test"].options["command"], "vitest run");
    // A project.json project gains no package metadata.
    assert!(!workspace.projects["other"].extra.contains_key("metadata"));
}

#[test]
fn implicit_dependencies_must_be_strings() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "nx.json",
        r#"{"implicitDependencies": {"package.json": "*"}}"#,
    );
    write(temp.path(), "a/project.json", r#"{"name": "a"}"#);
    let workspace = Workspace::load(temp.path()).unwrap();
    assert!(
        workspace.warnings[0].contains("implicitDependencies is no longer supported"),
        "{:?}",
        workspace.warnings
    );
    write(
        temp.path(),
        "a/project.json",
        r#"{"name": "a", "implicitDependencies": {"b": true}}"#,
    );
    let error = format!("{:#}", Workspace::load(temp.path()).unwrap_err());
    assert!(
        error.contains("implicitDependencies should be an array of strings"),
        "{error}"
    );
}

#[test]
fn workspace_patterns_come_from_every_package_manager_file() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "package.json",
        r#"{"name": "root", "workspaces": ["./apps/*/", "tools/cli/package.json"]}"#,
    );
    write(
        temp.path(),
        "pnpm-workspace.yaml",
        "packages:\n  - libs/**\n  - '!libs/skip'\n",
    );
    write(temp.path(), "lerna.json", r#"{"version": "1.0.0"}"#);
    for (directory, name) in [
        ("apps/web", "web"),
        ("tools/cli", "cli"),
        ("libs/a/b", "deep"),
        ("libs/skip", "skip"),
        ("libs/.hidden", "hidden"),
        ("packages/lerna", "lerna"),
        ("other", "other"),
    ] {
        write(
            temp.path(),
            &format!("{directory}/package.json"),
            &format!(r#"{{"name": "{name}"}}"#),
        );
    }
    let workspace = Workspace::load(temp.path()).unwrap();
    // All three files' patterns count; minimatch's `*` and `**` skip dot
    // directories, and an exclusion wins.
    assert_eq!(
        workspace
            .projects
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["cli", "deep", "lerna", "web"]
    );
}

#[test]
fn exclusions_alone_take_every_package_json_and_bad_pnpm_files_warn() {
    let temp = TempDir::new().unwrap();
    write(
        temp.path(),
        "package.json",
        r#"{"name": "root", "workspaces": ["!skipped"], "nx": {}}"#,
    );
    write(temp.path(), "pnpm-workspace.yaml", "packages: [unclosed\n");
    write(temp.path(), "a/b/package.json", r#"{"name": "b"}"#);
    write(
        temp.path(),
        "skipped/package.json",
        r#"{"name": "skipped"}"#,
    );
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(
        workspace
            .projects
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["b", "root"]
    );
    assert!(
        workspace.warnings[0].contains("cannot parse pnpm-workspace.yaml"),
        "{:?}",
        workspace.warnings
    );
    // A package.json beside a project.json is a project outside the
    // workspaces, and says so in its metadata.
    write(temp.path(), "skipped/project.json", "{}");
    let workspace = Workspace::load(temp.path()).unwrap();
    assert_eq!(
        workspace.projects["skipped"].extra["metadata"]["js"]["isInPackageManagerWorkspaces"],
        false
    );
}

#[test]
fn finds_the_workspace_root_as_nx_does() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    write(&root, "nx.json", "{}");
    write(&root, "packages/app/pnpm-workspace.yaml", "packages: []\n");
    write(&root, "packages/app/src/index.js", "");
    // A nearer package manager workspace does not shadow an Nx root.
    assert_eq!(
        find_workspace(&root.join("packages/app/src")).unwrap(),
        root
    );

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    write(&root, "repo/nx", "#!/bin/sh\n");
    write(
        &root,
        "repo/libs/a/package.json",
        r#"{"name": "a", "nx": {}}"#,
    );
    // The Nx wrapper script marks a root too.
    assert_eq!(
        find_workspace(&root.join("repo/libs/a")).unwrap(),
        root.join("repo")
    );

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    write(&root, "repo/node_modules/nx/package.json", "{}");
    write(&root, "repo/libs/a/node_modules/nx/package.json", "{}");
    write(&root, "repo/libs/a/project.json", r#"{"name": "a"}"#);
    // Without a marker, the topmost directory with Nx installed.
    assert_eq!(
        find_workspace(&root.join("repo/libs/a")).unwrap(),
        root.join("repo")
    );
}

#[test]
fn a_workspace_of_another_revision_reads_its_package_json_files_through_the_reader() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "nx.json", "{}");
    write(root, "package.json", r#"{"name": "root"}"#);
    write(
        root,
        "libs/lib/package.json",
        r#"{"name": "lib", "scripts": {"build": "echo head"}, "nx": {"tags": ["head"]}}"#,
    );
    // The package.json files as another revision held them. That revision's
    // workspaces take in libs; the one on disk takes in nothing.
    let read = |path: &Path| -> anyhow::Result<Option<String>> {
        let text = match path.file_name().and_then(|name| name.to_str()) {
            Some("package.json") if path.ends_with("libs/lib/package.json") => {
                r#"{"name": "lib", "scripts": {"build": "echo base"}, "nx": {"tags": ["base"]}}"#
                    .to_owned()
            }
            Some("package.json") => {
                r#"{"name": "root", "packageManager": "npm@10.0.0", "workspaces": ["libs/*"]}"#
                    .to_owned()
            }
            _ => return Ok(std::fs::read_to_string(path).ok()),
        };
        Ok(Some(text))
    };
    let workspace = Workspace::load_with(root, &read).unwrap();
    let lib = &workspace.projects["lib"];
    assert!(lib.tags.contains(&"base".to_owned()), "{:?}", lib.tags);
    assert_eq!(workspace.packages["lib"].scripts["build"], "echo base");
    assert_eq!(workspace.package_manager.as_deref(), Some("npm@10.0.0"));
}

#[test]
fn a_workspace_of_another_revision_extends_a_file_only_that_revision_holds() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "nx.json", "{}");
    write(
        root,
        "libs/lib/project.json",
        r#"{"name": "lib", "targets": {"build": {}}}"#,
    );
    // That revision's nx.json extends a file the working tree no longer has.
    let read = |path: &Path| -> anyhow::Result<Option<String>> {
        Ok(match path.file_name().and_then(|name| name.to_str()) {
            Some("nx.json") => Some(r#"{"extends": "./presets/base.json"}"#.to_owned()),
            Some("base.json") => {
                Some(r#"{"targetDefaults": {"build": {"command": "echo base"}}}"#.to_owned())
            }
            _ => std::fs::read_to_string(path).ok(),
        })
    };
    let workspace = Workspace::load_with(root, &read).unwrap();
    assert_eq!(
        workspace.projects["lib"].targets["build"].options["command"],
        json!("echo base")
    );
}

#[test]
fn an_extended_file_another_revision_cannot_read_fails_the_load() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "nx.json", "{}");
    write(root, "presets/base.json", r#"{"targetDefaults": {}}"#);
    let read = |path: &Path| -> anyhow::Result<Option<String>> {
        match path.file_name().and_then(|name| name.to_str()) {
            Some("nx.json") => Ok(Some(r#"{"extends": "./presets/base"}"#.to_owned())),
            // Neither a file nor no file: the revision cannot be read.
            Some(name) if name.starts_with("base") => anyhow::bail!("revision unreadable"),
            _ => Ok(std::fs::read_to_string(path).ok()),
        }
    };
    let error = Workspace::load_with(root, &read).unwrap_err();
    assert!(
        format!("{error:#}").contains("revision unreadable"),
        "{error:#}"
    );
}
