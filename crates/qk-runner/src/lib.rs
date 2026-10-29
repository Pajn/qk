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
//!
//! A task expected to use memory starts only when the machine has it free:
//! what is free, less a reserve and what the run's running tasks are still
//! expected to grow by, must cover what the task is expected to use. Free
//! memory is the machine's, so other runs and programs count. A task with
//! nothing else of the run running starts regardless, so a run cannot stall.

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

mod installs;
mod memory;
mod recorder;
pub mod sandbox;
pub mod threads;
use threads::Threads;

pub struct RunResult {
    pub outcomes: BTreeMap<String, Outcome>,
    pub skipped: BTreeSet<String>,
    /// As Nx's: 0 when every task succeeded, 130 when the run was
    /// cancelled, and 1 when a task failed or was skipped.
    pub exit_code: i32,
    /// What happened to each task that started.
    pub tasks: BTreeMap<String, TaskRecord>,
    /// The machine's CPU use sampled through the run, from 0 to 1, each with
    /// whether a ready task was waiting for a free slot at the time.
    pub load: Vec<(f32, bool)>,
    /// The most memory, in bytes, the run's tasks used together.
    pub memory: Option<u64>,
    /// Whether threaded tasks ran, rather than came from the cache, and none
    /// of them was expected to take anything, so that each was given an even
    /// split of the cores.
    pub unexpected: bool,
    /// Under `--sandbox`, what each task did beyond its declarations, the
    /// tasks that ran unsandboxed and those whose packages went unchecked,
    /// with why.
    pub sandbox: Option<SandboxResult>,
    pub input_analysis: Option<qk_input_analysis::Report>,
}

pub struct SandboxResult {
    pub mode: sandbox::Mode,
    pub findings: BTreeMap<String, sandbox::Findings>,
    pub unsandboxed: BTreeMap<String, String>,
    /// Enforce: tasks that could not be sandboxed, and so failed, and why.
    pub refused: BTreeMap<String, String>,
    pub packages_unchecked: BTreeMap<String, String>,
    /// Audit: the tasks held to their plans.
    pub audited: BTreeSet<String>,
    /// Tasks restored from the cache, which did not run to be checked.
    pub restored: BTreeSet<String>,
}

/// Runs a task without the cache, keeping its log for `qk show log` as a
/// cache miss does. Its inputs are not rechecked, so the log is not
/// evidence of flakiness.
fn execute_retained(
    task: &qk_executor::PreparedTask,
    cancelled: &AtomicBool,
) -> Result<qk_cache::TaskResult> {
    let capture = qk_executor::Capture::new(None, task.display.clone()).retain_log();
    let outcome = qk_executor::execute_captured(task, cancelled, Some(&capture))?;
    Ok(qk_cache::TaskResult {
        execution: capture.take_log().map(|log| qk_cache::Execution {
            log,
            inputs_unchanged: false,
        }),
        ..qk_cache::TaskResult::uncached(outcome)
    })
}

/// Gives each cacheable task with `qk:ultracache` a recording, in a directory
/// of its own: strace follows its processes, filtering in the kernel where
/// strace can. With `resolving`, an opened file's path is recorded with what
/// its links lead to, as an audit reads it. Elsewhere than on Linux, or where
/// no recorder works, nothing is recorded, and that is said once: the run
/// goes on, as a recording only keys what a later run may take. A recorder
/// `QK_RECORDER` names that cannot record fails the run, as asked for.
fn attach_recordings(
    graph: &TaskGraph,
    prepared: &mut BTreeMap<String, qk_executor::PreparedTask>,
    resolving: bool,
) -> Result<Option<tempfile::TempDir>> {
    if !cfg!(target_os = "linux") {
        qk_executor::status!("qk: --record records only on Linux; this run records nothing");
        return Ok(None);
    }
    // A recorder named wrong is a mistake to fix, not a runner without one.
    let choice = std::env::var("QK_RECORDER").unwrap_or_default();
    if !matches!(choice.as_str(), "" | "auto" | "ebpf" | "strace") {
        bail!("unknown QK_RECORDER {choice:?}; use auto, ebpf or strace");
    }
    let directory = tempfile::tempdir().context("cannot create a directory for recordings")?;
    let mut recording = match qk_executor::Recording::probe(directory.path()) {
        Ok(recording) => recording,
        // A recorder asked for by name that cannot record is a mistake to
        // fix, as one named wrong is.
        Err(error) if matches!(choice.as_str(), "ebpf" | "strace") => return Err(error),
        Err(error) => {
            qk_executor::status!(
                "qk: --record records nothing, as no recorder works here: {error:#}"
            );
            return Ok(None);
        }
    };
    if resolving {
        recording = recording.resolving();
    }
    for (index, (id, task)) in graph.tasks.iter().enumerate() {
        let opted = task.definition.cache == Some(true)
            && task.definition.continuous != Some(true)
            && matches!(qk_cache::ultracache::Settings::of(task), Ok(Some(_)));
        if let Some(prepared) = prepared.get_mut(id).filter(|_| opted) {
            let traces = directory.path().join(format!("task-{index}"));
            std::fs::create_dir(&traces)?;
            prepared.recording = Some(recording.in_directory(traces));
            prepared.recording_keys = true;
        }
    }
    Ok(Some(directory))
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
    /// The threads it was told to start, for targets with `qk:threads`.
    pub threads: Option<usize>,
    /// The cores it held, for targets with `qk:threads`: its share of the
    /// budget, which `scale` can tell it to start more threads than.
    pub cores: Option<usize>,
    /// The CPU time, in milliseconds, its processes used together, sampled as
    /// it ran: at least that, since a process that exits between samples is
    /// missed. None when it ran too briefly to be sampled.
    pub cpu: Option<u64>,
    /// The most memory, in bytes, its processes used together, sampled as it
    /// ran; none when it ran too briefly to be sampled.
    pub memory: Option<u64>,
    /// How long, of the time between `ready` and `started`, it waited for
    /// free memory rather than for a slot.
    pub memory_wait: Duration,
    /// Bounded output and input verification for an actual cacheable execution.
    pub execution: Option<qk_cache::Execution>,
    /// How a task with `qk:ultracache` was keyed.
    pub recorded: Option<qk_cache::Recorded>,
    /// What the recordings of a task with `qk:ultracache` mode `"record"`
    /// would have done.
    pub shadow: Option<qk_cache::Shadow>,
    /// Under `--sandbox=audit`, what an audited task did beyond its
    /// declarations, empty when nothing.
    pub audit: Option<sandbox::Findings>,
}

