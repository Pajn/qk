//! Reads a command line as Nx's yargs configuration does, then hands clap the
//! spelling it understands.
//!
//! Nx accepts each option in camelCase and kebab-case, a boolean with a value
//! (`--nx-bail=false`, `--skip-nx-cache true`) or negated (`--no-nx-bail`),
//! and on the commands that run tasks it passes what it does not know to the
//! tasks: an unknown option or an extra positional is an override, as if it
//! followed `--` (yargs' `unknown-options-as-args`).

use std::ffi::OsString;

use clap::{Arg, ArgAction, Command};

/// The commands whose unknown options and extra positionals reach the tasks.
const FORWARDING: &[&str] = &["run", "run-many", "affected"];

/// `nx exec` parses its options as `run-many` does, but its command is only
/// what follows `--`: anything else it does not know is left out, as in Nx.
const EXEC: &str = "exec";

/// Rewrites options to the kebab-case spelling clap defines, booleans to a
/// bare flag or their `--no-` form, and on the commands that run tasks moves
/// unknown options and extra positionals behind `--`, ahead of what already
/// followed it. Nx itself drops them when a `--` is present; qk keeps both.
pub fn normalize(args: Vec<OsString>, root: &Command) -> Vec<OsString> {
    let mut root = root.clone();
    root.build();
    let mut tokens = args.into_iter();
    let mut kept: Vec<OsString> = tokens.next().into_iter().collect();
    let mut command = &root;
    let mut forwarded = Vec::new();
    let mut after_dash = Vec::new();
    let mut dash = false;
    // Positionals the current command takes before the rest are forwarded.
    let mut positionals = 0;
    let mut descending = true;
    while let Some(token) = tokens.next() {
        let Some(text) = token.to_str().map(str::to_owned) else {
            route(command, token, &mut kept, &mut forwarded, &mut positionals);
            descending = false;
            continue;
        };
        if text == "--" {
            dash = true;
            after_dash.extend(tokens.by_ref());
            break;
        }
        if let Some(flag) = text.strip_prefix("--").filter(|flag| !flag.is_empty()) {
            let (name, value) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (flag, None),
            };
            let kebab = kebab(name);
            let found = find_long(command, &kebab).map(|arg| (arg, kebab.clone(), true));
            // `--no-x` negates a boolean `--x`, unless `--no-x` is its own option.
            let found = found.or_else(|| {
                let positive = kebab.strip_prefix("no-")?;
                let arg = find_long(command, positive).filter(|arg| is_boolean(arg))?;
                Some((arg, positive.to_owned(), false))
            });
            let Some((arg, long, positive)) = found else {
                route_unknown(command, token, &mut kept, &mut forwarded);
                continue;
            };
            if is_boolean(arg) {
                let value = match value {
                    Some(value) => Some(value),
                    // As yargs, a boolean takes `true` or `false` after it.
                    None => match tokens.as_slice().first().and_then(|next| next.to_str()) {
                        Some(next @ ("true" | "false")) => {
                            let next = next.to_owned();
                            tokens.next();
                            Some(next)
                        }
                        _ => None,
                    },
                };
                // As yargs, any value other than `true` is false.
                let set = positive == value.as_deref().is_none_or(|value| value == "true");
                if set {
                    kept.push(format!("--{long}").into());
                } else if find_long(command, &format!("no-{long}")).is_some() {
                    kept.push(format!("--no-{long}").into());
                }
                continue;
            }
            match value {
                Some(value) => kept.push(format!("--{long}={value}").into()),
                None => {
                    kept.push(format!("--{long}").into());
                    take_values(arg, &mut tokens, &mut kept);
                }
            }
            continue;
        }
        if let Some(short) = text
            .strip_prefix('-')
            .and_then(|rest| rest.chars().next())
            .filter(char::is_ascii_alphabetic)
        {
            match command
                .get_arguments()
                .find(|arg| arg.get_short() == Some(short))
            {
                Some(arg) => {
                    let attached = text.len() > 2;
                    kept.push(token);
                    if !attached && !is_boolean(arg) {
                        take_values(arg, &mut tokens, &mut kept);
                    }
                }
                None => route_unknown(command, token, &mut kept, &mut forwarded),
            }
            continue;
        }
        if descending
            && let Some(subcommand) = command.get_subcommands().find(|subcommand| {
                subcommand.get_name() == text || subcommand.get_all_aliases().any(|a| a == text)
            })
        {
            command = subcommand;
            positionals = 0;
            kept.push(token);
            continue;
        }
        descending = false;
        route(command, token, &mut kept, &mut forwarded, &mut positionals);
    }
    if !forwarded.is_empty() || dash {
        kept.push("--".into());
        if command.get_name() != EXEC {
            kept.extend(forwarded);
        }
        kept.extend(after_dash);
    }
    kept
}

/// A positional: the command's own while it takes more, else an override.
fn route(
    command: &Command,
    token: OsString,
    kept: &mut Vec<OsString>,
    forwarded: &mut Vec<OsString>,
    positionals: &mut usize,
) {
    let own = command
        .get_positionals()
        .filter(|arg| !arg.is_last_set())
        .count();
    if forwards(command) && *positionals >= own {
        forwarded.push(token);
    } else {
        *positionals += 1;
        kept.push(token);
    }
}

/// An option the command does not define: an override where the command
/// forwards them, else left for clap to report.
fn route_unknown(
    command: &Command,
    token: OsString,
    kept: &mut Vec<OsString>,
    forwarded: &mut Vec<OsString>,
) {
    if forwards(command) {
        forwarded.push(token);
    } else {
        kept.push(token);
    }
}

