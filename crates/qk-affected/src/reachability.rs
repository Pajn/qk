//! Opt-in import reachability: under an affected profile with `reachability`,
//! a project affected only through its dependencies is left out when none of
//! its anchors imports what changed.
//!
//! A project declares `qk:reachability` with `anchors`, the files whose
//! imports it depends on through, and `sources`, the changed files that matter
//! to it only through imports. Any other change it is affected through keeps
//! it, because no import carries it. Imports are followed by fallout, over the
//! checkout.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use qk_cache::Pattern;
use qk_config::Workspace;
use qk_graph::ProjectGraph;
use serde::Serialize;
use serde_json::Value;

use crate::{Cause, Changes, Reason, git_lines};

/// A project's or target's `qk:reachability`, its paths expanded.
pub(crate) struct Settings {
    pub anchors: Vec<String>,
    sources: Vec<Pattern>,
}

impl Settings {
    /// Whether a change to `file` matters only through imports.
    pub fn is_source(&self, file: &str) -> bool {
        file.rsplit('/').next() != Some("package.json")
            && self.sources.iter().any(|pattern| pattern.is_match(file))
    }
}

/// Reads `qk:reachability` from `extra`, expanding paths for `project`.
/// `what` names it in errors.
pub(crate) fn settings(
    workspace: &Workspace,
    project: &str,
    extra: &BTreeMap<String, Value>,
    what: &str,
) -> Result<Option<Settings>> {
    let Some(value) = extra.get("qk:reachability") else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .with_context(|| format!("{what} qk:reachability must be an object"))?;
    let mut lists: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (key, value) in object {
        let key = match key.as_str() {
            "anchors" | "sources" => key.as_str(),
            "cases" => bail!("{what} qk:reachability.cases belongs on a target"),
            _ => bail!("unknown {what} qk:reachability field {key:?}"),
        };
        let paths = value
            .as_array()
            .filter(|paths| paths.iter().all(Value::is_string))
            .with_context(|| format!("{what} qk:reachability.{key} must be an array of paths"))?;
        let expanded = paths
            .iter()
            .map(|path| {
                qk_cache::expand_project_path(workspace, project, path.as_str().unwrap())
                    .with_context(|| format!("{what} qk:reachability.{key}"))
            })
            .collect::<Result<_>>()?;
        lists.insert(key, expanded);
    }
    let anchors = lists.remove("anchors").unwrap_or_default();
    let sources = lists.remove("sources").unwrap_or_default();
    ensure!(!anchors.is_empty(), "{what} qk:reachability needs anchors");
    ensure!(
        !sources.is_empty(),
        "{what} qk:reachability needs sources: without them no change is carried only by imports"
    );
    Ok(Some(Settings {
        anchors,
        sources: sources
            .iter()
            .map(|pattern| Pattern::new(pattern, false))
            .collect::<Result<_>>()?,
    }))
}

/// What the profile decided for a project ordinary selection affects only
/// through its dependencies.
#[derive(Debug, Serialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum Decision {
    /// No anchor imports a change.
    LeftOut {
        /// The anchor files searched.
        anchors: Vec<String>,
        /// The changes the project was affected through.
        changed: Vec<String>,
    },
    /// Affected only through projects the profile left out.
    Through { project: String },
    /// Kept as ordinary selection affects it.
    Kept { why: Kept },
}

/// Why a project with `qk:reachability` was kept.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Kept {
    /// An anchor imports a changed file.
    Reached {
        anchor: String,
        changed: String,
        /// The anchor first, each file importing the next.
        chain: Vec<String>,
    },
    /// The search from an anchor could not place an import the repository
    /// answers for, so a change may hide behind it.
    Gap {
        anchor: String,
        specifier: String,
        from: Vec<String>,
    },
    /// A dependency is touched by a change imports do not carry.
    NotImported { project: String, reason: Reason },
    /// The imports could not be followed.
    Unanswered { detail: String },
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::LeftOut { anchors, .. } => {
                let count = anchors.len();
                let noun = if count == 1 { "anchor" } else { "anchors" };
                write!(f, "left out: none of its {count} {noun} imports a change")
            }
            Self::Through { project } => {
                write!(f, "left out: affected only through {project}, left out too")
            }
            Self::Kept { why } => write!(f, "kept: {why}"),
        }
    }
}

impl std::fmt::Display for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Reached {
                anchor, changed, ..
            } => write!(f, "{anchor} imports {changed}"),
            Self::Gap {
                specifier, from, ..
            } => write!(
                f,
                "{} imports {specifier:?}, which could not be resolved",
                from.first().map_or("a file", String::as_str)
            ),
            Self::NotImported { project, reason } => {
                write!(
                    f,
                    "{project} is touched as {reason}, which imports do not carry"
                )
            }
            Self::Unanswered { detail } => write!(f, "imports could not be followed: {detail}"),
        }
    }
}

