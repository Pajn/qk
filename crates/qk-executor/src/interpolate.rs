//! How Nx 24's run-commands puts a task's arguments into its commands:
//! `{args.name}` and `{args}` placeholders, or the arguments appended.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value};

use crate::tokens::{closing_brace, env_name, fallbacks};

/// The run-commands options Nx reads itself, which are never forwarded.
pub(super) const PROP_KEYS: &[&str] = &[
    "command",
    "commands",
    "color",
    "no-color",
    "parallel",
    "no-parallel",
    "readyWhen",
    "cwd",
    "args",
    "envFile",
    "__unparsed__",
    "env",
    "usePty",
    "streamOutput",
    "verbose",
    "forwardAllArgs",
    "tty",
];

pub(super) struct Interpolation {
    /// The task's arguments as given, Nx's `__unparsed__`.
    unparsed: Vec<String>,
    /// The target options run-commands does not know and no argument sets.
    unknown: Vec<(String, Value)>,
    /// What `{args.name}` reads: those options, then the `args` option
    /// parsed, then the arguments.
    named: Map<String, Value>,
    /// The `args` option: shell text, forwarded as it is.
    extra: Option<String>,
    /// The task's environment, which `{env.NAME}` fallbacks read, once built.
    env: BTreeMap<String, String>,
}

impl Interpolation {
    pub fn new(unparsed: &[String], options: &Map<String, Value>) -> Result<Self> {
        let parsed = yargs(unparsed, false, false);
        let unknown: Vec<(String, Value)> = options
            .iter()
            .filter(|(key, _)| !PROP_KEYS.contains(&key.as_str()) && !parsed.contains_key(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let extra = match options.get("args") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Array(words)) => Some(
                words
                    .iter()
                    .map(|word| word.as_str().context("args entries must be strings"))
                    .collect::<Result<Vec<_>>>()?
                    .join(" "),
            ),
            Some(_) => bail!("args must be a string or an array of strings"),
        }
        .filter(|extra| !extra.is_empty());
        let mut named: Map<String, Value> = unknown.iter().cloned().collect();
        if let Some(extra) = &extra {
            // Nx strips one pair of surrounding double quotes first.
            let text = extra.strip_prefix('"').unwrap_or(extra);
            let text = text.strip_suffix('"').unwrap_or(text);
            let words = shlex::split(text).context("the args option is not valid shell text")?;
            named.extend(yargs(&words, true, true));
        }
        named.extend(parsed);
        Ok(Self {
            unparsed: unparsed.to_vec(),
            unknown,
            named,
            extra,
            env: BTreeMap::new(),
        })
    }

    /// The unknown options as `--name=value`, for those no argument or the
    /// `args` option overrides; objects, lists and nulls are left out.
    fn unknown_arguments(&self) -> Vec<String> {
        self.unknown
            .iter()
            .filter(|(key, value)| {
                !matches!(value, Value::Object(_) | Value::Array(_) | Value::Null)
                    && self.named.get(key) == Some(value)
            })
            .map(|(key, value)| wrap(&format!("--{key}={}", text(value))))
            .collect()
    }

    /// The arguments less those setting run-commands options, as Nx
    /// forwards them.
    fn unparsed_arguments(&self) -> Vec<String> {
        let mut kept = Vec::new();
        let mut index = 0;
        while index < self.unparsed.len() {
            let element = &self.unparsed[index];
            if let Some(key) = element.strip_prefix("--") {
                if element.contains('=') {
                    let name = key.split('=').next().unwrap_or_default();
                    if !PROP_KEYS.contains(&name.split('.').next().unwrap_or_default()) {
                        kept.push(element.clone());
                    }
                } else if PROP_KEYS.contains(&key) {
                    // Its value goes with it.
                    if let Some(next) = self.unparsed.get(index + 1)
                        && let Some(value) = self.named.get(key).filter(|value| truthy(value))
                        && *next == text(value)
                    {
                        index += 1;
                    }
                } else {
                    kept.push(element.clone());
                }
            } else {
                kept.push(element.clone());
            }
            index += 1;
        }
        rejoin_quoted_fragments(kept)
            .iter()
            .map(|arg| wrap(arg))
            .collect()
    }

