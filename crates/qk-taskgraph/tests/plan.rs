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
        // A target that runs nothing is left out, so each runs nx:noop.
        if let Some(Value::Object(targets)) = project.get_mut("targets") {
            for target in targets.values_mut() {
                if target.get("executor").is_none() && target.get("command").is_none() {
                    target["executor"] = json!("nx:noop");
                }
            }
        }
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
                "prod":{"command":"prod", "env":{"CONFIG":"prod", "BOTH":"prod"}, "outputs":["{projectRoot}/out"]},
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
            // As in Nx, a configuration's `outputs` is an option like any
            // other, its tokens resolved.
            assert_eq!(task.definition.options["outputs"], json!(["app/out"]));
            assert_eq!(task.definition.outputs, None);
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
fn dependency_projects_are_read_as_nx_reads_them() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{"dependsOn":[
            {"target":"build", "projects":[]},
            {"target":"lint", "projects":["!tag:skip", "app"]}
        ]}, "lint":{}}},
        "lib": {"tags":["skip"], "targets":{"build":{}, "lint":{}}},
        "other": {"targets":{"build":{}, "lint":{}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    // No patterns select no project, and patterns apply in order.
    let mut tasks: Vec<&str> = graph.tasks.keys().map(String::as_str).collect();
    tasks.sort();
    assert_eq!(tasks, ["app:build", "app:lint", "other:lint"]);
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
fn a_shared_task_keeps_what_it_was_first_reached_with() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{
            "build":{"options":{"mode":"app"}, "dependsOn":[
                {"target":"lint", "options":"forward"},
                {"projects":"lib", "target":"build", "params":"forward"}
            ]},
            "lint":{"options":{"mode":"lint"}},
            "check":{"dependsOn":["lint", "build"]}
        }},
        "lib": {"targets":{"build":{}}}
    }));
    // lint is reached plainly first, then with app:build's options.
    let mut request = Request::parse("app:check").unwrap();
    request.args = vec!["--fast".into()];
    let graph = TaskGraph::build(&workspace, &[request]).unwrap();
    assert_eq!(graph.tasks["app:lint"].definition.options["mode"], "lint");
    assert_eq!(graph.tasks["lib:build"].args, ["--fast"]);
    let first = Request::parse("app:lint").unwrap();
    let mut second = first.clone();
    second.args.push("different".into());
    let graph = TaskGraph::build(&workspace, &[first, second]).unwrap();
    assert!(graph.tasks["app:lint"].args.is_empty());
}

#[test]
fn forwarded_options_become_the_dependency_options() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"build":{
            "options":{"list":["a", "b"], "none":null, "env":{"A":"1"}, "mode":"base"},
            "configurations":{"prod":{"mode":"prod"}},
            "dependsOn":[{"target":"lint", "options":"forward", "unknown":true}]
        }, "lint":{"options":{"mode":"lint", "own":true}}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build:prod").unwrap()]).unwrap();
    let lint = &graph.tasks["app:lint"];
    assert_eq!(
        Value::Object(lint.definition.options.clone()),
        json!({"mode": "prod", "own": true, "list": ["a", "b"], "none": null, "env": {"A": "1"}})
    );
    assert!(lint.args.is_empty());
}

#[test]
fn params_forward_the_runs_own_arguments_through_any_chain() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{"a":{"dependsOn":["b"]}, "b":{"dependsOn":[{"target":"c", "params":"forward"}]}, "c":{}}}
    }));
    let mut request = Request::parse("app:a").unwrap();
    request.args = vec!["--x=1".into()];
    let graph = TaskGraph::build(&workspace, &[request]).unwrap();
    assert!(graph.tasks["app:b"].args.is_empty());
    assert_eq!(graph.tasks["app:c"].args, ["--x=1"]);
}

#[test]
fn dependency_targets_may_contain_colons() {
    let (_temp, workspace) = workspace(json!({
        "app": {"implicitDependencies":["lib"], "targets":{
            "build":{"dependsOn":["test:unit", "lib:build", "^lint:ci", {"target":"e2e:ci"}, "missing:target"]},
            "test:unit":{}, "e2e:ci":{}
        }},
        "lib": {"targets":{"build":{}, "lint:ci":{}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    assert_eq!(
        graph.tasks["app:build"]
            .dependencies
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["app:e2e:ci", "app:test:unit", "lib:build", "lib:lint:ci"]
    );
}

#[test]
fn named_projects_win_over_dependencies_true() {
    let (_temp, workspace) = workspace(json!({
        "app": {"implicitDependencies":["lib"], "targets":{"build":{"dependsOn":[
            {"target":"build", "projects":["other"], "dependencies":true}
        ]}}},
        "lib": {"targets":{"build":{}}}, "other": {"targets":{"build":{}}}
    }));
    let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
    assert_eq!(
        graph.tasks["app:build"].dependencies,
        ["other:build".to_owned()].into()
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

/// As in Nx 24, a task's outputs start with a token, and building the task
/// graph refuses one that does not, with Nx's message.
#[test]
fn outputs_must_start_with_a_token() {
    let (_temp, workspace) = workspace(json!({
        "app": {"targets":{
            "bare":{"outputs":["dist", "!dist/keep", "{projectRoot}/out"]},
            "glob":{"outputs":["{workspaceRoot}/**/*.js"]},
            "tokens":{"outputs":["{options.out}", "!{projectRoot}/out/keep", "{workspaceRoot}/dist"]},
            "defaults":{"options":{"outputPath":"dist/app"}},
            "build":{"dependsOn":["bare"]}
        }}
    }));
    let error = |target: &str| {
        TaskGraph::build(&workspace, &[Request::parse(target).unwrap()])
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        error("app:bare"),
        "The following outputs are invalid: \n - dist\n   ** Reason: Outputs must start with either \"{workspaceRoot}/\" or \"{projectRoot}/\".\n - !dist/keep\n   ** Reason: Outputs must start with either \"{workspaceRoot}/\" or \"{projectRoot}/\".\n\nRun `nx repair` to fix this."
    );
    assert_eq!(error("app:build"), error("app:bare"));
    assert_eq!(
        error("app:glob"),
        "The following outputs are defined by a glob pattern from the workspace root: \n - {workspaceRoot}/**/*.js\n\nThese can be slow, replace them with a more specific pattern."
    );
    for target in ["app:tokens", "app:defaults"] {
        assert!(TaskGraph::build(&workspace, &[Request::parse(target).unwrap()]).is_ok());
    }
}
