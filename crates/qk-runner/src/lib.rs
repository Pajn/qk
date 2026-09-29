//! Bounded scheduling of finite and continuous tasks with dependency failure propagation.
//!
//! A continuous task satisfies its dependents once it has started. It does not count
//! towards the parallel limit, since it holds its slot indefinitely. Unless it was
//! requested directly, it is stopped once every task depending on it has finished.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_executor::{Display, Outcome, OutputStyle, environment, execute, prepare};
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
    style: OutputStyle,
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
                .map(|mut prepared| {
                    let continuous = task.definition.continuous == Some(true);
                    prepared.display = Display::for_task(style, id, &task.project, continuous);
                    (id.clone(), prepared)
                })
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let continuous: BTreeSet<_> = graph
        .tasks
        .iter()
        .filter(|(_, task)| task.definition.continuous == Some(true))
        .map(|(id, _)| id.clone())
        .collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, task) in &graph.tasks {
        for dependency in &task.dependencies {
            dependents.entry(dependency).or_default().push(id);
        }
    }
    let mut pending: BTreeSet<_> = graph.tasks.keys().cloned().collect();
    let mut active = BTreeSet::new();
    // Stop signals for running continuous tasks, and those already asked to stop.
    let mut stops: BTreeMap<String, Arc<AtomicBool>> = BTreeMap::new();
    let mut stopping = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    let mut fingerprints: BTreeMap<String, qk_cache::Fingerprint> = BTreeMap::new();
    let cache = (!skip_cache
        && graph
            .tasks
            .values()
            .any(|task| task.definition.cache == Some(true)))
    .then(|| qk_cache::Cache::new(qk_cache::cache_directory(&workspace.root)));
    let mut skipped = BTreeSet::new();
    let mut exit_code = 0;
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| -> Result<()> {
        while !pending.is_empty() || !active.is_empty() {
            if cancelled.load(Ordering::SeqCst) {
                skipped.append(&mut pending);
                exit_code = 130;
                for stop in stops.values() {
                    stop.store(true, Ordering::SeqCst);
                }
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
                    qk_executor::status!("qk: skipped {id} (dependency failed)");
                    continue;
                }
                let is_continuous = continuous.contains(&id);
                let finite_active = active.difference(&continuous).count();
                if (!is_continuous && finite_active >= parallel)
                    || !dependencies.iter().all(|dependency| {
                        outcomes.get(dependency) == Some(&Outcome::Success)
                            || (continuous.contains(dependency) && active.contains(dependency))
                    })
                {
                    continue;
                }
                pending.remove(&id);
                active.insert(id.clone());
                let task = &prepared[&id];
                let sender = sender.clone();
                if is_continuous {
                    qk_executor::status!("qk: started {id} (continuous)");
                    // Dependents start while it runs, and its live state cannot be keyed.
                    fingerprints.insert(
                        id.clone(),
                        Err(format!("{id}: continuous tasks are not fingerprinted")),
                    );
                    let stop = Arc::new(AtomicBool::new(false));
                    stops.insert(id.clone(), stop.clone());
                    scope.spawn(move || {
                        let result = execute(task, &stop)
                            .map(|outcome| qk_cache::TaskResult::uncached(outcome, String::new()));
                        let _ = sender.send((id, result));
                    });
                    continue;
                }
                let verb = if cache.is_some() {
                    "checking"
                } else {
                    "running"
                };
                qk_executor::status!("qk: {verb} {id}");
                let definition = &graph.tasks[&id];
                let dependency_keys = dependencies
                    .iter()
                    .map(|id| (id.clone(), fingerprints[id].clone()))
                    .collect::<BTreeMap<_, _>>();
                let cache = cache.as_ref();
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
                        execute(task, &cancelled).map(|outcome| {
                            qk_cache::TaskResult::uncached(outcome, "cache disabled".into())
                        })
                    };
                    let _ = sender.send((id, result));
                });
            }
            // Stop continuous dependencies that no remaining task needs.
            for (id, stop) in &stops {
                let needed = dependents.get(id.as_str()).is_some_and(|dependents| {
                    dependents.iter().any(|dependent| {
                        pending.contains(*dependent) || active.contains(*dependent)
                    })
                });
                if !graph.roots.contains(id) && !needed && stopping.insert(id.clone()) {
                    qk_executor::status!("qk: stopping {id} (no longer needed)");
                    stop.store(true, Ordering::SeqCst);
                }
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
                    stops.remove(&id);
                    let outcome = match result {
                        Ok(result) => {
                            if !continuous.contains(&id) {
                                fingerprints.insert(id.clone(), result.fingerprint);
                            }
                            result.outcome
                        }
                        Err(error) => {
                            fingerprints.insert(id.clone(), Err(format!("{id}: {error:#}")));
                            qk_executor::status!("qk: {id}: {error:#}");
                            Outcome::Failed(1)
                        }
                    };
                    // Stopping a continuous task on purpose is its normal end.
                    let outcome = if stopping.contains(&id) && outcome == Outcome::Cancelled {
                        qk_executor::status!("qk: stopped {id}");
                        Outcome::Success
                    } else {
                        match outcome {
                            Outcome::Success => qk_executor::status!("qk: finished {id}"),
                            Outcome::Failed(code) => {
                                qk_executor::status!("qk: failed {id} (exit {code})");
                                if exit_code == 0 {
                                    exit_code = code;
                                }
                            }
                            Outcome::Cancelled => exit_code = 130,
                        }
                        outcome
                    };
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
