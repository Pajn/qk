import { decompress } from "fzstd";
import { describe, expect, it } from "vitest";
import { buildUpload, rawFrame } from "../src/frames";
import { blobReads } from "../src/worker";
import { READ, blob, bundle, expectedPack, hash, putApart, putBundle, record, request, sha256, sorted, upload, type TestBlob } from "./helpers";

async function missing(hashes: string[]): Promise<{ missing: string[]; loose: string[] }> {
  const response = await request("POST", "blobs/missing", { token: READ, body: JSON.stringify({ blobs: hashes }) });
  expect(response.status).toBe(200);
  return (await response.json()) as { missing: string[]; loose: string[] };
}

async function commit(key: string, blobs: TestBlob[]): Promise<Uint8Array> {
  const body = record(key, blobs);
  const response = await request("PUT", `entries/${key}`, { body: upload(body, blobs) });
  expect(response.status, await response.clone().text()).toBe(204);
  return body;
}

async function pack(path: string): Promise<Uint8Array> {
  const response = await request("GET", path, { token: READ });
  expect(response.status).toBe(200);
  return decompress(new Uint8Array(await response.arrayBuffer()));
}

describe("bundles", () => {
  it("store small blobs, each found by its hash", async () => {
    const blobs = [blob("first", false), blob("second", false)];
    await putBundle(blobs);
    expect(await missing(blobs.map((blob) => blob.hash))).toEqual({ missing: [], loose: [] });
    for (const stored of blobs) {
      expect((await request("HEAD", `blobs/${stored.hash}`, { token: READ })).status).toBe(200);
      const response = await request("GET", `blobs/${stored.hash}`, { token: READ });
      expect(response.status).toBe(200);
      expect(new Uint8Array(await response.arrayBuffer())).toEqual(rawFrame(stored.raw));
    }
  });

  it("are refused unless named by their header and carrying every blob they list", async () => {
    const { id, body } = await bundle([blob("named", false)]);
    expect((await request("PUT", `bundles/${hash()}`, { body })).status).toBe(400);
    const listedOnly = buildUpload(new Uint8Array(0), [{ hash: hash(), raw: 4 }]);
    expect((await request("PUT", `bundles/${id}`, { body: listedOnly })).status).toBe(400);
    const withRecord = buildUpload(new Uint8Array([1]), [{ hash: hash(), raw: 4, frame: rawFrame(new Uint8Array(4)) }]);
    expect((await request("PUT", `bundles/${id}`, { body: withRecord })).status).toBe(400);
    expect((await request("PUT", `bundles/${id}`, { body, token: READ })).status).toBe(403);
  });

  it("are taken again once stored, as sent twice", async () => {
    const blobs = [blob("sent twice", false)];
    await putBundle(blobs);
    await putBundle(blobs);
    expect((await missing([blobs[0]!.hash])).missing).toEqual([]);
  });

  it("make loose blobs stored on their own no longer loose", async () => {
    const small = blob("stored on its own first", false);
    await putApart(small);
    expect(await missing([small.hash])).toEqual({ missing: [], loose: [small.hash] });
    await putBundle([small]);
    expect(await missing([small.hash])).toEqual({ missing: [], loose: [] });
  });
});

describe("packs citing bundled blobs", () => {
  it("commit, and carry them inline in order beside blobs stored otherwise", async () => {
    const key = hash();
    // Bundled with others the record does not cite, so their ranges have
    // gaps to skip, and in two bundles.
    const bundled = Array.from({ length: 30 }, (_, index) => blob(`bundled ${index}`, false));
    const unrelated = Array.from({ length: 10 }, (_, index) => blob(`not cited ${index}`, false));
    await putBundle([...bundled.slice(0, 20), ...unrelated]);
    await putBundle(bundled.slice(20));
    const own = blob("stored on its own", false);
    await putApart(own);
    const blobs = [...bundled, own, blob("carried inline", true)];
    const body = await commit(key, blobs);
    expect(await pack(`entries/${key}`)).toEqual(expectedPack(body, blobs, () => true));
  });

  it("carry blobs from bundles that alternate in hash order", async () => {
    const key = hash();
    const blobs = sorted(Array.from({ length: 40 }, (_, index) => blob(`alternating ${index}`, false)));
    await putBundle(blobs.filter((_, index) => index % 2 === 0));
    await putBundle(blobs.filter((_, index) => index % 2 === 1));
    const body = await commit(key, blobs);
    expect(await pack(`entries/${key}`)).toEqual(expectedPack(body, blobs, () => true));
  });

  it("read on their turn the blobs of bundles past what the pack may hold ahead", async () => {
    const key = hash();
    // Twenty bundles, alternating, two blobs each that one range reads: a
    // range is held from its first blob to its second, and twenty of them
    // hold more than the read-ahead budget, so some blobs are read alone.
    const blobs: TestBlob[] = sorted(
      Array.from({ length: 40 }, (_, index) => ({ hash: hash(), raw: new Uint8Array(500 << 10).fill(index), inline: false })),
    );
    for (let bundle = 0; bundle < 20; bundle++) {
      await putBundle(blobs.filter((_, index) => index % 20 === bundle));
    }
    const body = await commit(key, blobs);
    const expected = expectedPack(body, blobs, () => true);
    expect(await sha256(await pack(`entries/${key}`))).toEqual(await sha256(expected));
  });

  it("stream on its turn a bundled blob too large to read ahead", async () => {
    const key = hash();
    const large: TestBlob = { hash: hash(), raw: new Uint8Array(17 << 20).fill(9), inline: false };
    const blobs = [blob("small, bundled", false), large, blob("after", false)];
    await putBundle(blobs);
    const body = await commit(key, blobs);
    const expected = expectedPack(body, blobs, () => true);
    expect(await sha256(await pack(`entries/${key}`))).toEqual(await sha256(expected));
  });
});

describe("the reads of a pack's bundled blobs", () => {
  // Blobs of `size` bytes at consecutive offsets of their bundle.
  const stored = (bundles: string[], size: number) => {
    const offsets = new Map<string, number>();
    return bundles.map((bundle, index) => {
      const offset = offsets.get(bundle) ?? 0;
      offsets.set(bundle, offset + size);
      return { hash: `${index}`, size, bundle, offset };
    });
  };

  it("are one range of each bundle, whatever comes between", () => {
    const blobs = stored(
      Array.from({ length: 40 }, (_, index) => (index % 2 === 0 ? "a" : "b")),
      1000,
    );
    const { reads, where } = blobReads(blobs);
    expect(reads.map((read) => [read.key, read.parts.length])).toEqual([
      ["bundles/a", 20],
      ["bundles/b", 20],
    ]);
    expect(where.slice(0, 3)).toEqual([
      { read: 0, part: 0 },
      { read: 1, part: 0 },
      { read: 0, part: 1 },
    ]);
  });

  it("end past what one range takes, and for a blob stored on its own", () => {
    // Two of each fit a range of 1 MiB, not three.
    const blobs = [...stored(["a", "a", "a"], 400 << 10), { hash: "own", size: 10, bundle: null, offset: 0 }];
    const { reads } = blobReads(blobs);
    expect(reads.map((read) => [read.key, read.parts.length])).toEqual([
      ["bundles/a", 2],
      ["bundles/a", 1],
      ["blobs/own", 1],
    ]);
  });
});