/// Decides each project with `qk:reachability` that `projects` affects only
/// through dependencies, from the touched projects' reasons and the imports of
/// its anchors.
pub(crate) fn decide(
    workspace: &Workspace,
    graph: &ProjectGraph,
    changes: &Changes,
    projects: &BTreeMap<String, Cause>,
) -> Result<BTreeMap<String, Decision>> {
    let mut decisions = BTreeMap::new();
    let mut pending: BTreeMap<String, Settings> = BTreeMap::new();
    // Every project's settings are read, so a mistake is found before the
    // change that would need them.
    let mut declared = BTreeMap::new();
    for (name, project) in &workspace.projects {
        if let Some(settings) = settings(workspace, name, &project.extra, name)? {
            declared.insert(name, settings);
        }
    }
    for (name, cause) in projects {
        if !matches!(cause, Cause::DependsOn { .. }) {
            continue;
        }
        let Some(settings) = declared.remove(name) else {
            continue;
        };
        match not_imported(graph, projects, name, &settings) {
            Some(why) => {
                decisions.insert(name.clone(), Decision::Kept { why });
            }
            None => {
                pending.insert(name.clone(), settings);
            }
        }
    }
    if pending.is_empty() {
        return Ok(decisions);
    }
    let anchors = match anchor_files(workspace, &pending) {
        Ok(anchors) => anchors,
        Err(error) => {
            return Ok(unanswered(decisions, pending, format!("{error:#}")));
        }
    };
    let answers = match search(workspace, changes, anchors.values().flatten()) {
        Ok(answers) => answers,
        Err(detail) => return Ok(unanswered(decisions, pending, detail)),
    };
    for (name, _) in pending {
        let files = &anchors[&name];
        let decision = if files.is_empty() {
            Decision::Kept {
                why: Kept::Unanswered {
                    detail: "its anchors name no file".into(),
                },
            }
        } else if let Some(why) = files.iter().find_map(|anchor| answers[anchor].kept(anchor)) {
            Decision::Kept { why }
        } else {
            Decision::LeftOut {
                anchors: files.clone(),
                changed: changed_through(graph, projects, &name),
            }
        };
        decisions.insert(name, decision);
    }
    Ok(decisions)
}

fn unanswered(
    mut decisions: BTreeMap<String, Decision>,
    pending: BTreeMap<String, Settings>,
    detail: String,
) -> BTreeMap<String, Decision> {
    for name in pending.into_keys() {
        decisions.insert(
            name,
            Decision::Kept {
                why: Kept::Unanswered {
                    detail: detail.clone(),
                },
            },
        );
    }
    decisions
}

/// The touched projects `project` depends on, directly or not.
fn touched_dependencies<'a>(
    graph: &'a ProjectGraph,
    projects: &'a BTreeMap<String, Cause>,
    project: &str,
) -> Vec<(&'a str, &'a [Reason])> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![project];
    let mut touched = Vec::new();
    while let Some(current) = pending.pop() {
        for edge in &graph.dependencies[current] {
            if !seen.insert(edge.target.as_str()) {
                continue;
            }
            pending.push(&edge.target);
            if let Some(Cause::Touched { reasons }) = projects.get(&edge.target) {
                touched.push((edge.target.as_str(), reasons.as_slice()));
            }
        }
    }
    touched
}

/// The first reason touching a dependency that imports do not carry.
fn not_imported(
    graph: &ProjectGraph,
    projects: &BTreeMap<String, Cause>,
    project: &str,
    settings: &Settings,
) -> Option<Kept> {
    for (dependency, reasons) in touched_dependencies(graph, projects, project) {
        for reason in reasons {
            let carried = matches!(reason, Reason::File { file } if settings.is_source(file));
            if !carried {
                return Some(Kept::NotImported {
                    project: dependency.to_owned(),
                    reason: reason.clone(),
                });
            }
        }
    }
    None
}

