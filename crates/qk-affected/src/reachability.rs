//! Opt-in import reachability: under an affected profile with `reachability`,
//! a task is left out when none of its anchors or cases imports what changed,
//! and a project when every task of it that ordinary selection affects is.
//!
//! A task is narrowed by `qk:reachability` settings: `anchors`, the files
//! whose imports it depends on through, `cases`, the files it runs separately,
//! and `sources`, the changed files that matter to it only through imports.
//! Any other change it is affected by keeps it, because no import carries it.
//! A target declares the settings, names a config from nx.json's
//! `qk:reachability`, or takes the profile's default. Imports are followed by
//! fallout, over the checkout.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use qk_cache::Pattern;
use qk_config::Workspace;
use serde::Serialize;
use serde_json::Value;

use crate::projections::Reach;
use crate::tasks::{TaskAnalysis, TaskCause, TaskReason};
use crate::{Cause, Changes, git_lines};
use qk_taskgraph::{Task, TaskGraph};

/// A task's `qk:reachability`, its paths expanded.
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

/// One `qk:reachability` object, checked, its paths not yet expanded for a
/// project.
#[derive(Clone, Default)]
struct Declared {
    anchors: Vec<String>,
    cases: Vec<String>,
    sources: Vec<String>,
}

/// Reads a `qk:reachability` object; `what` names it in errors.
fn declared(value: &Value, what: &str) -> Result<Declared> {
    let object = value
        .as_object()
        .with_context(|| format!("{what} must be an object"))?;
    let mut declared = Declared::default();
    for (key, value) in object {
        let list = match key.as_str() {
            "anchors" => &mut declared.anchors,
            "cases" => &mut declared.cases,
            "sources" => &mut declared.sources,
            _ => bail!("unknown {what} field {key:?}"),
        };
        *list = value
            .as_array()
            .filter(|paths| paths.iter().all(Value::is_string))
            .with_context(|| format!("{what}.{key} must be an array of paths"))?
            .iter()
            .map(|path| path.as_str().unwrap().to_owned())
            .collect();
    }
    let includes = |paths: &[String]| paths.iter().any(|path| !path.starts_with('!'));
    ensure!(
        includes(&declared.anchors) || includes(&declared.cases),
        "{what} needs anchors or cases"
    );
    ensure!(
        includes(&declared.sources),
        "{what} needs sources: without them no change is carried only by imports"
    );
    Ok(declared)
}

impl Declared {
    /// The settings with paths expanded for `project`.
    fn expand(&self, workspace: &Workspace, project: &str, what: &str) -> Result<Settings> {
        let globs = |paths: &[String], key: &str| -> Result<Globs> {
            let expanded: Vec<String> = paths
                .iter()
                .map(|path| {
                    let (bang, path) = path
                        .strip_prefix('!')
                        .map_or(("", path.as_str()), |path| ("!", path));
                    qk_cache::expand_project_path(workspace, project, path)
                        .map(|path| format!("{bang}{path}"))
                        .with_context(|| format!("{what}.{key}"))
                })
                .collect::<Result<_>>()?;
            Globs::new(&expanded)
        };
        Ok(Settings {
            anchors: globs(&self.anchors, "anchors")?,
            cases: globs(&self.cases, "cases")?,
            sources: globs(&self.sources, "sources")?,
        })
    }
}

/// The configs nx.json names in `qk:reachability`, and the one the profile
/// gives tasks that declare none.
pub(crate) struct Configs {
    named: BTreeMap<String, Declared>,
    default: Option<String>,
}

impl Configs {
    pub fn load(workspace: &Workspace, reach: Reach) -> Result<Self> {
        for (name, project) in &workspace.projects {
            ensure!(
                !project.extra.contains_key("qk:reachability"),
                "project {name} declares qk:reachability, which is read from targets: declare it on \
                 its targets, name a config from nx.json's qk:reachability there, or give the \
                 affected profile a default"
            );
        }
        let mut named = BTreeMap::new();
        if let Some(value) = workspace.config.extra.get("qk:reachability") {
            let configs = value
                .as_object()
                .context("nx.json qk:reachability must map config names to settings")?;
            for (name, value) in configs {
                let what = format!("nx.json qk:reachability.{name}");
                named.insert(name.clone(), declared(value, &what)?);
            }
        }
        if let Some(default) = &reach.default {
            ensure!(
                named.contains_key(default),
                "the affected profile's reachability default {default:?} is not a config in nx.json's qk:reachability"
            );
        }
        let configs = Self {
            named,
            default: reach.default,
        };
        // Every target is read, so a mistake is found before the change that
        // would need it.
        for (name, project) in &workspace.projects {
            for (target, definition) in &project.targets {
                let id = format!("{name}:{target}");
                configs.settings(workspace, &id, name, &definition.extra)?;
            }
        }
        Ok(configs)
    }

