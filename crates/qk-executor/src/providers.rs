//! The variables a declared environment's `from` entries set: each the
//! output of a command printing an environment as JSON, run once per run,
//! and kept on disk until the files it names as its inputs change.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use qk_config::env::Provider;
use serde_json::{Value, json};

/// The variables `provider` sets, and what a task's key holds of it: the
/// command and its inputs' content when it names inputs, since its output
/// can hold paths that differ between machines, and otherwise a digest of
/// each value it printed.
pub fn load(
    provider: &Provider,
    inherited: &BTreeMap<OsString, OsString>,
    workspace_root: &Path,
) -> Result<(BTreeMap<String, String>, Value)> {
    let inputs = provider
        .inputs
        .as_ref()
        .map(|inputs| {
            inputs
                .iter()
                .map(|input| {
                    let digest = match std::fs::read(workspace_root.join(input)) {
                        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            "missing".to_owned()
                        }
                        Err(error) => {
                            return Err(error).with_context(|| format!("cannot read {input}"));
                        }
                    };
                    Ok((input.clone(), digest))
                })
                .collect::<Result<BTreeMap<_, _>>>()
        })
        .transpose()?;
    let mut hasher = blake3::Hasher::new();
    for part in [
        provider.command.as_bytes(),
        workspace_root.as_os_str().as_encoded_bytes(),
        std::env::consts::OS.as_bytes(),
        std::env::consts::ARCH.as_bytes(),
    ] {
        hasher.update(part);
        hasher.update(&[0]);
    }
    for (input, digest) in inputs.iter().flatten() {
        hasher.update(input.as_bytes());
        hasher.update(&[0]);
        hasher.update(digest.as_bytes());
        hasher.update(&[0]);
    }
    let digest = hasher.finalize().to_hex().to_string();
    let variables = output(
        provider,
        inputs.is_some(),
        &digest,
        inherited,
        workspace_root,
    )?;
    let keyed = match inputs {
        Some(inputs) => json!({ "command": provider.command, "inputs": inputs }),
        // A digest of each value rather than the value, which can be a
        // credential, as the history keeps what a key holds.
        None => {
            let digests: BTreeMap<&str, String> = variables
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str(),
                        blake3::hash(value.as_bytes()).to_hex().to_string(),
                    )
                })
                .collect();
            json!({ "command": provider.command, "values": digests })
        }
    };
    Ok((variables, keyed))
}

/// The command's variables: once per run for every task sharing it, and,
/// with inputs, from the copy kept on disk while they are unchanged.
fn output(
    provider: &Provider,
    kept: bool,
    digest: &str,
    inherited: &BTreeMap<OsString, OsString>,
    workspace_root: &Path,
) -> Result<BTreeMap<String, String>> {
    type Outputs = Mutex<HashMap<String, std::result::Result<BTreeMap<String, String>, String>>>;
    static RUN: OnceLock<Outputs> = OnceLock::new();
    let outputs = RUN.get_or_init(Default::default);
    // Held while the command runs, so tasks starting together run it once.
    let mut outputs = outputs.lock().unwrap();
    if let Some(output) = outputs.get(digest) {
        return output.clone().map_err(anyhow::Error::msg);
    }
    let file = kept
        .then(|| crate::bins::cache_directory().ok())
        .flatten()
        .map(|directory| directory.join("env").join(format!("{digest}.json")));
    let stored = file
        .as_ref()
        .and_then(|file| std::fs::read(file).ok())
        .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, String>>(&bytes).ok());
    let output = match stored {
        Some(variables) => Ok(variables),
        None => run(provider, inherited, workspace_root).inspect(|variables| {
            if let Some(file) = &file {
                store(file, variables);
            }
        }),
    };
    let output = output.map_err(|error| format!("{error:#}"));
    outputs.insert(digest.to_owned(), output.clone());
    output.map_err(anyhow::Error::msg)
}

