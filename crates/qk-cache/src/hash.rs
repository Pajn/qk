use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_executor::{Capture, Display, Outcome, PreparedTask, execute_captured, read_capture};
use qk_graph::ProjectGraph;
use qk_lockfile::Lockfile;
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

use crate::glob::is_literal;
use crate::paths::{self, Outputs, Pattern};

pub fn digest_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut reader = File::open(path)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Metadata that changes whenever a file's content can have changed: size,
/// modification time and, on Unix, change time, inode and mode.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Stamp(Vec<i64>);

impl Stamp {
    fn new(metadata: &std::fs::Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(-1, |duration| duration.as_nanos() as i64);
        #[allow(unused_mut)]
        let mut values = vec![metadata.len() as i64, modified];
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            values.extend([
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.ino() as i64,
                i64::from(metadata.mode()),
            ]);
        }
        Self(values)
    }

    /// A file written within the timestamp resolution window could change again
    /// without changing its stamp, so recent files are always re-read.
    fn settled(&self) -> bool {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_nanos(self.0[1].max(0) as u64);
        self.0[1] >= 0
            && SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > Duration::from_secs(2))
    }
}

/// Digests persisted between runs in the worktree's state, keyed by path and
/// stamp, so a warm run reads only the files that changed. Stamps include the
/// inode, so they belong to one worktree.
fn digests_path(root: &Path) -> PathBuf {
    paths::worktree_state(root).join("digests.json")
}

/// Workspace state shared by every fingerprint in one run. The candidate file list
/// is taken once, like Nx's file map; file contents are still re-read whenever
/// their metadata changes, so edits during the run are detected.
pub struct Snapshot {
    pub(crate) files: BTreeSet<String>,
    projects: ProjectGraph,
    canonical_root: PathBuf,
    workspace_prefix: Option<PathBuf>,
    patterns: Mutex<HashMap<(String, bool), Arc<Pattern>>>,
    digests: Mutex<HashMap<String, (Stamp, String)>>,
    /// Whether `digests` gained entries worth saving.
    digests_changed: std::sync::atomic::AtomicBool,
    /// Directories already checked not to be symlinks.
    directories: Mutex<std::collections::HashSet<PathBuf>>,
    /// Runtime input results by command and environment, computed once per run like Nx.
    runtime: Mutex<HashMap<String, Value>>,
    /// The parsed pnpm lockfile, replaced whenever its content changes.
    lockfile: Mutex<Option<Arc<Installed>>>,
}

/// One revision of `pnpm-lock.yaml`, with digests of what it installs computed
/// once and shared by every task that asks.
struct Installed {
    content: String,
    /// `None` when the file cannot be read as a pnpm v9 lockfile.
    lockfile: Option<Lockfile>,
    digests: Mutex<HashMap<String, String>>,
}

impl Installed {
    fn digest(&self, key: &str, fingerprints: impl FnOnce() -> BTreeSet<String>) -> String {
        if let Some(digest) = self.digests.lock().unwrap().get(key) {
            return digest.clone();
        }
        let mut hasher = blake3::Hasher::new();
        for fingerprint in fingerprints() {
            hasher.update(fingerprint.as_bytes());
            hasher.update(b"\n");
        }
        let digest = hasher.finalize().to_hex().to_string();
        self.digests
            .lock()
            .unwrap()
            .insert(key.to_owned(), digest.clone());
        digest
    }
}

