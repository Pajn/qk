import { env } from "cloudflare:test";
import { decompress } from "fzstd";
import { describe, expect, it } from "vitest";
import { rawFrame } from "../src/frames";
import { hash, hex } from "./helpers";

describe("raw frames", () => {
  it("of nothing are one empty last block", () => {
    expect(hex(rawFrame(new Uint8Array(0)))).toBe("28b52ffd0038010000");
  });

  it("split into blocks of at most 128 KiB that decode back", () => {
    const data = Uint8Array.from({ length: 300_000 }, (_, index) => (index * 7919) & 0xff);
    const frame = rawFrame(data);
    // Three blocks: 131072, 131072 and 37856 bytes.
    expect(frame.length).toBe(6 + 3 * 3 + data.length);
    expect(hex(frame.subarray(6, 9))).toBe(hex(new Uint8Array([0x00, 0x00, 0x10])));
    expect(decompress(frame)).toEqual(data);
  });
});

describe("R2", () => {
  // Commits rely on this to keep the first writer's entry.
  it("refuses a put conditional on no object when one exists", async () => {
    const key = `test/${hash()}`;
    expect(await env.STORE.put(key, "first", { onlyIf: { etagDoesNotMatch: "*" } })).not.toBeNull();
    expect(await env.STORE.put(key, "second", { onlyIf: { etagDoesNotMatch: "*" } })).toBeNull();
    expect(await (await env.STORE.get(key))!.text()).toBe("first");
  });
});