    /// A command with the task's arguments in it, as Nx's
    /// `interpolateArgsIntoCommand` puts them: values in place of
    /// `{args.name}`, everything in place of `{args}`, or, when it has
    /// neither and `forward` is set, everything appended. Values go in as
    /// they are, unquoted.
    pub fn command(&self, command: &str, forward: bool) -> Result<String> {
        if command.contains("{args.") && command.contains("{args}") {
            bail!(
                "Command should not contain both {{args}} and {{args.*}} values. Please choose one to use."
            );
        }
        if command.contains("{args.") {
            return self.expand(command);
        }
        if command.contains("{args}") {
            let mut args = self.unknown_arguments();
            args.extend(self.unparsed_arguments());
            let all = format!(
                "{} {}",
                args.join(" "),
                self.extra.as_deref().unwrap_or_default()
            );
            return Ok(command.replace("{args}", &all));
        }
        if !forward {
            return Ok(command.to_owned());
        }
        let mut command = command.to_owned();
        let unknown = self.unknown_arguments().join(" ");
        if !unknown.is_empty() {
            command.push(' ');
            command.push_str(&unknown);
        }
        if let Some(extra) = &self.extra {
            command.push(' ');
            command.push_str(extra);
        }
        let unparsed = self.unparsed_arguments();
        if !unparsed.is_empty() {
            command.push(' ');
            command.push_str(&unparsed.join(" "));
        }
        Ok(command)
    }

    /// Gives `{env.NAME}` fallbacks the task's environment.
    pub fn set_env(&mut self, env: &BTreeMap<std::ffi::OsString, std::ffi::OsString>) {
        self.env = env
            .iter()
            .filter_map(|(name, value)| {
                Some((name.to_str()?.to_owned(), value.to_str()?.to_owned()))
            })
            .collect();
    }

    fn expand(&self, text: &str) -> Result<String> {
        // One pass: values must never be interpreted as more tokens. As in
        // Nx, braces that are not a token stay as text, and the scan goes on
        // inside them.
        let mut result = String::new();
        let mut index = 0;
        while let Some(offset) = text[index..].find('{') {
            let open = index + offset;
            let value = match closing_brace(text, open) {
                Some(close) => self
                    .token(&text[open + 1..close])?
                    .map(|value| (value, close)),
                None => None,
            };
            match value {
                Some((value, close)) => {
                    result.push_str(&text[index..open]);
                    result.push_str(&value);
                    index = close + 1;
                }
                None => {
                    result.push_str(&text[index..=open]);
                    index = open + 1;
                }
            }
        }
        result.push_str(&text[index..]);
        Ok(result)
    }

    /// The value of the token between a pair of braces, or `None` when it is
    /// not one: the first of its alternatives that is set and not empty, an
    /// argument, `env.NAME` of the task's environment or a final literal, and
    /// nothing when none is.
    fn token(&self, token: &str) -> Result<Option<String>> {
        let sources = fallbacks(token);
        if !sources[0].starts_with("args.") {
            return Ok(None);
        }
        for (index, source) in sources.iter().enumerate() {
            let found = if let Some(name) = source.strip_prefix("args.") {
                self.named
                    .get(name)
                    .filter(|value| !value.is_null())
                    .map(text)
            } else if *source == "args" {
                None
            } else if let Some(name) = env_name(source) {
                self.env_value(name)
            } else if index + 1 == sources.len() {
                return Ok(Some(self.expand(source)?));
            } else {
                bail!(
                    "{{{token}}} has a fallback after the literal {source:?} that can never be used"
                );
            };
            if let Some(found) = found.filter(|found| !found.is_empty()) {
                return Ok(Some(found));
            }
        }
        Ok(Some(String::new()))
    }

    /// A variable of the task's environment; on Windows names ignore case.
    fn env_value(&self, name: &str) -> Option<String> {
        if let Some(value) = self.env.get(name) {
            return Some(value.clone());
        }
        if !cfg!(windows) {
            return None;
        }
        self.env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }
}

/// A value as JavaScript's `String` writes it.
fn text(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(text) => text.clone(),
        Value::Number(number) => number_text(number),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                item => text(item),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        Value::Bool(flag) => flag.to_string(),
    }
}

