//! Bounded scheduling of finite tasks with dependency failure propagation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_executor::{Outcome, environment, execute, prepare};
use qk_taskgraph::TaskGraph;

#[derive(Debug)]
pub struct RunResult {
    pub outcomes: BTreeMap<String, Outcome>,
    pub skipped: BTreeSet<String>,
    pub exit_code: i32,
}

pub fn run(
    workspace: &Workspace,
    graph: &TaskGraph,
    parallel: usize,
    skip_cache: bool,
    cancelled: Arc<AtomicBool>,
) -> Result<RunResult> {
    if parallel == 0 {
        bail!("parallel must be at least 1");
    }
    let environment = environment(&workspace.root)?;
    // Validate every task before the first command can have side effects.
    let prepared = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            prepare(workspace, task, &environment)
                .with_context(|| format!("cannot execute {id}"))
                .map(|task| (id.clone(), task))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut pending: BTreeSet<_> = graph.tasks.keys().cloned().collect();
    let mut active = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    let mut fingerprints: BTreeMap<String, Option<String>> = BTreeMap::new();
    let cache = (!skip_cache
        && graph
            .tasks
            .values()
            .any(|task| task.definition.cache == Some(true)))
    .then(|| qk_cache::Cache {
        root: qk_cache::cache_directory(&workspace.root),
    });
    let mut skipped = BTreeSet::new();
    let mut exit_code = 0;
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| -> Result<()> {
        while !pending.is_empty() || !active.is_empty() {
            if cancelled.load(Ordering::SeqCst) {
                skipped.append(&mut pending);
                exit_code = 130;
            }
            for id in pending.clone() {
                let dependencies = &graph.tasks[&id].dependencies;
                if dependencies.iter().any(|dependency| {
                    skipped.contains(dependency)
                        || outcomes
                            .get(dependency)
                            .is_some_and(|outcome| *outcome != Outcome::Success)
                }) {
                    pending.remove(&id);
                    skipped.insert(id.clone());
                    eprintln!("qk: skipped {id} (dependency failed)");
                    continue;
                }
                if active.len() >= parallel
                    || !dependencies
                        .iter()
                        .all(|dependency| outcomes.get(dependency) == Some(&Outcome::Success))
                {
                    continue;
                }
                pending.remove(&id);
                active.insert(id.clone());
                let verb = if cache.is_some() {
                    "checking"
                } else {
                    "running"
                };
                eprintln!("qk: {verb} {id}");
                let task = &prepared[&id];
                let definition = &graph.tasks[&id];
                let dependency_keys = dependencies
                    .iter()
                    .map(|id| (id.clone(), fingerprints[id].clone()))
                    .collect::<BTreeMap<_, _>>();
                let cache = cache.as_ref();
                let sender = sender.clone();
                let cancelled = cancelled.clone();
                scope.spawn(move || {
                    let result = if let Some(cache) = cache {
                        cache.run(
                            workspace,
                            graph,
                            definition,
                            task,
                            &dependency_keys,
                            &cancelled,
                        )
                    } else {
                        execute(task, &cancelled).map(|outcome| qk_cache::TaskResult {
                            outcome,
                            fingerprint: None,
                            hit: false,
                        })
                    };
                    let _ = sender.send((id, result));
                });
            }
            if active.is_empty() {
                if pending.is_empty() {
                    break;
                }
                // A skipped dependency can unlock more skips on the next pass.
                if pending.iter().any(|id| {
                    graph.tasks[id]
                        .dependencies
                        .iter()
                        .any(|dependency| skipped.contains(dependency))
                }) {
                    continue;
                }
                bail!("scheduler stalled: no runnable tasks");
            }
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok((id, result)) => {
                    active.remove(&id);
                    let outcome = match result {
                        Ok(result) => {
                            fingerprints.insert(id.clone(), result.fingerprint);
                            result.outcome
                        }
                        Err(error) => {
                            fingerprints.insert(id.clone(), None);
                            eprintln!("qk: {id}: {error:#}");
                            Outcome::Failed(1)
                        }
                    };
                    match outcome {
                        Outcome::Success => eprintln!("qk: finished {id}"),
                        Outcome::Failed(code) => {
                            eprintln!("qk: failed {id} (exit {code})");
                            if exit_code == 0 {
                                exit_code = code;
                            }
                        }
                        Outcome::Cancelled => exit_code = 130,
                    }
                    outcomes.insert(id, outcome);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error).context("task worker disconnected"),
            }
        }
        Ok(())
    })?;
    if cancelled.load(Ordering::SeqCst) {
        exit_code = 130;
    }
    Ok(RunResult {
        outcomes,
        skipped,
        exit_code,
    })
}
