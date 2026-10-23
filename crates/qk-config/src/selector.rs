//! Project patterns, as Nx's `findMatchingProjects` reads them.
//!
//! A pattern is a project name, a glob over names, or labelled: `name:`,
//! `tag:` or `directory:` (the project root). Patterns apply in order, each
//! adding what it matches or, with a leading `!`, removing it; a first
//! pattern that excludes starts from every project. An unlabelled pattern
//! is tried as a name, then as a directory when no name matched. A name
//! that is neither a project nor a glob matches the projects containing it
//! as a word, ignoring case: `foo` selects `foo_bar` and `@scope/foo` but
//! not `foo-e2e`. Globs are [minimatch](crate::minimatch) globs with `dot`
//! set. Empty patterns and `nx-cloud:` references to other workspaces are
//! skipped.

use std::collections::BTreeMap;

use crate::Project;
use crate::minimatch::Minimatch;

/// What a pattern can see of a project.
pub trait Matchable {
    fn root(&self) -> &str;
    fn tags(&self) -> &[String];
}

impl Matchable for Project {
    fn root(&self) -> &str {
        &self.root
    }

    fn tags(&self) -> &[String] {
        &self.tags
    }
}

impl<P: Matchable> Matchable for &P {
    fn root(&self) -> &str {
        (*self).root()
    }

    fn tags(&self) -> &[String] {
        (*self).tags()
    }
}

/// How the patterns are read. Nx has two readers: its JavaScript one for
/// almost everything, and its native one for the `projects` of task inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reader {
    /// `findMatchingProjects` in `utils/find-matching-projects.ts`.
    Nx,
    /// `find_matching_projects` in Nx's native hasher: no starting set for a
    /// first exclusion, no word matching of plain names, and no skipped
    /// patterns.
    Native,
}

/// Nx's `isGlobPattern`.
pub fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains(['*', '|', '{', '}', '(', ')', '['])
}

#[derive(Clone, Copy)]
enum Kind {
    Name,
    Tag,
    Directory,
    Unlabeled,
}

/// The projects `patterns` select, in the order Nx's set holds them.
pub fn find_matching_projects<P: Matchable>(
    patterns: &[impl AsRef<str>],
    projects: &BTreeMap<String, P>,
    reader: Reader,
) -> Vec<String> {
    let patterns: Vec<&str> = patterns.iter().map(AsRef::as_ref).collect();
    if reader == Reader::Nx && patterns.iter().all(|pattern| pattern.is_empty()) {
        return Vec::new();
    }
    let mut matched = Selection::default();
    if reader == Reader::Nx && patterns.first().is_some_and(|first| first.starts_with('!')) {
        projects.keys().for_each(|name| matched.add(name));
    }
    for pattern in patterns {
        if reader == Reader::Nx && (pattern.is_empty() || pattern.starts_with("nx-cloud:")) {
            continue;
        }
        let (exclude, pattern) = match pattern.strip_prefix('!') {
            Some(pattern) => (true, pattern),
            None => (false, pattern),
        };
        let (kind, value) = if projects.contains_key(pattern) {
            (Kind::Name, pattern)
        } else {
            match pattern.split_once(':') {
                Some(("name", value)) => (Kind::Name, value),
                Some(("tag", value)) => (Kind::Tag, value),
                Some(("directory", value)) => (Kind::Directory, value),
                // qk reads a pattern with an unknown label whole, as Nx
                // 24.0.1 does, so `scope:shared` does not select a project
                // named `shared`.
                _ => (Kind::Unlabeled, pattern),
            }
        };
        let selected = |kind: Kind| -> Vec<&str> {
            // Nx takes `tag:*` and `directory:*` for every project too; qk
            // matches them as globs, so `tag:*` leaves out untagged projects.
            if value == "*" && matches!(kind, Kind::Name | Kind::Unlabeled) {
                return projects.keys().map(String::as_str).collect();
            }
            let glob = Minimatch::new(value, true);
            let matches = |item: &str| item == value || glob.is_match(item);
            match kind {
                Kind::Name | Kind::Unlabeled => {
                    if let Some((name, _)) = projects.get_key_value(value) {
                        vec![name.as_str()]
                    } else if reader == Reader::Nx && !is_glob_pattern(value) {
                        projects
                            .keys()
                            .filter(|name| contains_word(name, value))
                            .map(String::as_str)
                            .collect()
                    } else {
                        projects
                            .keys()
                            .filter(|name| matches(name))
                            .map(String::as_str)
                            .collect()
                    }
                }
                Kind::Directory => projects
                    .iter()
                    .filter(|(_, project)| matches(project.root()))
                    .map(|(name, _)| name.as_str())
                    .collect(),
                Kind::Tag => projects
                    .iter()
                    .filter(|(_, project)| {
                        let tags = project.tags();
                        tags.iter().any(|tag| tag == value)
                            || ((reader == Reader::Native || is_glob_pattern(value))
                                && tags.iter().any(|tag| matches(tag)))
                    })
                    .map(|(name, _)| name.as_str())
                    .collect(),
            }
        };
        let apply = |matched: &mut Selection, names: Vec<&str>| {
            for name in names {
                if exclude {
                    matched.remove(name);
                } else {
                    matched.add(name);
                }
            }
        };
        if let Kind::Unlabeled = kind {
            // A change in the selection's size is a match by name; only
            // without one is the pattern tried as a directory.
            let before = matched.len();
            apply(&mut matched, selected(Kind::Name));
            if matched.len() == before {
                apply(&mut matched, selected(Kind::Directory));
            }
        } else {
            apply(&mut matched, selected(kind));
        }
    }
    matched.names
}

