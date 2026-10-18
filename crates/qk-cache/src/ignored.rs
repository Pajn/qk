//! `includeIgnored` filesets: globs read from disk, so they select files
//! `.gitignore` and `.nxignore` leave out of every other fileset, as Nx 24's
//! `disk_expansion` reads them.
//!
//! The filesets a project resolves together form one group, and a group's
//! `!` negations filter all of its positives. Each positive is read from the
//! literal directory it starts with. Without a pattern after that directory
//! it names a file, or a directory and everything under it; with one, the
//! pattern is matched below the directory. A positive read from inside a
//! project matches only that project's files, not those of a project nested
//! below it.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, bail};

use crate::glob::{Pattern, names_a_path};

/// Directories a walk never enters unless it starts inside one, as Nx's
/// walker; `.qk` holds qk's state outside Git.
const SKIPPED_DIRECTORIES: &[&str] = &[
    "node_modules",
    ".git",
    ".nx/cache",
    ".nx/workspace-data",
    ".yarn/cache",
    ".qk",
];

/// One group's resolved globs, workspace-relative, `!` for a negation.
pub(crate) struct Group {
    positives: Vec<Positive>,
    negations: Vec<Negation>,
    /// Project roots a positive's ownership is decided by: every root but
    /// the workspace root's own.
    roots: BTreeSet<String>,
}

/// A positive entry: the directory it is read from and the pattern below it.
struct Positive {
    root: String,
    remainder: Option<Pattern>,
    /// What every file it matches ends in, see [`extension`].
    extension: Option<String>,
}

/// A negation: what it excludes below its directory, or all of it.
struct Negation {
    root: String,
    remainder: Option<Pattern>,
    /// Whether it excludes the directory whole, so a walk need not enter it.
    whole: bool,
}

impl Group {
    /// The group of `globs` that `project` resolves, checked as Nx checks
    /// one before reading anything: no glob is absolute or has a `.` or
    /// `..` segment, a negation excludes something, and not all of them are
    /// negations.
    pub(crate) fn new<'a>(
        project: &str,
        globs: &[String],
        roots: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self> {
        if globs.iter().all(|glob| glob.starts_with('!')) {
            bail!(
                "the includeIgnored fileset {:?} applied to {project:?} is a negation with no positive includeIgnored fileset to filter",
                globs.first().map(String::as_str).unwrap_or_default()
            );
        }
        let mut positives = Vec::new();
        let mut negations = Vec::new();
        for glob in globs {
            let (negated, body) = match glob.strip_prefix('!') {
                Some(body) => (true, body),
                None => (false, glob.as_str()),
            };
            for entry in literal_braces(&normalize(body)) {
                validate(glob, &entry)?;
                let (root, remainder) = partition(&entry);
                let remainder = remainder
                    .map(|rest| Pattern::new(rest, false))
                    .transpose()?;
                if negated {
                    if root.is_empty() && remainder.is_none() {
                        bail!("the includeIgnored fileset {glob:?} names nothing to exclude");
                    }
                    let rest = partition(&entry).1;
                    negations.push(Negation {
                        whole: matches!(rest, None | Some("**" | "**/*")),
                        root,
                        remainder,
                    });
                } else {
                    positives.push(Positive {
                        root,
                        remainder,
                        extension: extension(&entry),
                    });
                }
            }
        }
        Ok(Self {
            positives,
            negations,
            roots: roots
                .into_iter()
                .filter(|root| !matches!(*root, "" | "."))
                .map(|root| root.trim_end_matches('/').to_owned())
                .collect(),
        })
    }

    /// Each positive's directory, and the extension every file it matches
    /// ends in, if one.
    pub(crate) fn reads(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.positives
            .iter()
            .map(|entry| (entry.root.as_str(), entry.extension.as_deref()))
    }

