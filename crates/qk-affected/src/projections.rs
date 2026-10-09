//! Opt-in projections replace declared source changes with revision snapshot changes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use command_group::{CommandGroup, GroupChild};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use qk_config::Workspace;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{Changes, Options, subprocess::workspace_command};

const MAX_OUTPUT: u64 = 1024 * 1024;

/// Which fallback conditions stop selection instead of retaining original changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionFallbackPolicy {
    All,
    Adapter,
}

/// Whether the adapter failed or the comparison needs conservative selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectionFallbackKind {
    Adapter,
    Comparison,
}

#[derive(Debug)]
struct AdapterFailure;
impl std::fmt::Display for AdapterFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("projection adapter failed")
    }
}
impl std::error::Error for AdapterFailure {}

/// What a projection replaced, or why the original changes were retained.
#[derive(Debug, Serialize)]
pub struct ProjectionReport {
    pub name: String,
    pub status: String,
    pub sources: Vec<String>,
    pub artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(rename = "fallbackKind", skip_serializing_if = "Option::is_none")]
    pub fallback_kind: Option<ProjectionFallbackKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    #[serde(default)]
    projections: Vec<Projection>,
    /// `true` or `{"default": name}`: whether tasks are narrowed by what their
    /// anchors and cases import, and the named config tasks without their own
    /// `qk:reachability` use.
    #[serde(default)]
    reachability: Option<serde_json::Value>,
}

/// A profile's reachability.
pub(super) struct Reach {
    /// The config in nx.json's `qk:reachability` for tasks that name none.
    pub default: Option<String>,
}

impl Profile {
    fn reach(&self) -> Result<Option<Reach>> {
        use serde_json::Value;
        Ok(match &self.reachability {
            None | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(Reach { default: None }),
            Some(Value::Object(object)) => {
                let mut default = None;
                for (key, value) in object {
                    match key.as_str() {
                        "default" => {
                            default = Some(
                                value
                                    .as_str()
                                    .context("reachability.default must name a config")?
                                    .to_owned(),
                            );
                        }
                        _ => bail!("unknown reachability field {key:?}"),
                    }
                }
                Some(Reach { default })
            }
            Some(_) => bail!("reachability must be true or an object with a default"),
        })
    }
}

/// The named profile, from `qk:affectedProfiles` or its compatibility alias.
fn profile(workspace: &Workspace, name: &str) -> Result<Profile> {
    let canonical = workspace.config.extra.get("qk:affectedProfiles");
    let legacy = workspace.config.extra.get("affectedProfiles");
    ensure!(
        canonical.is_none() || legacy.is_none(),
        "configure only qk:affectedProfiles; affectedProfiles is a compatibility alias and cannot be used alongside it"
    );
    let value = canonical
        .or(legacy)
        .and_then(|profiles| profiles.get(name))
        .with_context(|| format!("unknown affected profile {name:?}"))?;
    let profile: Profile = serde_json::from_value(value.clone())
        .with_context(|| format!("invalid affected profile {name:?}"))?;
    let reach = profile
        .reach()
        .with_context(|| format!("invalid affected profile {name:?}"))?;
    ensure!(
        !profile.projections.is_empty() || reach.is_some(),
        "affected profile must contain projections or enable reachability"
    );
    Ok(profile)
}

/// How the named profile narrows selection by import reachability, if it does.
pub(super) fn reachability(workspace: &Workspace, name: &str) -> Result<Option<Reach>> {
    profile(workspace, name)?.reach()
}

/// Whether the named profile replaces changes through projections.
pub(super) fn has_projections(workspace: &Workspace, name: &str) -> Result<bool> {
    Ok(!profile(workspace, name)?.projections.is_empty())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Projection {
    name: String,
    command: Vec<String>,
    sources: Vec<String>,
    outputs: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    60
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    #[serde(deserialize_with = "unique_artifacts")]
    artifacts: BTreeMap<String, String>,
}

fn unique_artifacts<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error> {
    struct Artifacts;
    impl<'de> serde::de::Visitor<'de> for Artifacts {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unique artifact paths mapped to fingerprints")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut artifacts = BTreeMap::new();
            while let Some((path, fingerprint)) = map.next_entry::<String, String>()? {
                if artifacts.insert(path.clone(), fingerprint).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate artifact {path:?}"
                    )));
                }
            }
            Ok(artifacts)
        }
    }
    deserializer.deserialize_map(Artifacts)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a> {
    version: u32,
    workspace_root: &'a Path,
    revision_root: &'a Path,
    adapter_root: &'a Path,
    revision: &'a str,
}

