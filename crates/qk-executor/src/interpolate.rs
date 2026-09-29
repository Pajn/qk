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

    fn expand(&self, text: &str, shell: bool) -> Result<String> {
        // One pass: user-provided values must never be interpreted as more tokens.
        let mut result = String::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            result.push_str(&rest[..start]);
            rest = &rest[start..];
            let Some(end) = rest.find('}') else {
                break;
            };
            let token = &rest[1..end];
            if shell && (token == "args" || token.starts_with("args.")) {
                let prefix = &text[..text.len() - rest.len()];
                if quoted_or_escaped(prefix) {
                    bail!(
                        "argument placeholders must be unquoted and unescaped; qk quotes their values"
                    );
                }
            }
            let value = match token {
                "workspaceRoot" => Some(
                    self.workspace
                        .root
                        .to_str()
                        .context("workspace path must be UTF-8")?
                        .to_owned(),
                ),
                "projectRoot" => Some(self.workspace.projects[&self.task.project].root.clone()),
                "projectName" => Some(self.task.project.clone()),
                "args" => Some(self.all(shell)?),
                _ if token.starts_with("args.") => {
                    // As in Nx, an argument that was not given interpolates as nothing.
                    let value = self.named.get(&token[5..]).map_or("", String::as_str);
                    Some(if shell && !value.is_empty() {
                        quote(value)?
                    } else {
                        value.to_owned()
                    })
                }
                _ => None,
            };
            if let Some(value) = value {
                result.push_str(&value);
            } else {
                result.push_str(&rest[..=end]);
            }
            rest = &rest[end + 1..];
        }
        result.push_str(rest);
        Ok(result)
    }
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