    /// The project roots a positive read from `root` cannot match, as Nx's
    /// `nested_below`: those below it, when a project holds it.
    pub(crate) fn skipped_below(&self, root: &str) -> Vec<&str> {
        if self.owner_of_directory(root).is_none() {
            return Vec::new();
        }
        self.roots
            .iter()
            .filter(|project| is_below(project, root))
            .map(String::as_str)
            .collect()
    }

    /// The files on disk the group selects under `workspace`, sorted.
    pub(crate) fn expand(&self, workspace: &Path) -> Result<BTreeSet<String>> {
        let mut found = BTreeSet::new();
        for entry in &self.positives {
            let start = workspace.join(&entry.root);
            let Ok(metadata) = std::fs::metadata(&start) else {
                continue;
            };
            if metadata.is_file() {
                // An exact path is read as written, whatever holds it.
                if entry.remainder.is_none() && !self.excluded(&entry.root) {
                    found.insert(entry.root.clone());
                }
                continue;
            }
            let mut skip: Vec<&str> = self.skipped_below(&entry.root);
            skip.extend(
                self.negations
                    .iter()
                    .filter(|negation| negation.whole && is_below(&negation.root, &entry.root))
                    .map(|negation| negation.root.as_str()),
            );
            for path in walk(workspace, &entry.root, &skip)? {
                if self.admits(entry, &path) {
                    found.insert(path);
                }
            }
        }
        Ok(found)
    }

    /// Whether the group selects `path`, for a path that need not exist,
    /// such as one a change set names.
    pub(crate) fn matches(&self, path: &str) -> bool {
        self.positives.iter().any(|entry| {
            if entry.remainder.is_none() && entry.root == path {
                return !self.excluded(path);
            }
            self.admits(entry, path)
        })
    }

    /// Whether `entry` matches `path` below its directory, `path` is not
    /// excluded, and `entry` may read it.
    fn admits(&self, entry: &Positive, path: &str) -> bool {
        let rest = if entry.root.is_empty() {
            Some(path)
        } else {
            path.strip_prefix(&entry.root)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        let matched = match (rest, &entry.remainder) {
            (None, _) => false,
            (Some(rest), Some(pattern)) => !rest.is_empty() && pattern.is_match(rest),
            (Some(_), None) => true,
        };
        matched && !self.excluded(path) && self.owns(entry, path)
    }

    fn excluded(&self, path: &str) -> bool {
        self.negations.iter().any(|negation| {
            let rest = if negation.root.is_empty() {
                Some(path)
            } else {
                path.strip_prefix(&negation.root)
                    .and_then(|rest| match rest {
                        "" => Some(rest),
                        rest => rest.strip_prefix('/'),
                    })
            };
            match (rest, &negation.remainder) {
                (None, _) => false,
                (Some(_), None) => true,
                (Some(rest), Some(pattern)) => pattern.is_match(rest),
            }
        })
    }

    /// Whether a positive read from `entry`'s directory may match `path`:
    /// read from inside a project, only that project's files.
    fn owns(&self, entry: &Positive, path: &str) -> bool {
        match self.owner_of_directory(&entry.root) {
            None => true,
            owner => parents(path).find(|parent| self.roots.contains(*parent)) == owner,
        }
    }

    /// The deepest project root that is `directory` or holds it.
    fn owner_of_directory<'a>(&self, directory: &'a str) -> Option<&'a str> {
        std::iter::once(directory)
            .chain(parents(directory))
            .find(|parent| self.roots.contains(*parent))
    }
}

/// `path`'s parent directories, nearest first, without the workspace root.
fn parents(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(path), |path| {
        path.rsplit_once('/').map(|(parent, _)| parent)
    })
    .skip(1)
}

