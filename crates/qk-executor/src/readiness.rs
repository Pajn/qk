//! A continuous target's `readyWhen`, as Nx 24 checks it: a url to request, a
//! port to connect to, a command that must succeed, or text to find in the
//! output. Tasks that depend on the target with `waitFor: "ready"` start once
//! the check passes.
//!
//! This is unrelated to the run-commands option of the same name, which marks
//! the run-commands task itself as ready.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use command_group::CommandGroup;
use serde_json::{Map, Value};

use crate::tokens::{closing_brace, env_name, fallbacks};

// As in Nx 24.0.0-rc.0, long enough for a dev server's cold first build.
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// As in Nx 24.0.0-beta.3, probes retry every 250 ms unless `interval` says
/// otherwise.
const DEFAULT_INTERVAL_MS: u64 = 250;
const MAX_DURATION_MS: u64 = 2_147_483_647;
/// How often a wait looks at whether it has been stopped or decided.
const POLL: Duration = Duration::from_millis(20);

/// What tells that a task is ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// A GET answered with a status from 200 to 403.
    Url(String),
    /// A TCP connection, to loopback unless a host is named.
    Port { port: u16, host: Option<String> },
    /// A shell command at the workspace root exiting with 0, run with the
    /// environment the task's commands run with and its options as JSON in
    /// `NX_TASK_OPTIONS`, as in Nx 24.
    Command {
        command: String,
        env: BTreeMap<OsString, OsString>,
    },
    /// Every one of these appearing in the task's output.
    LogMatches(Vec<String>),
}

/// A resolved `readyWhen`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyCheck {
    pub probe: Probe,
    pub timeout: Duration,
    pub interval: Duration,
}