impl Snapshot {
    pub fn new(workspace: &Workspace, graph: &TaskGraph, cache_path: &Path) -> Result<Self> {
        // Parsing the lockfile takes as long as listing files, and every early
        // task needs it, so the two overlap.
        let (files, lockfile) = std::thread::scope(|scope| {
            let lockfile = scope.spawn(|| parse_lockfile(&workspace.root.join("pnpm-lock.yaml")));
            let files = source_files(&workspace.root);
            (files, lockfile.join().expect("lockfile thread panicked"))
        });
        let mut files = files?;
        if let Ok(cache_relative) = cache_path.strip_prefix(&workspace.root) {
            files.retain(|path| !Path::new(path).starts_with(cache_relative));
        }
        // Generated artifacts must not make the next invocation invalidate itself.
        // Another task's unsupported outputs stay candidates: that only costs misses,
        // and so do Nx's default outputs, which may well hold sources.
        let mut generated = BTreeSet::new();
        for outputs in graph
            .tasks
            .values()
            .filter_map(|task| Outputs::new(workspace, task).ok())
            .filter(Outputs::is_explicit)
        {
            for anchor in outputs.anchors() {
                generated.extend(
                    under(&files, anchor)
                        .filter(|path| outputs.matches(path))
                        .cloned(),
                );
            }
        }
        // Warm scratch paths are never inputs either.
        for task in graph.tasks.values() {
            let Ok(Some(warm)) = crate::warm::config(workspace, task) else {
                continue;
            };
            if let Ok(paths) = Outputs::from_paths(&warm.paths) {
                for anchor in paths.anchors() {
                    generated.extend(
                        under(&files, anchor)
                            .filter(|path| paths.matches(path))
                            .cloned(),
                    );
                }
            }
        }
        files.retain(|path| !generated.contains(path));
        let canonical_root = workspace.root.canonicalize()?;
        let workspace_prefix = paths::git_path(&workspace.root, "--show-toplevel")
            .and_then(|root| root.canonicalize().ok())
            .and_then(|root| canonical_root.strip_prefix(root).ok().map(PathBuf::from));
        Ok(Self {
            files,
            projects: ProjectGraph::build(workspace)?,
            canonical_root,
            workspace_prefix,
            patterns: Mutex::default(),
            digests: Mutex::new(load_digests(&workspace.root)),
            digests_changed: Default::default(),
            directories: Mutex::default(),
            runtime: Mutex::default(),
            lockfile: Mutex::new(lockfile),
        })
    }

    /// Saves the digests of files that still exist, for the next run.
    pub fn save_digests(&self, root: &Path) {
        if !self
            .digests_changed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let digests = self.digests.lock().unwrap();
        let entries: BTreeMap<&String, (&Stamp, &String)> = digests
            .iter()
            .filter(|(path, _)| self.files.contains(*path))
            .map(|(path, (stamp, digest))| (path, (stamp, digest)))
            .collect();
        let path = digests_path(root);
        let saved = (|| -> Result<()> {
            let directory = path.parent().context("digests have a directory")?;
            std::fs::create_dir_all(directory)?;
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            serde_json::to_writer(
                std::io::BufWriter::new(file.as_file_mut()),
                &json!({"version": 1, "entries": entries}),
            )?;
            file.persist(&path)?;
            Ok(())
        })();
        if let Err(error) = saved {
            qk_executor::status!("qk: could not save file digests: {error:#}");
        }
    }

