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

use crate::tasks::{TaskCause, TaskReason};
use crate::{Cause, Changes, Reason, git_lines};
use qk_taskgraph::TaskGraph;

/// A project's or target's `qk:reachability`, its paths expanded.
pub(crate) struct Settings {
    pub anchors: Globs,
    pub cases: Globs,
    sources: Globs,
}

impl Settings {
    /// Whether a change to `file` matters only through imports.
    pub fn is_source(&self, file: &str) -> bool {
        file.rsplit('/').next() != Some("package.json") && self.sources.is_match(file)
    }
}

/// Globs, with `!` exclusions: a path matches when an inclusion matches it
/// and no exclusion does, whatever their order.
#[derive(Default)]
pub(crate) struct Globs {
    included: Vec<Pattern>,
    excluded: Vec<Pattern>,
}

impl Globs {
    fn new(patterns: &[String]) -> Result<Self> {
        let mut globs = Self::default();
        for pattern in patterns {
            match pattern.strip_prefix('!') {
                Some(excluded) => globs.excluded.push(Pattern::new(excluded, true)?),
                None => globs.included.push(Pattern::new(pattern, false)?),
            }
        }
        Ok(globs)
    }

    /// Whether nothing can match.
    pub fn is_empty(&self) -> bool {
        self.included.is_empty()
    }

    pub fn is_match(&self, path: &str) -> bool {
        self.included.iter().any(|pattern| pattern.is_match(path))
            && !self.excluded.iter().any(|pattern| pattern.is_match(path))
    }
}

