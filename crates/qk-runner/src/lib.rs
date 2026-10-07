//! Bounded scheduling of finite and continuous tasks with dependency failure propagation.
//!
//! A continuous task satisfies its dependents once it has started. It does not count
//! towards the parallel limit, since it holds its slot indefinitely. Unless it was
//! requested directly, it is stopped once every task depending on it has finished.
//!
//! Of the tasks ready to start, those with the longest expected path to the end
//! of the run start first: the task's own expected time and that of the
//! longest chain of tasks depending on it. A task expected to take long thus
//! does not start last and keep the run waiting for it alone.

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

mod memory;
mod recorder;
pub mod sandbox;
pub mod threads;
use threads::Threads;

pub struct RunResult {
    pub outcomes: BTreeMap<String, Outcome>,
    pub skipped: BTreeSet<String>,
    pub exit_code: i32,
    /// What happened to each task that started.
    pub tasks: BTreeMap<String, TaskRecord>,
    /// The machine's CPU use sampled through the run, from 0 to 1, each with
    /// whether a ready task was waiting for a free slot at the time.
    pub load: Vec<(f32, bool)>,
    /// The most memory, in bytes, the run's tasks used together.
    pub memory: Option<u64>,
    /// Under `--sandbox`, what each task did beyond its declarations, and
    /// the tasks that ran unsandboxed with why.
    pub sandbox: Option<SandboxResult>,
    pub input_analysis: Option<qk_input_analysis::Report>,
}

pub struct SandboxResult {
    pub mode: sandbox::Mode,
    pub findings: BTreeMap<String, sandbox::Findings>,
    pub unsandboxed: BTreeMap<String, String>,
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
    /// What warm state did, for targets that keep it.
    pub warm: Option<qk_cache::warm::WarmReport>,
    /// The threads it was given, for targets with `qk:threads`.
    pub threads: Option<usize>,
    /// The most memory, in bytes, its processes used together, sampled as it
    /// ran; none when it ran too briefly to be sampled.
    pub memory: Option<u64>,
    /// Bounded output and input verification for an actual cacheable execution.
    pub execution: Option<qk_cache::Execution>,
}

/// How long a task can be expected to run, from earlier runs that executed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    pub millis: u64,
    /// The threads it ran with: one, unless its target has `qk:threads`.
    pub threads: u64,
}

impl Expected {
    /// Core-milliseconds: the time it takes on the threads it ran with.
    fn work(&self) -> u64 {
        self.millis.saturating_mul(self.threads)
    }

    /// Historical work less what the current allocation has already done.
    fn remaining(&self, elapsed: Duration, threads: usize) -> u64 {
        let elapsed = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.work()
            .saturating_sub(elapsed.saturating_mul(threads as u64))
    }
}

/// How a run is carried out.
pub struct Settings<'a> {
    /// Tasks running at once, continuous ones aside.
    pub parallel: usize,
    /// Cores the tasks share.
    pub cores: usize,
    pub skip_cache: bool,
    /// Nx's `--nx-bail`: no task starts after one has failed.
    pub bail: bool,
    pub style: OutputStyle,
    /// Run each task in macOS's sandbox, auditing or enforcing what it declares.
    pub sandbox: Option<sandbox::Mode>,
    /// Execute finite tasks under an advisory file-access recorder.
    pub analyze_inputs: bool,
    /// What each task is expected to take; a task without one is expected to
    /// take no time.
    pub expected: &'a BTreeMap<String, Expected>,
    /// The cases each task narrowed to some of them runs, named to it in
    /// `QK_AFFECTED_CASES`. Such a task runs without the cache: its result
    /// covers those cases alone.
    pub cases: &'a BTreeMap<String, Vec<String>>,
}