impl ReadyCheck {
    pub fn describe(&self) -> String {
        match &self.probe {
            Probe::Url(url) => format!("url {url}"),
            Probe::Port {
                port,
                host: Some(host),
            } => format!("port {host}:{port}"),
            Probe::Port { port, host: None } => format!("port {port}"),
            Probe::Command { command, .. } => format!("command \"{command}\""),
            Probe::LogMatches(matches) => format!(
                "logMatches {}",
                matches
                    .iter()
                    .map(|text| format!("\"{text}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

const PENDING: u8 = 0;
const READY: u8 = 1;
const FAILED: u8 = 2;

/// What a task's readiness check concluded, shared between the check, the
/// task's output and the scheduler. The first conclusion stands.
#[derive(Debug, Default)]
pub struct Verdict {
    state: AtomicU8,
    reason: Mutex<Option<String>>,
}

impl Verdict {
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::SeqCst) == READY
    }

    pub fn is_decided(&self) -> bool {
        self.state.load(Ordering::SeqCst) != PENDING
    }

    /// Why the check failed, once it has.
    pub fn failure(&self) -> Option<String> {
        (self.state.load(Ordering::SeqCst) == FAILED)
            .then(|| self.reason.lock().unwrap().clone().unwrap_or_default())
    }

    pub fn set_ready(&self) {
        let _ = self
            .state
            .compare_exchange(PENDING, READY, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Records a failure unless the check was decided already, and says
    /// whether this call decided it.
    pub fn fail(&self, reason: String) -> bool {
        let mut stored = self.reason.lock().unwrap();
        let decided = self
            .state
            .compare_exchange(PENDING, FAILED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if decided {
            *stored = Some(reason);
        }
        decided
    }
}

/// What `readyWhen` placeholders read.
pub struct Scope<'a> {
    pub workspace_root: &'a str,
    pub project_root: &'a str,
    pub project_name: &'a str,
    /// The options the task runs with, its arguments applied.
    pub options: &'a Map<String, Value>,
    /// The environment the task runs with.
    pub env: &'a BTreeMap<String, String>,
}

/// How a placeholder treats what it reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Text,
    /// `true` and `false`, which a flag without a value gives, are unset.
    Host,
    /// As `Host`, and an empty value is `0`, a port the server picks.
    Port,
}

/// Resolves a target's `readyWhen`: `None` when the server picks its own
/// port, which leaves nothing to check.
pub fn resolve(task: &str, ready_when: &Value, scope: &Scope) -> Result<Option<ReadyCheck>> {
    let invalid = |reason: String| -> anyhow::Error {
        anyhow::anyhow!("Task \"{task}\" has an invalid \"readyWhen\": {reason}.")
    };
    let Value::Object(config) = ready_when else {
        return Err(invalid("expected an object".into()));
    };
    for key in config.keys() {
        if !matches!(
            key.as_str(),
            "url" | "port" | "host" | "command" | "logMatches" | "timeout" | "interval"
        ) {
            return Err(invalid(format!("unknown key \"{key}\"")));
        }
    }
    let present: Vec<&str> = ["url", "port", "command", "logMatches"]
        .into_iter()
        .filter(|key| config.get(*key).is_some_and(|value| !value.is_null()))
        .collect();
    let [kind] = present[..] else {
        return Err(invalid(
            "expected exactly one of \"url\", \"port\", \"command\", \"logMatches\"".into(),
        ));
    };
    let duration = |key: &str, default: Option<u64>| -> Result<Option<Duration>> {
        match config.get(key).filter(|value| !value.is_null()) {
            None => Ok(default.map(Duration::from_millis)),
            Some(value) => match value
                .as_u64()
                .filter(|ms| (1..=MAX_DURATION_MS).contains(ms))
            {
                Some(ms) => Ok(Some(Duration::from_millis(ms))),
                None => Err(invalid(format!(
                    "\"{key}\" must be an integer from 1 to {MAX_DURATION_MS}"
                ))),
            },
        }
    };
    let timeout = duration("timeout", Some(DEFAULT_TIMEOUT_MS))?.unwrap();
    let interval = duration("interval", Some(DEFAULT_INTERVAL_MS))?.unwrap();
    let text = |key: &str, field: Field| -> Result<Option<Value>> {
        match config.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(template)) => render(template, scope, field)
                .map(|text| Some(Value::String(text)))
                .map_err(invalid),
            Some(other) => Ok(Some(other.clone())),
        }
    };
    let probe = match kind {
        "url" => match text("url", Field::Text)? {
            Some(Value::String(url))
                if (url.starts_with("http://") || url.starts_with("https://"))
                    && url.parse::<ureq::http::Uri>().is_ok() =>
            {
                Probe::Url(url)
            }
            _ => return Err(invalid("\"url\" must be an http or https URL".into())),
        },
        "port" => {
            let port = text("port", Field::Port)?;
            let host = match text("host", Field::Host)? {
                None => None,
                Some(Value::String(host)) if !host.is_empty() && !host.contains(['{', '}']) => {
                    Some(host)
                }
                Some(Value::String(host)) if host.contains(['{', '}']) => {
                    return Err(invalid("\"host\" contains an unknown placeholder".into()));
                }
                _ => return Err(invalid("\"host\" must be a non-empty string".into())),
            };
            let port = match port {
                Some(Value::String(port)) if port == "auto" => 0,
                Some(Value::String(port)) if port.contains(['{', '}']) => {
                    return Err(invalid("\"port\" contains an unknown placeholder".into()));
                }
                Some(Value::String(port)) => port.parse::<u64>().unwrap_or(u64::MAX),
                Some(Value::Number(port)) => port.as_u64().unwrap_or(u64::MAX),
                _ => u64::MAX,
            };
            match port {
                // The server picks its port when it starts, so nothing can be
                // checked: it is ready once started.
                0 => return Ok(None),
                1..=65535 => Probe::Port {
                    port: port as u16,
                    host,
                },
                _ => {
                    return Err(invalid(
                        "\"port\" must be an integer from 0 to 65535, or \"auto\"".into(),
                    ));
                }
            }
        }
        "command" => match text("command", Field::Text)? {
            Some(Value::String(command)) if !command.is_empty() => Probe::Command {
                command,
                env: BTreeMap::new(),
            },
            _ => return Err(invalid("\"command\" must be a non-empty string".into())),
        },
        _ => {
            let templates = match &config["logMatches"] {
                Value::Array(templates) => templates.clone(),
                single => vec![single.clone()],
            };
            let mut matches = Vec::new();
            for template in &templates {
                match template {
                    Value::String(template) if !template.is_empty() => {
                        matches.push(render(template, scope, Field::Text).map_err(invalid)?);
                    }
                    _ => {
                        return Err(invalid(
                            "\"logMatches\" must be a non-empty string or an array of them".into(),
                        ));
                    }
                }
            }
            if matches.is_empty() || matches.iter().any(String::is_empty) {
                return Err(invalid(
                    "\"logMatches\" must be a non-empty string or an array of them".into(),
                ));
            }
            Probe::LogMatches(matches)
        }
    };
    Ok(Some(ReadyCheck {
        probe,
        timeout,
        interval,
    }))
}

/// A `readyWhen` string with its placeholders replaced. Braces that are no
/// placeholder stay as written.
fn render(template: &str, scope: &Scope, field: Field) -> std::result::Result<String, String> {
    let mut result = String::new();
    let mut index = 0;
    while let Some(offset) = template[index..].find('{') {
        let open = index + offset;
        let resolved = match closing_brace(template, open) {
            Some(close) => {
                placeholder(&template[open..=close], scope, field)?.map(|value| (value, close))
            }
            None => None,
        };
        match resolved {
            Some((value, close)) => {
                result.push_str(&template[index..open]);
                result.push_str(&value);
                index = close + 1;
            }
            None => {
                result.push_str(&template[index..=open]);
                index = open + 1;
            }
        }
    }
    result.push_str(&template[index..]);
    Ok(result)
}

/// The value a `{…}` placeholder stands for, or `None` when it is no
/// placeholder.
fn placeholder(
    raw: &str,
    scope: &Scope,
    field: Field,
) -> std::result::Result<Option<String>, String> {
    let sources = fallbacks(&raw[1..raw.len() - 1]);
    let read = |source: &str| -> Option<Option<Value>> {
        if let Some(name) = env_name(source) {
            return Some(scope.env.get(name).cloned().map(Value::String));
        }
        if let Some(path) = source.strip_prefix("options.") {
            let mut value = Some(&Value::Object(scope.options.clone()));
            let mut owned = None;
            for key in path.split('.') {
                owned = value.and_then(|value| value.get(key)).cloned();
                value = owned.as_ref();
            }
            return Some(owned);
        }
        match source {
            "workspaceRoot" => Some(Some(Value::String(scope.workspace_root.into()))),
            "projectRoot" => Some(Some(Value::String(scope.project_root.into()))),
            "projectName" => Some(Some(Value::String(scope.project_name.into()))),
            _ => None,
        }
    };
    if read(sources[0]).is_none() {
        return Ok(None);
    }
    for (index, source) in sources.iter().enumerate() {
        let value = match read(source) {
            Some(value) => value,
            None if index + 1 == sources.len() => {
                Some(Value::String(render(source, scope, Field::Text)?))
            }
            None => {
                return Err(format!(
                    "\"{raw}\" has a fallback after the literal \"{source}\" that can never be used"
                ));
            }
        };
        let value = match value {
            Some(Value::String(text)) if text.is_empty() && field == Field::Port => {
                Value::Number(0.into())
            }
            Some(value) => value,
            None => continue,
        };
        let unset = match &value {
            Value::Null => true,
            Value::String(text) => {
                text.is_empty()
                    || (field != Field::Text && matches!(text.as_str(), "true" | "false"))
            }
            Value::Bool(_) => field != Field::Text,
            _ => false,
        };
        if unset {
            continue;
        }
        return match value {
            Value::String(text) => Ok(Some(text)),
            Value::Number(number) => Ok(Some(number.to_string())),
            Value::Bool(flag) => Ok(Some(flag.to_string())),
            _ => Err(format!("\"{raw}\" is not a string or a number")),
        };
    }
    Err(format!("\"{raw}\" has no value"))
}

/// Checks until the task is ready, the check times out, or `stop` is set,
/// and records the conclusion in `verdict`. `logMatches` is decided by the
/// task's output, so this only times it out.
pub fn wait(task: &str, check: &ReadyCheck, root: &Path, verdict: &Verdict, stop: &AtomicBool) {
    let deadline = Instant::now() + check.timeout;
    loop {
        if verdict.is_decided() || stop.load(Ordering::SeqCst) {
            return;
        }
        let ready = match &check.probe {
            Probe::Url(url) => probe_url(url, deadline),
            Probe::Port { port, host } => probe_port(*port, host.as_deref(), deadline),
            Probe::Command { command, env } => probe_command(command, env, root, deadline, stop),
            Probe::LogMatches(_) => false,
        };
        if ready {
            verdict.set_ready();
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            let longer = (check.timeout.as_millis() as u64 * 2).min(MAX_DURATION_MS);
            verdict.fail(format!(
                "Task \"{task}\" did not become ready within {}ms (readyWhen: {}). If it needs \
                 longer, set \"timeout\": {longer} in the target's \"readyWhen\", as \
                 {{ \"...\": true, \"timeout\": {longer} }} to keep a check a target default \
                 sets. To change the check, set \"readyWhen\" on the target or in \
                 \"targetDefaults\" in nx.json, or set it to null to turn it off.",
                check.timeout.as_millis(),
                check.describe()
            ));
            return;
        }
        let pause = if matches!(check.probe, Probe::LogMatches(_)) {
            deadline - now
        } else {
            check.interval.min(deadline - now)
        };
        let until = now + pause;
        while Instant::now() < until {
            if verdict.is_decided() || stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
        }
    }
}

fn unbracketed(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

/// A wildcard means loopback of its own family, as Nx probes it.
fn destination(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) if v4.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(v6) if v6.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        _ => ip,
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// The address webpack-dev-server and the Rspack dev server pick for the
/// `local-ip` hosts: the machine's own outward address of that family, or
/// loopback when it has none.
fn local_ip(v6: bool) -> IpAddr {
    let (bind, probe): (SocketAddr, SocketAddr) = if v6 {
        (
            (Ipv6Addr::UNSPECIFIED, 0).into(),
            "[2001:4860:4860::8888]:53".parse().unwrap(),
        )
    } else {
        (
            (Ipv4Addr::UNSPECIFIED, 0).into(),
            "8.8.8.8:53".parse().unwrap(),
        )
    };
    // Connecting a UDP socket sends nothing; it only picks the route.
    UdpSocket::bind(bind)
        .and_then(|socket| socket.connect(probe).and_then(|()| socket.local_addr()))
        .map(|address| address.ip())
        .unwrap_or(if v6 {
            Ipv6Addr::LOCALHOST.into()
        } else {
            Ipv4Addr::LOCALHOST.into()
        })
}

fn connects(address: SocketAddr, deadline: Instant) -> bool {
    let left = deadline.saturating_duration_since(Instant::now());
    !left.is_zero() && TcpStream::connect_timeout(&address, left).is_ok()
}

/// Without a host, either loopback address accepting is enough: a server
/// bound to only one of them is still ready.
fn probe_port(port: u16, host: Option<&str>, deadline: Instant) -> bool {
    let addresses: Vec<SocketAddr> = match host.map(unbracketed) {
        None => vec![
            (Ipv4Addr::LOCALHOST, port).into(),
            (Ipv6Addr::LOCALHOST, port).into(),
        ],
        Some("local-ip") => {
            let ip = local_ip(false);
            let ip = if ip.is_loopback() { local_ip(true) } else { ip };
            vec![(ip, port).into()]
        }
        Some("local-ipv4") => vec![(local_ip(false), port).into()],
        Some("local-ipv6") => vec![(local_ip(true), port).into()],
        Some(host) => match host.parse::<IpAddr>() {
            Ok(ip) => vec![(destination(ip), port).into()],
            Err(_) => match (host, port).to_socket_addrs() {
                Ok(addresses) => addresses.collect(),
                Err(_) => return false,
            },
        },
    };
    addresses
        .into_iter()
        .any(|address| connects(address, deadline))
}

/// The url to request, a wildcard host rewritten to loopback, and whether it
/// stays on this machine: a loopback address, or a `localhost` name whose
/// every address is loopback.
fn target(url: &str) -> Option<(String, bool)> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    let host = uri.host()?;
    let bare = unbracketed(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        let ip = destination(ip);
        let written = match ip {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        let url = url.replacen(host, &written, 1);
        return Some((url, is_loopback(ip)));
    }
    let name = bare.trim_end_matches('.');
    let local = (name == "localhost" || name.ends_with(".localhost"))
        && (bare, uri.port_u16().unwrap_or(80))
            .to_socket_addrs()
            .is_ok_and(|addresses| {
                let addresses: Vec<_> = addresses.collect();
                !addresses.is_empty() && addresses.iter().all(|address| is_loopback(address.ip()))
            });
    Some((url.to_owned(), local))
}

/// Ready on a status from 200 to 403, a redirect included: an auth challenge
/// or a forbidden root still means the server is up. A 404 at the root is
/// tried again at `/index.html`, which may be all a static server answers.
fn probe_url(url: &str, deadline: Instant) -> bool {
    let Some((url, local)) = target(url) else {
        return false;
    };
    let status = |url: &str| -> Option<u16> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(left));
        if local {
            // A request to this machine skips proxies and accepts a
            // self-signed development certificate.
            config = config.proxy(None).tls_config(
                ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
            );
        }
        let agent: ureq::Agent = config.build().into();
        agent
            .get(url)
            .call()
            .ok()
            .map(|response| response.status().as_u16())
    };
    let ready = |status: u16| (200..=403).contains(&status);
    match status(&url) {
        Some(404) => {
            let uri: Option<ureq::http::Uri> = url.parse().ok();
            if uri.as_ref().is_some_and(|uri| uri.path() == "/") {
                let index = format!("{}/index.html", url.trim_end_matches('/'));
                status(&index).is_some_and(ready)
            } else {
                false
            }
        }
        Some(found) => ready(found),
        None => false,
    }
}

/// Runs the command through the shell at the workspace root, with `env` in
/// place of qk's own when it has any. On timeout or stop, its whole process
/// group is killed.
fn probe_command(
    command: &str,
    env: &BTreeMap<OsString, OsString>,
    root: &Path,
    deadline: Instant,
    stop: &AtomicBool,
) -> bool {
    let mut shell = crate::shell(command);
    if !env.is_empty() {
        shell.env_clear().envs(env);
    }
    shell
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = shell.group_spawn() else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline || stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Watches a task's output for `logMatches` text, in any order, on either
/// stream and across reads. Terminal control sequences are left out, as a
/// terminal would not show them.
#[derive(Debug)]
pub struct LogMatcher {
    pending: Vec<String>,
    /// Each stream's visible text, kept as long as the longest pending match.
    tails: [String; 2],
    /// Each stream's place in a control sequence a read ended inside.
    states: [Escape; 2],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Escape {
    #[default]
    Text,
    /// After ESC.
    Start,
    /// In a CSI sequence, up to its final byte.
    Csi,
    /// In an OSC, DCS, SOS, PM or APC string, up to BEL or ST.
    Control,
    /// After ESC inside such a string, which ST ends.
    ControlEnd,
}

impl LogMatcher {
    pub fn new(patterns: Vec<String>) -> Self {
        Self {
            pending: patterns,
            tails: Default::default(),
            states: Default::default(),
        }
    }

    /// Feeds one read; true once every pattern has appeared.
    pub fn feed(&mut self, stderr: bool, bytes: &[u8]) -> bool {
        let stream = usize::from(stderr);
        let text = String::from_utf8_lossy(bytes);
        let (tail, state) = (&mut self.tails[stream], &mut self.states[stream]);
        for c in text.chars() {
            *state = match (*state, c) {
                (Escape::Text, '\u{1b}') => Escape::Start,
                (Escape::Text, c) => {
                    tail.push(c);
                    Escape::Text
                }
                (Escape::Start, '[') => Escape::Csi,
                (Escape::Start, ']' | 'P' | 'X' | '^' | '_') => Escape::Control,
                (Escape::Start, _) => Escape::Text,
                (Escape::Csi, '\u{40}'..='\u{7e}') => Escape::Text,
                (Escape::Csi, _) => Escape::Csi,
                (Escape::Control, '\u{7}') => Escape::Text,
                (Escape::Control, '\u{1b}') => Escape::ControlEnd,
                (Escape::Control, _) => Escape::Control,
                (Escape::ControlEnd, '\\') => Escape::Text,
                (Escape::ControlEnd, _) => Escape::Control,
            };
        }
        self.pending
            .retain(|pattern| !tail.contains(pattern.as_str()));
        let keep = self
            .pending
            .iter()
            .map(|pattern| pattern.len().saturating_sub(1))
            .max()
            .unwrap_or(0);
        if tail.len() > keep {
            let mut start = tail.len() - keep;
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            tail.drain(..start);
        }
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope<'a>(options: &'a Map<String, Value>, env: &'a BTreeMap<String, String>) -> Scope<'a> {
        Scope {
            workspace_root: "/repo",
            project_root: "apps/app",
            project_name: "app",
            options,
            env,
        }
    }

    fn resolved(
        ready_when: Value,
        options: Value,
        env: &[(&str, &str)],
    ) -> Result<Option<ReadyCheck>> {
        let options = options.as_object().unwrap().clone();
        let env = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        resolve("app:serve", &ready_when, &scope(&options, &env))
    }

    #[test]
    fn placeholders_read_options_env_and_fallbacks() {
        let check = resolved(
            json!({"url": "http://localhost:{options.port ?? env.PORT ?? 4200}/{projectName}"}),
            json!({}),
            &[("PORT", "5001")],
        )
        .unwrap()
        .unwrap();
        assert_eq!(check.probe, Probe::Url("http://localhost:5001/app".into()));
        assert_eq!(check.timeout, Duration::from_millis(600_000));
        assert_eq!(check.interval, Duration::from_millis(250));
        let check = resolved(
            json!({"port": "{options.port}"}),
            json!({"port": 3000}),
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            check.probe,
            Probe::Port {
                port: 3000,
                host: None
            }
        );
        let check = resolved(
            json!({"logMatches": ["ready on {options.server.port}", "{other}"], "timeout": 5}),
            json!({"server": {"port": 1}}),
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            check.probe,
            Probe::LogMatches(vec!["ready on 1".into(), "{other}".into()])
        );
        assert_eq!(check.timeout, Duration::from_millis(5));
    }

    #[test]
    fn a_port_the_server_picks_leaves_nothing_to_check() {
        for port in [json!(0), json!("auto"), json!("{options.port}")] {
            let options = json!({"port": ""});
            assert_eq!(resolved(json!({"port": port}), options, &[]).unwrap(), None);
        }
    }

    #[test]
    fn a_flag_without_a_value_leaves_host_and_port_unset() {
        let check = resolved(
            json!({"port": "{options.port ?? 4200}", "host": "{options.host ?? env.HOST}"}),
            json!({"port": "true", "host": "true"}),
            &[],
        );
        assert!(format!("{:#}", check.unwrap_err()).contains("has no value"));
        let check = resolved(
            json!({"port": "{options.port ?? 4200}"}),
            json!({"port": true}),
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            check.probe,
            Probe::Port {
                port: 4200,
                host: None
            }
        );
    }

    #[test]
    fn invalid_settings_say_why() {
        for (ready_when, reason) in [
            (json!({}), "exactly one of"),
            (json!({"url": "a", "port": 1}), "exactly one of"),
            (json!({"url": "ftp://x"}), "http or https"),
            (json!({"port": 70000}), "from 0 to 65535"),
            (json!({"port": "{options.missing}"}), "has no value"),
            (json!({"command": ""}), "non-empty"),
            (json!({"logMatches": []}), "logMatches"),
            (json!({"port": 1, "timeout": 0}), "\"timeout\""),
            (json!({"port": 1, "other": 0}), "unknown key"),
            (
                json!({"url": "http://{options.a ?? x ?? options.b}"}),
                "can never be used",
            ),
        ] {
            let error = resolved(ready_when.clone(), json!({}), &[]).unwrap_err();
            assert!(
                format!("{error:#}").contains(reason),
                "{ready_when}: {error:#}"
            );
        }
    }

    #[test]
    fn log_matcher_needs_every_pattern_without_control_sequences() {
        let mut matcher = LogMatcher::new(vec!["ready".into(), "listening".into()]);
        assert!(!matcher.feed(false, b"server listening on 3000"));
        assert!(matcher.feed(true, b"\x1b[32mrea\x1b[0mdy"));
        let mut matcher = LogMatcher::new(vec!["ready".into()]);
        assert!(!matcher.feed(false, b"rea\x1b["));
        assert!(matcher.feed(false, b"32mdy"));
        let mut matcher = LogMatcher::new(vec!["ready".into()]);
        assert!(!matcher.feed(false, b"\x1b]0;ready\x07"));
        assert!(matcher.feed(false, b"rea\x1b]8;;http://x\x1b\\dy"));
        let mut matcher = LogMatcher::new(vec!["éready".into()]);
        assert!(!matcher.feed(false, "éééa".as_bytes()));
        assert!(matcher.feed(false, "éready".as_bytes()));
    }

    #[test]
    fn port_and_command_probes_wait_for_readiness() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = AtomicBool::new(false);
        for host in [None, Some("127.0.0.1"), Some("0.0.0.0")] {
            let verdict = Verdict::default();
            let check = ReadyCheck {
                probe: Probe::Port {
                    port,
                    host: host.map(str::to_owned),
                },
                timeout: Duration::from_secs(5),
                interval: Duration::from_millis(10),
            };
            wait("app:serve", &check, Path::new("."), &verdict, &stop);
            assert!(verdict.is_ready(), "{host:?}");
        }
        drop(listener);
        let verdict = Verdict::default();
        let check = ReadyCheck {
            probe: Probe::Command {
                command: "exit 1".into(),
                env: BTreeMap::new(),
            },
            timeout: Duration::from_millis(100),
            interval: Duration::from_millis(10),
        };
        wait("app:serve", &check, Path::new("."), &verdict, &stop);
        assert!(
            verdict
                .failure()
                .unwrap()
                .contains("did not become ready within 100ms")
        );
        let verdict = Verdict::default();
        let check = ReadyCheck {
            probe: Probe::Command {
                command: "exit 0".into(),
                env: BTreeMap::new(),
            },
            ..check
        };
        wait("app:serve", &check, Path::new("."), &verdict, &stop);
        assert!(verdict.is_ready());
    }

    #[test]
    fn url_probes_take_statuses_up_to_403_and_try_index_html() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let status = if request.starts_with("GET /index.html") {
                    "200 OK"
                } else if request.starts_with("GET /missing") || request.starts_with("GET / ") {
                    "404 Not Found"
                } else {
                    "302 Found\r\nlocation: /elsewhere"
                };
                let _ = stream.write_all(
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes(),
                );
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(probe_url(&format!("http://127.0.0.1:{port}/"), deadline));
        assert!(probe_url(
            &format!("http://0.0.0.0:{port}/redirects"),
            deadline
        ));
        assert!(!probe_url(
            &format!("http://127.0.0.1:{port}/missing"),
            deadline
        ));
    }
}