/// Reads `qk:reachability` from `extra`, expanding paths for `project`.
/// `what` names it in errors; `allow_cases` says whether cases may be declared,
/// which only a target can.
pub(crate) fn settings(
    workspace: &Workspace,
    project: &str,
    extra: &BTreeMap<String, Value>,
    what: &str,
    allow_cases: bool,
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
            "cases" if allow_cases => "cases",
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
                let path = path.as_str().unwrap();
                let (bang, path) = path
                    .strip_prefix('!')
                    .map_or(("", path), |path| ("!", path));
                qk_cache::expand_project_path(workspace, project, path)
                    .map(|path| format!("{bang}{path}"))
                    .with_context(|| format!("{what} qk:reachability.{key}"))
            })
            .collect::<Result<_>>()?;
        lists.insert(key, expanded);
    }
    let anchors = Globs::new(&lists.remove("anchors").unwrap_or_default())?;
    let cases = Globs::new(&lists.remove("cases").unwrap_or_default())?;
    let sources = Globs::new(&lists.remove("sources").unwrap_or_default())?;
    ensure!(
        !anchors.is_empty() || !cases.is_empty(),
        "{what} qk:reachability needs anchors{}",
        if allow_cases { " or cases" } else { "" }
    );
    ensure!(
        !sources.is_empty(),
        "{what} qk:reachability needs sources: without them no change is carried only by imports"
    );
    Ok(Some(Settings {
        anchors,
        cases,
        sources,
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
    /// The task is affected by a change imports do not carry.
    Input { reason: TaskReason },
    /// The task depends on an affected task, whose outputs it may read.
    Dependency { task: String },
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
            Self::Input { reason } => write!(f, "{reason}, which imports do not carry"),
            Self::Dependency { task } => write!(f, "it depends on affected {task}"),
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
        if let Some(settings) = settings(workspace, name, &project.extra, name, false)? {
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
        match not_imported(changes, graph, projects, name, &settings) {
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
    changes: &Changes,
    graph: &ProjectGraph,
    projects: &BTreeMap<String, Cause>,
    project: &str,
    settings: &Settings,
) -> Option<Kept> {
    for (dependency, reasons) in touched_dependencies(graph, projects, project) {
        for reason in reasons {
            let carried = matches!(
                reason,
                Reason::File { file } if settings.is_source(file) || manifest_unchanged(changes, file)
            );
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

/// Whether `file` is a `package.json` whose change decides nothing new for
/// its dependents: the same at both revisions once the fields no dependent
/// reads, such as `version` and `scripts`, are left out, as their cache keys
/// read it. Its declarations stay in, so a changed dependency still counts.
fn manifest_unchanged(changes: &Changes, file: &str) -> bool {
    if file.rsplit('/').next() != Some("package.json") {
        return false;
    }
    let read = |revision: Option<&str>| {
        changes
            .read(file, revision)
            .and_then(|text| qk_cache::manifest_for_dependents(&text, false))
    };
    match (read(changes.base.as_deref()), read(changes.head.as_deref())) {
        (Some(before), Some(after)) => before == after,
        _ => false,
    }
}

/// The workspace's files each glob of `patterns` matches.
fn matching(files: &BTreeSet<String>, globs: &Globs) -> Vec<String> {
    files
        .iter()
        .filter(|file| globs.is_match(file))
        .cloned()
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
        .map(|(name, settings)| Ok((name.clone(), matching(&files, &settings.anchors))))
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
    let resolved = resolve(head);
    if resolved.is_none() || resolved != resolve("HEAD") {
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

/// A task's anchor and case files.
type Matched = (Vec<String>, Vec<String>);

/// What the profile decided for a task with `qk:reachability` that ordinary
/// task selection affects.
#[derive(Debug, Serialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum TaskDecision {
    /// Neither its anchors nor its cases import a change.
    LeftOut {
        anchors: Vec<String>,
        cases: Vec<String>,
        /// The changed inputs, all of them sources.
        changed: Vec<String>,
    },
    /// Only some cases import a change, each with why it runs.
    Cases {
        cases: BTreeMap<String, Kept>,
        /// How many cases the task has.
        of: usize,
    },
    /// Affected only through tasks the profile left out.
    Through { task: String },
    /// The whole task runs.
    Whole { why: Kept },
}

impl std::fmt::Display for TaskDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::LeftOut { .. } => write!(f, "left out: nothing it imports changed"),
            Self::Cases { cases, of } => {
                write!(f, "{} of {of} cases import a change", cases.len())
            }
            Self::Through { task } => {
                write!(f, "left out: affected only through {task}, left out too")
            }
            Self::Whole { why } => write!(f, "whole task: {why}"),
        }
    }
}

/// Decides each task with `qk:reachability` that `tasks` affects, from its
/// reasons and the imports of its anchors and cases.
pub(crate) fn decide_tasks(
    workspace: &Workspace,
    graph: &TaskGraph,
    changes: &Changes,
    tasks: &BTreeMap<String, TaskCause>,
) -> Result<BTreeMap<String, TaskDecision>> {
    let mut decisions = BTreeMap::new();
    // Every task's settings are read, so a mistake is found before the change
    // that would need them.
    let mut declared = BTreeMap::new();
    for (id, task) in &graph.tasks {
        let extra = &task.definition.extra;
        if let Some(settings) = settings(workspace, &task.project, extra, id, true)? {
            declared.insert(id, settings);
        }
    }
    let mut pending: BTreeMap<String, (Settings, Vec<String>)> = BTreeMap::new();
    for (id, cause) in tasks {
        let Some(settings) = declared.remove(id) else {
            continue;
        };
        let reasons = match cause {
            TaskCause::DependsOn { task } => {
                let why = Kept::Dependency { task: task.clone() };
                decisions.insert(id.clone(), TaskDecision::Whole { why });
                continue;
            }
            TaskCause::Touched { reasons } => reasons,
        };
        let carried = |reason: &&TaskReason| {
            matches!(
                reason,
                TaskReason::Input { file }
                    if settings.is_source(file) || manifest_unchanged(changes, file)
            )
        };
        if let Some(reason) = reasons.iter().find(|reason| !carried(reason)) {
            let why = Kept::Input {
                reason: reason.clone(),
            };
            decisions.insert(id.clone(), TaskDecision::Whole { why });
            continue;
        }
        let changed = reasons
            .iter()
            .filter_map(|reason| match reason {
                TaskReason::Input { file } => Some(file.clone()),
                _ => None,
            })
            .collect();
        pending.insert(id.clone(), (settings, changed));
    }
    if pending.is_empty() {
        return Ok(decisions);
    }
    let found = (|| -> Result<BTreeMap<String, Matched>> {
        let files = qk_cache::source_files(&workspace.root)?;
        pending
            .iter()
            .map(|(id, (settings, _))| {
                Ok((
                    id.clone(),
                    (
                        matching(&files, &settings.anchors),
                        matching(&files, &settings.cases),
                    ),
                ))
            })
            .collect()
    })();
    let answers = found
        .map_err(|error| format!("{error:#}"))
        .and_then(|found| {
            let answers = search(
                workspace,
                changes,
                found
                    .values()
                    .flat_map(|(anchors, cases)| anchors.iter().chain(cases)),
            )?;
            Ok((found, answers))
        });
    let (mut found, answers) = match answers {
        Ok(answered) => answered,
        Err(detail) => {
            for id in pending.into_keys() {
                let why = Kept::Unanswered {
                    detail: detail.clone(),
                };
                decisions.insert(id, TaskDecision::Whole { why });
            }
            return Ok(decisions);
        }
    };
    for (id, (settings, changed)) in pending {
        let (anchors, cases) = found.remove(&id).expect("every pending task is matched");
        let named = if settings.cases.is_empty() {
            !anchors.is_empty()
        } else {
            !cases.is_empty()
        };
        let decision = if !named {
            TaskDecision::Whole {
                why: Kept::Unanswered {
                    detail: format!(
                        "its {} name no file",
                        if settings.cases.is_empty() {
                            "anchors"
                        } else {
                            "cases"
                        }
                    ),
                },
            }
        } else if let Some(why) = anchors
            .iter()
            .find_map(|anchor| answers[anchor].kept(anchor))
        {
            TaskDecision::Whole { why }
        } else {
            let selected: BTreeMap<String, Kept> = cases
                .iter()
                .filter_map(|case| Some((case.clone(), answers[case].kept(case)?)))
                .collect();
            if selected.is_empty() {
                TaskDecision::LeftOut {
                    anchors,
                    cases,
                    changed,
                }
            } else {
                TaskDecision::Cases {
                    of: cases.len(),
                    cases: selected,
                }
            }
        };
        decisions.insert(id, decision);
    }
    Ok(decisions)
}
