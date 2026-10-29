//! Streaming remote packs, with bounded expansion and verified local blobs.
//!
//! A pack is one Zstandard stream, possibly of several frames, holding:
//!
//! ```text
//! "qkp4"
//! u64 LE length, the record
//! u32 LE count of the blobs the record cites, then for each, in the order
//!     StoredRecord::blobs gives them: u8 placement (1 inline, 0 apart) and
//!     u64 LE raw length
//! the inline blobs' bytes, in that order
//! ```
//!
//! A blob stored apart is its own object, fetched by its hash only when the
//! local store lacks it. The table comes before the bodies, so those fetches
//! start while the inline blobs still stream.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::record::StoredRecord;
use anyhow::{Context, Result, bail};

const LEVEL: i32 = 3;
const LARGEST_RECORD: u64 = 1 << 30;
const WINDOW_LOG_MAX: u32 = 27;
const MAGIC: &[u8; 4] = b"qkp4";

/// Where a pack carries a blob it cites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Placement {
    /// In the pack itself.
    Inline,
    /// As its own object, named by its hash.
    Apart,
}

/// Writes the pack for `record` and the blobs it cites from the local store
/// at `root`, carrying each blob as `place` says.
pub(super) fn write(
    root: &Path,
    record: &[u8],
    place: impl Fn(&str) -> Result<Placement>,
    output: impl Write,
) -> Result<()> {
    let parsed = crate::record::StoredRecord::read(record)?;
    let mut writer = zstd::stream::write::Encoder::new(io::BufWriter::new(output), LEVEL)?;
    writer.include_checksum(true)?;
    let mut used = 0;
    part_size(&mut used, record.len() as u64, LARGEST_RECORD)?;
    writer.write_all(MAGIC)?;
    writer.write_all(&(record.len() as u64).to_le_bytes())?;
    writer.write_all(record)?;
    let mut inline = Vec::new();
    let blobs = parsed.blobs();
    writer.write_all(&u32::try_from(blobs.len())?.to_le_bytes())?;
    for blob in &blobs {
        if !crate::record::valid_hash(blob) {
            bail!("invalid blob name in remote record");
        }
        let file = File::open(root.join("blobs").join(blob))?;
        let length = file.metadata()?.len();
        let placement = place(blob)?;
        writer.write_all(&[u8::from(placement == Placement::Inline)])?;
        writer.write_all(&length.to_le_bytes())?;
        if placement == Placement::Inline {
            part_size(&mut used, length, super::LARGEST_UPLOAD)?;
            inline.push((blob, file, length));
        }
    }
    for (blob, mut file, length) in inline {
        if io::copy(&mut (&mut file).take(length), &mut writer)? != length {
            bail!("blob {blob} changed while it was uploaded");
        }
    }
    writer.finish()?.flush()?;
    Ok(())
}

/// Writes the upload body a store server keeps for `record`: a header naming
/// each blob it cites with its raw size, the record, then each blob `place`
/// carries inline as its own Zstandard frame, which the server copies into
/// the packs it sends. Its layout is in `tools/store-server/PROTOCOL.md`.
pub(super) fn write_upload(
    root: &Path,
    record: &[u8],
    place: impl Fn(&str) -> Result<Placement>,
    output: impl Write,
) -> Result<()> {
    let parsed = crate::record::StoredRecord::read(record)?;
    let mut frames = Vec::new();
    let mut blobs = Vec::new();
    for blob in parsed.blobs() {
        if !crate::record::valid_hash(&blob) {
            bail!("invalid blob name in remote record");
        }
        let length = std::fs::metadata(root.join("blobs").join(&blob))?.len();
        let compressed = if place(&blob)? == Placement::Inline {
            let frame = compress(root, &blob)?;
            let size = frame.as_file().metadata()?.len();
            frames.push(frame);
            serde_json::Value::from(size)
        } else {
            serde_json::Value::Null
        };
        blobs.push(serde_json::json!([blob, length, compressed]));
    }
    let header = serde_json::to_vec(&serde_json::json!({"record": record.len(), "blobs": blobs}))?;
    let mut output = io::BufWriter::new(output);
    output.write_all(&u32::try_from(header.len())?.to_le_bytes())?;
    output.write_all(&header)?;
    output.write_all(record)?;
    for frame in frames {
        io::copy(&mut frame.reopen()?, &mut output)?;
    }
    output.flush()?;
    Ok(())
}

