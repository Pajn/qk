//! Nx-compatible glob matching.
//!
//! Nx does not match extended globs directly. Patterns containing `|`, `(`, `{,`
//! or a leading `!` are first expanded into plain globs, and that expansion is
//! deliberately approximate: `+(a|b)` matches exactly one of `a` or `b`, `?(x)` and
//! `*(x)` mean "absent or once", an absent group in a directory segment widens that
//! segment to `*`, and a `?`, `+` or `@` not followed by `(` is dropped. The cache
//! mirrors the expansion of Nx 23 (`packages/nx/src/native/glob/glob_transform.rs`)
//! so input sets written for Nx select the same files. Inner negation such as
//! `!(a|b)` is rejected instead of approximated.

use anyhow::{Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

#[derive(Clone)]
pub struct Pattern {
    set: GlobSet,
}

impl Pattern {
    /// Compiles a glob without its leading `!`; `negated` says whether one was there,
    /// because Nx expands every negated pattern.
    pub fn new(pattern: &str, negated: bool) -> Result<Self> {
        let globs = if negated
            || ["|", "(", "{,", "!"]
                .iter()
                .any(|token| pattern.contains(token))
        {
            expand(pattern)?
        } else {
            vec![pattern.to_owned()]
        };
        let mut set = GlobSetBuilder::new();
        for glob in globs {
            let glob = if glob.ends_with('/') {
                format!("{glob}**")
            } else {
                glob
            };
            set.add(
                GlobBuilder::new(&glob)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()?,
            );
        }
        Ok(Self { set: set.build()? })
    }

    pub fn is_match(&self, path: impl AsRef<std::path::Path>) -> bool {
        self.set.is_match(path)
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
    Literal(String),
}

/// Expands one glob into plain globs, sorted and deduplicated like Nx.
fn expand(pattern: &str) -> Result<Vec<String>> {
    if pattern.contains('!') {
        bail!("negation inside a glob is not supported by the cache yet: {pattern:?}");
    }
    let mut globs = vec![String::new()];
    let segments: Vec<_> = pattern.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        let groups = parse_segment(segment)
            .map_err(|error| anyhow::anyhow!("{error} in glob {pattern:?}"))?;
        let options = build_segment("", &groups, index == segments.len() - 1);
        globs = globs
            .iter()
            .flat_map(|prefix| {
                options.iter().map(move |option| {
                    if index == 0 {
                        option.clone()
                    } else {
                        format!("{prefix}/{option}")
                    }
                })
            })
            .collect();
    }
    globs.sort();
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

fn build_segment(existing: &str, groups: &[Group], is_last: bool) -> Vec<String> {
    let Some((group, rest)) = groups.split_first() else {
        return vec![existing.to_owned()];
    };
    match group {
        Group::Optional(items) => {
            // Nx widens an omitted group in a directory segment to the whole segment.
            let omitted = if is_last { existing } else { "*" };
            let mut options = build_segment(omitted, rest, is_last);
            options.extend(build_segment(
                &format!("{existing}{}", display(items)),
                rest,
                is_last,
            ));
            options
        }
        Group::Once(items) => {
            build_segment(&format!("{existing}{}", display(items)), rest, is_last)
        }
        Group::Literal(text) => build_segment(&format!("{existing}{text}"), rest, is_last),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected expansions are Nx 23.1's own glob_transform tests.
    #[test]
    fn expands_like_nx() {
        assert_eq!(
            expand("libs/**/?(*.)+spec.ts?(.snap)").unwrap(),
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
            assert_eq!(
                expand(pattern).unwrap(),
                expand("libs/**/?(*.)+spec.ts?(.snap)").unwrap()
            );
        }
        assert_eq!(expand("dist/**/*.js").unwrap(), ["dist/**/*.js"]);
        assert_eq!(expand("**/*.(js|ts)").unwrap(), ["**/*.{js,ts}"]);
        assert_eq!(
            expand("**/*.spec.ts{,.snap}").unwrap(),
            ["**/*.spec.ts", "**/*.spec.ts.snap"]
        );
    }

    #[test]
    fn expands_workspace_patterns() {
        assert_eq!(
            expand("app/**/?(*.)+(spec|test).[jt]s?(x)?(.snap)").unwrap(),
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
        assert_eq!(
            expand("biome.json?(c)").unwrap(),
            ["biome.json", "biome.jsonc"]
        );
        assert_eq!(
            expand("app/(.cache|dist)/**/*").unwrap(),
            ["app/{.cache,dist}/**/*"]
        );
        // An omitted group in a directory segment widens it, as in Nx.
        assert_eq!(expand("a?(b)/c").unwrap(), ["*/c", "ab/c"]);
    }

    #[test]
    fn rejects_what_nx_rejects_or_negates() {
        assert!(expand("!(test)/*.ts").is_err());
        assert!(expand("src/a!b").is_err());
        assert!(expand("file?").is_err());
        assert!(expand("src/(unterminated").is_err());
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
}
