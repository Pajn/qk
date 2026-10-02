//! Presentation and compatible recording unions; configuration is never edited.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use qk_input_analysis::{Category, Report};

#[derive(Clone)]
pub struct Options {
    pub report: Option<PathBuf>,
    pub suggestions: Option<PathBuf>,
    pub previous: Vec<Report>,
}

pub fn load_previous(paths: &[PathBuf]) -> Result<Vec<Report>> {
    paths
        .iter()
        .map(|path| {
            let report: Report =
                serde_json::from_slice(&std::fs::read(path).with_context(|| {
                    format!("cannot read previous recording {}", path.display())
                })?)
                .with_context(|| format!("invalid input-analysis report {}", path.display()))?;
            if report.schema_version != 1 {
                bail!(
                    "unsupported input-analysis schema {} in {}",
                    report.schema_version,
                    path.display()
                );
            }
            Ok(report)
        })
        .collect()
}

pub fn present(mut report: Report, options: &Options) -> Result<()> {
    let mut lines = vec![
        "Input analysis (advisory; all tasks executed without result or warm-state caching)"
            .to_owned(),
    ];
    for (id, task) in &mut report.tasks {
        for previous in &options.previous {
            if let Some(previous) = previous.tasks.get(id)
                && !task.merge(previous)
            {
                lines.push(format!("  {id}: previous recording has different arguments, definition or platform; not merged"));
            }
        }
        lines.push(format!(
            "\n{id} — {}; {} recording, partial coverage",
            task.outcome, task.coverage.backend
        ));
        lines.push(format!("  {} declared files; {} observed keyed files in this run; {} accesses outside file-input coverage",
            task.declared_files.len(), task.observed_inputs.len(), task.uncovered_accesses.len()));
        for (paths, label, limit) in [
            (
                &task.access_review.content_reads,
                "uncovered potential content-read paths",
                20,
            ),
            (
                &task.access_review.manifest_reads,
                "workspace package-manifest reads (review package resolution)",
                3,
            ),
            (
                &task.access_review.toolchain_reads,
                "toolchain configuration reads",
                5,
            ),
            (
                &task.access_review.metadata_checks,
                "metadata-only checks (content was not observed)",
                3,
            ),
        ] {
            if paths.is_empty() {
                continue;
            }
            lines.push(format!("  {} {label}", paths.len()));
            for path in paths.iter().take(limit) {
                lines.push(format!("    {path}"));
            }
            if paths.len() > limit {
                lines.push("    … more paths in the JSON report".into());
            }
        }
        for (category, label) in [
            (
                Category::Discovery,
                "directory discovery or absent-path checks",
            ),
            (
                Category::InstalledPackage,
                "installed-package paths (package coverage needs review)",
            ),
            (Category::DependencyOutput, "dependency output paths"),
            (Category::OwnOutput, "own output paths"),
            (Category::Generated, "paths with write access before a read"),
            (Category::GitState, "Git state paths"),
            (Category::External, "external paths"),
        ] {
            let paths: std::collections::BTreeSet<_> = task
                .accesses
                .iter()
                .filter(|event| event.category == category)
                .map(|event| &event.path)
                .collect();
            if !paths.is_empty() {
                lines.push(format!("  {} {label}", paths.len()));
            }
        }
        if !task.successful_runs.is_empty() {
            lines.push(format!(
                "  {} declared files not observed across {} successful recording(s)",
                task.unobserved_inputs.len(),
                task.successful_runs.len()
            ));
            for path in task.unobserved_inputs.iter().take(10) {
                lines.push(format!("    review: {path}"));
            }
        }
        for suggestion in task.glob_suggestions.iter().take(12) {
            lines.push(format!(
                "  Suggested glob: {} ({} observed; {} additional declared files retained; {})",
                suggestion.fileset,
                suggestion.observed_files,
                suggestion.additional_declared_files,
                suggestion.reason
            ));
        }
        if task.glob_suggestions.len() > 12 {
            lines.push("  … more globs in the JSON report".into());
        }
        for note in &task.suggestion_notes {
            lines.push(format!("  Suggestion: {note}"));
        }
        if let Some(fragment) = &task.configuration_fragment {
            lines.push(
                "  Review-only project.json fragment (target-wide; validate every configuration):"
                    .into(),
            );
            for line in serde_json::to_string_pretty(fragment)?.lines() {
                lines.push(format!("    {line}"));
            }
        }
        for limitation in &task.coverage.limitations {
            lines.push(format!("  Coverage: {limitation}"));
        }
        for diagnostic in &task.coverage.diagnostics {
            lines.push(format!("  Recording issue: {diagnostic}"));
        }
    }
    lines.push("\nNot observed does not mean unnecessary. Test other configurations and clean builds before narrowing inputs; keep env, runtime and dependency inputs.".into());
    if let Some(path) = &options.suggestions {
        let fragments: std::collections::BTreeMap<_, _> = report
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                task.configuration_fragment
                    .as_ref()
                    .map(|fragment| (id, fragment))
            })
            .collect();
        let reviews: std::collections::BTreeMap<_, _> = report
            .tasks
            .iter()
            .map(|(id, task)| {
                (
                    id,
                    serde_json::json!({
                        "notes": task.suggestion_notes,
                        "globs": task.glob_suggestions,
                        "uncovered": task.access_review,
                    }),
                )
            })
            .collect();
        let document = serde_json::json!({
            "reviewRequired": true,
            "notes": ["Fragments replace target inputs in each project's configuration; validate every configuration and clean builds.", "Partial observations do not prove unused inputs. Uncovered accesses and directory discovery need separate review.", "Only broad project filesets are replaced; specific filesets, dependency and non-file inputs and exclusions are retained."],
            "tasks": fragments,
            "reviews": reviews
        });
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        std::fs::write(path, bytes)
            .with_context(|| format!("cannot write input suggestions {}", path.display()))?;
        lines.push(format!("Review-only input suggestions: {}", path.display()));
    }
    if let Some(path) = &options.report {
        let mut bytes = serde_json::to_vec_pretty(&report)?;
        bytes.push(b'\n');
        std::fs::write(path, bytes)
            .with_context(|| format!("cannot write input-analysis report {}", path.display()))?;
        lines.push(format!("Input-analysis report: {}", path.display()));
    }
    qk_executor::report::write_all(true, format!("\n{}\n", lines.join("\n")).as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use qk_input_analysis::TaskAnalysis;
    use serde_json::json;

    #[test]
    fn recursive_suggestions_match_future_files_with_the_cache_glob_engine() {
        let mut task: TaskAnalysis = serde_json::from_value(json!({
            "context": {"projectRoot": "apps/web"}, "outcome": "success",
            "coverage": {"backend": "fixture", "partial": true, "limitations": [], "diagnostics": []},
            "declaredFiles": ["apps/web/public/logo.svg", "apps/web/src/a.ts", "apps/web/src/b.tsx"],
            "mandatoryFiles": [], "observedInputs": [], "uncoveredAccesses": [],
            "unobservedInputs": [], "successfulRuns": [], "successfulObservations": [], "candidateFilesets": [],
            "declaration": {"target": "build", "inputs": ["default"], "namedInputs": {}},
            "accesses": [
                {"process": 1, "path": "apps/web/public/logo.svg", "operation": "openRead", "result": "success", "category": "input"},
                {"process": 1, "path": "apps/web/src/a.ts", "operation": "openRead", "result": "success", "category": "input"},
                {"process": 1, "path": "apps/web/public", "operation": "listDirectory", "result": "success", "category": "discovery"}
            ]
        })).unwrap();
        task.summarize("recording");
        let patterns: Vec<_> = task.configuration_fragment.unwrap()["targets"]["build"]["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|input| {
                qk_cache::Pattern::new(
                    &input.as_str().unwrap().replace("{projectRoot}", "apps/web"),
                    false,
                )
                .unwrap()
            })
            .collect();
        for path in [
            "apps/web/public/new.json",
            "apps/web/public/nested/image.png",
            "apps/web/src/new/Component.tsx",
            "apps/web/src/b.tsx",
        ] {
            assert!(
                patterns.iter().any(|pattern| pattern.is_match(path)),
                "{path}"
            );
        }
        for path in ["apps/web/dist/output.js", "apps/other/src/a.ts"] {
            assert!(
                !patterns.iter().any(|pattern| pattern.is_match(path)),
                "{path}"
            );
        }
    }
}