/// What reading a pack moved: the bytes received, as compressed, how many of
/// the inline blob bytes the local store held already, and how many bytes of
/// blobs stored apart it did not fetch because the local store held them.
#[derive(Default)]
pub(super) struct Transfer {
    pub(super) received: u64,
    pub(super) held: u64,
    pub(super) skipped: u64,
    /// Every blob the pack stores apart, held or not.
    pub(super) apart: Vec<String>,
    /// How long reading waited on the body, as it arrives.
    pub(super) waited: std::time::Duration,
    /// The inline blobs kept, which the local store lacked.
    pub(super) written: u64,
}

struct Counting<'a, R> {
    inner: R,
    count: &'a mut u64,
    waited: &'a mut std::time::Duration,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let started = std::time::Instant::now();
        let read = self.inner.read(buffer);
        // A read that fails counts too: a stall often ends in an error.
        *self.waited += started.elapsed();
        let read = read?;
        *self.count += read as u64;
        Ok(read)
    }
}

/// Reads a pack into the local store at `root`, returning its record with a
/// pin on every blob the record cites. The pin is taken once `check` accepts
/// the record and before any blob is checked for, so blobs it finds already
/// held cannot be pruned before the caller publishes the record. `apart`
/// fetches the blobs stored apart that the local store lacks, while the
/// inline ones are read; every blob the record cites is in the local store
/// when this returns.
pub(super) fn read(
    root: &Path,
    input: impl Read,
    check: impl Fn(&StoredRecord) -> Result<()>,
    transfer: &mut Transfer,
    apart: impl FnOnce(Vec<String>) -> Result<()> + Send,
) -> Result<(Vec<u8>, crate::evict::Pin)> {
    let input = Counting {
        inner: input,
        count: &mut transfer.received,
        waited: &mut transfer.waited,
    };
    let mut input = input.take(super::LARGEST_UPLOAD + 1);
    let mut decoder = zstd::stream::read::Decoder::new(&mut input)?;
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let counts = (
        &mut transfer.held,
        &mut transfer.skipped,
        &mut transfer.apart,
        &mut transfer.written,
    );
    let read = read_parts(root, &mut decoder, check, counts, apart)?;
    drop(decoder);
    if input.limit() == 0 {
        bail!("remote pack exceeds 5 GiB");
    }
    Ok(read)
}

/// The record at the start of the pack `input`, read without its blobs.
pub(super) fn read_record(input: impl Read) -> Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let mut magic = [0; 4];
    decoder
        .read_exact(&mut magic)
        .context("remote pack ends early")?;
    if &magic != MAGIC {
        bail!("not a qk remote pack");
    }
    let length = part_length(&mut decoder)?;
    part_size(&mut 0, length, LARGEST_RECORD)?;
    let mut record = Vec::new();
    if (&mut decoder).take(length).read_to_end(&mut record)? as u64 != length {
        bail!("remote pack ends early");
    }
    Ok(record)
}

