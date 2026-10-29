//! A task's declared environment: `qk:env` on a target, or nx.json's
//! `qk:defaultEnv` for every target that names none, built from the
//! environments nx.json names in `qk:environments`. A task with one starts
//! from only the variables it lets through, rather than all that qk inherits.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{Target, WorkspaceConfig};

/// The environments nx.json names, each an array of entries.
pub const ENVIRONMENTS: &str = "qk:environments";
/// The environment of every target that does not name one.
pub const DEFAULT: &str = "qk:defaultEnv";
/// A target's environment.
pub const TARGET: &str = "qk:env";

/// A task's environment, its entries flattened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    /// Every entry, named environments replaced by theirs, as the key holds
    /// them.
    entries: Vec<Value>,
    /// Patterns of the variables passed through and keyed.
    inherit: Vec<String>,
    /// Patterns of the variables passed through without being keyed.
    pass: Vec<String>,
    /// Variables set, the last entry setting one winning.
    set: BTreeMap<String, String>,
    /// The values of the variables it inherited, once filtered: what the
    /// key holds, rather than the values the task's own configuration
    /// gives them afterwards, which its definition keys.
    inherited: BTreeMap<String, Option<String>>,
    /// The executables `PATH` holds, by name, in order.
    bins: Vec<String>,
    /// The directories `PATH` holds after them, as written.
    dirs: Vec<String>,
    /// Where each of `bins` resolved to, for the key: the binary itself,
    /// whatever links led to it.
    binaries: BTreeMap<String, String>,
    /// The inherited variables it left out, by name, to name when the task
    /// fails.
    left_out: Vec<String>,
    /// The commands of `bins` keyed by their content.
    by_content: std::collections::BTreeSet<String>,
    /// The commands whose output sets variables, in order.
    providers: Vec<Provider>,
    /// What they set, once run, applied over what it inherits.
    provided: BTreeMap<String, String>,
    /// What the key holds of each of them.
    provided_keys: Vec<Value>,
}

/// A command printing an environment, as `nix print-dev-env --json` does,
/// whose variables a task's environment sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// The command, run by `sh` in the workspace root.
    pub command: String,
    /// The files its output follows from, relative to the workspace root:
    /// with them, the key holds their content rather than the output, and
    /// the output is kept until they change.
    pub inputs: Option<Vec<String>>,
}