fn number_text(number: &Number) -> String {
    match number.as_f64() {
        Some(float) if !number.is_f64() || float.fract() != 0.0 => number.to_string(),
        Some(float) if float.abs() < 1e21 => format!("{float:.0}"),
        _ => number.to_string(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// Nx's `needsShellQuoting`.
fn needs_quoting(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_whitespace() || "|&;<>()$`\\!\"'*?[]{}~#".contains(c))
}

/// Nx's `isAlreadyQuoted`.
fn already_quoted(text: &str) -> bool {
    text.len() >= 2
        && ((text.starts_with('\'') && text.ends_with('\''))
            || (text.starts_with('"') && text.ends_with('"')))
}

/// Nx's `wrapArgIntoQuotesIfNeeded`: an argument, or a `--name=` flag's
/// value, in double quotes when the shell would read it otherwise and it is
/// not quoted already.
fn wrap(arg: &str) -> String {
    let quoted = |text: &str| format!("\"{}\"", text.replace('"', "\\\""));
    if let Some((key, value)) = arg.split_once('=') {
        if key.starts_with("--") && needs_quoting(value) && !already_quoted(value) {
            return format!("{key}={}", quoted(value));
        }
        arg.to_owned()
    } else if needs_quoting(arg) && !already_quoted(arg) {
        quoted(arg)
    } else {
        arg.to_owned()
    }
}

/// Nx's `rejoinQuotedFragments`: the words of a quoted value the shell
/// split, back together.
fn rejoin_quoted_fragments(args: Vec<String>) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let quote = arg
            .chars()
            .next()
            .filter(|c| arg.chars().count() > 1 && (*c == '\'' || *c == '"'));
        match quote {
            Some(quote) if !arg.ends_with(quote) => {
                let mut fragments = vec![arg.clone()];
                let mut found = false;
                index += 1;
                while index < args.len() {
                    fragments.push(args[index].clone());
                    index += 1;
                    if fragments.last().unwrap().ends_with(quote) {
                        found = true;
                        break;
                    }
                }
                if found {
                    result.push(fragments.join(" "));
                } else {
                    result.extend(fragments);
                }
            }
            _ => {
                result.push(arg.clone());
                index += 1;
            }
        }
    }
    result
}

/// Whether yargs-parser reads a word as a flag rather than a value: it
/// starts with `-` and is not a negative number.
fn is_flag(word: &str) -> bool {
    word.starts_with('-')
        && word.len() > 1
        && !word[1..].starts_with(|c: char| c.is_ascii_digit() || c == '.')
}

/// Arguments as yargs-parser reads them with Nx's settings: `--name=value`,
/// `--name value`, `--name` for true and `--no-name` for false, `-abc` for
/// single letters, repeated names collecting a list, and the rest, and all
/// after `--`, in `_`. `camel` adds camel-case names for names with
/// dashes, and `numbers` reads numbers, as Nx reads the `args` option.
pub(super) fn yargs(words: &[String], camel: bool, numbers: bool) -> Map<String, Value> {
    let mut parsed = Map::new();
    let mut positional = Vec::new();
    let set = |key: &str, value: Value, parsed: &mut Map<String, Value>| {
        let value = match value {
            Value::String(text) if numbers => number(&text).unwrap_or(Value::String(text)),
            value => value,
        };
        let mut keys = vec![key.to_owned()];
        if camel && key.contains('-') {
            keys.push(camel_case(key));
        }
        for key in keys {
            let merged = match parsed.shift_remove(&key) {
                None => value.clone(),
                Some(Value::Array(mut items)) => {
                    items.push(value.clone());
                    Value::Array(items)
                }
                Some(previous) => Value::Array(vec![previous, value.clone()]),
            };
            parsed.insert(key, merged);
        }
    };
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        index += 1;
        if word == "--" {
            positional.extend(words[index..].iter().cloned());
            break;
        }
        if let Some(flag) = word.strip_prefix("--").filter(|flag| !flag.is_empty()) {
            if let Some((key, value)) = flag.split_once('=') {
                set(key, Value::String(value.into()), &mut parsed);
            } else if let Some(key) = flag.strip_prefix("no-") {
                set(key, Value::Bool(false), &mut parsed);
            } else if let Some(next) = words.get(index).filter(|next| !is_flag(next)) {
                set(flag, Value::String(next.clone()), &mut parsed);
                index += 1;
            } else {
                set(flag, Value::Bool(true), &mut parsed);
            }
        } else if is_flag(word) {
            let letters = &word[1..];
            if let Some((keys, value)) = letters.split_once('=') {
                let mut keys: Vec<char> = keys.chars().collect();
                if let Some(last) = keys.pop() {
                    for key in keys {
                        set(&key.to_string(), Value::Bool(true), &mut parsed);
                    }
                    set(&last.to_string(), Value::String(value.into()), &mut parsed);
                }
                continue;
            }
            let mut keys: Vec<char> = letters.chars().collect();
            let last = keys.pop().unwrap();
            for key in keys {
                set(&key.to_string(), Value::Bool(true), &mut parsed);
            }
            if let Some(next) = words.get(index).filter(|next| !is_flag(next)) {
                set(&last.to_string(), Value::String(next.clone()), &mut parsed);
                index += 1;
            } else {
                set(&last.to_string(), Value::Bool(true), &mut parsed);
            }
        } else {
            positional.push(word.clone());
        }
    }
    parsed.insert(
        "_".into(),
        Value::Array(positional.into_iter().map(Value::String).collect()),
    );
    parsed
}

