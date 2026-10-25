//! Opt-in elapsed timings for cache operations, written to stderr.

use std::sync::OnceLock;
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Enable timings for this process before its first cache operation.
/// `QK_PROFILE_CACHE=1` also enables them when no caller configures profiling.
pub fn enable() {
    let _ = ENABLED.set(true);
}

pub(crate) struct Span<'a> {
    task: &'a str,
    stage: &'static str,
    started: Option<Instant>,
    counts: Vec<(&'static str, String)>,
}

pub(crate) fn span<'a>(task: &'a str, stage: &'static str) -> Span<'a> {
    let enabled = *ENABLED
        .get_or_init(|| std::env::var_os("QK_PROFILE_CACHE").is_some_and(|value| value == "1"));
    Span {
        task,
        stage,
        started: enabled.then(Instant::now),
        counts: Vec::new(),
    }
}

impl Span<'_> {
    /// Adds a count to the span's line, such as the bytes it received.
    pub(crate) fn count(&mut self, name: &'static str, value: u64) {
        if self.started.is_some() {
            self.counts.push((name, value.to_string()));
        }
    }

    /// Adds text to the span's line, such as how a server says it spent the
    /// time; whitespace is left out, so the line still splits on spaces.
    pub(crate) fn note(&mut self, name: &'static str, value: &str) {
        if self.started.is_some() {
            let value: String = value.chars().filter(|ch| !ch.is_whitespace()).collect();
            self.counts.push((name, value));
        }
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let mut line = format!(
                "qk profile: task={} stage={} ms={:.3}",
                self.task,
                self.stage,
                started.elapsed().as_secs_f64() * 1000.0
            );
            for (name, value) in &self.counts {
                line.push_str(&format!(" {name}={value}"));
            }
            line.push('\n');
            qk_executor::report::output(true, line.as_bytes());
        }
    }
}
