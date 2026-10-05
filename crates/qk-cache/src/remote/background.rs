//! Detached uploads own a private snapshot, independent of cache eviction.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, SystemTime};

use super::Upload;

const MAX_REQUEST: u64 = 32 << 20;

#[derive(Serialize, Deserialize)]
pub(super) struct Configuration {
    pub config: Value,
    pub token: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Request {
    configuration: Configuration,
    snapshot: PathBuf,
    jobs: Vec<Upload>,
    held: Vec<String>,
    confirmed: Vec<String>,
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A killed worker cannot run cleanup. Later launches reclaim old, unlocked
/// snapshots; the lease protects an upload even when it runs unusually long.
fn cleanup_abandoned(parent: &std::path::Path) {
    let Ok(items) = fs::read_dir(parent) else {
        return;
    };
    for item in items.flatten() {
        if !item.file_name().to_string_lossy().starts_with("qk-upload-") {
            continue;
        }
        let Ok(metadata) = item.metadata() else {
            continue;
        };
        let expired = metadata
            .modified()
            .ok()
            .and_then(|time| SystemTime::now().duration_since(time).ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if !expired {
            continue;
        }
        if metadata.is_dir() {
            let Ok(lease) = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(item.path().join("lease"))
            else {
                continue;
            };
            if FileExt::try_lock_exclusive(&lease).is_ok() {
                let _ = fs::remove_dir_all(item.path());
            }
        } else if item
            .path()
            .extension()
            .is_some_and(|extension| extension == "log")
        {
            let snapshot = item.path().with_extension("");
            if snapshot.is_dir() {
                let Ok(lease) = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(snapshot.join("lease"))
                else {
                    continue;
                };
                if FileExt::try_lock_exclusive(&lease).is_err() {
                    continue;
                }
            }
            let _ = fs::remove_file(item.path());
        }
    }
}

/// Pin immutable blobs with hard links, falling back to copies where needed.
/// The sibling directory survives removal of the cache itself.
fn snapshot(jobs: &[Upload]) -> Result<(tempfile::TempDir, Vec<Upload>)> {
    let parent = jobs
        .first()
        .context("no queued uploads")?
        .local
        .parent()
        .context("cache has no parent")?;
    cleanup_abandoned(parent);
    let directory = tempfile::Builder::new()
        .prefix("qk-upload-")
        .tempdir_in(parent)?;
    let root = directory.path();
    for name in ["entries", "blobs", "tmp", "remote"] {
        fs::create_dir(root.join(name))?;
    }
    let mut pinned = Vec::with_capacity(jobs.len());
    for job in jobs {
        let record = match &job.warm {
            Some((_, record)) => record.clone(),
            None => {
                if !crate::record::valid_hash(&job.key) {
                    bail!("invalid upload key");
                }
                let record = fs::read(job.local.join("entries").join(format!("{}.json", job.key)))?;
                fs::write(
                    root.join("entries").join(format!("{}.json", job.key)),
                    &record,
                )?;
                record
            }
        };
        let parsed = crate::record::StoredRecord::read(&record)?;
        for blob in parsed.blobs() {
            if !crate::record::valid_hash(&blob) {
                bail!("invalid upload blob");
            }
            let destination = root.join("blobs").join(&blob);
            if !destination.is_file() {
                let source = job.local.join("blobs").join(&blob);
                if fs::hard_link(&source, &destination).is_err() {
                    fs::copy(&source, &destination)?;
                }
            }
        }
        let mut job = job.clone();
        job.local = root.to_owned();
        pinned.push(job);
    }
    Ok((directory, pinned))
}

pub(super) fn launch(
    configuration: &Configuration,
    jobs: &[Upload],
    held: Vec<String>,
    confirmed: Vec<String>,
) -> Result<PathBuf> {
    let (snapshot, jobs) = snapshot(jobs)?;
    let lease = fs::File::create(snapshot.path().join("lease"))?;
    FileExt::lock_exclusive(&lease)?;
    let request = Request {
        configuration: Configuration {
            config: configuration.config.clone(),
            token: configuration.token.clone(),
        },
        snapshot: snapshot.path().to_owned(),
        jobs,
        held,
        confirmed,
    };
    let bytes = serde_json::to_vec(&request)?;
    if bytes.len() as u64 > MAX_REQUEST {
        bail!("background upload request is too large");
    }
    // A private log persists after the worker removes its temporary snapshot.
    let log = tempfile::Builder::new()
        .prefix("qk-upload-")
        .suffix(".log")
        .tempfile_in(snapshot.path().parent().unwrap())?;
    let log_path = snapshot.path().with_extension("log");
    let log = log.persist(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__upload-worker")
        .current_dir(snapshot.path().parent().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log.try_clone()?)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN");
    detach(&mut command)?;
    let mut child = command.spawn().context("cannot start upload worker")?;
    let handoff = (|| -> Result<()> {
        // Credentials travel only through this private pipe, never through argv
        // or a request file. EOF completes the request before acknowledgement.
        let mut input = child.stdin.take().context("worker has no input pipe")?;
        input.write_all(&bytes)?;
        drop(input);
        let mut ready = [0];
        child
            .stdout
            .take()
            .context("worker has no output pipe")?
            .read_exact(&mut ready)?;
        if &ready != b"R" {
            bail!("upload worker did not accept its snapshot");
        }
        Ok(())
    })();
    if let Err(error) = handoff {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let _ = snapshot.keep();
    // Reap the child when qk remains alive (for example in watch mode).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(log_path)
}

#[cfg(unix)]
fn detach(command: &mut Command) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is the only operation after fork and is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(windows)]
fn detach(command: &mut Command) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    // Windows inherits every inheritable handle, even when a child's stdio is
    // redirected. Shell-provided stdio handles would keep the caller's pipes
    // alive until uploads finished. Command duplicates the worker's own stdio,
    // so these originals never need to remain inheritable.
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle returns a borrowed process handle; clearing its
        // inheritance flag neither closes it nor changes its read/write access.
        unsafe {
            let handle = GetStdHandle(kind);
            if !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) -> Result<()> {
    Ok(())
}

pub(crate) fn run() -> Result<()> {
    let request: Request = serde_json::from_reader(std::io::stdin().lock().take(MAX_REQUEST + 1))?;
    let mut environment = BTreeMap::new();
    environment.insert("NX_POWERPACK_CACHE_MODE".into(), "read-write".into());
    if let Some(token) = request.configuration.token {
        environment.insert("AWS_SESSION_TOKEN".into(), token.into());
    }
    let remote = Arc::new(
        super::configure(Some(&request.configuration.config), &environment)?
            .context("worker remote cache is disabled")?,
    );
    // Do not adopt/delete a snapshot until the complete request is accepted.
    let cleanup = Cleanup(request.snapshot.clone());
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(request.snapshot.join("lease"))?;
    std::io::stdout().write_all(b"R")?;
    std::io::stdout().flush()?;
    FileExt::lock_exclusive(&lease)?;
    eprintln!(
        "qk: background upload worker {} started",
        std::process::id()
    );
    remote.read_index(&request.snapshot);
    remote.confirmed.lock().unwrap().extend(request.confirmed);
    for key in request.held {
        remote.held(&key);
    }
    for job in request.jobs {
        remote.enqueue(job);
    }
    let failures = remote.finish();
    for failure in &failures {
        eprintln!("qk: remote cache upload failed for {failure}");
    }
    drop(lease);
    drop(cleanup);
    eprintln!(
        "qk: background uploads completed; {} failure(s)",
        failures.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_pins_result_logs_and_warm_blobs_independently_of_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let cache = crate::Cache::new(root.clone());
        cache.initialize().unwrap();
        let source = temporary.path().join("source");
        fs::write(&source, "cached bytes").unwrap();
        let blob = cache.put_blob(&source).unwrap();
        let key = "a".repeat(64);
        fs::write(
            root.join("entries").join(format!("{key}.json")),
            json!({"log": blob}).to_string(),
        )
        .unwrap();
        let jobs = vec![
            Upload {
                key,
                local: root.clone(),
                warm: None,
            },
            Upload {
                key: "warm state".into(),
                local: root.clone(),
                warm: Some((
                    "warm/object".into(),
                    serde_json::to_vec(
                        &json!({"groups": {"paths": {"artifacts": {"scratch": {"blob": blob}}}}}),
                    )
                    .unwrap(),
                )),
            },
        ];
        let (pinned, jobs) = snapshot(&jobs).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(
            fs::read(pinned.path().join("blobs").join(blob)).unwrap(),
            b"cached bytes"
        );
        assert!(jobs.iter().all(|job| job.local == pinned.path()));
        assert!(
            pinned
                .path()
                .join("entries")
                .join(format!("{}.json", jobs[0].key))
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn expired_snapshot_is_reclaimed_only_after_its_lease_is_released() {
        let temporary = tempfile::tempdir().unwrap();
        let snapshot = temporary.path().join("qk-upload-abandoned");
        fs::create_dir(&snapshot).unwrap();
        let lease = fs::File::create(snapshot.join("lease")).unwrap();
        FileExt::lock_exclusive(&lease).unwrap();
        fs::File::open(&snapshot)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        cleanup_abandoned(temporary.path());
        assert!(snapshot.is_dir());
        // Parallel tests may fork and briefly inherit this open file description.
        // Unlock explicitly so cleanup does not depend on those children execing.
        FileExt::unlock(&lease).unwrap();
        drop(lease);
        cleanup_abandoned(temporary.path());
        assert!(!snapshot.exists());
    }
}