/// Whether `path` lies strictly below `directory`; everything lies below the
/// workspace root, written empty.
fn is_below(path: &str, directory: &str) -> bool {
    if directory.is_empty() {
        return !path.is_empty();
    }
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Repeated and trailing slashes collapsed, as Nx's `normalize_glob`.
fn normalize(glob: &str) -> String {
    let mut result = String::new();
    for part in glob.split('/') {
        if part.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push('/');
        }
        result.push_str(part);
    }
    if glob.starts_with('/') {
        result.insert(0, '/');
    }
    result
}

/// Nx's `validate_shape`: a glob reads inside the workspace.
fn validate(glob: &str, entry: &str) -> Result<()> {
    if entry.starts_with('/') || Path::new(entry).is_absolute() {
        bail!(
            "the includeIgnored fileset {glob:?} is an absolute path; globs are workspace-relative"
        );
    }
    if entry.split('/').any(|part| matches!(part, "." | "..")) {
        bail!(
            "the includeIgnored fileset {glob:?} has a `.` or `..` segment; write it relative to the workspace root"
        );
    }
    Ok(())
}

/// A brace group of literal names expanded into each path it names, as
/// Nx's `expand_literal_braces`: `{nx,tsconfig.base}.json` is `nx.json` and
/// `tsconfig.base.json`. A group with a pattern in it stays a pattern.
fn literal_braces(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_owned()];
    };
    let Some(close) = glob[open..].find('}').map(|index| open + index) else {
        return vec![glob.to_owned()];
    };
    let alternatives: Vec<&str> = glob[open + 1..close].split(',').collect();
    let literal = alternatives.len() > 1
        && alternatives.iter().all(|alternative| {
            !alternative.is_empty()
                && !alternative.contains(['*', '?', '[', ']', '{', '}', '/', '\\'])
        });
    if !literal {
        return vec![glob.to_owned()];
    }
    alternatives
        .iter()
        .flat_map(|alternative| {
            literal_braces(&format!(
                "{}{alternative}{}",
                &glob[..open],
                &glob[close + 1..]
            ))
        })
        .collect()
}

/// A glob split, as Nx's `partition_glob`, into the directory it is read
/// from, its leading literal segments with their escapes read, and the
/// pattern below it, `None` when it is literal to its end.
pub(crate) fn partition(glob: &str) -> (String, Option<&str>) {
    let mut directory: Vec<String> = Vec::new();
    let mut consumed = 0;
    for segment in glob.split('/') {
        if !names_a_path(segment) || segment.contains('\\') && unescape(segment).is_none() {
            return (directory.join("/"), Some(&glob[consumed..]));
        }
        directory.push(unescape(segment).unwrap_or_else(|| segment.to_owned()));
        consumed += segment.len() + 1;
    }
    (directory.join("/"), None)
}

/// The extension every match of `glob` ends in, as Nx's
/// `extension_after_wildcard`: `js` for `dist/**/*.js`. None unless the last
/// segment is a pattern ending in a literal `.ext`, since a literal segment
/// may name a directory, such as `dist/lib.v2`.
pub(crate) fn extension(glob: &str) -> Option<String> {
    let last = glob.rsplit('/').next()?;
    if names_a_path(last) {
        return None;
    }
    let (_, extension) = last.rsplit_once('.')?;
    (!extension.is_empty() && names_a_path(extension) && !extension.contains(['@', '+']))
        .then(|| extension.to_owned())
}

/// A segment with its escapes read, or `None` when they would make it `.`
/// or `..`, which Nx reads as a pattern rather than a name.
fn unescape(segment: &str) -> Option<String> {
    let mut name = String::new();
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        name.push(if c == '\\' {
            chars.next().unwrap_or('\\')
        } else {
            c
        });
    }
    (segment == name || !matches!(name.as_str(), "." | "..")).then_some(name)
}