fn run(
    provider: &Provider,
    inherited: &BTreeMap<OsString, OsString>,
    workspace_root: &Path,
) -> Result<BTreeMap<String, String>> {
    // With what qk inherited: the command, such as Nix, needs its own
    // settings, which the task's environment need not hold.
    let output = crate::process::shell(&provider.command)
        .current_dir(workspace_root)
        .env_clear()
        .envs(inherited)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("cannot run {:?}", provider.command))?;
    if !output.status.success() {
        bail!(
            "{:?} failed ({}): {}",
            provider.command,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "{:?} did not print an environment as JSON",
            provider.command
        )
    })?;
    parse(&value)
        .with_context(|| format!("cannot read the environment {:?} printed", provider.command))
}

/// The variables of an environment printed as JSON: Nix's
/// `{"variables": {NAME: {"type", "value"}}}`, of which the exported ones,
/// less those `nix develop` leaves as they are, or a flat `{NAME: value}`,
/// as direnv and mise print, where `null` leaves a variable unset.
fn parse(value: &Value) -> Result<BTreeMap<String, String>> {
    let object = value.as_object().context("it is not an object")?;
    if let Some(Value::Object(variables)) = object.get("variables") {
        // The variables `nix develop` does not take from the environment it
        // prints, as they describe the build sandbox rather than the shell.
        const IGNORED: [&str; 16] = [
            "BASHOPTS",
            "HOME",
            "NIX_BUILD_TOP",
            "NIX_ENFORCE_PURITY",
            "NIX_LOG_FD",
            "NIX_REMOTE",
            "PPID",
            "SHELLOPTS",
            "SSL_CERT_FILE",
            "TEMP",
            "TEMPDIR",
            "TERM",
            "TMP",
            "TMPDIR",
            "TZ",
            "UID",
        ];
        return Ok(variables
            .iter()
            .filter(|(name, _)| !IGNORED.contains(&name.as_str()))
            .filter_map(|(name, variable)| {
                (variable.get("type")? == "exported")
                    .then(|| Some((name.clone(), variable.get("value")?.as_str()?.to_owned())))
                    .flatten()
            })
            .collect());
    }
    object
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, value)| {
            let value = value
                .as_str()
                .with_context(|| format!("{name} is neither a string nor null"))?;
            Ok((name.clone(), value.to_owned()))
        })
        .collect()
}

/// Keeps `variables` in `file`, written beside it and renamed into place so
/// a run reading it never sees it half written. Failing to keep it only
/// means running the command again.
fn store(file: &Path, variables: &BTreeMap<String, String>) {
    let Some(directory) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(directory).is_err() {
        return;
    }
    let partial = file.with_extension(format!("{}.partial", std::process::id()));
    let Ok(bytes) = serde_json::to_vec(variables) else {
        return;
    };
    if write_private(&partial, &bytes).is_err() || std::fs::rename(&partial, file).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
}

/// Writes `bytes` to `path`, readable by its owner alone on Unix: what a
/// command prints can hold credentials.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nix_and_flat_environments() {
        let nix = json!({ "variables": {
            "PATH": { "type": "exported", "value": "/nix/store/a-tool/bin" },
            "HOME": { "type": "exported", "value": "/homeless-shelter" },
            "shellHook": { "type": "var", "value": "echo hi" },
            "buildInputs": { "type": "array", "value": ["a"] },
        }, "bashFunctions": {} });
        assert_eq!(
            parse(&nix).unwrap(),
            BTreeMap::from([("PATH".to_owned(), "/nix/store/a-tool/bin".to_owned())])
        );
        let flat = json!({ "A": "a", "UNSET": null });
        assert_eq!(
            parse(&flat).unwrap(),
            BTreeMap::from([("A".to_owned(), "a".to_owned())])
        );
        assert!(parse(&json!({ "A": 1 })).is_err());
        assert!(parse(&json!(["A"])).is_err());
    }
}
