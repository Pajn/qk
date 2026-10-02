//! Per-task file recording. Coverage is always explicit; no cache keys change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_executor::{Outcome, PreparedTask};
use qk_input_analysis::{Access, Category, Coverage, Declaration, Operation, Report, TaskAnalysis};
use qk_taskgraph::TaskGraph;

use crate::sandbox::Watcher;

mod strace;

pub struct Recorder {
    root: PathBuf,
    directory: tempfile::TempDir,
    watcher: Option<Watcher>,
    program: Option<PathBuf>,
    declared: qk_cache::Resolution,
    started: std::time::SystemTime,
    symlinks: BTreeMap<String, PathBuf>,
    tag_prefix: String,
}

impl Recorder {
    pub fn start(
        workspace: &Workspace,
        graph: &TaskGraph,
        prepared: &mut BTreeMap<String, PreparedTask>,
    ) -> Result<Self> {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            bail!("input analysis needs macOS (Seatbelt) or Linux (strace)");
        }
        if graph
            .tasks
            .values()
            .any(|task| task.definition.continuous == Some(true))
            || prepared.values().any(|task| !task.ready_when.is_empty())
        {
            bail!("input analysis requires finite tasks without readyWhen");
        }
        let root = workspace.root.canonicalize()?;
        let directory = tempfile::Builder::new()
            .prefix("qk-input-analysis-")
            .tempdir()?;
        let tag_prefix = format!(
            "recording:{}:",
            directory.path().file_name().unwrap().to_string_lossy()
        );
        let mut recorder = Self {
            root,
            directory,
            watcher: None,
            program: None,
            declared: qk_cache::resolve_each(workspace, graph)?,
            started: std::time::SystemTime::now(),
            symlinks: BTreeMap::new(),
            tag_prefix,
        };
        for path in &recorder.declared.candidates {
            let absolute = recorder.root.join(path);
            if std::fs::symlink_metadata(&absolute)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
                && let Ok(target) = absolute.canonicalize()
            {
                recorder.symlinks.insert(path.clone(), target);
            }
        }
        if cfg!(target_os = "macos") {
            for (index, task) in prepared.values_mut().enumerate() {
                let path = recorder.directory.path().join(format!("task-{index}.sb"));
                let root = serde_json::to_string(
                    recorder
                        .root
                        .to_str()
                        .context("workspace path is not UTF-8")?,
                )?;
                let tag =
                    serde_json::to_string(&format!("qk-task:{}{}", recorder.tag_prefix, task.id))?;
                std::fs::write(
                    &path,
                    format!(
                        "(version 1)\n(allow default)\n(allow file-read* (subpath {root}) (with report) (with message {tag}))\n(allow file-write* (subpath {root}) (with report) (with message {tag}))\n"
                    ),
                )?;
                let probe = Command::new("sandbox-exec")
                    .arg("-f")
                    .arg(&path)
                    .arg("/usr/bin/true")
                    .output()
                    .context("input analysis needs sandbox-exec")?;
                if !probe.status.success() {
                    bail!(
                        "recorder profile rejected: {}",
                        String::from_utf8_lossy(&probe.stderr)
                    );
                }
                task.sandbox = Some(qk_executor::Confinement::Seatbelt(path));
            }
            recorder.watcher = Some(Watcher::start(recorder.directory.path())?);
        } else {
            let program = find_strace()?;
            let probe = qk_executor::Recording {
                program: program.clone(),
                directory: recorder.directory.path().to_owned(),
            }
            .command(0, "exec /bin/true")
            .output()
            .context("cannot probe strace")?;
            if !probe.status.success() {
                bail!(
                    "strace cannot record child processes: {}",
                    String::from_utf8_lossy(&probe.stderr)
                );
            }
            for (index, task) in prepared.values_mut().enumerate() {
                let directory = recorder.directory.path().join(format!("task-{index}"));
                std::fs::create_dir(&directory)?;
                task.recording = Some(qk_executor::Recording {
                    program: program.clone(),
                    directory,
                });
            }
            recorder.program = Some(program);
        }
        Ok(recorder)
    }

    pub fn finish(
        mut self,
        workspace: &Workspace,
        graph: &TaskGraph,
        prepared: &BTreeMap<String, PreparedTask>,
        outcomes: &BTreeMap<String, Outcome>,
    ) -> Result<Report> {
        let run_id = format!(
            "{}-{}",
            std::process::id(),
            self.started
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let mut observed: BTreeMap<String, Vec<Access>> = BTreeMap::new();
        let mut diagnostics = BTreeMap::<String, Vec<String>>::new();
        if let Some(watcher) = self.watcher.take() {
            match watcher.finish(self.directory.path()) {
                Ok((reports, dropped)) => {
                    if dropped > 0 {
                        for id in graph.tasks.keys() {
                            diagnostics.entry(id.clone()).or_default().push(format!(
                                "system log exceeded 100000 events; {dropped} events omitted"
                            ));
                        }
                    }
                    for event in reports {
                        let Some(task) = event.task.strip_prefix(&self.tag_prefix) else {
                            continue;
                        };
                        let operation = if event.operation.starts_with("file-write-unlink") {
                            Operation::Delete
                        } else if event.operation.starts_with("file-write") {
                            Operation::Write
                        } else if event.operation == "file-read-metadata" {
                            Operation::Metadata
                        } else if event.operation == "file-read-data" {
                            if Path::new(&event.path).is_dir() {
                                Operation::ReadDirectory
                            } else {
                                Operation::ReadData
                            }
                        } else {
                            continue;
                        };
                        observed.entry(task.to_owned()).or_default().push(Access {
                            process: event.process,
                            path: event.path,
                            resolved_path: None,
                            keyed_path: None,
                            operation,
                            result: "unknown".into(),
                            category: Category::Uncovered,
                        });
                    }
                }
                Err(error) => {
                    for id in graph.tasks.keys() {
                        diagnostics
                            .entry(id.clone())
                            .or_default()
                            .push(format!("collection interrupted: {error:#}"));
                    }
                }
            }
        } else {
            for (id, task) in prepared {
                let Some(recording) = &task.recording else {
                    continue;
                };
                match strace::collect(&recording.directory, &task.cwd) {
                    Ok((events, issues)) => {
                        observed.insert(id.clone(), events);
                        diagnostics.insert(id.clone(), issues);
                    }
                    Err(error) => {
                        diagnostics.insert(
                            id.clone(),
                            vec![format!("could not collect trace: {error:#}")],
                        );
                    }
                }
            }
        }
        let mut tasks = BTreeMap::new();
        for (id, task) in &graph.tasks {
            let mut coverage = self.coverage();
            coverage.diagnostics = diagnostics.remove(id).unwrap_or_default();
            let declared = match &self.declared.tasks[id] {
                Ok(resolved) => resolved.files.clone(),
                Err(error) => {
                    coverage
                        .diagnostics
                        .push(format!("inputs could not be resolved: {error}"));
                    BTreeSet::new()
                }
            };
            let outputs = qk_cache::Outputs::new(workspace, task);
            let warm = qk_cache::warm::config(workspace, task).and_then(|warm| {
                qk_cache::Outputs::from_paths(&warm.map_or_else(Vec::new, |warm| warm.kept_paths()))
            });
            let dependencies: Vec<_> = task
                .dependencies
                .iter()
                .map(|id| qk_cache::Outputs::new(workspace, &graph.tasks[id]))
                .collect();
            if outputs.is_err() || warm.is_err() || dependencies.iter().any(Result::is_err) {
                coverage
                    .diagnostics
                    .push("some output or warm paths could not be resolved".into());
            }
            let mut generated = BTreeSet::new();
            let mut accesses = observed.remove(id).unwrap_or_default();
            for event in &mut accesses {
                let absolute = strace::normalize(Path::new(&event.path));
                if let Some(resolved) = &event.resolved_path {
                    let resolved = Path::new(resolved);
                    event.resolved_path = Some(
                        resolved
                            .strip_prefix(&self.root)
                            .unwrap_or(resolved)
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                let relative = absolute
                    .strip_prefix(&self.root)
                    .ok()
                    .and_then(|path| path.to_str())
                    .map(|path| path.replace('\\', "/"));
                let Some(relative) = relative else {
                    event.category = Category::External;
                    continue;
                };
                event.path = relative;
                if event.path.is_empty() {
                    event.path = ".".into();
                }
                event.keyed_path = self.symlinks.iter().find_map(|(alias, target)| {
                    let suffix = absolute.strip_prefix(target).ok()?;
                    let keyed = if suffix.as_os_str().is_empty() {
                        alias.clone()
                    } else {
                        format!("{alias}/{}", suffix.to_string_lossy())
                    };
                    // A directory symlink input hashes the whole linked tree.
                    (declared.contains(&keyed) || declared.contains(alias)).then_some(alias.clone())
                });
                event.category = if event.path.split('/').any(|part| part == "node_modules") {
                    Category::InstalledPackage
                } else if event.path == ".git" || event.path.starts_with(".git/") {
                    Category::GitState
                } else if outputs
                    .as_ref()
                    .is_ok_and(|outputs| outputs.matches(&event.path))
                {
                    Category::OwnOutput
                } else if warm.as_ref().is_ok_and(|paths| paths.matches(&event.path)) {
                    Category::WarmState
                } else if dependencies
                    .iter()
                    .any(|paths| paths.as_ref().is_ok_and(|paths| paths.matches(&event.path)))
                {
                    Category::DependencyOutput
                } else if matches!(
                    event.operation,
                    Operation::ListDirectory | Operation::ReadDirectory
                ) || event.result == "missing"
                    || (!event.operation.writes() && self.root.join(&event.path).is_dir())
                {
                    Category::Discovery
                } else if declared.contains(&event.path) || event.keyed_path.is_some() {
                    Category::Input
                } else if self.declared.tasks[id].as_ref().is_ok_and(|resolved| {
                    resolved
                        .values
                        .contains_key(&format!("json:{}", event.path))
                }) {
                    Category::StructuredInput
                } else if generated.contains(&event.path) && !event.operation.writes() {
                    Category::Generated
                } else if self.declared.tasks[id].as_ref().is_ok_and(|resolved| {
                    (event.path == "pnpm-lock.yaml" && resolved.lockfile.is_some())
                        || (event.path == "pnpm-workspace.yaml" && resolved.workspace_file)
                }) {
                    Category::ResolutionMetadata
                } else {
                    Category::Uncovered
                };
                if event.operation.writes()
                    && matches!(event.result.as_str(), "success" | "unknown")
                {
                    generated.insert(event.path.clone());
                }
            }
            let outcome = match outcomes.get(id) {
                Some(Outcome::Success) => "success".into(),
                Some(Outcome::Failed(code)) => format!("failed:{code}"),
                Some(Outcome::Cancelled) => "cancelled".into(),
                None => "skipped".into(),
            };
            if outcome == "skipped" {
                coverage.diagnostics.push("task did not execute".into());
            }
            let context = serde_json::json!({
                "task": id, "configuration": task.configuration, "args": task.args,
                "definitionHash": blake3::hash(&serde_json::to_vec(&task.definition)?).to_hex().to_string(),
                "inputConfigurationHash": blake3::hash(&serde_json::to_vec(&(
                    &workspace.config.named_inputs, &workspace.config.extra,
                    workspace.projects.iter().map(|(name, project)| (name, &project.named_inputs)).collect::<BTreeMap<_, _>>()
                ))?).to_hex().to_string(),
                "projectRoot": workspace.projects[&task.project].root,
                "qkVersion": env!("CARGO_PKG_VERSION"),
                "platform": [std::env::consts::OS, std::env::consts::ARCH],
                "warmState": "existing on disk; restoration and publication disabled",
                "nonFileInputs": self.declared.tasks[id].as_ref().ok().map(|resolved| resolved.values.keys().collect::<Vec<_>>()),
            });
            let mut analysis = TaskAnalysis {
                context,
                outcome,
                coverage,
                declared_files: declared,
                mandatory_files: self.declared.tasks[id]
                    .as_ref()
                    .map(|resolved| resolved.mandatory.clone())
                    .unwrap_or_default(),
                accesses,
                observed_inputs: BTreeSet::new(),
                uncovered_accesses: BTreeSet::new(),
                unobserved_inputs: BTreeSet::new(),
                successful_runs: BTreeSet::new(),
                successful_observations: BTreeSet::new(),
                candidate_filesets: BTreeSet::new(),
                declaration: Some(Declaration {
                    target: task.target.clone(),
                    inputs: task.definition.inputs.clone().unwrap_or_else(|| {
                        vec![serde_json::json!("default"), serde_json::json!("^default")]
                    }),
                    named_inputs: workspace.projects[&task.project].named_inputs.clone(),
                }),
                glob_suggestions: vec![],
                configuration_fragment: None,
                successful_directories: BTreeSet::new(),
                suggestion_notes: vec![],
                access_review: Default::default(),
            };
            analysis.summarize(&run_id);
            tasks.insert(id.clone(), analysis);
        }
        Ok(Report {
            schema_version: 1,
            run_id,
            tasks,
        })
    }

    fn coverage(&self) -> Coverage {
        let macos = self.program.is_none();
        Coverage {
            backend: if macos { "seatbelt" } else { "strace" }.into(),
            partial: true,
            limitations: if macos {
                vec![
                    "Workspace accesses only; external files are not recorded.",
                    "Seatbelt reports authorization checks, not syscall results; failed lookups can be absent.",
                    "The system log can coalesce or lose events; directory reads do not prove enumeration.",
                    "Directory type is checked after execution; renamed or removed directories may be ambiguous.",
                    "Symlink accesses may be reported by target path; their link identity is not reconstructed.",
                ]
            } else {
                vec![
                    "Readable opens are potential inputs, not proof that file contents were consumed.",
                    "Inherited descriptors, io_uring, external services and processes outside the task tree are not covered.",
                    "Symlink targets and concurrent filesystem changes are not reconstructed.",
                    "Write-capable opens are recorded without observing whether content was written.",
                ]
            }.into_iter().map(str::to_owned).collect(),
            diagnostics: Vec::new(),
        }
    }
}

fn find_strace() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("input analysis needs strace on PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join("strace");
        if candidate.is_file() {
            return Ok(candidate.canonicalize()?);
        }
    }
    bail!("input analysis needs strace on PATH; install strace and retry")
}
