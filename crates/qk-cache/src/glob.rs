//! Nx-compatible glob matching.
//!
//! Nx does not match extended globs directly. Patterns containing `|`, `(`, `{,`
//! or a leading `!` are first expanded into plain globs, and that expansion is
//! deliberately approximate: `+(a|b)` matches exactly one of `a` or `b`, `?(x)` and
//! `*(x)` mean "absent or once", an absent group in a directory segment widens that
//! segment to `*`, and a `?`, `+` or `@` not followed by `(` is dropped. The cache
//! mirrors the expansion of Nx 23 (`packages/nx/src/native/glob/glob_transform.rs`)
//! so input sets written for Nx select the same files.
//!
//! A negated group such as `!(a|b)` expands, as in Nx, into a glob with the
//! group widened and one with the group's items that excludes: a path matches
//! when an including glob does and no excluding one does. In a directory
//! segment the exclusion covers that whole directory. `!(a).` and `!(a)*`
//! widen to `*.` and `*`. Nx applies those exclusions to every pattern of a
//! project together; qk applies them to the pattern they come from, which can
//! only add inputs.

use anyhow::{Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

#[derive(Clone)]
pub struct Pattern {
    included: GlobSet,
    excluded: GlobSet,
}

impl Pattern {
    /// Compiles a glob without its leading `!`; `negated` says whether one was
    /// there. Nx expands every negated pattern, and every glob it expands to
    /// then excludes, so a negated pattern matches what any of them matches.
    pub fn new(pattern: &str, negated: bool) -> Result<Self> {
        let globs = if negated
            || ["|", "(", "{,", "!"]
                .iter()
                .any(|token| pattern.contains(token))
        {
            expand(pattern)?
        } else {
            vec![(pattern.to_owned(), false)]
        };
        let mut included = GlobSetBuilder::new();
        let mut excluded = GlobSetBuilder::new();
        for (glob, excludes) in globs {
            let glob = if glob.ends_with('/') {
                format!("{glob}**")
            } else {
                glob
            };
            let glob = GlobBuilder::new(&glob)
                .literal_separator(true)
                .backslash_escape(false)
                .build()?;
            if excludes && !negated {
                excluded.add(glob);
            } else {
                included.add(glob);
            }
        }
        Ok(Self {
            included: included.build()?,
            excluded: excluded.build()?,
        })
    }

    /// As Nx's glob sets: an including glob matches and no excluding one, or,
    /// with only excluding globs, none of them.
    pub fn is_match(&self, path: impl AsRef<std::path::Path>) -> bool {
        let path = path.as_ref();
        let excluded = self.excluded.is_match(path);
        if self.included.is_empty() {
            !excluded
        } else {
            !excluded && self.included.is_match(path)
        }
    }
}

/// Whether a path segment needs matching rather than being a literal directory.
pub fn is_literal(segment: &str) -> bool {
    !segment.contains([
        '!', '?', '@', '+', '*', '|', ',', '{', '}', '[', ']', '(', ')',
    ])
}

#[derive(Debug, PartialEq)]
enum Group {
    /// `?(a|b)`, `*(a|b)` and `{,a}`: expanded as absent or once.
    Optional(String),
    /// `+(a|b)`, `@(a|b)` and `(a|b)`: expanded as exactly once.
    Once(String),
    /// `!(a|b)`.
    Negated(String),
    /// `!(a|b).`, which widens to `*.`.
    NegatedFileName(String),
    /// `!(a|b)*`, which widens to `*`.
    NegatedWildcard(String),
    Literal(String),
}

/// Expands one glob into plain globs, each with whether it excludes, sorted
/// and deduplicated like Nx's `convert_glob`. An excluding glob from a
/// directory segment ends there, with a `/`, and covers that directory.
fn expand(pattern: &str) -> Result<Vec<(String, bool)>> {
    let segments: Vec<_> = pattern.split('/').collect();
    let mut built = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let groups = parse_segment(segment)
            .map_err(|error| anyhow::anyhow!("{error} in glob {pattern:?}"))?;
        built.push(build_segment(
            "",
            &groups,
            index == segments.len() - 1,
            false,
        ));
    }
    // Every combination of one option per segment, as Nx's cartesian product.
    let mut products: Vec<Vec<&(String, bool)>> = vec![Vec::new()];
    for options in &built {
        products = products
            .iter()
            .flat_map(|product| {
                options.iter().map(move |option| {
                    let mut next = product.clone();
                    next.push(option);
                    next
                })
            })
            .collect();
    }
    let mut globs: Vec<(String, bool)> = products
        .into_iter()
        .map(|product| {
            let mut path = String::new();
            let mut excludes = false;
            let mut whole = false;
            for (index, (segment, negative)) in product.iter().enumerate() {
                whole = index == product.len() - 1;
                path.push_str(segment);
                path.push('/');
                if *negative {
                    excludes = true;
                    if !whole {
                        break;
                    }
                }
            }
            if whole {
                path.pop();
            }
            (path, excludes)
        })
        .collect();
    globs.sort_by(|a, b| (!a.1, &a.0).cmp(&(!b.1, &b.0)));
    globs.dedup();
    Ok(globs)
}