pub fn run(
    workspace: &Workspace,
    graph: &TaskGraph,
    settings: &Settings,
    cancelled: Arc<AtomicBool>,
) -> Result<RunResult> {
    let Settings {
        parallel,
        cores,
        skip_cache,
        bail: bails,
        style,
        sandbox: sandbox_mode,
        analyze_inputs,
        expected,
        cases,
    } = *settings;
    if analyze_inputs && sandbox_mode.is_some() {
        bail!("input analysis and sandbox enforcement/audit cannot be combined");
    }
    let skip_cache = skip_cache || analyze_inputs;
    if parallel == 0 || cores == 0 {
        bail!("parallel and cores must be at least 1");
    }
    // Each narrowed task's cases, one per line, in a file kept for the run.
    let case_directory = if cases.is_empty() {
        None
    } else {
        Some(tempfile::tempdir().context("cannot create a directory for task cases")?)
    };
    let mut case_files = BTreeMap::new();
    if let Some(directory) = &case_directory {
        for (index, (id, list)) in cases.iter().enumerate() {
            // Absolute, so a task finds it from whatever directory it runs in.
            let path = std::path::absolute(directory.path().join(format!("cases-{index}.txt")))
                .context("cannot locate the directory for task cases")?;
            let text: String = list.iter().map(|case| format!("{case}\n")).collect();
            std::fs::write(&path, text)
                .with_context(|| format!("cannot write the cases of {id}"))?;
            case_files.insert(id.clone(), path);
        }
    }
    // The runner's own environment includes the root dotenv files, which is
    // where remote cache credentials arrive; each task loads its own dotenv
    // files over the process environment instead.
    let environment = environment(&workspace.root)?;
    let process_environment: BTreeMap<_, _> = std::env::vars_os().collect();
    // Validate every task before the first command can have side effects.
    let mut prepared = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            prepare(workspace, task, &process_environment)
                .with_context(|| format!("cannot execute {id}"))
                .map(|mut prepared| {
                    let continuous = task.definition.continuous == Some(true);
                    // A task without commands, as nx:noop's, has nothing to
                    // show, and Nx leaves it out of the log.
                    prepared.display = if prepared.commands.is_empty() {
                        Display::Hidden
                    } else {
                        Display::for_task(
                            style,
                            id,
                            &task.project,
                            continuous,
                            graph.roots.contains(id),
                        )
                    };
                    (id.clone(), prepared)
                })
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    // As in Nx, the task a run is for can be interacted with, when it is a
    // single command streaming its output; its dependencies
    // have finished or run beside it without the terminal's input.
    if let [root] = graph.roots.iter().collect::<Vec<_>>()[..]
        && let Some(task) = prepared.get_mut(root)
        && matches!(task.display, Display::Stream | Display::Headed { .. })
        && task.commands.len() == 1
    {
        task.interactive = true;
    }
    let sandbox = sandbox_mode
        .map(|mode| sandbox::Sandbox::start(workspace, graph, mode))
        .transpose()?;
    let recorder = analyze_inputs
        .then(|| recorder::Recorder::start(workspace, graph, &mut prepared))
        .transpose()?;
    let prepared = prepared;
    let threads: BTreeMap<String, Threads> = graph
        .tasks
        .iter()
        .filter_map(|(id, task)| {
            Threads::config(task)
                .with_context(|| format!("invalid qk:threads for {id}"))
                .transpose()
                .map(|threads| threads.map(|threads| (id.clone(), threads)))
        })
        .collect::<Result<_>>()?;
    let continuous: BTreeSet<_> = graph
        .tasks
        .iter()
        .filter(|(_, task)| task.definition.continuous == Some(true))
        .map(|(id, _)| id.clone())
        .collect();
    let alone: BTreeSet<_> = graph
        .tasks
        .iter()
        .filter(|(_, task)| task.definition.parallelism == Some(false))
        .map(|(id, _)| id.clone())
        .collect();
    // Tasks with readyWhen run like continuous ones: their dependents start
    // once they are ready, and they stop when no remaining task needs them.
    // A continuous task without readyWhen is ready once it starts.
    let announced: BTreeSet<_> = prepared
        .iter()
        .filter(|(_, task)| !task.ready_when.is_empty())
        .map(|(id, _)| id.clone())
        .collect();
    let serving: BTreeSet<_> = continuous.union(&announced).cloned().collect();
    let is_up = |id: &str| prepared[id].ready.load(Ordering::SeqCst);
    // Running tasks holding a --parallel slot: as in Nx, one with readyWhen
    // gives its slot up once ready.
    let holding = |active: &BTreeSet<String>| {
        active
            .iter()
            .filter(|id| !continuous.contains(*id) && !(announced.contains(*id) && is_up(id)))
            .count()
    };
    // As in Nx: a task that runs alone cannot run beside a continuous task it
    // depends on, and a continuous task others depend on runs beside them.
    for (id, task) in &graph.tasks {
        for dependency in task
            .dependencies
            .iter()
            .filter(|id| continuous.contains(*id))
        {
            if alone.contains(id) {
                bail!(
                    "{id} does not support parallelism but depends on continuous task {dependency}"
                );
            }
            if alone.contains(dependency) {
                bail!(
                    "continuous task {dependency} does not support parallelism but {id} depends on it"
                );
            }
        }
    }
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, task) in &graph.tasks {
        for dependency in &task.dependencies {
            dependents.entry(dependency).or_default().push(id);
        }
    }
    let order = by_path(graph, &dependents, expected);
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
    let cacheable = graph
        .tasks
        .values()
        .any(|task| task.definition.cache == Some(true));
    let cache = if skip_cache {
        (sandbox.is_none() && !analyze_inputs && cacheable)
            .then(|| qk_cache::Cache::for_observation(workspace))
    } else {
        (warm || cacheable).then(|| qk_cache::Cache::for_workspace(workspace, &environment))
    };
    let mut skipped = BTreeSet::new();
    let mut exit_code = 0;
    let mut started = BTreeMap::new();
    let mut ready = BTreeMap::new();
    let mut records = BTreeMap::new();
    let mut bailed = false;
    // Cores held by each running finite task, and the threads each was given.
    let mut held: BTreeMap<String, usize> = BTreeMap::new();
    let mut given: BTreeMap<String, usize> = BTreeMap::new();
    let (sender, receiver) = mpsc::channel();
    report::event(Event::Planned {
        tasks: graph.tasks.len(),
    });
    let waiting = AtomicBool::new(false);
    let load = std::sync::Mutex::new(Vec::new());
    // The most memory each task, and the run's tasks together, have used.
    let peaks = std::sync::Mutex::new((BTreeMap::<String, u64>::new(), 0u64));
    std::thread::scope(|scope| -> Result<()> {
        let (finish_sampling, sampling_finished) = mpsc::channel::<()>();
        let load = &load;
        let waiting = &waiting;
        let peaks = &peaks;
        let prepared = &prepared;
        // Samples how busy the machine is, so the summary can tell a run held
        // back by --parallel from one held back by the machine, and the
        // memory each running task uses.
        scope.spawn(move || {
            let mut system = sysinfo::System::new();
            system.refresh_cpu_usage();
            while matches!(
                sampling_finished.recv_timeout(Duration::from_millis(250)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                system.refresh_cpu_usage();
                load.lock().unwrap().push((
                    system.global_cpu_usage() / 100.0,
                    waiting.load(Ordering::Relaxed),
                ));
                let roots: BTreeMap<&str, BTreeSet<u32>> = prepared
                    .iter()
                    .map(|(id, task)| (id.as_str(), task.processes.lock().unwrap().clone()))
                    .filter(|(_, processes)| !processes.is_empty())
                    .collect();
                if roots.is_empty() {
                    continue;
                }
                let used = memory::sample(&mut system, &roots);
                let (tasks, run) = &mut *peaks.lock().unwrap();
                for (id, bytes) in &used {
                    let peak = tasks.entry((*id).to_owned()).or_default();
                    *peak = (*peak).max(*bytes);
                }
                *run = (*run).max(used.values().sum());
            }
        });
        let result = (|| -> Result<()> {
            while !pending.is_empty() || !active.is_empty() {
                let mut waiting_now = false;
                // A task that became ready gives back its core.
                held.retain(|id, _| !(announced.contains(id) && is_up(id)));
                let dependencies_done = |id: &String| {
                    graph.tasks[id].dependencies.iter().all(|dependency| {
                        outcomes.get(dependency) == Some(&Outcome::Success)
                            || (active.contains(dependency) && is_up(dependency))
                    })
                };
                // The finite tasks that could start in this pass, which the
                // threaded ones among them share the free cores with.
                let finite_pending = pending.difference(&continuous).count();
                let ready_threaded = pending
                    .iter()
                    .filter(|id| threads.contains_key(*id) && dependencies_done(id))
                    .count();
                // Threaded tasks starting in this pass split the free cores
                // evenly, after a core for each slot other pending tasks could
                // take, so one starting alone does not hold up what follows.
                let even = {
                    let slots = parallel.saturating_sub(holding(&active));
                    let sharing = ready_threaded.min(slots).max(1);
                    let reserved =
                        (finite_pending - ready_threaded).min(slots.saturating_sub(sharing));
                    Threads::even(cores.saturating_sub(held.values().sum()), sharing, reserved)
                };
                // The core-milliseconds the run has left: what pending tasks are
                // expected to take, and what running ones have still to go.
                let now = SystemTime::now();
                let remaining_work: u64 = pending
                    .iter()
                    .filter_map(|id| expected.get(id))
                    .map(Expected::work)
                    .chain(held.iter().filter_map(|(id, cores)| {
                        let elapsed = now.duration_since(started[id]).unwrap_or_default();
                        Some(expected.get(id)?.remaining(elapsed, *cores))
                    }))
                    .sum();
                // After a failure under --nx-bail, what has not started is skipped
                // and what runs finishes.
                if bailed && !pending.is_empty() {
                    for id in &pending {
                        report::event(Event::Skipped { id: id.clone() });
                    }
                    skipped.append(&mut pending);
                }
                if cancelled.load(Ordering::SeqCst) {
                    skipped.append(&mut pending);
                    exit_code = 130;
                    for stop in stops.values() {
                        stop.store(true, Ordering::SeqCst);
                    }
                }
                for id in order
                    .iter()
                    .filter(|id| pending.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>()
                {
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
                    let finite_active = holding(&active);
                    if !dependencies.iter().all(|dependency| {
                        outcomes.get(dependency) == Some(&Outcome::Success)
                            || (active.contains(dependency) && is_up(dependency))
                    }) {
                        continue;
                    }
                    ready.entry(id.clone()).or_insert_with(SystemTime::now);
                    // A task that runs alone waits for every other, and blocks them.
                    if !active.is_empty()
                        && (alone.contains(&id) || active.iter().any(|id| alone.contains(id)))
                    {
                        continue;
                    }
                    if !is_continuous && finite_active >= parallel {
                        waiting_now = true;
                        continue;
                    }
                    if !is_continuous {
                        let free = cores.saturating_sub(held.values().sum());
                        let cores_for = match threads.get(&id) {
                            Some(config) => {
                                // Alone, nothing else can take a share. A task
                                // expected to do much of what is left gets as
                                // much of the cores, so that it is not left
                                // running once the rest is done.
                                // The even split was made before this pass
                                // started tasks; it cannot exceed what they left.
                                let even = if alone.contains(&id) {
                                    cores
                                } else {
                                    // Keep other slots usable, including tasks
                                    // with no history. Ready threaded tasks need
                                    // their minimum; future tasks reserve a core.
                                    let slots = parallel.saturating_sub(finite_active + 1);
                                    let dependencies_done = |id: &String| {
                                        graph.tasks[id].dependencies.iter().all(|dependency| {
                                            outcomes.get(dependency) == Some(&Outcome::Success)
                                                || (active.contains(dependency)
                                                    && is_up(dependency))
                                        })
                                    };
                                    let others = || {
                                        order.iter().filter(|other| {
                                            *other != &id
                                                && pending.contains(*other)
                                                && !continuous.contains(*other)
                                        })
                                    };
                                    let reserved = others()
                                        .filter(|other| dependencies_done(other))
                                        .map(|other| {
                                            threads
                                                .get(other)
                                                .map_or(1, |threads| threads.min.min(cores))
                                        })
                                        .chain(
                                            others()
                                                .filter(|other| !dependencies_done(other))
                                                .map(|_| 1),
                                        )
                                        .take(slots)
                                        .fold(0usize, usize::saturating_add);
                                    even.max(proportion(expected.get(&id), remaining_work, cores))
                                        .min(free.saturating_sub(reserved))
                                };
                                config.share(cores, even, free, held.is_empty())
                            }
                            None => (free > 0 || held.is_empty()).then_some(1),
                        };
                        let Some(cores_for) = cores_for else {
                            continue;
                        };
                        held.insert(id.clone(), cores_for);
                        if threads.contains_key(&id) {
                            given.insert(id.clone(), cores_for);
                        }
                    }
                    pending.remove(&id);
                    active.insert(id.clone());
                    started.insert(id.clone(), SystemTime::now());
                    // Sandboxed as it starts: on Linux the rules hold paths
                    // its dependencies have just created.
                    let mut task = prepared[&id].clone();
                    if let Some(sandbox) = &sandbox {
                        task.sandbox = sandbox.confine(&id);
                    }
                    let sender = sender.clone();
                    if serving.contains(&id) {
                        report::event(Event::StartedContinuous { id: id.clone() });
                        if !announced.contains(&id) {
                            prepared[&id].ready.store(true, Ordering::SeqCst);
                        }
                        // Dependents start while it runs, and its live state cannot be keyed.
                        fingerprints.insert(
                            id.clone(),
                            Err(if is_continuous {
                                format!("{id}: continuous tasks are not fingerprinted")
                            } else {
                                format!("{id}: tasks with readyWhen are not fingerprinted")
                            }),
                        );
                        let stop = Arc::new(AtomicBool::new(false));
                        stops.insert(id.clone(), stop.clone());
                        scope.spawn(move || {
                            let result = execute(&task, &stop).map(|outcome| {
                                qk_cache::TaskResult::uncached(outcome, String::new())
                            });
                            let _ = sender.send((id, result));
                        });
                        continue;
                    }
                    let narrowed = case_files.get(&id);
                    report::event(Event::Started {
                        id: id.clone(),
                        checking: cache.is_some() && narrowed.is_none(),
                    });
                    let definition = &graph.tasks[&id];
                    if let (Some(threads), Some(count)) = (threads.get(&id), given.get(&id)) {
                        task.execution.extend(threads.environment(*count));
                    }
                    if let Some(path) = narrowed {
                        task.execution
                            .insert("QK_AFFECTED_CASES".into(), path.as_os_str().to_owned());
                    }
                    let dependency_keys = dependencies
                        .iter()
                        .map(|id| (id.clone(), fingerprints[id].clone()))
                        .collect::<BTreeMap<_, _>>();
                    let cache = cache.as_ref().filter(|_| narrowed.is_none());
                    let uncached = if narrowed.is_some() {
                        "runs only the cases a change reaches"
                    } else {
                        "cache disabled"
                    };
                    let cancelled = cancelled.clone();
                    scope.spawn(move || {
                        let result = if let Some(cache) = cache {
                            cache.run(
                                workspace,
                                graph,
                                definition,
                                &task,
                                &dependency_keys,
                                &cancelled,
                            )
                        } else {
                            execute(&task, &cancelled).map(|outcome| {
                                qk_cache::TaskResult::uncached(outcome, uncached.into())
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
                    // A requested task with readyWhen is done once ready, as in Nx.
                    let done_when_ready = announced.contains(id) && !continuous.contains(id);
                    if is_up(id)
                        && !needed
                        && (done_when_ready || !graph.roots.contains(id))
                        && stopping.insert(id.clone())
                    {
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
                        held.remove(&id);
                        let (outcome, cache, key, warm, execution) = match result {
                            Ok(result) => {
                                if !serving.contains(&id) {
                                    fingerprints.insert(id.clone(), result.fingerprint);
                                }
                                (
                                    result.outcome,
                                    result.cache,
                                    result.key,
                                    result.warm,
                                    result.execution,
                                )
                            }
                            Err(error) => {
                                fingerprints.insert(id.clone(), Err(format!("{id}: {error:#}")));
                                qk_executor::status!("qk: {id}: {error:#}");
                                (
                                    Outcome::Failed(1),
                                    qk_cache::CacheStatus::Uncached,
                                    None,
                                    None,
                                    None,
                                )
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
                                    bailed |= bails;
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
                                warm,
                                threads: given.remove(&id),
                                memory: peaks
                                    .lock()
                                    .unwrap()
                                    .0
                                    .remove(&id)
                                    .filter(|bytes| *bytes > 0),
                                execution,
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
        drop(finish_sampling);
        result
    })?;
    if let Some(cache) = &cache {
        cache.finish(workspace);
    }
    if cancelled.load(Ordering::SeqCst) {
        exit_code = 130;
    }
    let sandbox = sandbox
        .map(|sandbox| -> Result<SandboxResult> {
            let mode = sandbox.mode;
            let (findings, unsandboxed) = sandbox.finish()?;
            Ok(SandboxResult {
                mode,
                findings,
                unsandboxed,
            })
        })
        .transpose()?;
    let input_analysis = recorder
        .map(|recorder| recorder.finish(workspace, graph, &prepared, &outcomes))
        .transpose()?;
    Ok(RunResult {
        outcomes,
        skipped,
        exit_code,
        tasks: records,
        load: load.into_inner().unwrap(),
        memory: Some(peaks.into_inner().unwrap().1).filter(|bytes| *bytes > 0),
        sandbox,
        input_analysis,
    })
}

/// The tasks, those with the longest expected path to the end of the run
/// first: a task's own expected time and the longest of its dependents'
/// paths. Ties keep the order of task ids.
fn by_path(
    graph: &TaskGraph,
    dependents: &BTreeMap<&str, Vec<&str>>,
    expected: &BTreeMap<String, Expected>,
) -> Vec<String> {
    fn path<'a>(
        id: &'a str,
        dependents: &BTreeMap<&str, Vec<&'a str>>,
        expected: &BTreeMap<String, Expected>,
        paths: &mut BTreeMap<&'a str, u64>,
    ) -> u64 {
        if let Some(path) = paths.get(id) {
            return *path;
        }
        let after = dependents
            .get(id)
            .into_iter()
            .flatten()
            .map(|dependent| path(dependent, dependents, expected, paths))
            .max()
            .unwrap_or(0);
        let path = expected.get(id).map_or(0, |expected| expected.millis) + after;
        paths.insert(id, path);
        path
    }
    let mut paths = BTreeMap::new();
    for id in graph.tasks.keys() {
        path(id, dependents, expected, &mut paths);
    }
    let mut order: Vec<String> = graph.tasks.keys().cloned().collect();
    order.sort_by_key(|id| std::cmp::Reverse(paths[id.as_str()]));
    order
}

/// The share of `cores` matching the share of `remaining` work a task is
/// expected to do, or none for a task without an expectation.
fn proportion(expected: Option<&Expected>, remaining: u64, cores: usize) -> usize {
    match expected {
        Some(expected) if remaining > 0 => {
            (expected.work() as u128 * cores as u128).div_ceil(remaining as u128) as usize
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::Expected;
    use std::time::Duration;

    #[test]
    fn remaining_work_accounts_for_the_current_allocation() {
        let expected = Expected {
            millis: 1_000,
            threads: 4,
        };
        assert_eq!(expected.remaining(Duration::ZERO, 8), 4_000);
        assert_eq!(expected.remaining(Duration::from_millis(250), 8), 2_000);
        assert_eq!(expected.remaining(Duration::from_millis(500), 8), 0);
        assert_eq!(expected.remaining(Duration::from_millis(500), 2), 3_000);
        assert_eq!(expected.remaining(Duration::from_secs(10), 8), 0);
        assert_eq!(expected.remaining(Duration::MAX, 8), 0);
    }
}
