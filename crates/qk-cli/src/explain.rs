//! `qk show affected`: why projects are affected.

use std::io::Write;

use anyhow::{Result, bail};
use qk_affected::{Analysis, Cause, Reason};
use qk_config::Workspace;
use qk_graph::graph_order;
use serde_json::json;

/// Packages listed per lockfile reason before summarising the rest.
const LISTED: usize = 20;

pub fn explain(
    workspace: &Workspace,
    analysis: &Analysis,
    project: Option<&str>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    if let Some(project) = project
        && !workspace.projects.contains_key(project)
    {
        bail!("unknown project {project:?}");
    }
    match (project, json) {
        (None, true) => {
            serde_json::to_writer_pretty(&mut *out, analysis)?;
            writeln!(out)?;
        }
        (Some(project), true) => {
            let chain = analysis.chain(project);
            let steps: Vec<_> = chain
                .iter()
                .flatten()
                .map(|name| match &analysis.projects[*name] {
                    Cause::DependsOn {
                        project,
                        kind,
                        also,
                    } => {
                        json!({"project": name, "dependsOn": project, "kind": kind, "alsoDependsOn": also})
                    }
                    Cause::Touched { reasons } => json!({"project": name, "reasons": reasons}),
                })
                .collect();
            let mut report = json!({
                "project": project,
                "affected": chain.is_some(),
                "base": analysis.base,
                "head": analysis.head,
                "chain": steps,
            });
            if !analysis.projections.is_empty() {
                report["projections"] = serde_json::to_value(&analysis.projections)?;
                report["originalFiles"] = serde_json::to_value(&analysis.original_files)?;
                report["files"] = serde_json::to_value(&analysis.files)?;
            }
            serde_json::to_writer_pretty(&mut *out, &report)?;
            writeln!(out)?;
        }
        (None, false) => {
            writeln!(out, "{}", comparison(analysis))?;
            explain_projections(analysis, out)?;
            let names = graph_order(&workspace.projects, analysis.projects.keys().cloned());
            let width = names.iter().map(String::len).max().unwrap_or(0);
            for name in names {
                let summary = match &analysis.projects[&name] {
                    Cause::Touched { reasons } => {
                        let more = match reasons.len() {
                            1 => String::new(),
                            count => format!(" (and {} more)", count - 1),
                        };
                        format!("touched: {}{more}", reasons[0])
                    }
                    Cause::DependsOn { project, .. } => format!("depends on {project}"),
                };
                writeln!(out, "{name:width$}  {summary}")?;
            }
        }
        (Some(project), false) => {
            writeln!(out, "{}", comparison(analysis))?;
            explain_projections(analysis, out)?;
            let Some(chain) = analysis.chain(project) else {
                writeln!(out, "{project} is not affected.")?;
                return Ok(());
            };
            let touched = chain.last().expect("a chain has its project");
            if chain.len() == 1 {
                writeln!(out, "{project} is affected because it is touched:")?;
            } else {
                writeln!(
                    out,
                    "{project} is affected because it depends on {touched}:"
                )?;
                for pair in chain.windows(2) {
                    let Cause::DependsOn { kind, .. } = &analysis.projects[pair[0]] else {
                        unreachable!("only the last step of a chain is touched");
                    };
                    writeln!(out, "  {} -> {} ({kind})", pair[0], pair[1])?;
                }
                if let Cause::DependsOn { also, .. } = &analysis.projects[project]
                    && !also.is_empty()
                {
                    writeln!(
                        out,
                        "  {project} also depends on affected {} (see `qk show affected <project>`)",
                        also.join(", ")
                    )?;
                }
                writeln!(out, "{touched} is touched:")?;
            }
            let Cause::Touched { reasons } = &analysis.projects[*touched] else {
                unreachable!("a chain ends at a touched project");
            };
            for reason in reasons {
                writeln!(out, "  {reason}")?;
                if let Reason::Installs {
                    direct,
                    added,
                    removed,
                    changed,
                    ..
                } = reason
                {
                    let lines = installs(direct, added, removed, changed);
                    for line in lines.iter().take(LISTED) {
                        writeln!(out, "      {line}")?;
                    }
                    if lines.len() > LISTED {
                        writeln!(out, "      ... {} more packages", lines.len() - LISTED)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn explain_projections(analysis: &Analysis, out: &mut impl Write) -> Result<()> {
    for projection in &analysis.projections {
        writeln!(
            out,
            "Projection {}: {}; {} source change{}, {} artifact change{}{}.",
            projection.name,
            projection.status,
            projection.sources.len(),
            if projection.sources.len() == 1 {
                ""
            } else {
                "s"
            },
            projection.artifacts.len(),
            if projection.artifacts.len() == 1 {
                ""
            } else {
                "s"
            },
            projection
                .detail
                .as_ref()
                .map(|detail| format!("; {detail}"))
                .unwrap_or_default()
        )?;
        for path in &projection.artifacts {
            writeln!(out, "  {path}")?;
        }
    }
    Ok(())
}

fn comparison(analysis: &Analysis) -> String {
    let mut text = comparison_of(
        analysis
            .original_files
            .as_ref()
            .map_or(analysis.files.len(), Vec::len),
        analysis.base.as_deref(),
        analysis.head.as_deref(),
        analysis.range.as_ref(),
    );
    if analysis.original_files.is_some() {
        text.push_str(&format!(
            "\n{} {} after projection.",
            analysis.files.len(),
            if analysis.files.len() == 1 {
                "file"
            } else {
                "files"
            }
        ));
    }
    text
}

/// What was compared, then how the base was found and a warning when the
/// range holds commits that already landed.
fn comparison_of(
    count: usize,
    base: Option<&str>,
    head: Option<&str>,
    range: Option<&qk_affected::Range>,
) -> String {
    let short = |revision: &str| revision.chars().take(12).collect::<String>();
    let files = if count == 1 { "file" } else { "files" };
    let mut text = format!(
        "{count} changed {files} between {} and {}.",
        base.map_or("(no base)".into(), short),
        head.map_or("the working tree".into(), short)
    );
    if let Some(range) = range {
        let left = match &range.upstream {
            Some(upstream) => format!("{upstream}, newer than {}", range.requested),
            None => range.requested.clone(),
        };
        let commits = if range.commits == 1 {
            "commit"
        } else {
            "commits"
        };
        text.push_str(&format!(
            "\nThe base is where {} left {left}, {} {commits} back{}.",
            head.map_or("HEAD".into(), short),
            range.commits,
            if head.is_none() {
                ", and the working tree counts too"
            } else {
                ""
            }
        ));
        if let Some(warning) = landed_warning(range) {
            text.push_str(&format!("\nwarning: {warning}"));
        }
    }
    text
}

/// A warning when the compared commits include some already on the default
/// branch, whose changes then count as affected.
pub fn landed_warning(range: &qk_affected::Range) -> Option<String> {
    let branch = range.default_branch.as_deref()?;
    (range.landed > 0).then(|| {
        format!(
            "{} of the {} commits compared are already on {branch}, so what they changed counts as affected; `--base {branch}` compares from where this branch left it",
            range.landed, range.commits
        )
    })
}

/// A lockfile reason by package, most telling first: versions that moved,
/// packages added or removed, then those that kept their version but changed
/// peers or patch, and last those whose dependencies changed, which usually
/// follow from the others. Direct dependencies are marked.
fn installs(
    direct: &[String],
    added: &[String],
    removed: &[String],
    changed: &[String],
) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};
    let direct: BTreeSet<&str> = direct
        .iter()
        .filter_map(|line| line.split_once(':').map(|(name, _)| name))
        .collect();
    #[derive(Default)]
    struct Versions<'a> {
        before: BTreeSet<&'a str>,
        after: BTreeSet<&'a str>,
    }
    let mut packages: BTreeMap<&str, Versions> = BTreeMap::new();
    for key in removed {
        let (name, version) = split(key);
        packages.entry(name).or_default().before.insert(version);
    }
    for key in added {
        let (name, version) = split(key);
        packages.entry(name).or_default().after.insert(version);
    }
    for key in changed {
        packages.entry(split(key).0).or_default();
    }
    let mut lines: Vec<_> = packages
        .into_iter()
        .map(|(name, versions)| {
            let list = |set: &BTreeSet<&str>| set.iter().copied().collect::<Vec<_>>().join(", ");
            let (rank, what) = if versions.before.is_empty() && versions.after.is_empty() {
                (3, "dependencies or integrity changed".to_owned())
            } else if versions.before.is_empty() {
                (1, format!("added {}", list(&versions.after)))
            } else if versions.after.is_empty() {
                (1, format!("removed {}", list(&versions.before)))
            } else if versions.before == versions.after {
                (
                    2,
                    format!("{}: peers or patch changed", list(&versions.after)),
                )
            } else {
                (
                    0,
                    format!("{} -> {}", list(&versions.before), list(&versions.after)),
                )
            };
            let marker = if direct.contains(name) {
                " (direct)"
            } else {
                ""
            };
            (rank, format!("{name}{marker}: {what}"))
        })
        .collect();
    lines.sort();
    lines.into_iter().map(|(_, line)| line).collect()
}

/// A snapshot key's package name and version, without peers or patch.
fn split(key: &str) -> (&str, &str) {
    let key = &key[..key.find('(').unwrap_or(key.len())];
    let start = usize::from(key.starts_with('@'));
    match key[start..].find('@') {
        Some(at) => (&key[..start + at], &key[start + at + 1..]),
        None => (key, ""),
    }
}

/// `qk show tasks`: the planned tasks, or with an analysis the affected ones
/// and why.
pub fn tasks(
    _workspace: &Workspace,
    graph: &qk_taskgraph::TaskGraph,
    analysis: Option<&qk_affected::TaskAnalysis>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let Some(analysis) = analysis else {
        let ids: Vec<&String> = graph.tasks.keys().collect();
        if json {
            serde_json::to_writer(&mut *out, &ids)?;
            writeln!(out)?;
        } else {
            for id in ids {
                writeln!(out, "{id}")?;
            }
        }
        return Ok(());
    };
    if json {
        serde_json::to_writer_pretty(&mut *out, analysis)?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(
        out,
        "{}",
        comparison_of(
            analysis.files.len(),
            analysis.base.as_deref(),
            analysis.head.as_deref(),
            analysis.range.as_ref(),
        )
    )?;
    let width = analysis.tasks.keys().map(String::len).max().unwrap_or(0);
    for (id, cause) in &analysis.tasks {
        let summary = match cause {
            qk_affected::TaskCause::Touched { reasons } => {
                let more = match reasons.len() {
                    1 => String::new(),
                    count => format!(" (and {} more)", count - 1),
                };
                format!("{}{more}", reasons[0])
            }
            qk_affected::TaskCause::DependsOn { task } => format!("depends on {task}"),
        };
        writeln!(out, "{id:width$}  {summary}")?;
    }
    Ok(())
}