impl Environment {
    /// The environment a target runs in: its own `qk:env`, or the
    /// workspace's `qk:defaultEnv`. `None`, as `false` or without either,
    /// passes every inherited variable through, as without the setting.
    pub fn of(config: &WorkspaceConfig, target: &Target) -> Result<Option<Self>> {
        let (value, what) = match target.extra.get(TARGET) {
            Some(value) => (value, TARGET),
            None => match config.extra.get(DEFAULT) {
                Some(value) => (value, DEFAULT),
                None => return Ok(None),
            },
        };
        let entries = match value {
            Value::Bool(false) => return Ok(None),
            Value::Array(entries) => entries,
            _ => bail!("{what} must be an array of environments and entries, or false"),
        };
        let environments = match config.extra.get(ENVIRONMENTS) {
            None => None,
            Some(Value::Object(environments)) => Some(environments),
            Some(_) => bail!("{ENVIRONMENTS} must be an object of environments"),
        };
        let mut environment = Self::default();
        environment
            .add(entries, environments, &mut Vec::new())
            .with_context(|| format!("invalid {what}"))?;
        if environment.builds_path() {
            let path = if cfg!(windows) { "Path" } else { "PATH" };
            if matches_any(&environment.inherit, path)
                || matches_any(&environment.pass, path)
                || environment
                    .set
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("PATH"))
            {
                bail!(
                    "invalid {what}: PATH is built from its bin and dirs entries, so it cannot also be inherited, passed or set"
                );
            }
        }
        Ok(Some(environment))
    }

    fn add(
        &mut self,
        entries: &[Value],
        environments: Option<&serde_json::Map<String, Value>>,
        within: &mut Vec<String>,
    ) -> Result<()> {
        for entry in entries {
            match entry {
                Value::String(name) => {
                    if within.contains(name) {
                        bail!("environment {name:?} includes itself");
                    }
                    let Some(named) = environments.and_then(|environments| environments.get(name))
                    else {
                        bail!("no environment {name:?} in {ENVIRONMENTS}");
                    };
                    let named = named
                        .as_array()
                        .with_context(|| format!("environment {name:?} must be an array"))?;
                    within.push(name.clone());
                    self.add(named, environments, within)
                        .with_context(|| format!("in environment {name:?}"))?;
                    within.pop();
                }
                Value::Object(object) if object.len() == 1 => {
                    let (kind, value) = object.iter().next().unwrap();
                    match kind.as_str() {
                        "inherit" | "pass" => {
                            let patterns = value
                                .as_array()
                                .and_then(|patterns| {
                                    patterns
                                        .iter()
                                        .map(|pattern| pattern.as_str().map(str::to_owned))
                                        .collect::<Option<Vec<_>>>()
                                })
                                .with_context(|| {
                                    format!("{kind} must be an array of variable names")
                                })?;
                            for pattern in &patterns {
                                if pattern.is_empty() || pattern.contains('=') {
                                    bail!("{pattern:?} is not a variable name");
                                }
                            }
                            if kind == "inherit" {
                                self.inherit.extend(patterns);
                            } else {
                                self.pass.extend(patterns);
                            }
                        }
                        "set" => {
                            let values = value
                                .as_object()
                                .context("set must be an object of variables")?;
                            for (name, value) in values {
                                if name.is_empty() || name.contains('=') {
                                    bail!("{name:?} is not a variable name");
                                }
                                let value = value
                                    .as_str()
                                    .with_context(|| format!("set.{name} must be a string"))?;
                                self.set.insert(name.clone(), value.to_owned());
                            }
                        }
                        "bin" | "dirs" => {
                            let items = value
                                .as_array()
                                .and_then(|items| {
                                    items
                                        .iter()
                                        .map(|item| match item {
                                            Value::String(name) => Some(Ok(name.clone())),
                                            // A command keyed by its content.
                                            Value::Object(object) if kind == "bin" => {
                                                Some(self.content_keyed(object))
                                            }
                                            _ => None,
                                        })
                                        .collect::<Option<Result<Vec<_>>>>()
                                })
                                .with_context(|| {
                                    if kind == "bin" {
                                        "bin must be an array of command names, or objects with a name and key".to_owned()
                                    } else {
                                        "dirs must be an array of strings".to_owned()
                                    }
                                })??;
                            for item in items {
                                if item.is_empty() {
                                    bail!("{kind} holds an empty string");
                                }
                                if kind == "bin" {
                                    if item.contains(['/', '\\']) {
                                        bail!(
                                            "{item:?} is not a command name: bin names commands found on PATH, and dirs adds a directory"
                                        );
                                    }
                                    if !self.bins.contains(&item) {
                                        self.bins.push(item);
                                    }
                                } else if !self.dirs.contains(&item) {
                                    self.dirs.push(item);
                                }
                            }
                        }
                        "from" => {
                            let object = value
                                .as_object()
                                .context("from must be an object with a command")?;
                            for field in object.keys() {
                                if !matches!(field.as_str(), "command" | "inputs") {
                                    bail!("from.{field} is not a field qk knows");
                                }
                            }
                            let command = object
                                .get("command")
                                .and_then(Value::as_str)
                                .filter(|command| !command.trim().is_empty())
                                .context("from.command must be a command")?
                                .to_owned();
                            let inputs = match object.get("inputs") {
                                None => None,
                                Some(inputs) => Some(
                                    inputs
                                        .as_array()
                                        .and_then(|inputs| {
                                            inputs
                                                .iter()
                                                .map(|input| input.as_str().map(str::to_owned))
                                                .collect::<Option<Vec<_>>>()
                                        })
                                        .context("from.inputs must be an array of paths")?,
                                ),
                            };
                            self.providers.push(Provider { command, inputs });
                        }
                        other => bail!("{other:?} is not an environment entry"),
                    }
                    self.entries.push(entry.clone());
                }
                _ => bail!(
                    "an environment entry is the name of an environment or an object with one of inherit, pass, set, bin, dirs or from"
                ),
            }
        }
        Ok(())
    }

    /// Keeps of `env`, the variables a task would inherit, only those the
    /// environment lets through, then sets its own, and notes the values of
    /// those it inherits for its key. A variable both inherited and passed is
    /// an error: whether it is keyed would be unclear.
    pub fn filter(&mut self, env: &mut BTreeMap<OsString, OsString>) -> Result<()> {
        let mut kept = BTreeMap::new();
        self.inherited.clear();
        self.left_out.clear();
        for (name, value) in std::mem::take(env) {
            let text = name.to_string_lossy().into_owned();
            let inherited = matches_any(&self.inherit, &text);
            let passed = matches_any(&self.pass, &text);
            if inherited && passed {
                bail!("the variable {text} is both inherited and passed by the task's environment");
            }
            if inherited && !self.set.contains_key(&text) {
                self.inherited
                    .insert(text.clone(), value.to_str().map(str::to_owned));
            }
            if inherited || passed {
                kept.insert(name, value);
            } else {
                self.left_out.push(text);
            }
        }
        for (name, value) in &self.provided {
            if name.eq_ignore_ascii_case("PATH")
                && (matches_any(&self.inherit, name) || matches_any(&self.pass, name))
            {
                bail!(
                    "PATH is set by a from entry of the task's environment, so it cannot also be inherited or passed"
                );
            }
            kept.insert(name.into(), value.into());
        }
        for (name, value) in &self.set {
            // A name differing only in case is the same variable on Windows.
            if cfg!(windows) {
                kept.retain(|existing: &OsString, _| {
                    !existing.to_string_lossy().eq_ignore_ascii_case(name)
                });
            }
            kept.insert(name.into(), value.into());
        }
        *env = kept;
        Ok(())
    }

    /// What a task's key holds of its environment: its entries, and the
    /// value each variable it inherits had, as [`Environment::filter`] found
    /// it. A passed variable is keyed by name only, as the entries hold it.
    pub fn keyed(&self) -> Value {
        let mut keyed = json!({ "entries": self.entries, "inherited": self.inherited });
        // A binary is keyed by where it resolved to, rather than by the
        // directory linking to it, which differs from machine to machine.
        if !self.binaries.is_empty() {
            keyed["binaries"] = json!(self.binaries);
        }
        if !self.provided_keys.is_empty() {
            keyed["provided"] = json!(self.provided_keys);
        }
        keyed
    }

    /// The inherited variables it left out that `env`, the task's
    /// environment once built, does not hold: those its dotenv files and
    /// settings do not set again.
    pub fn left_out(&self, env: &BTreeMap<OsString, OsString>) -> Vec<&str> {
        self.left_out
            .iter()
            .filter(|name| !env.contains_key(std::ffi::OsStr::new(name.as_str())))
            .map(String::as_str)
            .collect()
    }

    /// Whether the environment builds `PATH` from `bin` and `dirs` entries.
    pub fn builds_path(&self) -> bool {
        !self.bins.is_empty() || !self.dirs.is_empty()
    }

    /// The commands `PATH` holds, by name.
    pub fn bins(&self) -> &[String] {
        &self.bins
    }

    /// The directories `PATH` holds after them, as written.
    pub fn dirs(&self) -> &[String] {
        &self.dirs
    }

    /// The commands whose output sets variables.
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Notes what [`Environment::providers`] set, with what the key holds of
    /// each; [`Environment::filter`] applies it.
    pub fn provide(&mut self, variables: BTreeMap<String, String>, keys: Vec<Value>) {
        self.provided = variables;
        self.provided_keys = keys;
    }

    /// The `PATH` a from entry set, which a `PATH` built from `bin` and
    /// `dirs` entries ends with.
    pub fn provided_path(&self) -> Option<&str> {
        self.provided
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value.as_str())
    }

    /// A `bin` item written as an object: `{"name", "key"}`, where `key` is
    /// `"path"`, as for a name alone, or `"content"`.
    fn content_keyed(&mut self, object: &serde_json::Map<String, Value>) -> Result<String> {
        for field in object.keys() {
            if !matches!(field.as_str(), "name" | "key") {
                bail!("bin.{field} is not a field qk knows");
            }
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .context("a bin object needs a name")?
            .to_owned();
        match object.get("key").and_then(Value::as_str) {
            Some("content") => {
                self.by_content.insert(name.clone());
            }
            Some("path") | None => {}
            Some(other) => bail!("bin key {other:?} is neither \"path\" nor \"content\""),
        }
        Ok(name)
    }

    /// Whether the command `name` names is keyed by its content rather than
    /// by where it resolved to.
    pub fn keyed_by_content(&self, name: &str) -> bool {
        self.by_content.contains(name)
    }

    /// Notes where each of [`Environment::bins`] resolved to, for the key.
    pub fn resolved(&mut self, binaries: BTreeMap<String, String>) {
        self.binaries = binaries;
    }

    /// The patterns of the variables passed through and keyed.
    pub fn inherited(&self) -> &[String] {
        &self.inherit
    }

    /// The patterns of the variables passed through without being keyed.
    pub fn passed(&self) -> &[String] {
        &self.pass
    }

    /// The variables the environment sets.
    pub fn sets(&self) -> &BTreeMap<String, String> {
        &self.set
    }
}

