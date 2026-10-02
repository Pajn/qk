//! Advisory input analysis. Observations never change task cache keys.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod suggestions;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    ReadData,
    OpenRead,
    Metadata,
    ListDirectory,
    ReadDirectory,
    ReadLink,
    Execute,
    Write,
    Rename,
    Delete,
}

impl Operation {
    pub fn writes(&self) -> bool {
        matches!(self, Self::Write | Self::Rename | Self::Delete)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    Input,
    Uncovered,
    OwnOutput,
    DependencyOutput,
    WarmState,
    Generated,
    InstalledPackage,
    ResolutionMetadata,
    StructuredInput,
    Discovery,
    GitState,
    External,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    pub process: u32,
    /// Relative to the workspace, or absolute for external accesses.
    pub path: String,
    /// Descriptor target when the backend provides it at access time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    /// A declared symlink input that covers this observed target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyed_path: Option<String>,
    pub operation: Operation,
    /// `success`, `missing`, another errno, or `unknown` for Seatbelt events.
    pub result: String,
    pub category: Category,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub backend: String,
    pub partial: bool,
    pub limitations: Vec<String>,
    pub diagnostics: Vec<String>,
}

/// Recursive suggestions retain existing files and future files in active trees.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobSuggestion {
    pub fileset: String,
    pub observed_files: usize,
    pub additional_declared_files: usize,
    #[serde(default)]
    pub reason: String,
}

/// Only input declarations are retained; evaluated environment values are absent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Declaration {
    pub target: String,
    pub inputs: Vec<Value>,
    pub named_inputs: BTreeMap<String, Vec<Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAnalysis {
    pub context: Value,
    pub outcome: String,
    pub coverage: Coverage,
    pub declared_files: BTreeSet<String>,
    /// Configuration files qk always keys, regardless of target filesets.
    pub mandatory_files: BTreeSet<String>,
    pub accesses: Vec<Access>,
    pub observed_inputs: BTreeSet<String>,
    pub uncovered_accesses: BTreeSet<String>,
    pub unobserved_inputs: BTreeSet<String>,
    /// Successful runs contributing to the union; IDs prevent double counting.
    pub successful_runs: BTreeSet<String>,
    pub successful_observations: BTreeSet<String>,
    /// Exact observed paths for review.
    pub candidate_filesets: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Declaration>,
    #[serde(default)]
    pub glob_suggestions: Vec<GlobSuggestion>,
    /// A project.json fragment, always requiring review across configurations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_fragment: Option<Value>,
    #[serde(default)]
    pub successful_directories: BTreeSet<String>,
    #[serde(default)]
    pub suggestion_notes: Vec<String>,
    #[serde(default)]
    pub access_review: AccessReview,
}

/// Review groups never discard the raw uncovered accesses.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessReview {
    pub content_reads: BTreeSet<String>,
    pub manifest_reads: BTreeSet<String>,
    pub toolchain_reads: BTreeSet<String>,
    pub metadata_checks: BTreeSet<String>,
}

impl TaskAnalysis {
    pub fn summarize(&mut self, run: &str) {
        self.observed_inputs = self
            .accesses
            .iter()
            .filter(|event| !event.operation.writes() && event.result != "missing")
            .filter(|event| event.category == Category::Input)
            .map(|event| event.keyed_path.as_ref().unwrap_or(&event.path).clone())
            .collect();
        self.uncovered_accesses = self
            .accesses
            .iter()
            .filter(|event| !event.operation.writes() && event.category == Category::Uncovered)
            .map(|event| event.path.clone())
            .collect();
        if self.outcome == "success" && self.coverage.diagnostics.is_empty() {
            self.successful_runs.insert(run.to_owned());
            self.successful_observations
                .extend(self.observed_inputs.clone());
            self.successful_directories
                .extend(self.recorded_directories());
        }
        self.refresh_candidates();
    }

    /// Only observations of the same task definition, arguments and platform
    /// can support a union. Failed/aborted traces don't support narrowing.
    pub fn merge(&mut self, previous: &Self) -> bool {
        if self.context != previous.context {
            return false;
        }
        self.successful_runs
            .extend(previous.successful_runs.clone());
        self.successful_observations
            .extend(previous.successful_observations.clone());
        self.successful_directories
            .extend(previous.successful_directories.clone());
        // Version 1 reports predating directory unions still contain these events.
        if previous.outcome == "success" && previous.coverage.diagnostics.is_empty() {
            self.successful_directories
                .extend(previous.recorded_directories());
        }
        self.refresh_candidates();
        true
    }

    fn recorded_directories(&self) -> BTreeSet<String> {
        self.accesses
            .iter()
            .filter(|event| {
                event.category == Category::Discovery
                    && matches!(
                        event.operation,
                        Operation::ListDirectory | Operation::ReadDirectory
                    )
                    && matches!(event.result.as_str(), "success" | "unknown")
            })
            .map(|event| event.path.clone())
            .collect()
    }

