use qk_config::Workspace;
use qk_taskgraph::{Edge, Request, TaskGraph, WaitFor};
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
fn preserves_requested_configuration_across_dependency_defaults() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"defaultConfiguration":"prod", "dependsOn":["lib:build", "tool:build"], "configurations":{"prod":{}}}}},
        "lib": {"targets":{"build":{"configurations":{"prod":{}}}}},
        "tool": {"targets":{"build":{"defaultConfiguration":"dev", "dependsOn":["leaf:build"], "configurations":{"dev":{}}}}},
        "leaf": {"targets":{"build":{"configurations":{"prod":{}, "dev":{}}}}}
    }));
    for (request, lib, leaf) in [
        ("app:build", "lib:build", "leaf:build"),
        ("app:build:prod", "lib:build:prod", "leaf:build:prod"),
    ] {
        let graph = TaskGraph::build(&workspace, &[Request::parse(request).unwrap()]).unwrap();
        assert_eq!(graph.tasks.len(), 4);
        assert_eq!(
            graph.tasks["app:build:prod"].dependencies,
            [lib.to_owned(), "tool:build:dev".to_owned()].into()
        );
        assert_eq!(
            graph.tasks["tool:build:dev"].dependencies,
            [leaf.to_owned()].into()
        );
    }
}

#[test]
fn shared_tasks_collect_dependencies_from_each_requested_configuration() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"dependsOn":["tool:build"], "configurations":{"prod":{}}}}},
        "tool": {"targets":{"build":{"defaultConfiguration":"dev", "dependsOn":["leaf:build"], "configurations":{"dev":{}}}}},
        "leaf": {"targets":{"build":{"configurations":{"prod":{}}}}}
    }));
    let mut plans = Vec::new();
    for requests in [
        ["app:build:prod", "tool:build"],
        ["tool:build", "app:build:prod"],
    ] {
        let requests = requests.map(|request| Request::parse(request).unwrap());
        let graph = TaskGraph::build(&workspace, &requests).unwrap();
        assert_eq!(graph.tasks.len(), 4);
        assert_eq!(
            graph.tasks["tool:build:dev"].dependencies,
            ["leaf:build".to_owned(), "leaf:build:prod".to_owned()].into()
        );
        plans.push(serde_json::to_value(graph).unwrap());
    }
    assert_eq!(plans[0], plans[1]);
}

#[test]
fn configuration_environment_replaces_base_environment() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{
            "defaultConfiguration":"prod",
            "options":{"command":"base", "env":{"BASE":"base", "BOTH":"base"}},
            "configurations":{
                "prod":{"command":"prod", "env":{"CONFIG":"prod", "BOTH":"prod"}, "outputs":["out"]},
                "empty":{"env":{}},
                "unchanged":{}
            }
        }}}
    }));
    for (request, expected) in [
        ("app:build", json!({"CONFIG":"prod", "BOTH":"prod"})),
        ("app:build:prod", json!({"CONFIG":"prod", "BOTH":"prod"})),
        ("app:build:empty", json!({})),
        ("app:build:unchanged", json!({"BASE":"base", "BOTH":"base"})),
    ] {
        let graph = TaskGraph::build(&workspace, &[Request::parse(request).unwrap()]).unwrap();
        let task = graph.tasks.values().next().unwrap();
        assert_eq!(task.definition.options["env"], expected);
        if task.configuration.as_deref() == Some("prod") {
            assert_eq!(task.definition.options["command"], "prod");
            assert_eq!(task.definition.outputs, Some(vec!["out".into()]));
        }
    }
}

#[test]
fn object_selectors_forward_arguments_and_exclude_projects() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"dependsOn":[{"target":"build", "projects":["*", "!app"], "params":"forward"}]}}},
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

#[test]
fn continuous_dependencies_record_how_they_are_depended_on() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"serve":{"continuous":true}}},
        "app-e2e": {"targets":{
            "e2e":{"dependsOn":[{"projects":["app"], "target":"serve", "inputs":false, "waitFor":"ready"}]},
            "smoke":{"dependsOn":["app:serve"]}
        }}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app-e2e:e2e").unwrap()]).unwrap();
    let e2e = &graph.tasks["app-e2e:e2e"];
    assert_eq!(
        e2e.edge("app:serve"),
        Edge {
            inputs: false,
            wait_for: WaitFor::Ready
        }
    );
    let graph = TaskGraph::build(&workspace, &[Request::parse("app-e2e:smoke").unwrap()]).unwrap();
    assert_eq!(
        graph.tasks["app-e2e:smoke"].edge("app:serve"),
        Edge::default()
    );
    assert!(graph.tasks["app-e2e:smoke"].edges.is_empty());
}

#[test]
fn entries_naming_one_dependency_merge_as_nx_merges_them() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"serve":{"continuous":true}}},
        "app-e2e": {"targets":{
            "opted":{"dependsOn":[
                {"projects":"app", "target":"serve", "inputs":false},
                {"projects":"app", "target":"serve", "waitFor":"ready"}
            ]},
            "both":{"dependsOn":[
                {"projects":"app", "target":"serve", "inputs":false, "waitFor":"started"},
                {"projects":["tag:none", "app"], "target":"serve", "inputs":false}
            ]}
        }}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app-e2e:opted").unwrap()]).unwrap();
    assert_eq!(
        graph.tasks["app-e2e:opted"].edge("app:serve"),
        Edge {
            inputs: true,
            wait_for: WaitFor::Ready
        }
    );
    let graph = TaskGraph::build(&workspace, &[Request::parse("app-e2e:both").unwrap()]).unwrap();
    assert_eq!(
        graph.tasks["app-e2e:both"].edge("app:serve"),
        Edge {
            inputs: false,
            wait_for: WaitFor::Started
        }
    );
}

#[test]
fn dependency_edge_fields_are_validated() {
    for (entry, message) in [
        (
            json!({"target":"serve", "inputs":"no"}),
            "inputs must be boolean",
        ),
        (
            json!({"target":"serve", "waitFor":"up"}),
            "waitFor must be started or ready",
        ),
    ] {
        let (_temp, workspace) = workspace(json!({
            "app": {"targets":{"serve":{"continuous":true}, "e2e":{"dependsOn":[entry]}}}
        }));
        let error =
            TaskGraph::build(&workspace, &[Request::parse("app:e2e").unwrap()]).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }
}

#[test]
fn nx_15_project_spellings_fail_unless_a_project_has_the_name() {
    for spelling in ["self", "dependencies"] {
        let (_temp, workspace) = workspace(json!({
            "app": {"targets":{"lint":{}, "build":{"dependsOn":[{"target":"lint", "projects":spelling}]}}}
        }));
        let error =
            TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap_err();
        assert!(
            format!("{error:#}").contains("no longer supported"),
            "{error:#}"
        );
    }
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"dependsOn":[{"target":"lint", "projects":"self"}]}}},
        "self": {"targets":{"lint":{}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    assert!(graph.tasks["app:build"].dependencies.contains("self:lint"));
}
