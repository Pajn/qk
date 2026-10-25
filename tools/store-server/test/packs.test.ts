import { decompress } from "fzstd";
import { describe, expect, it } from "vitest";
import { Bloom } from "../src/bloom";
import { rawFrame } from "../src/frames";
import { READ, blob, bytes, expectedPack, hash, putApart, record, request, upload } from "./helpers";

async function commit(key: string, blobs: ReturnType<typeof blob>[]) {
  const body = record(key, blobs);
  const response = await request("PUT", `entries/${key}`, { body: upload(body, blobs) });
  expect(response.status, await response.clone().text()).toBe(204);
  return body;
}

async function pack(path: string): Promise<Uint8Array> {
  const response = await request("GET", path, { token: READ });
  expect(response.status).toBe(200);
  expect(response.headers.get("Content-Type")).toBe("application/x-qk-pack");
  return decompress(new Uint8Array(await response.arrayBuffer()));
}

describe("entries", () => {
  it("come back as a pack carrying every blob inline without a session", async () => {
    const key = hash();
    const blobs = [blob("first inline blob", true), blob("second inline blob", true), blob("stored apart", false)];
    await putApart(blobs[2]!);
    const body = await commit(key, blobs);
    expect(await pack(`entries/${key}`)).toEqual(expectedPack(body, blobs, () => true));
  });

  it("leave out the blobs a session holds", async () => {
    const key = hash();
    const blobs = [blob("held", true), blob("not held", true), blob("held apart", false), blob("apart", false)];
    await putApart(blobs[2]!);
    await putApart(blobs[3]!);
    const body = await commit(key, blobs);
    const bloom = Bloom.empty(4096, 7);
    bloom.insert(blobs[0]!.hash);
    bloom.insert(blobs[2]!.hash);
    const opened = await request("POST", "sessions", { token: READ, body: bloom.toBytes() });
    expect(opened.status).toBe(201);
    const { session, expires } = await opened.json<{ session: string; expires: number }>();
    expect(session).toMatch(/^[0-9a-f]{32}$/);
    expect(expires).toBeGreaterThan(Date.now() + 60 * 60 * 1000);
    const held = new Set([blobs[0]!.hash, blobs[2]!.hash]);
    expect(await pack(`entries/${key}?session=${session}`)).toEqual(
      expectedPack(body, blobs, (blob) => !held.has(blob.hash)),
    );
    // An unknown session holds nothing.
    expect(await pack(`entries/${key}?session=${"0".repeat(32)}`)).toEqual(expectedPack(body, blobs, () => true));
  });

  it("are missing until committed", async () => {
    const key = hash();
    expect((await request("GET", `entries/${key}`)).status).toBe(404);
    expect((await request("HEAD", `entries/${key}`)).status).toBe(404);
  });

  it("commit once the blobs stored apart are there, and only once", async () => {
    const key = hash();
    const blobs = [blob("inline", true), blob("apart", false)];
    const body = upload(record(key, blobs), blobs);
    const refused = await request("PUT", `entries/${key}`, { body });
    expect(refused.status).toBe(422);
    expect(await refused.json()).toMatchObject({ error: "missing_blobs", blobs: [blobs[1]!.hash] });
    await putApart(blobs[1]!);
    expect((await request("PUT", `entries/${key}`, { body })).status).toBe(204);
    const again = await request("PUT", `entries/${key}`, { body });
    expect(again.status).toBe(409);
    expect(await again.json()).toMatchObject({ error: "exists" });
  });

  it("refuse an upload that is not for its key or does not add up", async () => {
    const key = hash();
    const other = upload(record(hash(), []), []);
    expect((await request("PUT", `entries/${key}`, { body: other })).status).toBe(400);
    const blobs = [blob("inline", true)];
    const body = upload(record(key, blobs), blobs);
    const response = await request("PUT", `entries/${key}`, { body: body.subarray(0, body.length - 1) });
    expect(response.status).toBe(400);
    expect(await response.json()).toMatchObject({ error: "malformed" });
    const unsorted = [blob("a", true), blob("b", true)].sort((a, b) => (a.hash < b.hash ? 1 : -1));
    const header = bytes(JSON.stringify({ record: 2, blobs: unsorted.map((b) => [b.hash, 1, 10]) }));
    const raw = new Uint8Array(4 + header.length + 2 + 20);
    new DataView(raw.buffer).setUint32(0, header.length, true);
    raw.set(header, 4);
    expect((await request("PUT", `entries/${key}`, { body: raw })).status).toBe(400);
  });
});

