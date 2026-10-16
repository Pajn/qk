// The Bloom filter a client sends to open a session, naming the blobs it
// holds. The layout and the bit positions are in PROTOCOL.md; the client's
// implementation is crates/qk-cache/src/remote/bloom.rs.

const VERSION = 1;
const HEADER = 16;
const LARGEST_K = 32;
/** The largest filter a session accepts, header included. */
export const LARGEST_FILTER = 16 << 20;
const MASK = (1n << 64n) - 1n;

export class Bloom {
  private constructor(
    readonly k: number,
    readonly m: bigint,
    readonly bits: Uint8Array,
  ) {}

  /** An empty filter of `m` bits and `k` hashes per blob. */
  static empty(m: number, k: number): Bloom {
    return new Bloom(k, BigInt(m), new Uint8Array(Math.ceil(m / 8)));
  }

  /** The filter `bytes` hold, or `null` when they are not one. */
  static parse(bytes: Uint8Array): Bloom | null {
    if (bytes.length < HEADER || bytes.length > LARGEST_FILTER) {
      return null;
    }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const version = view.getUint32(0, true);
    const k = view.getUint32(4, true);
    const m = view.getBigUint64(8, true);
    if (version !== VERSION || k < 1 || k > LARGEST_K || m < 1n) {
      return null;
    }
    if (BigInt(bytes.length - HEADER) !== (m + 7n) / 8n) {
      return null;
    }
    return new Bloom(k, m, bytes.subarray(HEADER));
  }

  insert(hash: string): void {
    for (const bit of this.positions(hash)) {
      this.bits[bit >> 3]! |= 1 << (bit & 7);
    }
  }

  /** Whether the filter may hold `hash`, 64 hex digits. */
  has(hash: string): boolean {
    for (const bit of this.positions(hash)) {
      if ((this.bits[bit >> 3]! & (1 << (bit & 7))) === 0) {
        return false;
      }
    }
    return true;
  }

  toBytes(): Uint8Array {
    const bytes = new Uint8Array(HEADER + this.bits.length);
    const view = new DataView(bytes.buffer);
    view.setUint32(0, VERSION, true);
    view.setUint32(4, this.k, true);
    view.setBigUint64(8, this.m, true);
    bytes.set(this.bits, HEADER);
    return bytes;
  }

  /** The bits `hash` sets: `(h1 + i × h2) mod m`, the sum wrapping at 2^64.
   * The bit count is below 2^27, so each position fits a number. */
  private positions(hash: string): number[] {
    const bytes = decode(hash);
    const view = new DataView(bytes.buffer);
    const h1 = view.getBigUint64(0, true);
    const h2 = view.getBigUint64(8, true) | 1n;
    const positions = [];
    for (let i = 0n; i < BigInt(this.k); i++) {
      positions.push(Number(((h1 + i * h2) & MASK) % this.m));
    }
    return positions;
  }
}

/** The first 16 of the 32 bytes a hash names. */
function decode(hash: string): Uint8Array {
  const bytes = new Uint8Array(16);
  for (let index = 0; index < 16; index++) {
    bytes[index] = parseInt(hash.slice(index * 2, index * 2 + 2), 16) || 0;
  }
  return bytes;
}
