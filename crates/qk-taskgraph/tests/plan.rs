use qk_config::Workspace;
use qk_taskgraph::{Request, TaskGraph};
use serde_json::{Value, json};
use tempfile::TempDir;

fn workspace(projects: Value) -> (TempDir, Workspace) {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("nx.json"), "{}").unwrap();
    for (name, project) in projects.as_object().unwrap() {
        let directory = temp.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        let mut project = project.clone();
        project["name"] = json!(name);
        std::fs::write(directory.join("project.json"), project.to_string()).unwrap();
    }
    let loaded = Workspace::load(temp.path()).unwrap();
    (temp, loaded)
}

#[test]
fn expands_diamond_once_and_prunes_missing_dependency_targets() {
    let (_temp, workspace) = workspace(json!({
        "app": {"implicitDependencies":["lib", "empty"], "targets":{"build":{"dependsOn":["^build", "prepare"]}, "prepare":{"dependsOn":["lib:build"]}}},
        "lib": {"targets":{"build":{}}}, "empty": {"targets":{}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    assert_eq!(graph.tasks.len(), 3);
    assert_eq!(
        graph.tasks["app:build"]
            .dependencies
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["app:prepare", "lib:build"]
    );
}

#[test]
fn propagates_configurations_and_falls_back_to_dependency_default() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"defaultConfiguration":"prod", "dependsOn":["lib:build", "tool:build"], "options":{"command":"base", "env":{"A":"base"}}, "configurations":{"prod":{"command":"prod", "env":{"B":"prod"}, "outputs":["out"]}}}}},
        "lib": {"targets":{"build":{"configurations":{"prod":{"command":"lib prod"}}}}},
        "tool": {"targets":{"build":{"defaultConfiguration":"dev", "configurations":{"dev":{"command":"tool dev"}}}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    let app = &graph.tasks["app:build:prod"];
    assert!(app.dependencies.contains("lib:build:prod"));
    assert!(app.dependencies.contains("tool:build:dev"));
    assert_eq!(app.definition.options["command"], "prod");
    assert_eq!(
        app.definition.options["env"],
        json!({"A":"base", "B":"prod"})
    );
    assert_eq!(app.definition.outputs, Some(vec!["out".into()]));
}

#[test]
fn object_selectors_forward_arguments_and_exclude_self() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"dependsOn":[{"target":"build", "projects":["*", "!self"], "params":"forward"}]}}},
        "lib": {"targets":{"build":{}}}
    }));
    let mut request = Request::parse("app:build").unwrap();
    request.args = vec!["--mode=fast".into()];
    let graph = TaskGraph::build(&workspace, &[request]).unwrap();
    assert_eq!(graph.tasks["lib:build"].args, ["--mode=fast"]);
}

#[test]
fn object_dependencies_select_project_edges_and_ignore_arguments_by_default() {
    let (_temp, workspace) = workspace(json!({
        "app": {"implicitDependencies":["lib"], "targets":{"build":{"dependsOn":[{"target":"build", "dependencies":true}]}}},
        "lib": {"targets":{"build":{}}}, "other": {"targets":{"build":{}}}
    }));
    let mut request = Request::parse("app:build").unwrap();
    request.args = vec!["argument".into()];
    let graph = TaskGraph::build(&workspace, &[request]).unwrap();
    assert_eq!(graph.tasks.len(), 2);
    assert!(graph.tasks["lib:build"].args.is_empty());
}

#[test]
fn reports_cycle_path_and_unknown_root_or_configuration() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"a":{"dependsOn":["b"]}, "b":{"dependsOn":["a"]}}}
    }));
    let error = TaskGraph::build(&workspace, &[Request::parse("app:a").unwrap()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("app:a -> app:b -> app:a"), "{error}");
    for request in ["app:missing", "missing:a", "app:a:prod"] {
        assert!(TaskGraph::build(&workspace, &[Request::parse(request).unwrap()]).is_err());
    }
}

#[test]
fn rejects_conflicting_arguments_for_shared_task() {
    let (_temp, workspace) = workspace(json!({"app":{"targets":{"build":{}}}}));
    let first = Request::parse("app:build").unwrap();
    let mut second = first.clone();
    second.args.push("different".into());
    assert!(
        TaskGraph::build(&workspace, &[first, second])
            .unwrap_err()
            .to_string()
            .contains("conflicting")
    );
}

#[test]
fn reaches_through_dependencies_without_the_target_like_nx() {
    let (_temp, workspace) = workspace(json!({
        "app": {"implicitDependencies":["mid"], "targets":{"tsc":{"dependsOn":["^tsc", "^codegen"]}}},
        // mid has neither target, and leads back to app as well as on to base.
        "mid": {"implicitDependencies":["base", "app"], "targets":{}},
        "base": {"targets":{"tsc":{}, "codegen":{}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:tsc").unwrap()]).unwrap();
    assert_eq!(
        graph.tasks["app:tsc"]
            .dependencies
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["base:codegen", "base:tsc"]
    );
}
