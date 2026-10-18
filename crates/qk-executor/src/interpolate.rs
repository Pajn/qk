use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::Task;

pub(super) struct Interpolation<'a> {
    workspace: &'a Workspace,
    task: &'a Task,
    /// The task's arguments, less those that set run-commands options.
    args: Vec<String>,
    named: BTreeMap<String, String>,
    /// Target options run-commands does not know, as Nx forwards them:
    /// `--name=value` after the command, unless an argument of the same name
    /// overrides it.
    options: Vec<String>,
    /// The `args` option: shell text, forwarded as it is.
    extra: Option<String>,
    /// The task's environment, which `{env.NAME}` fallbacks read, once built.
    env: BTreeMap<String, String>,
}

impl<'a> Interpolation<'a> {
    pub fn new(
        workspace: &'a Workspace,
        task: &'a Task,
        args: Vec<String>,
        options: &BTreeMap<String, String>,
        extra: Option<&str>,
    ) -> Result<Self> {
        let mut interpolation = Self {
            workspace,
            task,
            args,
            named: BTreeMap::new(),
            options: Vec::new(),
            extra: None,
            env: BTreeMap::new(),
        };
        let options: BTreeMap<String, String> = options
            .iter()
            .map(|(key, value)| Ok((key.clone(), interpolation.project_tokens(value)?)))
            .collect::<Result<_>>()?;
        let extra = extra
            .map(|extra| interpolation.project_tokens(extra))
            .transpose()?
            .filter(|extra| !extra.trim().is_empty());
        // As in Nx, arguments win over the `args` option, which wins over
        // other options; `args` names are also readable in camel case.
        let mut named = options.clone();
        if let Some(extra) = &extra {
            let words = shlex::split(extra).context("the args option is not valid shell text")?;
            for (key, value) in flags(&words) {
                if key.contains('-') {
                    named.insert(camel_case(&key), value.clone());
                }
                named.insert(key, value);
            }
        }
        named.extend(flags(&interpolation.args));
        interpolation.options = options
            .iter()
            .filter(|(key, value)| named.get(*key) == Some(*value))
            .map(|(key, value)| format!("--{key}={value}"))
            .collect();
        interpolation.named = named;
        interpolation.extra = extra;
        Ok(interpolation)
    }

    /// `{projectRoot}`, `{workspaceRoot}` and `{projectName}`, which Nx
    /// replaces in every option.
    fn project_tokens(&self, text: &str) -> Result<String> {
        Ok(text
            .replace(
                "{workspaceRoot}",
                self.workspace
                    .root
                    .to_str()
                    .context("workspace path must be UTF-8")?,
            )
            .replace(
                "{projectRoot}",
                &self.workspace.projects[&self.task.project].root,
            )
            .replace("{projectName}", &self.task.project))
    }

    /// Forwarded options, the `args` option and then the task's arguments,
    /// for the shell.
    fn forwarded(&self) -> Result<String> {
        let mut parts = self
            .options
            .iter()
            .map(|arg| quote(arg))
            .collect::<Result<Vec<_>>>()?;
        parts.extend(self.extra.clone());
        for arg in &self.args {
            parts.push(quote(arg)?);
        }
        Ok(parts.join(" "))
    }

    /// What `{args}` stands for: as in Nx, the `args` option comes last.
    fn all(&self, shell: bool) -> Result<String> {
        let mut parts = Vec::new();
        for arg in self.options.iter().chain(&self.args) {
            parts.push(if shell { quote(arg)? } else { arg.clone() });
        }
        parts.extend(self.extra.clone());
        Ok(parts.join(" "))
    }

    pub fn arguments(&self) -> Result<String> {
        self.args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Result<Vec<_>>>()
            .map(|args| args.join(" "))
    }

    pub fn text(&self, text: &str) -> Result<String> {
        self.expand(text, false)
    }

