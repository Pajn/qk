use std::sync::OnceLock;
use std::time::Instant;

pub(crate) struct Span<'a> {
    task: &'a str,
    stage: &'static str,
    started: Option<Instant>,
}

pub(crate) fn span<'a>(task: &'a str, stage: &'static str) -> Span<'a> {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = *ENABLED
        .get_or_init(|| std::env::var_os("QK_PROFILE_CACHE").is_some_and(|value| value == "1"));
    Span {
        task,
        stage,
        started: enabled.then(Instant::now),
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            qk_executor::status!(
                "qk profile: task={} stage={} ms={:.3}",
                self.task,
                self.stage,
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
