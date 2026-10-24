//! Whether affected selection picks tasks or projects, as Nx 24's
//! `selectsAffectedTasks` decides it: `NX_LEGACY_AFFECTED=true` selects
//! projects and `false` tasks; otherwise nx.json's `affected.legacy` selects
//! projects, and tasks are the default. qk's `--granularity` comes before
//! both. Change projections only apply to project selection, so a profile
//! with projections selects projects unless `--granularity task` asks for
//! tasks, which then fails.

use anyhow::Result;
use qk_config::Workspace;
use serde::Serialize;
use serde_json::Value;

use crate::projections;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    Project,
    Task,
}

/// What decided the granularity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Source {
    #[serde(rename = "--granularity")]
    Flag,
    #[serde(rename = "NX_LEGACY_AFFECTED")]
    Env,
    #[serde(rename = "affected.legacy")]
    NxJson,
    /// The affected profile has projections, where tasks would be selected.
    #[serde(rename = "projections")]
    Projections,
    #[serde(rename = "default")]
    Default,
}

/// The granularity and what decided it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Selection {
    pub granularity: Granularity,
    pub from: Source,
}

impl std::fmt::Display for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let granularity = match self.granularity {
            Granularity::Project => "project",
            Granularity::Task => "task",
        };
        let why = match self.from {
            Source::Flag => format!("--granularity {granularity}"),
            Source::Env => format!(
                "NX_LEGACY_AFFECTED={}",
                self.granularity == Granularity::Project
            ),
            Source::NxJson => "nx.json affected.legacy".into(),
            Source::Projections => "the affected profile has projections".into(),
            Source::Default => "the default".into(),
        };
        write!(f, "{granularity} ({why})")
    }
}

/// The granularity selection uses: `explicit` if given, else what
/// `NX_LEGACY_AFFECTED` and nx.json say, with a profile's projections
/// selecting projects in place of tasks.
pub fn granularity(
    workspace: &Workspace,
    explicit: Option<Granularity>,
    profile: Option<&str>,
) -> Result<Selection> {
    let projections = match (explicit, profile) {
        (None, Some(profile)) => projections::has_projections(workspace, profile)?,
        _ => false,
    };
    Ok(decide(
        explicit,
        std::env::var("NX_LEGACY_AFFECTED").ok().as_deref(),
        workspace
            .config
            .extra
            .get("affected")
            .and_then(|affected| affected.get("legacy")),
        projections,
    ))
}

fn decide(
    explicit: Option<Granularity>,
    env: Option<&str>,
    legacy: Option<&Value>,
    projections: bool,
) -> Selection {
    let selection = |granularity, from| Selection { granularity, from };
    if let Some(granularity) = explicit {
        return selection(granularity, Source::Flag);
    }
    let implicit = match env {
        Some("true") => selection(Granularity::Project, Source::Env),
        Some("false") => selection(Granularity::Task, Source::Env),
        _ if legacy.is_some_and(truthy) => selection(Granularity::Project, Source::NxJson),
        _ => selection(Granularity::Task, Source::Default),
    };
    if projections && implicit.granularity == Granularity::Task {
        return selection(Granularity::Project, Source::Projections);
    }
    implicit
}

/// Whether JavaScript counts `value` as true, as Nx reads `affected.legacy`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decided(
        explicit: Option<Granularity>,
        env: Option<&str>,
        legacy: Option<Value>,
        projections: bool,
    ) -> (Granularity, Source) {
        let selection = decide(explicit, env, legacy.as_ref(), projections);
        (selection.granularity, selection.from)
    }

    #[test]
    fn the_flag_comes_first_then_the_variable_then_nx_json() {
        use Granularity::{Project, Task};
        let legacy = || Some(json!(true));
        assert_eq!(decided(None, None, None, false), (Task, Source::Default));
        assert_eq!(
            decided(None, None, legacy(), false),
            (Project, Source::NxJson)
        );
        assert_eq!(
            decided(None, Some("false"), legacy(), false),
            (Task, Source::Env)
        );
        assert_eq!(
            decided(None, Some("true"), Some(json!(false)), false),
            (Project, Source::Env)
        );
        // Nx reads only `true` and `false`.
        assert_eq!(
            decided(None, Some("1"), None, false),
            (Task, Source::Default)
        );
        assert_eq!(
            decided(Some(Project), Some("false"), None, false),
            (Project, Source::Flag)
        );
        assert_eq!(
            decided(Some(Task), Some("true"), legacy(), false),
            (Task, Source::Flag)
        );
    }

    #[test]
    fn legacy_is_read_as_javascript_truthiness() {
        for (value, legacy) in [
            (json!(true), true),
            (json!(1), true),
            (json!("yes"), true),
            (json!({}), true),
            (json!(false), false),
            (json!(0), false),
            (json!(""), false),
            (json!(null), false),
        ] {
            assert_eq!(
                decided(None, None, Some(value.clone()), false).0 == Granularity::Project,
                legacy,
                "{value}"
            );
        }
    }

    #[test]
    fn projections_select_projects_where_tasks_would_be_selected() {
        use Granularity::{Project, Task};
        assert_eq!(
            decided(None, None, None, true),
            (Project, Source::Projections)
        );
        assert_eq!(
            decided(None, Some("false"), None, true),
            (Project, Source::Projections)
        );
        // Already projects: the setting decided it.
        assert_eq!(
            decided(None, Some("true"), None, true),
            (Project, Source::Env)
        );
        assert_eq!(
            decided(None, None, Some(json!(true)), true),
            (Project, Source::NxJson)
        );
        assert_eq!(decided(Some(Task), None, None, true), (Task, Source::Flag));
    }

    #[test]
    fn display_says_what_decided() {
        let shown = |granularity, from| Selection { granularity, from }.to_string();
        assert_eq!(
            shown(Granularity::Task, Source::Default),
            "task (the default)"
        );
        assert_eq!(
            shown(Granularity::Project, Source::Env),
            "project (NX_LEGACY_AFFECTED=true)"
        );
        assert_eq!(
            shown(Granularity::Task, Source::Env),
            "task (NX_LEGACY_AFFECTED=false)"
        );
        assert_eq!(
            shown(Granularity::Project, Source::Flag),
            "project (--granularity project)"
        );
    }
}