    fn refresh_candidates(&mut self) {
        self.unobserved_inputs = if self.successful_runs.is_empty() {
            BTreeSet::new()
        } else {
            &(&self.declared_files - &self.mandatory_files) - &self.successful_observations
        };
        // Keep exact observations alongside the broader review suggestions.
        self.candidate_filesets = self
            .successful_observations
            .iter()
            .filter(|path| self.declared_files.contains(*path))
            .filter(|path| !self.mandatory_files.contains(*path))
            .filter(|path| {
                !path.contains([
                    '*', '?', '[', ']', '{', '}', '!', '(', ')', '\\', '+', '@', '|', ',',
                ])
            })
            .map(|path| format!("{{workspaceRoot}}/{path}"))
            .collect();
        self.access_review = AccessReview::default();
        let reads: BTreeSet<_> = self
            .accesses
            .iter()
            .filter(|event| {
                event.category == Category::Uncovered
                    && !event.operation.writes()
                    && event.result != "missing"
            })
            .filter(|event| event.operation != Operation::Metadata)
            .map(|event| event.path.clone())
            .collect();
        for path in &self.uncovered_accesses {
            if !reads.contains(path) {
                self.access_review.metadata_checks.insert(path.clone());
            } else if path.ends_with("/package.json") || path == "package.json" {
                self.access_review.manifest_reads.insert(path.clone());
            } else if matches!(
                path.as_str(),
                ".tool-versions"
                    | "mise.toml"
                    | ".nvmrc"
                    | ".node-version"
                    | ".npmrc"
                    | ".pnpmfile.cjs"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
            ) {
                self.access_review.toolchain_reads.insert(path.clone());
            } else {
                self.access_review.content_reads.insert(path.clone());
            }
        }
        self.suggest();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub schema_version: u32,
    pub run_id: String,
    pub tasks: BTreeMap<String, TaskAnalysis>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(outcome: &str, path: &str) -> TaskAnalysis {
        let mut task = TaskAnalysis {
            context: serde_json::json!({"args": []}),
            outcome: outcome.into(),
            coverage: Coverage {
                backend: "fixture".into(),
                partial: true,
                limitations: vec![],
                diagnostics: vec![],
            },
            declared_files: ["a".into(), "b".into()].into(),
            mandatory_files: BTreeSet::new(),
            accesses: vec![Access {
                process: 1,
                path: path.into(),
                resolved_path: None,
                keyed_path: None,
                operation: Operation::OpenRead,
                result: "success".into(),
                category: Category::Input,
            }],
            observed_inputs: BTreeSet::new(),
            uncovered_accesses: BTreeSet::new(),
            unobserved_inputs: BTreeSet::new(),
            successful_runs: BTreeSet::new(),
            successful_observations: BTreeSet::new(),
            candidate_filesets: BTreeSet::new(),
            declaration: None,
            glob_suggestions: vec![],
            configuration_fragment: None,
            successful_directories: BTreeSet::new(),
            suggestion_notes: vec![],
            access_review: AccessReview::default(),
        };
        task.summarize(path);
        task
    }

    #[test]
    fn union_only_uses_successful_compatible_recordings_and_deduplicates_runs() {
        let mut first = analysis("success", "a");
        let second = analysis("success", "b");
        assert!(first.merge(&second));
        assert!(first.merge(&second));
        assert_eq!(first.successful_runs.len(), 2);
        assert!(first.unobserved_inputs.is_empty());
        let mut different = analysis("success", "c");
        different.context = serde_json::json!({"args": ["--prod"]});
        assert!(!first.merge(&different));
        let mut failed = analysis("failed", "b");
        assert!(failed.unobserved_inputs.is_empty());
        failed.merge(&analysis("failed", "a"));
        assert!(failed.successful_runs.is_empty());
    }

    #[test]
    fn interrupted_collection_does_not_support_narrowing() {
        let mut task = analysis("failed", "a");
        task.outcome = "success".into();
        task.coverage.diagnostics.push("collector stopped".into());
        task.summarize("interrupted");
        assert!(task.candidate_filesets.is_empty());
    }
    #[test]
    fn drafts_expand_local_named_inputs_and_preserve_other_dependencies() {
        let mut task = analysis("success", "src/a.ts");
        task.declared_files = [
            "src/a.ts",
            "src/b.ts",
            "src/unused.ts",
            "src/nested/c.ts",
            "src/config.json",
        ]
        .map(String::from)
        .into();
        task.successful_observations
            .extend(["src/b.ts".into(), "src/config.json".into()]);
        let retained = vec![
            serde_json::json!("!{projectRoot}/src/**/*.test.ts"),
            serde_json::json!({"env": "MODE"}),
            serde_json::json!({"runtime": "node --version"}),
            serde_json::json!({"json": "{projectRoot}/settings.json", "fields": ["value"]}),
            serde_json::json!("^production"),
            serde_json::json!({"input": "shared", "projects": ["tools"]}),
            serde_json::json!({"fileset": "{projectRoot}/**/*", "dependencies": true}),
            serde_json::json!({"dependentTasksOutputFiles": "**/*.d.ts"}),
        ];
        let mut local = vec![serde_json::json!({"fileset": "{projectRoot}/**/*"})];
        local.extend(retained.clone());
        task.declaration = Some(Declaration {
            target: "build".into(),
            inputs: vec![serde_json::json!({"input": "production", "projects": "self"})],
            named_inputs: [
                ("production".into(), vec![serde_json::json!("shared")]),
                ("shared".into(), local),
            ]
            .into(),
        });
        task.refresh_candidates();
        assert_eq!(task.glob_suggestions.len(), 1);
        let glob = &task.glob_suggestions[0];
        assert_eq!(glob.fileset, "{projectRoot}/src/**/*.{json,ts}");
        assert_eq!(glob.observed_files, 3);
        assert_eq!(glob.additional_declared_files, 2);
        let inputs = task.configuration_fragment.as_ref().unwrap()["targets"]["build"]["inputs"]
            .as_array()
            .unwrap();
        assert!(inputs.contains(&serde_json::json!("{projectRoot}/src/**/*.{json,ts}")));
        for input in retained {
            assert!(inputs.contains(&input), "{input}");
        }
        assert!(!inputs.contains(&serde_json::json!("{projectRoot}/src/**/*")));
    }

    #[test]
    fn fragments_require_safe_observed_paths_and_successful_evidence() {
        let mut task = analysis("failed", "src/a.ts");
        task.declaration = Some(Declaration {
            target: "build".into(),
            inputs: vec![serde_json::json!("default"), serde_json::json!("^default")],
            named_inputs: BTreeMap::new(),
        });
        task.refresh_candidates();
        assert!(task.configuration_fragment.is_none());
        task.successful_runs.insert("prior".into());
        task.declared_files = ["src/a.ts".into(), "src/a[1].ts".into(), "nx.json".into()].into();
        task.mandatory_files.insert("nx.json".into());
        task.successful_observations = task.declared_files.clone();
        task.refresh_candidates();
        assert_eq!(
            task.candidate_filesets,
            ["{workspaceRoot}/src/a.ts".into()].into()
        );
        assert_eq!(
            task.configuration_fragment.unwrap(),
            serde_json::json!({"targets": {"build": {"inputs": ["{projectRoot}/src/**/*.ts", "^default"]}}})
        );
    }

    #[test]
    fn old_reports_deserialize_and_merging_rebuilds_drafts_from_current_declarations() {
        let mut old = serde_json::to_value(analysis("success", "a")).unwrap();
        for key in ["declaration", "globSuggestions", "configurationFragment"] {
            old.as_object_mut().unwrap().remove(key);
        }
        let old: TaskAnalysis = serde_json::from_value(old).unwrap();
        let mut current = analysis("success", "b");
        current.declaration = Some(Declaration {
            target: "check".into(),
            inputs: vec![
                serde_json::json!("default"),
                serde_json::json!({"env": "CHECK"}),
            ],
            named_inputs: BTreeMap::new(),
        });
        assert!(current.merge(&old));
        assert_eq!(
            current.configuration_fragment.unwrap(),
            serde_json::json!({"targets": {"check": {"inputs": ["{projectRoot}/a", "{projectRoot}/b", {"env": "CHECK"}]}}})
        );
    }
    #[test]
    fn specific_named_inputs_are_kept_without_a_fragment_even_with_sparse_observations() {
        let mut task = analysis("success", "src/a.ts");
        task.declaration = Some(Declaration {
            target: "lint".into(),
            inputs: vec![
                serde_json::json!("source"),
                serde_json::json!({"env": "CI"}),
            ],
            named_inputs: [(
                "source".into(),
                vec![
                    serde_json::json!("{projectRoot}/**/*.{ts,tsx}"),
                    serde_json::json!("!{projectRoot}/generated/**/*"),
                ],
            )]
            .into(),
        });
        task.refresh_candidates();
        assert!(task.configuration_fragment.is_none());
        assert!(task.glob_suggestions.is_empty());
        assert!(task.suggestion_notes[0].contains("retained"));
    }

    #[test]
    fn directory_unions_keep_future_asset_types_and_nested_files() {
        let mut task = analysis("success", "apps/web/public/logo.svg");
        task.context = serde_json::json!({"projectRoot": "apps/web"});
        task.declared_files = [
            "apps/web/public/logo.svg".into(),
            "apps/web/public/fonts/a.woff2".into(),
            "apps/web/src/a.ts".into(),
            "apps/web/src/b.tsx".into(),
        ]
        .into();
        task.successful_observations = [
            "apps/web/public/logo.svg".into(),
            "apps/web/src/a.ts".into(),
        ]
        .into();
        task.declaration = Some(Declaration {
            target: "build".into(),
            inputs: vec![serde_json::json!("default"), serde_json::json!("tsconfigs")],
            named_inputs: [(
                "tsconfigs".into(),
                vec![serde_json::json!("{projectRoot}/tsconfig*.json")],
            )]
            .into(),
        });
        let mut previous = task.clone();
        previous
            .successful_directories
            .insert("apps/web/public".into());
        task.merge(&previous);
        let fragment = task.configuration_fragment.as_ref().unwrap();
        assert_eq!(
            fragment["targets"]["build"]["inputs"],
            serde_json::json!([
                "{projectRoot}/public/**/*",
                "{projectRoot}/src/**/*.{ts,tsx}",
                "tsconfigs"
            ])
        );
        let matcher = globset::Glob::new("apps/web/public/**/*")
            .unwrap()
            .compile_matcher();
        assert!(matcher.is_match("apps/web/public/new.json"));
        assert!(matcher.is_match("apps/web/public/nested/image.png"));
        assert_eq!(task.glob_suggestions[0].additional_declared_files, 1);
        assert_eq!(task.glob_suggestions[1].additional_declared_files, 1);
    }

    #[test]
    fn rewriting_preserves_exclusion_order_and_opaque_cross_project_inputs() {
        let declaration = Declaration {
            target: "build".into(),
            inputs: vec![
                serde_json::json!("!{projectRoot}/src/private/**/*"),
                serde_json::json!("default"),
                serde_json::json!("!{projectRoot}/src/tests/**/*"),
                serde_json::json!({"input": "default", "projects": "dependencies"}),
            ],
            named_inputs: BTreeMap::new(),
        };
        let mut task = analysis("success", "src/a.ts");
        task.declared_files.insert("src/a.ts".into());
        task.declaration = Some(declaration);
        task.refresh_candidates();
        assert_eq!(
            task.configuration_fragment.unwrap()["targets"]["build"]["inputs"],
            serde_json::json!(["!{projectRoot}/src/private/**/*", "{projectRoot}/src/**/*.ts", "!{projectRoot}/src/tests/**/*", {"input": "default", "projects": "dependencies"}])
        );
    }

    #[test]
    fn uncovered_review_groups_reads_without_suppressing_raw_paths() {
        let mut task = analysis("failed", "unused");
        for (path, operation) in [
            ("packages/a/package.json", Operation::ReadData),
            ("mise.toml", Operation::OpenRead),
            ("src/hidden.ts", Operation::Metadata),
            ("src/hidden.ts", Operation::ReadData),
            ("patches/a.patch", Operation::Metadata),
        ] {
            task.accesses.push(Access {
                process: 1,
                path: path.into(),
                resolved_path: None,
                keyed_path: None,
                operation,
                result: "unknown".into(),
                category: Category::Uncovered,
            });
        }
        task.summarize("failed");
        assert_eq!(task.uncovered_accesses.len(), 4);
        assert_eq!(
            task.access_review.content_reads,
            ["src/hidden.ts".into()].into()
        );
        assert_eq!(
            task.access_review.manifest_reads,
            ["packages/a/package.json".into()].into()
        );
        assert_eq!(
            task.access_review.toolchain_reads,
            ["mise.toml".into()].into()
        );
        assert_eq!(
            task.access_review.metadata_checks,
            ["patches/a.patch".into()].into()
        );
        assert!(task.successful_directories.is_empty());
    }
    #[test]
    fn legacy_directory_events_merge_only_from_successful_collection() {
        let mut current = analysis("success", "public/logo.svg");
        let mut old = current.clone();
        old.accesses.push(Access {
            process: 1,
            path: "public".into(),
            resolved_path: None,
            keyed_path: None,
            operation: Operation::ReadDirectory,
            result: "unknown".into(),
            category: Category::Discovery,
        });
        assert!(current.merge(&old));
        assert!(current.successful_directories.contains("public"));
        let mut failed = old.clone();
        failed.outcome = "cancelled".into();
        failed.successful_directories.clear();
        let mut clean = analysis("success", "public/logo.svg");
        clean.merge(&failed);
        assert!(clean.successful_directories.is_empty());
        old.coverage.diagnostics.push("collector stopped".into());
        clean.merge(&old);
        assert!(clean.successful_directories.is_empty());
    }
}
