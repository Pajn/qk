use std::collections::BTreeSet;
use std::path::Path;

use qk_config::{Package, Workspace};
use qk_graph::{GraphReport, ProjectGraph, select_projects};
use serde_json::json;

fn example() -> Workspace {
    Workspace::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/basic")).unwrap()
}

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}
fn set(values: &[&str]) -> BTreeSet<String> {
    names(values).into_iter().collect()
}

#[test]
fn graph_maps_package_names_to_project_names_and_exports_nx_envelope() {
    let graph = ProjectGraph::build(&example()).unwrap();
    let web = &graph.dependencies["web"];
    assert_eq!(
        web.iter()
            .map(|edge| (edge.target.as_str(), edge.kind.as_str()))
            .collect::<Vec<_>>(),
        [("codegen", "implicit"), ("core", "static")]
    );
    assert!(graph.dependencies["core"].is_empty());
    let json = serde_json::to_value(GraphReport { graph: &graph }).unwrap();
    assert_eq!(json["graph"]["nodes"]["web"]["data"]["root"], "apps/web");
    assert_eq!(json["graph"]["nodes"]["web"]["type"], "app");
    assert_eq!(json["graph"]["nodes"]["core"]["type"], "lib");
}

#[test]
fn all_manifest_dependency_kinds_contribute_deduplicated_edges() {
    let mut workspace = example();
    let core = workspace.packages.get_mut("core").unwrap();
    core.dependencies
        .insert("@example/web".into(), "workspace:*".into());
    core.dev_dependencies
        .insert("@example/web".into(), "workspace:*".into());
    core.peer_dependencies
        .insert("@example/web".into(), "*".into());
    core.optional_dependencies
        .insert("@example/web".into(), "*".into());
    core.dependencies.insert("external".into(), "1".into());
    let graph = ProjectGraph::build(&workspace).unwrap();
    assert_eq!(graph.dependencies["core"].len(), 1);
    // Cycles are legal in the project graph; reverse traversal still terminates.
    assert_eq!(
        graph.dependents_of(&set(&["core"])).unwrap(),
        set(&["core", "web", "worker"])
    );
    assert!(graph.dependents_of(&set(&["missing"])).is_err());
}

#[test]
fn every_dependency_section_works_independently() {
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        let mut workspace = example();
        let package: Package = serde_json::from_value(json!({
            "name": "@example/web", section: {"@example/core": "workspace:*"}
        }))
        .unwrap();
        workspace.packages.insert("web".into(), package);
        let graph = ProjectGraph::build(&workspace).unwrap();
        assert!(
            graph.dependencies["web"]
                .iter()
                .any(|edge| edge.target == "core"),
            "{section}"
        );
    }
}

#[test]
fn selectors_union_names_and_tags_then_apply_exclusions() {
    let workspace = example();
    let selected = select_projects(
        &workspace.projects,
        &names(&["tag:scope:*", "worker", "!core"]),
        &names(&["work*"]),
    )
    .unwrap();
    assert_eq!(selected, set(&["web"]));
    assert_eq!(
        select_projects(&workspace.projects, &names(&["!tag:scope:*"]), &[]).unwrap(),
        set(&["codegen", "worker"])
    );
    assert_eq!(
        select_projects(&workspace.projects, &names(&["absent"]), &[]).unwrap(),
        BTreeSet::new()
    );
    assert!(select_projects(&workspace.projects, &names(&["["]), &[]).is_err());
}

#[test]
fn implicit_globs_exclude_self_and_negations_remove_manifest_edges() {
    let mut workspace = example();
    workspace
        .projects
        .get_mut("web")
        .unwrap()
        .implicit_dependencies = names(&["!core", "*"]);
    let graph = ProjectGraph::build(&workspace).unwrap();
    assert_eq!(
        graph.dependencies["web"]
            .iter()
            .map(|edge| edge.target.as_str())
            .collect::<Vec<_>>(),
        ["codegen", "worker"]
    );
}

#[test]
fn graph_rejects_dangling_implicit_edges_and_duplicate_package_names() {
    let mut workspace = example();
    workspace
        .projects
        .get_mut("web")
        .unwrap()
        .implicit_dependencies = names(&["missing"]);
    assert!(
        ProjectGraph::build(&workspace)
            .unwrap_err()
            .to_string()
            .contains("unknown implicit dependency")
    );
    workspace
        .projects
        .get_mut("web")
        .unwrap()
        .implicit_dependencies
        .clear();
    workspace.packages.get_mut("web").unwrap().name = Some("@example/core".into());
    assert!(
        ProjectGraph::build(&workspace)
            .unwrap_err()
            .to_string()
            .contains("duplicate workspace package name")
    );
}

#[test]
fn output_is_independent_of_insertion_order() {
    let workspace = example();
    let first = serde_json::to_string(&ProjectGraph::build(&workspace).unwrap()).unwrap();
    let mut reordered = example();
    reordered.projects = reordered.projects.into_iter().rev().collect();
    reordered.packages = reordered.packages.into_iter().rev().collect();
    let second = serde_json::to_string(&ProjectGraph::build(&reordered).unwrap()).unwrap();
    assert_eq!(first, second);
}