fn parse_segment(segment: &str) -> Result<Vec<Group>> {
    let mut groups = Vec::new();
    let mut rest = segment;
    while !rest.is_empty() {
        // Nx drops `?`, `+` and `@` when no group follows, e.g. `+spec` means `spec`.
        if let Some(after) = rest.strip_prefix(['?', '+', '@'])
            && !after.is_empty()
            && !after.starts_with('(')
        {
            rest = after;
        }
        let parsed = [
            ("(", ")", false),
            ("*(", ")", true),
            ("?(", ")", true),
            ("+(", ")", false),
            ("@(", ")", false),
            ("{,", "}", true),
        ]
        .into_iter()
        .find_map(|(open, close, optional)| {
            let body = rest.strip_prefix(open)?;
            let end = body.find(close)?;
            let items = body[..end].split(['|', ',']).collect::<Vec<_>>().join(",");
            let group = if optional {
                Group::Optional(items)
            } else {
                Group::Once(items)
            };
            Some((group, &body[end + close.len()..]))
        })
        // Nx tries `!(…).`, then `!(…)*`, then `!(…)`.
        .or_else(|| {
            let body = rest.strip_prefix("!(")?;
            let end = body.find(')')?;
            let items = body[..end].split(['|', ',']).collect::<Vec<_>>().join(",");
            let after = &body[end + 1..];
            Some(if let Some(after) = after.strip_prefix('.') {
                (Group::NegatedFileName(items), after)
            } else if let Some(after) = after.strip_prefix('*') {
                (Group::NegatedWildcard(items), after)
            } else {
                (Group::Negated(items), after)
            })
        });
        if let Some((group, remaining)) = parsed {
            groups.push(group);
            rest = remaining;
            continue;
        }
        // A literal runs to the next `{,` or, without one, to the next special character.
        let end = rest
            .find("{,")
            .unwrap_or_else(|| rest.find(['?', '+', '@', '!', '(']).unwrap_or(rest.len()));
        if end == 0 {
            bail!("unsupported glob syntax at {rest:?}");
        }
        groups.push(Group::Literal(rest[..end].to_owned()));
        rest = &rest[end..];
    }
    Ok(groups)
}

fn display(items: &str) -> String {
    if items.contains(',') {
        format!("{{{items}}}")
    } else {
        items.to_owned()
    }
}