/// How long a task can be expected to run, and the memory it can be expected
/// to use, from earlier runs that executed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    pub millis: u64,
    /// The cores it held: one, unless its target has `qk:threads`.
    pub cores: u64,
    /// The most memory, in bytes, it used, when that was measured.
    pub memory: Option<u64>,
}

impl Expected {
    /// Core-milliseconds: the time it takes on the cores it held.
    fn work(&self) -> u64 {
        self.millis.saturating_mul(self.cores)
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
    /// Run each task from scratch, restoring neither results nor warm state,
    /// and save what it gives in their place.
    pub cold: bool,
    /// Nx's `--nx-bail`: no task starts after one has failed.
    pub bail: bool,
    pub style: OutputStyle,
    /// Whether the static styles stream long-running tasks rather than hold
    /// their output: as in Nx, unless the user named a static style.
    pub stream_long_running: bool,
    /// Run each task sandboxed, auditing or enforcing what it declares.
    pub sandbox: Option<sandbox::Mode>,
    /// Execute finite tasks under an advisory file-access recorder.
    pub analyze_inputs: bool,
    /// Record what each task with `qk:ultracache` that executes reads, and
    /// key its result by that: see `qk_cache::recorded`. Linux only. With
    /// `sandbox` in audit mode, the tasks recorded are audited from their
    /// recordings.
    pub record: bool,
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
        cold,
        bail: bails,
        style,
        stream_long_running,
        sandbox: sandbox_mode,
        analyze_inputs,
        record,
        expected,
        cases,
    } = *settings;
    if cold && skip_cache {
        bail!("--cold saves what it runs, which --skip-cache does not; use one of them");
    }
    if analyze_inputs && sandbox_mode.is_some() {
        bail!("input analysis and sandbox enforcement/audit cannot be combined");
    }
    if record && (analyze_inputs || skip_cache) {
        bail!(
            "--record keys results in the cache, so it cannot be combined with inputs analyze or skipping the cache"
        );
    }
    // Landlock and the recorder are not yet tried together.
    if record && sandbox_mode == Some(sandbox::Mode::Enforce) {
        bail!("--record cannot be combined with --sandbox=enforce yet");
    }
    // An audit reads what the cache records, links resolved as its own are.
    let audits_recordings = record && sandbox_mode == Some(sandbox::Mode::Audit);
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
    // The project a single requested task belongs to, whose tasks Nx shows
    // in full under `static-failures-only`.
    let initiating = match graph.roots.iter().collect::<Vec<_>>()[..] {
        [root] => graph.tasks.get(root).map(|task| task.project.as_str()),
        _ => None,
    };
    // Where `summary` keeps the output of the tasks that fail.
    let logs = qk_cache::worktree_state(&workspace.root).join("terminal-outputs");
    // Validate every task before the first command can have side effects.
    qk_cache::scratch::check_overlaps(workspace, graph)?;
    let mut prepared = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            prepare(workspace, task, &process_environment)
                .and_then(|mut prepared| {
                    prepared.scratch = qk_cache::scratch::directories(workspace, task)?
                        .into_iter()
                        .map(|directory| qk_executor::Scratch {
                            root: workspace.root.clone(),
                            directory,
                        })
                        .collect();
                    Ok(prepared)
                })
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
                            &qk_executor::printable(
                                &task.project,
                                &task.target,
                                task.configuration.as_deref(),
                                &task.args,
                            ),
                            &task.project,
                            continuous,
                            graph.roots.contains(id),
                            initiating == Some(task.project.as_str()),
                            streams(task, stream_long_running),
                        )
                    };
                    if style == OutputStyle::Summary
                        && let Display::Failures { log, .. } = &mut prepared.display
                    {
                        let path = logs.join(log_name(id));
                        // Only this run's failure may name a log.
                        let _ = std::fs::remove_file(&path);
                        *log = Some(path);
                    }
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
    // Traces of the tasks this run records, kept until it ends.
    let recordings = record
        .then(|| attach_recordings(graph, &mut prepared, audits_recordings))
        .transpose()?
        .flatten();
    // An audit of what is recorded needs a recorder as much: where none works,
    // the run goes on unaudited, as it goes on unrecorded.
    let sandbox_mode = if audits_recordings && cfg!(target_os = "linux") && recordings.is_none() {
        qk_executor::status!("qk: nothing is audited, as nothing can be recorded here");
        None
    } else {
        sandbox_mode
    };
    let sandbox = sandbox_mode
        .map(|mode| sandbox::Sandbox::start(workspace, graph, mode))
        .transpose()?;
    let recorder = analyze_inputs
        .then(|| recorder::Recorder::start(workspace, graph, &mut prepared))
        .transpose()?;
    // As in Nx 24.0.1, a task waiting for a server with a `signal` check may
    // learn its address in NX_READY_URL, and resolves its own readyWhen once
    // that server is ready. Until then, its readyWhen counts as invalid only
    // when it is invalid both without an address and with a stand-in one,
    // which then says why. Only a signal check reports an address.
    let signalled: BTreeSet<String> = graph
        .tasks
        .iter()
        .filter(|(_, task)| {
            task.dependencies.iter().any(|dependency| {
                task.edge(dependency).wait_for == qk_taskgraph::WaitFor::Ready
                    && graph.tasks[dependency].definition.continuous == Some(true)
                    && matches!(
                        &prepared[dependency].ready_check,
                        Some(Ok(check)) if check.probe == qk_executor::readiness::Probe::Signal
                    )
            })
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in &signalled {
        let task = prepared.get_mut(id).unwrap();
        if matches!(task.ready_check, Some(Err(_))) {
            task.ready_check = task.resolve_ready_check(Some("http://localhost/"));
        }
    }
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
    // As in Nx 24, a task depending on a continuous task with
    // `waitFor: "ready"` waits for the target's own readyWhen check.
    let waiters: BTreeMap<&str, BTreeSet<&str>> = {
        let mut waiters: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (id, task) in &graph.tasks {
            for dependency in &task.dependencies {
                if continuous.contains(dependency)
                    && prepared[dependency].ready_check.is_some()
                    && task.edge(dependency).wait_for == qk_taskgraph::WaitFor::Ready
                {
                    waiters.entry(dependency).or_default().insert(id);
                }
            }
        }
        waiters
    };
    let waits =
        |id: &str, dependency: &str| waiters.get(dependency).is_some_and(|w| w.contains(id));
    // Whether a running dependency lets `id` start: once it is up, and for a
    // task that waits for it, once its check passed.
    let serves = |id: &str, dependency: &str| {
        is_up(dependency) && (!waits(id, dependency) || prepared[dependency].verdict.is_ready())
    };
    // Whether a dependency `id` waits for failed its check, which skips `id`.
    let not_ready = |id: &str, dependency: &str| {
        waits(id, dependency) && prepared[dependency].verdict.failure().is_some()
    };
    // Running tasks holding a --parallel slot: as in Nx, one with readyWhen
    // gives its slot up once ready.
    let holding = |active: &BTreeSet<String>| {
        active
            .iter()
            .filter(|id| !continuous.contains(*id) && !(announced.contains(*id) && is_up(id)))
            .count()
    };
    // Serving dependencies stay active beside their dependents, including
    // readyWhen tasks without continuous: true. Either side running alone
    // would prevent the dependent from starting and the server from stopping.
    for (id, task) in &graph.tasks {
        for dependency in task.dependencies.iter().filter(|id| serving.contains(*id)) {
            let kind = if continuous.contains(dependency) {
                "continuous"
            } else {
                "readyWhen"
            };
            if alone.contains(id) {
                bail!("{id} does not support parallelism but depends on {kind} task {dependency}");
            }
            if alone.contains(dependency) {
                bail!(
                    "{kind} task {dependency} does not support parallelism but {id} depends on it"
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
    let memory_expected = memory::expected(
        graph
            .tasks
            .iter()
            .map(|(id, task)| (id.as_str(), task.target.as_str())),
        expected,
    );
    let mut pending: BTreeSet<_> = graph.tasks.keys().cloned().collect();
    let mut active = BTreeSet::new();
    // Stop signals for running continuous tasks, and those already asked to stop.
    let mut stops: BTreeMap<String, Arc<AtomicBool>> = BTreeMap::new();
    let mut stopping = BTreeSet::new();
    // Tasks stopped because the readiness check something waits for failed.
    let mut not_ready_failed: BTreeSet<String> = BTreeSet::new();
    // Tasks that exited before the check something waits for passed.
    let mut exited_unready: BTreeSet<String> = BTreeSet::new();
    // Tasks that started with a valid check, as resolved when they started.
    let mut checked: BTreeSet<String> = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
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
    // As in Nx, every task is hashed, and sees its hash as NX_TASK_HASH,
    // whether or not anything can be cached; a run that cannot use the
    // cache only observes. A recorded task runs as it is; a sandboxed one is
    // checked when it runs, and restored from the cache otherwise.
    let cache = if analyze_inputs {
        None
    } else if skip_cache || !(warm || cacheable) {
        Some(qk_cache::Cache::for_observation(workspace))
    } else if cold {
        Some(qk_cache::Cache::for_workspace(workspace, &environment).cold())
    } else {
        Some(qk_cache::Cache::for_workspace(workspace, &environment))
    };
    // Warm state its tasks fetch while their dependencies run.
    if let Some(cache) = &cache {
        // Only those whose restore runs as they start: not a continuous
        // task, nor one narrowed to some of its cases, which run uncached.
        cache.prefetch_warm(workspace, graph, &prepared, |id| {
            graph.tasks[id].definition.continuous != Some(true) && !case_files.contains_key(id)
        });
    }
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
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let reserve = system.total_memory() / 10;
    let seen = std::sync::Mutex::new(Seen {
        free: system.available_memory(),
        ..Seen::default()
    });
    // Since when each task has waited for memory, and how long it has
    // waited in all.
    let mut memory_since: BTreeMap<String, SystemTime> = BTreeMap::new();
    let mut memory_waits: BTreeMap<String, Duration> = BTreeMap::new();
    std::thread::scope(|scope| -> Result<()> {
        let (finish_sampling, sampling_finished) = mpsc::channel::<()>();
        let load = &load;
        let waiting = &waiting;
        let seen = &seen;
        let prepared = &prepared;
        // Samples how busy the machine is, so the summary can tell a run held
        // back by --parallel from one held back by the machine, and its free
        // memory and what each running task uses.
        scope.spawn(move || {
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
                system.refresh_memory();
                let roots: BTreeMap<&str, BTreeSet<u32>> = prepared
                    .iter()
                    .map(|(id, task)| (id.as_str(), task.processes.lock().unwrap().clone()))
                    .filter(|(_, processes)| !processes.is_empty())
                    .collect();
                let used = if roots.is_empty() {
                    BTreeMap::new()
                } else {
                    memory::sample(&mut system, &roots)
                };
                let seen = &mut *seen.lock().unwrap();
                seen.free = system.available_memory();
                seen.now = used
                    .iter()
                    .map(|(id, sampled)| ((*id).to_owned(), sampled.used.now))
                    .collect();
                for (id, used) in &used {
                    let peak = seen.peaks.entry((*id).to_owned()).or_default();
                    *peak = (*peak).max(used.used.peak);
                    let cpu = seen.cpu.entry((*id).to_owned()).or_default();
                    for (pid, millis) in &used.cpu {
                        let kept = cpu.entry(*pid).or_default();
                        *kept = (*kept).max(*millis);
                    }
                }
                seen.run = seen.run.max(used.values().map(|used| used.used.now).sum());
            }
        });
        let result = (|| -> Result<()> {
            while !pending.is_empty() || !active.is_empty() {
                let mut waiting_now = false;
                // The tasks this pass held back for want of memory.
                let mut short_of_memory = BTreeSet::new();
                // A check that failed while something waits for it fails its
                // task, as in Nx; the waiting tasks are skipped.
                for (id, stop) in &stops {
                    if waiters.contains_key(id.as_str())
                        && !not_ready_failed.contains(id)
                        && let Some(reason) = prepared[id].verdict.failure()
                    {
                        qk_executor::status!("qk: {reason}");
                        not_ready_failed.insert(id.clone());
                        stop.store(true, Ordering::SeqCst);
                    }
                }
                // A task that became ready gives back its core.
                held.retain(|id, _| !(announced.contains(id) && is_up(id)));
                let dependencies_done = |id: &String| {
                    graph.tasks[id].dependencies.iter().all(|dependency| {
                        outcomes.get(dependency) == Some(&Outcome::Success)
                            || (active.contains(dependency) && serves(id, dependency))
                    })
                };
                // The finite tasks that could start in this pass, which the
                // threaded ones among them share the free cores with.
                let finite_pending = pending.difference(&continuous).count();
                let ready_threaded = pending
                    .difference(&continuous)
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
                // The core-milliseconds each finite task still to finish is
                // expected to take: what a pending one takes, and what a
                // running one has still to go.
                let now = SystemTime::now();
                let work_left: Vec<(String, u64)> = pending
                    .iter()
                    .filter(|id| !continuous.contains(*id))
                    .filter_map(|id| Some((id.clone(), expected.get(id)?.work())))
                    .chain(held.iter().filter_map(|(id, cores)| {
                        let elapsed = now.duration_since(started[id]).unwrap_or_default();
                        Some((id.clone(), expected.get(id)?.remaining(elapsed, *cores)))
                    }))
                    .collect();
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
                    // As in Nx, a task waiting for a server that exited
                    // before its check passed fails, naming the server.
                    if let Some(server) = dependencies.iter().find(|dependency| {
                        waits(&id, dependency) && exited_unready.contains(*dependency)
                    }) {
                        let reason = format!("Task \"{server}\" exited before it became ready.");
                        pending.remove(&id);
                        active.insert(id.clone());
                        ready.entry(id.clone()).or_insert_with(SystemTime::now);
                        started.insert(id.clone(), SystemTime::now());
                        report::event(Event::Started {
                            id: id.clone(),
                            checking: false,
                        });
                        let sender = sender.clone();
                        scope.spawn(move || {
                            let _ = sender.send((id, Err(anyhow::anyhow!(reason))));
                        });
                        continue;
                    }
                    if dependencies.iter().any(|dependency| {
                        skipped.contains(dependency)
                            // Once its failure has been reported: a check can
                            // fail at any moment, and the reason comes first.
                            || (not_ready(&id, dependency)
                                && (not_ready_failed.contains(dependency)
                                    || !active.contains(dependency)))
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
                            || (active.contains(dependency) && serves(&id, dependency))
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
                                // with an expectation gets the part of the cores
                                // that its work is of the work that can run
                                // beside it, so that a short one leaves cores
                                // for longer ones and a long one is not left
                                // running once the rest is done. One without
                                // takes the even split, which was made before
                                // this pass started tasks; neither can exceed
                                // what they left.
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
                                                    && serves(id, dependency))
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
                                    let sized = match expected.get(&id) {
                                        Some(own) => proportion(
                                            own.work(),
                                            beside(&id, &work_left, &dependents, parallel),
                                            cores,
                                        ),
                                        None => even,
                                    };
                                    sized.min(free.saturating_sub(reserved))
                                };
                                config.share(cores, even, free, held.is_empty())
                            }
                            None => (free > 0 || held.is_empty()).then_some(1),
                        };
                        let Some(cores_for) = cores_for else {
                            continue;
                        };
                        if let Some(need) = memory_expected.get(id.as_str())
                            && !held.is_empty()
                        {
                            let seen = seen.lock().unwrap();
                            let growth = held
                                .keys()
                                .filter_map(|other| {
                                    let expected = memory_expected.get(other.as_str())?;
                                    let now = seen.now.get(other).copied().unwrap_or(0);
                                    Some(expected.saturating_sub(now))
                                })
                                .fold(0u64, u64::saturating_add);
                            if !memory::fits(*need, seen.free, growth, reserve) {
                                short_of_memory.insert(id.clone());
                                continue;
                            }
                        }
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
                        if task.commands.is_empty() {
                            sandbox.ran_nothing(&id);
                        } else if let Some(recording) =
                            sandbox.recording(&id, &task.cwd, task.recording.as_ref())
                        {
                            task.recording = Some(recording);
                        }
                    }
                    // As in Nx, a task learns the address of the one server it
                    // waited for that reported one.
                    let urls: Vec<String> = graph.tasks[&id]
                        .dependencies
                        .iter()
                        .filter(|dependency| waits(&id, dependency))
                        .filter_map(|dependency| prepared[dependency].verdict.url())
                        .collect();
                    let url = match &urls[..] {
                        [url] => Some(url.as_str()),
                        _ => None,
                    };
                    if let Some(url) = url {
                        task.execution
                            .insert(qk_executor::ready_signal::URL_ENV.into(), url.into());
                    }
                    // Its own readyWhen, with the address it learnt, unless
                    // it is invalid whatever the address.
                    if signalled.contains(&id) && !matches!(task.ready_check, Some(Err(_))) {
                        task.ready_check = task.resolve_ready_check(url);
                    }
                    let sender = sender.clone();
                    if serving.contains(&id) {
                        report::event(Event::StartedContinuous { id: id.clone() });
                        if !announced.contains(&id) {
                            prepared[&id].ready.store(true, Ordering::SeqCst);
                        }
                        let stop = Arc::new(AtomicBool::new(false));
                        stops.insert(id.clone(), stop.clone());
                        let waited = waiters.contains_key(id.as_str());
                        // A port the server picks leaves nothing to check:
                        // it is ready once started.
                        if waited && task.ready_check.is_none() {
                            task.verdict.set_ready();
                        }
                        if matches!(task.ready_check, Some(Ok(_))) {
                            checked.insert(id.clone());
                        }
                        // A `signal` check's socket, kept while the task runs,
                        // or why it could not be opened.
                        let mut listener = None;
                        let mut unheard = None;
                        if waited
                            && let Some(Ok(check)) = &task.ready_check
                            && check.probe == qk_executor::readiness::Probe::Signal
                        {
                            match qk_executor::ready_signal::Listener::start(
                                &id,
                                task.verdict.clone(),
                            ) {
                                Ok(started) => {
                                    task.execution.extend(started.env(&id));
                                    listener = Some(started);
                                }
                                Err(error) => unheard = Some(format!("{error:#}")),
                            }
                        }
                        // As in Nx, an invalid readyWhen fails the task only
                        // when something in the run waits for it.
                        let invalid = match &task.ready_check {
                            _ if unheard.is_some() => unheard,
                            Some(Err(reason)) if waited => Some(reason.clone()),
                            Some(Err(reason)) => {
                                qk_executor::status!(
                                    "qk: {reason} Nothing in this run waits for it to be ready, \
                                     so it runs without a readiness check."
                                );
                                None
                            }
                            Some(Ok(check)) if waited => {
                                qk_executor::status!(
                                    "qk: waiting for {id} to be ready ({})",
                                    check.describe()
                                );
                                let (check, verdict, stop) =
                                    (check.clone(), task.verdict.clone(), stop.clone());
                                let (id, root) = (id.clone(), workspace.root.clone());
                                scope.spawn(move || {
                                    qk_executor::readiness::wait(
                                        &id, &check, &root, &verdict, &stop,
                                    )
                                });
                                None
                            }
                            _ => None,
                        };
                        scope.spawn(move || {
                            let _listener = listener;
                            let result = match invalid {
                                Some(reason) => Err(anyhow::anyhow!(reason)),
                                None => execute(&task, &stop).map(qk_cache::TaskResult::uncached),
                            };
                            if let Some(recording) = &task.recording {
                                let _ = recording.settle();
                            }
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
                    if let (Some(threads), Some(cores)) = (threads.get(&id), given.get(&id)) {
                        task.execution
                            .extend(threads.environment(threads.threads(*cores)));
                    }
                    if let Some(path) = narrowed {
                        task.execution
                            .insert("QK_AFFECTED_CASES".into(), path.as_os_str().to_owned());
                    }
                    let cache = cache.as_ref().filter(|_| narrowed.is_none());
                    let cancelled = cancelled.clone();
                    let sandbox = sandbox.as_ref();
                    scope.spawn(move || {
                        let run = |task: &qk_executor::PreparedTask| match cache {
                            Some(cache) => cache.run(workspace, graph, definition, task, &cancelled),
                            None => execute_retained(task, &cancelled),
                        };
                        let result = run(&task).and_then(|result| {
                            // Seatbelt refuses a sandbox inside one that
                            // restricts anything, as Xcode applies to Swift
                            // macros. An audit runs such a task again
                            // unsandboxed, as one whose profile Seatbelt
                            // rejects; enforcing, it fails, and says how to
                            // leave it out. A failure is not cached, so the
                            // cache runs it again too.
                            match sandbox {
                                Some(sandbox)
                                    if sandbox.mode == sandbox::Mode::Enforce
                                        && sandbox::nested(&task, &result) =>
                                {
                                    qk_executor::status!(
                                        "qk: {id}: it applies a sandbox of its own, which cannot run inside qk's; set \"qk:sandbox\": false on its target to run it unsandboxed"
                                    );
                                    Ok(result)
                                }
                                Some(sandbox) if sandbox::nested(&task, &result) => {
                                    let reason =
                                        "it applies a sandbox of its own, which cannot run inside qk's";
                                    qk_executor::status!(
                                        "qk: {id}: {reason}; running it again unsandboxed"
                                    );
                                    sandbox.unconfined(&id, reason);
                                    run(&qk_executor::PreparedTask {
                                        sandbox: None,
                                        ..task.clone()
                                    })
                                }
                                _ => Ok(result),
                            }
                        });
                        // Settled as the task ends: an eBPF recording fails
                        // when the recorder dropped events since it began, so
                        // settled when the run ends it would fail for what
                        // later tasks lost. A failure is kept for the audit.
                        if let Some(recording) = &task.recording {
                            let _ = recording.settle();
                        }
                        let _ = sender.send((id, result));
                    });
                }
                waiting.store(waiting_now, Ordering::Relaxed);
                // A wait for memory lasts from the first pass that held the
                // task back for it to the first that did not.
                let now = SystemTime::now();
                memory_since.retain(|id, since| {
                    if short_of_memory.contains(id) {
                        return true;
                    }
                    *memory_waits.entry(id.clone()).or_default() +=
                        now.duration_since(*since).unwrap_or_default();
                    false
                });
                for id in short_of_memory {
                    memory_since.entry(id).or_insert(now);
                }
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
                        && !not_ready_failed.contains(id)
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
                        // A task that ends before its check passes cannot
                        // serve the tasks waiting for it.
                        if waiters.contains_key(id.as_str()) {
                            let verdict = &prepared[&id].verdict;
                            // One step decides whether the task ended before
                            // its check did, so a check that fails meanwhile
                            // cannot slip between two reads.
                            if verdict.fail(format!("Task \"{id}\" exited before it became ready."))
                            {
                                // Only a task that started with a valid check
                                // exited early; an invalid one never ran.
                                if checked.contains(&id)
                                    && !stopping.contains(&id)
                                    && !cancelled.load(Ordering::SeqCst)
                                {
                                    exited_unready.insert(id.clone());
                                }
                            } else if let Some(reason) = verdict.failure() {
                                // The check failed before the pass that
                                // reports it saw the task, which has now ended.
                                if not_ready_failed.insert(id.clone()) {
                                    qk_executor::status!("qk: {reason}");
                                }
                            }
                        }
                        let (outcome, cache, key, warm, execution, recorded, shadow) = match result
                        {
                            Ok(result) => (
                                result.outcome,
                                result.cache,
                                result.key,
                                result.warm,
                                result.execution,
                                result.recorded,
                                result.shadow,
                            ),
                            Err(error) => {
                                qk_executor::status!("qk: {id}: {error:#}");
                                (
                                    Outcome::Failed(1),
                                    qk_cache::CacheStatus::Uncached,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                )
                            }
                        };
                        let outcome = if not_ready_failed.contains(&id) {
                            Outcome::Failed(1)
                        } else {
                            outcome
                        };
                        // A task restored from the cache did not run, so there
                        // is nothing to check.
                        if let Some(sandbox) = &sandbox
                            && matches!(
                                cache,
                                qk_cache::CacheStatus::LocalHit | qk_cache::CacheStatus::RemoteHit
                            )
                        {
                            sandbox.restored(&id);
                        }
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
                                Outcome::Failed(_) => {
                                    // As in Nx, a failure fails the run with 1,
                                    // whatever the task's own code.
                                    if exit_code == 0 {
                                        exit_code = 1;
                                    }
                                    bailed |= bails;
                                }
                                Outcome::Cancelled => exit_code = 130,
                            }
                            outcome
                        };
                        // One lock for both: guards in the record below
                        // would live until it is built.
                        let (memory, cpu) = {
                            let mut seen = seen.lock().unwrap();
                            let cpu = seen
                                .cpu
                                .remove(&id)
                                .map(|processes| processes.values().sum());
                            (seen.peaks.remove(&id), cpu)
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
                                threads: threads
                                    .get(&id)
                                    .zip(given.get(&id))
                                    .map(|(threads, cores)| threads.threads(*cores)),
                                cores: given.remove(&id),
                                memory_wait: memory_waits.remove(&id).unwrap_or_default(),
                                memory: memory.filter(|bytes| *bytes > 0),
                                cpu: cpu.filter(|millis: &u64| *millis > 0),
                                execution,
                                recorded,
                                shadow,
                                audit: None,
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
        // What was fetched ahead for a cancelled run is of no use to it.
        if cancelled.load(Ordering::SeqCst) {
            cache.abandon_prefetches();
        }
        cache.finish(workspace);
    }
    if cancelled.load(Ordering::SeqCst) {
        exit_code = 130;
    } else if !skipped.is_empty() {
        // A skipped task fails the run too, as in Nx.
        exit_code = exit_code.max(1);
    }
    let sandbox = sandbox
        .map(|sandbox| -> Result<SandboxResult> {
            let mode = sandbox.mode;
            let mut finished = sandbox.finish()?;
            // How a task would have been held says nothing of one that never
            // started, or was restored from the cache.
            let ran = |id: &String| records.contains_key(id) && !finished.restored.contains(id);
            finished.unsandboxed.retain(|id, _| ran(id));
            // A task that started no command was never refused.
            finished.refused.retain(|id, _| {
                ran(id)
                    && records
                        .get(id)
                        .is_some_and(|record| matches!(record.outcome, Outcome::Failed(_)))
            });
            finished.packages_unchecked.retain(|id, _| ran(id));
            finished.audited.retain(|id| records.contains_key(id));
            if mode == sandbox::Mode::Audit {
                for id in &finished.audited {
                    if let Some(record) = records.get_mut(id) {
                        record.audit = Some(finished.findings.get(id).cloned().unwrap_or_default());
                    }
                }
            }
            Ok(SandboxResult {
                mode,
                findings: finished.findings,
                unsandboxed: finished.unsandboxed,
                refused: finished.refused,
                packages_unchecked: finished.packages_unchecked,
                audited: finished.audited,
                restored: finished.restored,
            })
        })
        .transpose()?;
    let input_analysis = recorder
        .map(|recorder| recorder.finish(workspace, graph, &prepared, &outcomes))
        .transpose()?;
    let mut ran_threaded = records
        .iter()
        .filter(|(_, record)| {
            record.cores.is_some()
                && matches!(
                    record.cache,
                    qk_cache::CacheStatus::Miss | qk_cache::CacheStatus::Uncached
                )
        })
        .map(|(id, _)| id)
        .peekable();
    let unexpected =
        ran_threaded.peek().is_some() && ran_threaded.all(|id| !expected.contains_key(id));
    Ok(RunResult {
        outcomes,
        skipped,
        exit_code,
        tasks: records,
        load: load.into_inner().unwrap(),
        memory: Some(seen.into_inner().unwrap().run).filter(|bytes| *bytes > 0),
        unexpected,
        sandbox,
        input_analysis,
    })
}

/// Whether a task's output streams rather than being held, as Nx decides
/// it: `NX_STREAM_OUTPUT` when it says, else for a long-running task, one
/// that is continuous, runs with `--watch` or has a server's target name.
fn streams(task: &qk_taskgraph::Task, long_running: bool) -> bool {
    match std::env::var("NX_STREAM_OUTPUT").as_deref() {
        Ok("true") => return true,
        Ok("false") => return false,
        _ => {}
    }
    let watching = task.args.iter().any(|arg| {
        arg == "--watch"
            || arg
                .strip_prefix("--watch=")
                .is_some_and(|value| value != "false")
    });
    let target = &task.target;
    long_running
        && (task.definition.continuous == Some(true)
            || watching
            || target.ends_with(":watch")
            || target.ends_with("-watch")
            || matches!(target.as_str(), "serve" | "dev" | "start"))
}

/// The file `summary` keeps a failed task's output in: its id, with every
/// byte a file name cannot hold, and `_` itself, written as `_` and its hex
/// value, so two ids never share a file.
pub fn log_name(id: &str) -> String {
    let mut name = String::new();
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.') {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("_{byte:02x}"));
        }
    }
    format!("{name}.log")
}

/// What the sampler last saw of memory, in bytes.
#[derive(Default)]
struct Seen {
    /// Free on the machine.
    free: u64,
    /// What each running task uses now.
    now: BTreeMap<String, u64>,
    /// The most each task has used.
    peaks: BTreeMap<String, u64>,
    /// The most CPU time, in milliseconds, each process of each task has
    /// been seen to have used.
    cpu: BTreeMap<String, BTreeMap<u32, u64>>,
    /// The most the run's tasks have used together.
    run: u64,
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
        // Saturating: a shared record can name any time.
        let path = expected
            .get(id)
            .map_or(0, |expected| expected.millis)
            .saturating_add(after);
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

/// The share of `cores` matching the share `work` is of `concurrent` work,
/// rounded up; none when there is no work.
fn proportion(work: u64, concurrent: u64, cores: usize) -> usize {
    if concurrent == 0 {
        return 0;
    }
    (work as u128 * cores as u128).div_ceil(concurrent as u128) as usize
}

/// The work that can run beside task `id`, its own included: of the tasks
/// still to finish, `work_left`, those that do not depend on it, which wait
/// for it to end, and of those the most that the other `parallel` slots can
/// hold at once, the largest first.
fn beside(
    id: &str,
    work_left: &[(String, u64)],
    dependents: &BTreeMap<&str, Vec<&str>>,
    parallel: usize,
) -> u64 {
    let mut after = BTreeSet::new();
    let mut next = vec![id];
    while let Some(task) = next.pop() {
        for dependent in dependents.get(task).into_iter().flatten() {
            if after.insert(*dependent) {
                next.push(dependent);
            }
        }
    }
    let mut others: Vec<u64> = work_left
        .iter()
        .filter(|(other, _)| other != id && !after.contains(other.as_str()))
        .map(|(_, work)| *work)
        .collect();
    others.sort_unstable_by(|a, b| b.cmp(a));
    let own = work_left
        .iter()
        .find(|(other, _)| other == id)
        .map_or(0, |(_, work)| *work);
    others
        .into_iter()
        .take(parallel.saturating_sub(1))
        .fold(own, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::Expected;
    use std::time::Duration;

    #[test]
    fn distinct_task_ids_log_to_distinct_files() {
        assert_ne!(super::log_name("a:b_c"), super::log_name("a_b:c"));
        assert_eq!(super::log_name("app:build"), "app_3abuild.log");
    }
    #[test]
    fn work_beside_a_task_leaves_out_what_waits_for_it() {
        // `lib` feeds `test`; `other` and `small` are independent of both.
        let dependents = [("lib", vec!["test"])].into_iter().collect();
        let work_left: Vec<(String, u64)> =
            [("lib", 10), ("test", 1_000), ("other", 50), ("small", 5)]
                .into_iter()
                .map(|(id, work)| (id.to_owned(), work))
                .collect();
        // What depends on it cannot run beside it.
        assert_eq!(super::beside("lib", &work_left, &dependents, 4), 65);
        assert_eq!(super::beside("other", &work_left, &dependents, 4), 1_065);
        // The other slots hold the largest of the rest.
        assert_eq!(super::beside("other", &work_left, &dependents, 2), 1_050);
        assert_eq!(super::beside("other", &work_left, &dependents, 1), 50);
        assert_eq!(super::proportion(50, 1_065, 16), 1);
        assert_eq!(super::proportion(1_000, 1_065, 16), 16);
        assert_eq!(super::proportion(0, 0, 16), 0);
    }

    #[test]
    fn remaining_work_accounts_for_the_current_allocation() {
        let expected = Expected {
            millis: 1_000,
            cores: 4,
            memory: None,
        };
        assert_eq!(expected.remaining(Duration::ZERO, 8), 4_000);
        assert_eq!(expected.remaining(Duration::from_millis(250), 8), 2_000);
        assert_eq!(expected.remaining(Duration::from_millis(500), 8), 0);
        assert_eq!(expected.remaining(Duration::from_millis(500), 2), 3_000);
        assert_eq!(expected.remaining(Duration::from_secs(10), 8), 0);
        assert_eq!(expected.remaining(Duration::MAX, 8), 0);
    }
}