    /// `paths::safe_parents`, checking each directory once per run.
    fn safe_parents(&self, root: &Path, path: &str) -> Result<()> {
        paths::validate_path(path)?;
        let mut current = root.to_owned();
        for part in Path::new(path)
            .parent()
            .into_iter()
            .flat_map(Path::components)
        {
            current.push(part);
            if self.directories.lock().unwrap().contains(&current) {
                continue;
            }
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("input parent is a symlink: {}", current.display())
                }
                Ok(metadata) if !metadata.is_dir() => {
                    bail!("input parent is not a directory: {}", current.display())
                }
                Ok(_) => {
                    self.directories.lock().unwrap().insert(current.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn installed(&self, root: &Path) -> Result<Option<Arc<Installed>>> {
        let absolute = root.join("pnpm-lock.yaml");
        let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Ok(None);
        }
        let content = self.digest("pnpm-lock.yaml", &absolute, &metadata)?;
        let mut current = self.lockfile.lock().unwrap();
        if let Some(installed) = &*current
            && installed.content == content
        {
            return Ok(Some(installed.clone()));
        }
        let Some(installed) = parse_lockfile(&absolute) else {
            return Ok(None);
        };
        *current = Some(installed.clone());
        Ok(Some(installed))
    }

    /// Adds paths to the candidate files, such as files deleted since a base
    /// revision.
    pub fn with_candidates(mut self, paths: &[String]) -> Self {
        self.files.extend(paths.iter().cloned());
        self
    }

    fn pattern(&self, pattern: &str, negated: bool) -> Result<Arc<Pattern>> {
        let key = (pattern.to_owned(), negated);
        if let Some(pattern) = self.patterns.lock().unwrap().get(&key) {
            return Ok(pattern.clone());
        }
        let compiled = Arc::new(Pattern::new(pattern, negated)?);
        self.patterns.lock().unwrap().insert(key, compiled.clone());
        Ok(compiled)
    }

    fn digest(&self, path: &str, absolute: &Path, metadata: &std::fs::Metadata) -> Result<String> {
        let stamp = Stamp::new(metadata);
        if let Some((known, digest)) = self.digests.lock().unwrap().get(path)
            && *known == stamp
        {
            return Ok(digest.clone());
        }
        let digest = digest_file(absolute)?;
        // Re-check after reading so a concurrent write is never recorded as settled.
        let after = Stamp::new(&std::fs::symlink_metadata(absolute)?);
        if after == stamp && stamp.settled() {
            self.digests
                .lock()
                .unwrap()
                .insert(path.to_owned(), (stamp, digest.clone()));
            self.digests_changed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(digest)
    }

    fn file_value(&self, root: &Path, path: &str) -> Result<Value> {
        self.safe_parents(root, path)?;
        let absolute = root.join(path);
        let metadata = std::fs::symlink_metadata(&absolute)?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)?;
            // A dangling link is keyed as such, so the key changes once it resolves.
            let Ok(resolved) = absolute.canonicalize() else {
                return Ok(json!({"link":target, "dangling":true}));
            };
            let Ok(relative) = resolved.strip_prefix(&self.canonical_root) else {
                bail!("input symlink resolves outside the workspace: {path}");
            };
            if resolved.is_file() {
                return Ok(json!({"link":target, "content":digest_file(&resolved)?}));
            }
            // A directory link is keyed by the files below its target. Links found
            // there are keyed by their text only, which rules out cycles.
            let relative = paths::relative(Path::new(""), relative)?;
            if relative.is_empty() {
                bail!("input symlink resolves to the workspace root: {path}");
            }
            let mut contents = BTreeMap::new();
            for file in under(&self.files, &relative) {
                let absolute = self.canonical_root.join(file);
                let metadata = std::fs::symlink_metadata(&absolute)?;
                let value = if metadata.file_type().is_symlink() {
                    json!({"link": std::fs::read_link(&absolute)?})
                } else {
                    json!(self.digest(file, &absolute, &metadata)?)
                };
                contents.insert(file.strip_prefix(&relative).unwrap_or(file), value);
            }
            return Ok(json!({"link":target, "directory":contents}));
        }
        if !metadata.is_file() {
            bail!("input is not a regular file: {path}");
        }
        // Only the executable bit, which is all git records: other permission
        // bits follow the checkout's umask and would keep machines apart.
        let mode = crate::store::mode(&metadata) & 0o111 != 0;
        Ok(json!({"content":self.digest(path, &absolute, &metadata)?, "mode":mode}))
    }
}

/// Reads and parses a pnpm lockfile, `None` when there is none. One qk cannot
/// read is reported, and kept as unreadable so tasks key its whole content.
fn parse_lockfile(path: &Path) -> Option<Arc<Installed>> {
    let text = std::fs::read_to_string(path).ok()?;
    let lockfile = match Lockfile::parse(&text) {
        Ok(lockfile) => Some(lockfile),
        Err(error) => {
            qk_executor::status!("qk: pnpm-lock.yaml: keying tasks by the whole file ({error:#})");
            None
        }
    };
    Some(Arc::new(Installed {
        content: blake3::hash(text.as_bytes()).to_hex().to_string(),
        lockfile,
        digests: Mutex::default(),
    }))
}

fn load_digests(root: &Path) -> HashMap<String, (Stamp, String)> {
    #[derive(serde::Deserialize)]
    struct Saved {
        version: u32,
        entries: HashMap<String, (Stamp, String)>,
    }
    std::fs::read(digests_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
        .filter(|saved| saved.version == 1)
        .map(|saved| saved.entries)
        .unwrap_or_default()
}

/// Files equal to `prefix` or below it, using the sorted order of the set.
fn under<'a>(files: &'a BTreeSet<String>, prefix: &'a str) -> impl Iterator<Item = &'a String> {
    files
        .range::<str, _>((
            std::ops::Bound::Included(prefix),
            std::ops::Bound::Unbounded,
        ))
        .take_while(move |path| path.starts_with(prefix))
        .filter(move |path| path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
}

fn source_files(root: &Path) -> Result<BTreeSet<String>> {
    let lines =
        |output: std::io::Result<std::process::Output>| -> Option<Result<BTreeSet<String>>> {
            let output = output.ok().filter(|output| output.status.success())?;
            Some(
                output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| {
                        String::from_utf8(path.to_vec()).context("input paths must be UTF-8")
                    })
                    .collect(),
            )
        };
    // Tracked files deleted from the working tree are listed as cached; git
    // names them in a second listing, which is cheaper than checking each file.
    // Skip-worktree entries are listed whether or not they are on disk and never
    // as deleted, so those few are checked; `-v` tags them `S`.
    let (listed, deleted) = std::thread::scope(|scope| {
        let deleted = scope.spawn(|| paths::git(root, &["ls-files", "--deleted", "-z", "--", "."]));
        let listed = paths::git(
            root,
            &[
                "ls-files",
                "-v",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ],
        );
        (listed, deleted.join().expect("git listing thread panicked"))
    });
    let candidates = match (lines(listed), lines(deleted)) {
        (Some(listed), Some(deleted)) => {
            let deleted = deleted?;
            let mut files = BTreeSet::new();
            for entry in listed? {
                let Some((tag, path)) = entry.split_once(' ') else {
                    bail!("unexpected git ls-files entry {entry:?}");
                };
                let absent =
                    matches!(tag, "S" | "s") && std::fs::symlink_metadata(root.join(path)).is_err();
                if !absent && !deleted.contains(path) {
                    files.insert(path.to_owned());
                }
            }
            files
        }
        _ => {
            let mut files = BTreeSet::new();
            for entry in ignore::WalkBuilder::new(root)
                .hidden(false)
                .parents(false)
                .git_global(false)
                .require_git(false)
                .follow_links(false)
                .filter_entry(|entry| {
                    !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | ".qk" | "node_modules")
                    )
                })
                .build()
            {
                let entry = entry?;
                if entry.file_type().is_some_and(|kind| !kind.is_dir()) {
                    files.insert(paths::relative(root, entry.path())?);
                }
            }
            files
        }
    };
    Ok(candidates
        .into_iter()
        .filter(|path| {
            !path
                .split('/')
                .any(|part| matches!(part, ".git" | ".qk" | "node_modules"))
        })
        .collect())
}