    /// Whether a target with `extra` is narrowed by anything, as its settings
    /// were found valid when the configs were loaded.
    pub fn narrows(&self, extra: &BTreeMap<String, Value>) -> bool {
        match extra.get("qk:reachability") {
            None => self.default.is_some(),
            Some(value) => value != &Value::Bool(false),
        }
    }

    /// What `task` is narrowed by: its own settings, the config it names, or
    /// the default. `false` declines the default.
    fn task(&self, workspace: &Workspace, id: &str, task: &Task) -> Result<Option<Settings>> {
        self.settings(workspace, id, &task.project, &task.definition.extra)
    }

    fn settings(
        &self,
        workspace: &Workspace,
        id: &str,
        project: &str,
        extra: &BTreeMap<String, Value>,
    ) -> Result<Option<Settings>> {
        let what = format!("{id} qk:reachability");
        let declared = match extra.get("qk:reachability") {
            None => match &self.default {
                Some(name) => Cow::Borrowed(&self.named[name]),
                None => return Ok(None),
            },
            Some(Value::Bool(false)) => return Ok(None),
            Some(Value::String(name)) => {
                Cow::Borrowed(self.named.get(name).with_context(|| {
                    format!(
                        "{what} names {name:?}, which is not a config in nx.json's qk:reachability"
                    )
                })?)
            }
            Some(value @ Value::Object(_)) => Cow::Owned(declared(value, &what)?),
            Some(_) => bail!("{what} must name a config, be false or be an object"),
        };
        declared.expand(workspace, project, &what).map(Some)
    }
}

/// What the profile decided for a project ordinary selection affects, from
/// its tasks.
#[derive(Debug, Serialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum Decision {
    /// Every task of it that ordinary selection affects is left out.
    LeftOut {
        tasks: BTreeMap<String, TaskDecision>,
    },
    /// A task of it still runs.
    Kept {
        task: String,
        /// What the profile decided for that task, absent when it is not
        /// narrowed.
        #[serde(rename = "taskDecision", skip_serializing_if = "Option::is_none")]
        decision: Option<TaskDecision>,
    },
}

/// Why a task with `qk:reachability` runs, or a case of it.
#[derive(Clone, Debug, Serialize)]
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
            Self::LeftOut { tasks } => {
                let count = tasks.len();
                let noun = if count == 1 { "task" } else { "tasks" };
                write!(f, "left out: none of its {count} affected {noun} runs")
            }
            Self::Kept {
                task,
                decision: Some(decision),
            } => write!(f, "kept: {task} runs ({decision})"),
            Self::Kept {
                task,
                decision: None,
            } => write!(f, "kept: {task} runs, not narrowed by reachability"),
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
            Self::Input { reason } => write!(f, "{reason}, which imports do not carry"),
            Self::Dependency { task } => write!(f, "it depends on affected {task}"),
            Self::Unanswered { detail } => write!(f, "imports could not be followed: {detail}"),
        }
    }
}

/// Decides each project in `projects` by its tasks in `graph`, as `tasks`
/// selected them. A project is left out when the profile left out every task
/// of it that ordinary selection affects; one with no such task, or none the
/// profile decided, is not decided.
pub(crate) fn decide_projects(
    graph: &TaskGraph,
    tasks: &TaskAnalysis,
    projects: &BTreeMap<String, Cause>,
) -> BTreeMap<String, Decision> {
    let mut by_project: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, task) in &graph.tasks {
        let affected = tasks.tasks.contains_key(id) || tasks.reachability.contains_key(id);
        if affected && projects.contains_key(&task.project) {
            by_project.entry(&task.project).or_default().push(id);
        }
    }
    let mut decisions = BTreeMap::new();
    for (project, ids) in by_project {
        if !ids.iter().any(|id| tasks.reachability.contains_key(*id)) {
            continue;
        }
        let decision = match ids.iter().find(|id| tasks.tasks.contains_key(**id)) {
            Some(id) => Decision::Kept {
                task: (*id).to_owned(),
                decision: tasks.reachability.get(*id).cloned(),
            },
            None => Decision::LeftOut {
                tasks: ids
                    .iter()
                    .map(|id| ((*id).to_owned(), tasks.reachability[*id].clone()))
                    .collect(),
            },
        };
        decisions.insert(project.to_owned(), decision);
    }
    decisions
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

/// What the profile decided for a task narrowed by reachability that ordinary
/// task selection affects.
#[derive(Clone, Debug, Serialize)]
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

/// Decides each task narrowed by reachability that `tasks` affects, from its
/// reasons and the imports of its anchors and cases.
pub(crate) fn decide_tasks(
    workspace: &Workspace,
    graph: &TaskGraph,
    changes: &Changes,
    tasks: &BTreeMap<String, TaskCause>,
    configs: &Configs,
) -> Result<BTreeMap<String, TaskDecision>> {
    let mut decisions = BTreeMap::new();
    // Every task's settings are read, so a mistake is found before the change
    // that would need them.
    let mut declared = BTreeMap::new();
    for (id, task) in &graph.tasks {
        if let Some(settings) = configs.task(workspace, id, task)? {
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
