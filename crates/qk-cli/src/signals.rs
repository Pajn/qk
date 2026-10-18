//! Cancelling a run on SIGINT, SIGTERM or SIGHUP, remembering which one came,
//! so the run exits with 128 plus its number, as Nx's signalToCode has it:
//! 130, 143 or 129.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Result, bail};

/// The first signal received, or 0.
static RECEIVED: AtomicI32 = AtomicI32::new(0);
/// What the handler sets to cancel the run.
static CANCELLED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Sets `cancelled` when one of the signals arrives.
pub fn install(cancelled: Arc<AtomicBool>) -> Result<()> {
    if CANCELLED.set(cancelled).is_err() {
        bail!("cannot register cancellation handler twice");
    }
    #[cfg(unix)]
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to atomics, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                bail!(
                    "cannot register cancellation handler: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
    #[cfg(not(unix))]
    ctrlc::set_handler(|| handle(2))
        .map_err(|error| anyhow::anyhow!("cannot register cancellation handler: {error}"))?;
    Ok(())
}

extern "C" fn handle(signal: std::ffi::c_int) {
    let _ = RECEIVED.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    if let Some(cancelled) = CANCELLED.get() {
        cancelled.store(true, Ordering::SeqCst);
    }
}

/// The code a cancelled run exits with: 128 plus the signal's number, or
/// 130 when none was recorded.
pub fn exit_code() -> i32 {
    match RECEIVED.load(Ordering::SeqCst) {
        0 => 130,
        signal => 128 + signal,
    }
}