fn json_path<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.')
        .try_fold(value, |value, part| value.get(part))
}

fn set_json_path(target: &mut Value, path: &str, value: Value) {
    let mut parts = path.split('.').peekable();
    let mut current = target;
    while let Some(part) = parts.next() {
        let Value::Object(object) = current else {
            return;
        };
        if parts.peek().is_none() {
            object.insert(part.to_owned(), value);
            return;
        }
        current = object.entry(part).or_insert_with(|| json!({}));
    }
}

fn remove_json_path(target: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((parent, last)) => (Some(parent), last),
        None => (None, path),
    };
    let parent = match parent {
        Some(parent) => parent
            .split('.')
            .try_fold(&mut *target, |value, part| value.get_mut(part)),
        None => Some(target),
    };
    if let Some(Value::Object(object)) = parent {
        object.remove(last);
    }
}

struct Resolver<'a> {
    workspace: &'a Workspace,
    snapshot: &'a Snapshot,
    selected: BTreeSet<String>,
    values: BTreeMap<String, Value>,
    named_stack: Vec<(String, String)>,
    /// Dependency named inputs already expanded for `^` inputs.
    expanded: BTreeSet<(String, String)>,
    external: BTreeSet<String>,
    /// `None` resolves which inputs a task has without evaluating env or
    /// runtime inputs.
    prepared: Option<&'a PreparedTask>,
    cancelled: &'a AtomicBool,
}