fn read_parts(
    root: &Path,
    mut body: impl Read,
    check: impl Fn(&StoredRecord) -> Result<()>,
    (held, skipped, listed, written): (&mut u64, &mut u64, &mut Vec<String>, &mut u64),
    apart: impl FnOnce(Vec<String>) -> Result<()> + Send,
) -> Result<(Vec<u8>, crate::evict::Pin)> {
    let mut magic = [0; 4];
    body.read_exact(&mut magic)
        .context("remote pack ends early")?;
    if &magic != MAGIC {
        bail!("not a qk remote pack");
    }
    let mut used = 0;
    let length = part_length(&mut body)?;
    part_size(&mut used, length, LARGEST_RECORD)?;
    let mut record = Vec::new();
    if (&mut body).take(length).read_to_end(&mut record)? as u64 != length {
        bail!("remote pack ends early");
    }
    let parsed = crate::record::StoredRecord::read(&record).context("invalid remote record")?;
    check(&parsed)?;
    let blobs = parsed.blobs();
    let pin = crate::evict::pin(root, &blobs)?;
    let mut count = [0; 4];
    body.read_exact(&mut count)
        .context("remote pack ends early")?;
    if u32::from_le_bytes(count) as usize != blobs.len() {
        bail!("remote pack's blobs do not match its record");
    }
    let mut inline = Vec::new();
    let mut missing = Vec::new();
    for blob in blobs {
        if !crate::record::valid_hash(&blob) {
            bail!("invalid blob name in remote record");
        }
        let mut placement = [0; 1];
        body.read_exact(&mut placement)
            .context("remote pack ends early")?;
        let length = part_length(&mut body)?;
        match placement[0] {
            1 => {
                part_size(&mut used, length, super::LARGEST_UPLOAD)?;
                inline.push((blob, length));
            }
            0 if length > super::LARGEST_UPLOAD => bail!("remote pack part is too large"),
            0 => {
                listed.push(blob.clone());
                if root.join("blobs").join(&blob).is_file() {
                    *skipped += length;
                } else {
                    missing.push(blob);
                }
            }
            _ => bail!("invalid blob placement in remote pack"),
        }
    }
    std::thread::scope(|scope| {
        let fetching = (!missing.is_empty()).then(|| scope.spawn(move || apart(missing)));
        let streamed = read_inline(root, &mut body, &inline, held, written);
        let fetched = fetching.map_or(Ok(()), |fetching| {
            fetching.join().expect("blob fetches panicked")
        });
        streamed.and(fetched)
    })?;
    if body.read(&mut [0])? != 0 {
        bail!("remote pack holds more than its record cites");
    }
    Ok((record, pin))
}

/// The largest inline blob handed to a writer whole; a larger one is
/// written as it is read. The queue holds a few of them at most.
const BUFFERED_BLOB: u64 = 1 << 20;
const BLOB_WRITERS: usize = 8;

