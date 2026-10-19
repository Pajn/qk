//! The subset of the `minimatch` package Nx matches names and paths with:
//! project selectors, workspace package globs and glob keys in
//! `targetDefaults`.
//!
//! Patterns are split on `/`, and a path matches segment by segment. `*`
//! and `?` never cross a `/`; a `**` segment stands for any number of
//! segments, and `**` inside a segment is a `*`. Braces expand first, as
//! `{a,b}` and numeric `{1..3}`. Character classes take `!` or `^` to
//! negate and `a-z` ranges, and `\` escapes the next character. Without the
//! `dot` option a wildcard does not match a segment starting with `.`; with
//! it, as without, no wildcard matches a `.` or `..` segment. An unclosed
//! class is literal text, as in minimatch. Extended globs such as `@(a|b)`
//! are not supported: their characters match themselves.

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Minimatch {
    alternatives: Vec<Vec<Segment>>,
    dot: bool,
}

#[derive(Clone, Debug)]
enum Segment {
    GlobStar,
    Tokens(Vec<Token>),
}

#[derive(Clone, Debug)]
enum Token {
    Literal(char),
    One,
    Star,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Minimatch {
    pub fn new(pattern: &str, dot: bool) -> Self {
        let alternatives = expand_braces(pattern)
            .iter()
            .map(|pattern| pattern.split('/').map(segment).collect())
            .collect();
        Self { alternatives, dot }
    }

    pub fn is_match(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        self.alternatives
            .iter()
            .any(|segments| match_segments(segments, &parts, self.dot))
    }
}

/// Whether `path` matches `pattern`, as `minimatch(path, pattern, { dot })`.
pub fn is_match(pattern: &str, path: &str, dot: bool) -> bool {
    Minimatch::new(pattern, dot).is_match(path)
}

fn segment(text: &str) -> Segment {
    if text == "**" {
        return Segment::GlobStar;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' if index + 1 < chars.len() => {
                tokens.push(Token::Literal(chars[index + 1]));
                index += 2;
                continue;
            }
            '*' => {
                if !matches!(tokens.last(), Some(Token::Star)) {
                    tokens.push(Token::Star);
                }
            }
            '?' => tokens.push(Token::One),
            '[' => {
                if let Some((class, end)) = class(&chars, index) {
                    tokens.push(class);
                    index = end + 1;
                    continue;
                }
                tokens.push(Token::Literal('['));
            }
            other => tokens.push(Token::Literal(other)),
        }
        index += 1;
    }
    Segment::Tokens(tokens)
}

/// The class opening at `start`, and the index of its closing `]`.
fn class(chars: &[char], start: usize) -> Option<(Token, usize)> {
    let mut index = start + 1;
    let negated = matches!(chars.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let mut current = *chars.get(index)?;
        if current == ']' && !first {
            return Some((Token::Class { negated, ranges }, index));
        }
        first = false;
        if current == '\\' {
            index += 1;
            current = *chars.get(index)?;
        }
        if chars.get(index + 1) == Some(&'-') && chars.get(index + 2).is_some_and(|c| *c != ']') {
            let mut end_index = index + 2;
            let mut end = chars[end_index];
            if end == '\\' {
                end_index += 1;
                end = *chars.get(end_index)?;
            }
            ranges.push((current, end));
            index = end_index + 1;
        } else {
            ranges.push((current, current));
            index += 1;
        }
    }
}

fn match_segments(segments: &[Segment], parts: &[&str], dot: bool) -> bool {
    match segments.split_first() {
        // A path with a trailing slash matches the pattern without one.
        None => parts.is_empty() || parts == [""],
        Some((Segment::GlobStar, [])) => {
            // A last `**` takes at least one part, which may be empty.
            !parts.is_empty()
                && parts.iter().enumerate().all(|(index, part)| {
                    wildcard_may_match(part, dot) || (part.is_empty() && index == parts.len() - 1)
                })
        }
        Some((Segment::GlobStar, rest)) => {
            // A globstar takes no part, or one more and tries again.
            match_segments(rest, parts, dot)
                || parts.split_first().is_some_and(|(part, remaining)| {
                    wildcard_may_match(part, dot) && match_segments(segments, remaining, dot)
                })
        }
        Some((Segment::Tokens(tokens), rest)) => match parts.split_first() {
            Some((part, remaining)) => {
                part_matches(tokens, part, dot) && match_segments(rest, remaining, dot)
            }
            None => false,
        },
    }
}

/// Whether a wildcard may stand for this part at all.
fn wildcard_may_match(part: &str, dot: bool) -> bool {
    !part.is_empty() && part != "." && part != ".." && (dot || !part.starts_with('.'))
}

fn part_matches(tokens: &[Token], part: &str, dot: bool) -> bool {
    // `.` and `..`, and without `dot` any part starting with `.`, match only
    // a pattern spelling the dot out.
    if part.is_empty() {
        // No wildcard stands for an empty part.
        return tokens.is_empty();
    }
    if part == "." || part == ".." {
        if !tokens
            .iter()
            .all(|token| matches!(token, Token::Literal(_)))
        {
            return false;
        }
    } else if part.starts_with('.') && !dot && !matches!(tokens.first(), Some(Token::Literal(_))) {
        return false;
    }
    let chars: Vec<char> = part.chars().collect();
    match_tokens(tokens, &chars)
}