impl Resolver<'_> {
    fn input(&mut self, project: &str, input: &Value) -> Result<()> {
        match input {
            // As in Nx, `^name` is `name` of every project the project depends
            // on, directly or not.
            Value::String(name) if name.starts_with('^') => {
                self.dependencies_named(project, &name[1..])?;
            }
            // The object spellings of named inputs, as Nx reads them.
            Value::Object(object)
                if object.contains_key("input") && !object.contains_key("fileset") =>
            {
                let name = object["input"]
                    .as_str()
                    .context("input must name a named input")?;
                let projects = object.get("projects");
                let dependencies = object.get("dependencies") == Some(&json!(true))
                    || projects == Some(&json!("dependencies"));
                let listed = projects.filter(|projects| {
                    **projects != json!("self") && **projects != json!("dependencies")
                });
                if (dependencies || listed.is_some()) && !self.named_stack.is_empty() {
                    bail!("named inputs can only refer to named inputs of their own project");
                }
                if dependencies {
                    self.dependencies_named(project, name)?;
                } else if let Some(listed) = listed {
                    let patterns: Vec<String> = match listed {
                        Value::String(pattern) => vec![pattern.clone()],
                        other => serde_json::from_value(other.clone())
                            .context("input projects must be a string or an array")?,
                    };
                    for selected in
                        qk_graph::select_projects(&self.workspace.projects, &patterns, &[])?
                    {
                        if self.expanded.insert((selected.clone(), name.to_owned())) {
                            self.named(&selected, name)?;
                        }
                    }
                } else {
                    self.named(project, name)?;
                }
            }
            Value::Object(object)
                if object.contains_key("fileset")
                    && object.get("dependencies") == Some(&json!(true)) =>
            {
                if !self.named_stack.is_empty() {
                    bail!("named inputs can only refer to named inputs of their own project");
                }
                let pattern = object["fileset"]
                    .as_str()
                    .context("fileset input must be a glob")?
                    .to_owned();
                for dependency in self.dependency_closure(project) {
                    self.fileset(&dependency, &pattern)?;
                }
            }
            Value::Object(object)
                if object.len() == 1 && object.contains_key("workingDirectory") =>
            {
                let mode = object["workingDirectory"].as_str().unwrap_or_default();
                let directory = std::env::current_dir()?;
                let value = match mode {
                    "absolute" => directory.to_string_lossy().into_owned(),
                    "relative" => {
                        let root = self.workspace.root.canonicalize()?;
                        let directory = directory.canonicalize()?;
                        paths::relative(&root, &directory)
                            .unwrap_or_else(|_| directory.to_string_lossy().into_owned())
                    }
                    _ => bail!("workingDirectory must be \"relative\" or \"absolute\""),
                };
                self.values
                    .insert(format!("workingDirectory:{mode}"), json!(value));
            }
            Value::Object(object) if object.contains_key("json") => {
                self.json(project, object)?;
            }
            Value::String(name)
                if !name.contains("{projectRoot}") && !name.contains("{workspaceRoot}") =>
            {
                self.named(project, name)?
            }
            Value::Object(object) if object.len() == 1 && object.contains_key("fileset") => {
                let pattern = object["fileset"]
                    .as_str()
                    .context("fileset input must be a glob")?;
                self.fileset(project, pattern)?;
            }
            Value::String(pattern) => self.fileset(project, pattern)?,
            Value::Object(object) if object.len() == 1 && object.contains_key("env") => {
                let name = object["env"]
                    .as_str()
                    .context("env input must name a variable")?;
                let Some(prepared) = self.prepared else {
                    self.values.insert(format!("env:{name}"), Value::Null);
                    return Ok(());
                };
                let value = prepared
                    .env
                    .get(std::ffi::OsStr::new(name))
                    .map(|value| {
                        value
                            .to_str()
                            .map(str::to_owned)
                            .context("declared env input must be UTF-8")
                    })
                    .transpose()?;
                self.values.insert(format!("env:{name}"), json!(value));
            }
            Value::Object(object) if object.len() == 1 && object.contains_key("runtime") => {
                let command = object["runtime"]
                    .as_str()
                    .context("runtime input must be a command")?;
                let Some(prepared) = self.prepared else {
                    self.values
                        .insert(format!("runtime:{command}"), Value::Null);
                    return Ok(());
                };
                let memo = serde_json::to_string(&(
                    command,
                    prepared
                        .env
                        .iter()
                        .map(|(name, value)| (name.to_string_lossy(), value.to_string_lossy()))
                        .collect::<Vec<_>>(),
                ))?;
                if let Some(value) = self.snapshot.runtime.lock().unwrap().get(&memo) {
                    self.values
                        .insert(format!("runtime:{command}"), value.clone());
                    return Ok(());
                }
                let mut prepared = prepared.clone();
                prepared.commands = vec![command.into()];
                prepared.cwd = self.workspace.root.clone();
                prepared.parallel = false;
                let log = tempfile::NamedTempFile::new()?;
                let capture = Capture::new(Some(log.as_file().try_clone()?), Display::Hidden);
                if execute_captured(&prepared, self.cancelled, Some(&capture))? != Outcome::Success
                {
                    bail!("runtime input did not succeed");
                }
                let mut stdout = blake3::Hasher::new();
                let mut stderr = blake3::Hasher::new();
                read_capture(File::open(log.path())?, |error, bytes| {
                    if error {
                        stderr.update(bytes);
                    } else {
                        stdout.update(bytes);
                    }
                    Ok(())
                })?;
                let value = json!([
                    stdout.finalize().to_hex().to_string(),
                    stderr.finalize().to_hex().to_string()
                ]);
                self.snapshot
                    .runtime
                    .lock()
                    .unwrap()
                    .insert(memo, value.clone());
                self.values.insert(format!("runtime:{command}"), value);
            }
            Value::Object(object) if object.contains_key("dependentTasksOutputFiles") => {
                if object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "dependentTasksOutputFiles" | "transitive"))
                {
                    bail!("unsupported output input field");
                }
                let pattern = object["dependentTasksOutputFiles"]
                    .as_str()
                    .context("dependentTasksOutputFiles must be a glob")?;
                self.snapshot.pattern(pattern, false)?;
                let transitive = object
                    .get("transitive")
                    .map(|value| value.as_bool().context("transitive must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                // Resolved before execution too, so inspection can display the
                // selection without reading generated files. dependency_keys
                // hashes these files after the dependencies finish.
                let value = self
                    .values
                    .entry(format!("dependentTasksOutputFiles:{pattern}"))
                    .or_insert(json!(false));
                if transitive {
                    *value = json!(true);
                }
            }
            Value::Object(object)
                if object.len() == 1 && object.contains_key("externalDependencies") =>
            {
                let names: Vec<String> =
                    serde_json::from_value(object["externalDependencies"].clone())?;
                self.external.extend(names);
            }
            _ => bail!("unsupported task input declaration"),
        }
        Ok(())
    }

    fn fileset(&mut self, project: &str, pattern: &str) -> Result<()> {
        let (exclude, pattern) = pattern
            .strip_prefix('!')
            .map(|pattern| (true, pattern))
            .unwrap_or((false, pattern));
        let pattern = paths::expand(self.workspace, project, pattern)?;
        let matcher = self.snapshot.pattern(&pattern, exclude)?;
        // Only files below the pattern's literal directory prefix can match.
        let prefix = pattern
            .split('/')
            .take_while(|part| is_literal(part))
            .collect::<Vec<_>>()
            .join("/");
        let candidates: Box<dyn Iterator<Item = &String>> = if prefix.is_empty() {
            Box::new(self.snapshot.files.iter())
        } else {
            Box::new(under(&self.snapshot.files, &prefix))
        };
        for file in candidates.filter(|file| matcher.is_match(file)) {
            if exclude {
                self.selected.remove(file);
            } else {
                self.selected.insert(file.clone());
            }
        }
        Ok(())
    }

    /// Every project `project` depends on, directly or not.
    fn dependency_closure(&self, project: &str) -> BTreeSet<String> {
        let mut closure = BTreeSet::new();
        let mut pending = vec![project.to_owned()];
        while let Some(current) = pending.pop() {
            for edge in &self.snapshot.projects.dependencies[&current] {
                if edge.target != project && closure.insert(edge.target.clone()) {
                    pending.push(edge.target.clone());
                }
            }
        }
        closure
    }

    /// As in Nx, `^name` is `name` of every project the project depends on.
    fn dependencies_named(&mut self, project: &str, name: &str) -> Result<()> {
        for dependency in self.dependency_closure(project) {
            if self.expanded.insert((dependency.clone(), name.to_owned())) {
                self.named(&dependency, name)?;
            }
        }
        Ok(())
    }

    /// A JSON input: the fields of one JSON file, rather than all of it.
    /// `fields` keeps only the named dotted paths; `excludeFields` removes
    /// them.
    fn json(&mut self, project: &str, object: &serde_json::Map<String, Value>) -> Result<()> {
        for key in object.keys() {
            if !matches!(key.as_str(), "json" | "fields" | "excludeFields") {
                bail!("unsupported json input field {key:?}");
            }
        }
        let path = object["json"]
            .as_str()
            .context("json input must name a file")?;
        let path = paths::expand(self.workspace, project, path)?;
        paths::validate_path(&path)?;
        let list = |name: &str| -> Result<Option<Vec<String>>> {
            object
                .get(name)
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .with_context(|| format!("json input {name} must be an array of strings"))
                })
                .transpose()
        };
        let fields = list("fields")?;
        let exclude = list("excludeFields")?.unwrap_or_default();
        let value = match std::fs::read_to_string(self.workspace.root.join(&path)) {
            Ok(text) => {
                let mut value: Value =
                    jsonc_parser::parse_to_serde_value(&text, &Default::default())
                        .with_context(|| format!("cannot parse json input {path}"))?;
                if let Some(fields) = &fields {
                    let mut kept = json!({});
                    for field in fields {
                        if let Some(found) = json_path(&value, field) {
                            set_json_path(&mut kept, field, found.clone());
                        }
                    }
                    value = kept;
                }
                for field in &exclude {
                    remove_json_path(&mut value, field);
                }
                value
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error.into()),
        };
        self.values.insert(format!("json:{path}"), value);
        Ok(())
    }

    fn named(&mut self, project: &str, name: &str) -> Result<()> {
        let identity = (project.to_owned(), name.to_owned());
        if self.named_stack.contains(&identity) {
            bail!("named input cycle at {project}:{name}");
        }
        self.named_stack.push(identity);
        let inputs = self.workspace.projects[project]
            .named_inputs
            .get(name)
            .cloned()
            .or_else(|| (name == "default").then(|| vec![json!("{projectRoot}/**/*")]))
            .with_context(|| format!("unknown named input {name:?}"))?;
        for input in &inputs {
            self.input(project, input)?;
        }
        self.named_stack.pop();
        Ok(())
    }
}