/// Every environment the configuration names, and each target's, checked:
/// the errors, each with where it is.
pub fn validate<'a>(
    config: &WorkspaceConfig,
    targets: impl IntoIterator<Item = (String, &'a Target)>,
) -> Vec<(String, String)> {
    let mut errors = Vec::new();
    if let Some(value) = config.extra.get(ENVIRONMENTS) {
        match value {
            Value::Object(environments) => {
                for name in environments.keys() {
                    let mut target = Target::default();
                    target.extra.insert(TARGET.into(), json!([name]));
                    if let Err(error) = Environment::of(config, &target) {
                        errors.push((
                            format!("nx.json.{ENVIRONMENTS}.{name}"),
                            format!("{error:#}"),
                        ));
                    }
                }
            }
            _ => errors.push((
                format!("nx.json.{ENVIRONMENTS}"),
                format!("{ENVIRONMENTS} must be an object of environments"),
            )),
        }
    }
    let mut default_checked = false;
    for (location, target) in targets {
        if !target.extra.contains_key(TARGET) {
            if default_checked {
                continue;
            }
            default_checked = true;
        }
        if let Err(error) = Environment::of(config, target) {
            let location = if target.extra.contains_key(TARGET) {
                location
            } else {
                format!("nx.json.{DEFAULT}")
            };
            errors.push((location, format!("{error:#}")));
        }
    }
    let mut seen = BTreeSet::new();
    errors.retain(|error| seen.insert(error.clone()));
    errors
}

