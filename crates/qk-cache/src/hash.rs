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

/// Metadata that changes whenever a file's content can have changed.
#[derive(Clone, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    unix: (i64, i64, u64, u32),
}

impl Stamp {
    fn new(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            unix: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                    metadata.ino(),
                    metadata.mode(),
                )
            },
        }
    }

    /// A file written within the timestamp resolution window could change again
    /// without changing its stamp, so recent files are always re-read.
    fn settled(&self) -> bool {
        self.modified
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > Duration::from_secs(2))
    }
}

/// Workspace state shared by every fingerprint in one run. The candidate file list
/// is taken once, like Nx's file map; file contents are still re-read whenever
/// their metadata changes, so edits during the run are detected.
pub struct Snapshot {
    files: BTreeSet<String>,
    projects: ProjectGraph,
    canonical_root: PathBuf,
    workspace_prefix: Option<PathBuf>,
    patterns: Mutex<HashMap<(String, bool), Arc<Pattern>>>,
    digests: Mutex<HashMap<String, (Stamp, String)>>,
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
        let mut files = source_files(&workspace.root)?;
        if let Ok(cache_relative) = cache_path.strip_prefix(&workspace.root) {
            files.retain(|path| !Path::new(path).starts_with(cache_relative));
        }
        // Generated artifacts must not make the next invocation invalidate itself.
        // Another task's unsupported outputs stay candidates: that only costs misses.
        let mut generated = BTreeSet::new();
        for outputs in graph
            .tasks
            .values()
            .filter_map(|task| Outputs::new(workspace, task).ok())
        {
            for anchor in outputs.anchors() {
                generated.extend(
                    under(&files, anchor)
                        .filter(|path| outputs.matches(path))
                        .cloned(),
                );
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
            digests: Mutex::default(),
            runtime: Mutex::default(),
            lockfile: Mutex::default(),
        })
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
        let text = std::fs::read_to_string(&absolute)?;
        let lockfile = match Lockfile::parse(&text) {
            Ok(lockfile) => Some(lockfile),
            Err(error) => {
                qk_executor::status!(
                    "qk: pnpm-lock.yaml: keying tasks by the whole file ({error:#})"
                );
                None
            }
        };
        let installed = Arc::new(Installed {
            content: blake3::hash(text.as_bytes()).to_hex().to_string(),
            lockfile,
            digests: Mutex::default(),
        });
        *current = Some(installed.clone());
        Ok(Some(installed))
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
        }
        Ok(digest)
    }

    fn file_value(&self, root: &Path, path: &str) -> Result<Value> {
        paths::safe_parents(root, path)?;
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
        let mode = crate::store::mode(&metadata);
        Ok(json!({"content":self.digest(path, &absolute, &metadata)?, "mode":mode}))
    }
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
    let git = paths::git(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    );
    let candidates = match git {
        Ok(output) if output.status.success() => output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).context("input paths must be UTF-8"))
            .collect::<Result<BTreeSet<_>>>()?,
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
                && std::fs::symlink_metadata(root.join(path)).is_ok()
        })
        .collect())
}

struct Resolver<'a> {
    workspace: &'a Workspace,
    snapshot: &'a Snapshot,
    selected: BTreeSet<String>,
    values: BTreeMap<String, Value>,
    named_stack: Vec<(String, String)>,
    external: BTreeSet<String>,
    /// `None` resolves which inputs a task has without evaluating env or
    /// runtime inputs.
    prepared: Option<&'a PreparedTask>,
    cancelled: &'a AtomicBool,
}

impl Resolver<'_> {
    fn input(&mut self, project: &str, input: &Value) -> Result<()> {
        match input {
            Value::String(name) if name.starts_with('^') => {
                for dependency in self.snapshot.projects.dependencies[project]
                    .iter()
                    .map(|edge| edge.target.clone())
                    .collect::<Vec<_>>()
                {
                    self.named(&dependency, &name[1..])?;
                }
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
                // The key already contains every dependency's fingerprint, which covers
                // all of its declared outputs and, through its own key, those of its
                // dependencies. Hashing matching output files again adds nothing.
                self.values.insert(
                    format!("dependentTasksOutputFiles:{pattern}"),
                    json!(transitive),
                );
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
    // Always include workspace resolution/configuration, including ignored dotenv files.
    // A pnpm lockfile qk can read is keyed by what the task's projects install instead.
    for path in [
        "nx.json",
        "package.json",
        "pnpm-workspace.yaml",
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
        ".env",
        ".env.local",
    ] {
        if workspace.root.join(path).is_file() && !(path == "pnpm-lock.yaml" && readable) {
            resolver.selected.insert(path.into());
        }
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
        for name in ["project.json", "package.json"] {
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
        .map(|path| Ok((path.clone(), snapshot.file_value(&workspace.root, path)?)))
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
    let hash = json!({
        "schema":"qk-local-v1", "qk":env!("CARGO_PKG_VERSION"),
        "platform":[std::env::consts::OS, std::env::consts::ARCH], "workspace":snapshot.workspace_prefix,
        "id":task.id, "args":task.args, "definition":task.definition, "packageManager":workspace.package_manager,
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