/// Keeps each inline blob the local store lacks, verified against its hash.
/// The pack is one stream, but a cache of many small files costs more to
/// write than to decompress, so the blobs are hashed and written by a few
/// threads while the stream is read on.
fn read_inline(
    root: &Path,
    body: &mut impl Read,
    inline: &[(String, u64)],
    held: &mut u64,
    written: &mut u64,
) -> Result<()> {
    let keep = |blob: &str, bytes: &[u8]| -> Result<()> {
        if blake3::hash(bytes).to_hex().as_str() != blob {
            bail!("remote blob {blob} does not match its hash");
        }
        let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        file.as_file_mut().write_all(bytes)?;
        file.persist(root.join("blobs").join(blob))?;
        Ok(())
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel::<(&str, Vec<u8>)>(BLOB_WRITERS * 2);
    let receiver = std::sync::Mutex::new(receiver);
    let failed = std::sync::atomic::AtomicBool::new(false);
    // After a failure the queue is still drained, so a reader waiting for
    // room in it is never left waiting.
    let write = || -> Result<()> {
        let mut kept = Ok(());
        loop {
            let next = receiver.lock().unwrap().recv();
            let Ok((blob, bytes)) = next else {
                return kept;
            };
            if failed.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            if let Err(error) = keep(blob, &bytes) {
                failed.store(true, std::sync::atomic::Ordering::Relaxed);
                kept = Err(error);
            }
        }
    };
    std::thread::scope(|scope| -> Result<()> {
        // Started once a blob is to be written, which most reads of a pack
        // whose blobs are all held never come to.
        let mut writers = Vec::new();
        let read = (|| -> Result<()> {
            for (blob, length) in inline {
                if failed.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let length = *length;
                // Stops reading once a writer has failed, rather than read
                // on through a large blob the restore will not use.
                let mut part = Stopping {
                    inner: (&mut *body).take(length),
                    failed: &failed,
                };
                let target = root.join("blobs").join(blob);
                if target.is_file() {
                    let copied = io::copy(&mut part, &mut io::sink())?;
                    *held += copied;
                    if copied != length {
                        bail!("remote pack ends early");
                    }
                    continue;
                }
                *written += 1;
                if length <= BUFFERED_BLOB {
                    let mut bytes = Vec::with_capacity(length as usize);
                    if part.read_to_end(&mut bytes)? as u64 != length {
                        bail!("remote pack ends early");
                    }
                    if writers.is_empty() {
                        writers = (0..BLOB_WRITERS).map(|_| scope.spawn(write)).collect();
                    }
                    if sender.send((blob, bytes)).is_err() {
                        break;
                    }
                    continue;
                }
                // Hashed as it is written, rather than read back.
                let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
                let mut hashing = Hashing {
                    inner: file.as_file_mut(),
                    hasher: blake3::Hasher::new(),
                };
                if io::copy(&mut part, &mut hashing)? != length {
                    bail!("remote pack ends early");
                }
                if hashing.hasher.finalize().to_hex().as_str() != blob {
                    bail!("remote blob {blob} does not match its hash");
                }
                file.persist(&target)?;
            }
            Ok(())
        })();
        // Nothing more of a failed read is kept.
        if read.is_err() {
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        drop(sender);
        // Every writer is joined, and its error preferred: a reader stopped
        // by a failed writer says only that it stopped.
        let mut kept = Ok(());
        for writer in writers {
            let result = writer.join().expect("blob writer panicked");
            if kept.is_ok() {
                kept = result;
            }
        }
        kept.and(read)
    })
}

/// A reader that fails once `failed` is set.
struct Stopping<'a, R> {
    inner: R,
    failed: &'a std::sync::atomic::AtomicBool,
}

impl<R: Read> Read for Stopping<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.failed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::other("a blob writer failed"));
        }
        self.inner.read(buffer)
    }
}

/// A writer that hashes what passes through it.
struct Hashing<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The blob `hash` compressed as one frame into the local store's scratch
/// space, as it is stored apart.
pub(super) fn compress(root: &Path, hash: &str) -> Result<tempfile::NamedTempFile> {
    let mut source = File::open(root.join("blobs").join(hash))?;
    let mut compressed = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
    let mut writer =
        zstd::stream::write::Encoder::new(io::BufWriter::new(compressed.as_file_mut()), LEVEL)?;
    writer.include_checksum(true)?;
    io::copy(&mut source, &mut writer)?;
    writer.finish()?.flush()?;
    Ok(compressed)
}

/// Keeps the blob `hash`, stored apart and read from `body`, in the local
/// store at `root` once it matches its hash, returning the bytes received.
pub(super) fn read_blob(root: &Path, hash: &str, body: impl Read) -> Result<u64> {
    let mut received = 0;
    let mut waited = std::time::Duration::ZERO;
    let input = Counting {
        inner: body,
        count: &mut received,
        waited: &mut waited,
    };
    let mut input = input.take(super::LARGEST_UPLOAD + 1);
    let mut decoder = zstd::stream::read::Decoder::new(&mut input)?;
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
    let expanded = io::copy(
        &mut (&mut decoder).take(super::LARGEST_UPLOAD + 1),
        file.as_file_mut(),
    )?;
    drop(decoder);
    if expanded > super::LARGEST_UPLOAD || input.limit() == 0 {
        bail!("remote blob {hash} exceeds 5 GiB");
    }
    if crate::hash::digest_file(file.path())? != hash {
        bail!("remote blob {hash} does not match its hash");
    }
    file.persist(root.join("blobs").join(hash))?;
    Ok(received)
}

