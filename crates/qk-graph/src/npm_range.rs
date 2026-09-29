//! npm's semver ranges, as Nx checks a declared dependency against a
//! workspace package's version: `semver.satisfies(version, range,
//! { includePrerelease: true })`. Ranges desugar into comparators as npm's
//! `semver` does (`^`, `~`, x-ranges, hyphen ranges and `||`); an invalid
//! range or version satisfies nothing.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Alphanumeric(String),
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
            (Self::Numeric(_), Self::Alphanumeric(_)) => Ordering::Less,
            (Self::Alphanumeric(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Alphanumeric(a), Self::Alphanumeric(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Identifier>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A prerelease precedes its release.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn number(text: &str) -> Option<u64> {
    // As npm: digits only, and no leading zero.
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

fn prerelease(text: &str) -> Option<Vec<Identifier>> {
    text.split('.')
        .map(|part| {
            if part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                None
            } else if part.bytes().all(|byte| byte.is_ascii_digit()) {
                number(part).map(Identifier::Numeric)
            } else {
                Some(Identifier::Alphanumeric(part.to_owned()))
            }
        })
        .collect()
}

/// A version or a partial one: each of the three numbers may be missing or a
/// wildcard (`x`, `X`, `*`), with a prerelease only after all three.
#[derive(Debug)]
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<Identifier>,
}

impl Partial {
    fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (main, pre) = match text.split_once('-') {
            Some((main, pre)) => (main, Some(pre)),
            None => (text, None),
        };
        let mut parts = main.split('.');
        let mut part = || -> Option<Option<u64>> {
            match parts.next() {
                None => Some(None),
                Some("x" | "X" | "*") => Some(None),
                Some(text) => number(text).map(Some),
            }
        };
        let (major, minor, patch) = (part()?, part()?, part()?);
        if parts.next().is_some() {
            return None;
        }
        // A wildcard is followed only by wildcards.
        if (major.is_none() && (minor.is_some() || patch.is_some()))
            || (minor.is_none() && patch.is_some())
        {
            return None;
        }
        let pre = match pre {
            Some(pre) if patch.is_some() => prerelease(pre)?,
            Some(_) => return None,
            None => Vec::new(),
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    fn version(&self) -> Option<Version> {
        Some(Version {
            major: self.major?,
            minor: self.minor?,
            patch: self.patch?,
            pre: self.pre.clone(),
        })
    }
}

fn version(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
        pre: Vec::new(),
    }
}

/// `x.y.z-0`, the lowest version with those numbers.
fn lowest(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        pre: vec![Identifier::Numeric(0)],
        ..version(major, minor, patch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

type Comparator = (Operator, Version);

fn test(comparators: &[Comparator], candidate: &Version) -> bool {
    comparators.iter().all(|(operator, bound)| {
        let ordering = candidate.cmp(bound);
        match operator {
            Operator::Lt => ordering == Ordering::Less,
            Operator::Le => ordering != Ordering::Greater,
            Operator::Gt => ordering == Ordering::Greater,
            Operator::Ge => ordering != Ordering::Less,
            Operator::Eq => ordering == Ordering::Equal,
        }
    })
}

/// npm's `replaceCaret`, with `includePrerelease`.
fn caret(partial: &Partial) -> Vec<Comparator> {
    use Operator::{Ge, Lt};
    let Some(major) = partial.major else {
        return Vec::new();
    };
    let Some(minor) = partial.minor else {
        return vec![(Ge, lowest(major, 0, 0)), (Lt, lowest(major + 1, 0, 0))];
    };
    let Some(patch) = partial.patch else {
        let upper = if major == 0 {
            lowest(0, minor + 1, 0)
        } else {
            lowest(major + 1, 0, 0)
        };
        return vec![(Ge, lowest(major, minor, 0)), (Lt, upper)];
    };
    let from = Version {
        pre: partial.pre.clone(),
        ..version(major, minor, patch)
    };
    let upper = match (major, minor) {
        (0, 0) => lowest(0, 0, patch + 1),
        (0, _) => lowest(0, minor + 1, 0),
        _ => lowest(major + 1, 0, 0),
    };
    vec![(Ge, from), (Lt, upper)]
}

/// npm's `replaceTilde`.
fn tilde(partial: &Partial) -> Vec<Comparator> {
    use Operator::{Ge, Lt};
    let Some(major) = partial.major else {
        return Vec::new();
    };
    let Some(minor) = partial.minor else {
        return vec![(Ge, version(major, 0, 0)), (Lt, lowest(major + 1, 0, 0))];
    };
    let upper = lowest(major, minor + 1, 0);
    match partial.patch {
        None => vec![(Ge, version(major, minor, 0)), (Lt, upper)],
        Some(patch) => vec![
            (
                Ge,
                Version {
                    pre: partial.pre.clone(),
                    ..version(major, minor, patch)
                },
            ),
            (Lt, upper),
        ],
    }
}

/// npm's `replaceXRange` for a comparator, with `includePrerelease`.
fn primitive(operator: &str, partial: &Partial) -> Option<Vec<Comparator>> {
    use Operator::{Ge, Gt, Le, Lt};
    if let Some(exact) = partial.version() {
        let operator = match operator {
            "<" => Lt,
            "<=" => Le,
            ">" => Gt,
            ">=" => Ge,
            "" | "=" => Operator::Eq,
            _ => return None,
        };
        return Some(vec![(operator, exact)]);
    }
    let Some(major) = partial.major else {
        // `<x` and `>x` match nothing; any other wildcard matches everything.
        return Some(if matches!(operator, "<" | ">") {
            vec![(Lt, lowest(0, 0, 0))]
        } else {
            Vec::new()
        });
    };
    Some(match (operator, partial.minor) {
        ("" | "=", None) => vec![(Ge, lowest(major, 0, 0)), (Lt, lowest(major + 1, 0, 0))],
        ("" | "=", Some(minor)) => vec![
            (Ge, lowest(major, minor, 0)),
            (Lt, lowest(major, minor + 1, 0)),
        ],
        (">", None) => vec![(Ge, lowest(major + 1, 0, 0))],
        (">", Some(minor)) => vec![(Ge, lowest(major, minor + 1, 0))],
        ("<=", None) => vec![(Lt, lowest(major + 1, 0, 0))],
        ("<=", Some(minor)) => vec![(Lt, lowest(major, minor + 1, 0))],
        (">=", minor) => vec![(Ge, lowest(major, minor.unwrap_or(0), 0))],
        ("<", minor) => vec![(Lt, lowest(major, minor.unwrap_or(0), 0))],
        _ => return None,
    })
}

/// npm's hyphen range `a - b`, with `includePrerelease`.
fn hyphen(from: &Partial, to: &Partial) -> Vec<Comparator> {
    use Operator::{Ge, Le, Lt};
    let mut comparators = Vec::new();
    if let Some(major) = from.major {
        let bound = match (from.minor, from.patch) {
            (None, _) => lowest(major, 0, 0),
            (Some(minor), None) => lowest(major, minor, 0),
            (Some(minor), Some(patch)) if from.pre.is_empty() => lowest(major, minor, patch),
            (Some(minor), Some(patch)) => Version {
                pre: from.pre.clone(),
                ..version(major, minor, patch)
            },
        };
        comparators.push((Ge, bound));
    }
    if let Some(major) = to.major {
        comparators.push(match (to.minor, to.patch) {
            (None, _) => (Lt, lowest(major + 1, 0, 0)),
            (Some(minor), None) => (Lt, lowest(major, minor + 1, 0)),
            (Some(minor), Some(patch)) if to.pre.is_empty() => {
                (Lt, lowest(major, minor, patch + 1))
            }
            (Some(minor), Some(patch)) => (
                Le,
                Version {
                    pre: to.pre.clone(),
                    ..version(major, minor, patch)
                },
            ),
        });
    }
    comparators
}

/// One `||` alternative: a hyphen range, or comparators separated by spaces.
fn comparator_set(text: &str) -> Option<Vec<Comparator>> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if let [from, "-", to] = words[..] {
        return Some(hyphen(&Partial::parse(from)?, &Partial::parse(to)?));
    }
    // An operator may stand apart from its version, as in `>= 1.2`.
    let mut joined: Vec<String> = Vec::new();
    let mut pending = String::new();
    for word in words {
        let operator_only = matches!(word, "<" | "<=" | ">" | ">=" | "=" | "~" | "~>" | "^");
        if operator_only {
            pending.push_str(word);
        } else {
            joined.push(format!("{pending}{word}"));
            pending.clear();
        }
    }
    if !pending.is_empty() {
        return None;
    }
    let mut comparators = Vec::new();
    for word in joined {
        let (operator, rest) = ["~>", "<=", ">=", "<", ">", "=", "~", "^"]
            .iter()
            .find_map(|operator| word.strip_prefix(operator).map(|rest| (*operator, rest)))
            .unwrap_or(("", word.as_str()));
        let partial = Partial::parse(rest)?;
        comparators.extend(match operator {
            "^" => caret(&partial),
            "~" | "~>" => tilde(&partial),
            operator => primitive(operator, &partial)?,
        });
    }
    Some(comparators)
}

/// Whether `version` satisfies the npm range, prereleases included.
pub fn satisfies(version: &str, range: &str) -> bool {
    let Some(candidate) = Partial::parse(version.trim()).and_then(|partial| partial.version())
    else {
        return false;
    };
    let Some(sets) = range
        .split("||")
        .map(comparator_set)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    sets.iter().any(|set| test(set, &candidate))
}