describe("blobs", () => {
  it("are found inside the record that carries them inline", async () => {
    const key = hash();
    const blobs = [blob("one", true), blob("two", true), blob("three", true)];
    await commit(key, blobs);
    const wanted = blobs[1]!;
    expect((await request("GET", `blobs/${wanted.hash}`)).status).toBe(404);
    expect((await request("HEAD", `blobs/${wanted.hash}`)).status).toBe(404);
    const response = await request("GET", `blobs/${wanted.hash}?entry=${key}`, { token: READ });
    expect(response.status).toBe(200);
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(rawFrame(wanted.raw));
  });

  it("stored apart are served as uploaded", async () => {
    const stored = blob("stored apart", false);
    await putApart(stored);
    const response = await request("GET", `blobs/${stored.hash}`, { token: READ });
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(rawFrame(stored.raw));
    expect((await request("PUT", `blobs/${hash()}`, { body: bytes("not zstd") })).status).toBe(400);
  });

  it("missing are named", async () => {
    const stored = blob("there", false);
    await putApart(stored);
    const absent = hash();
    const response = await request("POST", "blobs/missing", {
      token: READ,
      body: JSON.stringify({ blobs: [stored.hash, absent] }),
    });
    expect(await response.json()).toEqual({ missing: [absent] });
    const tooMany = JSON.stringify({ blobs: Array.from({ length: 4097 }, hash) });
    expect((await request("POST", "blobs/missing", { body: tooMany })).status).toBe(400);
  });
});

describe("pack timing", () => {
  it("says what the server spent before the pack began", async () => {
    const name = hash().slice(0, 32);
    const record = bytes(JSON.stringify({ version: 1, name, tasks: {} }));
    expect((await request("PUT", `expectations/${name}`, { body: upload(record, []) })).status).toBe(204);
    const response = await request("GET", `expectations/${name}`, { token: READ });
    expect(response.status).toBe(200);
    expect(response.headers.get("Server-Timing")).toMatch(/^session;dur=\d+, object;dur=\d+, compose;dur=\d+$/);
    await response.arrayBuffer();
  });
});

describe("warm records", () => {
  it("are replaced by the newest save", async () => {
    const name = `${hash().slice(0, 32)}/${hash().slice(0, 32)}`;
    const first = [blob("first", true)];
    const second = [blob("second", true)];
    const firstRecord = bytes(JSON.stringify({ version: 1, key: ["a"], groups: { g: { artifacts: { x: { blob: first[0]!.hash } } } } }));
    const secondRecord = bytes(JSON.stringify({ version: 1, key: ["b"], groups: { g: { artifacts: { x: { blob: second[0]!.hash } } } } }));
    expect((await request("PUT", `warm/${name}`, { body: upload(firstRecord, first) })).status).toBe(204);
    expect((await request("PUT", `warm/${name}`, { body: upload(secondRecord, second) })).status).toBe(204);
    expect(await pack(`warm/${name}`)).toEqual(expectedPack(secondRecord, second, () => true));
    const found = await request("GET", `blobs/${second[0]!.hash}?warm=${name}`);
    expect(new Uint8Array(await found.arrayBuffer())).toEqual(rawFrame(second[0]!.raw));
  });
});

describe("expectations", () => {
  it("are replaced by the newest save under their name alone", async () => {
    const name = hash().slice(0, 32);
    const record = (millis: number) =>
      bytes(JSON.stringify({ version: 1, name, tasks: { "app:test": { millis, cores: 4, updated: 1 } } }));
    expect((await request("GET", `expectations/${name}`)).status).toBe(404);
    expect((await request("PUT", `expectations/${name}`, { body: upload(record(1), []) })).status).toBe(204);
    expect((await request("PUT", `expectations/${name}`, { body: upload(record(2), []) })).status).toBe(204);
    expect(await pack(`expectations/${name}`)).toEqual(expectedPack(record(2), [], () => true));
    expect((await request("HEAD", `expectations/${name}`)).status).toBe(200);
    expect((await request("PUT", `expectations/${hash().slice(0, 32)}`, { body: upload(record(3), []) })).status).toBe(400);
    expect((await request("PUT", `expectations/${hash()}`, { body: upload(record(3), []) })).status).toBe(404);
    const cited = [blob("cited", true)];
    expect((await request("PUT", `expectations/${name}`, { body: upload(record(4), cited) })).status).toBe(400);
    expect((await request("PUT", `expectations/${name}`, { token: READ, body: upload(record(5), []) })).status).toBe(403);
  });

  it("are refused past their size", async () => {
    const name = hash().slice(0, 32);
    const large = bytes(JSON.stringify({ version: 1, name, tasks: {}, padding: "x".repeat(4 << 20) }));
    expect((await request("PUT", `expectations/${name}`, { body: upload(large, []) })).status).toBe(413);
  });
});

describe("path sets", () => {
  it("are replaced by the newest save for their weak key alone", async () => {
    const weak = hash();
    const record = (read: string) => bytes(JSON.stringify({ version: 1, weak, sets: [{ reads: [read] }] }));
    expect((await request("PUT", `pathsets/${weak}`, { body: upload(record("a"), []) })).status).toBe(204);
    expect((await request("PUT", `pathsets/${weak}`, { body: upload(record("b"), []) })).status).toBe(204);
    expect(await pack(`pathsets/${weak}`)).toEqual(expectedPack(record("b"), [], () => true));
    expect((await request("HEAD", `pathsets/${weak}`)).status).toBe(200);
    expect((await request("PUT", `pathsets/${hash()}`, { body: upload(record("c"), []) })).status).toBe(400);
    const cited = [blob("cited", true)];
    expect((await request("PUT", `pathsets/${weak}`, { body: upload(record("d"), cited) })).status).toBe(400);
  });
});
