//! What a change to nx.json, the file it extends or nx.local.json affects.
//!
//! Nx 24 hashes nx.json into every task, so any change to it, even a
//! comment, affects every project. qk counts it by what it changes, as the
//! cache keys do: the settings keyed whole into every task, and each
//! project's configuration as `targetDefaults` and `namedInputs` resolve it.
//! The rest of nx.json changes no task's work.

use std::path::Path;

use anyhow::{Context, Result};
use qk_config::{Workspace, WorkspaceConfig};

use crate::Changes;

/// What the workspace configuration files changed.
pub(crate) enum NxJsonChange {
    /// Nothing that reaches a task: none changed, or only in comments,
    /// formatting or settings that change no task's work.
    None,
    /// What every task's key holds, or a change qk could not compare.
    All { file: String, detail: String },
    /// `targetDefaults` or `namedInputs`, which reach a task through its
    /// resolved configuration: `base` is the workspace the base revision's
    /// configuration makes of the head's manifests.
    Resolved {
        file: String,
        settings: Vec<&'static str>,
        base: Box<Workspace>,
    },
}

impl NxJsonChange {
    /// Compares the configuration at the two revisions of `changes`, when one
    /// of its files changed.
    pub(crate) fn new(workspace: &Workspace, changes: &Changes) -> Self {
        let files: Vec<&str> = std::iter::once("nx.json")
            .chain(workspace.extended.as_deref())
            .chain(workspace.extended_logical.as_deref())
            .chain([qk_config::LOCAL_WORKSPACE])
            .collect();
        let Some(file) = files.iter().find(|file| changes.changed(file)) else {
            return Self::None;
        };
        let all = |detail: String| Self::All {
            file: (*file).to_owned(),
            detail,
        };
        let Some(base) = changes.base.as_deref().filter(|_| !changes.whole) else {
            return all("changed, and its revisions are not compared for --files".into());
        };
        // Each file as the base revision had it: a changed one read there,
        // and one the change left alone, or outside the workspace, from disk.
        let read = |path: &Path| -> Result<Option<String>> {
            let relative = path
                .strip_prefix(&workspace.root)
                .ok()
                .and_then(Path::to_str)
                .map(|path| path.replace('\\', "/"));
            match relative {
                // Project manifests are the head's: their changes count by
                // their own rule.
                Some(relative)
                    if changes.changed(&relative) && !crate::is_project_file(&relative) =>
                {
                    changes
                        .read(&relative, Some(base))
                        .map(Some)
                        .with_context(|| {
                            format!("{relative} could not be read at the base revision")
                        })
                }
                _ => match std::fs::read_to_string(path) {
                    Ok(text) => Ok(Some(text)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => {
                        Err(error).with_context(|| format!("cannot read {}", path.display()))
                    }
                },
            }
        };
        let before = match WorkspaceConfig::read(&workspace.root, &read) {
            Ok(config) => config,
            Err(error) => return all(format!("changed and could not be compared ({error:#})")),
        };
        let after = &workspace.config;
        let (old, new) = (before.keyed_settings(), after.keyed_settings());
        if old != new {
            let keys: Vec<&str> = old
                .keys()
                .chain(new.keys())
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|key| old.get(*key) != new.get(*key))
                .collect();
            return all(format!("changed {}", keys.join(", ")));
        }
        let mut settings = Vec::new();
        if before.target_defaults != after.target_defaults {
            settings.push("targetDefaults");
        }
        if before.named_inputs != after.named_inputs {
            settings.push("namedInputs");
        }
        if settings.is_empty() {
            return Self::None;
        }
        match Workspace::load_with(&workspace.root, &read) {
            Ok(base) => Self::Resolved {
                file: (*file).to_owned(),
                settings,
                base: Box::new(base),
            },
            Err(error) => all(format!(
                "changed and the base revision's projects could not be read ({error:#})"
            )),
        }
    }
}
