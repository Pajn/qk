use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_executor::{Capture, Outcome, PreparedTask, execute_captured, read_capture};
use qk_graph::ProjectGraph;
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

use crate::paths::{self, Outputs};

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

pub fn file_value(root: &Path, path: &str) -> Result<Value> {
    paths::safe_parents(root, path)?;
    let absolute = root.join(path);
    let metadata = std::fs::symlink_metadata(&absolute)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&absolute)?;
        let resolved = absolute.canonicalize().context("dangling input symlink")?;
        if !resolved.starts_with(root.canonicalize()?) || !resolved.is_file() {
            bail!("input symlink must resolve to a file inside the workspace");
        }
        return Ok(json!({"link":target, "content":digest_file(&resolved)?}));
    }
    if !metadata.is_file() {
        bail!("input is not a regular file: {path}");
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = u32::from(metadata.permissions().readonly());
    Ok(json!({"content":digest_file(&absolute)?, "mode":mode}))
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
    graph: &'a TaskGraph,
    projects: ProjectGraph,
    task: &'a Task,
    files: BTreeSet<String>,
    selected: BTreeSet<String>,
    values: BTreeMap<String, Value>,
    named_stack: Vec<(String, String)>,
    prepared: &'a PreparedTask,
    cancelled: &'a AtomicBool,
}

impl Resolver<'_> {
    fn input(&mut self, project: &str, input: &Value) -> Result<()> {
        match input {
            Value::String(name) if name.starts_with('^') => {
                for dependency in self.projects.dependencies[project]
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
                let value = self
                    .prepared
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
                let mut prepared = self.prepared.clone();
                prepared.commands = vec![command.into()];
                prepared.cwd = self.workspace.root.clone();
                prepared.parallel = false;
                let log = tempfile::NamedTempFile::new()?;
                let capture = Capture::new(log.as_file().try_clone()?, false);
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
                self.values.insert(
                    format!("runtime:{command}"),
                    json!([
                        stdout.finalize().to_hex().to_string(),
                        stderr.finalize().to_hex().to_string()
                    ]),
                );
            }
            Value::Object(object) if object.contains_key("dependentTasksOutputFiles") => {
                if object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "dependentTasksOutputFiles" | "transitive"))
                {
                    bail!("unsupported output input field");
                }
                let matcher = paths::matcher(
                    object["dependentTasksOutputFiles"]
                        .as_str()
                        .context("dependentTasksOutputFiles must be a glob")?,
                )?;
                let transitive = object
                    .get("transitive")
                    .map(|value| value.as_bool().context("transitive must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                let mut dependencies = self.task.dependencies.clone();
                let mut visited = BTreeSet::new();
                while let Some(id) = dependencies.pop_first() {
                    if !visited.insert(id.clone()) {
                        continue;
                    }
                    let task = &self.graph.tasks[&id];
                    if transitive {
                        dependencies.extend(task.dependencies.iter().cloned());
                    }
                    for file in Outputs::new(self.workspace, task)?.paths(&self.workspace.root)? {
                        if matcher.is_match(&file) && !self.workspace.root.join(&file).is_dir() {
                            self.values.insert(
                                format!("output:{file}"),
                                file_value(&self.workspace.root, &file)?,
                            );
                        }
                    }
                }
            }
            Value::Object(object)
                if object.len() == 1 && object.contains_key("externalDependencies") =>
            {
                let _: Vec<String> =
                    serde_json::from_value(object["externalDependencies"].clone())?;
                // All root lockfiles are hashed conservatively below.
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
        let matcher = paths::matcher(&pattern)?;
        for file in &self.files {
            if matcher.is_match(file) {
                if exclude {
                    self.selected.remove(file);
                } else {
                    self.selected.insert(file.clone());
                }
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

pub fn fingerprint(
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    prepared: &PreparedTask,
    dependencies: &BTreeMap<String, String>,
    cache_path: &Path,
    cancelled: &AtomicBool,
) -> Result<String> {
    let mut files = source_files(&workspace.root)?;
    if let Ok(cache_relative) = cache_path.strip_prefix(&workspace.root) {
        files.retain(|path| !Path::new(path).starts_with(cache_relative));
    }
    // Generated artifacts must not make the next invocation invalidate itself.
    for task in graph.tasks.values() {
        let outputs = Outputs::new(workspace, task)?;
        files.retain(|path| !outputs.matches(path));
    }
    let projects = ProjectGraph::build(workspace)?;
    let mut resolver = Resolver {
        workspace,
        graph,
        projects,
        task,
        files,
        selected: BTreeSet::new(),
        values: BTreeMap::new(),
        named_stack: Vec::new(),
        prepared,
        cancelled,
    };
    let default = vec![json!("default"), json!("^default")];
    for input in task.definition.inputs.as_ref().unwrap_or(&default) {
        resolver.input(&task.project, input)?;
    }
    // Always include workspace resolution/configuration, including ignored dotenv files.
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
        if workspace.root.join(path).is_file() {
            resolver.selected.insert(path.into());
        }
    }
    // Package scripts and dependency declarations remain inputs even when filesets exclude them.
    let mut packages = BTreeSet::from([task.project.clone()]);
    let mut pending = packages.clone();
    while let Some(project) = pending.pop_first() {
        for edge in &resolver.projects.dependencies[&project] {
            if packages.insert(edge.target.clone()) {
                pending.insert(edge.target.clone());
            }
        }
    }
    for project in packages {
        for name in ["project.json", "package.json"] {
            let path = Path::new(&workspace.projects[&project].root).join(name);
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
    let files = resolver
        .selected
        .iter()
        .map(|path| Ok((path.clone(), file_value(&workspace.root, path)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let workspace_prefix = paths::git_path(&workspace.root, "--show-toplevel")
        .and_then(|root| root.canonicalize().ok())
        .and_then(|root| {
            let workspace = workspace.root.canonicalize().ok()?;
            workspace.strip_prefix(root).ok().map(PathBuf::from)
        });
    let hash = json!({
        "schema":"qk-local-v1", "qk":env!("CARGO_PKG_VERSION"),
        "platform":[std::env::consts::OS, std::env::consts::ARCH], "workspace":workspace_prefix,
        "id":task.id, "args":task.args, "definition":task.definition, "packageManager":workspace.package_manager,
        "files":files, "values":resolver.values, "dependencies":dependencies,
    });
    Ok(blake3::hash(&serde_json::to_vec(&hash)?)
        .to_hex()
        .to_string())
}