fn changed_through(
    graph: &ProjectGraph,
    projects: &BTreeMap<String, Cause>,
    project: &str,
) -> Vec<String> {
    touched_dependencies(graph, projects, project)
        .into_iter()
        .flat_map(|(_, reasons)| reasons)
        .filter_map(|reason| match reason {
            Reason::File { file } => Some(file.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Each project's anchor files, as its globs match the workspace's files.
fn anchor_files(
    workspace: &Workspace,
    pending: &BTreeMap<String, Settings>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let files = qk_cache::source_files(&workspace.root)?;
    pending
        .iter()
        .map(|(name, settings)| {
            let patterns = settings
                .anchors
                .iter()
                .map(|anchor| Pattern::new(anchor, false))
                .collect::<Result<Vec<_>>>()?;
            let matched = files
                .iter()
                .filter(|file| patterns.iter().any(|pattern| pattern.is_match(file)))
                .cloned()
                .collect();
            Ok((name.clone(), matched))
        })
        .collect()
}

/// One anchor's answer.
pub(crate) struct Answer {
    /// The chain of imports to a changed file, the anchor first.
    reached: Option<Vec<String>>,
    /// The first import the repository answers for that nothing placed.
    gap: Option<(String, Vec<String>)>,
}

impl Answer {
    fn kept(&self, anchor: &str) -> Option<Kept> {
        if let Some(chain) = &self.reached {
            return Some(Kept::Reached {
                anchor: anchor.to_owned(),
                changed: chain.last().cloned().unwrap_or_default(),
                chain: chain.clone(),
            });
        }
        self.gap.as_ref().map(|(specifier, from)| Kept::Gap {
            anchor: anchor.to_owned(),
            specifier: specifier.clone(),
            from: from.clone(),
        })
    }
}

/// Asks fallout which changed files each anchor imports, over the checkout.
/// An error explains why nothing could be answered.
pub(crate) fn search<'a>(
    workspace: &Workspace,
    changes: &Changes,
    anchors: impl Iterator<Item = &'a String>,
) -> std::result::Result<BTreeMap<String, Answer>, String> {
    let anchors: BTreeSet<&String> = anchors.collect();
    if let Some(detail) = not_checked_out(workspace, changes) {
        return Err(detail);
    }
    let options = fallout::Options {
        anchors: anchors.iter().map(PathBuf::from).collect(),
        changed: changes.files.iter().map(PathBuf::from).collect(),
        diff: None,
        base: changes.base.clone(),
        root: workspace.root.clone(),
        only: Some(fallout::query::Direction::Downstream),
        granularity: fallout::Granularity::File,
        include_types: false,
    };
    let outcomes = fallout::analyse_each(&options).map_err(|error| error.to_string())?;
    let root = fallout::canonical_root(&workspace.root);
    let relative = |path: &Path| relative(&root, &workspace.root, path);
    let mut answers = BTreeMap::new();
    for outcome in outcomes {
        let reached = match &outcome.verdict {
            fallout::Verdict::Affected(hit) => {
                Some(hit.path.iter().map(|path| relative(path)).collect())
            }
            fallout::Verdict::NotAffected => None,
        };
        let gap = outcome
            .unresolved
            .iter()
            .find(|import| import.kind.in_repo())
            .map(|import| {
                (
                    import.specifier.clone(),
                    import.from.iter().map(|path| relative(path)).collect(),
                )
            });
        answers.insert(relative(&outcome.anchor), Answer { reached, gap });
    }
    for anchor in anchors {
        ensure_answered(&answers, anchor)?;
    }
    Ok(answers)
}

fn ensure_answered(
    answers: &BTreeMap<String, Answer>,
    anchor: &str,
) -> std::result::Result<(), String> {
    if answers.contains_key(anchor) {
        Ok(())
    } else {
        Err(format!("fallout gave no answer for {anchor}"))
    }
}

/// `path` relative to the workspace root, with `/` separators.
fn relative(canonical: &Path, root: &Path, path: &Path) -> String {
    let path = path
        .strip_prefix(canonical)
        .or_else(|_| path.strip_prefix(root))
        .unwrap_or(path);
    path.to_string_lossy().replace('\\', "/")
}

/// Why fallout, which reads the checkout, cannot answer for the head
/// revision, or `None` when the checkout is the head.
fn not_checked_out(workspace: &Workspace, changes: &Changes) -> Option<String> {
    let head = changes.head.as_deref()?;
    let resolve = |revision: &str| {
        git_lines(
            &workspace.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{revision}^{{commit}}"),
            ],
        )
        .ok()
        .and_then(|lines| lines.into_iter().next())
    };
    if resolve(head).is_none() || resolve(head) != resolve("HEAD") {
        return Some(format!(
            "the head {head} is not checked out, and imports are read from the checkout"
        ));
    }
    let modified = git_lines(
        &workspace.root,
        &["status", "--porcelain", "--untracked-files=no", "--", "."],
    )
    .unwrap_or_default();
    (!modified.is_empty()).then(|| {
        format!("the checkout has changes the head {head} does not, and imports are read from the checkout")
    })
}