struct Rule {
    projection: Projection,
    sources: GlobSet,
    outputs: GlobSet,
}

pub(super) fn apply(
    workspace: &Workspace,
    options: &Options,
    changes: &Changes<'_>,
    name: &str,
) -> Result<(Vec<String>, Vec<ProjectionReport>)> {
    let profile = profile(workspace, name)?;
    let mut names = BTreeSet::new();
    let mut rules = Vec::new();
    for projection in profile.projections {
        ensure!(
            !projection.name.is_empty() && names.insert(projection.name.clone()),
            "projection names must be nonempty and unique"
        );
        ensure!(
            !projection.command.is_empty() && !projection.command[0].is_empty(),
            "projection {} needs a command argument array",
            projection.name
        );
        ensure!(
            (1..=3600).contains(&projection.timeout_seconds),
            "projection timeoutSeconds must be between 1 and 3600"
        );
        rules.push(Rule {
            sources: patterns(&projection.sources)?,
            outputs: patterns(&projection.outputs)?,
            projection,
        });
    }
    let covered: Vec<Vec<String>> = rules
        .iter()
        .map(|rule| {
            changes
                .files
                .iter()
                .filter(|file| rule.sources.is_match(file))
                .cloned()
                .collect()
        })
        .collect();
    let mut claimed = BTreeSet::new();
    for file in covered.iter().flatten() {
        ensure!(
            claimed.insert(file),
            "multiple projections cover changed source {file:?}"
        );
    }
    if claimed.is_empty() {
        return Ok((
            changes.files.clone(),
            rules
                .iter()
                .map(|rule| {
                    report(
                        rule,
                        "skipped",
                        Vec::new(),
                        Vec::new(),
                        Some("no declared sources changed".into()),
                    )
                })
                .collect(),
        ));
    }
    let result = (|| -> Result<Vec<Vec<String>>> {
        ensure!(
            !options.explicit_files
                && options.files.is_empty()
                && !options.uncommitted
                && !options.untracked,
            "projections require a committed base/head comparison, not an explicit or working-tree file list"
        );
        let head = changes.head.as_deref().context(
            "projections require --head (or NX_HEAD); working-tree comparisons remain conservative",
        )?;
        let base = changes.base.as_deref().context("missing base revision")?;
        let base = revision(&workspace.root, base)?;
        let head = revision(&workspace.root, head)?;
        let mut metadata = vec![
            "nx.json",
            "nx.local.json",
            "package.json",
            "pnpm-lock.yaml",
            "pnpm-workspace.yaml",
            "package-lock.json",
            "yarn.lock",
            "bun.lock",
            "bun.lockb",
        ];
        if let Some(path) = &workspace.extended {
            metadata.push(path);
        }
        let mut command = workspace_command("git");
        command
            .current_dir(&workspace.root)
            .args(["diff", "--quiet", &base, &head, "--"])
            .args(&metadata);
        let comparison = command
            .output()
            .context("cannot compare workspace metadata with git diff")?;
        match comparison.status.code() {
            Some(0) => {}
            Some(1) => anyhow::bail!(
                "workspace configuration or tool installation changed; projection remains conservative"
            ),
            _ => anyhow::bail!(
                "cannot compare workspace metadata: git diff failed ({}): {}",
                comparison.status,
                String::from_utf8_lossy(&comparison.stderr).trim()
            ),
        }
        let prefix = git(&workspace.root, &["rev-parse", "--show-prefix"])?;
        for path in metadata
            .iter()
            .filter(|path| !path.starts_with("node_modules/"))
        {
            let recorded = workspace_command("git")
                .current_dir(&workspace.root)
                .args(["show", &format!("{head}:{prefix}{path}")])
                .output()?;
            let recorded = recorded.status.success().then_some(recorded.stdout);
            // Local overrides configure the same selected profile for both snapshots.
            if *path == "nx.local.json"
                && recorded.is_none()
                && !workspace_command("git")
                    .current_dir(&workspace.root)
                    .args(["ls-files", "--error-unmatch", "--", path])
                    .output()?
                    .status
                    .success()
            {
                continue;
            }
            let current = std::fs::read(workspace.root.join(path)).ok();
            ensure!(
                current == recorded,
                "workspace metadata {path} differs from the head revision; projection remains conservative"
            );
        }
        ensure!(
            !claimed.iter().any(|path| protected(path)),
            "projections cannot replace workspace or project metadata"
        );
        let mut artifacts = Vec::new();
        let mut owners = BTreeSet::new();
        for (rule, sources) in rules.iter().zip(&covered) {
            if sources.is_empty() {
                artifacts.push(Vec::new());
                continue;
            }
            let timeout = Duration::from_secs(rule.projection.timeout_seconds);
            let before = Revision::new(&workspace.root, &base, timeout)?;
            let after = Revision::new(&workspace.root, &head, timeout)?;
            let adapter_before = Revision::new(&workspace.root, &head, timeout)?;
            let left = snapshot(rule, workspace, &before.root, &base, &adapter_before.root)
                .context(AdapterFailure)?;
            let adapter_after = Revision::new(&workspace.root, &head, timeout)?;
            let right = snapshot(rule, workspace, &after.root, &head, &adapter_after.root)
                .context(AdapterFailure)?;
            let changed = (|| -> Result<Vec<String>> {
                for path in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
                    ensure!(
                        owners.insert(path.clone()),
                        "multiple projections emit artifact {path:?}"
                    );
                    ensure!(
                        !claimed.contains(path),
                        "artifact {path:?} overlaps a replaced source"
                    );
                }
                Ok(left
                    .keys()
                    .chain(right.keys())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter(|path| left.get(*path) != right.get(*path))
                    .cloned()
                    .collect())
            })()
            .context(AdapterFailure)?;
            artifacts.push(changed);
        }
        Ok(artifacts)
    })();
    match result {
        Ok(artifacts) => {
            let mut files: BTreeSet<String> = changes
                .files
                .iter()
                .filter(|path| !claimed.contains(path))
                .cloned()
                .collect();
            let reports = rules
                .iter()
                .zip(covered)
                .zip(artifacts)
                .map(|((rule, sources), artifacts)| {
                    files.extend(artifacts.iter().cloned());
                    report(
                        rule,
                        if sources.is_empty() {
                            "skipped"
                        } else {
                            "applied"
                        },
                        sources,
                        artifacts,
                        None,
                    )
                })
                .collect();
            Ok((files.into_iter().collect(), reports))
        }
        Err(error) => {
            let kind = if error.is::<AdapterFailure>() {
                ProjectionFallbackKind::Adapter
            } else {
                ProjectionFallbackKind::Comparison
            };
            if options.fail_on_projection_fallback == Some(ProjectionFallbackPolicy::All)
                || (options.fail_on_projection_fallback == Some(ProjectionFallbackPolicy::Adapter)
                    && kind == ProjectionFallbackKind::Adapter)
            {
                return Err(error).with_context(|| format!("affected profile {name:?} fell back"));
            }
            Ok((
                changes.files.clone(),
                rules
                    .iter()
                    .zip(covered)
                    .map(|(rule, sources)| {
                        let mut report = report(
                            rule,
                            "fallback",
                            sources,
                            Vec::new(),
                            Some(format!("{error:#}")),
                        );
                        report.fallback_kind = Some(kind);
                        report
                    })
                    .collect(),
            ))
        }
    }
}

