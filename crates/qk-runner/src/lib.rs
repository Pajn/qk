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
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_executor::report::{self, Event};
use qk_executor::{Display, Outcome, OutputStyle, environment, execute, prepare};
use qk_taskgraph::TaskGraph;

pub struct RunResult {
    pub outcomes: BTreeMap<String, Outcome>,
    pub skipped: BTreeSet<String>,
    pub exit_code: i32,
    /// What happened to each task that started.
    pub tasks: BTreeMap<String, TaskRecord>,
    /// The machine's CPU use sampled through the run, from 0 to 1, each with
    /// whether a ready task was waiting for a free slot at the time.
    pub load: Vec<(f32, bool)>,
}

/// One task of a run, for history and reports.
pub struct TaskRecord {
    /// When its dependencies were done; before `started` when it then waited
    /// for a free slot.
    pub ready: SystemTime,
    pub started: SystemTime,
    pub ended: SystemTime,
    pub outcome: Outcome,
    pub cache: qk_cache::CacheStatus,
    /// The task's key and what it was computed from, when it has one.
    pub key: Option<(String, serde_json::Value)>,
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
    // The runner's own environment includes the root dotenv files, which is
    // where remote cache credentials arrive; each task loads its own dotenv
    // files over the process environment instead.
    let environment = environment(&workspace.root)?;
    let process_environment: BTreeMap<_, _> = std::env::vars_os().collect();
    // Validate every task before the first command can have side effects.
    let prepared = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            prepare(workspace, task, &process_environment)
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
    let warm = graph
        .tasks
        .values()
        .any(|task| task.definition.extra.contains_key("qk:warm"));
    if warm && !skip_cache {
        qk_cache::warm::check_overlaps(workspace, graph)?;
    }
    let cache = (!skip_cache
        && (warm
            || graph
                .tasks
                .values()
                .any(|task| task.definition.cache == Some(true))))
    .then(|| qk_cache::Cache::for_workspace(workspace, &environment));
    let mut skipped = BTreeSet::new();
    let mut exit_code = 0;
    let mut started = BTreeMap::new();
    let mut ready = BTreeMap::new();
    let mut records = BTreeMap::new();
    let (sender, receiver) = mpsc::channel();
    report::event(Event::Planned {
        tasks: graph.tasks.len(),
    });
    let waiting = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let load = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| -> Result<()> {
        // Samples how busy the machine is, so the summary can tell a run held
        // back by --parallel from one held back by the machine.
        scope.spawn(|| {
            let mut system = sysinfo::System::new();
            system.refresh_cpu_usage();
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                system.refresh_cpu_usage();
                load.lock().unwrap().push((
                    system.global_cpu_usage() / 100.0,
                    waiting.load(Ordering::Relaxed),
                ));
            }
        });
        let result = (|| -> Result<()> {
            while !pending.is_empty() || !active.is_empty() {
                let mut waiting_now = false;
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
                        report::event(Event::Skipped { id: id.clone() });
                        continue;
                    }
                    let is_continuous = continuous.contains(&id);
                    let finite_active = active.difference(&continuous).count();
                    if !dependencies.iter().all(|dependency| {
                        outcomes.get(dependency) == Some(&Outcome::Success)
                            || (continuous.contains(dependency) && active.contains(dependency))
                    }) {
                        continue;
                    }
                    ready.entry(id.clone()).or_insert_with(SystemTime::now);
                    if !is_continuous && finite_active >= parallel {
                        waiting_now = true;
                        continue;
                    }
                    pending.remove(&id);
                    active.insert(id.clone());
                    started.insert(id.clone(), SystemTime::now());
                    let task = &prepared[&id];
                    let sender = sender.clone();
                    if is_continuous {
                        report::event(Event::StartedContinuous { id: id.clone() });
                        // Dependents start while it runs, and its live state cannot be keyed.
                        fingerprints.insert(
                            id.clone(),
                            Err(format!("{id}: continuous tasks are not fingerprinted")),
                        );
                        let stop = Arc::new(AtomicBool::new(false));
                        stops.insert(id.clone(), stop.clone());
                        scope.spawn(move || {
                            let result = execute(task, &stop).map(|outcome| {
                                qk_cache::TaskResult::uncached(outcome, String::new())
                            });
                            let _ = sender.send((id, result));
                        });
                        continue;
                    }
                    report::event(Event::Started {
                        id: id.clone(),
                        checking: cache.is_some(),
                    });
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
                waiting.store(waiting_now, Ordering::Relaxed);
                // Stop continuous dependencies that no remaining task needs.
                for (id, stop) in &stops {
                    let needed = dependents.get(id.as_str()).is_some_and(|dependents| {
                        dependents.iter().any(|dependent| {
                            pending.contains(*dependent) || active.contains(*dependent)
                        })
                    });
                    if !graph.roots.contains(id) && !needed && stopping.insert(id.clone()) {
                        report::event(Event::Stopping { id: id.clone() });
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
                        let (outcome, cache, key) = match result {
                            Ok(result) => {
                                if !continuous.contains(&id) {
                                    fingerprints.insert(id.clone(), result.fingerprint);
                                }
                                (result.outcome, result.cache, result.key)
                            }
                            Err(error) => {
                                fingerprints.insert(id.clone(), Err(format!("{id}: {error:#}")));
                                qk_executor::status!("qk: {id}: {error:#}");
                                (Outcome::Failed(1), qk_cache::CacheStatus::Uncached, None)
                            }
                        };
                        // Stopping a continuous task on purpose is its normal end.
                        let outcome = if stopping.contains(&id) && outcome == Outcome::Cancelled {
                            report::event(Event::Stopped { id: id.clone() });
                            Outcome::Success
                        } else {
                            report::event(Event::Finished {
                                id: id.clone(),
                                outcome,
                            });
                            match outcome {
                                Outcome::Success => {}
                                Outcome::Failed(code) => {
                                    if exit_code == 0 {
                                        exit_code = code;
                                    }
                                }
                                Outcome::Cancelled => exit_code = 130,
                            }
                            outcome
                        };
                        records.insert(
                            id.clone(),
                            TaskRecord {
                                ready: ready.remove(&id).unwrap_or_else(SystemTime::now),
                                started: started.remove(&id).unwrap_or_else(SystemTime::now),
                                ended: SystemTime::now(),
                                outcome,
                                cache,
                                key,
                            },
                        );
                        outcomes.insert(id, outcome);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(error) => return Err(error).context("task worker disconnected"),
                }
            }
            Ok(())
        })();
        finished.store(true, Ordering::Relaxed);
        result
    })?;
    if let Some(cache) = &cache {
        cache.finish(workspace);
    }
    if cancelled.load(Ordering::SeqCst) {
        exit_code = 130;
    }
    Ok(RunResult {
        outcomes,
        skipped,
        exit_code,
        tasks: records,
        load: load.into_inner().unwrap(),
    })
}