fn part_length(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    reader
        .read_exact(&mut bytes)
        .context("remote pack ends early")?;
    Ok(u64::from_le_bytes(bytes))
}

fn part_size(used: &mut u64, length: u64, limit: u64) -> Result<()> {
    if length > limit {
        bail!("remote pack part is too large");
    }
    *used = used
        .checked_add(8)
        .and_then(|n| n.checked_add(length))
        .filter(|n| *n <= super::LARGEST_UPLOAD)
        .context("remote pack expands beyond 5 GiB")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(
        root: &Path,
        input: impl Read,
        check: impl Fn(&StoredRecord) -> Result<()>,
    ) -> Result<Vec<u8>> {
        super::read(root, input, check, &mut Transfer::default(), no_apart)
            .map(|(record, _)| record)
    }

    fn no_apart(blobs: Vec<String>) -> Result<()> {
        panic!("no blob is stored apart, yet {blobs:?} were fetched")
    }

    /// Writes a pack carrying every blob inline.
    fn write(root: &Path, record: &[u8], output: impl Write) -> Result<()> {
        super::write(root, record, |_| Ok(Placement::Inline), output)
    }

    #[test]
    fn a_record_is_read_without_the_blobs_after_it() {
        let source = root();
        let blob = source.path().join("blobs/a");
        std::fs::write(&blob, "contents").unwrap();
        let name = crate::hash::digest_file(&blob).unwrap();
        std::fs::rename(&blob, source.path().join("blobs").join(&name)).unwrap();
        let record = serde_json::to_vec(&serde_json::json!({
            "artifacts": {"file": {"blob": name}}
        }))
        .unwrap();
        let mut encoded = Vec::new();
        write(source.path(), &record, &mut encoded).unwrap();
        assert_eq!(read_record(&encoded[..]).unwrap(), record);
        assert!(read_record(&b"not a pack"[..]).is_err());
    }

    #[test]
    fn reading_counts_received_and_held_bytes() {
        let source = root();
        let blob = source.path().join("blobs/a");
        std::fs::write(&blob, "contents").unwrap();
        let name = crate::hash::digest_file(&blob).unwrap();
        std::fs::rename(&blob, source.path().join("blobs").join(&name)).unwrap();
        let record = serde_json::to_vec(&serde_json::json!({
            "artifacts": {"file": {"blob": name}}
        }))
        .unwrap();
        let mut encoded = Vec::new();
        write(source.path(), &record, &mut encoded).unwrap();
        let mut empty = Transfer::default();
        super::read(
            root().path(),
            &encoded[..],
            |_| Ok(()),
            &mut empty,
            no_apart,
        )
        .unwrap();
        assert_eq!(empty.received, encoded.len() as u64);
        assert_eq!(empty.held, 0);
        let mut holding = Transfer::default();
        super::read(
            source.path(),
            &encoded[..],
            |_| Ok(()),
            &mut holding,
            no_apart,
        )
        .unwrap();
        assert_eq!(holding.held, "contents".len() as u64);
    }

    #[test]
    fn reading_pins_every_cited_blob_until_the_pin_is_dropped() {
        let source = root();
        let blob = source.path().join("blobs/a");
        std::fs::write(&blob, "contents").unwrap();
        let name = crate::hash::digest_file(&blob).unwrap();
        std::fs::rename(&blob, source.path().join("blobs").join(&name)).unwrap();
        let record = serde_json::to_vec(&serde_json::json!({
            "artifacts": {"file": {"blob": name}}
        }))
        .unwrap();
        let mut encoded = Vec::new();
        write(source.path(), &record, &mut encoded).unwrap();
        let pins = source.path().join("locks/.pins");
        let listed = || -> Vec<String> {
            std::fs::read_dir(&pins)
                .unwrap()
                .flatten()
                .filter(|item| item.path().extension().is_some_and(|e| e == "blobs"))
                .map(|item| std::fs::read_to_string(item.path()).unwrap())
                .collect()
        };
        let (_, pin) = super::read(
            source.path(),
            &encoded[..],
            |_| Ok(()),
            &mut Transfer::default(),
            no_apart,
        )
        .unwrap();
        assert_eq!(listed(), [format!("{name}\n")]);
        drop(pin);
        assert!(listed().is_empty());
    }

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tmp")).unwrap();
        std::fs::create_dir(root.path().join("blobs")).unwrap();
        root
    }

    fn sample(root: &Path) -> (Vec<u8>, Vec<u8>) {
        let bytes = b"export const value = 42;\n".repeat(1000);
        let hash = blake3::hash(&bytes).to_hex().to_string();
        std::fs::write(root.join("blobs").join(&hash), &bytes).unwrap();
        let record = serde_json::to_vec(&serde_json::json!({"artifacts": {
            "dist/main.js": {"type": "File", "blob": hash, "mode": 420}
        }}))
        .unwrap();
        let mut encoded = Vec::new();
        write(root, &record, &mut encoded).unwrap();
        (record, encoded)
    }

    /// Many blobs are written by several writers, and one too large to hand
    /// over whole as it is read; every one lands verified.
    #[test]
    fn packs_of_many_blobs_restore_each_one() {
        let source = root();
        let mut artifacts = serde_json::Map::new();
        let mut contents = Vec::new();
        for index in 0..200 {
            let bytes = format!("module {index}\n").repeat(if index == 0 { 200_000 } else { 50 });
            assert_eq!(index == 0, bytes.len() as u64 > BUFFERED_BLOB);
            let hash = blake3::hash(bytes.as_bytes()).to_hex().to_string();
            std::fs::write(source.path().join("blobs").join(&hash), &bytes).unwrap();
            artifacts.insert(
                format!("cache/{index}"),
                serde_json::json!({"type": "File", "blob": hash, "mode": 420}),
            );
            contents.push((hash, bytes));
        }
        let record = serde_json::to_vec(&serde_json::json!({ "artifacts": artifacts })).unwrap();
        let mut encoded = Vec::new();
        write(source.path(), &record, &mut encoded).unwrap();
        let destination = root();
        let mut transfer = Transfer::default();
        super::read(
            destination.path(),
            &encoded[..],
            |_| Ok(()),
            &mut transfer,
            no_apart,
        )
        .unwrap();
        assert_eq!(transfer.written, 200);
        for (hash, bytes) in contents {
            let kept = std::fs::read(destination.path().join("blobs").join(hash)).unwrap();
            assert_eq!(kept, bytes.as_bytes());
        }
    }

    /// When every write fails, as on a full disk, the read ends in their
    /// error. Whether the reader is waiting for room in the queue as the last
    /// writer fails depends on timing, which this does not control.
    #[test]
    fn a_read_whose_writers_all_fail_ends_in_their_error() {
        let source = root();
        let mut artifacts = serde_json::Map::new();
        for index in 0..100 {
            let bytes = format!("module {index}\n").repeat(50);
            let hash = blake3::hash(bytes.as_bytes()).to_hex().to_string();
            std::fs::write(source.path().join("blobs").join(&hash), &bytes).unwrap();
            artifacts.insert(
                format!("cache/{index}"),
                serde_json::json!({"type": "File", "blob": hash, "mode": 420}),
            );
        }
        let record = serde_json::to_vec(&serde_json::json!({ "artifacts": artifacts })).unwrap();
        let mut encoded = Vec::new();
        write(source.path(), &record, &mut encoded).unwrap();
        let destination = root();
        // Without its scratch directory, no blob can be written.
        std::fs::remove_dir(destination.path().join("tmp")).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let path = destination.path().to_owned();
        std::thread::spawn(move || {
            let read = read(&path, &encoded[..], |_| Ok(()));
            let _ = done.send(read.map(|_| ()));
        });
        let read = finished
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the read did not finish");
        assert!(read.is_err());
    }

    #[test]
    fn compressed_packs_restore_verified_blobs() {
        let source = root();
        let (record, encoded) = sample(source.path());
        assert!(encoded.len() < 1000);
        let destination = root();
        assert_eq!(
            read(destination.path(), &encoded[..], |_| Ok(())).unwrap(),
            record
        );
        let blob = std::fs::read_dir(destination.path().join("blobs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            crate::hash::digest_file(&blob).unwrap(),
            blob.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(
            read(destination.path(), &encoded[..], |_| Ok(())).unwrap(),
            record
        );
    }

    #[test]
    fn key_check_precedes_blob_ingestion_without_typed_restore_validation() {
        let source = root();
        let (record, _) = sample(source.path());
        let mut value: serde_json::Value = serde_json::from_slice(&record).unwrap();
        value["key"] = serde_json::json!("requested-key");
        value["version"] = serde_json::json!(99);
        let record = serde_json::to_vec(&value).unwrap();
        let mut encoded = Vec::new();
        write(source.path(), &record, &mut encoded).unwrap();
        let destination = root();
        let error = read(destination.path(), &encoded[..], |record| {
            record.check_key("another-key")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "remote manifest is for another key");
        assert_eq!(
            std::fs::read_dir(destination.path().join("blobs"))
                .unwrap()
                .count(),
            0
        );
        // Transport has historically accepted this partial record; version and
        // structure remain the typed restore reader's decision.
        assert_eq!(
            read(destination.path(), &encoded[..], |record| record
                .check_key("requested-key"))
            .unwrap(),
            record
        );
        assert!(crate::record::ResultRecord::read(&record, "requested-key").is_err());
    }

    #[test]
    fn truncated_frames_and_bad_checksums_are_rejected_with_local_blobs_present() {
        let source = root();
        let (_, encoded) = sample(source.path());
        for removed in [1, 4, encoded.len() / 2] {
            assert!(
                read(source.path(), &encoded[..encoded.len() - removed], |_| Ok(
                    ()
                ))
                .is_err()
            );
        }
        let mut corrupt = encoded;
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(read(source.path(), &corrupt[..], |_| Ok(())).is_err());
    }

    #[test]
    fn decompressed_blob_hashes_and_trailing_data_are_checked() {
        let source = root();
        let (_, encoded) = sample(source.path());
        let raw = zstd::stream::decode_all(&encoded[..]).unwrap();
        let mut corrupt = raw.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        let encoded = zstd::stream::encode_all(&corrupt[..], LEVEL).unwrap();
        let destination = root();
        assert!(
            read(destination.path(), &encoded[..], |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("does not match its hash")
        );
        assert_eq!(
            std::fs::read_dir(destination.path().join("blobs"))
                .unwrap()
                .count(),
            0
        );
        let mut trailing = raw;
        trailing.push(0);
        let encoded = zstd::stream::encode_all(&trailing[..], LEVEL).unwrap();
        assert!(
            read(destination.path(), &encoded[..], |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("more than its record cites")
        );
    }

    #[test]
    fn record_and_expansion_lengths_are_bounded_before_reading_payloads() {
        let source = root();
        let mut raw = MAGIC.to_vec();
        raw.extend((LARGEST_RECORD + 1).to_le_bytes());
        let bytes = zstd::stream::encode_all(&raw[..], LEVEL).unwrap();
        assert!(
            read(source.path(), &bytes[..], |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
        let mut used = super::super::LARGEST_UPLOAD - 8;
        assert!(part_size(&mut used, 1, super::super::LARGEST_UPLOAD).is_err());
        let mut used = u64::MAX;
        assert!(part_size(&mut used, 0, super::super::LARGEST_UPLOAD).is_err());
    }

    #[test]
    fn blobs_stored_apart_are_fetched_only_when_missing() {
        let source = root();
        let (record, _) = sample(source.path());
        let mut encoded = Vec::new();
        super::write(
            source.path(),
            &record,
            |_| Ok(Placement::Apart),
            &mut encoded,
        )
        .unwrap();
        let hash = std::fs::read_dir(source.path().join("blobs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        // Only the record and the table travel in the pack.
        assert!(zstd::stream::decode_all(&encoded[..]).unwrap().len() < 200);
        let destination = root();
        let asked = std::sync::Mutex::new(Vec::new());
        super::read(
            destination.path(),
            &encoded[..],
            |_| Ok(()),
            &mut Transfer::default(),
            |missing| {
                asked.lock().unwrap().extend(missing);
                let compressed = compress(source.path(), &hash)?;
                read_blob(destination.path(), &hash, compressed.reopen()?)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*asked.lock().unwrap(), std::slice::from_ref(&hash));
        assert!(destination.path().join("blobs").join(&hash).is_file());
        let mut again = Transfer::default();
        super::read(
            destination.path(),
            &encoded[..],
            |_| Ok(()),
            &mut again,
            no_apart,
        )
        .unwrap();
        assert_eq!(
            again.skipped,
            std::fs::metadata(source.path().join("blobs").join(&hash))
                .unwrap()
                .len()
        );
    }

    #[test]
    fn a_missing_apart_blob_fails_the_read() {
        let source = root();
        let (record, _) = sample(source.path());
        let mut encoded = Vec::new();
        super::write(
            source.path(),
            &record,
            |_| Ok(Placement::Apart),
            &mut encoded,
        )
        .unwrap();
        let error = super::read(
            root().path(),
            &encoded[..],
            |_| Ok(()),
            &mut Transfer::default(),
            |_| anyhow::bail!("gone"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "gone");
    }

    #[test]
    fn the_blob_table_is_checked_against_the_record() {
        let source = root();
        let (record, encoded) = sample(source.path());
        let raw = zstd::stream::decode_all(&encoded[..]).unwrap();
        let table = 4 + 8 + record.len();
        let reencode = |raw: &[u8]| zstd::stream::encode_all(raw, LEVEL).unwrap();
        let mut count = raw.clone();
        count[table] = 2;
        assert!(
            read(root().path(), &reencode(&count)[..], |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("do not match its record")
        );
        let mut placement = raw.clone();
        placement[table + 4] = 7;
        assert!(
            read(root().path(), &reencode(&placement)[..], |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("invalid blob placement")
        );
        let mut magic = raw;
        magic[0] = b'x';
        assert!(
            read(root().path(), &reencode(&magic)[..], |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("not a qk remote pack")
        );
    }

    #[test]
    fn a_blob_stored_apart_must_match_its_hash() {
        let source = root();
        sample(source.path());
        let hash = std::fs::read_dir(source.path().join("blobs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let compressed = compress(source.path(), &hash).unwrap();
        let destination = root();
        let other = "0".repeat(64);
        assert!(
            read_blob(destination.path(), &other, compressed.reopen().unwrap())
                .unwrap_err()
                .to_string()
                .contains("does not match its hash")
        );
        let received = read_blob(destination.path(), &hash, compressed.reopen().unwrap()).unwrap();
        assert_eq!(received, compressed.as_file().metadata().unwrap().len());
        assert!(destination.path().join("blobs").join(&hash).is_file());
    }

    #[test]
    fn oversized_decoder_windows_are_rejected() {
        let source = root();
        let (_, mut encoded) = sample(source.path());
        // Streaming frames omit content size; their window descriptor follows
        // the magic and frame header. Advertise a 1 GiB decoder window.
        assert_eq!(encoded[4] & 0x20, 0);
        encoded[5] = (30 - 10) << 3;
        assert!(read(source.path(), &encoded[..], |_| Ok(())).is_err());
    }
}
