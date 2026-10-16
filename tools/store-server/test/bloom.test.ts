import { describe, expect, it } from "vitest";
import { Bloom } from "../src/bloom";
import { hex } from "./helpers";

// Shared with crates/qk-cache/src/remote/bloom.rs: both sides must set the
// same bits.
const VECTORS = [
  "0000000000000000000000000000000000000000000000000000000000000000",
  "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
  "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
];

function filter(): Bloom {
  // The client sizes m = max(64, ceil(-n ln 0.01 / ln² 2)), 64 for n = 3.
  const bloom = Bloom.empty(64, 7);
  for (const hash of VECTORS) {
    bloom.insert(hash);
  }
  return bloom;
}

describe("Bloom filters", () => {
  it("lay out as the protocol says, setting the client's bits", () => {
    const bytes = filter().toBytes();
    const view = new DataView(bytes.buffer);
    expect(view.getUint32(0, true)).toBe(1);
    expect(view.getUint32(4, true)).toBe(7);
    expect(view.getBigUint64(8, true)).toBe(64n);
    expect(hex(bytes.subarray(16))).toBe("ff000000000000fe");
  });

  it("hold what was inserted and not what was not", () => {
    const parsed = Bloom.parse(filter().toBytes())!;
    expect(VECTORS.every((hash) => parsed.has(hash))).toBe(true);
    expect(parsed.has("1".repeat(64))).toBe(false);
  });

  it("refuse filters that are not version 1 or the wrong size", () => {
    const bytes = filter().toBytes();
    const wrongVersion = bytes.slice();
    wrongVersion[0] = 2;
    expect(Bloom.parse(wrongVersion)).toBeNull();
    expect(Bloom.parse(bytes.subarray(0, bytes.length - 1))).toBeNull();
    const noHashes = bytes.slice();
    noHashes.fill(0, 4, 8);
    expect(Bloom.parse(noHashes)).toBeNull();
  });
});