    pub fn command(&self, command: &str, forward: bool) -> Result<String> {
        if command.contains("{args.") && command.contains("{args}") {
            bail!("a command cannot use both {{args}} and {{args.*}}; choose one");
        }
        let interpolates_args = command.contains("{args.") || command.contains("{args}");
        let mut command = self.expand(command, true)?;
        if forward
            && !interpolates_args
            && !(self.args.is_empty() && self.options.is_empty() && self.extra.is_none())
        {
            command.push(' ');
            command.push_str(&self.forwarded()?);
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

    fn expand(&self, text: &str, shell: bool) -> Result<String> {
        // One pass: user-provided values must never be interpreted as more
        // tokens. As in Nx, braces that are not a token stay as text, and the
        // scan goes on inside them.
        let mut result = String::new();
        let mut index = 0;
        while let Some(offset) = text[index..].find('{') {
            let open = index + offset;
            let value = match closing_brace(text, open) {
                Some(close) => self
                    .token(&text[open + 1..close], &text[..open], shell)?
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
    /// not one.
    fn token(&self, token: &str, prefix: &str, shell: bool) -> Result<Option<String>> {
        let value = match token {
            "workspaceRoot" => self
                .workspace
                .root
                .to_str()
                .context("workspace path must be UTF-8")?
                .to_owned(),
            "projectRoot" => self.workspace.projects[&self.task.project].root.clone(),
            "projectName" => self.task.project.clone(),
            "args" => {
                if shell && quoted_or_escaped(prefix) {
                    bail!(
                        "argument placeholders must be unquoted and unescaped; qk quotes their values"
                    );
                }
                self.all(shell)?
            }
            _ => {
                let sources = fallbacks(token);
                if !sources[0].starts_with("args.") {
                    return Ok(None);
                }
                if shell && quoted_or_escaped(prefix) {
                    bail!(
                        "argument placeholders must be unquoted and unescaped; qk quotes their values"
                    );
                }
                // As in Nx 24, the first value that is set and not empty, and
                // nothing when none is: an argument, `env.NAME` of the task's
                // environment, or a final literal.
                let mut value = String::new();
                for (index, source) in sources.iter().enumerate() {
                    let found = if let Some(name) = source.strip_prefix("args.") {
                        self.named.get(name).cloned()
                    } else if let Some(name) = source.strip_prefix("env.").filter(|name| {
                        !name.is_empty()
                            && name
                                .chars()
                                .all(|c| c.is_alphanumeric() || "_.()-".contains(c))
                    }) {
                        self.env_value(name)
                    } else if index + 1 == sources.len() {
                        Some(self.expand(source, false)?)
                    } else {
                        bail!(
                            "{{{token}}} has a fallback after the literal {source:?} that can never be used"
                        );
                    };
                    if let Some(found) = found.filter(|found| !found.is_empty()) {
                        value = found;
                        break;
                    }
                }
                if shell && !value.is_empty() {
                    quote(&value)?
                } else {
                    value
                }
            }
        };
        Ok(Some(value))
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

/// Where the brace opening at `open` closes, counting nested pairs.
fn closing_brace(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + index);
                }
            }
            _ => {}
        }
    }
    None
}

/// A token's `??` alternatives, trimmed, outside any nested braces.
fn fallbacks(token: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = token.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'?' if depth == 0 && bytes.get(index + 1) == Some(&b'?') => {
                parts.push(token[start..index].trim());
                start = index + 2;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(token[start..].trim());
    parts
}

/// Flags as yargs reads them: `--name=value`, `--name value`, `--name` for
/// true and `--no-name` for false.
pub(super) fn flags(words: &[String]) -> BTreeMap<String, String> {
    let mut named = BTreeMap::new();
    let mut words = words.iter().peekable();
    while let Some(word) = words.next() {
        let Some(flag) = word.strip_prefix("--") else {
            continue;
        };
        if let Some((key, value)) = flag.split_once('=') {
            named.insert(key.into(), value.into());
        } else if let Some(key) = flag.strip_prefix("no-") {
            named.insert(key.into(), "false".into());
        } else {
            let value = if words.peek().is_some_and(|word| !word.starts_with('-')) {
                words.next().unwrap().clone()
            } else {
                "true".into()
            };
            named.insert(flag.into(), value);
        }
    }
    named
}

fn camel_case(key: &str) -> String {
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

fn quoted_or_escaped(text: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') && !cfg!(windows) {
            escaped = true;
        } else if Some(ch) == quote {
            quote = None;
        } else if quote.is_none() && (ch == '"' || (ch == '\'' && !cfg!(windows))) {
            quote = Some(ch);
        }
    }
    quote.is_some() || escaped
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
