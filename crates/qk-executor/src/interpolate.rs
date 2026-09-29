use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::Task;

pub(super) struct Interpolation<'a> {
    workspace: &'a Workspace,
    task: &'a Task,
    named: BTreeMap<String, String>,
}

impl<'a> Interpolation<'a> {
    pub fn new(workspace: &'a Workspace, task: &'a Task) -> Self {
        let mut named = BTreeMap::new();
        let mut args = task.args.iter().peekable();
        while let Some(arg) = args.next() {
            if let Some(flag) = arg.strip_prefix("--") {
                if let Some((key, value)) = flag.split_once('=') {
                    named.insert(key.into(), value.into());
                } else {
                    let value = if args.peek().is_some_and(|arg| !arg.starts_with('-')) {
                        args.next().unwrap().clone()
                    } else {
                        "true".into()
                    };
                    named.insert(flag.into(), value);
                }
            }
        }
        Self {
            workspace,
            task,
            named,
        }
    }

    pub fn arguments(&self) -> Result<String> {
        self.task
            .args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Result<Vec<_>>>()
            .map(|args| args.join(" "))
    }

    pub fn text(&self, text: &str) -> Result<String> {
        self.expand(text, false)
    }

    pub fn command(&self, command: &str, forward: bool) -> Result<String> {
        let interpolates_args = command.contains("{args.") || command.contains("{args}");
        let mut command = self.expand(command, true)?;
        if forward && !interpolates_args && !self.task.args.is_empty() {
            command.push(' ');
            command.push_str(&self.arguments()?);
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
                "args" => Some(if shell {
                    self.arguments()?
                } else {
                    self.task.args.join(" ")
                }),
                _ if token.starts_with("args.") => {
                    let key = &token[5..];
                    let value = self
                        .named
                        .get(key)
                        .with_context(|| format!("missing argument --{key} for {{{token}}}"))?;
                    Some(if shell { quote(value)? } else { value.clone() })
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
