//! Streaming remote packs, with bounded expansion and verified local blobs.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::record::StoredRecord;
use anyhow::{Context, Result, bail};

const LEVEL: i32 = 3;
const LARGEST_RECORD: u64 = 1 << 30;
const WINDOW_LOG_MAX: u32 = 27;

pub(super) fn write(root: &Path, record: &[u8], output: impl Write) -> Result<()> {
    let parsed = crate::record::StoredRecord::read(record)?;
    let mut writer = zstd::stream::write::Encoder::new(io::BufWriter::new(output), LEVEL)?;
    writer.include_checksum(true)?;
    let mut used = 0;
    part_size(&mut used, record.len() as u64, LARGEST_RECORD)?;
    writer.write_all(&(record.len() as u64).to_le_bytes())?;
    writer.write_all(record)?;
    for blob in parsed.blobs() {
        if !crate::record::valid_hash(&blob) {
            bail!("invalid blob name in remote record");
        }
        let mut file = File::open(root.join("blobs").join(&blob))?;
        let length = file.metadata()?.len();
        part_size(&mut used, length, super::LARGEST_UPLOAD)?;
        writer.write_all(&length.to_le_bytes())?;
        if io::copy(&mut (&mut file).take(length), &mut writer)? != length {
            bail!("blob {blob} changed while it was uploaded");
        }
    }
    writer.finish()?.flush()?;
    Ok(())
}

pub(super) fn read(
    root: &Path,
    input: impl Read,
    check: impl Fn(&StoredRecord) -> Result<()>,
) -> Result<Vec<u8>> {
    let mut input = input.take(super::LARGEST_UPLOAD + 1);
    let mut decoder = zstd::stream::read::Decoder::new(&mut input)?;
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let record = read_parts(root, &mut decoder, check)?;
    drop(decoder);
    if input.limit() == 0 {
        bail!("remote pack exceeds 5 GiB");
    }
    Ok(record)
}

fn read_parts(
    root: &Path,
    mut body: impl Read,
    check: impl Fn(&StoredRecord) -> Result<()>,
) -> Result<Vec<u8>> {
    let mut used = 0;
    let length = part_length(&mut body)?;
    part_size(&mut used, length, LARGEST_RECORD)?;
    let mut record = Vec::new();
    if (&mut body).take(length).read_to_end(&mut record)? as u64 != length {
        bail!("remote pack ends early");
    }
    let parsed = crate::record::StoredRecord::read(&record).context("invalid remote record")?;
    check(&parsed)?;
    for blob in parsed.blobs() {
        if !crate::record::valid_hash(&blob) {
            bail!("invalid blob name in remote record");
        }
        let length = part_length(&mut body)?;
        part_size(&mut used, length, super::LARGEST_UPLOAD)?;
        let mut part = (&mut body).take(length);
        let target = root.join("blobs").join(&blob);
        let copied = if target.is_file() {
            io::copy(&mut part, &mut io::sink())?
        } else {
            let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
            let copied = io::copy(&mut part, file.as_file_mut())?;
            if copied == length {
                if crate::hash::digest_file(file.path())? != blob {
                    bail!("remote blob {blob} does not match its hash");
                }
                file.persist(&target)?;
            }
            copied
        };
        if copied != length {
            bail!("remote pack ends early");
        }
    }
    if body.read(&mut [0])? != 0 {
        bail!("remote pack holds more than its record cites");
    }
    Ok(record)
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
        let raw = (LARGEST_RECORD + 1).to_le_bytes();
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
