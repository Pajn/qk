//! Dotenv files as Nx reads them, with npm's `dotenv` and `dotenv-expand`:
//! every file of a set is parsed first, then `$VAR` and `${VAR}` references
//! are expanded once across all of them.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;

/// The entries of one dotenv file, in order, as `dotenv`'s `parse` reads
/// them: lines it cannot read are left out, a key given twice keeps its
/// first place and its last value, quotes are removed, and `\n` and `\r`
/// become line breaks inside double quotes. Nothing is expanded here.
pub fn parse(text: &str) -> Vec<(String, String)> {
    static LINE: OnceLock<Regex> = OnceLock::new();
    // dotenv 16's LINE expression, with JavaScript's ASCII `\w`.
    let line = LINE.get_or_init(|| {
        Regex::new(
            r#"(?m)(?:^|^)\s*(?:export\s+)?((?-u:[\w.-])+)(?:\s*=\s*?|:\s+?)(\s*'(?:\\'|[^'])*'|\s*"(?:\\"|[^"])*"|\s*`(?:\\`|[^`])*`|[^#\r\n]+)?\s*(?:#.*)?$"#,
        )
        .unwrap()
    });
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut entries: Vec<(String, String)> = Vec::new();
    for captures in line.captures_iter(&text) {
        let key = captures[1].to_owned();
        let raw = captures.get(2).map_or("", |value| value.as_str()).trim();
        let quote = raw.chars().next();
        let mut value = match quote {
            Some(quote @ ('\'' | '"' | '`')) if raw.len() >= 2 && raw.ends_with(quote) => {
                raw[1..raw.len() - 1].to_owned()
            }
            _ => raw.to_owned(),
        };
        if quote == Some('"') {
            value = value.replace("\\n", "\n").replace("\\r", "\r");
        }
        match entries.iter_mut().find(|(name, _)| *name == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
        }
    }
    entries
}

/// Reads `paths` into `env` as Nx's `loadAndExpandDotEnvFile` does. The files
/// are merged first, the first to set a key winning; each key `env` does not
/// hold is set; then each value is expanded against `env` and what the files
/// expanded before it, and set, which also fills a variable set but empty. A
/// variable `env` already holds with another value keeps it. A file that
/// does not exist is skipped.
pub fn load(paths: &[impl AsRef<Path>], env: &mut BTreeMap<OsString, OsString>) -> Result<()> {
    let mut parsed: Vec<(String, String)> = Vec::new();
    for path in paths {
        let path = path.as_ref();
        let text = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        for (key, value) in parse(&text) {
            if !parsed.iter().any(|(name, _)| *name == key) {
                parsed.push((key, value));
            }
        }
    }
    for (key, value) in &parsed {
        let name = crate::environment_key(env, key.into());
        env.entry(name).or_insert_with(|| value.into());
    }
    // Expanded against `env` as populated, which takes the results only
    // once every value is expanded, as dotenv-expand does.
    let mut running: BTreeMap<String, String> = BTreeMap::new();
    let mut expanded = Vec::new();
    for (key, value) in &parsed {
        let name = crate::environment_key(env, key.into());
        let current = env
            .get(&name)
            .map(|value| value.to_string_lossy().into_owned());
        let value = match current {
            Some(current) if !current.is_empty() && current != *value => current,
            _ => expand(value, env, &running),
        };
        let value = value.replace("\\$", "$");
        running.insert(key.clone(), value.clone());
        expanded.push((name, value));
    }
    for (name, value) in expanded {
        env.insert(name, value.into());
    }
    Ok(())
}

/// The variables of one file as `load` would set them in an empty
/// environment: what Nx compares to unload a root dotenv file.
pub fn read_alone(path: &Path) -> Result<BTreeMap<OsString, OsString>> {
    let mut env = BTreeMap::new();
    load(&[path], &mut env)?;
    Ok(env)
}