/// Whether `name` holds `word` with no letter, digit, `-` or `@` on either
/// side, ignoring case: Nx's ``(?<![@a-zA-Z0-9-])${word}(?![@a-zA-Z0-9-])``.
/// As in Nx 24.0.1, the pattern's characters match as they are.
fn contains_word(name: &str, word: &str) -> bool {
    let joins = |char: Option<char>| {
        char.is_some_and(|char| char.is_ascii_alphanumeric() || char == '-' || char == '@')
    };
    let name: Vec<char> = name.chars().collect();
    let word: Vec<char> = word.chars().collect();
    if word.len() > name.len() {
        return false;
    }
    (0..=name.len() - word.len()).any(|start| {
        let end = start + word.len();
        name[start..end]
            .iter()
            .zip(&word)
            .all(|(a, b)| a == b || a.to_lowercase().eq(b.to_lowercase()))
            && !joins(start.checked_sub(1).map(|before| name[before]))
            && !joins(name.get(end).copied())
    })
}

/// An insertion-ordered set, as a JavaScript `Set`.
#[derive(Default)]
struct Selection {
    names: Vec<String>,
}

impl Selection {
    fn add(&mut self, name: &str) {
        if !self.names.iter().any(|existing| existing == name) {
            self.names.push(name.to_owned());
        }
    }

    fn remove(&mut self, name: &str) {
        self.names.retain(|existing| existing != name);
    }

    fn len(&self) -> usize {
        self.names.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Node {
        root: String,
        tags: Vec<String>,
    }

    impl Matchable for Node {
        fn root(&self) -> &str {
            &self.root
        }
        fn tags(&self) -> &[String] {
            &self.tags
        }
    }

    fn projects() -> BTreeMap<String, Node> {
        [
            ("app", "apps/app", &["type:app"][..]),
            ("app-e2e", "apps/app-e2e", &["type:e2e"][..]),
            ("foo_bar", "libs/foo_bar", &[][..]),
            ("@scope/foo", "libs/foo", &["scope:shared"][..]),
            ("shared", "libs/shared", &[][..]),
            ("root", ".", &[][..]),
        ]
        .into_iter()
        .map(|(name, root, tags)| {
            (
                name.to_owned(),
                Node {
                    root: root.to_owned(),
                    tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                },
            )
        })
        .collect()
    }

    fn find(patterns: &[&str]) -> Vec<String> {
        find_matching_projects(patterns, &projects(), Reader::Nx)
    }

    #[test]
    fn labels_select_by_name_tag_and_directory() {
        assert_eq!(find(&["tag:type:*"]), ["app", "app-e2e"]);
        assert_eq!(find(&["tag:type:app"]), ["app"]);
        assert_eq!(
            find(&["directory:libs/*"]),
            ["@scope/foo", "foo_bar", "shared"]
        );
        assert_eq!(find(&["name:app"]), ["app"]);
        // An unlabelled pattern falls back to the project root.
        assert_eq!(find(&["apps/*"]), ["app", "app-e2e"]);
        assert_eq!(find(&["libs/shared"]), ["shared"]);
        // An unknown label is part of the pattern.
        assert!(find(&["scope:shared"]).is_empty());
        assert!(!find(&["tag:*"]).contains(&"root".to_owned()));
    }

    #[test]
    fn plain_names_match_as_words_ignoring_case() {
        assert_eq!(find(&["FOO"]), ["@scope/foo", "foo_bar"]);
        assert_eq!(find(&["app"]), ["app"]);
        // `-` joins words, so `e2e` is not a word of `app-e2e`.
        assert!(find(&["e2e"]).is_empty());
        assert!(find(&["fo"]).is_empty());
        assert!(find(&["fo."]).is_empty());
        // The native reader matches only exactly.
        assert!(find_matching_projects(&["FOO"], &projects(), Reader::Native).is_empty());
    }

    #[test]
    fn patterns_apply_in_order() {
        assert_eq!(find(&["!tag:type:*", "app"]).len(), 5);
        assert!(
            find(&["!tag:type:*"])
                .iter()
                .all(|name| !name.starts_with("app"))
        );
        assert_eq!(find(&["app*", "!app-e2e", "app-e2e"]), ["app", "app-e2e"]);
        assert_eq!(find(&["app*", "!app-e2e"]), ["app"]);
        assert!(find_matching_projects(&["!app"], &projects(), Reader::Native).is_empty());
        assert!(find(&["*"]).contains(&"root".to_owned()));
        assert!(!find(&["@scope*"]).contains(&"@scope/foo".to_owned()));
        assert_eq!(find(&["@scope/*"]), ["@scope/foo"]);
    }

    #[test]
    fn empty_and_cloud_patterns_are_skipped() {
        assert!(find(&[""]).is_empty());
        assert!(find(&[]).is_empty());
        assert_eq!(find(&["", "nx-cloud:other", "app"]), ["app"]);
    }
}