fn match_tokens(tokens: &[Token], chars: &[char]) -> bool {
    match tokens.split_first() {
        None => chars.is_empty(),
        Some((Token::Star, rest)) => {
            (0..=chars.len()).any(|skip| match_tokens(rest, &chars[skip..]))
        }
        Some((token, rest)) => match chars.split_first() {
            Some((char, remaining)) => {
                let matched = match token {
                    Token::Literal(literal) => literal == char,
                    Token::One => true,
                    Token::Class { negated, ranges } => {
                        ranges
                            .iter()
                            .any(|(low, high)| (*low..=*high).contains(char))
                            != *negated
                    }
                    Token::Star => unreachable!(),
                };
                matched && match_tokens(rest, remaining)
            }
            None => false,
        },
    }
}

/// Bash-style brace expansion, as the `brace-expansion` package minimatch
/// uses: comma lists, nested, and numeric ranges. A brace with neither is
/// literal text.
fn expand_braces(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '{' => {
                if let Some(end) = closing_brace(&chars, index) {
                    let body: String = chars[index + 1..end].iter().collect();
                    let options = brace_options(&body);
                    if let Some(options) = options {
                        let prefix: String = chars[..index].iter().collect();
                        let suffix: String = chars[end + 1..].iter().collect();
                        return options
                            .iter()
                            .flat_map(|option| expand_braces(&format!("{prefix}{option}{suffix}")))
                            .collect();
                    }
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    vec![pattern.to_owned()]
}

fn closing_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0;
    let mut index = start;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 1,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The alternatives a brace body stands for, or `None` when it is literal.
fn brace_options(body: &str) -> Option<Vec<String>> {
    let mut options = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    let mut escaped = false;
    for char in body.chars() {
        if escaped {
            current.push(char);
            escaped = false;
            continue;
        }
        match char {
            '\\' => {
                current.push(char);
                escaped = true;
            }
            '{' => {
                depth += 1;
                current.push(char);
            }
            '}' => {
                depth -= 1;
                current.push(char);
            }
            ',' if depth == 0 => options.push(std::mem::take(&mut current)),
            _ => current.push(char),
        }
    }
    if !options.is_empty() {
        options.push(current);
        return Some(options);
    }
    let (start, end) = body.split_once("..")?;
    let (start, end): (i64, i64) = (start.parse().ok()?, end.parse().ok()?);
    let range: Vec<i64> = if start <= end {
        (start..=end).collect()
    } else {
        (end..=start).rev().collect()
    };
    Some(range.iter().map(i64::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::is_match;

    #[test]
    fn a_star_stays_within_a_segment() {
        assert!(is_match("@scope/*", "@scope/a", true));
        assert!(!is_match("*", "@scope/a", true));
        assert!(is_match("**", "@scope/a", true));
        assert!(is_match("libs/**", "libs/a/b", true));
        assert!(!is_match("libs/**", "libs", true));
        assert!(is_match("libs/**", "libs/", true));
        assert!(is_match("a/*", "a/b/", true));
        assert!(is_match("libs/**/a", "libs/a", true));
        assert!(is_match("a**b", "axxb", true));
        assert!(!is_match("a**b", "ax/xb", true));
        assert!(!is_match("?", "/", true));
    }

    #[test]
    fn dots_need_the_option_and_never_match_dot_segments() {
        assert!(!is_match("*", ".hidden", false));
        assert!(is_match("*", ".hidden", true));
        assert!(is_match(".*", ".hidden", false));
        assert!(!is_match(
            "packages/*/package.json",
            "packages/.a/package.json",
            false
        ));
        assert!(!is_match("**/package.json", ".a/package.json", false));
        assert!(is_match("**/package.json", ".a/package.json", true));
        assert!(!is_match("*", ".", true));
        assert!(!is_match("*", "..", true));
        assert!(is_match(".", ".", true));
        assert!(!is_match(".*", ".", true));
        assert!(!is_match("?", ".", true));
        assert!(!is_match("**", "a/.b", false));
        assert!(is_match("**", "a/.b", true));
        assert!(is_match("**/package.json", "package.json", false));
    }

    #[test]
    fn braces_classes_and_escapes() {
        assert!(is_match("{a,b}-*", "b-x", true));
        assert!(is_match("a{1..3}", "a2", true));
        assert!(!is_match("a{1..3}", "a4", true));
        assert!(is_match("{x}", "{x}", true));
        assert!(is_match("[a-c]x", "bx", true));
        assert!(!is_match("[!a-c]x", "bx", true));
        assert!(is_match("[!a-c]x", "dx", true));
        assert!(is_match("[", "[", true));
        assert!(is_match("\\*", "*", true));
        assert!(!is_match("\\*", "a", true));
    }
}
