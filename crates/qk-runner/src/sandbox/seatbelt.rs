//! Seatbelt profiles: a task's policy as the rules `sandbox-exec` takes.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::policy::{Grant, Policy, Scope};
use super::{Mode, Packages, TAG};

// Rules name `file-read-data` rather than `file-read*`: Seatbelt lets a rule
// for a specific operation win over one for a wildcard, whatever their order,
// so a wildcard allow would not override the reporting rule.

/// A string in a profile.
pub(super) fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A regular expression matching `text` literally.
fn literal_regex(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if "\\.+*?()|[]{}^$".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// How much of a task's plan its profile holds it to.
#[derive(Debug, PartialEq)]
pub(super) enum Fit {
    Whole,
    /// All but its installed packages, with why.
    WithoutPackages(String),
    /// Nothing: Seatbelt rejects its profile, with why.
    Rejected(String),
}

/// Writes a task's profile to `path`. Seatbelt caps the paths a profile with
/// reports may name, and a large store can take them all: a task whose
/// profile it rejects is still held to its inputs and outputs, without
/// reporting packages.
pub(super) fn seatbelt(policy: &Policy, id: &str, path: &Path) -> Result<Fit> {
    std::fs::write(path, profile(policy, id)?)?;
    let Err(reason) = compiles(path) else {
        return Ok(Fit::Whole);
    };
    if !policy.checks_packages() {
        return Ok(Fit::Rejected(reason));
    }
    std::fs::write(path, profile(&policy.without_packages(), id)?)?;
    Ok(match compiles(path) {
        Ok(()) => Fit::WithoutPackages(reason),
        Err(reason) => Fit::Rejected(reason),
    })
}

/// Whether Seatbelt accepts a profile, or its complaint.
fn compiles(profile: &Path) -> std::result::Result<(), String> {
    let output = Command::new("sandbox-exec")
        .arg("-f")
        .arg(profile)
        .arg("/usr/bin/true")
        .output()
        .map_err(|error| format!("cannot run sandbox-exec: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("sandbox-exec rejected the profile")
            .trim_start_matches("sandbox-exec: ")
            .to_owned())
    }
}

/// The Seatbelt profile for a task's policy: everything in the workspace
/// reported or refused, then each grant allowed in turn.
fn profile(policy: &Policy, id: &str) -> Result<String> {
    let text = |path: &Path| -> Result<String> {
        path.to_str()
            .map(quote)
            .context("sandboxed paths must be UTF-8")
    };
    let root_text = policy
        .root()
        .to_str()
        .context("the workspace path must be UTF-8")?;
    let tag = quote(&format!("{TAG}{id}"));
    let mut rules = vec!["(version 1)".to_owned(), "(allow default)".to_owned()];
    match policy.mode() {
        Mode::Audit => {
            rules.push(format!(
                "(allow file-read-data (subpath {}) (with report) (with message {tag}))",
                quote(root_text)
            ));
            rules.push(format!(
                "(allow file-write* (subpath {}) (with report) (with message {tag}))",
                quote(root_text)
            ));
        }
        Mode::Enforce => {
            rules.push(format!(
                "(deny file-read-data (subpath {}) (with message {tag}))",
                quote(root_text)
            ));
            rules.push(format!(
                "(deny file-write* (subpath {}) (with message {tag}))",
                quote(root_text)
            ));
        }
    }
    let operations = |grant: &Grant| {
        if grant.writes() {
            "file-read-data file-write*"
        } else {
            "file-read-data"
        }
    };
    let mut grants = policy.grants().peekable();
    while let Some(grant) = grants.next() {
        match grant.scope {
            Scope::Tree(tree) => rules.push(format!(
                "(allow {} (subpath {}))",
                operations(&grant),
                text(tree)?
            )),
            // Files one after another are spread over rules of a few hundred
            // each; together they count toward Seatbelt's cap, as the
            // packages' do.
            Scope::File(file) => {
                let mut files = vec![file];
                while let Some(Grant {
                    scope: Scope::File(file),
                    ..
                }) = grants.next_if(|next| {
                    matches!(next.scope, Scope::File(_)) && next.writes() == grant.writes()
                }) {
                    files.push(file);
                }
                for chunk in files.chunks(200) {
                    rules.push(format!("(allow {}", operations(&grant)));
                    for file in chunk {
                        rules.push(format!("  (literal {})", text(file)?));
                    }
                    rules.push(")".to_owned());
                }
            }
            Scope::NodeModules => rules.push(format!(
                "(allow {} (regex #\"^{}/(.*/)?node_modules/\"))",
                operations(&grant),
                literal_regex(root_text)
            )),
            Scope::Dotenv => rules.push(format!(
                "(allow {} (regex #\"^{}/(.*/)?\\.(env|env\\.[^/]*|[^/]*\\.env)$\"))",
                operations(&grant),
                literal_regex(root_text)
            )),
            Scope::Directories => rules.push(format!(
                "(allow {} (vnode-type DIRECTORY))",
                operations(&grant)
            )),
            Scope::Manifests(packages) => {
                rules.extend(manifests(packages, root_text, &tag, &text)?);
            }
        }
    }
    Ok(rules.join("\n") + "\n")
}

/// The rules reporting installed packages' manifests.
///
/// A package the key leaves out is reported by its manifests, which Node
/// and bundlers read for every package they load: a report for each of
/// the thousands of files a tool reads would crowd out the run's other
/// reports, which the log drops when many arrive at once. Seatbelt matches
/// the path a link resolves to, so reads through an importer's
/// `node_modules/<name>` land here too.
///
/// Seatbelt caps the paths a profile with reports may name at 64 KiB
/// together, whatever rules they are spread over, and a store may hold
/// thousands of directories: the shorter of the two lists is named, those
/// to report or, after a rule reporting every manifest in the store, those
/// not to, since a later rule wins.
fn manifests(
    packages: &Packages,
    root_text: &str,
    tag: &str,
    text: &dyn Fn(&Path) -> Result<String>,
) -> Result<Vec<String>> {
    let named = |directories: &[PathBuf], report: &str| -> Result<Vec<String>> {
        let mut rules = Vec::new();
        for chunk in directories.chunks(200) {
            rules.push(
                "(allow file-read-data (require-all (regex #\"/package\\.json$\") (require-any"
                    .to_owned(),
            );
            for directory in chunk {
                rules.push(format!("  (subpath {})", text(directory)?));
            }
            rules.push(format!(")){report})"));
        }
        Ok(rules)
    };
    let report = format!(" (with report) (with message {tag})");
    Ok(match packages {
        Packages::Unkeyed(directories) => named(directories, &report)?,
        Packages::AllBut(directories) => {
            let mut rules = vec![format!(
                "(allow file-read-data (regex #\"^{}/node_modules/\\.pnpm/[^/]+/(.*/)?package\\.json$\"){report})",
                literal_regex(root_text)
            )];
            rules.extend(named(directories, "")?);
            rules
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Plan;

    /// A plan naming one of everything a profile renders, in `/w`, with qk's
    /// state in `/w/.qk`.
    fn rich_plan(packages: Option<Packages>) -> Plan {
        let root = Path::new("/w");
        Plan {
            read_directories: vec![root.join("libs/lib")],
            read_files: vec![root.join("package.json"), root.join("apps/app/src/main.ts")],
            read_trees: [root.join("libs/dep/dist")].into(),
            unlinked: [root.join("libs/dep/dist")].into(),
            write_trees: [root.join("apps/app/dist"), root.join("tmp/app")].into(),
            scratch: [root.join("tmp/app")].into(),
            packages,
        }
    }

    fn rendered(mode: Mode, packages: Option<Packages>) -> String {
        let policy = Policy::new(
            mode,
            Path::new("/w"),
            Path::new("/w/.qk"),
            rich_plan(packages),
        );
        profile(&policy, "app:build").unwrap()
    }

    /// Audit reports everything in the workspace, then allows what the plan
    /// names; the store's manifests are reported again after `node_modules`
    /// is allowed, but for the directories named after them. The paths are
    /// Unix paths, as on macOS, where profiles are made.
    #[cfg(unix)]
    #[test]
    fn renders_an_audit_profile() {
        let keyed = vec![PathBuf::from("/w/node_modules/.pnpm/a@1.0.0")];
        assert_eq!(
            rendered(Mode::Audit, Some(Packages::AllBut(keyed))),
            r##"(version 1)
(allow default)
(allow file-read-data (subpath "/w") (with report) (with message "qk-task:app:build"))
(allow file-write* (subpath "/w") (with report) (with message "qk-task:app:build"))
(allow file-read-data file-write* (regex #"^/w/(.*/)?node_modules/"))
(allow file-read-data (regex #"^/w/node_modules/\.pnpm/[^/]+/(.*/)?package\.json$") (with report) (with message "qk-task:app:build"))
(allow file-read-data (require-all (regex #"/package\.json$") (require-any
  (subpath "/w/node_modules/.pnpm/a@1.0.0")
)))
(allow file-read-data (vnode-type DIRECTORY))
(allow file-read-data file-write* (subpath "/w/.qk"))
(allow file-read-data (subpath "/w/libs/lib"))
(allow file-read-data
  (literal "/w/package.json")
  (literal "/w/apps/app/src/main.ts")
)
(allow file-read-data (subpath "/w/libs/dep/dist"))
(allow file-read-data file-write* (subpath "/w/apps/app/dist"))
(allow file-read-data file-write* (subpath "/w/tmp/app"))
"##
        );
        let unkeyed = ["b@1.0.0", "c@1.0.0"]
            .map(|name| PathBuf::from("/w/node_modules/.pnpm").join(name))
            .to_vec();
        assert_eq!(
            rendered(Mode::Audit, Some(Packages::Unkeyed(unkeyed))),
            r##"(version 1)
(allow default)
(allow file-read-data (subpath "/w") (with report) (with message "qk-task:app:build"))
(allow file-write* (subpath "/w") (with report) (with message "qk-task:app:build"))
(allow file-read-data file-write* (regex #"^/w/(.*/)?node_modules/"))
(allow file-read-data (require-all (regex #"/package\.json$") (require-any
  (subpath "/w/node_modules/.pnpm/b@1.0.0")
  (subpath "/w/node_modules/.pnpm/c@1.0.0")
)) (with report) (with message "qk-task:app:build"))
(allow file-read-data (vnode-type DIRECTORY))
(allow file-read-data file-write* (subpath "/w/.qk"))
(allow file-read-data (subpath "/w/libs/lib"))
(allow file-read-data
  (literal "/w/package.json")
  (literal "/w/apps/app/src/main.ts")
)
(allow file-read-data (subpath "/w/libs/dep/dist"))
(allow file-read-data file-write* (subpath "/w/apps/app/dist"))
(allow file-read-data file-write* (subpath "/w/tmp/app"))
"##
        );
    }

    /// Enforce refuses everything in the workspace, then allows `.git`,
    /// dotenv files and what the plan names.
    #[cfg(unix)]
    #[test]
    fn renders_an_enforce_profile() {
        assert_eq!(
            rendered(Mode::Enforce, None),
            r##"(version 1)
(allow default)
(deny file-read-data (subpath "/w") (with message "qk-task:app:build"))
(deny file-write* (subpath "/w") (with message "qk-task:app:build"))
(allow file-read-data file-write* (subpath "/w/.git"))
(allow file-read-data (regex #"^/w/(.*/)?\.(env|env\.[^/]*|[^/]*\.env)$"))
(allow file-read-data file-write* (regex #"^/w/(.*/)?node_modules/"))
(allow file-read-data (vnode-type DIRECTORY))
(allow file-read-data file-write* (subpath "/w/.qk"))
(allow file-read-data (subpath "/w/libs/lib"))
(allow file-read-data
  (literal "/w/package.json")
  (literal "/w/apps/app/src/main.ts")
)
(allow file-read-data (subpath "/w/libs/dep/dist"))
(allow file-read-data file-write* (subpath "/w/apps/app/dist"))
(allow file-read-data file-write* (subpath "/w/tmp/app"))
"##
        );
    }

    /// Files are named in rules of 200 each.
    #[test]
    fn spreads_files_over_rules() {
        let mut plan = rich_plan(None);
        plan.read_files = (0..201)
            .map(|index| PathBuf::from(format!("/w/src/{index}.ts")))
            .collect();
        let policy = Policy::new(Mode::Enforce, Path::new("/w"), Path::new("/w/.qk"), plan);
        let text = profile(&policy, "app:build").unwrap();
        let rules: Vec<usize> = text
            .split("(allow file-read-data\n")
            .skip(1)
            .map(|rule| rule.split_once("\n)").unwrap().0.lines().count())
            .collect();
        assert_eq!(rules, [200, 1]);
    }

    /// Seatbelt rejects a profile naming thousands of store directories with
    /// reports; the shorter list fits, and a store too large for either still
    /// holds the task to its inputs and outputs.
    #[cfg(target_os = "macos")]
    #[test]
    fn fits_a_large_store_into_a_profile() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = |range: std::ops::Range<usize>| -> Vec<PathBuf> {
            range
                .map(|index| {
                    root.join(format!(
                        "node_modules/.pnpm/@scope+package-{index}@1.0.{index}_peer@2.0.0"
                    ))
                })
                .collect()
        };
        let plan = |packages| Plan {
            read_files: vec![root.join("src/index.ts")],
            packages: Some(packages),
            ..Plan::default()
        };
        let fit = |plan: Plan| {
            let policy = Policy::new(Mode::Audit, &root, &root.join(".qk"), plan);
            seatbelt(&policy, "app:lint", &root.join("p.sb")).unwrap()
        };
        let (keyed, unkeyed) = (store(0..100), store(100..2600));
        assert_eq!(
            fit(plan(Packages::shorter(keyed, unkeyed.clone()))),
            Fit::Whole
        );
        assert!(matches!(
            fit(plan(Packages::Unkeyed(unkeyed))),
            Fit::WithoutPackages(reason) if reason.contains("exceeds maximum")
        ));
        assert!(matches!(
            Packages::shorter(store(0..2), store(2..3)),
            Packages::Unkeyed(directories) if directories.len() == 1
        ));
    }
}