/// What a task's key is computed from, as JSON: kept by the history so a
/// changed key can be explained.
/// What a task's key depends on, before any file is read.
pub struct Resolved {
    /// Workspace-relative files whose content is part of the key.
    pub files: BTreeSet<String>,
    /// Env, runtime and other named values; env and runtime are evaluated only
    /// with a prepared task.
    pub values: BTreeMap<String, Value>,
    /// With a readable pnpm lockfile: the importers whose installs count, and
    /// packages named by `externalDependencies`.
    pub lockfile: Option<(BTreeSet<String>, BTreeSet<String>)>,
    /// `pnpm-workspace.yaml` counts without its resolution keys, which reach
    /// tasks through the lockfile instead.
    pub workspace_file: bool,
    /// Explicit external dependency names, even without a readable lockfile.
    pub external: BTreeSet<String>,
}

pub fn resolve(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: Option<&PreparedTask>,
    cancelled: &AtomicBool,
) -> Result<Resolved> {
    let mut resolver = Resolver {
        workspace,
        snapshot,
        selected: BTreeSet::new(),
        values: BTreeMap::new(),
        named_stack: Vec::new(),
        expanded: BTreeSet::new(),
        external: BTreeSet::new(),
        prepared,
        cancelled,
    };
    let default = vec![json!("default"), json!("^default")];
    for input in task.definition.inputs.as_ref().unwrap_or(&default) {
        resolver.input(&task.project, input)?;
    }
    let readable = snapshot
        .installed(&workspace.root)?
        .is_some_and(|installed| installed.lockfile.is_some());
    // Always include workspace resolution and configuration. Dotenv files are
    // not keyed, as in Nx: they hold per-machine values and credentials, and a
    // task's `env` inputs key the variables it declares.
    // A pnpm lockfile qk can read is keyed by what the task's projects install instead.
    // The root tsconfig, as Nx hashes it into every task.
    let tsconfig = ["tsconfig.base.json", "tsconfig.json"]
        .into_iter()
        .find(|name| workspace.root.join(name).is_file());
    for path in tsconfig.into_iter().chain([
        "nx.json",
        qk_config::LOCAL_WORKSPACE,
        "package.json",
        "pnpm-workspace.yaml",
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
    ]) {
        if workspace.root.join(path).is_file() && !(path == "pnpm-lock.yaml" && readable) {
            resolver.selected.insert(path.into());
        }
    }
    if let Some(extended) = &workspace.extended {
        resolver.selected.insert(extended.clone());
    }
    // Package scripts and dependency declarations remain inputs even when filesets exclude them.
    let mut packages = BTreeSet::from([task.project.clone()]);
    let mut pending = packages.clone();
    while let Some(project) = pending.pop_first() {
        for edge in &snapshot.projects.dependencies[&project] {
            if packages.insert(edge.target.clone()) {
                pending.insert(edge.target.clone());
            }
        }
    }
    for project in &packages {
        for name in ["project.json", qk_config::LOCAL_OVERRIDES, "package.json"] {
            let path = Path::new(&workspace.projects[project].root).join(name);
            let path = path.strip_prefix(".").unwrap_or(&path).to_owned();
            if workspace.root.join(&path).is_file() {
                resolver.selected.insert(
                    path.to_str()
                        .context("project path must be UTF-8")?
                        .replace('\\', "/"),
                );
            }
        }
    }
    // However it was selected, the workspace file's resolution keys reach the
    // task through the lockfile when qk can read it.
    let workspace_file = readable && resolver.selected.remove("pnpm-workspace.yaml");
    let lockfile = readable.then(|| {
        // The root importer's packages resolve from every package in the
        // workspace, so they count for every task.
        let importers = std::iter::once(".".to_owned())
            .chain(
                packages
                    .iter()
                    .map(|project| workspace.projects[project].root.clone()),
            )
            .collect();
        (importers, resolver.external.clone())
    });
    Ok(Resolved {
        files: resolver.selected,
        values: resolver.values,
        lockfile,
        workspace_file,
        external: resolver.external,
    })
}