/// `dotenv-expand`'s expansion of one value: `${KEY}`, `$KEY` and the
/// `${KEY:-default}`, `${KEY-default}`, `${KEY:+alternate}` and
/// `${KEY+alternate}` forms, not preceded by a backslash, the environment
/// winning over values the files set before. A reference is expanded again
/// until nothing changes, and one that comes back to an earlier result takes
/// its default instead.
fn expand(
    value: &str,
    env: &BTreeMap<OsString, OsString>,
    running: &BTreeMap<String, String>,
) -> String {
    let lookup = |key: &str| -> Option<String> {
        env.get(OsStr::new(key))
            .map(|value| value.to_string_lossy().into_owned())
            .or_else(|| running.get(key).cloned())
            .filter(|value| !value.is_empty())
    };
    let mut result = value.to_owned();
    let mut seen: Vec<String> = Vec::new();
    while let Some((start, end, expression)) = reference(&result) {
        seen.push(result.clone());
        let (key, operator, rest) = match ["-", ":-", "+", ":+"]
            .iter()
            .filter_map(|operator| expression.find(operator).map(|index| (index, *operator)))
            // The first operator found, the longer at the same place.
            .min_by_key(|(index, operator)| (*index, std::cmp::Reverse(operator.len())))
        {
            Some((index, operator)) => (
                &expression[..index],
                Some(operator),
                expression[index..]
                    .split(operator)
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(operator),
            ),
            None => (expression.as_str(), None, String::new()),
        };
        let replacement = match operator {
            Some("+" | ":+") => {
                if lookup(key).is_some() {
                    rest
                } else {
                    String::new()
                }
            }
            _ => match lookup(key) {
                Some(found) if seen.contains(&found) => rest,
                Some(found) => found,
                None => rest,
            },
        };
        result.replace_range(start..end, &replacement);
        if running.get(key) == Some(&result) {
            break;
        }
    }
    result
}

/// The first `${...}` or `$NAME` in `text` not preceded by a backslash: its
/// span and what it names.
fn reference(text: &str) -> Option<(usize, usize, String)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(offset) = text[index..].find('$') {
        let start = index + offset;
        index = start + 1;
        if start > 0 && bytes[start - 1] == b'\\' {
            continue;
        }
        let rest = &text[start + 1..];
        if let Some(inner) = rest.strip_prefix('{') {
            if let Some(close) = inner.find(['{', '}'])
                && inner.as_bytes()[close] == b'}'
                && close > 0
            {
                return Some((start, start + 2 + close + 1, inner[..close].to_owned()));
            }
            continue;
        }
        let name: String = rest
            .chars()
            .enumerate()
            .take_while(|(position, character)| {
                character.is_ascii_alphabetic()
                    || *character == '_'
                    || (*position > 0 && character.is_ascii_digit())
            })
            .map(|(_, character)| character)
            .collect();
        if !name.is_empty() {
            return Some((start, start + 1 + name.len(), name));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Vec<(String, String)> {
        parse(text)
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn parses_as_npm_dotenv_does() {
        assert_eq!(
            parsed(
                "# comment\nexport A=1\nB = two words # note\nC='single $X'\nD=\"line\\nbreak\"\nE=`tick`\nF=\n  G=x\nnot a line\nH: yaml\nB=again\n"
            ),
            pairs(&[
                ("A", "1"),
                ("B", "again"),
                ("C", "single $X"),
                ("D", "line\nbreak"),
                ("E", "tick"),
                ("F", ""),
                ("G", "x"),
                ("H", "yaml"),
            ])
        );
        // Quoted values span lines; `\"` stays as written.
        assert_eq!(
            parsed("A=\"one\ntwo\"\nB=\"say \\\"hi\\\"\"\nC=a#b\n"),
            pairs(&[("A", "one\ntwo"), ("B", "say \\\"hi\\\""), ("C", "a")])
        );
    }

    fn env(entries: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        entries
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn expands_once_across_every_file() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join(".env.build");
        let second = directory.path().join(".env");
        std::fs::write(&first, "URL=${HOST}:${PORT}\nNAME=first\n").unwrap();
        std::fs::write(
            &second,
            "HOST=localhost\nPORT=${DEFAULT_PORT:-80}\nNAME=second\nSHELL_SET=file\nEMPTY=filled\nLITERAL=\\$HOME\nALT=${HOST:+yes}\n",
        )
        .unwrap();
        let mut variables = env(&[("SHELL_SET", "shell"), ("EMPTY", "")]);
        load(&[&first, &second], &mut variables).unwrap();
        let get = |name: &str| variables[OsStr::new(name)].to_str().unwrap().to_owned();
        assert_eq!(get("URL"), "localhost:80");
        assert_eq!(get("NAME"), "first");
        assert_eq!(get("SHELL_SET"), "shell");
        // dotenv-expand fills a variable set but empty.
        assert_eq!(get("EMPTY"), "filled");
        assert_eq!(get("LITERAL"), "$HOME");
        assert_eq!(get("ALT"), "yes");
    }

    #[test]
    fn a_reference_to_itself_ends() {
        let mut variables = env(&[]);
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join(".env");
        std::fs::write(&file, "A=${A}\nB=x${B:-fallback}\n").unwrap();
        load(&[&file], &mut variables).unwrap();
        assert_eq!(variables[OsStr::new("A")], "");
        assert_eq!(variables[OsStr::new("B")], "xfallback");
    }
}