/// Every file below `directory`, workspace-relative, as Nx's walker lists
/// them: the directory itself is read wherever it points, a linked file
/// counts and a linked directory below it is not entered, nor are `skip`,
/// [`SKIPPED_DIRECTORIES`] below the start, or the files tools write and
/// remove moments later.
fn walk(workspace: &Path, directory: &str, skip: &[&str]) -> Result<Vec<String>> {
    let start = workspace.join(directory);
    let prefix = if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/")
    };
    let mut files = Vec::new();
    let mut walker = walkdir::WalkDir::new(&start)
        .follow_links(false)
        .follow_root_links(true)
        .into_iter();
    while let Some(entry) = walker.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error)
                if error
                    .io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if entry.depth() == 0 {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&start) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        let path = format!("{prefix}{}", relative.replace('\\', "/"));
        let file_type = entry.file_type();
        if file_type.is_dir() {
            let skipped = skip.contains(&path.as_str())
                || SKIPPED_DIRECTORIES
                    .iter()
                    .any(|name| path == *name || path.ends_with(&format!("/{name}")));
            if skipped {
                walker.skip_current_dir();
            }
            continue;
        }
        if file_type.is_symlink()
            && std::fs::metadata(entry.path()).is_ok_and(|target| target.is_dir())
        {
            continue;
        }
        if transient(entry.file_name().to_str().unwrap_or_default()) {
            continue;
        }
        files.push(path);
    }
    Ok(files)
}

