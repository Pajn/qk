import { decompress } from "fzstd";
import { describe, expect, it } from "vitest";
import { buildUpload, rawFrame } from "../src/frames";
import { READ, blob, expectedPack, hash, hex, putApart, record, request, sorted, upload, type TestBlob } from "./helpers";

async function sha256(data: Uint8Array): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest("SHA-256", data.slice())));
}

/** The body of a bundle carrying `blobs`, and the name it is stored under. */
async function bundle(blobs: TestBlob[]): Promise<{ id: string; body: Uint8Array }> {
  const body = buildUpload(
    new Uint8Array(0),
    sorted(blobs).map((blob) => ({ hash: blob.hash, raw: blob.raw.length, frame: rawFrame(blob.raw) })),
  );
  const length = new DataView(body.buffer, body.byteOffset).getUint32(0, true);
  return { id: await sha256(body.subarray(4, 4 + length)), body };
}

async function putBundle(blobs: TestBlob[]): Promise<string> {
  const { id, body } = await bundle(blobs);
  const response = await request("PUT", `bundles/${id}`, { body });
  expect(response.status, await response.clone().text()).toBe(204);
  return id;
}

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