fn report(
    rule: &Rule,
    status: &str,
    sources: Vec<String>,
    artifacts: Vec<String>,
    detail: Option<String>,
) -> ProjectionReport {
    ProjectionReport {
        name: rule.projection.name.clone(),
        status: status.into(),
        sources,
        artifacts,
        detail,
        fallback_kind: None,
    }
}

fn patterns(patterns: &[String]) -> Result<GlobSet> {
    ensure!(
        !patterns.is_empty(),
        "projection sources and outputs must be nonempty glob arrays"
    );
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        ensure!(
            !pattern.starts_with(['/', '!'])
                && !pattern.contains(':')
                && !pattern.contains('\\')
                && !pattern
                    .split('/')
                    .any(|part| matches!(part, ".." | "." | "")),
            "projection patterns must be workspace-relative: {pattern:?}"
        );
        set.add(GlobBuilder::new(pattern).literal_separator(true).build()?);
    }
    Ok(set.build()?)
}

fn protected(path: &str) -> bool {
    matches!(
        path.rsplit('/').next().unwrap_or(path),
        "nx.json"
            | "nx.local.json"
            | "project.json"
            | "project.local.json"
            | "package.json"
            | "pnpm-lock.yaml"
            | "pnpm-workspace.yaml"
            | "package-lock.json"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
    )
}