/// A segment's options, each with whether a negated group made it one that
/// excludes: Nx's `build_segment`.
fn build_segment(
    existing: &str,
    groups: &[Group],
    is_last: bool,
    negative: bool,
) -> Vec<(String, bool)> {
    let Some((group, rest)) = groups.split_first() else {
        return vec![(existing.to_owned(), negative)];
    };
    // Nx widens an omitted group in a directory segment to the whole segment.
    let omitted = if is_last { existing } else { "*" };
    let both = |off: &str, on: &str, on_negative: bool| {
        let mut options = build_segment(off, rest, is_last, negative);
        options.extend(build_segment(on, rest, is_last, on_negative));
        options
    };
    match group {
        Group::Optional(items) => both(omitted, &format!("{existing}{}", display(items)), negative),
        Group::Negated(items) => both(omitted, &format!("{existing}{}", display(items)), true),
        Group::NegatedFileName(items) => {
            both("*.", &format!("{existing}{}.", display(items)), true)
        }
        Group::NegatedWildcard(items) => both("*", &format!("{existing}{}*", display(items)), true),
        Group::Once(items) => build_segment(
            &format!("{existing}{}", display(items)),
            rest,
            is_last,
            negative,
        ),
        Group::Literal(text) => {
            build_segment(&format!("{existing}{text}"), rest, is_last, negative)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expansion as Nx's `convert_glob` writes it, `!` for excluding.
    fn nx(pattern: &str) -> Vec<String> {
        expand(pattern)
            .unwrap()
            .into_iter()
            .map(|(glob, excludes)| if excludes { format!("!{glob}") } else { glob })
            .collect()
    }

    // Expected expansions are Nx 23.1's own glob_transform tests.
    #[test]
    fn expands_like_nx() {
        assert_eq!(
            nx("libs/**/?(*.)+spec.ts?(.snap)"),
            [
                "libs/**/*.spec.ts",
                "libs/**/*.spec.ts.snap",
                "libs/**/spec.ts",
                "libs/**/spec.ts.snap"
            ]
        );
        for pattern in [
            "libs/**/?(*.)@spec.ts?(.snap)",
            "libs/**/?(*.)?spec.ts?(.snap)",
        ] {
            assert_eq!(nx(pattern), nx("libs/**/?(*.)+spec.ts?(.snap)"));
        }
        assert_eq!(nx("dist/**/*.js"), ["dist/**/*.js"]);
        assert_eq!(nx("**/*.(js|ts)"), ["**/*.{js,ts}"]);
        assert_eq!(
            nx("**/*.spec.ts{,.snap}"),
            ["**/*.spec.ts", "**/*.spec.ts.snap"]
        );
    }

    #[test]
    fn expands_workspace_patterns() {
        assert_eq!(
            nx("app/**/?(*.)+(spec|test).[jt]s?(x)?(.snap)"),
            [
                "app/**/*.{spec,test}.[jt]s",
                "app/**/*.{spec,test}.[jt]s.snap",
                "app/**/*.{spec,test}.[jt]sx",
                "app/**/*.{spec,test}.[jt]sx.snap",
                "app/**/{spec,test}.[jt]s",
                "app/**/{spec,test}.[jt]s.snap",
                "app/**/{spec,test}.[jt]sx",
                "app/**/{spec,test}.[jt]sx.snap",
            ]
        );
        assert_eq!(nx("biome.json?(c)"), ["biome.json", "biome.jsonc"]);
        assert_eq!(nx("app/(.cache|dist)/**/*"), ["app/{.cache,dist}/**/*"]);
        // An omitted group in a directory segment widens it, as in Nx.
        assert_eq!(nx("a?(b)/c"), ["*/c", "ab/c"]);
    }

    #[test]
    fn rejects_what_nx_rejects() {
        assert!(expand("src/a!b").is_err());
        assert!(expand("file?").is_err());
        assert!(expand("src/(unterminated").is_err());
    }

    // Nx 23.1's own convert_glob tests.
    #[test]
    fn expands_negated_groups_like_nx() {
        assert_eq!(
            nx("dist/!(cache|cache2)/**/!(README|LICENSE).(js|ts)"),
            [
                "!dist/*/**/{README,LICENSE}.{js,ts}",
                "!dist/{cache,cache2}/",
                "dist/*/**/*.{js,ts}",
            ]
        );
        assert_eq!(
            nx("dist/**/!(README|LICENSE).(js|ts)"),
            ["!dist/**/{README,LICENSE}.{js,ts}", "dist/**/*.{js,ts}"]
        );
        assert_eq!(
            nx("dist/!(cache|cache2)/**/*.(js|ts)"),
            ["!dist/{cache,cache2}/", "dist/*/**/*.{js,ts}"]
        );
        assert_eq!(
            nx("dist/!(cache|cache2)/**/*.js"),
            ["!dist/{cache,cache2}/", "dist/*/**/*.js"]
        );
        assert_eq!(
            nx("packages/!(package-a)*"),
            ["!packages/package-a*", "packages/*"]
        );
        assert_eq!(
            nx("packages/!(package-a)*/package.json"),
            ["!packages/package-a*/", "packages/*/package.json"]
        );
        assert_eq!(nx("**/!(package-a)*"), ["!**/package-a*", "**/*"]);
        assert_eq!(
            nx("!(test|e2e)/?(*.)+(spec|test).[jt]s!(x)?(.snap)"),
            [
                "!*/*.{spec,test}.[jt]sx",
                "!*/*.{spec,test}.[jt]sx.snap",
                "!*/{spec,test}.[jt]sx",
                "!*/{spec,test}.[jt]sx.snap",
                "!{test,e2e}/",
                "*/*.{spec,test}.[jt]s",
                "*/*.{spec,test}.[jt]s.snap",
                "*/{spec,test}.[jt]s",
                "*/{spec,test}.[jt]s.snap",
            ]
        );
    }

    #[test]
    fn negated_groups_exclude_what_they_name() {
        let pattern = Pattern::new("src/**/!(*.test|*.spec).ts", false).unwrap();
        assert!(pattern.is_match("src/a/x.ts"));
        assert!(!pattern.is_match("src/a/x.test.ts") && !pattern.is_match("src/a/x.spec.ts"));
        let directories = Pattern::new("src/!(a|b)/*", false).unwrap();
        assert!(directories.is_match("src/c/z.md"));
        assert!(!directories.is_match("src/a/x.ts"));
        // As in Nx, a negated group after a literal in its segment leaves
        // only files ending in that literal.
        let odd = Pattern::new("src/**/*.!(ts)", false).unwrap();
        assert!(!odd.is_match("src/b/y.js") && !odd.is_match("src/b/y.ts"));
        // Negated as a whole, every expansion excludes.
        let whole = Pattern::new("src/**/!(*.test).ts", true).unwrap();
        assert!(whole.is_match("src/a/x.ts") && whole.is_match("src/a/x.test.ts"));
    }

    #[test]
    fn matches_expanded_patterns() {
        let tests = Pattern::new("app/**/?(*.)+(spec|test).[jt]s?(x)?(.snap)", true).unwrap();
        for path in [
            "app/a.spec.ts",
            "app/deep/b.test.jsx",
            "app/test.ts",
            "app/c.spec.tsx.snap",
        ] {
            assert!(tests.is_match(path), "{path}");
        }
        for path in ["app/a.ts", "app/a.spec.mts", "other/a.spec.ts"] {
            assert!(!tests.is_match(path), "{path}");
        }
        let plain = Pattern::new("src/*.ts", false).unwrap();
        assert!(plain.is_match("src/a.ts") && !plain.is_match("src/a/b.ts"));
    }

    // What Nx 23.1's own glob sets select from the same files.
    #[test]
    fn selects_the_files_nx_selects() {
        let files = [
            "src/a/x.ts",
            "src/a/x.test.ts",
            "src/a/x.spec.ts",
            "src/b/y.ts",
            "src/b/y.js",
            "src/c/deep/z.ts",
            "src/c/z.md",
            "src/root.ts",
            "src/README.md",
        ];
        let selected = |pattern: &str| -> Vec<&str> {
            let pattern = Pattern::new(pattern, false).unwrap();
            files
                .iter()
                .copied()
                .filter(|file| pattern.is_match(file))
                .collect()
        };
        assert_eq!(
            selected("src/**/!(*.test).ts"),
            [
                "src/a/x.ts",
                "src/a/x.spec.ts",
                "src/b/y.ts",
                "src/c/deep/z.ts",
                "src/root.ts"
            ]
        );
        assert_eq!(
            selected("src/!(a)/**"),
            ["src/b/y.ts", "src/b/y.js", "src/c/deep/z.ts", "src/c/z.md"]
        );
        assert_eq!(selected("src/**/*.!(ts)"), [] as [&str; 0]);
        assert_eq!(selected("src/!(a|b)/*"), ["src/c/z.md"]);
        assert_eq!(
            selected("src/*/!(x)*"),
            ["src/b/y.ts", "src/b/y.js", "src/c/z.md"]
        );
        assert_eq!(
            selected("src/**/!(*.test|*.spec).ts"),
            ["src/a/x.ts", "src/b/y.ts", "src/c/deep/z.ts", "src/root.ts"]
        );
    }
}