/// What a task's key is computed from, as JSON: kept by the history so a
/// changed key can be explained.
pub fn inputs(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: &PreparedTask,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<Value> {
    let Resolved {
        files,
        mut values,
        lockfile,
        workspace_file,
        ..
    } = resolve(snapshot, workspace, task, Some(prepared), cancelled)?;
    let installed = snapshot.installed(&workspace.root)?;
    if let (Some((importers, external)), Some(installed)) = (lockfile, &installed)
        && let Some(lockfile) = &installed.lockfile
    {
        let importers: BTreeMap<_, _> = importers
            .iter()
            .filter(|importer| lockfile.has_importer(importer))
            .map(|importer| {
                let digest = installed.digest(&format!("importer:{importer}"), || {
                    lockfile.installed(importer).unwrap_or_default()
                });
                (importer.clone(), digest)
            })
            .collect();
        let external: BTreeMap<_, _> = external
            .iter()
            .map(|name| {
                let digest =
                    installed.digest(&format!("package:{name}"), || lockfile.package(name));
                (name.clone(), digest)
            })
            .collect();
        values.insert(
            "lockfile".into(),
            json!({
                "global": installed.digest("global", || BTreeSet::from([lockfile.global().to_string()])),
                "importers": importers,
                "external": external,
            }),
        );
    }
    let mut files = files
        .iter()
        .map(|path| {
            let value = snapshot
                .file_value(&workspace.root, path)
                .with_context(|| format!("cannot read input {path}"))?;
            Ok((path.clone(), value))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if workspace_file {
        match workspace_without_resolution(&workspace.root) {
            Some(value) => {
                values.insert("pnpm-workspace".into(), value);
            }
            None => {
                files.insert(
                    "pnpm-workspace.yaml".into(),
                    snapshot.file_value(&workspace.root, "pnpm-workspace.yaml")?,
                );
            }
        }
    }
    // Warm state configuration never changes a result.
    let mut definition = task.definition.clone();
    definition.extra.remove("qk:warm");
    definition.extra.remove("qk:threads");
    let hash = json!({
        "schema":"qk-local-v1", "qk":env!("CARGO_PKG_VERSION"),
        "platform":[std::env::consts::OS, std::env::consts::ARCH], "workspace":snapshot.workspace_prefix,
        "id":task.id, "args":task.args, "definition":definition, "packageManager":workspace.package_manager,
        "files":files, "values":values, "dependencies":dependencies,
    });
    Ok(hash)
}

/// `pnpm-workspace.yaml` without the keys that configure resolution, or `None`
/// when it cannot be read as YAML.
pub fn workspace_without_resolution(root: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(root.join("pnpm-workspace.yaml")).ok()?;
    without_resolution(&text)
}

/// A `pnpm-workspace.yaml` text without its resolution keys.
pub fn without_resolution(text: &str) -> Option<Value> {
    let mut value: Value = serde_yaml_ng::from_str(text).ok()?;
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| !qk_lockfile::RESOLUTION_KEYS.contains(&key.as_str()));
    }
    Some(value)
}

/// The key for a task's inputs.
pub fn key(inputs: &Value) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(inputs)?)
        .to_hex()
        .to_string())
}