fn revision(root: &Path, revision: &str) -> Result<String> {
    git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = workspace_command("git")
        .current_dir(root)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

struct Revision {
    root: PathBuf,
    checkout: PathBuf,
    repository: PathBuf,
    _temp: TempDir,
}

impl Revision {
    fn new(repository: &Path, revision: &str, timeout: Duration) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let checkout = temp.path().join("checkout");
        let hooks = temp.path().join("hooks");
        std::fs::create_dir(&hooks)?;
        // Register cleanup before starting Git: interrupted checkouts can still
        // leave a worktree entry, even when worktree add never succeeds.
        let result = Self {
            root: checkout.join(git(repository, &["rev-parse", "--show-prefix"])?),
            checkout,
            repository: repository.into(),
            _temp: temp,
        };
        let mut stderr = tempfile::tempfile()?;
        let mut command = workspace_command("git");
        command
            .current_dir(repository)
            .args([
                "-c",
                &format!("core.hooksPath={}", hooks.display()),
                "worktree",
                "add",
                "--detach",
                "--quiet",
                result
                    .checkout
                    .to_str()
                    .context("non-UTF-8 temporary directory")?,
                revision,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr.try_clone()?));
        let mut child = Process(
            command
                .group_spawn()
                .context("cannot start git worktree add")?,
        );
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.0.inner().try_wait()? {
                stderr.rewind()?;
                let mut diagnostic = String::new();
                stderr.take(MAX_OUTPUT).read_to_string(&mut diagnostic)?;
                ensure!(
                    status.success(),
                    "git worktree add failed ({status}): {}",
                    diagnostic.trim()
                );
                break;
            }
            ensure!(Instant::now() < deadline, "git worktree add timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(child);
        ensure!(
            result.root.is_dir(),
            "workspace is absent at revision {revision}"
        );
        Ok(result)
    }
}

impl Drop for Revision {
    fn drop(&mut self) {
        let _ = git(
            &self.repository,
            &[
                "worktree",
                "remove",
                // An interrupted worktree add can leave its registration locked.
                "--force",
                "--force",
                self.checkout.to_str().unwrap_or(""),
            ],
        );
    }
}

struct Process(GroupChild);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn snapshot(
    rule: &Rule,
    workspace: &Workspace,
    root: &Path,
    revision: &str,
    adapter_root: &Path,
) -> Result<BTreeMap<String, String>> {
    let mut input = tempfile::tempfile()?;
    serde_json::to_writer(
        &mut input,
        &Request {
            version: 1,
            workspace_root: &workspace.root,
            revision_root: root,
            adapter_root,
            revision,
        },
    )?;
    input.rewind()?;
    let mut output = tempfile::tempfile()?;
    let arguments: Vec<_> = rule
        .projection
        .command
        .iter()
        .map(|argument| {
            argument
                .replace("{workspaceRoot}", &workspace.root.to_string_lossy())
                .replace("{revisionRoot}", &root.to_string_lossy())
                .replace("{adapterRoot}", &adapter_root.to_string_lossy())
        })
        .collect();
    let executable = Path::new(&arguments[0]);
    let executable = if executable.is_relative() && executable.components().count() > 1 {
        adapter_root.join(executable)
    } else {
        executable.to_owned()
    };
    let mut command = workspace_command(executable);
    command
        .args(&arguments[1..])
        .current_dir(adapter_root)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::inherit());
    let mut child = Process(
        command
            .group_spawn()
            .with_context(|| format!("cannot start projection {}", rule.projection.name))?,
    );
    let deadline = Instant::now() + Duration::from_secs(rule.projection.timeout_seconds);
    loop {
        ensure!(
            output.metadata()?.len() <= MAX_OUTPUT,
            "projection {} exceeded the 1 MiB output limit",
            rule.projection.name
        );
        if let Some(status) = child.0.inner().try_wait()? {
            ensure!(
                status.success(),
                "projection {} exited with {status}",
                rule.projection.name
            );
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "projection {} timed out",
            rule.projection.name
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(child);
    output.rewind()?;
    let mut bytes = Vec::new();
    output.take(MAX_OUTPUT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_OUTPUT,
        "projection output exceeded 1 MiB"
    );
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("invalid projection manifest")?;
    ensure!(
        manifest.version == 1,
        "unsupported projection protocol version {}",
        manifest.version
    );
    for (path, fingerprint) in &manifest.artifacts {
        if path.is_empty()
            || path.contains(['\\', '\0', ':'])
            || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            bail!("invalid workspace-relative artifact path {path:?}");
        }
        ensure!(
            rule.outputs.is_match(path) && !protected(path),
            "artifact {path:?} is outside declared outputs or is protected metadata"
        );
        ensure!(
            !fingerprint.is_empty() && fingerprint.len() <= 1024,
            "artifact {path:?} needs a nonempty fingerprint of at most 1024 bytes"
        );
    }
    Ok(manifest.artifacts)
}
