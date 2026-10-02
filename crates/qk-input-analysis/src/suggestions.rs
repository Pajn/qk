use std::collections::{BTreeMap, BTreeSet};

use globset::GlobBuilder;
use serde_json::Value;

use crate::{Declaration, GlobSuggestion, TaskAnalysis};

fn local_name(input: &Value) -> Option<&str> {
    input
        .as_str()
        .filter(|name| !name.starts_with('^') && !name.contains('{'))
        .or_else(|| {
            input
                .as_object()
                .filter(|object| {
                    object.len() == 1
                        || (object.len() == 2
                            && object.get("projects") == Some(&Value::String("self".into())))
                })
                .and_then(|object| object.get("input"))
                .and_then(Value::as_str)
        })
}

fn broad(input: &Value) -> bool {
    let pattern = input.as_str().or_else(|| {
        input
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get("fileset"))
            .and_then(Value::as_str)
    });
    matches!(pattern, Some("{projectRoot}/**/*" | "{projectRoot}/**"))
}

fn literal(path: &str) -> bool {
    !path.contains([
        '*', '?', '[', ']', '{', '}', '!', '(', ')', '\\', '+', '@', '|', ',',
    ])
}

fn extension(path: &str) -> Option<&str> {
    let (stem, extension) = path.rsplit('/').next()?.rsplit_once('.')?;
    (!stem.is_empty()
        && !extension.is_empty()
        && extension
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then_some(extension)
}

impl Declaration {
    fn children(&self, name: &str) -> Option<Vec<Value>> {
        self.named_inputs.get(name).cloned().or_else(|| {
            (name == "default").then(|| vec![Value::String("{projectRoot}/**/*".into())])
        })
    }

    fn has_broad(&self, inputs: &[Value], stack: &mut BTreeSet<String>) -> bool {
        inputs.iter().any(|input| {
            if broad(input) {
                return true;
            }
            let Some(name) = local_name(input) else {
                return false;
            };
            if !stack.insert(name.into()) {
                return false;
            }
            let found = self
                .children(name)
                .is_some_and(|children| self.has_broad(&children, stack));
            stack.remove(name);
            found
        })
    }

    fn rewrite(
        &self,
        inputs: &[Value],
        replacements: &[Value],
        stack: &mut BTreeSet<String>,
    ) -> Vec<Value> {
        let mut result = vec![];
        for input in inputs {
            if broad(input) {
                result.extend_from_slice(replacements);
            } else if let Some(name) = local_name(input)
                && let Some(children) = self.children(name)
                && self.has_broad(&children, &mut BTreeSet::new())
                && stack.insert(name.into())
            {
                result.extend(self.rewrite(&children, replacements, stack));
                stack.remove(name);
            } else {
                result.push(input.clone());
            }
        }
        result
    }
}

impl TaskAnalysis {
    pub(super) fn suggest(&mut self) {
        self.glob_suggestions.clear();
        self.configuration_fragment = None;
        self.suggestion_notes.clear();
        let Some(declaration) = &self.declaration else {
            return;
        };
        if self.successful_runs.is_empty() {
            return;
        }
        if !declaration.has_broad(&declaration.inputs, &mut BTreeSet::new()) {
            self.suggestion_notes.push(
                "Existing specific filesets and named inputs retained; no replacement suggested."
                    .into(),
            );
            return;
        }
        let root = self
            .context
            .get("projectRoot")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .trim_matches('/');
        let prefix = if root == "." || root.is_empty() {
            String::new()
        } else {
            format!("{root}/")
        };
        let local = |path: &str| path.strip_prefix(&prefix).map(str::to_owned);
        let observed: BTreeSet<String> = self
            .successful_observations
            .iter()
            .filter(|path| {
                self.declared_files.contains(*path) && !self.mandatory_files.contains(*path)
            })
            .filter_map(|path| local(path))
            .collect();
        if observed.is_empty() {
            return;
        }
        let declared: BTreeSet<String> = self
            .declared_files
            .iter()
            .filter(|path| !self.mandatory_files.contains(*path))
            .filter_map(|path| local(path))
            .collect();
        let mut trees: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut root_extensions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut patterns: BTreeMap<String, String> = BTreeMap::new();
        for path in &observed {
            if let Some((directory, _)) = path.split_once('/') {
                if !literal(directory) {
                    self.suggestion_notes.push("Observed paths contain glob syntax in their directories; broad input retained.".into());
                    return;
                }
                trees
                    .entry(directory.into())
                    .or_default()
                    .insert(path.clone());
            } else if let Some(ext) = extension(path) {
                root_extensions
                    .entry(ext.into())
                    .or_default()
                    .insert(path.clone());
            } else if literal(path) {
                patterns.insert(path.clone(), "observed root file".into());
            } else {
                self.suggestion_notes.push(
                    "Observed root paths cannot be represented literally; broad input retained."
                        .into(),
                );
                return;
            }
        }
        let enumerated: BTreeSet<String> = self
            .successful_directories
            .iter()
            .filter_map(|path| local(path))
            .filter_map(|path| {
                path.split('/')
                    .next()
                    .filter(|part| !part.is_empty() && *part != ".")
                    .map(str::to_owned)
            })
            .filter(|directory| {
                literal(directory)
                    && declared
                        .iter()
                        .any(|path| path.starts_with(&format!("{directory}/")))
            })
            .collect();
        for directory in &enumerated {
            trees.entry(directory.clone()).or_default();
        }
        for (directory, _) in trees {
            let members: Vec<_> = declared
                .iter()
                .filter(|path| path.starts_with(&format!("{directory}/")))
                .collect();
            let extensions: BTreeSet<_> =
                members.iter().filter_map(|path| extension(path)).collect();
            let pattern = if enumerated.contains(&directory)
                || members.iter().any(|path| extension(path).is_none())
            {
                format!("{directory}/**/*")
            } else {
                let suffix = if extensions.len() == 1 {
                    extensions.first().unwrap().to_string()
                } else {
                    format!(
                        "{{{}}}",
                        extensions.into_iter().collect::<Vec<_>>().join(",")
                    )
                };
                format!("{directory}/**/*.{suffix}")
            };
            patterns.insert(
                pattern,
                if enumerated.contains(&directory) {
                    "directory enumeration; retain future files"
                } else {
                    "recursive source tree; retain all declared extensions"
                }
                .into(),
            );
        }
        for (extension, paths) in root_extensions {
            if paths.len() >= 2 || paths.iter().any(|path| !literal(path)) {
                patterns.insert(
                    format!("*.{extension}"),
                    "root extension group; retain future files".into(),
                );
            } else {
                patterns.insert(
                    paths.into_iter().next().unwrap(),
                    "observed root file".into(),
                );
            }
        }
        let mut replacements = vec![];
        for (pattern, reason) in patterns {
            let fileset = format!("{{projectRoot}}/{pattern}");
            replacements.push(Value::String(fileset.clone()));
            if !literal(&pattern) {
                let matcher = GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .expect("generated glob is valid")
                    .compile_matcher();
                self.glob_suggestions.push(GlobSuggestion {
                    fileset,
                    observed_files: observed
                        .iter()
                        .filter(|path| matcher.is_match(path))
                        .count(),
                    additional_declared_files: declared
                        .iter()
                        .filter(|path| matcher.is_match(path) && !observed.contains(*path))
                        .count(),
                    reason,
                });
            }
        }
        let inputs = declaration.rewrite(&declaration.inputs, &replacements, &mut BTreeSet::new());
        if inputs != declaration.inputs {
            self.configuration_fragment =
                Some(serde_json::json!({"targets": {&declaration.target: {"inputs": inputs}}}));
            self.suggestion_notes.push("Only broad project filesets are replaced. Specific filesets, exclusions and named references without broad filesets remain intact.".into());
            self.suggestion_notes.push("Root-level file discovery and absent-path probes still need review; partial traces cannot prove completeness.".into());
        }
    }
}
