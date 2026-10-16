//! The Bloom filter a client sends a store server, naming the blobs it holds
//! so their bytes can be left out of what the server sends back. Its layout is
//! in `tools/store-server/PROTOCOL.md`.

use std::path::Path;

/// Hashes per blob.
const K: u32 = 7;
/// The false-positive rate the filter is sized for.
const RATE: f64 = 0.01;
/// The most blobs a filter names; a store larger than this sends none.
const LARGEST: usize = 2_000_000;

pub(super) struct Bloom {
    k: u32,
    bits: Vec<u8>,
    m: u64,
}

impl Bloom {
    /// An empty filter sized for `count` blobs.
    pub(super) fn new(count: usize) -> Self {
        let n = count.max(1) as f64;
        let m = ((-n * RATE.ln()) / (2f64.ln() * 2f64.ln()))
            .ceil()
            .max(64.0) as u64;
        Self {
            k: K,
            bits: vec![0; m.div_ceil(8) as usize],
            m,
        }
    }

    /// A filter of the blobs in the local store at `root`, or `None` when
    /// there are none or too many.
    pub(super) fn for_blobs(root: &Path) -> Option<Self> {
        let names: Vec<String> = std::fs::read_dir(root.join("blobs"))
            .ok()?
            .flatten()
            .filter_map(|item| item.file_name().into_string().ok())
            .filter(|name| crate::record::valid_hash(name))
            .take(LARGEST + 1)
            .collect();
        if names.is_empty() || names.len() > LARGEST {
            return None;
        }
        let mut bloom = Self::new(names.len());
        for name in &names {
            bloom.insert(name);
        }
        Some(bloom)
    }

    fn positions(&self, hash: &str) -> impl Iterator<Item = u64> + '_ {
        let bytes = decode(hash);
        let h1 = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let h2 = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) | 1;
        (0..u64::from(self.k)).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % self.m)
    }

    pub(super) fn insert(&mut self, hash: &str) {
        let positions: Vec<u64> = self.positions(hash).collect();
        for bit in positions {
            self.bits[(bit >> 3) as usize] |= 1 << (bit & 7);
        }
    }

    #[cfg(test)]
    pub(super) fn contains(&self, hash: &str) -> bool {
        self.positions(hash)
            .all(|bit| self.bits[(bit >> 3) as usize] & (1 << (bit & 7)) != 0)
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(16 + self.bits.len());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&self.k.to_le_bytes());
        bytes.extend_from_slice(&self.m.to_le_bytes());
        bytes.extend_from_slice(&self.bits);
        bytes
    }
}

/// The 32 bytes a 64-hex-digit hash names; digits that are not hex count as 0.
fn decode(hash: &str) -> [u8; 32] {
    let mut bytes = [0; 32];
    for (index, pair) in hash.as_bytes().chunks(2).take(32).enumerate() {
        let digit = |byte: u8| (byte as char).to_digit(16).unwrap_or(0) as u8;
        bytes[index] = digit(pair[0]) << 4 | pair.get(1).map_or(0, |byte| digit(*byte));
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared with `tools/store-server/test/bloom.test.ts`: both sides must set
    /// the same bits.
    pub(crate) const VECTORS: [&str; 3] = [
        "0000000000000000000000000000000000000000000000000000000000000000",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ];

    #[test]
    fn a_filter_holds_what_was_inserted() {
        let mut bloom = Bloom::new(3);
        for hash in VECTORS {
            bloom.insert(hash);
        }
        assert!(VECTORS.iter().all(|hash| bloom.contains(hash)));
        assert!(!bloom.contains(&"1".repeat(64)));
    }

    #[test]
    fn the_layout_matches_the_protocol() {
        let mut bloom = Bloom::new(3);
        for hash in VECTORS {
            bloom.insert(hash);
        }
        let bytes = bloom.to_bytes();
        assert_eq!(&bytes[..4], &1u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &7u32.to_le_bytes());
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 64);
        // Recorded once from this implementation; the Worker's tests repeat it.
        assert_eq!(hex(&bytes[16..]), EXPECTED);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    const EXPECTED: &str = "ff000000000000fe";
}