fn forwards(command: &Command) -> bool {
    FORWARDING.contains(&command.get_name()) || command.get_name() == EXEC
}

/// Keeps the value an option takes from the following tokens: one, or for a
/// list every token up to the next option, as clap would read them.
fn take_values(arg: &Arg, tokens: &mut std::vec::IntoIter<OsString>, kept: &mut Vec<OsString>) {
    let Some(range) = arg.get_num_args() else {
        return;
    };
    let mut taken = 0;
    while taken < range.max_values() {
        let Some(next) = tokens.as_slice().first() else {
            break;
        };
        if next == "--" || next.to_str().is_some_and(|next| next.starts_with('-')) {
            break;
        }
        kept.push(tokens.next().unwrap());
        taken += 1;
    }
}

fn find_long<'a>(command: &'a Command, name: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn is_boolean(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::SetTrue)
}

/// `outputStyle` as `output-style`; kebab-case names are unchanged.
fn kebab(name: &str) -> String {
    let mut kebab = String::new();
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            kebab.push('-');
        }
        kebab.push(character.to_ascii_lowercase());
    }
    kebab
}

#[cfg(test)]
mod tests {
    use clap::{Args, Parser, Subcommand};

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[arg(long, global = true)]
        workspace: Option<String>,
        #[command(subcommand)]
        command: Commands,
    }

    #[derive(Subcommand)]
    enum Commands {
        Run {
            task: Option<String>,
            #[command(flatten)]
            options: Options,
        },
        RunMany {
            #[arg(short = 't', long, value_delimiter = ',', num_args = 1..)]
            targets: Vec<String>,
            #[command(flatten)]
            options: Options,
        },
        Exec {
            #[command(flatten)]
            options: Options,
        },
        Show {
            #[arg(long)]
            json: bool,
        },
    }

    #[derive(Args)]
    struct Options {
        #[arg(long)]
        nx_bail: bool,
        #[arg(long, hide = true)]
        no_nx_bail: bool,
        #[arg(long, alias = "disable-nx-cache")]
        skip_cache: bool,
        #[arg(short = 'c', long)]
        configuration: Option<String>,
        #[arg(last = true)]
        args: Vec<String>,
    }

    fn normalized(args: &[&str]) -> Vec<String> {
        use clap::CommandFactory;
        let args = std::iter::once("qk")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        normalize(args, &Cli::command())
            .into_iter()
            .skip(1)
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn unknown_options_and_extra_positionals_are_forwarded_in_order() {
        assert_eq!(
            normalized(&["run", "app:test", "--coverage", "x", "--max-workers", "2"]),
            [
                "run",
                "app:test",
                "--",
                "--coverage",
                "x",
                "--max-workers",
                "2"
            ]
        );
        // Unknown options keep their spelling; known ones take clap's.
        assert_eq!(
            normalized(&["run", "app:test", "--fooBar=1", "--nxBail", "-x"]),
            ["run", "app:test", "--nx-bail", "--", "--fooBar=1", "-x"]
        );
        assert_eq!(
            normalized(&[
                "run-many",
                "-t",
                "build",
                "test",
                "--coverage",
                "--",
                "--watch"
            ]),
            [
                "run-many",
                "-t",
                "build",
                "test",
                "--",
                "--coverage",
                "--watch"
            ]
        );
    }

    #[test]
    fn known_options_keep_their_values() {
        assert_eq!(
            normalized(&["run", "app:test", "-c", "prod", "--configuration", "ci"]),
            ["run", "app:test", "-c", "prod", "--configuration", "ci"]
        );
        assert_eq!(
            normalized(&["--workspace", "w", "run-many", "--workspace=v", "-t", "a"]),
            ["--workspace", "w", "run-many", "--workspace=v", "-t", "a"]
        );
    }

    #[test]
    fn booleans_take_values_and_negations_as_yargs_does() {
        assert_eq!(
            normalized(&["run", "a:b", "--nxBail=true"]),
            ["run", "a:b", "--nx-bail"]
        );
        assert_eq!(
            normalized(&["run", "a:b", "--nx-bail", "false"]),
            ["run", "a:b", "--no-nx-bail"]
        );
        assert_eq!(
            normalized(&["run", "a:b", "--no-nx-bail"]),
            ["run", "a:b", "--no-nx-bail"]
        );
        assert_eq!(
            normalized(&["run", "a:b", "--skipCache=yes"]),
            ["run", "a:b"]
        );
        assert_eq!(
            normalized(&["run", "a:b", "--disableNxCache"]),
            ["run", "a:b", "--disable-nx-cache"]
        );
        // `true` is the flag's value; another word is a positional.
        assert_eq!(
            normalized(&["run", "a:b", "--nx-bail", "x"]),
            ["run", "a:b", "--nx-bail", "--", "x"]
        );
    }

    #[test]
    fn exec_keeps_only_the_command_after_the_separator() {
        assert_eq!(
            normalized(&["exec", "--coverage", "--nxBail", "--", "echo", "--x"]),
            ["exec", "--nx-bail", "--", "echo", "--x"]
        );
    }

    #[test]
    fn other_commands_leave_unknown_options_to_clap() {
        assert_eq!(
            normalized(&["show", "--unknownThing"]),
            ["show", "--unknownThing"]
        );
        assert_eq!(normalized(&["show", "--json=false"]), ["show"]);
    }
}