/// Whether `name` matches one of `patterns`, in which `*` stands for any
/// run of characters. On Windows, where variable names are not cased, case
/// is ignored.
fn matches_any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pattern| {
        if cfg!(windows) {
            wildcard(&pattern.to_ascii_uppercase(), &name.to_ascii_uppercase())
        } else {
            wildcard(pattern, name)
        }
    })
}

fn wildcard(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, rest)) => {
            let Some(name) = name.strip_prefix(prefix) else {
                return false;
            };
            // Each place the rest of the pattern could start.
            (0..=name.len())
                .filter(|start| name.is_char_boundary(*start))
                .any(|start| wildcard(rest, &name[start..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(value: Value) -> WorkspaceConfig {
        serde_json::from_value(value).unwrap()
    }

    fn target(value: Value) -> Target {
        let mut target = Target::default();
        target.extra.insert(TARGET.into(), value);
        target
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect()
    }

    #[test]
    fn matches_names_with_wildcards() {
        assert!(wildcard("LC_*", "LC_ALL"));
        assert!(wildcard("LC_*", "LC_"));
        assert!(!wildcard("LC_*", "LANG"));
        assert!(wildcard("*_PROXY", "HTTPS_PROXY"));
        assert!(wildcard("A*B*C", "AxxBxxC"));
        assert!(!wildcard("A*B*C", "AxxC"));
        assert!(wildcard("*", "ANY"));
        assert!(!wildcard("HOME", "HOMEDIR"));
    }

    #[test]
    fn flattens_named_environments_in_order() {
        let config = config(json!({
            "qk:environments": {
                "locale": [{ "inherit": ["LANG", "LC_*"] }],
                "git": ["locale", { "pass": ["HOME"] }, { "set": { "TZ": "UTC" } }],
            }
        }));
        let mut environment =
            Environment::of(&config, &target(json!(["git", { "set": { "TZ": "CET" } }])))
                .unwrap()
                .unwrap();
        assert_eq!(environment.inherited(), ["LANG", "LC_*"]);
        assert_eq!(environment.passed(), ["HOME"]);
        assert_eq!(environment.sets()["TZ"], "CET");
        let mut inherited = env(&[
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "C"),
            ("HOME", "/home/a"),
            ("SECRET", "x"),
            ("TZ", "PST"),
        ]);
        environment.filter(&mut inherited).unwrap();
        assert_eq!(
            inherited,
            env(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_ALL", "C"),
                ("HOME", "/home/a"),
                ("TZ", "CET"),
            ])
        );
        // The passed variable's value is not keyed; the inherited ones are.
        let keyed = environment.keyed();
        assert_eq!(
            keyed["inherited"],
            json!({ "LANG": "en_US.UTF-8", "LC_ALL": "C" })
        );
        environment
            .filter(&mut env(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_ALL", "C"),
                ("HOME", "/home/b"),
            ]))
            .unwrap();
        assert_eq!(environment.keyed(), keyed);
    }

    #[test]
    fn a_target_without_one_takes_the_default() {
        let config = config(json!({ "qk:defaultEnv": [{ "inherit": ["LANG"] }] }));
        let default = Environment::of(&config, &Target::default())
            .unwrap()
            .unwrap();
        assert_eq!(default.inherited(), ["LANG"]);
        // An empty environment lets nothing through.
        let mut empty = Environment::of(&config, &target(json!([])))
            .unwrap()
            .unwrap();
        let mut inherited = env(&[("LANG", "C")]);
        empty.filter(&mut inherited).unwrap();
        assert!(inherited.is_empty());
        // `false` opts out of the default.
        assert_eq!(
            Environment::of(&config, &target(json!(false))).unwrap(),
            None
        );
        // Without either, nothing changes.
        assert_eq!(
            Environment::of(&WorkspaceConfig::default(), &Target::default()).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_what_it_cannot_follow() {
        let config = config(json!({
            "qk:environments": {
                "loop": ["other"],
                "other": ["loop"],
                "both": [{ "inherit": ["HOME"] }, { "pass": ["HO*"] }],
            }
        }));
        for (value, message) in [
            (json!(["loop"]), "includes itself"),
            (json!(["missing"]), "no environment \"missing\""),
            (
                json!([{ "path": ["nix"] }]),
                "\"path\" is not an environment entry",
            ),
            (
                json!([{ "from": { "command": "" } }]),
                "from.command must be a command",
            ),
            (
                json!([{ "from": { "command": "x", "input": [] } }]),
                "from.input is not a field",
            ),
            (json!([{ "bin": ["./node"] }]), "is not a command name"),
            (
                json!([{ "bin": [{ "name": "node", "key": "hash" }] }]),
                "neither \"path\" nor \"content\"",
            ),
            (
                json!([{ "bin": [{ "key": "content" }] }]),
                "a bin object needs a name",
            ),
            (
                json!([{ "bin": ["node"] }, { "pass": ["PATH"] }]),
                "PATH is built from its bin and dirs entries",
            ),
            (json!([{ "inherit": "HOME" }]), "must be an array"),
            (json!([{ "set": { "A": 1 } }]), "set.A must be a string"),
            (json!(true), "must be an array"),
        ] {
            let error = Environment::of(&config, &target(value.clone())).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{value}: {error:#}");
        }
        let mut both = Environment::of(&config, &target(json!(["both"])))
            .unwrap()
            .unwrap();
        let error = both.filter(&mut env(&[("HOME", "/h")])).unwrap_err();
        assert!(error.to_string().contains("both inherited and passed"));
        let errors = validate(
            &config,
            [("app:build".to_owned(), &target(json!(["missing"])))],
        );
        let locations: Vec<&str> = errors
            .iter()
            .map(|(location, _)| location.as_str())
            .collect();
        assert_eq!(
            locations,
            [
                "nx.json.qk:environments.loop",
                "nx.json.qk:environments.other",
                "app:build"
            ]
        );
    }
}