pub fn fingerprint(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: &PreparedTask,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<String> {
    key(&inputs(
        snapshot,
        workspace,
        task,
        prepared,
        dependencies,
        cancelled,
    )?)
}

/// Use selected dependency artifacts when output inputs are declared; otherwise
/// preserve the normal dependency fingerprints. All dependencies must have
/// succeeded and be fingerprintable before this selection is applied.
pub(crate) fn dependency_keys(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<BTreeMap<String, String>> {
    let resolved = resolve(snapshot, workspace, task, None, cancelled)?;
    let selections: Vec<_> = resolved
        .values
        .iter()
        .filter_map(|(name, transitive)| {
            name.strip_prefix("dependentTasksOutputFiles:")
                .map(|pattern| (pattern, transitive == &json!(true)))
        })
        .collect();
    if selections.is_empty() {
        return Ok(dependencies.clone());
    }
    let mut keys = BTreeMap::new();
    for (pattern, transitive) in selections {
        let matcher = snapshot.pattern(pattern, false)?;
        let mut selected = task.dependencies.clone();
        if transitive {
            let mut pending = selected.clone();
            while let Some(id) = pending.pop_first() {
                for dependency in &graph.tasks[&id].dependencies {
                    if selected.insert(dependency.clone()) {
                        pending.insert(dependency.clone());
                    }
                }
            }
        }
        for id in selected {
            let outputs = Outputs::new(workspace, &graph.tasks[&id])?;
            let mut files = BTreeMap::new();
            for path in outputs.paths(&workspace.root)? {
                if matcher.is_match(&path) {
                    files.insert(path.clone(), snapshot.file_value(&workspace.root, &path)?);
                }
            }
            keys.insert(format!("{id}:{pattern}"), key(&json!(files))?);
        }
    }
    Ok(keys)
}