/// Files Vite and Vitest write beside their config while loading it, and
/// pnpm's store probes, which Nx's walker leaves out.
fn transient(name: &str) -> bool {
    let timestamp = [
        "vitest.config.ts.timestamp",
        "vite.config.ts.timestamp",
        "vitest.config.mts.timestamp",
        "vite.config.mts.timestamp",
    ]
    .iter()
    .any(|config| name.starts_with(config) && name.ends_with(".mjs"));
    let probe = name
        .strip_prefix("_tmp_")
        .is_some_and(|rest| rest.contains('_'));
    timestamp || probe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(globs: &[&str], roots: &[&str]) -> Result<Group> {
        let globs: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
        Group::new("web", &globs, roots.iter().copied())
    }

    fn workspace(files: &[&str]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for file in files {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, *file).unwrap();
        }
        root
    }

    fn expand(root: &tempfile::TempDir, globs: &[&str], roots: &[&str]) -> Vec<String> {
        group(globs, roots)
            .unwrap()
            .expand(root.path())
            .unwrap()
            .into_iter()
            .collect()
    }

    const FILES: &[&str] = &[
        "dist/gen/a.js",
        "dist/gen/a.js.map",
        "dist/gen/nested/b.js",
        "dist/other/c.js",
        "dist/gen/node_modules/dep/index.js",
        "node_modules/foo/package.json",
    ];

    // Nx 24's `expands_from_the_partitioned_directory_and_applies_negations`
    // and its neighbours.
    #[test]
    fn negations_filter_every_positive_in_any_order() {
        let root = workspace(FILES);
        assert_eq!(
            expand(&root, &["!dist/gen/**/*.map", "dist/gen/**/*"], &[]),
            ["dist/gen/a.js", "dist/gen/nested/b.js"]
        );
        assert_eq!(
            expand(&root, &["dist/**", "!dist/gen/nested"], &[]),
            ["dist/gen/a.js", "dist/gen/a.js.map", "dist/other/c.js"]
        );
        assert_eq!(
            expand(
                &root,
                &["dist/gen/a.js", "dist/other/c.js", "!dist/gen/a.js"],
                &[]
            ),
            ["dist/other/c.js"]
        );
    }

    #[test]
    fn an_exact_path_names_a_file_or_everything_under_a_directory() {
        let root = workspace(FILES);
        assert_eq!(expand(&root, &["dist/other"], &[]), ["dist/other/c.js"]);
        assert_eq!(expand(&root, &["dist/gen/"], &[]).len(), 3);
        assert!(expand(&root, &["dist/missing.js"], &[]).is_empty());
        assert!(
            group(&["dist/missing.js"], &[])
                .unwrap()
                .matches("dist/missing.js")
        );
    }

    #[test]
    fn walks_skip_node_modules_unless_they_start_inside_it() {
        let root = workspace(FILES);
        assert!(
            !expand(&root, &["dist/**"], &[])
                .iter()
                .any(|file| file.contains("node_modules"))
        );
        assert_eq!(
            expand(&root, &["node_modules/foo/package.json"], &[]),
            ["node_modules/foo/package.json"]
        );
        assert_eq!(
            expand(&root, &["node_modules/**/package.json"], &[]),
            ["node_modules/foo/package.json"]
        );
    }

    #[test]
    fn a_positive_skips_projects_nested_below_where_it_reads() {
        let root = workspace(&[
            "packages/gradle/src/a.ts",
            "packages/gradle/build/out.json",
            "packages/gradle/runner/build.gradle.kts",
            "packages/gradle/runner/build/runner.jar",
        ]);
        let roots = ["packages/gradle", "packages/gradle/runner", "."];
        assert_eq!(
            expand(&root, &["packages/gradle/**/*"], &roots),
            ["packages/gradle/build/out.json", "packages/gradle/src/a.ts"]
        );
        assert_eq!(
            expand(&root, &["packages/gradle/runner/build/**"], &roots),
            ["packages/gradle/runner/build/runner.jar"]
        );
        // Read from outside every project, it matches what it names.
        assert_eq!(
            expand(&root, &["packages/**/*.jar"], &roots),
            ["packages/gradle/runner/build/runner.jar"]
        );
        let group = group(&["packages/gradle/**/*"], &roots).unwrap();
        assert!(!group.matches("packages/gradle/runner/build/runner.jar"));
        assert!(group.matches("packages/gradle/build/new.json"));
    }

    #[test]
    fn brackets_braces_and_escapes_read_as_any_fileset() {
        let root = workspace(&[
            "app/(marketing)/page.tsx",
            "app/marketing/page.tsx",
            "app/[id]/page.tsx",
            "app/i/page.tsx",
            "nx.json",
            "tsconfig.base.json",
        ]);
        assert_eq!(
            expand(&root, &["app/[id]/page.tsx"], &[]),
            ["app/i/page.tsx"]
        );
        assert_eq!(
            expand(&root, &[r"app/\[id\]/page.tsx"], &[]),
            ["app/[id]/page.tsx"]
        );
        assert_eq!(
            expand(&root, &[r"app/\(marketing\)"], &[]),
            ["app/(marketing)/page.tsx"]
        );
        assert_eq!(
            expand(&root, &["{nx,tsconfig.base,missing}.json"], &[]),
            ["nx.json", "tsconfig.base.json"]
        );
    }

    #[test]
    fn rejects_what_nx_rejects() {
        for globs in [
            &["!dist/**/*.map"][..],
            &["dist/**", "!"],
            &["../outside/**"],
            &["dist/./gen/**"],
            &["/etc/passwd"],
        ] {
            assert!(group(globs, &[]).is_err(), "{globs:?}");
        }
        assert!(group(&["dist//gen/", "!dist//gen//**/*.map"], &[]).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_read_where_they_point_but_not_walked_into() {
        let root = workspace(FILES);
        let elsewhere = workspace(&["linked.js", "tree/deep.js"]);
        std::os::unix::fs::symlink(
            elsewhere.path().join("linked.js"),
            root.path().join("dist/gen/escape.js"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("tree"),
            root.path().join("dist/gen/tree"),
        )
        .unwrap();
        assert_eq!(
            expand(&root, &["dist/gen/*.js"], &[]),
            ["dist/gen/a.js", "dist/gen/escape.js"]
        );
        assert_eq!(
            expand(&root, &["dist/gen/tree"], &[]),
            ["dist/gen/tree/deep.js"]
        );
        assert!(
            !expand(&root, &["dist/**"], &[])
                .iter()
                .any(|file| file.contains("tree/"))
        );
    }
}