/// A number as yargs-parser reads one.
fn number(text: &str) -> Option<Value> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok().map(Value::from);
    }
    // A leading zero before another digit is text, as is anything else
    // that is not a plain decimal.
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.starts_with('0') && digits.len() > 1 && !digits.starts_with("0.") {
        return None;
    }
    let mantissa = digits.split(['e', 'E']).next().unwrap_or_default();
    if mantissa.is_empty() || !mantissa.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let float: f64 = text.parse().ok()?;
    if !float.is_finite() || float.floor().abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    if float.fract() == 0.0 {
        Some(Value::from(float as i64))
    } else {
        Number::from_f64(float).map(Value::Number)
    }
}

/// The arguments for a package script: each quoted, so that the shell
/// passes it on as given.
pub(super) fn quoted_arguments(args: &[String]) -> Result<String> {
    args.iter()
        .map(|arg| quote(arg))
        .collect::<Result<Vec<_>>>()
        .map(|args| args.join(" "))
}

pub(super) fn camel_case(key: &str) -> String {
    let mut result = String::new();
    let mut upper = false;
    for ch in key.chars() {
        if ch == '-' {
            upper = true;
        } else if upper {
            result.extend(ch.to_uppercase());
            upper = false;
        } else {
            result.push(ch);
        }
    }
    result
}

pub(super) fn quote(value: &str) -> Result<String> {
    if value.contains('\0') {
        bail!("arguments cannot contain NUL bytes");
    }
    #[cfg(windows)]
    {
        if value.contains(['"', '%', '!', '^', '\r', '\n']) {
            bail!(
                "this argument cannot yet be safely forwarded through cmd.exe; use an environment variable instead"
            );
        }
        Ok(format!("\"{value}\""))
    }
    #[cfg(not(windows))]
    {
        Ok(format!("'{}'", value.replace('\'', "'\\''")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn arguments_parse_as_yargs_parser_reads_them() {
        // Each case checked against yargs-parser 21 with Nx's settings.
        assert_eq!(
            Value::Object(yargs(
                &words(&[
                    "--a=1", "--b", "2", "--c", "--no-d", "pos", "--e=true", "--f", "false"
                ]),
                false,
                false
            )),
            json!({"a": "1", "b": "2", "c": true, "d": false, "e": "true", "f": "false", "_": ["pos"]})
        );
        assert_eq!(
            Value::Object(yargs(&words(&["--a=1", "--a=2", "--a", "3"]), false, false)),
            json!({"a": ["1", "2", "3"], "_": []})
        );
        assert_eq!(
            Value::Object(yargs(
                &words(&["-x", "val", "-abc", "--k=v=w", "--", "--after", "x"]),
                false,
                false
            )),
            json!({"x": "val", "a": true, "b": true, "c": true, "k": "v=w", "_": ["--after", "x"]})
        );
        assert_eq!(
            Value::Object(yargs(
                &words(&["--n", "-1", "--m", "--o=", "--p=null"]),
                false,
                false
            )),
            json!({"n": "-1", "m": true, "o": "", "p": "null", "_": []})
        );
        assert_eq!(
            Value::Object(yargs(&words(&["--foo-bar=1", "--x", "a b"]), true, true)),
            json!({"foo-bar": 1, "fooBar": 1, "x": "a b", "_": []})
        );
    }

    #[test]
    fn commands_take_arguments_as_nx_puts_them() {
        let options = json!({"port": 3000, "name": "a b", "list": [1], "args": "--extra"});
        let options = options.as_object().unwrap();
        let interpolation = Interpolation::new(
            &words(&[
                "--mode",
                "dev",
                "--cwd",
                "elsewhere",
                "two words",
                "'{\"a\":",
                "1}'",
            ]),
            options,
        )
        .unwrap();
        assert_eq!(
            interpolation.command("serve", true).unwrap(),
            "serve --port=3000 --name=\"a b\" --extra --mode dev \"two words\" '{\"a\": 1}'"
        );
        assert_eq!(
            interpolation.command("serve {args} end", true).unwrap(),
            "serve --port=3000 --name=\"a b\" --mode dev \"two words\" '{\"a\": 1}' --extra end"
        );
        // Values go in as they are.
        assert_eq!(
            interpolation
                .command(
                    "echo {args.mode} {args.name} {args.list} {args.extra} {args.none}!",
                    true
                )
                .unwrap(),
            "echo dev a b 1 true !"
        );
        assert_eq!(interpolation.command("serve", false).unwrap(), "serve");
        assert!(
            interpolation
                .command("{args} {args.mode}", true)
                .unwrap_err()
                .to_string()
                .contains("should not contain both")
        );
        let plain = Interpolation::new(&[], &Map::new()).unwrap();
        assert_eq!(plain.command("echo {args}", true).unwrap(), "echo  ");
        assert_eq!(plain.command("echo", true).unwrap(), "echo");
    }
}
